// SPDX-License-Identifier: Apache-2.0
//! Drift guard for the generated wrapper types (#581).
//!
//! The checked-in `clients/npm/generated/*` must match what
//! `cli::ts_codegen::generate()` produces from the live schema catalog. If a
//! schema changes without regenerating, this fails — run
//! `scripts/gen-ts-types.sh` and commit the result.
//!
//! The checked-in artifacts are generated with `--all-features`
//! (`scripts/gen-ts-types.sh` does the same). Under a feature-pruned CI job
//! fewer verbs compile, so the live catalog is a strict subset and the drift
//! assertion would spuriously fail — gate the assertions to the feature set
//! that exposes every generated verb.

#[cfg(all(
    feature = "git-overlay",
    feature = "native",
    feature = "semantic",
    feature = "zstd",
    feature = "client",
    feature = "local-services"
))]
mod full_feature {
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
            on_disk,
            generated.typescript,
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
            on_disk,
            generated.json,
            "{} is stale — run `scripts/gen-ts-types.sh` and commit the result",
            path.display()
        );
    }
}

#[cfg(not(all(
    feature = "git-overlay",
    feature = "native",
    feature = "semantic",
    feature = "zstd",
    feature = "client",
    feature = "local-services"
)))]
#[test]
fn drift_check_skipped_under_feature_pruned_build() {
    eprintln!(
        "skipping generated-TS drift check: requires the full generated feature set \
         (git-overlay,native,semantic,zstd,client,local-services) the checked-in types were generated from"
    );
}
