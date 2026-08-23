// SPDX-License-Identifier: Apache-2.0
//! Exact budgets for low-level ref writes that cannot create new operations.

use std::path::Path;

pub(super) fn path_key(path: &Path) -> &str {
    const KEYS: &[&str] = &[
        "repo/src/repository.rs",
        "repo/src/discovery.rs",
        "repo/src/repository_goto.rs",
        "repo/src/repository_thread_materialize.rs",
        "cli/src/cli/commands/undo_apply/mod.rs",
        "cli/src/cli/commands/start_atomic.rs",
        "cli/src/cli/commands/clone.rs",
        "cli/src/cli/commands/git_projection_io.rs",
        "git-projection/src/git_core.rs",
    ];
    KEYS.iter()
        .copied()
        .find(|key| path.ends_with(key))
        .unwrap_or("")
}

pub(super) fn budget(path: &Path, function: &str, method: &str) -> usize {
    match (path_key(path), function, method) {
        ("repo/src/discovery.rs", "init_git_overlay_sidecar", "write_head") => 1,
        ("repo/src/discovery.rs", "init_with_source_authority", "write_head") => 1,
        ("repo/src/repository.rs", "open", "write_head") => 2,
        ("repo/src/repository.rs", "seed_default_thread", "set_thread") => 1,
        ("repo/src/repository_goto.rs", "fast_forward_attached_internal", "set_thread") => 1,
        ("repo/src/repository_goto.rs", "fast_forward_attached_internal", "write_head") => 1,
        ("repo/src/repository_goto.rs", "goto_internal", "write_head") => 1,
        (
            "repo/src/repository_thread_materialize.rs",
            "cas_guarded_thread_ref_rollback",
            "set_thread_cas",
        ) => 1,
        (
            "repo/src/repository_thread_materialize.rs",
            "cas_guarded_thread_ref_rollback",
            "delete_thread_cas",
        ) => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "restore_head", "write_head") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "write_head", "write_head") => 2,
        ("cli/src/cli/commands/undo_apply/mod.rs", "set_thread", "set_thread") => 2,
        ("cli/src/cli/commands/undo_apply/mod.rs", "set_thread", "delete_thread") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "delete_thread", "delete_thread") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "delete_thread", "set_thread") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "create_marker", "create_marker") => 2,
        ("cli/src/cli/commands/undo_apply/mod.rs", "create_marker", "delete_marker") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "delete_marker", "delete_marker") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "delete_marker", "create_marker") => 1,
        ("cli/src/cli/commands/undo_apply/mod.rs", "apply", "set_undo_recovery") => 2,
        ("cli/src/cli/commands/undo_apply/mod.rs", "apply", "clear_undo_recovery") => 1,
        ("cli/src/cli/commands/start_atomic.rs", "stage_ref", "set_thread_cas") => 2,
        ("cli/src/cli/commands/clone.rs", "clone_network_connected", "write_head") => 2,
        ("cli/src/cli/commands/clone.rs", "recover_interrupted_clone_connected", "write_head") => 2,
        (
            "cli/src/cli/commands/git_projection_io.rs",
            "materialize_imported_attached_thread",
            "set_thread",
        ) => 2,
        (
            "cli/src/cli/commands/git_projection_io.rs",
            "materialize_imported_attached_thread",
            "write_head",
        ) => 2,
        ("git-projection/src/git_core.rs", "pull", "set_thread") => 2,
        ("git-projection/src/git_core.rs", "pull", "write_head") => 2,
        _ => 0,
    }
}
