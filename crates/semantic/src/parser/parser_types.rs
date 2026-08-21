// SPDX-License-Identifier: Apache-2.0
//! Parsed AST data types.

/// A function definition.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub name: String,
    /// Enclosing impl / type / module / receiver, when the parser can
    /// see one. Empty for a file-scope function.
    pub container: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
}

impl FunctionDef {
    /// `container::name`, or the bare name when the parser saw no container.
    pub fn qualified_name(&self) -> String {
        if self.container.is_empty() {
            self.name.clone()
        } else {
            format!("{}::{}", self.container, self.name)
        }
    }

    /// Identity that distinguishes `Foo::run` from `Bar::run` and
    /// overloads that share a bare name. Used as the capture-time
    /// `changed_symbols` key.
    pub fn symbol_identity(&self) -> String {
        let qualified = self.qualified_name();
        if self.signature.is_empty() {
            qualified
        } else {
            format!("{qualified}|{}", self.signature)
        }
    }
}

/// A call expression extracted from the parsed tree, not source text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallSite {
    pub name: String,
    /// Path or receiver segments ahead of the callee (`Bar` in `Bar::run`,
    /// `foo` in `foo.run()`). Empty for a bare `run()`.
    pub qualifier: Vec<String>,
}

/// An import statement.
#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub raw: String,
    pub kind: ImportKind,
}

/// Type of import.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportKind {
    Use,
    ExternCrate,
    Require,
    Import,
}
