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
    pub fn ownership_resolution_admission(
        &self,
    ) -> Result<Option<crypto::thread_authority_admission::SignedAuthorityAdmission>> {
        let row: Option<(Option<Vec<u8>>, Option<Vec<u8>>)> = self.connect()?.query_row(
            "SELECT admission,admission_signature FROM thread_owner_resolutions WHERE thread=?1",
            [self.thread.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        match row {
            None | Some((None, None)) => Ok(None),
            Some((Some(canonical), Some(signature))) => Ok(Some(
                crypto::thread_authority_admission::SignedAuthorityAdmission {
                    boundary_acceptance: super::boundary_evidence::load(
                        &self.connect()?, &canonical,
                        &objects::object::thread_authority_admission::ThreadAuthorityAdmission::decode(&canonical)?.basis,
                    )?,
                    canonical,
                    signature,
                }
            )),
            _ => Err(Error::Invalid("incomplete ownership resolution admission".into())),
        }
    }
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
        self.resolve_ownership_inner(signed, Some((authority, spool_path, now)), None)
    }
    pub fn resolve_ownership_with_admission(
        &self,
        signed: &SignedOwnershipResolution,
        admission: &crypto::thread_authority_admission::SignedAuthorityAdmission,
    ) -> Result<ContentHash> {
        self.resolve_ownership_inner(signed, None, Some(admission))
    }
    fn resolve_ownership_inner(
        &self,
        signed: &SignedOwnershipResolution,
        authority: Option<(&crate::device_authority::DeviceAuthority, &str, i64)>,
        admission: Option<&crypto::thread_authority_admission::SignedAuthorityAdmission>,
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
        if let Some(receipt) = admission {
            receipt.verify_resolution(
                signed,
                &winner.1.verify()?,
                &self.genesis()?,
                &self.authority_admission_trust(receipt)?,
            )?;
        } else if let Some((authority, spool_path, now)) = authority {
            verify_resolution_account_authority(&value, authority, spool_path, now)?;
        } else {
            return Err(Error::Invalid(
                "resolution requires fresh account authority or portable admission".into(),
            ));
        }
        let mut query = tx.prepare("SELECT o.id FROM operations o WHERE o.thread=?1 AND o.facet=1 AND o.status=1 AND NOT EXISTS(SELECT 1 FROM parents p JOIN operations c ON c.id=p.child WHERE p.parent=o.id AND c.thread=o.thread AND c.status=1) ORDER BY o.id LIMIT 129")?;
        let frontier = query
            .query_map([self.thread.as_bytes()], |row| row.get::<_, Vec<u8>>(0))?
            .map(|row| super::hash(&row?))
            .collect::<Result<BTreeSet<_>>>()?;
        drop(query);
        if authority.is_some() && frontier != value.frontier {
            return Err(Error::Invalid("resolution source frontier changed".into()));
        }
        if admission.is_some() {
            for head in &value.frontier {
                let accepted: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM operations WHERE thread=?1 AND id=?2 AND facet=1 AND status=1)",
                    params![self.thread.as_bytes(), head.as_bytes()],
                    |row| row.get(0),
                )?;
                if !accepted {
                    return Err(Error::Invalid(
                        "resolution source frontier original is missing".into(),
                    ));
                }
            }
        }
        tx.execute(
            "INSERT INTO thread_owner_resolutions(thread,id,winner,canonical,local_signature,acceptance_signature,admission,admission_signature) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![self.thread.as_bytes(), id.as_bytes(), value.winning_claim.as_bytes(), signed.canonical, signed.local_signature, signed.acceptance_signature, admission.map(|v| v.canonical.as_slice()), admission.map(|v| v.signature.as_slice())],
        )?;
        if let Some(receipt) = admission {
            super::boundary_evidence::persist(
                &tx,
                &receipt.canonical,
                receipt.boundary_acceptance.as_deref(),
            )?;
        }
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
