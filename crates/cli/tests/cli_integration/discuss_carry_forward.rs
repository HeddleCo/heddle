// SPDX-License-Identifier: Apache-2.0
//! #836: a discussion must remain reachable by `discuss show <id>` after the
//! state advances via a NON-capture path (`context set` / annotations).
//!
//! Root cause: `build_context_state` advanced HEAD via `State::new(...)` which
//! zeroes `discussions`, and nothing copied the parent's discussions pointer
//! forward — so HEAD became discussion-less and `discuss show` (HEAD-only)
//! could no longer resolve the thread. The capture/snapshot path already
//! carried discussions forward (anchor travel); this closes the gap on the
//! annotation path. See also the `--state` recoverability safety net.

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

/// Set up an initialised repo with one captured state so `discuss open` has a
/// real HEAD to anchor against.
fn setup() -> TempDir {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(temp.path().join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(temp.path().join("other.rs"), "fn f() {}\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    temp
}

fn json(out: &str) -> Value {
    serde_json::from_str(out.trim()).expect("valid JSON output")
}

/// The core repro from the issue: open a discussion, advance the state with an
/// unrelated `context set`, then `discuss show <id>` (HEAD default) must still
/// find it — and `discuss list` (HEAD default) must still surface it.
#[test]
fn discussion_survives_context_set_advance() {
    let temp = setup();
    let dir = Some(temp.path());

    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "main", "review q",
            ],
            dir,
        )
        .unwrap(),
    );
    let id = opened["id"].as_str().unwrap().to_string();

    // Advance HEAD via a non-capture (annotation) path, touching a DIFFERENT
    // file than the discussion anchor — exactly the issue's scenario.
    heddle(
        &[
            "context",
            "set",
            "--path",
            "other.rs",
            "--scope",
            "symbol:f",
            "--kind",
            "rationale",
            "-m",
            "note",
        ],
        dir,
    )
    .unwrap();

    // Was: "Error: ... discussion <id> not found". Now succeeds against HEAD.
    let shown = json(
        &heddle(&["--output", "json", "discuss", "show", &id], dir)
            .expect("discuss show must resolve the discussion on the advanced HEAD"),
    );
    assert_eq!(shown["id"].as_str(), Some(id.as_str()));

    // HEAD-default `discuss list` must still surface the carried-forward thread.
    let listed = json(&heddle(&["--output", "json", "discuss", "list"], dir).unwrap());
    let ids: Vec<&str> = listed["discussions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["id"].as_str())
        .collect();
    assert!(
        ids.contains(&id.as_str()),
        "HEAD-default discuss list must include the carried-forward discussion, got {ids:?}"
    );
}

/// `discuss list --state <newHEAD>` must show the discussion attached to the
/// new HEAD state (proves the carry-forward actually persisted on the advanced
/// state's blob, not just that `show` fell back somewhere).
#[test]
fn advanced_head_state_carries_the_discussion() {
    let temp = setup();
    let dir = Some(temp.path());

    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "main", "q",
            ],
            dir,
        )
        .unwrap(),
    );
    let id = opened["id"].as_str().unwrap().to_string();

    heddle(
        &[
            "context",
            "set",
            "--path",
            "other.rs",
            "--scope",
            "symbol:f",
            "--kind",
            "rationale",
            "-m",
            "note",
        ],
        dir,
    )
    .unwrap();

    // Find the new HEAD anchor from `discuss list` (default = HEAD) and re-query
    // it explicitly by state — it must resolve there.
    let head_listed = json(&heddle(&["--output", "json", "discuss", "list"], dir).unwrap());
    let head_state = head_listed["discussions"][0]["opened_against_state"]
        .as_str()
        .expect("discussion carries its (traveled) anchor state")
        .to_string();

    let by_state = json(
        &heddle(
            &[
                "--output",
                "json",
                "discuss",
                "list",
                "--state",
                &head_state,
            ],
            dir,
        )
        .unwrap(),
    );
    let ids: Vec<&str> = by_state["discussions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["id"].as_str())
        .collect();
    assert!(
        ids.contains(&id.as_str()),
        "per-state list on the advanced HEAD must include the discussion, got {ids:?}"
    );
}

/// Part B safety net: `discuss show <id> --state <priorState>` resolves a
/// discussion living on a specific prior state, independent of HEAD.
#[test]
fn discuss_show_state_flag_resolves_on_prior_state() {
    let temp = setup();
    let dir = Some(temp.path());

    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "main", "q",
            ],
            dir,
        )
        .unwrap(),
    );
    let id = opened["id"].as_str().unwrap().to_string();
    // The state the discussion was originally opened against.
    let prior_state = opened["opened_against_state"].as_str().unwrap().to_string();

    heddle(
        &[
            "context",
            "set",
            "--path",
            "other.rs",
            "--scope",
            "symbol:f",
            "--kind",
            "rationale",
            "-m",
            "note",
        ],
        dir,
    )
    .unwrap();

    let shown = json(
        &heddle(
            &[
                "--output",
                "json",
                "discuss",
                "show",
                &id,
                "--state",
                &prior_state,
            ],
            dir,
        )
        .expect("discuss show --state must resolve a discussion on the named prior state"),
    );
    assert_eq!(shown["id"].as_str(), Some(id.as_str()));
}

/// `discuss append` must still work after a `context set` advance — it shares
/// the HEAD-blob assumption, so if the drop broke it, Part A fixes it too.
#[test]
fn discuss_append_works_after_context_set_advance() {
    let temp = setup();
    let dir = Some(temp.path());

    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "main", "first",
            ],
            dir,
        )
        .unwrap(),
    );
    let id = opened["id"].as_str().unwrap().to_string();

    heddle(
        &[
            "context",
            "set",
            "--path",
            "other.rs",
            "--scope",
            "symbol:f",
            "--kind",
            "rationale",
            "-m",
            "note",
        ],
        dir,
    )
    .unwrap();

    let appended = json(
        &heddle(
            &["--output", "json", "discuss", "append", &id, "second"],
            dir,
        )
        .expect("discuss append must find the discussion on HEAD after the advance"),
    );
    let turns = appended["turns"].as_array().unwrap();
    assert_eq!(
        turns.len(),
        2,
        "append should add a second turn: {appended}"
    );
    assert_eq!(turns[1]["body"].as_str(), Some("second"));
}
