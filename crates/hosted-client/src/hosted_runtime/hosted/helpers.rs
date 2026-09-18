use api::heddle::api::v1alpha1::StateId as ProtoStateId;
use objects::object::StateId;
use wire::ProtocolError;

use super::HostedError;

pub(super) fn hosted_to_protocol_error(error: HostedError) -> ProtocolError {
    use api::heddle::api::v1alpha1::CallFailureCode;
    match error {
        HostedError::Call {
            code,
            message,
            error,
        } => {
            if let Some(error) = error {
                return ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: vec![remote_failure_detail(*error)],
                };
            }

            match code {
                CallFailureCode::PermissionDenied => ProtocolError::AuthorizationFailed(message),
                // Keep the CallFailureCode so doorbell fetch can treat
                // expired/missing creds as fatal instead of a visibility skip.
                CallFailureCode::Unauthenticated => ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: Vec::new(),
                },
                CallFailureCode::NotFound => ProtocolError::ObjectNotFound(message),
                CallFailureCode::AlreadyExists => ProtocolError::AlreadyExists(message),
                CallFailureCode::InvalidArgument | CallFailureCode::FailedPrecondition => {
                    ProtocolError::InvalidState(message)
                }
                _ => ProtocolError::RemoteFailure {
                    code: remote_failure_code(code),
                    message,
                    details: Vec::new(),
                },
            }
        }
        HostedError::Decode(error) => ProtocolError::Serialization(error.to_string()),
        HostedError::Transport(message) => ProtocolError::Io(std::io::Error::other(message)),
        error => ProtocolError::Remote(error.to_string()),
    }
}

/// Preserve the shared failure envelope at the native client boundary. Callers
/// can distinguish retryable conflicts, missing resources and revoked authority.
pub(super) fn native_client_error(
    error: api::v2::client::ClientError<thread_api::transport::Error>,
) -> ProtocolError {
    use api::v2::client::ClientError;
    use thread_api::transport::Error;
    match error {
        ClientError::Transport(Error::Remote(failure)) => {
            let detail = match failure.detail() {
                Ok(detail) => detail,
                Err(error) => return ProtocolError::Serialization(error.to_string()),
            };
            let code = api::heddle::api::v1alpha1::CallFailureCode::try_from(failure.code)
                .unwrap_or(api::heddle::api::v1alpha1::CallFailureCode::Unknown);
            if detail.is_none() {
                match code {
                    api::heddle::api::v1alpha1::CallFailureCode::AlreadyExists => {
                        return ProtocolError::AlreadyExists(failure.message);
                    }
                    api::heddle::api::v1alpha1::CallFailureCode::NotFound => {
                        return ProtocolError::ObjectNotFound(failure.message);
                    }
                    _ => {}
                }
            }
            ProtocolError::RemoteFailure {
                code: remote_failure_code(code),
                message: failure.message,
                details: detail.into_iter().map(remote_failure_detail).collect(),
            }
        }
        ClientError::Decode(error) => ProtocolError::Serialization(error.to_string()),
        ClientError::Transport(Error::Io(message)) => {
            ProtocolError::Io(std::io::Error::other(message))
        }
        ClientError::Transport(Error::Timeout) => ProtocolError::RemoteFailure {
            code: wire::RemoteFailureCode::DeadlineExceeded,
            message: "native request made no progress before its deadline".into(),
            details: Vec::new(),
        },
        ClientError::NotImplemented(method) => ProtocolError::RemoteFailure {
            code: wire::RemoteFailureCode::Unimplemented,
            message: format!("endpoint does not implement {method}"),
            details: Vec::new(),
        },
        error => ProtocolError::InvalidState(error.to_string()),
    }
}

fn remote_failure_code(
    code: api::heddle::api::v1alpha1::CallFailureCode,
) -> wire::RemoteFailureCode {
    use api::heddle::api::v1alpha1::CallFailureCode as Api;
    use wire::RemoteFailureCode as Wire;
    match code {
        Api::Unspecified => Wire::Unspecified,
        Api::Cancelled => Wire::Cancelled,
        Api::Unknown => Wire::Unknown,
        Api::InvalidArgument => Wire::InvalidArgument,
        Api::DeadlineExceeded => Wire::DeadlineExceeded,
        Api::NotFound => Wire::NotFound,
        Api::AlreadyExists => Wire::AlreadyExists,
        Api::PermissionDenied => Wire::PermissionDenied,
        Api::ResourceExhausted => Wire::ResourceExhausted,
        Api::FailedPrecondition => Wire::FailedPrecondition,
        Api::Aborted => Wire::Aborted,
        Api::OutOfRange => Wire::OutOfRange,
        Api::Unimplemented => Wire::Unimplemented,
        Api::Internal => Wire::Internal,
        Api::Unavailable => Wire::Unavailable,
        Api::DataLoss => Wire::DataLoss,
        Api::Unauthenticated => Wire::Unauthenticated,
    }
}

