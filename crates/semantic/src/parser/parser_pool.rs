// SPDX-License-Identifier: Apache-2.0
//! Thread-local tree-sitter parser reuse.

#[cfg(test)]
use std::cell::Cell;
use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
};

use tree_sitter::{Parser, Tree as TSTree};

use super::parser_language::Language;

thread_local! {
    static PARSERS: RefCell<HashMap<Language, Parser>> = RefCell::new(HashMap::new());
    #[cfg(test)]
    static PARSE_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_parse_count() {
    PARSE_COUNT.set(0);
}

#[cfg(test)]
pub(crate) fn parse_count() -> usize {
    PARSE_COUNT.get()
}

/// Parse a complete document with the pooled parser for `language`.
///
/// Tree-sitter parsers are stateful. We reset before each unrelated document
/// parse and pass `None` for the old tree because callers do not currently
/// retain an edited old tree that corresponds to `source`.
pub(super) fn parse_fresh(source: &[u8], language: Language) -> Option<TSTree> {
    parse_fresh_bounded(source, language, None)
}

pub(super) fn parse_fresh_bounded(
    source: &[u8],
    language: Language,
    budget: Option<&super::parser_core::ParseBudget>,
) -> Option<TSTree> {
    if budget.is_some_and(|budget| budget.interrupted()) {
        return None;
    }
    let ts_language = language.parser()?;
    PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let parser = match parsers.entry(language) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let mut parser = Parser::new();
                parser.set_language(&ts_language).ok()?;
                entry.insert(parser)
            }
        };
        parser.reset();
        #[cfg(test)]
        PARSE_COUNT.set(PARSE_COUNT.get() + 1);
        match budget {
            None => parser.parse(source, None),
            Some(budget) => {
                let mut progress = |_: &tree_sitter::ParseState| {
                    if budget.interrupted() {
                        std::ops::ControlFlow::Break(())
                    } else {
                        std::ops::ControlFlow::Continue(())
                    }
                };
                parser.parse_with_options(
                    &mut |offset, _| &source[offset..],
                    None,
                    Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
                )
            }
        }
    })
}
