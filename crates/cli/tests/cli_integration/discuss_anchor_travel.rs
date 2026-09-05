// SPDX-License-Identifier: Apache-2.0
//! CLI coverage for durable semantic discussion-anchor travel.

use objects::object::{CollaborationAnchor, CollaborationAnchorStatus, DiscussionRecordId};
use repo::{CollaborationStore, Repository};
use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

fn json(output: &str) -> Value {
    serde_json::from_str(output.trim()).expect("valid JSON output")
}

struct PersistedAnchor {
    path: String,
    symbol: String,
    status: CollaborationAnchorStatus,
    body_changed_since_open: bool,
    operation_count: usize,
}

fn assert_views_and_get(
    dir: &std::path::Path,
    id: &str,
    expected_path: &str,
    expected_symbol: &str,
    expected_status: &str,
) -> PersistedAnchor {
    let listed = json(&heddle(&["--output", "json", "discuss", "list"], Some(dir)).unwrap());
    let listed = listed["discussions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|discussion| discussion["id"] == id)
        .expect("opened discussion should be listed");
    let shown = json(&heddle(&["--output", "json", "discuss", "show", id], Some(dir)).unwrap());
    for discussion in [listed, &shown["discussion"]] {
        assert_eq!(discussion["anchor"]["path"], expected_path);
        assert_eq!(discussion["anchor"]["symbol"], expected_symbol);
        assert_eq!(discussion["anchor_status"], expected_status);
    }

    let repo = Repository::open(dir).unwrap();
    let store = CollaborationStore::open(repo.heddle_dir()).unwrap();
    let discussion_id: DiscussionRecordId = id.parse().unwrap();
    let discussion = store
        .materialize_discussion(&discussion_id)
        .unwrap()
        .expect("opened discussion should materialize");
    let CollaborationAnchor::Symbol { path, symbol, .. } = discussion.anchor else {
        panic!("discussion should retain a symbol anchor");
    };
    let operation_count = store
        .discussion_operation_ids(&discussion_id)
        .unwrap()
        .len();
    PersistedAnchor {
        path,
        symbol,
        status: discussion.anchor_status,
        body_changed_since_open: discussion.body_changed_since_open,
        operation_count,
    }
}

#[test]
fn discuss_views_follow_an_in_file_symbol_rename() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join("main.rs"),
        "fn foo() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "foo", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::write(
        temp.path().join("main.rs"),
        "fn bar() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "rename foo"], Some(temp.path())).unwrap();

    let persisted = assert_views_and_get(temp.path(), id, "main.rs", "bar", "moved");
    assert_eq!(persisted.path, "main.rs");
    assert_eq!(persisted.symbol, "bar");
    assert_eq!(persisted.status, CollaborationAnchorStatus::Moved);
    assert!(!persisted.body_changed_since_open);
    assert_eq!(persisted.operation_count, 2);
}

#[test]
fn discuss_anchor_stays_rebound_after_a_later_body_edit() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join("main.rs"),
        "fn foo() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "foo", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::write(
        temp.path().join("main.rs"),
        "fn bar() {\n    let value = 42;\n    println!(\"{}\", value);\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "rename foo"], Some(temp.path())).unwrap();

    std::fs::write(
        temp.path().join("main.rs"),
        "fn bar() {\n    let value = 43;\n    println!(\"changed {}\", value);\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "edit bar"], Some(temp.path())).unwrap();

    let persisted = assert_views_and_get(temp.path(), id, "main.rs", "bar", "current");
    assert_eq!(persisted.path, "main.rs");
    assert_eq!(persisted.symbol, "bar");
    assert_eq!(persisted.status, CollaborationAnchorStatus::Current);
    assert!(persisted.body_changed_since_open);
    assert_eq!(persisted.operation_count, 3);
}

#[test]
fn discuss_ambiguous_symbol_rename_requires_attention_without_picking() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join("main.rs"),
        "fn foo() {\n    let x = 1;\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "foo", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::write(
        temp.path().join("main.rs"),
        concat!(
            "fn bar() {\n    let x = 1;\n}\n",
            "fn baz() {\n    let x = 1;\n}\n",
        ),
    )
    .unwrap();
    heddle(&["capture", "-m", "ambiguous rename"], Some(temp.path())).unwrap();

    let persisted = assert_views_and_get(temp.path(), id, "main.rs", "foo", "ambiguous");
    assert_eq!(persisted.path, "main.rs");
    assert_eq!(persisted.symbol, "foo");
    assert_eq!(persisted.status, CollaborationAnchorStatus::Ambiguous);
    assert_eq!(persisted.operation_count, 2);
}

#[test]
fn discuss_deleted_symbol_becomes_orphaned() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join("main.rs"),
        "fn foo() {\n    let x = 1;\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "foo", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::write(temp.path().join("main.rs"), "// foo was deleted\n").unwrap();
    heddle(&["capture", "-m", "delete foo"], Some(temp.path())).unwrap();

    let persisted = assert_views_and_get(temp.path(), id, "main.rs", "foo", "orphaned");
    assert_eq!(persisted.path, "main.rs");
    assert_eq!(persisted.symbol, "foo");
    assert_eq!(persisted.status, CollaborationAnchorStatus::Orphaned);
    assert_eq!(persisted.operation_count, 2);
}

#[test]
fn discuss_anchor_still_follows_a_file_rename() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join("main.rs"),
        "fn foo() {\n    let x = 1;\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "main.rs", "foo", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::rename(temp.path().join("main.rs"), temp.path().join("moved.rs")).unwrap();
    heddle(&["capture", "-m", "move file"], Some(temp.path())).unwrap();

    let persisted = assert_views_and_get(temp.path(), id, "moved.rs", "foo", "moved");
    assert_eq!(persisted.path, "moved.rs");
    assert_eq!(persisted.symbol, "foo");
    assert_eq!(persisted.status, CollaborationAnchorStatus::Moved);
    assert_eq!(persisted.operation_count, 2);
}

#[test]
fn discuss_anchor_still_follows_a_mkdir_rename() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join("lib.py"),
        "def greet(name):\n    return f\"hello {name}\"\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();
    let opened = json(
        &heddle(
            &[
                "--output", "json", "discuss", "open", "lib.py", "greet", "review q",
            ],
            Some(temp.path()),
        )
        .unwrap(),
    );
    let id = opened["discussion"]["id"].as_str().unwrap();

    std::fs::create_dir(temp.path().join("pkg")).unwrap();
    std::fs::rename(
        temp.path().join("lib.py"),
        temp.path().join("pkg/greeter.py"),
    )
    .unwrap();
    heddle(
        &["capture", "-m", "move lib.py into pkg"],
        Some(temp.path()),
    )
    .unwrap();

    let persisted = assert_views_and_get(temp.path(), id, "pkg/greeter.py", "greet", "moved");
    assert_eq!(persisted.path, "pkg/greeter.py");
    assert_eq!(persisted.symbol, "greet");
    assert_eq!(persisted.status, CollaborationAnchorStatus::Moved);
    assert_eq!(persisted.operation_count, 2);
}
