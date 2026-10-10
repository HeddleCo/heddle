// SPDX-License-Identifier: Apache-2.0
#![cfg(feature = "gateway-publication")]
//! Local-only structural receiver and publication binding adversarial tests.
//! Fixture account envelopes never establish real authority or enrollment.
#[path = "gateway_publication/support.rs"]
mod support;
use api::v2::{
    MethodDescriptor,
    client::{Client, MessageReader, MessageWriter, RpcTransport},
};
use heddle_git_projection::{
    GitProjectionError,
    gateway_publication::{HistoryBudget, PreparedHistory},
};
use objects::object::ContentHash;
use prost::Message;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use support::*;
use thread_api::{Remote, contract::*, transport::Error};

fn plan(f: &Fixture) -> PreparedHistory {
    PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget::default(),
        |_| Ok(()),
    )
    .expect("offline preparation")
}

#[test]
fn whole_history_metadata_budget_bounds_duplicated_causal_proposals() {
    let f = fixture(20);
    let input = f.genesis.encoded_len()
        + f.originals
            .iter()
            .map(|s| s.canonical.len() + s.signature.len() + 512)
            .sum::<usize>();
    let result = PreparedHistory::prepare(
        &remote(),
        &f.store,
        f.selection(),
        f.scope(),
        f.root.path(),
        HistoryBudget {
            metadata_bytes: input,
            ..HistoryBudget::default()
        },
        |_| Ok(()),
    );
    let error = result
        .err()
        .expect("individually fitting inputs cannot bypass cumulative retained proposal budget");
    assert!(error.to_string().contains("metadata"), "{error}");
    assert_eq!(
        std::fs::read_dir(f.root.path()).expect("scratch").count(),
        1,
        "failed packs are discarded"
    );
}

#[test]
fn deterministic_child_command_identity_changes_with_endpoint_policy_and_spool_scope() {
    let f = fixture(20);
    let original = plan(&f);
    let ids = |p: &PreparedHistory| {
        p.revisions()
            .iter()
            .map(|r| r.publication().opening().client_operation_id.clone())
            .collect::<Vec<_>>()
    };
    for changed in [
        "source",
        "destination",
        "policy",
        "spool-genesis",
        "parent-command",
    ] {
        let mut scope = f.scope();
        let mut endpoint = remote();
        match changed {
            "source" => scope.source.public_key[0] ^= 1,
            "destination" => {
                endpoint
                    .description
                    .endpoint
                    .as_mut()
                    .expect("endpoint")
                    .public_key[0] ^= 1
            }
            "policy" => scope.sharing_policy = ContentHash::compute(b"different policy"),
            "spool-genesis" => {
                scope.spool_genesis =
                    ContentHash::compute(b"different independently selected spool genesis")
            }
            "parent-command" => {
                scope.command = "22222222-2222-2222-2222-222222222222"
                    .parse()
                    .expect("command")
            }
            _ => unreachable!(),
        }
        let changed_plan = PreparedHistory::prepare(
            &endpoint,
            &f.store,
            f.selection(),
            scope,
            f.root.path(),
            HistoryBudget::default(),
            |_| Ok(()),
        )
        .expect("distinct offline scope");
        assert_ne!(ids(&original), ids(&changed_plan), "{changed}");
    }
}

#[tokio::test]
async fn changed_remote_endpoint_and_revoked_disclosure_never_send_rpc() {
    let f = fixture(20);
    let plan = plan(&f);
    let revision = &plan.revisions()[0];
    let mut wrong = remote();
    wrong
        .description
        .endpoint
        .as_mut()
        .expect("endpoint")
        .public_key[0] ^= 1;
    assert!(
        revision.send(&wrong, |_| Ok(())).await.is_err(),
        "NoNetwork transport panics if destination mismatch reaches RPC"
    );
    let error = revision
        .send(&remote(), |_| {
            Err::<(), _>(GitProjectionError::Git(
                "current history disclosure revoked".into(),
            ))
        })
        .await
        .expect_err("revoked sender cannot disclose already prepared bytes");
    assert!(error.to_string().contains("disclosure revoked"), "{error}");
}

#[tokio::test]
async fn receiver_cannot_substitute_spool_genesis_or_another_revision_pack() {
    let f = fixture(20);
    assert_ne!(
        f.blobs[0].hash(),
        f.blobs[1].hash(),
        "substituted pack really contains different source bytes"
    );
    let plan = plan(&f);
    let first = &plan.revisions()[0];
    let second = &plan.revisions()[1];
    let error = first
        .validate_received(
            received(first.source()).await,
            ContentHash::compute(b"wrong Spool genesis"),
        )
        .err()
        .expect("receiver intent cannot change");
    assert!(error.to_string().contains("Spool genesis"), "{error}");
    assert!(
        first
            .validate_received(received(second.source()).await, f.spool_genesis())
            .is_err(),
        "a genuine other history pack cannot substitute for selected revision"
    );
}

