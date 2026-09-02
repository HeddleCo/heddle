// SPDX-License-Identifier: Apache-2.0
//! Lazy HDC1-to-HLR1 projection for entry-level store reads.

use crate::object::{
    OpenedTreeBody, TREE_DELTA_HEADER_LEN, TREE_LEAN_MAGIC, TreeBodyIntegrity, TreeByteSource,
    TreeDeltaHeader, TreeDeltaOp, TreeEntry, TreeEntryReader, TreeStreamError,
    decode_tree_delta_header_prefix, decode_tree_delta_ops_prefix, encode_lean_entry,
};

/// Exposes a cumulative delta as a virtual HLR1 body. The first-entry and
/// first-100 porch bounds decide how much of the delta is transferred; the
/// materialized anchor is paged through its own seekable reader.
pub(super) struct DeltaTreeSource {
    delta: OpenedTreeBody,
    delta_len: usize,
    header: TreeDeltaHeader,
    ops: Vec<TreeDeltaOp>,
    op_index: usize,
    anchor: TreeEntryReader<OpenedTreeBody>,
    anchor_next: Option<TreeEntry>,
    anchor_done: bool,
    body: Vec<u8>,
    previous_name: String,
    generated: usize,
    finalized: bool,
}

impl DeltaTreeSource {
    pub(super) fn open(
        mut delta: OpenedTreeBody,
        anchor: TreeEntryReader<OpenedTreeBody>,
    ) -> Result<Self, TreeStreamError> {
        let delta_len = usize::try_from(delta.len())
            .map_err(|_| TreeStreamError::Malformed("HDC1 body exceeds usize".into()))?;
        let mut header_bytes = vec![0u8; delta_len.min(TREE_DELTA_HEADER_LEN)];
        delta.read_exact_at(0, &mut header_bytes)?;
        let header = decode_tree_delta_header_prefix(&header_bytes, delta_len)?;
        let mut body = Vec::new();
        body.extend_from_slice(TREE_LEAN_MAGIC);
        put_varint(header.result_count, &mut body);
        let mut source = Self {
            delta,
            delta_len,
            header,
            ops: Vec::new(),
            op_index: 0,
            anchor,
            anchor_next: None,
            anchor_done: false,
            body,
            previous_name: String::new(),
            generated: 0,
            finalized: false,
        };
        if source.header.result_count == 0 {
            source.load_ops(source.header.op_count)?;
            source.finish_merge()?;
        }
        Ok(source)
    }

    fn load_ops(&mut self, wanted: usize) -> Result<(), TreeStreamError> {
        if wanted <= self.ops.len() {
            return Ok(());
        }
        let end = if wanted == self.header.first_op_count {
            self.header.first_end
        } else if wanted == self.header.hundred_op_count {
            self.header.hundred_end
        } else if wanted == self.header.op_count {
            self.delta_len
        } else {
            return Err(TreeStreamError::Malformed(
                "HDC1 read requested a non-porch operation count".into(),
            ));
        };
        let mut prefix = vec![0u8; end];
        self.delta.read_exact_at(0, &mut prefix)?;
        let (header, ops, consumed) =
            decode_tree_delta_ops_prefix(&prefix, self.delta_len, wanted)?;
        if header != self.header || consumed != end {
            return Err(TreeStreamError::Malformed(
                "HDC1 porch does not end at its declared offset".into(),
            ));
        }
        self.ops = ops;
        Ok(())
    }

    fn prepare_ops_for_result(&mut self, ordinal: usize) -> Result<(), TreeStreamError> {
        let wanted = if ordinal <= 1 {
            self.header.first_op_count
        } else if ordinal <= 100 {
            self.header.hundred_op_count
        } else {
            self.header.op_count
        };
        self.load_ops(wanted)
    }

    fn fill_anchor_next(&mut self) -> Result<(), TreeStreamError> {
        if self.anchor_next.is_none() && !self.anchor_done {
            self.anchor_next = self.anchor.next_entry()?;
            self.anchor_done = self.anchor_next.is_none();
        }
        Ok(())
    }

    fn next_merged_entry(&mut self) -> Result<Option<TreeEntry>, TreeStreamError> {
        loop {
            self.fill_anchor_next()?;
            let op = self.ops.get(self.op_index);
            match (self.anchor_next.as_ref(), op) {
                (Some(anchor), Some(op)) => match anchor.name().cmp(op.name()) {
                    std::cmp::Ordering::Less => return Ok(self.anchor_next.take()),
                    std::cmp::Ordering::Greater => {
                        self.op_index += 1;
                        if let TreeDeltaOp::Upsert(entry) = op {
                            return Ok(Some(entry.clone()));
                        }
                    }
                    std::cmp::Ordering::Equal => {
                        self.anchor_next = None;
                        self.op_index += 1;
                        if let TreeDeltaOp::Upsert(entry) = op {
                            return Ok(Some(entry.clone()));
                        }
                    }
                },
                (Some(_), None) => return Ok(self.anchor_next.take()),
                (None, Some(op)) => {
                    self.op_index += 1;
                    if let TreeDeltaOp::Upsert(entry) = op {
                        return Ok(Some(entry.clone()));
                    }
                }
                (None, None) => return Ok(None),
            }
        }
    }

    fn generate_entry(&mut self) -> Result<(), TreeStreamError> {
        self.prepare_ops_for_result(self.generated + 1)?;
        let entry = self.next_merged_entry()?.ok_or_else(|| {
            TreeStreamError::Malformed("HDC1 result ended before its declared count".into())
        })?;
        encode_lean_entry(&entry, &self.previous_name, &mut self.body)?;
        self.previous_name = entry.name().to_string();
        self.generated += 1;
        if self.generated == self.header.result_count {
            self.load_ops(self.header.op_count)?;
            self.finish_merge()?;
        }
        Ok(())
    }

    fn finish_merge(&mut self) -> Result<(), TreeStreamError> {
        if self.next_merged_entry()?.is_some() {
            return Err(TreeStreamError::Malformed(
                "HDC1 result exceeds its declared count".into(),
            ));
        }
        if self.op_index != self.header.op_count {
            return Err(TreeStreamError::Malformed(
                "HDC1 reconstruction did not consume every operation".into(),
            ));
        }
        self.anchor.finish_and_verify()?;
        self.finalized = true;
        Ok(())
    }
}

impl TreeByteSource for DeltaTreeSource {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), TreeStreamError> {
        let start =
            usize::try_from(offset).map_err(|_| TreeStreamError::TruncatedFrame { offset })?;
        let end = start
            .checked_add(buf.len())
            .ok_or(TreeStreamError::TruncatedFrame { offset })?;
        while self.body.len() < end && self.generated < self.header.result_count {
            self.generate_entry()?;
        }
        let bytes = self
            .body
            .get(start..end)
            .ok_or(TreeStreamError::TruncatedFrame { offset })?;
        buf.copy_from_slice(bytes);
        Ok(())
    }

    fn len(&self) -> u64 {
        self.body.len() as u64
    }

    fn integrity(&self) -> TreeBodyIntegrity {
        TreeBodyIntegrity::SequentialVerify
    }

    fn bytes_read(&self) -> u64 {
        self.delta
            .bytes_read()
            .saturating_add(self.anchor.bytes_read())
    }
}

fn put_varint(mut value: usize, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}
