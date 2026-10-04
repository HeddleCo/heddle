//! HYBRID transport checks. Structural proof closure is preparation only;
//! authority is resolved independently at each serialized durable mutation.
use api::heddle::api::common::ProtocolCompatibility;

use crate::contract::{
    FetchOpen, ImportPublicProofBundleV1, NativePublicProofBundleV1, PublicationReceipt,
    PublishContentOpen, ReplicationOpen, ReplicationOperations, ReplicationReady, StreamOpen,
    TransferReady,
};

#[cfg(feature = "native")]
pub mod authority;
pub mod history;
#[cfg(test)]
mod history_tests;
#[cfg(all(test, feature = "native"))]
mod native_tests;
#[cfg(test)]
mod protocol_tests;
pub type Rejection = &'static str;

/// Integration routes support HYBRID. Keep this single switch OFF until the
/// coordinated Sync mandatory cutover in api#307; optional capable paths still
/// require exact protocol negotiation before accepting any authority bundle.
pub const SYNC_MANDATORY_GATE: bool = false;

/// All ordinary Sync constructors use the same coordinated cutover switch.
pub fn sync_protocol() -> Option<ProtocolCompatibility> {
    SYNC_MANDATORY_GATE.then(protocol)
}

/// Keep RPC preludes and stream openings on the same cutover switch. Until
/// api#307, Sync declares no protocol support in its CallContext.
pub fn call_protocol(method_path: &str, requires_hybrid: bool) -> Option<ProtocolCompatibility> {
    if method_path
        .trim_start_matches('/')
        .starts_with("heddle.api.v1alpha2.SyncService/")
    {
        return sync_protocol();
    }
    requires_hybrid.then(protocol)
}

