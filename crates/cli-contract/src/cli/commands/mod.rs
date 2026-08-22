// SPDX-License-Identifier: Apache-2.0
//! CLI verb catalog, schemas, help contracts, and recovery advice.

pub mod advice;
pub mod command_catalog;
pub mod doctor_docs;
pub mod doctor_schemas;
pub mod init_output;
pub mod schemas;
pub mod surface_conformance;
pub mod verification_health;

pub use advice::RecoveryAdvice;
pub use command_catalog::{
    CommandCatalogOutput, CommandRuntimeContract, build_command_catalog, command_canonical_command,
    command_contract_root_commands, command_help_visibility, command_path,
    command_persists_op_id, command_runtime_contract, command_runtime_contract_for_command,
    command_supports_json_for_command, command_supports_op_id,
    command_supports_op_id_for_command, command_surface, command_uses_bootstrap_op_id_store,
    observe_only_root_commands, operator_envelope_verbs, ranked_visible_roots,
    root_commands_for_help_visibility,
};
pub use doctor_docs::cmd_doctor_docs;
pub use doctor_schemas::{cmd_doctor_schemas, documented_samples_with_bound_verbs};
pub use init_output::{InitOutput, InitPrincipalOutput};
pub use schemas::{documented_schema_verbs, schema_for_verb, schema_verbs};
pub use surface_conformance::{
    APPROVED_NON_EVERYDAY_ROOT_COMMANDS, APPROVED_ROOT_ALIASES, CANONICAL_ROOT_COMMANDS,
    CommandSurfaceViolation, command_surface_violations, is_approved_root_command,
    unapproved_root_command_names,
};
