//! Bounded terminal proof for a server-owned tool execution ledger.
//!
//! The receipt contains fixed-size aggregates and a rolling root, never a
//! per-call history.  `digest` is a checksum of the canonical receipt payload
//! (everything except `digest` itself), so an Edge consumer can validate the
//! envelope without access to the Server's private execution records.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const EMPTY_TOOL_LEDGER_ROOT: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLedgerResultClassCounts {
    pub succeeded: u32,
    pub failed: u32,
    pub rejected: u32,
    pub reused: u32,
    pub suppressed: u32,
}

/// Fixed-size execution aggregate carried from the runtime accumulator to
/// user-facing terminal projection. It deliberately contains no run binding,
/// digest, or per-call identity; those belong to the durable receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLedgerCanonicalAggregate {
    pub attempted: u32,
    pub terminal: u32,
    pub unresolved: u32,
    pub result_classes: ToolLedgerResultClassCounts,
    pub consistent: bool,
}

impl Default for ToolLedgerCanonicalAggregate {
    fn default() -> Self {
        Self {
            attempted: 0,
            terminal: 0,
            unresolved: 0,
            result_classes: ToolLedgerResultClassCounts::default(),
            consistent: true,
        }
    }
}

impl ToolLedgerCanonicalAggregate {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.terminal.checked_add(self.unresolved) != Some(self.attempted) {
            return Err("tool ledger aggregate counters do not close");
        }
        if self.result_classes.total() != Some(self.terminal) {
            return Err("tool ledger aggregate result classes do not equal terminal count");
        }
        Ok(())
    }

    #[must_use]
    pub fn is_complete_for(self, tool_calls_count: u32) -> bool {
        self.validate().is_ok()
            && self.consistent
            && self.unresolved == 0
            && self.attempted == tool_calls_count
    }
}

impl ToolLedgerResultClassCounts {
    #[must_use]
    pub fn total(self) -> Option<u32> {
        self.succeeded
            .checked_add(self.failed)?
            .checked_add(self.rejected)?
            .checked_add(self.reused)?
            .checked_add(self.suppressed)
    }

    pub fn checked_add_assign(&mut self, other: Self) -> bool {
        let Some(succeeded) = self.succeeded.checked_add(other.succeeded) else {
            return false;
        };
        let Some(failed) = self.failed.checked_add(other.failed) else {
            return false;
        };
        let Some(rejected) = self.rejected.checked_add(other.rejected) else {
            return false;
        };
        let Some(reused) = self.reused.checked_add(other.reused) else {
            return false;
        };
        let Some(suppressed) = self.suppressed.checked_add(other.suppressed) else {
            return false;
        };
        *self = Self {
            succeeded,
            failed,
            rejected,
            reused,
            suppressed,
        };
        true
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLedgerReceipt {
    pub run_id: String,
    pub owner_generation: u64,
    pub attempted: u32,
    pub terminal: u32,
    pub unresolved: u32,
    pub result_classes: ToolLedgerResultClassCounts,
    /// Highest contiguous attempt sequence folded into `ledger_root`.
    pub watermark: u64,
    /// SHA-256 rolling root over canonical settled call facts.
    pub ledger_root: String,
    /// False after a conflicting duplicate or a live reorder-window overflow.
    pub consistent: bool,
    /// SHA-256 of the canonical payload above, excluding this field.
    pub digest: String,
}

impl Default for ToolLedgerReceipt {
    fn default() -> Self {
        // Default is deliberately not a valid authority receipt. Callers must
        // bind an exact run/generation before crossing a terminal boundary.
        Self::empty(String::new(), 0)
    }
}

#[derive(Serialize)]
struct CanonicalReceiptPayload<'a> {
    run_id: &'a str,
    owner_generation: u64,
    attempted: u32,
    terminal: u32,
    unresolved: u32,
    result_classes: ToolLedgerResultClassCounts,
    watermark: u64,
    ledger_root: &'a str,
    consistent: bool,
}

impl ToolLedgerReceipt {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: impl Into<String>,
        owner_generation: u64,
        attempted: u32,
        terminal: u32,
        unresolved: u32,
        result_classes: ToolLedgerResultClassCounts,
        watermark: u64,
        ledger_root: impl Into<String>,
        consistent: bool,
    ) -> Self {
        let mut receipt = Self {
            run_id: run_id.into(),
            owner_generation,
            attempted,
            terminal,
            unresolved,
            result_classes,
            watermark,
            ledger_root: ledger_root.into(),
            consistent,
            digest: String::new(),
        };
        receipt.digest = receipt.canonical_digest();
        receipt
    }

