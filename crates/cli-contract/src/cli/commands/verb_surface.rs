// SPDX-License-Identifier: Apache-2.0
//! Construction-linked CLI verbs.
//!
//! A migrated verb declares its key once on the clap args type
//! (`#[heddle_verb]`) and once on the output type. Both derives stamp
//! `HEDDLE_VERB`. The command catalog and schema registry reuse that
//! key; they are not a second catalog.
//!
//! Unmigrated verbs still use hand-written schema mirrors. This list is
//! the set for which a mirror is forbidden.

use crate::cli::InitArgs;

use super::init_output::InitOutput;

/// Schema verbs whose JSON Schema is derived from the real output type.
pub const CONSTRUCTED_SCHEMA_VERBS: &[&str] = &[InitOutput::HEDDLE_VERB];

/// Pairing facts a construction-link test can assert without walking
/// every remaining mirror in `schemas.rs`.
pub struct ConstructedVerb {
    pub verb: &'static str,
    pub args_verb: &'static str,
    pub output_verb: &'static str,
}

/// Migrated verbs in the order they should be checked.
pub fn constructed_verbs() -> [ConstructedVerb; 1] {
    [ConstructedVerb {
        verb: InitOutput::HEDDLE_VERB,
        args_verb: InitArgs::HEDDLE_VERB,
        output_verb: InitOutput::HEDDLE_VERB,
    }]
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use schemars::schema_for;
    use serde_json::Value;

    use crate::cli::Cli;

    use super::super::{command_catalog, schemas};
    use super::*;

    fn property_keys(schema: &Value) -> Vec<String> {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return Vec::new();
        };
        properties.keys().cloned().collect()
    }

    #[test]
    fn constructed_verbs_share_one_key_across_clap_catalog_and_schema() {
        let catalog = command_catalog::build_command_catalog();
        for constructed in constructed_verbs() {
            assert_eq!(constructed.args_verb, constructed.verb);
            assert_eq!(constructed.output_verb, constructed.verb);
            assert!(
                Cli::command().find_subcommand(constructed.verb).is_some(),
                "clap is missing subcommand `{}`",
                constructed.verb
            );
            assert!(
                catalog.commands.iter().any(|command| {
                    command.display == constructed.verb
                        && command
                            .schema_verbs
                            .iter()
                            .any(|verb| verb == constructed.verb)
                }),
                "command catalog is missing schema verb `{}`",
                constructed.verb
            );
            assert!(
                schemas::schema_verbs().contains(&constructed.verb),
                "schema registry is missing constructed verb `{}`",
                constructed.verb
            );
        }
    }

    #[test]
    fn init_schema_is_the_real_output_type() {
        let from_type = serde_json::to_value(schema_for!(InitOutput))
            .expect("InitOutput schema should serialize");
        let from_registry =
            schemas::schema_for_verb(InitOutput::HEDDLE_VERB).expect("init must stay registered");
        let type_keys = property_keys(&from_type);
        let registry_keys = property_keys(&from_registry);
        for key in &type_keys {
            assert!(
                registry_keys.contains(key),
                "registry schema dropped constructed field `{key}`"
            );
        }
        assert!(
            !type_keys.iter().any(|key| key == "verification"),
            "skip-serialized verification must not appear on the derived schema"
        );
        assert!(
            !type_keys
                .iter()
                .any(|key| key == "placeholder_principal_warning"),
            "text-only placeholder warning must not appear on the derived schema"
        );
        assert_eq!(
            from_type.get("title").and_then(Value::as_str),
            Some("InitSchema"),
            "published schema title stays InitSchema"
        );
    }
}
