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
    /// Concurrent policies restrict lifetime; they never extend one another.
    /// The same rule applies to Thread policy and a material-specific limit.
    pub fn intersection(self, other: Self) -> Result<Self> {
        self.validate()?;
        other.validate()?;
        Ok(match (self, other) {
            (Self::Discard, _) | (_, Self::Discard) => Self::Discard,
            (Self::Retain, value) | (value, Self::Retain) => value,
            (Self::Bounded(left), Self::Bounded(right)) => Self::Bounded(left.min(right)),
        })
    }

    /// Resolve from the immutable original retention timestamp, in Unix seconds.
    /// Callers persist this deadline; retry or delivery time must not replace
    /// the original timestamp. `None` denotes explicitly unbounded retention.
    pub fn deadline(self, original_unix_seconds: i64) -> Result<Option<i64>> {
        self.validate()?;
        match self {
            Self::Discard => Ok(Some(original_unix_seconds)),
            Self::Retain => Ok(None),
            Self::Bounded(seconds) => {
                let seconds = i64::try_from(seconds)
                    .map_err(|_| invalid("Thread retention duration overflows timestamp"))?;
                original_unix_seconds
                    .checked_add(seconds)
                    .map(Some)
                    .ok_or_else(|| invalid("Thread retention deadline overflows timestamp"))
            }
        }
    }

    pub fn validate(self) -> Result<()> {
        if let Self::Bounded(seconds) = self {
            if seconds == 0 || seconds > (i64::MAX / 1000) as u64 {
                return Err(invalid("invalid bounded Thread retention duration"));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MaterialRetention::{self, Bounded, Discard, Retain};

    #[test]
    fn concurrent_limits_are_order_independent_and_never_extend_lifetime() {
        let policies = [Discard, Bounded(10), Bounded(60), Retain];
        for left in policies {
            for right in policies {
                let resolved = left.intersection(right).expect("valid policies");
                assert_eq!(resolved, right.intersection(left).expect("valid policies"));
                for original in [100, 200] {
                    let deadline = resolved.deadline(original).expect("bounded timestamp");
                    for candidate in [left, right] {
                        if let Some(limit) = candidate.deadline(original).expect("valid deadline") {
                            assert!(deadline.is_some_and(|value| value <= limit));
                        }
                    }
                }
            }
        }
        assert_eq!(Retain.intersection(Retain).expect("valid policies"), Retain);
    }

    #[test]
    fn deadlines_reject_invalid_or_overflowing_lifetimes() {
        assert!(Bounded(0).deadline(100).is_err());
        assert!(Bounded(1).deadline(i64::MAX).is_err());
        assert!(
            MaterialRetention::Bounded(u64::MAX)
                .intersection(Discard)
                .is_err()
        );
        assert_eq!(
            Bounded(10).deadline(100).expect("valid deadline"),
            Some(110)
        );
        assert_eq!(Discard.deadline(100).expect("discard deadline"), Some(100));
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
