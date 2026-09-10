//! Native immutable check admission reuses the canonical evidence and capability engines.
use anyhow::{Context, Result, ensure};
use api::heddle::api::v2alpha1::*;
use objects::object::{ContentHash, OperationId};
use prost::Message;
use thread_api::evidence::{self as codec, CheckAuthor, CheckEvidence};

use super::{DeviceRpc, account_auth::AccountSession, auth::Session, checkout};
const RECORD: &str = "/heddle.api.v2alpha1.EvidenceService/RecordEvidence";
const ACK: &str = "/heddle.api.v2alpha1.EvidenceService/AcknowledgeCheck";

impl DeviceRpc {
    pub(super) fn evidence_command(
        &self,
        session: &Session,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let repository = repo::Repository::open(&session.spool.root)?;
        let namespace = session.command_namespace()?;
        let now = chrono::Utc::now().timestamp();
        let authority = repo::device_authority::load(&self.home, now)?;
        if method == RECORD {
            let request = RecordEvidenceRequest::decode(body)?;
            let supplied = request.evidence.context("evidence required")?;
            let record = supplied
                .evidence
                .as_ref()
                .context("original signed evidence required")?;
            let value = codec::verify_evidence(record)?;
            validate_projection(&supplied, &value)?;
            checkout::same_spool(
                session,
                supplied.r#ref.as_ref().and_then(|r| r.spool.as_ref()),
            )?;
            authorize_origin(session, &repository, value.thread, value.revision)?;
            if !value.artifacts.is_empty()
                && repo::device_evidence::get(&session.spool.heddle_dir, 1, value.id)?.is_none()
            {
                ensure!(
                    session.permits("/heddle.api.v2alpha1.ContentService/ReadArtifact"),
                    "referencing retained artifacts requires artifact read capability"
                );
                let artifacts =
                    repo::device_artifacts::ArtifactStore::open(&session.spool.heddle_dir)?;
                for id in &value.artifacts {
                    artifacts.current(
                        &RecordRef {
                            spool: Some(SpoolRef {
                                id: value.spool.to_string(),
                            }),
                            id: id.to_string(),
                        },
                        now,
                    )?;
                }
            }
            let version = codec::record_version(record);
            let original = record.encode_to_vec();
            repo::device_evidence::accept(
                &session.spool.heddle_dir,
                repo::device_evidence::Command {
                    namespace: &namespace,
                    id: request.client_operation_id.parse::<OperationId>()?,
                    method,
                    request_hash: *blake3::hash(body).as_bytes(),
                },
                repo::device_evidence::Original {
                    kind: 1,
                    id: value.id,
                    thread: value.thread,
                    revision: value.revision,
                    bytes: &original,
                    version,
                },
                |connection, admitted| {
                    if !admitted {
                        original_author(
                            &authority,
                            &value.author,
                            RECORD,
                            &session.spool.capability_path,
                            now,
                        )?;
                    }
                    for previous in &value.supersedes {
                        let bytes = repo::device_evidence::get_in(connection, 1, *previous)?
                            .context("superseded result has not been admitted")?;
                        let previous =
                            codec::verify_evidence(&SignedRecord::decode(bytes.as_slice())?)?;
                        ensure!(
                            previous.spool == value.spool
                                && previous.thread == value.thread
                                && previous.revision == value.revision
                                && previous.check == value.check
                                && previous.author.actor == value.author.actor,
                            "superseded evidence must retain original actor, check and exact revision"
                        );
                    }
                    Ok(())
                },
                || {
                    Ok(self
                        .evidence_receipt(
                            &request.client_operation_id,
                            entity_ref::Entity::Evidence(
                                supplied.r#ref.clone().context("evidence ref")?,
                            ),
                            version,
                        )
                        .encode_to_vec())
                },
            )
        } else {
            ensure!(method == ACK, "unknown evidence mutation");
            let request = AcknowledgeCheckRequest::decode(body)?;
            let record = request
                .acknowledgement
                .as_ref()
                .context("original acknowledgement required")?;
            let value = codec::verify_acknowledgement(record)?;
            let reference = request
                .evidence
                .as_ref()
                .context("evidence identity required")?;
            checkout::same_spool(session, reference.spool.as_ref())?;
            ensure!(
                value.spool == session.spool.id
                    && reference.id == value.evidence.to_string()
                    && request.client_operation_id == value.client_operation_id.to_string()
                    && checkout::revision(session, request.revision.as_ref())? == value.revision
                    && request.policy_version == value.policy_version.as_bytes(),
                "acknowledgement request differs from original signed bindings"
            );
            ensure!(
                super::land::policy_version(&repository)? == value.policy_version,
                "native review policy changed"
            );
            let evidence_bytes =
                repo::device_evidence::get(&session.spool.heddle_dir, 1, value.evidence)?
                    .context("acknowledged evidence has not been admitted")?;
            let acknowledged =
                codec::verify_evidence(&SignedRecord::decode(evidence_bytes.as_slice())?)?;
            authorize_origin(session, &repository, acknowledged.thread, value.revision)?;
            let original = record.encode_to_vec();
            let version = codec::record_version(record);
            repo::device_evidence::accept(
                &session.spool.heddle_dir,
                repo::device_evidence::Command {
                    namespace: &namespace,
                    id: request.client_operation_id.parse::<OperationId>()?,
                    method,
                    request_hash: *blake3::hash(body).as_bytes(),
                },
                repo::device_evidence::Original {
                    kind: 2,
                    id: value.client_operation_id,
                    thread: acknowledged.thread,
                    revision: value.revision,
                    bytes: &original,
                    version,
                },
                |connection, admitted| {
                    if !admitted {
                        original_author(
                            &authority,
                            &value.author,
                            ACK,
                            &session.spool.capability_path,
                            now,
                        )?;
                    }
                    let bytes = repo::device_evidence::get_in(connection, 1, value.evidence)?
                        .context("acknowledged evidence has not been admitted")?;
                    let evidence =
                        codec::verify_evidence(&SignedRecord::decode(bytes.as_slice())?)?;
                    ensure!(
                        evidence.spool == value.spool
                            && evidence.revision == value.revision
                            && evidence.id()? == value.evidence_digest,
                        "acknowledgement must bind exact admitted evidence and revision"
                    );
                    Ok(())
                },
                || {
                    Ok(self
                        .evidence_receipt(
                            &request.client_operation_id,
                            entity_ref::Entity::CheckAcknowledgement(RecordRef {
                                spool: Some(SpoolRef {
                                    id: value.spool.to_string(),
                                }),
                                id: value.client_operation_id.to_string(),
                            }),
                            version,
                        )
                        .encode_to_vec())
                },
            )
        }
    }
    fn evidence_receipt(
        &self,
        id: &str,
        entity: entity_ref::Entity,
        version: ContentHash,
    ) -> MutationResponse {
        let mut receipt = self.receipt(id);
        receipt.outcome = Some(mutation_receipt::Outcome::Applied(Applied {
            resulting_versions: vec![ExpectedVersion {
                resource: Some(EntityRef {
                    entity: Some(entity),
                }),
                version: version.as_bytes().to_vec(),
            }],
        }));
        MutationResponse {
            receipt: Some(receipt),
        }
    }
    pub(super) fn verify_local_evidence(
        &self,
        session: &AccountSession,
        body: &[u8],
    ) -> Result<Vec<u8>> {
        let request = VerifyEvidenceRequest::decode(body)?;
        ensure!(
            !request.evidence.is_empty()
                && request.evidence.len() <= 32
                && body.len() <= 2 * 1024 * 1024,
            "evidence verification batch bound"
        );
        let now = chrono::Utc::now().timestamp();
        let authority = repo::device_authority::load(&self.home, now)?;
        let mut scopes = std::collections::BTreeSet::new();
        let mut results = Vec::new();
        for supplied in request.evidence {
            let record = supplied
                .evidence
                .as_ref()
                .context("original signed evidence required")?;
            let value = codec::verify_evidence(record)?;
            validate_projection(&supplied, &value)?;
            scopes.insert(value.spool);
            ensure!(scopes.len() <= 8, "evidence verification Spool bound");
            let spool = repo::device_catalog::load(&self.home, value.spool)?;
            let facts = session.facts(Some(&spool.capability_path))?;
            let repository = repo::Repository::open(&spool.root)?;
            let replica =
                repo::thread_replication::ThreadReplica::open(&spool.heddle_dir, value.thread)?;
            ensure!(
                replica.genesis()?.spool == value.spool.to_string(),
                "evidence Thread Spool mismatch"
            );
            ensure!(
                super::auth::thread_visible(
                    &repository,
                    &replica,
                    uuid::Uuid::parse_str(&session.principal)?,
                    facts.delegation_agent_id.as_deref()
                )?,
                "evidence origin Thread is not audience-authorized"
            );
            ensure!(
                replica.genesis()?.base == value.revision
                    || replica.accepted_source_revision(value.revision)?.is_some(),
                "evidence revision does not belong to its origin Thread"
            );
            let accepted = repo::device_evidence::get(&spool.heddle_dir, 1, value.id)?;
            let verified = match accepted {
                Some(bytes) => bytes == record.encode_to_vec(),
                None => original_author(
                    &authority,
                    &value.author,
                    RECORD,
                    &spool.capability_path,
                    now,
                )
                .is_ok(),
            };
            results.push(EvidenceVerification {
                evidence: supplied.r#ref,
                verified,
                requirements: if verified {
                    Vec::new()
                } else {
                    vec![Requirement {
                        kind: RequirementKind::Evidence as i32,
                        explanation:
                            "Original evidence is neither exactly admitted nor currently authorized"
                                .into(),
                        ..Default::default()
                    }]
                },
            });
        }
        Ok(VerifyEvidenceResponse { results }.encode_to_vec())
    }
}
fn authorize_origin(
    session: &Session,
    repository: &repo::Repository,
    thread: ContentHash,
    revision: objects::object::StateId,
) -> Result<()> {
    let replica = repo::thread_replication::ThreadReplica::open(&session.spool.heddle_dir, thread)?;
    session.authorize_thread(repository, &replica)?;
    ensure!(
        replica.genesis()?.base == revision
            || replica.accepted_source_revision(revision)?.is_some(),
        "evidence revision does not belong to its origin Thread"
    );
    session.authorize_revision(repository, revision)?;
    session.bind_thread(thread)?;
    Ok(())
}
fn validate_projection(supplied: &EvidenceRecord, value: &CheckEvidence) -> Result<()> {
    let record = supplied
        .evidence
        .as_ref()
        .context("original evidence required")?;
    let projected = codec::project(record)?;
    ensure!(
        supplied.r#ref == projected.r#ref
            && supplied.thread == projected.thread
            && supplied.revision == projected.revision
            && supplied.check == projected.check
            && supplied
                .summary
                .as_ref()
                .is_none_or(|summary| *summary == codec::summary(value))
            && (supplied.version.is_empty() || supplied.version == projected.version),
        "evidence projection differs from verified original"
    );
    Ok(())
}
fn original_author(
    authority: &repo::device_authority::DeviceAuthority,
    author: &CheckAuthor,
    method: &str,
    path: &str,
    now: i64,
) -> Result<()> {
    repo::device_evidence::verify_original_author(authority, author, method, path, now)
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct EvidenceCursor {
    source: usize,
    after: Option<(i32, uuid::Uuid)>,
}
impl DeviceRpc {
    pub(super) fn thread_evidence_snapshot(
        &self,
        session: &Session,
        overview: &ThreadOverview,
        page: &PageRequest,
        budget: &ReadBudget,
        binding: &[u8],
    ) -> Result<(Vec<(String, ThreadEvent)>, PageInfo)> {
        let sources = if overview.source_heads.is_empty() {
            overview.base.iter().collect::<Vec<_>>()
        } else {
            overview.source_heads.iter().collect()
        };
        ensure!(sources.len() <= 128, "evidence source head bound");
        let mut cursor: EvidenceCursor =
            super::account_observe::decode_page(&page.after_page, binding, b"evidence")?
                .unwrap_or_default();
        ensure!(cursor.source <= sources.len(), "evidence page source bound");
        let limit = super::account_observe::page_size(page, budget).min(256);
        let mut events = Vec::new();
        let mut bytes_used = 0usize;
        while cursor.source < sources.len() && events.len() < limit {
            let revision = checkout::revision(session, Some(sources[cursor.source]))?;
            let remaining = limit - events.len();
            let mut rows = repo::device_evidence::page(
                &session.spool.heddle_dir,
                checkout::thread(session, overview.r#ref.as_ref())?,
                revision,
                cursor.after,
                remaining + 1,
            )?;
            let exhausted = rows.len() <= remaining;
            rows.truncate(remaining);
            for (kind, id, bytes) in rows {
                cursor.after = Some((kind, id));
                let record = SignedRecord::decode(bytes.as_slice())?;
                let payload = if kind == 1 {
                    thread_event::Payload::Evidence(codec::project(&record)?)
                } else {
                    thread_event::Payload::CheckAcknowledgement(codec::project_acknowledgement(
                        &record,
                    )?)
                };
                let event = ThreadEvent {
                    frame: None,
                    payload: Some(payload),
                };
                bytes_used = bytes_used
                    .checked_add(event.encoded_len())
                    .context("evidence snapshot byte overflow")?;
                ensure!(
                    event.encoded_len() <= budget.max_frame_bytes as usize
                        && bytes_used <= budget.max_snapshot_bytes as usize,
                    "evidence snapshot budget exhausted; request a smaller page"
                );
                events.push((format!("check:{kind}:{id}"), event));
            }
            if exhausted {
                cursor.source += 1;
                cursor.after = None;
            } else {
                break;
            }
        }
        let exhausted = cursor.source == sources.len();
        let next_page = if exhausted {
            Vec::new()
        } else {
            super::account_observe::encode_page(&cursor, binding, b"evidence")?
        };
        Ok((
            events,
            PageInfo {
                exhausted,
                next_page,
                ..Default::default()
            },
        ))
    }
}
