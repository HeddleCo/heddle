// SPDX-License-Identifier: Apache-2.0
//! Per-record outcome of replicating collaboration during `push`.
//!
//! Source publication and collaboration replication succeed or fail
//! independently. A push reports, for each collaboration record the hosted
//! service does not hold afterwards, why not and whether resending the same
//! signed command could ever help. Callers turn these facts into recovery
//! advice; this module only classifies.

use std::fmt;

use wire::{ProtocolError, RemoteFailureCode, RemoteFailureDetail};

/// Why a collaboration record is not on the hosted service after a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicationIssueKind {
    /// The hosted record changed after this command was prepared against it.
    /// The service refuses the same signed command every time it is resent.
    StaleVersion,
    /// The service refused the signed command itself. Resending the same
    /// bytes is refused again.
    InvalidCommand,
    /// Delivery did not complete. Resending the identical signed command is
    /// safe: the service applies it at most once.
    Transient,
    /// The credential lacks authority for this command.
    PermissionDenied,
    /// No hosted command can carry this local record, so nothing was sent.
    NotReplicable,
    /// A revision refused as stale that the author then replaced with an
    /// explicit revision. It stays in local history and is never sent.
    Superseded,
}

impl ReplicationIssueKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StaleVersion => "stale_version",
            Self::InvalidCommand => "invalid_command",
            Self::Transient => "transient",
            Self::PermissionDenied => "permission_denied",
            Self::NotReplicable => "not_replicable",
            Self::Superseded => "superseded",
        }
    }

    /// Whether resending the same signed command can succeed.
    pub fn retry_unchanged(self) -> bool {
        matches!(self, Self::Transient)
    }

    /// Whether the record stays local by construction rather than awaiting
    /// delivery.
    pub fn local_only(self) -> bool {
        matches!(self, Self::NotReplicable | Self::Superseded)
    }

    /// Whether this outcome leaves replication incomplete.
    pub fn blocks_completion(self) -> bool {
        !matches!(self, Self::Superseded)
    }
}

/// The identities of one signed collaboration command, kept stable across
/// retries so a report can name exactly what was, or will be, redelivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandIds {
    /// The command ID carried on the wire.
    pub client_operation_id: String,
    /// The signed Thread operation's content ID.
    pub signed_operation_id: String,
}

/// One collaboration record the hosted service does not hold after a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicationIssue {
    /// Local discussion or annotation ID. `None` when the whole surface failed
    /// before any single record was attempted.
    pub record_id: Option<String>,
    /// The annotation revision involved, when one is.
    pub revision_id: Option<String>,
    pub kind: ReplicationIssueKind,
    /// The signed command involved, when one was prepared.
    pub command: Option<CommandIds>,
    pub message: String,
}

/// What one collaboration surface replicated during a push.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplicationReport {
    /// Records the hosted service accepted during this push.
    pub accepted: usize,
    pub issues: Vec<ReplicationIssue>,
}

impl ReplicationReport {
    /// True when every local record is on the hosted service or stays local
    /// by an explicit decision.
    pub fn complete(&self) -> bool {
        !self
            .issues
            .iter()
            .any(|issue| issue.kind.blocks_completion())
    }
}

/// Error context naming the signed command whose delivery failed. Its
/// presence in an error chain means a request reached the transport.
#[derive(Debug, Clone)]
pub(crate) struct DeliveredCommand(pub(crate) CommandIds);

impl fmt::Display for DeliveredCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "deliver command {} (signed operation {})",
            self.0.client_operation_id, self.0.signed_operation_id
        )
    }
}

/// Error context naming the annotation revision a failure belongs to.
#[derive(Debug, Clone)]
pub(crate) struct FailedRevision(pub(crate) String);

impl fmt::Display for FailedRevision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "revision {}", self.0)
    }
}

/// A revision the service refused as stale is waiting for the author to
/// refresh, compare and revise explicitly. Nothing was sent.
#[derive(Debug, Clone)]
pub(crate) struct StaleRevisionPending {
    pub(crate) revision_id: String,
    pub(crate) command: CommandIds,
    pub(crate) refreshed: bool,
}

impl fmt::Display for StaleRevisionPending {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let step = if self.refreshed {
            "author an explicit revision"
        } else {
            "refresh, compare the history, then author an explicit revision"
        };
        write!(
            f,
            "revision {} was refused because the hosted context changed after command {} was prepared; resending it unchanged cannot succeed, so it was not sent again: {step}",
            self.revision_id, self.command.client_operation_id
        )
    }
}

