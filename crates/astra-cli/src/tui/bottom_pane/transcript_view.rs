use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use astra_core::sync_poison::recover_mutex_lock;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};
use unicode_width::UnicodeWidthStr;

use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ConversationTabId,
    ViewActionDisposition, ViewActionRequest, ViewCompletion,
};
use crate::tui::history_cell::{
    HistoryCell, assistant::AssistantCell, reasoning::ReasoningCell, tool::ToolCell,
};
use crate::tui::render::line_utils::{history_cell_lines_for_buffer, sanitize_lines_for_buffer};

/// Lines reserved for chrome (title + scroll indicator + hint blank + hint).
/// Kept in one place so `desired_height` and `visible_line_count` agree.
const CHROME_LINES: u16 = 4;

/// Floor for the visible content region — below this the overlay is
/// useless even on a tiny terminal.
const MIN_VISIBLE_LINES: u16 = 8;

/// Default target when we don't know the terminal height. Matches the
/// pre-refactor cap so behaviour is unchanged on first-render fallback.
const DEFAULT_VISIBLE_LINES: u16 = 16;

/// Lossless identity for one transcript object.
///
/// Local cells use the immutable id allocated by `ChatWidget`. Durable cells
/// retain their canonical event/item identity plus a typed projection
/// component. Keeping those fields instead of hashing them is what lets a
/// refresh or paginated prepend preserve the exact cursor/expansion anchor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TranscriptItemId {
    Widget(u64),
    Canonical { item: Arc<str>, component: Arc<str> },
}

impl TranscriptItemId {
    pub(crate) fn from_widget_id(id: u64) -> Self {
        Self::Widget(id)
    }

    pub(crate) fn from_canonical(
        item: impl Into<Arc<str>>,
        component: impl Into<Arc<str>>,
    ) -> Self {
        Self::Canonical {
            item: item.into(),
            component: component.into(),
        }
    }
}

#[derive(Debug, Clone)]
enum TranscriptContent {
    Committed(Arc<dyn HistoryCell>),
    Rendered(Vec<Line<'static>>),
    Activity(Arc<str>),
    ToolRequest {
        cell: Arc<ToolCell>,
        label: &'static str,
    },
    RenderedTrusted(Vec<Line<'static>>),
    Reasoning(ReasoningCell),
    LiveAssistant(Arc<Mutex<AssistantCell>>),
    LiveReasoning(Arc<Mutex<ReasoningCell>>),
    Tool(Arc<ToolCell>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptFilter {
    All,
    Conversation,
    User,
    Assistant,
    Reasoning,
    Tools,
    Agents,
    System,
    Errors,
}

impl TranscriptFilter {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Conversation,
            Self::Conversation => Self::User,
            Self::User => Self::Assistant,
            Self::Assistant => Self::Reasoning,
            Self::Reasoning => Self::Tools,
            Self::Tools => Self::Agents,
            Self::Agents => Self::System,
            Self::System => Self::Errors,
            Self::Errors => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Conversation => "conversation",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Reasoning => "reasoning",
            Self::Tools => "tools",
            Self::Agents => "agents",
            Self::System => "system",
            Self::Errors => "errors",
        }
    }
}

/// Semantic class carried by every transcript object.
///
/// Filtering uses this typed projection rather than labels or rendered text,
/// so root, delegated, live and durable transcript sources behave identically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TranscriptItemKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    Agent,
    System,
    Error,
}

/// One selectable transcript object. Compact and expanded rendering are
/// projections of the same canonical cell; expansion state lives in the view.
#[derive(Debug, Clone)]
pub(crate) struct TranscriptItem {
    model_item_id: Option<String>,
    tool_identity: Option<(String, bool)>,
    id: TranscriptItemId,
    kind: TranscriptItemKind,
    content: TranscriptContent,
    separator_rows: usize,
}

impl TranscriptItem {
    pub(crate) fn with_tool_use_id(mut self, tool_use_id: Option<&str>, settled: bool) -> Self {
        self.tool_identity = tool_use_id.map(|id| (id.to_owned(), settled));
        self
    }

    pub(crate) fn tool_identity(&self) -> Option<&str> {
        self.tool_identity.as_ref().map(|(id, _)| id.as_str())
    }

    pub(crate) fn is_settled_tool(&self) -> bool {
        self.tool_identity
            .as_ref()
            .is_some_and(|(_, settled)| *settled)
    }
    pub(crate) fn model_identity(&self) -> Option<(&str, TranscriptItemKind)> {
        self.model_item_id.as_deref().map(|id| (id, self.kind))
    }
    #[cfg(test)]
    pub(crate) fn id(&self) -> &TranscriptItemId {
        &self.id
    }

    pub(crate) fn committed(
        id: TranscriptItemId,
        cell: Arc<dyn HistoryCell>,
        separator_rows: usize,
    ) -> Self {
        Self {
            model_item_id: cell.model_item_id().map(str::to_owned),
            tool_identity: None,
            id,
            kind: committed_cell_kind(cell.as_ref()),
            content: TranscriptContent::Committed(cell),
            separator_rows,
        }
    }

    pub(crate) fn rendered(
        id: TranscriptItemId,
        lines: Vec<Line<'static>>,
        separator_rows: usize,
    ) -> Self {
        Self::rendered_kind(id, TranscriptItemKind::System, lines, separator_rows)
    }

    /// Recorded activity keeps full text for expansion, search, and export;
    /// the normal view renders a bounded preview.
    pub(crate) fn activity(id: TranscriptItemId, kind: TranscriptItemKind, text: String) -> Self {
        let text = crate::tui::render::line_utils::sanitize_terminal_text(&text).into_owned();
        Self {
            model_item_id: None,
            tool_identity: None,
            id,
            kind,
            content: TranscriptContent::Activity(text.into()),
            separator_rows: 0,
        }
    }

    pub(crate) fn rendered_kind(
        id: TranscriptItemId,
        kind: TranscriptItemKind,
        lines: Vec<Line<'static>>,
        separator_rows: usize,
    ) -> Self {
        Self {
            model_item_id: None,
            tool_identity: None,
            id,
            kind,
            content: TranscriptContent::Rendered(lines),
            separator_rows,
        }
    }

    pub(crate) fn rendered_cell(
        id: TranscriptItemId,
        cell: &dyn HistoryCell,
        lines: Vec<Line<'static>>,
        separator_rows: usize,
    ) -> Self {
        let kind = committed_cell_kind(cell);
        let content = if cell
            .as_any_ref()
            .is::<crate::tui::history_cell::system::SystemCell>()
        {
            TranscriptContent::RenderedTrusted(lines)
        } else {
            TranscriptContent::Rendered(lines)
        };
        Self {
            model_item_id: cell.model_item_id().map(str::to_owned),
            tool_identity: None,
            id,
            kind,
            content,
            separator_rows,
        }
    }

    pub(crate) fn reasoning(
        id: TranscriptItemId,
        cell: ReasoningCell,
        separator_rows: usize,
    ) -> Self {
        Self {
            model_item_id: cell.model_item_id.clone(),
            tool_identity: None,
            id,
            kind: TranscriptItemKind::Reasoning,
            content: TranscriptContent::Reasoning(cell),
            separator_rows,
        }
    }

    /// Share the live source with the agent view. Rendering stays bounded,
    /// while explicit search and export can read the complete current item.
    pub(crate) fn live_assistant(
        id: TranscriptItemId,
        cell: Arc<Mutex<AssistantCell>>,
        separator_rows: usize,
    ) -> Self {
        let model_item_id = recover_mutex_lock(&cell).model_item_id().map(str::to_owned);
        Self {
            model_item_id,
            tool_identity: None,
            id,
            kind: TranscriptItemKind::Assistant,
            content: TranscriptContent::LiveAssistant(cell),
            separator_rows,
        }
    }

    pub(crate) fn live_reasoning(
        id: TranscriptItemId,
        cell: Arc<Mutex<ReasoningCell>>,
        separator_rows: usize,
    ) -> Self {
        let model_item_id = recover_mutex_lock(&cell).model_item_id().map(str::to_owned);
        Self {
            model_item_id,
            tool_identity: None,
            id,
            kind: TranscriptItemKind::Reasoning,
            content: TranscriptContent::LiveReasoning(cell),
            separator_rows,
        }
    }

    pub(crate) fn tool(
        id: TranscriptItemId,
        cell: impl Into<Arc<ToolCell>>,
        separator_rows: usize,
    ) -> Self {
        Self {
            model_item_id: None,
            tool_identity: None,
            id,
            kind: TranscriptItemKind::Tool,
            content: TranscriptContent::Tool(cell.into()),
            separator_rows,
        }
    }

    pub(crate) fn tool_cell(&self) -> Option<&ToolCell> {
        match &self.content {
            TranscriptContent::Tool(cell) | TranscriptContent::ToolRequest { cell, .. } => {
                Some(cell)
            }
            _ => None,
        }
    }

    pub(crate) fn tool_request(&self) -> Option<Self> {
        let cell = match &self.content {
            TranscriptContent::Tool(cell) | TranscriptContent::ToolRequest { cell, .. } => cell,
            _ => return None,
        };
        Some(Self::request(
            self.id.clone(),
            cell.clone(),
            "request · result not loaded",
        ))
    }

    pub(crate) fn observed_tool_request(id: TranscriptItemId, cell: Arc<ToolCell>) -> Self {
        Self::request(id, cell, "observed request")
    }

    fn request(id: TranscriptItemId, cell: Arc<ToolCell>, label: &'static str) -> Self {
        Self {
            model_item_id: None,
            tool_identity: None,
            id,
            kind: TranscriptItemKind::Tool,
            content: TranscriptContent::ToolRequest { cell, label },
            separator_rows: 1,
        }
    }

    pub(crate) fn running_request(&self) -> Option<Self> {
        match &self.content {
            TranscriptContent::ToolRequest { cell, .. } => {
                Some(Self::request(self.id.clone(), cell.clone(), "Running"))
            }
            _ => None,
        }
    }

