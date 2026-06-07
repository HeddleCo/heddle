// SPDX-License-Identifier: Apache-2.0
//! Drift guard for the generated wrapper types (#581).
//!
//! The checked-in `clients/npm/generated/*` must match what
//! `cli::ts_codegen::generate()` produces from the live schema catalog. If a
//! schema changes without regenerating, this fails — run
//! `scripts/gen-ts-types.sh` and commit the result.

use std::path::PathBuf;

fn generated_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/crates/cli`.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../clients/npm/generated")
        .canonicalize()
        .expect("clients/npm/generated exists")
}

#[test]
fn generated_typescript_is_in_sync() {
    let generated = cli::ts_codegen::generate();
    let path = generated_dir().join("heddle-schemas.ts");
    let on_disk = std::fs::read_to_string(&path).expect("read heddle-schemas.ts");
    assert_eq!(
        on_disk, generated.typescript,
        "{} is stale — run `scripts/gen-ts-types.sh` and commit the result",
        path.display()
    );
}

#[test]
fn generated_json_is_in_sync() {
    let generated = cli::ts_codegen::generate();
    let path = generated_dir().join("heddle-schemas.json");
    let on_disk = std::fs::read_to_string(&path).expect("read heddle-schemas.json");
    assert_eq!(
        on_disk, generated.json,
        "{} is stale — run `scripts/gen-ts-types.sh` and commit the result",
        path.display()
    );
}
