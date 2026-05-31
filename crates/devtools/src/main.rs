// SPDX-License-Identifier: Apache-2.0
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

mod check_no_silent_default_tree_load;
mod check_oprecord_exhaustiveness;
mod check_snapshot_atomicity;
mod fuse_dispatch_bench;

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("web-proto") => run_web_proto(args.collect()),
        Some("audit-idempotency") => run_audit_idempotency(),
        Some("audit-coverage") => run_audit_coverage(args.collect()),
        Some("check-no-silent-default-tree-load") => {
            check_no_silent_default_tree_load::run(args.collect())
        }
        Some("check-snapshot-atomicity") => check_snapshot_atomicity::run(args.collect()),
        Some("check-oprecord-exhaustiveness") => {
            check_oprecord_exhaustiveness::run(args.collect())
        }
        Some("fuse-dispatch-bench") => fuse_dispatch_bench::run(args.collect()),
        Some(command) => bail!("unknown command '{command}'"),
        None => bail!("expected a command (for example: web-proto)"),
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

/// Audit-idempotency check: fail when any state-changing RPC's request
/// message lacks `string client_operation_id = 15`. The service.proto
/// comment block explicitly reserves tag 15 for this; this audit is
/// what keeps the convention from rotting.
///
/// Rules the audit applies:
///   1. State-changing RPCs are detected by RPC name prefix
///      (`Update*`, `Push`, `Pull`, `Mint*`, `Issue*`, `Revoke*`,
///      `Rotate*`, `Sign*`, `Begin*` for transactions, `Commit*`,
///      `Abort*`, `Create*`, `Delete*`, `Add*`, `Remove*`,
///      `Approve*`, `Register*`, `Deregister*`, `Resolve*` plus
///      every `Finish*` outside the auth-flow allow-list).
///   2. The auth-flow allow-list (`BeginWebAuthn*`, `BeginDeviceAuth`,
///      `BeginOAuth*`, `GetInvitationSummary`, etc.) is the explicit
///      escape hatch — Begin* RPCs that start a flow rather than
///      mutate persistent state.
///   3. For every state-changing RPC, the request message must
///      declare a field literally `string client_operation_id = 15;`.
///
/// Exits with code 1 (via `bail`) when any rule fires; exit 0 means
/// every state-changing RPC carries the field at the expected tag.
fn run_audit_idempotency() -> Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .context("failed to locate workspace root")?;
    let proto_path = workspace_root.join("crates/grpc/proto/heddle/v1/service.proto");
    let source = fs::read_to_string(&proto_path)
        .with_context(|| format!("read {}", proto_path.display()))?;

    let rpcs = extract_rpcs(&source);
    let messages = extract_messages(&source);

    let mut missing: Vec<String> = Vec::new();
    let mut audited = 0usize;
    for rpc in &rpcs {
        if !is_state_changing(&rpc.name) {
            continue;
        }
        audited += 1;
        // Stream-envelope unwrap: when the request message is a
        // single `oneof body { ... Request first = 1; ... }` envelope
        // (Push/Pull style) the actual operation lives on the inner
        // `Request` variant. Follow into that type and audit it
        // instead — the envelope itself never carries the op-id.
        let target_message = stream_envelope_target(&rpc.request_message, &messages)
            .unwrap_or_else(|| rpc.request_message.clone());
        let body = messages.get(&target_message);
        let body = match body {
            Some(b) => b,
            None => {
                missing.push(format!(
                    "{}::{} -> request message {} not found in proto",
                    rpc.service, rpc.name, target_message
                ));
                continue;
            }
        };
        if !body.contains("string client_operation_id = 15;") {
            missing.push(format!(
                "{}::{} -> {} is missing `string client_operation_id = 15;`",
                rpc.service, rpc.name, target_message
            ));
        }
    }

    if !missing.is_empty() {
        eprintln!(
            "audit-idempotency: {} state-changing RPC(s) missing client_operation_id = 15:",
            missing.len()
        );
        for m in &missing {
            eprintln!("  {m}");
        }
        bail!("audit-idempotency failed");
    }

    println!(
        "audit-idempotency: {} state-changing RPC(s) carry `client_operation_id = 15`.",
        audited
    );
    Ok(())
}

#[derive(Debug)]
struct ProtoRpc {
    service: String,
    name: String,
    request_message: String,
}

