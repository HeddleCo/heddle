// SPDX-License-Identifier: Apache-2.0
//! Opt-in end-to-end coverage for the real Heddle client against a live weft.
//!
//! This test deliberately does not start or mock weft. See
//! `docs/testing/live-weft-client-flow.md` for the required environment and
//! exact invocation.

#![cfg(feature = "client")]

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, ensure};
use hosted_client::client::{HostedAuthMode, HostedClient, HostedSession};
use objects::{object::StateId, store::ObjectStore};
use repo::{Repository, remote::RemoteTarget};
use tempfile::TempDir;

const WEFT_URL_ENV: &str = "HEDDLE_E2E_WEFT_URL";
const PUBLIC_GIT_URL_ENV: &str = "HEDDLE_E2E_PUBLIC_GIT_URL";
const NAMED_THREAD: &str = "client-flow-e2e";

#[derive(Clone, Debug, Eq, PartialEq)]
struct AdvertisedThread {
    state: StateId,
    id: String,
}

/// Exercises public Git fetch in the real weft executor, then reads the
/// imported source back through the production hosted clone path.
#[tokio::test]
#[ignore = "requires a live weft; see docs/testing/live-weft-client-flow.md"]
async fn real_import_source_fetches_public_git_and_clones_round_trip() -> Result<()> {
    let Some(endpoint) = std::env::var_os(WEFT_URL_ENV) else {
        eprintln!("skipping live-weft source import: {WEFT_URL_ENV} is unset");
        return Ok(());
    };
    let endpoint = endpoint
        .into_string()
        .map_err(|_| anyhow!("{WEFT_URL_ENV} must be valid UTF-8"))?;
    let authority = authority_only_endpoint(&endpoint)?;
    let source_url = std::env::var(PUBLIC_GIT_URL_ENV)
        .unwrap_or_else(|_| "https://github.com/octocat/Hello-World.git".to_string());

    let temp = TempDir::new().context("create source-import e2e root")?;
    let expected = temp.path().join("expected-git");
    let expected_text = path_text(&expected, "expected Git checkout")?;
    run_git(
        temp.path(),
        &["clone", "--quiet", "--depth=1", &source_url, &expected_text],
    )?;

    let mut client = connect_client(&endpoint, &authority).await?;
    let personal = client
        .get_current_user_spool()
        .await
        .context("resolve authenticated user's personal spool")?;
    client.close().await;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    let destination = format!(
        "{}/import-source-e2e-{}-{nonce}",
        personal.full_path,
        std::process::id()
    );
    let remote_url = format!("{}/{destination}", endpoint.trim_end_matches('/'));
    let output = run_heddle(
        temp.path(),
        &[
            "remote",
            "import-source",
            &source_url,
            "--to",
            &remote_url,
            "--name",
            "main",
        ],
    )?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let terminal = stdout
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .context("source import emitted no JSON operation event")?;
    let terminal: serde_json::Value =
        serde_json::from_str(terminal).context("decode terminal source-import event")?;
    ensure!(
        terminal.get("output_kind").and_then(|value| value.as_str()) == Some("import_source")
            && terminal.get("state").and_then(|value| value.as_str()) == Some("completed")
            && terminal.get("terminal").and_then(|value| value.as_bool()) == Some(true),
        "source import did not emit a completed terminal event: {terminal}"
    );

    let cloned = temp.path().join("cloned-import");
    clone_thread(temp.path(), &remote_url, &cloned, Some("main"))?;
    ensure!(
        file_snapshot(&expected)? == file_snapshot(&cloned)?,
        "fresh hosted clone did not match files and content at the public Git HEAD"
    );
    Ok(())
}

