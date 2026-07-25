// SPDX-License-Identifier: Apache-2.0
//! CLI verb catalog, schemas, help contracts, and recovery advice.

pub mod advice;
pub mod command_catalog;
pub mod doctor_docs;
pub mod doctor_schemas;
pub mod schemas;
pub mod verification_health;

pub use advice::RecoveryAdvice;
pub use command_catalog::{
    CommandCatalogOutput, CommandRuntimeContract, advanced_help_groups, build_command_catalog,
    command_canonical_command, command_contract_root_commands, command_help_tier,
    command_help_visibility, command_path, command_persists_op_id, command_runtime_contract,
    command_runtime_contract_for_command, command_supports_json_for_command,
    command_supports_op_id, command_supports_op_id_for_command, command_surface,
    command_uses_bootstrap_op_id_store, observe_only_root_commands, operator_envelope_verbs,
    root_commands_for_advanced_help, root_commands_for_help_visibility,
};
pub use doctor_docs::cmd_doctor_docs;
pub use doctor_schemas::{cmd_doctor_schemas, documented_samples_with_bound_verbs};
pub use schemas::{cmd_schemas, documented_schema_verbs, schema_for_verb, schema_verbs};
