// SPDX-License-Identifier: Apache-2.0
//! Exact demo scope and live-disclosure contract. A static bundle is not live authority.
use crate::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::Read,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_POLICY: usize = 64 * 1024;
pub fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
pub fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn thread(s: &str) -> bool {
    s.len() <= 255 && s.split('/').count() <= 8 && s.split('/').all(name)
}
pub fn canonical<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    // Recursively sorted map keys regardless of serde_json feature unification.
    fn sorted(value: serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let ordered: BTreeMap<_, _> =
                    map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
                serde_json::Value::Object(ordered.into_iter().collect())
            }
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.into_iter().map(sorted).collect())
            }
            other => other,
        }
    }
    // Serialize through a dedicated map rather than relying on the Value map order.
    fn encode(value: &serde_json::Value, out: &mut Vec<u8>) -> Result<()> {
        match value {
            serde_json::Value::Object(map) => {
                out.push(b'{');
                for (i, (key, val)) in map
                    .iter()
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .enumerate()
                {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend(serde_json::to_vec(key)?);
                    out.push(b':');
                    encode(val, out)?;
                }
                out.push(b'}');
            }
            serde_json::Value::Array(a) => {
                out.push(b'[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    encode(v, out)?;
                }
                out.push(b']');
            }
            _ => out.extend(serde_json::to_vec(value)?),
        }
        Ok(())
    }
    let mut bytes = Vec::new();
    encode(&sorted(serde_json::to_value(value)?), &mut bytes)?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: u64,
    pub repository: String,
    pub source: String,
    pub thread: String,
    pub state: String,
    pub git_oid: String,
    pub mode: String,
    pub policy_epoch: u64,
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        if self.schema != 1
            || !name(&self.repository)
            || !name(&self.source)
            || !thread(&self.thread)
            || !self.state.starts_with("hs-")
            || self.state.len() != 55
            || !self.state[3..]
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            || !hex(&self.git_oid, 40)
            || !matches!(self.mode.as_str(), "snapshot" | "history")
            || self.policy_epoch == 0
            || self.policy_epoch > 9_007_199_254_740_991
        {
            return Err("invalid manifest".into());
        }
        Ok(())
    }
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema: u64,
    pub expires_at: u64,
    pub published_pins: Vec<String>,
    pub reader_sha256: String,
    pub service_sha256: String,
    pub views: Vec<Manifest>,
    pub descriptors: BTreeMap<String, String>,
    pub authorized_threads: BTreeMap<String, Vec<String>>,
}
impl Config {
    pub fn validate(&self) -> Result<()> {
        let now = now()?;
        if self.schema != 1
            || !(now < self.expires_at && self.expires_at <= now + 900)
            || !hex(&self.reader_sha256, 64)
            || !hex(&self.service_sha256, 64)
            || self.reader_sha256 == self.service_sha256
            || self.published_pins.is_empty()
            || self.published_pins.len() > 2
            || self.published_pins.len() != self.views.len()
            || self.published_pins.iter().any(|p| !hex(p, 40))
            || self.published_pins.windows(2).any(|w| w[0] >= w[1])
            || self.descriptors.len() != 1
            || self.authorized_threads.len() != 1
        {
            return Err("invalid or expired configuration".into());
        }
        for (source, sha) in &self.descriptors {
            let names = self
                .authorized_threads
                .get(source)
                .ok_or("missing Thread grants")?;
            if !name(source)
                || !hex(sha, 64)
                || names.is_empty()
                || names.len() > 128
                || names.iter().any(|s| !thread(s))
                || names
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != names.len()
            {
                return Err("invalid source grants".into());
            }
        }
        for m in &self.views {
            m.validate()?;
            if !self.descriptors.contains_key(&m.source)
                || !self
                    .authorized_threads
                    .get(&m.source)
                    .is_some_and(|n| n.contains(&m.thread))
            {
                return Err("ungranted view".into());
            }
        }
        Ok(())
    }
    pub fn authorize(&self, pin: &str, reader: &str, service: &str) -> Result<&Manifest> {
        self.validate()?;
        if !constant_eq(&credential(reader)?, &self.reader_sha256)
            || !constant_eq(&credential(service)?, &self.service_sha256)
        {
            return Err("denied".into());
        }
        let index = self
            .published_pins
            .iter()
            .position(|p| p == pin)
            .ok_or("unpublished pin")?;
        Ok(&self.views[index])
    }
    pub fn threads(&self, m: &Manifest) -> Result<&[String]> {
        Ok(self
            .authorized_threads
            .get(&m.source)
            .ok_or("missing Threads")?)
    }
    pub fn bundle(&self, m: &Manifest) -> Result<&str> {
        Ok(self
            .descriptors
            .get(&m.source)
            .ok_or("missing native source")?)
    }
}
fn constant_eq(a: &str, b: &str) -> bool {
    let mut diff = a.len() ^ b.len();
    for i in 0..64 {
        diff |= usize::from(
            a.as_bytes().get(i).copied().unwrap_or(0) ^ b.as_bytes().get(i).copied().unwrap_or(0),
        );
    }
    diff == 0
}
fn credential(s: &str) -> Result<String> {
    let t = s.strip_prefix("Bearer ").ok_or("missing bearer")?;
    if !(32..=256).contains(&t.len())
        || !t
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("invalid bearer".into());
    }
    Ok(digest(t.as_bytes()))
}

