// SPDX-License-Identifier: Apache-2.0
//! CI guard for Rust sources that Cargo never compiles.

use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use syn::{Attribute, Item, Meta, parse::Parser, punctuated::Punctuated, spanned::Spanned};
use walkdir::WalkDir;

mod roots;

pub fn run(args: Vec<String>) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!("check-rust-source-reachability: unexpected argument '{arg}'");
    }
    let root = env::current_dir().context("resolve workspace root")?;
    let report = check_workspace(&root)?;

    for path in &report.unreachable {
        eprintln!(
            "::error file={}::unreachable Rust module: no crate root reaches this file via `mod` or `#[path]`",
            display_from(&root, path)
        );
    }
    for blanket in &report.dead_code_blankets {
        eprintln!(
            "::error file={},line={}::blanket dead_code allow hides unused code; remove it and use a documented per-symbol allow only when intentional",
            display_from(&root, &blanket.path),
            blanket.line
        );
    }

    if !report.unreachable.is_empty() || !report.dead_code_blankets.is_empty() {
        bail!(
            "Rust source guard failed: {} unreachable module(s), {} blanket dead_code allow(s)",
            report.unreachable.len(),
            report.dead_code_blankets.len()
        );
    }
    println!(
        "Rust source guard clean: all {} source file(s) across {} crate(s) are reachable; no blanket dead_code allows",
        report.source_count, report.crate_count
    );
    Ok(())
}

#[derive(Debug, Default)]
struct Report {
    unreachable: Vec<PathBuf>,
    dead_code_blankets: Vec<Blanket>,
    source_count: usize,
    crate_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
struct Blanket {
    path: PathBuf,
    line: usize,
}

fn check_workspace(root: &Path) -> Result<Report> {
    let crates_dir = root.join("crates");
    let mut report = Report::default();
    for entry in
        fs::read_dir(&crates_dir).with_context(|| format!("read {}", crates_dir.display()))?
    {
        let crate_dir = entry?.path();
        if crate_dir.is_dir() && crate_dir.join("Cargo.toml").is_file() {
            check_crate(&crate_dir, &mut report)?;
            report.crate_count += 1;
        }
    }
    report.unreachable.sort();
    report
        .dead_code_blankets
        .sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.line.cmp(&b.line)));
    Ok(report)
}

fn check_crate(crate_dir: &Path, report: &mut Report) -> Result<()> {
    let src = crate_dir.join("src");
    if !src.is_dir() {
        return Ok(());
    }
    let sources = rust_sources(&src)?;
    let roots = roots::crate_roots(crate_dir, &src)?;
    let mut reachable = BTreeSet::new();
    for root in roots {
        visit_file(&root, true, &sources, &mut reachable)?;
    }

    for path in &sources {
        let parsed = parse_file(path)?;
        find_dead_code_blankets(path, &parsed.attrs, &parsed.items, report);
    }
    report.source_count += sources.len();
    report
        .unreachable
        .extend(sources.difference(&reachable).cloned());
    Ok(())
}

fn rust_sources(src: &Path) -> Result<BTreeSet<PathBuf>> {
    let mut paths = BTreeSet::new();
    for entry in WalkDir::new(src).follow_links(false) {
        let entry = entry.with_context(|| format!("walk {}", src.display()))?;
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|ext| ext == "rs") {
            paths.insert(
                fs::canonicalize(entry.path())
                    .with_context(|| format!("resolve {}", entry.path().display()))?,
            );
        }
    }
    Ok(paths)
}

fn visit_file(
    path: &Path,
    is_root: bool,
    sources: &BTreeSet<PathBuf>,
    reachable: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let path = fs::canonicalize(path).with_context(|| format!("resolve {}", path.display()))?;
    if !sources.contains(&path) || !reachable.insert(path.clone()) {
        return Ok(());
    }
    let parsed = parse_file(&path)?;
    let module_dir = if is_root || path.file_name().is_some_and(|name| name == "mod.rs") {
        path.parent().context("mod.rs has no parent")?.to_path_buf()
    } else {
        path.with_extension("")
    };
    visit_items(
        &parsed.items,
        path.parent().context("source has no parent")?,
        &module_dir,
        sources,
        reachable,
    )
}

fn visit_items(
    items: &[Item],
    path_base: &Path,
    module_dir: &Path,
    sources: &BTreeSet<PathBuf>,
    reachable: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    for item in items {
        let Item::Mod(module) = item else { continue };
        let name = module.ident.to_string();
        if let Some((_, nested)) = &module.content {
            let nested_dir = module_dir.join(&name);
            visit_items(nested, &nested_dir, &nested_dir, sources, reachable)?;
            continue;
        }
        let paths = declared_paths(&module.attrs);
        if paths.is_empty() {
            visit_file(
                &module_dir.join(format!("{name}.rs")),
                false,
                sources,
                reachable,
            )?;
            visit_file(
                &module_dir.join(name).join("mod.rs"),
                false,
                sources,
                reachable,
            )?;
        } else {
            for declared in paths {
                visit_file(&path_base.join(declared), false, sources, reachable)?;
            }
        }
    }
    Ok(())
}

fn parse_file(path: &Path) -> Result<syn::File> {
    let source = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    syn::parse_file(&source).with_context(|| format!("parse {}", path.display()))
}

fn declared_paths(attrs: &[Attribute]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for attr in attrs {
        collect_declared_paths(&attr.meta, &mut paths);
    }
    paths
}

fn collect_declared_paths(meta: &Meta, paths: &mut Vec<PathBuf>) {
    if meta.path().is_ident("path") {
        if let Meta::NameValue(value) = meta
            && let syn::Expr::Lit(expr) = &value.value
            && let syn::Lit::Str(path) = &expr.lit
        {
            paths.push(PathBuf::from(path.value()));
        }
    } else if meta.path().is_ident("cfg_attr")
        && let Meta::List(list) = meta
        && let Ok(nested) =
            Punctuated::<Meta, syn::Token![,]>::parse_terminated.parse2(list.tokens.clone())
    {
        for meta in nested.iter().skip(1) {
            collect_declared_paths(meta, paths);
        }
    }
}

fn find_dead_code_blankets(
    path: &Path,
    file_attrs: &[Attribute],
    items: &[Item],
    report: &mut Report,
) {
    for attr in file_attrs
        .iter()
        .filter(|attr| allows_dead_code(&attr.meta))
    {
        report.dead_code_blankets.push(Blanket {
            path: path.to_path_buf(),
            line: attr.span().start().line,
        });
    }
    for item in items {
        if let Item::Mod(module) = item {
            for attr in module
                .attrs
                .iter()
                .filter(|attr| allows_dead_code(&attr.meta))
            {
                report.dead_code_blankets.push(Blanket {
                    path: path.to_path_buf(),
                    line: attr.span().start().line,
                });
            }
            if let Some((_, nested)) = &module.content {
                find_dead_code_blankets(path, &[], nested, report);
            }
        }
    }
}

fn allows_dead_code(meta: &Meta) -> bool {
    let Meta::List(list) = meta else { return false };
    let Ok(nested) =
        Punctuated::<Meta, syn::Token![,]>::parse_terminated.parse2(list.tokens.clone())
    else {
        return false;
    };
    if meta.path().is_ident("allow") {
        return nested.iter().any(|meta| meta.path().is_ident("dead_code"));
    }
    meta.path().is_ident("cfg_attr") && nested.iter().skip(1).any(allows_dead_code)
}

fn display_from(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
#[path = "check_rust_source_reachability_tests.rs"]
mod tests;
