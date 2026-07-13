// SPDX-License-Identifier: Apache-2.0
use objects::object::{MarkerName, ThreadName};

use super::{git_overlay_fixtures::GitOverlayFixture, *};

#[test]
fn git_owned_source_commands_refuse_with_exact_git_argv() {
    let source = TempDir::new().unwrap();
    init_git_repo_with_branch(source.path(), "main");
    std::fs::write(source.path().join("tracked.txt"), "tracked\n").unwrap();
    git_commit_all(source.path(), "initial");

    let destination = source.path().join("clone-destination");
    let clone = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            source.path().to_str().unwrap(),
            destination.to_str().unwrap(),
        ],
        None,
    )
    .unwrap();
    assert!(!clone.status.success());
    let clone_error: Value = serde_json::from_slice(&clone.stderr).unwrap();
    assert_eq!(clone_error["kind"], "source_authority_direct_git");
    assert_eq!(
        clone_error["primary_command"],
        format!(
            "git clone {} {}",
            source.path().display(),
            destination.display()
        )
    );
    assert!(!destination.exists());

    heddle(&["init"], Some(source.path())).unwrap();
    for (args, expected) in [
        (&["remote", "list"][..], "git remote -v"),
        (&["push"][..], "git push"),
        (&["pull"][..], "git pull"),
    ] {
        let output =
            heddle_output(&[&["--output", "json"], args].concat(), Some(source.path())).unwrap();
        assert!(!output.status.success(), "{args:?}");
        let error: Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(error["kind"], "source_authority_direct_git");
        assert_eq!(error["primary_command"], expected);
    }
}

fn heddle_without_git_for_remote_tests(args: &[&str], cwd: &std::path::Path) -> String {
    let output = heddle_output_with_env(args, Some(cwd), &[("PATH", ""), ("NO_COLOR", "1")])
        .expect("invoke heddle without git on PATH");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(
        output.status.success(),
        "heddle {args:?} should succeed without git on PATH\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

fn verify_json(cwd: &std::path::Path) -> Value {
    let output =
        heddle_output(&["--output", "json", "verify"], Some(cwd)).expect("invoke verify JSON");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        let parsed: Value = serde_json::from_str(&stdout).expect("verify JSON should parse");
        return parsed.get("verification").cloned().unwrap_or(parsed);
    }
    let envelope: Value =
        serde_json::from_str(&stderr).expect("verify failure envelope should parse");
    assert_eq!(envelope["kind"], "verify_failed", "{envelope}");
    envelope["verification"].clone()
}

fn current_thread_state(cwd: &std::path::Path, thread: &str) -> String {
    let repo = Repository::open(cwd).expect("open repository");
    repo.refs()
        .get_thread(&ThreadName::new(thread))
        .expect("read thread ref")
        .unwrap_or_else(|| panic!("{thread} should have a current state"))
        .to_string()
}

fn log_head_state(cwd: &std::path::Path) -> String {
    let log_json =
        heddle(&["--output", "json", "log", "--limit", "1"], Some(cwd)).expect("log current state");
    let log: Value = serde_json::from_str(&log_json).expect("log JSON parses");
    log["states"][0]["state_id"]
        .as_str()
        .expect("log entry has state_id")
        .to_string()
}

/// Mutation `--output json` replies no longer embed `verification`
/// (the verification-claim gate still consults it in-memory, but it
/// is omitted from the wire to keep mutation replies focused).
/// Some integration tests pattern-match on the field; this helper
/// invokes `heddle verify --output json` after the fact and grafts
/// the proof back onto the test fixture so the existing assertions
/// keep working without per-call rewrites. Real consumers see the
/// field omitted.
fn inject_post_verification_at(cwd: &std::path::Path, mut value: Value) -> Value {
    let obj = match value.as_object_mut() {
        Some(obj) => obj,
        None => return value,
    };
    if obj.contains_key("verification") {
        return value;
    }
    let verify_out = match heddle_output(&["--output", "json", "verify"], Some(cwd)) {
        Ok(out) => out,
        Err(_) => return value,
    };
    let stream = if !verify_out.status.success() {
        verify_out.stderr
    } else {
        verify_out.stdout
    };
    let text = std::str::from_utf8(&stream).unwrap_or("");
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return value,
    };
    let verification = if parsed.get("kind") == Some(&Value::String("verify_failed".to_string())) {
        parsed.get("verification").cloned().unwrap_or(Value::Null)
    } else if let Some(verification) = parsed.get("verification") {
        verification.clone()
    } else {
        let mut obj_map = parsed.as_object().cloned().unwrap_or_default();
        obj_map.remove("output_kind");
        obj_map.remove("repository_label");
        obj_map.remove("repository_context");
        obj_map.remove("clean");
        Value::Object(obj_map)
    };
    obj.insert("verification".to_string(), verification);
    value
}

#[test]
fn test_cli_remote_operations() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(
        &[
            "--output",
            "text",
            "remote",
            "add",
            "origin",
            "localhost:8421",
        ],
        Some(temp.path()),
    );
    assert!(result.is_ok(), "Remote add failed: {:?}", result.err());
    assert!(
        result.as_ref().unwrap().contains("added remote origin"),
        "Remote add should confirm creation: {:?}",
        result.as_ref().ok()
    );

    let output = heddle(&["--output", "text", "remote", "list"], Some(temp.path())).unwrap();
    assert!(
        output.contains("origin") && output.contains("localhost:8421"),
        "Should list added remote: {}",
        output
    );

    let output = heddle(
        &["--output", "text", "remote", "show", "origin"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(
        output.contains("origin") && output.contains("localhost:8421"),
        "Remote show should include details: {}",
        output
    );

    heddle(
        &[
            "--output",
            "text",
            "remote",
            "add",
            "backup",
            "localhost:8422",
        ],
        Some(temp.path()),
    )
    .unwrap();
    heddle(
        &["--output", "text", "remote", "set-default", "backup"],
        Some(temp.path()),
    )
    .unwrap();
    let json = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("remote list JSON should parse");
    assert_eq!(parsed["output_kind"], "remote_list");
    let remotes = parsed["remotes"].as_array().unwrap();
    assert!(
        remotes
            .iter()
            .any(|remote| remote["name"] == "backup" && remote["is_default"] == true),
        "remote list should mark the configured default: {parsed}"
    );
    let json = heddle(
        &["--output", "json", "remote", "show", "backup"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("remote show JSON should parse");
    assert_eq!(parsed["output_kind"], "remote_show");
    assert_eq!(parsed["is_default"], true);

    let verify = verify_json(temp.path());
    assert_eq!(
        verify["default_remote"], "backup",
        "verify should report the configured default remote: {verify}"
    );

    heddle(
        &["--output", "text", "remote", "remove", "backup"],
        Some(temp.path()),
    )
    .unwrap();
    let result = heddle(
        &["--output", "text", "remote", "remove", "origin"],
        Some(temp.path()),
    );
    assert!(result.is_ok(), "Remote remove failed: {:?}", result.err());
    assert!(
        result.as_ref().unwrap().contains("removed remote origin"),
        "Remote remove should confirm deletion: {:?}",
        result.as_ref().ok()
    );

    let result = heddle(&["--output", "text", "remote", "list"], Some(temp.path())).unwrap();
    assert!(
        result.contains("No remotes configured"),
        "empty remote list should advertise the empty state: {result}"
    );
    let json = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("empty remote list JSON should parse");
    assert_eq!(parsed["output_kind"], "remote_list");
    assert_eq!(parsed["remotes"].as_array().unwrap().len(), 0);
}

#[test]
fn native_remote_add_rejects_local_git_remote_before_configuring_default() {
    let temp = TempDir::new().unwrap();
    let bare = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    SleyRepository::init_bare(bare.path()).expect("init bare Git remote");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "remote",
            "add",
            "origin",
            bare.path().to_str().unwrap(),
        ],
        Some(temp.path()),
    )
    .expect("invoke remote add");
    assert!(
        !output.status.success(),
        "remote add should reject Git remote"
    );
    assert!(output.stdout.is_empty());
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("transport mismatch should emit JSON");
    assert_eq!(envelope["kind"], "remote_transport_mismatch");
    assert_json_recovery_advice_fields(&envelope, stderr);
    assert_eq!(
        envelope["primary_command"], "heddle clone <remote> <fresh-path>",
        "Git remote mismatch should point to a Git-overlay checkout, not retry native remote add: {envelope}"
    );
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!([
            "heddle clone <remote> <fresh-path>",
            "heddle remote add <name> <url>",
        ]),
        "Git remote mismatch should offer clone/adopt path before native remote setup: {envelope}"
    );

    let remotes = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let remotes: Value = serde_json::from_str(&remotes).expect("remote list JSON");
    assert_eq!(remotes["remotes"].as_array().unwrap().len(), 0);
    let verify = verify_json(temp.path());
    assert_eq!(verify["default_remote"], Value::Null);
}

#[test]
fn native_push_and_pull_reject_direct_git_remote_before_native_sync() {
    let temp = TempDir::new().unwrap();
    let bare = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    SleyRepository::init_bare(bare.path()).expect("init bare Git remote");

    for command in ["push", "pull"] {
        let output = heddle_output(
            &["--output", "json", command, bare.path().to_str().unwrap()],
            Some(temp.path()),
        )
        .unwrap_or_else(|err| panic!("invoke heddle {command}: {err}"));
        assert!(
            !output.status.success(),
            "{command} should reject Git remote before native sync"
        );
        assert!(output.stdout.is_empty());
        let stderr = std::str::from_utf8(&output.stderr).unwrap();
        let envelope: Value =
            serde_json::from_str(stderr).expect("transport mismatch should emit JSON");
        assert_eq!(envelope["kind"], "remote_transport_mismatch");
        assert_eq!(
            envelope["primary_command"], "heddle clone <remote> <fresh-path>",
            "{command} mismatch should point to Git-overlay clone/adopt path: {envelope}"
        );
        assert!(
            !envelope["error"]
                .as_str()
                .unwrap_or_default()
                .contains("repository_not_found"),
            "{command} should not fall through to Repository::open: {stderr}"
        );
    }
}

/// Regression (push-routing silent no-op): a `heddle://` HOSTED remote on a
/// git-overlay repo whose `[hosted]` config block is EMPTY must route to the
/// native hosted-sync path, NOT the local git-overlay refs exporter. The bug:
/// the exporter treats `heddle://` as a generic git network URL, "reconciles"
/// refs locally, and returns `{"transport":"git","success":true,"refs_written":[]}`
/// without ever contacting the server — a silent no-op reported as success.
///
/// No live server runs in CI, so we assert on the ROUTING DECISION, not an
/// end-to-end push: a correctly-routed hosted push fails LOUDLY trying to reach
/// the (absent) server; the bug-signature is a `transport:"git"` SUCCESS
/// envelope. We accept any loud failure (or a non-git transport) and reject the
/// silent-git-success signature specifically.
#[test]
fn git_overlay_push_to_heddle_scheme_routes_to_hosted_not_git_exporter() {
    let source = TempDir::new().unwrap();

    // Build a real git-overlay repo (git init + heddle adopt) — `capability()`
    // is GitOverlay and `[hosted]` is empty (hosted_enabled() == false), which
    // is exactly the buggy predicate's input.
    git_ok(&["init", "-b", "main"], source.path());
    git_ok(&["config", "user.name", "Heddle Test"], source.path());
    git_ok(
        &["config", "user.email", "heddle@example.com"],
        source.path(),
    );
    std::fs::write(source.path().join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], source.path());
    git_ok(&["commit", "-m", "seed"], source.path());
    heddle(&["adopt", "--ref", "main"], Some(source.path())).expect("adopt source Git repo");

    // A hosted heddle:// remote pointing at a port nothing is listening on,
    // exercised through BOTH invocation forms that resolve to it:
    //   1. inline URL:        `heddle push heddle://...`
    //   2. named remote:      `heddle push origin`  (origin -> heddle://)
    // The named-remote form is the common real-world repro and resolves the
    // URL via RemoteConfig, so it must route identically.
    let hosted_url = "heddle://127.0.0.1:1/org/repo";
    heddle(
        &["remote", "add", "origin", hosted_url],
        Some(source.path()),
    )
    .expect("add hosted origin remote");

    for push_arg in [hosted_url, "origin"] {
        let output = heddle_output(&["--output", "json", "push", push_arg], Some(source.path()))
            .expect("spawn heddle push");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // The exact bug signature: a SUCCESS via the git exporter. If the push
        // routed to the git-overlay exporter, stdout carries
        // {"transport":"git","success":true,...}. That must never happen for a
        // heddle:// remote regardless of how it was named.
        if let Ok(envelope) = serde_json::from_str::<Value>(stdout.trim()) {
            let transport = envelope["transport"].as_str().unwrap_or_default();
            let success = envelope["success"].as_bool().unwrap_or(false);
            assert!(
                !(transport == "git" && success),
                "`heddle push {push_arg}` silently no-opped via the git-overlay \
                 exporter (transport=git, success=true) instead of routing to the \
                 hosted path: {envelope}"
            );
        }

        // Positively, a hosted push that cannot reach a server must FAIL loudly
        // (non-zero exit) rather than report success of any kind — proving it
        // reached the hosted transport (dead-port connect error, or a
        // `client`-feature-less build's `network_feature_unavailable`) instead
        // of the git-overlay exporter's silent local reconcile.
        assert!(
            !output.status.success(),
            "`heddle push {push_arg}` to an unreachable hosted remote must fail \
             loudly, not succeed (silent no-op). stdout: {stdout}\nstderr: {stderr}"
        );
    }
}

