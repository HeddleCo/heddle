// SPDX-License-Identifier: Apache-2.0
//! References: threads (branches), markers (tags), and HEAD.

mod backend;
mod facet;
mod head;
mod name;
pub mod operation_index;
mod packed_refs;
mod reconcile;
mod ref_backend;
mod ref_summary_index;
mod refs_head;
mod refs_manager;
mod refs_storage;
mod refs_transactions;
mod refs_types;
mod resolve;
mod text;
mod types;

#[cfg(feature = "postgres")]
mod pg_refs;

#[cfg(test)]
mod refs_tests;

#[cfg(test)]
mod refs_packed_tests;

#[cfg(test)]
mod reftable_tests;

pub use backend::CoreRefBackend;
pub use facet::SpoolFacet;
pub use head::{Head, HeadParseError};
pub use heddle_schema::refs::{
    FOOTER_LEN, HEADER_LEN, MAGIC, PackedRefsModel, ReftableError, ReftableModel,
};
pub use name::{RefNameError, validate_ref_name};
pub use operation_index::{IndexedOperation, OperationLogIndex, OperationLogQuery};
#[cfg(feature = "postgres")]
pub use pg_refs::PgRefBackend;
pub use reconcile::{LoadRequest, Loaded, ReconcileOutcome, RefClass, RefCommitter, RefReconciler};
pub use ref_backend::RefBackend;
pub use ref_summary_index::RefSummaryIndexInspection;
pub use refs_manager::{RefManager, UNDO_RECOVERY_HANDLE};
pub use resolve::resolve_refspec;
pub use text::{StateIdTextError, format_state_id_text, parse_state_id_text};
pub use types::{RefExpectation, RefUpdate};

#[cfg(test)]
pub(crate) fn fresh_state_id() -> objects::object::StateId {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut bytes = [0; 32];
    bytes[..8].copy_from_slice(&NEXT.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    objects::object::StateId::from_bytes(bytes)
}
