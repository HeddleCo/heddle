// SPDX-License-Identifier: Apache-2.0
use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

mod asserter;
mod audit_grpc_contract;
mod check_atomic_ledger_encapsulation;
mod check_no_silent_default_tree_load;
mod check_oprecord_exhaustiveness;
mod check_snapshot_atomicity;
mod check_verification_owner;
mod fuse_dispatch_bench;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("grpc-ts" | "web-proto") => run_grpc_ts(args.collect()),
        Some("audit-grpc-contract") => {
            let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .and_then(Path::parent)
                .context("failed to locate workspace root")?;
            audit_grpc_contract::run(workspace_root)
        }
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
        None => bail!("expected a command (for example: grpc-ts)"),
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

fn run_grpc_ts(args: Vec<String>) -> Result<()> {
    let mut check = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check = true,
            other => bail!("grpc-ts: unknown flag '{other}'"),
        }
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .context("failed to locate workspace root")?;
    // Canonical proto source — same path the `heddle-grpc` build
    // script and the descriptor audit read from. Keeping a
    // single source eliminates the drift class that landed stale
    // mirrors under `proto/` (see heddle#71).
    let proto_dir = workspace_root.join("crates/grpc/proto");
    let proto_files = collect_grpc_proto_files(&proto_dir)?;
    let client_dir = workspace_root.join("clients/grpc");
    let output_root = client_dir.join("src/gen");
    let root_index = client_dir.join("src/index.ts");
    let package_json = client_dir.join("package.json");
    let grpc_version = grpc_crate_version(workspace_root)?;
    let expected_root_index = render_ts_root_index(&proto_dir, &proto_files)?;

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let es_plugin = resolve_plugin_path(&client_dir, "PROTOC_GEN_ES", "protoc-gen-es")?;

    if check {
        let temp = tempfile::tempdir().context("failed to create temp directory")?;
        generate_grpc_ts_client(&protoc, &es_plugin, &proto_dir, &proto_files, temp.path())?;
        assert_tree_matches(temp.path(), &output_root)?;
        assert_file_contents(&root_index, &expected_root_index)?;
        assert_package_json_version(&package_json, &grpc_version)?;
        println!(
            "gRPC TypeScript client is up to date at {}",
            output_root.display()
        );
        return Ok(());
    }

    if output_root.exists() {
        fs::remove_dir_all(&output_root).with_context(|| {
            format!(
                "failed to remove stale generated tree '{}'",
                output_root.display()
            )
        })?;
    }
    generate_grpc_ts_client(&protoc, &es_plugin, &proto_dir, &proto_files, &output_root)?;
    fs::write(&root_index, expected_root_index)
        .with_context(|| format!("failed to write '{}'", root_index.display()))?;
    sync_package_json_version(&package_json, &grpc_version)?;
    println!(
        "generated {} from heddle-grpc {}",
        output_root.display(),
        grpc_version
    );
    Ok(())
}

fn proto_namespace(file_stem: &str) -> String {
    file_stem
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .concat()
}

fn render_ts_root_index(proto_dir: &Path, proto_files: &[PathBuf]) -> Result<String> {
    let mut namespaces = Vec::new();
    let mut service_namespaces = Vec::new();
    for relative in proto_files {
        let stem = relative
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("invalid proto filename '{}'", relative.display()))?;
        let namespace = proto_namespace(stem);
        let import_path = format!("./gen/heddle/v1/{stem}_pb.js");
        namespaces.push(format!("export * as {namespace} from \"{import_path}\";"));

        let source_path = proto_dir.join(relative);
        let source = fs::read_to_string(&source_path)
            .with_context(|| format!("failed to read '{}'", source_path.display()))?;
        if source
            .lines()
            .any(|line| line.trim_start().starts_with("service "))
        {
            service_namespaces.push(format!(
                "export * as {namespace}Connect from \"{import_path}\";"
            ));
        }
    }

    Ok(format!(
        "{}\n\n{}\n",
        namespaces.join("\n"),
        service_namespaces.join("\n")
    ))
}

fn assert_file_contents(path: &Path, expected: &str) -> Result<()> {
    let actual =
        fs::read_to_string(path).with_context(|| format!("failed to read '{}'", path.display()))?;
    if actual != expected {
        bail!(
            "{} is out of sync with the canonical proto inventory. Run `npm run --prefix clients/grpc generate`.",
            path.display()
        );
    }
    Ok(())
}

