#![cfg(feature = "replication")]

use api::v2::{
    MethodDescriptor,
    client::{Client, MessageReader, MessageWriter, RpcTransport},
};
use crypto::{Ed25519Signer, Signer};
use heddle_object_model::object::{
    StateId,
    thread_replication::{GENESIS_FORMAT, ThreadGenesis},
};
use heddle_thread_api::{
    Remote, Rpc, contract::*, creation::ThreadCreation, rpc, transport::Error,
};
use prost::Message;

struct Reply {
    request: StartThreadRequest,
    response: ThreadMutationResponse,
}
struct NoStream;
impl MessageReader for NoStream {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        panic!("creation is unary")
    }
    fn cancel(&mut self) {}
}
impl MessageWriter for NoStream {
    type Error = Error;
    async fn send(&mut self, _: Vec<u8>) -> Result<(), Error> {
        panic!("creation is unary")
    }
    async fn finish(&mut self) -> Result<(), Error> {
        panic!("creation is unary")
    }
    fn abort(&mut self) {}
}
impl RpcTransport for Reply {
    type Error = Error;
    type Reader = NoStream;
    type Writer = NoStream;
    async fn unary(
        &self,
        method: &'static MethodDescriptor,
        bytes: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        assert_eq!(method.path, rpc::ThreadServiceStartThread::METHOD.path);
        assert_eq!(
            StartThreadRequest::decode(bytes.as_slice()).expect("request"),
            self.request
        );
        Ok(self.response.encode_to_vec())
    }
    async fn observe(&self, _: &'static MethodDescriptor, _: Vec<u8>) -> Result<NoStream, Error> {
        panic!("creation is unary")
    }
    async fn exchange(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<(NoStream, NoStream), Error> {
        panic!("creation is unary")
    }
}

#[tokio::test]
async fn creation_returns_a_bound_receipt_and_rejects_incomplete_or_unrelated_results() {
    let signer = Ed25519Signer::from_seed(&[11; 32]).expect("creator");
    let creation = ThreadCreation::sign(
        "01980000-0000-7000-8000-000000000002",
        &ThreadGenesis {
            version: 1,
            spool: "01980000-0000-7000-8000-000000000001".into(),
            parent: None,
            base: StateId::from_bytes([17; 32]),
            name: "work".into(),
            intent: "create once".into(),
            creator: signer.public_key().try_into().expect("key"),
            nonce: vec![23; 16],
        },
        &signer,
    )
    .expect("prepare");
    let description = DescribeEndpointResponse {
        endpoint: Some(EndpointRef {
            public_key: vec![7; 32],
            kind: EndpointKind::Weft as i32,
        }),
        understood_signed_record_formats: vec![GENESIS_FORMAT.into()],
        ..Default::default()
    };
    let valid = ThreadMutationResponse {
        receipt: Some(MutationReceipt {
            client_operation_id: creation.request().client_operation_id.clone(),
            endpoint: description.endpoint.clone(),
            outcome: Some(mutation_receipt::Outcome::Applied(Default::default())),
            ..Default::default()
        }),
        thread: Some(ThreadOverview {
            r#ref: Some(creation.reference().clone()),
            ..Default::default()
        }),
    };
    for case in 0..9 {
        let mut response = valid.clone();
        match case {
            0 => {}
            1 => response.receipt = None,
            2 => {
                response
                    .receipt
                    .as_mut()
                    .expect("receipt")
                    .client_operation_id = "other operation".into()
            }
            3 => {
                response
                    .receipt
                    .as_mut()
                    .expect("receipt")
                    .endpoint
                    .as_mut()
                    .expect("endpoint")
                    .public_key[0] ^= 1
            }
            4 => response.receipt.as_mut().expect("receipt").outcome = None,
            5 => {
                response
                    .thread
                    .as_mut()
                    .expect("overview")
                    .r#ref
                    .as_mut()
                    .expect("ref")
                    .id
                    .as_mut()
                    .expect("id")
                    .value[0] ^= 1
            }
            6 => response.thread = None,
            7 => {
                response.thread = None;
                response.receipt.as_mut().expect("receipt").outcome =
                    Some(mutation_receipt::Outcome::Blocked(Default::default()));
            }
            8 => {
                response.thread = None;
                response.receipt.as_mut().expect("receipt").outcome = Some(
                    mutation_receipt::Outcome::PendingOperation(Default::default()),
                );
            }
            _ => unreachable!(),
        }
        let remote = Remote {
            api: Client::new(
                Reply {
                    request: creation.request().clone(),
                    response: response.clone(),
                },
                [rpc::ThreadServiceStartThread::METHOD.path.into()],
            ),
            description: description.clone(),
        };
        let result = remote.start_thread(&creation).await;
        if matches!(case, 0 | 7 | 8) {
            assert_eq!(result.expect("typed outcome"), response);
        } else {
            assert!(result.is_err(), "invalid response accepted: case {case}");
        }
    }
}