/// Tokenize service / rpc declarations. The proto syntax is regular
/// enough that a small line-walker beats pulling in a real parser.
fn extract_rpcs(source: &str) -> Vec<ProtoRpc> {
    let mut out = Vec::new();
    let mut current_service: Option<String> = None;
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("service ") {
            current_service = rest
                .split_whitespace()
                .next()
                .map(|s| s.trim_end_matches('{').to_string());
            continue;
        }
        if trimmed == "}" {
            current_service = None;
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("rpc ")
            && let Some(service) = &current_service
        {
            // Shape: `rpc <Name>(<Req>) returns (<Resp>);`
            // Stream-typed RPCs (`stream Foo`) need the `stream` token
            // stripped before the message name.
            let after_name = rest
                .split_once('(')
                .map(|(name, after)| (name.trim(), after));
            let Some((name, after_paren)) = after_name else {
                continue;
            };
            let req = after_paren
                .split_once(')')
                .map(|(req, _)| req.trim().trim_start_matches("stream").trim());
            let Some(req) = req else { continue };
            out.push(ProtoRpc {
                service: service.clone(),
                name: name.to_string(),
                request_message: req.to_string(),
            });
        }
    }
    out
}

/// Pull the body of every `message X { ... }` block keyed by name.
/// Only top-level messages — nested ones inside a service or another
/// message are ignored, which is fine because the idempotency field
/// always lives at the top level of the request message.
fn extract_messages(source: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let bytes = source.as_bytes();
    let needle = "message ";
    let mut cursor = 0usize;
    while let Some(rel) = source[cursor..].find(needle) {
        let start = cursor + rel;
        // Word-boundary check: skip when preceded by an identifier
        // char (e.g. inside a doc comment text like "the message Foo").
        if start > 0 {
            let prev = bytes[start - 1];
            if prev.is_ascii_alphanumeric() || prev == b'_' {
                cursor = start + needle.len();
                continue;
            }
        }
        let after = start + needle.len();
        let name_end = after
            + source[after..]
                .find(|c: char| c.is_whitespace() || c == '{')
                .unwrap_or(0);
        let name = source[after..name_end].trim();
        let Some(brace_open) = source[name_end..].find('{') else {
            cursor = name_end;
            continue;
        };
        let brace_open = name_end + brace_open;
        let brace_close = match_close_brace(bytes, brace_open).unwrap_or(bytes.len());
        let body = &source[brace_open..brace_close.min(bytes.len())];
        out.insert(name.to_string(), body.to_string());
        cursor = brace_close;
    }
    out
}

/// Detect the "stream envelope" pattern — a message whose body is a
/// single `oneof body { ... }` block whose first variant is named
/// `request` and points at a request-shaped message. When matched,
/// returns the inner request type so the audit can check the op-id
/// field there. Used by `Push`/`Pull` and any future streaming RPCs
/// that follow the same envelope layout.
fn stream_envelope_target(
    name: &str,
    messages: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let body = messages.get(name)?;
    // Quick filter: the envelope body has exactly one `oneof body {`
    // and (effectively) no other field declarations outside it.
    if !body.contains("oneof body") {
        return None;
    }
    // The convention is `<RequestType> request = 1;` as the first
    // variant. Look for that line shape.
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(after) = trimmed.strip_suffix(" request = 1;") {
            return Some(after.trim().to_string());
        }
    }
    None
}

fn match_close_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Decide whether an RPC is state-changing. The state-changing test is
/// a name-prefix heuristic plus an explicit auth-flow allow-list — the
/// proto comment block reserves tag 15 for these.
fn is_state_changing(name: &str) -> bool {
    // Auth-flow `Begin*` RPCs start a challenge; the persistent state
    // change happens in the matching `Finish*` / `Complete*`. Same for
    // `GetInvitationSummary` (read-only public lookup).
    const AUTH_FLOW_BEGIN_ALLOW: &[&str] = &[
        "BeginWebAuthnRegistration",
        "BeginWebAuthnAuthentication",
        "BeginDeviceAuthorization",
        "BeginOAuthLogin",
        "BeginOAuthLink",
        "BeginInvitationFlow",
    ];
    if AUTH_FLOW_BEGIN_ALLOW.contains(&name) {
        return false;
    }
    // Mutating prefixes — order matters only because `Update`/`Create`
    // are common substrings of read-shaped names. Every prefix below
    // is anchored at the start of the RPC name.
    const PREFIXES: &[&str] = &[
        "Update",
        "Push",
        "Pull",
        "Mint",
        "Issue",
        "Revoke",
        "Rotate",
        "Sign",
        "Begin", // any non-allow-listed Begin* is state-changing (transactions etc.)
        "Commit",
        "Abort",
        "Create",
        "Delete",
        "Add",
        "Remove",
        "Approve",
        "Register",
        "Deregister",
        "ResolveDiscussion",
        "RespondToHook",
        "OpenDiscussion",
        "AppendTurn",
        "Finish",
        "Complete",
        "Cancel",
        "Set", // Set* RPCs (SetThreadPolicy etc.) mutate
    ];
    PREFIXES.iter().any(|p| name.starts_with(p))
}

