// SPDX-License-Identifier: Apache-2.0
//! One signature over an import, instead of an authorship signature per state.
//!
//! The publisher attests "I imported exactly this": a manifest that names the
//! source and the import parameters and commits to a Merkle root. Each leaf is
//! one imported state on one thread, including its Git object id when the
//! import map has one. A state is admitted only when an inclusion proof folds
//! to that signed root.
//!
//! # Manifest bytes
//!
//! ```text
//! version            u8 = 1
//! publisher          32 bytes
//! source_kind        u8     1 = local path, 2 = git URL
//! source_identity    u32 length + UTF-8
//! parameters         u32 length + bytes (see [`import_parameters`])
//! state_count        u64 little-endian
//! merkle_root        32 bytes
//! ```
//!
//! The Ed25519 signature covers `heddle.import.batch.v1`, a zero byte, then
//! those manifest bytes. It is not a signature over a thread operation, and it
//! does not mean the publisher authored the imported states.
//!
//! # Leaf
//!
//! `heddle.import.leaf.v1` over:
//!
//! ```text
//! thread_id     32 bytes
//! state_id      32 bytes
//! git_oid_len   u8
//! git_oid       raw bytes (empty when the state has no Git commit)
//! parent_count  u32 little-endian
//! parent_ids    parent_count × 32 bytes, in the state's parent order
//! ```
//!
//! Leaves are sorted by `(thread_id, state_id)` before the tree is built.
//! Internal nodes are `heddle.import.node.v1` over `left || right`. A level
//! with an odd trailing node promotes that node unchanged.
//!
//! # Envelope stored on each operation
//!
//! The operation row keeps its canonical bytes, so operation ids and parent
//! links do not change. The signature column holds:
//!
//! ```text
//! magic            "HBIM"
//! version          u8 = 1
//! manifest_len     u32
//! manifest         bytes
//! manifest_sig     64 bytes
//! git_oid_len      u8
//! git_oid          bytes
//! leaf_index       u32
//! sibling_count    u8
//! siblings         sibling_count × 32 bytes, leaf level first
//! ```
//!
//! Verifying one state recomputes the leaf from the operation and the Git id
//! in the envelope, folds the siblings using the manifest's state count, and
//! checks the manifest signature. The same check accepts the whole batch by
//! recomputing the root from every leaf.

use std::collections::HashMap;

use heddle_object_model::object::{ContentHash, thread_replication::ThreadOperation};

use crate::{Ed25519Signer, Signer, SignerError, thread_operation::Error as OperationError};

/// Envelope discriminator. Longer than an Ed25519 signature, so a 64-byte
/// authorship signature is never read as a batch.
pub const IMPORT_BATCH_MAGIC: &[u8; 4] = b"HBIM";
pub const IMPORT_BATCH_VERSION: u8 = 1;
pub const IMPORT_BATCH_DOMAIN: &str = "heddle.import.batch.v1";
pub const IMPORT_LEAF_DOMAIN: &str = "heddle.import.leaf.v1";
pub const IMPORT_NODE_DOMAIN: &str = "heddle.import.node.v1";
/// Source identity is a filesystem path (local `import local`).
pub const IMPORT_SOURCE_LOCAL_PATH: u8 = 1;
/// Source identity is a Git remote URL.
pub const IMPORT_SOURCE_GIT_URL: u8 = 2;

/// One imported state, in the form the Merkle leaf commits to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportLeaf {
    pub thread: [u8; 32],
    pub state_id: [u8; 32],
    /// Raw Git commit id. Empty when this state did not come from a mapped commit.
    pub git_oid: Vec<u8>,
    /// State parent ids, in the order stored on the state.
    pub parents: Vec<[u8; 32]>,
}

/// What the single signature commits to besides the Merkle root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportBatchCommitment {
    pub source_kind: u8,
    pub source_identity: String,
    pub parameters: Vec<u8>,
}

/// A signed import manifest plus the tree needed to prove each leaf.
#[derive(Clone, Debug)]
pub struct SignedImportBatch {
    manifest: Vec<u8>,
    signature: [u8; 64],
    leaf_count: usize,
    levels: Vec<Vec<[u8; 32]>>,
    index: HashMap<([u8; 32], [u8; 32]), LeafPlace>,
}

