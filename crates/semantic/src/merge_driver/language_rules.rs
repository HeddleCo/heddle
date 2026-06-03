// SPDX-License-Identifier: Apache-2.0
//! Per-language behavior for the AST-defined item extractor in [`super::items`].
//!
//! Each supported [`crate::parser::Language`] has a unit-struct implementation
//! of [`LanguageRules`]. The trait isolates the four decision points where
//! per-language behavior diverges (audit: HeddleCo/heddle#133):
//!
//! 1. [`LanguageRules::classify_node`] — is this AST node an item?
//! 2. [`LanguageRules::leading_metadata_kinds`] — what siblings attach to the
//!    next item?
//! 3. [`LanguageRules::signature_hash`] — what disambiguates overloads?
//! 4. [`LanguageRules::extra_scope`] — what extra context keys an item?
//!
//! The single dispatch site is [`rules_for`]; everywhere else in this module
//! queries the chosen rules object rather than match-ing on `Language`
//! directly. New languages plug in by adding a unit struct, implementing the
//! trait, and adding one arm to [`rules_for`] — the structural shape forces
//! each new implementation to confront all four decision axes rather than
//! letting any of them silently default to a no-op.
//!
//! **Discipline guard**: C++'s declarator/qualified-name machinery
//! ([`c_function_name`], [`c_function_scope`], [`c_signature_hash`], and
//! their support functions) lives inside this file alongside [`CppRules`]
//! and **must not** be generalised into a shared helper. Per the audit's
//! biggest-risk note, that generalisation is a separate, deferred decision
//! (HeddleCo/heddle#188 TW4) and conflating it with this refactor is
//! exactly the regret the audit warned about. If you find yourself
//! tempted to lift C++ machinery into the trait or into [`super::items`],
//! stop.

use std::hash::{Hash, Hasher};

use tree_sitter::Node;

use crate::parser::Language;

/// Cap on per-traversal iterations through the C/C++ declarator subtree.
/// **If you bump this, audit every walk for cycle safety** — tree-sitter
/// nodes form a DAG in practice but pathological inputs can create
/// cycle-shaped traversals.
const WALK_LIMIT: usize = 32;

/// Cap on parent-chain traversal depth when collecting enclosing
/// `template_declaration` parameter lists. **If you bump this, audit
/// every walk for cycle safety.**
const MAX_PARENT_WALK_DEPTH: usize = 64;

/// Categorisation of an item. Used as part of [`super::items::ItemKey`] so
/// two items with the same name but different shapes (e.g. a struct `Foo`
/// and a function `Foo`) don't collide.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) enum ItemKind {
    Function,
    Method,
    Impl,
    Module,
    /// A `use` / `pub use` declaration, keyed by its import path so additive
    /// re-exports on disjoint paths union and same-path divergence conflicts
    /// (HeddleCo/heddle#468).
    Use,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Static,
}

/// Classifier output: what kind of item, its name, an optional body to
/// recurse into (for container items), and a parameter-signature hash for
/// overload disambiguation (zero for non-function items).
pub(super) struct Classified<'a> {
    pub kind: ItemKind,
    pub name: String,
    pub container_body: Option<Node<'a>>,
    pub signature_hash: u64,
    /// Extra scope components appended to the inherited scope before
    /// constructing the ItemKey. Used for Go method receivers (without
    /// the receiver type in scope, two methods named `String` on
    /// different receiver types collide) and for C/C++ out-of-class
    /// definitions like `void A::foo()` (`extra_scope = ["A"]`).
    pub extra_scope: Vec<String>,
}

/// Static description of a leading-sibling node kind that "belongs to"
/// the next item — outer attributes, annotations, decorators, doc
/// comments. Returned in language-specific slices from
/// [`LanguageRules::leading_metadata_kinds`] and consumed by
/// [`super::items::leading_metadata_start`].
pub(super) struct MetadataRule {
    pub kind: &'static str,
    pub binding: MetadataBinding,
}

/// Condition under which a [`MetadataRule`] binds its node kind to the
/// following item. Modelling binding as data (rather than a hardcoded
/// per-language `match`) is the structural payoff of this refactor: new
/// languages enumerate their leading-sibling rules in one slice, and the
/// shared [`super::items::is_leading_metadata_for`] interprets them.
pub(super) enum MetadataBinding {
    /// Always bind this node kind to the next item.
    Always,
    /// Bind only when no blank line separates this node from the next
    /// item — distinguishes a doc-comment block attached to the
    /// following symbol from a free-floating comment.
    NoBlankLine,
    /// Rust line/block comments. Bind iff (a) the comment is NOT an
    /// inner doc comment (`//!` / `/*!`, which document the enclosing
    /// module / crate rather than the following item) AND (b) no blank
    /// line separates the comment from the item.
    RustOuterComment,
}

/// Per-language behaviour for AST item extraction. Implementations are
/// unit structs; the single dispatch site is [`rules_for`].
///
/// **All four methods are required** — none has a default. The intent is
/// that adding a new language forces the implementer to make an explicit
/// decision on every axis rather than silently inheriting an empty / generic
/// answer. Use [`generic_signature_hash`] and [`no_extra_scope`] from the
/// impl body when the generic / empty behaviour is correct; the explicit
/// call documents the decision at the impl site and the per-language
/// snapshot tests in [`tests`] pin the chosen shape.
pub(super) trait LanguageRules: Sync {
    /// Try to classify `node` as an item the merger recognises. Returns
    /// `None` for nodes the language doesn't treat as items (the
    /// extractor then walks their children looking for items inside).
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>>;

    /// Static list of leading-sibling node kinds that bind to the next
    /// item. Returning an empty slice is meaningful — it says "this
    /// language has no leading-metadata pattern" rather than "we
    /// forgot" (Python's decorators are absorbed by classify_node's
    /// `decorated_definition` arm, not by leading siblings).
    fn leading_metadata_kinds(&self) -> &'static [MetadataRule];

    /// Compute the signature hash for a function-like item. Most
    /// languages delegate to [`generic_signature_hash`], which hashes
    /// the parameter list at the `"parameters"` field. [`CppRules`]
    /// overrides to walk the declarator subtree and fold in cv/ref
    /// qualifiers.
    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64;

