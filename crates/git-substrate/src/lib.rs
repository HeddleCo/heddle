// SPDX-License-Identifier: Apache-2.0
//! Single gix↔sley translation point for Heddle git operations (#595).
//!
//! Object ids, framing/hashing, object kinds, read/write adapters over the local
//! [`sley`](../../sley) checkout (`git-core`, `git-formats`, `git-odb`, `git-refs`,
//! `git-rev`). P0: read + hash; P1: loose-object write sink.

pub mod copy;
pub mod framing;
pub mod gix_interop;
pub mod remote;
pub mod transport;
pub mod id;
pub mod index;
pub mod kind;
pub mod refs;
pub mod repo;
pub mod worktree;
pub mod write;

pub use framing::{
    actor_suffix_bytes, append_labeled_actor_line, format_tz_offset, frame_git_object,
    object_id_for_content,
};
pub use gix_interop::{blob_object_id, commit_object_id, is_commit, read_object_kind};
pub use sley_config::{
    load_config_with_includes, ConfigIncludeContext, GitConfig,
};
pub use sley_config::remotes::{add_remote_with_fetch, remove_remote, RemoteEditError};
pub use sley_core::ObjectFormat;
pub use sley_index::{Index, IndexEntry};
pub use sley_object::{Commit, EncodedObject, ObjectType, Tag, Tree, TreeEntry};
pub use sley_refs::{FileRefStore, Ref, RefTarget};
pub use sley_notes::{
    iter_notes, notes_ref_expected, read_note_bytes, remove_notes_for, upsert_note_bytes_for,
    NotesCommitIdentity, NotesRef,
};
pub use id::{
    empty_blob_sha1, empty_tree_sha1, null_sha1, parse_sha1_hex, short_hex, ObjectId,
    EMPTY_BLOB_SHA1, EMPTY_TREE_SHA1,
};
pub use kind::ObjectKind;
pub use refs::{
    branch_name_is_valid, bridge_reflog_committer, delete_reference_if_present,
    delete_reference_matching, ref_name_is_valid, set_reference, update_head_target_ref,
    RefConstraint, RefDeleteConstraint,
};
pub use copy::{collect_reachable_object_ids, copy_reachable_objects, pack_reachable_objects};
pub use remote::{
    configured_remote_is_local_path, configured_remote_local_path, local_path_from_remote_url,
    normalize_configured_remote_url, remote_url_is_file,
};
pub use repo::GitRepo;
pub use transport::{
    fetch_bare_mirror, push_receive_pack, receive_pack_ref_map, supports_native_fetch,
    supports_native_fetch_with_depth, supports_native_push, transport_capabilities, PushCommand,
};
pub use sley_transport::parse_remote_url;
pub use sley_remote::TransportCapabilities;
pub use index::{
    index_cached_stat_from_path, index_entry_is_intent_to_add, index_entry_stage,
    index_file_mtime_secs, read_disk_index, tracked_index_entries, tree_index_entry_map,
    IndexCachedStat, IndexStatProbe, TrackedIndexEntry, INDEX_FLAG_INTENT_TO_ADD,
};
pub use worktree::{
    index_lock_exists, index_path, read_index_from_tree, reconcile_intent_to_add,
    write_index_from_commit, write_index_from_tree, IntentToAddMode,
};
pub use write::{
    write_blob, write_commit_content, write_simple_commit, write_tag, write_tree, TreeEntryInput,
    TreeEntryMode,
};

/// Errors surfaced by the git substrate adapter.
#[derive(Debug, thiserror::Error)]
pub enum GitSubstrateError {
    #[error("git: {0}")]
    Git(#[from] sley_core::GitError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, GitSubstrateError>;