fn remote_duration(value: prost_types::Duration) -> wire::RemoteDuration {
    wire::RemoteDuration {
        seconds: value.seconds,
        nanos: value.nanos,
    }
}

fn remote_cursor(value: api::heddle::api::v1alpha1::CursorFailure) -> wire::RemoteCursorFailure {
    use api::heddle::api::v1alpha1::cursor_failure::Reason as Api;
    use wire::RemoteCursorReason as Wire;
    let reason = match value.reason() {
        Api::Unspecified => Wire::Unspecified,
        Api::Stale => Wire::Stale,
        Api::Expired => Wire::Expired,
    };
    wire::RemoteCursorFailure {
        reason,
        expired_at: value.expired_at.map(|timestamp| wire::RemoteTimestamp {
            seconds: timestamp.seconds,
            nanos: timestamp.nanos,
        }),
        restart_cursor: value.restart_cursor,
    }
}

fn remote_failure_detail(
    detail: api::heddle::api::v1alpha1::ErrorDetail,
) -> wire::RemoteFailureDetail {
    use api::heddle::api::v1alpha1::error_detail::Context;
    use prost::Message as _;

    let encoded = detail.encode_to_vec();
    match detail.context {
        Some(Context::Retry(value)) => wire::RemoteFailureDetail::Retry {
            retry_after: value.retry_after.map(remote_duration),
        },
        Some(Context::Conflict(value)) => wire::RemoteFailureDetail::Conflict {
            resource: value.resource,
            expected_version: value.expected_version,
            actual_version: value.actual_version,
        },
        Some(Context::Cursor(value)) => wire::RemoteFailureDetail::Cursor(remote_cursor(value)),
        Some(Context::Capability(value)) => wire::RemoteFailureDetail::CapabilityRequirement {
            capabilities: value.capabilities,
        },
        Some(Context::Policy(value)) => wire::RemoteFailureDetail::PolicyDenial {
            policy_id: value.policy_id,
            rule: value.rule,
            human_verification_can_override: value.human_verification_can_override,
        },
        Some(Context::Stream(value)) => remote_stream_failure(*value),
        Some(Context::Unknown(value)) => wire::RemoteFailureDetail::Unknown {
            type_url: value.type_url,
            value: value.value,
        },
        Some(Context::HumanVerification(_))
        | Some(Context::AmbiguousChangeId(_))
        | Some(Context::Signup(_))
        | None => wire::RemoteFailureDetail::Unknown {
            type_url: "type.googleapis.com/heddle.api.v1alpha1.ErrorDetail".to_string(),
            value: encoded,
        },
    }
}

fn remote_stream_failure(
    value: api::heddle::api::v1alpha1::StreamFailure,
) -> wire::RemoteFailureDetail {
    use api::heddle::api::v1alpha1::{CallFailureCode, error_detail::Context};

    let (retry_after, cursor) = match value.error.and_then(|detail| detail.context) {
        Some(Context::Retry(retry)) => (retry.retry_after.map(remote_duration), None),
        Some(Context::Cursor(cursor)) => (None, Some(remote_cursor(cursor))),
        _ => (None, None),
    };
    wire::RemoteFailureDetail::Stream {
        code: remote_failure_code(CallFailureCode::try_from(value.code).unwrap_or_default()),
        message: value.message,
        retry_after,
        cursor,
    }
}

