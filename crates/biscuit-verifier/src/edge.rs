//! Transport-free edge extent authorization (weft#1055, weft#1056).
//!
//! Weft evaluates audience in Postgres, then attenuates a short-lived Biscuit
//! bound to one canonical **authorized extent set**. The DO/R2 edge verifies
//! that Biscuit on every request and serves only the exact extents it names.
//! Weft never touches the body bytes.
//!
//! # Why a check, not a fact
//!
//! Weft's attenuation carries the scope as a Datalog **check**, never as a
//! bare fact. `Authorizer::dump()` returns facts from every block in
//! randomized order (see the walk in [`crate::facts`]), so a bearer who holds
//! a valid token for audience A could append their own block asserting a scope
//! fact for audience B and win the coin flip. Checks compose the other way:
//! the verifier runs *every* block's checks, so an appended block can only
//! narrow authority. The edge injects the request's real parameters as
//! authorizer facts and weft's check decides.
//!
//! # Why the extent set is a digest, not N facts
//!
//! A clone plan can name thousands of extents. Binding the Biscuit to
//! `blake3(canonical_extent_set)` keeps the token O(1) while the extent set
//! travels beside it. The digest is pinned inside the unforgeable check, so a
//! tampered extent set fails authorization rather than widening it.
//!
//! # The single-audience invariant (weft#1056)
//!
//! [`EdgeExtentSet::canonical_bytes`] refuses to encode — and
//! [`EdgeExtentSet::decode_canonical`] refuses to accept — an extent whose
//! byte span is not **exactly tiled** by its own records. An authorized range
//! therefore contains the concatenated bytes of its records and nothing else:
//! no unselected object's bytes can ride along inside an authorized range.
//! Because weft resolves extents only from an already audience-filtered object
//! set, every record in an extent belongs to the requesting audience, so every
//! authorized extent is single-audience by construction. A future "coalesce
//! across small gaps" optimization cannot silently defeat this — it would
//! produce a span wider than its records and fail to encode.

use chrono::{DateTime, Utc};

use crate::{BiscuitError, PublicKey, biscuit_string};

/// Operation token the edge authorizes against. A token attenuated for edge
/// serving is useless for any other RPC, and every other RPC's token is
/// useless here.
pub const EDGE_SERVE_OPERATION: &str = "EdgeServeExtent";

/// Predicate the edge injects with the live request's parameters. Weft's
/// attenuation check reads it; nothing else in the rule pack emits it.
///
/// The verifier **reserves** this predicate: a token whose own blocks assert it
/// is rejected outright. Without that, a bearer could append a block asserting
/// the parameters their capability was scoped to, satisfy weft's check no
/// matter what the edge actually asked for, and then present a different
/// audience's extent set to the structural checks that follow.
pub const EDGE_REQUEST_PREDICATE: &str = "edge_extent_request_v1";

/// Marker identifying a first-party edge-serving attenuation block.
///
/// The delegation-chain walk otherwise requires every post-authority block to
/// be either a trusted third-party presence block or a proof-of-possession
/// delegation. This marker declares the third shape: a block that adds only
/// checks. Because a check can only narrow, such a block needs no signature —
/// but the verifier still requires it to carry nothing else.
pub const EDGE_ATTENUATION_FACT: &str = "weft_edge_serving_attenuation_v1";

const EXTENT_SET_MAGIC: &[u8; 4] = b"WEDG";
const EXTENT_SET_VERSION: u8 = 1;

/// Bound on a single authorized extent set. Keeps a hostile or buggy producer
/// from handing the edge an unbounded decode.
pub const MAX_EXTENTS: usize = 65_536;
/// Bound on records inside one extent.
pub const MAX_RECORDS_PER_EXTENT: usize = 65_536;
/// Bound on the content roots one capability may be derived from.
pub const MAX_CONTENT_ROOTS: usize = 4_096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EdgeError {
    #[error("edge extent set is invalid: {0}")]
    Invalid(String),
    #[error("edge extent set bound exceeded: {0}")]
    Bound(String),
    #[error("edge extent set integrity failure: {0}")]
    Integrity(String),
    #[error("edge request is not authorized: {0}")]
    Unauthorized(String),
}

pub type Result<T> = std::result::Result<T, EdgeError>;

impl From<BiscuitError> for EdgeError {
    fn from(error: BiscuitError) -> Self {
        // Every Biscuit failure mode collapses to "not authorized" at the
        // edge. Distinguishing parse from authorization here would hand a
        // prober a signal without helping a legitimate client.
        EdgeError::Unauthorized(error.to_string())
    }
}

