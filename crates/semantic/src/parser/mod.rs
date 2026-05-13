// SPDX-License-Identifier: Apache-2.0
//! Language parsing using tree-sitter.

mod parser_core;
mod parser_deps;
mod parser_language;
mod parser_types;

#[cfg(test)]
mod parser_tests;

pub use parser_core::ParsedFile;
pub use parser_deps::extract_dependencies;
pub use parser_language::Language;
pub use parser_types::{FunctionDef, Import, ImportKind};