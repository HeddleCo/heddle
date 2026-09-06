// SPDX-License-Identifier: Apache-2.0
//! Policy broker: authorize a scoped, time-boxed slot request and return
//! values, never key material.
//!
//! The broker holds [`ProviderHandle`]s. A handle wraps a software recipient
//! secret (weaker-custody fallback) or, later, a TPM/HSM reference. Callers
//! cannot export the secret. Same-UID processes can still read the 0600 key
//! file; that is cooperative, not OS isolation (phase 4).
//!
//! There is no general "get secret" API. The only value-returning path is
//! [`PolicyBroker::unwrap_for_run`], used by `heddle env run` (and the
//! matching IPC verb). Grants are single-use.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use crypto::{Signer, SoftwareRecipientSecret};
use heddle_object_model::object::Attribution;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::error::{BrokerDenialReason, Result, EnvStoreError};
use crate::ids::{RecipientId, EnvProfileId, EnvProfileVersionId};
use crate::store::{EnvStore, now_ms};
use crate::types::AuditEventKind;

/// Ceiling on a grant's time-to-live. A caller cannot request an unbounded
/// decrypt window; the broker clamps and rejects anything larger.
pub const MAX_GRANT_TTL_MS: i64 = 15 * 60 * 1000;

/// The only v1 decrypt purpose. Phase 4 may add more; the broker still
/// refuses anything except `run`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecryptPurpose {
    Run,
}

impl DecryptPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
        }
    }
}

/// Scoped, time-boxed request. Attenuation is the request itself until
/// Biscuit runtime-slot vocabulary lands (phase 4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecryptRequest {
    pub profile: String,
    pub slots: Vec<String>,
    pub expires_at_ms: i64,
    pub purpose: DecryptPurpose,
    pub caller: String,
}

/// Single-use grant. Carries no plaintext.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecryptGrant {
    id: Uuid,
    profile_id: EnvProfileId,
    profile_name: String,
    state_id: EnvProfileVersionId,
    slots: Vec<String>,
    expires_at_ms: i64,
}

impl DecryptGrant {
    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn slots(&self) -> &[String] {
        &self.slots
    }

    pub fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }
}

/// Provider handle held by the broker process. Not exportable.
pub struct ProviderHandle {
    recipient_id: RecipientId,
    secret: SoftwareRecipientSecret,
}

impl fmt::Debug for ProviderHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderHandle")
            .field("recipient_id", &self.recipient_id)
            .field("secret", &"<held>")
            .finish()
    }
}

/// Slot values for the run path only. Drop zeroizes. Not `Debug`.
pub struct RunSecrets {
    values: Vec<(String, Vec<u8>)>,
}

impl RunSecrets {
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn slot_names(&self) -> impl Iterator<Item = &str> {
        self.values.iter().map(|(name, _)| name.as_str())
    }

    /// Consume into `(name, utf-8 value)` pairs for child env injection.
    pub fn into_env_pairs(mut self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::with_capacity(self.values.len());
        for (name, bytes) in self.values.drain(..) {
            let value = String::from_utf8(bytes).map_err(|_| {
                EnvStoreError::Invalid(format!("slot {name} is not valid UTF-8"))
            })?;
            out.push((name, value));
        }
        Ok(out)
    }
}

impl Drop for RunSecrets {
    fn drop(&mut self) {
        for (_, value) in &mut self.values {
            // Volatile zeroization the optimizer may not elide.
            value.zeroize();
        }
    }
}

struct PendingGrant {
    profile_id: EnvProfileId,
    profile_name: String,
    state_id: EnvProfileVersionId,
    slots: Vec<String>,
    caller: String,
    expires_at_ms: i64,
}

pub struct PolicyBroker {
    store: EnvStore,
    handles: HashMap<RecipientId, ProviderHandle>,
    pending: Mutex<HashMap<Uuid, PendingGrant>>,
    attribution: Attribution,
}

