// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "client")]

use std::process::Command;

use tempfile::TempDir;

#[test]
fn built_cli_parses_credential_login_and_routes_to_verifying_load() {
    let home = TempDir::new().expect("temp Heddle home");
    let credential_path = home.path().join("agent.hcred");
    // A structurally invalid `.hcred` must be rejected by the verifying load
    // chokepoint — this proves `--credential` routes into that path.
    std::fs::write(&credential_path, "{\"not\":\"a credential\"}\n")
        .expect("write invalid credential file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&credential_path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict credential perms");
    }

    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "auth",
            "login",
            "--credential",
            credential_path.to_str().expect("UTF-8 credential path"),
        ])
        .current_dir(home.path())
        .env("HOME", home.path())
        .env("HEDDLE_HOME", home.path())
        .env_remove("HEDDLE_CONFIG")
        .output()
        .expect("run built heddle binary");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("parsing credential file")
            || stderr.contains("not a Heddle credential file"),
        "expected verifying-load error, got: {stderr}"
    );
}