    fn detail_text(&self) -> Option<(&str, Option<(&'static str, &str)>)> {
        match &self.content {
            TranscriptContent::Activity(text) => Some((text, None)),
            TranscriptContent::ToolRequest { cell, label } => {
                Some((&cell.description, Some((label, &cell.name))))
            }
            _ => None,
        }
    }

    /// Materialize text only for an explicit full-detail operation.
    fn full_detail_text(&self) -> Option<String> {
        match &self.content {
            TranscriptContent::Committed(cell) => {
                if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>() {
                    Some(assistant.transcript_detail_text().into_owned())
                } else {
                    cell.as_any_ref()
                        .downcast_ref::<ReasoningCell>()
                        .map(|reasoning| format!("▼ Thought\n{}", reasoning.text()))
                }
            }
            TranscriptContent::Reasoning(cell) => Some(format!("▼ Thought\n{}", cell.text())),
            TranscriptContent::LiveAssistant(cell) => Some(
                recover_mutex_lock(cell)
                    .transcript_detail_text()
                    .into_owned(),
            ),
            TranscriptContent::LiveReasoning(cell) => {
                Some(format!("▼ Thought\n{}", recover_mutex_lock(cell).text()))
            }
            _ => None,
        }
    }

    fn same_detail_source(&self, other: &TranscriptContent) -> bool {
        match (&self.content, other) {
            (TranscriptContent::Activity(a), TranscriptContent::Activity(b)) => {
                Arc::ptr_eq(a, b) || a == b
            }
            (
                TranscriptContent::ToolRequest { cell: a, label: x },
                TranscriptContent::ToolRequest { cell: b, label: y },
            ) => x == y && Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    fn is_expandable(&self) -> bool {
        match &self.content {
            TranscriptContent::Committed(cell) => cell_expandable(cell.as_ref()),
            TranscriptContent::Rendered(_) | TranscriptContent::RenderedTrusted(_) => false,
            TranscriptContent::Activity(text) => !text.is_empty(),
            TranscriptContent::ToolRequest { cell, .. } => !cell.description.is_empty(),
            TranscriptContent::Reasoning(cell) => !cell.text().is_empty(),
            TranscriptContent::LiveAssistant(cell) => !recover_mutex_lock(cell).source().is_empty(),
            TranscriptContent::LiveReasoning(cell) => !recover_mutex_lock(cell).text().is_empty(),
            TranscriptContent::Tool(cell) => cell.has_transcript_details(),
        }
    }

    fn label(&self) -> &'static str {
        match &self.content {
            TranscriptContent::Committed(cell) => cell_label(cell.as_ref()),
            TranscriptContent::Rendered(_) | TranscriptContent::RenderedTrusted(_) => "item",
            TranscriptContent::Activity(_) => "activity details",
            TranscriptContent::ToolRequest { .. } => "tool request",
            TranscriptContent::Reasoning(_) => "reasoning",
            TranscriptContent::LiveAssistant(_) => "assistant output",
            TranscriptContent::LiveReasoning(_) => "reasoning",
            TranscriptContent::Tool(_) => "tool details",
        }
    }

    fn render(&self, width: u16, expanded: bool) -> Vec<Line<'static>> {
        let lines = match &self.content {
            TranscriptContent::Committed(cell) => cell_lines(cell.as_ref(), width, expanded),
            TranscriptContent::Rendered(lines) => lines.clone(),
            TranscriptContent::Activity(_) | TranscriptContent::ToolRequest { .. } => {
                let (text, heading) = self.detail_text().unwrap();
                render_detail(text, heading, width, expanded)
            }
            TranscriptContent::RenderedTrusted(lines) => lines.clone(),
            TranscriptContent::Reasoning(cell) if expanded => {
                render_detail(&format!("▼ Thought\n{}", cell.text()), None, width, true)
            }
            TranscriptContent::Reasoning(cell) => cell.transcript_lines(width, false),
            TranscriptContent::LiveAssistant(cell) => {
                let cell = recover_mutex_lock(cell);
                if expanded {
                    render_detail(&cell.transcript_detail_text(), None, width, true)
                } else if !cell.is_live() {
                    history_cell_lines_for_buffer(&*cell, width)
                } else {
                    cell.live_viewport_lines(width, 48)
                }
            }
            TranscriptContent::LiveReasoning(cell) => {
                let cell = recover_mutex_lock(cell);
                if expanded {
                    render_detail(&format!("▼ Thought\n{}", cell.text()), None, width, true)
                } else {
                    cell.transcript_lines(width, false)
                }
            }
            TranscriptContent::Tool(cell) => cell.transcript_lines(width, expanded),
        };
        if matches!(
            &self.content,
            TranscriptContent::Committed(cell)
                if cell
                    .as_any_ref()
                    .is::<crate::tui::history_cell::system::SystemCell>()
        ) || matches!(&self.content, TranscriptContent::RenderedTrusted(_))
        {
            crate::tui::render::line_utils::sanitize_lines_for_buffer_with_links(lines)
        } else {
            sanitize_lines_for_buffer(lines)
        }
    }

    fn matches_filter(&self, filter: TranscriptFilter) -> bool {
        match filter {
            TranscriptFilter::All => true,
            TranscriptFilter::Conversation => matches!(
                self.kind,
                TranscriptItemKind::User | TranscriptItemKind::Assistant
            ),
            TranscriptFilter::User => self.kind == TranscriptItemKind::User,
            TranscriptFilter::Assistant => self.kind == TranscriptItemKind::Assistant,
            TranscriptFilter::Reasoning => self.kind == TranscriptItemKind::Reasoning,
            TranscriptFilter::Tools => self.kind == TranscriptItemKind::Tool,
            TranscriptFilter::Agents => self.kind == TranscriptItemKind::Agent,
            TranscriptFilter::System => self.kind == TranscriptItemKind::System,
            TranscriptFilter::Errors => self.kind == TranscriptItemKind::Error,
        }
    }
}

fn render_detail(
    text: &str,
    heading: Option<(&'static str, &str)>,
    width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    let preview_bytes = usize::from(width.max(1)).saturating_mul(20).min(512);
    let mut end = text.len().min(preview_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let input = if expanded { text } else { &text[..end] };
    let input = crate::tui::render::line_utils::sanitize_terminal_text(input);
    let mut source = input
        .lines()
        .take(if expanded { usize::MAX } else { 6 })
        .map(|line| Line::from(line.to_owned()))
        .collect::<Vec<_>>();
    let omitted = !expanded && (end < text.len() || source.len() > 5);
    if !expanded {
        source.truncate(5);
    }
    let mut lines = crate::tui::wrapping::word_wrap_lines(source, width.max(1) as usize);
    let wrapped_truncated = !expanded && lines.len() > 5;
    if wrapped_truncated {
        lines.truncate(5);
    }
    if omitted || wrapped_truncated {
        lines.push(Line::from("… expand for full activity"));
    }
    if let Some((heading, name)) = heading {
        lines.insert(0, Line::from(format!("{name} · {heading}")));
    }
    lines
}

fn committed_cell_kind(cell: &dyn HistoryCell) -> TranscriptItemKind {
    use crate::tui::history_cell::{assistant::AssistantCell, system::SystemCell, user::UserCell};
    use crate::tui::turn_event::SystemLevel;

    if cell.as_any_ref().is::<UserCell>() {
        TranscriptItemKind::User
    } else if cell.as_any_ref().is::<AssistantCell>() {
        TranscriptItemKind::Assistant
    } else if cell.as_any_ref().is::<ReasoningCell>() {
        TranscriptItemKind::Reasoning
    } else if cell.as_any_ref().is::<ToolCell>() {
        TranscriptItemKind::Tool
    } else if cell
        .as_any_ref()
        .downcast_ref::<SystemCell>()
        .is_some_and(|cell| cell.level() == SystemLevel::Error)
    {
        TranscriptItemKind::Error
    } else {
        TranscriptItemKind::System
    }
}

fn cell_expandable(cell: &dyn HistoryCell) -> bool {
    if let Some(reasoning) = cell.as_any_ref().downcast_ref::<ReasoningCell>() {
        !reasoning.text().is_empty()
    } else if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>() {
        !assistant.source().is_empty()
    } else if let Some(tool) = cell.as_any_ref().downcast_ref::<ToolCell>() {
        tool.has_transcript_details()
    } else {
        false
    }
}

fn cell_label(cell: &dyn HistoryCell) -> &'static str {
    if cell.as_any_ref().is::<ReasoningCell>() {
        "reasoning"
    } else if cell.as_any_ref().is::<ToolCell>() {
        "tool details"
    } else {
        "item"
    }
}

fn cell_lines(cell: &dyn HistoryCell, width: u16, expanded: bool) -> Vec<Line<'static>> {
    if let Some(reasoning) = cell.as_any_ref().downcast_ref::<ReasoningCell>() {
        if expanded {
            render_detail(
                &format!("▼ Thought\n{}", reasoning.text()),
                None,
                width,
                true,
            )
        } else {
            reasoning.transcript_lines(width, false)
        }
    } else if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>()
        && expanded
    {
        render_detail(&assistant.transcript_detail_text(), None, width, true)
    } else if let Some(tool) = cell.as_any_ref().downcast_ref::<ToolCell>() {
        tool.transcript_lines(width, expanded)
    } else {
        history_cell_lines_for_buffer(cell, width)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TranscriptSnapshot {
    items: Vec<TranscriptItem>,
}

impl TranscriptSnapshot {
    pub(crate) fn new(items: Vec<TranscriptItem>) -> Self {
        Self { items }
    }

    pub(crate) fn item_count(&self) -> usize {
        self.items.len()
    }

    fn map_item_id(&self, previous: &Self, id: &TranscriptItemId) -> Option<TranscriptItemId> {
        if self.items.iter().any(|item| &item.id == id) {
            return Some(id.clone());
        }
        let identity = previous
            .items
            .iter()
            .find(|item| &item.id == id)?
            .model_identity()?;
        if previous
            .items
            .iter()
            .filter(|item| item.model_identity() == Some(identity))
            .count()
            != 1
        {
            return None;
        }
        let mut matches = self
            .items
            .iter()
            .filter(|item| item.model_identity() == Some(identity));
        let mapped = matches.next()?.id.clone();
        matches.next().is_none().then_some(mapped)
    }
}

type ExpandedActivityCache = (
    TranscriptItemId,
    u16,
    TranscriptContent,
    Vec<Arc<Line<'static>>>,
);

/// Full conversation transcript including thinking content and tool output.
pub(crate) struct TranscriptView {
    title: String,
    snapshot: TranscriptSnapshot,
    lines: Vec<Arc<Line<'static>>>,
    row_items: Vec<Option<TranscriptItemId>>,
    expanded: HashSet<TranscriptItemId>,
    activity_cache: Option<ExpandedActivityCache>,
    /// Expanded live content is a stable reading snapshot until navigation
    /// refreshes it or the user explicitly returns to the live tail.
    live_tail_dirty: bool,
    width: u16,
    scroll: usize,
    cursor: usize,
    selection_anchor: Option<usize>,
    completed: bool,
    status: Option<String>,
    search_input: Option<String>,
    last_search: Option<String>,
    filter: TranscriptFilter,
    pending_action: Option<ViewActionRequest>,
    /// Max content rows to show at once. Derived from the terminal
    /// height at push time so tall windows get a full-screen overlay
    /// instead of a fixed 16-line peephole.
    max_visible: u16,
}

impl TranscriptView {
    /// Build from the typed transcript projection. Pass a zero terminal
    /// height to use the headless-test fallback window.
    pub(crate) fn from_snapshot(
        snapshot: TranscriptSnapshot,
        terminal_height: u16,
        width: u16,
    ) -> Self {
        let max_visible = if terminal_height == 0 {
            DEFAULT_VISIBLE_LINES
        } else {
            // Leave room for the composer/footer below and the chrome
            // inside the view. 80% of the terminal is close to what
            // Codex uses for full-screen overlays.
            let budget = (terminal_height as u32 * 80 / 100) as u16;
            budget.saturating_sub(CHROME_LINES).max(MIN_VISIBLE_LINES)
        };
        let mut view = Self {
            title: "Transcript".to_string(),
            snapshot,
            lines: Vec::new(),
            row_items: Vec::new(),
            expanded: HashSet::new(),
            activity_cache: None,
            live_tail_dirty: false,
            width,
            scroll: 0,
            cursor: 0,
            selection_anchor: None,
            completed: false,
            status: None,
            search_input: None,
            last_search: None,
            filter: TranscriptFilter::All,
            pending_action: None,
            max_visible,
        };
        view.rebuild_lines();
        view.cursor = view.lines.len().saturating_sub(1);
        view.scroll = view.max_scroll();
        view
    }

    /// Let this transcript own the main terminal canvas instead of appearing
    /// as a bounded pane beneath another conversation. The selected item's
    /// location and tail-following state survive the resize.
    pub(crate) fn fit_workspace(&mut self, terminal_height: u16, width: u16) {
        let max_visible = if terminal_height == 0 {
            DEFAULT_VISIBLE_LINES
        } else {
            terminal_height
                .saturating_sub(CHROME_LINES)
                .max(MIN_VISIBLE_LINES)
        };
        if self.max_visible == max_visible && self.width == width {
            return;
        }

        if self.selection_holds_live_projection() {
            // Keep the visible rows being selected stable, even when a
            // resize or a canonical refresh changes the latest projection.
            self.max_visible = max_visible;
            self.width = width;
            self.live_tail_dirty = true;
            self.status = Some("View updated · finish selecting to refresh".into());
            self.scroll = self.scroll.min(self.max_scroll());
            self.ensure_cursor_visible();
            return;
        }

        let follow_tail = self.is_following_tail();
        let cursor_locator = self.row_locator(self.cursor);
        let anchor_locator = self
            .selection_anchor
            .and_then(|anchor| self.row_locator(anchor));
        self.max_visible = max_visible;
        self.width = width;
        self.rebuild_lines();

        if self.lines.is_empty() {
            self.scroll = 0;
            self.cursor = 0;
            self.selection_anchor = None;
            return;
        }
        if follow_tail {
            self.cursor = self.lines.len() - 1;
            self.scroll = self.max_scroll();
            return;
        }
        self.cursor = cursor_locator
            .and_then(|locator| self.row_for_locator(locator))
            .unwrap_or_else(|| self.cursor.min(self.lines.len() - 1));
        self.selection_anchor = anchor_locator
            .and_then(|locator| self.row_for_locator(locator))
            .or_else(|| {
                self.selection_anchor
                    .map(|anchor| anchor.min(self.lines.len() - 1))
            });
        self.ensure_cursor_visible();
    }

    pub(crate) fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    pub(crate) fn replace_with(&mut self, snapshot: TranscriptSnapshot, width: u16) {
        self.replace_snapshot(snapshot, width);
    }

    /// Update an existing live tail without rendering the unchanged history.
    /// A different layout or identity falls back to the normal full projection.
    pub(crate) fn replace_live_tail(&mut self, item: TranscriptItem, width: u16) -> bool {
        let Some(old) = self.snapshot.items.last() else {
            return false;
        };
        if self.width != width || old.id != item.id || old.kind != item.kind {
            return false;
        }
        if !old.matches_filter(self.filter) {
            *self.snapshot.items.last_mut().expect("checked tail") = item;
            return true;
        }
        if (self.expanded.contains(&item.id) || self.selection_anchor.is_some())
            && matches!(
                &item.content,
                TranscriptContent::LiveAssistant(_) | TranscriptContent::LiveReasoning(_)
            )
        {
            *self.snapshot.items.last_mut().expect("checked tail") = item;
            self.live_tail_dirty = true;
            self.status = Some("New live output · End follows the latest".into());
            return true;
        }
        let Some(last_row) = self
            .row_items
            .iter()
            .rposition(|id| id.as_ref() == Some(&item.id))
        else {
            return false;
        };
        if self.row_items[last_row + 1..].iter().any(Option::is_some) {
            return false;
        }
        let mut start = last_row;
        while start > 0 && self.row_items[start - 1].as_ref() == Some(&item.id) {
            start -= 1;
        }
        let mut lines = item.render(width, self.expanded.contains(&item.id));
        while lines.last().is_some_and(|line| line.spans.is_empty()) {
            lines.pop();
        }
        if lines.is_empty() {
            return false;
        }
        let follow_tail = self.is_following_tail();
        let cursor = self.cursor;
        let anchor = self.selection_anchor;
        self.lines.truncate(start);
        self.row_items.truncate(start);
        self.row_items
            .extend(std::iter::repeat_n(Some(item.id.clone()), lines.len()));
        self.lines.extend(lines.into_iter().map(Arc::new));
        *self.snapshot.items.last_mut().expect("checked tail") = item;
        if follow_tail {
            self.cursor = self.lines.len() - 1;
            self.scroll = self.max_scroll();
        } else {
            self.cursor = cursor.min(self.lines.len() - 1);
            self.selection_anchor = anchor.map(|row| row.min(self.lines.len() - 1));
            self.scroll = self.scroll.min(self.max_scroll());
            self.ensure_cursor_visible();
        }
        true
    }

    /// Set a view-local status for an asynchronous transcript operation.
    ///
    /// This is deliberately outside [`TranscriptSnapshot`]: loading and
    /// transport failures describe the reader, not a conversation event, and
    /// must never become durable transcript content or model context.
    pub(crate) fn set_activity_status(&mut self, status: Option<String>) {
        self.status = status;
    }

    /// Render every currently projected object in its detailed form for a
    /// portable text artifact. This deliberately exports the typed projection
    /// rather than scraping the visible viewport, so collapsed content and
    /// off-screen rows are not lost.
    pub(crate) fn export_plain_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (index, item) in self.snapshot.items.iter().enumerate() {
            if let Some(text) = item.full_detail_text() {
                let content = if item.kind == TranscriptItemKind::Reasoning {
                    text.strip_prefix("▼ ").unwrap_or(&text)
                } else {
                    &text
                };
                let clean = crate::tui::render::line_utils::sanitize_terminal_text(content);
                lines.extend(clean.split('\n').map(str::to_owned));
            } else if let Some((text, heading)) = item.detail_text() {
                if let Some((heading, name)) = heading {
                    let heading = format!("{name} · {heading}");
                    lines.push(
                        crate::tui::render::line_utils::sanitize_terminal_text(&heading)
                            .into_owned(),
                    );
                }
                let clean = crate::tui::render::line_utils::sanitize_terminal_text(text);
                lines.extend(clean.split('\n').map(str::to_owned));
            } else {
                lines.extend(item.render(self.width, true).iter().map(line_plain_text));
            }
            if index + 1 < self.snapshot.items.len() {
                lines.extend(std::iter::repeat_n(String::new(), item.separator_rows));
            }
        }
        lines
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.max_visible as usize)
    }

    fn is_following_tail(&self) -> bool {
        self.selection_anchor.is_none()
            && (self.lines.is_empty()
                || (self.cursor == self.lines.len().saturating_sub(1)
                    && self.scroll == self.max_scroll()))
    }

    fn selection_holds_live_projection(&self) -> bool {
        self.selection_anchor.is_some()
            && (self.live_tail_dirty
                || self.snapshot.items.iter().any(|item| {
                    matches!(
                        &item.content,
                        TranscriptContent::LiveAssistant(_) | TranscriptContent::LiveReasoning(_)
                    )
                }))
    }

    fn replace_snapshot(&mut self, snapshot: TranscriptSnapshot, width: u16) {
        let freeze_selection = self.selection_holds_live_projection();
        let follow_tail = self.is_following_tail();
        let map_locator = |(id, offset): (TranscriptItemId, usize)| {
            snapshot
                .map_item_id(&self.snapshot, &id)
                .map(|id| (id, offset))
        };
        let cursor_locator = self.row_locator(self.cursor).and_then(map_locator);
        let anchor_locator = self
            .selection_anchor
            .and_then(|anchor| self.row_locator(anchor))
            .and_then(map_locator);
        let expanded = self
            .expanded
            .iter()
            .filter_map(|id| snapshot.map_item_id(&self.snapshot, id))
            .collect();
        if freeze_selection {
            let mut mapped = HashMap::new();
            for id in self.row_items.iter_mut().flatten() {
                let replacement = mapped
                    .entry(id.clone())
                    .or_insert_with(|| snapshot.map_item_id(&self.snapshot, id));
                if let Some(replacement) = replacement {
                    *id = replacement.clone();
                }
            }
        }
        self.snapshot = snapshot;
        self.expanded = expanded;
        self.width = width;
        self.expanded.retain(|id| {
            self.snapshot
                .items
                .iter()
                .find(|item| &item.id == id)
                .is_some_and(TranscriptItem::is_expandable)
        });
        if freeze_selection {
            self.live_tail_dirty = true;
            self.status = Some("View updated · finish selecting to refresh".into());
            return;
        }
        self.rebuild_lines();

        if self.lines.is_empty() {
            self.scroll = 0;
            self.cursor = 0;
            self.selection_anchor = None;
            return;
        }

        if follow_tail {
            self.cursor = self.lines.len() - 1;
            self.scroll = self.max_scroll();
            return;
        }

        let cursor = cursor_locator.and_then(|locator| self.row_for_locator(locator));
        self.cursor = cursor.unwrap_or_else(|| self.cursor.min(self.lines.len() - 1));
        self.scroll = self.scroll.min(self.max_scroll());
        self.selection_anchor =
            cursor.and_then(|_| anchor_locator.and_then(|locator| self.row_for_locator(locator)));
        self.ensure_cursor_visible();
    }

    fn rebuild_lines(&mut self) {
        self.live_tail_dirty = false;
        self.lines.clear();
        self.row_items.clear();
        if !self
            .snapshot
            .items
            .iter()
            .any(|item| self.expanded.contains(&item.id) && item.detail_text().is_some())
        {
            self.activity_cache = None;
        }
        for item in &self.snapshot.items {
            if !item.matches_filter(self.filter) {
                continue;
            }
            let expanded = self.expanded.contains(&item.id);
            let item_lines = if item.detail_text().is_some() && expanded {
                let hit = self
                    .activity_cache
                    .as_ref()
                    .is_some_and(|(id, width, old, _)| {
                        id == &item.id && *width == self.width && item.same_detail_source(old)
                    });
                if !hit {
                    self.activity_cache = Some((
                        item.id.clone(),
                        self.width,
                        item.content.clone(),
                        item.render(self.width, true)
                            .into_iter()
                            .map(Arc::new)
                            .collect(),
                    ));
                }
                self.activity_cache.as_ref().unwrap().3.clone()
            } else {
                item.render(self.width, expanded)
                    .into_iter()
                    .map(Arc::new)
                    .collect()
            };
            if item_lines.is_empty() {
                continue;
            }
            self.row_items
                .extend(std::iter::repeat_n(Some(item.id.clone()), item_lines.len()));
            self.lines.extend(item_lines);
            for _ in 0..item.separator_rows {
                self.lines.push(Arc::new(Line::default()));
                self.row_items.push(None);
            }
        }
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
            self.row_items.pop();
        }
    }

