//! Explicit local-key ownership transition. The CLI prepares the exact signed
//! records from retained local keys; users choose a claim, not cryptographic bytes.
use anyhow::{Context, Result, bail, ensure};
use crypto::{Ed25519Signer, Signer};
use heddle_cli_contract::cli::commands::wire::ThreadOwnershipOutput;
use objects::object::{
    ContentHash,
    thread_replication::{
        GenesisOwner, SourceAuthor, ThreadFacet, ownership_claim::ThreadOwnershipClaim,
        ownership_resolution::ThreadOwnershipResolution,
    },
};
use repo::{Repository, thread_replication::ThreadReplica};

use crate::cli::{Cli, ThreadOwnershipCommands, should_output_json};

pub(super) fn cmd_thread_ownership(
    cli: &Cli,
    repo: &Repository,
    command: ThreadOwnershipCommands,
) -> Result<()> {
    match command {
        ThreadOwnershipCommands::Status { thread } => {
            let (name, replica) = selected(repo, thread)?;
            let claims = replica.ownership_claims()?;
            let resolution = replica.ownership_resolution()?;
            let status = if resolution.is_some() {
                "resolved"
            } else if claims.len() > 1 {
                "conflict"
            } else if claims.len() == 1 {
                "account"
            } else {
                "local"
            };
            let owner = match replica.effective_owner() {
                Ok(GenesisOwner::Account(account)) => Some(account.to_string()),
                Ok(GenesisOwner::LocalKey(key)) => Some(format!("local key {}", hex::encode(key))),
                Err(error)
                    if status == "conflict"
                        && error
                            .to_string()
                            .contains("conflicting Thread ownership claims") =>
                {
                    None
                }
                Err(error) => return Err(error.into()),
            };
            let claim_ids = claims
                .iter()
                .map(|claim| {
                    claim
                        .verify()
                        .and_then(|claim| claim.id().map_err(Into::into))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let winning_claim = resolution
                .as_ref()
                .map(|signed| ThreadOwnershipResolution::decode(&signed.canonical))
                .transpose()?
                .map(|value| value.winning_claim.to_hex());
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&ThreadOwnershipOutput {
                        output_kind: "thread_ownership",
                        thread: name.clone(),
                        status,
                        owner: owner.clone(),
                        claim_ids: claim_ids.iter().map(ContentHash::to_hex).collect(),
                        winning_claim: winning_claim.clone(),
                        resolution_id: None,
                    })?
                );
            } else {
                println!("Thread {name}: {status}");
                if let Some(owner) = owner {
                    println!("Owner: {owner}");
                }
                for claim in claim_ids {
                    println!("Claim: {}", claim.to_hex());
                }
                if let Some(winner) = winning_claim {
                    println!("Selected claim: {winner}");
                }
                if status == "conflict" {
                    println!("Resolve: heddle thread ownership resolve {name} --claim <claim-id>");
                }
            }
            Ok(())
        }
        ThreadOwnershipCommands::Claim { thread } => {
            let (name, replica) = selected(repo, thread)?;
            let GenesisOwner::LocalKey(original) = replica.effective_owner()? else {
                bail!("Thread {name} already has an account owner");
            };
            let local = repo
                .native_original_owner_signer(&replica)
                .context("claim requires the original local owner key on this device")?;
            ensure!(local.public_key() == original, "original owner key changed");
            let (account, authority, author) = current_account_author(&replica)?;
            let claim = ThreadOwnershipClaim {
                version: 1,
                thread: replica.thread_id(),
                prior_local_key: original,
                accepting_publisher: account.public_key().try_into().context("account key")?,
                acceptance: author,
                source_frontier: frontier(&replica)?,
            };
            let signed = crypto::thread_ownership_claim::SignedOwnershipClaim::sign(
                &claim, &local, &account,
            )?;
            let id = replica.claim_ownership(
                &signed,
                &authority,
                &spool_path(repo)?,
                chrono::Utc::now().timestamp(),
            )?;
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&ThreadOwnershipOutput {
                        output_kind: "thread_ownership",
                        thread: name.clone(),
                        status: "claimed",
                        owner: Some(claim.account()?.to_string()),
                        claim_ids: vec![id.to_hex()],
                        winning_claim: Some(id.to_hex()),
                        resolution_id: None,
                    })?
                );
            } else {
                println!("Thread {name} claimed by current account ({id}).");
            }
            Ok(())
        }
        ThreadOwnershipCommands::Resolve { thread, claim } => {
            let (name, replica) = selected(repo, thread)?;
            let local = repo
                .native_original_owner_signer(&replica)
                .context("resolution requires the original local owner key on this device")?;
            let winner = ContentHash::from_hex(&claim)
                .context("--claim needs a full claim ID from ownership status")?;
            let claims = replica.ownership_claims()?;
            ensure!(
                claims.len() >= 2,
                "Thread {name} has no unresolved ownership conflict"
            );
            let claim_ids = claims
                .iter()
                .map(|signed| {
                    signed
                        .verify()
                        .and_then(|value| value.id().map_err(Into::into))
                })
                .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
            ensure!(
                claim_ids.contains(&winner),
                "selected claim is not in this Thread's conflict"
            );
            let (account, authority, author) = current_account_author(&replica)?;
            let selected = claims
                .iter()
                .find(|signed| {
                    signed
                        .verify()
                        .and_then(|value| value.id().map_err(Into::into))
                        .ok()
                        == Some(winner)
                })
                .context("selected original claim missing")?
                .verify()?;
            let SourceAuthor::Account { actor, .. } = &author else {
                bail!("current account acceptance is unavailable");
            };
            ensure!(
                selected.account()? == actor.principal_id,
                "the current account must accept the selected claim"
            );
            let genesis = replica.genesis()?;
            let value = ThreadOwnershipResolution {
                version: 1,
                spool: genesis.spool.parse()?,
                thread: replica.thread_id(),
                winning_claim: winner,
                conflicting_claims: claim_ids,
                frontier: frontier(&replica)?,
                local_owner: local.public_key().try_into().context("local key")?,
                accepting_publisher: account.public_key().try_into().context("account key")?,
                acceptance: author,
                occurred_at_ms: chrono::Utc::now().timestamp_millis(),
            };
            let signed = crypto::thread_ownership_resolution::SignedOwnershipResolution::sign(
                &value, &local, &account,
            )?;
            let id = replica.resolve_ownership(
                &signed,
                &authority,
                &spool_path(repo)?,
                chrono::Utc::now().timestamp(),
            )?;
            if should_output_json(cli, Some(repo.config())) {
                println!(
                    "{}",
                    serde_json::to_string(&ThreadOwnershipOutput {
                        output_kind: "thread_ownership",
                        thread: name.clone(),
                        status: "resolved",
                        owner: Some(value.account()?.to_string()),
                        claim_ids: value
                            .conflicting_claims
                            .iter()
                            .map(ContentHash::to_hex)
                            .collect(),
                        winning_claim: Some(winner.to_hex()),
                        resolution_id: Some(id.to_hex()),
                    })?
                );
            } else {
                println!("Thread {name} ownership resolved to claim {winner}.");
            }
            Ok(())
        }
    }
}

