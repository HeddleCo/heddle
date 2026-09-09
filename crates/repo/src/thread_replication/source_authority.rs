//! Source signatures bind an explicit original author. Delivery credentials
//! never supply or replace the signed account, agent, or authority envelope.
use objects::object::thread_replication::{
    ThreadOperation, ThreadOperationBody, ThreadGenesis, GenesisOwner,
    SourceAuthor, SOURCE_AUTHORIZATION_METHOD,
};
use super::{Error, Result};

/// Fresh first admission only. Previously independently admitted originals and
/// pinned hosted receipts use their durable admission path, not this clock.
/// Caller verifies the outer signature and separately enforces current audience.
pub fn verify_source_authority(
    operation: &ThreadOperation,
    genesis: &ThreadGenesis,
    authority: &crate::device_authority::DeviceAuthority,
    spool_path: &str,
    now: i64,
) -> Result<()> {
    if operation.thread != genesis.id()? { return Err(Error::Invalid("source author Thread differs".into())); }
    let ThreadOperationBody::Capture(capture) = &operation.body else {
        return Err(Error::Invalid("source author gate requires authored capture".into()));
    };
    capture.author.validate()?;
    match &capture.author {
        SourceAuthor::LocalKey => {
            verify_local_source_owner(operation, genesis)?;
        }
        SourceAuthor::Account { spool, actor, authority: envelope, .. } => {
            if spool.to_string() != genesis.spool { return Err(Error::Invalid("source author Spool differs".into())); }
            let owner = crate::verify_account_owner_observation(&authority.owner, now)
                .map_err(|error| Error::Invalid(error.to_string()))?;
            heddleco_capability_verifier::thread_control_authority::verify_with_retained_mint_roots(
                envelope,
                heddleco_capability_verifier::thread_control_authority::Context {
                    owner: &owner, account_uuid: actor.principal_id.as_bytes(), publisher: &operation.publisher,
                    agent_id: actor.agent_id.as_deref(), method: SOURCE_AUTHORIZATION_METHOD, spool_path, now,
                }, &authority.mint_roots, |kind| authority.is_revoked(kind),
            ).map_err(|error| Error::Invalid(error.to_string()))?;
        }
    }
    Ok(())
}

/// Unclaimed local source ownership is proven by the original owner signature,
/// never by fabricating an account authority for an unregistered device.
pub fn verify_local_source_owner(operation: &ThreadOperation, genesis: &ThreadGenesis) -> Result<()> {
    if operation.thread != genesis.id()? || !matches!(&operation.body,
        ThreadOperationBody::Capture(capture) if matches!(capture.author, SourceAuthor::LocalKey))
        || !matches!(&genesis.owner, GenesisOwner::LocalKey(key) if *key == operation.publisher)
    {
        return Err(Error::Invalid("local source author is not this Thread's original local owner".into()));
    }
    Ok(())
}
