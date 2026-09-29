// SPDX-License-Identifier: Apache-2.0
use super::*;

fn git_at(cwd: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output")
}

fn git_overlay_fixture(committed: bool) -> TempDir {
    let main = TempDir::new().expect("fixture");
    git_at(main.path(), &["init", "-b", "main"]);
    git_at(main.path(), &["config", "user.name", "Fanout Test"]);
    git_at(main.path(), &["config", "user.email", "fanout@example.com"]);
    if committed {
        fs::write(main.path().join("base.txt"), "committed\n").expect("base file");
        git_at(main.path(), &["add", "base.txt"]);
        git_at(main.path(), &["commit", "-m", "base"]);
    }
    heddle(&["init"], Some(main.path())).expect("overlay init");
    main
}

#[test]
fn fanout_unborn_git_head_leaves_no_lane_or_task() {
    for untracked in [false, true] {
        let main = git_overlay_fixture(false);
        if untracked {
            fs::write(main.path().join("draft.txt"), "draft\n").expect("draft file");
        }
        let output = heddle_output(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "start",
                "--title",
                "Coordinate",
                "--lane",
                "feature/a=Implement A",
            ],
            Some(main.path()),
        )
        .expect("fanout refusal");
        assert!(!output.status.success(), "unborn HEAD must be refused");
        let error: Value = serde_json::from_slice(&output.stderr).expect("typed refusal");
        assert_eq!(error["kind"], "agent_fanout_unborn_git_head", "{error}");
        assert_eq!(
            error["git_recovery_command"],
            format!(
                "git -C {path} add -A && git -C {path} commit --allow-empty -m 'Initial commit'",
                path = main.path().display()
            ),
            "{error}"
        );
        assert_eq!(error["primary_command"], "heddle status", "{error}");
        assert!(!main.path().join(".heddle/threads/feature%2Fa").exists());
        assert!(!main.path().join(".heddle/agent-tasks").exists());
        let head = std::process::Command::new("git")
            .args(["rev-parse", "--verify", "HEAD"])
            .current_dir(main.path())
            .output()
            .expect("read Git HEAD");
        assert!(!head.status.success(), "fanout created a parent Git commit");
    }
}

#[test]
fn fanout_dirty_git_overlay_refuses_staged_unstaged_and_untracked_work() {
    for kind in ["staged", "unstaged", "untracked"] {
        let main = git_overlay_fixture(true);
        match kind {
            "staged" => {
                fs::write(main.path().join("base.txt"), "staged\n").expect("staged edit");
                git_at(main.path(), &["add", "base.txt"]);
            }
            "unstaged" => {
                fs::write(main.path().join("base.txt"), "unstaged\n").expect("unstaged edit");
            }
            _ => {
                fs::write(main.path().join("draft.txt"), "untracked\n").expect("untracked file");
            }
        }
        let status_before = git_at(main.path(), &["status", "--short"]);
        let output = heddle_output(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "start",
                "--title",
                "Coordinate",
                "--lane",
                "feature/a=Implement A",
            ],
            Some(main.path()),
        )
        .expect("fanout refusal");
        assert!(
            !output.status.success(),
            "{kind}: dirty HEAD must be refused"
        );
        let error: Value = serde_json::from_slice(&output.stderr).expect("typed refusal");
        assert_eq!(
            error["kind"], "agent_fanout_dirty_git_overlay",
            "{kind}: {error}"
        );
        assert_eq!(
            error["git_recovery_command"],
            format!(
                "git -C {path} add -A && git -C {path} commit -m 'Prepare fanout'",
                path = main.path().display()
            ),
            "{error}"
        );
        assert_eq!(error["primary_command"], "heddle status", "{error}");
        assert_eq!(git_at(main.path(), &["status", "--short"]), status_before);
        assert!(!main.path().join(".heddle/threads/feature%2Fa").exists());
        assert!(!main.path().join(".heddle/agent-tasks").exists());
    }
}

