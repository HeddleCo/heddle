//! BeginPairing carries `--host` as `web_origin`, and a server with no default
//! web origin answers with a typed `--host` remedy (heddle#1930).
use std::sync::{Arc, Mutex};

use ::api::{
    heddle::api::common::{CallFailure, CallFailureCode},
    v2::{
        MethodDescriptor,
        client::{Client, MessageReader, MessageWriter, RpcTransport},
    },
};
use config::web_origin::PairingWebOrigin;
use crypto::Ed25519Signer;
use objects::HeddleError;
use prost::Message as _;
use thread_api::{Remote, Rpc as _, contract as api, rpc, transport::Error};

use super::begin_pairing;

const SERVER: &str = "api.staging.heddle.test";
/// weft `DEVICE_PAIRING_NO_WEB_ORIGIN` (weft#2427), verbatim.
const NO_WEB_ORIGIN: &str = "this server has no web origin configured (SERVER_WEB_ORIGIN is unset), so it cannot choose the device-approval page for a CLI pairing; pass the web host explicitly with `heddle auth login --host <web host>` (BeginPairingRequest.web_origin, api#278)";

struct NoStream;
impl MessageReader for NoStream {
    type Error = Error;
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        Err(Error::Protocol("BeginPairing is unary"))
    }
    fn cancel(&mut self) {}
}
impl MessageWriter for NoStream {
    type Error = Error;
    async fn send(&mut self, _: Vec<u8>) -> Result<(), Error> {
        Err(Error::Protocol("BeginPairing is unary"))
    }
    async fn finish(&mut self) -> Result<(), Error> {
        Err(Error::Protocol("BeginPairing is unary"))
    }
    fn abort(&mut self) {}
}

/// Records every BeginPairing request and answers with `refusal`, or with an
/// empty success when there is none.
struct PairingPeer {
    sent: Arc<Mutex<Vec<api::BeginPairingRequest>>>,
    refusal: Option<(CallFailureCode, &'static str)>,
}
impl RpcTransport for PairingPeer {
    type Error = Error;
    type Reader = NoStream;
    type Writer = NoStream;
    async fn unary(
        &self,
        method: &'static MethodDescriptor,
        bytes: Vec<u8>,
    ) -> Result<Vec<u8>, Error> {
        assert_eq!(method.path, rpc::IdentityServiceBeginPairing::METHOD.path);
        let request = api::BeginPairingRequest::decode(bytes.as_slice())?;
        self.sent
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(request);
        match self.refusal {
            Some((code, message)) => Err(Error::Remote(
                CallFailure {
                    code: code as i32,
                    message: message.into(),
                    error: None,
                }
                .into(),
            )),
            None => Ok(api::BeginPairingResponse::default().encode_to_vec()),
        }
    }
    async fn observe(&self, _: &'static MethodDescriptor, _: Vec<u8>) -> Result<NoStream, Error> {
        Err(Error::Protocol("BeginPairing is unary"))
    }
    async fn exchange(
        &self,
        _: &'static MethodDescriptor,
        _: Vec<u8>,
    ) -> Result<(NoStream, NoStream), Error> {
        Err(Error::Protocol("BeginPairing is unary"))
    }
}

async fn begin(
    web_origin: Option<&PairingWebOrigin>,
    refusal: Option<(CallFailureCode, &'static str)>,
) -> (
    anyhow::Result<api::BeginPairingResponse>,
    api::BeginPairingRequest,
) {
    let sent = Arc::new(Mutex::new(Vec::new()));
    let peer = PairingPeer {
        sent: Arc::clone(&sent),
        refusal,
    };
    let remote = Remote {
        api: Client::new(
            peer,
            [rpc::IdentityServiceBeginPairing::METHOD.path.to_string()],
        ),
        description: api::DescribeEndpointResponse {
            endpoint: Some(api::EndpointRef {
                public_key: vec![7; 32],
                kind: api::EndpointKind::Weft as i32,
            }),
            ..Default::default()
        },
    };
    let subject = Ed25519Signer::from_seed(&[41; 32]).expect("subject");
    let endpoint = Ed25519Signer::from_seed(&[42; 32]).expect("endpoint");
    let result = begin_pairing(
        &remote,
        SERVER,
        &subject,
        &endpoint,
        "begin-pairing-op".into(),
        web_origin,
    )
    .await;
    let mut sent = sent.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(sent.len(), 1, "exactly one BeginPairing is sent");
    (result, sent.remove(0))
}

fn preview_host() -> PairingWebOrigin {
    PairingWebOrigin::parse("PR-17-Tapestry.zephyr-forge.workers.dev").expect("preview host")
}

#[tokio::test]
async fn begin_pairing_sends_host_as_web_origin_only_when_given() {
    let _process_env_guard = crate::test_process_env::shared().await;
    let host = preview_host();
    let (result, with_host) = begin(Some(&host), None).await;
    result.expect("pairing begins");
    assert_eq!(
        with_host.web_origin, "https://pr-17-tapestry.zephyr-forge.workers.dev",
        "--host travels as the canonical web_origin"
    );

    let (result, without_host) = begin(None, None).await;
    result.expect("pairing begins");
    assert_eq!(
        without_host.web_origin, "",
        "no --host sends empty, meaning the server default"
    );

    // web_origin is outside the signed binding: the possession signature is
    // over the same canonical record either way.
    let record = |request: &api::BeginPairingRequest| {
        request
            .subject_possession
            .as_ref()
            .map(|signed| signed.canonical_record.len())
    };
    assert!(record(&with_host).is_some());
    assert_eq!(record(&with_host), record(&without_host));
}

#[tokio::test]
async fn no_web_origin_refusal_recommends_login_with_host() {
    let _process_env_guard = crate::test_process_env::shared().await;
    let (result, _) = begin(
        None,
        Some((CallFailureCode::FailedPrecondition, NO_WEB_ORIGIN)),
    )
    .await;
    let error = result.expect_err("the server refused");
    let Some(HeddleError::Recovery(advice)) = error.downcast_ref::<HeddleError>() else {
        panic!("expected a typed recovery refusal, got {error:#}");
    };
    assert_eq!(advice.kind, "auth_login_web_host_required");
    let remedy = format!("heddle auth login --server {SERVER} --host <web-host>");
    assert_eq!(
        advice.recovery_commands.as_deref(),
        Some([remedy.clone()].as_slice())
    );
    assert!(advice.hint.contains(&remedy), "{}", advice.hint);
    assert!(
        advice
            .unsafe_condition
            .contains("SERVER_WEB_ORIGIN is unset")
    );
}

#[tokio::test]
async fn other_refusals_keep_the_server_message() {
    let _process_env_guard = crate::test_process_env::shared().await;
    let host = preview_host();
    for (web_origin, refusal) in [
        // --host was already sent: repeating the remedy would mislead.
        (
            Some(&host),
            (CallFailureCode::FailedPrecondition, NO_WEB_ORIGIN),
        ),
        // A host outside the server's CORS/preview policy.
        (
            Some(&host),
            (
                CallFailureCode::InvalidArgument,
                "web_origin is not an allowed web origin",
            ),
        ),
        (
            None,
            (CallFailureCode::FailedPrecondition, "pairing is disabled"),
        ),
    ] {
        let (result, _) = begin(web_origin, Some(refusal)).await;
        let error = result.expect_err("the server refused");
        assert!(
            error.downcast_ref::<HeddleError>().is_none(),
            "{refusal:?} must not become the --host remedy: {error:#}"
        );
        assert!(format!("{error:#}").contains(refusal.1), "{error:#}");
    }
}