#[derive(Clone, Debug)]
struct LeafPlace {
    index: u32,
    git_oid: Vec<u8>,
}

/// Canonical parameters: version 1, then the sorted unique thread names admitted
/// together. Each name is a `u32` length and UTF-8 bytes. Commit ids live in
/// the leaves; lossy translation is inside each state.
pub fn import_parameters(thread_names: &[&str]) -> Result<Vec<u8>, SignerError> {
    let mut names: Vec<&str> = thread_names.to_vec();
    names.sort_unstable();
    names.dedup();
    let mut out = Vec::new();
    out.push(IMPORT_BATCH_VERSION);
    let count = u32::try_from(names.len())
        .map_err(|_| SignerError::InvalidSignature("too many import threads".into()))?;
    out.extend_from_slice(&count.to_le_bytes());
    for name in names {
        push_counted(&mut out, name.as_bytes())?;
    }
    Ok(out)
}

/// Sign one manifest. Refuses an empty batch. Leaf order in the returned tree
/// is `(thread, state_id)`, independent of the input order.
pub fn sign_import_batch(
    signer: &Ed25519Signer,
    commitment: &ImportBatchCommitment,
    mut leaves: Vec<ImportLeaf>,
) -> Result<SignedImportBatch, SignerError> {
    if leaves.is_empty() {
        return Err(SignerError::InvalidSignature(
            "import batch has no states".into(),
        ));
    }
    if signer.public_key().len() != 32 {
        return Err(SignerError::InvalidPublicKey(
            "publisher key must be 32 bytes".into(),
        ));
    }
    validate_commitment(commitment)?;
    leaves.sort_by(|left, right| {
        (&left.thread, &left.state_id).cmp(&(&right.thread, &right.state_id))
    });
    for pair in leaves.windows(2) {
        if pair[0].thread == pair[1].thread && pair[0].state_id == pair[1].state_id {
            return Err(SignerError::InvalidSignature(
                "import batch lists one state twice on the same thread".into(),
            ));
        }
    }
    let mut hashes = Vec::with_capacity(leaves.len());
    let mut index = HashMap::with_capacity(leaves.len());
    for (position, leaf) in leaves.iter().enumerate() {
        hashes.push(leaf_hash(leaf)?);
        let position = u32::try_from(position)
            .map_err(|_| SignerError::InvalidSignature("import batch is too large".into()))?;
        index.insert(
            (leaf.thread, leaf.state_id),
            LeafPlace {
                index: position,
                git_oid: leaf.git_oid.clone(),
            },
        );
    }
    let levels = build_levels(hashes);
    let root = *levels
        .last()
        .and_then(|level| level.first())
        .ok_or_else(|| SignerError::InvalidSignature("import batch has no root".into()))?;
    let mut publisher = [0u8; 32];
    publisher.copy_from_slice(signer.public_key());
    let manifest = manifest_bytes(&publisher, commitment, leaves.len() as u64, &root)?;
    let signature = signer.sign(&signing_bytes(&manifest))?;
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| SignerError::InvalidSignature("Ed25519 signature must be 64 bytes".into()))?;
    let batch = SignedImportBatch {
        manifest,
        signature,
        leaf_count: leaves.len(),
        levels,
        index,
    };
    batch.verify_manifest()?;
    Ok(batch)
}

impl SignedImportBatch {
    /// The one Ed25519 signature over the manifest.
    pub fn manifest_signature(&self) -> &[u8; 64] {
        &self.signature
    }

    pub fn manifest(&self) -> &[u8] {
        &self.manifest
    }

    /// Confirm the manifest signature before any state is inserted.
    pub fn verify_manifest(&self) -> Result<(), SignerError> {
        let parsed = parse_manifest(&self.manifest)?;
        let root = self
            .levels
            .last()
            .and_then(|level| level.first())
            .copied()
            .ok_or(SignerError::VerificationFailed)?;
        if parsed.root != root || parsed.count as usize != self.leaf_count {
            return Err(SignerError::VerificationFailed);
        }
        Ed25519Signer::verify_with_public_key(
            &signing_bytes(&self.manifest),
            &parsed.publisher,
            &self.signature,
        )
    }

