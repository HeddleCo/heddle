// SPDX-License-Identifier: Apache-2.0

#![cfg(feature = "client")]

use std::{path::Path, process::Command};

use biscuit_auth::KeyPair;
use crypto::{Ed25519Signer, Signer};
use serde_json::Value;
use tempfile::TempDir;

const SERVER: &str = "127.0.0.1:9";

fn write_env_credential(path: &Path) {
    let signer = Ed25519Signer::generate().expect("proof key");
    let expires_at = chrono::Utc::now() + chrono::Duration::hours(2);
    let token = biscuit_auth::Biscuit::builder()
        .fact(r#"user("env-agent")"#)
        .expect("user fact")
        .fact(format!("device_pop_key(\"{}\")", hex::encode(signer.public_key())).as_str())
        .expect("proof-key fact")
        .fact(format!("expires_at({})", expires_at.to_rfc3339()).as_str())
        .expect("expiry fact")
        .check(format!("check if time($now), $now < {}", expires_at.to_rfc3339()).as_str())
        .expect("expiry check")
        .build(&KeyPair::new())
        .expect("build credential token")
        .to_base64()
        .expect("encode credential token");
    let credential = serde_json::json!({
        "format": "heddle-credential",
        "version": 1,
        "server": SERVER,
        "kind": "device",
        "subject": "env-agent",
        "token": token,
        "proof_key_pem": signer.to_pem().expect("proof PEM"),
        "expires_at": expires_at.to_rfc3339(),
        "credential_id": null,
    });
    std::fs::write(
        path,
        serde_json::to_vec(&credential).expect("serialize credential"),
    )
    .expect("write credential fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .expect("restrict credential fixture");
    }
}

fn run_json(home: &Path, credential: Option<&Path>, args: &[&str]) -> Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .arg("--output")
        .arg("json")
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env_remove("HEDDLE_CONFIG");
    match credential {
        Some(path) => {
            command.env("HEDDLE_CREDENTIAL", path);
        }
        None => {
            command.env_remove("HEDDLE_CREDENTIAL");
        }
    }
    let output = command.output().expect("run heddle");
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "command did not emit JSON: {error}; stdout={}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

fn run_derive(home: &Path, credential: Option<&Path>, out: &Path) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .args([
            "auth",
            "derive-agent",
            "--server",
            SERVER,
            "--agent-id",
            "child-agent",
            "--out",
        ])
        .arg(out)
        .current_dir(home)
        .env("HOME", home)
        .env("HEDDLE_HOME", home)
        .env_remove("HEDDLE_CONFIG");
    match credential {
        Some(path) => {
            command.env("HEDDLE_CREDENTIAL", path);
        }
        None => {
            command.env_remove("HEDDLE_CREDENTIAL");
        }
    }
    command.output().expect("run derive-agent")
}

#[test]
fn auth_family_agrees_for_env_only_and_absent_credentials() {
    let home = TempDir::new().expect("isolated Heddle home");
    let credential_path = home.path().join("headless.hcred");
    write_env_credential(&credential_path);
    assert!(
        !home.path().join("credentials.toml").exists(),
        "the positive case must start with no keystore entry",
    );

    let status_with_env = run_json(
        home.path(),
        Some(&credential_path),
        &["auth", "status", "--server", SERVER],
    );
    let whoami_with_env = run_json(
        home.path(),
        Some(&credential_path),
        &["whoami", "--server", SERVER],
    );
    assert_eq!(status_with_env["authenticated"], true);
    assert_eq!(whoami_with_env["authenticated"], true);
    assert_eq!(
        status_with_env["authenticated"],
        whoami_with_env["authenticated"]
    );
    assert_eq!(status_with_env["subject"], "env-agent");
    assert_eq!(whoami_with_env["subject"], "env-agent");
    let expected_source = format!("env:{}", credential_path.display());
    assert_eq!(status_with_env["source"], expected_source);
    assert_eq!(whoami_with_env["source"], expected_source);

    let derived_path = home.path().join("derived.hcred");
    let derive_with_env = run_derive(home.path(), Some(&credential_path), &derived_path);
    assert!(
        derive_with_env.status.success(),
        "derive-agent rejected the env credential: stdout={} stderr={}",
        String::from_utf8_lossy(&derive_with_env.stdout),
        String::from_utf8_lossy(&derive_with_env.stderr),
    );
    assert!(
        derived_path.is_file(),
        "derive-agent should write its child credential"
    );
    assert!(
        String::from_utf8_lossy(&derive_with_env.stdout).contains(&expected_source),
        "derive-agent should identify the parent credential source",
    );
    assert!(
        !home.path().join("credentials.toml").exists(),
        "deriving to --out must not populate the empty keystore",
    );

    let status_without_env = run_json(home.path(), None, &["auth", "status", "--server", SERVER]);
    let whoami_without_env = run_json(home.path(), None, &["whoami", "--server", SERVER]);
    assert_eq!(status_without_env["authenticated"], false);
    assert_eq!(whoami_without_env["authenticated"], false);
    assert_eq!(
        status_without_env["authenticated"],
        whoami_without_env["authenticated"]
    );
    assert_eq!(status_without_env["source"], "none");
    assert_eq!(whoami_without_env["source"], "none");

    let absent_child = home.path().join("absent-child.hcred");
    let derive_without_env = run_derive(home.path(), None, &absent_child);
    assert!(!derive_without_env.status.success());
    assert!(
        String::from_utf8_lossy(&derive_without_env.stderr).contains("Not authenticated"),
        "derive-agent should report the absent credential without exposing credential material",
    );
    assert!(!absent_child.exists());
}
