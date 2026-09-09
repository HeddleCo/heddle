//! Lossless typed mention conversion shared by command builders and hosts.
use heddle_object_model::object::{
    CollaborationMention as Mention, CollaborationRecordKind as Kind, ContentHash, StateId,
};
use uuid::Uuid;

use crate::{contract::*, transport::Error};

fn spool(value: Option<&SpoolRef>) -> Result<Uuid, Error> {
    let id = value
        .ok_or(Error::Protocol("mention requires spool"))?
        .id
        .parse::<Uuid>()
        .map_err(|_| Error::Protocol("mention spool must be UUID"))?;
    if id.is_nil() {
        return Err(Error::Protocol("mention spool cannot be nil"));
    }
    Ok(id)
}
fn key(value: &[u8]) -> Result<[u8; 32], Error> {
    value
        .try_into()
        .map_err(|_| Error::Protocol("mention identity must contain 32 bytes"))
}
fn endpoint(value: Option<&EndpointRef>) -> Result<[u8; 32], Error> {
    let endpoint = value.ok_or(Error::Protocol("device mention requires endpoint"))?;
    if endpoint.kind != EndpointKind::Device as i32 {
        return Err(Error::Protocol("device mention must target a device"));
    }
    key(&endpoint.public_key)
}
pub fn mention(value: &EntityRef) -> Result<Mention, Error> {
    use entity_ref::Entity;
    Ok(
        match value
            .entity
            .as_ref()
            .ok_or(Error::Protocol("empty mention"))?
        {
            Entity::Spool(r) => Mention::Spool {
                spool: spool(Some(r))?,
            },
            Entity::Thread(r) => Mention::Thread {
                spool: spool(r.spool.as_ref())?,
                thread: ContentHash::from_bytes(key(&r
                    .id
                    .as_ref()
                    .ok_or(Error::Protocol("Thread mention requires ID"))?
                    .value)?),
            },
            Entity::Checkout(r) => Mention::Checkout {
                spool: spool(r.spool.as_ref())?,
                device: endpoint(r.device.as_ref())?,
                id: r.id.clone(),
            },
            Entity::Revision(r) => match r
                .revision
                .as_ref()
                .ok_or(Error::Protocol("revision mention requires exact revision"))?
            {
                revision_ref::Revision::State(id) => Mention::State {
                    spool: spool(r.spool.as_ref())?,
                    state: StateId::from_bytes(key(&id.value)?),
                },
                revision_ref::Revision::GitCommitOid(oid) => Mention::GitCommit {
                    spool: spool(r.spool.as_ref())?,
                    oid: oid.clone(),
                },
            },
            Entity::Device(r) => Mention::Device {
                key: endpoint(Some(r))?,
            },
            Entity::Discussion(r) => record(r, Kind::Discussion)?,
            Entity::Context(r) => record(r, Kind::Context)?,
            Entity::Operation(r) => record(r, Kind::Operation)?,
            Entity::Run(r) => record(r, Kind::Run)?,
            Entity::Policy(r) => record(r, Kind::Policy)?,
            Entity::Analysis(r) => record(r, Kind::Analysis)?,
            Entity::Invitation(r) => record(r, Kind::Invitation)?,
            Entity::Grant(r) => record(r, Kind::Grant)?,
            Entity::DiscussionTurn(r) => record(r, Kind::DiscussionTurn)?,
            Entity::Review(r) => record(r, Kind::Review)?,
            Entity::Notification(r) => record(r, Kind::Notification)?,
            Entity::AttentionItem(r) => record(r, Kind::AttentionItem)?,
            Entity::Member(r) => record(r, Kind::Member)?,
            Entity::ApprovalGroup(r) => record(r, Kind::ApprovalGroup)?,
            Entity::Session(r) => record(r, Kind::Session)?,
            Entity::SignupInvitation(r) => record(r, Kind::SignupInvitation)?,
            Entity::TimelineEvent(r) => record(r, Kind::TimelineEvent)?,
            Entity::Artifact(r) => record(r, Kind::Artifact)?,
            Entity::Mount(r) => record(r, Kind::Mount)?,
            Entity::SupportAccess(r) => record(r, Kind::SupportAccess)?,
            Entity::DeviceRecord(r) => record(r, Kind::DeviceRecord)?,
            Entity::Delegation(r) => record(r, Kind::Delegation)?,
            Entity::Recovery(r) => record(r, Kind::Recovery)?,
            Entity::OwnerTransition(r) => record(r, Kind::OwnerTransition)?,
            Entity::Bookmark(_) => {
                return Err(Error::Protocol(
                    "bookmarks are account-private; mention their Spool or Thread instead",
                ));
            }
        },
    )
}
fn record(r: &RecordRef, record_kind: Kind) -> Result<Mention, Error> {
    Ok(Mention::Record {
        spool: r.spool.as_ref().map(|r| spool(Some(r))).transpose()?,
        record_kind,
        id: r.id.clone(),
    })
}
fn wire_spool(id: Uuid) -> Option<SpoolRef> {
    Some(SpoolRef { id: id.to_string() })
}
fn wire_device(key: &[u8; 32]) -> EndpointRef {
    EndpointRef {
        kind: EndpointKind::Device as i32,
        public_key: key.to_vec(),
    }
}
pub fn mention_ref(value: &Mention) -> EntityRef {
    use entity_ref::Entity;
    let entity = match value {
        Mention::Spool { spool } => Entity::Spool(SpoolRef {
            id: spool.to_string(),
        }),
        Mention::Thread { spool, thread } => Entity::Thread(ThreadRef {
            spool: wire_spool(*spool),
            id: Some(ThreadId {
                value: thread.as_bytes().to_vec(),
            }),
        }),
        Mention::Checkout { spool, device, id } => Entity::Checkout(CheckoutRef {
            spool: wire_spool(*spool),
            device: Some(wire_device(device)),
            id: id.clone(),
        }),
        Mention::State { spool, state } => Entity::Revision(RevisionRef {
            spool: wire_spool(*spool),
            revision: Some(revision_ref::Revision::State(
                api::heddle::api::v1alpha1::StateId {
                    value: state.as_bytes().to_vec(),
                },
            )),
        }),
        Mention::GitCommit { spool, oid } => Entity::Revision(RevisionRef {
            spool: wire_spool(*spool),
            revision: Some(revision_ref::Revision::GitCommitOid(oid.clone())),
        }),
        Mention::Device { key } => Entity::Device(wire_device(key)),
        Mention::Record {
            spool,
            record_kind,
            id,
        } => {
            let r = RecordRef {
                spool: spool.and_then(wire_spool),
                id: id.clone(),
            };
            match record_kind {
                Kind::Discussion => Entity::Discussion(r),
                Kind::Context => Entity::Context(r),
                Kind::Operation => Entity::Operation(r),
                Kind::Run => Entity::Run(r),
                Kind::Policy => Entity::Policy(r),
                Kind::Analysis => Entity::Analysis(r),
                Kind::Invitation => Entity::Invitation(r),
                Kind::Grant => Entity::Grant(r),
                Kind::DiscussionTurn => Entity::DiscussionTurn(r),
                Kind::Review => Entity::Review(r),
                Kind::Notification => Entity::Notification(r),
                Kind::AttentionItem => Entity::AttentionItem(r),
                Kind::Member => Entity::Member(r),
                Kind::ApprovalGroup => Entity::ApprovalGroup(r),
                Kind::Session => Entity::Session(r),
                Kind::SignupInvitation => Entity::SignupInvitation(r),
                Kind::TimelineEvent => Entity::TimelineEvent(r),
                Kind::Artifact => Entity::Artifact(r),
                Kind::Mount => Entity::Mount(r),
                Kind::SupportAccess => Entity::SupportAccess(r),
                Kind::DeviceRecord => Entity::DeviceRecord(r),
                Kind::Delegation => Entity::Delegation(r),
                Kind::Recovery => Entity::Recovery(r),
                Kind::OwnerTransition => Entity::OwnerTransition(r),
            }
        }
    };
    EntityRef {
        entity: Some(entity),
    }
}

