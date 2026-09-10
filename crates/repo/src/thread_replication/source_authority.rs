//! Source signatures bind an explicit original author. Delivery credentials
//! never supply or replace the signed account, agent, or authority envelope.
use objects::object::thread_replication::{
    ThreadOperation,
    SourceAuthor, SOURCE_AUTHORIZATION_METHOD,
};
use super::{Error, Result, ThreadReplica};

/// Fresh first admission only. Previously independently admitted originals and
/// pinned hosted receipts use their durable admission path, not this clock.
/// Caller verifies the outer signature and separately enforces current audience.
impl ThreadReplica {
pub fn verify_source_authority(
    &self,
    operation: &ThreadOperation,
    authority: &crate::device_authority::DeviceAuthority,
    spool_path: &str,
    now: i64,
) -> Result<()> {
    let genesis = self.genesis()?;
    if operation.thread != genesis.id()? { return Err(Error::Invalid("source author Thread differs".into())); }
    let author = operation.source_author()?.ok_or_else(|| Error::Invalid("source author gate requires authored source".into()))?;
    author.validate()?;
    match &author {
        SourceAuthor::LocalKey => {
            self.verify_local_source_owner(operation)?;
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
            if !self.audience_allows(actor.principal_id, actor.agent_id.as_deref(), true, None)? {
                return Err(Error::Invalid("original source author is outside current Thread audience".into()));
            }
        }
    }
    Ok(())
}

}

/// Immutable local signatures authorize new work only while locally owned.
/// Following a signed claim, the shared indexed cutoff admits historical source
/// ancestors and rejects new former-owner writes.
impl ThreadReplica {
    pub fn verify_local_source_owner(&self, operation: &ThreadOperation) -> Result<()> {
        if !self.local_source_author_allowed(operation)? {
            return Err(Error::Invalid("local source author is outside the effective ownership cutoff".into()));
        }
        Ok(())
    }
}
