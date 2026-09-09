//! Material lifetime, independent of audience and replication destination.
use serde::{Deserialize, Serialize};

use super::super::invalid;
use crate::error::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "seconds", rename_all = "snake_case")]
pub enum MaterialRetention {
    Discard,
    Bounded(u64),
    Retain,
}

impl MaterialRetention {
    pub fn validate(self) -> Result<()> {
        if let Self::Bounded(seconds) = self {
            if seconds == 0 || seconds > (i64::MAX / 1000) as u64 {
                return Err(invalid("invalid bounded Thread retention duration"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionPolicy {
    pub source: MaterialRetention,
    pub collaboration: MaterialRetention,
    pub evidence: MaterialRetention,
    pub scrubbed_timeline: MaterialRetention,
    pub raw_transcripts: MaterialRetention,
}

impl RetentionPolicy {
    pub fn validate(&self) -> Result<()> {
        for policy in [
            self.source,
            self.collaboration,
            self.evidence,
            self.scrubbed_timeline,
            self.raw_transcripts,
        ] {
            policy.validate()?;
        }
        if self.raw_transcripts == MaterialRetention::Retain {
            return Err(invalid(
                "raw Thread retention requires an explicit bounded lifetime",
            ));
        }
        Ok(())
    }
}