#[test]
fn fanout_rebinds_existing_overlay_to_current_committed_git_head() {
    let main = git_overlay_fixture(true);
    heddle(
        &[
            "agent",
            "fanout",
            "start",
            "--title",
            "First",
            "--lane",
            "feature/first=First lane",
        ],
        Some(main.path()),
    )
    .expect("first fanout");
    fs::write(main.path().join("base.txt"), "second commit\n").expect("second edit");
    git_at(main.path(), &["add", "base.txt"]);
    git_at(main.path(), &["commit", "-m", "second"]);
    let parent_head = git_at(main.path(), &["rev-parse", "HEAD"]);
    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "start",
                "--title",
                "Second",
                "--lane",
                "feature/second=Second lane",
            ],
            Some(main.path()),
        )
        .expect("second fanout"),
    )
    .expect("fanout JSON");
    let lane = std::path::Path::new(started["lanes"][0]["path"].as_str().expect("lane path"));
    assert_eq!(git_at(lane, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(git_at(lane, &["status", "--short"]), "");
    assert_eq!(
        fs::read_to_string(lane.join("base.txt")).expect("lane file"),
        "second commit\n"
    );
}

#[test]
fn fresh_git_overlay_fanout_has_isolated_git_and_launch_commands() {
    let main = TempDir::new().expect("fixture");
    let git = |cwd: &std::path::Path, args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("git output")
    };
    git(main.path(), &["init", "-b", "main"]);
    git(main.path(), &["config", "user.name", "Fanout Test"]);
    git(main.path(), &["config", "user.email", "fanout@example.com"]);
    fs::write(main.path().join("base.txt"), "base\n").expect("base file");
    git(main.path(), &["add", "base.txt"]);
    git(main.path(), &["commit", "-m", "base"]);
    let parent_git = git(main.path(), &["rev-parse", "--absolute-git-dir"]);
    heddle(&["init"], Some(main.path())).expect("overlay init");

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "start",
                "--title",
                "Coordinate",
                "--lane",
                "feature/a=Implement A",
                "--harness",
                "claude-code",
                "--lane",
                "feature/b=Implement B",
                "--harness",
                "codex",
                "--lane",
                "feature/c=Implement C",
                "--harness",
                "opencode",
            ],
            Some(main.path()),
        )
        .expect("fresh overlay fanout"),
    )
    .expect("fanout JSON");
    let lanes = started["lanes"].as_array().expect("lanes");
    assert_eq!(lanes.len(), 3);
    assert_eq!(started["commands"].as_array().expect("commands").len(), 3);
    for (index, lane) in lanes.iter().enumerate() {
        let path = std::path::Path::new(lane["path"].as_str().expect("checkout path"));
        assert!(path.starts_with(main.path().join(".heddle/threads")));
        let child_git = git(path, &["rev-parse", "--absolute-git-dir"]);
        assert_ne!(
            child_git.trim(),
            parent_git.trim(),
            "child must own git metadata"
        );
        assert!(git(path, &["log", "-1", "--format=%s"]).contains("base"));
        fs::write(path.join("lane.txt"), format!("lane {index}\n")).expect("lane edit");
        assert!(git(path, &["status", "--short"]).contains("lane.txt"));
        fs::write(path.join("base.txt"), format!("base lane {index}\n")).expect("tracked edit");
        assert!(git(path, &["diff"]).contains("base lane"));
        let command = &started["commands"][index];
        assert_eq!(command["cwd"].as_str(), lane["path"].as_str());
        assert!(
            command["command"]
                .as_str()
                .expect("command")
                .contains("writer-credential.json")
        );
        assert!(command["argv"].as_array().expect("argv").iter().any(|arg| {
            arg.as_str()
                .is_some_and(|arg| arg.contains(lane["title"].as_str().expect("title")))
        }));
    }
    assert!(
        git(main.path(), &["status", "--short"]).is_empty(),
        "parent sees child contents"
    );
    let parent_capture = heddle_output(
        &["capture", "-m", "should find no child content"],
        Some(main.path()),
    )
    .expect("parent capture attempt");
    assert!(
        !parent_capture.status.success(),
        "parent capture swept child contents"
    );
    let first = std::path::Path::new(lanes[0]["path"].as_str().expect("first path"));
    fs::write(first.join("capture.txt"), "captured\n").expect("capture edit");
    let credential: Value = serde_json::from_slice(
        &fs::read(first.join(".heddle/writer-credential.json")).expect("credential file"),
    )
    .expect("credential JSON");
    let lease = credential["lease"].as_str().expect("lease");
    let token = credential["token"].as_str().expect("token");
    let captured = heddle_output_with_env(
        &["agent", "capture", "--lease", lease, "-m", "lane capture"],
        Some(first),
        &[("HEDDLE_RESERVATION_TOKEN", token)],
    )
    .expect("lane capture command");
    assert!(
        captured.status.success(),
        "lane capture: {}",
        String::from_utf8_lossy(&captured.stderr)
    );
    let log: Value =
        serde_json::from_str(&heddle(&["--output", "json", "log"], Some(first)).expect("lane log"))
            .expect("log JSON");
    assert!(log.to_string().contains("lane capture"));
    heddle(
        &["thread", "drop", "feature/b", "--force"],
        Some(main.path()),
    )
    .expect("drop lane");
    assert!(!std::path::Path::new(lanes[1]["path"].as_str().expect("dropped path")).exists());
}

