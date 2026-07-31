// SPDX-License-Identifier: Apache-2.0
use std::collections::HashSet;

use objects::store::pack::{PackContainerSpec, PackIndex, PackObjectId, verify_container};

use crate::{MAX_RECEIVED_PACK_SIZE, NativePackBundle, ProtocolError, Result};

const PACK_HEADER_LEN: usize = 16;
const PACK_TRAILER_LEN: usize = 32;
const PACK_SPEC: PackContainerSpec = PackContainerSpec {
    magic: b"LMPK",
    version: 3,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPackIndexEntry {
    pub id: PackObjectId,
    pub output_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPackExtent {
    pub output_offset: u64,
    pub length: u64,
    pub digest: [u8; 32],
    pub objects: Vec<ProviderPackIndexEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPackManifest {
    pub header: [u8; PACK_HEADER_LEN],
    pub output_pack_length: u64,
    pub extents: Vec<ProviderPackExtent>,
}

#[derive(Debug)]
pub struct ProviderPackBundle {
    pub pack: NativePackBundle,
    pub trailer_digest: [u8; 32],
}

/// Assemble and verify one virtual native pack from provider extent bodies.
///
/// `extent_bodies` uses the same order as `manifest.extents`. The manifest may
/// arrive in any order, but its output layout must cover every byte between the
/// 16-byte header and 32-byte trailer exactly once.
pub fn assemble_provider_pack(
    manifest: &ProviderPackManifest,
    extent_bodies: &[Vec<u8>],
) -> Result<ProviderPackBundle> {
    validate_manifest(manifest)?;
    if extent_bodies.len() != manifest.extents.len() {
        return Err(ProtocolError::InvalidState(format!(
            "provider extent count mismatch: expected {}, got {}",
            manifest.extents.len(),
            extent_bodies.len()
        )));
    }

    let mut order = (0..manifest.extents.len()).collect::<Vec<_>>();
    order.sort_unstable_by_key(|index| manifest.extents[*index].output_offset);
    let output_len = usize::try_from(manifest.output_pack_length).map_err(|_| {
        ProtocolError::InvalidState("provider output pack exceeds this platform".to_string())
    })?;
    let mut pack_data = Vec::with_capacity(output_len);
    pack_data.extend_from_slice(&manifest.header);
    for index in order {
        let extent = &manifest.extents[index];
        let body = &extent_bodies[index];
        let expected_len = usize::try_from(extent.length).map_err(|_| {
            ProtocolError::InvalidState("provider extent exceeds this platform".to_string())
        })?;
        if body.len() != expected_len {
            return Err(ProtocolError::InvalidState(format!(
                "provider extent length mismatch: expected {}, got {}",
                extent.length,
                body.len()
            )));
        }
        if blake3::hash(body).as_bytes() != &extent.digest {
            return Err(ProtocolError::InvalidState(
                "provider extent digest mismatch".to_string(),
            ));
        }
        pack_data.extend_from_slice(body);
    }

    let trailer_digest = *blake3::hash(&pack_data).as_bytes();
    pack_data.extend_from_slice(&trailer_digest);
    if pack_data.len() != output_len {
        return Err(ProtocolError::InvalidState(format!(
            "provider output pack length mismatch: expected {}, got {}",
            manifest.output_pack_length,
            pack_data.len()
        )));
    }
    verify_container(&pack_data, PACK_SPEC).map_err(ProtocolError::from)?;

    let mut index = PackIndex::new();
    for extent in &manifest.extents {
        for object in &extent.objects {
            index.add(object.id, object.output_offset);
        }
    }
    index.sort();

    Ok(ProviderPackBundle {
        pack: NativePackBundle {
            pack_data,
            index_data: index.to_bytes(),
        },
        trailer_digest,
    })
}

fn validate_manifest(manifest: &ProviderPackManifest) -> Result<()> {
    if manifest.output_pack_length > MAX_RECEIVED_PACK_SIZE
        || manifest.output_pack_length < (PACK_HEADER_LEN + PACK_TRAILER_LEN) as u64
    {
        return Err(ProtocolError::InvalidState(
            "provider output pack length is invalid".to_string(),
        ));
    }
    if &manifest.header[..4] != PACK_SPEC.magic
        || u32::from_be_bytes(manifest.header[4..8].try_into().map_err(|_| {
            ProtocolError::InvalidState("provider pack header is truncated".to_string())
        })?) != PACK_SPEC.version
    {
        return Err(ProtocolError::InvalidState(
            "provider pack header has invalid magic or version".to_string(),
        ));
    }
    let object_count = u64::from_be_bytes(manifest.header[8..16].try_into().map_err(|_| {
        ProtocolError::InvalidState("provider pack header is truncated".to_string())
    })?);
    let expected_body_end = manifest.output_pack_length - PACK_TRAILER_LEN as u64;
    let mut order = manifest.extents.iter().collect::<Vec<_>>();
    order.sort_unstable_by_key(|extent| extent.output_offset);
    let mut next_offset = PACK_HEADER_LEN as u64;
    let mut ids = HashSet::new();
    let mut object_offsets = HashSet::new();
    let mut actual_object_count = 0_u64;

    for extent in order {
        if extent.length == 0 || extent.output_offset != next_offset {
            return Err(ProtocolError::InvalidState(
                "provider extents do not exactly cover the virtual pack body".to_string(),
            ));
        }
        next_offset = extent
            .output_offset
            .checked_add(extent.length)
            .ok_or_else(|| {
                ProtocolError::InvalidState("provider extent output range overflows".to_string())
            })?;
        if next_offset > expected_body_end {
            return Err(ProtocolError::InvalidState(
                "provider extent exceeds the virtual pack body".to_string(),
            ));
        }
        for object in &extent.objects {
            if object.output_offset < extent.output_offset
                || object.output_offset >= next_offset
                || !ids.insert(object.id)
                || !object_offsets.insert(object.output_offset)
            {
                return Err(ProtocolError::InvalidState(
                    "provider object index is outside its extent or duplicated".to_string(),
                ));
            }
            actual_object_count = actual_object_count.checked_add(1).ok_or_else(|| {
                ProtocolError::InvalidState("provider object count overflows".to_string())
            })?;
        }
    }
    if next_offset != expected_body_end || actual_object_count != object_count {
        return Err(ProtocolError::InvalidState(
            "provider manifest body or object count is incomplete".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use objects::{
        object::ContentHash,
        store::{
            CompressionConfig,
            pack::{ObjectType, PackBuilder, PackIndex, PackObjectId},
        },
    };

    use super::*;

    fn source_pack() -> (Vec<u8>, Vec<u8>, Vec<PackObjectId>) {
        let ids = vec![
            PackObjectId::Hash(ContentHash::from_bytes([1; 32])),
            PackObjectId::Hash(ContentHash::from_bytes([2; 32])),
        ];
        let mut builder = PackBuilder::new(CompressionConfig {
            enabled: false,
            ..CompressionConfig::default()
        });
        builder.add_id(ids[0], ObjectType::Blob, b"provider-one".to_vec());
        builder.add_id(ids[1], ObjectType::Blob, b"provider-two".to_vec());
        let (pack, index, _) = builder.build().unwrap();
        (pack, index, ids)
    }

    fn split_manifest() -> (ProviderPackManifest, Vec<Vec<u8>>, Vec<u8>, Vec<u8>) {
        let (pack, index, ids) = source_pack();
        let parsed_index = PackIndex::from_bytes(&index).unwrap();
        let first = parsed_index.find(&ids[0]).unwrap();
        let second = parsed_index.find(&ids[1]).unwrap();
        let (first_id, first_offset, second_id, second_offset) = if first < second {
            (ids[0], first, ids[1], second)
        } else {
            (ids[1], second, ids[0], first)
        };
        let body_end = pack.len() - PACK_TRAILER_LEN;
        let first_body = pack[first_offset as usize..second_offset as usize].to_vec();
        let second_body = pack[second_offset as usize..body_end].to_vec();
        let manifest = ProviderPackManifest {
            header: pack[..PACK_HEADER_LEN].try_into().unwrap(),
            output_pack_length: pack.len() as u64,
            extents: vec![
                ProviderPackExtent {
                    output_offset: first_offset,
                    length: first_body.len() as u64,
                    digest: *blake3::hash(&first_body).as_bytes(),
                    objects: vec![ProviderPackIndexEntry {
                        id: first_id,
                        output_offset: first_offset,
                    }],
                },
                ProviderPackExtent {
                    output_offset: second_offset,
                    length: second_body.len() as u64,
                    digest: *blake3::hash(&second_body).as_bytes(),
                    objects: vec![ProviderPackIndexEntry {
                        id: second_id,
                        output_offset: second_offset,
                    }],
                },
            ],
        };
        (manifest, vec![first_body, second_body], pack, index)
    }

    #[test]
    fn provider_and_ordinary_pack_results_are_byte_identical() {
        let (manifest, bodies, source_pack, source_index) = split_manifest();

        let assembled = assemble_provider_pack(&manifest, &bodies).unwrap();

        assert_eq!(assembled.pack.pack_data, source_pack);
        assert_eq!(assembled.pack.index_data, source_index);
        let source_digest = blake3::Hash::from_bytes(
            source_pack[source_pack.len() - PACK_TRAILER_LEN..]
                .try_into()
                .unwrap(),
        );
        println!(
            "byte_identical provider_digest={} weft_digest={} pack_bytes={} index_bytes={} identical=true",
            blake3::Hash::from_bytes(assembled.trailer_digest),
            source_digest,
            source_pack.len(),
            source_index.len(),
        );
    }

    #[test]
    fn digest_mismatch_never_produces_an_installable_pack() {
        let (manifest, mut bodies, _, _) = split_manifest();
        bodies[1][0] ^= 0xff;

        let error = assemble_provider_pack(&manifest, &bodies).unwrap_err();

        assert!(error.to_string().contains("digest mismatch"));
    }

    #[test]
    fn manifest_gaps_and_duplicate_index_entries_fail_closed() {
        let (mut manifest, bodies, _, _) = split_manifest();
        manifest.extents[1].output_offset += 1;
        assert!(assemble_provider_pack(&manifest, &bodies).is_err());

        let (mut manifest, bodies, _, _) = split_manifest();
        manifest.extents[1].objects[0].id = manifest.extents[0].objects[0].id;
        assert!(assemble_provider_pack(&manifest, &bodies).is_err());
    }
}
