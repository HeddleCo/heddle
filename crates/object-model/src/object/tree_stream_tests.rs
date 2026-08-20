// SPDX-License-Identifier: Apache-2.0

use sley::{ObjectFormat as GitObjectFormat, ObjectId as GitObjectId};

use crate::object::{
    BytesTreeSource, ContentHash, SpoolId, StateId, TREE_ENCODING_VERSION, TREE_HEADER_LEN, Tree,
    TreeBodyIntegrity, TreeByteSource, TreeEntry, TreeEntryReader, TreePageLimits,
    TreeResumeCursor, TreeStreamError, is_canonical_tree,
};

fn blob(name: &str, payload: &[u8]) -> TreeEntry {
    TreeEntry::file(name, ContentHash::compute(payload), false).expect("file entry")
}

fn sample_tree() -> Tree {
    Tree::from_entries(vec![
        blob("a", b"a"),
        blob("b", b"b"),
        TreeEntry::directory("c", ContentHash::compute(b"dir")).expect("dir"),
        TreeEntry::symlink("d", ContentHash::compute(b"link")).expect("link"),
        TreeEntry::gitlink(
            "e",
            GitObjectId::from_raw(GitObjectFormat::Sha1, &[7; 20]).expect("oid"),
        )
        .expect("gitlink"),
        TreeEntry::spoollink(
            "f",
            SpoolId::parse("acme/child").expect("spool"),
            StateId::from_bytes([3; 32]),
        )
        .expect("spoollink"),
    ])
}

fn open_reader(tree: &Tree) -> (Vec<u8>, TreeEntryReader<BytesTreeSource>) {
    let bytes = tree.encode_canonical().expect("encode");
    let reader = TreeEntryReader::open(
        BytesTreeSource::sequential_verify(bytes.clone()),
        tree.hash(),
        None,
    )
    .expect("open");
    (bytes, reader)
}

#[test]
fn canonical_round_trip_matches_eager_tree() {
    let tree = sample_tree();
    let bytes = tree.encode_canonical().expect("encode");
    assert!(is_canonical_tree(&bytes));
    assert_eq!(Tree::decode_canonical(&bytes).expect("decode"), tree);
    assert_eq!(
        Tree::decode_canonical_streamed(&bytes).expect("streamed"),
        tree
    );
}

#[test]
fn streaming_pages_respect_entry_and_byte_limits() {
    let tree = sample_tree();
    let (_bytes, mut reader) = open_reader(&tree);
    let limits = TreePageLimits::new(2, usize::MAX).expect("limits");
    let first = reader.next_page(limits).expect("page").expect("some");
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.entries[0].name(), "a");
    assert_eq!(first.resume_cursor.ordinal(), 2);
    let second = reader.next_page(limits).expect("page").expect("some");
    assert_eq!(second.entries.len(), 2);
    reader.next_page(limits).expect("page").expect("remaining");
    assert!(reader.next_page(limits).expect("eof").is_none());
    reader.finish_and_verify().expect("verify");
}

#[test]
fn resume_cursor_survives_restart_without_rereading_prefix() {
    let tree = sample_tree();
    let (bytes, mut reader) = open_reader(&tree);
    let limits = TreePageLimits::new(2, usize::MAX).expect("limits");
    let first = reader.next_page(limits).expect("page").expect("some");
    let cursor = first.resume_cursor;
    let prefix_end = cursor.byte_offset();
    drop(reader);

    let mut spy = OffsetSpy::new(bytes);
    let mut resumed = TreeEntryReader::open(&mut spy, tree.hash(), Some(&cursor)).expect("resume");
    let rest = resumed
        .next_page(TreePageLimits::new(64, usize::MAX).expect("limits"))
        .expect("page")
        .expect("some");
    assert_eq!(rest.entries[0].name(), "c");
    resumed.finish_and_verify().expect("verify");
    assert!(
        spy.reads
            .iter()
            .all(|(offset, _)| *offset == 0 || *offset >= prefix_end),
        "resume must not transfer prefix bytes below {prefix_end}, reads={:?}",
        spy.reads
    );
}

