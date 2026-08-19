// SPDX-License-Identifier: Apache-2.0

use super::{Result, invalid};

pub(super) const FRAME_CHECKSUM_LEN: usize = 32;

pub(super) struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub(super) fn new(magic: &[u8; 4]) -> Self {
        Self {
            bytes: magic.to_vec(),
        }
    }

    pub(super) fn put_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub(super) fn put_bool(&mut self, value: bool) {
        self.put_u8(u8::from(value));
    }

    pub(super) fn put_fixed(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub(super) fn put_u64(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.put_u8((value as u8) | 0x80);
            value >>= 7;
        }
        self.put_u8(value as u8);
    }

    pub(super) fn put_i64(&mut self, value: i64) {
        self.put_u64(((value << 1) ^ (value >> 63)) as u64);
    }

    pub(super) fn put_bytes(&mut self, value: &[u8]) {
        self.put_u64(value.len() as u64);
        self.put_fixed(value);
    }

    pub(super) fn put_optional_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.put_u8(1);
                self.put_bytes(value);
            }
            None => self.put_u8(0),
        }
    }

    pub(super) fn finish(mut self) -> Vec<u8> {
        let checksum = blake3::hash(&self.bytes);
        self.bytes.extend_from_slice(checksum.as_bytes());
        self.bytes
    }
}

pub(super) struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub(super) fn verified(bytes: &'a [u8], magic: &[u8; 4]) -> Result<Self> {
        let content_len = bytes
            .len()
            .checked_sub(FRAME_CHECKSUM_LEN)
            .ok_or_else(|| invalid("frame is shorter than its checksum"))?;
        let (content, checksum) = bytes.split_at(content_len);
        if content.get(..4) != Some(magic) {
            return Err(invalid("frame magic does not match its object kind"));
        }
        if blake3::hash(content).as_bytes() != checksum {
            return Err(invalid("compact frame checksum mismatch"));
        }
        Ok(Self {
            bytes: content,
            position: magic.len(),
        })
    }

    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| invalid("compact offset overflow"))?;
        if end > self.bytes.len() {
            return Err(invalid(format!(
                "truncated compact payload at byte {}",
                self.position
            )));
        }
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    pub(super) fn get_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(super) fn get_bool(&mut self) -> Result<bool> {
        match self.get_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(invalid(format!("invalid compact boolean {value}"))),
        }
    }

    pub(super) fn get_fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }

    pub(super) fn get_u64(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.get_u8()?;
            if shift == 63 && byte > 1 {
                return Err(invalid("compact varint overflow"));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("compact varint overflow"))
    }

    pub(super) fn get_i64(&mut self) -> Result<i64> {
        let value = self.get_u64()?;
        Ok(((value >> 1) as i64) ^ (-((value & 1) as i64)))
    }

    pub(super) fn get_count(&mut self, field: &str, min_item_bytes: usize) -> Result<usize> {
        self.get_count_at_most(field, min_item_bytes, super::limits::MAX_COMPACT_COUNT)
    }

    pub(super) fn get_count_at_most(
        &mut self,
        field: &str,
        min_item_bytes: usize,
        max_count: usize,
    ) -> Result<usize> {
        let count = usize::try_from(self.get_u64()?)
            .map_err(|_| invalid(format!("{field} count exceeds platform limits")))?;
        super::limits::admit_count(field, count, self.remaining(), min_item_bytes, max_count)?;
        Ok(count)
    }

    pub(super) fn get_bytes(&mut self) -> Result<Vec<u8>> {
        let len = usize::try_from(self.get_u64()?)
            .map_err(|_| invalid("compact byte length exceeds platform limits"))?;
        Ok(self.take(len)?.to_vec())
    }

    pub(super) fn get_optional_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        match self.get_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.get_bytes()?)),
            value => Err(invalid(format!("invalid compact option tag {value}"))),
        }
    }

    pub(super) fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    pub(super) fn finish(self) -> Result<()> {
        if self.position != self.bytes.len() {
            return Err(invalid(format!(
                "{} trailing bytes in compact payload",
                self.bytes.len() - self.position
            )));
        }
        Ok(())
    }
}

pub(super) fn varint_len(mut value: usize) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}