#[cfg(unix)]
#[test]
fn fanout_run_launches_harness_in_lane_with_file_credential() {
    use std::os::unix::fs::PermissionsExt;

    let main = setup_repo("base.txt", "base");
    let bin = TempDir::new().expect("fake harness directory");
    let markers = bin.path().join("markers");
    fs::create_dir(&markers).expect("marker directory");
    for harness in ["claude", "codex", "opencode"] {
        let fake = bin.path().join(harness);
        fs::write(&fake, "#!/bin/sh\ntest -z \"$HEDDLE_RESERVATION_TOKEN\" || exit 2\ntest -f \"$HEDDLE_WRITER_CREDENTIAL_FILE\" || exit 3\ntest -z \"$HEDDLE_WRITER_OTHER_LANE\" || exit 4\npwd > \"$FANOUT_LAUNCH_MARKER/${0##*/}\"\nenv >&2\n")
            .expect("fake harness");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("executable harness");
    }
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").expect("PATH")
    );
    let output = heddle_output_with_env(
        &[
            "--output",
            "json",
            "agent",
            "fanout",
            "start",
            "--title",
            "Coordinate",
            "--lane",
            "feature/claude=Implement Claude",
            "--harness",
            "claude-code",
            "--lane",
            "feature/codex=Implement Codex",
            "--harness",
            "codex",
            "--lane",
            "feature/opencode=Implement Opencode",
            "--harness",
            "opencode",
            "--run",
        ],
        Some(main.path()),
        &[
            ("PATH", &path),
            ("HEDDLE_RESERVATION_TOKEN", "foreign-writer-token"),
            (
                "HEDDLE_WRITER_CREDENTIAL_FILE",
                "/tmp/foreign-writer-credential",
            ),
            ("HEDDLE_WRITER_OTHER_LANE", "foreign-lane-token"),
            (
                "FANOUT_LAUNCH_MARKER",
                markers.to_str().expect("marker path"),
            ),
        ],
    )
    .expect("run fanout");
    assert!(
        output.status.success(),
        "run fanout: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).expect("JSON output");
    for (index, harness) in ["claude", "codex", "opencode"].iter().enumerate() {
        let lane_path = started["lanes"][index]["path"].as_str().expect("lane path");
        assert_eq!(
            fs::read_to_string(markers.join(harness))
                .expect("launch marker")
                .trim(),
            lane_path
        );
        let credential_path =
            std::path::Path::new(lane_path).join(".heddle/writer-credential.json");
        let credential: Value =
            serde_json::from_slice(&fs::read(credential_path).expect("credential"))
                .expect("credential JSON");
        let token = credential["token"].as_str().expect("token");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains(token),
            "child stderr exposed its writer token"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(token));
    }
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("foreign-writer-token")
            && !String::from_utf8_lossy(&output.stderr).contains("foreign-lane-token"),
        "child stderr exposed inherited writer credentials"
    );
}

