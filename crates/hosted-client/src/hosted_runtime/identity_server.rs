//! Detached lifetime management for the claim authorization endpoint.

use std::{
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use config::UserConfig;
use objects::lock::RepoLock;

use super::{HostedAuthMode, HostedSession, identity_state};

pub(crate) async fn ensure_running(server: &str) -> Result<()> {
    let lock = server_lock();
    if let Some(guard) = lock.try_write()? {
        let _ = std::fs::remove_file(ready_path());
        drop(guard);
        spawn(server)?;
    }
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if std::fs::read_to_string(ready_path()).is_ok_and(|ready| ready == server) {
            return Ok(());
        }
    }
    bail!("claim endpoint did not become ready")
}

fn spawn(server: &str) -> Result<()> {
    let current_exe = std::env::current_exe().context("locating heddle executable")?;
    let mut command = Command::new(current_exe);
    command
        .env_clear()
        .env("HEDDLE_HOME", repo::identity::heddle_home_dir())
        .args(["identity", "serve", "--server", server])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` has no arguments or memory effects. Failure is
        // propagated and prevents the background endpoint from starting.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    command
        .spawn()
        .context("starting claim authorization endpoint")?;
    Ok(())
}

pub(crate) async fn serve(server: String) -> Result<()> {
    let _guard = match server_lock().try_write()? {
        Some(guard) => guard,
        None => return Ok(()),
    };
    let state = identity_state::load()?.ok_or_else(|| anyhow::anyhow!("claim state is absent"))?;
    if state.server != server {
        bail!("claim state belongs to {}, not {server}", state.server);
    }
    let user_config = UserConfig::load_default()?;
    let session = HostedSession::build(
        &user_config,
        Some(server.clone()),
        HostedAuthMode::CredentialFallback,
    )?;
    let client = session.connect(([127, 0, 0, 1], 0).into()).await?;
    objects::fs_atomic::write_file_atomic(&ready_path(), server.as_bytes())
        .context("publishing claim endpoint readiness")?;
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Some(state) = identity_state::load()? else {
            break;
        };
        if !state.is_active(chrono::Utc::now().timestamp_millis()) {
            break;
        }
    }
    client.close().await;
    let _ = std::fs::remove_file(ready_path());
    Ok(())
}

fn ready_path() -> std::path::PathBuf {
    repo::identity::heddle_home_dir().join("agent-claim-server.ready")
}

fn server_lock() -> RepoLock {
    RepoLock::at(repo::identity::heddle_home_dir().join("locks/agent-claim-server.lock"))
}