    pub fn covers(&self, thread: &[u8; 32], state_id: &[u8; 32]) -> bool {
        self.index.contains_key(&(*thread, *state_id))
    }

    /// Inclusion proof bound to this manifest. Fails when the state is not a leaf.
    pub fn envelope(&self, thread: &[u8; 32], state_id: &[u8; 32]) -> Result<Vec<u8>, SignerError> {
        let place = self.index.get(&(*thread, *state_id)).ok_or_else(|| {
            SignerError::InvalidSignature("state is not in the import batch".into())
        })?;
        let siblings = siblings_for(&self.levels, self.leaf_count, place.index as usize)?;
        encode_envelope(
            &self.manifest,
            &self.signature,
            &place.git_oid,
            place.index,
            &siblings,
        )
    }
}

/// `Some` when `signature` is an import-batch envelope. `Err` means the
/// envelope is present and does not cover this operation.
pub fn verify_batch_operation(
    canonical: &[u8],
    signature: &[u8],
) -> Option<Result<ThreadOperation, OperationError>> {
    if signature.len() == 64 || !signature.starts_with(IMPORT_BATCH_MAGIC) {
        return None;
    }
    Some(verify_covered_operation(canonical, signature))
}

pub fn is_import_batch_signature(signature: &[u8]) -> bool {
    signature.len() != 64 && signature.starts_with(IMPORT_BATCH_MAGIC)
}

/// The manifest signature carried inside an envelope.
pub fn manifest_signature_of(envelope: &[u8]) -> Option<&[u8]> {
    let rest = envelope.strip_prefix(IMPORT_BATCH_MAGIC.as_slice())?;
    let (version, rest) = rest.split_first()?;
    if *version != IMPORT_BATCH_VERSION {
        return None;
    }
    let (len, rest) = read_u32(rest).ok()?;
    let len = len as usize;
    if rest.len() < len + 64 {
        return None;
    }
    Some(&rest[len..len + 64])
}

fn verify_covered_operation(
    canonical: &[u8],
    signature: &[u8],
) -> Result<ThreadOperation, OperationError> {
    let envelope = parse_envelope(signature)?;
    let manifest = parse_manifest(&envelope.manifest)?;
    Ed25519Signer::verify_with_public_key(
        &signing_bytes(&envelope.manifest),
        &manifest.publisher,
        &envelope.signature,
    )?;
    let operation = ThreadOperation::decode(canonical)?;
    if operation.publisher != manifest.publisher {
        return Err(OperationError::Publisher);
    }
    let state = operation
        .source_state()?
        .ok_or_else(|| SignerError::InvalidSignature("import batch covers captures only".into()))?;
    let leaf = ImportLeaf {
        thread: *operation.thread.as_bytes(),
        state_id: *state.id().as_bytes(),
        git_oid: envelope.git_oid,
        parents: state
            .parents
            .iter()
            .map(|parent| *parent.as_bytes())
            .collect(),
    };
    let hash = leaf_hash(&leaf)?;
    if !inclusion_matches(
        envelope.leaf_index as usize,
        &hash,
        &envelope.siblings,
        manifest.count as usize,
        &manifest.root,
    ) {
        return Err(SignerError::VerificationFailed.into());
    }
    Ok(operation)
}

struct ParsedManifest {
    publisher: [u8; 32],
    count: u64,
    root: [u8; 32],
}

struct ParsedEnvelope {
    manifest: Vec<u8>,
    signature: [u8; 64],
    git_oid: Vec<u8>,
    leaf_index: u32,
    siblings: Vec<[u8; 32]>,
}

fn validate_commitment(commitment: &ImportBatchCommitment) -> Result<(), SignerError> {
    if commitment.source_kind != IMPORT_SOURCE_LOCAL_PATH
        && commitment.source_kind != IMPORT_SOURCE_GIT_URL
    {
        return Err(SignerError::InvalidSignature(
            "unknown import source kind".into(),
        ));
    }
    if commitment.source_identity.is_empty() {
        return Err(SignerError::InvalidSignature(
            "import source identity is empty".into(),
        ));
    }
    Ok(())
}