pub(super) fn parse_proto_state_id(
    state_id: Option<ProtoStateId>,
) -> Result<Option<StateId>, ProtocolError> {
    state_id
        .map(|state_id| {
            let value: [u8; 32] = state_id.value.try_into().map_err(|value: Vec<u8>| {
                ProtocolError::InvalidState(format!(
                    "state ID must be 32 bytes, got {}",
                    value.len()
                ))
            })?;
            Ok(StateId::from_bytes(value))
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use api::heddle::api::v1alpha1::{
        CallFailure, CallFailureCode, StateId as ProtoStateId, StreamFailure,
    };

    use super::*;

    #[test]
    fn native_auth_failure_maps_without_transport_status_types() {
        let error = hosted_to_protocol_error(HostedError::Call {
            code: api::heddle::api::v1alpha1::CallFailureCode::Unauthenticated,
            message: "invalid proof".to_string(),
            error: None,
        });
        assert!(matches!(
            error,
            ProtocolError::RemoteFailure {
                code: wire::RemoteFailureCode::Unauthenticated,
                ..
            }
        ));
    }

    #[test]
    fn native_call_failure_preserves_typed_error_detail() {
        use api::heddle::api::v1alpha1::{ConflictDetail, ErrorDetail, ErrorReason, error_detail};

        let error = hosted_to_protocol_error(HostedError::Call {
            code: api::heddle::api::v1alpha1::CallFailureCode::AlreadyExists,
            message: "ref changed".to_string(),
            error: Some(Box::new(ErrorDetail {
                reason: ErrorReason::VersionConflict as i32,
                resource: "refs/heads/main".to_string(),
                field: String::new(),
                context: Some(error_detail::Context::Conflict(ConflictDetail {
                    resource: "refs/heads/main".to_string(),
                    expected_version: "old".to_string(),
                    actual_version: "new".to_string(),
                })),
            })),
        });

        let ProtocolError::RemoteFailure { code, details, .. } = error else {
            panic!("expected remote failure");
        };
        assert_eq!(code, wire::RemoteFailureCode::AlreadyExists);
        assert!(matches!(
            &details[0],
            wire::RemoteFailureDetail::Conflict { resource, .. }
                if resource == "refs/heads/main"
        ));
    }

    #[test]
    fn hosted_to_protocol_error_maps_call_codes_without_detail() {
        use api::heddle::api::v1alpha1::CallFailureCode;

        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::Unauthenticated,
                message: "nope".into(),
                error: None,
            }),
            ProtocolError::RemoteFailure {
                code: wire::RemoteFailureCode::Unauthenticated,
                ..
            }
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::PermissionDenied,
                message: "nope".into(),
                error: None,
            }),
            ProtocolError::AuthorizationFailed(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::NotFound,
                message: "missing".into(),
                error: None,
            }),
            ProtocolError::ObjectNotFound(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::AlreadyExists,
                message: "exists".into(),
                error: None,
            }),
            ProtocolError::AlreadyExists(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::InvalidArgument,
                message: "bad".into(),
                error: None,
            }),
            ProtocolError::InvalidState(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::FailedPrecondition,
                message: "pre".into(),
                error: None,
            }),
            ProtocolError::InvalidState(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Call {
                code: CallFailureCode::Internal,
                message: "boom".into(),
                error: None,
            }),
            ProtocolError::RemoteFailure { .. }
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Transport("down".into())),
            ProtocolError::Io(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::Framing("bad frame".into())),
            ProtocolError::Remote(_)
        ));
        assert!(matches!(
            hosted_to_protocol_error(HostedError::SigningIdentityRequired),
            ProtocolError::Remote(_)
        ));
    }

    #[test]
    fn remote_failure_detail_maps_every_context_variant() {
        use api::heddle::api::v1alpha1::{
            CapabilityRequirement, ConflictDetail, CursorFailure, ErrorDetail, ErrorReason,
            PolicyDenial, RetryAdvice, error_detail,
        };

        let retry = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Retry(RetryAdvice {
                retry_after: Some(prost_types::Duration {
                    seconds: 1,
                    nanos: 0,
                }),
            })),
        });
        assert!(matches!(retry, wire::RemoteFailureDetail::Retry { .. }));

        let conflict = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::VersionConflict as i32,
            resource: "r".into(),
            field: String::new(),
            context: Some(error_detail::Context::Conflict(ConflictDetail {
                resource: "r".into(),
                expected_version: "a".into(),
                actual_version: "b".into(),
            })),
        });
        assert!(matches!(
            conflict,
            wire::RemoteFailureDetail::Conflict { .. }
        ));

        let cursor = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Cursor(CursorFailure {
                reason: 1,
                expired_at: None,
                restart_cursor: "cursor-token".into(),
            })),
        });
        assert!(matches!(cursor, wire::RemoteFailureDetail::Cursor(_)));

        let capability = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Capability(CapabilityRequirement {
                capabilities: vec!["push".into()],
            })),
        });
        assert!(matches!(
            capability,
            wire::RemoteFailureDetail::CapabilityRequirement { .. }
        ));

        let policy = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(error_detail::Context::Policy(PolicyDenial {
                policy_id: "p1".into(),
                rule: "r1".into(),
                human_verification_can_override: true,
            })),
        });
        assert!(matches!(
            policy,
            wire::RemoteFailureDetail::PolicyDenial { .. }
        ));

        let unknown = remote_failure_detail(ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: None,
        });
        assert!(matches!(unknown, wire::RemoteFailureDetail::Unknown { .. }));
    }

    fn decoded_failure_details(failure: &CallFailure) -> Vec<wire::RemoteFailureDetail> {
        use api::framing::{decode_response_frame, encode_failure_response};

        let encoded = encode_failure_response(failure).expect("encode failure frame");
        let api::framing::ResponseFrame::Failure(decoded) =
            decode_response_frame(&encoded).expect("decode failure frame")
        else {
            panic!("expected a failure frame");
        };
        match hosted_to_protocol_error(decoded.into()) {
            ProtocolError::RemoteFailure { details, .. } => details,
            other => panic!("expected remote failure, got {other:?}"),
        }
    }

    #[test]
    fn every_known_failure_detail_arm_survives_heddles_wire_path() {
        use api::heddle::api::v1alpha1::{
            CapabilityRequirement, ConflictDetail, CursorFailure, ErrorDetail, ErrorReason,
            PolicyDenial, RetryAdvice, UnknownDetail, cursor_failure, error_detail,
        };
        use prost::Message as _;

        let detail = |context| ErrorDetail {
            reason: ErrorReason::Unspecified as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(context),
        };
        let failure = |context| CallFailure {
            code: CallFailureCode::FailedPrecondition as i32,
            message: "call failed".to_string(),
            error: Some(detail(context)),
        };

        let retry = failure(error_detail::Context::Retry(RetryAdvice {
            retry_after: Some(prost_types::Duration {
                seconds: 3,
                nanos: 0,
            }),
        }));
        assert_eq!(
            decoded_failure_details(&retry),
            vec![wire::RemoteFailureDetail::Retry {
                retry_after: Some(wire::RemoteDuration {
                    seconds: 3,
                    nanos: 0
                }),
            }]
        );

        let conflict = failure(error_detail::Context::Conflict(ConflictDetail {
            resource: "refs/heads/main".to_string(),
            expected_version: "old".to_string(),
            actual_version: "new".to_string(),
        }));
        assert_eq!(
            decoded_failure_details(&conflict),
            vec![wire::RemoteFailureDetail::Conflict {
                resource: "refs/heads/main".to_string(),
                expected_version: "old".to_string(),
                actual_version: "new".to_string(),
            }]
        );

        let cursor = failure(error_detail::Context::Cursor(CursorFailure {
            reason: cursor_failure::Reason::Stale as i32,
            expired_at: None,
            restart_cursor: "page-42".to_string(),
        }));
        assert_eq!(
            decoded_failure_details(&cursor),
            vec![wire::RemoteFailureDetail::Cursor(
                wire::RemoteCursorFailure {
                    reason: wire::RemoteCursorReason::Stale,
                    expired_at: None,
                    restart_cursor: "page-42".to_string(),
                }
            )]
        );

        let capability = failure(error_detail::Context::Capability(CapabilityRequirement {
            capabilities: vec!["repo.pull".to_string()],
        }));
        assert_eq!(
            decoded_failure_details(&capability),
            vec![wire::RemoteFailureDetail::CapabilityRequirement {
                capabilities: vec!["repo.pull".to_string()],
            }]
        );

        let policy = failure(error_detail::Context::Policy(PolicyDenial {
            policy_id: "retention".to_string(),
            rule: "no-purge".to_string(),
            human_verification_can_override: false,
        }));
        assert_eq!(
            decoded_failure_details(&policy),
            vec![wire::RemoteFailureDetail::PolicyDenial {
                policy_id: "retention".to_string(),
                rule: "no-purge".to_string(),
                human_verification_can_override: false,
            }]
        );

        // An arm from a newer contract version passes through losslessly and
        // its opaque payload still decodes into the original typed message.
        let future_arm = StreamFailure {
            code: CallFailureCode::Internal as i32,
            message: "from the future".to_string(),
            error: None,
        };
        let unknown = failure(error_detail::Context::Unknown(UnknownDetail {
            type_url: "type.googleapis.com/heddle.api.v1alpha1.StreamFailure".to_string(),
            value: future_arm.encode_to_vec(),
        }));
        let details = decoded_failure_details(&unknown);
        let wire::RemoteFailureDetail::Unknown { type_url, value } = &details[0] else {
            panic!("unknown arm must stay unknown, got {:?}", details[0]);
        };
        assert_eq!(details.len(), 1);
        assert_eq!(
            type_url,
            "type.googleapis.com/heddle.api.v1alpha1.StreamFailure"
        );
        assert_eq!(
            &StreamFailure::decode(value.as_slice()).expect("recovered typed payload"),
            &future_arm
        );
    }

    #[test]
    fn stream_failure_round_trips_with_nested_resume_hints() {
        use api::{
            framing::{StreamFrame, decode_stream_frame, encode_stream_failure},
            heddle::api::v1alpha1::{
                CursorFailure, ErrorDetail, ErrorReason, RetryAdvice, cursor_failure, error_detail,
            },
        };

        let hint = |context| ErrorDetail {
            reason: ErrorReason::Transient as i32,
            resource: String::new(),
            field: String::new(),
            context: Some(context),
        };
        let stream_failure = |hint_context| CallFailure {
            code: CallFailureCode::Unavailable as i32,
            message: "pull stream aborted".to_string(),
            error: Some(ErrorDetail {
                reason: ErrorReason::Transient as i32,
                resource: String::new(),
                field: String::new(),
                context: Some(error_detail::Context::Stream(Box::new(StreamFailure {
                    code: CallFailureCode::Internal as i32,
                    message: "pack writer reset".to_string(),
                    error: Some(Box::new(hint(hint_context))),
                }))),
            }),
        };

        let encoded =
            encode_stream_failure(&stream_failure(error_detail::Context::Retry(RetryAdvice {
                retry_after: Some(prost_types::Duration {
                    seconds: 3,
                    nanos: 0,
                }),
            })))
            .expect("encode stream failure");
        let Some((StreamFrame::Failure(decoded), _)) =
            decode_stream_frame(&encoded).expect("decode stream frame")
        else {
            panic!("expected a stream failure frame");
        };
        let ProtocolError::RemoteFailure {
            code,
            message,
            details,
        } = hosted_to_protocol_error(decoded.into())
        else {
            panic!("expected remote failure");
        };
        assert_eq!(code, wire::RemoteFailureCode::Unavailable);
        assert_eq!(message, "pull stream aborted");
        assert_eq!(
            details,
            vec![wire::RemoteFailureDetail::Stream {
                code: wire::RemoteFailureCode::Internal,
                message: "pack writer reset".to_string(),
                retry_after: Some(wire::RemoteDuration {
                    seconds: 3,
                    nanos: 0
                }),
                cursor: None,
            }]
        );

        let encoded = encode_stream_failure(&stream_failure(error_detail::Context::Cursor(
            CursorFailure {
                reason: cursor_failure::Reason::Stale as i32,
                expired_at: None,
                restart_cursor: "page-42".to_string(),
            },
        )))
        .expect("encode stream failure");
        let Some((StreamFrame::Failure(decoded), _)) =
            decode_stream_frame(&encoded).expect("decode stream frame")
        else {
            panic!("expected a stream failure frame");
        };
        let ProtocolError::RemoteFailure { details, .. } = hosted_to_protocol_error(decoded.into())
        else {
            panic!("expected remote failure");
        };
        assert_eq!(
            details,
            vec![wire::RemoteFailureDetail::Stream {
                code: wire::RemoteFailureCode::Internal,
                message: "pack writer reset".to_string(),
                retry_after: None,
                cursor: Some(wire::RemoteCursorFailure {
                    reason: wire::RemoteCursorReason::Stale,
                    expired_at: None,
                    restart_cursor: "page-42".to_string(),
                }),
            }]
        );
    }

    #[test]
    fn reopen_retryable_classifier_matches_weft_v2_signals() {
        use api::heddle::api::v1alpha1::CallFailureCode;
        let aborted = |message: &str| CallFailure {
            code: CallFailureCode::Aborted as i32,
            message: message.into(),
            error: None,
        };
        assert!(thread_api::is_reopen_retryable(&aborted(
            "material authority changed; reopen exact selection"
        )));
        assert!(thread_api::is_reopen_retryable(&aborted(
            "authorization changed"
        )));
        assert!(!thread_api::is_reopen_retryable(&CallFailure {
            code: CallFailureCode::PermissionDenied as i32,
            message: "hidden".into(),
            error: None,
        }));
    }

    #[test]
    fn parse_proto_state_id_requires_32_bytes() {
        let state = StateId::from_bytes([0x55; 32]);
        let proto = ProtoStateId {
            value: state.as_bytes().to_vec(),
        };
        let parsed = parse_proto_state_id(Some(proto))
            .expect("parse")
            .expect("present");
        assert_eq!(parsed, state);
        assert_eq!(parse_proto_state_id(None).expect("none ok"), None);
        let bad = ProtoStateId {
            value: vec![1, 2, 3],
        };
        assert!(parse_proto_state_id(Some(bad)).is_err());
    }
}
