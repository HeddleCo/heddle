use std::collections::BTreeMap;

use objects::object::{SymbolKindTag, compute_symbol_semantic_hash};
use tree_sitter::Node;

use super::*;
use crate::{
    parser::{ParsedFile, is_comment_node, walk_non_comment_leaves},
    symbol_resolver::visit_definitions,
};

pub(super) type Groups = BTreeMap<(String, String), Vec<Change>>;

pub(super) fn hash(parts: &[&[u8]]) -> ContentHash {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(&(part.len() as u64).to_le_bytes());
        bytes.extend_from_slice(part);
    }
    ContentHash::compute_typed("hd-behavior-v1", &bytes)
}

pub(super) fn tokens(node: Node<'_>, source: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    walk_non_comment_leaves(node, |leaf| {
        let text = &source.as_bytes()[leaf.byte_range()];
        bytes.extend_from_slice(&(text.len() as u32).to_le_bytes());
        bytes.extend_from_slice(text);
    });
    bytes
}

fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| !is_comment_node(n.kind()))
        .collect()
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn spelling(node: Node<'_>, source: &str) -> String {
    let mut result = String::new();
    walk_non_comment_leaves(node, |leaf| result.push_str(text(leaf, source)));
    result
}

struct Builder<'a> {
    parsed: &'a ParsedFile,
    side: Side,
    seed: ContentHash,
    next: u64,
    change: Change,
}

impl Builder<'_> {
    fn id(&mut self, role: &str) -> String {
        self.next += 1;
        hash(&[
            self.seed.as_bytes(),
            role.as_bytes(),
            &self.next.to_le_bytes(),
        ])
        .to_hex()
    }

    fn location(&self, node: Node<'_>) -> Location {
        Location {
            side: self.side,
            span: ByteSpan::new(node.start_byte() as u32, node.end_byte() as u32),
            text: text(node, self.parsed.source()).into(),
        }
    }

    fn limit(&mut self, reason: Limitation) {
        self.change.support = Support::Partial;
        if !self.change.limitations.contains(&reason) {
            self.change.limitations.push(reason);
        }
    }

    fn expression(&mut self, node: Node<'_>) -> String {
        // One source occurrence has one identity even if used in several edges.
        if let Some(existing) = self.change.expressions.iter().find(|e| {
            e.source.span.start as usize == node.start_byte()
                && e.source.span.end as usize == node.end_byte()
        }) {
            return existing.id.clone();
        }
        let id = self.id("expression");
        self.change.expressions.push(Expression {
            id: id.clone(),
            source: self.location(node),
            normalized_hash: hash(&[b"tokens", &tokens(node, self.parsed.source())]),
            conditional: None,
        });
        id
    }

    fn value(&mut self, node: Node<'_>) -> String {
        let id = self.expression(node);
        if node.kind() != "if_expression" {
            if descendants(node).iter().any(|n| {
                matches!(
                    n.kind(),
                    "if_expression"
                        | "match_expression"
                        | "return_expression"
                        | "loop_expression"
                        | "closure_expression"
                        | "macro_invocation"
                        | "block"
                )
            }) {
                self.limit(Limitation::Syntax);
            }
            return id;
        }
        let Some(predicate) = node.child_by_field_name("condition") else {
            self.limit(Limitation::Syntax);
            return id;
        };
        let predicate_id = self.expression(predicate);
        if matches!(predicate.kind(), "let_condition" | "let_chain") {
            self.limit(Limitation::Syntax);
        }
        if predicate.kind() == "identifier" {
            self.resolve(predicate, &predicate_id, 0);
        } else {
            for occurrence in descendants(predicate)
                .into_iter()
                .filter(|n| n.kind() == "identifier")
            {
                match lookup_binding(
                    occurrence,
                    text(occurrence, self.parsed.source()),
                    self.parsed.source(),
                ) {
                    Lookup::Local { .. } => {
                        let id = self.expression(occurrence);
                        self.resolve(occurrence, &id, 0);
                    }
                    Lookup::Unsupported => self.limit(Limitation::Syntax),
                    Lookup::External => {}
                }
            }
        }
        let mut branches = Vec::new();
        for (label, branch) in [
            (BranchLabel::True, node.child_by_field_name("consequence")),
            (
                BranchLabel::False,
                node.child_by_field_name("alternative")
                    .and_then(|n| children(n).first().copied()),
            ),
        ] {
            let Some(branch) = branch else {
                self.limit(Limitation::MissingElse);
                continue;
            };
            let body = children(branch);
            // A sole tail expression: statements, side effects before a tail,
            // else-if, nested conditions and guards need a later extractor.
            let result = if branch.kind() == "block"
                && body.len() == 1
                && !matches!(
                    body[0].kind(),
                    "let_declaration" | "expression_statement" | "empty_statement"
                ) {
                body[0]
            } else {
                self.limit(Limitation::Syntax);
                branch
            };
            if descendants(result).iter().any(|n| {
                matches!(
                    n.kind(),
                    "if_expression"
                        | "match_expression"
                        | "return_expression"
                        | "break_expression"
                        | "continue_expression"
                        | "macro_invocation"
                        | "closure_expression"
                )
            }) {
                self.limit(Limitation::Syntax);
            }
            let result_id = self.expression(result);
            let branch_id = self.id("branch");
            branches.push(Branch {
                id: branch_id,
                label,
                result_id,
                source: self.location(branch),
            });
        }
        if let Some(expression) = self.change.expressions.iter_mut().find(|e| e.id == id) {
            expression.conditional = Some(Conditional {
                predicate_id,
                branches,
            });
        }
        id
    }

    fn resolve(&mut self, occurrence: Node<'_>, occurrence_id: &str, depth: usize) {
        if occurrence.kind() != "identifier" {
            for nested in descendants(occurrence) {
                if matches!(
                    nested.kind(),
                    "if_expression"
                        | "match_expression"
                        | "block"
                        | "closure_expression"
                        | "macro_invocation"
                ) {
                    self.limit(Limitation::Syntax);
                }
                if nested.kind() == "identifier" {
                    match lookup_binding(
                        nested,
                        text(nested, self.parsed.source()),
                        self.parsed.source(),
                    ) {
                        Lookup::Local { .. } => {
                            let id = self.expression(nested);
                            self.resolve(nested, &id, depth);
                        }
                        Lookup::Unsupported => self.limit(Limitation::Syntax),
                        Lookup::External => {}
                    }
                }
            }
            return;
        }
        if self
            .change
            .bindings
            .iter()
            .any(|b| b.occurrence_id == occurrence_id)
        {
            return;
        }
        let name = text(occurrence, self.parsed.source()).to_owned();
        let found = lookup_binding(occurrence, &name, self.parsed.source());
        let mut binding = Binding {
            id: self.id("binding"),
            occurrence_id: occurrence_id.into(),
            name,
            value_id: None,
            declaration: None,
            lexical_scope: None,
            support: Support::Supported,
            limitations: vec![],
        };
        let issue = match found {
            Lookup::Local {
                declaration,
                value,
                scope,
                mutable,
            } => {
                binding.declaration = Some(self.location(declaration));
                // Reuse the semantic index's lexical scope span, not a guessed
                // line range. Some grammar scopes have no semantic scope entry.
                binding.lexical_scope = self
                    .parsed
                    .syntax_index()
                    .semantic_scopes()
                    .iter()
                    .find(|entry| {
                        entry.span.start as usize == scope.start_byte()
                            && entry.span.end as usize == scope.end_byte()
                    })
                    .map(|entry| entry.span);
                if mutable {
                    Some(Limitation::MutableBinding)
                } else if depth >= 16 {
                    Some(Limitation::Budget)
                } else if let Some(value) = value {
                    let value_id = self.expression(value);
                    binding.value_id = Some(value_id.clone());
                    self.resolve(value, &value_id, depth + 1);
                    None
                } else {
                    Some(Limitation::UnresolvedBinding)
                }
            }
            Lookup::Unsupported => Some(Limitation::Syntax),
            Lookup::External => Some(Limitation::UnresolvedBinding),
        };
        if let Some(issue) = issue {
            binding.support = Support::Unsupported;
            binding.limitations.push(issue);
            self.limit(issue);
        }
        self.change.bindings.push(binding);
    }
}

