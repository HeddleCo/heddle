// SPDX-License-Identifier: Apache-2.0

mod codec;
mod ids;
mod materialize;
mod operation;

pub use codec::{CollaborationCodecError, DecodedCollaborationOperation};
pub use ids::{
    CollabOpId, CollabOpIdParseError, CollaborationIdempotencyKey, DiscussionRecordId,
    DiscussionRecordIdParseError, LegacyDiscussionId, LegacySourceLocator,
};
pub use materialize::{
    HostedCollaborationSet, MaterializedDiscussion, MaterializedRepositoryCollaboration,
    materialize_repository_collaboration,
};
pub use operation::{
    CollaborationAnchor, CollaborationOperationBodyV1, CollaborationOperationEnvelope,
    CollaborationResolution, DiscussionTurnV1, LegacyDiscussionResolutionV1,
};

pub const COLLABORATION_OPERATION_SCHEMA_VERSION: u16 = 1;
