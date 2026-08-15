// SPDX-License-Identifier: Apache-2.0

use ci_config::{CheckClass, ConfigError, Trigger, definition_digest, parse};
use heddle_object_model::object::Blob;

#[test]
fn parses_the_treadle_definition_model() {
    let config = parse(
        r#"
[meta]
schema = 1
[[check]]
name = "build"
class = "advisory"
command = ["cargo", "build", "--locked"]
timeout_secs = 30
triggers = ["push", "cron:0 2 * * 1"]
[check.retry]
max = 1
flake_signatures = ["dns error"]
"#,
    )
    .unwrap();
    assert_eq!(config.checks.len(), 1);
    assert_eq!(config.checks[0].class, CheckClass::Advisory);
    assert_eq!(config.checks[0].retry.max, 1);
    assert!(matches!(config.checks[0].triggers[0], Trigger::Push));
}

#[test]
fn rejects_duplicate_names_and_empty_argv() {
    let duplicate = parse(
        "[meta]\nschema=1\n[[check]]\nname='x'\ncommand=['true']\n[[check]]\nname='x'\ncommand=['true']\n",
    )
    .unwrap_err();
    assert!(matches!(duplicate, ConfigError::DuplicateCheckName { .. }));

    let empty = parse("[meta]\nschema=1\n[[check]]\nname='x'\n").unwrap_err();
    assert!(matches!(empty, ConfigError::EmptyCommand { .. }));
}

#[test]
fn digest_is_the_heddle_blob_hash() {
    let raw = b"[meta]\nschema = 1\n";
    assert_eq!(
        definition_digest(raw),
        Blob::from_slice(raw).hash().to_hex()
    );
}