/// Reader audience tier, canonicalized to the same strings the planner and the
/// CLI's `--audience` flag use, so a tier round-trips losslessly between
/// Postgres evaluation, the Biscuit check, and the edge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeAudience {
    Public,
    Internal,
    Team(String),
    Restricted(String),
}

impl EdgeAudience {
    /// Canonical `--audience`-form string.
    pub fn as_canonical(&self) -> String {
        match self {
            Self::Public => "public".to_string(),
            Self::Internal => "internal".to_string(),
            Self::Team(name) => format!("team:{name}"),
            Self::Restricted(label) => format!("restricted:{label}"),
        }
    }

    /// Parse the canonical form. Returns `None` for anything unrecognized —
    /// callers must treat that as "no audience", never as a default tier.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "internal" => Some(Self::Internal),
            other => {
                if let Some(name) = other.strip_prefix("team:") {
                    (!name.is_empty()).then(|| Self::Team(name.to_string()))
                } else if let Some(label) = other.strip_prefix("restricted:") {
                    (!label.is_empty()).then(|| Self::Restricted(label.to_string()))
                } else {
                    None
                }
            }
        }
    }

    fn tag(&self) -> u8 {
        match self {
            Self::Public => 0,
            Self::Internal => 1,
            Self::Team(_) => 2,
            Self::Restricted(_) => 3,
        }
    }
}

/// One object's physical placement inside an extent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeRecord {
    /// Wire object-type discriminant. Carried so the edge can reject a request
    /// whose declared object type disagrees with the authorized plan.
    pub object_type: u8,
    pub object_hash: [u8; 32],
    pub decoded_size: u64,
    /// Absolute offset in the pack, not relative to the extent.
    pub offset: u64,
    pub length: u64,
    /// BLAKE3 of this record's encoded bytes, so the edge (and the client) can
    /// verify each record without trusting the storage layer.
    pub encoded_digest: [u8; 32],
}

/// A contiguous authorized byte range of one pack version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeExtent {
    pub pack_id: [u8; 32],
    /// Pack version. The edge must issue its R2 read conditional on this ETag;
    /// a repacked object under a reused offset must not be served.
    pub etag: String,
    pub offset: u64,
    pub length: u64,
    /// Records tiling `[offset, offset + length)` exactly, offset-ordered.
    pub records: Vec<EdgeRecord>,
}

impl EdgeExtent {
    fn end(&self) -> Result<u64> {
        self.offset
            .checked_add(self.length)
            .ok_or_else(|| EdgeError::Bound("extent end overflows u64".to_string()))
    }

    /// The weft#1056 structural guard. An extent's span must be exactly the
    /// concatenation of its records — no leading gap, no interior gap, no
    /// trailing slack. A gap would mean authorizing bytes that belong to an
    /// object the audience filter did not select.
    fn validate_tiling(&self) -> Result<()> {
        if self.length == 0 {
            return Err(EdgeError::Invalid(
                "authorized extent must be non-empty".to_string(),
            ));
        }
        if self.records.is_empty() {
            return Err(EdgeError::Invalid(
                "authorized extent must name the records it covers".to_string(),
            ));
        }
        if self.records.len() > MAX_RECORDS_PER_EXTENT {
            return Err(EdgeError::Bound(format!(
                "extent names {} records, exceeding {MAX_RECORDS_PER_EXTENT}",
                self.records.len()
            )));
        }
        if self.etag.is_empty() {
            return Err(EdgeError::Invalid(
                "authorized extent must carry a pack ETag".to_string(),
            ));
        }
        let end = self.end()?;
        let mut cursor = self.offset;
        for record in &self.records {
            if record.length == 0 {
                return Err(EdgeError::Invalid(
                    "authorized record must be non-empty".to_string(),
                ));
            }
            if record.offset != cursor {
                return Err(EdgeError::Invalid(format!(
                    "extent at {} is not exactly tiled by its records: expected a record at {cursor}, found one at {}",
                    self.offset, record.offset
                )));
            }
            cursor = cursor
                .checked_add(record.length)
                .ok_or_else(|| EdgeError::Bound("record extent overflows u64".to_string()))?;
        }
        if cursor != end {
            return Err(EdgeError::Invalid(format!(
                "extent [{}, {end}) is not exactly tiled by its records: coverage ends at {cursor}",
                self.offset
            )));
        }
        Ok(())
    }
}

/// The canonical set of extents one attenuated Biscuit authorizes.
///
/// This is the whole authorization surface: the edge serves an extent iff it
/// appears here verbatim and the Biscuit's check pins this set's digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeExtentSet {
    pub repository_id: [u8; 16],
    /// Spool facet (`content`, `governance`, `membership`, `children`).
    pub facet: String,
    pub audience: EdgeAudience,
    /// Plan-manifest roots this capability was derived from. A request naming a
    /// root outside this list is refused even if its byte range happens to be
    /// authorized, so a capability minted for one content root cannot be
    /// replayed against another.
    pub content_roots: Vec<[u8; 32]>,
    pub extents: Vec<EdgeExtent>,
    pub issued_at_unix: u64,
    pub expires_at_unix: u64,
    pub nonce: [u8; 16],
}

