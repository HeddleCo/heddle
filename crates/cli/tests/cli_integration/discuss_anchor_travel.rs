// SPDX-License-Identifier: Apache-2.0
//! CLI coverage for semantic discussion-anchor projection.

use serde_json::Value;
use tempfile::TempDir;

use super::heddle;

fn json(output: &str) -> Value {
    serde_json::from_str(output.trim()).expect("valid JSON output")
}

#[test]
fn discuss_show_follows_an_in_file_symbol_rename() {
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

    let shown = json(
        &heddle(
            &["--output", "json", "discuss", "show", id],
            Some(temp.path()),
        )
        .unwrap(),
    );
    assert_eq!(shown["discussion"]["anchor"]["path"], "main.rs");
    assert_eq!(shown["discussion"]["anchor"]["symbol"], "bar");
    assert_eq!(shown["discussion"]["anchor_status"], "moved");
}
