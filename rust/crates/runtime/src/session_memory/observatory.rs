//! Post-hoc observability for the session-memory subsystem.
//!
//! Two bounded rings answer the recurring operator question "what did
//! astra extract / inject this session, and when?". They do **not**
//! touch the LLM message stream: every write happens off the hot path
//! after the relevant I/O already settled, and reads only surface
//! through the [`introspect`] tool result (never back into system
//! prompts or rolling cache).
//!
//! Ring sizes are deliberately small. Extraction at most once per turn,
//! injection at most once per compaction — 32/16 is ~1 hour of a live
//! session at typical cadence, enough for postmortem without bloating
//! memory.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};

/// Newest-first cap for extraction records. 32 ≈ an hour of a busy
/// session; older entries drop silently.
pub const EXTRACTION_RING_CAPACITY: usize = 32;

/// Newest-first cap for injection records. Compaction is sparser than
/// extraction, so 16 is enough.
pub const INJECTION_RING_CAPACITY: usize = 16;

/// Hard cap for any short human-readable preview stored in the
/// observatory. Chosen so a ring at capacity stays well under
/// `RING_CAPACITY * 256` bytes — tight enough that introspect can dump
/// the whole ring without bloating context.
pub const PREVIEW_CHAR_CAP: usize = 200;

// ── Extraction side ────────────────────────────────────────────────────

/// What triggered a background extraction attempt. Matches the gate's
/// decision vocabulary so operators can correlate observed behaviour
/// with gate thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionTrigger {
    /// First extraction after crossing the init token gate.
    InitGate,
    /// Subsequent extraction after crossing a growth delta.
    GrowthGate,
    /// Gate bypass — errors (or §4.4 staleness) forced an extra
    /// extraction past the normal debounce.
    ErrorOverride,
}

/// Terminal outcome of one extraction. Mirrors the
/// [`crate::session_memory::runner::ExtractionArtifacts`] shape but is
/// loggable — no runtime objects, no dyn client references.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExtractionOutcome {
    /// LLM or rule-based produced content and it landed in Memoria.
    Persisted {
        source: ExtractionSource,
        bytes_written: u64,
        store_attempt: u32,
    },
    /// LLM attempted and failed; rule-based fallback persisted.
    LlmFailedFallbackPersisted {
        reason: ErrorReason,
        bytes_written: u64,
        store_attempt: u32,
    },
    /// Persist step itself failed. Nothing landed.
    PersistFailed {
        reason: ErrorReason,
        llm_reason: Option<ErrorReason>,
    },
    /// Gate/request decided not to spawn. Recorded so "nothing happened"
    /// is still visible in introspect, rather than silently absent.
    Skipped { reason: String },
}

/// Stable re-export of the journal's source enum. Keeping a
/// parallel type insulates the observatory from churn in
/// `astra-services` and keeps serialisation under our control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionSource {
    Llm,
    RuleFallback,
}

impl From<SessionMemoryExtractionSource> for ExtractionSource {
    fn from(src: SessionMemoryExtractionSource) -> Self {
        match src {
            SessionMemoryExtractionSource::Llm => ExtractionSource::Llm,
            SessionMemoryExtractionSource::RuleFallback => ExtractionSource::RuleFallback,
        }
    }
}

/// Stable re-export of the journal's error-reason enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorReason {
    LlmTimeout,
    LlmError,
    EmptyResponse,
    PurgeFailed,
    WriteFailed,
}

impl From<SessionMemoryExtractionErrorReason> for ErrorReason {
    fn from(e: SessionMemoryExtractionErrorReason) -> Self {
        match e {
            SessionMemoryExtractionErrorReason::LlmTimeout => ErrorReason::LlmTimeout,
            SessionMemoryExtractionErrorReason::LlmError => ErrorReason::LlmError,
            SessionMemoryExtractionErrorReason::EmptyResponse => ErrorReason::EmptyResponse,
            SessionMemoryExtractionErrorReason::PurgeFailed => ErrorReason::PurgeFailed,
            SessionMemoryExtractionErrorReason::WriteFailed => ErrorReason::WriteFailed,
        }
    }
}