impl EdgeExtentSet {
    /// Canonical bytes. Sorting and structural validation happen here, so a
    /// caller cannot produce two encodings of one logical set, and cannot
    /// produce any encoding of a set that violates the tiling invariant.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        if self.facet.is_empty() {
            return Err(EdgeError::Invalid(
                "edge extent set must name a facet".to_string(),
            ));
        }
        if self.expires_at_unix <= self.issued_at_unix {
            return Err(EdgeError::Invalid(
                "edge extent set expiry must follow its issue time".to_string(),
            ));
        }
        if self.extents.len() > MAX_EXTENTS {
            return Err(EdgeError::Bound(format!(
                "edge extent set names {} extents, exceeding {MAX_EXTENTS}",
                self.extents.len()
            )));
        }
        let mut extents = self.extents.clone();
        extents.sort_unstable_by(|left, right| {
            left.pack_id
                .cmp(&right.pack_id)
                .then_with(|| left.etag.cmp(&right.etag))
                .then_with(|| left.offset.cmp(&right.offset))
        });

        let mut bytes = Vec::new();
        bytes.extend_from_slice(EXTENT_SET_MAGIC);
        bytes.push(EXTENT_SET_VERSION);
        bytes.extend_from_slice(&self.repository_id);
        push_bounded_string(&mut bytes, &self.facet)?;
        bytes.push(self.audience.tag());
        match &self.audience {
            EdgeAudience::Public | EdgeAudience::Internal => {}
            EdgeAudience::Team(value) | EdgeAudience::Restricted(value) => {
                if value.is_empty() {
                    return Err(EdgeError::Invalid(
                        "scoped audience must name a team or label".to_string(),
                    ));
                }
                push_bounded_string(&mut bytes, value)?;
            }
        }
        // Sorted and deduplicated here so two callers listing the same roots in
        // different orders produce one digest, and so membership below is a
        // decision about the set rather than about a particular listing.
        let mut content_roots = self.content_roots.clone();
        content_roots.sort_unstable();
        content_roots.dedup();
        if content_roots.is_empty() {
            return Err(EdgeError::Invalid(
                "edge extent set must name the content root it was derived from".to_string(),
            ));
        }
        if content_roots.len() > MAX_CONTENT_ROOTS {
            return Err(EdgeError::Bound(format!(
                "edge extent set names {} content roots, exceeding {MAX_CONTENT_ROOTS}",
                content_roots.len()
            )));
        }
        let root_count = u16::try_from(content_roots.len())
            .map_err(|_| EdgeError::Bound("content root count exceeds u16".to_string()))?;
        bytes.extend_from_slice(&root_count.to_be_bytes());
        for root in &content_roots {
            bytes.extend_from_slice(root);
        }

        let count = u32::try_from(extents.len())
            .map_err(|_| EdgeError::Bound("extent count exceeds u32".to_string()))?;
        bytes.extend_from_slice(&count.to_be_bytes());

        let mut previous: Option<(&[u8; 32], &str, u64)> = None;
        for extent in &extents {
            extent.validate_tiling()?;
            if let Some((prior_pack, prior_etag, prior_end)) = previous
                && prior_pack == &extent.pack_id
            {
                if prior_etag != extent.etag {
                    return Err(EdgeError::Invalid(
                        "edge extent set mixes ETags for one pack".to_string(),
                    ));
                }
                if extent.offset < prior_end {
                    return Err(EdgeError::Invalid(
                        "edge extent set overlaps or repeats a range".to_string(),
                    ));
                }
            }
            bytes.extend_from_slice(&extent.pack_id);
            push_bounded_string(&mut bytes, &extent.etag)?;
            bytes.extend_from_slice(&extent.offset.to_be_bytes());
            bytes.extend_from_slice(&extent.length.to_be_bytes());
            let record_count = u32::try_from(extent.records.len())
                .map_err(|_| EdgeError::Bound("record count exceeds u32".to_string()))?;
            bytes.extend_from_slice(&record_count.to_be_bytes());
            for record in &extent.records {
                bytes.push(record.object_type);
                bytes.extend_from_slice(&record.object_hash);
                bytes.extend_from_slice(&record.decoded_size.to_be_bytes());
                bytes.extend_from_slice(&record.offset.to_be_bytes());
                bytes.extend_from_slice(&record.length.to_be_bytes());
                bytes.extend_from_slice(&record.encoded_digest);
            }
            previous = Some((&extent.pack_id, &extent.etag, extent.end()?));
        }
        bytes.extend_from_slice(&self.issued_at_unix.to_be_bytes());
        bytes.extend_from_slice(&self.expires_at_unix.to_be_bytes());
        bytes.extend_from_slice(&self.nonce);
        Ok(bytes)
    }

    /// BLAKE3 over the canonical bytes. This is what the Biscuit check pins.
    pub fn digest(&self) -> Result<[u8; 32]> {
        Ok(*blake3::hash(&self.canonical_bytes()?).as_bytes())
    }

    /// Decode, re-encode, and require byte equality, so a non-canonical
    /// encoding of an otherwise-valid set is rejected rather than accepted
    /// under a digest an attacker chose.
    pub fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        let mut cursor = Cursor::new(bytes);
        if cursor.take(4)? != EXTENT_SET_MAGIC {
            return Err(EdgeError::Invalid(
                "edge extent set magic mismatch".to_string(),
            ));
        }
        let version = cursor.u8()?;
        if version != EXTENT_SET_VERSION {
            return Err(EdgeError::Invalid(format!(
                "unsupported edge extent set version {version}"
            )));
        }
        let repository_id = cursor.array_16()?;
        let facet = cursor.bounded_string()?;
        let audience = match cursor.u8()? {
            0 => EdgeAudience::Public,
            1 => EdgeAudience::Internal,
            2 => EdgeAudience::Team(cursor.bounded_string()?),
            3 => EdgeAudience::Restricted(cursor.bounded_string()?),
            other => {
                return Err(EdgeError::Invalid(format!(
                    "unknown edge audience tag {other}"
                )));
            }
        };
        let root_count = usize::from(cursor.u16()?);
        if root_count > MAX_CONTENT_ROOTS {
            return Err(EdgeError::Bound(format!(
                "encoded content root count {root_count} exceeds {MAX_CONTENT_ROOTS}"
            )));
        }
        let mut content_roots = Vec::with_capacity(root_count.min(256));
        for _ in 0..root_count {
            content_roots.push(cursor.array_32()?);
        }
        let extent_count = cursor.u32()? as usize;
        if extent_count > MAX_EXTENTS {
            return Err(EdgeError::Bound(format!(
                "encoded extent count {extent_count} exceeds {MAX_EXTENTS}"
            )));
        }
        let mut extents = Vec::with_capacity(extent_count.min(1024));
        for _ in 0..extent_count {
            let pack_id = cursor.array_32()?;
            let etag = cursor.bounded_string()?;
            let offset = cursor.u64()?;
            let length = cursor.u64()?;
            let record_count = cursor.u32()? as usize;
            if record_count > MAX_RECORDS_PER_EXTENT {
                return Err(EdgeError::Bound(format!(
                    "encoded record count {record_count} exceeds {MAX_RECORDS_PER_EXTENT}"
                )));
            }
            let mut records = Vec::with_capacity(record_count.min(1024));
            for _ in 0..record_count {
                records.push(EdgeRecord {
                    object_type: cursor.u8()?,
                    object_hash: cursor.array_32()?,
                    decoded_size: cursor.u64()?,
                    offset: cursor.u64()?,
                    length: cursor.u64()?,
                    encoded_digest: cursor.array_32()?,
                });
            }
            extents.push(EdgeExtent {
                pack_id,
                etag,
                offset,
                length,
                records,
            });
        }
        let issued_at_unix = cursor.u64()?;
        let expires_at_unix = cursor.u64()?;
        let nonce = cursor.array_16()?;
        if !cursor.is_empty() {
            return Err(EdgeError::Invalid(
                "trailing bytes after edge extent set".to_string(),
            ));
        }
        let decoded = Self {
            repository_id,
            facet,
            audience,
            content_roots,
            extents,
            issued_at_unix,
            expires_at_unix,
            nonce,
        };
        if decoded.canonical_bytes()? != bytes {
            return Err(EdgeError::Invalid(
                "non-canonical edge extent set encoding".to_string(),
            ));
        }
        Ok(decoded)
    }

    /// Find the extent exactly matching a requested range.
    ///
    /// Exact match only: a sub-range, a super-range, or a whole-pack request
    /// finds nothing. There is deliberately no "contains" or "overlaps"
    /// variant — a partial match is the leak this module exists to prevent.
    fn exact_extent(
        &self,
        pack_id: &[u8; 32],
        etag: &str,
        offset: u64,
        length: u64,
    ) -> Option<&EdgeExtent> {
        let mut found = self.extents.iter().filter(|extent| {
            &extent.pack_id == pack_id
                && extent.etag == etag
                && extent.offset == offset
                && extent.length == length
        });
        match (found.next(), found.next()) {
            (Some(extent), None) => Some(extent),
            // Zero matches is an unauthorized range. Two is a malformed set
            // that canonical encoding should already have rejected; treat the
            // ambiguity as a refusal rather than picking one.
            _ => None,
        }
    }
}

