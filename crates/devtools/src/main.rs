// SPDX-License-Identifier: Apache-2.0
use std::{env, fs, path::PathBuf};

use anyhow::{Context, Result, bail};

mod asserter;
mod check_atomic_ledger_encapsulation;
mod check_no_silent_default_tree_load;
mod check_oprecord_exhaustiveness;
mod check_snapshot_atomicity;
mod check_verification_owner;
mod fuse_dispatch_bench;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("audit-coverage") => run_audit_coverage(args.collect()),
        Some("check-no-silent-default-tree-load") => {
            check_no_silent_default_tree_load::run(args.collect())
        }
        Some("check-snapshot-atomicity") => check_snapshot_atomicity::run(args.collect()),
        Some("check-atomic-ledger-encapsulation") => {
            check_atomic_ledger_encapsulation::run(args.collect())
        }
        Some("check-oprecord-exhaustiveness") => check_oprecord_exhaustiveness::run(args.collect()),
        Some("check-verification-owner") => check_verification_owner::run(args.collect()),
        Some("fuse-dispatch-bench") => fuse_dispatch_bench::run(args.collect()),
        Some(command) => bail!("unknown command '{command}'"),
        None => bail!("expected a command (for example: audit-coverage)"),
    }
}

/// Audit-coverage gate: parse an `lcov.info` report, aggregate line
/// coverage per workspace crate, and fail when any crate listed in a
/// `--gate <crate>=<pct>` argument falls below its threshold.
///
/// Invocation:
///   heddle-devtools audit-coverage <lcov-path> --gate objects=80 --gate repo=78.66 --gate refs=80
///
/// Used from `.github/workflows/rust-tests.yml` after `cargo llvm-cov`
/// emits `lcov.info`. The gate is per-crate (not workspace-global) so
/// that low-coverage crates can't be masked by high-coverage neighbors.
fn run_audit_coverage(args: Vec<String>) -> Result<()> {
    let mut lcov_path: Option<PathBuf> = None;
    let mut gates: Vec<(String, f64)> = Vec::new();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--gate" => {
                let spec = it
                    .next()
                    .context("--gate expects an argument of the form <crate>=<pct>")?;
                let (krate, pct_str) = spec
                    .split_once('=')
                    .with_context(|| format!("--gate value '{spec}' is not <crate>=<pct>"))?;
                let pct: f64 = pct_str
                    .parse()
                    .with_context(|| format!("--gate threshold '{pct_str}' is not a number"))?;
                if !(0.0..=100.0).contains(&pct) {
                    bail!("--gate threshold {pct} for '{krate}' is outside 0..=100");
                }
                gates.push((krate.to_string(), pct));
            }
            other if other.starts_with("--") => bail!("unknown flag '{other}'"),
            other => {
                if lcov_path.is_some() {
                    bail!("unexpected positional argument '{other}'");
                }
                lcov_path = Some(PathBuf::from(other));
            }
        }
    }

    let lcov_path = lcov_path
        .context("audit-coverage: expected a path to lcov.info as the first positional argument")?;
    if gates.is_empty() {
        bail!(
            "audit-coverage: at least one --gate <crate>=<pct> is required (CI passes objects/repo/refs)"
        );
    }

    let source =
        fs::read_to_string(&lcov_path).with_context(|| format!("read {}", lcov_path.display()))?;
    let coverage = aggregate_per_crate(&source);

    let mut failed: Vec<(String, f64, f64)> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for (krate, threshold) in &gates {
        match coverage.get(krate) {
            Some(stats) if stats.found > 0 => {
                let pct = stats.percent();
                let mark = if pct >= *threshold { "OK  " } else { "FAIL" };
                println!(
                    "{mark} {krate:<20} lines {hit:>6}/{found:<6} = {pct:6.2}%  (>= {threshold:.2}%)",
                    hit = stats.hit,
                    found = stats.found,
                );
                if pct < *threshold {
                    failed.push((krate.clone(), pct, *threshold));
                }
            }
            _ => missing.push(krate.clone()),
        }
    }

    if !missing.is_empty() {
        bail!(
            "audit-coverage: no lines counted for crate(s): {} (lcov SF: paths did not match `crates/<name>/`)",
            missing.join(", ")
        );
    }
    if !failed.is_empty() {
        eprintln!(
            "\naudit-coverage: {} crate(s) below threshold:",
            failed.len()
        );
        for (krate, pct, threshold) in &failed {
            eprintln!("  {krate}: {pct:.2}% < {threshold:.2}%");
        }
        bail!("audit-coverage failed");
    }

    println!(
        "\naudit-coverage: {} crate(s) at or above their per-crate line-coverage threshold.",
        gates.len()
    );
    Ok(())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct LineStats {
    /// Lines executed at least once. lcov spelling: `LH:`.
    hit: u64,
    /// Lines counted for coverage. lcov spelling: `LF:`.
    found: u64,
}