#[cfg(unix)]
#[test]
fn printed_fanout_commands_scrub_inherited_writer_credentials() {
    use std::os::unix::fs::PermissionsExt;

    let bin = TempDir::new().expect("fake harness directory");
    for harness in ["claude", "codex", "opencode"] {
        let fake = bin.path().join(harness);
        fs::write(
            &fake,
            "#!/bin/sh\nprintf 'reservation=%s other=%s late=%s writer=%s\\n' \"$HEDDLE_RESERVATION_TOKEN\" \"$HEDDLE_WRITER_OTHER_LANE\" \"$HEDDLE_WRITER_ADDED_LATER\" \"$HEDDLE_WRITER_CREDENTIAL_FILE\" >&2\ntest -z \"$HEDDLE_RESERVATION_TOKEN\" || exit 2\ntest -z \"$HEDDLE_WRITER_OTHER_LANE\" || exit 3\ntest -z \"$HEDDLE_WRITER_ADDED_LATER\" || exit 5\ntest -f \"$HEDDLE_WRITER_CREDENTIAL_FILE\" || exit 4\n",
        )
        .expect("fake harness");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("executable harness");
    }
    let path = format!(
        "{}:{}",
        bin.path().display(),
        std::env::var("PATH").expect("PATH")
    );
    let inherited = [
        ("PATH", path.as_str()),
        ("HEDDLE_RESERVATION_TOKEN", "foreign-reservation"),
        ("HEDDLE_WRITER_OTHER_LANE", "foreign-lane"),
        ("HEDDLE_WRITER_CREDENTIAL_FILE", "/tmp/foreign-writer"),
    ];
    for json in [false, true] {
        let main = git_overlay_fixture(true);
        let mut args = vec!["agent", "fanout", "start", "--title", "Coordinate"];
        if json {
            args.splice(0..0, ["--output", "json"]);
        }
        args.extend([
            "--lane",
            "feature/a=Implement A",
            "--harness",
            "claude-code",
            "--lane",
            "feature/b=Implement B",
            "--harness",
            "codex",
            "--lane",
            "feature/c=Implement C",
            "--harness",
            "opencode",
        ]);
        let output =
            heddle_output_with_env(&args, Some(main.path()), &inherited).expect("fanout output");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let commands: Vec<String> = if json {
            let value: Value = serde_json::from_slice(&output.stdout).expect("JSON fanout");
            value["commands"]
                .as_array()
                .expect("commands")
                .iter()
                .map(|item| {
                    let scrub = item["env_unset"].as_array().expect("JSON scrub list");
                    for key in ["HEDDLE_RESERVATION_*", "HEDDLE_WRITER_*"] {
                        assert!(
                            scrub.iter().any(|entry| entry.as_str() == Some(key)),
                            "missing {key}: {item}"
                        );
                    }
                    item["command"].as_str().expect("launch command").to_owned()
                })
                .collect()
        } else {
            let text = String::from_utf8(output.stdout).expect("text fanout");
            text.lines()
                .filter_map(|line| line.strip_prefix("  cd "))
                .map(|line| format!("cd {line}"))
                .collect()
        };
        assert_eq!(commands.len(), 3, "one command per harness");
        for command in commands {
            let launch = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(main.path())
                .envs(inherited)
                .env("HEDDLE_WRITER_ADDED_LATER", "late-token")
                .output()
                .expect("printed launch");
            assert!(
                launch.status.success(),
                "printed launch inherited a writer credential: {}",
                String::from_utf8_lossy(&launch.stderr)
            );
            let stderr = String::from_utf8_lossy(&launch.stderr);
            assert!(
                !stderr.contains("foreign-reservation")
                    && !stderr.contains("foreign-lane")
                    && !stderr.contains("late-token")
                    && !stderr.contains("foreign-writer")
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn git_index_failure_at_each_lane_rolls_back_entire_fanout() {
    let preload = TempDir::new().expect("fault injector directory");
    let library = preload.path().join("fail-index.so");
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fanout_index_fault.c");
    let compile = std::process::Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&library)
        .arg(&source)
        .arg("-ldl")
        .output()
        .expect("compile fault injector");
    assert!(
        compile.status.success(),
        "{}",
        String::from_utf8_lossy(&compile.stderr)
    );
    let args = [
        "--output",
        "json",
        "agent",
        "fanout",
        "start",
        "--title",
        "Coordinate",
        "--lane",
        "feature/a=Implement A",
        "--lane",
        "feature/b=Implement B",
        "--lane",
        "feature/c=Implement C",
    ];
    for failed in ["a", "b", "c"] {
        let main = git_overlay_fixture(true);
        let target = main
            .path()
            .join(format!(".heddle/threads/feature%2F{failed}"));
        let fault = heddle_output_with_env(
            &args,
            Some(main.path()),
            &[
                ("LD_PRELOAD", library.to_str().expect("library path")),
                (
                    "HEDDLE_TEST_FAIL_INDEX",
                    target.to_str().expect("failed lane path"),
                ),
            ],
        )
        .expect("faulted fanout");
        assert!(!fault.status.success(), "{failed}: index write should fail");
        assert!(String::from_utf8_lossy(&fault.stderr).contains("INJECTED_INDEX_FAILURE"));
        for lane in ["feature%2Fa", "feature%2Fb", "feature%2Fc"] {
            assert!(
                !main.path().join(".heddle/threads").join(lane).exists(),
                "{failed}: failed fanout left lane {lane} behind"
            );
        }
        for folder in ["agent-tasks", "writer-leases", "thread_records"] {
            let path = main.path().join(".heddle").join(folder);
            let records = fs::read_dir(&path)
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "toml"))
                        .count()
                })
                .unwrap_or(0);
            assert_eq!(
                records, 0,
                "{failed}: failed fanout left records in {folder}"
            );
        }
        assert!(git_at(main.path(), &["status", "--short"]).is_empty());
        let retry = heddle_output(&args, Some(main.path())).expect("retry fanout");
        assert!(
            retry.status.success(),
            "{failed}: retry failed: {}",
            String::from_utf8_lossy(&retry.stderr)
        );
    }
}

