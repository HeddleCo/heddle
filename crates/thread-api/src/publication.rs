// SPDX-License-Identifier: Apache-2.0
//! Flow-controlled source publication. Sending and receiving run together so
//! server checkpoints cannot block an upload on a full response stream.
use api::v2::client::{ClientError, RpcTransport};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::{Remote, contract::*, rpc, transport};

#[cfg(feature = "source-transfer")]
mod source;
#[cfg(feature = "source-transfer")]
mod staging;
#[cfg(feature = "source-transfer")]
pub use staging::validate_source_artifacts;
#[cfg(feature = "source-transfer")]
pub use source::{PublicationOptions, SourceBudget, SourcePack};

/// Exact original creator wrappers and signed source operations, including
/// every foreign integration dependency. The receiver verifies their authority
/// and complete causal/source closure before any replica admission.
#[derive(Clone)]
pub struct PublicationOriginals {
    pub geneses: Vec<ThreadGenesisRecord>,
    pub operations: Vec<SignedRecord>,
}
impl PublicationOriginals {
    fn validate_bounds(&self) -> Result<(), Error> {
        if self.geneses.is_empty()
            || self.geneses.len() > 128
            || self.operations.is_empty()
            || self.operations.len() > 10_000
        {
            return Err(Error::Invalid(
                "bounded original genesis and source operations required",
            ));
        }
        let mut bytes = 0usize;
        for length in self
            .geneses
            .iter()
            .map(Message::encoded_len)
            .chain(self.operations.iter().map(Message::encoded_len))
        {
            bytes = bytes
                .checked_add(length)
                .ok_or(Error::Invalid("publication original size overflow"))?;
            if length > 256 * 1024 || bytes > 16 * 1024 * 1024 {
                return Err(Error::Invalid(
                    "publication original metadata budget exceeded",
                ));
            }
        }
        if self.geneses.iter().any(|genesis| {
            genesis.genesis.as_ref().is_none_or(|record| {
                record.canonical_record.is_empty() || record.signatures.is_empty()
            })
        }) || self
            .operations
            .iter()
            .any(|record| record.canonical_record.is_empty() || record.signatures.is_empty())
        {
            return Err(Error::Invalid(
                "original signatures and canonical records required",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Client(#[from] ClientError<transport::Error>),
    #[error(transparent)]
    Transport(#[from] transport::Error),
    #[error("publication source: {0}")]
    Source(#[from] std::io::Error),
    #[error("invalid publication: {0}")]
    Invalid(&'static str),
}

impl<T: RpcTransport<Error = transport::Error>> Remote<T> {
    /// Publish original source proofs and exact source in one exchange. Inputs are the native pack
    /// and its index in opening order; neither is buffered in full. The caller
    /// retains the operation ID and opening to retry after an interrupted call.
    /// A receipt is returned only after verifying its exact publication scope.
    pub async fn publish_content<R: AsyncRead + Unpin + Send>(
        &self,
        opening: &PublishContentClientFrame,
        originals: &PublicationOriginals,
        mut artifacts: [R; 2],
    ) -> Result<PublicationReceipt, Error> {
        originals.validate_bounds()?;
        let Some(publish_content_client_frame::Body::Open(open)) = &opening.body else {
            return Err(Error::Invalid("Open required"));
        };
        if open.destination != self.description.endpoint
            || open.thread.is_none()
            || open.revision.is_none()
            || open.packs.len() != 2
            || open.packs[0].kind != pack_extent::Kind::NativePack as i32
            || open.packs[1].kind != pack_extent::Kind::NativeIndex as i32
        {
            return Err(Error::Invalid(
                "endpoint, capture and ordered native artifacts required",
            ));
        }
        let mut inventory = Vec::new();
        for extent in &open.packs {
            let Some(address) = &extent.pack else {
                return Err(Error::Invalid("artifact address required"));
            };
            if address.algorithm != "blake3"
                || address.digest.len() != 32
                || extent.length == 0
                || extent.offset != 0
                || extent.extent_digest.as_ref() != Some(address)
            {
                return Err(Error::Invalid(
                    "complete BLAKE3-addressed artifacts required",
                ));
            }
            extent
                .encode_length_delimited(&mut inventory)
                .map_err(|_| Error::Invalid("inventory encoding failed"))?;
        }
        let inventory = typed_digest("thread-source-inventory-v1", &inventory);
        let mut logical = opening.clone();
        if let Some(publish_content_client_frame::Body::Open(open)) = logical.body.as_mut() {
            open.checkpoint = None;
        }
        let digest = typed_digest("thread-source-transfer-v1", &logical.encode_to_vec());
        let expected = TransferCheckpoint {
            transfer_id: digest[..16].to_vec(),
            plan_digest: digest.to_vec(),
            resume_token: Vec::new(),
            committed_bytes: 0,
        };
        let (mut sender, mut responses) = self
            .api
            .exchange::<rpc::SyncServicePublishContent>(opening)
            .await?;
        let first = responses
            .next()
            .await?
            .ok_or(Error::Invalid("publication ended before admission"))?;
        let ready = match first.body {
            Some(publish_content_server_frame::Body::Receipt(receipt)) => {
                return validate_receipt(receipt, opening, &inventory);
            }
            Some(publish_content_server_frame::Body::Ready(ready)) => ready,
            _ => return Err(Error::Invalid("Ready or replay receipt required")),
        };
        if ready.endpoint != open.destination
            || ready.thread != open.thread
            || ready.current != open.revision
            || ready.checkpoint.as_ref() != Some(&expected)
        {
            return Err(Error::Invalid("admission differs from publication plan"));
        }
        let budget = ready
            .budget
            .ok_or(Error::Invalid("publication frame budget required"))?;
        let frame_limit = budget.max_frame_bytes as usize;
        if !(1024..=512 * 1024).contains(&frame_limit) {
            return Err(Error::Invalid("unsupported publication frame budget"));
        }
        let upload = async {
            for body in originals
                .geneses
                .iter()
                .cloned()
                .map(publish_content_client_frame::Body::ThreadGenesis)
                .chain(
                    originals
                        .operations
                        .iter()
                        .cloned()
                        .map(publish_content_client_frame::Body::Operation),
                )
            {
                let frame = PublishContentClientFrame {
                    client_operation_id: opening.client_operation_id.clone(),
                    body: Some(body),
                };
                if frame.encoded_len() > frame_limit {
                    return Err(Error::Invalid(
                        "original exceeds negotiated publication frame budget",
                    ));
                }
                sender.send(&frame).await?;
            }
            for (artifact, planned) in artifacts.iter_mut().zip(&open.packs) {
                let mut offset = 0;
                let mut digest = blake3::Hasher::new();
                let mut buffer = vec![0; frame_limit / 2];
                while offset < planned.length {
                    let length = (planned.length - offset).min(buffer.len() as u64) as usize;
                    let read = artifact.read(&mut buffer[..length]).await?;
                    if read == 0 {
                        return Err(Error::Invalid("artifact ended before declared length"));
                    }
                    let data = &buffer[..read];
                    digest.update(data);
                    let chunk = PublishContentClientFrame {
                        client_operation_id: opening.client_operation_id.clone(),
                        body: Some(publish_content_client_frame::Body::Pack(PackChunk {
                            extent: Some(PackExtent {
                                pack: planned.pack.clone(),
                                kind: planned.kind,
                                offset,
                                length: read as u64,
                                extent_digest: Some(ObjectAddress {
                                    algorithm: "blake3".into(),
                                    digest: blake3::hash(data).as_bytes().to_vec(),
                                }),
                            }),
                            data: data.to_vec(),
                        })),
                    };
                    if chunk.encoded_len() > frame_limit {
                        return Err(Error::Invalid("chunk exceeds negotiated frame budget"));
                    }
                    sender.send(&chunk).await?;
                    offset += read as u64;
                }
                if artifact.read(&mut buffer[..1]).await? != 0
                    || planned
                        .pack
                        .as_ref()
                        .is_none_or(|address| address.digest != digest.finalize().as_bytes())
                {
                    return Err(Error::Invalid(
                        "artifact differs from declared length or digest",
                    ));
                }
            }
            sender
                .send(&PublishContentClientFrame {
                    client_operation_id: opening.client_operation_id.clone(),
                    body: Some(publish_content_client_frame::Body::Finish(
                        PublishContentFinish {
                            checkpoint: Some(expected.clone()),
                        },
                    )),
                })
                .await?;
            sender.finish().await?;
            Ok::<_, Error>(())
        };
        let receive = async {
            while let Some(frame) = responses.next().await? {
                match frame.body {
                    Some(publish_content_server_frame::Body::Checkpoint(checkpoint))
                        if checkpoint == expected => {}
                    Some(publish_content_server_frame::Body::Receipt(receipt)) => {
                        return validate_receipt(receipt, opening, &inventory);
                    }
                    _ => return Err(Error::Invalid("unexpected publication response")),
                }
            }
            Err(Error::Invalid(
                "publication ended without a durable receipt",
            ))
        };
        let (_, receipt) = tokio::try_join!(upload, receive)?;
        Ok(receipt)
    }
}

fn validate_receipt(
    receipt: PublicationReceipt,
    opening: &PublishContentClientFrame,
    inventory: &[u8; 32],
) -> Result<PublicationReceipt, Error> {
    let Some(publish_content_client_frame::Body::Open(open)) = &opening.body else {
        return Err(Error::Invalid("Open required"));
    };
    if receipt.client_operation_id != opening.client_operation_id
        || receipt.destination != open.destination
        || receipt.thread != open.thread
        || receipt.revision != open.revision
        || receipt.sharing_policy_version != open.sharing_policy_version
    {
        return Err(Error::Invalid("receipt differs from requested publication"));
    }
    match &receipt.outcome {
        Some(publication_receipt::Outcome::Accepted(_))
            if receipt.accepted_inventory.as_ref().is_some_and(|address| {
                address.algorithm == "blake3" && address.digest == inventory
            }) =>
        {
            Ok(receipt)
        }
        Some(publication_receipt::Outcome::Rejected(error)) => {
            Err(transport::Error::Remote(error.clone().into()).into())
        }
        _ => Err(Error::Invalid(
            "receipt did not accept the complete inventory",
        )),
    }
}

fn typed_digest(kind: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(kind.as_bytes());
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(&[0]);
    hasher.update(bytes);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use api::v2::{
        MethodDescriptor,
        client::{MessageReader, MessageWriter},
    };
    use tokio::sync::mpsc;

    use super::*;

    struct Reader(mpsc::Receiver<Vec<u8>>);
    struct Writer(Option<mpsc::Sender<Vec<u8>>>);
    impl MessageReader for Reader {
        type Error = transport::Error;
        async fn next(&mut self) -> Result<Option<Vec<u8>>, Self::Error> {
            Ok(self.0.recv().await)
        }
        fn cancel(&mut self) {
            self.0.close();
        }
    }
    impl MessageWriter for Writer {
        type Error = transport::Error;
        async fn send(&mut self, bytes: Vec<u8>) -> Result<(), Self::Error> {
            self.0
                .as_ref()
                .ok_or(transport::Error::Protocol("closed"))?
                .send(bytes)
                .await
                .map_err(|_| transport::Error::Protocol("peer closed"))
        }
        async fn finish(&mut self) -> Result<(), Self::Error> {
            self.0.take();
            Ok(())
        }
        fn abort(&mut self) {
            self.0.take();
        }
    }
    struct Peer {
        wrong_receipt: bool,
    }
    impl RpcTransport for Peer {
        type Error = transport::Error;
        type Reader = Reader;
        type Writer = Writer;
        async fn unary(
            &self,
            _: &'static MethodDescriptor,
            _: Vec<u8>,
        ) -> Result<Vec<u8>, Self::Error> {
            unreachable!("test exercises only publication");
        }
        async fn observe(
            &self,
            _: &'static MethodDescriptor,
            _: Vec<u8>,
        ) -> Result<Reader, Self::Error> {
            unreachable!("test exercises only publication");
        }
        async fn exchange(
            &self,
            _: &'static MethodDescriptor,
            bytes: Vec<u8>,
        ) -> Result<(Writer, Reader), Self::Error> {
            let opening = PublishContentClientFrame::decode(bytes.as_slice())?;
            let Some(publish_content_client_frame::Body::Open(open)) = opening.body.clone() else {
                panic!("Open");
            };
            let mut logical = opening.clone();
            if let Some(publish_content_client_frame::Body::Open(open)) = logical.body.as_mut() {
                open.checkpoint = None;
            }
            let digest = typed_digest("thread-source-transfer-v1", &logical.encode_to_vec());
            let checkpoint = TransferCheckpoint {
                transfer_id: digest[..16].to_vec(),
                plan_digest: digest.to_vec(),
                committed_bytes: 0,
                resume_token: vec![],
            };
            let (tx, mut incoming) = mpsc::channel::<Vec<u8>>(1);
            let (outgoing, rx) = mpsc::channel(1);
            let wrong_receipt = self.wrong_receipt;
            tokio::spawn(async move {
                let send = |body| {
                    let outgoing = &outgoing;
                    async move {
                        outgoing
                            .send(PublishContentServerFrame { body: Some(body) }.encode_to_vec())
                            .await
                    }
                };
                send(publish_content_server_frame::Body::Ready(TransferReady {
                    endpoint: open.destination.clone(),
                    thread: open.thread.clone(),
                    current: open.revision.clone(),
                    checkpoint: Some(checkpoint.clone()),
                    budget: Some(ReadBudget {
                        max_frame_bytes: 2048,
                        ..Default::default()
                    }),
                    ..Default::default()
                }))
                .await
                .expect("Ready");
                let mut lengths = [0_u64; 2];
                let mut original_counts = [0usize; 2];
                while let Some(bytes) = incoming.recv().await {
                    let frame = PublishContentClientFrame::decode(bytes.as_slice()).expect("frame");
                    assert_eq!(frame.client_operation_id, opening.client_operation_id);
                    match frame.body {
                        Some(publish_content_client_frame::Body::ThreadGenesis(genesis)) => {
                            assert_eq!(lengths, [0, 0]);
                            assert!(genesis.genesis.is_some());
                            original_counts[0] += 1;
                            send(publish_content_server_frame::Body::Checkpoint(
                                checkpoint.clone(),
                            ))
                            .await
                            .expect("original checkpoint");
                        }
                        Some(publish_content_client_frame::Body::Operation(operation)) => {
                            assert_eq!(lengths, [0, 0]);
                            assert!(!operation.canonical_record.is_empty());
                            original_counts[1] += 1;
                            send(publish_content_server_frame::Body::Checkpoint(
                                checkpoint.clone(),
                            ))
                            .await
                            .expect("original checkpoint");
                        }
                        Some(publish_content_client_frame::Body::Pack(chunk)) => {
                            assert_eq!(
                                original_counts,
                                [1, 1],
                                "original proofs precede source artifacts"
                            );
                            let extent = chunk.extent.expect("extent");
                            let index = if extent.kind == pack_extent::Kind::NativePack as i32 {
                                0
                            } else {
                                1
                            };
                            assert_eq!(extent.offset, lengths[index]);
                            assert_eq!(extent.length, chunk.data.len() as u64);
                            assert_eq!(
                                extent.extent_digest.expect("chunk digest").digest,
                                blake3::hash(&chunk.data).as_bytes()
                            );
                            lengths[index] += extent.length;
                            // Capacity one in BOTH directions. An upload that
                            // waits to read responses until all sends complete
                            // deadlocks after a few chunks here.
                            send(publish_content_server_frame::Body::Checkpoint(
                                checkpoint.clone(),
                            ))
                            .await
                            .expect("checkpoint");
                        }
                        Some(publish_content_client_frame::Body::Finish(finish)) => {
                            assert_eq!(finish.checkpoint, Some(checkpoint.clone()));
                            assert_eq!(lengths, [open.packs[0].length, open.packs[1].length]);
                            let mut inventory = Vec::new();
                            for extent in &open.packs {
                                extent
                                    .encode_length_delimited(&mut inventory)
                                    .expect("inventory");
                            }
                            let mut receipt = PublicationReceipt {
                                client_operation_id: opening.client_operation_id.clone(),
                                destination: open.destination.clone(),
                                thread: open.thread.clone(),
                                revision: open.revision.clone(),
                                sharing_policy_version: open.sharing_policy_version.clone(),
                                accepted_inventory: Some(ObjectAddress {
                                    algorithm: "blake3".into(),
                                    digest: typed_digest("thread-source-inventory-v1", &inventory)
                                        .to_vec(),
                                }),
                                outcome: Some(publication_receipt::Outcome::Accepted(
                                    Applied::default(),
                                )),
                            };
                            if wrong_receipt {
                                receipt.thread = None;
                            }
                            let _ =
                                send(publish_content_server_frame::Body::Receipt(receipt)).await;
                            break;
                        }
                        _ => panic!("unexpected frame"),
                    }
                }
            });
            Ok((Writer(Some(tx)), Reader(rx)))
        }
    }

    fn fixture(
        wrong_receipt: bool,
    ) -> (
        Remote<Peer>,
        PublishContentClientFrame,
        [std::io::Cursor<Vec<u8>>; 2],
    ) {
        let endpoint = EndpointRef {
            public_key: vec![8; 32],
            kind: EndpointKind::Weft as i32,
        };
        let remote = Remote {
            api: api::v2::client::Client::new(
                Peer { wrong_receipt },
                ["/heddle.api.v2alpha1.SyncService/PublishContent".into()],
            ),
            description: DescribeEndpointResponse {
                endpoint: Some(endpoint.clone()),
                ..Default::default()
            },
        };
        let artifacts = [vec![1; 128 * 1024], vec![2; 16 * 1024]];
        let packs = artifacts
            .iter()
            .zip([
                pack_extent::Kind::NativePack,
                pack_extent::Kind::NativeIndex,
            ])
            .map(|(data, kind)| {
                let address = ObjectAddress {
                    algorithm: "blake3".into(),
                    digest: blake3::hash(data).as_bytes().to_vec(),
                };
                PackExtent {
                    pack: Some(address.clone()),
                    kind: kind as i32,
                    offset: 0,
                    length: data.len() as u64,
                    extent_digest: Some(address),
                }
            })
            .collect();
        let open = PublishContentClientFrame {
            client_operation_id: "op-test".into(),
            body: Some(publish_content_client_frame::Body::Open(
                PublishContentOpen {
                    destination: Some(endpoint),
                    thread: Some(ThreadRef::default()),
                    revision: Some(RevisionRef::default()),
                    packs,
                    ..Default::default()
                },
            )),
        };
        (remote, open, artifacts.map(std::io::Cursor::new))
    }
    // These byte fixtures exercise framing/backpressure, not original authority
    // admission; real Iroh tests independently verify canonical signatures.
    fn originals() -> PublicationOriginals {
        let record = SignedRecord {
            format: "transport-fixture".into(),
            canonical_record: vec![1],
            signatures: vec![RecordSignature {
                public_key: vec![2; 32],
                signature: vec![3; 64],
            }],
        };
        PublicationOriginals {
            geneses: vec![ThreadGenesisRecord {
                genesis: Some(record.clone()),
                ..Default::default()
            }],
            operations: vec![record],
        }
    }

    #[test]
    fn publication_originals_require_bounded_complete_metadata() {
        let valid = originals();
        assert!(valid.validate_bounds().is_ok());
        let mut missing = valid.clone();
        missing.operations.clear();
        assert!(matches!(
            missing.validate_bounds(),
            Err(Error::Invalid(
                "bounded original genesis and source operations required"
            ))
        ));
        let mut unsigned = valid.clone();
        unsigned.operations[0].signatures.clear();
        assert!(matches!(
            unsigned.validate_bounds(),
            Err(Error::Invalid(
                "original signatures and canonical records required"
            ))
        ));
        let mut oversized = valid.clone();
        oversized.operations[0].canonical_record = vec![1; 256 * 1024];
        assert!(matches!(
            oversized.validate_bounds(),
            Err(Error::Invalid(
                "publication original metadata budget exceeded"
            ))
        ));
        let mut too_many = valid;
        too_many.geneses = vec![too_many.geneses[0].clone(); 129];
        assert!(matches!(
            too_many.validate_bounds(),
            Err(Error::Invalid(
                "bounded original genesis and source operations required"
            ))
        ));
    }

    #[tokio::test]
    async fn publication_drains_checkpoints_while_uploading_under_backpressure() {
        let (remote, open, artifacts) = fixture(false);
        let receipt = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            remote.publish_content(&open, &originals(), artifacts),
        )
        .await
        .expect("must not deadlock")
        .expect("publication");
        assert_eq!(receipt.client_operation_id, "op-test");
    }
    #[tokio::test]
    async fn publication_refuses_a_receipt_for_another_scope() {
        let (remote, open, artifacts) = fixture(true);
        let error = remote
            .publish_content(&open, &originals(), artifacts)
            .await
            .expect_err("mismatched scope");
        assert!(matches!(
            error,
            Error::Invalid("receipt differs from requested publication")
        ));
    }
    #[cfg(feature = "replication")]
    #[test]
    fn publication_digests_match_the_native_typed_hash_format() {
        assert_eq!(
            typed_digest("thread-source-inventory-v1", b"bytes"),
            *heddle_object_model::object::ContentHash::compute_typed(
                "thread-source-inventory-v1",
                b"bytes"
            )
            .as_bytes()
        );
    }

    #[cfg(feature = "source-transfer")]
    #[tokio::test]
    async fn thread_publication_prepares_only_selected_source_and_binds_its_revision() {
        use objects::{
            object::{Attribution, Blob, Principal, State, Tree, TreeEntry},
            store::{FsStore, ObjectStore},
        };
        let root = tempfile::tempdir().expect("local source scratch");
        let store = FsStore::new(root.path().join("objects"));
        store.init().expect("store");
        let blob = Blob::new(vec![1; 128 * 1024]);
        store.put_blob(&blob).expect("source");
        let tree = Tree::from_entries(vec![
            TreeEntry::file("source.rs", blob.hash(), false).expect("entry"),
        ]);
        store.put_tree(&tree).expect("tree");
        let state = State::new_snapshot(
            tree.hash(),
            vec![],
            Attribution::human(Principal::new("user", "user@example.test")),
        );
        let prepared = SourcePack::prepare(
            &store,
            &state,
            root.path(),
            SourceBudget {
                max_objects: 16,
                max_decoded_bytes: 256 * 1024,
            },
        )
        .expect("prepare exact source");
        let (remote, _, _) = fixture(false);
        let thread = ThreadRef {
            spool: Some(SpoolRef {
                id: "spool-test".into(),
            }),
            id: Some(ThreadId { value: vec![1; 32] }),
        };
        let receipt = remote
            .thread(thread.clone())
            .publish_source(
                &prepared,
                &originals(),
                PublicationOptions {
                    client_operation_id: "source-upload".into(),
                    source: EndpointRef {
                        public_key: vec![2; 32],
                        kind: EndpointKind::Device as i32,
                    },
                    sharing_policy_version: vec![3; 32],
                    checkpoint: None,
                },
            )
            .await
            .expect("one Thread-bound publication");
        assert_eq!(receipt.thread, Some(thread.clone()));
        assert_eq!(
            receipt.revision,
            Some(RevisionRef {
                spool: thread.spool,
                revision: Some(revision_ref::Revision::State(
                    api::heddle::api::v1alpha1::StateId {
                        value: state.id().as_bytes().to_vec()
                    }
                ))
            })
        );
    }
}