    fn refresh_live_snapshot(&mut self) {
        if !self.live_tail_dirty {
            return;
        }
        let cursor = self.row_locator(self.cursor);
        let anchor = self.selection_anchor.and_then(|row| self.row_locator(row));
        self.rebuild_lines();
        self.cursor = cursor
            .and_then(|at| self.row_for_locator(at))
            .unwrap_or_else(|| self.cursor.min(self.lines.len().saturating_sub(1)));
        self.selection_anchor = anchor.and_then(|at| self.row_for_locator(at));
        self.scroll = self.scroll.min(self.max_scroll());
        self.ensure_cursor_visible();
    }

    fn row_locator(&self, row: usize) -> Option<(TranscriptItemId, usize)> {
        let id = self.item_at_or_near(row)?;
        let offset = self
            .row_items
            .iter()
            .take(row.saturating_add(1))
            .filter(|candidate| candidate.as_ref() == Some(&id))
            .count()
            .saturating_sub(1);
        Some((id, offset))
    }

    fn row_for_locator(&self, (id, offset): (TranscriptItemId, usize)) -> Option<usize> {
        let mut last = None;
        let mut seen = 0;
        for (row, candidate) in self.row_items.iter().enumerate() {
            if candidate.as_ref() == Some(&id) {
                if seen == offset {
                    return Some(row);
                }
                last = Some(row);
                seen += 1;
            }
        }
        last
    }