/// Exercises the complete native client lifecycle against a real weft.
///
/// Ignored because it requires externally managed weft, Postgres, object
/// storage, and credentials. It also remains intentionally red on the
/// first-`main`-push metadata assertion until heddle#1638 lands.
#[tokio::test]
#[ignore = "requires a live weft; see docs/testing/live-weft-client-flow.md"]
async fn real_heddle_push_pull_clone_preserves_hosted_thread_identity() -> Result<()> {
    let Some(endpoint) = std::env::var_os(WEFT_URL_ENV) else {
        eprintln!("skipping live-weft client flow: {WEFT_URL_ENV} is unset");
        return Ok(());
    };
    let endpoint = endpoint
        .into_string()
        .map_err(|_| anyhow!("{WEFT_URL_ENV} must be valid UTF-8"))?;
    let authority = authority_only_endpoint(&endpoint)?;

    let temp = TempDir::new().context("create live-weft e2e root")?;
    let source = unique_repo_path(temp.path())?;
    fs::create_dir(&source).context("create source repository directory")?;

    run_heddle(&source, &["init"])?;
    run_heddle(&source, &["remote", "add", "origin", &endpoint])?;

    fs::write(source.join("main.txt"), "main-v1\n").context("write first main state")?;
    run_heddle(&source, &["capture", "-m", "live e2e main v1"])?;
    let main_v1 = local_thread_state(&source, "main")?;

    // An authority-only remote auto-provisions a fresh child spool and rewrites
    // origin to its full path. That keeps every run isolated without hardcoding
    // a weft port, namespace, spool name, or credential.
    run_heddle(&source, &["push", "origin"])?;
    let remote_url = configured_origin(&source)?;
    let (remote_authority, remote_path) = hosted_remote_parts(&remote_url)?;
    ensure!(
        remote_authority == authority,
        "auto-provisioned remote changed authority from {authority} to {remote_authority}"
    );

    let mut client = connect_client(&remote_url, &remote_authority).await?;
    let first_main = advertised_thread(&mut client, &remote_path, "main").await?;
    ensure!(
        first_main.state == main_v1,
        "first main push advertised state {} instead of {}",
        first_main.state,
        main_v1
    );
    assert_thread_metadata(
        &mut client,
        &source,
        &remote_path,
        "main",
        main_v1,
        &first_main.id,
    )
    .await?;
    client.close().await;

    fs::write(source.join("main.txt"), "main-v2\n").context("write second main state")?;
    run_heddle(&source, &["capture", "-m", "live e2e main v2"])?;
    let main_v2 = local_thread_state(&source, "main")?;
    ensure!(
        main_v2 != main_v1,
        "second main capture did not advance main"
    );

    run_heddle(&source, &["push", "origin"])?;
    let mut client = connect_client(&remote_url, &remote_authority).await?;
    let second_main = advertised_thread(&mut client, &remote_path, "main").await?;
    ensure!(
        second_main.id == first_main.id,
        "REGRESSION: second main push changed hosted thread_id from {:?} to {:?}",
        first_main.id,
        second_main.id
    );
    ensure!(
        second_main.state == main_v2,
        "second main push advertised state {} instead of {}",
        second_main.state,
        main_v2
    );
    assert_thread_metadata(
        &mut client,
        &source,
        &remote_path,
        "main",
        main_v2,
        &first_main.id,
    )
    .await?;
    client.close().await;

    let named_checkout = temp.path().join("named-checkout");
    let named_checkout_arg = path_text(&named_checkout, "named checkout")?;
    run_heddle(
        &source,
        &["start", NAMED_THREAD, "--path", named_checkout_arg.as_str()],
    )?;
    fs::write(named_checkout.join("named.txt"), "named-v1\n")
        .context("write named-thread state")?;
    run_heddle(&named_checkout, &["capture", "-m", "live e2e named thread"])?;
    let named_state = local_thread_state(&named_checkout, NAMED_THREAD)?;
    run_heddle(&named_checkout, &["push", "origin"])?;

    let mut client = connect_client(&remote_url, &remote_authority).await?;
    let main_after_named = advertised_thread(&mut client, &remote_path, "main").await?;
    let named = advertised_thread(&mut client, &remote_path, NAMED_THREAD).await?;
    ensure!(
        main_after_named == second_main,
        "named-thread push changed main's advertised identity or state"
    );
    ensure!(
        !named.id.is_empty(),
        "named thread was advertised with an empty thread_id"
    );
    ensure!(
        named.id != first_main.id,
        "named thread reused main's hosted thread_id {:?}",
        named.id
    );
    ensure!(
        named.state == named_state,
        "named push advertised state {} instead of {}",
        named.state,
        named_state
    );
    assert_thread_metadata(
        &mut client,
        &named_checkout,
        &remote_path,
        NAMED_THREAD,
        named_state,
        &named.id,
    )
    .await?;
    client.close().await;

    let pulled = temp.path().join("pulled");
    fs::create_dir(&pulled).context("create pull destination")?;
    run_heddle(&pulled, &["init"])?;
    run_heddle(&pulled, &["remote", "add", "origin", &remote_url])?;
    pull_thread(&pulled, "main")?;
    assert_local_thread(&pulled, "main", main_v2)?;
    ensure!(
        fs::read_to_string(pulled.join("main.txt"))? == "main-v2\n",
        "pull did not materialize main-v2"
    );
    let mut client = connect_client(&remote_url, &remote_authority).await?;
    assert_thread_metadata(
        &mut client,
        &pulled,
        &remote_path,
        "main",
        main_v2,
        &first_main.id,
    )
    .await?;
    client.close().await;

    pull_thread(&pulled, NAMED_THREAD)?;
    assert_local_thread(&pulled, NAMED_THREAD, named_state)?;
    run_heddle(&pulled, &["thread", "switch", NAMED_THREAD])?;
    ensure!(
        fs::read_to_string(pulled.join("named.txt"))? == "named-v1\n",
        "pull did not materialize the named-thread state"
    );
    let mut client = connect_client(&remote_url, &remote_authority).await?;
    assert_thread_metadata(
        &mut client,
        &pulled,
        &remote_path,
        NAMED_THREAD,
        named_state,
        &named.id,
    )
    .await?;
    assert_advertised_pair(
        &mut client,
        &remote_path,
        (&first_main.id, main_v2),
        (&named.id, named_state),
    )
    .await?;
    client.close().await;

    // Clone each advertised thread into a genuinely fresh repository. Hosted
    // clone selects one checkout at a time, so the pair proves both tips and
    // their identities survive the fresh-clone path.
    let cloned_main = temp.path().join("cloned-main");
    clone_thread(temp.path(), &remote_url, &cloned_main, None)?;
    assert_local_thread(&cloned_main, "main", main_v2)?;
    ensure!(
        fs::read_to_string(cloned_main.join("main.txt"))? == "main-v2\n",
        "fresh clone did not materialize main-v2"
    );
    let mut client = connect_client(&remote_url, &remote_authority).await?;
    assert_thread_metadata(
        &mut client,
        &cloned_main,
        &remote_path,
        "main",
        main_v2,
        &first_main.id,
    )
    .await?;
    client.close().await;

    let cloned_named = temp.path().join("cloned-named");
    clone_thread(temp.path(), &remote_url, &cloned_named, Some(NAMED_THREAD))?;
    assert_local_thread(&cloned_named, NAMED_THREAD, named_state)?;
    ensure!(
        fs::read_to_string(cloned_named.join("named.txt"))? == "named-v1\n",
        "fresh named-thread clone did not materialize named-v1"
    );
    let mut client = connect_client(&remote_url, &remote_authority).await?;
    assert_thread_metadata(
        &mut client,
        &cloned_named,
        &remote_path,
        NAMED_THREAD,
        named_state,
        &named.id,
    )
    .await?;
    assert_advertised_pair(
        &mut client,
        &remote_path,
        (&first_main.id, main_v2),
        (&named.id, named_state),
    )
    .await?;

    client.close().await;
    Ok(())
}

