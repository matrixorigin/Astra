//! Durable facts for optional, source-bound tool-result projection.
//!
//! A judgment recommendation is not an adopted context mutation. A decision
//! is frozen only inside the existing owner/session/generation admission
//! envelope; a receipt is meaningful only when persisted with that physical
//! attempt and matched to its exact provider wire hash.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TOOL_RESULT_PROJECTION_POLICY_VERSION: u32 = 2;
pub const TOOL_RESULT_PROJECTION_RENDERER_VERSION: u32 = 2;
const MAX_RANGES: usize = 32;
const MAX_ID_BYTES: usize = 256;
const MAX_RUN_ID_BYTES: usize = 64;
const MAX_CALL_ID_BYTES: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultProjectionDispositionV1 {
    Baseline,
    Selected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultProjectionFallbackV1 {
    JudgmentUnavailable,
    JudgmentInvalid,
    NoClearMatch,
    IncompleteCoverage,
    ProjectionNotSmaller,
    RecoveryUnavailable,
    PresentationNotEligible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultProjectionRangeV1 {
    pub chunk_id: String,
    pub start_byte: u64,
    pub end_byte: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultProjectionDecisionV1 {
    pub policy_version: u32,
    pub producer_run_id: String,
    pub producer_call_id: String,
    pub source_sha256: String,
    pub source_bytes: u64,
    /// Identity of the goal-bound typed selection subject. A changed target
    /// creates a distinct freeze slot so a projection selected for one goal
    /// cannot be silently reused for another. The subject is derived from
    /// trusted artifact and chunk geometry, not from provider request text.
    pub target_sha256: String,
    /// Stable identity of the canonical tool-result message, not the digest
    /// of a continually growing conversation.
    pub canonical_message_sha256: String,
    /// Stable within the outer owner/session/generation admission envelope.
    pub freeze_key_sha256: String,
    pub disposition: ToolResultProjectionDispositionV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_ranges: Vec<ToolResultProjectionRangeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judgment_invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<ToolResultProjectionFallbackV1>,
    pub renderer_version: u32,
    pub rendered_body_sha256: String,
    pub rendered_body_bytes: u64,
    /// Digest of the decision record, deliberately distinct from body hash.
    pub decision_sha256: String,
}

#[derive(Serialize)]
struct DecisionDigestInput<'a> {
    policy_version: u32,
    producer_run_id: &'a str,
    producer_call_id: &'a str,
    source_sha256: &'a str,
    source_bytes: u64,
    target_sha256: &'a str,
    canonical_message_sha256: &'a str,
    freeze_key_sha256: &'a str,
    disposition: ToolResultProjectionDispositionV1,
    selected_ranges: &'a [ToolResultProjectionRangeV1],
    judgment_invocation_id: &'a Option<String>,
    fallback: Option<ToolResultProjectionFallbackV1>,
    renderer_version: u32,
    rendered_body_sha256: &'a str,
    rendered_body_bytes: u64,
}

impl ToolResultProjectionDecisionV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        producer_run_id: impl Into<String>,
        producer_call_id: impl Into<String>,
        source_sha256: impl Into<String>,
        source_bytes: u64,
        target_sha256: impl Into<String>,
        canonical_message_sha256: impl Into<String>,
        disposition: ToolResultProjectionDispositionV1,
        selected_ranges: Vec<ToolResultProjectionRangeV1>,
        judgment_invocation_id: Option<String>,
        fallback: Option<ToolResultProjectionFallbackV1>,
        rendered_body: &[u8],
    ) -> Result<Self, &'static str> {
        let mut decision = Self {
            policy_version: TOOL_RESULT_PROJECTION_POLICY_VERSION,
            producer_run_id: producer_run_id.into(),
            producer_call_id: producer_call_id.into(),
            source_sha256: source_sha256.into(),
            source_bytes,
            target_sha256: target_sha256.into(),
            canonical_message_sha256: canonical_message_sha256.into(),
            freeze_key_sha256: String::new(),
            disposition,
            selected_ranges,
            judgment_invocation_id,
            fallback,
            renderer_version: TOOL_RESULT_PROJECTION_RENDERER_VERSION,
            rendered_body_sha256: format!("{:x}", Sha256::digest(rendered_body)),
            rendered_body_bytes: u64::try_from(rendered_body.len()).unwrap_or(u64::MAX),
            decision_sha256: String::new(),
        };
        decision.freeze_key_sha256 = decision.compute_freeze_key();
        decision.validate_without_digest()?;
        decision.decision_sha256 = decision.compute_digest()?;
        Ok(decision)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_without_digest()?;
        if self.compute_freeze_key() != self.freeze_key_sha256
            || self.compute_digest()? != self.decision_sha256
        {
            return Err("tool-result projection digest does not match its decision");
        }
        Ok(())
    }

    fn validate_without_digest(&self) -> Result<(), &'static str> {
        if self.policy_version != TOOL_RESULT_PROJECTION_POLICY_VERSION
            || self.renderer_version != TOOL_RESULT_PROJECTION_RENDERER_VERSION
            || self.producer_run_id.trim().is_empty()
            || self.producer_run_id.len() > MAX_RUN_ID_BYTES
            || self.producer_call_id.trim().is_empty()
            || self.producer_call_id.len() > MAX_CALL_ID_BYTES
            || self.source_bytes == 0
            || self.rendered_body_bytes == 0
            || !is_sha256(&self.source_sha256)
            || !is_sha256(&self.target_sha256)
            || !is_sha256(&self.canonical_message_sha256)
            || !is_sha256(&self.freeze_key_sha256)
            || !is_sha256(&self.rendered_body_sha256)
            || (!self.decision_sha256.is_empty() && !is_sha256(&self.decision_sha256))
            || self
                .judgment_invocation_id
                .as_deref()
                .is_some_and(|v| v.trim().is_empty() || v.len() > MAX_ID_BYTES)
        {
            return Err("invalid tool-result projection identity");
        }
        match self.disposition {
            ToolResultProjectionDispositionV1::Baseline
                if !self.selected_ranges.is_empty() || self.fallback.is_none() =>
            {
                return Err("baseline projection requires one fallback and no selected ranges");
            }
            ToolResultProjectionDispositionV1::Selected
                if self.selected_ranges.is_empty() || self.fallback.is_some() =>
            {
                return Err("selected projection requires ranges and no fallback");
            }
            _ => {}
        }
        if self.selected_ranges.len() > MAX_RANGES {
            return Err("too many tool-result projection ranges");
        }
        let mut previous_end = 0;
        let mut ids = BTreeSet::new();
        for range in &self.selected_ranges {
            if range.chunk_id.trim().is_empty()
                || range.chunk_id.len() > MAX_ID_BYTES
                || !ids.insert(range.chunk_id.as_str())
                || range.start_byte < previous_end
                || range.end_byte <= range.start_byte
                || range.end_byte > self.source_bytes
            {
                return Err("invalid tool-result projection range");
            }
            previous_end = range.end_byte;
        }
        Ok(())
    }

    fn compute_freeze_key(&self) -> String {
        tool_result_projection_freeze_key(
            &self.producer_run_id,
            &self.producer_call_id,
            &self.source_sha256,
            &self.canonical_message_sha256,
            &self.target_sha256,
        )
    }

    fn compute_digest(&self) -> Result<String, &'static str> {
        let encoded = serde_json::to_vec(&DecisionDigestInput {
            policy_version: self.policy_version,
            producer_run_id: &self.producer_run_id,
            producer_call_id: &self.producer_call_id,
            source_sha256: &self.source_sha256,
            source_bytes: self.source_bytes,
            target_sha256: &self.target_sha256,
            canonical_message_sha256: &self.canonical_message_sha256,
            freeze_key_sha256: &self.freeze_key_sha256,
            disposition: self.disposition,
            selected_ranges: &self.selected_ranges,
            judgment_invocation_id: &self.judgment_invocation_id,
            fallback: self.fallback,
            renderer_version: self.renderer_version,
            rendered_body_sha256: &self.rendered_body_sha256,
            rendered_body_bytes: self.rendered_body_bytes,
        })
        .map_err(|_| "serialize tool-result projection decision")?;
        Ok(format!("{:x}", Sha256::digest(encoded)))
    }
}

