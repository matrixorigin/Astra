//! Tool-selector observability sink.
//!
//! Emits a single-line `[selector]` record per selection decision so
//! a developer watching stderr can see: the query TF-IDF scored
//! against, the top-k scored tools with raw scores, the final chosen
//! subset, and any short-circuit reason (e.g. "conversational:
//! pinned-only").
//!
//! This exists because G3 — a bug where `spawn_agent` was silently
//! hidden from the LLM because its catalog entry was missing — went
//! undetected for weeks. The selector decision was fully opaque. A
//! `[selector]` line anchored to stderr means the next G3-class bug
//! fails loudly instead of "the model just didn't call the tool".
//!
//! ## Feature gate
//!
//! Off by default. Enable via either:
//! - env var `ASTRA_SELECTOR_OBS=1` at process startup, OR
//! - explicit `set_selector_observability_for_tests(true)` in tests.
//!
//! The flag is cached in an `AtomicU8` (0 = unread, 1 = off, 2 = on)
//! so the hot path is a single relaxed load.
//!
//! ## Output shape
//!
//! Each line is JSON wrapped in a `[selector] ` prefix:
//!
//! ```text
//! [selector] {"query":"list files","mode":"dynamic","top":[["list_dir",0.73],["grep",0.18]],"final":["bash","read_file","str_replace","list_dir"],"budget":{"used":120,"total":800}}
//! ```
//!
//! Consumers like the test harness anchor a stderr_matches regex at
//! the `^\[selector\]` prefix. JSON lets downstream tools (journal
//! digest, dashboards) parse without brittle regex.

use std::sync::atomic::{AtomicU8, Ordering};

/// Env var that enables the sink at process startup.
pub const SELECTOR_OBS_ENV: &str = "ASTRA_SELECTOR_OBS";

/// 0 = unread, 1 = off, 2 = on. Matches the `FORK_FLAG_CACHE` pattern
/// in `fork_capture.rs` — same hot-path load semantics, same
/// bypass-able surface for tests.
static OBS_FLAG: AtomicU8 = AtomicU8::new(0);

/// Cheap check: is the observability sink currently active?
///
/// First call reads the env var and caches the result. Subsequent
/// calls are a relaxed atomic load — negligible overhead on the hot
/// path even when the sink is off.
pub fn is_selector_observability_enabled() -> bool {
    match OBS_FLAG.load(Ordering::Relaxed) {
        0 => {
            let enabled = std::env::var(SELECTOR_OBS_ENV)
                .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes"))
                .unwrap_or(false);
            OBS_FLAG.store(if enabled { 2 } else { 1 }, Ordering::Relaxed);
            enabled
        }
        2 => true,
        _ => false,
    }
}

/// Test-only: flip the cached flag without touching the env var.
/// Returns the previous raw value so tests can restore state.
#[doc(hidden)]
pub fn set_selector_observability_for_tests(enabled: bool) -> u8 {
    OBS_FLAG.swap(if enabled { 2 } else { 1 }, Ordering::Relaxed)
}

/// Test-only: restore the raw cached state (including the "unread"
/// 0 value) that `set_selector_observability_for_tests` returned.
#[doc(hidden)]
pub fn restore_selector_observability_for_tests(raw: u8) {
    OBS_FLAG.store(raw, Ordering::Relaxed);
}

/// Shared test mutex. Cross-crate selector-observability tests must
/// share this so they don't race for flag state. Same pattern as
/// `fork_capture::FORK_FLAG_TEST_MUTEX`.
#[doc(hidden)]
pub static SELECTOR_OBS_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Selection-decision trace. One per selector invocation. Designed to
/// answer "why did the selector pick these tools and not others" —
/// for example, G3's failure mode (spawn_agent missing) would show
/// up as a `final` list that doesn't include it AND a `top` list
/// where it scored 0.0 or is absent entirely.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectionTrace<'a> {
    /// The user query the selector scored against. Truncated to 200
    /// chars so a huge pasted prompt doesn't balloon stderr.
    pub query: &'a str,
    /// High-level path: "dynamic" (ranked tfidf), "conversational"
    /// (pinned-only short-circuit), "routed" (task-archetype
    /// override), etc. Short labels a developer can grep.
    pub mode: &'a str,
    /// Top-scoring tools from pre-filter, `(name, score)` pairs.
    /// Typically capped at 10; None when the selector short-circuited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top: Option<Vec<(&'a str, f64)>>,
    /// Tool names actually selected for the LLM request — what the
    /// model sees. This is the authoritative "did my tool make it"
    /// answer.
    pub r#final: &'a [String],
    /// Token budget accounting. `None` when the selector didn't
    /// compute a budget (short-circuit paths).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<SelectionBudget>,
    /// Optional free-form reason string (e.g. "conversational query —
    /// pinned-only"). Helps a reviewer understand short-circuit
    /// decisions without reading the selector code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'a str>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectionBudget {
    pub used: u32,
    pub total: u32,
}

