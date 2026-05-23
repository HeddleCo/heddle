// SPDX-License-Identifier: Apache-2.0
//! Clone command - clone from remote.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Mutex,
};

#[cfg(feature = "client")]
use anyhow::Context;
use anyhow::{Result, anyhow};
#[cfg(feature = "client")]
use heddle_client::grpc_hosted::PullMaterialization;
use objects::{
    error::{HeddleError, Result as HeddleResult},
    object::{Blob, ContentHash},
};
use refs::Head;
use repo::{BlobHydrator, Repository};

use super::advice::RecoveryAdvice;
#[cfg(feature = "client")]
use crate::remote::credential_key_from_remote_url;
use crate::{
    bridge::{
        GitBridge,
        git_core::{clone_url_to_bare, copy_local_repo_to_bare, open_repo},
        git_import::{import_all, import_selected_refs},
    },
    cli::{Cli, should_output_json, style},
    client::LocalSync,
    remote::RemoteTarget,
};

/// Pull/materialization options shared by local and network clone paths.
struct CloneOptions {
    thread: Option<String>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<String>,
}

pub async fn cmd_clone(
    cli: &Cli,
    remote: String,
    local: String,
    thread: Option<String>,
    depth: Option<u32>,
    lazy: bool,
    filter: Option<String>,
) -> Result<()> {
    let local_path = Path::new(&local);
    let options = CloneOptions {
        thread,
        depth: depth.filter(|depth| *depth > 0),
        lazy,
        filter,
    };

    if local_path.exists() {
        return Err(anyhow!(clone_destination_exists_advice(&local)));
    }

    // Parse the remote URL
    #[cfg(feature = "client")]
    let server_key = credential_key_from_remote_url(&remote);
    let target = match RemoteTarget::parse(&remote) {
        Ok(target) => target,
        Err(_) => {
            if looks_like_local_path(&remote) {
                return Err(anyhow!(clone_remote_not_found_advice(Path::new(&remote))));
            }
            if let Ok(url) = gix::url::parse(remote.as_bytes().into()) {
                return clone_git_overlay_url(cli, &url, local_path, &options);
            }
            return Err(anyhow!(clone_invalid_remote_url_advice(&remote)));
        }
    };

    match target {
        RemoteTarget::Local(remote_path) => {
            if !remote_path.join(".heddle").exists() && gix::open(&remote_path).is_ok() {
                return clone_git_overlay_path(cli, &remote_path, local_path, &options);
            }
            clone_local(cli, &remote_path, local_path, &options).await?;
        }
        RemoteTarget::Network { addr, repo_path } => {
            #[cfg(feature = "client")]
            clone_network(
                cli,
                addr,
                repo_path.as_deref(),
                local_path,
                &options,
                server_key,
                hosted_endpoint_spec(&remote),
            )
            .await?;
            #[cfg(not(feature = "client"))]
            let _ = (addr, repo_path);
            #[cfg(not(feature = "client"))]
            return Err(anyhow!(network_clone_unavailable_advice()));
        }
    }

    Ok(())
}

fn clone_invalid_remote_url_advice(remote: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_invalid_remote_url",
        format!("Invalid remote URL: {remote}"),
        "Use `file:///path/to/repo`, an existing local path, a hosted/network remote, or a Git clone URL.",
        format!("remote '{remote}' could not be parsed as a supported Heddle or Git remote"),
        "clone cannot determine which transport or repository to read from",
        "no destination directory, repository metadata, refs, or worktree files were written",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn clone_destination_exists_advice(local: &str) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_destination_exists",
        format!("Local path '{local}' already exists"),
        "Choose an empty destination path, or move the existing path aside before retrying `heddle clone`.",
        format!("destination path '{local}' already exists"),
        "clone would need to write repository metadata and worktree files into that destination",
        "existing destination path and current repository state were left unchanged",
        "heddle clone <remote> <new-path>",
        vec!["heddle clone <remote> <new-path>".to_string()],
    )
}

fn looks_like_local_path(remote: &str) -> bool {
    let path = Path::new(remote);
    path.is_absolute()
        || remote == "."
        || remote == ".."
        || remote.starts_with("./")
        || remote.starts_with("../")
        || remote.starts_with("~/")
}

fn clone_git_overlay_url(
    cli: &Cli,
    url: &gix::Url,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    reject_unsupported_for_git_overlay(options)?;
    fs::create_dir_all(local_path)?;
    clone_url_to_bare(url, &local_path.join(".git"), None, None).map_err(anyhow::Error::msg)?;
    finish_git_overlay_clone(cli, local_path, options, url.to_string())
}

fn clone_git_overlay_path(
    cli: &Cli,
    remote_path: &Path,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    reject_unsupported_for_git_overlay(options)?;
    fs::create_dir_all(local_path)?;
    gix::init(local_path).map_err(anyhow::Error::msg)?;
    copy_local_repo_to_bare(remote_path, &local_path.join(".git")).map_err(anyhow::Error::msg)?;
    finish_git_overlay_clone(cli, local_path, options, remote_path.display().to_string())
}

/// Reject `--depth` / `--lazy` / `--filter` for Git-overlay clones before
/// any filesystem or network work runs. The wire-level plumbing in
/// `clone_url_to_bare` can already negotiate shallow + partial-clone
/// capabilities, but the import step (`import_all` →
/// `GitTreeImporter::import_blob` + ancestry walk) requires every blob
/// and parent commit to be present locally. Until the importer learns
/// to tolerate missing objects, accepting these flags would just trade
/// an upfront rejection for a half-built clone that fails partway
/// through import. Each flag has its own message so the error stays
/// scannable.
fn reject_unsupported_for_git_overlay(options: &CloneOptions) -> Result<()> {
    if let Some(filter) = options.filter.as_deref() {
        return Err(anyhow!(unsupported_git_overlay_clone_option_advice(
            "--filter",
            Some(filter)
        )));
    }
    if options.lazy {
        return Err(anyhow!(unsupported_git_overlay_clone_option_advice(
            "--lazy", None
        )));
    }
    if options.depth.is_some() {
        return Err(anyhow!(unsupported_git_overlay_clone_option_advice(
            "--depth", None
        )));
    }
    Ok(())
}

