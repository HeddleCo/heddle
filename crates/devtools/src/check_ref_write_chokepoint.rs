// SPDX-License-Identifier: Apache-2.0
//! Workspace guard against production callers bypassing `commit_and_publish`.

use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use syn::{Attribute, Expr, ImplItemFn, ItemFn, ItemMod, Meta, TraitItemFn, visit::Visit};

use crate::asserter::for_each_rs_file;

mod exemptions;

const WRITERS: &[&str] = &[
    "set_thread",
    "set_thread_cas",
    "write_head",
    "write_head_cas",
    "set_marker_cas",
    "create_marker",
    "delete_thread",
    "delete_thread_cas",
    "delete_marker",
    "delete_marker_cas",
    "set_remote_thread",
    "delete_remote_thread",
    "set_undo_recovery",
    "clear_undo_recovery",
    "update_refs",
];

#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    function: String,
    method: String,
}

fn scan_dirs(dirs: &[PathBuf]) -> Result<Vec<Hit>> {
    let mut found = Vec::new();
    let mut scanned = 0;
    for dir in dirs {
        for_each_rs_file(dir, &mut scanned, is_skipped_path, |path, source| {
            let file =
                syn::parse_file(source).with_context(|| format!("parse {}", path.display()))?;
            let mut visitor = Finder {
                path,
                found: &mut found,
            };
            visitor.visit_file(&file);
            Ok(())
        })?;
    }
    Ok(apply_exemption_budgets(found))
}

fn is_skipped_path(path: &Path) -> bool {
    if path.components().any(|part| {
        matches!(
            part.as_os_str().to_str(),
            Some("tests" | "benches" | "devtools")
        )
    }) {
        return true;
    }
    if path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name == "tests.rs" || name.ends_with("_tests.rs"))
    {
        return true;
    }
    path.components()
        .collect::<Vec<_>>()
        .windows(3)
        .any(|parts| {
            parts[0].as_os_str() == "crates"
                && parts[1].as_os_str() == "refs"
                && parts[2].as_os_str() == "src"
        })
}

fn is_cfg_test(attrs: &[Attribute]) -> bool {
    fn mentions_test(meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => path.is_ident("test"),
            Meta::List(list) => list.tokens.to_string().contains("test"),
            Meta::NameValue(_) => false,
        }
    }
    attrs
        .iter()
        .any(|attr| attr.path().is_ident("cfg") && mentions_test(&attr.meta))
}

struct Finder<'a> {
    path: &'a Path,
    found: &'a mut Vec<Hit>,
}

impl Finder<'_> {
    fn inspect(&mut self, function: &str, block: &syn::Block) {
        let mut collector = Collector::default();
        collector.visit_block(block);
        self.found
            .extend(collector.writes.into_iter().map(|(method, line)| Hit {
                path: self.path.to_path_buf(),
                line,
                function: function.to_string(),
                method,
            }));
    }
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if !is_cfg_test(&node.attrs) {
            syn::visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if !is_cfg_test(&node.attrs) {
            self.inspect(&node.sig.ident.to_string(), &node.block);
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        if !is_cfg_test(&node.attrs) {
            self.inspect(&node.sig.ident.to_string(), &node.block);
        }
    }

    fn visit_trait_item_fn(&mut self, node: &'ast TraitItemFn) {
        if !is_cfg_test(&node.attrs)
            && let Some(block) = &node.default
        {
            self.inspect(&node.sig.ident.to_string(), block);
        }
    }
}

#[derive(Default)]
struct Collector {
    writes: Vec<(String, usize)>,
    refs_aliases: HashSet<String>,
}

impl Collector {
    fn is_refs_handle(&self, expr: &Expr) -> bool {
        let mut current = expr;
        loop {
            match current {
                Expr::Field(field) => {
                    return matches!(
                        &field.member,
                        syn::Member::Named(ident) if ident == "refs"
                    );
                }
                Expr::MethodCall(call) => return call.method == "refs",
                Expr::Path(path) => {
                    return path.path.get_ident().is_some_and(|ident| {
                        ident == "refs" || self.refs_aliases.contains(&ident.to_string())
                    });
                }
                Expr::Reference(value) => current = &value.expr,
                Expr::Try(value) => current = &value.expr,
                Expr::Paren(value) => current = &value.expr,
                Expr::Group(value) => current = &value.expr,
                Expr::Await(value) => current = &value.base,
                _ => return false,
            }
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_local(&mut self, node: &'ast syn::Local) {
        if let Some(init) = &node.init
            && self.is_refs_handle(&init.expr)
            && let syn::Pat::Ident(ident) = &node.pat
            && ident.subpat.is_none()
        {
            self.refs_aliases.insert(ident.ident.to_string());
        }
        syn::visit::visit_local(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let method = node.method.to_string();
        if WRITERS.contains(&method.as_str()) && self.is_refs_handle(&node.receiver) {
            self.writes.push((method, node.method.span().start().line));
        }
        syn::visit::visit_expr_method_call(self, node);
    }
}

fn apply_exemption_budgets(hits: Vec<Hit>) -> Vec<Hit> {
    let mut used = BTreeMap::<(String, String, String), usize>::new();
    hits.into_iter()
        .filter(|hit| {
            let key = (
                path_key(&hit.path).to_string(),
                hit.function.clone(),
                hit.method.clone(),
            );
            let count = used.entry(key).or_default();
            let budget = exemptions::budget(&hit.path, &hit.function, &hit.method);
            *count += 1;
            *count > budget
        })
        .collect()
}

fn path_key(path: &Path) -> &str {
    exemptions::path_key(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn planted_cross_crate_bypass_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("bypass.rs"),
            "fn bypass(&self) { self.refs.set_thread(&thread, &state).unwrap(); }",
        )
        .expect("write fixture");
        let hits = scan_dirs(&[dir.path().to_path_buf()]).expect("scan fixture");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "set_thread");
    }

    #[test]
    fn planted_refs_parameter_bypass_is_detected() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(
            dir.path().join("bypass.rs"),
            "fn bypass(refs: &RefManager) { refs.write_head(&head).unwrap(); }",
        )
        .expect("write fixture");
        let hits = scan_dirs(&[dir.path().to_path_buf()]).expect("scan fixture");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].method, "write_head");
    }

    #[test]
    fn production_tree_has_no_bare_ref_writes() {
        let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("devtools lives under crates/")
            .to_path_buf();
        let hits = scan_dirs(&[crates]).expect("scan workspace");
        let first = hits.first().map(|hit| {
            format!(
                "{}:{} `{}` calls `.{}(...)`",
                hit.path.display(),
                hit.line,
                hit.function,
                hit.method
            )
        });
        assert!(
            hits.is_empty(),
            "{} bare production ref write(s) bypass commit_and_publish; first: {:?}",
            hits.len(),
            first
        );
    }
}