#[test]
fn start_registers_thread_with_agent_metadata() {
    let main = setup_repo("base.txt", "base");

    let out = heddle(
        &[
            "--output",
            "json",
            "start",
            "feature/spawned",
            "--workspace",
            "solid",
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();

    assert_eq!(v["name"].as_str(), Some("feature/spawned"));
    assert!(v["message"].as_str().unwrap_or("").contains("Started"));

    let inspect: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/spawned"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(inspect["actor"]["provider"].as_str(), Some("anthropic"));
    assert_eq!(
        inspect["actor"]["model"].as_str(),
        Some("claude-sonnet-4-6")
    );
}

#[test]
fn thread_list_returns_all_started_threads() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &["start", "feature/list-a", "--workspace", "solid"],
        Some(main.path()),
    )
    .unwrap();
    heddle(
        &["start", "feature/list-b", "--workspace", "solid"],
        Some(main.path()),
    )
    .unwrap();

    let out = heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap();
    let v: Value = serde_json::from_str(&out).unwrap();
    let threads = v["threads"].as_array().unwrap();

    assert!(
        threads
            .iter()
            .any(|thread| thread["name"] == "feature/list-a")
    );
    assert!(
        threads
            .iter()
            .any(|thread| thread["name"] == "feature/list-b")
    );
}

