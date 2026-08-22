// SPDX-License-Identifier: Apache-2.0
//! Test reachability: fires when no test in the repo statically reaches
//! the changed symbol via tree-sitter call-graph traversal.
//!
//! Pure: walks the `SemanticContext`'s parsed-file map, identifies test
//! functions by language-specific naming heuristics, and BFS through
//! callers. The reason text MUST clarify this is **static reachability via
//! tree-sitter call graph; this is not runtime coverage** — that phrase
//! is asserted in tests.
//!
//! Skips silently when the repo has fewer than the configured minimum
//! number of test functions, since firing on every greenfield repo would
//! be noise.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::Path,
};

use objects::object::{ProducerId, RiskSignal, RiskSignalKind, SignalAnchor, State};
use semantic::{
    Language, SemanticParseCache,
    parser::{CallSite, FunctionDef},
};

use crate::{config::ReviewSignalsConfig, registry::SemanticContext};

const VERSION: u32 = 1;
const MODULE_ID: &str = "test_reachability.tree_sitter";
const REASON_TEXT: &str = "no test reaches this symbol via static reachability via tree-sitter call graph; \
     this is not runtime coverage";

pub fn run(
    _prior: &State,
    new: &State,
    cfg: &ReviewSignalsConfig,
    ctx: &SemanticContext,
) -> Vec<RiskSignal> {
    if !cfg.test_reachability.enabled || !ctx.corpus_complete {
        return Vec::new();
    }
    let computed_at = new
        .authored_at
        .map(|dt| dt.timestamp())
        .unwrap_or_else(|| new.created_at.timestamp());

    let catalog: Vec<CatalogEntry<'_>> = ctx
        .new_functions
        .iter()
        .flat_map(|(path, fns)| {
            fns.iter().map(|def| CatalogEntry {
                path: path.as_path(),
                identity: def.symbol_identity(),
                def,
            })
        })
        .collect();
    let all_fns: HashMap<(&Path, &str), &FunctionDef> = catalog
        .iter()
        .map(|entry| ((entry.path, entry.identity.as_str()), entry.def))
        .collect();

    let mut test_set: HashSet<(&Path, &str)> = HashSet::new();
    for (&key, def) in &all_fns {
        if is_test_function(&def.name, Language::from_path(key.0)) {
            test_set.insert(key);
        }
    }

    if test_set.len() < cfg.test_reachability.min_test_functions_in_repo as usize {
        return Vec::new();
    }

    let containers: HashSet<&str> = catalog
        .iter()
        .map(|entry| entry.def.container.as_str())
        .filter(|container| !container.is_empty())
        .collect();
    let cache = SemanticParseCache::shared();
    let mut callers_of: HashMap<(&Path, &str), Vec<(&Path, &str)>> = HashMap::new();
    let mut silenced_methods: HashSet<String> = HashSet::new();
    for caller in &catalog {
        let caller_key = (caller.path, caller.identity.as_str());
        let calls = calls_in(caller.def, caller.path, cache);
        for call in calls {
            match resolve_call(&call, caller, &catalog, &containers) {
                CallResolve::Hits(callees) => {
                    for callee in callees {
                        if caller.path == callee.path && caller.identity == callee.identity {
                            continue;
                        }
                        callers_of
                            .entry((callee.path, callee.identity.as_str()))
                            .or_default()
                            .push(caller_key);
                    }
                }
                CallResolve::Ambiguous if test_set.contains(&caller_key) => {
                    silenced_methods.insert(call.name.clone());
                }
                CallResolve::Ambiguous | CallResolve::None => {}
            }
        }
    }

    let mut out = Vec::new();
    for entry in &catalog {
        let key = (entry.path, entry.identity.as_str());
        if !ctx.is_emit_target(entry.path, entry.def) {
            continue;
        }
        if test_set.contains(&key) {
            continue;
        }
        if !entry.def.container.is_empty() && silenced_methods.contains(&entry.def.name) {
            continue;
        }
        if !reaches_test(key, &callers_of, &test_set) {
            out.push(RiskSignal {
                kind: RiskSignalKind::TestReachability,
                anchor: SignalAnchor::symbol(entry.path.to_string_lossy(), &entry.def.name),
                reason: REASON_TEXT.to_string(),
                producer: ProducerId::new(MODULE_ID, VERSION),
                computed_at,
                computed_against: Some(new.state_id),
            });
        }
    }
    out
}

