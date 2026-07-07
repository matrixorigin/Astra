//! Core types and data structures for the background task view.

use ratatui::style::Color;

pub(crate) const BACKGROUND_TASK_STOP_SENTINEL: &str = "__background_task_stop__\n";
pub(crate) const BACKGROUND_TASK_OUTPUT_SENTINEL: &str = "__background_task_output__\n";
pub(crate) const PAGE_STEP: usize = 8;
pub(crate) const DETAIL_TAIL_LINES: usize = 8;

pub(crate) fn parse_stop_sentinel(s: &str) -> Option<&str> {
    s.strip_prefix(BACKGROUND_TASK_STOP_SENTINEL)
        .map(|rest| {
            let rest = rest.trim_start_matches('\n');
            rest.split_once('\n').map(|(id, _)| id).unwrap_or(rest)
        })
        .filter(|id| !id.is_empty())
}

pub(crate) fn parse_output_sentinel(s: &str) -> Option<&str> {
    s.strip_prefix(BACKGROUND_TASK_OUTPUT_SENTINEL)
        .map(|rest| {
            let rest = rest.trim_start_matches('\n');
            rest.split_once('\n').map(|(id, _)| id).unwrap_or(rest)
        })
        .filter(|id| !id.is_empty())
}

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

    pub(crate) fn supports_output_action(self) -> bool {
        matches!(self, Self::Shell | Self::LocalAgent)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundTaskStatus {
    Pending,
    WaitingForInput,
    Interrupted,
    Failed,
    Running,
    Killed,
    Completed,
    Unavailable,
}

impl BackgroundTaskStatus {
    pub(crate) fn from_str(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "waiting_for_input" => Self::WaitingForInput,
            "interrupted" => Self::Interrupted,
            "failed" => Self::Failed,
            "running" => Self::Running,
            "killed" => Self::Killed,
            "completed" => Self::Completed,
            "unavailable" => Self::Unavailable,
            _ => Self::Unavailable,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForInput => "waiting_for_input",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Killed => "killed",
            Self::Completed => "completed",
            Self::Unavailable => "unavailable",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForInput => "needs input",
            Self::Interrupted => "interrupted",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Killed => "stopped",
            Self::Completed => "completed",
            Self::Unavailable => "unavailable",
        }
    }

    pub(crate) fn color(self) -> Color {
        match self {
            Self::Pending => Color::Blue,
            Self::WaitingForInput => Color::Yellow,
            Self::Interrupted => Color::Yellow,
            Self::Failed => Color::Red,
            Self::Running => Color::Cyan,
            Self::Completed => Color::Green,
            Self::Killed | Self::Unavailable => Color::DarkGray,
        }
    }

    pub(crate) fn is_killable(self) -> bool {
        matches!(self, Self::Running | Self::WaitingForInput)
    }

    pub(crate) fn attention_rank(self) -> u8 {
        match self {
            Self::WaitingForInput | Self::Interrupted | Self::Failed => 0,
            Self::Running | Self::Pending => 1,
            Self::Killed => 2,
            Self::Completed => 3,
            Self::Unavailable => 4,
        }
    }

    pub(crate) fn empty_output_state(self) -> &'static str {
        match self {
            Self::Pending => "Pending · no output yet",
            Self::WaitingForInput => "Waiting for input · no output yet",
            Self::Interrupted => "Interrupted with no output",
            Self::Running => "No output yet · still running",
            Self::Completed => "Completed with no output",
            Self::Failed => "Failed with no output",
            Self::Killed => "Stopped with no output",
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
            Self::StaleHandle => Some("stale handle"),
            Self::UnsupportedInMode => Some("control unavailable"),
        }
    }

    pub(crate) fn can_stop(self) -> bool {
        matches!(self, Self::Available)
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
            status: BackgroundTaskStatus::from_str(status.as_ref()),
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
        if !live_control.can_stop()
            && matches!(
                self.status,
                BackgroundTaskStatus::Pending
                    | BackgroundTaskStatus::Running
                    | BackgroundTaskStatus::WaitingForInput
            )
        {
            self.status = BackgroundTaskStatus::Unavailable;
        }
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

    pub(crate) fn with_fanout(mut self, fanout: BackgroundTaskFanoutMembership) -> Self {
        self.fanout = Some(fanout);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    List,
    Detail,
}

pub(crate) fn sort_rows(mut rows: Vec<BackgroundTaskRow>) -> Vec<BackgroundTaskRow> {
    rows.sort_by_key(|row| (row.status.attention_rank(), row.elapsed_ms));
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

pub(crate) fn detail_actions_label(can_output: bool, can_stop: bool, width: usize) -> &'static str {
    match (can_output, can_stop, width) {
        (true, true, 34..) => "  actions: output · stop · return",
        (true, true, 24..) => "  actions: output · stop",
        (true, true, _) => "  actions: output",
        (true, false, 26..) => "  actions: output · return",
        (true, false, _) => "  actions: output",
        (false, true, 26..) => "  actions: stop · return",
        (false, true, _) => "  actions: stop",
        (false, false, _) => "  actions: return",
    }
}
