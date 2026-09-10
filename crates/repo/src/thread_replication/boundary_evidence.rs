//! Exact acceptance evidence retained atomically with an independently verified
//! admission. The canonical receipt is the lookup key; no issuer is enrolled.
use crypto::original_boundary_acceptance::SignedBoundaryAcceptance;
use objects::object::{
    ContentHash,
    original_boundary_acceptance::{AdmissionBasis, FORMAT},
};
use rusqlite::{Connection, OptionalExtension, params};

use super::{Error, Result};

pub(super) const SCHEMA:&str="
CREATE TABLE IF NOT EXISTS boundary_acceptances(id BLOB PRIMARY KEY CHECK(length(id)=32),canonical BLOB NOT NULL CHECK(length(canonical)<=98304),signature BLOB NOT NULL CHECK(length(signature)=64));
CREATE TABLE IF NOT EXISTS boundary_admission_evidence(receipt BLOB PRIMARY KEY CHECK(length(receipt)<=2048),acceptance BLOB NOT NULL CHECK(length(acceptance)=32) REFERENCES boundary_acceptances(id));";

pub(super) fn persist(
    connection: &Connection,
    receipt: &[u8],
    evidence: Option<&SignedBoundaryAcceptance>,
) -> Result<()> {
    let Some(evidence) = evidence else {
        return Ok(());
    };
    let value = evidence.verify_signature()?;
    let id = value.id()?;
    connection.execute(
        "INSERT OR IGNORE INTO boundary_acceptances(id,canonical,signature) VALUES(?1,?2,?3)",
        params![id.as_bytes(), evidence.canonical, evidence.signature],
    )?;
    let prior: (Vec<u8>, Vec<u8>) = connection.query_row(
        "SELECT canonical,signature FROM boundary_acceptances WHERE id=?1",
        [id.as_bytes()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if prior.0 != evidence.canonical || prior.1 != evidence.signature {
        return Err(Error::Invalid(
            "conflicting stored boundary evidence".into(),
        ));
    }
    connection.execute(
        "INSERT OR IGNORE INTO boundary_admission_evidence(receipt,acceptance) VALUES(?1,?2)",
        params![receipt, id.as_bytes()],
    )?;
    let prior: Vec<u8> = connection.query_row(
        "SELECT acceptance FROM boundary_admission_evidence WHERE receipt=?1",
        [receipt],
        |row| row.get(0),
    )?;
    if prior != id.as_bytes() {
        return Err(Error::Invalid(
            "receipt boundary evidence is immutable".into(),
        ));
    }
    Ok(())
}
pub(super) fn load(
    connection: &Connection,
    receipt: &[u8],
    basis: &AdmissionBasis,
) -> Result<Option<std::sync::Arc<SignedBoundaryAcceptance>>> {
    let row:Option<(Vec<u8>,Vec<u8>)>=connection.query_row("SELECT a.canonical,a.signature FROM boundary_admission_evidence e JOIN boundary_acceptances a ON a.id=e.acceptance WHERE e.receipt=?1",[receipt],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
    matched(row, basis)
}
pub(super) fn matched(
    row: Option<(Vec<u8>, Vec<u8>)>,
    basis: &AdmissionBasis,
) -> Result<Option<std::sync::Arc<SignedBoundaryAcceptance>>> {
    match (basis, row) {
        (AdmissionBasis::OriginalAuthority, None) => Ok(None),
        (AdmissionBasis::BoundaryAcceptance { acceptance }, Some((canonical, signature))) => {
            let signed = SignedBoundaryAcceptance {
                canonical,
                signature,
            };
            let value = signed.verify_signature()?;
            if value.id()? != *acceptance {
                return Err(Error::Invalid(
                    "stored receipt acceptance digest differs".into(),
                ));
            }
            Ok(Some(std::sync::Arc::new(signed)))
        }
        _ => Err(Error::Invalid(
            "stored receipt lacks exact boundary evidence".into(),
        )),
    }
}
pub(super) fn wire(
    evidence: &SignedBoundaryAcceptance,
) -> Result<api::heddle::api::v2alpha1::SignedRecord> {
    let value = evidence.verify_signature()?;
    Ok(api::heddle::api::v2alpha1::SignedRecord {
        format: FORMAT.into(),
        canonical_record: evidence.canonical.clone(),
        signatures: vec![api::heddle::api::v2alpha1::RecordSignature {
            public_key: value.accepting_publisher.to_vec(),
            signature: evidence.signature.clone(),
        }],
    })
}
pub(super) fn add_wire(
    output: &mut std::collections::BTreeMap<ContentHash, api::heddle::api::v2alpha1::SignedRecord>,
    evidence: Option<&SignedBoundaryAcceptance>,
) -> Result<()> {
    if let Some(evidence) = evidence {
        let id = ContentHash::compute_typed(FORMAT, &evidence.canonical);
        if let Some(prior) = output.get(&id) {
            if prior.canonical_record != evidence.canonical
                || prior
                    .signatures
                    .first()
                    .is_none_or(|signature| signature.signature != evidence.signature)
            {
                return Err(Error::Invalid(
                    "conflicting exported boundary evidence".into(),
                ));
            }
        } else {
            output.insert(id, wire(evidence)?);
        }
    }
    Ok(())
}
/// One DISTINCT-key lookup after bounded ancestry rows. Check declared column
/// lengths before copying canonical bytes; every shared acceptance is owned once.
pub(super) fn load_many(
    connection: &Connection,
    ids: &std::collections::BTreeSet<ContentHash>,
    max_bytes: usize,
) -> Result<std::collections::BTreeMap<ContentHash, std::sync::Arc<SignedBoundaryAcceptance>>> {
    if ids.len() > 128 {
        return Err(Error::Invalid("boundary evidence count exceeded".into()));
    }
    let mut output = std::collections::BTreeMap::new();
    if ids.is_empty() {
        return Ok(output);
    }
    let marks = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut statement=connection.prepare(&format!("SELECT id,length(canonical),length(signature),canonical,signature FROM boundary_acceptances WHERE id IN({marks})"))?;
    let mut rows = statement.query(rusqlite::params_from_iter(
        ids.iter().map(|id| id.as_bytes().as_slice()),
    ))?;
    let mut bytes = 0usize;
    while let Some(row) = rows.next()? {
        let canonical_len = usize::try_from(row.get::<_, i64>(1)?)
            .map_err(|_| Error::Invalid("invalid evidence length".into()))?;
        let signature_len = usize::try_from(row.get::<_, i64>(2)?)
            .map_err(|_| Error::Invalid("invalid evidence signature length".into()))?;
        bytes = bytes
            .checked_add(canonical_len)
            .and_then(|n| n.checked_add(signature_len))
            .ok_or_else(|| Error::Invalid("boundary evidence byte overflow".into()))?;
        if canonical_len > 96 * 1024 || signature_len != 64 || bytes > max_bytes {
            return Err(Error::Invalid(
                "source boundary evidence exceeds transfer budget".into(),
            ));
        }
        let id = super::hash(&row.get::<_, Vec<u8>>(0)?)?;
        let signed = SignedBoundaryAcceptance {
            canonical: row.get(3)?,
            signature: row.get(4)?,
        };
        if signed.verify_signature()?.id()? != id {
            return Err(Error::Invalid("stored acceptance identity differs".into()));
        }
        output.insert(id, std::sync::Arc::new(signed));
    }
    if output.len() != ids.len() {
        return Err(Error::Invalid("missing retained boundary evidence".into()));
    }
    Ok(output)
}