impl std::error::Error for StaleRevisionPending {}

/// Build the issue for one record from the error that stopped it.
pub(crate) fn issue_from_error(
    record_id: Option<String>,
    revision_id: Option<String>,
    error: &anyhow::Error,
) -> ReplicationIssue {
    if let Some(pending) = error.downcast_ref::<StaleRevisionPending>() {
        return ReplicationIssue {
            record_id,
            revision_id: Some(pending.revision_id.clone()),
            kind: ReplicationIssueKind::StaleVersion,
            command: Some(pending.command.clone()),
            message: format!("{error:#}"),
        };
    }
    ReplicationIssue {
        record_id,
        revision_id: revision_id.or_else(|| {
            error
                .downcast_ref::<FailedRevision>()
                .map(|revision| revision.0.clone())
        }),
        kind: classify_error(error),
        command: error
            .downcast_ref::<DeliveredCommand>()
            .map(|delivered| delivered.0.clone()),
        message: format!("{error:#}"),
    }
}

/// Classify the error that stopped one record or a whole surface.
pub fn classify_error(error: &anyhow::Error) -> ReplicationIssueKind {
    if error.downcast_ref::<StaleRevisionPending>().is_some() {
        return ReplicationIssueKind::StaleVersion;
    }
    let delivered = error.downcast_ref::<DeliveredCommand>().is_some();
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ProtocolError>())
        .map_or(ReplicationIssueKind::NotReplicable, |protocol| {
            classify_protocol(protocol, delivered)
        })
}

fn classify_protocol(error: &ProtocolError, delivered: bool) -> ReplicationIssueKind {
    use ReplicationIssueKind as Kind;
    match error {
        ProtocolError::RemoteFailure {
            code,
            message,
            details,
        } => {
            if details
                .iter()
                .any(|detail| matches!(detail, RemoteFailureDetail::Conflict { .. }))
            {
                return Kind::StaleVersion;
            }
            match code {
                RemoteFailureCode::Aborted if is_stale_message(message) => Kind::StaleVersion,
                RemoteFailureCode::AlreadyExists if is_stale_message(message) => Kind::StaleVersion,
                RemoteFailureCode::InvalidArgument
                | RemoteFailureCode::FailedPrecondition
                | RemoteFailureCode::OutOfRange
                | RemoteFailureCode::AlreadyExists
                | RemoteFailureCode::NotFound
                | RemoteFailureCode::Unimplemented
                | RemoteFailureCode::DataLoss => Kind::InvalidCommand,
                RemoteFailureCode::PermissionDenied | RemoteFailureCode::Unauthenticated => {
                    Kind::PermissionDenied
                }
                RemoteFailureCode::Aborted
                | RemoteFailureCode::Unavailable
                | RemoteFailureCode::DeadlineExceeded
                | RemoteFailureCode::ResourceExhausted
                | RemoteFailureCode::Internal
                | RemoteFailureCode::Unknown
                | RemoteFailureCode::Cancelled
                | RemoteFailureCode::Unspecified => Kind::Transient,
            }
        }
        ProtocolError::AlreadyExists(message) if is_stale_message(message) => Kind::StaleVersion,
        ProtocolError::AlreadyExists(_) | ProtocolError::ObjectNotFound(_) => Kind::InvalidCommand,
        ProtocolError::AuthenticationFailed(_) | ProtocolError::AuthorizationFailed(_) => {
            Kind::PermissionDenied
        }
        ProtocolError::Io(_) | ProtocolError::Remote(_) | ProtocolError::LockError(_) => {
            Kind::Transient
        }
        // Local refusals say so; anything else refused after a request left
        // is the service rejecting what was sent.
        ProtocolError::InvalidState(message) if !delivered && is_local_refusal(message) => {
            Kind::NotReplicable
        }
        ProtocolError::InvalidState(_)
        | ProtocolError::PublicationLimitExceeded { .. }
        | ProtocolError::PublicationOperationTooLarge { .. }
        | ProtocolError::Serialization(_)
        | ProtocolError::MessageTooLarge { .. }
        | ProtocolError::InvalidMessageType(_)
        | ProtocolError::VersionMismatch { .. }
        | ProtocolError::CapabilityNotSupported(_) => Kind::InvalidCommand,
    }
}

/// Weft's expected-version refusals ask the author to refresh; its other
/// `Aborted` refusals (authority epoch, in-flight command) ask for a retry.
fn is_stale_message(message: &str) -> bool {
    message.contains("refresh before") || message.contains("use observed version")
}