enum Lookup<'a> {
    Local {
        declaration: Node<'a>,
        value: Option<Node<'a>>,
        scope: Node<'a>,
        mutable: bool,
    },
    External,
    Unsupported,
}

fn pattern_mentions(pattern: Node<'_>, name: &str, source: &str) -> bool {
    descendants(pattern)
        .iter()
        .any(|n| n.kind() == "identifier" && text(*n, source) == name)
}

fn lookup_binding<'a>(occurrence: Node<'a>, name: &str, source: &str) -> Lookup<'a> {
    let mut child = occurrence;
    let mut intervening_writes = false;
    while let Some(parent) = child.parent() {
        if parent.kind() == "block" {
            for sibling in children(parent)
                .into_iter()
                .rev()
                .filter(|n| n.end_byte() <= child.start_byte())
            {
                if sibling.kind() == "let_declaration"
                    && let Some(pattern) = sibling.child_by_field_name("pattern")
                    && pattern_mentions(pattern, name, source)
                {
                    if pattern.kind() != "identifier" {
                        return Lookup::Unsupported;
                    }
                    let mutable = children(sibling)
                        .iter()
                        .any(|n| n.kind() == "mutable_specifier")
                        || intervening_writes;
                    return Lookup::Local {
                        declaration: sibling,
                        value: sibling.child_by_field_name("value"),
                        scope: parent,
                        mutable,
                    };
                }
                // Conservative: writes in prior nested control flow may have
                // run. Never substitute a possibly reassigned binding.
                intervening_writes |= descendants(sibling).iter().any(|n| {
                    matches!(
                        n.kind(),
                        "assignment_expression" | "compound_assignment_expr"
                    ) && n
                        .child_by_field_name("left")
                        .is_some_and(|left| pattern_mentions(left, name, source))
                });
            }
        }
        // Parameters and pattern-bound names are barriers, not permission to
        // accidentally resolve an outer homonym. Pattern resolution is future work.
        if matches!(
            parent.kind(),
            "closure_expression" | "for_expression" | "match_arm"
        ) {
            return Lookup::Unsupported;
        }
        if matches!(parent.kind(), "if_expression" | "while_expression")
            && parent.child_by_field_name("condition").is_some_and(|n| {
                matches!(n.kind(), "let_condition" | "let_chain")
                    && pattern_mentions(n, name, source)
            })
        {
            return Lookup::Unsupported;
        }
        if parent.kind() == "function_item" {
            return Lookup::External;
        }
        child = parent;
    }
    Lookup::External
}

