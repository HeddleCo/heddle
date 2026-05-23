// SPDX-License-Identifier: Apache-2.0
//! References: threads (branches), markers (tags), and HEAD.

mod backend;
mod head;
mod name;
pub mod operation_index;
mod packed_model;
mod packed_refs;
mod ref_backend;
mod ref_summary_index;
mod refs_head;
mod refs_manager;
mod refs_storage;
mod refs_transactions;
mod refs_types;
mod reftable_model;
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
pub use head::{Head, HeadParseError};
pub use name::{RefNameError, validate_ref_name};
pub use operation_index::{IndexedOperation, OperationLogIndex, OperationLogQuery};
pub use packed_model::PackedRefsModel;
#[cfg(feature = "postgres")]
pub use pg_refs::PgRefBackend;
pub use ref_backend::RefBackend;
pub use ref_summary_index::RefSummaryIndexInspection;
pub use refs_manager::RefManager;
pub use reftable_model::{ReftableError, ReftableModel};
pub use resolve::resolve_refspec;
pub use text::{ChangeIdTextError, format_change_id_text, parse_change_id_text};
pub use types::{RefExpectation, RefUpdate};
