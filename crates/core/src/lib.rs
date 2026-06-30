// SPDX-License-Identifier: Apache-2.0
//! Embeddable Heddle facade scaffolding.

pub mod context;
pub mod contract;
pub mod fsck;
pub mod query;
pub mod verify;

pub use context::{ExecutionContext, ExecutionContextBuilder, Verbosity};
pub use contract::{
    HeddleReport, MachineOutputKind, OutputDiscriminator, ReportContract, schema_for_report,
};
pub use fsck::{FsckError, FsckOptions, FsckReport, fsck};
pub use objects::{
    CollectingWarnings, HeddleError, NoopProgress, NoopWarnings, ProgressEvent, ProgressSink,
    TaskId, Warning, WarningSink,
};
pub use query::{QueryHit, QueryReport, QueryRequest, query};
pub use verify::{
    ActionTemplate, MachineContractCoverage, PlainGitVerifyProbe, RepositoryContextInfo,
    RepositoryPresentation, RepositoryVerificationState, VerificationCheck, VerifyOptions,
    VerifyProfile, VerifyReport, dirty_path_count, repository_mode_label, repository_presentation,
    verify,
};
