// SPDX-License-Identifier: Apache-2.0
//! Shared resource caps for scratch-budgeted LCS and resumable blame slices.

use std::fmt;

/// Kind of bounded resource consumed by a line-diff or blame slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    ScratchBytes,
    Lines,
    Work,
    States,
    DecodedBytes,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScratchBytes => "scratch_bytes",
            Self::Lines => "lines",
            Self::Work => "work",
            Self::States => "states",
            Self::DecodedBytes => "decoded_bytes",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Terminal, typed budget failure. Distinct from malformed input or I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub kind: ResourceKind,
    pub limit: u64,
    pub needed: u64,
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "budget exceeded for {}: needed {}, limit {}",
            self.kind, self.needed, self.limit
        )
    }
}

impl std::error::Error for BudgetExceeded {}

/// Observable consumption after a bounded operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResourceUsage {
    pub scratch_bytes: u64,
    pub lines: u64,
    pub work: u64,
    pub states: u64,
    pub decoded_bytes: u64,
}

impl ResourceUsage {
    pub fn get(self, kind: ResourceKind) -> u64 {
        match kind {
            ResourceKind::ScratchBytes => self.scratch_bytes,
            ResourceKind::Lines => self.lines,
            ResourceKind::Work => self.work,
            ResourceKind::States => self.states,
            ResourceKind::DecodedBytes => self.decoded_bytes,
        }
    }
}

/// One shared cap/usage tracker. LCS and blame slices use this type, not
/// per-call helper counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBudget {
    limits: ResourceUsage,
    used: ResourceUsage,
}

impl ResourceBudget {
    pub fn new(limits: ResourceUsage) -> Self {
        Self {
            limits,
            used: ResourceUsage::default(),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(ResourceUsage {
            scratch_bytes: u64::MAX,
            lines: u64::MAX,
            work: u64::MAX,
            states: u64::MAX,
            decoded_bytes: u64::MAX,
        })
    }

    pub fn limit(&self, kind: ResourceKind) -> u64 {
        self.limits.get(kind)
    }

    pub fn used(&self) -> ResourceUsage {
        self.used
    }

    pub fn require(&self, kind: ResourceKind, needed: u64) -> Result<(), BudgetExceeded> {
        let limit = self.limit(kind);
        if needed > limit {
            return Err(BudgetExceeded {
                kind,
                limit,
                needed,
            });
        }
        Ok(())
    }

    pub fn consume(&mut self, kind: ResourceKind, amount: u64) -> Result<(), BudgetExceeded> {
        let used = self.used.get(kind).saturating_add(amount);
        self.require(kind, used)?;
        match kind {
            ResourceKind::ScratchBytes => self.used.scratch_bytes = used,
            ResourceKind::Lines => self.used.lines = used,
            ResourceKind::Work => self.used.work = used,
            ResourceKind::States => self.used.states = used,
            ResourceKind::DecodedBytes => self.used.decoded_bytes = used,
        }
        Ok(())
    }

    pub fn record(&mut self, kind: ResourceKind, used: u64) {
        match kind {
            ResourceKind::ScratchBytes => self.used.scratch_bytes = used,
            ResourceKind::Lines => self.used.lines = used,
            ResourceKind::Work => self.used.work = used,
            ResourceKind::States => self.used.states = used,
            ResourceKind::DecodedBytes => self.used.decoded_bytes = used,
        }
    }
}
