//! Allocation-free structural pass before owned MessagePack deserialization.
//! Each visited value consumes a wire token; depth and container fanout are
//! bounded before Serde can reserve a Vec or internally tagged Content buffer.
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use super::{MAX_RECORDS, invalid};
use crate::error::Result;

const MAX_DEPTH: usize = 12;
const MAX_FIELDS: usize = 8;
const MAX_SEQUENCE: usize = 32;
const MAX_TEXT: usize = 256;
const MAX_AUTHORITY: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum Position {
    Root { manifest: bool },
    Entries,
    Authority,
    Value,
}

pub(super) fn decode<T>(
    bytes: &[u8],
    manifest: bool,
    owned: impl FnOnce(&[u8]) -> Result<T>,
) -> Result<T> {
    let mut remaining = bytes.len();
    let mut decoder = rmp_serde::Deserializer::from_read_ref(bytes);
    Node {
        remaining: &mut remaining,
        depth: 0,
        position: Position::Root { manifest },
    }
    .deserialize(&mut decoder)
    .map_err(|_| invalid("boundary MessagePack structure exceeds decoding bounds"))?;
    owned(bytes)
}

struct Node<'a> {
    remaining: &'a mut usize,
    depth: usize,
    position: Position,
}
impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = ();
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        decoder: D,
    ) -> std::result::Result<(), D::Error> {
        if self.depth > MAX_DEPTH || *self.remaining == 0 {
            return Err(de::Error::custom("boundary decoding work bound"));
        }
        *self.remaining -= 1;
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Node<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("bounded boundary record")
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<(), E> {
        if value.len() > MAX_TEXT {
            return Err(E::custom("boundary text bound"));
        }
        Ok(())
    }
    fn visit_bytes<E: de::Error>(self, value: &[u8]) -> std::result::Result<(), E> {
        let limit = if matches!(self.position, Position::Authority) {
            MAX_AUTHORITY
        } else {
            32
        };
        if value.len() > limit {
            return Err(E::custom("boundary bytes bound"));
        }
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        let limit = if matches!(self.position, Position::Entries) {
            MAX_RECORDS
        } else {
            MAX_SEQUENCE
        };
        let declared = seq
            .size_hint()
            .ok_or_else(|| de::Error::custom("missing sequence bound"))?;
        if declared > limit || declared > *self.remaining {
            return Err(de::Error::custom("boundary sequence bound"));
        }
        for _ in 0..declared {
            if seq
                .next_element_seed(Node {
                    remaining: self.remaining,
                    depth: self.depth + 1,
                    position: Position::Value,
                })?
                .is_none()
            {
                return Err(de::Error::custom("truncated boundary sequence"));
            }
        }
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let declared = map
            .size_hint()
            .ok_or_else(|| de::Error::custom("missing map bound"))?;
        if declared > MAX_FIELDS || declared.saturating_mul(2) > *self.remaining {
            return Err(de::Error::custom("boundary field bound"));
        }
        for _ in 0..declared {
            let key: &str = map
                .next_key()?
                .ok_or_else(|| de::Error::custom("truncated boundary map"))?;
            if key.len() > MAX_TEXT {
                return Err(de::Error::custom("boundary key bound"));
            }
            *self.remaining -= 1;
            let position = match (self.position, key) {
                (Position::Root { manifest: true }, "entries") => Position::Entries,
                (_, "authority") => Position::Authority,
                _ => Position::Value,
            };
            map.next_value_seed(Node {
                remaining: self.remaining,
                depth: self.depth + 1,
                position,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_containers_never_reach_owned_deserialization() {
        let mut cases = vec![
            (true, b"\x81\xa7entries\xdd\xff\xff\xff\xff".to_vec()),
            (
                false,
                b"\x81\xb0accepting_author\xdf\xff\xff\xff\xff".to_vec(),
            ),
            (
                false,
                b"\x81\xb0accepting_author\x81\xa9authority\xc6\xff\xff\xff\xff".to_vec(),
            ),
        ];
        let mut nested = b"\x81\xb0accepting_author\x81\xa9authority".to_vec();
        nested.extend(std::iter::repeat_n(0x91, MAX_DEPTH + 2));
        nested.push(0xc0);
        cases.push((false, nested));
        for (manifest, bytes) in cases {
            let mut entered_owned = false;
            let result = decode(&bytes, manifest, |_| {
                entered_owned = true;
                Ok(())
            });
            assert!(result.is_err(), "malformed structure must fail preflight");
            assert!(
                !entered_owned,
                "owned allocation phase must remain unreachable"
            );
        }
    }
}