/// What the edge is being asked to serve, taken from the live request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeExtentRequest {
    pub repository_id: [u8; 16],
    pub facet: String,
    pub audience: EdgeAudience,
    /// Plan-manifest root the caller claims to be reconstructing.
    pub content_root: [u8; 32],
    pub pack_id: [u8; 32],
    pub etag: String,
    pub offset: u64,
    pub length: u64,
}

/// A verified authorization to serve exactly one extent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedExtent {
    /// The byte range the edge may read. Nothing outside it is authorized.
    pub extent: EdgeExtent,
    /// Subject the capability resolved to, for the edge's audit log.
    pub subject: String,
    pub audience: EdgeAudience,
}

/// The scope weft pins into its attenuation block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeServingScope {
    pub repository_id: [u8; 16],
    pub facet: String,
    pub audience: EdgeAudience,
    /// `blake3` over [`EdgeExtentSet::canonical_bytes`].
    pub extent_set_digest: [u8; 32],
    pub expires_at: DateTime<Utc>,
}

impl EdgeServingScope {
    /// Datalog check pinning every scope field. Weft appends this; the edge
    /// injects [`EDGE_REQUEST_PREDICATE`] from the live request and the
    /// verifier decides.
    ///
    /// Because this is a check rather than a fact, a bearer who appends their
    /// own block cannot relax it — the verifier runs every block's checks.
    pub fn datalog_check(&self) -> String {
        format!(
            "check if {EDGE_REQUEST_PREDICATE}($repo, $facet, $audience, $digest), \
             $repo == {repo}, $facet == {facet}, $audience == {audience}, $digest == {digest}",
            repo = biscuit_string(&hex::encode(self.repository_id)),
            facet = biscuit_string(&self.facet),
            audience = biscuit_string(&self.audience.as_canonical()),
            digest = biscuit_string(&hex::encode(self.extent_set_digest)),
        )
    }