    fn item_at_or_near(&self, row: usize) -> Option<TranscriptItemId> {
        self.row_items
            .get(row)
            .and_then(Clone::clone)
            .or_else(|| {
                self.row_items
                    .iter()
                    .take(row.saturating_add(1))
                    .rev()
                    .flatten()
                    .next()
                    .cloned()
            })
            .or_else(|| {
                self.row_items
                    .iter()
                    .skip(row.saturating_add(1))
                    .flatten()
                    .next()
                    .cloned()
            })
    }

    fn expand_item(&mut self, id: &TranscriptItemId) {
        if self
            .snapshot
            .items
            .iter()
            .any(|item| item.id == *id && item.detail_text().is_some())
        {
            let items = &self.snapshot.items;
            self.expanded.retain(|other| {
                !items
                    .iter()
                    .any(|item| item.id == *other && item.detail_text().is_some())
            });
        }
        self.expanded.insert(id.clone());
    }

    fn toggle_current_item(&mut self) {
        let Some(id) = self.item_at_or_near(self.cursor) else {
            self.status = Some("Nothing to expand".to_string());
            return;
        };
        let Some(item) = self.snapshot.items.iter().find(|item| item.id == id) else {
            if self.live_tail_dirty && self.selection_anchor.take().is_some() {
                self.refresh_live_snapshot();
                self.status = Some("Selection ended · view updated".into());
                return;
            }
            self.status = Some("Selected item is no longer available".to_string());
            return;
        };
        if !item.is_expandable() {
            self.status = Some("This item has no hidden details".to_string());
            return;
        }
        let label = item.label();
        let ended_selection = self.selection_anchor.take().is_some();
        let expanded = if self.expanded.remove(&id) {
            false
        } else {
            self.expand_item(&id);
            true
        };
        self.rebuild_lines();
        if let Some(row) = self.row_for_locator((id, 0)) {
            self.cursor = row;
        }
        self.ensure_cursor_visible();
        self.status = Some(format!(
            "{} {label}{}",
            if expanded { "Expanded" } else { "Collapsed" },
            if ended_selection {
                " · selection ended"
            } else {
                ""
            }
        ));
    }

    /// Expand the selected content object without turning Right into a toggle.
    /// Returns `true` only when a collapsed expandable object changed state.
    pub(crate) fn expand_current_item(&mut self) -> bool {
        let ended_selection = self.selection_anchor.take().is_some();
        let unavailable_frozen = ended_selection
            && self.live_tail_dirty
            && self
                .item_at_or_near(self.cursor)
                .is_none_or(|id| !self.snapshot.items.iter().any(|item| item.id == id));
        if ended_selection {
            self.status = Some("Selection ended".into());
        }
        self.refresh_live_snapshot();
        if unavailable_frozen {
            self.status = Some("Selection ended · view updated".into());
            return false;
        }
        let Some(id) = self.item_at_or_near(self.cursor) else {
            return false;
        };
        let Some(item) = self.snapshot.items.iter().find(|item| item.id == id) else {
            return false;
        };
        if !item.is_expandable() || self.expanded.contains(&id) {
            return false;
        }
        let label = item.label();
        self.expand_item(&id);
        self.rebuild_lines();
        if let Some(row) = self.row_for_locator((id, 0)) {
            self.move_cursor_to(row);
        }
        self.ensure_cursor_visible();
        self.status = Some(format!(
            "Expanded {label}{}",
            if ended_selection {
                " · selection ended"
            } else {
                ""
            }
        ));
        true
    }

    /// Collapse the selected expanded object before the enclosing view uses
    /// Left as hierarchy navigation. This preserves the familiar tree rule:
    /// Right expands, Left collapses, and another Left goes to the parent.
    pub(crate) fn collapse_current_item(&mut self) -> bool {
        let Some(id) = self.item_at_or_near(self.cursor) else {
            return false;
        };
        if !self.expanded.remove(&id) {
            if self.selection_anchor.is_some()
                && self.live_tail_dirty
                && !self.snapshot.items.iter().any(|item| item.id == id)
            {
                self.selection_anchor = None;
                self.refresh_live_snapshot();
                self.status = Some("Selection ended · view updated".into());
                return true;
            }
            return false;
        }
        let label = self
            .snapshot
            .items
            .iter()
            .find(|item| item.id == id)
            .map(TranscriptItem::label)
            .unwrap_or("item");
        let ended_selection = self.selection_anchor.take().is_some();
        self.rebuild_lines();
        if let Some(row) = self.row_for_locator((id, 0)) {
            self.move_cursor_to(row);
        }
        self.ensure_cursor_visible();
        self.status = Some(format!(
            "Collapsed {label}{}",
            if ended_selection {
                " · selection ended"
            } else {
                ""
            }
        ));
        true
    }

    pub(crate) fn is_search_active(&self) -> bool {
        self.search_input.is_some()
    }

    fn begin_search(&mut self) {
        self.selection_anchor = None;
        self.search_input = Some(self.last_search.clone().unwrap_or_default());
        self.status = None;
    }

    fn cycle_filter(&mut self) {
        let follow_tail = self.is_following_tail();
        self.filter = self.filter.next();
        self.selection_anchor = None;
        self.rebuild_lines();
        if follow_tail {
            self.cursor = self.lines.len().saturating_sub(1);
            self.scroll = self.max_scroll();
        } else {
            self.cursor = self.cursor.min(self.lines.len().saturating_sub(1));
            self.ensure_cursor_visible();
        }
        self.status = Some(format!("Showing {}", self.filter.label()));
    }

    fn append_search_text(&mut self, text: &str) -> bool {
        let Some(query) = self.search_input.as_mut() else {
            return false;
        };
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' || ch.is_control() {
                if !query.ends_with(' ') && !query.is_empty() {
                    query.push(' ');
                }
            } else {
                query.push(ch);
            }
        }
        true
    }

    fn submit_search(&mut self) {
        let Some(query) = self.search_input.take() else {
            return;
        };
        let query = query.trim().to_string();
        if query.is_empty() {
            self.status = Some("Enter search text".to_string());
            self.search_input = Some(String::new());
            return;
        }
        self.last_search = Some(query.clone());
        self.find_search_match(&query, true);
    }

    fn repeat_search(&mut self, forward: bool) {
        let Some(query) = self.last_search.clone() else {
            self.status = Some("Press / to search".to_string());
            return;
        };
        self.find_search_match(&query, forward);
    }

    fn find_search_match(&mut self, query: &str, forward: bool) {
        let needle = query.to_lowercase();
        let locator = self.row_locator(self.cursor);
        let current = locator
            .as_ref()
            .and_then(|(id, offset)| {
                self.snapshot
                    .items
                    .iter()
                    .position(|item| &item.id == id)
                    .map(|index| (index, *offset))
            })
            .unwrap_or((usize::MAX, usize::MAX));
        // Keep only the neighbouring matches. A newline-heavy activity page can
        // contain millions of matching lines; search must not retain them all.
        let mut first = None;
        let mut last = None;
        let mut after = None;
        let mut before = None;
        let mut count = 0usize;
        let mut record = |index: usize, offset: usize| {
            count += 1;
            let candidate = (index, offset, count);
            if first.is_none() {
                first = Some(candidate);
            }
            last = Some(candidate);
            if (index, offset) > current && after.is_none() {
                after = Some(candidate);
            }
            if (index, offset) < current {
                before = Some(candidate);
            }
        };
        for (index, item) in self.snapshot.items.iter().enumerate() {
            if !item.matches_filter(self.filter) {
                continue;
            }
            let full_text = item.full_detail_text();
            let detail = full_text
                .as_deref()
                .map(|text| (text, None))
                .or_else(|| item.detail_text());
            if let Some((text, heading)) = detail {
                let text = crate::tui::render::line_utils::sanitize_terminal_text(text);
                let lower = text.to_lowercase();
                let mut offset = usize::from(heading.is_some());
                if heading.is_some_and(|(label, name)| {
                    crate::tui::render::line_utils::sanitize_terminal_text(&format!(
                        "{name} · {label}"
                    ))
                    .to_lowercase()
                    .contains(&needle)
                }) {
                    record(index, 0);
                }
                for (line, lower_line) in text.lines().zip(lower.lines()) {
                    let width = usize::from(self.width.max(1));
                    if UnicodeWidthStr::width(line) <= width && !line.contains('\t') {
                        if lower_line.contains(&needle) {
                            record(index, offset);
                        }
                        offset = offset.saturating_add(1);
                        continue;
                    }
                    let wrapped =
                        crate::tui::wrapping::word_wrap_lines([Line::from(line.to_owned())], width);
                    if lower_line.contains(&needle) {
                        let mut boundaries = Vec::with_capacity(wrapped.len());
                        let mut consumed = 0usize;
                        let mut lower_consumed = 0usize;
                        for row in &wrapped {
                            let text = line_plain_text(row);
                            if !text.is_empty() {
                                if let Some(start) = line[consumed..].find(&text) {
                                    let end = consumed + start + text.len();
                                    lower_consumed += line[consumed..end].to_lowercase().len();
                                    consumed = end;
                                }
                            }
                            boundaries.push(lower_consumed);
                        }
                        let mut previous_row = None;
                        for (position, _) in lower_line.match_indices(&needle) {
                            let row = boundaries
                                .partition_point(|end| *end <= position)
                                .min(boundaries.len().saturating_sub(1));
                            if previous_row != Some(row) {
                                record(index, offset.saturating_add(row));
                                previous_row = Some(row);
                            }
                        }
                    }
                    offset = offset.saturating_add(wrapped.len().max(1));
                }
            } else {
                for (offset, line) in item.render(self.width, true).iter().enumerate() {
                    if line_plain_text(line).to_lowercase().contains(&needle) {
                        record(index, offset);
                    }
                }
            }
        }
        let selected = if forward {
            after.or(first)
        } else {
            before.or(last)
        };
        let Some((index, offset, ordinal)) = selected else {
            self.status = Some(format!("No matches for /{query}"));
            return;
        };
        let id = self.snapshot.items[index].id.clone();
        if self.snapshot.items[index].is_expandable() {
            self.expand_item(&id);
            self.rebuild_lines();
        }
        if let Some(row) = self.row_for_locator((id, offset)) {
            self.move_cursor_to(row);
        }
        self.status = Some(format!("Match {ordinal}/{count} · /{query}"));
    }

    fn selection_bounds(&self) -> Option<(usize, usize)> {
        let anchor = self.selection_anchor?;
        Some((anchor.min(self.cursor), anchor.max(self.cursor)))
    }

    fn is_selected_row(&self, index: usize) -> bool {
        self.selection_bounds()
            .is_some_and(|(start, end)| (start..=end).contains(&index))
    }

    fn ensure_cursor_visible(&mut self) {
        let max_scroll = self.max_scroll();
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + self.max_visible as usize {
            self.scroll = self
                .cursor
                .saturating_add(1)
                .saturating_sub(self.max_visible as usize)
                .min(max_scroll);
        }
    }

    fn move_cursor_to(&mut self, cursor: usize) {
        self.cursor = cursor.min(self.lines.len().saturating_sub(1));
        self.ensure_cursor_visible();
    }

    fn selected_text(&self) -> String {
        if self.lines.is_empty() {
            return String::new();
        }
        let (start, end) = self
            .selection_bounds()
            .unwrap_or((self.cursor, self.cursor));
        let Some(selected) = self.lines.get(start..=end) else {
            return String::new();
        };
        selected
            .iter()
            .map(|line| line_plain_text(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn queue_selection_copy(&mut self) {
        let text = self.selected_text();
        if text.is_empty() {
            self.status = Some("Nothing to copy".to_string());
            return;
        }
        let line_count = text.lines().count();
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::CopyToClipboard {
                text,
                success_message: format!("Copied {line_count} line(s) to clipboard"),
            },
            disposition: ViewActionDisposition::KeepOpen,
        });
        self.status = Some("Copy queued".to_string());
    }

    fn return_to_conversation_navigator(&mut self) {
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::ReturnToConversationNavigator,
            disposition: ViewActionDisposition::KeepOpen,
        });
    }
}