impl PolicyBroker {
    pub fn new(store: EnvStore, attribution: Attribution) -> Self {
        Self {
            store,
            handles: HashMap::new(),
            pending: Mutex::new(HashMap::new()),
            attribution,
        }
    }

    pub fn store(&self) -> &EnvStore {
        &self.store
    }

    /// Hold an already-loaded software secret. The secret stays in this
    /// process; nothing here returns key bytes.
    pub fn hold_software_secret(&mut self, id: RecipientId, secret: SoftwareRecipientSecret) {
        self.handles.insert(
            id,
            ProviderHandle {
                recipient_id: id,
                secret,
            },
        );
    }

    /// Load the on-disk 0600 software secret into a handle (weaker custody).
    pub fn hold_on_disk_software_secret(&mut self, id: RecipientId) -> Result<()> {
        let secret = self.store.load_software_secret(id)?;
        self.hold_software_secret(id, secret);
        Ok(())
    }

    /// Load every on-disk software secret the current profile head names.
    pub fn hold_profile_recipients(&mut self, profile_name: &str) -> Result<()> {
        let profile = self.store.find_profile_by_name(profile_name)?;
        let state = self.store.load_state(profile.head)?;
        for id in state.recipient_ids {
            if !self.handles.contains_key(&id) {
                self.hold_on_disk_software_secret(id)?;
            }
        }
        Ok(())
    }

    /// Authorize a request. The broker reads its OWN clock — a caller cannot
    /// supply `now` and neutralize the time-box — and enforces a TTL ceiling.
    /// Both the grant and the denial are audited (and an audit-write failure
    /// is surfaced, never swallowed).
    pub fn authorize(&self, request: &DecryptRequest, signer: &impl Signer) -> Result<DecryptGrant> {
        let now = now_ms()?;
        match self.authorize_inner(request, now) {
            Ok(grant) => {
                self.store.record_audit(
                    Some(grant.profile_id),
                    &grant.profile_name,
                    Some(grant.state_id),
                    &grant.slots,
                    request.purpose.as_str(),
                    &request.caller,
                    AuditEventKind::Granted,
                    None,
                    self.attribution.clone(),
                    signer,
                )?;
                Ok(grant)
            }
            Err(err) => {
                self.store.record_audit(
                    None,
                    &request.profile,
                    None,
                    &request.slots,
                    request.purpose.as_str(),
                    &request.caller,
                    AuditEventKind::Denied,
                    Some(err.to_string()),
                    self.attribution.clone(),
                    signer,
                )?;
                Err(err)
            }
        }
    }

    fn authorize_inner(&self, request: &DecryptRequest, now: i64) -> Result<DecryptGrant> {
        if request.purpose != DecryptPurpose::Run {
            return Err(EnvStoreError::BrokerDenied(
                BrokerDenialReason::PurposeNotAllowed,
            ));
        }
        if request.expires_at_ms <= now {
            return Err(EnvStoreError::BrokerDenied(BrokerDenialReason::Expired));
        }
        if request.expires_at_ms.saturating_sub(now) > MAX_GRANT_TTL_MS {
            return Err(EnvStoreError::BrokerDenied(
                BrokerDenialReason::TtlTooLong,
            ));
        }
        let profile = self.store.find_profile_by_name(&request.profile)?;
        // Gate on the authoritative signed lifecycle, not the unsigned field.
        let lifecycle = self
            .store
            .effective_lifecycle(profile.profile_id, profile.head)?;
        if !lifecycle.decrypt_allowed() {
            return Err(EnvStoreError::DecryptForbidden(lifecycle.to_string()));
        }
        let state = self.store.load_state(profile.head)?;
        // Empty `slots` explicitly means "every slot on the head version"; the
        // resolved set is recorded in the grant and audit, so it is never a
        // silent all-access.
        let slot_names: Vec<String> = if request.slots.is_empty() {
            state.slots.iter().map(|slot| slot.name.clone()).collect()
        } else {
            for name in &request.slots {
                if !state.slots.iter().any(|slot| slot.name == *name) {
                    return Err(EnvStoreError::SlotNotFound(name.clone()));
                }
            }
            request.slots.clone()
        };
        for name in &slot_names {
            let slot = state
                .slots
                .iter()
                .find(|slot| slot.name == *name)
                .ok_or_else(|| EnvStoreError::SlotNotFound(name.clone()))?;
            let has_handle = slot
                .dek_wraps
                .iter()
                .any(|wrap| self.handles.contains_key(&wrap.recipient_id));
            if !has_handle {
                return Err(EnvStoreError::BrokerDenied(
                    BrokerDenialReason::NoProviderHandle(name.clone()),
                ));
            }
        }
        let id = Uuid::now_v7();
        let mut pending = self.pending.lock().map_err(|_| {
            EnvStoreError::Invalid("broker grant lock was poisoned".to_string())
        })?;
        pending.insert(
            id,
            PendingGrant {
                profile_id: profile.profile_id,
                profile_name: profile.name.clone(),
                state_id: profile.head,
                slots: slot_names.clone(),
                caller: request.caller.clone(),
                expires_at_ms: request.expires_at_ms,
            },
        );
        Ok(DecryptGrant {
            id,
            profile_id: profile.profile_id,
            profile_name: profile.name,
            state_id: profile.head,
            slots: slot_names,
            expires_at_ms: request.expires_at_ms,
        })
    }