/// Regression (#839): `heddle fetch` on a git-overlay repo whose remote is a
/// hosted `heddle://` endpoint (with an EMPTY `[hosted]` config block, so
/// `hosted_enabled() == false`) must route through the native hosted-sync path
/// (`fetch_network`), NOT the git-overlay exporter. The bug: the entry-gate
/// guard in `cmd_fetch` lacked pull's `!fetch_uses_hosted_network` term, so a
/// hosted remote entered the overlay branch and hit `local_path_from_url`,
/// which HARD-ERRORS on any `heddle://` URL with "...cannot be pushed via the
/// git-overlay exporter" — a push-flavoured error during a *fetch*.
///
/// No live server runs in CI, so we assert on the ROUTING DECISION: a correctly
/// routed hosted fetch fails on the CONNECTION to the (absent) server, while the
/// bug fails on the exporter's scheme rejection. We reject that specific
/// scheme-rejection signature and accept any connection-flavoured failure.
#[test]
fn git_overlay_fetch_heddle_scheme_routes_to_hosted_not_git_exporter() {
    let source = TempDir::new().unwrap();

    // Real git-overlay repo (git init + heddle adopt): capability() is
    // GitOverlay and `[hosted]` is empty — exactly the buggy predicate's input.
    git_ok(&["init", "-b", "main"], source.path());
    git_ok(&["config", "user.name", "Heddle Test"], source.path());
    git_ok(
        &["config", "user.email", "heddle@example.com"],
        source.path(),
    );
    std::fs::write(source.path().join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], source.path());
    git_ok(&["commit", "-m", "seed"], source.path());
    heddle(&["adopt", "--ref", "main"], Some(source.path())).expect("adopt source Git repo");

    // A hosted heddle:// remote pointing at a port nothing is listening on,
    // exercised through both invocation forms that resolve to it (inline URL
    // and named remote), each of which must route identically.
    let hosted_url = "heddle://127.0.0.1:1/org/repo";
    heddle(
        &["remote", "add", "origin", hosted_url],
        Some(source.path()),
    )
    .expect("add hosted origin remote");

    for fetch_arg in [hosted_url, "origin"] {
        let output =
            heddle_output(&["fetch", fetch_arg], Some(source.path())).expect("spawn heddle fetch");

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = format!("{stdout}{stderr}");

        // The exact bug signature: the git-overlay exporter rejecting the
        // heddle:// scheme. That message must never appear for a fetch — a
        // fetch that reaches the git-overlay exporter is mis-routed.
        assert!(
            !combined.contains("cannot be pushed via the git-overlay exporter"),
            "`heddle fetch {fetch_arg}` was mis-routed to the git-overlay exporter \
             (scheme-rejection error) instead of the hosted-sync path.\n\
             stdout: {stdout}\nstderr: {stderr}"
        );

        // A hosted fetch that cannot reach a server must fail loudly (the dead
        // port yields a connection error, or a `client`-feature-less build's
        // `network_feature_unavailable`) — proving it reached the hosted
        // transport rather than the overlay exporter's local reconcile.
        assert!(
            !output.status.success(),
            "`heddle fetch {fetch_arg}` to an unreachable hosted remote must fail \
             loudly (connection error), not succeed.\nstdout: {stdout}\nstderr: {stderr}"
        );
    }
}

/// Regression (#839, `--all` mixed set): `heddle fetch --all` on a git-overlay
/// repo that has BOTH a local-git remote and a hosted `heddle://` remote must
/// route each remote by its own scheme — overlay-fetch the git remote, hosted
/// `fetch_network` the heddle:// remote — not gate the whole batch on a single
/// classification. The git remote is reachable (a real bare repo) so the batch
/// gets as far as the hosted remote, which then fails on its dead-port
/// connection — NOT on the overlay exporter's scheme rejection.
#[test]
fn git_overlay_fetch_all_routes_mixed_remotes_per_scheme() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");
    let src = SleyRepository::init_bare(&source).expect("init bare source");

    let tree = git_tree_with_file(&src, "tracked.txt", b"one\n");
    git_commit_with_tree(&src, Some("refs/heads/main"), tree, "one", &[]);
    std::fs::write(source.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let source_arg = source.to_str().expect("source path utf8");
    let work_arg = work.to_str().expect("work path utf8");
    // Clone from the local git remote — `origin` now points at the bare repo.
    heddle(&["clone", source_arg, work_arg], Some(temp.path())).expect("clone succeeds");

    // Add a SECOND, hosted heddle:// remote alongside the git `origin`.
    let hosted_url = "heddle://127.0.0.1:1/org/repo";
    heddle(&["remote", "add", "hosted", hosted_url], Some(&work)).expect("add hosted remote");

    let output = heddle_output(&["fetch", "--all"], Some(&work)).expect("spawn heddle fetch --all");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    // The hosted remote in the batch must never be routed to the git-overlay
    // exporter — that scheme-rejection error is the mis-route signature.
    assert!(
        !combined.contains("cannot be pushed via the git-overlay exporter"),
        "`heddle fetch --all` mis-routed the hosted remote to the git-overlay \
         exporter instead of the hosted-sync path.\nstdout: {stdout}\nstderr: {stderr}"
    );

    // The git remote is reachable but the hosted one is not, so the overall
    // command must fail loudly on the hosted connection rather than silently
    // succeed or reject the whole batch on the exporter scheme error.
    assert!(
        !output.status.success(),
        "`heddle fetch --all` with an unreachable hosted remote in the set must \
         fail loudly (hosted connection error).\nstdout: {stdout}\nstderr: {stderr}"
    );

    // Regression guard: the reachable git remote was still fetched — its
    // remote-tracking ref exists — proving the hosted failure did not skip the
    // git remote's overlay fetch.
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &work),
        git_stdout_trimmed(&["rev-parse", "refs/heads/main"], &source),
        "the git remote in a mixed --all set must still be overlay-fetched"
    );
}

#[test]
fn test_cli_remote_show_missing_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "remote", "show", "missing"],
        Some(temp.path()),
    )
    .expect("invoke missing remote show");
    assert!(!output.status.success(), "remote show missing should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode remote show refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "remote_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Remote 'missing' not found")),
        "missing remote refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle remote list")
                && hint.contains("heddle remote add <name> <url>")),
        "missing remote hint should name inspect and setup commands: {stderr}"
    );
}

#[test]
fn test_cli_pull_local_updates_requested_track() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();

    heddle(&["init"], Some(target.path())).unwrap();

    let source_path = source.path().to_str().unwrap().to_string();
    let output = heddle(
        &[
            "--output",
            "json",
            "pull",
            &source_path,
            "--thread",
            "main",
            "--local-thread",
            "imported",
        ],
        Some(target.path()),
    )
    .unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "pull --output json must emit exactly one JSON value: {output}"
    );
    let parsed: Value = inject_post_verification_at(
        target.path(),
        serde_json::from_str(&output).expect("pull JSON should parse"),
    );
    assert_eq!(parsed["output_kind"], "pull");
    assert_eq!(parsed["action"], "pull");
    assert_eq!(
        parsed["success"], true,
        "pull should report success: {parsed}"
    );
    assert_eq!(parsed["status"], "updated");
    assert_eq!(parsed["transport"], "heddle");
    assert_eq!(parsed["thread"], "imported");
    assert!(
        parsed["state"].is_string(),
        "pull should report state: {parsed}"
    );
    assert!(
        parsed["objects"].is_number(),
        "pull should report objects: {parsed}"
    );
    assert_eq!(parsed["verification"]["status"], "clean");

    let target_repo = Repository::open(target.path()).unwrap();
    assert!(
        target_repo
            .refs()
            .get_thread(&ThreadName::new("imported"))
            .unwrap()
            .is_some(),
        "imported thread should be created"
    );
    heddle(&["thread", "switch", "imported"], Some(target.path())).unwrap();
    let blob = std::fs::read_to_string(target.path().join("hello.txt")).unwrap();
    assert_eq!(blob, "from source");
}

#[test]
fn test_cli_pull_local_dirty_refusal_leaves_thread_ref_unchanged() {
    let source = TempDir::new().unwrap();
    let target_parent = TempDir::new().unwrap();
    let target_path = target_parent.path().join("target");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("shared.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "Base state"], Some(source.path())).unwrap();

    let source_path = source.path().to_str().unwrap().to_string();
    let target_path_arg = target_path.to_str().unwrap().to_string();
    heddle(&["clone", &source_path, &target_path_arg], None).unwrap();

    let target_repo = Repository::open(&target_path).unwrap();
    let pre_pull_ref = target_repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("cloned main ref exists");

    std::fs::write(source.path().join("shared.txt"), "remote\n").unwrap();
    heddle(&["capture", "-m", "Remote state"], Some(source.path())).unwrap();
    std::fs::write(target_path.join("shared.txt"), "local dirty\n").unwrap();

    let pull = heddle_output(
        &["--output", "json", "pull", &source_path],
        Some(&target_path),
    )
    .expect("invoke dirty local pull");
    assert!(
        !pull.status.success(),
        "dirty local pull should refuse before publishing the ref"
    );
    assert!(
        pull.stdout.is_empty(),
        "JSON refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&pull.stdout)
    );

    let target_repo = Repository::open(&target_path).unwrap();
    assert_eq!(
        target_repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap(),
        Some(pre_pull_ref),
        "dirty pull refusal must leave main at the pre-pull state"
    );
    assert_eq!(
        std::fs::read_to_string(target_path.join("shared.txt")).unwrap(),
        "local dirty\n",
        "dirty pull refusal must preserve the user's edit"
    );

    let status_json =
        heddle(&["--output", "json", "status"], Some(&target_path)).expect("status succeeds");
    let status: Value = serde_json::from_str(&status_json).expect("status JSON parses");
    assert_eq!(
        status["state"]["state_id"],
        pre_pull_ref.to_string(),
        "status must continue attributing the dirty file against the pre-pull state: {status_json}"
    );
    assert_eq!(
        status["changes"]["modified"],
        serde_json::json!(["shared.txt"]),
        "dirty edit should remain attributed to the pulled clone's original baseline: {status_json}"
    );
}

/// heddle#646: the planned lazy/partial-clone flags stay `hide = true`
/// (out of the human options list), and human help carries a one-line
/// breadcrumb to the detailed topic.
#[test]
fn test_cli_clone_help_keeps_planned_lazy_flag_to_breadcrumb() {
    let output = heddle_help(&["clone", "--help"]);
    assert!(
        output.contains("Advanced/planned flags: see `heddle help clone`."),
        "clone help carries the advanced/planned flags breadcrumb: {output}"
    );
    assert!(
        !output.contains("--lazy") && !output.contains("--filter"),
        "clone help should keep planned lazy/partial clone flags out of first-run help: {output}"
    );
}

#[test]
fn test_cli_pull_local_side_thread_updates_ref_without_materializing_checkout() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("source.txt"), "from source\n").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();

    heddle(&["init"], Some(target.path())).unwrap();
    std::fs::write(target.path().join("target.txt"), "from target\n").unwrap();
    heddle(&["capture", "-m", "Target state"], Some(target.path())).unwrap();
    let main_before = current_thread_state(target.path(), "main");

    let source_path = source.path().to_str().unwrap().to_string();
    let pull_json = heddle(
        &[
            "--output",
            "json",
            "pull",
            &source_path,
            "--thread",
            "main",
            "--local-thread",
            "imported",
        ],
        Some(target.path()),
    )
    .expect("side-thread pull succeeds");
    let pull: Value = serde_json::from_str(&pull_json).expect("pull JSON parses");
    assert_eq!(pull["thread"], "imported", "{pull}");

    assert_eq!(
        current_thread_state(target.path(), "main"),
        main_before,
        "side-thread pull must not advance the active main thread"
    );
    assert!(
        !target.path().join("source.txt").exists(),
        "side-thread pull must not materialize remote files into the active checkout"
    );
    assert_eq!(
        std::fs::read_to_string(target.path().join("target.txt")).unwrap(),
        "from target\n",
        "side-thread pull must leave active checkout content untouched"
    );

    heddle(&["thread", "switch", "imported"], Some(target.path()))
        .expect("imported thread should be switchable after direct ref update");
    assert_eq!(
        std::fs::read_to_string(target.path().join("source.txt")).unwrap(),
        "from source\n"
    );
}

#[test]
fn test_cli_pull_local_clean_active_checkout_materializes_before_publish() {
    let source = TempDir::new().unwrap();
    let target_parent = TempDir::new().unwrap();
    let target_path = target_parent.path().join("target");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("shared.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "Base state"], Some(source.path())).unwrap();

    let source_path = source.path().to_str().unwrap().to_string();
    let target_path_arg = target_path.to_str().unwrap().to_string();
    heddle(&["clone", &source_path, &target_path_arg], None).unwrap();
    let pre_pull_ref = current_thread_state(&target_path, "main");

    std::fs::write(source.path().join("shared.txt"), "remote\n").unwrap();
    heddle(&["capture", "-m", "Remote state"], Some(source.path())).unwrap();
    let source_main = current_thread_state(source.path(), "main");

    heddle(
        &["--output", "json", "pull", &source_path],
        Some(&target_path),
    )
    .expect("clean active-checkout pull succeeds");

    assert_eq!(
        current_thread_state(&target_path, "main"),
        source_main,
        "clean pull should publish main after materializing the worktree"
    );
    assert_eq!(
        std::fs::read_to_string(target_path.join("shared.txt")).unwrap(),
        "remote\n",
        "clean pull should materialize the pulled content"
    );
    let status_json =
        heddle(&["--output", "json", "status"], Some(&target_path)).expect("status JSON");
    let status: Value = serde_json::from_str(&status_json).expect("status JSON parses");
    assert_eq!(status["thread"], "main", "{status_json}");

    heddle(&["undo"], Some(&target_path)).expect("pull fast-forward should be undoable");
    assert_eq!(
        current_thread_state(&target_path, "main"),
        pre_pull_ref,
        "undo should restore the pre-pull main ref recorded before publication"
    );
    assert_eq!(
        std::fs::read_to_string(target_path.join("shared.txt")).unwrap(),
        "base\n",
        "undo should restore the pre-pull materialized checkout"
    );
}

#[test]
fn test_cli_pull_local_detached_head_materializes_then_publishes_thread() {
    let source = TempDir::new().unwrap();
    let target_parent = TempDir::new().unwrap();
    let target_path = target_parent.path().join("target");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("shared.txt"), "base\n").unwrap();
    heddle(&["capture", "-m", "Base state"], Some(source.path())).unwrap();

    let source_path = source.path().to_str().unwrap().to_string();
    let target_path_arg = target_path.to_str().unwrap().to_string();
    heddle(&["clone", &source_path, &target_path_arg], None).unwrap();
    let base_state = current_thread_state(&target_path, "main");
    heddle(&["switch", &base_state], Some(&target_path)).expect("detach target HEAD");
    let head_before = std::fs::read_to_string(target_path.join(".heddle").join("HEAD"))
        .expect("read detached HEAD");
    assert!(
        !head_before.trim().starts_with("ref:"),
        "test setup should leave HEAD detached: {head_before}"
    );

    std::fs::write(source.path().join("shared.txt"), "remote\n").unwrap();
    heddle(&["capture", "-m", "Remote state"], Some(source.path())).unwrap();
    let source_main = current_thread_state(source.path(), "main");

    heddle(
        &["--output", "json", "pull", &source_path],
        Some(&target_path),
    )
    .expect("detached local pull succeeds");

    assert_eq!(
        current_thread_state(&target_path, "main"),
        source_main,
        "detached pull should publish the local thread after materializing"
    );
    assert_eq!(
        log_head_state(&target_path),
        source_main,
        "detached pull should move detached HEAD to the pulled state"
    );
    let head_after = std::fs::read_to_string(target_path.join(".heddle").join("HEAD"))
        .expect("read detached HEAD after pull");
    assert!(
        !head_after.trim().starts_with("ref:"),
        "detached pull should not attach HEAD to the published thread: {head_after}"
    );
    assert_eq!(
        std::fs::read_to_string(target_path.join("shared.txt")).unwrap(),
        "remote\n",
        "detached pull should materialize the pulled content"
    );
}