fn authority_only_endpoint(endpoint: &str) -> Result<String> {
    let target = RemoteTarget::parse_native(endpoint)
        .map_err(|error| anyhow!("invalid {WEFT_URL_ENV} {endpoint:?}: {error}"))?;
    let RemoteTarget::Network {
        authority,
        repo_path,
    } = target
    else {
        return Err(anyhow!(
            "{WEFT_URL_ENV} must be a hosted authority, not a local path"
        ));
    };
    ensure!(
        repo_path.is_none(),
        "{WEFT_URL_ENV} must contain only the weft authority; the test auto-provisions a fresh child spool"
    );
    Ok(authority)
}

fn unique_repo_path(root: &Path) -> Result<PathBuf> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_nanos();
    Ok(root.join(format!(
        "heddle-client-flow-e2e-{}-{nonce}",
        std::process::id()
    )))
}

fn run_heddle(cwd: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .current_dir(cwd)
        .args(["--output", "json"])
        .args(args)
        .env("HEDDLE_PRINCIPAL_NAME", "Heddle live-weft e2e")
        .env("HEDDLE_PRINCIPAL_EMAIL", "live-weft-e2e@heddle.dev")
        .env("HEDDLE_FSMONITOR", "off")
        .env("NO_COLOR", "1")
        .env_remove("HEDDLE_AGENT_PROVIDER")
        .env_remove("HEDDLE_AGENT_MODEL")
        .output()
        .with_context(|| format!("run heddle {args:?} in {}", cwd.display()))?;
    ensure_command_succeeded(args, &output)?;
    Ok(output)
}