struct OffsetSpy {
    inner: BytesTreeSource,
    reads: Vec<(u64, usize)>,
}

impl OffsetSpy {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            inner: BytesTreeSource::verified_placement(bytes),
            reads: Vec::new(),
        }
    }
}

impl TreeByteSource for OffsetSpy {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError> {
        self.reads.push((offset, buf.len()));
        self.inner.read_exact_at(offset, buf)
    }
    fn len(&self) -> u64 {
        self.inner.len()
    }
    fn integrity(&self) -> TreeBodyIntegrity {
        self.inner.integrity()
    }
    fn bytes_read(&self) -> u64 {
        self.inner.bytes_read()
    }
}

impl TreeByteSource for &mut OffsetSpy {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError> {
        <OffsetSpy as TreeByteSource>::read_exact_at(self, offset, buf)
    }
    fn len(&self) -> u64 {
        <OffsetSpy as TreeByteSource>::len(self)
    }
    fn integrity(&self) -> TreeBodyIntegrity {
        <OffsetSpy as TreeByteSource>::integrity(self)
    }
    fn bytes_read(&self) -> u64 {
        <OffsetSpy as TreeByteSource>::bytes_read(self)
    }
}

#[test]
fn resume_rejects_unverified_source() {
    let tree = sample_tree();
    let (bytes, mut reader) = open_reader(&tree);
    let page = reader
        .next_page(TreePageLimits::new(1, usize::MAX).expect("limits"))
        .expect("page")
        .expect("some");
    let error = TreeEntryReader::open(
        BytesTreeSource::sequential_verify(bytes),
        tree.hash(),
        Some(&page.resume_cursor),
    )
    .expect_err("unverified range");
    assert!(matches!(error, TreeStreamError::UnverifiedRange));
}

#[test]
fn resume_rejects_foreign_tree_id_and_encoding_version() {
    let tree = sample_tree();
    let bytes = tree.encode_canonical().expect("encode");
    let mut cursor = TreeResumeCursor::start(ContentHash::compute(b"other"));
    let error = TreeEntryReader::open(
        BytesTreeSource::verified_placement(bytes.clone()),
        tree.hash(),
        Some(&cursor),
    )
    .expect_err("foreign id");
    assert!(matches!(
        error,
        TreeStreamError::CursorMismatch(_) | TreeStreamError::HashMismatch { .. }
    ));

    cursor = TreeResumeCursor::start(tree.hash());
    cursor.encoding_version = TREE_ENCODING_VERSION + 1;
    let error = TreeEntryReader::open(
        BytesTreeSource::verified_placement(bytes),
        tree.hash(),
        Some(&cursor),
    )
    .expect_err("version");
    assert!(matches!(error, TreeStreamError::CursorMismatch(_)));
}

#[test]
fn resume_rejects_mid_frame_byte_offset() {
    let tree = sample_tree();
    let bytes = tree.encode_canonical().expect("encode");
    let mut cursor = TreeResumeCursor::start(tree.hash());
    cursor.ordinal = 1;
    cursor.byte_offset = TREE_HEADER_LEN as u64 + 1;
    cursor.prev_name = Some("a".into());
    let error = TreeEntryReader::open(
        BytesTreeSource::verified_placement(bytes),
        tree.hash(),
        Some(&cursor),
    )
    .expect_err("mid-frame");
    assert!(matches!(
        error,
        TreeStreamError::CursorMismatch(_)
            | TreeStreamError::Malformed(_)
            | TreeStreamError::TruncatedFrame { .. }
            | TreeStreamError::Invalid(_)
    ));
}

#[test]
fn zero_page_limits_fail_closed() {
    assert!(matches!(
        TreePageLimits::new(0, 16),
        Err(TreeStreamError::InvalidPageLimits)
    ));
    assert!(matches!(
        TreePageLimits::new(16, 0),
        Err(TreeStreamError::InvalidPageLimits)
    ));
}