/// heddle#646: `pull --lazy` stays `hide = true` (out of the options
/// list) but is named once in the after-help "Advanced (hidden) flags"
/// breadcrumb so it's discoverable.
#[test]
fn test_cli_pull_help_keeps_planned_lazy_flag_to_breadcrumb() {
    let output = heddle_help(&["pull", "--help"]);
    let (first_run, breadcrumb) = output
        .split_once("Advanced (hidden) flags:")
        .expect("pull help carries the advanced-flags breadcrumb (heddle#646)");
    assert!(
        !first_run.contains("--lazy"),
        "pull help should keep planned lazy pull out of first-run help: {output}"
    );
    assert!(
        breadcrumb.contains("--lazy"),
        "pull help's breadcrumb should name the hidden --lazy flag: {output}"
    );
}

#[test]
fn git_overlay_push_help_names_git_tag_scope_explicitly() {
    let help = heddle_help(&["push", "--help"]);
    assert!(
        help.contains("Git tag visible to this checkout")
            && help.contains("skips Git tags")
            && !help.contains("including tags"),
        "push help should make default/all-threads tag behavior concrete: {help}"
    );
}

#[test]
fn push_help_documents_written_refs_namespace() {
    let help = heddle_help(&["push", "--help"]);
    assert!(
        help.contains("refs/heads/<thread>")
            && help.contains("refs/notes/heddle")
            && help.contains("refs/tags/<tag>")
            && help.contains("git ls-remote")
            && help.contains("refs_written"),
        "push help should document exactly which Git refs a push writes and how to verify them: {help}"
    );
}

/// Every full ref name under `refs/` at the remote, sorted — the
/// `git ls-remote` view (minus HEAD) the `refs_written` round-trip
/// contract is asserted against.
fn remote_ref_names(remote_repo: &SleyRepository) -> Vec<String> {
    let mut names: Vec<String> = remote_repo
        .references()
        .list_refs()
        .expect("iterate remote refs")
        .into_iter()
        .map(|reference| reference.name)
        .filter(|name| name.starts_with("refs/"))
        .collect();
    names.sort_unstable();
    names
}

