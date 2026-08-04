// SPDX-License-Identifier: Apache-2.0
//! Thread-local tree-sitter parser reuse.

use std::{
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
};

#[cfg(test)]
use std::cell::Cell;

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
        parser.parse(source, None)
    })
}
