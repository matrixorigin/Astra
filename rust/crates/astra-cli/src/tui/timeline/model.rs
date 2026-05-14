//! Pure `Timeline` model — RED phase stub.

#![allow(dead_code)]

use std::sync::Arc;

/// Detail record for one tool call within a turn (mirrors journal's `ToolCallRecord`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolCallDetail {
    pub name: String,
    pub ok: bool,
    pub ms: u64,
    pub error: Option<String>,
    pub input_bytes: Option<u32>,
    pub output_bytes: Option<u32>,
    pub args_preview: Option<String>,
    pub start_offset_ms: Option<u64>,
    pub parallel: Option<bool>,
    pub round: Option<u32>,
}

/// A single turn's worth of journal metadata rendered in the timeline.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TimelineTurn {
    pub turn: u32,
    pub started_at: String,
    pub duration_ms: Option<u64>,
    pub model: Option<String>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub tool_count: Option<u32>,
    pub user_preview: Option<String>,
    pub assistant_preview: Option<String>,
    pub error: Option<String>,
    /// Cumulative total tokens in (including this turn).
    pub cumulative_tokens_in: u64,
    /// Cumulative total tokens out (including this turn).
    pub cumulative_tokens_out: u64,
    // ── Trace detail (populated from journal observability fields) ──
    pub ttft_ms: Option<u64>,
    pub context_ms: Option<u64>,
    pub memoria_ms: Option<u64>,
    pub llm_rounds: Option<u32>,
    pub selected_skills: Option<Vec<String>>,
    pub total_tool_ms: Option<u64>,
    pub total_llm_ms: Option<u64>,
    pub tool_calls: Vec<ToolCallDetail>,
    pub user_input: Option<String>,
    pub assistant_output: Option<String>,
}

impl TimelineTurn {
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

/// IO abstraction — tests inject [`StaticTurnSource`], production
/// uses `JournalTurnSource` (see `impl` below).
pub(crate) trait TurnSource: std::fmt::Debug + Send + Sync {
    fn load(&self, session_id: &str) -> Vec<TimelineTurn>;
}

// ── Journal-backed source (production) ────────────────────────────

const PREVIEW_CHARS: usize = 80;

fn preview(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(PREVIEW_CHARS).collect())
}

#[derive(Debug, Default, Clone)]
pub(crate) struct JournalTurnSource;

impl JournalTurnSource {
    pub fn new() -> Self {
        Self
    }
}

impl TurnSource for JournalTurnSource {
    fn load(&self, session_id: &str) -> Vec<TimelineTurn> {
        use astra_services::session_journal::{JournalEventType, read_journal};
        let Ok(events) = read_journal(session_id) else {
            return Vec::new();
        };
        events
            .into_iter()
            .filter(|e| {
                matches!(
                    e.event_type,
                    JournalEventType::Turn | JournalEventType::TurnError
                )
            })
            .filter_map(|e| {
                let tool_calls = e
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|tc| ToolCallDetail {
                                name: tc.name.clone(),
                                ok: tc.ok,
                                ms: tc.ms,
                                error: tc.error.clone(),
                                input_bytes: tc.input_bytes,
                                output_bytes: tc.output_bytes,
                                args_preview: tc.args_preview.clone(),
                                start_offset_ms: tc.start_offset_ms,
                                parallel: tc.parallel,
                                round: tc.round,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(TimelineTurn {
                    turn: e.turn?,
                    started_at: e.ts.clone(),
                    duration_ms: e.duration_ms,
                    model: e.model.clone(),
                    tokens_in: e.tokens_in,
                    tokens_out: e.tokens_out,
                    tool_count: e.tool_count,
                    user_preview: e.user_input.as_deref().and_then(preview),
                    assistant_preview: e.assistant_output.as_deref().and_then(preview),
                    error: e.error.clone(),
                    cumulative_tokens_in: 0,
                    cumulative_tokens_out: 0,
                    ttft_ms: e.ttft_ms,
                    context_ms: e.context_ms,
                    memoria_ms: e.memoria_ms,
                    llm_rounds: e.llm_rounds,
                    selected_skills: e.selected_skills.clone(),
                    total_tool_ms: e.total_tool_ms,
                    total_llm_ms: e.total_llm_ms,
                    tool_calls,
                    user_input: e.user_input.clone(),
                    assistant_output: e.assistant_output.clone(),
                })
            })
            .collect()
    }
}

// ── Static source for tests ───────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct StaticTurnSource {
    pub turns: Vec<TimelineTurn>,
}