#[test]
fn oversized_entry_is_explicit() {
    let tree = Tree::from_entries(vec![blob("wide", b"payload")]);
    let (_bytes, mut reader) = open_reader(&tree);
    let limits = TreePageLimits::new(8, 1).expect("limits");
    let error = reader.next_page(limits).expect_err("oversized");
    assert!(matches!(
        error,
        TreeStreamError::OversizedEntry {
            max_decoded_bytes: 1,
            ..
        }
    ));
}

#[test]
fn duplicate_name_and_order_fail_identically() {
    let tree = Tree::from_entries(vec![blob("a", b"1"), blob("b", b"2")]);
    let bytes = tree.encode_canonical().expect("encode");
    let swapped = Tree::from_entries(vec![blob("a", b"1"), blob("b", b"2")])
        .encode_canonical()
        .expect("encode");
    // Rebuild an illegal payload by swapping the two frames after the header.
    let first = frame_at(&bytes, 0);
    let second = frame_at(&bytes, first);
    let mut illegal = bytes[..TREE_HEADER_LEN].to_vec();
    illegal.extend_from_slice(&bytes[TREE_HEADER_LEN + first..TREE_HEADER_LEN + first + second]);
    illegal.extend_from_slice(&bytes[TREE_HEADER_LEN..TREE_HEADER_LEN + first]);
    // Keep declared counts; order is now b then a.
    let eager = Tree::decode_canonical(&illegal).expect_err("eager order");
    let streamed = Tree::decode_canonical_streamed(&illegal).expect_err("stream order");
    assert!(matches!(eager, TreeStreamError::Invalid(_)));
    assert!(matches!(streamed, TreeStreamError::Invalid(_)));

    // Duplicate: copy first frame twice.
    illegal = bytes[..TREE_HEADER_LEN].to_vec();
    illegal.extend_from_slice(&bytes[TREE_HEADER_LEN..TREE_HEADER_LEN + first]);
    illegal.extend_from_slice(&bytes[TREE_HEADER_LEN..TREE_HEADER_LEN + first]);
    write_payload_len(&mut illegal, (first * 2) as u64);
    let eager = Tree::decode_canonical(&illegal).expect_err("eager dup");
    let streamed = Tree::decode_canonical_streamed(&illegal).expect_err("stream dup");
    assert!(matches!(eager, TreeStreamError::Invalid(_)));
    assert!(matches!(streamed, TreeStreamError::Invalid(_)));
    let _ = (swapped, bytes);
}

fn frame_at(bytes: &[u8], rel: usize) -> usize {
    let offset = TREE_HEADER_LEN + rel;
    4 + u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("len")) as usize
}

fn write_payload_len(bytes: &mut [u8], payload_len: u64) {
    bytes[45..53].copy_from_slice(&payload_len.to_le_bytes());
}

#[test]
fn malformed_mode_and_oid_are_rejected() {
    let tree = Tree::from_entries(vec![blob("a", b"1")]);
    let mut bytes = tree.encode_canonical().expect("encode");
    let frame = TREE_HEADER_LEN + 4;
    bytes[frame] = 99;
    assert!(Tree::decode_canonical(&bytes).is_err());
    assert!(Tree::decode_canonical_streamed(&bytes).is_err());

    let mut oid_bytes = tree.encode_canonical().expect("encode");
    let frame_len = u32::from_le_bytes(
        oid_bytes[TREE_HEADER_LEN..TREE_HEADER_LEN + 4]
            .try_into()
            .expect("len"),
    );
    oid_bytes[TREE_HEADER_LEN..TREE_HEADER_LEN + 4].copy_from_slice(&(frame_len - 1).to_le_bytes());
    oid_bytes.pop();
    let payload_len = (oid_bytes.len() - TREE_HEADER_LEN) as u64;
    write_payload_len(&mut oid_bytes, payload_len);
    let eager = Tree::decode_canonical(&oid_bytes).expect_err("eager oid");
    let streamed = Tree::decode_canonical_streamed(&oid_bytes).expect_err("stream oid");
    assert!(eager.to_string().contains("object id"), "{eager}");
    assert!(streamed.to_string().contains("object id"), "{streamed}");
}

