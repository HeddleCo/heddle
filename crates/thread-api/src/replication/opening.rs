//! Shared negotiation for already-authorized, already-resolved Threads.
//! This validates transport bindings and limits; it does not grant authority.
use std::collections::BTreeSet;

use heddle_object_model::object::thread_replication::{OPERATION_FORMAT, ThreadFacet};

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
    if values.is_empty() || values.len() > 2 {
        return Err(Error::Protocol(
            "replication requires one or two distinct facets",
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
/// Creation is a separate authorized operation; this opening cannot install
/// or replace a Thread genesis. The caller verifies PoP over the exact bytes.
pub fn accept(
    open: &ReplicationOpen,
    thread: &ThreadRef,
    local: &EndpointRef,
    remote_key: [u8; 32],
    admission: &BTreeSet<ThreadFacet>,
    sharing_policy_version: Vec<u8>,
) -> Result<ReplicationReady, Error> {
    validate_endpoint(local)?;
    if open.thread.as_ref() != Some(thread) || open.thread_genesis.is_some() {
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
    Ok(ReplicationReady {
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
    })
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
            .expect("authorized opening");
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