fn leaf_hash(leaf: &ImportLeaf) -> Result<[u8; 32], SignerError> {
    if leaf.git_oid.len() > 32 {
        return Err(SignerError::InvalidSignature(
            "git object id is longer than 32 bytes".into(),
        ));
    }
    let mut body =
        Vec::with_capacity(32 + 32 + 1 + leaf.git_oid.len() + 4 + leaf.parents.len() * 32);
    body.extend_from_slice(&leaf.thread);
    body.extend_from_slice(&leaf.state_id);
    let oid_len = u8::try_from(leaf.git_oid.len()).map_err(|_| {
        SignerError::InvalidSignature("git object id is longer than 32 bytes".into())
    })?;
    body.push(oid_len);
    body.extend_from_slice(&leaf.git_oid);
    let count = u32::try_from(leaf.parents.len())
        .map_err(|_| SignerError::InvalidSignature("too many state parents".into()))?;
    body.extend_from_slice(&count.to_le_bytes());
    for parent in &leaf.parents {
        body.extend_from_slice(parent);
    }
    Ok(*ContentHash::compute_typed(IMPORT_LEAF_DOMAIN, &body).as_bytes())
}

fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut body = [0u8; 64];
    body[..32].copy_from_slice(left);
    body[32..].copy_from_slice(right);
    *ContentHash::compute_typed(IMPORT_NODE_DOMAIN, &body).as_bytes()
}

fn build_levels(mut level: Vec<[u8; 32]>) -> Vec<Vec<[u8; 32]>> {
    let mut levels = Vec::new();
    loop {
        if level.len() <= 1 {
            levels.push(level);
            break;
        }
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut index = 0;
        while index < level.len() {
            if index + 1 == level.len() {
                next.push(level[index]);
            } else {
                next.push(node_hash(&level[index], &level[index + 1]));
            }
            index += 2;
        }
        levels.push(level);
        level = next;
    }
    levels
}

fn siblings_for(
    levels: &[Vec<[u8; 32]>],
    leaf_count: usize,
    mut index: usize,
) -> Result<Vec<[u8; 32]>, SignerError> {
    let mut siblings = Vec::new();
    let mut width = leaf_count;
    let mut level = 0;
    while width > 1 {
        let nodes = levels.get(level).ok_or_else(|| {
            SignerError::InvalidSignature("import proof walked off the tree".into())
        })?;
        if index >= nodes.len() {
            return Err(SignerError::InvalidSignature(
                "import proof index is outside the level".into(),
            ));
        }
        if index.is_multiple_of(2) {
            if index + 1 < width {
                siblings.push(nodes[index + 1]);
            }
        } else {
            siblings.push(nodes[index - 1]);
        }
        index /= 2;
        width = width.div_ceil(2);
        level += 1;
    }
    if siblings.len() > u8::MAX as usize {
        return Err(SignerError::InvalidSignature(
            "import proof is too deep".into(),
        ));
    }
    Ok(siblings)
}

fn inclusion_matches(
    mut index: usize,
    leaf: &[u8; 32],
    siblings: &[[u8; 32]],
    mut width: usize,
    root: &[u8; 32],
) -> bool {
    if width == 0 || index >= width {
        return false;
    }
    let mut hash = *leaf;
    let mut sibling = 0;
    while width > 1 {
        if index.is_multiple_of(2) {
            if index + 1 < width {
                let Some(right) = siblings.get(sibling) else {
                    return false;
                };
                sibling += 1;
                hash = node_hash(&hash, right);
            }
        } else {
            let Some(left) = siblings.get(sibling) else {
                return false;
            };
            sibling += 1;
            hash = node_hash(left, &hash);
        }
        index /= 2;
        width = width.div_ceil(2);
    }
    sibling == siblings.len() && hash == *root
}

fn manifest_bytes(
    publisher: &[u8; 32],
    commitment: &ImportBatchCommitment,
    count: u64,
    root: &[u8; 32],
) -> Result<Vec<u8>, SignerError> {
    let mut out = Vec::new();
    out.push(IMPORT_BATCH_VERSION);
    out.extend_from_slice(publisher);
    out.push(commitment.source_kind);
    push_counted(&mut out, commitment.source_identity.as_bytes())?;
    push_counted(&mut out, &commitment.parameters)?;
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(root);
    Ok(out)
}

