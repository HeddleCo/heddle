// SPDX-License-Identifier: Apache-2.0
//! Parsed AST data types.

/// A function definition.
#[derive(Clone, Debug, PartialEq)]
pub struct FunctionDef {
    pub name: String,
    pub signature: String,
    pub start_line: usize,
    pub end_line: usize,
    pub content: String,
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