fn collect_grpc_proto_files(proto_dir: &Path) -> Result<Vec<PathBuf>> {
    let source_dir = proto_dir.join("heddle/v1");
    let mut files = Vec::new();
    for entry in fs::read_dir(&source_dir)
        .with_context(|| format!("failed to read proto dir '{}'", source_dir.display()))?
    {
        let path = entry
            .with_context(|| format!("failed to read entry under '{}'", source_dir.display()))?
            .path();
        if path.is_dir() {
            bail!(
                "nested protobuf directory '{}' violates the flat heddle.v1 contract",
                path.display()
            );
        }
        if path.extension().is_some_and(|ext| ext == "proto") {
            files.push(
                path.strip_prefix(proto_dir)
                    .with_context(|| {
                        format!(
                            "proto file '{}' is not under '{}'",
                            path.display(),
                            proto_dir.display()
                        )
                    })?
                    .to_path_buf(),
            );
        }
    }
    files.sort();
    if files.is_empty() {
        bail!("no .proto files found under '{}'", source_dir.display());
    }
    Ok(files)
}

fn grpc_crate_version(workspace_root: &Path) -> Result<String> {
    let manifest_path = workspace_root.join("crates/grpc/Cargo.toml");
    let manifest = fs::read_to_string(&manifest_path)
        .with_context(|| format!("failed to read '{}'", manifest_path.display()))?;
    let value: toml::Value = toml::from_str(&manifest)
        .with_context(|| format!("failed to parse '{}'", manifest_path.display()))?;
    value
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(|version| version.as_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("missing package.version in '{}'", manifest_path.display()))
}

fn generate_grpc_ts_client(
    protoc: &Path,
    es_plugin: &Path,
    proto_dir: &Path,
    proto_files: &[PathBuf],
    output_root: &Path,
) -> Result<()> {
    fs::create_dir_all(output_root).with_context(|| {
        format!(
            "failed to create output directory '{}'",
            output_root.display()
        )
    })?;

    let mut command = Command::new(protoc);
    command
        .arg(format!("--plugin=protoc-gen-es={}", es_plugin.display()))
        .arg(format!("--proto_path={}", proto_dir.display()))
        .arg(format!(
            "--es_out=target=ts,import_extension=js:{}",
            output_root.display()
        ));
    for proto_file in proto_files {
        command.arg(proto_dir.join(proto_file));
    }

    let status = command
        .status()
        .with_context(|| format!("failed to run protoc at '{}'", protoc.display()))?;

    if !status.success() {
        bail!("protoc exited with status {status}");
    }

    Ok(())
}

fn resolve_plugin_path(client_dir: &Path, env_var: &str, binary: &str) -> Result<PathBuf> {
    if let Ok(value) = env::var(env_var) {
        let path = PathBuf::from(value);
        if path.exists() {
            return Ok(path);
        }
        bail!("{env_var} was set, but '{}' does not exist", path.display());
    }

    let candidates = [
        client_dir.join("node_modules/.bin").join(binary),
        client_dir
            .join("node_modules/.bin")
            .join(format!("{binary}.cmd")),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not find {binary} in clients/grpc/node_modules/.bin.\n\
Install client dependencies first (for example: `cd clients/grpc && npm install`) or set {env_var}."
    )
}

fn collect_relative_files(root: &Path) -> Result<BTreeSet<PathBuf>> {
    if !root.exists() {
        bail!("generated tree '{}' does not exist", root.display());
    }
    let mut files = BTreeSet::new();
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.with_context(|| format!("failed to walk '{}'", root.display()))?;
        if entry.file_type().is_file() {
            files.insert(
                entry
                    .path()
                    .strip_prefix(root)
                    .with_context(|| {
                        format!(
                            "walked path '{}' was not under '{}'",
                            entry.path().display(),
                            root.display()
                        )
                    })?
                    .to_path_buf(),
            );
        }
    }
    Ok(files)
}

fn assert_tree_matches(generated_root: &Path, checked_in_root: &Path) -> Result<()> {
    let generated_files = collect_relative_files(generated_root)?;
    let checked_in_files = collect_relative_files(checked_in_root)?;

    if generated_files != checked_in_files {
        let missing: Vec<_> = generated_files.difference(&checked_in_files).collect();
        let stale: Vec<_> = checked_in_files.difference(&generated_files).collect();
        bail!(
            "generated proto tree differs from '{}'. Missing checked-in files: {:?}; stale checked-in files: {:?}. Run `npm run --prefix clients/grpc generate`.",
            checked_in_root.display(),
            missing,
            stale
        );
    }

    for relative in generated_files {
        let generated = generated_root.join(&relative);
        let checked_in = checked_in_root.join(&relative);
        let generated_contents = fs::read(&generated)
            .with_context(|| format!("failed to read generated file '{}'", generated.display()))?;
        let checked_in_contents = fs::read(&checked_in).with_context(|| {
            format!("failed to read checked-in file '{}'", checked_in.display())
        })?;
        if generated_contents != checked_in_contents {
            bail!(
                "generated proto output differs from '{}'. Run `npm run --prefix clients/grpc generate`.",
                checked_in.display()
            );
        }
    }

    Ok(())
}