struct CatalogEntry<'a> {
    path: &'a Path,
    identity: String,
    def: &'a FunctionDef,
}

enum CallResolve<'a> {
    Hits(Vec<&'a CatalogEntry<'a>>),
    Ambiguous,
    None,
}

fn calls_in(def: &FunctionDef, path: &Path, cache: &SemanticParseCache) -> Vec<CallSite> {
    let language = Language::from_path(path);
    if matches!(language, Language::Unknown) {
        return Vec::new();
    }
    cache
        .parse(&def.content, language)
        .map(|parsed| parsed.extract_own_calls())
        .unwrap_or_default()
}

fn resolve_call<'a>(
    call: &CallSite,
    caller: &CatalogEntry<'a>,
    catalog: &'a [CatalogEntry<'a>],
    containers: &HashSet<&str>,
) -> CallResolve<'a> {
    let named: Vec<&CatalogEntry<'_>> = catalog
        .iter()
        .filter(|entry| entry.def.name == call.name)
        .collect();
    if named.is_empty() {
        return CallResolve::None;
    }
    if call.qualifier.is_empty() {
        return resolve_bare_call(caller, named);
    }
    let joined = call.qualifier.join("::");
    let exact: Vec<&CatalogEntry<'_>> = named
        .iter()
        .copied()
        .filter(|entry| qualifier_matches_container(&entry.def.container, &call.qualifier, &joined))
        .collect();
    if !exact.is_empty() {
        return CallResolve::Hits(exact);
    }
    if known_container(
        &joined,
        call.qualifier.last().map(String::as_str),
        containers,
    ) {
        return CallResolve::None;
    }
    if named.len() == 1 {
        return CallResolve::Hits(named);
    }
    CallResolve::Ambiguous
}

fn resolve_bare_call<'a>(
    caller: &CatalogEntry<'a>,
    named: Vec<&'a CatalogEntry<'a>>,
) -> CallResolve<'a> {
    let same_container: Vec<&CatalogEntry<'_>> = named
        .iter()
        .copied()
        .filter(|entry| entry.def.container == caller.def.container)
        .collect();
    if !same_container.is_empty() {
        return CallResolve::Hits(same_container);
    }
    CallResolve::Hits(
        named
            .into_iter()
            .filter(|entry| entry.def.container.is_empty())
            .collect(),
    )
}

fn qualifier_matches_container(container: &str, qualifier: &[String], joined: &str) -> bool {
    !container.is_empty()
        && (container == joined || qualifier.last().is_some_and(|segment| segment == container))
}

fn known_container(joined: &str, last: Option<&str>, containers: &HashSet<&str>) -> bool {
    containers.contains(joined) || last.is_some_and(|segment| containers.contains(segment))
}

fn reaches_test<'a>(
    start: (&'a Path, &'a str),
    callers_of: &HashMap<(&'a Path, &'a str), Vec<(&'a Path, &'a str)>>,
    test_set: &HashSet<(&Path, &str)>,
) -> bool {
    let mut visited: HashSet<(&Path, &str)> = HashSet::new();
    let mut queue: VecDeque<(&Path, &str)> = VecDeque::new();
    queue.push_back(start);
    visited.insert(start);
    while let Some(node) = queue.pop_front() {
        if test_set.contains(&node) {
            return true;
        }
        if let Some(callers) = callers_of.get(&node) {
            for &caller in callers {
                if visited.insert(caller) {
                    queue.push_back(caller);
                }
            }
        }
    }
    false
}

fn is_test_function(name: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => name.starts_with("test_") || name.ends_with("_test"),
        Language::Python => name.starts_with("test_") || name == "setUp",
        Language::JavaScript | Language::TypeScript => {
            name.starts_with("test") || name.starts_with("it") || name.starts_with("describe")
        }
        Language::Go => name.starts_with("Test"),
        Language::Zig => name.starts_with("test:"),
        _ => false,
    }
}

#[cfg(test)]
#[path = "test_reachability_tests.rs"]
mod tests;
