use anyhow::Result;
use cli_shared::{UserConfig, resolve_principal, resolve_principal_without_repo};
use repo::Repository;
use weft_client_shim::CliContext;

use super::report::CaptureActor;

/// Resolve who the next capture is attributed to.
///
/// An explicit `--repo` fails closed if that path cannot be opened. Discovery
/// from the current directory falls back to environment / user_config when no
/// repository is present, so `whoami` still works outside a repo.
pub(super) fn resolve_capture_actor(ctx: &dyn CliContext) -> Result<CaptureActor> {
    let user_config = UserConfig::load_default()?;
    let resolved = match ctx.repo_path() {
        Some(path) => resolve_principal(&Repository::open(path)?, &user_config)?,
        None => match std::env::current_dir() {
            Ok(cwd) => match Repository::open(cwd) {
                Ok(repo) => resolve_principal(&repo, &user_config)?,
                Err(_) => resolve_principal_without_repo(&user_config),
            },
            Err(_) => resolve_principal_without_repo(&user_config),
        },
    };
    Ok(CaptureActor::from_resolved(&resolved))
}