fn package_json_version(package_json: &Path) -> Result<String> {
    let contents = fs::read_to_string(package_json)
        .with_context(|| format!("failed to read '{}'", package_json.display()))?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse '{}'", package_json.display()))?;
    value
        .get("version")
        .and_then(|version| version.as_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("missing version in '{}'", package_json.display()))
}

fn assert_package_json_version(package_json: &Path, expected: &str) -> Result<()> {
    let actual = package_json_version(package_json)?;
    if actual != expected {
        bail!(
            "{} version is {actual}, but crates/grpc/Cargo.toml is {expected}. Run `npm run --prefix clients/grpc generate`.",
            package_json.display()
        );
    }
    Ok(())
}

fn sync_package_json_version(package_json: &Path, expected: &str) -> Result<()> {
    let contents = fs::read_to_string(package_json)
        .with_context(|| format!("failed to read '{}'", package_json.display()))?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse '{}'", package_json.display()))?;
    let actual = value
        .get("version")
        .and_then(|version| version.as_str())
        .with_context(|| format!("missing version in '{}'", package_json.display()))?;
    if actual == expected {
        return Ok(());
    }

    let needle = format!("\"version\": \"{actual}\"");
    let replacement = format!("\"version\": \"{expected}\"");
    if !contents.contains(&needle) {
        bail!(
            "could not locate version field in '{}' for in-place update",
            package_json.display()
        );
    }
    fs::write(package_json, contents.replacen(&needle, &replacement, 1))
        .with_context(|| format!("failed to write '{}'", package_json.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests_proto_single_source {
    use std::path::PathBuf;

    use super::{collect_grpc_proto_files, render_ts_root_index};

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root from CARGO_MANIFEST_DIR")
            .to_path_buf()
    }

    // Heddle ships one canonical proto tree under
    // `crates/grpc/proto/heddle/v1/`. The historical mirrors at
    // `proto/heddle/v1/` and `proto/proto/heddle/v1/` drifted
    // (missing `RedactionTransfer` before heddle#63 r1).
    #[test]
    fn only_canonical_proto_tree_exists() {
        let root = workspace_root();
        let canonical = root.join("crates/grpc/proto/heddle/v1");
        assert!(
            canonical.join("service.proto").exists(),
            "canonical proto entrypoint missing under {}",
            canonical.display()
        );
        assert!(
            canonical.join("common.proto").exists(),
            "canonical proto shared types missing under {}",
            canonical.display()
        );

        for mirror in ["proto/heddle/v1", "proto/proto/heddle/v1"] {
            let p = root.join(mirror);
            assert!(
                !p.exists(),
                "duplicate proto mirror still present: {} — single-source contract requires {} only",
                p.display(),
                canonical.display()
            );
        }
    }

    #[test]
    fn canonical_entrypoint_imports_every_schema_file() {
        let proto_root = workspace_root().join("crates/grpc/proto");
        let files = collect_grpc_proto_files(&proto_root).expect("collect canonical schemas");
        let entrypoint = std::fs::read_to_string(proto_root.join("heddle/v1/service.proto"))
            .expect("read service.proto");

        for file in files {
            if file.ends_with("service.proto") {
                continue;
            }
            let import = format!("import public \"{}\";", file.display());
            assert!(
                entrypoint.contains(&import),
                "canonical entrypoint is missing {import}"
            );
        }
    }

    #[test]
    fn typescript_root_exports_match_proto_inventory() {
        let root = workspace_root();
        let proto_root = root.join("crates/grpc/proto");
        let files = collect_grpc_proto_files(&proto_root).expect("collect canonical schemas");
        let expected = render_ts_root_index(&proto_root, &files).expect("render root exports");
        let actual = std::fs::read_to_string(root.join("clients/grpc/src/index.ts"))
            .expect("read checked-in root exports");
        assert_eq!(actual, expected);
    }
}