/// Bind an API anchor to the canonical scope before signing or admitting it.
pub fn anchor(
    value: &CollaborationAnchor,
    scope: &heddle_object_model::object::CollaborationScope,
) -> Result<heddle_object_model::object::CollaborationAnchor, Error> {
    use heddle_object_model::object::{
        CollaborationAnchor as Anchor, CollaborationRevision, CollaborationSourceAnchor,
    };
    fn thread(
        value: &ThreadRef,
        scope: &heddle_object_model::object::CollaborationScope,
    ) -> Result<(), Error> {
        if spool(value.spool.as_ref())? != scope.spool
            || Some(ContentHash::from_bytes(key(&value
                .id
                .as_ref()
                .ok_or(Error::Protocol("anchor Thread requires ID"))?
                .value)?))
                != scope.thread
        {
            return Err(Error::Protocol("anchor belongs to another Thread or spool"));
        }
        Ok(())
    }
    Ok(
        match value
            .target
            .as_ref()
            .ok_or(Error::Protocol("collaboration anchor required"))?
        {
            collaboration_anchor::Target::Thread(value) => {
                thread(value, scope)?;
                Anchor::Repository
            }
            collaboration_anchor::Target::Spool(value) => {
                if scope.thread.is_some() || spool(Some(value))? != scope.spool {
                    return Err(Error::Protocol(
                        "spool anchor requires independent spool scope",
                    ));
                }
                Anchor::Repository
            }
            collaboration_anchor::Target::Source(value) => {
                thread(
                    value
                        .thread
                        .as_ref()
                        .ok_or(Error::Protocol("source anchor requires owning Thread"))?,
                    scope,
                )?;
                let revision = value
                    .revision
                    .as_ref()
                    .ok_or(Error::Protocol("source anchor requires exact revision"))?;
                if spool(revision.spool.as_ref())? != scope.spool {
                    return Err(Error::Protocol("source revision belongs to another spool"));
                }
                let revision = match revision
                    .revision
                    .as_ref()
                    .ok_or(Error::Protocol("exact revision required"))?
                {
                    revision_ref::Revision::State(id) => CollaborationRevision::State {
                        state_id: StateId::from_bytes(key(&id.value)?),
                    },
                    revision_ref::Revision::GitCommitOid(oid) => {
                        CollaborationRevision::GitCommit { oid: oid.clone() }
                    }
                };
                Anchor::Source {
                    source: CollaborationSourceAnchor {
                        revision,
                        path: value.path.clone(),
                        symbol_id: value.symbol_id.clone(),
                        start_line: value.start_line,
                        end_line: value.end_line,
                    },
                }
            }
        },
    )
}