impl LineStats {
    fn percent(&self) -> f64 {
        if self.found == 0 {
            0.0
        } else {
            (self.hit as f64) / (self.found as f64) * 100.0
        }
    }
}

/// Given an absolute or repo-relative path from an lcov `SF:` record,
/// return the owning workspace crate name (i.e. the directory under
/// `crates/`). Returns `None` for files outside `crates/`.
///
/// Matches the workspace-member shape `crates/<name>/<role>/...` where
/// `<role>` is one of `src` / `tests` / `benches` / `examples` — i.e.,
/// the segment-triple that uniquely identifies a Cargo workspace member
/// directory. This rejects three classes of false match:
///
/// - **substring matches** like `.../mycrates/foo.rs` (the segment must
///   be exactly `crates`),
/// - **inner-`crates`-dir matches** like `crates/repo/src/crates/mod.rs`
///   (the inner `crates` segment doesn't have a `<name>/<role>/`
///   triple after it, so it's skipped and `repo` wins), and
/// - **nested-checkout false matches** like `/work/crates/heddle/crates/repo/src/lib.rs`
///   (both `crates` segments exist, but only the second has the
///   `repo/src/` shape, so it wins — `heddle` is rejected because the
///   segment after it is `crates`, not a role dir).
///
/// Walks segments right-to-left so the deepest valid match wins, which
/// is the workspace-member dir for any normal cargo-llvm-cov path.
fn crate_of(path: &str) -> Option<String> {
    const ROLE_DIRS: &[&str] = &["src", "tests", "benches", "examples"];
    let normalized = path.replace('\\', "/");
    let segments: Vec<&str> = normalized.split('/').collect();
    for i in (0..segments.len().saturating_sub(2)).rev() {
        if segments[i] != "crates" {
            continue;
        }
        let name = segments[i + 1];
        if name.is_empty() {
            continue;
        }
        let role = segments[i + 2];
        if ROLE_DIRS.contains(&role) {
            return Some(name.to_string());
        }
    }
    None
}