/// One extraction attempt, post-hoc. Produced by the service after the
/// Memoria write (success or failure) has resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractionRecord {
    pub session_id: String,
    pub turn: u32,
    pub at: SystemTime,
    pub trigger: ExtractionTrigger,
    /// `None` when rule-based ran without an LLM call (gate configured
    /// with no selector) or when Skipped.
    pub selector_model: Option<String>,
    pub outcome: ExtractionOutcome,
    /// Names of narrative sections that survived parsing in the
    /// persisted L1. Empty for rule-based and non-persisted outcomes.
    pub narrative_sections: Vec<String>,
    /// First [`PREVIEW_CHAR_CAP`] **chars** of the persisted content.
    /// Char-bounded, not byte-bounded — Unicode safe. Empty when
    /// nothing was persisted.
    pub content_preview: String,
    pub latency: Duration,
}

// ── Injection side ─────────────────────────────────────────────────────

/// What the pressure-adaptive decision picked for this compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectionLevel {
    L1Full,
    L1Minimal,
    L0Only,
}

/// Post-hoc snapshot of the SessionFacts state that fed the injection.
/// Not the full facts — just the few numbers that matter for operator
/// forensics. Keeps the record tiny.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FactsSummary {
    pub turn: u32,
    pub estimated_tokens: u64,
    pub plan_completed: u32,
    pub plan_total: u32,
    pub active_files_count: u32,
    pub error_count: u32,
    /// First [`PREVIEW_CHAR_CAP`] chars of the last error message, if
    /// any. Redaction (token/password/api_key) already applied upstream
    /// by the facts layer — do not add another one here.
    pub last_error_preview: Option<String>,
}

/// The §4.4 cross-validation signals we surfaced for this injection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalenessSignals {
    pub task_contradicted: bool,
    pub missing_corrections: bool,
}

/// One Memoria memory that was part of the retrieval feeding this
/// injection. Just the metadata — never the full content, to keep the
/// ring from ballooning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievedMemoryRef {
    pub memory_id: String,
    pub memory_type: String,
    pub score: Option<f64>,
    /// First ~200 chars of the memory content for diagnostics.
    #[serde(default)]
    pub content_preview: Option<String>,
}

/// One compaction-time injection event, post-hoc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InjectionRecord {
    pub session_id: String,
    pub turn: u32,
    pub at: SystemTime,
    pub pressure: f64,
    pub level: InjectionLevel,
    /// `injected_chars == 0` for `L0Only` (empty by protocol).
    pub injected_chars: u32,
    pub facts_summary: FactsSummary,
    pub staleness: StalenessSignals,
    pub retrieved_memories: Vec<RetrievedMemoryRef>,
    /// Section titles that survived cross-validation and made it into
    /// the injected block. Empty for `L0Only` / `L1Minimal` (facts
    /// only).
    pub narrative_sections_kept: Vec<String>,
}

// ── The observatory ────────────────────────────────────────────────────

/// Arc-shared across the extraction service and the compaction call
/// site. `Mutex` chosen over `RwLock` because writes are the common
/// case (read happens only when the user types `introspect`).
#[derive(Debug)]
pub struct SessionMemoryObservatory {
    extractions: Mutex<VecDeque<ExtractionRecord>>,
    injections: Mutex<VecDeque<InjectionRecord>>,
    extraction_capacity: usize,
    injection_capacity: usize,
}

impl Default for SessionMemoryObservatory {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionMemoryObservatory {
    pub fn new() -> Self {
        Self::with_capacity(EXTRACTION_RING_CAPACITY, INJECTION_RING_CAPACITY)
    }

    /// Test-friendly constructor. Production always uses [`new`]; tests
    /// shrink the rings to exercise eviction deterministically.
    pub fn with_capacity(extraction: usize, injection: usize) -> Self {
        Self {
            extractions: Mutex::new(VecDeque::with_capacity(extraction.max(1))),
            injections: Mutex::new(VecDeque::with_capacity(injection.max(1))),
            extraction_capacity: extraction.max(1),
            injection_capacity: injection.max(1),
        }
    }

    pub fn record_extraction(&self, rec: ExtractionRecord) {
        if let Ok(mut ring) = self.extractions.lock() {
            while ring.len() >= self.extraction_capacity {
                ring.pop_front();
            }
            ring.push_back(rec);
        }
    }