/// Every local refusal in collaboration replication ends with this phrase.
fn is_local_refusal(message: &str) -> bool {
    message.contains("replication is incomplete")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(code: RemoteFailureCode, message: &str) -> anyhow::Error {
        anyhow::Error::new(ProtocolError::RemoteFailure {
            code,
            message: message.into(),
            details: Vec::new(),
        })
    }

    fn delivered(error: anyhow::Error) -> anyhow::Error {
        error.context(DeliveredCommand(CommandIds {
            client_operation_id: "client".into(),
            signed_operation_id: "signed".into(),
        }))
    }

    #[test]
    fn weft_refusals_classify_by_whether_a_resend_can_succeed() {
        use ReplicationIssueKind as Kind;
        for (code, message, kind) in [
            (
                RemoteFailureCode::Aborted,
                "context changed; refresh before revising",
                Kind::StaleVersion,
            ),
            (
                RemoteFailureCode::Aborted,
                "discussion changed; refresh before changing resolution",
                Kind::StaleVersion,
            ),
            (
                RemoteFailureCode::AlreadyExists,
                "context already exists; use observed version",
                Kind::StaleVersion,
            ),
            (
                RemoteFailureCode::Aborted,
                "authority changed; retry collaboration command",
                Kind::Transient,
            ),
            (
                RemoteFailureCode::Aborted,
                "command in flight; retry same operation",
                Kind::Transient,
            ),
            (
                RemoteFailureCode::InvalidArgument,
                "command ID differs from signed operation",
                Kind::InvalidCommand,
            ),
            (
                RemoteFailureCode::FailedPrecondition,
                "operation ID names another command",
                Kind::InvalidCommand,
            ),
            (
                RemoteFailureCode::PermissionDenied,
                "discussion audience does not permit this command",
                Kind::PermissionDenied,
            ),
            (RemoteFailureCode::Unavailable, "lost", Kind::Transient),
        ] {
            let error = delivered(remote(code, message));
            assert_eq!(classify_error(&error), kind, "{message}");
            let error = error.context(FailedRevision("rev".into()));
            let issue = issue_from_error(Some("record".into()), None, &error);
            assert_eq!(issue.kind, kind);
            assert_eq!(issue.revision_id.as_deref(), Some("rev"));
            assert_eq!(
                issue.command.map(|command| command.client_operation_id),
                Some("client".into()),
                "the delivered command's IDs are kept"
            );
            assert_eq!(kind.retry_unchanged(), kind == Kind::Transient);
        }
    }

    #[test]
    fn local_refusals_are_local_only_and_nothing_was_sent() {
        let refused = anyhow::Error::new(ProtocolError::InvalidState(
            "into-annotation discussion resolution has no native hosted command; replication is incomplete".into(),
        ));
        assert_eq!(
            classify_error(&refused),
            ReplicationIssueKind::NotReplicable
        );
        let bailed = anyhow::anyhow!("context x is not attributed to the local principal");
        let issue = issue_from_error(Some("x".into()), None, &bailed);
        assert_eq!(issue.kind, ReplicationIssueKind::NotReplicable);
        assert!(issue.kind.local_only());
        assert_eq!(issue.command, None);
        // The same text after a request left is the service's refusal.
        let after_send = delivered(anyhow::Error::new(ProtocolError::InvalidState(
            "put context was not applied to the requested endpoint".into(),
        )));
        assert_eq!(
            classify_error(&after_send),
            ReplicationIssueKind::InvalidCommand
        );
    }

    #[test]
    fn pending_stale_revision_keeps_its_command_and_blocks_completion() {
        let error = anyhow::Error::new(StaleRevisionPending {
            revision_id: "rev".into(),
            command: CommandIds {
                client_operation_id: "client".into(),
                signed_operation_id: "signed".into(),
            },
            refreshed: false,
        })
        .context("revise hosted annotation a");
        let issue = issue_from_error(Some("a".into()), None, &error);
        assert_eq!(issue.kind, ReplicationIssueKind::StaleVersion);
        assert_eq!(issue.revision_id.as_deref(), Some("rev"));
        assert!(!issue.kind.retry_unchanged());
        let report = ReplicationReport {
            accepted: 0,
            issues: vec![issue],
        };
        assert!(!report.complete());
        let superseded = ReplicationReport {
            accepted: 1,
            issues: vec![ReplicationIssue {
                record_id: Some("a".into()),
                revision_id: Some("rev".into()),
                kind: ReplicationIssueKind::Superseded,
                command: None,
                message: String::new(),
            }],
        };
        assert!(superseded.complete());
    }
}