#[test]
fn start_path_inherits_codex_probe_identity_into_actor_metadata() {
    let main = setup_repo("base.txt", "base");
    let work = TempDir::new().unwrap();

    let output = heddle_output_with_env(
        &[
            "--output",
            "json",
            "start",
            "feature/codex-probed",
            "--workspace",
            "materialized",
            "--path",
            work.path().to_str().unwrap(),
        ],
        Some(main.path()),
        &[
            ("CODEX_THREAD_ID", "thread-start-probe"),
            ("OPENAI_MODEL", "gpt-5.3-codex"),
            ("OPENAI_REASONING_EFFORT", "high"),
        ],
    )
    .expect("start with codex environment");
    assert!(
        output.status.success(),
        "start should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let started: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(started["name"].as_str(), Some("feature/codex-probed"));

    let actor: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "agent", "presence", "show"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let actor_entry = &actor["presence"];
    assert_eq!(actor_entry["thread"].as_str(), Some("feature/codex-probed"));
    assert_eq!(actor_entry["harness"].as_str(), Some("codex"));
    assert_eq!(actor_entry["provider"].as_str(), Some("openai"));
    assert!(
        actor_entry.get("model").and_then(Value::as_str).is_none(),
        "start must not invent a model from OPENAI_MODEL: {actor_entry}"
    );
    assert_eq!(actor_entry["thinking_level"].as_str(), Some("high"));
    assert_eq!(actor_entry["probe_source"].as_str(), Some("app_protocol"));

    let shown: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/codex-probed"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["harness"].as_str(), Some("codex"));
    assert_eq!(shown["actor"]["provider"].as_str(), Some("openai"));
    assert!(
        shown["actor"]
            .get("model")
            .and_then(Value::as_str)
            .is_none(),
        "thread show must not invent a model from OPENAI_MODEL: {shown}"
    );
}

#[test]
fn actor_show_defaults_to_current_thread_actor() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &[
            "start",
            "feature/current-actor",
            "--workspace",
            "solid",
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();

    let actor: Value = inject_post_verification_at(
        main.path(),
        serde_json::from_str(
            &heddle(
                &["--output", "json", "agent", "presence", "show"],
                Some(main.path()),
            )
            .unwrap(),
        )
        .unwrap(),
    );

    let actor_entry = &actor["presence"];
    assert_eq!(
        actor_entry["thread"].as_str(),
        Some("feature/current-actor")
    );
    assert_eq!(actor_entry["provider"].as_str(), Some("anthropic"));
    assert_eq!(actor_entry["model"].as_str(), Some("claude-sonnet-4-6"));
    assert!(actor_entry["session_id"].as_str().is_some());
    assert!(actor["verification"].is_object());
}

#[test]
fn actor_explain_reports_attach_reason_for_current_actor() {
    let main = setup_repo("base.txt", "base");

    heddle(
        &[
            "start",
            "feature/explain-actor",
            "--workspace",
            "solid",
            "--agent-provider",
            "anthropic",
            "--agent-model",
            "claude-sonnet-4-6",
        ],
        Some(main.path()),
    )
    .unwrap();

    let explained: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "agent", "presence", "explain"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(explained["thread"].as_str(), Some("feature/explain-actor"));
    assert!(
        explained["attach_reason"]
            .as_str()
            .unwrap_or("")
            .contains("thread")
    );
}

#[test]
fn agent_task_create_list_show_update_round_trip() {
    let main = setup_repo("base.txt", "base");

    let created: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "create",
                "--task-id",
                "task-cli-roundtrip",
                "--title",
                "Implement local task store",
                "--body",
                "Persist task provenance locally.",
                "--thread",
                "feature/task-roundtrip",
                "--allow-offline",
                "--delegated-by",
                "coordinator",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(created["output_kind"].as_str(), Some("agent_task_create"));
    assert_eq!(created["task"]["schema_version"].as_u64(), Some(1));
    assert_eq!(
        created["task"]["task_id"].as_str(),
        Some("task-cli-roundtrip")
    );
    assert_eq!(created["task"]["status"].as_str(), Some("open"));
    assert_eq!(created["task"]["allow_offline"].as_bool(), Some(true));

    let listed: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "list",
                "--thread",
                "feature/task-roundtrip",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(listed["output_kind"].as_str(), Some("agent_task_list"));
    let tasks = listed["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0]["task_id"].as_str(), Some("task-cli-roundtrip"));

    let updated: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "update",
                "task-cli-roundtrip",
                "--status",
                "complete",
                "--title",
                "Local task store complete",
                "--no-allow-offline",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(updated["output_kind"].as_str(), Some("agent_task_update"));
    assert_eq!(updated["task"]["status"].as_str(), Some("complete"));
    assert_eq!(
        updated["task"]["title"].as_str(),
        Some("Local task store complete")
    );
    assert_eq!(updated["task"]["allow_offline"].as_bool(), Some(false));
    assert!(updated["task"]["completed_at"].as_str().is_some());

    let shown: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "task",
                "show",
                "task-cli-roundtrip",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["output_kind"].as_str(), Some("agent_task_show"));
    assert_eq!(shown["task"]["status"].as_str(), Some("complete"));
    assert_eq!(
        shown["task"]["body"].as_str(),
        Some("Persist task provenance locally.")
    );
}

#[test]
fn agent_fanout_plan_is_read_only_and_returns_start_commands() {
    let main = setup_repo("base.txt", "base");
    let lane_spec = "feature/fanout-plan=Implement fanout plan lane";

    let planned: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "plan",
                "--title",
                "Coordinate fanout",
                "--coordination-discussion-id",
                "discussion-123",
                "--lane",
                lane_spec,
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(planned["output_kind"].as_str(), Some("agent_fanout_plan"));
    assert_eq!(planned["parent_task"], Value::Null);
    assert_eq!(
        planned["coordination_discussion_id"].as_str(),
        Some("discussion-123")
    );
    assert_eq!(planned["lanes"][0]["status"].as_str(), Some("planned"));
    let lane_path = std::path::PathBuf::from(
        planned["lanes"][0]["path"]
            .as_str()
            .expect("managed checkout path"),
    );
    assert_eq!(
        planned["commands"][0]["argv"].as_array().unwrap()[1].as_str(),
        Some("agent")
    );
    assert_eq!(
        planned["commands"][0]["argv"].as_array().unwrap()[2].as_str(),
        Some("fanout")
    );
    assert_eq!(
        planned["commands"][0]["argv"].as_array().unwrap()[3].as_str(),
        Some("start")
    );
    assert!(
        !main.path().join(".heddle").join("agent-tasks").exists(),
        "plan must not create task records"
    );
    assert!(
        !lane_path.exists(),
        "plan must not materialize the lane checkout"
    );
}

#[test]
fn agent_fanout_start_preflights_all_lanes_before_creating_tasks() {
    let main = setup_repo("base.txt", "base");
    let first_lane = "feature/fanout-preflight-a=First lane";
    let blocked_lane = "feature/fanout-preflight-b=Blocked lane";
    let planned: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "plan",
                "--title",
                "Coordinate failing fanout",
                "--lane",
                first_lane,
                "--lane",
                blocked_lane,
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let first_lane_path = std::path::PathBuf::from(planned["lanes"][0]["path"].as_str().unwrap());
    let blocked_lane_path = std::path::PathBuf::from(planned["lanes"][1]["path"].as_str().unwrap());
    std::fs::create_dir_all(&blocked_lane_path).unwrap();
    std::fs::write(blocked_lane_path.join("already-here.txt"), "occupied").unwrap();
    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "fanout",
            "start",
            "--title",
            "Coordinate failing fanout",
            "--lane",
            first_lane,
            "--lane",
            blocked_lane,
        ],
        Some(main.path()),
    )
    .unwrap();

    assert!(
        !output.status.success(),
        "fanout start should fail before creating any lane; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !main.path().join(".heddle").join("agent-tasks").exists(),
        "failed fanout preflight must not create task records"
    );
    assert!(
        !first_lane_path.exists(),
        "failed fanout preflight must not materialize earlier lanes"
    );
}

