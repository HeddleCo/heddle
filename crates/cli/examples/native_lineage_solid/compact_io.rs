// SPDX-License-Identifier: Apache-2.0

use anyhow::{Result, bail};

pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn put_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn put_bool(&mut self, value: bool) {
        self.put_u8(u8::from(value));
    }

    pub fn put_fixed(&mut self, value: &[u8]) {
        self.bytes.extend_from_slice(value);
    }

    pub fn put_u64(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.put_u8((value as u8) | 0x80);
            value >>= 7;
        }
        self.put_u8(value as u8);
    }

    pub fn put_i64(&mut self, value: i64) {
        self.put_u64(((value << 1) ^ (value >> 63)) as u64);
    }

    pub fn put_bytes(&mut self, value: &[u8]) {
        self.put_u64(value.len() as u64);
        self.put_fixed(value);
    }

    pub fn put_optional_bytes(&mut self, value: Option<&[u8]>) {
        match value {
            Some(value) => {
                self.put_u8(1);
                self.put_bytes(value);
            }
            None => self.put_u8(0),
        }
    }

    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

pub struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(len)
            .ok_or_else(|| anyhow::anyhow!("compact offset overflow"))?;
        if end > self.bytes.len() {
            bail!("truncated compact payload at byte {}", self.position);
        }
        let value = &self.bytes[self.position..end];
        self.position = end;
        Ok(value)
    }

    pub fn get_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn get_bool(&mut self) -> Result<bool> {
        match self.get_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            value => bail!("invalid compact boolean {value}"),
        }
    }

    pub fn get_fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }

    pub fn get_u64(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..=63).step_by(7) {
            let byte = self.get_u8()?;
            if shift == 63 && byte > 1 {
                bail!("compact varint overflow");
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        bail!("compact varint overflow")
    }

    pub fn get_i64(&mut self) -> Result<i64> {
        let value = self.get_u64()?;
        Ok(((value >> 1) as i64) ^ (-((value & 1) as i64)))
    }

    pub fn get_bytes(&mut self) -> Result<Vec<u8>> {
        let len = usize::try_from(self.get_u64()?)?;
        Ok(self.take(len)?.to_vec())
    }

    pub fn get_optional_bytes(&mut self) -> Result<Option<Vec<u8>>> {
        match self.get_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.get_bytes()?)),
            value => bail!("invalid compact option tag {value}"),
        }
    }

    pub fn finish(self) -> Result<()> {
        if self.position != self.bytes.len() {
            bail!(
                "{} trailing bytes in compact payload",
                self.bytes.len() - self.position
            );
        }
        Ok(())
    }
}
