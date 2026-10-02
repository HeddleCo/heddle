//! HYBRID import authority is not supported yet (api#307).
//!
//! The api contract (`docs/alpha-v2/import-authority-host-witness.md`) says a
//! non-HYBRID peer MUST reject any Sync record or frame carrying
//! `import_authority` before staging, installation, relay or publication, and
//! never silently ignore it. Presence alone rejects, including an empty bundle.
//! Openings, ready replies and stream openings that declare HYBRID protocol
//! support or carry a witness set are rejected the same way: this peer cannot
//! honour them. Fields this peer sends stay empty.
//!
//! Each check returns the rejection message so the caller can raise it in its
//! own protocol error type.
use crate::contract::{
    FetchOpen, PublicationReceipt, PublishContentOpen, ReplicationOpen, ReplicationOperations,
    ReplicationReady, StreamOpen, TransferReady,
};

/// The reason a received message was rejected.
pub type Rejection = &'static str;

fn absent<T>(field: &Option<T>, message: Rejection) -> Result<(), Rejection> {
    match field {
        Some(_) => Err(message),
        None => Ok(()),
    }
}

pub fn operations(batch: &ReplicationOperations) -> Result<(), Rejection> {
    absent(
        &batch.import_authority,
        "replication operations carry HYBRID import authority, which this peer does not support (api#307)",
    )
}

pub fn replication_open(open: &ReplicationOpen) -> Result<(), Rejection> {
    absent(
        &open.protocol,
        "replication opening declares HYBRID protocol support, which this peer does not support (api#307)",
    )?;
    absent(
        &open.import_authority,
        "replication opening carries HYBRID import authority, which this peer does not support (api#307)",
    )
}

pub fn replication_ready(ready: &ReplicationReady) -> Result<(), Rejection> {
    absent(
        &ready.protocol,
        "replication ready declares HYBRID protocol support, which this peer does not support (api#307)",
    )?;
    absent(
        &ready.import_authority,
        "replication ready carries HYBRID import authority, which this peer does not support (api#307)",
    )
}

pub fn fetch_open(open: &FetchOpen) -> Result<(), Rejection> {
    absent(
        &open.protocol,
        "fetch opening declares HYBRID protocol support, which this peer does not support (api#307)",
    )
}

pub fn transfer_ready(ready: &TransferReady) -> Result<(), Rejection> {
    absent(
        &ready.protocol,
        "transfer ready declares HYBRID protocol support, which this peer does not support (api#307)",
    )?;
    absent(
        &ready.import_authority,
        "transfer ready carries HYBRID import authority, which this peer does not support (api#307)",
    )
}

pub fn publish_open(open: &PublishContentOpen) -> Result<(), Rejection> {
    absent(
        &open.protocol,
        "publication opening declares HYBRID protocol support, which this peer does not support (api#307)",
    )?;
    absent(
        &open.import_authority,
        "publication opening carries HYBRID import authority, which this peer does not support (api#307)",
    )
}

pub fn publication_receipt(receipt: &PublicationReceipt) -> Result<(), Rejection> {
    absent(
        &receipt.import_authority,
        "publication receipt carries HYBRID import authority, which this peer does not support (api#307)",
    )
}

pub fn stream_open(open: &StreamOpen) -> Result<(), Rejection> {
    absent(
        &open.protocol,
        "stream opening declares HYBRID protocol support, which this peer does not support (api#307)",
    )?;
    absent(
        &open.witness_set,
        "stream opening carries a HYBRID witness set, which this peer does not support (api#307)",
    )
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
    fn present_import_authority_is_rejected_never_ignored() {
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
    fn declared_hybrid_protocol_and_witness_sets_are_rejected() {
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
