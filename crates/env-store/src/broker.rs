// SPDX-License-Identifier: Apache-2.0
//! Policy broker: resolve scoped run values, never key material.
//!
//! The broker holds software recipient secrets for the v1 weaker-custody
//! fallback. Callers cannot export them through this interface. Same-UID
//! processes can still read the 0600 key file; that is cooperative, not OS
//! isolation (phase 4).
//!
//! There is no general "get secret" API. The only value-returning path is
//! private to [`PolicyBroker::run`], which owns the child command until exit.

use std::collections::HashMap;
use std::process::{Command, ExitStatus};

use crypto::{Signer, SoftwareRecipientSecret};
use heddle_object_model::object::Attribution;
use zeroize::Zeroize;

use crate::{
    error::{EnvStoreError, Result},
    ids::RecipientId,
    store::EnvStore,
    types::AuditEventKind,
};

/// Slot values held only while preparing the child environment. Not `Debug`.
struct RunSecrets {
    values: Vec<(String, Vec<u8>)>,
}

impl RunSecrets {
    fn inject_into(mut self, command: &mut Command) -> Result<()> {
        for (name, bytes) in self.values.drain(..) {
            let mut value = match String::from_utf8(bytes) {
                Ok(value) => value,
                Err(err) => {
                    let mut bytes = err.into_bytes();
                    bytes.zeroize();
                    return Err(EnvStoreError::Invalid(format!(
                        "slot {name} is not valid UTF-8"
                    )));
                }
            };
            command.env(name, &value);
            value.zeroize();
        }
        Ok(())
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

pub struct PolicyBroker {
    store: EnvStore,
    software_secrets: HashMap<RecipientId, SoftwareRecipientSecret>,
    attribution: Attribution,
}

impl PolicyBroker {
    pub fn new(store: EnvStore, attribution: Attribution) -> Self {
        Self {
            store,
            software_secrets: HashMap::new(),
            attribution,
        }
    }

    #[cfg(test)]
    pub(crate) fn store(&self) -> &EnvStore {
        &self.store
    }

    fn hold_software_secret(&mut self, id: RecipientId, secret: SoftwareRecipientSecret) {
        self.software_secrets.insert(id, secret);
    }

    fn hold_on_disk_software_secret(&mut self, id: RecipientId) -> Result<()> {
        let secret = self.store.load_software_secret(id)?;
        self.hold_software_secret(id, secret);
        Ok(())
    }

    /// Resolve one run request against one profile version, inject its values,
    /// execute the child, and record one broker outcome. Plaintext never
    /// crosses the interface. Audit-write failures are surfaced, never
    /// swallowed.
    pub fn run(
        &mut self,
        profile_name: &str,
        requested_slots: &[String],
        signer: &impl Signer,
        mut command: Command,
    ) -> Result<ExitStatus> {
        let outcome = (|| {
            let profile = self.store.find_profile_by_name(profile_name)?;
            let lifecycle = self
                .store
                .effective_lifecycle(profile.profile_id, profile.head)?;
            if !lifecycle.decrypt_allowed() {
                return Err(EnvStoreError::DecryptForbidden(lifecycle.to_string()));
            }
            let state = self.store.load_state(profile.head)?;
            for id in &state.recipient_ids {
                if !self.software_secrets.contains_key(id) {
                    self.hold_on_disk_software_secret(*id)?;
                }
            }
            // Empty `slots` explicitly means every slot on the head version.
            // The resolved names, rather than an implicit all-access marker,
            // are what the audit records.
            let slot_names: Vec<String> = if requested_slots.is_empty() {
                state.slots.iter().map(|slot| slot.name.clone()).collect()
            } else {
                for name in requested_slots {
                    if !state.slots.iter().any(|slot| slot.name == *name) {
                        return Err(EnvStoreError::SlotNotFound(name.clone()));
                    }
                }
                requested_slots.to_vec()
            };
            let mut values = Vec::with_capacity(slot_names.len());
            for slot_name in &slot_names {
                let slot = state
                    .slots
                    .iter()
                    .find(|slot| slot.name == *slot_name)
                    .ok_or_else(|| EnvStoreError::SlotNotFound(slot_name.clone()))?;
                let (recipient_id, secret) = slot
                    .dek_wraps
                    .iter()
                    .find_map(|wrap| {
                        self.software_secrets
                            .get(&wrap.recipient_id)
                            .map(|secret| (wrap.recipient_id, secret))
                    })
                    .ok_or_else(|| EnvStoreError::NoProviderHandle(slot_name.clone()))?;
                let plaintext = self.store.decrypt_slot_in_state(
                    profile.head,
                    slot_name,
                    recipient_id,
                    secret,
                )?;
                values.push((slot_name.clone(), plaintext));
            }
            RunSecrets { values }.inject_into(&mut command)?;
            Ok((profile, slot_names, command))
        })();

        match outcome {
            Ok((profile, slot_names, mut command)) => {
                self.store.record_audit(
                    Some(profile.profile_id),
                    &profile.name,
                    Some(profile.head),
                    &slot_names,
                    AuditEventKind::Run,
                    None,
                    self.attribution.clone(),
                    signer,
                )?;
                Ok(command.status()?)
            }
            Err(err) => {
                self.store.record_audit(
                    None,
                    profile_name,
                    None,
                    requested_slots,
                    AuditEventKind::Denied,
                    Some(err.to_string()),
                    self.attribution.clone(),
                    signer,
                )?;
                Err(err)
            }
        }
    }
}
