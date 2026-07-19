//! Core types and data structures for the background task view.

use astra_core::work_unit::WorkUnitStatus;
use ratatui::style::Color;

pub(crate) const PAGE_STEP: usize = 8;
pub(crate) const DETAIL_TAIL_LINES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundTaskKind {
    Shell,
    LocalAgent,
    CloudSession,
    MainSession,
    Monitor,
}

impl BackgroundTaskKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::LocalAgent => "local agent",
            Self::CloudSession => "cloud session",
            Self::MainSession => "main session",
            Self::Monitor => "monitor",
        }
    }
}

pub(crate) type BackgroundTaskStatus = WorkUnitStatus;

pub(crate) trait BackgroundTaskStatusExt {
    fn label(self) -> &'static str;
    fn color(self) -> Color;
    fn is_killable(self) -> bool;
    fn empty_output_state(self) -> &'static str;
}

impl BackgroundTaskStatusExt for WorkUnitStatus {
    fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForInput => "needs input",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Cancelled => "stopped",
            Self::Completed => "completed",
            Self::CompletedWithIssues => "completed with issues",
            Self::Unavailable => "unavailable",
        }
    }

    fn color(self) -> Color {
        let theme = crate::tui::theme::current();
        match self {
            Self::Pending => theme.accent,
            Self::WaitingForInput => theme.warn,
            Self::Interrupted => theme.warn,
            Self::Failed => theme.error,
            Self::Running => theme.gutter,
            Self::Stopping => theme.accent,
            Self::Completed | Self::CompletedWithIssues => theme.success,
            Self::Cancelled | Self::Unavailable => theme.dim,
        }
    }

    fn is_killable(self) -> bool {
        matches!(self, Self::Running | Self::WaitingForInput)
    }

    fn empty_output_state(self) -> &'static str {
        match self {
            Self::Pending => "Pending · no output yet",
            Self::WaitingForInput => "Waiting for input · no output yet",
            Self::Interrupted => "Interrupted with no output",
            Self::Running => "No output yet · still running",
            Self::Stopping => "Stopping · no output captured yet",
            Self::Completed => "Completed with no output",
            Self::CompletedWithIssues => "Completed with issues and no output",
            Self::Failed => "Failed with no output",
            Self::Cancelled => "Stopped with no output",
            Self::Unavailable => "Unavailable · stale handle or unsupported runner",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveControlState {
    Available,
    StaleHandle,
    UnsupportedInMode,
}

impl LiveControlState {
    pub(crate) fn label(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::StaleHandle => Some("restored snapshot · no live control"),
            Self::UnsupportedInMode => Some("control unavailable in this mode"),
        }
    }

    pub(crate) fn can_stop(self) -> bool {
        matches!(self, Self::Available)
    }

    pub(crate) fn list_label(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::StaleHandle => Some("stale snapshot"),
            Self::UnsupportedInMode => Some("no control"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundTaskFanoutMembership {
    pub group_id: String,
    pub group_title: String,
    pub target_count: usize,
    pub slot_index: usize,
    pub slot_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundTaskRow {
    pub id: String,
    pub kind: BackgroundTaskKind,
    pub status: BackgroundTaskStatus,
    pub live_control: LiveControlState,
    pub elapsed_ms: u64,
    pub no_recent_output_ms: Option<u64>,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
    pub title: String,
    pub output_ref: Option<String>,
    pub output_tail: Option<String>,
    pub output_offset: Option<u64>,
    pub total_bytes: Option<u64>,
    pub total_lines: Option<u64>,
    pub exit_code: Option<i32>,
    pub terminal_reason: Option<String>,
    pub fanout: Option<BackgroundTaskFanoutMembership>,
    /// Whether the work unit has been explicitly detached from its parent.
    /// Foreground fan-in remains observable in this view without pretending
    /// that observation changed lifecycle ownership.
    pub run_in_background: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundTaskRowInit {
    id: String,
    kind: BackgroundTaskKind,
    status: BackgroundTaskStatus,
    elapsed_ms: u64,
    title: String,
    output_ref: Option<String>,
    output_tail: Option<String>,
    total_bytes: Option<u64>,
}

impl BackgroundTaskRowInit {
    pub(crate) fn new(
        id: impl Into<String>,
        kind: BackgroundTaskKind,
        status: impl AsRef<str>,
        elapsed_ms: u64,
        title: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            status: BackgroundTaskStatus::parse(status.as_ref())
                .unwrap_or(BackgroundTaskStatus::Unavailable),
            elapsed_ms,
            title: title.into(),
            output_ref: None,
            output_tail: None,
            total_bytes: None,
        }
    }

    pub(crate) fn with_output(
        mut self,
        output_ref: Option<String>,
        output_tail: Option<String>,
        total_bytes: Option<u64>,
    ) -> Self {
        self.output_ref = output_ref;
        self.output_tail = output_tail;
        self.total_bytes = total_bytes;
        self
    }
}

impl BackgroundTaskRow {
    pub(crate) fn new(init: BackgroundTaskRowInit) -> Self {
        Self {
            id: init.id,
            kind: init.kind,
            status: init.status,
            live_control: LiveControlState::Available,
            elapsed_ms: init.elapsed_ms,
            no_recent_output_ms: None,
            started_at_ms: None,
            ended_at_ms: None,
            title: init.title,
            output_ref: init.output_ref,
            output_tail: init.output_tail,
            output_offset: None,
            total_bytes: init.total_bytes,
            total_lines: None,
            exit_code: None,
            terminal_reason: None,
            fanout: None,
            run_in_background: true,
        }
    }

    pub(crate) fn shell(
        id: impl Into<String>,
        status: impl AsRef<str>,
        elapsed_ms: u64,
        title: impl Into<String>,
        output_ref: Option<String>,
        output_tail: Option<String>,
        total_bytes: Option<u64>,
    ) -> Self {
        Self::new(
            BackgroundTaskRowInit::new(id, BackgroundTaskKind::Shell, status, elapsed_ms, title)
                .with_output(output_ref, output_tail, total_bytes),
        )
    }

    pub(crate) fn with_live_control(mut self, live_control: LiveControlState) -> Self {
        self.live_control = live_control;
        self
    }

    pub(crate) fn with_output_stats(
        mut self,
        output_offset: Option<u64>,
        total_lines: Option<u64>,
    ) -> Self {
        self.output_offset = output_offset;
        self.total_lines = total_lines;
        self
    }

    pub(crate) fn with_terminal(
        mut self,
        exit_code: Option<i32>,
        terminal_reason: Option<String>,
    ) -> Self {
        self.exit_code = exit_code;
        self.terminal_reason = terminal_reason;
        self
    }

    pub(crate) fn with_timing(
        mut self,
        started_at_ms: Option<u64>,
        ended_at_ms: Option<u64>,
    ) -> Self {
        self.started_at_ms = started_at_ms;
        self.ended_at_ms = ended_at_ms;
        self
    }

    pub(crate) fn with_no_recent_output(mut self, inactive_ms: Option<u64>) -> Self {
        self.no_recent_output_ms = inactive_ms;
        self
    }

    pub(crate) fn with_fanout(mut self, fanout: BackgroundTaskFanoutMembership) -> Self {
        self.fanout = Some(fanout);
        self
    }

    pub(crate) fn with_run_in_background(mut self, run_in_background: bool) -> Self {
        self.run_in_background = run_in_background;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    List,
    Detail,
}

pub(crate) fn sort_rows(mut rows: Vec<BackgroundTaskRow>) -> Vec<BackgroundTaskRow> {
    // The view is refreshed on every TUI tick. Status and elapsed time are
    // mutable, so including either in this key makes rows jump while the user
    // is reading or navigating the list. Fanout slots additionally need one
    // shared anchor so that independently-started children remain contiguous.
    let mut fanout_started_at = std::collections::HashMap::<String, u64>::new();
    for row in &rows {
        let (Some(fanout), Some(started_at_ms)) = (row.fanout.as_ref(), row.started_at_ms) else {
            continue;
        };
        fanout_started_at
            .entry(fanout.group_id.clone())
            .and_modify(|current| *current = (*current).min(started_at_ms))
            .or_insert(started_at_ms);
    }
    rows.sort_by_key(|row| {
        if let Some(fanout) = row.fanout.as_ref() {
            (
                fanout_started_at
                    .get(&fanout.group_id)
                    .copied()
                    .unwrap_or(u64::MAX),
                format!("fanout:{}", fanout.group_id),
                fanout.slot_index,
                row.id.clone(),
            )
        } else {
            (
                row.started_at_ms.unwrap_or(u64::MAX),
                format!("task:{}", row.id),
                0,
                row.id.clone(),
            )
        }
    });
    rows
}

pub(crate) fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs}s")
    }
}

pub(crate) fn format_timestamp_ms(ms: u64) -> String {
    let Ok(ms) = i64::try_from(ms) else {
        return format!("{ms}ms");
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{ms}ms"))
}

pub(crate) fn pluralize_with_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

pub(crate) fn detail_actions_label(can_stop: bool, width: usize) -> &'static str {
    match (can_stop, width) {
        (true, 26..) => "  actions: stop · return",
        (true, _) => "  actions: stop",
        (false, _) => "  actions: return",
    }
}