pub fn protocol() -> ProtocolCompatibility {
    ProtocolCompatibility {
        protocol_version: 2,
        mandatory_features: vec![1],
    }
}
fn check_protocol(value: Option<&ProtocolCompatibility>) -> Result<(), Rejection> {
    if SYNC_MANDATORY_GATE || value.is_some() {
        api::import_authority::require_hybrid_peer(value)
            .map_err(|_| "incompatible HYBRID protocol (api#307 cutover)")?;
    }
    Ok(())
}
/// Complete structural closure, never signature trust, enrollment or admission.
pub fn bundle(value: Option<&ImportPublicProofBundleV1>) -> Result<(), Rejection> {
    if let Some(value) = value {
        api::import_authority::validate_public_bundle(value)
            .map_err(|_| "incomplete HYBRID import authority (api#307 cutover)")?;
    }
    Ok(())
}
pub fn bundles(
    imported: Option<&ImportPublicProofBundleV1>,
    native: Option<&NativePublicProofBundleV1>,
) -> Result<(), Rejection> {
    api::native_witness::validate_carriers(imported, native).map_err(|_| {
        if native.is_some() {
            "incomplete or conflicting HYBRID native authority (api#307 cutover)"
        } else {
            "incomplete HYBRID import authority (api#307 cutover)"
        }
    })
}
fn carrier(
    protocol: Option<&ProtocolCompatibility>,
    value: Option<&ImportPublicProofBundleV1>,
    native: Option<&NativePublicProofBundleV1>,
) -> Result<(), Rejection> {
    check_protocol(protocol)?;
    if value.is_some() || native.is_some() {
        api::import_authority::require_hybrid_peer(protocol).map_err(
            |_| "import authority requires negotiated HYBRID protocol (api#307 cutover)",
        )?;
    }
    bundles(value, native)
}
/// Bind support to both ends of this exact stream, including its first Ready.
pub fn negotiated(
    open: Option<&ProtocolCompatibility>,
    ready: Option<&ProtocolCompatibility>,
) -> Result<(), Rejection> {
    check_protocol(open)?;
    check_protocol(ready)?;
    if open != ready {
        return Err("HYBRID protocol differs from stream opening");
    }
    Ok(())
}
pub fn operations(batch: &ReplicationOperations) -> Result<(), Rejection> {
    bundles(
        batch.import_authority.as_ref(),
        batch.native_authority.as_ref(),
    )
}
pub fn replication_open(open: &ReplicationOpen) -> Result<(), Rejection> {
    carrier(
        open.protocol.as_ref(),
        open.import_authority.as_ref(),
        open.native_authority.as_ref(),
    )
}
pub fn replication_ready(ready: &ReplicationReady) -> Result<(), Rejection> {
    carrier(
        ready.protocol.as_ref(),
        ready.import_authority.as_ref(),
        ready.native_authority.as_ref(),
    )
}
pub fn fetch_open(open: &FetchOpen) -> Result<(), Rejection> {
    check_protocol(open.protocol.as_ref())
}
pub fn transfer_ready(ready: &TransferReady) -> Result<(), Rejection> {
    carrier(
        ready.protocol.as_ref(),
        ready.import_authority.as_ref(),
        ready.native_authority.as_ref(),
    )
}
pub fn publish_open(open: &PublishContentOpen) -> Result<(), Rejection> {
    carrier(
        open.protocol.as_ref(),
        open.import_authority.as_ref(),
        open.native_authority.as_ref(),
    )
}
pub fn publication_receipt(receipt: &PublicationReceipt) -> Result<(), Rejection> {
    bundles(
        receipt.import_authority.as_ref(),
        receipt.native_authority.as_ref(),
    )
}
pub fn stream_open(open: &StreamOpen) -> Result<(), Rejection> {
    check_protocol(open.protocol.as_ref())?;
    if let Some(set) = &open.witness_set {
        use prost::Message;
        api::import_authority::require_hybrid_peer(open.protocol.as_ref())
            .map_err(|_| "witness set requires HYBRID protocol (api#307 cutover)")?;
        if set.encoded_len() > api::witness_trust::MAX_SET_BYTES || set.body.is_none() {
            return Err("invalid HYBRID witness set");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use api::heddle::api::common::{ProtocolCompatibility, SignedHostedWitnessSetV1};

    use super::*;
    use crate::contract::ImportPublicProofBundleV1;

    fn rejected(result: Result<(), Rejection>, field: &str) {
        let message = result.expect_err(field);
        assert!(message.contains(field), "{message}");
        assert!(message.contains("api#307"), "{message}");
    }

    // Presence alone rejects: an empty-but-present bundle or protocol is still
    // a claim this peer cannot honour, so the defaults below are deliberate.
    fn bundle() -> Option<ImportPublicProofBundleV1> {
        Some(ImportPublicProofBundleV1::default())
    }
    fn protocol() -> Option<ProtocolCompatibility> {
        Some(ProtocolCompatibility::default())
    }

    #[test]
    fn absent_hybrid_fields_are_accepted() {
        operations(&ReplicationOperations::default()).expect("operations");
        replication_open(&ReplicationOpen::default()).expect("replication open");
        replication_ready(&ReplicationReady::default()).expect("replication ready");
        fetch_open(&FetchOpen::default()).expect("fetch open");
        transfer_ready(&TransferReady::default()).expect("transfer ready");
        publish_open(&PublishContentOpen::default()).expect("publish open");
        publication_receipt(&PublicationReceipt::default()).expect("receipt");
        stream_open(&StreamOpen::default()).expect("stream open");
    }

    #[test]
    fn incomplete_import_authority_is_rejected_never_ignored() {
        rejected(
            operations(&ReplicationOperations {
                import_authority: bundle(),
                ..Default::default()
            }),
            "import authority",
        );
        rejected(
            replication_open(&ReplicationOpen {
                import_authority: bundle(),
                ..Default::default()
            }),
            "import authority",
        );
        rejected(
            replication_ready(&ReplicationReady {
                import_authority: bundle(),
                ..Default::default()
            }),
            "import authority",
        );
        rejected(
            transfer_ready(&TransferReady {
                import_authority: bundle(),
                ..Default::default()
            }),
            "import authority",
        );
        rejected(
            publish_open(&PublishContentOpen {
                import_authority: bundle(),
                ..Default::default()
            }),
            "import authority",
        );
        rejected(
            publication_receipt(&PublicationReceipt {
                import_authority: bundle(),
                ..Default::default()
            }),
            "import authority",
        );
    }

    #[test]
    fn unsupported_protocol_and_incomplete_witness_sets_are_rejected() {
        rejected(
            replication_open(&ReplicationOpen {
                protocol: protocol(),
                ..Default::default()
            }),
            "protocol",
        );
        rejected(
            replication_ready(&ReplicationReady {
                protocol: protocol(),
                ..Default::default()
            }),
            "protocol",
        );
        rejected(
            fetch_open(&FetchOpen {
                protocol: protocol(),
                ..Default::default()
            }),
            "protocol",
        );
        rejected(
            transfer_ready(&TransferReady {
                protocol: protocol(),
                ..Default::default()
            }),
            "protocol",
        );
        rejected(
            publish_open(&PublishContentOpen {
                protocol: protocol(),
                ..Default::default()
            }),
            "protocol",
        );
        rejected(
            stream_open(&StreamOpen {
                protocol: protocol(),
                ..Default::default()
            }),
            "protocol",
        );
        rejected(
            stream_open(&StreamOpen {
                witness_set: Some(SignedHostedWitnessSetV1::default()),
                ..Default::default()
            }),
            "witness set",
        );
    }
}