fn run_web_proto(args: Vec<String>) -> Result<()> {
    let check = args.iter().any(|arg| arg == "--check");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|path| path.parent())
        .context("failed to locate workspace root")?;
    // Canonical proto source — same path the `heddle-grpc` build
    // script and the `audit-idempotency` lint read from. Keeping a
    // single source eliminates the drift class that landed stale
    // mirrors under `proto/` (see heddle#71).
    let proto_dir = workspace_root.join("crates/grpc/proto");
    let proto_file = proto_dir.join("heddle/v1/service.proto");
    let web_dir = workspace_root.join("web");
    let output_root = web_dir.join("src/lib/gen/proto");
    let relative_output = PathBuf::from("heddle/v1/service_pb.ts");

    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    let plugin = resolve_plugin_path(&web_dir)?;

    if check {
        let temp = tempfile::tempdir().context("failed to create temp directory")?;
        generate_service_descriptor(&protoc, &plugin, &proto_dir, &proto_file, temp.path())?;
        let generated = temp.path().join(&relative_output);
        let checked_in = output_root.join(&relative_output);
        assert_file_matches(&generated, &checked_in)?;
        println!("web proto output is up to date");
        return Ok(());
    }

    generate_service_descriptor(&protoc, &plugin, &proto_dir, &proto_file, &output_root)?;
    println!("generated {}", output_root.join(relative_output).display());
    Ok(())
}

fn generate_service_descriptor(
    protoc: &Path,
    plugin: &Path,
    proto_dir: &Path,
    proto_file: &Path,
    output_root: &Path,
) -> Result<()> {
    fs::create_dir_all(output_root).with_context(|| {
        format!(
            "failed to create output directory '{}'",
            output_root.display()
        )
    })?;

    let status = Command::new(protoc)
        .arg(format!("--plugin=protoc-gen-es={}", plugin.display()))
        .arg(format!("--proto_path={}", proto_dir.display()))
        .arg(format!("--es_out=target=ts:{}", output_root.display()))
        .arg(proto_file)
        .status()
        .with_context(|| format!("failed to run protoc at '{}'", protoc.display()))?;

    if !status.success() {
        bail!("protoc exited with status {status}");
    }

    Ok(())
}

fn resolve_plugin_path(web_dir: &Path) -> Result<PathBuf> {
    if let Ok(value) = env::var("PROTOC_GEN_ES") {
        let path = PathBuf::from(value);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "PROTOC_GEN_ES was set, but '{}' does not exist",
            path.display()
        );
    }

    let candidates = [
        web_dir.join("node_modules/.bin/protoc-gen-es"),
        web_dir.join("node_modules/.bin/protoc-gen-es.cmd"),
    ];

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    bail!(
        "could not find protoc-gen-es in web/node_modules/.bin.\n\
Install web dependencies first (for example: `cd web && npm install`) or set PROTOC_GEN_ES."
    )
}

fn assert_file_matches(generated: &Path, checked_in: &Path) -> Result<()> {
    let generated_contents = fs::read_to_string(generated)
        .with_context(|| format!("failed to read generated file '{}'", generated.display()))?;
    let checked_in_contents = fs::read_to_string(checked_in)
        .with_context(|| format!("failed to read checked-in file '{}'", checked_in.display()))?;

    if generated_contents != checked_in_contents {
        bail!(
            "generated proto output differs from '{}'. Run `npm run proto:gen` in web.",
            checked_in.display()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests_proto_single_source {
    use std::path::PathBuf;

    fn workspace_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root from CARGO_MANIFEST_DIR")
            .to_path_buf()
    }

    // Heddle ships exactly one canonical `service.proto`, at
    // `crates/grpc/proto/heddle/v1/service.proto`. The historical
    // mirrors at `proto/heddle/v1/` and `proto/proto/heddle/v1/`
    // drifted (missing `RedactionTransfer` before heddle#63 r1).
    #[test]
    fn only_canonical_proto_exists() {
        let root = workspace_root();
        let canonical = root.join("crates/grpc/proto/heddle/v1/service.proto");
        assert!(
            canonical.exists(),
            "canonical proto missing at {}",
            canonical.display()
        );

        for mirror in [
            "proto/heddle/v1/service.proto",
            "proto/proto/heddle/v1/service.proto",
        ] {
            let p = root.join(mirror);
            assert!(
                !p.exists(),
                "duplicate proto mirror still present: {} — single-source contract requires {} only",
                p.display(),
                canonical.display()
            );
        }
    }
}
