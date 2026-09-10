//! Indexed causal Thread property heads. Admission updates one field and its
//! observed parents in the same transaction as the immutable signed operation.
use std::collections::BTreeSet;

use crypto::thread_operation::SignedOperation;
use objects::{
    object::{
        ContentHash,
        thread_replication::{
            ThreadOperation, ThreadOperationBody,
            metadata::{Property, ThreadControl},
        },
    },
    store::ObjectStore,
};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Admission, Error, Result, ThreadReplica, hash};

/// Seal portable original authority using the already enrolled local account.
/// The proof contains public signed history and a sealed Biscuit, never the
/// appendable credential proof secret. Preparation performs no hosted calls.
pub fn prepare_control_authority(
    authority: &crate::device_authority::DeviceAuthority,
    mint_root: &[u8; 32],
    token: &biscuit_auth::Biscuit,
    now: i64,
) -> Result<Vec<u8>> {
    authority
        .verify_mint_root(mint_root, now)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let owner = crate::verify_account_owner_observation(&authority.owner, now)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let root = owner
        .signed_root()
        .root
        .as_ref()
        .ok_or_else(|| Error::Invalid("owner root missing".into()))?;
    let attachment = if owner.authority_key().public_key == mint_root.as_slice() {
        None
    } else {
        Some(
            authority
                .mint_roots
                .iter()
                .find(|attachment| {
                    heddleco_capability_verifier::creation::verify_retained_mint_root_attachment(
                        attachment,
                        &owner,
                        &root.account_uuid,
                        mint_root,
                        now,
                    )
                    .is_ok()
                })
                .ok_or_else(|| {
                    Error::Invalid("independently retained mint root attachment missing".into())
                })?,
        )
    };
    heddleco_capability_verifier::thread_control_authority::encode(
        &api::heddle::api::v2alpha1::OwnerHistory {
            root: authority.owner.root.clone(),
            accepted_transitions: authority.owner.accepted_transitions.clone(),
            state_hash: authority.owner.version.clone(),
        },
        mint_root,
        attachment,
        token,
    )
    .map_err(|error| Error::Invalid(error.to_string()))
}

/// Independently authorize the original signed metadata actor at first local
/// admission. Delivery credentials and author-supplied timestamps cannot replace
/// this proof. The caller verifies the enclosing signature before using it.
pub fn verify_control_authority(
    operation: &ThreadOperation,
    authority: &crate::device_authority::DeviceAuthority,
    spool_path: &str,
    now: i64,
) -> Result<()> {
    let ThreadOperationBody::Metadata(bytes) = &operation.body else {
        return Err(Error::Invalid(
            "original control authority requires metadata".into(),
        ));
    };
    let control = ThreadControl::decode(bytes)?;
    let owner = crate::verify_account_owner_observation(&authority.owner, now)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    heddleco_capability_verifier::thread_control_authority::verify_with_retained_mint_roots(
        &control.authority_envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: &owner,
            account_uuid: control.actor.principal_id.as_bytes(),
            publisher: &operation.publisher,
            agent_id: control.actor.agent_id.as_deref(),
            method: control.authorization_method(),
            spool_path,
            now,
        },
        &authority.mint_roots,
        |kind| authority.is_revoked(kind),
    )
    .map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(())
}

/// Verify first admission of account-owned genesis against the independently
/// retained account. Local-key ownership uses its explicit local/device trust
/// path and is never silently attached to this account by this function.
///
/// Genesis has no agent field. Attribution is derived from the verified
/// original credential (`agent_id: None` here); source ops and claims keep
/// strict signed-actor checks against that same envelope.
pub fn verify_genesis_authority(
    genesis: &objects::object::thread_replication::ThreadGenesis,
    envelope: &[u8],
    authority: &crate::device_authority::DeviceAuthority,
    spool_path: &str,
    method: &str,
    now: i64,
) -> Result<heddleco_capability_verifier::thread_control_authority::VerifiedAuthor> {
    let objects::object::thread_replication::GenesisOwner::Account(account) = &genesis.owner else {
        return Err(Error::Invalid(
            "local-key ownership requires explicit claim before account admission".into(),
        ));
    };
    let owner = crate::verify_account_owner_observation(&authority.owner, now)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    heddleco_capability_verifier::thread_control_authority::verify_genesis_with_retained_mint_roots(
        envelope,
        heddleco_capability_verifier::thread_control_authority::Context {
            owner: &owner,
            account_uuid: account.as_bytes(),
            publisher: &genesis.creator,
            agent_id: None,
            method,
            spool_path,
            now,
        },
        &authority.mint_roots,
        |kind| authority.is_revoked(kind),
    )
    .map_err(|error| Error::Invalid(error.to_string()))
}