    /// Extra scope components appended to the inherited container chain.
    /// Most languages return [`no_extra_scope`]. [`GoRules`] returns the
    /// method receiver type; [`CppRules`] returns the qualified-name
    /// scope chain for out-of-class definitions.
    fn extra_scope(&self, language: Language, source: &str, item_node: Node<'_>) -> Vec<String>;
}

/// Generic [`LanguageRules::signature_hash`] body for languages that
/// hash the parameter list at the `"parameters"` field. Use from the
/// impl body so the decision is documented at the call site:
/// `fn signature_hash(...) -> u64 { generic_signature_hash(...) }`.
pub(super) fn generic_signature_hash(language: Language, source: &str, item_node: Node<'_>) -> u64 {
    signature_hash_from_field(language, source, item_node, "parameters")
}

/// Generic [`LanguageRules::extra_scope`] body for languages whose
/// items don't contribute any extra scope beyond the inherited
/// container chain. Use from the impl body so the decision is
/// documented at the call site.
pub(super) fn no_extra_scope(
    _language: Language,
    _source: &str,
    _item_node: Node<'_>,
) -> Vec<String> {
    Vec::new()
}

/// Single dispatch site mapping a [`Language`] to its rules implementation.
/// Returns `None` for [`Language::Unknown`].
pub(super) fn rules_for(language: Language) -> Option<&'static dyn LanguageRules> {
    match language {
        Language::Rust => Some(&RustRules),
        Language::Python => Some(&PythonRules),
        Language::JavaScript | Language::TypeScript => Some(&JsTsRules),
        Language::Go => Some(&GoRules),
        Language::C | Language::Cpp => Some(&CppRules),
        Language::Java => Some(&JavaRules),
        Language::Unknown => None,
    }
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

pub(super) struct RustRules;

static RUST_METADATA: &[MetadataRule] = &[
    // Outer attributes only. `inner_attribute_item` (`#![...]`) applies
    // to the enclosing scope — absorbing it into the next item drops or
    // relocates crate-/module-level attributes (`#![no_std]`,
    // `#![allow(...)]`) when that item is deleted, modified, or
    // duplicated across sides.
    MetadataRule {
        kind: "attribute_item",
        binding: MetadataBinding::Always,
    },
    MetadataRule {
        kind: "line_comment",
        binding: MetadataBinding::RustOuterComment,
    },
    MetadataRule {
        kind: "block_comment",
        binding: MetadataBinding::RustOuterComment,
    },
];

impl LanguageRules for RustRules {
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>> {
        let kind = node.kind();
        match kind {
            "function_item" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Function,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            "function_signature_item" => {
                // Trait method signature without body.
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Method,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            "impl_item" => {
                // Name an impl by `<type>` or `<trait> for <type>` so two
                // impls for the same type but different traits get
                // distinct keys.
                let name = rust_impl_name(source, node)?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Impl,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            "mod_item" => {
                let name = name_from_field(source, node, "name")?;
                // mod may be a header (no body, `mod foo;`) or have a body.
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Module,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            "struct_item" => leaf_item(ItemKind::Struct, source, node, "name"),
            "enum_item" => leaf_item(ItemKind::Enum, source, node, "name"),
            "trait_item" => {
                let name = name_from_field(source, node, "name")?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Trait,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            "union_item" => leaf_item(ItemKind::Struct, source, node, "name"),
            "type_item" => leaf_item(ItemKind::TypeAlias, source, node, "name"),
            "const_item" => leaf_item(ItemKind::Const, source, node, "name"),
            "static_item" => leaf_item(ItemKind::Static, source, node, "name"),
            // Key a `use` / `pub use` by its import-path `argument` (the use
            // tree: `crate::x::Y`, `a::{B, C}`, `a::*`, `a::B as C`, …),
            // whitespace-stripped so cosmetic reformatting doesn't split
            // identity. Visibility is intentionally NOT part of the key: two
            // sides adding the same path with divergent visibility
            // (`pub use a::B` vs `use a::B`) share a key and surface as an
            // add/add conflict, while disjoint paths get distinct keys and
            // union cleanly (HeddleCo/heddle#468). A missing `argument`
            // (malformed) falls through to the unclassified walker, leaving
            // the `use` in inter-item content as before.
            "use_declaration" => {
                let argument = node.child_by_field_name("argument")?;
                let name = strip_whitespace(&source[argument.byte_range()]);
                Some(Classified {
                    kind: ItemKind::Use,
                    name,
                    container_body: None,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            _ => None,
        }
    }

    fn leading_metadata_kinds(&self) -> &'static [MetadataRule] {
        RUST_METADATA
    }

    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64 {
        generic_signature_hash(language, source, item_node)
    }

    fn extra_scope(&self, language: Language, source: &str, item_node: Node<'_>) -> Vec<String> {
        no_extra_scope(language, source, item_node)
    }
}

/// Name an impl block. Two impls of the same type with different traits must
/// produce different keys, so we include the trait when present:
///   `impl Foo` → `Foo`
///   `impl Trait for Foo` → `Trait for Foo`
fn rust_impl_name(source: &str, node: Node<'_>) -> Option<String> {
    let trait_node = node.child_by_field_name("trait");
    let type_node = node.child_by_field_name("type")?;
    let type_name = source[type_node.byte_range()].to_string();
    let key = if let Some(trait_node) = trait_node {
        format!("{} for {}", &source[trait_node.byte_range()], type_name)
    } else {
        type_name
    };
    // Strip ALL whitespace from the key so cosmetic reformatting around
    // `::`, `<>`, etc. doesn't turn into a "different impl"
    // misclassification (r3 fix `021ed8e`).
    Some(strip_whitespace(&key))
}

// ---------------------------------------------------------------------------
// Python
// ---------------------------------------------------------------------------

pub(super) struct PythonRules;

impl LanguageRules for PythonRules {
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>> {
        let kind = node.kind();
        match kind {
            "function_definition" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Function,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            "class_definition" => {
                let name = name_from_field(source, node, "name")?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Module,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            // tree-sitter Python wraps decorated symbols in
            // `decorated_definition` with children:
            //   * one or more `decorator` nodes (`@foo`, `@bar.baz`, ...)
            //   * a `definition` field pointing at a class_definition or
            //     function_definition
            // Treat the whole wrapper as a leaf item so the decorators
            // are part of the item's byte range. Otherwise the inner def
            // classifies first and the decorators end up as orphaned
            // inter-item content — reorder/delete merges drop or
            // misattach them. Inner classification (name + signature) is
            // copied from the inner definition; container_body is FORCED
            // to None even when the inner is a class, so the decorated
            // class merges as one atomic unit (we lose per-method
            // resolution inside decorated classes, but keep the
            // decorator bound to its class — the simpler trade-off,
            // since reordering a decorated class while editing its
            // methods is rarer than simply moving/deleting the whole
            // decorated symbol).
            "decorated_definition" => {
                let inner = node.child_by_field_name("definition")?;
                let inner_classified = self.classify_node(language, source, inner)?;
                Some(Classified {
                    container_body: None,
                    ..inner_classified
                })
            }
            _ => None,
        }
    }

    fn leading_metadata_kinds(&self) -> &'static [MetadataRule] {
        // Python decorators are absorbed via `decorated_definition` in
        // classify_node; there is no leading-sibling pattern to bind.
        &[]
    }

    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64 {
        generic_signature_hash(language, source, item_node)
    }

    fn extra_scope(&self, language: Language, source: &str, item_node: Node<'_>) -> Vec<String> {
        no_extra_scope(language, source, item_node)
    }
}

// ---------------------------------------------------------------------------
// JavaScript / TypeScript
// ---------------------------------------------------------------------------

pub(super) struct JsTsRules;

static JS_TS_METADATA: &[MetadataRule] = &[
    // In tree-sitter-js/ts the decorator is a sibling of
    // `method_definition` / `class_declaration` inside `class_body`, not
    // a wrapper — so without explicit recognition the decorator stays in
    // inter-item content and reorder / delete / add merges leak it onto
    // the wrong symbol.
    MetadataRule {
        kind: "decorator",
        binding: MetadataBinding::Always,
    },
];

impl LanguageRules for JsTsRules {
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>> {
        let kind = node.kind();
        match kind {
            "function_declaration" | "generator_function_declaration" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Function,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            // `class_declaration` covers concrete classes;
            // `abstract_class_declaration` is the TS-only variant for
            // `abstract class`. `interface_declaration` is the TS
            // interface container. All three carry a `name` and a body
            // that holds methods we want extracted as per-method items —
            // without explicit classification their bodies extract zero
            // items and the whole container routes through whole-file
            // text-merge (Codex r8 P2, cid 3256283862).
            "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
                let name = name_from_field(source, node, "name")?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Module,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            // `method_definition` is the concrete class/object method
            // (with body). `method_signature` and `abstract_method_signature`
            // are TS-only body-less declarations inside interfaces and
            // abstract classes respectively. They share the same `name`
            // and `parameters` field shape, so the same key-derivation
            // applies — abstract methods just don't carry a body but
            // remain identifiable by (name, parameter signature).
            "method_definition" | "method_signature" | "abstract_method_signature" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Method,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            _ => None,
        }
    }

    fn leading_metadata_kinds(&self) -> &'static [MetadataRule] {
        JS_TS_METADATA
    }

    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64 {
        generic_signature_hash(language, source, item_node)
    }

    fn extra_scope(&self, language: Language, source: &str, item_node: Node<'_>) -> Vec<String> {
        no_extra_scope(language, source, item_node)
    }
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

pub(super) struct GoRules;

static GO_METADATA: &[MetadataRule] = &[MetadataRule {
    kind: "comment",
    binding: MetadataBinding::NoBlankLine,
}];

impl LanguageRules for GoRules {
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>> {
        let kind = node.kind();
        match kind {
            "function_declaration" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Function,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            "method_declaration" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                // Receiver type disambiguates two methods with the same
                // name on different receivers — `func (a A) String()`
                // vs `func (b B) String()`. Without it the BTreeMap
                // collapses them and one method is dropped from the
                // merge.
                let extra_scope = self.extra_scope(language, source, node);
                Some(Classified {
                    kind: ItemKind::Method,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope,
                })
            }
            _ => None,
        }
    }

    fn leading_metadata_kinds(&self) -> &'static [MetadataRule] {
        GO_METADATA
    }

    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64 {
        generic_signature_hash(language, source, item_node)
    }

    fn extra_scope(&self, _language: Language, source: &str, item_node: Node<'_>) -> Vec<String> {
        go_receiver_type(source, item_node)
            .map(|t| vec![t])
            .unwrap_or_default()
    }
}

/// Extract the receiver type from a Go `method_declaration` as a
/// whitespace-stripped string (e.g. `"A"`, `"*A"`, `"Foo[T]"`). Returns
/// `None` for non-methods or malformed receivers.
fn go_receiver_type(source: &str, node: Node<'_>) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.children(&mut cursor) {
        if child.kind() == "parameter_declaration"
            && let Some(ty) = child.child_by_field_name("type")
        {
            return Some(strip_whitespace(&source[ty.byte_range()]));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Java
// ---------------------------------------------------------------------------

pub(super) struct JavaRules;

static JAVA_METADATA: &[MetadataRule] = &[
    MetadataRule {
        kind: "marker_annotation",
        binding: MetadataBinding::Always,
    },
    MetadataRule {
        kind: "annotation",
        binding: MetadataBinding::Always,
    },
    // Matches the Rust/Go rule — standalone comments separated by blank
    // lines must NOT migrate with the next method/class during merges.
    MetadataRule {
        kind: "line_comment",
        binding: MetadataBinding::NoBlankLine,
    },
    MetadataRule {
        kind: "block_comment",
        binding: MetadataBinding::NoBlankLine,
    },
];

impl LanguageRules for JavaRules {
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>> {
        let kind = node.kind();
        match kind {
            "method_declaration" | "constructor_declaration" => {
                let name = name_from_field(source, node, "name")?;
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Method,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope: Vec::new(),
                })
            }
            "class_declaration" | "interface_declaration" => {
                let name = name_from_field(source, node, "name")?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Module,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            _ => None,
        }
    }

    fn leading_metadata_kinds(&self) -> &'static [MetadataRule] {
        JAVA_METADATA
    }

    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64 {
        generic_signature_hash(language, source, item_node)
    }

    fn extra_scope(&self, language: Language, source: &str, item_node: Node<'_>) -> Vec<String> {
        no_extra_scope(language, source, item_node)
    }
}

// ---------------------------------------------------------------------------
// C / C++
// ---------------------------------------------------------------------------
//
// CppRules handles BOTH `Language::C` and `Language::Cpp`. The `language`
// parameter threaded through trait methods distinguishes them — needed
// because `namespace_definition` is a C++-only node kind that must not be
// matched against C input even if a future grammar revision exposed it.
//
// All C/C++ machinery (declarator walks, qualified-name resolution,
// template-aware scope stripping) is intentionally local to this section.
// Per the audit's biggest-risk note (HeddleCo/heddle#133), this complexity
// **must not** be lifted into the [`LanguageRules`] trait or shared with
// other languages — its shape is intrinsic to C++ and any generalisation
// attempted alongside this refactor would be regretted.

pub(super) struct CppRules;

impl LanguageRules for CppRules {
    fn classify_node<'a>(
        &self,
        language: Language,
        source: &'a str,
        node: Node<'a>,
    ) -> Option<Classified<'a>> {
        let kind = node.kind();
        match kind {
            "function_definition" => {
                let declarator = node.child_by_field_name("declarator")?;
                let name = c_function_name(source, declarator)?;
                // Out-of-class definitions (`A::foo`, `ns::Foo::bar`)
                // need the qualified scope as part of the key — without
                // it, methods sharing a name across unrelated classes /
                // namespaces collapse to the same ItemKey and the
                // per-side occurrence indexer can pair unrelated
                // functions across sides whenever one side adds or
                // reorders a same-named method (Codex r6 P1 #1).
                let extra_scope = self.extra_scope(language, source, node);
                // C/C++ parameter list lives inside the declarator
                // subtree as a `parameter_list` node — find it for
                // overload disambiguation. Use the structural hash
                // (arity + per-parameter type + per-parameter declarator
                // shape) so a parameter-name rename doesn't split
                // function identity AND so pointer/reference/array/
                // function-pointer modifiers in the declarator field
                // disambiguate `f(int)` vs `f(int*)` (Codex r6 P1 #2).
                //
                // Trailing cv- and ref-qualifiers (`const`, `volatile`,
                // `&`, `&&`) live as CHILDREN of the outer
                // `function_declarator`, alongside `parameters` and
                // `declarator`. Without folding them into the hash,
                // member-function overloads on cv- or ref-qualifier
                // alone (`foo()` vs `foo() const`) collapse to identical
                // signature_hashes (Codex r8 P2, cid 3256283859).
                //
                // `noexcept` is deliberately NOT folded in: C++ does
                // not allow overloading by exception specification, so
                // a noexcept addition/removal is a REDECLARATION of the
                // same function — not a new overload. Including it
                // would split identity across sides whenever noexcept
                // changes, degrading the resolution to delete + add
                // (Codex r9 P1, cid 3256397416).
                let signature_hash = self.signature_hash(language, source, node);
                Some(Classified {
                    kind: ItemKind::Function,
                    name,
                    container_body: None,
                    signature_hash,
                    extra_scope,
                })
            }
            // C++ user-defined-type containers: classify with the type's
            // name as the scope component, walk into the body so
            // per-method items inherit `scope=[ClassName]`. Without
            // this, inline methods inside `class A { void foo() {} }`
            // extract as (Function, "foo", [], _) — identical to inline
            // `foo` in any other class — and the per-side occurrence
            // indexer mis-pairs unrelated functions across sides
            // whenever one side adds or reorders a same-named class
            // (Codex r8 P1, cid 3256283864).
            //
            // Out-of-class definitions (`void A::foo()`) still land in
            // the top-level walker with `extra_scope=["A"]` from
            // `c_function_scope`, producing the same scope `["A"]` —
            // so both forms key identically and merge consistently
            // across refactors that move methods inside/outside class
            // bodies.
            //
            // Anonymous classes / structs / unions (no `name` field)
            // skip classification and fall through to the unclassified
            // walker, contributing empty scope. That keeps existing
            // behavior for anonymous types — their methods are rare
            // and any disambiguation we'd invent would diverge between
            // sides.
            "class_specifier" | "struct_specifier" | "union_specifier" => {
                let name = stripped_name(source, node, "name")?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Module,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            "namespace_definition" if language == Language::Cpp => {
                // Anonymous namespaces (`namespace { ... }`) have no
                // `name` field — fall through to the walker so their
                // contents key at file scope (consistent with C++
                // semantics where anonymous-namespace symbols have
                // internal linkage at translation-unit scope).
                let name = stripped_name(source, node, "name")?;
                let container_body = node.child_by_field_name("body");
                Some(Classified {
                    kind: ItemKind::Module,
                    name,
                    container_body,
                    signature_hash: 0,
                    extra_scope: Vec::new(),
                })
            }
            _ => None,
        }
    }

    fn leading_metadata_kinds(&self) -> &'static [MetadataRule] {
        // C/C++ has no equivalent leading-sibling metadata pattern;
        // doc comments are not part of the AST as siblings.
        &[]
    }

    fn signature_hash(&self, language: Language, source: &str, item_node: Node<'_>) -> u64 {
        let Some(declarator) = item_node.child_by_field_name("declarator") else {
            return 0;
        };
        c_signature_hash(language, source, declarator)
    }

    fn extra_scope(&self, _language: Language, source: &str, item_node: Node<'_>) -> Vec<String> {
        let Some(declarator) = item_node.child_by_field_name("declarator") else {
            return Vec::new();
        };
        c_function_scope(source, item_node, declarator)
    }
}

/// Event surfaced by [`walk_c_declarator`] for each node along the
/// C/C++ declarator chain that carries function-identity signal. The
/// walker hides the chain mechanics (pointer / reference /
/// parenthesized / nested `function_declarator` wrappers, descent
/// through `qualified_identifier.name` and `template_function.name`)
/// and emits only the structural events consumers act on: scope
/// components and the terminal name leaf.
enum DeclaratorEvent<'a> {
    /// A `qualified_identifier`'s `scope` sub-node — one component of
    /// the qualified-name chain. Emitted outermost-first as the walker
    /// descends through nested qualifications.
    Scope(Node<'a>),
    /// The identifier-ish leaf at the bottom of the declarator chain
    /// — the function's plain name (`identifier`, `field_identifier`,
    /// `type_identifier`, `property_identifier`, `operator_name`, or
    /// `destructor_name`). Always the last event emitted on a given
    /// walk.
    Name(Node<'a>),
}

/// Walk a C/C++ `function_declarator`'s declarator chain and invoke
/// `callback` for each structural event affecting function identity.
///
/// Centralises the chain-traversal rules that
/// [`c_function_name`] and [`c_function_scope`] previously duplicated.
/// The two consumers had to keep their `match` arms in lockstep — the
/// r11 finding on PR #114 was exactly a `template_function` arm
/// present on the name side but forgotten on the scope side — so the
/// walker turns that mirror-or-diverge tax into symmetry by
/// construction.
///
/// Traversal rules:
/// * `pointer_declarator`, `reference_declarator`,
///   `parenthesized_declarator`, and nested `function_declarator`
///   wrappers are stripped via their `declarator` field. No event.
/// * `qualified_identifier` emits a [`DeclaratorEvent::Scope`] for its
///   `scope` field (when present) and descends into its `name` field.
/// * `template_function` emits nothing of its own and descends into
///   its `name` field. The grammar can place a `qualified_identifier`
///   underneath `template_function.name` in some shapes, so descending
///   here is required to surface nested qualification (Codex r10 P2,
///   cid 3256487046).
/// * An identifier-ish leaf (`identifier`, `field_identifier`,
///   `type_identifier`, `property_identifier`, `operator_name`,
///   `destructor_name`) emits a single [`DeclaratorEvent::Name`] and
///   terminates the walk.
///
/// Bounded at [`WALK_LIMIT`] iterations so a pathological declarator
/// chain cannot loop. Any unrecognised node kind, missing required
/// field, or walk-limit exhaustion ends the walk silently without
/// emitting a Name event — matching the legacy behaviour of returning
/// `None`/`Vec::new()` on those conditions.
fn walk_c_declarator<'tree, F>(function_declarator: Node<'tree>, mut callback: F)
where
    F: FnMut(DeclaratorEvent<'tree>),
{
    let Some(mut current) = function_declarator.child_by_field_name("declarator") else {
        return;
    };
    for _ in 0..WALK_LIMIT {
        match current.kind() {
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "property_identifier"
            | "operator_name"
            | "destructor_name" => {
                callback(DeclaratorEvent::Name(current));
                return;
            }
            "qualified_identifier" => {
                if let Some(scope_node) = current.child_by_field_name("scope") {
                    callback(DeclaratorEvent::Scope(scope_node));
                }
                let Some(next) = current.child_by_field_name("name") else {
                    return;
                };
                current = next;
            }
            "template_function" => {
                let Some(next) = current.child_by_field_name("name") else {
                    return;
                };
                current = next;
            }
            "pointer_declarator"
            | "reference_declarator"
            | "function_declarator"
            | "parenthesized_declarator" => {
                let Some(next) = current.child_by_field_name("declarator") else {
                    return;
                };
                current = next;
            }
            _ => return,
        }
    }
}

/// Resolve the actual function name from a C/C++ `function_declarator`
/// by running [`walk_c_declarator`] and capturing the terminal
/// [`DeclaratorEvent::Name`]. The walker owns the chain mechanics —
/// wrapper stripping and descent through `qualified_identifier` /
/// `template_function` — so this consumer only converts the leaf node
/// to a `String`.
///
/// Returning the FIRST identifier-ish leaf reached (rather than e.g.
/// `identifier_in_subtree`'s DFS-first match over the whole declarator
/// subtree) avoids the hazard where, for a templated qualified name
/// like `Foo<U>::bar()`, the scope's inner `type_identifier` ("Foo")
/// wins and collapses all methods on the same scope onto `name="Foo"`
/// (Codex r5 P1 #2). The walker yields names regardless of how complex
/// the scope prefix is (`Foo::Bar::baz` → "baz"; `Foo<U>::bar` →
/// "bar"; `ns::operator+` → "operator+"; `Foo::~Foo` → "~Foo").
fn c_function_name(source: &str, function_declarator: Node<'_>) -> Option<String> {
    let mut name = None;
    walk_c_declarator(function_declarator, |event| {
        if let DeclaratorEvent::Name(node) = event {
            name = Some(source[node.byte_range()].to_string());
        }
    });
    name
}

/// Extract the qualified scope chain from a C/C++ `function_declarator`,
/// outermost first. Returns `[]` for unqualified definitions
/// (`void foo()` inside the file scope) and the chain of scope
/// identifiers for out-of-class definitions: `void A::foo()` → `["A"]`,
/// `void ns::A::foo()` → `["ns", "A"]`, `template <T> void Foo<T>::bar()`
/// → `["Foo"]` (whitespace stripped). Whitespace is stripped from each
/// component so cosmetic reformatting (`A :: foo` vs `A::foo`) doesn't
/// produce different keys.
///
/// Template-argument lists on scope components are stripped ONLY when
/// the args are a *usage* of an enclosing `template_declaration`'s
/// parameters — i.e., the args list IS the parameter list spelled
/// back out. `template<class T> void Foo<T>::bar()` strips to
/// `["Foo"]` (the `<T>` references the enclosing template parameter)
/// so it matches the inline form `template<class T> class Foo { void
/// bar() {} };` (Codex r9 P2, cid 3256397418).
///
/// Specialization arguments are NEVER stripped, so distinct
/// specializations get distinct keys:
///   * Explicit specializations defined outside a `template_declaration`
///     or inside `template<>` (empty parameter list) keep their args:
///     `void A<int>::foo()` and `void A<float>::foo()` get
///     `["A<int>"]` vs `["A<float>"]` (Codex r10 P2, cid 3256487042).
///   * Partial specializations under a non-empty `template_declaration`
///     also keep their args because the args are NOT the bare
///     parameter list: `template<class T> void A<T*>::foo()` keeps
///     `["A<T*>"]` distinct from `template<class T> void A<T&>::foo()`'s
///     `["A<T&>"]` (Codex r11 P1 #3, cid 3256623807).
///
/// Traversal is shared with [`c_function_name`] via
/// [`walk_c_declarator`]: pointer / reference / parenthesized / nested
/// `function_declarator` wrappers are stripped, `template_function` is
/// descended through its `name` field (the walker may surface a
/// `qualified_identifier` underneath — Codex r10 P2, cid 3256487046),
/// and each `qualified_identifier`'s `scope` sub-node is surfaced as a
/// [`DeclaratorEvent::Scope`] for this consumer to collect. Sharing
/// the walker eliminates the "forgot to mirror `template_function`"
/// class of bug that previously required this function and
/// [`c_function_name`] to be kept in lockstep (Codex r11 finding).
fn c_function_scope(
    source: &str,
    function_definition: Node<'_>,
    function_declarator: Node<'_>,
) -> Vec<String> {
    let mut scope = Vec::new();
    let param_lists = enclosing_template_param_lists(function_definition, source);
    walk_c_declarator(function_declarator, |event| {
        if let DeclaratorEvent::Scope(node) = event {
            scope.push(scope_component_text(source, node, &param_lists));
        }
    });
    scope
}

/// Render a single scope component as a `String`. Strips the template
/// argument list iff `scope_node` is a `template_type` whose arguments
/// are a parameter usage of some enclosing `template_declaration` —
/// e.g. `Foo<T>` inside `template<class T> ...` collapses to `Foo`.
/// Partial specialization patterns (`A<T*>`, `A<T&>`) and concrete
/// specialization args (`A<int>`) fail the usage test and survive
/// verbatim so distinct specializations get distinct keys.
fn scope_component_text(source: &str, scope_node: Node<'_>, param_lists: &[Vec<String>]) -> String {
    let raw = strip_whitespace(&source[scope_node.byte_range()]);
    if scope_node.kind() != "template_type" || param_lists.is_empty() {
        return raw;
    }
    let Some(name_node) = scope_node.child_by_field_name("name") else {
        return raw;
    };
    let Some(args_node) = scope_node.child_by_field_name("arguments") else {
        return raw;
    };
    if template_args_match_any_param_list(source, args_node, param_lists) {
        strip_whitespace(&source[name_node.byte_range()])
    } else {
        raw
    }
}

/// Collect parameter-name lists from every `template_declaration`
/// enclosing `node`, innermost first. Each list contains the parameter
/// identifiers in declaration order (`class T` → `"T"`, `int N` →
/// `"N"`). Lists are omitted if any parameter's name can't be extracted
/// (variadic packs, template-template params, etc.) — better to
/// over-keep template args than collapse a specialization onto the
/// primary template's scope.
///
/// Multiple enclosing template_declarations matter for member
/// templates defined out-of-class:
/// `template<class T> template<class U> void A<T>::foo()`. The
/// innermost params are `[U]`, but the scope's `<T>` references the
/// outer `[T]`. Stripping must succeed against ANY enclosing
/// template_declaration's parameter list.
fn enclosing_template_param_lists(node: Node<'_>, source: &str) -> Vec<Vec<String>> {
    let mut lists = Vec::new();
    let mut current = node;
    for _ in 0..MAX_PARENT_WALK_DEPTH {
        let Some(parent) = current.parent() else {
            break;
        };
        if parent.kind() == "template_declaration"
            && let Some(params_node) = parent.child_by_field_name("parameters")
        {
            let mut names = Vec::new();
            let mut cursor = params_node.walk();
            let mut all_named = true;
            for child in params_node.named_children(&mut cursor) {
                match template_param_name(source, child) {
                    Some(n) => names.push(n),
                    None => {
                        all_named = false;
                        break;
                    }
                }
            }
            if all_named && !names.is_empty() {
                lists.push(names);
            }
        }
        current = parent;
    }
    lists
}

/// Extract the name of a single `template_parameter_list` child.
/// Handles `type_parameter_declaration` (`class T` / `typename T` →
/// `"T"`), `variadic_type_parameter_declaration` (`class... Ts` →
/// `"Ts"`, whose `class` keyword and `...` punctuation are anonymous
/// so the trailing `type_identifier` falls out of the default scan),
/// `parameter_declaration` for non-type parameters (`int N` → `"N"`),
/// and `template_template_parameter_declaration`
/// (`template<...> class Tmpl` → `"Tmpl"`) by descending into the
/// trailing inner declaration; the leading inner
/// `template_parameter_list` belongs to `Tmpl`'s own template header
/// and must not contribute to the outer name (Codex r12 audit
/// pre-fix B).
///
/// Falls back to the LAST `identifier`-or-`type_identifier`
/// named-child's text. Returns `None` for shapes we still don't
/// recognise (e.g. parameter packs of template-template params) so
/// the caller bails out conservatively and the enclosing
/// `template_declaration`'s param list is dropped from matching.
fn template_param_name(source: &str, param: Node<'_>) -> Option<String> {
    // Template-template parameters wrap the declared name inside a
    // trailing nested declaration node; the leading
    // `template_parameter_list` is the inner-template header (not
    // the parameter name) and must be skipped.
    if param.kind() == "template_template_parameter_declaration" {
        let mut cursor = param.walk();
        let last_decl = param
            .named_children(&mut cursor)
            .filter(|c| {
                matches!(
                    c.kind(),
                    "type_parameter_declaration"
                        | "variadic_type_parameter_declaration"
                        | "template_template_parameter_declaration"
                )
            })
            .last();
        return last_decl.and_then(|n| template_param_name(source, n));
    }
    let mut last = None;
    let mut cursor = param.walk();
    for child in param.named_children(&mut cursor) {
        if matches!(child.kind(), "identifier" | "type_identifier") {
            last = Some(child);
        }
    }
    last.map(|n| strip_whitespace(&source[n.byte_range()]))
}

/// True iff every named child of `args` (a `template_argument_list`)
/// is a bare parameter-name reference AND that sequence of references
/// equals some enclosing template_declaration's parameter list in
/// order. A `type_descriptor` whose only named child is a single
/// `type_identifier` counts as a bare reference; anything with a
/// pointer/reference/array/function-pointer wrapper or non-type-
/// descriptor shape (literal, template_type, etc.) is treated as a
/// specialization pattern and short-circuits to false.
fn template_args_match_any_param_list(
    source: &str,
    args: Node<'_>,
    param_lists: &[Vec<String>],
) -> bool {
    let mut arg_names = Vec::new();
    let mut cursor = args.walk();
    for child in args.named_children(&mut cursor) {
        let Some(name) = parameter_usage_arg_name(source, child) else {
            return false;
        };
        arg_names.push(name);
    }
    param_lists.contains(&arg_names)
}

/// If `arg` is a bare parameter-name reference inside a
/// `template_argument_list`, return that name. A bare reference is a
/// `type_descriptor` whose only named child is a single
/// `type_identifier` — meaning the argument is just `T`, with no
/// pointer / reference / array / function-pointer wrappers. Any other
/// shape (specialization pattern like `T*`, concrete type like `int`,
/// nested template like `Foo<T>`, non-type literal like `5`) yields
/// `None`.
///
/// Variadic parameter packs at the use site (`Ts...`) parse as
/// `parameter_pack_expansion` wrapping the pattern (typically a
/// `type_descriptor`); recurse on the pattern field so a pack
/// usage of an enclosing `class... Ts` reads as a bare parameter
/// usage of `Ts` — matching the param-list name and letting
/// `c_function_scope` strip the args (Codex r12 audit pre-fix A).
fn parameter_usage_arg_name(source: &str, arg: Node<'_>) -> Option<String> {
    if arg.kind() == "parameter_pack_expansion" {
        let pattern = arg.child_by_field_name("pattern")?;
        return parameter_usage_arg_name(source, pattern);
    }
    if arg.kind() != "type_descriptor" {
        return None;
    }
    let mut cursor = arg.walk();
    let mut only: Option<Node<'_>> = None;
    let mut count = 0usize;
    for child in arg.named_children(&mut cursor) {
        count += 1;
        only = Some(child);
        if count > 1 {
            return None;
        }
    }
    let only = only?;
    if only.kind() != "type_identifier" {
        return None;
    }
    Some(strip_whitespace(&source[only.byte_range()]))
}

/// Combined signature hash for a C/C++ outer `function_declarator`:
/// mixes the parameter-list hash with the trailing cv- and ref-
/// qualifiers (`type_qualifier`, `ref_qualifier`) that live as direct
/// children of the declarator.
///
/// The parameter list is resolved by walking down the declarator
/// subtree to the function_declarator that carries the actual name —
/// mirroring `c_function_name` — then taking its `parameters` field.
/// DFS-finding the first `parameter_list` in the subtree (the r9
/// implementation) picks the wrong list when the qualified scope's
/// template arguments contain function-pointer types whose abstract
/// declarators have their own parameter_list (e.g.
/// `A<int(*)(double)>::foo(int x)` would hash `(double)` instead of
/// `(int x)`), collapsing overloads onto identical signature hashes
/// (Codex r10 P2, cid 3256487049). Going through the named
/// function_declarator's field accessor avoids the DFS hazard
/// entirely.
///
/// Trailing-return-type, `virtual`, and `noexcept` are deliberately
/// NOT mixed in: none of them change overload identity. `noexcept`
/// in particular is metadata — C++ does NOT allow overloading by
/// exception specification, so `foo()` and `foo() noexcept` are
/// REDECLARATIONS of the same function. Including it would split
/// identity across sides whenever noexcept is added/removed and
/// degrade the resolution to delete + add (Codex r9 P1, cid
/// 3256397416). It also incidentally avoids the parameter-name
/// leakage hazard from conditional `noexcept(noexcept(x.foo()))`
/// clauses where parameter names appear in the hashed text (Codex
/// r9 P2, cid 3256397421).
///
/// Source spelling is hashed after whitespace stripping so cosmetic
/// reformatting (`foo() const` vs `foo()  const`) doesn't split keys.
fn c_signature_hash(language: Language, source: &str, declarator: Node<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let name_fd = c_name_bearing_function_declarator(declarator);
    let param_hash = name_fd
        .and_then(|fd| fd.child_by_field_name("parameters"))
        .map(|n| signature_hash_from_parameter_list(language, source, n))
        .unwrap_or(0);
    param_hash.hash(&mut hasher);
    // cv-/ref-qualifiers live on the OUTER function_declarator
    // alongside `parameters` and `declarator`. For
    // function-returning-function-pointer shapes the outer
    // function_declarator's qualifiers belong to the OUTER (return)
    // function, not the one being defined; we want the qualifiers on
    // the same function_declarator we took parameters from, so walk
    // its direct children.
    if let Some(fd) = name_fd {
        let mut cursor = fd.walk();
        for child in fd.children(&mut cursor) {
            match child.kind() {
                "type_qualifier" | "ref_qualifier" => {
                    b"@".hash(&mut hasher);
                    child.kind().hash(&mut hasher);
                    strip_whitespace(&source[child.byte_range()]).hash(&mut hasher);
                }
                _ => {}
            }
        }
    }
    hasher.finish()
}

/// Walk down the declarator chain to the function_declarator whose
/// declarator field eventually resolves to the actual function name
/// (identifier-ish leaf). Mirrors `c_function_name`'s walk through
/// pointer/reference/parenthesized wrappers and through
/// `qualified_identifier.name` / `template_function.name`. Returns
/// the deepest `function_declarator` encountered on the path.
///
/// For `int* f()` (pointer return wrapping the function): outer
/// pointer_declarator → function_declarator → identifier; returns the
/// function_declarator.
///
/// For `int (*f(int x))(double)` (function returning function pointer):
/// outer function_declarator (parameters `(double)`) →
/// parenthesized_declarator → pointer_declarator → inner
/// function_declarator (parameters `(int x)`) → identifier "f". The
/// inner function_declarator is the name-bearing one, so its
/// `(int x)` parameters are returned — not the outer's `(double)`.
///
/// For `void A<int(*)(double)>::foo(int x)` (templated scope with
/// function-pointer argument): outer function_declarator (parameters
/// `(int x)`) → qualified_identifier → identifier. The
/// parameter_list inside the scope's template_argument_list is never
/// visited, so it can't be picked up by mistake.
fn c_name_bearing_function_declarator(declarator: Node<'_>) -> Option<Node<'_>> {
    let mut current = declarator;
    let mut last_fd: Option<Node<'_>> = None;
    for _ in 0..WALK_LIMIT {
        match current.kind() {
            "function_declarator" => {
                last_fd = Some(current);
                let Some(next) = current.child_by_field_name("declarator") else {
                    return last_fd;
                };
                current = next;
            }
            "pointer_declarator" | "reference_declarator" | "parenthesized_declarator" => {
                let Some(next) = current.child_by_field_name("declarator") else {
                    return last_fd;
                };
                current = next;
            }
            "qualified_identifier" | "template_function" => {
                let Some(next) = current.child_by_field_name("name") else {
                    return last_fd;
                };
                current = next;
            }
            _ => return last_fd,
        }
    }
    last_fd
}

/// Emit a name-stripped canonical shape for a C/C++ parameter
/// declarator. Pointer / reference / array / function-pointer wrappers
/// contribute single-character symbols; identifier leaves (the
/// parameter name, when present) are dropped so a name-only rename
/// doesn't change the hash. Abstract and named declarator variants
/// (`int*` vs `int* p`) collapse to the same shape — they describe
/// identical parameter types.
///
/// Examples:
/// * `int x` (declarator: identifier) → ""
/// * `int* p` / `int*` → "*"
/// * `const T& r` → "&"
/// * `int** pp` → "**"
/// * `int (*fp)(int)` → "*()" (function-pointer wrapper around a
///   pointer wrapper)
/// * `T arr[]` → "[]"
///
/// Unknown declarator kinds emit a `<kind>` token verbatim so we don't
/// silently collapse distinctions in rarer shapes (operator overloads
/// with reference-qualifiers, structured-binding declarators, etc.).
fn emit_c_declarator_shape(node: Node<'_>, out: &mut String) {
    match node.kind() {
        // Name leaves — strip across both named and abstract forms.
        "identifier" | "field_identifier" | "type_identifier" => {}
        "pointer_declarator" | "abstract_pointer_declarator" => out.push('*'),
        "reference_declarator" | "abstract_reference_declarator" => out.push('&'),
        "array_declarator" | "abstract_array_declarator" => out.push_str("[]"),
        "function_declarator" | "abstract_function_declarator" => out.push_str("()"),
        // Pass-through wrappers — no symbol of their own, just recurse.
        "parenthesized_declarator" | "abstract_parenthesized_declarator" => {}
        // Unknown shape — include verbatim so we don't lose signal.
        k => {
            out.push('<');
            out.push_str(k);
            out.push('>');
        }
    }
    // Recurse into NAMED children so identifier leaves can be stripped
    // by the leaf-clause above. Anonymous punctuation (`*`, `&`, etc.)
    // is excluded from named-children iteration.
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        emit_c_declarator_shape(child, out);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers used by multiple LanguageRules impls
// ---------------------------------------------------------------------------

/// Classify a leaf item (no container body, no parameter signature, no extra
/// scope) — the shared shape of Rust `struct` / `enum` / `union` / `type` /
/// `const` / `static`. `name_field` names the tree-sitter field carrying the
/// declared name.
fn leaf_item<'a>(
    kind: ItemKind,
    source: &'a str,
    node: Node<'a>,
    name_field: &str,
) -> Option<Classified<'a>> {
    let name = name_from_field(source, node, name_field)?;
    Some(Classified {
        kind,
        name,
        container_body: None,
        signature_hash: 0,
        extra_scope: Vec::new(),
    })
}

fn name_from_field(source: &str, node: Node<'_>, field: &str) -> Option<String> {
    let name_node = node.child_by_field_name(field)?;
    Some(source[name_node.byte_range()].to_string())
}

fn stripped_name(source: &str, node: Node<'_>, field: &str) -> Option<String> {
    name_from_field(source, node, field).map(|n| strip_whitespace(&n))
}

/// Drop all Unicode whitespace from `s`, preserving every other byte.
/// Cosmetic reformatting that only adds/removes whitespace becomes
/// invisible to the identity comparison; punctuation that semantically
/// distinguishes spellings (`*A` vs `A`, `Foo[T]` vs `Foo`) is retained.
fn strip_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Hash the parameter list at field `field`, keying on arity + types
/// only. Returns 0 when the field is absent (e.g. parameterless
/// declarations).
fn signature_hash_from_field(language: Language, source: &str, node: Node<'_>, field: &str) -> u64 {
    let Some(params) = node.child_by_field_name(field) else {
        return 0;
    };
    signature_hash_from_parameter_list(language, source, params)
}

/// Hash a parameter-list node by arity + per-parameter type spelling,
/// IGNORING parameter names. A pure parameter rename (`foo(x: u32)` →
/// `foo(y: u32)`) must NOT change the hash — otherwise the renamed
/// function gets a different `ItemKey.signature_hash` from base, the
/// merger treats it as delete+add, and a disjoint body change on the
/// other side surfaces as a modify/delete conflict instead of merging
/// cleanly (Codex r5 P1 #1).
///
/// The walk is uniform across languages: for each NAMED child of the
/// parameter-list (anonymous punctuation `(`, `)`, `,` is skipped
/// because tree-sitter anonymous nodes are excluded from named-children
/// iteration), look for a `type` field. Hash its whitespace-stripped
/// spelling when present, else a placeholder so untyped parameters
/// still contribute to arity. The parameter node KIND is also mixed in
/// so syntactically-distinct parameter classes (TypeScript
/// `required_parameter` vs `optional_parameter` vs Python
/// `default_parameter`) don't collapse on identical type field text —
/// `foo(x: number)` and `foo(x?: number)` are different overload
/// declarations. Arity is mixed in at the end so `foo(x: u32)` and
/// `foo(x: u32, y: u32)` don't collide.
///
/// For C/C++ the parameter `type` field carries only the type
/// specifier (`int`, `T`, `Foo`). Pointer / reference / array /
/// function-pointer modifiers and cv-qualifiers live in the
/// `declarator` field alongside the parameter name, so a name-stripped
/// declarator shape is mixed in too — without it, `f(int)`, `f(int*)`,
/// `f(int&)`, `f(int[])` all collapse to the same hash (Codex r6 P1
/// #2).
///
/// Per-language notes on the `type` field:
/// * Rust `parameter` has `type`; `self_parameter` does not — hashed
///   as the placeholder (consistent across sides).
/// * Python `typed_parameter` / `typed_default_parameter` have `type`;
///   bare `identifier` / `default_parameter` (untyped) hash as the
///   placeholder.
/// * TypeScript `required_parameter` / `optional_parameter` have
///   `type`; plain JavaScript parameters don't (placeholder).
/// * Java `formal_parameter` and Go `parameter_declaration` always
///   have `type`.
/// * C/C++ `parameter_declaration` has `type` (the type specifier);
///   modifiers come from the declarator shape, not the `type` field.
fn signature_hash_from_parameter_list(language: Language, source: &str, params: Node<'_>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut cursor = params.walk();
    let mut arity: u64 = 0;
    let is_c_family = matches!(language, Language::C | Language::Cpp);
    for child in params.named_children(&mut cursor) {
        if child.kind() == "comment" {
            continue;
        }
        arity += 1;
        // Parameter NODE KIND distinguishes `required_parameter` from
        // `optional_parameter` etc. — same type field text, different
        // overload identity.
        child.kind().hash(&mut hasher);
        b":".hash(&mut hasher);
        let type_text = child
            .child_by_field_name("type")
            .map(|t| strip_whitespace(&source[t.byte_range()]))
            .unwrap_or_else(|| "_".to_string());
        type_text.hash(&mut hasher);
        if is_c_family {
            b"@".hash(&mut hasher);
            let mut shape = String::new();
            if let Some(decl) = child.child_by_field_name("declarator") {
                emit_c_declarator_shape(decl, &mut shape);
            }
            shape.hash(&mut hasher);
        }
        // Separator so `foo(ab, c)` and `foo(a, bc)` don't collide on
        // concatenated type spellings.
        b"|".hash(&mut hasher);
    }
    arity.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    //! Discipline-guard tests for [`LanguageRules`] implementations.
    //!
    //! These tests do not exercise any merge behaviour. They pin two
    //! invariants per language so that future additions don't silently
    //! skip a decision axis (heddle#133 audit motivation):
    //!
    //! 1. Every non-[`Language::Unknown`] variant resolves to a
    //!    rules object via [`rules_for`]. The match in
    //!    [`leading_metadata_kinds_per_language_is_exhaustive`] is
    //!    exhaustive over the [`Language`] enum, so adding a new
    //!    variant without a [`rules_for`] arm is a compile error.
    //! 2. Each language's [`LanguageRules::leading_metadata_kinds`] is
    //!    snapshotted to its current shape. An empty slice means
    //!    "this language genuinely has no leading-sibling metadata
    //!    pattern" (Python, C, C++) rather than "we forgot" — a future
    //!    agent adding a language and leaving the slice empty by
    //!    accident has no snapshot to compare against and must
    //!    confront the omission explicitly.
    use super::*;

    fn kinds_set(rules: &dyn LanguageRules) -> std::collections::BTreeSet<&'static str> {
        rules
            .leading_metadata_kinds()
            .iter()
            .map(|r| r.kind)
            .collect()
    }

    #[test]
    fn rust_leading_metadata_includes_attributes_and_outer_comments() {
        let rules = rules_for(Language::Rust).expect("rust rules registered");
        let kinds = kinds_set(rules);
        assert!(kinds.contains("attribute_item"));
        assert!(kinds.contains("line_comment"));
        assert!(kinds.contains("block_comment"));
        assert_eq!(rules.leading_metadata_kinds().len(), 3);
    }

    #[test]
    fn python_leading_metadata_is_empty() {
        // Python decorators are absorbed via `decorated_definition` in
        // classify_node, not as leading siblings; an empty slice is the
        // correct answer here.
        let rules = rules_for(Language::Python).expect("python rules registered");
        assert!(rules.leading_metadata_kinds().is_empty());
    }

    #[test]
    fn javascript_leading_metadata_is_decorator_only() {
        let rules = rules_for(Language::JavaScript).expect("javascript rules registered");
        let kinds = kinds_set(rules);
        assert_eq!(kinds.len(), 1);
        assert!(kinds.contains("decorator"));
    }

    #[test]
    fn typescript_shares_javascript_rules() {
        // JsTsRules covers both Language::JavaScript and Language::TypeScript;
        // the kinds slice must be identical between them.
        let js_rules = rules_for(Language::JavaScript).expect("js rules registered");
        let ts_rules = rules_for(Language::TypeScript).expect("ts rules registered");
        assert_eq!(kinds_set(js_rules), kinds_set(ts_rules));
    }

    #[test]
    fn go_leading_metadata_is_comment_only() {
        let rules = rules_for(Language::Go).expect("go rules registered");
        let kinds = kinds_set(rules);
        assert_eq!(kinds.len(), 1);
        assert!(kinds.contains("comment"));
    }

    #[test]
    fn c_leading_metadata_is_empty() {
        // C has no AST-sibling doc-comment pattern; empty is correct.
        let rules = rules_for(Language::C).expect("c rules registered");
        assert!(rules.leading_metadata_kinds().is_empty());
    }

    #[test]
    fn cpp_leading_metadata_is_empty() {
        // C++ shares CppRules with C; same reasoning.
        let rules = rules_for(Language::Cpp).expect("cpp rules registered");
        assert!(rules.leading_metadata_kinds().is_empty());
    }

    #[test]
    fn java_leading_metadata_includes_annotations_and_comments() {
        let rules = rules_for(Language::Java).expect("java rules registered");
        let kinds = kinds_set(rules);
        assert!(kinds.contains("marker_annotation"));
        assert!(kinds.contains("annotation"));
        assert!(kinds.contains("line_comment"));
        assert!(kinds.contains("block_comment"));
        assert_eq!(rules.leading_metadata_kinds().len(), 4);
    }

    #[test]
    fn unknown_language_has_no_rules() {
        assert!(rules_for(Language::Unknown).is_none());
    }

    #[test]
    fn leading_metadata_kinds_per_language_is_exhaustive() {
        // The match below is the discipline guard. Adding a new
        // `Language` variant without an arm here is a compile error;
        // each arm asserts the expected emptiness of that language's
        // leading-metadata slice. A future agent adding a language
        // MUST decide (empty vs non-empty) and update this test —
        // they cannot silently inherit the default.
        for lang in [
            Language::Rust,
            Language::Python,
            Language::JavaScript,
            Language::TypeScript,
            Language::Go,
            Language::C,
            Language::Cpp,
            Language::Java,
            Language::Unknown,
        ] {
            let expected_empty = match lang {
                Language::Rust => false,
                Language::Python => true,
                Language::JavaScript => false,
                Language::TypeScript => false,
                Language::Go => false,
                Language::C => true,
                Language::Cpp => true,
                Language::Java => false,
                Language::Unknown => continue,
            };
            let rules = rules_for(lang).expect("non-unknown language has rules");
            assert_eq!(
                rules.leading_metadata_kinds().is_empty(),
                expected_empty,
                "leading_metadata_kinds emptiness mismatch for {lang:?}"
            );
        }
    }
}