/// Native audiences map exactly to canonical tiers. An omitted audience must
/// never silently turn a private record into public data.
pub fn visibility(
    audience: i32,
    label: &str,
) -> Result<heddle_object_model::object::VisibilityTier, Error> {
    use heddle_object_model::object::VisibilityTier as Tier;
    match Audience::try_from(audience) {
        Ok(Audience::Public) if label.is_empty() => Ok(Tier::Public),
        Ok(Audience::Members) if label.is_empty() => Ok(Tier::Internal),
        Ok(Audience::Private)
            if !label.trim().is_empty()
                && label.len() <= 512
                && !label.chars().any(char::is_control) =>
        {
            Ok(Tier::Private {
                scope_label: label.into(),
            })
        }
        _ => Err(Error::Protocol(
            "explicit audience and matching private label required",
        )),
    }
}
/// None means this canonical tier has no lossless native audience projection;
/// callers retain its original signed record and original visibility guard.
pub fn audience(tier: &heddle_object_model::object::VisibilityTier) -> Option<(Audience, String)> {
    use heddle_object_model::object::VisibilityTier as Tier;
    match tier {
        Tier::Public => Some((Audience::Public, String::new())),
        Tier::Internal => Some((Audience::Members, String::new())),
        Tier::Private { scope_label } => Some((Audience::Private, scope_label.clone())),
        Tier::TeamScoped { .. } | Tier::Restricted { .. } => None,
    }
}

