//! Effective ownership is an explicit, independently admitted claim. Genesis and
//! Thread identity never change. Conflicting signed claims fail closed.
use crypto::thread_ownership_claim::SignedOwnershipClaim;
use objects::object::{ContentHash, thread_replication::{GenesisOwner, SourceAuthor, ThreadGenesis, ThreadOperation, ownership_claim::{METHOD, ThreadOwnershipClaim}}};
use rusqlite::{TransactionBehavior, params};
use super::{Error, Result, ThreadReplica};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS thread_owner_claims(thread BLOB NOT NULL, id BLOB NOT NULL, canonical BLOB NOT NULL CHECK(length(canonical)<=262144), local_signature BLOB NOT NULL CHECK(length(local_signature)=64), acceptance_signature BLOB NOT NULL CHECK(length(acceptance_signature)=64), account TEXT NOT NULL, admission BLOB, admission_signature BLOB, PRIMARY KEY(thread,id));
CREATE TABLE IF NOT EXISTS thread_owner_claim_frontier(thread BLOB NOT NULL, claim BLOB NOT NULL, operation BLOB NOT NULL, PRIMARY KEY(thread,claim,operation));
CREATE TABLE IF NOT EXISTS thread_owner_claim_history(thread BLOB NOT NULL, claim BLOB NOT NULL, operation BLOB NOT NULL, PRIMARY KEY(thread,operation));";

