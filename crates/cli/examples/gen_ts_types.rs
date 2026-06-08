// SPDX-License-Identifier: Apache-2.0
//! Write the wrapper-facing TypeScript types + raw JSON Schemas to disk.
//!
//! The generation logic lives in [`cli::ts_codegen`] (so the drift test can
//! reuse it); this binary just writes the result. Run from the repo root:
//!
//!     cargo run -p heddle-cli --example gen_ts_types -- clients/npm/generated
//!
//! Output is deterministic, so regenerating on an unchanged contract is a
//! no-op diff. `crates/cli/tests/ts_types_in_sync.rs` asserts the checked-in
//! files match.

use std::path::PathBuf;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let out_dir = PathBuf::from(
        std::env::args()
            .nth(1)
            .unwrap_or_else(|| "clients/npm/generated".to_string()),
    );
    std::fs::create_dir_all(&out_dir)
        .with_context(|| format!("create output dir {}", out_dir.display()))?;

    let generated = cli::ts_codegen::generate();

    let ts_path = out_dir.join("heddle-schemas.ts");
    let json_path = out_dir.join("heddle-schemas.json");
    std::fs::write(&ts_path, &generated.typescript)
        .with_context(|| format!("write {}", ts_path.display()))?;
    std::fs::write(&json_path, &generated.json)
        .with_context(|| format!("write {}", json_path.display()))?;

    eprintln!(
        "wrote {} and {} (pinned to heddle-cli {})",
        ts_path.display(),
        json_path.display(),
        cli::ts_codegen::SCHEMA_VERSION,
    );
    Ok(())
}