#[test]
fn truncated_frame_and_trailing_bytes_are_errors() {
    let tree = sample_tree();
    let bytes = tree.encode_canonical().expect("encode");
    let truncated = &bytes[..bytes.len() - 3];
    assert!(matches!(
        Tree::decode_canonical(truncated),
        Err(TreeStreamError::TruncatedFrame { .. })
    ));
    assert!(matches!(
        Tree::decode_canonical_streamed(truncated),
        Err(TreeStreamError::TruncatedFrame { .. })
    ));

    let mut trailing = bytes.clone();
    trailing.push(0x0a);
    assert!(matches!(
        Tree::decode_canonical(&trailing),
        Err(TreeStreamError::TrailingBytes { extra: 1 })
    ));
    assert!(matches!(
        Tree::decode_canonical_streamed(&trailing),
        Err(TreeStreamError::TrailingBytes { extra: 1 })
    ));
}

#[test]
fn leftover_payload_after_declared_count_is_trailing_bytes() {
    let tree = Tree::from_entries(vec![blob("a", b"1"), blob("b", b"2")]);
    let bytes = tree.encode_canonical().expect("encode");
    let first = frame_at(&bytes, 0);
    let mut short_count = bytes.clone();
    short_count[37..45].copy_from_slice(&1u64.to_le_bytes());
    write_payload_len(&mut short_count, (first + frame_at(&bytes, first)) as u64);
    let eager = Tree::decode_canonical(&short_count).expect_err("eager leftover");
    let streamed = Tree::decode_canonical_streamed(&short_count).expect_err("stream leftover");
    assert!(
        matches!(eager, TreeStreamError::TrailingBytes { .. }),
        "{eager}"
    );
    assert!(
        matches!(streamed, TreeStreamError::TrailingBytes { .. }),
        "{streamed}"
    );
}

#[test]
fn huge_frame_prefix_is_truncated_without_allocating() {
    let tree = Tree::from_entries(vec![blob("a", b"1")]);
    let mut bytes = tree.encode_canonical().expect("encode");
    bytes[TREE_HEADER_LEN..TREE_HEADER_LEN + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let eager = Tree::decode_canonical(&bytes).expect_err("eager huge");
    let streamed = Tree::decode_canonical_streamed(&bytes).expect_err("stream huge");
    assert!(
        matches!(eager, TreeStreamError::TruncatedFrame { .. }),
        "{eager}"
    );
    assert!(
        matches!(streamed, TreeStreamError::TruncatedFrame { .. }),
        "{streamed}"
    );
}

#[test]
fn declared_count_mismatch_is_an_error() {
    let tree = Tree::from_entries(vec![blob("a", b"1")]);
    let mut bytes = tree.encode_canonical().expect("encode");
    bytes[37..45].copy_from_slice(&2u64.to_le_bytes());
    assert!(Tree::decode_canonical(&bytes).is_err());
    assert!(Tree::decode_canonical_streamed(&bytes).is_err());
}

#[test]
fn empty_tree_streams_and_verifies() {
    let tree = Tree::new();
    let (_bytes, mut reader) = open_reader(&tree);
    assert!(
        reader
            .next_page(TreePageLimits::new(8, 64).expect("limits"))
            .expect("page")
            .is_none()
    );
    reader.finish_and_verify().expect("empty verify");
}

#[test]
fn name_over_u16_is_rejected_at_the_model_boundary() {
    let name = "n".repeat(u16::MAX as usize + 1);
    let error = TreeEntry::file(name, ContentHash::compute(b"x"), false).expect_err("too long");
    assert!(error.to_string().contains("u16"));
}

#[test]
fn source_integrity_is_explicit() {
    assert_eq!(
        BytesTreeSource::verified_placement(vec![0]).integrity(),
        TreeBodyIntegrity::VerifiedPlacement
    );
    assert_eq!(
        BytesTreeSource::sequential_verify(vec![0]).integrity(),
        TreeBodyIntegrity::SequentialVerify
    );
}
