// SPDX-License-Identifier: Apache-2.0
//! Bounded raw transport for the private Worker/native bridge. Headers contain
//! only canonical JSON metadata; binary source bytes are never base64 copied.
use crate::{Result, policy::canonical, transport};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const CONTENT_TYPE: &str = "application/vnd.heddle.native-frame-v1";
pub const REQUEST_MAX: usize = 112 * 1024 * 1024;
pub const RESPONSE_MAX: usize = 144 * 1024 * 1024;
pub const HEADER_MAX: usize = 256 * 1024;
pub const PARTS_MAX: usize = 260;
pub const ARTIFACTS_MAX: usize = 256;
const MAGIC: &[u8; 4] = b"HGF1";

// No Debug: a frame can contain credentials, private source or large packs.
pub struct Frame {
    pub payload: Value,
    pub parts: BTreeMap<String, Vec<u8>>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    payload: Value,
    parts: Vec<Part>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Part {
    name: String,
    length: u64,
}

impl Frame {
    pub fn json(payload: Value) -> Self {
        Self {
            payload,
            parts: BTreeMap::new(),
        }
    }

    /// Consume the request allocation. The usual single-part request keeps its
    /// buffer, shifting off the small header instead of cloning its raw body.
    pub fn decode(mut bytes: Vec<u8>, maximum: usize) -> Result<Self> {
        if bytes.len() < 8 || bytes.len() > maximum || &bytes[..4] != MAGIC {
            return Err("invalid native frame envelope".into());
        }
        let length =
            u32::from_be_bytes(bytes[4..8].try_into().map_err(|_| "native frame width")?) as usize;
        if length == 0 || length > HEADER_MAX {
            return Err("native frame header limit".into());
        }
        let start = 8usize
            .checked_add(length)
            .ok_or("native frame length overflow")?;
        let raw = bytes.get(8..start).ok_or("truncated native frame header")?;
        let header: Header =
            serde_json::from_slice(raw).map_err(|_| "invalid native frame header")?;
        if canonical(&header).map_err(|_| "invalid native frame header")? != raw {
            return Err("noncanonical native frame header".into());
        }
        let total = validate(&header, start, maximum)?;
        if total != bytes.len() {
            return Err("truncated or extra native frame bytes".into());
        }
        let mut parts = BTreeMap::new();
        if header.parts.len() == 1 {
            bytes.drain(..start);
            let part = header
                .parts
                .into_iter()
                .next()
                .ok_or("native frame part absent")?;
            parts.insert(part.name, bytes);
        } else {
            let mut offset = start;
            for part in header.parts {
                let length = usize::try_from(part.length).map_err(|_| "native frame part width")?;
                let end = offset
                    .checked_add(length)
                    .ok_or("native frame length overflow")?;
                let body = bytes
                    .get(offset..end)
                    .ok_or("truncated native frame part")?;
                parts.insert(part.name, body.to_vec());
                offset = end;
            }
        }
        Ok(Self {
            payload: header.payload,
            parts,
        })
    }