fn parse_manifest(bytes: &[u8]) -> Result<ParsedManifest, SignerError> {
    if bytes.first().copied() != Some(IMPORT_BATCH_VERSION) {
        return Err(SignerError::InvalidSignature(
            "unsupported import manifest version".into(),
        ));
    }
    if bytes.len() < 1 + 32 + 1 + 4 {
        return Err(SignerError::InvalidSignature(
            "import manifest is truncated".into(),
        ));
    }
    let mut publisher = [0u8; 32];
    publisher.copy_from_slice(&bytes[1..33]);
    let kind = bytes[33];
    if kind != IMPORT_SOURCE_LOCAL_PATH && kind != IMPORT_SOURCE_GIT_URL {
        return Err(SignerError::InvalidSignature(
            "unknown import source kind".into(),
        ));
    }
    let (identity_len, rest) = read_u32(&bytes[34..])?;
    if identity_len == 0 || rest.len() < identity_len as usize + 4 {
        return Err(SignerError::InvalidSignature(
            "import manifest identity is truncated".into(),
        ));
    }
    let identity = &rest[..identity_len as usize];
    if std::str::from_utf8(identity).is_err() {
        return Err(SignerError::InvalidSignature(
            "import source identity is not UTF-8".into(),
        ));
    }
    let after_identity = &rest[identity_len as usize..];
    let (params_len, rest) = read_u32(after_identity)?;
    if rest.len() != params_len as usize + 8 + 32 {
        return Err(SignerError::InvalidSignature(
            "import manifest parameters are truncated".into(),
        ));
    }
    let tail = &rest[params_len as usize..];
    let mut count_bytes = [0u8; 8];
    count_bytes.copy_from_slice(&tail[..8]);
    let count = u64::from_le_bytes(count_bytes);
    let mut root = [0u8; 32];
    root.copy_from_slice(&tail[8..]);
    Ok(ParsedManifest {
        publisher,
        count,
        root,
    })
}

fn encode_envelope(
    manifest: &[u8],
    signature: &[u8; 64],
    git_oid: &[u8],
    leaf_index: u32,
    siblings: &[[u8; 32]],
) -> Result<Vec<u8>, SignerError> {
    if git_oid.len() > 32 {
        return Err(SignerError::InvalidSignature(
            "git object id is longer than 32 bytes".into(),
        ));
    }
    let sibling_count = u8::try_from(siblings.len())
        .map_err(|_| SignerError::InvalidSignature("import proof is too deep".into()))?;
    let manifest_len = u32::try_from(manifest.len())
        .map_err(|_| SignerError::InvalidSignature("import manifest is too long".into()))?;
    let mut out = Vec::with_capacity(
        4 + 1 + 4 + manifest.len() + 64 + 1 + git_oid.len() + 4 + 1 + siblings.len() * 32,
    );
    out.extend_from_slice(IMPORT_BATCH_MAGIC);
    out.push(IMPORT_BATCH_VERSION);
    out.extend_from_slice(&manifest_len.to_le_bytes());
    out.extend_from_slice(manifest);
    out.extend_from_slice(signature);
    let oid_len = u8::try_from(git_oid.len()).map_err(|_| {
        SignerError::InvalidSignature("git object id is longer than 32 bytes".into())
    })?;
    out.push(oid_len);
    out.extend_from_slice(git_oid);
    out.extend_from_slice(&leaf_index.to_le_bytes());
    out.push(sibling_count);
    for sibling in siblings {
        out.extend_from_slice(sibling);
    }
    Ok(out)
}