    #[must_use]
    pub fn empty(run_id: impl Into<String>, owner_generation: u64) -> Self {
        Self::new(
            run_id,
            owner_generation,
            0,
            0,
            0,
            ToolLedgerResultClassCounts::default(),
            0,
            EMPTY_TOOL_LEDGER_ROOT,
            true,
        )
    }

    #[must_use]
    pub fn canonical_digest(&self) -> String {
        let payload = CanonicalReceiptPayload {
            run_id: &self.run_id,
            owner_generation: self.owner_generation,
            attempted: self.attempted,
            terminal: self.terminal,
            unresolved: self.unresolved,
            result_classes: self.result_classes,
            watermark: self.watermark,
            ledger_root: &self.ledger_root,
            consistent: self.consistent,
        };
        let bytes =
            serde_json::to_vec(&payload).expect("fixed ToolLedgerReceipt payload must serialize");
        format!("{:x}", Sha256::digest(bytes))
    }

    #[must_use]
    pub fn canonical_aggregate(&self) -> ToolLedgerCanonicalAggregate {
        ToolLedgerCanonicalAggregate {
            attempted: self.attempted,
            terminal: self.terminal,
            unresolved: self.unresolved,
            result_classes: self.result_classes,
            consistent: self.consistent,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.run_id.trim().is_empty() || self.run_id.trim() != self.run_id {
            return Err("tool ledger receipt run_id is missing or non-canonical");
        }
        self.canonical_aggregate().validate()?;
        if self.watermark != u64::from(self.terminal) {
            return Err("tool ledger receipt watermark does not equal contiguous terminal count");
        }
        if !is_sha256_hex(&self.ledger_root) || !is_sha256_hex(&self.digest) {
            return Err("tool ledger receipt digest is not canonical SHA-256 hex");
        }
        if self.digest != self.canonical_digest() {
            return Err("tool ledger receipt payload digest does not match");
        }
        Ok(())
    }

    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.validate().is_ok() && self.consistent && self.unresolved == 0
    }
}

#[must_use]
pub fn roll_tool_ledger_root(
    previous_root: &str,
    sequence: u64,
    call_id: &str,
    result_class: &str,
) -> String {
    let mut hasher = Sha256::new();
    for bytes in [
        previous_root.as_bytes(),
        call_id.as_bytes(),
        result_class.as_bytes(),
    ] {
        hasher.update(bytes.len().to_be_bytes());
        hasher.update(bytes);
    }
    hasher.update(sequence.to_be_bytes());
    format!("{:x}", hasher.finalize())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_digest_excludes_itself_and_detects_mutation() {
        let receipt = ToolLedgerReceipt::empty("run-1", 7);
        assert_eq!(receipt.digest, receipt.canonical_digest());
        assert!(receipt.validate().is_ok());

        let mut changed = receipt;
        changed.unresolved = 1;
        assert!(changed.validate().is_err());
    }

    #[test]
    fn first_generation_zero_is_valid_exact_authority() {
        let receipt = ToolLedgerReceipt::empty("run-generation-zero", 0);
        assert!(receipt.validate().is_ok());
        assert!(receipt.is_complete());
    }

    #[test]
    fn aggregate_completion_requires_class_sum_and_tool_count() {
        let complete = ToolLedgerCanonicalAggregate {
            attempted: 2,
            terminal: 2,
            unresolved: 0,
            result_classes: ToolLedgerResultClassCounts {
                succeeded: 1,
                failed: 1,
                ..Default::default()
            },
            consistent: true,
        };
        assert!(complete.is_complete_for(2));
        assert!(!complete.is_complete_for(1));

        let mut malformed = complete;
        malformed.result_classes.failed = 0;
        assert!(malformed.validate().is_err());
        assert!(!malformed.is_complete_for(2));
    }

    #[test]
    fn rolling_root_is_deterministic_but_sequence_sensitive() {
        let a = roll_tool_ledger_root(EMPTY_TOOL_LEDGER_ROOT, 1, "call-1", "failed");
        let replay = roll_tool_ledger_root(EMPTY_TOOL_LEDGER_ROOT, 1, "call-1", "failed");
        let different = roll_tool_ledger_root(EMPTY_TOOL_LEDGER_ROOT, 2, "call-1", "failed");
        assert_eq!(a, replay);
        assert_ne!(a, different);
    }
}