/// Project source location and ownership without discarding path, symbol or
/// line information. Change anchors require an exact source revision first.
pub fn anchor_ref(
    value: &heddle_object_model::object::CollaborationAnchor,
    scope: &heddle_object_model::object::CollaborationScope,
) -> Result<CollaborationAnchor, Error> {
    use heddle_object_model::object::{
        CollaborationAnchor as Anchor, CollaborationRevision as Revision,
    };
    let thread = scope.thread.map(|id| ThreadRef {
        spool: wire_spool(scope.spool),
        id: Some(ThreadId {
            value: id.as_bytes().to_vec(),
        }),
    });
    let source = match value {
        Anchor::Repository => {
            return Ok(CollaborationAnchor {
                target: Some(match thread {
                    Some(thread) => collaboration_anchor::Target::Thread(thread),
                    None => collaboration_anchor::Target::Spool(SpoolRef {
                        id: scope.spool.to_string(),
                    }),
                }),
            });
        }
        Anchor::Source { source } => SourceAnchor {
            revision: Some(RevisionRef {
                spool: wire_spool(scope.spool),
                revision: Some(match &source.revision {
                    Revision::State { state_id } => {
                        revision_ref::Revision::State(api::heddle::api::v1alpha1::StateId {
                            value: state_id.as_bytes().to_vec(),
                        })
                    }
                    Revision::GitCommit { oid } => {
                        revision_ref::Revision::GitCommitOid(oid.clone())
                    }
                }),
            }),
            path: source.path.clone(),
            symbol_id: source.symbol_id.clone(),
            start_line: source.start_line,
            end_line: source.end_line,
            thread,
        },
        Anchor::State { state_id }
        | Anchor::Path { state_id, .. }
        | Anchor::Symbol { state_id, .. } => SourceAnchor {
            revision: Some(RevisionRef {
                spool: wire_spool(scope.spool),
                revision: Some(revision_ref::Revision::State(
                    api::heddle::api::v1alpha1::StateId {
                        value: state_id.as_bytes().to_vec(),
                    },
                )),
            }),
            path: match value {
                Anchor::Path { path, .. } | Anchor::Symbol { path, .. } => path.clone(),
                _ => String::new(),
            },
            symbol_id: match value {
                Anchor::Symbol { symbol, .. } => symbol.clone(),
                _ => String::new(),
            },
            start_line: None,
            end_line: None,
            thread,
        },
        Anchor::Change { .. } => {
            return Err(Error::Protocol(
                "change anchor requires exact source revision",
            ));
        }
    };
    Ok(CollaborationAnchor {
        target: Some(collaboration_anchor::Target::Source(source)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_native_mention_retains_kind_scope_and_identity() {
        let spool = Uuid::from_u128(1);
        let mut mentions = vec![
            Mention::Spool { spool },
            Mention::Thread {
                spool,
                thread: ContentHash::from_bytes([2; 32]),
            },
            Mention::State {
                spool,
                state: StateId::from_bytes([3; 32]),
            },
            Mention::GitCommit {
                spool,
                oid: "a".repeat(40),
            },
            Mention::Checkout {
                spool,
                device: [4; 32],
                id: "checkout".into(),
            },
            Mention::Device { key: [5; 32] },
        ];
        for record_kind in [
            Kind::Discussion,
            Kind::Context,
            Kind::Operation,
            Kind::Run,
            Kind::Policy,
            Kind::Analysis,
            Kind::Invitation,
            Kind::Grant,
            Kind::DiscussionTurn,
            Kind::Review,
            Kind::Notification,
            Kind::AttentionItem,
            Kind::Member,
            Kind::ApprovalGroup,
            Kind::Session,
            Kind::SignupInvitation,
            Kind::TimelineEvent,
            Kind::Artifact,
            Kind::Mount,
            Kind::SupportAccess,
            Kind::DeviceRecord,
            Kind::Delegation,
            Kind::Recovery,
            Kind::OwnerTransition,
        ] {
            for spool in [None, Some(spool)] {
                mentions.push(Mention::Record {
                    spool,
                    record_kind,
                    id: "record".into(),
                });
            }
        }
        assert_eq!(mentions.len(), 54);
        for original in mentions {
            assert_eq!(
                mention(&mention_ref(&original)).expect("typed mention"),
                original
            );
        }
        assert!(
            mention(&EntityRef {
                entity: Some(entity_ref::Entity::Device(EndpointRef {
                    kind: EndpointKind::Weft as i32,
                    public_key: vec![5; 32]
                }))
            })
            .is_err()
        );
    }
    #[test]
    fn source_anchor_binds_owning_thread_and_preserves_lines() {
        use heddle_object_model::object::{CollaborationAnchor as Anchor, CollaborationScope};
        let scope = CollaborationScope {
            spool: Uuid::from_u128(1),
            thread: Some(ContentHash::from_bytes([2; 32])),
        };
        let thread = ThreadRef {
            spool: wire_spool(scope.spool),
            id: Some(ThreadId { value: vec![2; 32] }),
        };
        let mut source = SourceAnchor {
            revision: Some(RevisionRef {
                spool: wire_spool(scope.spool),
                revision: Some(revision_ref::Revision::GitCommitOid("a".repeat(40))),
            }),
            path: "src/main.rs".into(),
            symbol_id: "run".into(),
            start_line: Some(12),
            end_line: Some(18),
            thread: Some(thread),
        };
        let value = CollaborationAnchor {
            target: Some(collaboration_anchor::Target::Source(source.clone())),
        };
        let Anchor::Source { source: actual } =
            anchor(&value, &scope).expect("bound source anchor")
        else {
            panic!("source");
        };
        assert_eq!(
            (
                actual.path.as_str(),
                actual.symbol_id.as_str(),
                actual.start_line,
                actual.end_line
            ),
            ("src/main.rs", "run", Some(12), Some(18))
        );
        assert_eq!(
            anchor_ref(&Anchor::Source { source: actual }, &scope).expect("source projection"),
            value,
            "source fields survive both directions"
        );
        source
            .thread
            .as_mut()
            .expect("Thread")
            .id
            .as_mut()
            .expect("ID")
            .value[0] ^= 1;
        assert!(
            anchor(
                &CollaborationAnchor {
                    target: Some(collaboration_anchor::Target::Source(source))
                },
                &scope
            )
            .is_err(),
            "source must not escape the Thread"
        );
    }
    #[test]
    fn audience_mapping_is_explicit_and_lossless() {
        use heddle_object_model::object::VisibilityTier as Tier;
        for (kind, label) in [
            (Audience::Public, ""),
            (Audience::Members, ""),
            (Audience::Private, "review-team"),
        ] {
            let tier = visibility(kind as i32, label).expect("valid audience");
            assert_eq!(audience(&tier), Some((kind, label.into())));
        }
        for (kind, label) in [
            (Audience::Unspecified, ""),
            (Audience::Private, ""),
            (Audience::Public, "review-team"),
            (Audience::Members, "review-team"),
        ] {
            assert!(
                visibility(kind as i32, label).is_err(),
                "must not change audience or fabricate label"
            );
        }
        assert!(
            audience(&Tier::TeamScoped {
                team_id: "team".into()
            })
            .is_none()
        );
        assert!(
            audience(&Tier::Restricted {
                scope_label: "restricted".into()
            })
            .is_none()
        );
    }
}