impl StaticTurnSource {
    pub fn new(turns: Vec<TimelineTurn>) -> Self {
        Self { turns }
    }
}

impl TurnSource for StaticTurnSource {
    fn load(&self, _session_id: &str) -> Vec<TimelineTurn> {
        self.turns.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::JournalTurnSource;
    use super::TurnSource;
    use astra_services::session_journal::{JournalDirGuard, JournalEvent, JournalWriter};
    use tempfile::tempdir;

    #[test]
    fn journal_source_propagates_context_time() {
        let dir = tempdir().expect("tempdir");
        let sessions_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let _guard = JournalDirGuard::new(&sessions_dir);
        let writer = JournalWriter::new("sess-timeline").expect("writer");

        writer
            .append(
                &JournalEvent::turn(
                    Some("sess-timeline"),
                    1,
                    Some("sonnet-4.6"),
                    "inspect timeline",
                    "done",
                    0,
                    120,
                    40,
                    500,
                )
                .with_context_time(Some(91)),
            )
            .expect("append turn");

        let turns = JournalTurnSource::new().load("sess-timeline");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].context_ms, Some(91));
    }
}

// ── Timeline engine — RED stubs ───────────────────────────────────

pub(crate) struct Timeline {
    source: Arc<dyn TurnSource>,
    turns: Vec<TimelineTurn>,
    selected: usize,
    drilled: bool,
}

impl std::fmt::Debug for Timeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timeline")
            .field("turns_len", &self.turns.len())
            .field("selected", &self.selected)
            .finish()
    }
}

impl Timeline {
    pub fn new<S: TurnSource + 'static>(source: S, session_id: &str) -> Self {
        Self::from_arc(Arc::new(source), session_id)
    }

    pub fn from_arc(source: Arc<dyn TurnSource>, session_id: &str) -> Self {
        let mut turns = source.load(session_id);
        // Running totals for cumulative views.
        let mut cum_in = 0u64;
        let mut cum_out = 0u64;
        for t in turns.iter_mut() {
            cum_in = cum_in.saturating_add(t.tokens_in.unwrap_or(0));
            cum_out = cum_out.saturating_add(t.tokens_out.unwrap_or(0));
            t.cumulative_tokens_in = cum_in;
            t.cumulative_tokens_out = cum_out;
        }
        Self {
            source,
            turns,
            selected: 0,
            drilled: false,
        }
    }

    pub fn total(&self) -> usize {
        self.turns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.turns.is_empty()
    }

    pub fn turns(&self) -> &[TimelineTurn] {
        &self.turns
    }

    pub fn selected(&self) -> Option<usize> {
        if self.turns.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    pub fn selected_turn(&self) -> Option<&TimelineTurn> {
        self.turns.get(self.selected)
    }

    pub fn move_up(&mut self) {
        if self.turns.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.turns.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.turns.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.turns.len();
    }

    pub fn is_drilled(&self) -> bool {
        self.drilled
    }

    pub fn enter_drill(&mut self) {
        if !self.turns.is_empty() {
            self.drilled = true;
        }
    }

    pub fn exit_drill(&mut self) {
        self.drilled = false;
    }

    pub fn grand_total_tokens_in(&self) -> u64 {
        self.turns
            .last()
            .map(|t| t.cumulative_tokens_in)
            .unwrap_or(0)
    }

    pub fn grand_total_tokens_out(&self) -> u64 {
        self.turns
            .last()
            .map(|t| t.cumulative_tokens_out)
            .unwrap_or(0)
    }
}