fn parse_envelope(bytes: &[u8]) -> Result<ParsedEnvelope, SignerError> {
    let rest = bytes
        .strip_prefix(IMPORT_BATCH_MAGIC.as_slice())
        .ok_or_else(|| {
            SignerError::InvalidSignature("import signature is not a batch envelope".into())
        })?;
    let (version, rest) = rest
        .split_first()
        .ok_or_else(|| SignerError::InvalidSignature("import envelope is truncated".into()))?;
    if *version != IMPORT_BATCH_VERSION {
        return Err(SignerError::InvalidSignature(
            "unsupported import envelope version".into(),
        ));
    }
    let (manifest_len, rest) = read_u32(rest)?;
    let manifest_len = manifest_len as usize;
    if rest.len() < manifest_len + 64 + 1 + 4 + 1 {
        return Err(SignerError::InvalidSignature(
            "import envelope is truncated".into(),
        ));
    }
    let manifest = rest[..manifest_len].to_vec();
    let mut signature = [0u8; 64];
    signature.copy_from_slice(&rest[manifest_len..manifest_len + 64]);
    let mut cursor = &rest[manifest_len + 64..];
    let (oid_len, next) = cursor
        .split_first()
        .ok_or_else(|| SignerError::InvalidSignature("import envelope is truncated".into()))?;
    cursor = next;
    if *oid_len > 32 || cursor.len() < *oid_len as usize + 4 + 1 {
        return Err(SignerError::InvalidSignature(
            "import envelope git id is truncated".into(),
        ));
    }
    let git_oid = cursor[..*oid_len as usize].to_vec();
    cursor = &cursor[*oid_len as usize..];
    let (leaf_index, cursor) = read_u32(cursor)?;
    let (sibling_count, mut cursor) = cursor
        .split_first()
        .ok_or_else(|| SignerError::InvalidSignature("import envelope is truncated".into()))?;
    let mut siblings = Vec::with_capacity(*sibling_count as usize);
    for _ in 0..*sibling_count {
        if cursor.len() < 32 {
            return Err(SignerError::InvalidSignature(
                "import proof is truncated".into(),
            ));
        }
        let mut sibling = [0u8; 32];
        sibling.copy_from_slice(&cursor[..32]);
        siblings.push(sibling);
        cursor = &cursor[32..];
    }
    if !cursor.is_empty() {
        return Err(SignerError::InvalidSignature(
            "import envelope has trailing bytes".into(),
        ));
    }
    Ok(ParsedEnvelope {
        manifest,
        signature,
        git_oid,
        leaf_index,
        siblings,
    })
}

fn signing_bytes(manifest: &[u8]) -> Vec<u8> {
    let mut bytes = IMPORT_BATCH_DOMAIN.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(manifest);
    bytes
}