/// Shared local/hosted admission: signatures bind exact prior key, target account,
/// frontier and acceptance; pinned current account authority verifies permissions.
pub fn verify_claim_authority(signed: &SignedOwnershipClaim, genesis: &ThreadGenesis,
    authority: &crate::device_authority::DeviceAuthority, spool_path: &str, now: i64,
) -> Result<ThreadOwnershipClaim> {
    let claim = signed.verify()?;
    verify_claim_account_authority(&claim, genesis, authority, spool_path, now)?;
    Ok(claim)
}
/// The statement must already have its account acceptance signature verified.
pub fn verify_claim_account_authority(claim: &ThreadOwnershipClaim, genesis: &ThreadGenesis,
    authority: &crate::device_authority::DeviceAuthority, spool_path: &str, now: i64,
) -> Result<()> {
    claim.validate_genesis(genesis)?;
    let SourceAuthor::Account { actor, authority: envelope, .. } = &claim.acceptance else {
        return Err(Error::Invalid("claim requires account acceptance".into()));
    };
    let owner = crate::verify_account_owner_observation(&authority.owner, now)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    authority.verify_publisher(&claim.prior_local_key).map_err(|error| Error::Invalid(error.to_string()))?;
    authority.verify_publisher(&claim.accepting_publisher).map_err(|error| Error::Invalid(error.to_string()))?;
    heddleco_capability_verifier::thread_control_authority::verify_with_retained_mint_roots(
        envelope, heddleco_capability_verifier::thread_control_authority::Context {
            owner: &owner, account_uuid: actor.principal_id.as_bytes(), publisher: &claim.accepting_publisher,
            agent_id: actor.agent_id.as_deref(), method: METHOD, spool_path, now,
        }, &authority.mint_roots, |kind| authority.is_revoked(kind),
    ).map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(())
}
impl ThreadReplica {
    pub fn source_author_for(&self, publisher: &[u8; 32]) -> Result<SourceAuthor> {
        self.source_author_for_at(publisher, &crate::identity::heddle_home_dir())
    }
    pub(crate) fn source_author_for_at(&self, publisher: &[u8;32], home:&std::path::Path) -> Result<SourceAuthor> {
        match self.effective_owner()? {
            GenesisOwner::LocalKey(owner) if &owner == publisher => Ok(SourceAuthor::LocalKey),
            GenesisOwner::LocalKey(_) => Err(Error::Invalid("unclaimed Thread requires original local source signer".into())),
            GenesisOwner::Account(_) => {
                let spool = self.genesis()?.spool.parse().map_err(|error: uuid::Error| Error::Invalid(error.to_string()))?;
                let author = crate::identity::source_author::load(home,publisher,spool).map_err(|error|Error::Invalid(error.to_string()))?;
                if matches!(author, SourceAuthor::LocalKey) {
                    return Err(Error::Invalid("account Thread requires original device account proof".into()));
                }
                Ok(author)
            }
        }
    }
    pub fn ownership_claims_with_admission(&self) -> Result<Vec<(SignedOwnershipClaim,Option<crypto::thread_authority_admission::SignedAuthorityAdmission>)>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare("SELECT canonical,local_signature,acceptance_signature,admission,admission_signature FROM thread_owner_claims WHERE thread=?1 ORDER BY id LIMIT 2")?;
        let rows = statement.query_map([self.thread.as_bytes()], |row| Ok((SignedOwnershipClaim {
            canonical: row.get(0)?, local_signature: row.get(1)?, acceptance_signature: row.get(2)?,
        },row.get::<_,Option<Vec<u8>>>(3)?,row.get::<_,Option<Vec<u8>>>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(|(claim,canonical,signature)| {
            let admission = match (canonical,signature) {
                (None,None) => None,
                (Some(canonical),Some(signature)) => Some(crypto::thread_authority_admission::SignedAuthorityAdmission {canonical,signature}),
                _ => return Err(Error::Invalid("incomplete ownership claim admission".into())),
            };
            Ok((claim,admission))
        }).collect()
    }
    pub fn ownership_claims(&self) -> Result<Vec<SignedOwnershipClaim>> {
        Ok(self.ownership_claims_with_admission()?.into_iter().map(|(claim,_)|claim).collect())
    }
    pub fn effective_owner(&self) -> Result<GenesisOwner> {
        let claims = self.ownership_claims()?;
        match claims.as_slice() {
            [] => Ok(self.genesis()?.owner),
            [signed] => Ok(GenesisOwner::Account(signed.verify()?.account()?)),
            _ => Err(Error::Invalid("conflicting Thread ownership claims require explicit resolution".into())),
        }
    }
    /// Historical local records must belong to the signed cutoff ancestry;
    /// untrusted timestamps and an enrolled former key are never authority.
    pub fn local_source_author_allowed(&self, operation: &ThreadOperation) -> Result<bool> {
        if operation.thread != self.thread || !matches!(operation.source_author()?, Some(SourceAuthor::LocalKey)) { return Ok(false); }
        let genesis = self.genesis()?;
        if genesis.owner != GenesisOwner::LocalKey(operation.publisher) { return Ok(false); }
        let claims = self.ownership_claims()?;
        if claims.is_empty() { return Ok(true); }
        if claims.len() != 1 { return Ok(false); }
        let connection = self.connect()?;
        let id = operation.id()?;
        let found: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM thread_owner_claim_history WHERE thread=?1 AND operation=?2)", params![self.thread.as_bytes(), id.as_bytes()], |row| row.get(0))?;
        Ok(found)
    }
    pub fn ownership_claim_admission(&self, id: &ContentHash) -> Result<Option<crypto::thread_authority_admission::SignedAuthorityAdmission>> {
        use rusqlite::OptionalExtension;
        let row: Option<(Option<Vec<u8>>,Option<Vec<u8>>)> = self.connect()?.query_row(
            "SELECT admission,admission_signature FROM thread_owner_claims WHERE thread=?1 AND id=?2",
            params![self.thread.as_bytes(),id.as_bytes()], |row| Ok((row.get(0)?,row.get(1)?)),
        ).optional()?;
        match row {
            Some((Some(canonical),Some(signature))) => Ok(Some(crypto::thread_authority_admission::SignedAuthorityAdmission { canonical,signature })),
            None | Some((None,None)) => Ok(None),
            _ => Err(Error::Invalid("incomplete ownership claim admission".into())),
        }
    }
    pub fn claim_ownership(&self, signed: &SignedOwnershipClaim,
        authority: &crate::device_authority::DeviceAuthority, spool_path: &str, now: i64,
    ) -> Result<ContentHash> {
        self.claim_inner(signed, Some((authority,spool_path,now)), None, None, true).map(|(id,_)|id)
    }
    pub fn claim_ownership_with_command(&self, signed: &SignedOwnershipClaim,
        authority: &crate::device_authority::DeviceAuthority, spool_path: &str, now: i64,
        command: &crate::device_operations::Command<'_>, response: &[u8],
    ) -> Result<Vec<u8>> {
        self.claim_inner(signed, Some((authority,spool_path,now)), None, Some((command,response)), true)?
            .1.ok_or_else(||Error::Invalid("ownership command receipt missing".into()))
    }
    /// Portable historical acceptance requires the same independently pinned
    /// executor proof as original operations, never a refreshed creator bearer.
    pub fn claim_ownership_with_admission(&self, signed: &SignedOwnershipClaim,
        admission: &crypto::thread_authority_admission::SignedAuthorityAdmission,
    ) -> Result<ContentHash> {
        self.claim_inner(signed, None, Some(admission), None, false).map(|(id,_)|id)
    }
    fn claim_inner(&self, signed: &SignedOwnershipClaim,
        authority: Option<(&crate::device_authority::DeviceAuthority,&str,i64)>,
        admission: Option<&crypto::thread_authority_admission::SignedAuthorityAdmission>,
        command: Option<(&crate::device_operations::Command<'_>,&[u8])>, exact_frontier: bool,
    ) -> Result<(ContentHash,Option<Vec<u8>>)> {
        let claim = signed.verify()?;
        let genesis = self.genesis()?;
        claim.validate_genesis(&genesis)?;
        let id = claim.id()?;
        let existing = self.ownership_claims()?;
        let retained = existing.iter().any(|stored| stored == signed);
        if !retained {
            if let Some(receipt) = admission {
                receipt.verify_claim(signed, &genesis, &self.authority_admission_trust(receipt)?)?;
            } else if let Some((authority,path,now)) = authority {
                verify_claim_authority(signed, &genesis, authority, path, now)?;
            } else { return Err(Error::Invalid("claim requires original account authority or retained admission".into())); }
        }
        if command.is_some_and(|(command,response)| command.namespace.is_empty() || command.namespace.len()>1024 || response.len()>1024*1024) {
            return Err(Error::Invalid("ownership command receipt exceeds bounds".into()));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((command,_)) = command {
            if let Some(prior) = crate::device_operations::replay(&transaction,command).map_err(|error|Error::Invalid(error.to_string()))? {
                return Ok((id,Some(prior)));
            }
        }
        let present:bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM thread_owner_claims WHERE thread=?1 AND id=?2)",params![self.thread.as_bytes(),id.as_bytes()],|row|row.get(0))?;
        if present {
            if let Some((command,response)) = command { crate::device_operations::receipt(&transaction,command,response).map_err(|error|Error::Invalid(error.to_string()))?; }
            transaction.commit()?;
            return Ok((id,command.map(|(_,response)|response.to_vec())));
        }
        let count:i64 = transaction.query_row("SELECT count(*) FROM thread_owner_claims WHERE thread=?1", [self.thread.as_bytes()], |row|row.get(0))?;
        if count >= 2 { return Err(Error::Invalid("ownership claim conflict is unresolved".into())); }
        if exact_frontier {
            let mut query = transaction.prepare("SELECT o.id FROM operations o WHERE o.thread=?1 AND o.facet=1 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child WHERE p.parent=o.id AND c.thread=o.thread AND c.status=1) ORDER BY o.id LIMIT 129")?;
            let frontier = query.query_map([self.thread.as_bytes()], |row| row.get::<_,Vec<u8>>(0))?
                .map(|row| super::hash(&row?)).collect::<Result<std::collections::BTreeSet<_>>>()?;
            if frontier != claim.source_frontier { return Err(Error::Invalid("ownership claim source frontier changed".into())); }
        } else {
            for head in &claim.source_frontier {
                let accepted: bool = transaction.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND id=?2 AND facet=1 AND status=1)", params![self.thread.as_bytes(),head.as_bytes()],|row|row.get(0))?;
                if !accepted { return Err(Error::Invalid("claim cutoff original source proof is missing".into())); }
            }
        }
        transaction.execute("INSERT INTO thread_owner_claims(thread,id,canonical,local_signature,acceptance_signature,account,admission,admission_signature) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)", params![self.thread.as_bytes(), id.as_bytes(), signed.canonical, signed.local_signature, signed.acceptance_signature, claim.account()?.to_string(), admission.map(|value|value.canonical.as_slice()), admission.map(|value|value.signature.as_slice())])?;
        for head in &claim.source_frontier {
            transaction.execute("INSERT INTO thread_owner_claim_frontier(thread,claim,operation) VALUES(?1,?2,?3)", params![self.thread.as_bytes(),id.as_bytes(),head.as_bytes()])?;
        }
        transaction.execute("WITH RECURSIVE history(id) AS (SELECT operation FROM thread_owner_claim_frontier WHERE thread=?1 AND claim=?2 UNION SELECT p.parent FROM parents p JOIN history h ON p.child=h.id JOIN operations o ON o.id=p.parent AND o.thread=?1 AND o.status=1) INSERT OR IGNORE INTO thread_owner_claim_history(thread,claim,operation) SELECT ?1,?2,id FROM history", params![self.thread.as_bytes(),id.as_bytes()])?;
        transaction.execute("UPDATE threads SET generation=generation+1 WHERE id=?1", [self.thread.as_bytes()])?;
        if count == 0 {
            if let Some((command,response)) = command { crate::device_operations::receipt(&transaction,command,response).map_err(|error|Error::Invalid(error.to_string()))?; }
        }
        transaction.commit()?;
        drop(connection);
        self.notify_committed()?;
        if count != 0 { return Err(Error::Invalid("conflicting Thread ownership claims require explicit resolution".into())); }
        Ok((id,command.map(|(_,response)|response.to_vec())))
    }
}