fn unsupported_git_overlay_clone_option_advice(
    flag: &'static str,
    value: Option<&str>,
) -> RecoveryAdvice {
    let flag_with_value = value
        .map(|value| format!("{flag} {value}"))
        .unwrap_or_else(|| flag.to_string());
    let detail = match flag {
        "--depth" => "the import step walks ancestry past the shallow boundary",
        _ => "the import step requires all blobs locally",
    };
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_option_unsupported",
        format!("{flag_with_value} is not yet supported for Git-overlay clones; {detail}"),
        format!("Run a full Git-overlay clone without `{flag}` for now."),
        "Git-overlay import requires a complete local Git object graph",
        format!(
            "accepting `{flag}` now could leave a partially imported clone that Heddle cannot trust"
        ),
        "no clone directory, Git refs, or Heddle state were written",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn finish_git_overlay_clone(
    cli: &Cli,
    local_path: &Path,
    options: &CloneOptions,
    remote_label: String,
) -> Result<()> {
    write_git_overlay_origin(local_path, &remote_label)?;
    let repo = Repository::init(local_path)?;
    let mut bridge = GitBridge::new(&repo);
    let stats = if let Some(thread) = options.thread.as_ref() {
        import_selected_refs(
            &mut bridge,
            Some(&local_path.join(".git")),
            std::slice::from_ref(thread),
        )
        .map_err(|err| {
            anyhow!(clone_git_overlay_import_failed_advice(
                Some(thread),
                &remote_label,
                err.to_string()
            ))
        })?
    } else {
        import_all(&mut bridge, Some(&local_path.join(".git"))).map_err(|err| {
            anyhow!(clone_git_overlay_import_failed_advice(
                None,
                &remote_label,
                err.to_string()
            ))
        })?
    };

    let track_name = select_clone_thread(
        &repo,
        options.thread.as_deref(),
        read_git_head_branch(&local_path.join(".git")).as_deref(),
        &remote_label,
    )?;
    let state_id = repo.refs().get_thread(&track_name)?.ok_or_else(|| {
        anyhow!(clone_git_overlay_branch_not_imported_advice(
            &track_name,
            &remote_label
        ))
    })?;
    // Materialize the imported tip *while HEAD is still on the
    // init-time default* — `goto` writes `Head::Detached`, which is
    // fine here because we re-attach immediately below. Switching to
    // `fast_forward_attached` would mis-advance whichever thread HEAD
    // happens to be on at this point (the seeded `main`, not the
    // cloned thread).
    repo.goto(&state_id)?;
    // Re-attach HEAD to the cloned thread, AND mirror the choice into
    // `.git/HEAD`. `Repository::open` on a git-overlay repo
    // unconditionally syncs heddle's HEAD from `.git/HEAD` via
    // `detect_git_head`, so if we left `.git/HEAD` pointing at gix's
    // init-time default ("main" / "master") the very next `heddle`
    // command would silently reset HEAD to a thread that doesn't
    // exist — and `current_state` would return `None`, causing
    // `heddle log` to snapshot a "Bootstrap git-overlay before
    // viewing log" state instead of walking the imported history.
    repo.refs().write_head(&Head::Attached {
        thread: track_name.clone(),
    })?;
    write_git_head_branch(&local_path.join(".git"), &track_name)?;
    verify_git_overlay_clone(&repo, local_path, &track_name, &state_id)?;

    if should_output_json(cli, Some(repo.config())) {
        println!(
            "{{\"status\":\"cloned\",\"transport\":\"git\",\"remote\":{:?},\"local\":{:?},\"branch\":{:?},\"commits_imported\":{}}}",
            remote_label,
            local_path.display().to_string(),
            track_name,
            stats.commits_imported
        );
    } else {
        let repo_name = clone_repo_name_from_label(&remote_label);
        for line in format_clone_completion_lines(repo_name, stats.commits_imported, &track_name) {
            println!("{line}");
        }
    }
    Ok(())
}

fn write_git_overlay_origin(local_path: &Path, remote_label: &str) -> Result<()> {
    let config_path = local_path.join(".git").join("config");
    let mut contents = fs::read_to_string(&config_path).unwrap_or_default();
    if contents.contains("[remote \"origin\"]") {
        return Ok(());
    }
    if !contents.ends_with('\n') && !contents.is_empty() {
        contents.push('\n');
    }
    contents.push_str(&format!(
        "[remote \"origin\"]\n\turl = {remote_label}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"
    ));
    fs::write(config_path, contents)?;
    Ok(())
}

fn verify_git_overlay_clone(
    repo: &Repository,
    local_path: &Path,
    track_name: &str,
    state_id: &objects::object::ChangeId,
) -> Result<()> {
    ensure_git_excludes_heddle(local_path)?;
    if git_available() {
        run_git_clone_step(local_path, &["config", "core.bare", "false"])?;
        run_git_clone_step(local_path, &["reset", "--hard", "HEAD"])?;

        let status = git_output(local_path, &["status", "--short"])?;
        if !status.trim().is_empty() {
            return Err(anyhow!(clone_verification_failed_advice(
                format!(
                    "clone verification failed: Git worktree is not clean after checkout: {}",
                    status.trim()
                ),
                format!(
                    "Git status reports dirty path(s) after clone checkout at {}: {}",
                    local_path.display(),
                    status.trim()
                ),
                "trusting this clone could hide checkout files that were not imported into Heddle",
                "heddle status",
            )));
        }
    }

    let git_head = read_git_head_branch(&local_path.join(".git")).ok_or_else(|| {
        anyhow!(clone_verification_failed_advice(
            "clone verification failed: .git/HEAD is not attached to a branch",
            "Git HEAD is detached after clone verification",
            "Heddle cannot prove which Git branch should map to the imported thread",
            format!("heddle bridge git import --ref {track_name}"),
        ))
    })?;
    if git_head != track_name {
        return Err(anyhow!(clone_verification_failed_advice(
            format!(
                "clone verification failed: .git/HEAD points at '{git_head}', but Heddle attached '{track_name}'"
            ),
            format!("Git HEAD branch '{git_head}' does not match Heddle thread '{track_name}'"),
            "continuing would leave Git and Heddle attached to different active names",
            format!("heddle bridge git import --ref {git_head}"),
        )));
    }

    match repo.current_lane()? {
        Some(current) if current == track_name => {}
        Some(current) => {
            return Err(anyhow!(clone_verification_failed_advice(
                format!(
                    "clone verification failed: Heddle active thread is '{current}', expected '{track_name}'"
                ),
                format!(
                    "Heddle active thread '{current}' does not match imported Git branch '{track_name}'"
                ),
                "continuing would report the clone as trusted while Heddle is attached to the wrong thread",
                format!("heddle thread switch {track_name} --force"),
            )));
        }
        None => {
            return Err(anyhow!(clone_verification_failed_advice(
                "clone verification failed: Heddle HEAD is detached after clone",
                "Heddle HEAD is detached after clone verification",
                "continuing would report the clone as trusted without an attached Heddle thread",
                format!("heddle thread switch {track_name} --force"),
            )));
        }
    }

    let imported = repo.refs().get_thread(track_name)?;
    if imported.as_ref() != Some(state_id) {
        return Err(anyhow!(clone_verification_failed_advice(
            format!(
                "clone verification failed: Git branch '{track_name}' did not map to the imported Heddle state"
            ),
            format!("Git branch '{track_name}' does not map to imported Heddle state {state_id}"),
            "continuing would leave the Git/Heddle mapping unproven for this clone",
            format!("heddle bridge git import --ref {track_name}"),
        )));
    }

    Ok(())
}

fn clone_verification_failed_advice(
    error: impl Into<String>,
    unsafe_condition: impl Into<String>,
    would_change: impl Into<String>,
    primary_command: impl Into<String>,
) -> RecoveryAdvice {
    let primary_command = primary_command.into();
    RecoveryAdvice::safety_refusal(
        "clone_verification_failed",
        error,
        format!("Repair the clone mapping, then rerun `{primary_command}`."),
        unsafe_condition,
        would_change,
        "the cloned files, Git refs, and Heddle metadata were left for inspection",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_overlay_import_failed_advice(
    requested_ref: Option<&str>,
    remote_label: &str,
    cause: String,
) -> RecoveryAdvice {
    let requested = requested_ref
        .map(|name| format!(" for requested ref '{name}'"))
        .unwrap_or_default();
    let primary_command = requested_ref
        .map(|name| format!("heddle clone {remote_label} <path> --thread {name}"))
        .unwrap_or_else(|| format!("heddle clone {remote_label} <path>"));
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_import_failed",
        format!("Git-overlay clone import failed{requested}: {cause}"),
        "Retry with an existing commit-pointing branch or repair the source repository, then clone again.",
        format!("Git-overlay import failed{requested}: {cause}"),
        "clone cannot create a trusted Git/Heddle mapping until the requested refs import cleanly",
        "the clone directory, copied Git objects, and Heddle metadata were left for inspection",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_overlay_branch_not_imported_advice(
    track_name: &str,
    remote_label: &str,
) -> RecoveryAdvice {
    let primary_command = format!("heddle clone {remote_label} <path> --thread {track_name}");
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_branch_not_imported",
        format!("Git clone did not import branch '{track_name}'"),
        "Retry with an existing commit-pointing branch or repair the source repository, then clone again.",
        format!(
            "Git-overlay clone selected branch '{track_name}', but no Heddle thread was imported for it"
        ),
        "materializing this clone would attach Git and Heddle to an untrusted or missing branch mapping",
        "the clone directory, copied Git objects, and Heddle metadata were left for inspection",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_overlay_no_branch_refs_advice(remote_label: &str) -> RecoveryAdvice {
    let primary_command = format!("heddle clone {remote_label} <path>");
    RecoveryAdvice::safety_refusal(
        "git_overlay_clone_no_branch_refs",
        "Git clone did not import any branch refs",
        "Clone from a repository with at least one commit-pointing branch, or pass `--thread <branch>` after creating one.",
        format!("Git-overlay import from '{remote_label}' produced no branch refs"),
        "clone cannot choose a trusted active branch without an imported Git/Heddle mapping",
        "the clone directory, copied Git objects, and Heddle metadata were left for inspection",
        primary_command.clone(),
        vec![primary_command],
    )
}

fn clone_git_step_failed_advice(args: &[&str], cause: impl Into<String>) -> RecoveryAdvice {
    let command = format!("git {}", args.join(" "));
    let cause = cause.into();
    RecoveryAdvice::safety_refusal(
        "clone_git_verification_failed",
        format!("Clone verification failed while running `{command}`: {cause}"),
        "Inspect the preserved clone, repair the Git/Heddle mapping, then run `heddle status`.",
        format!("Git verification command `{command}` failed: {cause}"),
        "clone cannot prove that the Git checkout and Heddle mapping agree",
        "the cloned files, Git refs, and Heddle metadata were left for inspection",
        "heddle status",
        vec!["heddle status".to_string()],
    )
}

#[cfg(feature = "client")]
fn network_clone_failed_advice(error: &str, local_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "network_clone_failed",
        format!("Clone failed: {error}"),
        "Check the remote, credentials, and requested ref, then retry `heddle clone`.",
        format!(
            "network clone reported failure for '{}': {error}",
            local_path.display()
        ),
        "clone cannot prove that all requested remote objects and refs were materialized",
        "any created destination files or metadata were left for inspection",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

#[cfg(not(feature = "client"))]
fn network_clone_unavailable_advice() -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "network_clone_unavailable",
        "Network clone support is not available in this build",
        "Use a build with the `client` feature enabled, or clone from a local path.",
        "this heddle binary was built without hosted/network clone support",
        "clone cannot contact hosted/network remotes without the client transport",
        "no destination directory, repository metadata, refs, or worktree files were written",
        "heddle clone <local-path> <path>",
        vec!["heddle clone <local-path> <path>".to_string()],
    )
}

fn git_available() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn ensure_git_excludes_heddle(local_path: &Path) -> Result<()> {
    let exclude_path = local_path.join(".git").join("info").join("exclude");
    if let Some(parent) = exclude_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut contents = fs::read_to_string(&exclude_path).unwrap_or_default();
    for pattern in [".heddle/", ".heddleignore"] {
        if !contents.lines().any(|line| line.trim() == pattern) {
            if !contents.ends_with('\n') && !contents.is_empty() {
                contents.push('\n');
            }
            contents.push_str(pattern);
            contents.push('\n');
        }
    }
    fs::write(exclude_path, contents)?;
    Ok(())
}

fn run_git_clone_step(local_path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(local_path)
        .args(args)
        .output()
        .map_err(|error| anyhow!(clone_git_step_failed_advice(args, error.to_string())))?;
    if !output.status.success() {
        return Err(anyhow!(clone_git_step_failed_advice(
            args,
            format!(
                "git {} exited with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        )));
    }
    Ok(())
}

fn git_output(local_path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(local_path)
        .args(args)
        .output()
        .map_err(|error| anyhow!(clone_git_step_failed_advice(args, error.to_string())))?;
    if !output.status.success() {
        return Err(anyhow!(clone_git_step_failed_advice(
            args,
            format!(
                "git {} exited with {}: {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Best-effort repo-name extraction for the text-mode clone summary.
///
/// The remote label can be a HTTPS URL, an SSH spec
/// (`git@host:owner/repo.git`), a `file://` URL, or a plain filesystem
/// path. We do not try to fully parse any of these — we just want the
/// last path-like segment so the human-facing line can say "Cloned
/// ripgrep" instead of dumping the whole URL again next to where the
/// URL was already echoed by the dim-styled source label. If the input
/// has no usable segment, return it unchanged so the rendered summary
/// still carries something identifying.
fn clone_repo_name_from_label(label: &str) -> &str {
    // `:` is only an SSH/SCP host/path separator when the prefix has no
    // path separator (git's local-path rule) and isn't a Windows drive
    // (`C:\…` or `C:/…`). Splitting unconditionally truncated Windows
    // drive paths and any local path with a literal colon.
    let after_colon = match label.find(':') {
        Some(colon_pos) => {
            let prefix = &label[..colon_pos];
            let rest = &label[colon_pos + 1..];
            let is_windows_drive = prefix.len() == 1
                && prefix
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic())
                && (rest.starts_with('\\') || rest.starts_with('/'));
            let prefix_has_separator = prefix.contains('/') || prefix.contains('\\');
            if is_windows_drive || prefix_has_separator {
                label
            } else {
                rest
            }
        }
        None => label,
    };
    let is_sep = |c: char| c == '/' || c == '\\';
    let segment = after_colon
        .trim_end_matches(is_sep)
        .rsplit(is_sep)
        .find(|part| !part.is_empty())
        .unwrap_or(after_colon);
    segment.strip_suffix(".git").unwrap_or(segment)
}

/// Render the human-facing clone-completion summary as three lines.
///
/// The shape — repo name + commit count, current thread, next-step
/// hint — comes from heddle#161: the previous text mode printed a terse
/// `cloned <url> into <path>` / `imported: N Git commits` pair that
/// scanned like a JSON dump rather than guidance. Returning a `Vec<String>`
/// (one entry per output line) keeps the formatter unit-testable without
/// having to capture process stdout.
fn format_clone_completion_lines(
    repo_name: &str,
    commits_imported: usize,
    thread_name: &str,
) -> Vec<String> {
    vec![
        format!(
            "{} Cloned {} ({} imported).",
            style::ok_marker(),
            style::bold(repo_name),
            style::count(commits_imported, "commit"),
        ),
        format!(
            "  {}",
            style::field("current thread", &style::bold(thread_name))
        ),
        format!("  Next: {}", style::bold("heddle log")),
    ]
}

/// Pick which imported branch the clone should land on.
///
/// Priority order:
///
/// 1. `--thread <name>` if the user asked for one explicitly. We
///    trust the user even if the name doesn't match a thread yet —
///    the subsequent `get_thread` lookup will surface a clear error.
/// 2. The branch the remote advertises as `HEAD` (passed in via
///    `git_head_branch_hint`, read from `.git/HEAD` after the bare
///    clone — `git clone --bare` and our `clone_url_to_bare` +
///    `git ls-remote --symref` path both mirror the remote's
///    symref). This is what fixes heddle#141: cloning ripgrep should
///    land on `master`, not the alphabetically-first imported branch
///    `ag/bstr-migration`.
/// 3. `"main"` if present — preserves the long-standing UX for
///    repos that *do* have a `main` branch but somehow lack a
///    `.git/HEAD` symref (e.g. transports that don't surface one).
/// 4. Alphabetically first imported thread, as a last resort. We
///    deliberately keep this fallback because erroring out on an
///    unhinted clone would be worse than landing on a working ref.
fn select_clone_thread(
    repo: &Repository,
    requested: Option<&str>,
    git_head_branch_hint: Option<&str>,
    remote_label: &str,
) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(requested.to_string());
    }
    let threads = repo.refs().list_threads()?;
    if let Some(hint) = git_head_branch_hint
        && threads.iter().any(|thread| thread == hint)
    {
        return Ok(hint.to_string());
    }
    if threads.iter().any(|thread| thread == "main") {
        return Ok("main".to_string());
    }
    threads
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!(clone_git_overlay_no_branch_refs_advice(remote_label)))
}

/// Read `.git/HEAD` as a symbolic ref into `refs/heads/`, returning
/// the bare branch name. Returns `None` for detached HEAD, malformed
/// files, or symrefs outside `refs/heads/` — none of which can drive
/// thread selection.
fn read_git_head_branch(git_dir: &Path) -> Option<String> {
    let contents = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let trimmed = contents.trim();
    let suffix = trimmed.strip_prefix("ref: ")?;
    let branch = suffix.strip_prefix("refs/heads/")?;
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

/// Pin `.git/HEAD` to `refs/heads/<branch>`. Called after clone so a
/// future `Repository::open` reads the same branch heddle attached to,
/// rather than the init-time default gix wrote (typically `main`).
fn write_git_head_branch(git_dir: &Path, branch: &str) -> Result<()> {
    fs::write(git_dir.join("HEAD"), format!("ref: refs/heads/{branch}\n"))?;
    Ok(())
}

async fn clone_local(
    cli: &Cli,
    remote_path: &Path,
    local_path: &Path,
    options: &CloneOptions,
) -> Result<()> {
    let CloneOptions {
        thread,
        depth,
        lazy,
        filter,
    } = options;
    let depth = *depth;
    if let Some(filter) = filter.as_deref() {
        return Err(anyhow!(local_clone_option_unsupported_advice(
            "--filter", filter
        )));
    }
    if *lazy {
        return Err(anyhow!(local_clone_option_unsupported_advice(
            "--lazy", "true"
        )));
    }

    if !remote_path.exists() {
        return Err(anyhow!(clone_remote_not_found_advice(remote_path)));
    }

    // Resolve the requested remote thread before creating the
    // destination. Missing-thread refusals should not leave behind a
    // half-initialized clone directory.
    let sync = LocalSync::open(remote_path)?;
    let remote_repo = sync.source();
    let track_name = thread.as_deref().unwrap_or("main");
    let state_id = remote_repo
        .refs()
        .get_thread(track_name)?
        .ok_or_else(|| clone_remote_thread_not_found_advice(track_name, remote_path))?;

    // Create and initialize the local repository only after all
    // preflight target selection has succeeded.
    fs::create_dir_all(local_path)?;
    let local_repo = Repository::init(local_path)?;

    // Fetch the state and dependencies
    let objects_copied = if let Some(d) = depth {
        sync.fetch_state_with_depth(&local_repo, &state_id, d)?
    } else {
        sync.fetch_state(&local_repo, &state_id)?
    };

    // Set up the thread locally
    local_repo.refs().set_thread(track_name, &state_id)?;

    // Intentional raw `goto`: a fresh `Repository::init` writes HEAD as
    // `Attached { thread: "main" }`, but we may be cloning a non-"main"
    // thread (e.g. `develop`). `fast_forward_attached` here would
    // mis-advance the "main" thread ref instead of the cloned one. Clone
    // post-HEAD-attach is tracked separately.
    local_repo.goto(&state_id)?;

    // Copy worktree files from remote (for file:// protocol, we can do a direct copy)
    copy_worktree(remote_repo.root(), local_repo.root())?;

    if should_output_json(cli, Some(local_repo.config())) {
        println!(
            "{{\"status\": \"cloned\", \"remote\": \"file://{}\", \"local\": \"{}\", \"objects\": {}}}",
            remote_path.display(),
            local_path.display(),
            objects_copied
        );
    } else {
        let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
        println!(
            "{} cloned {} into {}{}",
            style::ok_marker(),
            style::dim(&format!("file://{}", remote_path.display())),
            style::bold(&local_path.display().to_string()),
            style::dim(&depth_info)
        );
        println!(
            "  {}",
            style::field("copied", &style::count(objects_copied, "object"))
        );
    }

    Ok(())
}

fn local_clone_option_unsupported_advice(option: &'static str, value: &str) -> RecoveryAdvice {
    let detail = if option == "--filter" {
        format!("{option} {value}")
    } else {
        option.to_string()
    };
    RecoveryAdvice::safety_refusal(
        "local_clone_option_unsupported",
        format!("{detail} is only supported for hosted/network remotes"),
        "Retry without lazy/filter options for local remotes, or use a hosted/network remote that supports lazy materialization.",
        format!("selected clone transport is local but {detail} requires hosted/network hydration"),
        "clone cannot create a lazy local checkout because the local transport does not provide on-demand object hydration",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn clone_remote_not_found_advice(remote_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_remote_not_found",
        format!(
            "Remote repository '{}' does not exist",
            remote_path.display()
        ),
        "Check the remote path or URL, then retry `heddle clone` with an existing repository.",
        format!(
            "remote repository '{}' does not exist or is not reachable as a local path",
            remote_path.display()
        ),
        "clone cannot read refs, objects, or worktree data from the requested source",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

fn clone_remote_thread_not_found_advice(track_name: &str, remote_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_remote_thread_not_found",
        format!("Thread '{track_name}' not found in remote"),
        "Inspect the remote with `heddle thread list`, then retry `heddle clone --thread <thread>` with an existing thread.",
        format!(
            "remote '{}' has no Heddle thread named '{track_name}'",
            remote_path.display()
        ),
        "clone cannot choose a state to fetch or materialize until the remote thread resolves",
        "destination path was left unchanged; no local clone repository was initialized",
        "heddle thread list",
        vec!["heddle thread list".to_string()],
    )
}

/// Extract the `host:port` substring from a raw remote URL so the lazy
/// hydrator config can persist it instead of the post-DNS `SocketAddr`.
/// Keeping the hostname matters when the upstream service rotates IPs
/// (e.g. behind a load balancer): a SocketAddr baked into the marker at
/// clone time would pin to a stale IP and break later hydrate calls even
/// though the original URL still resolves. The hydrator re-resolves DNS
/// on every process start when given a hostname spec.
#[cfg(feature = "client")]
fn hosted_endpoint_spec(remote: &str) -> String {
    let trimmed = remote.strip_prefix("heddle://").unwrap_or(remote);
    // The address ends at the first slash that introduces a repo path.
    trimmed.split('/').next().unwrap_or(trimmed).to_string()
}

#[cfg(feature = "client")]
async fn clone_network(
    cli: &Cli,
    addr: std::net::SocketAddr,
    repo_path: Option<&str>,
    local_path: &Path,
    options: &CloneOptions,
    server_key: Option<String>,
    endpoint_spec: String,
) -> Result<()> {
    use crate::{client::HostedGrpcClient, config::UserConfig};

    let CloneOptions {
        thread,
        depth,
        lazy,
        filter,
    } = options;
    let depth = *depth;
    // `--filter blob:none` is a synonym for `--lazy` on hosted/network
    // remotes; both produce a clone whose blob content is hydrated on demand.
    let lazy = *lazy || filter.is_some();

    // Create the local directory
    fs::create_dir_all(local_path)?;

    // Initialize the local repository
    let local_repo = Repository::init(local_path)?;

    let user_config = UserConfig::load_default().unwrap_or_default();

    // Connect to remote
    let mut config = user_config.heddle_client_config(None);
    if let Some(key) = server_key {
        config = config.with_server_key(key);
    }
    let repo_path = repo_path.context("network remotes must include a hosted repository path")?;

    let mut client = HostedGrpcClient::connect(addr, &config).await?;
    client.auto_rotate_if_needed().await;

    if should_output_json(cli, Some(local_repo.config())) {
        println!("{{\"status\":\"connected\",\"address\":\"{}\"}}", addr);
    } else {
        println!("Connected to {}", addr);
    }

    let track_name = thread.as_deref().unwrap_or("main");
    let materialization = if lazy {
        PullMaterialization::Lazy
    } else {
        PullMaterialization::Full
    };
    let result = client
        .pull_with_depth_and_materialization(
            &local_repo,
            repo_path,
            track_name,
            Some(track_name),
            depth,
            materialization,
        )
        .await?;
    if result.success {
        // Lazy clone: persist the hydrator metadata so future
        // `Repository::open` calls (in any process) can reconstruct
        // the on-read hydrator. Without this, lazy clones would only
        // hydrate inside the single `cmd_clone` process — every
        // subsequent `heddle <verb>` would surface MissingObject on
        // any blob read.
        if lazy {
            use repo::lazy_hydrator::LazyHydratorConfig;
            // Persist the original `host:port` spec (not `addr.to_string()`,
            // which is a resolved IP). The hydrator re-resolves DNS on
            // every process start so a future LB rotation doesn't pin us
            // to a stale IP.
            let cfg = LazyHydratorConfig::hosted(endpoint_spec, repo_path, track_name, track_name);
            cfg.save(local_repo.heddle_dir())
                .context("failed to persist lazy-hydrator.toml")?;
        }
        if should_output_json(cli, Some(local_repo.config())) {
            println!(
                "{{\"status\": \"cloned\", \"remote\": \"{}\", \"local\": \"{}\", \"state\": \"{}\"}}",
                addr,
                local_path.display(),
                result
                    .final_state
                    .map(|s| s.to_string())
                    .unwrap_or_default()
            );
        } else {
            let depth_info = depth.map(|d| format!(" (depth {})", d)).unwrap_or_default();
            println!(
                "{} cloned {} into {}{}",
                style::ok_marker(),
                style::dim(&addr.to_string()),
                style::bold(&local_path.display().to_string()),
                style::dim(&depth_info)
            );
            if let Some(state) = result.final_state {
                println!(
                    "  {}",
                    style::field("state", &style::change_id(&state.to_string()))
                );
            }
        }
    } else {
        let err = result.error.unwrap_or_else(|| "Unknown error".to_string());
        return Err(anyhow!(network_clone_failed_advice(&err, local_path)));
    }

    Ok(())
}

fn copy_worktree(from: &Path, to: &Path) -> Result<()> {
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let file_name = entry.file_name();

        if file_name == ".heddle" || file_name == ".git" {
            continue;
        }

        let dest_path = to.join(&file_name);
        copy_entry(&path, &dest_path)?;
    }

    Ok(())
}

fn copy_dir_recursive(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;

    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let path = entry.path();
        let dest_path = to.join(entry.file_name());
        copy_entry(&path, &dest_path)?;
    }

    Ok(())
}

fn copy_entry(path: &Path, dest_path: &Path) -> Result<()> {
    if path.is_symlink() {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, dest_path)?;
        #[cfg(not(unix))]
        return Err(anyhow!(clone_symlink_unsupported_advice(path, dest_path)));
    } else if path.is_dir() {
        copy_dir_recursive(path, dest_path)?;
    } else {
        fs::copy(path, dest_path)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn clone_symlink_unsupported_advice(path: &Path, dest_path: &Path) -> RecoveryAdvice {
    RecoveryAdvice::safety_refusal(
        "clone_symlink_unsupported",
        "Symlinks are not supported on this platform",
        "Retry on a platform with symlink support, or remove the symlink from the source before cloning.",
        format!(
            "source path '{}' is a symlink but this platform cannot create symlinks",
            path.display()
        ),
        format!(
            "clone would need to create symlink '{}' to preserve the worktree exactly",
            dest_path.display()
        ),
        "the clone operation stopped before replacing the unsupported symlink with different file contents",
        "heddle clone <remote> <path>",
        vec!["heddle clone <remote> <path>".to_string()],
    )
}

/// Read-time blob hydrator for **Git-overlay** lazy clones (issue #50).
///
/// Plugs into [`repo::Repository::set_blob_hydrator`]. When
/// [`Repository::require_blob`] hits a missing-blob marker — i.e. the
/// blake3-hashed blob is recorded in `.heddle/partial-fetch` but is
/// absent from the local object store — the read path delegates here.
/// This hydrator looks up the corresponding Git object id, fetches the
/// blob from the underlying gix repo (which triggers the gix promisor
/// fetch against the original remote when `extensions.partialClone =
/// origin` is set in `.git/config`), and writes the bytes into the
/// heddle store. The retry-read then surfaces the blob normally.
///
/// ## Why a side-table?
///
/// `PartialFetchMetadata` records blake3 hashes only, but
/// `gix::Repository::find_blob` is keyed by Git OID. The bridge
/// already computes blake3↔git mappings *for commits* (see
/// `SyncMapping` in `bridge/git_core.rs`); blob mappings are
/// constructed on-the-fly during import. We accept the same shape of
/// mapping here, populated by the caller (clone-time or test-time)
/// before [`Self::hydrate`] fires. Future work: persist a sidecar
/// blob mapping during import so a fresh `Repository::open` in a
/// separate process can rebuild this map without re-walking history.
pub struct GitOverlayBlobHydrator {
    git_repo_path: PathBuf,
    /// Pre-seeded blake3 → git OID mapping for missing blobs. Held
    /// behind `Mutex` so a long-lived `Arc<GitOverlayBlobHydrator>` is
    /// `Send + Sync` while still allowing the mapping to grow over
    /// time (e.g. if the import path is later extended to record new
    /// blobs as it walks).
    blob_oid_map: Mutex<std::collections::HashMap<ContentHash, gix::ObjectId>>,
}

impl GitOverlayBlobHydrator {
    pub fn new(git_repo_path: PathBuf) -> Self {
        Self {
            git_repo_path,
            blob_oid_map: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// Pre-seed the blake3 → git OID mapping. Called by the importer
    /// (or by tests) as missing blobs are discovered.
    pub fn record_blob_oid(&self, hash: ContentHash, oid: gix::ObjectId) {
        self.blob_oid_map.lock().unwrap().insert(hash, oid);
    }
}

impl BlobHydrator for GitOverlayBlobHydrator {
    fn hydrate(&self, repo: &Repository, hash: &ContentHash) -> HeddleResult<()> {
        let oid = self
            .blob_oid_map
            .lock()
            .unwrap()
            .get(hash)
            .copied()
            .ok_or_else(|| {
                HeddleError::Config(format!(
                    "Git-overlay hydrator has no Git OID mapping for blake3 {}; \
                     the importer must call `record_blob_oid` for every missing blob \
                     before reads can be served lazily",
                    hash.to_hex()
                ))
            })?;

        // Try the local ODB first; if absent, shell out to git which
        // honours `extensions.partialClone = <remote>` and triggers
        // the promisor fetch on miss. gix 0.80 cannot do the
        // promisor fetch itself (the v2 `filter` capability isn't
        // surfaced through its fetch builder), but it CAN read the
        // bytes after `git` has populated them — so a follow-up
        // local lookup is enough to keep the loose-blob bookkeeping
        // in one place.
        let bytes = self.read_blob_bytes(oid)?;
        let heddle_blob = Blob::new(bytes);
        // Sanity-check the upstream gave us bytes that match the
        // blake3 we were asked for — protects against an oid mapping
        // corruption silently delivering the wrong content.
        let computed = heddle_blob.hash();
        if computed != *hash {
            return Err(HeddleError::Corruption {
                expected: *hash,
                found: computed,
            });
        }
        repo.store().put_blob(&heddle_blob)?;
        Ok(())
    }
}

impl GitOverlayBlobHydrator {
    fn read_blob_bytes(&self, oid: gix::ObjectId) -> HeddleResult<Vec<u8>> {
        let local_first = open_repo(&self.git_repo_path)
            .map_err(|err| HeddleError::Io(std::io::Error::other(err.to_string())))?
            .find_blob(oid)
            .ok()
            .map(|mut blob| blob.take_data());
        if let Some(bytes) = local_first {
            return Ok(bytes);
        }

        // Promisor refetch via the git CLI. The git binary speaks the
        // full v2 wire protocol and honours `extensions.partialClone`,
        // so a `cat-file -p` against a missing blob transparently
        // fetches it from the recorded remote. `--batch-command` or
        // `-p` both work; `-p` is the simpler one-shot.
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.git_repo_path)
            .args(["cat-file", "-p"])
            .arg(oid.to_string())
            .output()
            .map_err(HeddleError::Io)?;
        if !output.status.success() {
            return Err(HeddleError::Io(std::io::Error::other(format!(
                "git cat-file -p {oid} in {} failed: {}",
                self.git_repo_path.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))));
        }
        Ok(output.stdout)
    }
}

/// Register the `"git-overlay"` factory in the global lazy-hydrator
/// registry. Call once at process startup (from `main()`) so a
/// `Repository::open` on a lazy-cloned repo can reconstruct the
/// hydrator without re-running `cmd_clone`.
///
/// Note: the rebuilt hydrator's `blob_oid_map` starts empty, since the
/// blake3 → git-OID map is populated only by the importer (currently
/// in-process only). Cross-process git-overlay lazy reads are not yet
/// fully wired — `--lazy` for git-overlay clones is rejected at the
/// flag-validation surface (see `reject_unsupported_for_git_overlay`),
/// so this factory is registered for symmetry and forward-compat with
/// follow-up work that persists the OID map sidecar. Until then the
/// hydrator returns the descriptive `"no Git OID mapping"` error if a
/// missing blob is requested.
pub fn register_git_overlay_factory() {
    use std::{path::Path as StdPath, sync::Arc as StdArc};

    use repo::lazy_hydrator::{
        BlobHydratorFactory, HydratorSection, KIND_GIT_OVERLAY, register_factory,
    };

    let factory: BlobHydratorFactory = StdArc::new(
        |root: &StdPath, _section: &HydratorSection| -> HeddleResult<StdArc<dyn BlobHydrator>> {
            let bare = root.join(".git");
            Ok(StdArc::new(GitOverlayBlobHydrator::new(bare)))
        },
    );
    register_factory(KIND_GIT_OVERLAY, factory);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(depth: Option<u32>, lazy: bool, filter: Option<&str>) -> CloneOptions {
        CloneOptions {
            thread: None,
            depth,
            lazy,
            filter: filter.map(str::to_string),
        }
    }

    #[test]
    fn reject_unsupported_passes_when_no_flags_set() {
        assert!(reject_unsupported_for_git_overlay(&opts(None, false, None)).is_ok());
    }

    // ---- clone text-mode summary helpers (heddle#161) ----

    #[test]
    fn clone_repo_name_strips_https_url_and_dot_git() {
        assert_eq!(
            clone_repo_name_from_label("https://github.com/BurntSushi/ripgrep.git"),
            "ripgrep"
        );
        assert_eq!(
            clone_repo_name_from_label("https://github.com/BurntSushi/ripgrep"),
            "ripgrep"
        );
    }

    #[test]
    fn clone_repo_name_strips_ssh_url_and_dot_git() {
        assert_eq!(
            clone_repo_name_from_label("git@github.com:owner/repo.git"),
            "repo"
        );
    }

    #[test]
    fn clone_repo_name_extracts_last_filesystem_segment() {
        assert_eq!(clone_repo_name_from_label("/home/user/foo"), "foo");
        assert_eq!(
            clone_repo_name_from_label("file:///tmp/projects/bar/"),
            "bar"
        );
    }

    #[test]
    fn clone_repo_name_falls_back_to_label_when_no_segment() {
        // Empty or pathologic input: don't panic, return the input as-is
        // so the rendered summary still carries *something* identifying.
        assert_eq!(clone_repo_name_from_label(""), "");
        assert_eq!(clone_repo_name_from_label("///"), "///");
    }

    #[test]
    fn clone_repo_name_handles_windows_drive_paths() {
        // Windows drive prefixes (`C:\…` and `C:/…`) are not SSH/SCP
        // shorthand — earlier versions unconditionally split on `:` and
        // dropped the drive letter, producing `\src\ripgrep` instead of
        // `ripgrep`.
        assert_eq!(clone_repo_name_from_label("C:\\src\\ripgrep"), "ripgrep");
        assert_eq!(clone_repo_name_from_label("C:/src/ripgrep"), "ripgrep");
        assert_eq!(
            clone_repo_name_from_label("D:\\workspaces\\heddle.git"),
            "heddle"
        );
    }

    #[test]
    fn clone_repo_name_handles_local_paths_with_colon() {
        // Git treats `host:path` as SCP shorthand only when the prefix
        // contains no path separator. `/tmp/foo:bar/repo` and
        // `./foo:bar/repo.git` are valid local paths whose basename
        // must not be shadowed by the colon.
        assert_eq!(clone_repo_name_from_label("/tmp/foo:bar/repo.git"), "repo");
        assert_eq!(clone_repo_name_from_label("./foo:bar/repo.git"), "repo");
    }

    #[test]
    fn format_clone_completion_text_names_repo_and_count_and_thread_and_next_command() {
        // Style helpers are no-ops when color is uninitialized (test
        // default), so substring assertions work on the raw text. Keeps
        // the assertions independent of ANSI escape sequences.
        let lines = format_clone_completion_lines("ripgrep", 2249, "master");
        let joined = lines.join("\n");
        assert!(
            joined.contains("ripgrep"),
            "summary must name the repo: {joined}"
        );
        assert!(
            joined.contains("2249"),
            "summary must include the commit count: {joined}"
        );
        assert!(
            joined.contains("commit"),
            "summary must use the word 'commit': {joined}"
        );
        assert!(
            joined.contains("master"),
            "summary must name the current thread: {joined}"
        );
        assert!(
            joined.to_lowercase().contains("heddle log"),
            "summary must suggest `heddle log` as the next step: {joined}"
        );
    }

    #[test]
    fn format_clone_completion_singularizes_one_commit() {
        // Avoid "1 commits" — the style::count helper already singularizes,
        // but pin it here so a future formatter refactor doesn't regress.
        let lines = format_clone_completion_lines("tiny", 1, "main");
        let joined = lines.join("\n");
        assert!(
            joined.contains("1 commit ")
                || joined.contains("1 commit\n")
                || joined.ends_with("1 commit"),
            "one commit must not pluralize: {joined}"
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_endpoint_spec_preserves_hostname_with_port() {
        // The lazy-hydrator marker must carry the original hostname so
        // the hydrator can re-resolve DNS on every process start. If we
        // accidentally persist a resolved IP, hosts behind a rotating-IP
        // load balancer break on the next process restart.
        assert_eq!(
            hosted_endpoint_spec("example.heddle.cloud:443"),
            "example.heddle.cloud:443",
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_endpoint_spec_strips_scheme_prefix() {
        assert_eq!(
            hosted_endpoint_spec("heddle://example.heddle.cloud:443"),
            "example.heddle.cloud:443",
        );
    }

    #[cfg(feature = "client")]
    #[test]
    fn hosted_endpoint_spec_strips_repo_path_suffix() {
        assert_eq!(
            hosted_endpoint_spec("example.heddle.cloud:443/org/acme/repo"),
            "example.heddle.cloud:443",
        );
        assert_eq!(
            hosted_endpoint_spec("heddle://example.heddle.cloud:443/org/acme/repo"),
            "example.heddle.cloud:443",
        );
    }

    #[test]
    fn reject_unsupported_rejects_filter() {
        let err = reject_unsupported_for_git_overlay(&opts(None, false, Some("blob:none")))
            .expect_err("filter must be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("filter refusal should use typed recovery advice");
        assert_eq!(advice.kind, "git_overlay_clone_option_unsupported");
        assert_eq!(advice.primary_command, "heddle clone <remote> <path>");
        assert!(advice.preserved.contains("no clone directory"));
        let msg = err.to_string();
        assert!(
            msg.contains("--filter"),
            "message must name --filter: {msg}"
        );
        assert!(
            msg.contains("not yet supported"),
            "message must say not yet supported: {msg}"
        );
    }

    #[test]
    fn reject_unsupported_rejects_lazy() {
        let err = reject_unsupported_for_git_overlay(&opts(None, true, None))
            .expect_err("lazy must be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("lazy refusal should use typed recovery advice");
        assert_eq!(advice.kind, "git_overlay_clone_option_unsupported");
        assert!(advice.primary_hint().contains("full Git-overlay clone"));
        let msg = err.to_string();
        assert!(msg.contains("--lazy"), "message must name --lazy: {msg}");
        assert!(
            msg.contains("not yet supported"),
            "message must say not yet supported: {msg}"
        );
    }

    #[test]
    fn reject_unsupported_rejects_depth() {
        let err = reject_unsupported_for_git_overlay(&opts(Some(1), false, None))
            .expect_err("depth must be rejected");
        let advice = err
            .chain()
            .find_map(|cause| cause.downcast_ref::<RecoveryAdvice>())
            .expect("depth refusal should use typed recovery advice");
        assert_eq!(advice.kind, "git_overlay_clone_option_unsupported");
        assert!(advice.error.contains("shallow boundary"));
        let msg = err.to_string();
        assert!(msg.contains("--depth"), "message must name --depth: {msg}");
        assert!(
            msg.contains("not yet supported"),
            "message must say not yet supported: {msg}"
        );
    }

    #[test]
    fn clone_verification_failures_use_typed_recovery_advice() {
        let advice = clone_verification_failed_advice(
            "clone verification failed: Heddle active thread is 'dev', expected 'main'",
            "Heddle active thread 'dev' does not match imported Git branch 'main'",
            "continuing would report the clone as trusted while Heddle is attached to the wrong thread",
            "heddle thread switch main --force",
        );

        assert_eq!(advice.kind, "clone_verification_failed");
        assert!(
            advice
                .error
                .contains("clone verification failed: Heddle active thread")
        );
        assert_eq!(advice.primary_command, "heddle thread switch main --force");
        assert_eq!(
            advice.recovery_commands,
            vec!["heddle thread switch main --force"]
        );
        assert!(advice.preserved.contains("left for inspection"));
        assert!(advice.primary_hint().contains("Repair the clone mapping"));
    }

    /// Standalone helpers to exercise [`GitOverlayBlobHydrator`]'s
    /// error and fallback branches that the kernel/hermetic end-to-end
    /// test (in `tests/lazy_blob_hydration_kernel.rs`) doesn't reach.
    /// Each test sets up the smallest possible bare gix repo it needs;
    /// none of them hit the network.
    mod git_overlay_hydrator {
        use objects::object::ContentHash;
        use repo::{BlobHydrator, Repository};
        use tempfile::TempDir;

        use super::*;

        /// Build a fresh empty bare gix repo and a fresh `Repository`,
        /// returning `(temp, bare_path, repo)` for use in a single test.
        fn fixtures() -> (TempDir, std::path::PathBuf, Repository) {
            let temp = TempDir::new().expect("temp");
            let bare = temp.path().join("source.git");
            gix::init_bare(&bare).expect("init bare gix");
            let heddle_root = temp.path().join("heddle");
            std::fs::create_dir_all(&heddle_root).expect("mkdir heddle");
            let repo =
                Repository::init_default(&heddle_root).expect("init heddle repo for hydrator");
            (temp, bare, repo)
        }

        /// Write a single blob into the bare repo and return its OID.
        fn write_local_blob(bare: &std::path::Path, payload: &[u8]) -> gix::ObjectId {
            let g = gix::open(bare).expect("open bare");
            g.write_blob(payload).expect("write blob").detach()
        }

        #[test]
        fn hydrate_errors_descriptively_when_blob_oid_mapping_is_missing() {
            let (_temp, bare, repo) = fixtures();
            let hydrator = GitOverlayBlobHydrator::new(bare);
            let blake3 = objects::object::Blob::new(b"unknown".to_vec()).hash();

            let err = hydrator
                .hydrate(&repo, &blake3)
                .expect_err("missing mapping must be an error");
            let msg = err.to_string();
            assert!(
                msg.contains("no Git OID mapping"),
                "error message must explain why the mapping is missing: {msg}"
            );
            assert!(
                msg.contains(&blake3.to_hex()),
                "error message must name the blake3 the caller asked for: {msg}"
            );
        }

        #[test]
        fn hydrate_rejects_corrupted_mapping_via_blake3_check() {
            // Mapping points at an OID whose bytes don't match the
            // requested blake3 — the hydrator must NOT silently
            // deliver the wrong content. (Defends against a stale or
            // mis-imported sidecar mapping.)
            let (_temp, bare, repo) = fixtures();
            let real_bytes = b"genuine content".to_vec();
            let oid = write_local_blob(&bare, &real_bytes);

            let lying_blake3 = objects::object::Blob::new(b"different content".to_vec()).hash();
            let hydrator = GitOverlayBlobHydrator::new(bare);
            hydrator.record_blob_oid(lying_blake3, oid);

            let err = hydrator
                .hydrate(&repo, &lying_blake3)
                .expect_err("corrupted mapping must be rejected");
            assert!(
                matches!(err, objects::error::HeddleError::Corruption { .. }),
                "expected Corruption, got: {err:?}"
            );
        }

        #[test]
        fn read_blob_bytes_local_first_path_succeeds() {
            // Direct test of the local-first branch in
            // `read_blob_bytes` — independent of the trait hydrate
            // wrapper so the branch is reachable even if the trait
            // surface evolves.
            let (_temp, bare, _repo) = fixtures();
            let payload = b"local first".to_vec();
            let oid = write_local_blob(&bare, &payload);

            let hydrator = GitOverlayBlobHydrator::new(bare);
            let bytes = hydrator
                .read_blob_bytes(oid)
                .expect("local-first lookup must succeed");
            assert_eq!(bytes, payload);
        }

        #[test]
        fn read_blob_bytes_falls_back_to_git_cli_and_surfaces_its_error() {
            // No blob in the bare repo for this OID and no remote to
            // refetch from → git cat-file -p exits non-zero and the
            // error must surface with the OID + bare-repo path
            // mentioned so an operator can debug it.
            let (_temp, bare, _repo) = fixtures();
            let absent_oid = gix::ObjectId::null(gix::hash::Kind::Sha1);
            let hydrator = GitOverlayBlobHydrator::new(bare.clone());

            let err = hydrator
                .read_blob_bytes(absent_oid)
                .expect_err("missing blob + no promisor must fail");
            let msg = err.to_string();
            assert!(
                msg.contains("git cat-file"),
                "error must name the fallback command: {msg}"
            );
            assert!(
                msg.contains(&absent_oid.to_string()),
                "error must include the OID we asked for: {msg}"
            );
            assert!(
                msg.contains(&bare.display().to_string()),
                "error must include the bare-repo path: {msg}"
            );
        }

        #[test]
        fn record_blob_oid_is_last_write_wins_for_a_given_blake3() {
            // The importer may revisit a blake3 (e.g. when an
            // ancestry walk hits the same blob via two trees);
            // `record_blob_oid` is documented as a side-table insert,
            // not a checked-insert, so the second write is the value
            // any subsequent hydrate sees. Pin that behaviour so
            // future tightening to checked-insert doesn't silently
            // change semantics under existing callers.
            let (_temp, bare, _repo) = fixtures();
            let bytes_a = b"first".to_vec();
            let bytes_b = b"second".to_vec();
            let oid_a = write_local_blob(&bare, &bytes_a);
            let oid_b = write_local_blob(&bare, &bytes_b);
            // Two different blob bodies, but we deliberately pin both
            // OIDs to the SAME blake3 (the blake3 of bytes_b) so the
            // hydrate call ends up reading whichever OID is currently
            // recorded for that blake3 — that's what the test is about.
            let blake3 =
                ContentHash::from_hex(&objects::object::Blob::new(bytes_b.clone()).hash().to_hex())
                    .unwrap();

            let hydrator = GitOverlayBlobHydrator::new(bare.clone());
            hydrator.record_blob_oid(blake3, oid_a);
            hydrator.record_blob_oid(blake3, oid_b);

            // The current stored mapping is oid_b → so read_blob_bytes
            // should return bytes_b.
            let bytes = hydrator.read_blob_bytes(oid_b).expect("read");
            assert_eq!(bytes, bytes_b);
            // Independent sanity check via the original oid_a path.
            let bytes_a_read = hydrator.read_blob_bytes(oid_a).expect("read a");
            assert_eq!(bytes_a_read, bytes_a);
        }
    }
}