    /// Validate everything before any response byte is written. Returned bytes
    /// are only HGF1 + header length + metadata, never concatenated raw parts.
    pub(crate) fn encoded_header(&self, maximum: usize) -> Result<(Vec<u8>, usize)> {
        let header = Header {
            payload: self.payload.clone(),
            parts: self
                .parts
                .iter()
                .map(|(name, body)| Part {
                    name: name.clone(),
                    length: body.len() as u64,
                })
                .collect(),
        };
        // Bound shape, lengths and marker depth before canonical serialization.
        validate(&header, 8, maximum)?;
        let raw = canonical(&header).map_err(|_| "invalid native frame header")?;
        if raw.is_empty() || raw.len() > HEADER_MAX {
            return Err("native frame header limit".into());
        }
        let total = validate(&header, raw.len() + 8, maximum)?;
        let mut prefix = Vec::with_capacity(raw.len() + 8);
        prefix.extend_from_slice(MAGIC);
        prefix.extend_from_slice(&(raw.len() as u32).to_be_bytes());
        prefix.extend_from_slice(&raw);
        Ok((prefix, total))
    }
}

fn part_limit(name: &str) -> Option<usize> {
    match name {
        "proof" => Some(17 * 1024 * 1024),
        "request" => Some(17 * 1024 * 1024),
        "output" => Some(96 * 1024 * 1024),
        "manifest" => Some(64 * 1024),
        name if name
            .strip_prefix("artifact/")
            .is_some_and(|hash| crate::policy::hex(hash, 64)) =>
        {
            Some(64 * 1024 * 1024)
        }
        _ => None,
    }
}
fn validate(header: &Header, prefix: usize, maximum: usize) -> Result<usize> {
    if !header.payload.is_object() || header.parts.len() > PARTS_MAX || prefix > maximum {
        return Err("native frame shape or count".into());
    }
    if header
        .parts
        .iter()
        .filter(|part| part.name.starts_with("artifact/"))
        .count()
        > ARTIFACTS_MAX
    {
        return Err("native frame artifact count".into());
    }
    let mut names = BTreeSet::new();
    let mut total = prefix;
    let mut artifacts = 0usize;
    for part in &header.parts {
        let limit = part_limit(&part.name).ok_or("invalid native frame name")?;
        if !names.insert(part.name.as_str()) {
            return Err("invalid or duplicate native frame name".into());
        }
        let length = usize::try_from(part.length).map_err(|_| "native frame part width")?;
        if length > limit {
            return Err("native frame part limit".into());
        }
        if part.name.starts_with("artifact/") {
            artifacts = artifacts
                .checked_add(length)
                .filter(|size| *size <= 64 * 1024 * 1024)
                .ok_or("native frame artifact byte limit")?;
        }
        total = total
            .checked_add(length)
            .filter(|n| *n <= maximum)
            .ok_or("native frame body limit")?;
    }
    let mut references = BTreeSet::new();
    markers(&header.payload, 0, &mut references)?;
    if references != names {
        return Err("native frame references differ from parts".into());
    }
    Ok(total)
}
fn markers<'a>(value: &'a Value, depth: usize, references: &mut BTreeSet<&'a str>) -> Result<()> {
    if depth > 64 {
        return Err("native frame metadata depth".into());
    }
    match value {
        Value::Object(object) => {
            for (key, value) in object {
                if key.ends_with("_part") {
                    let name = value.as_str().ok_or("native frame marker type")?;
                    let valid =
                        match key.as_str() {
                            "proof_part" => name == "proof",
                            "request_part" => name == "request",
                            "output_part" => name == "output",
                            "manifest_part" => name == "manifest",
                            "bytes_part" => object
                                .get("sha256")
                                .and_then(Value::as_str)
                                .is_some_and(|hash| {
                                    crate::policy::hex(hash, 64)
                                        && name.strip_prefix("artifact/") == Some(hash)
                                }),
                            _ => false,
                        };
                    if !valid {
                        return Err("native frame marker name mismatch".into());
                    }
                    references.insert(name);
                } else {
                    markers(value, depth + 1, references)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                markers(value, depth + 1, references)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub fn reply(request: &mut transport::Request, frame: Frame) -> Result<()> {
    transport::reply_native_frame(request, frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn encode(frame: &Frame, max: usize) -> Result<Vec<u8>> {
        let (mut bytes, _) = frame.encoded_header(max)?;
        for body in frame.parts.values() {
            bytes.extend_from_slice(body);
        }
        Ok(bytes)
    }
    fn raw(header: Value, bodies: &[u8]) -> Vec<u8> {
        let h = canonical(&header).expect("header");
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&(h.len() as u32).to_be_bytes());
        out.extend_from_slice(&h);
        out.extend_from_slice(bodies);
        out
    }
    #[test]
    fn shared_javascript_golden_vector_is_byte_exact() {
        let golden_hex = "48474631000000627b227061727473223a5b7b226c656e677468223a342c226e616d65223a2270726f6f66227d5d2c227061796c6f6164223a7b226d6574686f64223a2276616c69646174652d706c616e222c2270726f6f665f70617274223a2270726f6f66227d7d0a00ff0180";
        let golden: Vec<u8> = golden_hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair).expect("hex UTF-8"), 16)
                    .expect("hex byte")
            })
            .collect();
        let frame = Frame {
            payload: json!({"method":"validate-plan","proof_part":"proof"}),
            parts: BTreeMap::from([("proof".into(), vec![0x00, 0xff, 0x01, 0x80])]),
        };
        assert_eq!(golden.len(), 110);
        assert!(encode(&frame, REQUEST_MAX).expect("encode") == golden);
        let decoded = Frame::decode(golden, REQUEST_MAX).expect("decode golden");
        assert!(decoded.payload == frame.payload);
        assert!(decoded.parts == frame.parts);
    }
    #[test]
    fn raw_parts_round_trip_with_exact_references_and_no_body_concatenation_in_header() {
        let hash = "a".repeat(64);
        let name = format!("artifact/{hash}");
        let frame = Frame {
            payload: json!({"proof_part":"proof","manifest_part":"manifest","git":{"request_part":"request"},"artifacts":[{"sha256":hash,"bytes_part":name}]}),
            parts: BTreeMap::from([
                ("proof".into(), vec![0, 255, 1]),
                ("request".into(), b"Git raw pack".to_vec()),
                ("manifest".into(), b"{}\n".to_vec()),
                (name, vec![8; 4096]),
            ]),
        };
        let (header, total) = frame.encoded_header(REQUEST_MAX).expect("header");
        assert!(header.len() < 1024);
        assert_eq!(
            total,
            header.len() + frame.parts.values().map(Vec::len).sum::<usize>()
        );
        let decoded = Frame::decode(encode(&frame, REQUEST_MAX).expect("encode"), REQUEST_MAX)
            .expect("decode");
        assert!(decoded.payload == frame.payload, "metadata differs");
        assert!(decoded.parts == frame.parts, "binary parts differ");
    }
    #[test]
    fn single_part_decoder_reuses_request_allocation_and_empty_frame_is_valid() {
        let frame = Frame {
            payload: json!({"request_part":"request"}),
            parts: BTreeMap::from([("request".into(), vec![7; 64 * 1024])]),
        };
        let bytes = encode(&frame, REQUEST_MAX).expect("encode");
        let pointer = bytes.as_ptr();
        let decoded = Frame::decode(bytes, REQUEST_MAX).expect("decode");
        assert_eq!(decoded.parts["request"].as_ptr(), pointer);
        let empty = Frame::json(json!({"authorized":true}));
        assert!(Frame::decode(encode(&empty, REQUEST_MAX).expect("encode"), REQUEST_MAX).is_ok());
    }
    #[test]
    fn malformed_widths_lengths_names_references_and_extra_bytes_fail_closed() {
        let cases = [
            json!({"payload":{},"parts":[{"name":"proof","length":0}]}),
            json!({"payload":{"proof_part":"proof"},"parts":[]}),
            json!({"payload":{"proof_part":"request"},"parts":[{"name":"request","length":0}]}),
            json!({"payload":{"unknown_part":"proof"},"parts":[{"name":"proof","length":0}]}),
            json!({"payload":{"proof_part":"proof"},"parts":[{"name":"proof","length":0},{"name":"proof","length":0}]}),
            json!({"payload":{"proof_part":"proof"},"parts":[{"name":"proof","length":u64::MAX}]}),
            json!({"payload":{"artifacts":[{"sha256":"a".repeat(64),"bytes_part":format!("artifact/{}","b".repeat(64))}]},"parts":[{"name":format!("artifact/{}","b".repeat(64)),"length":0}]}),
            json!({"payload":{"proof_part":"proof"},"parts":[{"name":"proof","length":1}],"unknown":0}),
        ];
        for header in cases {
            assert!(Frame::decode(raw(header, &[]), REQUEST_MAX).is_err());
        }
        let valid = raw(
            json!({"payload":{"output_part":"output"},"parts":[{"name":"output","length":1}]}),
            &[3],
        );
        let mut extra = valid.clone();
        extra.push(9);
        assert!(Frame::decode(extra, RESPONSE_MAX).is_err());
        assert!(Frame::decode(valid[..valid.len() - 1].to_vec(), RESPONSE_MAX).is_err());
        assert!(Frame::decode(valid.clone(), valid.len() - 1).is_err());
        let mut magic = valid;
        magic[0] = b'X';
        assert!(Frame::decode(magic, RESPONSE_MAX).is_err());
    }
    #[test]
    fn metadata_part_count_manifest_and_total_boundaries_are_enforced() {
        let oversized = Frame::json(json!({"large":"x".repeat(HEADER_MAX)}));
        assert!(oversized.encoded_header(RESPONSE_MAX).is_err());
        let mut frame = Frame {
            payload: json!({"manifest_part":"manifest"}),
            parts: BTreeMap::from([("manifest".into(), vec![0; 64 * 1024])]),
        };
        let bytes = encode(&frame, REQUEST_MAX).expect("exact manifest bound");
        assert!(Frame::decode(bytes.clone(), bytes.len()).is_ok());
        frame.parts.get_mut("manifest").expect("manifest").push(0);
        assert!(frame.encoded_header(REQUEST_MAX).is_err());
        let mut header = json!({"payload":{},"parts":[]});
        header["parts"] = Value::Array(
            (0..261)
                .map(|_| json!({"name":"proof","length":0}))
                .collect(),
        );
        assert!(Frame::decode(raw(header, &[]), REQUEST_MAX).is_err());
    }
    #[test]
    fn full_artifact_inventory_leaves_room_for_all_four_named_parts() {
        let mut frame = Frame {
            payload: json!({"proof_part":"proof","request_part":"request","output_part":"output","manifest_part":"manifest","artifacts":[]}),
            parts: ["proof", "request", "output", "manifest"]
                .into_iter()
                .map(|name| (name.into(), Vec::new()))
                .collect(),
        };
        for i in 0..ARTIFACTS_MAX {
            let hash = format!("{i:064x}");
            let name = format!("artifact/{hash}");
            frame.payload["artifacts"]
                .as_array_mut()
                .expect("artifacts")
                .push(json!({"sha256":hash,"bytes_part":name}));
            frame.parts.insert(name, Vec::new());
        }
        assert_eq!(frame.parts.len(), PARTS_MAX);
        assert!(
            Frame::decode(
                encode(&frame, REQUEST_MAX).expect("full inventory"),
                REQUEST_MAX
            )
            .is_ok()
        );
        // A repeated reference is a shared artifact, not another raw body.
        let alias = frame.payload["artifacts"][0].clone();
        frame.payload["artifacts"]
            .as_array_mut()
            .expect("artifacts")
            .push(alias);
        assert!(frame.encoded_header(REQUEST_MAX).is_ok());
        // The artifact ceiling still holds even below the total part ceiling.
        for name in ["proof", "request", "output", "manifest"] {
            frame.parts.remove(name);
            frame
                .payload
                .as_object_mut()
                .expect("payload")
                .remove(&format!("{name}_part"));
        }
        let hash = format!("{:064x}", ARTIFACTS_MAX);
        let name = format!("artifact/{hash}");
        frame.payload["artifacts"]
            .as_array_mut()
            .expect("artifacts")
            .push(json!({"sha256":hash,"bytes_part":name}));
        frame.parts.insert(name, Vec::new());
        assert_eq!(frame.parts.len(), 257);
        assert!(frame.encoded_header(REQUEST_MAX).is_err());
    }
    #[test]
    fn per_part_and_aggregate_artifact_caps_precede_body_allocation() {
        for (name, maximum) in [
            ("proof", 17 * 1024 * 1024),
            ("request", 17 * 1024 * 1024),
            ("output", 96 * 1024 * 1024),
            ("manifest", 64 * 1024),
        ] {
            let mut payload = serde_json::Map::new();
            payload.insert(format!("{name}_part"), Value::String(name.into()));
            let mut header = Header {
                payload: Value::Object(payload),
                parts: vec![Part {
                    name: name.into(),
                    length: maximum,
                }],
            };
            assert_eq!(
                validate(&header, 8, RESPONSE_MAX).expect("exact part cap"),
                maximum as usize + 8
            );
            header.parts[0].length += 1;
            assert!(validate(&header, 8, RESPONSE_MAX).is_err());
        }
        let hashes = ["a".repeat(64), "b".repeat(64)];
        let mut header = Header {
            payload: json!({"artifacts":hashes.iter().map(|hash| json!({"sha256":hash,"bytes_part":format!("artifact/{hash}")})).collect::<Vec<_>>()}),
            parts: hashes
                .iter()
                .map(|hash| Part {
                    name: format!("artifact/{hash}"),
                    length: 32 * 1024 * 1024,
                })
                .collect(),
        };
        assert_eq!(
            validate(&header, 8, REQUEST_MAX).expect("exact combined artifact cap"),
            64 * 1024 * 1024 + 8
        );
        header.parts[1].length += 1;
        assert!(validate(&header, 8, REQUEST_MAX).is_err());
    }
    #[test]
    fn noncanonical_json_and_duplicate_header_fields_never_echo_payloads() {
        let secret = "SYNTHETIC_PRIVATE_DATA";
        for header in [
            format!("{{\"payload\":{{\"note\":\"{secret}\"}},\"parts\":[]}}"),
            format!("{{\"parts\":[],\"payload\":{{}},\"payload\":{{\"note\":\"{secret}\"}}}}\n"),
        ] {
            let mut bytes = MAGIC.to_vec();
            bytes.extend_from_slice(&(header.len() as u32).to_be_bytes());
            bytes.extend_from_slice(header.as_bytes());
            let error = Frame::decode(bytes, REQUEST_MAX)
                .err()
                .expect("bad header")
                .to_string();
            assert!(!error.contains(secret));
        }
    }
}