/// Stable lookup key for a trusted canonical tool-result source before a
/// frozen decision has been loaded. Callers must obtain every component from
/// validated runtime metadata, never from provider-visible text.
#[must_use]
pub fn tool_result_projection_freeze_key(
    producer_run_id: &str,
    producer_call_id: &str,
    source_sha256: &str,
    canonical_message_sha256: &str,
    target_sha256: &str,
) -> String {
    let mut digest = Sha256::new();
    for value in [
        producer_run_id.as_bytes(),
        producer_call_id.as_bytes(),
        source_sha256.as_bytes(),
        canonical_message_sha256.as_bytes(),
        target_sha256.as_bytes(),
    ] {
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    format!("{:x}", digest.finalize())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultProjectionWireStateV1 {
    Included,
    PartiallyIncluded,
    Omitted,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultProjectionReceiptV1 {
    pub decision_sha256: String,
    pub provider_wire_sha256: String,
    pub state: ToolResultProjectionWireStateV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actual_ranges: Vec<ToolResultProjectionRangeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_body_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultProjectionBindingV1 {
    pub decision: ToolResultProjectionDecisionV1,
    pub receipt: ToolResultProjectionReceiptV1,
}

impl ToolResultProjectionBindingV1 {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.receipt.validate_against(&self.decision)
    }
}

impl ToolResultProjectionReceiptV1 {
    /// Structural validation only. Persistence must additionally bind this to
    /// the current owner generation and physical provider attempt.
    pub fn validate_against(
        &self,
        decision: &ToolResultProjectionDecisionV1,
    ) -> Result<(), &'static str> {
        decision.validate()?;
        if self.decision_sha256 != decision.decision_sha256
            || !is_sha256(&self.provider_wire_sha256)
        {
            return Err("tool-result projection receipt identity mismatch");
        }
        let actual_is_strict_subsequence = || {
            let mut selected = decision.selected_ranges.iter();
            self.actual_ranges
                .iter()
                .all(|actual| selected.by_ref().any(|expected| expected == actual))
        };
        match self.state {
            ToolResultProjectionWireStateV1::Included
                if self.actual_ranges != decision.selected_ranges
                    || self.actual_body_sha256.as_deref()
                        != Some(decision.rendered_body_sha256.as_str())
                    || self.reason.is_some() =>
            {
                Err("included projection receipt does not match the frozen body")
            }
            ToolResultProjectionWireStateV1::PartiallyIncluded
                if decision.disposition != ToolResultProjectionDispositionV1::Selected
                    || self.actual_ranges.is_empty()
                    || self.actual_ranges.len() >= decision.selected_ranges.len()
                    || !actual_is_strict_subsequence()
                    || self
                        .actual_body_sha256
                        .as_deref()
                        .is_none_or(|value| !is_sha256(value))
                    || self.reason.as_deref().is_none_or(str::is_empty) =>
            {
                Err("partial projection receipt is not a verified selected subset")
            }
            ToolResultProjectionWireStateV1::Omitted | ToolResultProjectionWireStateV1::Unknown
                if !self.actual_ranges.is_empty()
                    || self.actual_body_sha256.is_some()
                    || self.reason.as_deref().is_none_or(str::is_empty) =>
            {
                Err("non-included projection receipt requires a reason and no body")
            }
            _ => Ok(()),
        }
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn digest(byte: char) -> String {
        byte.to_string().repeat(64)
    }
    fn selected() -> ToolResultProjectionDecisionV1 {
        ToolResultProjectionDecisionV1::new(
            "run-1",
            "call-1",
            digest('a'),
            100,
            digest('b'),
            digest('c'),
            ToolResultProjectionDispositionV1::Selected,
            vec![ToolResultProjectionRangeV1 {
                chunk_id: "chunk-1".into(),
                start_byte: 10,
                end_byte: 20,
            }],
            Some("judgment-1".into()),
            None,
            b"selected body",
        )
        .unwrap()
    }

    #[test]
    fn decision_binds_body_and_is_tamper_evident() {
        let decision = selected();
        decision.validate().unwrap();
        assert_ne!(decision.decision_sha256, decision.rendered_body_sha256);
        let mut tampered = decision.clone();
        tampered.rendered_body_bytes += 1;
        assert!(tampered.validate().is_err());
    }

    #[test]
    fn duplicate_chunk_identity_is_rejected() {
        let first = selected().selected_ranges[0].clone();
        let mut duplicate = first.clone();
        duplicate.start_byte = 20;
        duplicate.end_byte = 30;
        assert!(
            ToolResultProjectionDecisionV1::new(
                "run-1",
                "call-1",
                digest('a'),
                100,
                digest('b'),
                digest('c'),
                ToolResultProjectionDispositionV1::Selected,
                vec![first, duplicate],
                Some("judgment-1".into()),
                None,
                b"selected body"
            )
            .is_err()
        );
    }

    #[test]
    fn baseline_and_partial_receipts_are_explicit() {
        let baseline = ToolResultProjectionDecisionV1::new(
            "run-1",
            "call-1",
            digest('a'),
            100,
            digest('b'),
            digest('c'),
            ToolResultProjectionDispositionV1::Baseline,
            Vec::new(),
            None,
            Some(ToolResultProjectionFallbackV1::JudgmentUnavailable),
            b"baseline body",
        )
        .unwrap();
        ToolResultProjectionReceiptV1 {
            decision_sha256: baseline.decision_sha256.clone(),
            provider_wire_sha256: digest('d'),
            state: ToolResultProjectionWireStateV1::Included,
            actual_ranges: Vec::new(),
            actual_body_sha256: Some(baseline.rendered_body_sha256.clone()),
            reason: None,
        }
        .validate_against(&baseline)
        .unwrap();

        let mut two = selected();
        two.selected_ranges.push(ToolResultProjectionRangeV1 {
            chunk_id: "chunk-2".into(),
            start_byte: 20,
            end_byte: 30,
        });
        two.selected_ranges.push(ToolResultProjectionRangeV1 {
            chunk_id: "chunk-3".into(),
            start_byte: 30,
            end_byte: 40,
        });
        two.decision_sha256 = two.compute_digest().unwrap();
        ToolResultProjectionReceiptV1 {
            decision_sha256: two.decision_sha256.clone(),
            provider_wire_sha256: digest('d'),
            state: ToolResultProjectionWireStateV1::PartiallyIncluded,
            actual_ranges: vec![
                two.selected_ranges[0].clone(),
                two.selected_ranges[2].clone(),
            ],
            actual_body_sha256: Some(digest('e')),
            reason: Some("provider transform omitted one segment".into()),
        }
        .validate_against(&two)
        .unwrap();

        for invalid_ranges in [
            vec![
                two.selected_ranges[0].clone(),
                two.selected_ranges[0].clone(),
            ],
            vec![
                two.selected_ranges[1].clone(),
                two.selected_ranges[0].clone(),
            ],
            vec![ToolResultProjectionRangeV1 {
                chunk_id: "unknown".into(),
                start_byte: 30,
                end_byte: 40,
            }],
        ] {
            assert!(
                ToolResultProjectionReceiptV1 {
                    decision_sha256: two.decision_sha256.clone(),
                    provider_wire_sha256: digest('d'),
                    state: ToolResultProjectionWireStateV1::PartiallyIncluded,
                    actual_ranges: invalid_ranges,
                    actual_body_sha256: Some(digest('e')),
                    reason: Some("provider transform omitted a segment".into()),
                }
                .validate_against(&two)
                .is_err()
            );
        }
    }

    #[test]
    fn freeze_key_changes_with_semantic_target() {
        let first = tool_result_projection_freeze_key(
            "run-1",
            "call-1",
            &digest('a'),
            &digest('b'),
            &digest('c'),
        );
        let second = tool_result_projection_freeze_key(
            "run-1",
            "call-1",
            &digest('a'),
            &digest('b'),
            &digest('d'),
        );
        assert_ne!(first, second);
    }
}