fn line_plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

impl BottomPaneView for TranscriptView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width < 10 || area.height < 3 {
            return;
        }

        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(theme.dim);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let cursor_style = Style::default().bg(theme.selected_bg);
        let selection_style = Style::default()
            .fg(theme.selected_fg)
            .bg(theme.selected_bg)
            .add_modifier(Modifier::BOLD);
        let mut y = area.y;
        let bottom = area.bottom();

        // Helper: advance `y` by 1 with saturating add. Without this,
        // a child Rect placed near `u16::MAX` (deeply nested overlay /
        // tiled layout edge) would wrap to 0 and start drawing rows at
        // the top of the buffer.
        let next_y = |y: u16| y.saturating_add(1).min(bottom);

        // Title
        if y < bottom {
            Widget::render(
                Line::from(vec![
                    Span::styled(format!("  {}", self.title), bold),
                    Span::styled(
                        format!(
                            "  ({} items · {} lines · filter: {})",
                            self.snapshot.items.len(),
                            self.lines.len(),
                            self.filter.label(),
                        ),
                        dim,
                    ),
                ]),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y = next_y(y);
        }

        // Content. The workspace is normally fitted before render, but a
        // terminal can shrink between layout and draw. Derive a safe render
        // window from the actual rectangle so a tail-following conversation
        // never shows an older middle slice for one frame (or indefinitely in
        // embedders that do not call `fit_workspace`).
        let max_visible = usize::from(
            self.max_visible
                .min(area.height.saturating_sub(CHROME_LINES).max(1)),
        );
        let actual_max_scroll = self.lines.len().saturating_sub(max_visible);
        let render_scroll = if self.is_following_tail() {
            actual_max_scroll
        } else if self.cursor < self.scroll {
            self.cursor.min(actual_max_scroll)
        } else if self.cursor >= self.scroll.saturating_add(max_visible) {
            self.cursor
                .saturating_add(1)
                .saturating_sub(max_visible)
                .min(actual_max_scroll)
        } else {
            self.scroll.min(actual_max_scroll)
        };
        let visible_end = (render_scroll + max_visible).min(self.lines.len());
        if self.lines.is_empty() && y < bottom {
            let message = if self.snapshot.items.is_empty() {
                "  No conversation yet."
            } else if self.filter != TranscriptFilter::All {
                "  No transcript items match the active filter."
            } else {
                "  This run has no displayable transcript content yet."
            };
            Widget::render(
                Line::from(Span::styled(message, dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y = next_y(y);
        }
        for i in render_scroll..visible_end {
            if y >= bottom {
                break;
            }
            let mut line = self.lines[i].as_ref().clone();
            if self.is_selected_row(i) {
                line.style = selection_style;
            } else if i == self.cursor {
                line.style = cursor_style;
            }
            Widget::render(
                crate::tui::render::line_utils::FullRowParagraph::new(vec![line]),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y = next_y(y);
        }

        // Scroll indicator
        if self.lines.len() > max_visible && y < bottom {
            Widget::render(
                Line::from(Span::styled(
                    format!(
                        "  ({}-{} of {})",
                        render_scroll + 1,
                        visible_end,
                        self.lines.len()
                    ),
                    dim,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y = next_y(y);
        }

        // Hint
        if y < bottom {
            y = next_y(y);
        }
        if y < bottom {
            let hint = self
                .search_input
                .as_ref()
                .map(|query| format!("  Search: /{query}  · Enter find · Esc cancel"))
                .unwrap_or_else(|| {
                    self.status.clone().unwrap_or_else(|| {
                        "  ↑↓ move  → expand  ← back  Enter/Ctrl+E details  / search  n/N repeat  F filter  Ctrl+G conversations  Shift+←/→ switch  V select  Y copy"
                            .to_string()
                    })
                });
            Widget::render(
                Line::from(Span::styled(hint, dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let title_h = 1;
        let content_h = (self.lines.len() as u16).min(self.max_visible).max(1);
        let scroll_h = if self.lines.len() as u16 > self.max_visible {
            1
        } else {
            0
        };
        let hint_h = 2;
        title_h + content_h + scroll_h + hint_h
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.search_input.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.search_input = None;
                    self.status = Some("Search cancelled".to_string());
                }
                KeyCode::Enter => self.submit_search(),
                KeyCode::Backspace => {
                    if let Some(query) = self.search_input.as_mut() {
                        query.pop();
                    }
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(query) = self.search_input.as_mut() {
                        query.clear();
                    }
                }
                KeyCode::Char(ch)
                    if !key.modifiers.intersects(
                        KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER,
                    ) =>
                {
                    self.append_search_text(&ch.to_string());
                }
                _ => {}
            }
            return;
        }

        if self.live_tail_dirty && key.code == KeyCode::End && self.selection_anchor.is_none() {
            if let Some(item) = self.snapshot.items.last()
                && matches!(
                    &item.content,
                    TranscriptContent::LiveAssistant(_) | TranscriptContent::LiveReasoning(_)
                )
            {
                self.expanded.remove(&item.id);
                if let TranscriptContent::LiveAssistant(cell) = &item.content {
                    recover_mutex_lock(cell).refresh_live_viewport();
                }
            }
            self.rebuild_lines();
            self.cursor = self.lines.len().saturating_sub(1);
            self.scroll = self.max_scroll();
            self.status = None;
            return;
        }
        if self.live_tail_dirty
            && self.selection_anchor.is_none()
            && matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::Char('j' | 'k')
            )
        {
            self.refresh_live_snapshot();
        }

        let max_visible = self.max_visible as usize;
        let max_scroll = self.max_scroll();
        match key.code {
            KeyCode::Esc => self.return_to_conversation_navigator(),
            KeyCode::Left if !self.collapse_current_item() => {
                self.return_to_conversation_navigator()
            }
            KeyCode::Right => {
                self.expand_current_item();
            }
            KeyCode::Char('/') => self.begin_search(),
            KeyCode::Char('f' | 'F') => self.cycle_filter(),
            KeyCode::Char('n') => self.repeat_search(true),
            KeyCode::Char('N') => self.repeat_search(false),
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_cursor_to(self.cursor.saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_cursor_to((self.cursor + 1).min(self.lines.len().saturating_sub(1)));
            }
            KeyCode::PageUp => {
                self.move_cursor_to(self.cursor.saturating_sub(max_visible));
            }
            KeyCode::PageDown => {
                self.move_cursor_to(
                    (self.cursor + max_visible).min(self.lines.len().saturating_sub(1)),
                );
            }
            KeyCode::Home => self.move_cursor_to(0),
            KeyCode::End => self.move_cursor_to(self.lines.len().saturating_sub(1)),
            KeyCode::Enter => self.toggle_current_item(),
            KeyCode::Char('e') if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.toggle_current_item();
            }
            KeyCode::Char('v') => {
                self.selection_anchor = match self.selection_anchor {
                    Some(_) => None,
                    None => Some(self.cursor),
                };
                self.status = None;
            }
            KeyCode::Char('y') | KeyCode::Char('c') => {
                self.queue_selection_copy();
            }
            _ => {}
        }
        if matches!(
            key.code,
            KeyCode::Up
                | KeyCode::Char('k')
                | KeyCode::Down
                | KeyCode::Char('j')
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Home
                | KeyCode::End
        ) {
            self.status = None;
            self.scroll = self.scroll.min(max_scroll);
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let query = self.search_input.as_deref()?;
        let column = "  Search: /".width() + query.width();
        Some((
            area.x
                + u16::try_from(column)
                    .unwrap_or(u16::MAX)
                    .min(area.width.saturating_sub(1)),
            area.bottom().saturating_sub(1),
        ))
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        if self.search_input.take().is_some() {
            self.status = Some("Search cancelled".to_string());
            return CancellationEvent::Consumed;
        }
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if self.completed {
            Some(ViewCompletion {
                result: None,
                reopen: None,
            })
        } else {
            None
        }
    }

    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action.take()
    }

    fn refresh_transcript_snapshot(&mut self, snapshot: TranscriptSnapshot, width: u16) -> bool {
        self.replace_snapshot(snapshot, width);
        true
    }

    fn uses_local_root_transcript_snapshot(&self) -> bool {
        true
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        self.append_search_text(text)
    }

    fn is_transcript_view(&self) -> bool {
        true
    }

    fn is_root_transcript_view(&self) -> bool {
        true
    }

    fn conversation_tab_id(&self) -> Option<ConversationTabId> {
        Some(ConversationTabId::Root)
    }

    fn fit_conversation_workspace(&mut self, terminal_height: u16, width: u16) {
        self.fit_workspace(terminal_height, width);
    }
}

#[cfg(test)]
mod tests {
    use super::super::view::{
        BottomPaneView, BottomPaneViewAction, ViewActionDisposition, ViewActionRequest,
    };
    use super::{
        DEFAULT_VISIBLE_LINES, MIN_VISIBLE_LINES, TranscriptItem, TranscriptItemId,
        TranscriptItemKind, TranscriptSnapshot, TranscriptView,
    };
    use crate::tui::history_cell::{reasoning::ReasoningCell, tool::ToolCell};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{buffer::Buffer, layout::Rect, text::Line};

    fn lines(n: usize) -> Vec<Line<'static>> {
        (0..n).map(|i| Line::from(format!("line {i}"))).collect()
    }

    fn plain_snapshot(lines: Vec<Line<'static>>) -> TranscriptSnapshot {
        TranscriptSnapshot::new(vec![TranscriptItem::rendered(
            TranscriptItemId::from_widget_id(1),
            lines,
            0,
        )])
    }

    fn plain_view(lines: Vec<Line<'static>>, terminal_height: u16) -> TranscriptView {
        TranscriptView::from_snapshot(plain_snapshot(lines), terminal_height, 80)
    }

    fn rendered(view: &TranscriptView) -> String {
        view.lines
            .iter()
            .map(|line| super::line_plain_text(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn reasoning_snapshot(id: usize, text: &str) -> TranscriptSnapshot {
        TranscriptSnapshot::new(vec![TranscriptItem::reasoning(
            TranscriptItemId::from_widget_id(id as u64),
            ReasoningCell::from_text(text, Some(1_200)),
            0,
        )])
    }

    #[test]
    fn expanded_activity_cache_tracks_text_width_and_removal() {
        let snapshot = |text: String| {
            TranscriptSnapshot::new(vec![TranscriptItem::activity(
                TranscriptItemId::from_widget_id(8),
                TranscriptItemKind::Tool,
                text,
            )])
        };
        let mut view = TranscriptView::from_snapshot(snapshot("A".repeat(100)), 24, 80);
        view.expand_item(&TranscriptItemId::from_widget_id(8));
        view.replace_snapshot(snapshot("A".repeat(100)), 80);
        assert_eq!(view.lines.len(), 2);
        view.replace_snapshot(snapshot("B".repeat(100)), 10);
        assert_eq!(view.lines.len(), 10);
        assert!(
            view.lines
                .iter()
                .all(|line| !super::line_plain_text(line).contains('A'))
        );
        view.replace_snapshot(TranscriptSnapshot::new(vec![]), 10);
        assert!(view.lines.is_empty());
        assert!(view.activity_cache.is_none());
    }

    #[test]
    fn activity_search_finds_matches_across_wrapped_lines() {
        let text = format!("{}needle suffix", "x".repeat(78));
        let item = TranscriptItem::activity(
            TranscriptItemId::from_widget_id(1),
            TranscriptItemKind::Tool,
            text,
        );
        let mut view = TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item]), 24, 80);
        view.find_search_match("needle", true);
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.starts_with("Match 1/1")),
            "{:?}",
            view.status
        );
        assert!(view.expanded.contains(&TranscriptItemId::from_widget_id(1)));
    }

    #[test]
    fn activity_search_tracks_word_wrap_and_repeated_matches() {
        let item = TranscriptItem::activity(
            TranscriptItemId::from_widget_id(1),
            TranscriptItemKind::Tool,
            "one two three TARGET".into(),
        );
        let mut view = TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item]), 24, 10);
        view.find_search_match("target", true);
        assert!(
            super::line_plain_text(&view.lines[view.cursor]).contains("TARGET"),
            "{:?}",
            view.lines[view.cursor]
        );
        let item = TranscriptItem::activity(
            TranscriptItemId::from_widget_id(2),
            TranscriptItemKind::Tool,
            "needle----needle".into(),
        );
        let mut view = TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item]), 24, 8);
        view.find_search_match("needle", true);
        assert!(
            view.status
                .as_deref()
                .is_some_and(|s| s.starts_with("Match 1/2")),
            "{:?}",
            view.status
        );
        view.find_search_match("needle", true);
        assert!(
            view.status
                .as_deref()
                .is_some_and(|s| s.starts_with("Match 2/2")),
            "{:?}",
            view.status
        );
    }

    #[test]
    fn wrapped_activity_preview_always_indicates_omission() {
        let text = format!(
            "heading\n{}",
            (0..4)
                .map(|_| "x".repeat(81))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let item = TranscriptItem::activity(
            TranscriptItemId::from_widget_id(1),
            TranscriptItemKind::Tool,
            text,
        );
        let view = TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item]), 24, 80);
        assert!(rendered(&view).contains("expand for full activity"));
    }

    #[test]
    fn activity_search_over_newline_heavy_window_keeps_one_item_expanded() {
        let text = "x\n".repeat(10_000);
        let items = (0..128)
            .map(|index| {
                TranscriptItem::activity(
                    TranscriptItemId::from_widget_id(index),
                    TranscriptItemKind::Tool,
                    text.clone(),
                )
            })
            .collect();
        let snapshot = TranscriptSnapshot::new(items);
        let mut view = TranscriptView::from_snapshot(snapshot.clone(), 24, 80);
        view.find_search_match("x", true);
        assert!(
            view.status
                .as_deref()
                .is_some_and(|status| status.contains("/1280000")),
            "{:?}",
            view.status
        );
        view.find_search_match("x", true);
        view.replace_snapshot(snapshot, 80);
        assert!(view.lines.len() <= 10_000 + 128 * 6, "{}", view.lines.len());
    }

    #[test]
    fn recorded_activity_preview_is_bounded_and_full_text_is_expandable() {
        let text = format!("{}\n{}", "x".repeat(2000), "\n".repeat(16_000));
        let item = TranscriptItem::activity(
            TranscriptItemId::from_widget_id(1),
            TranscriptItemKind::Tool,
            text.clone(),
        );
        let snapshot = TranscriptSnapshot::new((0..128).map(|_| item.clone()).collect());
        let mut view = TranscriptView::from_snapshot(snapshot.clone(), 24, 80);
        assert!(view.lines.len() <= 128 * 6, "{}", view.lines.len());
        view.replace_snapshot(snapshot, 80);
        assert!(view.lines.len() <= 128 * 6);
        let full = item
            .render(80, true)
            .iter()
            .map(super::line_plain_text)
            .collect::<String>();
        assert!(full.contains(&"x".repeat(2000)));
        let single =
            TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item.clone()]), 24, 80);
        assert_eq!(single.export_plain_lines()[0], "x".repeat(2000));
        let narrow = TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item]), 24, 1);
        assert!(narrow.lines.len() <= 6);
    }

    #[test]
    fn tool_request_search_expands_shared_sanitized_details() {
        let description = format!("\x1b[31m{}TAIL-MARKER\x1b[0m", "中".repeat(180));
        let tool = TranscriptItem::tool(
            TranscriptItemId::from_widget_id(77),
            ToolCell::new_running("read_file", description),
            1,
        );
        let request = tool.tool_request().unwrap();
        let same = tool.tool_request().unwrap();
        assert!(request.same_detail_source(&same.content));
        let mut view =
            TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![request]), 24, 80);
        assert!(!rendered(&view).contains("TAIL-MARKER"));
        view.find_search_match("31m", true);
        assert!(
            view.status
                .as_deref()
                .is_some_and(|s| s.starts_with("No matches"))
        );
        view.find_search_match("TAIL-MARKER", true);
        let visible = rendered(&view);
        assert!(visible.contains("TAIL-MARKER"), "{visible}");
        assert!(!visible.contains('\x1b'));
        let cached_line = view.lines[1].clone();
        view.replace_snapshot(view.snapshot.clone(), 80);
        assert!(std::sync::Arc::ptr_eq(&cached_line, &view.lines[1]));
        let exported = view.export_plain_lines().join("\n");
        assert!(exported.contains("TAIL-MARKER") && !exported.contains('\x1b'));
    }

    #[test]
    fn snapshot_removal_preserves_or_clears_selection_by_identity() {
        let item = |id, label: &'static str| {
            TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(id),
                vec![Line::from(label)],
                0,
            )
        };
        let mut view = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(vec![item(1, "first"), item(2, "second"), item(3, "third")]),
            24,
            80,
        );
        view.cursor = 1;
        view.selection_anchor = Some(1);
        view.replace_snapshot(
            TranscriptSnapshot::new(vec![item(2, "second"), item(3, "third")]),
            80,
        );
        assert_eq!(view.selection_anchor, Some(0));
        view.replace_snapshot(TranscriptSnapshot::new(vec![item(3, "third")]), 80);
        assert_eq!(view.selection_anchor, None);

        let mut view = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(vec![item(1, "first"), item(2, "second"), item(3, "third")]),
            24,
            80,
        );
        view.cursor = 1;
        view.selection_anchor = Some(0);
        view.replace_snapshot(
            TranscriptSnapshot::new(vec![item(1, "first"), item(3, "third")]),
            80,
        );
        assert_eq!(view.selection_anchor, None);
    }

    #[test]
    fn tall_terminal_scales_visible_window() {
        // 50-row terminal → 80% budget = 40 → minus chrome (4) = 36
        let v = plain_view(lines(100), 50);
        assert_eq!(v.max_visible, 36);
    }

    #[test]
    fn focused_conversation_uses_the_primary_terminal_canvas() {
        let mut v = plain_view(lines(100), 50);
        v.fit_workspace(50, 80);

        // A focused root/agent conversation does not reserve the old
        // composer-sized overlay budget: all rows except transcript chrome
        // are available to the selected conversation.
        assert_eq!(v.max_visible, 46);
        assert_eq!(v.cursor, 99);
        assert_eq!(v.scroll + v.max_visible as usize, 100);
    }

    #[test]
    fn short_terminal_clamps_to_minimum() {
        // 10-row terminal → 80% = 8, minus chrome (4) = 4, clamped up to MIN (8).
        let v = plain_view(lines(100), 10);
        assert_eq!(v.max_visible, MIN_VISIBLE_LINES);
    }

    #[test]
    fn zero_height_falls_back_to_default() {
        // Caller didn't know terminal height (headless/test).
        let v = plain_view(lines(100), 0);
        assert_eq!(v.max_visible, DEFAULT_VISIBLE_LINES);
    }

    #[test]
    fn initial_scroll_shows_tail() {
        // Opening the view should land at the bottom (latest content),
        // not at the top.
        let v = plain_view(lines(100), 50);
        let visible_end = v.scroll + v.max_visible as usize;
        assert_eq!(visible_end, 100, "initial view must show the last line");
        assert_eq!(v.cursor, 99);
    }

    #[test]
    fn render_uses_actual_smaller_area_while_following_tail() {
        // Layout and draw can observe different terminal sizes. A view built
        // for the old, tall terminal must still show the newest conversation
        // lines when the draw area has already shrunk.
        let view = plain_view(lines(100), 120);
        let area = Rect::new(0, 0, 80, 20);
        let mut buffer = Buffer::empty(area);

        view.render(area, &mut buffer);

        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("line 99"), "{text}");
        assert!(!text.contains("line 7\n"), "{text}");
        assert!(text.contains("(85-100 of 100)"), "{text}");
    }

    #[test]
    fn short_history_has_zero_scroll() {
        // Fewer lines than the window → nothing to scroll, view anchors
        // at the top.
        let v = plain_view(lines(5), 50);
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn pgdn_respects_dynamic_window() {
        // PageDown pages by max_visible, not by a fixed 16.
        let mut v = plain_view(lines(200), 50);
        v.move_cursor_to(0);
        v.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(v.cursor, v.max_visible as usize);
    }

    #[test]
    fn selection_queues_full_range_text_for_async_clipboard() {
        let mut v = plain_view(lines(6), 0);
        v.move_cursor_to(1);
        v.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        v.move_cursor_to(3);

        v.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        let request = v.take_action_request().expect("clipboard action");
        assert!(matches!(
            request.action,
            BottomPaneViewAction::CopyToClipboard {
                text,
                success_message,
            } if text == "line 1\nline 2\nline 3"
                && success_message == "Copied 3 line(s) to clipboard"
        ));
        assert_eq!(v.status.as_deref(), Some("Copy queued"));
    }

    #[test]
    fn export_uses_full_projection_including_collapsed_details() {
        let view = TranscriptView::from_snapshot(
            reasoning_snapshot(3, "inspect state\ncompare evidence\nchoose next step"),
            30,
            80,
        );

        let exported = view.export_plain_lines().join("\n");
        assert!(exported.contains("compare evidence"), "{exported}");
        assert!(exported.contains("choose next step"), "{exported}");
    }

    #[test]
    fn typed_filter_cycles_without_hiding_content_from_export() {
        let snapshot = TranscriptSnapshot::new(vec![
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(1),
                TranscriptItemKind::User,
                vec![Line::from("user question")],
                0,
            ),
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(2),
                TranscriptItemKind::Assistant,
                vec![Line::from("assistant answer")],
                0,
            ),
            TranscriptItem::reasoning(
                TranscriptItemId::from_widget_id(3),
                ReasoningCell::from_text("private reasoning", Some(10)),
                0,
            ),
            TranscriptItem::tool(
                TranscriptItemId::from_widget_id(4),
                ToolCell::new_running("read_file", "src/lib.rs"),
                0,
            ),
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(5),
                TranscriptItemKind::Agent,
                vec![Line::from("agent message")],
                0,
            ),
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(6),
                TranscriptItemKind::System,
                vec![Line::from("system notice")],
                0,
            ),
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(7),
                TranscriptItemKind::Error,
                vec![Line::from("terminal error")],
                0,
            ),
        ]);
        let mut view = TranscriptView::from_snapshot(snapshot, 30, 80);
        let complete_export = view.export_plain_lines().join("\n");

        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        let conversation = rendered(&view);
        assert!(conversation.contains("user question"), "{conversation}");
        assert!(conversation.contains("assistant answer"), "{conversation}");
        assert!(!conversation.contains("Thought"), "{conversation}");
        assert!(!conversation.contains("read_file"), "{conversation}");
        assert!(!conversation.contains("agent message"), "{conversation}");
        assert!(!conversation.contains("system notice"), "{conversation}");
        assert!(!conversation.contains("terminal error"), "{conversation}");

        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(rendered(&view), "user question");
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(rendered(&view), "assistant answer");
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(rendered(&view).contains("Thought"));
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(rendered(&view).contains("src/lib.rs"));
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(rendered(&view), "agent message");
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(rendered(&view), "system notice");
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert_eq!(rendered(&view), "terminal error");
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(rendered(&view).contains("user question"));
        assert!(rendered(&view).contains("terminal error"));

        assert!(complete_export.contains("user question"));
        assert!(complete_export.contains("assistant answer"));
        assert!(complete_export.contains("private reasoning"));
        assert!(complete_export.contains("src/lib.rs"));
        assert!(complete_export.contains("agent message"));
        assert!(complete_export.contains("system notice"));
        assert!(complete_export.contains("terminal error"));
    }

    #[test]
    fn copy_without_selection_queues_cursor_line() {
        let mut v = plain_view(lines(6), 0);
        v.move_cursor_to(4);

        v.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

        assert!(matches!(
            v.take_action_request().map(|request| request.action),
            Some(BottomPaneViewAction::CopyToClipboard { text, .. }) if text == "line 4"
        ));
    }

    #[test]
    fn live_refresh_follows_new_tail_when_user_is_at_tail() {
        let mut v = plain_view(lines(30), 0);

        v.replace_snapshot(plain_snapshot(lines(35)), 80);

        assert_eq!(v.cursor, 34);
        assert_eq!(v.scroll, v.max_scroll());
    }

    #[test]
    fn live_refresh_preserves_manual_position() {
        let mut v = plain_view(lines(30), 0);
        v.move_cursor_to(5);
        let scroll = v.scroll;

        v.replace_snapshot(plain_snapshot(lines(35)), 80);

        assert_eq!(v.cursor, 5);
        assert_eq!(v.scroll, scroll);
    }

    #[test]
    fn empty_transcript_can_receive_its_first_live_lines() {
        let mut v = plain_view(Vec::new(), 0);

        v.replace_snapshot(plain_snapshot(lines(2)), 80);

        assert_eq!(v.cursor, 1);
        assert_eq!(v.scroll, 0);
        assert_eq!(v.lines.len(), 2);
    }

    #[test]
    fn settled_reasoning_expands_and_collapses_as_one_selected_object() {
        let mut view = TranscriptView::from_snapshot(
            reasoning_snapshot(3, "inspect state\ncompare evidence\nchoose next step"),
            30,
            80,
        );
        assert!(rendered(&view).contains("▶ Thought"));
        assert!(!rendered(&view).contains("compare evidence"));

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rendered(&view).contains("▼ Thought"));
        assert!(rendered(&view).contains("compare evidence"));
        assert_eq!(view.status.as_deref(), Some("Expanded reasoning"));

        view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(rendered(&view).contains("▶ Thought"));
        assert!(!rendered(&view).contains("compare evidence"));
        assert_eq!(view.status.as_deref(), Some("Collapsed reasoning"));
    }

    #[test]
    fn ctrl_e_toggles_only_the_selected_expandable_item() {
        let snapshot = TranscriptSnapshot::new(vec![
            TranscriptItem::reasoning(
                TranscriptItemId::from_widget_id(1),
                ReasoningCell::from_text("first reasoning detail", None),
                1,
            ),
            TranscriptItem::reasoning(
                TranscriptItemId::from_widget_id(2),
                ReasoningCell::from_text("second reasoning detail", None),
                1,
            ),
        ]);
        let mut view = TranscriptView::from_snapshot(snapshot, 30, 80);

        view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(
            view.expanded,
            [TranscriptItemId::from_widget_id(1)].into_iter().collect()
        );
        assert_eq!(view.status.as_deref(), Some("Expanded reasoning"));

        view.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(
            view.expanded,
            [
                TranscriptItemId::from_widget_id(1),
                TranscriptItemId::from_widget_id(2)
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(view.status.as_deref(), Some("Expanded reasoning"));
    }

    #[test]
    fn live_reasoning_stays_bounded_until_user_expands_it() {
        let mut reasoning = ReasoningCell::new_streaming();
        reasoning.push_delta("one\ntwo\nthree\nfour\nfive\nsix\nseven");
        let snapshot = TranscriptSnapshot::new(vec![TranscriptItem::reasoning(
            TranscriptItemId::from_widget_id(1),
            reasoning,
            0,
        )]);
        let mut view = TranscriptView::from_snapshot(snapshot, 30, 80);

        assert_eq!(view.lines.len(), 5, "header plus four-row live preview");
        assert!(rendered(&view).contains("earlier lines"));
        assert!(!rendered(&view).contains("one"));

        view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let expanded = rendered(&view);
        assert!(expanded.contains("one"));
        assert!(expanded.contains("seven"));
        assert_eq!(view.lines.len(), 8, "header plus every reasoning row");
    }

    #[test]
    fn arrows_expand_then_collapse_before_returning_to_the_navigator() {
        let mut view =
            TranscriptView::from_snapshot(reasoning_snapshot(5, "first version"), 30, 80);

        view.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(view.expanded.contains(&TranscriptItemId::from_widget_id(5)));
        assert!(!view.is_complete());

        view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!view.expanded.contains(&TranscriptItemId::from_widget_id(5)));
        assert!(!view.is_complete());

        view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!view.is_complete());
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ReturnToConversationNavigator,
                disposition: ViewActionDisposition::KeepOpen,
            })
        ));
    }

    #[test]
    fn tool_expansion_reveals_raw_result_without_changing_compact_summary() {
        let mut tool = ToolCell::new_running("bash", "$ inspect");
        tool.complete(
            "completed",
            25,
            String::new(),
            Some("8 records captured".to_string()),
            Some(
                (1..=8)
                    .map(|n| format!("record {n}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        );
        let snapshot = TranscriptSnapshot::new(vec![TranscriptItem::tool(
            TranscriptItemId::from_widget_id(1),
            tool,
            0,
        )]);
        let mut view = TranscriptView::from_snapshot(snapshot, 30, 80);

        assert!(rendered(&view).contains("8 records captured"));
        assert!(!rendered(&view).contains("record 8"));

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rendered(&view).contains("record 1"));
        assert!(rendered(&view).contains("record 8"));
        assert!(!rendered(&view).contains("8 records captured"));
    }

    #[test]
    fn expansion_survives_live_refresh_by_stable_item_identity() {
        let mut view =
            TranscriptView::from_snapshot(reasoning_snapshot(5, "first version"), 30, 80);
        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        view.replace_snapshot(reasoning_snapshot(5, "updated version"), 80);

        assert!(view.expanded.contains(&TranscriptItemId::from_widget_id(5)));
        assert!(rendered(&view).contains("updated version"));
    }

    #[test]
    fn canonical_identity_preserves_exact_object_across_prepend_and_refresh() {
        let target = TranscriptItemId::from_canonical("event:turn-42", "reasoning");
        let snapshot = TranscriptSnapshot::new(vec![
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_canonical("event:turn-41", "content"),
                TranscriptItemKind::Assistant,
                vec![Line::from("older answer")],
                1,
            ),
            TranscriptItem::reasoning(
                target.clone(),
                ReasoningCell::from_text("inspect exact durable object", None),
                1,
            ),
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_canonical("event:turn-42", "content"),
                TranscriptItemKind::Assistant,
                vec![Line::from("target answer")],
                0,
            ),
        ]);
        let mut view = TranscriptView::from_snapshot(snapshot, 30, 80);
        let target_row = view
            .row_for_locator((target.clone(), 0))
            .expect("target reasoning row");
        view.move_cursor_to(target_row);
        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));

        assert_eq!(
            view.row_locator(view.cursor).map(|locator| locator.0),
            Some(target.clone())
        );
        assert!(view.expanded.contains(&target));

        view.replace_snapshot(
            TranscriptSnapshot::new(vec![
                TranscriptItem::rendered_kind(
                    TranscriptItemId::from_canonical("event:turn-40", "content"),
                    TranscriptItemKind::User,
                    vec![Line::from("newly paged older question")],
                    1,
                ),
                TranscriptItem::rendered_kind(
                    TranscriptItemId::from_canonical("event:turn-41", "content"),
                    TranscriptItemKind::Assistant,
                    vec![Line::from("older answer")],
                    1,
                ),
                TranscriptItem::reasoning(
                    target.clone(),
                    ReasoningCell::from_text("updated exact durable object", None),
                    1,
                ),
                TranscriptItem::rendered_kind(
                    TranscriptItemId::from_canonical("event:turn-42", "content"),
                    TranscriptItemKind::Assistant,
                    vec![Line::from("updated target answer")],
                    0,
                ),
            ]),
            80,
        );

        assert_eq!(
            view.row_locator(view.cursor).map(|locator| locator.0),
            Some(target.clone())
        );
        assert_eq!(
            view.selection_anchor
                .and_then(|row| view.row_locator(row))
                .map(|locator| locator.0),
            Some(target.clone())
        );
        assert!(view.expanded.contains(&target));
        assert!(rendered(&view).contains("updated exact durable object"));
    }

    #[test]
    fn new_transcript_view_defaults_to_collapsed_after_resume_projection() {
        let snapshot = reasoning_snapshot(5, "restored reasoning body");
        let mut prior_view = TranscriptView::from_snapshot(snapshot.clone(), 30, 80);
        prior_view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(rendered(&prior_view).contains("restored reasoning body"));

        let resumed_view = TranscriptView::from_snapshot(snapshot, 30, 80);

        assert!(!rendered(&resumed_view).contains("restored reasoning body"));
        assert!(resumed_view.expanded.is_empty());
    }

    #[test]
    fn non_expandable_selection_reports_why_the_action_did_nothing() {
        let mut view = plain_view(vec![Line::from("plain assistant reply")], 0);

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert_eq!(
            view.status.as_deref(),
            Some("This item has no hidden details")
        );
        assert_eq!(rendered(&view), "plain assistant reply");
    }

    #[test]
    fn existing_but_empty_live_object_is_not_mislabeled_as_a_new_session() {
        let snapshot = TranscriptSnapshot::new(vec![TranscriptItem::reasoning(
            TranscriptItemId::from_widget_id(1),
            ReasoningCell::new_streaming(),
            0,
        )]);
        let view = TranscriptView::from_snapshot(snapshot, 20, 80);
        let area = ratatui::layout::Rect::new(0, 0, 80, 12);
        let mut buffer = ratatui::buffer::Buffer::empty(area);

        view.render(area, &mut buffer);

        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("no displayable transcript content yet"));
        assert!(!text.contains("No conversation yet"));
    }

    fn type_search(view: &mut TranscriptView, query: &str) {
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for ch in query.chars() {
            view.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    }

    #[test]
    fn search_expands_collapsed_reasoning_and_locates_hidden_evidence() {
        let mut view = TranscriptView::from_snapshot(
            reasoning_snapshot(
                7,
                "inspect state\ncompare evidence\nneedle from hidden reasoning\nchoose next step",
            ),
            30,
            80,
        );
        assert!(!rendered(&view).contains("needle from hidden reasoning"));

        type_search(&mut view, "hidden reasoning");

        assert!(rendered(&view).contains("needle from hidden reasoning"));
        assert!(super::line_plain_text(&view.lines[view.cursor]).contains("hidden reasoning"));
        assert_eq!(
            view.status.as_deref(),
            Some("Match 1/1 · /hidden reasoning")
        );
    }

    #[test]
    fn repeated_search_wraps_in_both_directions() {
        let mut view = plain_view(
            vec![
                Line::from("start"),
                Line::from("needle first"),
                Line::from("middle"),
                Line::from("needle second"),
                Line::from("tail"),
            ],
            30,
        );

        type_search(&mut view, "needle");
        assert_eq!(view.cursor, 1);
        view.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(view.cursor, 3);
        view.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(view.cursor, 1);
        view.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT));
        assert_eq!(view.cursor, 3);
    }

    #[test]
    fn search_escape_and_paste_stay_in_the_transcript_surface() {
        let mut view = plain_view(vec![Line::from("pasted query target")], 20);
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        assert!(view.handle_paste("pasted\nquery"));
        assert_eq!(view.search_input.as_deref(), Some("pasted query"));
        assert!(view.cursor_pos(Rect::new(0, 0, 80, 10)).is_some());

        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(!view.completed);
        assert!(view.search_input.is_none());
        assert_eq!(view.status.as_deref(), Some("Search cancelled"));
    }

    #[test]
    fn bottom_pane_routes_search_paste_to_transcript_not_hidden_composer() {
        let mut pane = crate::tui::bottom_pane::BottomPane::new();
        pane.push_view(Box::new(plain_view(vec![Line::from("needle target")], 20)));
        let _ = pane.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        pane.handle_paste("needle");

        assert!(pane.composer.is_empty());
        let area = Rect::new(0, 0, 80, 12);
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("Search: /needle"), "{text}");
    }

    #[test]
    fn ctrl_c_cancels_transcript_search_without_closing_the_conversation() {
        let mut pane = crate::tui::bottom_pane::BottomPane::new();
        pane.push_view(Box::new(plain_view(vec![Line::from("needle target")], 20)));
        let _ = pane.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        assert!(matches!(
            pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            crate::tui::bottom_pane::BottomPaneAction::Consumed
        ));
        assert!(pane.has_active_view());

        let area = Rect::new(0, 0, 80, 12);
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("Search cancelled"), "{text}");
    }

    #[test]
    fn replacing_live_tail_reuses_history_rows() {
        let history = TranscriptItem::rendered(
            TranscriptItemId::from_widget_id(1),
            vec![Line::from("history")],
            1,
        );
        let tail = |text: &str| {
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(2),
                TranscriptItemKind::Assistant,
                vec![Line::from(text.to_string())],
                1,
            )
        };
        let mut view = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(vec![history, tail("first")]),
            24,
            80,
        );
        let cached = view.lines[0].clone();
        assert!(view.replace_live_tail(tail("second"), 80));
        assert!(std::sync::Arc::ptr_eq(&cached, &view.lines[0]));
        assert!(rendered(&view).contains("second"));
        assert!(!view.replace_live_tail(tail("wrong width"), 40));
    }

    #[test]
    fn copying_selection_uses_the_frozen_live_snapshot() {
        use crate::tui::history_cell::assistant::AssistantCell;
        use std::sync::{Arc, Mutex};

        let cell = Arc::new(Mutex::new(AssistantCell::new_streaming()));
        cell.lock().unwrap().push_delta("first line\nsecond line\n");
        let id = TranscriptItemId::from_widget_id(5);
        let item = || TranscriptItem::live_assistant(id.clone(), cell.clone(), 0);
        let mut view = TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![item()]), 24, 80);
        assert!(view.expand_current_item());
        view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let selected = view.selected_text();
        assert!(selected.contains("first line"));

        cell.lock().unwrap().push_delta("new output\n");
        assert!(view.replace_live_tail(item(), 80));
        assert!(view.live_tail_dirty);
        view.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
        assert!(
            matches!(view.take_action_request(), Some(ViewActionRequest {
            action: BottomPaneViewAction::CopyToClipboard { text, .. }, ..
        }) if text == selected && !text.contains("new output"))
        );
        assert!(view.export_plain_lines().join("\n").contains("new output"));
    }

    #[test]
    fn changing_an_unmapped_frozen_item_keeps_the_transcript_open() {
        use crate::tui::history_cell::assistant::AssistantCell;
        use std::sync::{Arc, Mutex};

        for action in [KeyCode::Left, KeyCode::Enter, KeyCode::Right] {
            let cell = Arc::new(Mutex::new(AssistantCell::new_streaming()));
            cell.lock().unwrap().push_delta("selected content");
            let live = TranscriptItem::live_assistant(TranscriptItemId::from_widget_id(1), cell, 0);
            let mut view =
                TranscriptView::from_snapshot(TranscriptSnapshot::new(vec![live]), 24, 80);
            assert!(view.expand_current_item());
            view.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
            view.replace_snapshot(
                TranscriptSnapshot::new(vec![TranscriptItem::rendered(
                    TranscriptItemId::from_widget_id(2),
                    vec![Line::from("unrelated replacement")],
                    0,
                )]),
                80,
            );
            view.handle_key(KeyEvent::new(action, KeyModifiers::NONE));
            assert_eq!(
                view.status.as_deref(),
                Some("Selection ended · view updated"),
                "{action:?}"
            );
            assert!(view.selection_anchor.is_none());
            assert!(view.take_action_request().is_none());
        }
    }

    #[test]
    fn export_uses_logical_lines_at_every_terminal_width() {
        use crate::tui::history_cell::assistant::AssistantCell;
        use std::sync::{Arc, Mutex};

        let source = "0123456789012345678needle  \t界\u{1b}[31m红\u{1b}[0m\n\nlast\n";
        let clean = "0123456789012345678needle  \t界红\n\nlast\n";
        let cell = Arc::new(Mutex::new(AssistantCell::new_streaming()));
        cell.lock().unwrap().push_delta(source);
        for width in [12, 22, 80] {
            let view = TranscriptView::from_snapshot(
                TranscriptSnapshot::new(vec![TranscriptItem::live_assistant(
                    TranscriptItemId::from_widget_id(1),
                    cell.clone(),
                    0,
                )]),
                24,
                width,
            );
            assert_eq!(view.export_plain_lines().join("\n"), clean);
        }
        let view = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(vec![TranscriptItem::committed(
                TranscriptItemId::from_widget_id(1),
                Arc::new(AssistantCell::from_markdown(source)),
                0,
            )]),
            24,
            12,
        );
        assert_eq!(view.export_plain_lines().join("\n"), clean);

        // Root conversation history stores the same reasoning as a committed
        // HistoryCell; its search/export contract must match child history.
        let mut view = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(vec![TranscriptItem::committed(
                TranscriptItemId::from_widget_id(2),
                Arc::new(ReasoningCell::from_text(source, None)),
                0,
            )]),
            24,
            22,
        );
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for key in "needle".chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
            view.handle_key(KeyEvent::new(key, KeyModifiers::NONE));
        }
        assert_eq!(view.status.as_deref(), Some("Match 1/1 · /needle"));
        assert_eq!(
            view.export_plain_lines().join("\n"),
            format!("Thought\n{clean}")
        );
    }
}