    /// Expiry check. Separate from [`Self::datalog_check`] so the two failures
    /// stay distinguishable in the verifier's error text.
    pub fn expiry_check(&self) -> String {
        format!(
            "check if time($now), $now < {}",
            self.expires_at.to_rfc3339()
        )
    }

    /// Marker fact declaring this block a first-party checks-only narrowing.
    /// Its only term is the format version — everything that matters is in the
    /// checks, and a scope term here would be a fact a bearer could try to
    /// reuse.
    pub fn marker_fact(&self) -> String {
        format!("{EDGE_ATTENUATION_FACT}({EXTENT_SET_VERSION})")
    }

    /// Operation ceiling: this token authorizes edge extent serving and
    /// nothing else.
    pub fn operation_check(&self) -> String {
        format!(
            "check if operation($op), $op == {}",
            biscuit_string(EDGE_SERVE_OPERATION)
        )
    }
}

/// The edge's single entry point. Fails closed on every path.
///
/// `extent_set_bytes` is the canonical extent set that travelled beside the
/// token; `request` is what the caller is actually asking for. The two are
/// tied together by the digest pinned in the token's check, so a swapped or
/// edited extent set fails authorization instead of widening it.
#[allow(clippy::too_many_arguments)]
pub fn authorize_extent_request(
    token_b64: &str,
    grant_envelope_b64: Option<&str>,
    trust_list: &[PublicKey],
    trusted_presence_signers: &[PublicKey],
    extent_set_bytes: &[u8],
    request: &EdgeExtentRequest,
    now: DateTime<Utc>,
) -> Result<AuthorizedExtent> {
    // 1. Decode the extent set canonically. This runs the weft#1056 tiling
    //    invariant before anything is authorized, so a malformed set is
    //    rejected even if its digest were somehow pinned.
    let extent_set = EdgeExtentSet::decode_canonical(extent_set_bytes)?;

    // 2. Digest the bytes as received, not a re-encoding of the decoded value.
    let digest = hex::encode(blake3::hash(extent_set_bytes).as_bytes());

    // 3. Run the Biscuit with the live request injected. Weft's check pins
    //    repository, facet, audience, and the extent-set digest; a mismatch on
    //    any of them fails here.
    let request_fact = format!(
        "{EDGE_REQUEST_PREDICATE}({}, {}, {}, {})",
        biscuit_string(&hex::encode(request.repository_id)),
        biscuit_string(&request.facet),
        biscuit_string(&request.audience.as_canonical()),
        biscuit_string(&digest),
    );
    let facts = crate::verify_any_at_with_extra_facts(
        token_b64,
        grant_envelope_b64,
        trust_list,
        trusted_presence_signers,
        EDGE_SERVE_OPERATION,
        None,
        &[request_fact],
        now,
    )?;

    // 4. The extent set's own header must agree with the request. The check
    //    above already pins these; re-checking here means a future change to
    //    either side cannot silently drift them apart.
    if extent_set.repository_id != request.repository_id {
        return Err(EdgeError::Unauthorized(
            "extent set is scoped to a different repository".to_string(),
        ));
    }
    if extent_set.facet != request.facet {
        return Err(EdgeError::Unauthorized(
            "extent set is scoped to a different facet".to_string(),
        ));
    }
    if extent_set.audience != request.audience {
        return Err(EdgeError::Unauthorized(
            "extent set is scoped to a different audience".to_string(),
        ));
    }
    if !extent_set.content_roots.contains(&request.content_root) {
        return Err(EdgeError::Unauthorized(format!(
            "extent set does not authorize content root {}",
            hex::encode(request.content_root)
        )));
    }

    // 5. Independent expiry on the extent set itself. The token's own expiry
    //    check has already run; this bounds the set even if a token were
    //    minted with a longer life than the set it names.
    let now_unix = u64::try_from(now.timestamp())
        .map_err(|_| EdgeError::Unauthorized("edge clock is before the epoch".to_string()))?;
    if now_unix >= extent_set.expires_at_unix {
        return Err(EdgeError::Unauthorized(format!(
            "extent set expired at {} (now {now_unix})",
            extent_set.expires_at_unix
        )));
    }

    // 6. Exact extent match. A whole-pack request, a widened range, or a
    //    sub-range of an authorized extent all land here and are refused.
    let extent = extent_set
        .exact_extent(
            &request.pack_id,
            &request.etag,
            request.offset,
            request.length,
        )
        .ok_or_else(|| {
            EdgeError::Unauthorized(format!(
                "no authorized extent matches pack {} range [{}, {})",
                hex::encode(request.pack_id),
                request.offset,
                request.offset.saturating_add(request.length),
            ))
        })?;

    Ok(AuthorizedExtent {
        extent: extent.clone(),
        subject: facts.sub,
        audience: extent_set.audience,
    })
}