fn selected(repo: &Repository, name: Option<String>) -> Result<(String, ThreadReplica)> {
    let name = match name {
        Some(name) => name,
        None => match repo.head_ref()? {
            refs::Head::Attached { thread } => thread.to_string(),
            refs::Head::Detached { .. } => bail!("choose a Thread name from `heddle thread list`"),
        },
    };
    let replica = repo
        .native_thread(&name)
        .with_context(|| format!("Thread {name} has no native identity"))?;
    Ok((name, replica))
}
fn frontier(replica: &ThreadReplica) -> Result<std::collections::BTreeSet<ContentHash>> {
    let page = replica.frontier_page(ThreadFacet::Source, None, 129)?;
    ensure!(
        page.len() <= 128,
        "source frontier exceeds the signed bound"
    );
    Ok(page.into_iter().collect())
}
fn current_account_author(
    replica: &ThreadReplica,
) -> Result<(
    Ed25519Signer,
    repo::device_authority::DeviceAuthority,
    SourceAuthor,
)> {
    let home = repo::identity::heddle_home_dir();
    let device = repo::identity::load_device(&repo::identity::device_identity_path())?
        .context("current account device key is unavailable; pair or enroll this device")?;
    let signer = Ed25519Signer::from_pem(&device.private_key_pem)?;
    let now = chrono::Utc::now().timestamp();
    let authority = repo::device_authority::load(&home, now)
        .context("current account authority is unavailable; connect and refresh this device")?;
    let spool = replica.genesis()?.spool.parse()?;
    let publisher = signer.public_key().try_into().context("device key")?;
    let author = repo::identity::source_author::load(&home, &publisher, spool)?;
    ensure!(
        matches!(author, SourceAuthor::Account { .. }),
        "current account acceptance proof is unavailable on this device"
    );
    Ok((signer, authority, author))
}
fn spool_path(repo: &Repository) -> Result<String> {
    let catalog = repo::device_catalog::store::Catalog::read(&repo::identity::heddle_home_dir())?
        .context("current Spool catalog is unavailable")?;
    let spool = catalog
        .spool(repo.native_spool_id()?)?
        .context("current Spool authorization path is unavailable")?;
    Ok(spool.registration.capability_path)
}