pub enum ConfigSource {
    File(PathBuf),
    Environment(String),
}
impl ConfigSource {
    pub fn load(&self) -> Result<Config> {
        let data = match self {
            Self::File(path) => {
                let mut bytes = Vec::new();
                File::open(path)?
                    .take((MAX_POLICY + 1) as u64)
                    .read_to_end(&mut bytes)?;
                bytes
            }
            Self::Environment(s) => s.as_bytes().to_vec(),
        };
        if data.len() > MAX_POLICY {
            return Err("configuration limit".into());
        }
        let c: Config = serde_json::from_slice(&data)?;
        if canonical(&c)? != data {
            return Err("noncanonical configuration".into());
        }
        c.validate()?;
        Ok(c)
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Disclosure {
    pub schema: u64,
    pub authority: String,
    pub allow: bool,
    pub audience: String,
    pub reader_sha256: String,
    pub pin: String,
    pub manifest_sha256: String,
    pub native_sha256: String,
    pub threads_sha256: String,
    pub generation: String,
    pub expires_at: u64,
}
impl Disclosure {
    pub fn validate(&self, c: &Config, m: &Manifest, pin: &str, local: bool) -> Result<()> {
        let now = now()?;
        let mut threads = c.threads(m)?.to_vec();
        threads.sort();
        let authority = if local {
            "quiescent-synthetic"
        } else {
            "heddle-current-disclosure"
        };
        if self.schema != 1
            || !self.allow
            || self.authority != authority
            || self.audience != "public"
            || self.reader_sha256 != c.reader_sha256
            || self.pin != pin
            || self.manifest_sha256 != digest(&canonical(m)?)
            || self.native_sha256 != c.bundle(m)?
            || self.threads_sha256 != digest(&canonical(&threads)?)
            || !hex(&self.generation, 64)
            || !(now < self.expires_at
                && self.expires_at <= now + 30
                && self.expires_at <= c.expires_at)
        {
            return Err("missing, stale or mismatched disclosure authority".into());
        }
        Ok(())
    }
    pub fn same_generation(&self, other: &Self) -> bool {
        // Short-lived decision expiry may refresh; every authority/scope field and generation must remain identical.
        let mut a = self.clone();
        let mut b = other.clone();
        a.expires_at = 0;
        b.expires_at = 0;
        a == b
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_nested_keys() {
        assert_eq!(
            canonical(&serde_json::json!({"z":1,"a":{"z":2,"a":3}})).expect("json"),
            b"{\"a\":{\"a\":3,\"z\":2},\"z\":1}\n"
        );
    }
    #[test]
    fn credentials_strict() {
        assert!(credential("Bearer PUBLIC_TEST_VECTOR_READER_NOT_SECRET_0000000").is_ok());
        assert!(credential("Bearer short").is_err());
        assert!(credential("Basic foo").is_err());
    }
    #[test]
    fn manifest_duplicate_and_unknown_denied() {
        assert!(serde_json::from_str::<Manifest>(r#"{"schema":1,"schema":1}"#).is_err());
        assert!(!hex("A000000000000000000000000000000000000000", 40));
        assert!(!thread("main/../private"));
    }
}