fn push_bounded_string(bytes: &mut Vec<u8>, value: &str) -> Result<()> {
    let length = u16::try_from(value.len())
        .map_err(|_| EdgeError::Bound("edge extent set string exceeds u16".to_string()))?;
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(count)
            .ok_or_else(|| EdgeError::Invalid("edge extent set offset overflow".to_string()))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| EdgeError::Invalid("truncated edge extent set".to_string()))?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().map_err(
            |_| EdgeError::Invalid("invalid u16 field".to_string()),
        )?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(
            |_| EdgeError::Invalid("invalid u32 field".to_string()),
        )?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.take(8)?.try_into().map_err(
            |_| EdgeError::Invalid("invalid u64 field".to_string()),
        )?))
    }

    fn array_16(&mut self) -> Result<[u8; 16]> {
        self.take(16)?
            .try_into()
            .map_err(|_| EdgeError::Invalid("invalid 16-byte field".to_string()))
    }

    fn array_32(&mut self) -> Result<[u8; 32]> {
        self.take(32)?
            .try_into()
            .map_err(|_| EdgeError::Invalid("invalid 32-byte field".to_string()))
    }

    fn bounded_string(&mut self) -> Result<String> {
        let length = usize::from(self.u16()?);
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| EdgeError::Invalid("edge extent set string is not UTF-8".to_string()))
    }

    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seed: u8, offset: u64, length: u64) -> EdgeRecord {
        EdgeRecord {
            object_type: 0,
            object_hash: [seed; 32],
            decoded_size: length * 2,
            offset,
            length,
            encoded_digest: [seed.wrapping_add(100); 32],
        }
    }

    /// Two adjacent records tiling `[100, 130)` exactly — the shape
    /// `resolve_pack_read_plan_on` produces when both objects passed the
    /// audience filter and are physically adjacent in the pack.
    fn tiled_extent() -> EdgeExtent {
        EdgeExtent {
            pack_id: [7; 32],
            etag: "etag-v1".to_string(),
            offset: 100,
            length: 30,
            records: vec![record(1, 100, 10), record(2, 110, 20)],
        }
    }

    fn extent_set(audience: EdgeAudience, extents: Vec<EdgeExtent>) -> EdgeExtentSet {
        EdgeExtentSet {
            repository_id: [5; 16],
            facet: "content".to_string(),
            audience,
            content_roots: vec![[9; 32]],
            extents,
            issued_at_unix: 1_000,
            expires_at_unix: 2_000,
            nonce: [6; 16],
        }
    }

    // ---------------------------------------------------------------
    // The weft#1056 property: an authorized range contains exactly its
    // own records' bytes and nothing else.
    // ---------------------------------------------------------------

    #[test]
    fn an_authorized_range_is_exactly_tiled_by_its_own_records() {
        extent_set(EdgeAudience::Internal, vec![tiled_extent()])
            .canonical_bytes()
            .expect("adjacent selected records tile their coalesced range exactly");
    }

    #[test]
    fn a_range_wider_than_its_records_cannot_be_authorized() {
        // The leak this module exists to prevent: a range spanning [100, 140)
        // whose records only cover [100, 130) would hand the caller ten bytes
        // belonging to an object the audience filter did not select.
        let mut extent = tiled_extent();
        extent.length = 40;
        let error = extent_set(EdgeAudience::Internal, vec![extent])
            .canonical_bytes()
            .expect_err("a range wider than its records must not encode");
        assert!(
            matches!(&error, EdgeError::Invalid(message) if message.contains("exactly tiled")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_range_with_an_interior_gap_cannot_be_authorized() {
        // The gap is where an unselected — possibly other-audience — object
        // lives. Coalescing across it is exactly the optimization that would
        // silently defeat the invariant.
        let extent = EdgeExtent {
            records: vec![record(1, 100, 10), record(2, 115, 15)],
            ..tiled_extent()
        };
        let error = extent_set(EdgeAudience::Internal, vec![extent])
            .canonical_bytes()
            .expect_err("a range with an unselected interior gap must not encode");
        assert!(
            matches!(&error, EdgeError::Invalid(message) if message.contains("exactly tiled")),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn a_range_that_names_no_records_cannot_be_authorized() {
        let extent = EdgeExtent {
            records: Vec::new(),
            ..tiled_extent()
        };
        assert!(
            extent_set(EdgeAudience::Internal, vec![extent])
                .canonical_bytes()
                .is_err(),
            "an extent that names no records authorizes bytes with no owner"
        );
    }

    #[test]
    fn overlapping_extents_in_one_pack_cannot_be_authorized() {
        let second = EdgeExtent {
            offset: 120,
            length: 30,
            records: vec![record(3, 120, 30)],
            ..tiled_extent()
        };
        assert!(
            extent_set(EdgeAudience::Internal, vec![tiled_extent(), second])
                .canonical_bytes()
                .is_err(),
            "overlapping authorized ranges make the tiling argument unsound"
        );
    }

    // ---------------------------------------------------------------
    // Canonical encoding: one logical set has exactly one digest.
    // ---------------------------------------------------------------

    #[test]
    fn extent_order_does_not_change_the_digest() {
        let low = tiled_extent();
        let high = EdgeExtent {
            offset: 200,
            length: 12,
            records: vec![record(4, 200, 12)],
            ..tiled_extent()
        };
        let forward = extent_set(EdgeAudience::Internal, vec![low.clone(), high.clone()]);
        let reverse = extent_set(EdgeAudience::Internal, vec![high, low]);
        assert_eq!(
            forward.digest().unwrap(),
            reverse.digest().unwrap(),
            "a caller must not be able to mint two digests for one authorization"
        );
    }

    #[test]
    fn content_root_order_and_duplicates_do_not_change_the_digest() {
        let mut ordered = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        ordered.content_roots = vec![[1; 32], [2; 32]];
        let mut shuffled = ordered.clone();
        shuffled.content_roots = vec![[2; 32], [1; 32], [2; 32]];
        assert_eq!(ordered.digest().unwrap(), shuffled.digest().unwrap());
    }

    #[test]
    fn every_authorization_relevant_field_changes_the_digest() {
        let base = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        let baseline = base.digest().unwrap();

        let mut audience = base.clone();
        audience.audience = EdgeAudience::Team("core".to_string());
        let mut facet = base.clone();
        facet.facet = "governance".to_string();
        let mut repository = base.clone();
        repository.repository_id = [11; 16];
        let mut root = base.clone();
        root.content_roots = vec![[12; 32]];
        let mut object_type = base.clone();
        object_type.extents[0].records[0].object_type = 1;
        let mut object_hash = base.clone();
        object_hash.extents[0].records[0].object_hash = [13; 32];
        let mut etag = base.clone();
        etag.extents[0].etag = "etag-v2".to_string();
        let mut expiry = base.clone();
        expiry.expires_at_unix = 3_000;
        let mut nonce = base.clone();
        nonce.nonce = [14; 16];

        for (label, mutated) in [
            ("audience", audience),
            ("facet", facet),
            ("repository", repository),
            ("content root", root),
            ("record object type", object_type),
            ("record object hash", object_hash),
            ("pack etag", etag),
            ("expiry", expiry),
            ("nonce", nonce),
        ] {
            assert_ne!(
                baseline,
                mutated.digest().unwrap(),
                "{label} must be inside the digest the capability pins"
            );
        }
    }

    #[test]
    fn canonical_bytes_round_trip_through_decode() {
        let set = extent_set(EdgeAudience::Team("core".to_string()), vec![tiled_extent()]);
        let bytes = set.canonical_bytes().unwrap();
        assert_eq!(EdgeExtentSet::decode_canonical(&bytes).unwrap(), set);
    }

    #[test]
    fn decode_rejects_a_non_canonical_encoding_of_a_valid_set() {
        // Re-serializing with the extents in a non-canonical order yields the
        // same logical set under a different digest. Accepting it would let a
        // caller choose which digest their bytes hash to.
        let set = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        let mut bytes = set.canonical_bytes().unwrap();
        bytes.push(0);
        assert!(EdgeExtentSet::decode_canonical(&bytes).is_err());
    }

    #[test]
    fn decode_rejects_a_truncated_set_rather_than_reading_past_the_end() {
        let set = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        let bytes = set.canonical_bytes().unwrap();
        for cut in 1..bytes.len() {
            assert!(
                EdgeExtentSet::decode_canonical(&bytes[..cut]).is_err(),
                "truncation at {cut} must fail closed"
            );
        }
    }

    #[test]
    fn decode_enforces_the_tiling_invariant_on_bytes_it_did_not_produce() {
        // The guard has to hold against an encoder that is not ours. Build a
        // valid encoding, then widen the extent length in place; the record
        // partition no longer covers the span.
        let set = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        let bytes = set.canonical_bytes().unwrap();
        let widened = bytes
            .windows(8)
            .position(|window| window == 30_u64.to_be_bytes())
            .expect("the extent length must appear in the encoding");
        let mut tampered = bytes.clone();
        tampered[widened..widened + 8].copy_from_slice(&40_u64.to_be_bytes());
        assert!(
            EdgeExtentSet::decode_canonical(&tampered).is_err(),
            "a hand-widened range must not decode"
        );
    }

    #[test]
    fn an_expiry_at_or_before_issue_cannot_be_authorized() {
        let mut set = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        set.expires_at_unix = set.issued_at_unix;
        assert!(set.canonical_bytes().is_err());
    }

    #[test]
    fn a_set_naming_no_content_root_cannot_be_authorized() {
        let mut set = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        set.content_roots.clear();
        assert!(
            set.canonical_bytes().is_err(),
            "an unrooted capability could be replayed against any content root"
        );
    }

    // ---------------------------------------------------------------
    // Audience canonicalization round-trips losslessly.
    // ---------------------------------------------------------------

    #[test]
    fn audience_tiers_round_trip_through_their_canonical_form() {
        for audience in [
            EdgeAudience::Public,
            EdgeAudience::Internal,
            EdgeAudience::Team("core".to_string()),
            EdgeAudience::Restricted("legal-hold".to_string()),
        ] {
            assert_eq!(
                EdgeAudience::parse(&audience.as_canonical()),
                Some(audience.clone()),
                "{audience:?} must survive the Postgres → biscuit → edge trip"
            );
        }
    }

    #[test]
    fn an_unrecognized_audience_string_is_not_a_default_tier() {
        for value in ["", "team:", "restricted:", "admin", "TEAM:core", "public "] {
            assert_eq!(
                EdgeAudience::parse(value),
                None,
                "{value:?} must not resolve to any tier"
            );
        }
    }

    // ---------------------------------------------------------------
    // Exact-match extent lookup.
    // ---------------------------------------------------------------

    #[test]
    fn only_the_exact_authorized_range_matches() {
        let set = extent_set(EdgeAudience::Internal, vec![tiled_extent()]);
        let pack = [7_u8; 32];
        assert!(set.exact_extent(&pack, "etag-v1", 100, 30).is_some());

        // A whole-pack request, a widened range, a sub-range, a shifted range,
        // a different pack version, and a different pack all miss.
        for (label, offset, length) in [
            ("whole pack", 0, u64::MAX),
            ("widened", 100, 40),
            ("sub-range", 100, 10),
            ("shifted", 104, 30),
        ] {
            assert!(
                set.exact_extent(&pack, "etag-v1", offset, length).is_none(),
                "{label} must not match an authorized extent"
            );
        }
        assert!(
            set.exact_extent(&pack, "etag-v2", 100, 30).is_none(),
            "a repacked object under a reused offset must not be served"
        );
        assert!(set.exact_extent(&[8; 32], "etag-v1", 100, 30).is_none());
    }
}
