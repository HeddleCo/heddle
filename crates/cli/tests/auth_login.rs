// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "client")]

use std::process::Command;

use tempfile::TempDir;

#[test]
fn built_cli_parses_headless_login_args_and_routes_to_install() {
    let home = TempDir::new().expect("temp Heddle home");
    let key_path = home.path().join("device.pem");
    std::fs::write(&key_path, "not an Ed25519 private key").expect("write invalid device key");

    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "auth",
            "login",
            "--token",
            "biscuit-token",
            "--key-file",
            key_path.to_str().expect("UTF-8 key path"),
            "--server",
            "127.0.0.1:8421",
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
        stderr.contains("invalid Ed25519 device private key"),
        "expected headless credential install error, got: {stderr}"
    );
}