#[test]
fn agent_fanout_start_rejects_duplicate_lane_threads_before_creating_tasks() {
    let main = setup_repo("base.txt", "base");
    let lane_a = "feature/fanout-duplicate=First duplicate lane";
    let lane_b = "feature/fanout-duplicate=Second duplicate lane";
    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "fanout",
            "start",
            "--title",
            "Coordinate duplicate fanout",
            "--lane",
            lane_a,
            "--lane",
            lane_b,
        ],
        Some(main.path()),
    )
    .unwrap();

    assert!(!output.status.success());
    assert!(
        !main.path().join(".heddle").join("agent-tasks").exists(),
        "duplicate lane preflight must not create task records"
    );
}

#[test]
fn agent_fanout_start_creates_tasks_lanes_and_reservation_links() {
    let main = setup_repo("base.txt", "base");
    let lane_spec = "feature/fanout-start=Implement fanout start lane";

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "fanout",
                "start",
                "--title",
                "Coordinate fanout start",
                "--coordination-discussion-id",
                "discussion-start",
                "--lane",
                lane_spec,
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(started["output_kind"].as_str(), Some("agent_fanout_start"));
    let lane_path = std::path::PathBuf::from(
        started["lanes"][0]["path"]
            .as_str()
            .expect("managed checkout path"),
    );
    let parent_task_id = started["parent_task"]["task_id"]
        .as_str()
        .expect("parent task id");
    let child_task_id = started["lanes"][0]["task"]["task_id"]
        .as_str()
        .expect("child task id");
    assert_ne!(parent_task_id, child_task_id);
    assert_eq!(
        started["lanes"][0]["task"]["parent_task_id"].as_str(),
        Some(parent_task_id)
    );
    assert_eq!(
        started["lanes"][0]["task"]["coordination_discussion_id"].as_str(),
        Some("discussion-start")
    );
    let parent_body = started["parent_task"]["body"].as_str().unwrap_or("");
    assert!(parent_body.contains("feature/fanout-start"));
    assert!(parent_body.contains("Implement fanout start lane"));
    assert!(
        !parent_body.contains(&lane_path.display().to_string()),
        "parent task body should not persist checkout paths"
    );
    assert!(
        lane_path.join(".heddle").exists(),
        "fanout start should materialize a real lane checkout"
    );

    let listed: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "list",
                "--thread",
                "feature/fanout-start",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        listed["reservations"][0]["task_assignment_id"].as_str(),
        Some(child_task_id)
    );

    let shown: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "feature/fanout-start"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(shown["parent_thread"].as_str(), Some("main"));
}

