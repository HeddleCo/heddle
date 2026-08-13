// SPDX-License-Identifier: Apache-2.0
//! Compression-wrapper framing shared by plain and dictionary zstd objects.

pub(super) const HEADER_LEN: usize = 9;
pub(super) const DICTIONARY_HEADER_LEN: usize = HEADER_LEN + size_of::<u32>();
pub(super) const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

const ZSTD_TYPE: u8 = 1;

#[derive(Clone, Copy)]
pub(super) struct ZstdHeader {
    pub(super) len: usize,
    pub(super) uncompressed_size: u64,
    pub(super) dictionary_id: u32,
}

pub(super) fn parse_zstd(data: &[u8]) -> Option<ZstdHeader> {
    if data.len() < HEADER_LEN || data[0] != ZSTD_TYPE {
        return None;
    }
    let uncompressed_size = u64::from_be_bytes(data[1..HEADER_LEN].try_into().ok()?);
    if has_magic_at(data, HEADER_LEN) {
        return Some(ZstdHeader {
            len: HEADER_LEN,
            uncompressed_size,
            dictionary_id: 0,
        });
    }

    let dictionary_id = u32::from_be_bytes(
        data.get(HEADER_LEN..DICTIONARY_HEADER_LEN)?
            .try_into()
            .ok()?,
    );
    (dictionary_id != 0 && has_magic_at(data, DICTIONARY_HEADER_LEN)).then_some(ZstdHeader {
        len: DICTIONARY_HEADER_LEN,
        uncompressed_size,
        dictionary_id,
    })
}

fn has_magic_at(data: &[u8], offset: usize) -> bool {
    data.get(offset..offset + ZSTD_MAGIC.len()) == Some(ZSTD_MAGIC.as_slice())
}