    pub fn record_injection(&self, rec: InjectionRecord) {
        if let Ok(mut ring) = self.injections.lock() {
            while ring.len() >= self.injection_capacity {
                ring.pop_front();
            }
            ring.push_back(rec);
        }
    }

    /// Clone the extraction ring newest-last. Allocates — called only
    /// from introspect, never from the hot path.
    pub fn extractions_snapshot(&self) -> Vec<ExtractionRecord> {
        self.extractions
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Clone the injection ring newest-last.
    pub fn injections_snapshot(&self) -> Vec<InjectionRecord> {
        self.injections
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn extraction_count(&self) -> usize {
        self.extractions.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn injection_count(&self) -> usize {
        self.injections.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Truncate `s` to at most `PREVIEW_CHAR_CAP` chars (not bytes) —
/// Unicode-safe. Appends a single ellipsis `…` only when truncation
/// happened, so consumers can distinguish "exactly cap chars" from
/// "was longer".
pub fn clip_preview(s: &str) -> String {
    let mut out = String::with_capacity(PREVIEW_CHAR_CAP.saturating_add(4));
    for (count, ch) in s.chars().enumerate() {
        if count == PREVIEW_CHAR_CAP {
            out.push('…');
            return out;
        }
        out.push(ch);
    }
    out
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn extraction(session: &str, turn: u32) -> ExtractionRecord {
        ExtractionRecord {
            session_id: session.into(),
            turn,
            at: SystemTime::UNIX_EPOCH,
            trigger: ExtractionTrigger::GrowthGate,
            selector_model: Some("sonnet-mini".into()),
            outcome: ExtractionOutcome::Persisted {
                source: ExtractionSource::Llm,
                bytes_written: 512,
                store_attempt: 1,
            },
            narrative_sections: vec!["Task Specification".into()],
            content_preview: "ok".into(),
            latency: Duration::from_millis(40),
        }
    }

    fn injection(session: &str, turn: u32) -> InjectionRecord {
        InjectionRecord {
            session_id: session.into(),
            turn,
            at: SystemTime::UNIX_EPOCH,
            pressure: 0.6,
            level: InjectionLevel::L1Full,
            injected_chars: 512,
            facts_summary: FactsSummary::default(),
            staleness: StalenessSignals::default(),
            retrieved_memories: Vec::new(),
            narrative_sections_kept: vec!["Task Specification".into()],
        }
    }

    // ── Unhappy: capacity overflow evicts oldest, not most recent. ────

    #[test]
    fn extraction_ring_evicts_oldest_when_full() {
        let obs = SessionMemoryObservatory::with_capacity(3, 3);
        for turn in 0..5u32 {
            obs.record_extraction(extraction("sess", turn));
        }
        let snap = obs.extractions_snapshot();
        assert_eq!(snap.len(), 3, "capacity must cap ring length");
        assert_eq!(
            snap.first().map(|r| r.turn),
            Some(2),
            "oldest (0, 1) must be evicted; survivor starts at turn 2"
        );
        assert_eq!(
            snap.last().map(|r| r.turn),
            Some(4),
            "newest must always survive"
        );
    }

    #[test]
    fn injection_ring_evicts_oldest_when_full() {
        let obs = SessionMemoryObservatory::with_capacity(3, 2);
        for turn in 0..5u32 {
            obs.record_injection(injection("sess", turn));
        }
        let snap = obs.injections_snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap.first().map(|r| r.turn), Some(3));
        assert_eq!(snap.last().map(|r| r.turn), Some(4));
    }

    #[test]
    fn capacity_zero_is_coerced_to_one() {
        let obs = SessionMemoryObservatory::with_capacity(0, 0);
        obs.record_extraction(extraction("sess", 0));
        obs.record_extraction(extraction("sess", 1));
        assert_eq!(obs.extraction_count(), 1);
        assert_eq!(
            obs.extractions_snapshot().first().map(|r| r.turn),
            Some(1),
            "cap=1 keeps only the latest record"
        );
    }

    // ── Unhappy: empty ring reads are safe. ───────────────────────────

    #[test]
    fn empty_ring_snapshots_return_empty_vec_not_panic() {
        let obs = SessionMemoryObservatory::new();
        assert!(obs.extractions_snapshot().is_empty());
        assert!(obs.injections_snapshot().is_empty());
        assert_eq!(obs.extraction_count(), 0);
        assert_eq!(obs.injection_count(), 0);
    }

    // ── Unhappy: concurrent writes across sessions don't race. ────────

    #[test]
    fn concurrent_writes_across_sessions_all_land() {
        let obs = Arc::new(SessionMemoryObservatory::new());
        let handles: Vec<_> = (0..8)
            .map(|tid| {
                let obs = obs.clone();
                thread::spawn(move || {
                    for turn in 0..4u32 {
                        obs.record_extraction(extraction(&format!("s{tid}"), turn));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("writer thread must not panic");
        }
        let count = obs.extraction_count();
        // 8 threads × 4 records = 32 which matches EXTRACTION_RING_CAPACITY;
        // nothing should be evicted.
        assert_eq!(count, 32);
    }

    // ── Unhappy: preview truncation is Unicode-safe. ──────────────────

    #[test]
    fn clip_preview_never_splits_unicode_scalar() {
        let four_byte_char = "🦀"; // 4 bytes, 1 char
        let s: String = four_byte_char.repeat(PREVIEW_CHAR_CAP + 5);
        let clipped = clip_preview(&s);
        // One … appended when truncation happens.
        let char_count = clipped.chars().count();
        assert_eq!(
            char_count,
            PREVIEW_CHAR_CAP + 1,
            "clipped preview must carry exactly cap+1 chars when truncated (cap chars + ellipsis)"
        );
        assert!(
            clipped.ends_with('…'),
            "truncated preview must end with an ellipsis marker"
        );
        // And the whole thing must still be valid UTF-8 (implicit via
        // String, explicit via the round-trip).
        assert!(
            clipped.chars().all(|c| c == '🦀' || c == '…'),
            "no partial codepoints"
        );
    }

    #[test]
    fn clip_preview_does_not_touch_short_strings() {
        let s = "short";
        let clipped = clip_preview(s);
        assert_eq!(clipped, "short");
        assert!(!clipped.contains('…'));
    }

    #[test]
    fn clip_preview_exactly_at_cap_does_not_add_ellipsis() {
        let s: String = "a".repeat(PREVIEW_CHAR_CAP);
        let clipped = clip_preview(&s);
        assert_eq!(clipped.chars().count(), PREVIEW_CHAR_CAP);
        assert!(
            !clipped.contains('…'),
            "exactly-at-cap must not be flagged as truncated"
        );
    }

    // ── Unhappy: serialisation covers every outcome variant. ──────────

    #[test]
    fn outcome_variants_roundtrip_through_serde() {
        for outcome in [
            ExtractionOutcome::Persisted {
                source: ExtractionSource::Llm,
                bytes_written: 1,
                store_attempt: 2,
            },
            ExtractionOutcome::LlmFailedFallbackPersisted {
                reason: ErrorReason::LlmTimeout,
                bytes_written: 1,
                store_attempt: 1,
            },
            ExtractionOutcome::PersistFailed {
                reason: ErrorReason::PurgeFailed,
                llm_reason: Some(ErrorReason::LlmError),
            },
            ExtractionOutcome::Skipped {
                reason: "breaker_open".into(),
            },
        ] {
            let json = serde_json::to_string(&outcome).expect("ser");
            let back: ExtractionOutcome = serde_json::from_str(&json).expect("de");
            assert_eq!(back, outcome);
        }
    }

    // ── Unhappy: journal-enum conversions cover every variant. ────────

    #[test]
    fn error_reason_covers_every_journal_variant() {
        // Exhaustive match so adding a new variant upstream forces us
        // to extend this map — and thus the record schema.
        for reason in [
            SessionMemoryExtractionErrorReason::LlmTimeout,
            SessionMemoryExtractionErrorReason::LlmError,
            SessionMemoryExtractionErrorReason::EmptyResponse,
            SessionMemoryExtractionErrorReason::PurgeFailed,
            SessionMemoryExtractionErrorReason::WriteFailed,
        ] {
            let _: ErrorReason = reason.into();
        }
    }

    #[test]
    fn source_covers_every_journal_variant() {
        for s in [
            SessionMemoryExtractionSource::Llm,
            SessionMemoryExtractionSource::RuleFallback,
        ] {
            let _: ExtractionSource = s.into();
        }
    }
}
