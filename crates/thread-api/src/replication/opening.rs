//! Shared negotiation for already-authorized, already-resolved Threads.
//! This validates transport bindings and limits; it does not grant authority.
use std::collections::BTreeSet;

use crypto::{Signer, thread_operation::SignedGenesis};
use heddle_object_model::object::thread_replication::{
    GENESIS_FORMAT, OPERATION_FORMAT, ThreadFacet, ThreadGenesis,
};

use crate::{contract::*, transport::Error};

pub const FRAME_LIMIT: usize = 512 * 1024;
pub const MAX_ITEMS: u32 = 64;

pub fn validate_endpoint(endpoint: &EndpointRef) -> Result<(), Error> {
    if endpoint.public_key.len() != 32
        || !matches!(
            EndpointKind::try_from(endpoint.kind),
            Ok(EndpointKind::Device | EndpointKind::Weft)
        )
    {
        return Err(Error::Protocol("invalid replication endpoint"));
    }
    Ok(())
}

pub fn parse_facets(values: &[i32]) -> Result<BTreeSet<ThreadFacet>, Error> {
    if values.is_empty() || values.len() > ThreadFacet::ALL.len() {
        return Err(Error::Protocol(
            "replication requires a bounded set of distinct facets",
        ));
    }
    let facets = values
        .iter()
        .map(|value| {
            super::native_facet(*value)
                .map_err(|_| Error::Protocol("unsupported replication facet"))
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if facets.len() != values.len() {
        return Err(Error::Protocol("duplicate replication facet"));
    }
    Ok(facets)
}

/// The remote key must come from the authenticated transport, never the frame.
/// An attached genesis retains the original creator's signature and identity.
/// The host must authorize publication and commit that genesis before Ready;
/// parsing it grants no authority. PoP covers the entire exact opening.
pub fn accept(
    open: &ReplicationOpen,
    thread: &ThreadRef,
    local: &EndpointRef,
    remote_key: [u8; 32],
    admission: &BTreeSet<ThreadFacet>,
    sharing_policy_version: Vec<u8>,
) -> Result<AcceptedOpening, Error> {
    validate_endpoint(local)?;
    if open.thread.as_ref() != Some(thread) {
        return Err(Error::Protocol(
            "replication requires an already resolved Thread",
        ));
    }
    let source = open
        .source
        .as_ref()
        .ok_or(Error::Protocol("opening requires source endpoint"))?;
    validate_endpoint(source)?;
    if source.public_key != remote_key || open.destination.as_ref() != Some(local) {
        return Err(Error::Protocol(
            "opening endpoints differ from Iroh connection",
        ));
    }
    if open.session_nonce.len() != 16
        || !open
            .record_formats
            .iter()
            .any(|format| format == OPERATION_FORMAT)
    {
        return Err(Error::Protocol("unsupported replication session or format"));
    }
    // Native records are indivisible. Negotiate the supported frame size
    // explicitly instead of promising a smaller budget and exceeding it.
    if open.budget.as_ref().is_some_and(|budget| {
        budget.max_frame_bytes != 0 && budget.max_frame_bytes != FRAME_LIMIT as u32
    }) {
        return Err(Error::Protocol("unsupported replication frame budget"));
    }
    let facets: BTreeSet<_> = parse_facets(&open.facets)?
        .intersection(admission)
        .copied()
        .collect();
    if facets.is_empty() {
        return Err(Error::Protocol("no authorized replication facets"));
    }
    let requested = open.budget.as_ref().map_or(0, |budget| budget.max_items);
    let genesis = open
        .thread_genesis
        .as_ref()
        .map(|record| verify_genesis_record(record, thread))
        .transpose()?;
    Ok(AcceptedOpening {
        genesis,
        genesis_record: open.thread_genesis.clone(),
        ready: ReplicationReady {
            thread: Some(thread.clone()),
            endpoint: Some(local.clone()),
            facets: facets.into_iter().map(super::wire_facet).collect(),
            sharing_policy_version,
            budget: Some(ReadBudget {
                max_items: if requested == 0 {
                    MAX_ITEMS
                } else {
                    requested.min(MAX_ITEMS)
                },
                max_frame_bytes: FRAME_LIMIT as u32,
                max_snapshot_bytes: 0,
            }),
            record_formats: vec![OPERATION_FORMAT.into()],
        },
    })
}

/// Parsed proposal; authorization and durable installation belong to the host.
pub struct AcceptedOpening {
    pub ready: ReplicationReady,
    pub genesis: Option<ThreadGenesis>,
    pub genesis_record: Option<ThreadGenesisRecord>,
}

/// Structural and original-signature validation only. Account authority and
/// hosted admission receipts still require independently retained trust.
pub fn verify_genesis_record(
    record: &ThreadGenesisRecord,
    thread: &ThreadRef,
) -> Result<ThreadGenesis, Error> {
    use prost::Message;
    if record.boundary_acceptances.len() > crate::boundary_acceptance::MAX_ACCEPTANCES
        || record.encoded_len() > 256 * 1024
    {
        return Err(Error::Protocol("genesis wrapper evidence exceeds bounds"));
    }
    let signed = record
        .genesis
        .as_ref()
        .ok_or(Error::Protocol("original signed genesis missing"))?;
    let genesis = verify_genesis(signed, thread)?;
    if record.creator_authority.len() > 64 * 1024 {
        return Err(Error::Protocol("creator authority exceeds bound"));
    }
    use heddle_object_model::object::thread_replication::GenesisOwner;
    match genesis.owner {
        GenesisOwner::LocalKey(_)
            if !record.creator_authority.is_empty() || record.admission.is_some() =>
        {
            return Err(Error::Protocol(
                "local-key ownership requires an explicit claim, not an account envelope",
            ));
        }
        GenesisOwner::Account(_) if record.creator_authority.is_empty() => {
            return Err(Error::Protocol(
                "account-owned genesis requires original creator authority",
            ));
        }
        _ => {}
    }
    super::ownership::verify_claims(record, &genesis)?;
    Ok(genesis)
}

pub fn sign_genesis(genesis: &ThreadGenesis, signer: &impl Signer) -> Result<SignedRecord, Error> {
    let signed =
        SignedGenesis::sign(genesis, signer).map_err(|error| Error::Io(error.to_string()))?;
    Ok(SignedRecord {
        format: GENESIS_FORMAT.into(),
        canonical_record: signed.canonical,
        signatures: vec![RecordSignature {
            public_key: genesis.creator.to_vec(),
            signature: signed.signature,
        }],
    })
}

pub fn verify_genesis(record: &SignedRecord, thread: &ThreadRef) -> Result<ThreadGenesis, Error> {
    if record.format != GENESIS_FORMAT || record.signatures.len() != 1 {
        return Err(Error::Protocol("unsupported Thread genesis record"));
    }
    let genesis = SignedGenesis {
        canonical: record.canonical_record.clone(),
        signature: record.signatures[0].signature.clone(),
    }
    .verify()
    .map_err(|_| Error::Protocol("invalid Thread genesis signature"))?;
    let id = genesis
        .id()
        .map_err(|_| Error::Protocol("invalid Thread genesis"))?;
    if record.signatures[0].public_key != genesis.creator
        || thread
            .spool
            .as_ref()
            .is_none_or(|spool| spool.id != genesis.spool)
        || thread
            .id
            .as_ref()
            .is_none_or(|thread| thread.value != id.as_bytes())
    {
        return Err(Error::Protocol(
            "Thread genesis identity does not match opening",
        ));
    }
    Ok(genesis)
}

pub fn validate_ready(
    ready: &ReplicationReady,
    thread: &ThreadRef,
    destination: &EndpointRef,
    requested_facets: &BTreeSet<ThreadFacet>,
    requested_max_items: u32,
) -> Result<(BTreeSet<ThreadFacet>, usize), Error> {
    validate_endpoint(destination)?;
    if ready.endpoint.as_ref() != Some(destination)
        || ready.thread.as_ref() != Some(thread)
        || ready.record_formats != [OPERATION_FORMAT]
    {
        return Err(Error::Protocol(
            "replication Ready binding differs from opening",
        ));
    }
    let facets = parse_facets(&ready.facets)?;
    if !facets.is_subset(requested_facets) {
        return Err(Error::Protocol("replication Ready widened admission scope"));
    }
    let budget = ready
        .budget
        .as_ref()
        .ok_or(Error::Protocol("replication Ready requires budget"))?;
    let ceiling = if requested_max_items == 0 {
        MAX_ITEMS
    } else {
        requested_max_items.min(MAX_ITEMS)
    };
    if budget.max_items == 0
        || budget.max_items > ceiling
        || budget.max_frame_bytes != FRAME_LIMIT as u32
    {
        return Err(Error::Protocol("unsupported replication Ready budget"));
    }
    Ok((facets, budget.max_items as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_publication_retains_signed_genesis_and_rejects_changed_identity() {
        use crypto::{Ed25519Signer, Signer};
        use heddle_object_model::object::{
            StateId,
            thread_replication::{GENESIS_FORMAT, ThreadGenesis},
        };
        let signer = Ed25519Signer::from_seed(&[23; 32]).expect("origin device");
        let genesis = ThreadGenesis {
            version: 1,
            spool: "01980000-0000-7000-8000-000000000001".into(),
            parent: None,
            base: StateId::from_bytes([1; 32]),
            name: "original".into(),
            intent: "publish once".into(),
            creator: signer.public_key().try_into().expect("public key"),
            owner: heddle_object_model::object::thread_replication::GenesisOwner::LocalKey(
                signer.public_key().try_into().expect("public key"),
            ),
            nonce: vec![2; 16],
        };
        let canonical = genesis.encode().expect("canonical genesis");
        let mut signing = GENESIS_FORMAT.as_bytes().to_vec();
        signing.push(0);
        signing.extend(&canonical);
        let signed = SignedRecord {
            format: GENESIS_FORMAT.into(),
            canonical_record: canonical,
            signatures: vec![RecordSignature {
                public_key: signer.public_key().to_vec(),
                signature: signer.sign(&signing).expect("origin signature"),
            }],
        };
        let thread = ThreadRef {
            spool: Some(SpoolRef {
                id: genesis.spool.clone(),
            }),
            id: Some(ThreadId {
                value: genesis.id().expect("Thread ID").as_bytes().to_vec(),
            }),
        };
        let local = EndpointRef {
            public_key: vec![7; 32],
            kind: EndpointKind::Weft as i32,
        };
        let open = ReplicationOpen {
            thread: Some(thread.clone()),
            thread_genesis: Some(ThreadGenesisRecord {
                boundary_acceptances: Vec::new(),
                ownership_claims: vec![],
                ownership_claim_admissions: vec![],
                genesis: Some(signed),
                creator_authority: vec![],
                admission: None,
            }),
            source: Some(EndpointRef {
                public_key: vec![8; 32],
                kind: EndpointKind::Device as i32,
            }),
            destination: Some(local.clone()),
            facets: vec![SharedFacet::Source as i32],
            session_nonce: vec![9; 16],
            record_formats: vec![OPERATION_FORMAT.into()],
            ..Default::default()
        };
        let facets = BTreeSet::from([ThreadFacet::Source]);
        assert!(
            accept(&open, &thread, &local, [8; 32], &facets, vec![]).is_ok(),
            "first publication must accept the original signed genesis"
        );
        let mut changed = open.clone();
        changed
            .thread_genesis
            .as_mut()
            .expect("genesis")
            .genesis
            .as_mut()
            .expect("signed genesis")
            .signatures[0]
            .signature[0] ^= 1;
        assert!(accept(&changed, &thread, &local, [8; 32], &facets, vec![]).is_err());
        changed = open.clone();
        changed
            .thread
            .as_mut()
            .expect("Thread")
            .id
            .as_mut()
            .expect("ID")
            .value[0] ^= 1;
        let claimed = changed.thread.clone().expect("claimed Thread");
        assert!(accept(&changed, &claimed, &local, [8; 32], &facets, vec![]).is_err());
        changed = open;
        changed
            .thread
            .as_mut()
            .expect("Thread")
            .spool
            .as_mut()
            .expect("spool")
            .id = "another".into();
        let claimed = changed.thread.clone().expect("claimed Thread");
        assert!(accept(&changed, &claimed, &local, [8; 32], &facets, vec![]).is_err());
    }

    #[test]
    fn opening_binds_both_endpoints_thread_formats_and_negotiated_limits() {
        let thread = ThreadRef {
            spool: Some(SpoolRef { id: "spool".into() }),
            id: Some(ThreadId { value: vec![3; 32] }),
        };
        let local = EndpointRef {
            kind: EndpointKind::Weft as i32,
            public_key: vec![1; 32],
        };
        let source = EndpointRef {
            kind: EndpointKind::Device as i32,
            public_key: vec![2; 32],
        };
        let facets = BTreeSet::from([ThreadFacet::Source, ThreadFacet::Discussion]);
        let open = ReplicationOpen {
            thread: Some(thread.clone()),
            source: Some(source),
            destination: Some(local.clone()),
            facets: facets
                .iter()
                .copied()
                .map(super::super::wire_facet)
                .collect(),
            session_nonce: vec![4; 16],
            record_formats: vec![OPERATION_FORMAT.into()],
            budget: Some(ReadBudget {
                max_items: 1,
                max_frame_bytes: FRAME_LIMIT as u32,
                max_snapshot_bytes: 0,
            }),
            ..Default::default()
        };
        let allowed = BTreeSet::from([ThreadFacet::Source]);
        let ready = accept(&open, &thread, &local, [2; 32], &allowed, vec![5; 32])
            .expect("authorized opening")
            .ready;
        assert_eq!(
            validate_ready(&ready, &thread, &local, &facets, 1).expect("bound ready"),
            (allowed.clone(), 1)
        );
        assert_eq!(ready.sharing_policy_version, vec![5; 32]);
        assert!(accept(&open, &thread, &local, [7; 32], &allowed, vec![]).is_err());
        let mut changed = open.clone();
        changed
            .destination
            .as_mut()
            .expect("destination")
            .public_key = vec![8; 32];
        assert!(accept(&changed, &thread, &local, [2; 32], &allowed, vec![]).is_err());
        changed = open.clone();
        changed
            .thread
            .as_mut()
            .expect("thread")
            .id
            .as_mut()
            .expect("id")
            .value = vec![8; 32];
        assert!(accept(&changed, &thread, &local, [2; 32], &allowed, vec![]).is_err());
        changed = open.clone();
        changed.facets = vec![SharedFacet::Source as i32; 2];
        assert!(accept(&changed, &thread, &local, [2; 32], &allowed, vec![]).is_err());
        changed = open.clone();
        changed.record_formats.clear();
        assert!(accept(&changed, &thread, &local, [2; 32], &allowed, vec![]).is_err());
        changed = open.clone();
        changed.budget.as_mut().expect("budget").max_frame_bytes = 128;
        assert!(accept(&changed, &thread, &local, [2; 32], &allowed, vec![]).is_err());
        let mut widened = ready.clone();
        widened.budget.as_mut().expect("budget").max_items = 2;
        assert!(validate_ready(&widened, &thread, &local, &facets, 1).is_err());
        widened = ready;
        widened.facets.push(SharedFacet::Collaboration as i32);
        assert!(validate_ready(&widened, &thread, &local, &allowed, 1).is_err());
    }
}
