//! Structured persistence payload for every committed history cell.
//!
//! A `TurnEvent` is what gets written to
//! `~/.astra/transcripts/<session_id>.jsonl`, one JSON object per
//! line. Exactly one `TurnEvent` variant per cell kind. The schema
//! is the *contract* between a cell's in-memory representation and
//! its on-disk form — if a cell grows a new field, add it here and
//! the renderer will automatically pick it up on resume.
//!
//! ## Schema stability
//!
//! All variants carry an explicit `"kind"` tag (via `serde(tag)`),
//! not structural discriminants, so adding new variants in the
//! future doesn't break old sessions. Unknown variants on load are
//! dropped with a warning rather than panicking — see
//! [`transcript_jsonl::load`].
//!
//! ## Why not reuse the session journal
//!
//! `~/.astra/sessions/<id>.jsonl` stores *agent* events
//! (tool_call_start, reasoning_delta, usage, etc.) — low-level
//! protocol noise. The transcript file stores *rendered* events —
//! one row per user-visible cell. Conceptually distinct, so they
//! get sibling files.

use serde::{Deserialize, Serialize};

/// One committed entry in a session transcript.
///
/// Each variant must:
/// - carry enough text to re-render the cell **byte-identically**
///   on resume (we don't want "missing details" reruns),
/// - be forward-compatible (new optional fields with `#[serde(default)]`),
/// - never include style/colour state — rendering is deterministic
///   from the raw fields plus the current theme.
// `Eq` is intentionally NOT derived: `TurnEvent::TurnSummary` carries
// an `Option<f64>` cumulative cost. Tests use `PartialEq` which is
// the right contract for struct equality comparisons.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TurnEvent {
    /// User-typed message. The text is exactly what the composer
    /// submitted (pastes already expanded, no placeholders).
    User {
        /// RFC3339 timestamp of when the user hit Enter.
        #[serde(default)]
        ts: Option<String>,
        text: String,
    },

    /// Thinking / reasoning content. Kept separate from
    /// `Assistant` because it renders dimmed and is optional at
    /// render time (user can toggle visibility).
    Thinking {
        #[serde(default)]
        ts: Option<String>,
        /// Full reasoning payload concatenated from all chunks.
        text: String,
        /// Wall-clock thinking duration captured at `finish_thinking`.
        /// `None` when the model didn't bracket its thinking (just
        /// emitted a single final chunk).
        #[serde(default)]
        duration_ms: Option<u64>,
    },

    /// Assistant's final reply body (the markdown the user will
    /// see rendered). Stored as raw markdown, not as pre-rendered
    /// lines — that way theme changes or width changes between
    /// runs render correctly.
    Assistant {
        #[serde(default)]
        ts: Option<String>,
        /// Raw markdown the model emitted. No ANSI, no line splits.
        markdown: String,
    },

    /// Tool invocation. Captures enough for the cell to re-render:
    /// name, description line, final status, duration, and output
    /// summary.
    Tool {
        #[serde(default)]
        ts: Option<String>,
        name: String,
        /// The `│ <description>` line under the header (e.g. the
        /// command for `bash`).
        #[serde(default)]
        description: String,
        /// Final status. During a live turn the cell may be in
        /// `running` state, but that never gets persisted — we only
        /// write on commit.
        status: ToolStatus,
        duration_ms: u64,
        /// Output preview shown inline in scrollback. Full output
        /// (if any) lives in `output`.
        #[serde(default)]
        output_summary: Option<String>,
        #[serde(default)]
        output: Option<String>,
    },

    /// System info / warning / error. Flags like "session
    /// resumed", permission changes, or non-fatal errors.
    System {
        #[serde(default)]
        ts: Option<String>,
        level: SystemLevel,
        text: String,
    },

    /// End-of-turn metric band (the `⏱ … · ⚡ … · Σ …` line).
    TurnSummary {
        #[serde(default)]
        ts: Option<String>,
        #[serde(default)]
        elapsed_ms: Option<u64>,
        #[serde(default)]
        ttft_ms: Option<u64>,
        #[serde(default)]
        tokens_in: Option<u64>,
        #[serde(default)]
        tokens_out: Option<u64>,
        /// Of `tokens_in`, how many were served from the provider's
        /// prompt cache. Drives the `💾 N%` segment. Absent on older
        /// transcripts and on providers that don't surface cache
        /// stats — `#[serde(default)]` keeps the schema additive.
        #[serde(default)]
        cache_read_tokens: Option<u64>,
        #[serde(default)]
        tools: u32,
        /// Session-cumulative totals at the moment this turn ended.
        /// Allows accurate `Σ` rendering after resume without
        /// recomputing from scratch.
        #[serde(default)]
        cumulative_tokens: Option<u64>,
        #[serde(default)]
        cumulative_cost_usd: Option<f64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ToolStatus {
    Success,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SystemLevel {
    /// Free-floating TUI notice (session resumed, etc.). Dim.
    Info,
    /// Result of a slash command, styled to match Claude Code's
    /// `  ⎿  Set model to …` callback line. Dim + corner glyph so
    /// the eye can pair it with the trailing `› /cmd` prompt above.
    #[serde(alias = "response")]
    Response,
    /// Non-fatal advisory — token budget near limit, etc. Yellow.
    Warning,
    /// Fatal-ish: turn failed, tool denied, etc. Red.
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_roundtrip(ev: &TurnEvent) {
        let j = serde_json::to_string(ev).expect("serialize");
        let back: TurnEvent = serde_json::from_str(&j).expect("deserialize");
        assert_eq!(ev, &back, "round-trip mismatch for {ev:?}\njson: {j}");
    }

    #[test]
    fn user_event_roundtrip() {
        assert_roundtrip(&TurnEvent::User {
            ts: Some("2026-05-09T12:00:00Z".into()),
            text: "make a plan".into(),
        });
    }

    #[test]
    fn user_event_without_ts_still_roundtrips() {
        // `ts` is optional — older logs may lack it. #[serde(default)]
        // means None fields are omitted in JSON and None on reload.
        assert_roundtrip(&TurnEvent::User {
            ts: None,
            text: "hi".into(),
        });
    }

    #[test]
    fn assistant_event_roundtrip_preserves_markdown() {
        // Backticks, newlines, unicode — all survive the JSON layer.
        let ev = TurnEvent::Assistant {
            ts: None,
            markdown: "# Plan\n\n- step `a`\n- step 你好\n".into(),
        };
        assert_roundtrip(&ev);
    }

    #[test]
    fn thinking_event_roundtrip_with_and_without_duration() {
        assert_roundtrip(&TurnEvent::Thinking {
            ts: None,
            text: "some reasoning".into(),
            duration_ms: Some(3120),
        });
        assert_roundtrip(&TurnEvent::Thinking {
            ts: None,
            text: "reasoning without bracket".into(),
            duration_ms: None,
        });
    }

    #[test]
    fn tool_event_roundtrip_success_and_failure() {
        assert_roundtrip(&TurnEvent::Tool {
            ts: None,
            name: "bash".into(),
            description: "ls /tmp".into(),
            status: ToolStatus::Success,
            duration_ms: 42,
            output_summary: Some("3 entries".into()),
            output: None,
        });
        assert_roundtrip(&TurnEvent::Tool {
            ts: None,
            name: "read".into(),
            description: String::new(),
            status: ToolStatus::Failed,
            duration_ms: 120,
            output_summary: None,
            output: Some("ENOENT".into()),
        });
    }

    #[test]
    fn system_event_levels_roundtrip() {
        for lv in [
            SystemLevel::Info,
            SystemLevel::Response,
            SystemLevel::Warning,
            SystemLevel::Error,
        ] {
            assert_roundtrip(&TurnEvent::System {
                ts: None,
                level: lv,
                text: "msg".into(),
            });
        }
    }

    #[test]
    fn turn_summary_roundtrip_with_missing_optional_fields() {
        // Intentionally sparse — most legacy sessions won't have
        // ttft or cumulative_cost. The deserialiser must treat
        // missing fields as `None`.
        let j = r#"{"kind":"turn_summary","elapsed_ms":1500,"tokens_in":500,"tokens_out":200}"#;
        let ev: TurnEvent = serde_json::from_str(j).expect("deserialize sparse");
        match ev {
            TurnEvent::TurnSummary {
                elapsed_ms,
                ttft_ms,
                tokens_in,
                tokens_out,
                tools,
                ..
            } => {
                assert_eq!(elapsed_ms, Some(1500));
                assert_eq!(ttft_ms, None);
                assert_eq!(tokens_in, Some(500));
                assert_eq!(tokens_out, Some(200));
                assert_eq!(tools, 0, "default for u32 is 0");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn kind_tag_is_exposed_as_snake_case() {
        // The on-disk tag is what tooling (grep, jq) will match.
        // Lock it in so a future rename doesn't silently break
        // every existing transcript.
        let j = serde_json::to_string(&TurnEvent::User {
            ts: None,
            text: "x".into(),
        })
        .unwrap();
        assert!(j.contains(r#""kind":"user""#), "unexpected tag: {j}");

        let j = serde_json::to_string(&TurnEvent::TurnSummary {
            ts: None,
            elapsed_ms: None,
            ttft_ms: None,
            tokens_in: None,
            tokens_out: None,
            cache_read_tokens: None,
            tools: 0,
            cumulative_tokens: None,
            cumulative_cost_usd: None,
        })
        .unwrap();
        assert!(
            j.contains(r#""kind":"turn_summary""#),
            "unexpected tag: {j}"
        );
    }

    #[test]
    fn unknown_variant_fails_deserialise_loudly() {
        // Defensive: if an older build wrote an unknown `kind`, we
        // want the loader to surface that (and the enclosing file-
        // reader will just skip the bad line). A silent pass-through
        // would mean quietly dropping user data.
        let r: Result<TurnEvent, _> = serde_json::from_str(r#"{"kind":"nonsense","text":"x"}"#);
        assert!(r.is_err(), "unknown kind must be rejected");
    }
}