fn push_counted(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), SignerError> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| SignerError::InvalidSignature("import field is too long".into()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn read_u32(bytes: &[u8]) -> Result<(u32, &[u8]), SignerError> {
    if bytes.len() < 4 {
        return Err(SignerError::InvalidSignature(
            "import field length is truncated".into(),
        ));
    }
    let mut len = [0u8; 4];
    len.copy_from_slice(&bytes[..4]);
    Ok((u32::from_le_bytes(len), &bytes[4..]))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use heddle_object_model::object::{
        Attribution, ContentHash, Principal, State, StateId, Tree,
        thread_replication::{AuthoredCapture, ThreadOperation, ThreadOperationBody},
    };

    use super::*;
    use crate::thread_operation::SignedOperation;

    fn commitment() -> ImportBatchCommitment {
        ImportBatchCommitment {
            source_kind: IMPORT_SOURCE_GIT_URL,
            source_identity: "https://example.test/repo.git".into(),
            parameters: import_parameters(&["main", "topic"]).expect("parameters"),
        }
    }

    fn leaf(thread: u8, state: u8, oid: u8) -> ImportLeaf {
        ImportLeaf {
            thread: [thread; 32],
            state_id: [state; 32],
            git_oid: vec![oid; 20],
            parents: vec![[state.wrapping_add(1); 32]],
        }
    }

    #[test]
    fn one_signature_covers_every_leaf_and_rejects_a_stranger() {
        let signer = Ed25519Signer::from_seed(&[9; 32]).expect("signer");
        let batch = sign_import_batch(
            &signer,
            &commitment(),
            vec![leaf(1, 3, 7), leaf(1, 1, 8), leaf(2, 1, 9)],
        )
        .expect("batch");
        batch.verify_manifest().expect("manifest");
        assert_eq!(batch.leaf_count, 3);
        let first = batch.envelope(&[1; 32], &[1; 32]).expect("first proof");
        let third = batch.envelope(&[2; 32], &[1; 32]).expect("odd proof");
        assert_eq!(
            manifest_signature_of(&first),
            Some(batch.manifest_signature().as_slice())
        );
        assert_eq!(manifest_signature_of(&first), manifest_signature_of(&third));
        assert!(batch.covers(&[1; 32], &[3; 32]));
        assert!(!batch.covers(&[9; 32], &[9; 32]));
        assert!(batch.envelope(&[9; 32], &[9; 32]).is_err());

        let promoted = siblings_for(&batch.levels, 3, 2).expect("promoted proof");
        assert!(inclusion_matches(
            2,
            &batch.levels[0][2],
            &promoted,
            3,
            &parse_manifest(batch.manifest()).expect("manifest").root,
        ));
        let mut broken = promoted.clone();
        if let Some(sibling) = broken.first_mut() {
            sibling[0] ^= 0xff;
            assert!(!inclusion_matches(
                2,
                &batch.levels[0][2],
                &broken,
                3,
                &parse_manifest(batch.manifest()).expect("manifest").root,
            ));
        }
    }

    #[test]
    fn a_single_leaf_is_the_root() {
        let signer = Ed25519Signer::from_seed(&[4; 32]).expect("signer");
        let only = leaf(1, 2, 3);
        let hash = leaf_hash(&only).expect("leaf");
        let batch = sign_import_batch(&signer, &commitment(), vec![only]).expect("batch");
        assert_eq!(batch.levels[0][0], hash);
        assert_eq!(
            parse_manifest(batch.manifest()).expect("manifest").root,
            hash
        );
        let envelope = batch.envelope(&[1; 32], &[2; 32]).expect("proof");
        let parsed = parse_envelope(&envelope).expect("envelope");
        assert!(parsed.siblings.is_empty());
    }

    #[test]
    fn operation_verification_checks_inclusion_not_authorship() {
        let signer = Ed25519Signer::from_seed(&[11; 32]).expect("signer");
        let parent = StateId::from_bytes([7; 32]);
        let state = State::new_snapshot(
            Tree::new().hash(),
            vec![parent],
            Attribution::human(Principal::new("Git Author", "git@example.test")),
        );
        let operation = ThreadOperation {
            version: 1,
            thread: ContentHash::from_bytes([1; 32]),
            parents: BTreeSet::new(),
            publisher: signer.public_key().try_into().expect("key"),
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                state.encode_current_msgpack().expect("state").into(),
            )),
        };
        let git_oid = vec![4; 20];
        let batch = sign_import_batch(
            &signer,
            &commitment(),
            vec![ImportLeaf {
                thread: [1; 32],
                state_id: *state.id().as_bytes(),
                git_oid: git_oid.clone(),
                parents: vec![*parent.as_bytes()],
            }],
        )
        .expect("batch");
        let signed = SignedOperation {
            canonical: operation.encode().expect("canonical"),
            signature: batch
                .envelope(&[1; 32], state.id().as_bytes())
                .expect("envelope"),
        };
        assert!(is_import_batch_signature(&signed.signature));
        assert_ne!(signed.signature.len(), 64);
        assert_eq!(signed.verify().expect("covered"), operation);

        let mut swapped_oid = signed.clone();
        let parsed = parse_envelope(&swapped_oid.signature).expect("envelope");
        swapped_oid.signature = encode_envelope(
            &parsed.manifest,
            &parsed.signature,
            &[9; 20],
            parsed.leaf_index,
            &parsed.siblings,
        )
        .expect("re-encode");
        assert!(
            swapped_oid.verify().is_err(),
            "a different git id is a different leaf"
        );

        let other = State::new_snapshot(
            Tree::new().hash(),
            vec![StateId::from_bytes([8; 32])],
            Attribution::human(Principal::new("Git Author", "git@example.test")),
        );
        let other_operation = ThreadOperation {
            body: ThreadOperationBody::Capture(AuthoredCapture::local(
                other.encode_current_msgpack().expect("state").into(),
            )),
            ..operation.clone()
        };
        let forged = SignedOperation {
            canonical: other_operation.encode().expect("other"),
            signature: signed.signature,
        };
        assert!(
            forged.verify().is_err(),
            "a state outside the batch is refused"
        );
        let _ = git_oid;
    }
}