struct ReplayTransport {
    reply: Vec<u8>,
    exchanges: Arc<AtomicUsize>,
    guard: Arc<AtomicBool>,
}
struct ReplayReader(Option<Vec<u8>>);
struct ReplayWriter;
impl MessageReader for ReplayReader {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.take())
    }
    fn cancel(&mut self) {}
}
impl MessageWriter for ReplayWriter {
    type Error = Error;
    async fn send(&mut self, _: Vec<u8>) -> Result<(), Error> {
        panic!("replay should not upload artifacts")
    }
    async fn finish(&mut self) -> Result<(), Error> {
        Ok(())
    }
    fn abort(&mut self) {}
}
impl RpcTransport for ReplayTransport {
    type Error = Error;
    type Reader = ReplayReader;
    type Writer = ReplayWriter;
    async fn unary(&self, _: &'static MethodDescriptor, _: Vec<u8>) -> Result<Vec<u8>, Error> {
        panic!("unexpected unary")
    }
    async fn observe(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<Self::Reader, Error> {
        panic!("unexpected observation")
    }
    async fn exchange(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<(Self::Writer, Self::Reader), Error> {
        assert!(
            self.guard.load(Ordering::SeqCst),
            "fresh disclosure guard must remain live across exchange"
        );
        self.exchanges.fetch_add(1, Ordering::SeqCst);
        Ok((ReplayWriter, ReplayReader(Some(self.reply.clone()))))
    }
}
struct Guard(Arc<AtomicBool>);
impl Drop for Guard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn replay_receipts_are_bound_to_exact_command_destination_revision_policy_and_inventory() {
    let f = fixture(20);
    let plan = plan(&f);
    let revision = &plan.revisions()[0];
    let Some(publish_content_client_frame::Body::Open(open)) =
        &revision.publication().opening().body
    else {
        panic!("Open")
    };
    let valid = PublicationReceipt {
        client_operation_id: revision.publication().opening().client_operation_id.clone(),
        destination: open.destination.clone(),
        thread: open.thread.clone(),
        revision: open.revision.clone(),
        sharing_policy_version: open.sharing_policy_version.clone(),
        accepted_inventory: Some(ObjectAddress {
            algorithm: "blake3".into(),
            digest: revision
                .source()
                .inventory_digest()
                .expect("inventory")
                .to_vec(),
        }),
        outcome: Some(publication_receipt::Outcome::Accepted(Applied::default())),
        ..Default::default()
    };
    for mutation in [
        "valid",
        "command",
        "destination",
        "thread",
        "revision",
        "policy",
        "inventory",
        "outcome",
    ] {
        let mut receipt = valid.clone();
        match mutation {
            "valid" => {}
            "command" => receipt.client_operation_id = "other-command".into(),
            "destination" => receipt.destination.as_mut().expect("endpoint").public_key[0] ^= 1,
            "thread" => {
                receipt
                    .thread
                    .as_mut()
                    .expect("thread")
                    .id
                    .as_mut()
                    .expect("id")
                    .value[0] ^= 1
            }
            "revision" => receipt.revision = None,
            "policy" => receipt.sharing_policy_version[0] ^= 1,
            "inventory" => {
                receipt
                    .accepted_inventory
                    .as_mut()
                    .expect("inventory")
                    .digest[0] ^= 1
            }
            "outcome" => receipt.outcome = None,
            _ => unreachable!(),
        }
        let exchanges = Arc::new(AtomicUsize::new(0));
        let guard = Arc::new(AtomicBool::new(false));
        let reply = PublishContentServerFrame {
            body: Some(publish_content_server_frame::Body::Receipt(receipt)),
        }
        .encode_to_vec();
        let replay = Remote {
            api: Client::new(
                ReplayTransport {
                    reply,
                    exchanges: exchanges.clone(),
                    guard: guard.clone(),
                },
                ["/heddle.api.v1alpha2.SyncService/PublishContent".into()],
            ),
            description: remote().description,
        };
        let result = revision
            .send(&replay, |prepared| {
                assert_eq!(prepared.opening(), revision.publication().opening());
                guard.store(true, Ordering::SeqCst);
                Ok(Guard(guard.clone()))
            })
            .await;
        assert_eq!(result.is_ok(), mutation == "valid", "{mutation}");
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        assert!(
            !guard.load(Ordering::SeqCst),
            "guard drops only after completed exchange"
        );
    }
}