#[test]
fn git_overlay_push_reports_refs_written_matching_ls_remote() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();

    let output = heddle(&["--output", "json", "push", "origin"], Some(work.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(
        parsed["refs_written"],
        serde_json::json!(["refs/heads/main", "refs/notes/heddle"]),
        "current-thread push should report exactly the branch + notes refs it wrote: {parsed}"
    );

    // Round-trip: the destination's refs are exactly the refs the push
    // reported — a git veteran running `git ls-remote` sees the same set.
    let reported: Vec<String> = parsed["refs_written"]
        .as_array()
        .expect("refs_written should be an array")
        .iter()
        .map(|name| name.as_str().expect("ref name is a string").to_string())
        .collect();
    assert_eq!(
        remote_ref_names(&remote_repo),
        reported,
        "refs at the remote should be exactly the refs the push output reported"
    );

    // A no-op repeat push writes nothing and says so.
    let output = heddle(&["--output", "json", "push", "origin"], Some(work.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("no-op push JSON should parse");
    assert_eq!(
        parsed["refs_written"],
        serde_json::json!([]),
        "a no-op push should report an empty refs_written array: {parsed}"
    );
}

#[test]
fn git_overlay_push_all_threads_reports_tag_and_sibling_refs_written() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();

    let output = heddle(
        &["--output", "json", "push", "origin", "--all-threads"],
        Some(work.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(
        parsed["refs_written"],
        serde_json::json!([
            "refs/heads/main",
            "refs/heads/side",
            "refs/notes/heddle",
            "refs/tags/v1.0"
        ]),
        "all-threads push should report every branch, tag, and notes ref it wrote: {parsed}"
    );
    assert_eq!(
        remote_ref_names(&remote_repo),
        vec![
            "refs/heads/main".to_string(),
            "refs/heads/side".to_string(),
            "refs/notes/heddle".to_string(),
            "refs/tags/v1.0".to_string(),
        ],
        "refs at the remote should be exactly the refs the push output reported"
    );
}

#[test]
fn git_overlay_push_defaults_to_current_thread_branch() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();

    let output = heddle(&["--output", "json", "push", "origin"], Some(work.path())).unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(parsed["push_scope"], "current_thread");
    assert_eq!(parsed["ref_scope"], "branch_and_heddle_notes");
    assert_eq!(parsed["tags_included"], false);
    assert_eq!(parsed["thread"], "main");

    assert!(
        find_reference(&remote_repo, "refs/heads/main").is_ok(),
        "default push should push the current branch"
    );
    assert!(
        find_reference(&remote_repo, "refs/heads/side").is_err(),
        "default push must not push sibling Heddle threads"
    );
    assert!(
        find_reference(&remote_repo, "refs/tags/v1.0").is_err(),
        "default push must not push tags"
    );
    assert!(
        find_reference(
            &remote_repo,
            cli::git_projection_engine::git_notes::NOTES_REF
        )
        .is_ok(),
        "default push must carry Heddle notes so clones preserve state identity"
    );
}

#[test]
fn git_overlay_push_all_threads_preserves_all_refs_behavior() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();

    let output = heddle(
        &["--output", "json", "push", "origin", "--all-threads"],
        Some(work.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(parsed["push_scope"], "all_threads");
    assert_eq!(parsed["ref_scope"], "all_threads_tags_and_heddle_notes");
    assert_eq!(parsed["tags_included"], true);
    assert!(parsed["thread"].is_null());

    assert!(find_reference(&remote_repo, "refs/heads/main").is_ok());
    assert!(find_reference(&remote_repo, "refs/heads/side").is_ok());
    assert!(find_reference(&remote_repo, "refs/tags/v1.0").is_ok());
}

#[test]
fn git_overlay_push_all_threads_does_not_promote_remote_tracking_threads() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();
    let repo = Repository::open(work.path()).expect("open work repo");
    let main = repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .expect("read main thread")
        .expect("main thread exists");
    repo.refs()
        .set_thread(&ThreadName::new("origin/remote-only"), &main)
        .expect("seed remote-tracking-shaped Heddle thread");

    let output = heddle(
        &["--output", "json", "push", "origin", "--all-threads"],
        Some(work.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(parsed["push_scope"], "all_threads");

    assert!(
        find_reference(&remote_repo, "refs/heads/main").is_ok(),
        "all-threads push should still publish owned local threads"
    );
    assert!(
        find_reference(&remote_repo, "refs/heads/origin/remote-only").is_err(),
        "all-threads push must not promote remote-tracking-shaped threads into remote heads"
    );
}

#[test]
fn git_overlay_push_all_threads_skips_threads_pruned_by_cleanup() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();
    let checkout = work.path().parent().unwrap().join(format!(
        "{}-heddle-cleaned-thread",
        work.path().file_name().unwrap().to_string_lossy()
    ));
    let checkout_arg = checkout.to_str().expect("checkout path utf8");

    heddle(
        &["start", "feature/cleaned", "--path", checkout_arg],
        Some(work.path()),
    )
    .unwrap();
    std::fs::write(checkout.join("cleaned.txt"), "cleaned\n").unwrap();
    heddle(
        &["ready", "-m", "cleaned feature"],
        Some(checkout.as_path()),
    )
    .unwrap();
    heddle(
        &["land", "--thread", "feature/cleaned", "--no-push"],
        Some(work.path()),
    )
    .unwrap();
    heddle(&["thread", "cleanup", "--merged"], Some(work.path())).unwrap();

    let list = heddle(&["thread", "list", "--output", "json"], Some(work.path())).unwrap();
    let list: Value = serde_json::from_str(&list).expect("thread list JSON should parse");
    let threads = list["threads"].as_array().expect("threads array");
    assert!(
        !threads
            .iter()
            .any(|thread| thread["name"] == "feature/cleaned"),
        "cleanup should remove the merged thread from default thread surfaces: {list}"
    );

    let output = heddle(
        &["--output", "json", "push", "origin", "--all-threads"],
        Some(work.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(parsed["push_scope"], "all_threads");

    assert!(
        find_reference(&remote_repo, "refs/heads/main").is_ok(),
        "all-threads push should still publish the active main thread"
    );
    assert!(
        find_reference(&remote_repo, "refs/heads/feature/cleaned").is_err(),
        "all-threads push must not recreate a Git branch for a cleaned merged thread"
    );
}

#[test]
fn git_overlay_push_all_threads_includes_checkout_tags_created_after_adopt() {
    let (work, _remote, remote_repo) = setup_git_overlay_push_fixture();
    git_ok(&["tag", "v2-local"], work.path());

    let output = heddle(
        &["--output", "json", "push", "origin", "--all-threads"],
        Some(work.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(parsed["tags_included"], true);

    assert!(
        find_reference(&remote_repo, "refs/tags/v2-local").is_ok(),
        "all-threads push should include raw checkout tags created after Heddle adoption"
    );
}

#[test]
fn git_overlay_remote_list_show_labels_local_bare_git_remote_as_git_overlay() {
    let work = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    SleyRepository::init(work.path()).expect("init git worktree");
    SleyRepository::init_bare(remote.path()).expect("init bare git remote");

    heddle(&["init"], Some(work.path())).unwrap();
    let remote_path = remote.path().to_str().expect("remote path utf8");
    heddle(&["remote", "add", "origin", remote_path], Some(work.path())).unwrap();

    let list_json = heddle(&["--output", "json", "remote", "list"], Some(work.path())).unwrap();
    let list: Value = serde_json::from_str(&list_json).expect("remote list JSON parses");
    let origin = list["remotes"]
        .as_array()
        .expect("remotes array")
        .iter()
        .find(|remote| remote["name"] == "origin")
        .expect("origin listed");
    assert_eq!(
        origin["source"], "git-overlay",
        "local bare Git remotes in a Git-overlay repo should not be labeled as native Heddle remotes: {list}"
    );

    let show_json = heddle(
        &["--output", "json", "remote", "show", "origin"],
        Some(work.path()),
    )
    .unwrap();
    let show: Value = serde_json::from_str(&show_json).expect("remote show JSON parses");
    assert_eq!(show["source"], "git-overlay", "{show}");

    let show_text = heddle(
        &["--output", "text", "remote", "show", "origin"],
        Some(work.path()),
    )
    .unwrap();
    assert!(
        show_text.contains("git-overlay") && !show_text.contains("source: heddle"),
        "remote show text should reflect Git-overlay transport: {show_text}"
    );
}

#[test]
fn git_overlay_remote_remove_uneditable_include_leaves_both_configs_unmutated() {
    // Regression: a git-overlay remote defined in BOTH `.heddle/remotes.toml`
    // and a Git config pulled in via an include from OUTSIDE the Git directory
    // must not be half-removed. The removal has to refuse on the uneditable
    // include BEFORE persisting the Heddle side — either both configs drop the
    // remote or neither does. The old order saved the Heddle removal first and
    // only then hit the include refusal, stranding partial state.
    let work = TempDir::new().unwrap();
    SleyRepository::init(work.path()).expect("init git worktree");
    heddle(&["init"], Some(work.path())).unwrap();

    // A `[remote "origin"]` section in a config file outside the repository's
    // own Git directory, reachable only through `include.path`.
    let external = work.path().join("external.config");
    std::fs::write(
        &external,
        "[remote \"origin\"]\n\turl = https://example.com/repo\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
    )
    .unwrap();
    let git_config = work.path().join(".git").join("config");
    std::fs::write(
        &git_config,
        format!(
            "[core]\n\trepositoryformatversion = 0\n[include]\n\tpath = {}\n",
            external.display()
        ),
    )
    .unwrap();
    // The same remote also adopted into the native Heddle config.
    let remotes_toml = work.path().join(".heddle").join("remotes.toml");
    std::fs::write(
        &remotes_toml,
        "default = \"origin\"\n\n[remotes.origin]\nurl = \"https://example.com/repo\"\n",
    )
    .unwrap();

    let before_remotes_toml = std::fs::read_to_string(&remotes_toml).unwrap();
    let before_git_config = std::fs::read_to_string(&git_config).unwrap();
    let before_external = std::fs::read_to_string(&external).unwrap();

    let output = heddle_output(
        &["--output", "json", "remote", "remove", "origin"],
        Some(work.path()),
    )
    .expect("invoke remote remove");
    assert!(
        !output.status.success(),
        "removing a remote defined in an uneditable include must refuse, not partially apply"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value = serde_json::from_str(stderr).unwrap_or_else(|err| {
        panic!("uneditable-include refusal should emit JSON: {err}: {stderr}")
    });
    assert_eq!(
        envelope["kind"], "git_remote_in_included_config",
        "{stderr}"
    );

    // No partial state: every config the command could have touched is unchanged.
    assert_eq!(
        std::fs::read_to_string(&remotes_toml).unwrap(),
        before_remotes_toml,
        "Heddle remote config must be untouched when the git-side removal refuses"
    );
    assert_eq!(
        std::fs::read_to_string(&git_config).unwrap(),
        before_git_config,
        "Git config must be untouched when the removal refuses"
    );
    assert_eq!(
        std::fs::read_to_string(&external).unwrap(),
        before_external,
        "included config must be untouched when the removal refuses"
    );
}

fn setup_git_overlay_push_fixture() -> (TempDir, TempDir, SleyRepository) {
    let work = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();
    let remote_repo = SleyRepository::init_bare(remote.path()).expect("init bare remote");
    let git_repo = SleyRepository::init(work.path()).expect("init git repo");
    let tree = git_empty_tree_oid(&git_repo);
    let main = git_commit_with_tree(&git_repo, Some("refs/heads/main"), tree, "main", &[]);
    let side = git_commit_with_tree(&git_repo, Some("refs/heads/side"), tree, "side", &[main]);
    git_set_reference(&git_repo, "refs/tags/v1.0", side);
    std::fs::write(
        work.path().join(".git").join("HEAD"),
        "ref: refs/heads/main\n",
    )
    .unwrap();
    std::fs::write(
        work.path().join(".git").join("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n[user]\n\tname = Heddle Test\n\temail = heddle@example.com\n[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
            remote.path().display()
        ),
    )
    .unwrap();

    heddle(&["init"], Some(work.path())).unwrap();
    heddle(&["import", "git"], Some(work.path())).unwrap();
    (work, remote, remote_repo)
}

#[test]
fn test_cli_clone_local_lazy_is_rejected() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let clone_dir = target.path().join("clone");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();

    let source_path = source.path().to_string_lossy().to_string();
    let clone_path = clone_dir.to_string_lossy().to_string();
    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            &source_path,
            &clone_path,
            "--lazy",
        ],
        None,
    )
    .expect("invoke local lazy clone");
    assert!(
        !output.status.success(),
        "local lazy clone should fail with a typed refusal"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode local lazy clone refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !clone_dir.exists(),
        "local lazy clone refusal must run before destination initialization"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("local lazy clone should emit JSON envelope");
    assert_eq!(envelope["kind"], "local_clone_option_unsupported");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("--lazy is only supported")),
        "local lazy clone should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("without lazy/filter")),
        "local lazy clone hint should name the safe retry: {stderr}"
    );
}

#[test]
fn test_cli_clone_local_attaches_head_to_cloned_thread() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let clone_dir = target.path().join("clone");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();

    let source_path = source.path().to_string_lossy().to_string();
    let clone_path = clone_dir.to_string_lossy().to_string();
    let clone_json = heddle(
        &["--output", "json", "clone", &source_path, &clone_path],
        None,
    )
    .expect("local clone succeeds");
    let clone_output: Value = inject_post_verification_at(
        &clone_dir,
        serde_json::from_str(&clone_json).expect("clone JSON parses"),
    );
    assert_eq!(clone_output["output_kind"], "clone");
    assert_eq!(clone_output["action"], "clone");
    assert_eq!(clone_output["status"], "cloned");
    assert_eq!(clone_output["success"], true);
    assert_eq!(clone_output["cloned"], true);
    assert_eq!(clone_output["transport"], "heddle");
    assert_eq!(clone_output["branch"], "main");
    assert_eq!(clone_output["repository_capability"], "native-heddle");
    assert!(clone_output["objects"].is_number());
    assert!(clone_output["state"].is_string());
    assert_eq!(clone_output["verification"]["status"], "clean");

    let head = std::fs::read_to_string(clone_dir.join(".heddle").join("HEAD"))
        .expect("read cloned Heddle HEAD");
    assert_eq!(
        head.trim(),
        "ref: main",
        "native clone should attach Heddle HEAD to the cloned thread, not leave a detached checkout"
    );
    let status_json =
        heddle(&["status", "--output", "json"], Some(&clone_dir)).expect("clone status JSON");
    let status: Value = serde_json::from_str(&status_json).expect("status JSON parses");
    assert_eq!(
        status["thread"], "main",
        "fresh native clone status should identify the active thread: {status_json}"
    );
}

#[test]
fn test_cli_local_sync_copies_context_and_discussion_blobs() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let remote = temp.path().join("remote");
    let clone_dir = temp.path().join("clone");
    std::fs::create_dir_all(source.join("src")).unwrap();
    std::fs::create_dir_all(&remote).unwrap();

    heddle(&["init"], Some(&source)).unwrap();
    std::fs::write(source.join("src/lib.rs"), "pub fn run() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(&source)).unwrap();
    heddle(
        &[
            "context",
            "set",
            "--path",
            "src/lib.rs",
            "--scope",
            "symbol:run",
            "--kind",
            "rationale",
            "-m",
            "run is the smoke-test entry point",
        ],
        Some(&source),
    )
    .unwrap();
    let open_json = heddle(
        &[
            "--output",
            "json",
            "discuss",
            "open",
            "src/lib.rs",
            "run",
            "should this remain the entry point?",
        ],
        Some(&source),
    )
    .unwrap();
    let opened: Value = serde_json::from_str(&open_json).expect("discuss open JSON parses");
    let discussion_id = opened["id"].as_str().expect("discussion id").to_string();

    heddle(&["init"], Some(&remote)).unwrap();
    let remote_path = remote.to_str().expect("remote path utf8");
    heddle(&["remote", "add", "local", remote_path], Some(&source)).unwrap();
    heddle(&["push", "local"], Some(&source)).unwrap();
    heddle(
        &[
            "clone",
            remote_path,
            clone_dir.to_str().expect("clone path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("local clone succeeds after push");

    let context_json = heddle(&["--output", "json", "context", "list"], Some(&clone_dir))
        .expect("cloned repo can list context");
    let context: Value = serde_json::from_str(&context_json).expect("context list JSON parses");
    assert!(
        context["items"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "context list should survive local push/clone sync: {context}"
    );

    let discussions_json = heddle(&["--output", "json", "discuss", "list"], Some(&clone_dir))
        .expect("cloned repo can list discussions");
    let discussions: Value =
        serde_json::from_str(&discussions_json).expect("discussion list JSON parses");
    let entries = discussions["discussions"]
        .as_array()
        .expect("discussion list array");
    assert!(
        entries
            .iter()
            .any(|discussion| discussion["id"].as_str() == Some(discussion_id.as_str())),
        "discussion list should survive local push/clone sync: {discussions}"
    );
}

#[test]
fn test_cli_clone_local_bare_git_heddle_remote_skips_admin_files_and_sets_origin() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let remote = temp.path().join("remote.git");
    let clone = temp.path().join("clone");
    std::fs::create_dir_all(&source).unwrap();

    heddle_without_git_for_remote_tests(&["init"], &source);
    std::fs::write(source.join("app.txt"), "from source\n").unwrap();
    heddle_without_git_for_remote_tests(&["capture", "-m", "seed app"], &source);

    SleyRepository::init_bare(&remote).expect("init local bare Git remote");
    heddle_without_git_for_remote_tests(&["init"], &remote);
    heddle_without_git_for_remote_tests(
        &["push", remote.to_str().expect("remote path utf8")],
        &source,
    );

    heddle_without_git_for_remote_tests(
        &[
            "clone",
            remote.to_str().expect("remote path utf8"),
            clone.to_str().expect("clone path utf8"),
        ],
        temp.path(),
    );

    assert_eq!(
        std::fs::read_to_string(clone.join("app.txt")).unwrap(),
        "from source\n",
        "clone should materialize the Heddle state from the bare remote"
    );
    for admin_path in [
        "HEAD",
        "config",
        "hooks",
        "info",
        "objects",
        "refs",
        "branches",
        "packed-refs",
    ] {
        assert!(
            !clone.join(admin_path).exists(),
            "clone must not materialize bare Git admin path `{admin_path}` as a worktree file"
        );
    }

    let list = heddle_without_git_for_remote_tests(&["--output", "json", "remote", "list"], &clone);
    let list: Value = serde_json::from_str(&list).expect("remote list JSON should parse");
    let origin = list["remotes"]
        .as_array()
        .expect("remotes array")
        .iter()
        .find(|remote| remote["name"] == "origin")
        .expect("clone should configure origin");
    assert_eq!(origin["source"], "heddle");
    assert_eq!(origin["is_default"], true);
    assert_eq!(
        origin["url"],
        format!("file://{}", remote.canonicalize().unwrap().display())
    );

    std::fs::write(clone.join("app.txt"), "from clone\n").unwrap();
    heddle_without_git_for_remote_tests(&["capture", "-m", "clone update"], &clone);
    let push = heddle_without_git_for_remote_tests(&["--output", "json", "push"], &clone);
    let push: Value = serde_json::from_str(&push).expect("push JSON should parse");
    assert_eq!(push["status"], "pushed");

    let clone_repo = Repository::open(&clone).expect("open clone repo");
    let remote_repo = Repository::open(&remote).expect("open remote repo");
    assert_eq!(
        remote_repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap(),
        clone_repo
            .refs()
            .get_thread(&ThreadName::new("main"))
            .unwrap(),
        "default origin should let a later `heddle push` update the cloned remote"
    );
}

#[test]
fn test_cli_clone_missing_local_remote_uses_typed_advice() {
    let target = TempDir::new().unwrap();
    let missing_remote = target.path().join("missing-source");
    let clone_dir = target.path().join("clone");

    let remote_path = missing_remote.to_string_lossy().to_string();
    let clone_path = clone_dir.to_string_lossy().to_string();
    let output = heddle_output(
        &["--output", "json", "clone", &remote_path, &clone_path],
        None,
    )
    .expect("invoke missing local remote clone");
    assert!(
        !output.status.success(),
        "clone with missing local remote should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing local remote refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !clone_dir.exists(),
        "missing local remote refusal must not initialize the destination"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing local remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_remote_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(
            |error| error.contains("Remote repository") && error.contains("does not exist")
        ),
        "missing local remote should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("retry `heddle clone`")),
        "missing local remote hint should name the safe retry: {stderr}"
    );
}

#[test]
#[cfg(feature = "client")]
fn clone_network_validates_tls_config_before_creating_destination() {
    let temp = TempDir::new().unwrap();
    let local = temp.path().join("network-clone");
    let config_path = temp.path().join("bad-tls-config.toml");
    let missing_ca = temp.path().join("missing-ca.pem");
    std::fs::write(
        &config_path,
        format!(
            "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n\n[remote]\ntls_ca_certificate_path = \"{}\"\n",
            missing_ca.display()
        ),
    )
    .unwrap();

    let config = config_path.to_string_lossy().to_string();
    let local_arg = local.to_string_lossy().to_string();
    let output = heddle_output_with_env(
        &[
            "clone",
            "heddle://127.0.0.1:1/owner/repo",
            local_arg.as_str(),
        ],
        Some(temp.path()),
        &[("HEDDLE_CONFIG", &config)],
    )
    .expect("invoke network clone");

    assert!(!output.status.success(), "clone should fail closed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty(),
        "failed clone should not write stdout: {stdout}"
    );
    assert!(
        stderr.contains("fatal TLS/auth configuration error")
            && stderr.contains("remote.tls_ca_certificate_path"),
        "clone should fail on TLS config before transport: {stderr}"
    );
    assert!(
        !local.exists(),
        "TLS config failure must not create a partial clone destination at {}",
        local.display()
    );
}

#[test]
#[cfg(feature = "client")]
fn clone_network_removes_self_created_destination_after_later_failure() {
    let temp = TempDir::new().unwrap();
    let local = temp.path().join("network-clone-cleanup");
    let local_arg = local.to_string_lossy().to_string();

    let output = heddle_output(
        &[
            "clone",
            "heddle://127.0.0.1:1/owner/repo",
            local_arg.as_str(),
        ],
        Some(temp.path()),
    )
    .expect("invoke network clone");

    assert!(
        !output.status.success(),
        "clone should fail against a closed local port"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("fatal TLS/auth configuration error"),
        "default TLS/auth config should pass before the transport failure: {stderr}"
    );
    assert!(
        !local.exists(),
        "later network clone failure must remove the self-created destination at {}",
        local.display()
    );
}

#[test]
fn test_cli_clone_git_overlay_depth_is_rejected() {
    // Issue 49 / 20b: `--depth` is wired through to Sley at the wire
    // layer (`clone_url_to_bare` honours it), but the import step
    // (`import_all` ancestry walk) still requires every parent commit
    // locally. Until the importer tolerates missing parents, the
    // user-facing flag is rejected up-front so we never leave a
    // half-built clone behind.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    SleyRepository::init_bare(&origin).expect("init bare git origin");

    let err = heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
            "--depth",
            "1",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--depth") && err.contains("not yet supported"),
        "depth must be rejected with 'not yet supported': {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

#[test]
fn test_cli_clone_git_overlay_lazy_is_rejected() {
    // Issue 49 / 20b: same shape as --depth — `--lazy` (the
    // `--filter blob:none` synonym) gets rejected up-front because the
    // import step requires all blobs locally.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    SleyRepository::init_bare(&origin).expect("init bare git origin");

    let err = heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
            "--lazy",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--lazy") && err.contains("not yet supported"),
        "lazy must be rejected with 'not yet supported': {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

#[test]
fn test_cli_clone_git_overlay_missing_requested_branch_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");

    let src = SleyRepository::init_bare(&source).expect("init bare source");
    let _main_tip = git_commit_with_tree(
        &src,
        Some("refs/heads/main"),
        git_empty_tree_oid(&src),
        "seed main",
        &[],
    );
    std::fs::write(source.join("HEAD"), b"ref: refs/heads/main\n").unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            source.to_str().expect("source path utf8"),
            work.to_str().expect("work path utf8"),
            "--thread",
            "missing",
        ],
        None,
    )
    .expect("invoke missing git-overlay branch clone");
    assert!(
        !output.status.success(),
        "clone with missing requested Git branch should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing Git branch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        work.exists(),
        "Git-overlay import failure happens after clone preflight and should preserve the partial clone"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing Git branch should emit JSON envelope");
    assert_eq!(envelope["kind"], "git_overlay_clone_import_failed");
    let source_path = canonical_path_string(&source);
    let expected_action = format!("heddle clone {} <path> --thread missing", source_path);
    assert_eq!(envelope["primary_command"], expected_action);
    assert_eq!(
        envelope["primary_command_template"]["argv_template"],
        heddle_argv_json([
            "clone",
            source_path.as_str(),
            "<path>",
            "--thread",
            "missing"
        ]),
        "dynamic clone recovery should expose a central template for agents: {stderr}"
    );
    assert_eq!(
        envelope["primary_command_template"]["required_inputs"],
        serde_json::json!(["path"])
    );
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("requested ref(s) not found")),
        "missing Git branch should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("existing commit-pointing branch")),
        "missing Git branch hint should name the safe retry: {stderr}"
    );
}

/// heddle#141 + heddle#142 regression: cloning a git repo whose `HEAD`
/// points at a branch that is not alphabetically first must (1) land
/// the user on the remote's actual default branch and (2) leave
/// `heddle log` walking the imported history, not just a freshly
/// minted bootstrap state.
///
/// We exercise the local-overlay path (`copy_local_repo_to_bare`)
/// because it's hermetic. The URL-overlay path has its own unit-level
/// regression in `git_projection_engine::git_core::tests` that verifies
/// `clone_url_to_bare` mirrors the remote symref into `.git/HEAD`,
/// which is what feeds the same `select_clone_thread` selection
/// logic this test pins.
#[test]
fn test_cli_clone_git_overlay_lands_on_remote_default_branch_and_log_walks_history() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");

    // Build a bare git source with `trunk` (the default, with two
    // commits so we can confirm log walks history) and
    // `abc-feature` (alphabetically first — the trap heddle#141 used
    // to fall into). Branch names deliberately avoid `main`/`master`
    // so neither Git's `init.defaultBranch` nor the previous
    // fallback could land here by accident.
    let src = SleyRepository::init_bare(&source).expect("init bare source");
    let empty_tree = git_empty_tree_oid(&src);
    // Use explicit signatures for the seed commits so CI user config
    // does not affect this clone selection test.
    let commit_as = |message: &str, parents: Vec<ObjectId>| -> ObjectId {
        git_commit_with_tree(&src, None, empty_tree, message, &parents)
    };
    let trunk_first = commit_as("seed trunk", vec![]);
    let trunk_tip = commit_as("advance trunk", vec![trunk_first]);
    let abc_feature = commit_as("seed abc-feature", vec![]);
    git_set_reference(&src, "refs/heads/trunk", trunk_tip);
    git_set_reference(&src, "refs/heads/abc-feature", abc_feature);
    std::fs::write(source.join("HEAD"), b"ref: refs/heads/trunk\n").unwrap();

    let source_arg = source.to_str().expect("source path utf8");
    let work_arg = work.to_str().expect("work path utf8");
    let output = heddle(&["clone", source_arg, work_arg], None).expect("clone succeeds");
    assert!(
        output.contains("trunk"),
        "clone output should advertise the chosen branch (trunk): {output}"
    );

    // heddle#141: HEAD should land on `trunk`, not the
    // alphabetically-first `abc-feature`.
    let heddle_head =
        std::fs::read_to_string(work.join(".heddle").join("HEAD")).expect("read heddle HEAD");
    assert_eq!(
        heddle_head.trim(),
        "ref: trunk",
        "heddle HEAD must attach to the remote's default branch (trunk), \
         not the alphabetically-first imported branch (abc-feature) — \
         see heddle#141. Got: {heddle_head:?}"
    );

    // heddle#142: log must walk the imported history, not surface a
    // freshly-minted bootstrap state. Two trunk commits → two real
    // states (the synthetic `heddle init` root is filtered).
    let log_json = heddle(&["log", "--output", "json"], Some(&work)).expect("log succeeds");
    let parsed: serde_json::Value = serde_json::from_str(&log_json).expect("log json parses");
    let states = parsed
        .get("states")
        .and_then(|s| s.as_array())
        .expect("log output has a states array");
    assert!(
        states.len() >= 2,
        "heddle log should walk the imported trunk history (>=2 states), \
         not just a fresh bootstrap snapshot — see heddle#142. \
         States: {states:#?}"
    );
    let bootstrap_only = states.len() == 1
        && states[0]
            .get("intent")
            .and_then(|v| v.as_str())
            .is_some_and(|intent| intent.contains("Bootstrap git-overlay"));
    assert!(
        !bootstrap_only,
        "heddle log surfaced only the synthetic bootstrap state — \
         this is the heddle#142 failure mode. States: {states:#?}"
    );

    let git_status = Command::new("git")
        .args(["-C", work_arg, "status", "--short"])
        .output()
        .expect("git status");
    assert!(
        git_status.status.success(),
        "git status should succeed after clone: {}",
        String::from_utf8_lossy(&git_status.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_status.stdout),
        "",
        "git-overlay clone should leave a Git-clean checkout"
    );

    let git_branch = Command::new("git")
        .args(["-C", work_arg, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git branch");
    assert!(
        git_branch.status.success(),
        "git branch should succeed after clone: {}",
        String::from_utf8_lossy(&git_branch.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_branch.stdout).trim(),
        "trunk",
        "Git HEAD should match the active Heddle thread"
    );
}

fn git_tree_with_file(repo: &SleyRepository, path: &str, content: &[u8]) -> ObjectId {
    let blob = repo.write_blob(content).expect("write git blob");
    let empty = git_empty_tree_oid(repo);
    let mut editor = repo.edit_tree(&empty).expect("edit git tree");
    editor.upsert(path, EntryKind::Blob, blob);
    repo.write_tree(editor).expect("write git tree")
}

fn git_ok(args: &[&str], cwd: &std::path::Path) {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout_trimmed(args: &[&str], cwd: &std::path::Path) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn test_cli_clone_git_overlay_sets_origin_tracking_for_selected_branch() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");
    let src = SleyRepository::init_bare(&source).expect("init bare source");

    let tree = git_tree_with_file(&src, "tracked.txt", b"one\n");
    let main = git_commit_with_tree(&src, Some("refs/heads/main"), tree, "one", &[]);
    std::fs::write(source.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let source_arg = source.to_str().expect("source path utf8");
    let work_arg = work.to_str().expect("work path utf8");
    heddle(&["clone", source_arg, work_arg], Some(temp.path())).expect("clone succeeds");

    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &work),
        main.to_string(),
        "Git-overlay clone should seed origin/main at the cloned remote tip"
    );
    let branch_status = git_stdout_trimmed(&["status", "--short", "--branch"], &work);
    assert!(
        branch_status.contains("## main...origin/main"),
        "git status should show local main tracking origin/main after clone: {branch_status}"
    );
}

#[test]
fn test_cli_clone_git_overlay_rewrites_origin_and_default_pull_keeps_git_clean() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let stale_origin = temp.path().join("stale-origin.git");
    let work = temp.path().join("work");
    let src = SleyRepository::init_bare(&source).expect("init bare source");

    let first_tree = git_tree_with_file(&src, "tracked.txt", b"one\n");
    let first = git_commit_with_tree(&src, Some("refs/heads/main"), first_tree, "one", &[]);
    std::fs::write(source.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let mut source_config = std::fs::read_to_string(source.join("config")).unwrap();
    source_config.push_str(&format!(
        "\n[remote \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n",
        stale_origin.display()
    ));
    std::fs::write(source.join("config"), source_config).unwrap();

    let source_arg = "source.git";
    let canonical_source = source.canonicalize().expect("canonical source");
    let canonical_source_arg = canonical_source
        .to_str()
        .expect("canonical source path utf8");
    let work_arg = work.to_str().expect("work path utf8");
    heddle(&["clone", source_arg, work_arg], Some(temp.path())).expect("clone succeeds");

    let origin = Command::new("git")
        .args(["-C", work_arg, "config", "--get", "remote.origin.url"])
        .output()
        .expect("read clone origin");
    assert!(
        origin.status.success(),
        "git config should read clone origin: {}",
        String::from_utf8_lossy(&origin.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&origin.stdout).trim(),
        canonical_source_arg,
        "heddle clone must point origin at the cloned source, not inherit the source repo's stale origin"
    );

    let second_tree = git_tree_with_file(&src, "tracked.txt", b"two\n");
    let second = git_commit_with_tree(&src, Some("refs/heads/main"), second_tree, "two", &[first]);

    let pull_json =
        heddle(&["--output", "json", "pull"], Some(&work)).expect("default pull succeeds");
    let pull: Value = inject_post_verification_at(
        &work,
        serde_json::from_str(&pull_json).expect("pull JSON parses"),
    );
    assert_eq!(pull["output_kind"], "pull");
    assert_eq!(pull["action"], "pull");
    assert_eq!(pull["status"], "updated");
    assert_eq!(pull["transport"], "git");
    assert_eq!(pull["remote"], "origin");
    assert_eq!(pull["branch"], "main");
    assert_eq!(pull["old_git_head"], first.to_string());
    assert_eq!(pull["new_git_head"], second.to_string());
    assert_eq!(pull["changed_path_count"], 1);
    assert_eq!(pull["changed_paths"], serde_json::json!(["tracked.txt"]));
    assert_eq!(pull["states_created"], 1);
    assert_eq!(pull["commits_seen_scope"], "branches_and_heddle_notes");
    assert_eq!(pull["materialized_checkout"], true);
    assert_eq!(
        pull["verification"]["worktree_state"], "clean",
        "pull should write through to Git instead of leaving a checkpoint-needed checkout: {pull_json}"
    );
    assert_ne!(pull["verification"]["status"], "needs_checkpoint");

    assert_eq!(
        std::fs::read_to_string(work.join("tracked.txt")).unwrap(),
        "two\n"
    );

    let git_head = Command::new("git")
        .args(["-C", work_arg, "rev-parse", "HEAD"])
        .output()
        .expect("git rev-parse HEAD");
    assert!(
        git_head.status.success(),
        "git rev-parse HEAD should succeed: {}",
        String::from_utf8_lossy(&git_head.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_head.stdout).trim(),
        second.to_string(),
        "Git HEAD should advance to the pulled commit"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &work),
        second.to_string(),
        "pull must refresh the checkout's remote-tracking ref so Git does not report stale upstream drift"
    );

    let git_branch_status = Command::new("git")
        .args(["-C", work_arg, "status", "-sb"])
        .output()
        .expect("git status -sb");
    assert!(
        git_branch_status.status.success(),
        "git status -sb should succeed after pull: {}",
        String::from_utf8_lossy(&git_branch_status.stderr)
    );
    let branch_status = String::from_utf8_lossy(&git_branch_status.stdout);
    assert!(
        !branch_status.contains("[ahead"),
        "pull should not leave Git believing local main is ahead of origin/main: {branch_status}"
    );

    let git_status = Command::new("git")
        .args(["-C", work_arg, "status", "--short"])
        .output()
        .expect("git status");
    assert!(
        git_status.status.success(),
        "git status should succeed after pull: {}",
        String::from_utf8_lossy(&git_status.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_status.stdout),
        "",
        "default pull should leave the Git checkout clean"
    );

    let verify = verify_json(&work);
    assert_eq!(
        verify["status"], "clean",
        "verify should be clean: {verify}"
    );
    assert_eq!(
        verify["remote_drift"], "clean",
        "verify must not recommend push after a successful pull: {verify}"
    );

    let pull_text =
        heddle(&["pull", "--output", "text"], Some(&work)).expect("up-to-date pull text succeeds");
    assert!(
        pull_text.contains("already up to date with")
            && pull_text.contains("Branch:")
            && pull_text.contains("Imported:")
            && pull_text.contains("Workspace: verified"),
        "pull text should explain branch/import/verification context even when up to date: {pull_text}"
    );
}

#[test]
fn test_cli_git_overlay_fetch_refreshes_tracking_ref_and_verify_reports_behind() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");
    let src = SleyRepository::init_bare(&source).expect("init bare source");

    let first_tree = git_tree_with_file(&src, "tracked.txt", b"one\n");
    let first = git_commit_with_tree(&src, Some("refs/heads/main"), first_tree, "one", &[]);
    std::fs::write(source.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let source_arg = source.to_str().expect("source path utf8");
    let work_arg = work.to_str().expect("work path utf8");
    heddle(&["clone", source_arg, work_arg], Some(temp.path())).expect("clone succeeds");

    let second_tree = git_tree_with_file(&src, "tracked.txt", b"two\n");
    let second = git_commit_with_tree(&src, Some("refs/heads/main"), second_tree, "two", &[first]);
    git_set_reference(&src, "refs/tags/v2.0", second);
    git_set_reference(
        &src,
        cli::git_projection_engine::git_notes::NOTES_REF,
        second,
    );

    let fetch_json =
        heddle(&["--output", "json", "fetch", "origin"], Some(&work)).expect("fetch succeeds");
    let fetch: Value = inject_post_verification_at(
        &work,
        serde_json::from_str(&fetch_json).expect("fetch JSON parses"),
    );
    assert_eq!(fetch["ref_scope"], "branches_and_heddle_notes", "{fetch}");
    assert_eq!(fetch["tags_included"], false, "{fetch}");
    assert_eq!(
        fetch["verification"]["remote_drift"], "remote_behind",
        "fetch should immediately surface fetched upstream drift: {fetch}"
    );
    assert_eq!(
        fetch["verification"]["recommended_action"], "heddle pull",
        "fetched behind state should recommend a Heddle pull: {fetch}"
    );

    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &work),
        second.to_string(),
        "fetch must refresh the checkout's remote-tracking ref so verify/status can see behind drift"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "HEAD"], &work),
        first.to_string(),
        "fetch must not move the local checkout HEAD"
    );

    let mirror = open_git(work.join(".heddle").join("git")).expect("open legacy Bridge Mirror");
    assert!(
        find_reference(&mirror, cli::git_projection_engine::git_notes::NOTES_REF).is_ok(),
        "fetch should carry refs/notes/heddle for Heddle identity metadata"
    );
    assert_eq!(
        git_stdout_trimmed(
            &[
                "rev-parse",
                cli::git_projection_engine::git_notes::NOTES_REF
            ],
            &work
        ),
        second.to_string(),
        "fetch should refresh the checkout's refs/notes/heddle when it reports fetching Heddle notes"
    );
    assert!(
        find_reference(&mirror, "refs/tags/v2.0").is_err(),
        "default Git-overlay fetch should not import arbitrary Git tags"
    );

    let status_json = heddle(&["--output", "json", "status"], Some(&work)).unwrap();
    let status: Value = serde_json::from_str(&status_json).expect("status JSON parses");
    assert_eq!(
        status["verification"]["remote_drift"], "remote_behind",
        "status should not report clean once fetched upstream is ahead: {status}"
    );

    let verify = verify_json(&work);
    assert_eq!(verify["status"], "remote_behind", "{verify}");
    assert_eq!(verify["verified"], false, "{verify}");
    assert_eq!(verify["remote_drift"], "remote_behind", "{verify}");
    assert_eq!(verify["recommended_action"], "heddle pull", "{verify}");
}