fn run_git(cwd: &Path, args: &[&str]) -> Result<Output> {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("run git {args:?} in {}", cwd.display()))?;
    ensure!(
        output.status.success(),
        "git {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn file_snapshot(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    fn visit(root: &Path, current: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
        for entry in fs::read_dir(current)
            .with_context(|| format!("read source tree directory {}", current.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("derive relative path for {}", path.display()))?;
            if relative
                .components()
                .next()
                .is_some_and(|part| part.as_os_str() == ".git" || part.as_os_str() == ".heddle")
            {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                visit(root, &path, files)?;
            } else if metadata.file_type().is_symlink() {
                files.insert(
                    relative.to_path_buf(),
                    fs::read_link(&path)?
                        .as_os_str()
                        .as_encoded_bytes()
                        .to_vec(),
                );
            } else if metadata.is_file() {
                files.insert(relative.to_path_buf(), fs::read(&path)?);
            }
        }
        Ok(())
    }

    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn ensure_command_succeeded(args: &[&str], output: &Output) -> Result<()> {
    ensure!(
        output.status.success(),
        "heddle {args:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

fn configured_origin(repo_path: &Path) -> Result<String> {
    let repo = Repository::open(repo_path).context("open source after first push")?;
    repo::remote::RemoteConfig::open(&repo)
        .context("open source remote config")?
        .get("origin")
        .context("read auto-provisioned origin")
        .map(|remote| remote.url)
}

fn hosted_remote_parts(remote_url: &str) -> Result<(String, String)> {
    let target = RemoteTarget::parse_native(remote_url)
        .map_err(|error| anyhow!("invalid auto-provisioned remote {remote_url:?}: {error}"))?;
    let RemoteTarget::Network {
        authority,
        repo_path,
    } = target
    else {
        return Err(anyhow!("auto-provisioned origin is not hosted"));
    };
    let repo_path = repo_path.context("auto-provisioned origin has no spool path")?;
    Ok((authority, repo_path))
}

async fn connect_client(remote_url: &str, authority: &str) -> Result<HostedClient> {
    let user_config = config::UserConfig::load_default().context("load hosted client config")?;
    let server_key = repo::remote::credential_key_from_remote_url(remote_url);
    let session =
        HostedSession::build(&user_config, server_key, HostedAuthMode::CredentialFallback)
            .context("build authenticated hosted session")?;
    session
        .connect(authority)
        .await
        .context("connect real hosted client to weft")
}

async fn advertised_thread(
    client: &mut HostedClient,
    remote_path: &str,
    name: &str,
) -> Result<AdvertisedThread> {
    let refs = client
        .list_refs_with_revision_addresses(remote_path)
        .await
        .with_context(|| format!("ListRefs {remote_path} while resolving {name}"))?;
    let advertised = refs
        .into_iter()
        .find(|entry| entry.is_user_thread() && entry.name == name)
        .with_context(|| format!("ListRefs did not advertise thread {name:?}"))?;
    let id = advertised
        .thread_id
        .with_context(|| format!("ListRefs omitted thread_id for {name:?}"))?;
    ensure!(
        !id.is_empty(),
        "ListRefs returned an empty thread_id for {name:?}"
    );
    Ok(AdvertisedThread {
        state: advertised.state_id,
        id,
    })
}

async fn assert_thread_metadata(
    client: &mut HostedClient,
    local_path: &Path,
    remote_path: &str,
    name: &str,
    expected_state: StateId,
    expected_id: &str,
) -> Result<()> {
    let repo = Repository::open(local_path)
        .with_context(|| format!("open local repository at {}", local_path.display()))?;
    let metadata = client
        .get_thread_metadata(&repo, remote_path, name, expected_state)
        .await
        .with_context(|| format!("resolve hosted metadata for {name:?}"))?;
    ensure!(
        metadata.id == expected_id,
        "hosted metadata for {name:?} used thread_id {:?}, expected {:?}",
        metadata.id,
        expected_id
    );
    Ok(())
}

async fn assert_advertised_pair(
    client: &mut HostedClient,
    remote_path: &str,
    main: (&str, StateId),
    named: (&str, StateId),
) -> Result<()> {
    let advertised_main = advertised_thread(client, remote_path, "main").await?;
    let advertised_named = advertised_thread(client, remote_path, NAMED_THREAD).await?;
    ensure!(
        advertised_main.id == main.0 && advertised_main.state == main.1,
        "main identity/state changed after a later client operation: {advertised_main:?}"
    );
    ensure!(
        advertised_named.id == named.0 && advertised_named.state == named.1,
        "named identity/state changed after a later client operation: {advertised_named:?}"
    );
    Ok(())
}

fn pull_thread(local_path: &Path, thread: &str) -> Result<()> {
    run_heddle(
        local_path,
        &[
            "pull",
            "origin",
            "--thread",
            thread,
            "--local-thread",
            thread,
        ],
    )?;
    Ok(())
}

fn clone_thread(
    cwd: &Path,
    remote_url: &str,
    destination: &Path,
    thread: Option<&str>,
) -> Result<()> {
    let destination = path_text(destination, "clone destination")?;
    let mut args = vec!["clone", remote_url, destination.as_str()];
    if let Some(thread) = thread {
        args.extend(["--thread", thread]);
    }
    run_heddle(cwd, &args)?;
    Ok(())
}

fn local_thread_state(repo_path: &Path, thread: &str) -> Result<StateId> {
    let repo = Repository::open(repo_path)
        .with_context(|| format!("open local repository at {}", repo_path.display()))?;
    repo.refs()
        .get_thread(&objects::object::ThreadName::new(thread))
        .with_context(|| format!("read local thread {thread:?}"))?
        .with_context(|| format!("local thread {thread:?} has no state"))
}

fn assert_local_thread(repo_path: &Path, thread: &str, expected: StateId) -> Result<()> {
    let repo = Repository::open(repo_path)
        .with_context(|| format!("open local repository at {}", repo_path.display()))?;
    let actual = repo
        .refs()
        .get_thread(&objects::object::ThreadName::new(thread))
        .with_context(|| format!("read materialized thread {thread:?}"))?
        .with_context(|| format!("materialized repository has no thread {thread:?}"))?;
    ensure!(
        actual == expected,
        "local thread {thread:?} points to {actual}, expected {expected}"
    );
    ensure!(
        repo.store().get_state(&expected)?.is_some(),
        "local thread {thread:?} points to missing state {expected}"
    );
    Ok(())
}

fn path_text(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .with_context(|| format!("{label} path is not valid UTF-8: {}", path.display()))
}