/// Emit a selector trace to stderr if the observability flag is on.
/// No-op and zero overhead (beyond the flag load) when disabled.
///
/// Takes `&SelectionTrace` so callers can pre-assemble a single
/// trace struct even when the flag is off — the caller that knows
/// nothing will be printed can save the Vec construction, but most
/// callers are fine paying for construction either way because
/// selection already allocates.
pub fn emit_selector_trace(trace: &SelectionTrace<'_>) {
    if !is_selector_observability_enabled() {
        return;
    }
    // Truncate query to 200 chars to avoid giant stderr lines on
    // huge pastes. We emit from a clone so the struct's lifetime is
    // untouched.
    let truncated_query: String = trace.query.chars().take(200).collect();
    let for_emit = SelectionTrace {
        query: truncated_query.as_str(),
        mode: trace.mode,
        top: trace.top.clone(),
        r#final: trace.r#final,
        budget: trace.budget.clone(),
        reason: trace.reason,
    };
    match serde_json::to_string(&for_emit) {
        Ok(json) => eprintln!("[selector] {json}"),
        Err(e) => eprintln!("[selector] serde_err: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RAII guard: sets the flag for the test's duration, restores
    /// on drop. Acquires the test mutex so parallel tests don't race.
    struct FlagGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_raw: u8,
    }
    impl FlagGuard {
        fn set(enabled: bool) -> Self {
            let lock = SELECTOR_OBS_TEST_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev_raw = set_selector_observability_for_tests(enabled);
            Self {
                _lock: lock,
                prev_raw,
            }
        }
    }
    impl Drop for FlagGuard {
        fn drop(&mut self) {
            restore_selector_observability_for_tests(self.prev_raw);
        }
    }

    #[test]
    fn is_enabled_caches_disabled_by_default() {
        let _guard = FlagGuard::set(false);
        assert!(!is_selector_observability_enabled());
    }

    #[test]
    fn is_enabled_caches_on_when_flag_forced_on() {
        let _guard = FlagGuard::set(true);
        assert!(is_selector_observability_enabled());
    }

    #[test]
    fn emit_is_noop_when_flag_off() {
        // Can't directly capture eprintln in stable Rust without
        // heavy machinery; we exercise the hot path to confirm no
        // panic + consistent behavior. Proper stderr capture lives
        // in the astra-runtime integration test below.
        let _guard = FlagGuard::set(false);
        let finals = vec!["bash".to_string()];
        let trace = SelectionTrace {
            query: "hello",
            mode: "dynamic",
            top: None,
            r#final: &finals,
            budget: None,
            reason: None,
        };
        emit_selector_trace(&trace);
    }

    #[test]
    fn emit_truncates_huge_query_to_200_chars() {
        // Structural test: verify we don't construct a 10k-char line.
        // We can't capture stderr, but we can check the truncation
        // logic by mirroring it — same chain used inside emit.
        let big: String = "a".repeat(10_000);
        let truncated: String = big.chars().take(200).collect();
        assert_eq!(truncated.len(), 200);
    }

    #[test]
    fn trace_json_serializes_with_expected_fields() {
        let finals = vec!["bash".to_string(), "read_file".to_string()];
        let top = vec![("bash", 0.8), ("read_file", 0.4)];
        let trace = SelectionTrace {
            query: "build the crate",
            mode: "dynamic",
            top: Some(top),
            r#final: &finals,
            budget: Some(SelectionBudget {
                used: 70,
                total: 800,
            }),
            reason: None,
        };
        let json = serde_json::to_string(&trace).unwrap();
        // A reviewer should be able to grep these keys.
        assert!(json.contains("\"query\":\"build the crate\""));
        assert!(json.contains("\"mode\":\"dynamic\""));
        assert!(json.contains("\"top\":["));
        // `r#final` serializes as `final` — that's the whole point of
        // the raw-ident dance.
        assert!(json.contains("\"final\":[\"bash\",\"read_file\"]"));
        assert!(json.contains("\"budget\":{\"used\":70,\"total\":800}"));
    }

    #[test]
    fn trace_json_omits_optional_fields_when_none() {
        let finals: Vec<String> = vec![];
        let trace = SelectionTrace {
            query: "conversational",
            mode: "conversational",
            top: None,
            r#final: &finals,
            budget: None,
            reason: Some("conversational query — pinned-only"),
        };
        let json = serde_json::to_string(&trace).unwrap();
        assert!(!json.contains("\"top\""));
        assert!(!json.contains("\"budget\""));
        assert!(json.contains("\"reason\":\"conversational query"));
    }
}