#[test]
fn test_cli_git_overlay_fetch_resolves_relative_local_git_remote_from_checkout_root() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let other = temp.path().join("other");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    std::fs::create_dir_all(&work).unwrap();
    git_ok(&["init", "-b", "main"], &work);
    git_ok(&["config", "user.name", "Heddle Test"], &work);
    git_ok(&["config", "user.email", "heddle@example.com"], &work);
    std::fs::write(work.join("README.md"), "one\n").unwrap();
    git_ok(&["add", "README.md"], &work);
    git_ok(&["commit", "-m", "initial"], &work);
    heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt succeeds");
    heddle(&["remote", "add", "origin", "../origin.git"], Some(&work))
        .expect("remote add succeeds");
    heddle(&["push"], Some(&work)).expect("push to relative local git remote succeeds");

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            other.to_str().expect("other path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Remote Test"], &other);
    git_ok(&["config", "user.email", "remote@example.com"], &other);
    std::fs::write(other.join("remote.txt"), "two\n").unwrap();
    git_ok(&["add", "remote.txt"], &other);
    git_ok(&["commit", "-m", "remote advance"], &other);
    git_ok(&["push", "origin", "main"], &other);
    let remote_tip = git_stdout_trimmed(&["rev-parse", "origin/main"], &other);

    let fetch_json =
        heddle(&["--output", "json", "fetch", "origin"], Some(&work)).expect("fetch succeeds");
    let fetch: Value = inject_post_verification_at(
        &work,
        serde_json::from_str(&fetch_json).expect("fetch JSON parses"),
    );
    assert_eq!(
        fetch["verification"]["remote_drift"], "remote_behind",
        "fetch should resolve ../origin.git relative to the checkout root and surface behind drift: {fetch}"
    );
    assert_eq!(
        fetch["verification"]["recommended_action"], "heddle pull",
        "fetched behind state should recommend Heddle pull: {fetch}"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &work),
        remote_tip,
        "fetch should refresh the checkout remote-tracking ref from the relative local remote"
    );
}

#[test]
fn test_cli_git_overlay_fetch_uses_configured_default_not_origin_fallback() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");
    let missing_origin = temp.path().join("missing-origin.git");
    let src = SleyRepository::init_bare(&source).expect("init bare source");

    let first_tree = git_tree_with_file(&src, "tracked.txt", b"one\n");
    let first = git_commit_with_tree(&src, Some("refs/heads/main"), first_tree, "one", &[]);
    std::fs::write(source.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    heddle(
        &[
            "clone",
            source.to_str().expect("source path utf8"),
            work.to_str().expect("work path utf8"),
        ],
        Some(temp.path()),
    )
    .expect("clone succeeds");
    heddle(
        &[
            "remote",
            "add",
            "backup",
            source.to_str().expect("source path utf8"),
        ],
        Some(&work),
    )
    .expect("add backup remote");
    heddle(&["remote", "set-default", "backup"], Some(&work)).expect("set backup default");

    git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            missing_origin.to_str().expect("missing path utf8"),
        ],
        &work,
    );

    let second_tree = git_tree_with_file(&src, "tracked.txt", b"two\n");
    let second = git_commit_with_tree(&src, Some("refs/heads/main"), second_tree, "two", &[first]);

    let fetch_json = heddle(&["--output", "json", "fetch"], Some(&work))
        .expect("bare fetch should use configured default backup, not bad origin");
    let fetch: Value = inject_post_verification_at(
        &work,
        serde_json::from_str(&fetch_json).expect("fetch JSON parses"),
    );
    assert_eq!(
        fetch["remote"], "backup",
        "no-arg Git-overlay fetch should honor Heddle's configured default remote: {fetch}"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/backup/main"], &work),
        second.to_string(),
        "fetch should refresh the selected default remote's tracking ref"
    );
    assert_eq!(
        fetch["verification"]["default_remote"], "backup",
        "post-fetch verification should carry the same configured default remote: {fetch}"
    );
    assert!(
        !missing_origin.exists(),
        "test fixture should keep origin broken so an origin fallback would fail"
    );
}

