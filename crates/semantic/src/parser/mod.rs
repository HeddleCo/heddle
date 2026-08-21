// SPDX-License-Identifier: Apache-2.0
//! Language parsing using tree-sitter.

mod comment_walk;
mod parser_core;
mod parser_deps;
mod parser_language;
mod parser_pool;
mod parser_types;
mod syntax_index;

#[cfg(test)]
mod parser_tests;

pub use comment_walk::{is_comment_node, walk_non_comment_leaves};
pub use parser_core::ParsedFile;
pub use parser_deps::extract_dependencies;
pub use parser_language::Language;
#[cfg(test)]
pub(crate) use parser_pool::{parse_count, reset_parse_count};
pub use parser_types::{CallSite, FunctionDef, Import, ImportKind};
pub use syntax_index::{FunctionRef, ImportRef, SyntaxIndex};