#[test]
fn agent_reserve_records_task_assignment_id() {
    let main = setup_repo("base.txt", "base");
    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-reserve-success",
            "--title",
            "Reserve task",
            "--thread",
            "feature/task-reserve-success",
        ],
        Some(main.path()),
    )
    .unwrap();

    let reserved: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "reserve",
                "--thread",
                "feature/task-reserve-success",
                "--task-id",
                "task-reserve-success",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(
        reserved["reservation"]["task_assignment_id"].as_str(),
        Some("task-reserve-success")
    );
    assert_eq!(
        reserved["reservation"]["thread"].as_str(),
        Some("feature/task-reserve-success")
    );
}

#[test]
fn agent_task_correlation_surfaces_in_capture_thread_and_retro() {
    let main = setup_repo("base.txt", "base");
    let payload_hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-main-correlation",
            "--title",
            "Correlate agent work",
            "--thread",
            "main",
        ],
        Some(main.path()),
    )
    .unwrap();

    let reserved: Value = serde_json::from_str(
        &heddle(
            &[
                "--output",
                "json",
                "agent",
                "reserve",
                "--thread",
                "main",
                "--task-id",
                "task-main-correlation",
            ],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        reserved["reservation"]["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );

    heddle(
        &[
            "--output",
            "json",
            "agent",
            "timeline",
            "record-start",
            "--tool-call",
            "call-task-correlation",
            "--tool-name",
            "edit",
            "--summary",
            "safe timeline summary",
            "--payload-hash",
            payload_hash,
        ],
        Some(main.path()),
    )
    .unwrap();
    fs::write(main.path().join("private-secret-name.txt"), "changed\n").unwrap();
    let captured: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "capture", "-m", "correlated capture"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        captured["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );
    heddle(
        &[
            "--output",
            "json",
            "agent",
            "timeline",
            "record-finish",
            "--tool-call",
            "call-task-correlation",
            "--status",
            "succeeded",
            "--summary",
            "safe timeline finish",
            "--payload-hash",
            payload_hash,
        ],
        Some(main.path()),
    )
    .unwrap();

    let shown: Value = serde_json::from_str(
        &heddle(
            &["--output", "json", "thread", "show", "main"],
            Some(main.path()),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        shown["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );
    assert_eq!(
        shown["task_summary"]["title"].as_str(),
        Some("Correlate agent work")
    );

    let listed: Value = serde_json::from_str(
        &heddle(&["--output", "json", "thread", "list"], Some(main.path())).unwrap(),
    )
    .unwrap();
    let main_thread = listed["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["name"] == "main")
        .expect("main thread appears in thread list");
    assert_eq!(
        main_thread["task_assignment_id"].as_str(),
        Some("task-main-correlation")
    );
    assert_eq!(main_thread["task_summary"]["status"].as_str(), Some("open"));
}

#[test]
fn agent_reserve_rejects_unknown_task_id() {
    let main = setup_repo("base.txt", "base");

    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/missing-task",
            "--task-id",
            "task-does-not-exist",
        ],
        Some(main.path()),
    )
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent_task_not_found") || stderr.contains("task-does-not-exist"),
        "stderr should identify missing task: {stderr}"
    );
}

#[test]
fn agent_reserve_rejects_task_target_thread_mismatch() {
    let main = setup_repo("base.txt", "base");
    heddle(
        &[
            "agent",
            "task",
            "create",
            "--task-id",
            "task-thread-mismatch",
            "--title",
            "Wrong thread",
            "--thread",
            "feature/expected-thread",
        ],
        Some(main.path()),
    )
    .unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "agent",
            "reserve",
            "--thread",
            "feature/actual-thread",
            "--task-id",
            "task-thread-mismatch",
        ],
        Some(main.path()),
    )
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("agent_task_mismatch") || stderr.contains("feature/expected-thread"),
        "stderr should identify task/thread mismatch: {stderr}"
    );
}

#[test]
fn start_without_name_is_rejected() {
    let main = setup_repo("base.txt", "base");
    let result = heddle(&["start"], Some(main.path()));
    assert!(result.is_err(), "start without a thread name should fail");
}

#[test]
fn removed_actor_surface_is_rejected() {
    let main = setup_repo("base.txt", "base");
    let err = heddle(&["actor", "list"], Some(main.path()))
        .expect_err("the removed top-level actor surface must not parse");
    assert!(err.contains("unrecognized subcommand"), "{err}");
}