#[test]
fn test_cli_git_overlay_remote_add_does_not_steal_tracked_branch_default() {
    let fixture = GitOverlayFixture::imported_main().with_bare_origin();
    let work = fixture.path();
    let origin = fixture.origin_path();
    let backup = TempDir::new().unwrap();
    SleyRepository::init_bare(backup.path()).expect("init bare backup");
    std::fs::write(backup.path().join("HEAD"), "ref: refs/heads/main\n").unwrap();

    heddle(
        &[
            "remote",
            "add",
            "backup",
            backup.path().to_str().expect("backup path utf8"),
        ],
        Some(work),
    )
    .expect("add backup remote");
    let list_json = heddle(&["remote", "list", "--output", "json"], Some(work)).unwrap();
    let list: Value = serde_json::from_str(&list_json).expect("remote list JSON parses");
    assert!(
        list["remotes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|remote| remote["name"] == "origin" && remote["is_default"] == true),
        "tracked Git upstream should remain the default after adding backup: {list}"
    );
    assert!(
        list["remotes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|remote| remote["name"] == "backup" && remote["is_default"] == false),
        "new backup remote should not silently become default: {list}"
    );

    std::fs::write(work.join("README.md"), "base\nlocal heddle\n").unwrap();
    let commit_json = heddle(
        &["commit", "-m", "local heddle", "--output", "json"],
        Some(work),
    )
    .expect("heddle commit succeeds");
    let commit: Value = serde_json::from_str(&commit_json).expect("commit JSON parses");
    let git_oid = commit["git_commit"]
        .as_str()
        .expect("commit should report Git OID")
        .to_string();

    let push_json = heddle(&["push", "--output", "json"], Some(work)).expect("push succeeds");
    let push: Value = serde_json::from_str(&push_json).expect("push JSON parses");
    assert_eq!(
        push["remote"], "origin",
        "bare push should use tracked origin: {push}"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/heads/main"], origin),
        git_oid,
        "origin should receive the default push"
    );
    assert!(
        !Command::new("git")
            .args([
                "--git-dir",
                backup.path().to_str().unwrap(),
                "show-ref",
                "--verify",
                "refs/heads/main"
            ])
            .status()
            .expect("inspect backup ref")
            .success(),
        "backup should not receive a bare push unless selected explicitly"
    );
}

#[test]
fn test_cli_git_overlay_current_push_carries_notes_for_cross_clone_identity() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let clone = temp.path().join("clone");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    std::fs::create_dir_all(&work).unwrap();
    git_ok(&["init", "-b", "main"], &work);
    git_ok(&["config", "user.name", "Heddle Test"], &work);
    git_ok(&["config", "user.email", "heddle@example.com"], &work);
    git_ok(
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("origin path utf8"),
        ],
        &work,
    );
    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], &work);
    git_ok(&["commit", "-m", "seed"], &work);

    heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt seeded Git repo");
    std::fs::write(work.join("README.md"), "seed\nfirst heddle change\n").unwrap();
    let commit_json = heddle(
        &["--output", "json", "commit", "-m", "First Heddle change"],
        Some(&work),
    )
    .expect("heddle commit succeeds");
    let commit: Value = serde_json::from_str(&commit_json).expect("commit JSON parses");
    let first_state = commit["state_id"]
        .as_str()
        .expect("commit should report state_id")
        .to_string();

    let push_text = heddle(&["push", "origin"], Some(&work)).expect("current-thread push succeeds");
    assert!(
        push_text.contains("refs/notes/heddle")
            && push_text.contains("git log --all")
            && push_text.contains("Heddle metadata commits"),
        "push text should disclose the Git-visible notes ref Heddle publishes: {push_text}"
    );
    git_ok(&["show-ref", "--verify", "refs/notes/heddle"], &origin);

    heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            clone.to_str().expect("clone path utf8"),
        ],
        None,
    )
    .expect("clone succeeds");
    let clone_status_json = heddle(&["--output", "json", "status"], Some(&clone)).unwrap();
    let clone_status: Value = serde_json::from_str(&clone_status_json).expect("status JSON parses");
    assert_eq!(
        clone_status["state"]["state_id"], first_state,
        "clone should preserve the note-backed Heddle state id instead of deriving a second id"
    );

    std::fs::write(
        work.join("README.md"),
        "seed\nfirst heddle change\nsecond heddle change\n",
    )
    .unwrap();
    heddle(
        &["--output", "json", "commit", "-m", "Second Heddle change"],
        Some(&work),
    )
    .expect("second heddle commit succeeds");
    heddle(&["push", "origin"], Some(&work)).expect("second current-thread push succeeds");

    let pull_json = heddle(&["--output", "json", "pull", "origin"], Some(&clone))
        .expect("clone pull should not hit mapping conflict");
    let pull: Value = inject_post_verification_at(
        &clone,
        serde_json::from_str(&pull_json).expect("pull JSON parses"),
    );
    assert_eq!(
        pull["verification"]["status"], "clean",
        "pull should preserve cross-clone Git/Heddle mapping agreement: {pull}"
    );
}

#[test]
fn test_cli_git_overlay_explicit_path_push_discloses_configured_git_tracking_remote() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    std::fs::create_dir_all(&work).unwrap();
    git_ok(&["init", "-b", "main"], &work);
    git_ok(&["config", "user.name", "Heddle Test"], &work);
    git_ok(&["config", "user.email", "heddle@example.com"], &work);
    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], &work);
    git_ok(&["commit", "-m", "seed"], &work);

    heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt seeded Git repo");
    std::fs::write(work.join("README.md"), "seed\nlocal heddle\n").unwrap();
    heddle(&["commit", "-m", "local heddle"], Some(&work)).expect("heddle commit succeeds");

    let origin_arg = origin.to_str().expect("origin path utf8");
    let push_text = heddle(&["--output", "text", "push", origin_arg], Some(&work))
        .expect("explicit path push succeeds");
    assert!(
        push_text.contains("configured remote origin")
            && push_text.contains(origin_arg)
            && push_text.contains("branch main tracks origin/main"),
        "explicit-path push should disclose the Git config side effect: {push_text}"
    );
    assert_eq!(
        git_stdout_trimmed(&["config", "--get", "remote.origin.url"], &work),
        origin_arg,
        "push should have configured the same remote it disclosed"
    );
}

#[test]
fn test_cli_git_overlay_explicit_path_push_json_reports_configured_git_tracking_remote() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    std::fs::create_dir_all(&work).unwrap();
    git_ok(&["init", "-b", "main"], &work);
    git_ok(&["config", "user.name", "Heddle Test"], &work);
    git_ok(&["config", "user.email", "heddle@example.com"], &work);
    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], &work);
    git_ok(&["commit", "-m", "seed"], &work);

    heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt seeded Git repo");
    std::fs::write(work.join("README.md"), "seed\nlocal heddle\n").unwrap();
    heddle(&["commit", "-m", "local heddle"], Some(&work)).expect("heddle commit succeeds");

    let origin_arg = origin.to_str().expect("origin path utf8");
    let push_json = heddle(&["--output", "json", "push", origin_arg], Some(&work))
        .expect("explicit path JSON push succeeds");
    let push: Value = inject_post_verification_at(
        &work,
        serde_json::from_str(&push_json).expect("push JSON parses"),
    );
    assert_eq!(push["action"], "push");
    assert_eq!(push["remote"], origin_arg);
    assert_eq!(push["git_tracking_remote"], "origin");
    assert_eq!(push["git_remote_configured"]["name"], "origin");
    assert_eq!(push["git_remote_configured"]["url"], origin_arg);
    assert_eq!(push["git_upstream_configured"]["branch"], "main");
    assert_eq!(push["git_upstream_configured"]["remote"], "origin");
    assert_eq!(push["verification"]["status"], "clean");
}

#[test]
fn test_cli_raw_git_clone_adopt_fetches_notes_before_import() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let raw_clone = temp.path().join("raw-clone");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    std::fs::create_dir_all(&work).unwrap();
    git_ok(&["init", "-b", "main"], &work);
    git_ok(&["config", "user.name", "Heddle Test"], &work);
    git_ok(&["config", "user.email", "heddle@example.com"], &work);
    git_ok(
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("origin path utf8"),
        ],
        &work,
    );
    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], &work);
    git_ok(&["commit", "-m", "seed"], &work);

    heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt seeded Git repo");
    std::fs::write(work.join("README.md"), "seed\npublished by heddle\n").unwrap();
    let first_commit_json = heddle(
        &[
            "--output",
            "json",
            "commit",
            "-m",
            "Publish Heddle identity",
        ],
        Some(&work),
    )
    .expect("first Heddle commit succeeds");
    let first_commit: Value = serde_json::from_str(&first_commit_json).expect("commit JSON parses");
    let first_state = first_commit["state_id"]
        .as_str()
        .expect("commit reports change id")
        .to_string();
    heddle(&["push", "origin"], Some(&work)).expect("current-thread push succeeds");
    git_ok(&["show-ref", "--verify", "refs/notes/heddle"], &origin);

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            raw_clone.to_str().expect("raw clone path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Raw Clone"], &raw_clone);
    git_ok(&["config", "user.email", "raw@example.com"], &raw_clone);
    assert!(
        !Command::new("git")
            .args(["show-ref", "--verify", "refs/notes/heddle"])
            .current_dir(&raw_clone)
            .output()
            .expect("inspect raw clone notes ref")
            .status
            .success(),
        "plain git clone should start without the Heddle notes ref; adopt must fetch it"
    );

    heddle(&["adopt"], Some(&raw_clone))
        .expect("raw Git clone adopt should fetch notes before importing");
    let raw_status_json = heddle(&["--output", "json", "status"], Some(&raw_clone)).unwrap();
    let raw_status: Value = serde_json::from_str(&raw_status_json).expect("status JSON parses");
    assert_eq!(
        raw_status["state"]["state_id"], first_state,
        "raw Git clone adoption should reuse note-backed Heddle identity instead of deriving a second id"
    );
    git_ok(&["show-ref", "--verify", "refs/notes/heddle"], &raw_clone);
    assert!(
        !raw_clone.join(".heddle").join("git").exists(),
        "unscoped raw Git clone adopt should hydrate notes without creating the legacy mirror"
    );

    std::fs::write(
        raw_clone.join("README.md"),
        "seed\npublished by heddle\nraw clone follow-up\n",
    )
    .unwrap();
    heddle(
        &["--output", "json", "commit", "-m", "Raw clone follow-up"],
        Some(&raw_clone),
    )
    .expect("raw clone Heddle commit succeeds");
    heddle(&["push", "origin"], Some(&raw_clone)).expect("raw clone push succeeds");

    heddle(&["fetch", "origin"], Some(&work)).expect("original fetch succeeds");
    let pull_json = heddle(&["--output", "json", "pull", "origin"], Some(&work))
        .expect("original pull should not hit a mapping conflict");
    let pull: Value = inject_post_verification_at(
        &work,
        serde_json::from_str(&pull_json).expect("pull JSON parses"),
    );
    assert_eq!(
        pull["verification"]["status"], "clean",
        "pull should preserve cross-clone Git/Heddle mapping agreement after raw clone adoption: {pull}"
    );
}

#[test]
fn test_cli_git_overlay_push_refuses_to_rewrite_remote_heddle_notes() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let raw_clone = temp.path().join("raw-clone");
    let missing_remote = temp.path().join("missing.git");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    std::fs::create_dir_all(&work).unwrap();
    git_ok(&["init", "-b", "main"], &work);
    git_ok(&["config", "user.name", "Heddle Test"], &work);
    git_ok(&["config", "user.email", "heddle@example.com"], &work);
    git_ok(
        &[
            "remote",
            "add",
            "origin",
            origin.to_str().expect("origin path utf8"),
        ],
        &work,
    );
    std::fs::write(work.join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], &work);
    git_ok(&["commit", "-m", "seed"], &work);
    heddle(&["adopt", "--ref", "main"], Some(&work)).expect("adopt seeded Git repo");
    std::fs::write(work.join("README.md"), "seed\npublished by heddle\n").unwrap();
    heddle(
        &[
            "--output",
            "json",
            "commit",
            "-m",
            "Publish Heddle identity",
        ],
        Some(&work),
    )
    .expect("first Heddle commit succeeds");
    heddle(&["push", "origin"], Some(&work)).expect("initial push succeeds");
    let remote_notes_before = git_stdout_trimmed(&["rev-parse", "refs/notes/heddle"], &origin);

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            raw_clone.to_str().expect("raw clone path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Raw Clone"], &raw_clone);
    git_ok(&["config", "user.email", "raw@example.com"], &raw_clone);

    git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            missing_remote.to_str().expect("missing path utf8"),
        ],
        &raw_clone,
    );
    heddle(&["adopt", "--ref", "main"], Some(&raw_clone))
        .expect("offline raw Git clone adopt can still import local Git history");
    git_ok(
        &[
            "remote",
            "set-url",
            "origin",
            origin.to_str().expect("origin path utf8"),
        ],
        &raw_clone,
    );

    let output = heddle_output(&["--output", "json", "push", "origin"], Some(&raw_clone))
        .expect("invoke push with mismatched local notes");
    assert!(
        !output.status.success(),
        "push must fail closed instead of rewriting remote Heddle notes"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode push refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("notes conflict should emit JSON envelope");
    assert_eq!(envelope["kind"], "git_overlay_note_ref_conflict");
    assert_eq!(
        envelope["primary_command"], "heddle fetch",
        "notes conflict should first refresh remote Heddle notes before asking for a fresh clone: {stderr}"
    );
    assert_ne!(
        envelope["primary_command"], "heddle pull",
        "notes conflict must not recommend retrying the operation that cannot repair identity"
    );
    assert_eq!(
        envelope["recovery_commands"],
        serde_json::json!([
            "heddle fetch",
            "heddle push",
            "heddle clone <remote> <fresh-path>"
        ]),
        "notes conflict should keep fresh clone as the fallback after fetch/retry: {stderr}"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/notes/heddle"], &origin),
        remote_notes_before,
        "failed push must leave remote refs/notes/heddle unchanged"
    );
}

