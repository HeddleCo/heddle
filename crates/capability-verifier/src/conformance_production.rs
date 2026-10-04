// SPDX-License-Identifier: MIT OR Apache-2.0
//! Native dispatch for the differential corpus of production byte bindings.
use crate::{Error, Result, observed};
use serde_json::{Value, json};

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
        let now = case["now"]
            .as_i64()
            .ok_or_else(|| Error::Invalid("missing now".into()))?;
        let ttl = case["max_ttl"].as_i64().unwrap_or(3600);
        let value = match case["api"].as_str() {
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
                case["sequence"]
                    .as_u64()
                    .ok_or_else(|| Error::Invalid("missing sequence".into()))?,
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
