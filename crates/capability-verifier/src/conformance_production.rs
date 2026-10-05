// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native dispatch for the differential corpus of production byte bindings.
use serde_json::{Value, json};

use crate::{Error, Result, observed};

/// Evaluate one byte-identical production case. The envelope retains the typed
/// rejection so differential tests compare codes as well as diagnostics.
pub fn evaluate(case: &Value) -> Result<Value> {
    let result = (|| -> Result<Value> {
        let bytes = |field: &str| -> Result<Vec<u8>> {
            hex::decode(
                case[field]
                    .as_str()
                    .ok_or_else(|| Error::Invalid(format!("missing {field}")))?,
            )
            .map_err(|e| Error::Invalid(e.to_string()))
        };
        let now = integer::<i64>(case, "now", "now_unix_seconds")?;
        let ttl = if case.get("max_ttl").is_some() {
            integer::<i64>(case, "max_ttl", "max_capability_ttl_seconds")?
        } else {
            3600
        };
        let value = match case["api"].as_str() {
            Some("import-scope") => serde_json::to_value(hex::encode(
                crate::import_delegation::remaining_scope_bytes(
                    &bytes("scope_hex")?,
                    &bytes("manifest_hex")?,
                )?,
            )),
            Some("native-genesis") => {
                let digest = crate::native_genesis::verify_bytes(
                    &bytes("binding_hex")?,
                    &bytes("original_hex")?,
                    &bytes("envelope_hex")?,
                    &bytes("keyring_hex")?,
                    &bytes("current_owner_hex")?,
                    &bytes("author_history_hex")?,
                    case["admitted_mint_roots_json"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing admitted mint roots".into()))?,
                    &bytes("initial_owner_hex")?,
                    &bytes("spool_genesis_hex")?,
                    case["revoked_keys_json"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing revoked keys".into()))?,
                    case["revoked_credentials_json"]
                        .as_str()
                        .ok_or_else(|| Error::Invalid("missing revoked credentials".into()))?,
                    now,
                    ttl,
                )?;
                serde_json::to_value(digest)
            }
            Some("owner-root") => {
                serde_json::to_value(observed::verify_owner_root_bytes(&bytes("root_hex")?)?)
            }
            Some("resource-keyring" | "transfer-chain") => {
                serde_json::to_value(observed::verify_resource_keyring_bytes(
                    &bytes("keyring_hex")?,
                    &bytes("current_owner_hex")?,
                    now,
                    ttl,
                )?)
            }
            Some("transfer") => serde_json::to_value(observed::verify_ownership_transfer_bytes(
                &bytes("transfer_hex")?,
                &bytes("source_history_hex")?,
                &bytes("destination_history_hex")?,
                &bytes("resource_uuid_hex")?,
                integer::<u64>(case, "sequence", "expected_sequence")?,
                now,
                ttl,
            )?),
            Some("genesis") => serde_json::to_value(observed::verify_spool_owner_genesis_bytes(
                &bytes("genesis_hex")?,
                now,
            )?),
            Some("policy") => {
                let records = case["records_hex"]
                    .as_array()
                    .ok_or_else(|| Error::Invalid("missing policy records".into()))?
                    .iter()
                    .map(|v| {
                        hex::decode(
                            v.as_str()
                                .ok_or_else(|| Error::Invalid("missing record hex".into()))?,
                        )
                        .map_err(|e| Error::Invalid(e.to_string()))
                    })
                    .collect::<Result<Vec<_>>>()?;
                serde_json::to_value(observed::verify_signed_policy_chain_bytes(
                    &records,
                    &bytes("keyring_hex")?,
                    &bytes("current_owner_hex")?,
                    now,
                    ttl,
                )?)
            }
            _ => return Err(Error::Invalid("unknown production corpus API".into())),
        };
        value.map_err(|e| Error::Invalid(e.to_string()))
    })();
    Ok(match result {
        Ok(value) => json!({"ok": value}),
        Err(error) => json!({"error": observed::VerificationError::from(error)}),
    })
}

fn integer<T: std::str::FromStr>(case: &Value, field: &str, parameter: &str) -> Result<T> {
    case[field]
        .as_str()
        .ok_or_else(|| Error::Invalid(format!("missing decimal {field}")))?
        .parse()
        .map_err(|_| {
            Error::Invalid(format!(
                "{parameter} is outside the {} range",
                std::any::type_name::<T>()
            ))
        })
}