#[test]
fn test_cli_git_overlay_sync_refuses_diverged_branch_before_rebase() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let local = temp.path().join("local");
    let peer = temp.path().join("peer");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            local.to_str().expect("local path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Heddle Test"], &local);
    git_ok(&["config", "user.email", "heddle@example.com"], &local);
    git_ok(&["checkout", "-b", "main"], &local);
    std::fs::write(local.join("file.txt"), "base\n").unwrap();
    git_ok(&["add", "file.txt"], &local);
    git_ok(&["commit", "-m", "seed"], &local);
    git_ok(&["push", "-u", "origin", "main"], &local);
    heddle(&["adopt", "--ref", "main"], Some(&local)).expect("adopt local");

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            peer.to_str().expect("peer path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Peer"], &peer);
    git_ok(&["config", "user.email", "peer@example.com"], &peer);
    git_ok(&["checkout", "main"], &peer);

    std::fs::write(local.join("file.txt"), "local heddle\n").unwrap();
    heddle(
        &["--output", "json", "commit", "-m", "local heddle commit"],
        Some(&local),
    )
    .expect("local Heddle commit");
    let head_before = git_stdout_trimmed(&["rev-parse", "HEAD"], &local);

    std::fs::write(peer.join("file.txt"), "remote git\n").unwrap();
    git_ok(&["add", "file.txt"], &peer);
    git_ok(&["commit", "-m", "remote git commit"], &peer);
    git_ok(&["push", "origin", "main"], &peer);

    let push = heddle_output(&["push", "--output", "json"], Some(&local)).expect("invoke push");
    assert!(
        !push.status.success(),
        "diverged push should fail before rewriting remote work"
    );
    assert!(
        push.stdout.is_empty(),
        "JSON refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&push.stdout)
    );
    let push_stderr = std::str::from_utf8(&push.stderr).expect("push stderr utf8");
    let push_envelope: Value = serde_json::from_str(push_stderr).expect("push refusal JSON parses");
    assert_eq!(
        push_envelope["kind"], "git_overlay_remote_diverged",
        "{push_envelope}"
    );
    assert_eq!(
        push_envelope["primary_command"], "heddle fetch",
        "push should guide users to refresh the remote proof before choosing an integration path: {push_envelope}"
    );

    heddle(&["fetch", "origin"], Some(&local)).expect("fetch remote divergence");

    let verify = verify_json(&local);
    assert_eq!(verify["remote_drift"], "remote_diverged", "{verify}");
    assert_eq!(
        verify["recommended_action"], "heddle import git --ref origin/main",
        "diverged verify should recommend importing the fetched upstream tip before previewing integration: {verify}"
    );
    let short_status = heddle(&["status", "--short", "--output", "text"], Some(&local))
        .expect("short status should render");
    assert!(
        short_status.contains("remote_diverged")
            && !short_status.contains("repository clean")
            && !short_status.contains("main clean"),
        "short status must not claim clean when remote drift blocks verification: {short_status}"
    );

    let sync_json = heddle(&["--output", "json", "sync"], Some(&local)).unwrap();
    let sync: Value = serde_json::from_str(&sync_json).expect("sync JSON parses");
    assert_eq!(sync["status"], "blocked", "{sync}");
    assert_eq!(
        sync["recommended_action"], "heddle import git --ref origin/main",
        "sync should fail closed before invoking raw git rebase and point at remote integration: {sync}"
    );
    let neutral_preview_json = heddle(
        &[
            "--output",
            "json",
            "fsck",
            "repair",
            "git",
            "--ref",
            "main",
            "--preview",
        ],
        Some(&local),
    )
    .expect("neutral reconcile preview should succeed");
    let neutral_preview: Value =
        serde_json::from_str(&neutral_preview_json).expect("neutral preview JSON parses");
    assert_eq!(neutral_preview["valid"], true, "{neutral_preview}");
    assert_eq!(neutral_preview["repair_target"], "git", "{neutral_preview}");
    assert_eq!(
        neutral_preview["repaired"], false,
        "neutral local reconcile preview must not mutate any side: {neutral_preview}"
    );
    let neutral_repairs = neutral_preview["repairs"]
        .as_array()
        .expect("preview should report the authority-valid repair");
    assert_eq!(
        neutral_repairs.len(),
        1,
        "preview should report only the Git-owned repair direction: {neutral_preview}"
    );
    assert!(
        neutral_repairs
            .iter()
            .all(|repair| repair["repaired"] == false),
        "neutral preview must leave every repair choice unapplied: {neutral_preview}"
    );
    assert_eq!(
        neutral_repairs[0]["detail"], "heddle fsck repair git --prefer git --ref main",
        "preview should derive direction from Git Overlay authority"
    );
    let authority_default = heddle(
        &["--output", "json", "fsck", "repair", "git", "--ref", "main"],
        Some(&local),
    )
    .expect("authority-derived reconcile should succeed");
    let authority_default: Value =
        serde_json::from_str(&authority_default).expect("authority repair JSON parses");
    assert_eq!(authority_default["valid"], true, "{authority_default}");
    let import_remote_json = heddle(
        &["--output", "json", "import", "git", "--ref", "origin/main"],
        Some(&local),
    )
    .expect("import fetched upstream branch");
    let import_remote: Value =
        serde_json::from_str(&import_remote_json).expect("remote import JSON parses");
    assert_eq!(import_remote["branches_synced"], 1, "{import_remote}");
    let after_import = verify_json(&local);
    assert_eq!(
        after_import["recommended_action"], "heddle fsck repair git --ref origin/main --preview",
        "after importing the upstream tip, verify should recommend upstream integration, not local Git/Heddle reconcile: {after_import}"
    );
    let thread_list_json = heddle(&["thread", "list", "--output", "json"], Some(&local))
        .expect("thread list should render after remote-tracking import");
    let thread_list: Value =
        serde_json::from_str(&thread_list_json).expect("thread list JSON parses");
    let origin_main = thread_list["threads"]
        .as_array()
        .expect("threads array")
        .iter()
        .find(|thread| thread["name"] == "origin/main")
        .unwrap_or_else(|| {
            panic!("imported origin/main should be listed as an imported ref: {thread_list}")
        });
    assert_eq!(
        origin_main["thread_health"], "remote_tracking",
        "{thread_list}"
    );
    assert_eq!(
        origin_main["recommended_action"], "heddle fsck repair git --ref origin/main --preview",
        "remote-tracking refs should be presented as upstream integration previews: {thread_list}"
    );
    assert!(
        origin_main["recommended_action"]
            .as_str()
            .is_some_and(|action| !action.contains("land")
                && action.contains("fsck repair git --ref origin/main --preview")),
        "remote-tracking refs must avoid dead-end land advice: {thread_list}"
    );
    let merge_preview = heddle(
        &["merge", "origin/main", "--preview", "--output", "text"],
        Some(&local),
    )
    .expect("remote-tracking merge preview should render");
    assert!(
        merge_preview.contains("Would merge origin/main")
            && !merge_preview.contains("Preview complete"),
        "merge preview should explain the upstream integration, not emit a generic completion line: {merge_preview}"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "--abbrev-ref", "HEAD"], &local),
        "main",
        "sync refusal must not detach the Git checkout"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "HEAD"], &local),
        head_before,
        "sync refusal must not move the local branch"
    );
    assert_eq!(
        std::fs::read_to_string(local.join("file.txt")).unwrap(),
        "local heddle\n",
        "sync refusal must not write conflict markers or remote content into the worktree"
    );
}

#[test]
fn test_cli_git_overlay_pull_refuses_diverged_branch_before_visible_git_updates() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let local = temp.path().join("local");
    let peer = temp.path().join("peer");
    SleyRepository::init_bare(&origin).expect("init bare origin");
    std::fs::write(origin.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            local.to_str().expect("local path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Heddle Test"], &local);
    git_ok(&["config", "user.email", "heddle@example.com"], &local);
    git_ok(&["checkout", "-b", "main"], &local);
    std::fs::write(local.join("file.txt"), "base\n").unwrap();
    git_ok(&["add", "file.txt"], &local);
    git_ok(&["commit", "-m", "seed"], &local);
    git_ok(&["push", "-u", "origin", "main"], &local);
    let tracking_before = git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &local);
    heddle(&["adopt", "--ref", "main"], Some(&local)).expect("adopt local");

    git_ok(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            peer.to_str().expect("peer path utf8"),
        ],
        temp.path(),
    );
    git_ok(&["config", "user.name", "Peer"], &peer);
    git_ok(&["config", "user.email", "peer@example.com"], &peer);
    git_ok(&["checkout", "main"], &peer);

    std::fs::write(local.join("file.txt"), "local heddle\n").unwrap();
    heddle(
        &["--output", "json", "commit", "-m", "local heddle commit"],
        Some(&local),
    )
    .expect("local Heddle commit");
    let head_before = git_stdout_trimmed(&["rev-parse", "HEAD"], &local);

    std::fs::write(peer.join("file.txt"), "remote git\n").unwrap();
    git_ok(&["add", "file.txt"], &peer);
    git_ok(&["commit", "-m", "remote git commit"], &peer);
    git_ok(&["push", "origin", "main"], &peer);

    let pull = heddle_output(&["pull", "--output", "json"], Some(&local)).expect("invoke pull");
    assert!(!pull.status.success(), "diverged pull should fail closed");
    assert!(
        pull.stdout.is_empty(),
        "JSON refusal should keep stdout quiet: {}",
        String::from_utf8_lossy(&pull.stdout)
    );
    let stderr = std::str::from_utf8(&pull.stderr).expect("stderr utf8");
    let envelope: Value = serde_json::from_str(stderr).expect("pull refusal JSON parses");
    assert_eq!(
        envelope["kind"], "git_overlay_remote_diverged",
        "{envelope}"
    );
    assert_eq!(
        envelope["primary_command"], "heddle import git --ref origin/main",
        "{envelope}"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "refs/remotes/origin/main"], &local),
        tracking_before,
        "failed pull must not refresh the visible checkout's remote-tracking ref"
    );
    assert_eq!(
        git_stdout_trimmed(&["rev-parse", "HEAD"], &local),
        head_before,
        "failed pull must not move the local branch"
    );
    assert_eq!(
        std::fs::read_to_string(local.join("file.txt")).unwrap(),
        "local heddle\n",
        "failed pull must not write remote content or conflict markers into the worktree"
    );
}