    /// Return slot values for the run path only. Consumes the grant; the broker
    /// reads its own clock for the expiry check.
    pub fn unwrap_for_run(&self, grant: DecryptGrant, signer: &impl Signer) -> Result<RunSecrets> {
        let now = now_ms()?;
        // Consume the grant first so an expired or replayed grant cannot linger
        // in the pending map.
        let pending = {
            let mut map = self.pending.lock().map_err(|_| {
                EnvStoreError::Invalid("broker grant lock was poisoned".to_string())
            })?;
            map.remove(&grant.id)
        };
        let Some(pending) = pending else {
            return Err(EnvStoreError::InvalidGrant(grant.id.to_string()));
        };
        if now >= pending.expires_at_ms {
            self.store.record_audit(
                Some(pending.profile_id),
                &pending.profile_name,
                Some(pending.state_id),
                &pending.slots,
                DecryptPurpose::Run.as_str(),
                &pending.caller,
                AuditEventKind::Denied,
                Some("grant expired".to_string()),
                self.attribution.clone(),
                signer,
            )?;
            return Err(EnvStoreError::BrokerDenied(BrokerDenialReason::Expired));
        }
        let mut values = Vec::with_capacity(pending.slots.len());
        for slot_name in &pending.slots {
            let (recipient_id, secret) = self.secret_for_slot(pending.state_id, slot_name)?;
            let plaintext =
                self.store
                    .decrypt_slot_in_state(pending.state_id, slot_name, recipient_id, secret)?;
            values.push((slot_name.clone(), plaintext));
        }
        self.store.record_audit(
            Some(pending.profile_id),
            &pending.profile_name,
            Some(pending.state_id),
            &pending.slots,
            DecryptPurpose::Run.as_str(),
            &pending.caller,
            AuditEventKind::Run,
            None,
            self.attribution.clone(),
            signer,
        )?;
        Ok(RunSecrets { values })
    }

    fn secret_for_slot(
        &self,
        state_id: EnvProfileVersionId,
        slot_name: &str,
    ) -> Result<(RecipientId, &SoftwareRecipientSecret)> {
        let state = self.store.load_state(state_id)?;
        let slot = state
            .slots
            .iter()
            .find(|slot| slot.name == slot_name)
            .ok_or_else(|| EnvStoreError::SlotNotFound(slot_name.to_string()))?;
        for wrap in &slot.dek_wraps {
            if let Some(handle) = self.handles.get(&wrap.recipient_id) {
                return Ok((wrap.recipient_id, &handle.secret));
            }
        }
        Err(EnvStoreError::BrokerDenied(
            BrokerDenialReason::NoProviderHandle(slot_name.to_string()),
        ))
    }
}