/// Parse an lcov.info body and return aggregated `LineStats` per
/// workspace crate. Records whose `SF:` path is outside `crates/<x>/`
/// (build scripts, examples at workspace root, etc.) are ignored.
fn aggregate_per_crate(lcov: &str) -> std::collections::HashMap<String, LineStats> {
    let mut out: std::collections::HashMap<String, LineStats> = std::collections::HashMap::new();
    let mut current: Option<String> = None;
    for raw in lcov.lines() {
        let line = raw.trim_end();
        if let Some(path) = line.strip_prefix("SF:") {
            current = crate_of(path);
        } else if line == "end_of_record" {
            current = None;
        } else if let Some(krate) = &current {
            if let Some(rest) = line.strip_prefix("LF:")
                && let Ok(n) = rest.parse::<u64>()
            {
                out.entry(krate.clone()).or_default().found += n;
            } else if let Some(rest) = line.strip_prefix("LH:")
                && let Ok(n) = rest.parse::<u64>()
            {
                out.entry(krate.clone()).or_default().hit += n;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests_coverage {
    use super::*;

    const SAMPLE: &str = "\
TN:
SF:/work/crates/objects/src/lib.rs
LF:100
LH:90
end_of_record
TN:
SF:/work/crates/repo/src/lib.rs
LF:200
LH:120
end_of_record
TN:
SF:/work/crates/repo/src/store.rs
LF:50
LH:40
end_of_record
TN:
SF:/work/crates/refs/src/main.rs
LF:80
LH:72
end_of_record
TN:
SF:/work/build.rs
LF:10
LH:0
end_of_record
";

    #[test]
    fn crate_of_extracts_top_level_crate_dir() {
        assert_eq!(
            crate_of("/work/crates/objects/src/lib.rs").as_deref(),
            Some("objects")
        );
        assert_eq!(
            crate_of("crates/refs/src/store.rs").as_deref(),
            Some("refs")
        );
        assert_eq!(
            crate_of("crates/cli-shared/src/lib.rs").as_deref(),
            Some("cli-shared")
        );
    }

    #[test]
    fn crate_of_returns_none_outside_crates_dir() {
        assert!(crate_of("/work/build.rs").is_none());
        assert!(crate_of("proto/heddle/v1/service.proto").is_none());
        assert!(crate_of("crates/").is_none());
    }

    #[test]
    fn crate_of_matches_only_on_path_segment_boundaries() {
        // Substring match is wrong: a path containing `mycrates/` or
        // `some_crates/` must not be parsed as a crate. Using
        // path-segment-aware matching, only an exact `crates` segment counts.
        assert_eq!(crate_of("/foo/some_crates/bar.rs"), None);
        assert_eq!(crate_of("/foo/mycrates/bar.rs"), None);
        // A real `crates/` parent followed by a directory whose name *contains*
        // `crates` later in the path resolves to the workspace crate, not the
        // confusing inner directory.
        assert_eq!(
            crate_of("crates/repo/src/mycrates/foo.rs").as_deref(),
            Some("repo")
        );
        // Nested checkouts where a parent dir is also literally `crates`
        // resolve to the workspace-member match (the segment whose successor
        // is a role dir like `src`/`tests`).
        assert_eq!(
            crate_of("/home/user/crates/heddle/crates/repo/src/lib.rs").as_deref(),
            Some("repo")
        );
    }

    #[test]
    fn crate_of_skips_inner_crates_dir_inside_workspace_member() {
        // A workspace member that itself happens to have an inner directory
        // literally named `crates/` (e.g., `crates/repo/src/crates/mod.rs`)
        // must not be parsed as crate `mod.rs`. The role-dir requirement
        // (`crates/<name>/<src|tests|...>`) ensures the inner `crates` is
        // skipped and the outer one (with `src/` after) wins.
        assert_eq!(
            crate_of("crates/repo/src/crates/mod.rs").as_deref(),
            Some("repo")
        );
        assert_eq!(
            crate_of("crates/repo/tests/crates/integration.rs").as_deref(),
            Some("repo")
        );
        assert_eq!(
            crate_of("/work/crates/objects/benches/crates/perf.rs").as_deref(),
            Some("objects")
        );
    }

    #[test]
    fn crate_of_returns_none_for_non_workspace_paths_under_crates() {
        // A `crates/<name>/<other>/...` shape where `<other>` isn't a
        // recognized role dir is rejected — typical for generated files
        // (target/, build/) that shouldn't count toward the gate.
        assert_eq!(crate_of("crates/repo/target/debug/build/foo.rs"), None);
        assert_eq!(crate_of("/work/crates/repo/.cargo/config.toml"), None);
    }

    #[test]
    fn aggregate_sums_lines_within_each_crate() {
        let agg = aggregate_per_crate(SAMPLE);
        assert_eq!(
            agg.get("objects").copied(),
            Some(LineStats {
                hit: 90,
                found: 100,
            })
        );
        assert_eq!(
            agg.get("repo").copied(),
            Some(LineStats {
                hit: 160,
                found: 250,
            })
        );
        assert_eq!(
            agg.get("refs").copied(),
            Some(LineStats { hit: 72, found: 80 })
        );
    }

    #[test]
    fn aggregate_ignores_files_outside_crates_dir() {
        let agg = aggregate_per_crate(SAMPLE);
        assert!(!agg.contains_key("build.rs"));
        assert!(!agg.contains_key(""));
        assert!(!agg.contains_key("work"));
    }

    #[test]
    fn line_stats_percent_is_ratio_times_hundred() {
        assert!(
            (LineStats {
                hit: 60,
                found: 100
            }
            .percent()
                - 60.0)
                .abs()
                < 1e-9
        );
        assert!((LineStats { hit: 1, found: 3 }.percent() - 33.333_333_333).abs() < 1e-6);
        assert_eq!(LineStats { hit: 0, found: 0 }.percent(), 0.0);
    }
}
