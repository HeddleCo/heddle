//! Append-only local ownership adjudication. Claims remain immutable evidence;
//! the resolved owner is projected from the signed record on every read.
use std::collections::BTreeSet;

use crypto::thread_ownership_resolution::SignedOwnershipResolution;
use objects::object::{
    ContentHash,
    thread_replication::{
        SourceAuthor,
        ownership_resolution::{METHOD, ThreadOwnershipResolution},
    },
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::{Error, Result, ThreadReplica};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS thread_owner_resolutions(
    thread BLOB NOT NULL PRIMARY KEY,
    id BLOB NOT NULL,
    winner BLOB NOT NULL,
    canonical BLOB NOT NULL CHECK(length(canonical)<=262144),
    local_signature BLOB NOT NULL CHECK(length(local_signature)=64),
    acceptance_signature BLOB NOT NULL CHECK(length(acceptance_signature)=64),
    admission BLOB,
    admission_signature BLOB
);";

pub fn verify_resolution_account_authority(
    value: &ThreadOwnershipResolution,
    authority: &crate::device_authority::DeviceAuthority,
    spool_path: &str,
    now: i64,
) -> Result<()> {
    let SourceAuthor::Account {
        actor,
        authority: envelope,
        ..
    } = &value.acceptance
    else {
        return Err(Error::Invalid(
            "resolution requires account acceptance".into(),
        ));
    };
    let owner = crate::verify_account_owner_observation(&authority.owner, now)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    authority
        .verify_publisher(&value.accepting_publisher)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    heddleco_capability_verifier::thread_control_authority::verify_with_retained_mint_roots(
        envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: &owner,
            account_uuid: actor.principal_id.as_bytes(),
            publisher: &value.accepting_publisher,
            agent_id: actor.agent_id.as_deref(),
            method: METHOD,
            spool_path,
            now,
        },
        &authority.mint_roots,
        |kind| authority.is_revoked(kind),
    )
    .map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(())
}

impl ThreadReplica {
    /// Retained immutable result, including both original signatures.
    pub fn ownership_resolution(&self) -> Result<Option<SignedOwnershipResolution>> {
        let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = self.connect()?.query_row(
            "SELECT canonical,local_signature,acceptance_signature FROM thread_owner_resolutions WHERE thread=?1",
            [self.thread.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        Ok(
            row.map(|(canonical, local_signature, acceptance_signature)| {
                SignedOwnershipResolution {
                    canonical,
                    local_signature,
                    acceptance_signature,
                }
            }),
        )
    }

    /// First admission requires both original signatures, current recipient
    /// authority, the complete stored conflict, and the exact current frontier.
    /// Exact retained retries are idempotent even after recipient rotation.
    pub fn resolve_ownership(
        &self,
        signed: &SignedOwnershipResolution,
        authority: &crate::device_authority::DeviceAuthority,
        spool_path: &str,
        now: i64,
    ) -> Result<ContentHash> {
        let value = ThreadOwnershipResolution::decode(&signed.canonical)?;
        value.validate_genesis(&self.genesis()?)?;
        let id = value.id()?;
        let mut connection = self.connect()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> = tx.query_row(
            "SELECT canonical,local_signature,acceptance_signature FROM thread_owner_resolutions WHERE thread=?1",
            [self.thread.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if let Some((canonical, local_signature, acceptance_signature)) = existing {
            if canonical == signed.canonical
                && local_signature == signed.local_signature
                && acceptance_signature == signed.acceptance_signature
            {
                return Ok(id);
            }
            return Err(Error::Invalid(
                "Thread ownership already resolved differently".into(),
            ));
        }
        let mut query = tx.prepare(
            "SELECT id,canonical,local_signature,acceptance_signature FROM thread_owner_claims WHERE thread=?1 ORDER BY id LIMIT 129",
        )?;
        let claims = query
            .query_map([self.thread.as_bytes()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    crypto::thread_ownership_claim::SignedOwnershipClaim {
                        canonical: row.get(1)?,
                        local_signature: row.get(2)?,
                        acceptance_signature: row.get(3)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(query);
        let claim_ids = claims
            .iter()
            .map(|(id, _)| super::hash(id))
            .collect::<Result<BTreeSet<_>>>()?;
        if claim_ids != value.conflicting_claims {
            return Err(Error::Invalid(
                "resolution differs from complete stored claim set".into(),
            ));
        }
        let winner = claims
            .iter()
            .find(|(id, _)| id.as_slice() == value.winning_claim.as_bytes())
            .ok_or_else(|| Error::Invalid("winning ownership claim missing".into()))?;
        signed.verify(&winner.1.verify()?)?;
        verify_resolution_account_authority(&value, authority, spool_path, now)?;
        let mut query = tx.prepare("SELECT o.id FROM operations o WHERE o.thread=?1 AND o.facet=1 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child WHERE p.parent=o.id AND c.thread=o.thread AND c.status=1) ORDER BY o.id LIMIT 129")?;
        let frontier = query
            .query_map([self.thread.as_bytes()], |row| row.get::<_, Vec<u8>>(0))?
            .map(|row| super::hash(&row?))
            .collect::<Result<BTreeSet<_>>>()?;
        drop(query);
        if frontier != value.frontier {
            return Err(Error::Invalid("resolution source frontier changed".into()));
        }
        tx.execute(
            "INSERT INTO thread_owner_resolutions(thread,id,winner,canonical,local_signature,acceptance_signature) VALUES(?1,?2,?3,?4,?5,?6)",
            params![self.thread.as_bytes(), id.as_bytes(), value.winning_claim.as_bytes(), signed.canonical, signed.local_signature, signed.acceptance_signature],
        )?;
        tx.execute(
            "UPDATE threads SET generation=generation+1 WHERE id=?1",
            [self.thread.as_bytes()],
        )?;
        tx.commit()?;
        drop(connection);
        self.notify_committed()?;
        Ok(id)
    }
}