fn descendants(root: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        result.push(node);
        stack.extend(children(node).into_iter().rev());
    }
    result
}

pub(super) fn extract(
    path: &str,
    parsed: &ParsedFile,
    side: Side,
    symbols: &[String],
) -> (Groups, bool) {
    let mut groups = Groups::new();
    let source = parsed.source();
    let mut function_ordinal: u64 = 0;
    let mut count = 0;
    let mut exhausted = true;
    let artifact = binding_key(extraction_key(ContentHash::compute_typed(
        "blob",
        source.as_bytes(),
    )));
    visit_definitions(parsed.root_node(), source.as_bytes(), &mut |site| {
        if site.kind != SymbolKindTag::Function {
            return;
        }
        function_ordinal += 1;
        let symbol = SymbolEntry {
            name: site.name,
            kind: site.kind,
            container_path: site.parent_name.map(|s| vec![s]).unwrap_or_default(),
            semantic_hash: compute_symbol_semantic_hash(site.kind, &tokens(site.node, source)),
            span: (site.start_line, site.end_line),
        };
        let address = symbol.address();
        if !symbols.is_empty() && !symbols.contains(&address) {
            return;
        }
        let mut stack = children(site.node);
        stack.reverse();
        let mut ordinal: u64 = 0;
        while let Some(node) = stack.pop() {
            if node.kind() == "function_item" {
                continue;
            }
            let fields = match node.kind() {
                "field_initializer" => Some(("field", "value", AssignmentKind::FieldInitializer)),
                "assignment_expression" => Some(("left", "right", AssignmentKind::Assignment)),
                _ => None,
            };
            if let Some((target_field, value_field, kind)) = fields
                && let (Some(target_node), Some(value)) = (
                    node.child_by_field_name(target_field),
                    node.child_by_field_name(value_field),
                )
            {
                ordinal += 1;
                if count >= MAX_ASSIGNMENTS {
                    exhausted = false;
                    return;
                }
                count += 1;
                let target = if kind == AssignmentKind::FieldInitializer {
                    let owner = node
                        .parent()
                        .and_then(|n| n.parent())
                        .and_then(|n| n.child_by_field_name("name"));
                    match owner {
                        Some(owner) => {
                            format!(
                                "{}.{}",
                                spelling(owner, source),
                                spelling(target_node, source)
                            )
                        }
                        None => text(target_node, source).into(),
                    }
                } else {
                    spelling(target_node, source)
                };
                let seed = hash(&[
                    path.as_bytes(),
                    artifact.as_bytes(),
                    if side == Side::Base {
                        b"base"
                    } else {
                        b"source"
                    },
                    address.as_bytes(),
                    &function_ordinal.to_le_bytes(),
                    &ordinal.to_le_bytes(),
                ]);
                let mut builder = Builder {
                    parsed,
                    side,
                    seed,
                    next: 0,
                    change: Change {
                        id: seed.to_hex(),
                        symbol_address: address.clone(),
                        support: Support::Supported,
                        limitations: vec![],
                        operations: vec![],
                        assignments: vec![],
                        expressions: vec![],
                        bindings: vec![],
                        correspondences: vec![],
                    },
                };
                let operation_id = builder.id("operation");
                let name = site.node.child_by_field_name("name").unwrap_or(site.node);
                builder.change.operations.push(Operation {
                    id: operation_id.clone(),
                    symbol: symbol.clone(),
                    source: builder.location(name),
                });
                let value_id = builder.value(value);
                let id = builder.id("assignment");
                builder.change.assignments.push(Assignment {
                    id,
                    operation_id,
                    target: target.clone(),
                    kind,
                    value_id,
                    source: builder.location(target_node),
                });
                groups
                    .entry((address.clone(), target))
                    .or_default()
                    .push(builder.change);
            }
            stack.extend(children(node).into_iter().rev());
        }
    });
    (groups, exhausted)
}