#[test]
fn test_cli_clone_git_overlay_filter_is_rejected() {
    // Issue 49 / 20b: same shape as --depth / --lazy — `--filter` is
    // rejected up-front. The wire-layer plumbing in `clone_url_to_bare`
    // is real prep for 20c; the user-facing flag flip waits on
    // import-side support.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    SleyRepository::init_bare(&origin).expect("init bare git origin");

    let err = heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
            "--filter",
            "blob:none",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--filter") && err.contains("not yet supported"),
        "filter must be rejected with 'not yet supported': {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

#[test]
fn test_cli_clone_git_overlay_file_url_rejects_unsupported_flags() {
    // Issue 49 / 20b round-2 P1: previously the local-path rejection
    // told users to "use a file:// URL instead" — but `file://` parses
    // as `RemoteTarget::Local` and routes through the same
    // `clone_git_overlay_path`, hitting the same rejection. Confirm
    // the file:// scheme path is rejected with the same shape (no
    // dead-end loop) and leaves no partial directory behind.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    SleyRepository::init_bare(&origin).expect("init bare git origin");

    let file_url = format!("file://{}", origin.display());
    let err = heddle(
        &[
            "clone",
            &file_url,
            work.to_str().expect("work path utf8"),
            "--filter",
            "blob:none",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--filter") && err.contains("not yet supported"),
        "file:// + --filter must reject with the same 'not yet supported' shape: {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

#[test]
fn test_cli_pull_local_lazy_is_rejected() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();
    heddle(&["init"], Some(target.path())).unwrap();

    let source_path = source.path().to_string_lossy().to_string();
    let output = heddle_output(
        &["--output", "json", "pull", &source_path, "--lazy"],
        Some(target.path()),
    )
    .expect("invoke local lazy pull");
    assert!(
        !output.status.success(),
        "local lazy pull should fail with a typed refusal"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode lazy pull refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("local lazy pull should emit JSON envelope");
    assert_eq!(envelope["kind"], "local_lazy_pull_unsupported");
    assert!(
        envelope["error"].as_str().is_some_and(
            |error| error.contains("lazy materialization requires a hosted or network remote")
        ),
        "local lazy pull should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("without `--lazy`")),
        "local lazy pull hint should name the safe retry: {stderr}"
    );
}

#[test]
fn test_cli_fetch_requires_remote_without_all() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output =
        heddle_output(&["--output", "json", "fetch"], Some(temp.path())).expect("invoke fetch");
    assert!(!output.status.success(), "fetch without remote should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode fetch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "remote_name_required");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("remote name required")),
        "fetch should explain the typed missing-remote refusal: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle fetch <remote>")
                && hint.contains("heddle fetch --all")),
        "fetch hint should name both valid recovery shapes: {stderr}"
    );
}

#[test]
fn test_cli_fetch_local_creates_remote_thread_and_marker() {
    let remote = TempDir::new().unwrap();
    let local = TempDir::new().unwrap();

    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(remote.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();
    heddle(&["thread", "marker", "create", "v1.0"], Some(remote.path())).unwrap();

    heddle(&["init"], Some(local.path())).unwrap();
    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(
        &["remote", "add", "origin", &remote_path],
        Some(local.path()),
    )
    .unwrap();

    assert!(heddle(&["fetch", "origin"], Some(local.path())).is_ok());

    let repo = Repository::open(local.path()).unwrap();
    assert!(
        repo.refs()
            .get_remote_thread("origin", &ThreadName::new("main"))
            .unwrap()
            .is_some()
    );
    assert!(
        repo.refs()
            .get_marker(&MarkerName::new("v1.0"))
            .unwrap()
            .is_some()
    );
}

#[test]
fn test_cli_fetch_uses_default_remote_and_emits_single_json_value() {
    let remote = TempDir::new().unwrap();
    let local = TempDir::new().unwrap();

    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(remote.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();

    heddle(&["init"], Some(local.path())).unwrap();
    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(
        &["remote", "add", "origin", &remote_path],
        Some(local.path()),
    )
    .unwrap();

    let stdout = heddle(&["--output", "json", "fetch"], Some(local.path())).unwrap();
    let parsed: Value =
        serde_json::from_str(&stdout).expect("fetch JSON should be exactly one JSON value");
    assert_eq!(parsed["output_kind"], "fetch");
    assert_eq!(parsed["remote"], "origin");
    assert_eq!(parsed["refs_fetched"], 1);
    assert!(
        parsed["objects_fetched"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "fetch should copy remote objects: {parsed}"
    );
}

#[test]
fn test_cli_fetch_all_uses_discovered_remotes() {
    let remote = TempDir::new().unwrap();
    let local = TempDir::new().unwrap();

    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(remote.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();

    heddle(&["init"], Some(local.path())).unwrap();
    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(
        &["remote", "add", "origin", &remote_path],
        Some(local.path()),
    )
    .unwrap();
    heddle(&["fetch", "origin"], Some(local.path())).unwrap();

    let output = heddle(&["--output", "json", "fetch", "--all"], Some(local.path())).unwrap();
    assert!(
        output.contains("Fetched") || output.contains("\"refs_fetched\""),
        "fetch --all should report summary"
    );
}

#[test]
fn test_cli_push_defaults_to_current_attached_thread() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(source.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(source.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "start",
                "feature/push-default",
                "--workspace",
                "auto",
            ],
            Some(source.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread.join("feature.txt"), "feature").unwrap();
    heddle(&["capture", "-m", "feature"], Some(&thread)).unwrap();

    let remote_path = remote.path().to_string_lossy().to_string();
    let output = heddle(&["--output", "json", "push", &remote_path], Some(&thread)).unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "push --output json must emit exactly one JSON value: {output}"
    );
    let parsed: Value = inject_post_verification_at(
        &thread,
        serde_json::from_str(&output).expect("push JSON should parse"),
    );
    assert_eq!(
        parsed["success"], true,
        "push should report success: {parsed}"
    );
    assert_eq!(parsed["verification"]["status"], "clean");

    let remote_repo = Repository::open(remote.path()).unwrap();
    assert!(
        remote_repo
            .refs()
            .get_thread(&ThreadName::new("feature/push-default"))
            .unwrap()
            .is_some(),
        "push without --thread should update the current attached thread"
    );
}

#[test]
fn test_cli_git_overlay_push_to_native_heddle_local_path_uses_heddle_sync() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    git_ok(&["init", "-b", "main"], source.path());
    git_ok(&["config", "user.name", "Heddle Test"], source.path());
    git_ok(
        &["config", "user.email", "heddle@example.com"],
        source.path(),
    );
    std::fs::write(source.path().join("README.md"), "seed\n").unwrap();
    git_ok(&["add", "README.md"], source.path());
    git_ok(&["commit", "-m", "seed"], source.path());
    heddle(&["adopt", "--ref", "main"], Some(source.path())).expect("adopt source Git repo");

    std::fs::write(
        source.path().join("README.md"),
        "seed\nnative remote push\n",
    )
    .unwrap();
    let commit_json = heddle(
        &["--output", "json", "commit", "-m", "Native local push"],
        Some(source.path()),
    )
    .expect("heddle commit succeeds");
    let commit: Value = serde_json::from_str(&commit_json).expect("commit JSON parses");
    let source_state = commit["state_id"]
        .as_str()
        .expect("commit should report state_id")
        .to_string();

    heddle(&["init"], Some(remote.path())).expect("init native target");
    let remote_path = remote.path().to_str().expect("remote path utf8");
    let push_json = heddle(
        &["--output", "json", "push", remote_path],
        Some(source.path()),
    )
    .expect("push to native Heddle path should use local Heddle sync");
    let push: Value = serde_json::from_str(&push_json).expect("push JSON parses");
    assert_eq!(push["success"], true, "push should succeed: {push}");

    let remote_repo = Repository::open(remote.path()).expect("open native target");
    let remote_state = remote_repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .expect("read target main")
        .expect("target main should be updated");
    assert_eq!(
        remote_state.short().to_string(),
        source_state,
        "native Heddle local path push should preserve the Heddle state id"
    );
    assert!(
        !remote.path().join(".git").exists(),
        "push to a native Heddle path must not turn the target into a Git remote"
    );
}

#[test]
fn push_bootstrap_validates_tls_config_before_creating_state() {
    let source = TempDir::new().unwrap();
    heddle(&["init"], Some(source.path())).expect("init source");

    let repo = Repository::open(source.path()).expect("open source");
    repo.refs()
        .delete_thread(&ThreadName::new("main"))
        .expect("clear current thread ref");
    assert!(
        repo.current_state().unwrap().is_none(),
        "fresh source should have no current state before push"
    );
    let states_before = repo.store().list_states().unwrap();

    let config_path = source.path().join("bad-tls-config.toml");
    let missing_ca = source.path().join("missing-ca.pem");
    std::fs::write(
        &config_path,
        format!(
            "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n\n[remote]\ntls_ca_certificate_path = \"{}\"\n",
            missing_ca.display()
        ),
    )
    .unwrap();

    let config = config_path.to_string_lossy().to_string();
    let output = heddle_output_with_env(
        &["push", "heddle://127.0.0.1:1/owner/repo"],
        Some(source.path()),
        &[("HEDDLE_CONFIG", &config)],
    )
    .expect("invoke push");
    assert!(!output.status.success(), "push should fail closed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty(),
        "failed push should not write stdout: {stdout}"
    );
    assert!(
        stderr.contains("fatal TLS/auth configuration error")
            && stderr.contains("remote.tls_ca_certificate_path"),
        "push should fail on TLS config before transport: {stderr}"
    );

    let repo = Repository::open(source.path()).expect("reopen source");
    assert!(
        repo.current_state().unwrap().is_none(),
        "TLS config failure must not bootstrap a current state"
    );
    assert_eq!(
        repo.store().list_states().unwrap(),
        states_before,
        "TLS config failure must not record a default-attributed state"
    );
}

#[test]
fn push_bootstrap_with_valid_config_still_creates_state() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).expect("init source");
    heddle(&["init"], Some(remote.path())).expect("init target");

    let before = Repository::open(source.path()).expect("open source");
    before
        .refs()
        .delete_thread(&ThreadName::new("main"))
        .expect("clear current thread ref");
    assert!(
        before.current_state().unwrap().is_none(),
        "fresh source should start without current state"
    );

    let remote_path = remote.path().to_str().expect("remote path utf8");
    heddle(&["push", remote_path], Some(source.path())).expect("bootstrap push succeeds");

    let source_repo = Repository::open(source.path()).expect("reopen source");
    let source_state = source_repo
        .current_state()
        .unwrap()
        .expect("valid push should bootstrap source state")
        .state_id;
    let remote_repo = Repository::open(remote.path()).expect("open target");
    let remote_state = remote_repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("valid push should update target main");
    assert_eq!(
        remote_state, source_state,
        "bootstrap push should send the newly created state"
    );
}

#[test]
fn push_network_validates_valid_config_before_bootstrapping_state() {
    let source = TempDir::new().unwrap();
    heddle(&["init"], Some(source.path())).expect("init source");

    let before = Repository::open(source.path()).expect("open source");
    before
        .refs()
        .delete_thread(&ThreadName::new("main"))
        .expect("clear current thread ref");
    assert!(
        before.current_state().unwrap().is_none(),
        "fresh source should start without current state"
    );

    let ca_path = source.path().join("ca.pem");
    std::fs::write(
        &ca_path,
        "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let config_path = source.path().join("valid-network-config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n\n[remote]\ntls_ca_certificate_path = \"{}\"\n",
            ca_path.display()
        ),
    )
    .unwrap();

    let config = config_path.to_string_lossy().to_string();
    let output = heddle_output_with_env(
        &["push", "heddle://127.0.0.1:1/owner/repo"],
        Some(source.path()),
        &[("HEDDLE_CONFIG", &config)],
    )
    .expect("invoke push");
    assert!(!output.status.success(), "push should fail at transport");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("fatal TLS/auth configuration error"),
        "valid TLS config should pass prevalidation before transport failure: {stderr}"
    );

    let repo = Repository::open(source.path()).expect("reopen source");
    assert!(
        repo.current_state().unwrap().is_some(),
        "valid network config should allow push bootstrap before transport failure"
    );
}

/// heddle#837: `push <remote> <thread>` names an EXISTING thread whose tip
/// differs from the current checkout. Push always ships the CURRENT state, so
/// this must REFUSE without `--force` (guard against overwriting a mismatched
/// thread's ref), and with `--force` publish the current checkout's state under
/// the named thread.
#[test]
fn native_push_named_mismatched_thread_refuses_without_force() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(source.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(source.path())).unwrap();

    // A sibling thread `feat-x` with its own isolated checkout + work, so its
    // tip differs from main's.
    let started: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "start", "feat-x", "--workspace", "auto"],
            Some(source.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let feat_checkout = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(feat_checkout.join("feat.txt"), "feat work").unwrap();
    heddle(&["capture", "-m", "feat work"], Some(&feat_checkout)).unwrap();

    let feat_state = current_thread_state(source.path(), "feat-x");
    let main_state = current_thread_state(source.path(), "main");
    assert_ne!(
        feat_state, main_state,
        "fixture invalid: feat-x tip must differ from main tip"
    );

    // Push `feat-x` BY NAME from the MAIN checkout (HEAD is on main). feat-x
    // exists with a DIFFERENT tip → must refuse without --force.
    let remote_path = remote.path().to_string_lossy().to_string();
    let output =
        heddle_output(&["push", &remote_path, "feat-x"], Some(source.path())).expect("invoke push");
    assert!(
        !output.status.success(),
        "push under a mismatched existing thread must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("feat-x") && stderr.contains("--force"),
        "refusal should name the thread and guide to --force: {stderr}"
    );

    // The remote must NOT have been given feat-x's ref by the refused push.
    let remote_repo = Repository::open(remote.path()).unwrap();
    assert!(
        remote_repo
            .refs()
            .get_thread(&ThreadName::new("feat-x"))
            .unwrap()
            .is_none(),
        "refused push must not write any ref for the mismatched thread"
    );

    // With --force, push the CURRENT (main) checkout state under feat-x.
    let output = heddle(
        &[
            "--output",
            "json",
            "push",
            &remote_path,
            "feat-x",
            "--force",
        ],
        Some(source.path()),
    )
    .expect("forced push under named thread succeeds");
    let parsed: Value = serde_json::from_str(&output).expect("push JSON parses");
    assert_eq!(
        parsed["success"], true,
        "forced push should succeed: {parsed}"
    );

    let remote_repo = Repository::open(remote.path()).unwrap();
    let remote_feat = remote_repo
        .refs()
        .get_thread(&ThreadName::new("feat-x"))
        .unwrap()
        .expect("feat-x ref should be written on the remote after --force");
    assert_eq!(
        remote_feat.short().to_string(),
        main_state,
        "forced named-thread push must publish the CURRENT checkout state (heddle#837)"
    );
    assert_ne!(
        remote_feat.short().to_string(),
        feat_state,
        "push never resolves the named thread's own tip (heddle#837)"
    );
}

/// heddle#837: `push <remote> <thread>` naming a thread that does NOT exist
/// locally must CREATE it on the remote from the current checkout state (this
/// is the create-thread path — no guard applies, no `--force` needed).
#[test]
fn native_push_new_named_thread_creates_from_current_state() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(source.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(source.path())).unwrap();
    let main_state = current_thread_state(source.path(), "main");

    let remote_path = remote.path().to_string_lossy().to_string();
    let output = heddle(
        &["--output", "json", "push", &remote_path, "brand-new"],
        Some(source.path()),
    )
    .expect("push creating a new named thread succeeds");
    let parsed: Value = serde_json::from_str(&output).expect("push JSON parses");
    assert_eq!(
        parsed["success"], true,
        "create-thread push should succeed: {parsed}"
    );

    // The remote brand-new ref must hold the current checkout state.
    let remote_repo = Repository::open(remote.path()).unwrap();
    let remote_new = remote_repo
        .refs()
        .get_thread(&ThreadName::new("brand-new"))
        .unwrap()
        .expect("brand-new ref should be created on the remote");
    assert_eq!(
        remote_new.short().to_string(),
        main_state,
        "creating a named thread must publish the current checkout state (heddle#837)"
    );
}

/// heddle#837: an explicit `--state` pushes that exact state under the named
/// thread, bypassing the current-checkout default and the mismatch guard.
#[test]
fn native_push_explicit_state_overrides_named_thread_tip() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(source.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(source.path())).unwrap();
    let base_state = current_thread_state(source.path(), "main");

    let started: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "start", "feat-x", "--workspace", "auto"],
            Some(source.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let feat_checkout = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(feat_checkout.join("feat.txt"), "feat work").unwrap();
    heddle(&["capture", "-m", "feat work"], Some(&feat_checkout)).unwrap();
    let feat_state = current_thread_state(source.path(), "feat-x");
    assert_ne!(base_state, feat_state);

    // Name feat-x but pin the state to the base state — the explicit state wins.
    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(
        &["push", &remote_path, "feat-x", "--state", &base_state],
        Some(source.path()),
    )
    .expect("push with explicit --state succeeds");

    let remote_repo = Repository::open(remote.path()).unwrap();
    let remote_feat = remote_repo
        .refs()
        .get_thread(&ThreadName::new("feat-x"))
        .unwrap()
        .expect("feat-x ref written");
    assert_eq!(
        remote_feat.short().to_string(),
        base_state,
        "explicit --state must win over the named thread's tip (heddle#837)"
    );
}

/// heddle#838: `push <remote> --all-threads` to a native-local target must push
/// EVERY thread (including a sibling with its own isolated checkout), and
/// `refs_written` must list exactly what was pushed.
#[test]
fn native_push_all_threads_fans_out_every_thread() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(source.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(source.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "start", "feat-x", "--workspace", "auto"],
            Some(source.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let feat_checkout = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(feat_checkout.join("feat.txt"), "feat work").unwrap();
    heddle(&["capture", "-m", "feat work"], Some(&feat_checkout)).unwrap();
    let feat_state = current_thread_state(source.path(), "feat-x");
    let main_state = current_thread_state(source.path(), "main");

    // Push --all-threads from the MAIN checkout.
    let remote_path = remote.path().to_string_lossy().to_string();
    let output = heddle(
        &["--output", "json", "push", &remote_path, "--all-threads"],
        Some(source.path()),
    )
    .expect("all-threads push succeeds");
    assert_eq!(
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "push --output json must emit exactly one JSON value: {output}"
    );
    let parsed: Value = serde_json::from_str(&output).expect("push JSON parses");
    assert_eq!(parsed["success"], true, "push should succeed: {parsed}");
    assert_eq!(parsed["push_scope"], "all_threads");
    let refs_written: Vec<String> = parsed["refs_written"]
        .as_array()
        .expect("refs_written present")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        refs_written.contains(&"main".to_string()) && refs_written.contains(&"feat-x".to_string()),
        "refs_written must list every pushed thread (heddle#838): {refs_written:?}"
    );

    // Both refs must land on the remote at their own tips.
    let remote_repo = Repository::open(remote.path()).unwrap();
    let remote_feat = remote_repo
        .refs()
        .get_thread(&ThreadName::new("feat-x"))
        .unwrap()
        .expect("feat-x ref should be written by --all-threads (heddle#838)");
    assert_eq!(
        remote_feat.short().to_string(),
        feat_state,
        "--all-threads must push feat-x at its own tip"
    );
    let remote_main = remote_repo
        .refs()
        .get_thread(&ThreadName::new("main"))
        .unwrap()
        .expect("main ref should be written by --all-threads");
    assert_eq!(remote_main.short().to_string(), main_state);
}