impl ThreadReplica {
    /// Audience is an additional ceiling after current capability checks.
    /// `owned_local_key` is supplied only after verifying device-held key
    /// possession; neither a remote parameter nor a Spool role proves it.
    pub fn audience_allows(
        &self,
        principal: uuid::Uuid,
        agent_id: Option<&str>,
        in_spool_audience: bool,
        owned_local_key: Option<&[u8; 32]>,
    ) -> Result<bool> {
        use objects::object::thread_replication::{GenesisOwner, metadata::Control};
        let owner = self.effective_owner()?;
        let is_owner = match owner {
            GenesisOwner::Account(owner) => owner == principal && !principal.is_nil(),
            GenesisOwner::LocalKey(key) => owned_local_key == Some(&key),
        };
        if is_owner {
            return Ok(true);
        }
        let candidates = self.metadata_frontier(&Property::Audience)?;
        if candidates.is_empty() {
            return Ok(false);
        }
        for (_, signed) in candidates {
            let operation = signed.verify()?;
            let ThreadOperationBody::Metadata(bytes) = operation.body else {
                return Err(Error::Invalid(
                    "audience projection names another facet".into(),
                ));
            };
            let Control::Audience(policy) = ThreadControl::decode(&bytes)?.control else {
                return Err(Error::Invalid(
                    "audience projection names another property".into(),
                ));
            };
            if !policy.includes_non_owner(principal, agent_id, in_spool_audience) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Exact durable original-author admission, separate from causal readiness.
    /// Only host-authorized receive writes this bit atomically with signed bytes.
    pub fn original_authority_admitted(&self, signed: &SignedOperation) -> Result<bool> {
        let operation = signed.verify()?;
        if operation.thread != self.thread
            || objects::object::thread_authority_admission::OriginalAuthorityBinding::from_operation(&operation)?.is_none()
        {
            return Ok(false);
        }
        let row: Option<(Vec<u8>, Vec<u8>, bool)> = self.connect()?.query_row(
            "SELECT canonical,signature,authority_admitted FROM operations WHERE thread=?1 AND id=?2",
            params![self.thread.as_bytes(), operation.id()?.as_bytes()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        Ok(row.is_some_and(|(canonical, signature, admitted)| {
            admitted && canonical == signed.canonical && signature == signed.signature
        }))
    }
    /// An interactive mutation compares the exact observed property frontier.
    /// Historical replication uses ordinary receive and retains concurrency.
    pub fn receive_control_cas(
        &self,
        signed: &SignedOperation,
        store: &impl ObjectStore,
        authorize: impl FnOnce(&ThreadOperation) -> Result<()>,
    ) -> Result<Admission> {
        if !matches!(signed.verify()?.body, ThreadOperationBody::Metadata(_)) {
            return Err(Error::Invalid("control CAS requires metadata".into()));
        }
        self.receive_inner(signed, store, authorize, true, None)
    }
    pub fn metadata_frontier(
        &self,
        property: &Property,
    ) -> Result<Vec<(ContentHash, SignedOperation)>> {
        let connection = self.connect()?;
        let mut query=connection.prepare("SELECT o.id,o.canonical,o.signature FROM thread_control_heads h JOIN operations o ON o.id=h.operation AND o.thread=h.thread WHERE h.thread=?1 AND h.property=?2 AND o.status=1 ORDER BY o.id LIMIT 129")?;
        let rows = query
            .query_map(params![self.thread.as_bytes(), key(property)], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if rows.len() > 128 {
            return Err(Error::Invalid(
                "Thread property frontier exceeds budget".into(),
            ));
        }
        rows.into_iter()
            .map(|(id, canonical, signature)| {
                let signed = SignedOperation {
                    canonical,
                    signature,
                };
                let operation = signed.verify()?;
                let ThreadOperationBody::Metadata(bytes) = &operation.body else {
                    return Err(Error::Invalid("property index names another facet".into()));
                };
                if operation.thread != self.thread
                    || ThreadControl::decode(bytes)?.property() != *property
                    || operation.id()?.as_bytes().as_slice() != id
                {
                    return Err(Error::Invalid(
                        "Thread property index integrity failure".into(),
                    ));
                }
                Ok((hash(&id)?, signed))
            })
            .collect()
    }
    /// Cursor and limit bound distinct fields/review IDs, not historical rows.
    pub fn metadata_property_page(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Property>> {
        if limit == 0 || limit > 1024 {
            return Err(Error::Invalid(
                "Thread property page must be 1..1024".into(),
            ));
        }
        let connection = self.connect()?;
        let mut query=connection.prepare("SELECT DISTINCT property FROM thread_control_heads WHERE thread=?1 AND property>COALESCE(?2,'') ORDER BY property LIMIT ?3")?;
        let keys = query
            .query_map(
                params![self.thread.as_bytes(), after, limit as u32],
                |row| row.get::<_, String>(0),
            )?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        keys.into_iter().map(|key| parse_key(&key)).collect()
    }
    pub(super) fn check_control_command(
        &self,
        tx: &Transaction<'_>,
        operation: &ThreadOperation,
        id: ContentHash,
        compare_frontier: bool,
    ) -> Result<()> {
        let ThreadOperationBody::Metadata(bytes) = &operation.body else {
            return Ok(());
        };
        let control = ThreadControl::decode(bytes)?;
        let previous:Option<Vec<u8>>=tx.query_row("SELECT operation FROM thread_control_commands WHERE thread=?1 AND publisher=?2 AND command=?3",params![self.thread.as_bytes(),operation.publisher,control.client_operation_id.as_bytes()],|row|row.get(0)).optional()?;
        if previous
            .as_ref()
            .is_some_and(|old| old.as_slice() != id.as_bytes())
        {
            return Err(Error::Invalid(
                "Thread control operation ID reused with different signed bytes".into(),
            ));
        }
        let accepted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND thread=?2 AND status=1)",
            params![id.as_bytes(), self.thread.as_bytes()],
            |row| row.get(0),
        )?;
        if compare_frontier && !accepted {
            let mut query=tx.prepare("SELECT operation FROM thread_control_heads WHERE thread=?1 AND property=?2 ORDER BY operation LIMIT 129")?;
            let current = query
                .query_map(
                    params![self.thread.as_bytes(), key(&control.property())],
                    |row| row.get::<_, Vec<u8>>(0),
                )?
                .map(|row| hash(&row?))
                .collect::<Result<BTreeSet<_>>>()?;
            if current != operation.parents {
                return Err(Error::Invalid("Thread property frontier changed".into()));
            }
        }
        tx.execute("INSERT OR IGNORE INTO thread_control_commands(thread,publisher,command,operation) VALUES(?1,?2,?3,?4)",params![self.thread.as_bytes(),operation.publisher,control.client_operation_id.as_bytes(),id.as_bytes()])?;
        Ok(())
    }
    pub(super) fn accept_control_heads(
        &self,
        tx: &Transaction<'_>,
        operation: &ThreadOperation,
        id: ContentHash,
    ) -> Result<()> {
        let ThreadOperationBody::Metadata(bytes) = &operation.body else {
            return Ok(());
        };
        let property = key(&ThreadControl::decode(bytes)?.property());
        tx.execute(
            "INSERT INTO thread_control_heads(thread,property,operation) VALUES(?1,?2,?3)",
            params![self.thread.as_bytes(), property, id.as_bytes()],
        )?;
        for parent in &operation.parents {
            tx.execute(
                "DELETE FROM thread_control_heads WHERE thread=?1 AND property=?2 AND operation=?3",
                params![self.thread.as_bytes(), property, parent.as_bytes()],
            )?;
        }
        super::listing::refresh(tx, self.thread, &ThreadControl::decode(bytes)?.property())?;
        Ok(())
    }
}
pub(super) fn key(property: &Property) -> String {
    match property {
        Property::Name => "name".into(),
        Property::Intent => "intent".into(),
        Property::Lifecycle => "lifecycle".into(),
        Property::Sharing => "sharing".into(),
        Property::Audience => "audience".into(),
        Property::Retention => "retention".into(),
        Property::Review(id) => format!("review:{id}"),
    }
}
fn parse_key(value: &str) -> Result<Property> {
    Ok(match value {
        "name" => Property::Name,
        "intent" => Property::Intent,
        "lifecycle" => Property::Lifecycle,
        "sharing" => Property::Sharing,
        "audience" => Property::Audience,
        "retention" => Property::Retention,
        _ => Property::Review(
            value
                .strip_prefix("review:")
                .ok_or_else(|| Error::Invalid("unknown Thread property".into()))?
                .parse()
                .map_err(|error| Error::Invalid(format!("invalid review property: {error}")))?,
        ),
    })
}

#[cfg(test)]
#[path = "metadata_tests.rs"]
mod tests;
