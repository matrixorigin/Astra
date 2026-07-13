use std::collections::HashSet;
use std::sync::Arc;

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
use crate::tui::history_cell::{HistoryCell, reasoning::ReasoningCell, tool::ToolCell};
use crate::tui::render::line_utils::sanitize_lines_for_terminal;

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
    Reasoning(ReasoningCell),
    Tool(ToolCell),
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
    id: TranscriptItemId,
    kind: TranscriptItemKind,
    content: TranscriptContent,
    separator_rows: usize,
}

impl TranscriptItem {
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

    pub(crate) fn rendered_kind(
        id: TranscriptItemId,
        kind: TranscriptItemKind,
        lines: Vec<Line<'static>>,
        separator_rows: usize,
    ) -> Self {
        Self {
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
        Self::rendered_kind(id, committed_cell_kind(cell), lines, separator_rows)
    }

    pub(crate) fn reasoning(
        id: TranscriptItemId,
        cell: ReasoningCell,
        separator_rows: usize,
    ) -> Self {
        Self {
            id,
            kind: TranscriptItemKind::Reasoning,
            content: TranscriptContent::Reasoning(cell),
            separator_rows,
        }
    }

    pub(crate) fn tool(id: TranscriptItemId, cell: ToolCell, separator_rows: usize) -> Self {
        Self {
            id,
            kind: TranscriptItemKind::Tool,
            content: TranscriptContent::Tool(cell),
            separator_rows,
        }
    }

    fn is_expandable(&self, width: u16) -> bool {
        match &self.content {
            TranscriptContent::Committed(cell) => cell_expandable(cell.as_ref(), width),
            TranscriptContent::Rendered(_) => false,
            TranscriptContent::Reasoning(cell) => cell.has_transcript_details(width),
            TranscriptContent::Tool(cell) => cell.has_transcript_details(),
        }
    }

    fn label(&self) -> &'static str {
        match &self.content {
            TranscriptContent::Committed(cell) => cell_label(cell.as_ref()),
            TranscriptContent::Rendered(_) => "item",
            TranscriptContent::Reasoning(_) => "reasoning",
            TranscriptContent::Tool(_) => "tool details",
        }
    }

    fn render(&self, width: u16, expanded: bool) -> Vec<Line<'static>> {
        let lines = match &self.content {
            TranscriptContent::Committed(cell) => cell_lines(cell.as_ref(), width, expanded),
            TranscriptContent::Rendered(lines) => lines.clone(),
            TranscriptContent::Reasoning(cell) => cell.transcript_lines(width, expanded),
            TranscriptContent::Tool(cell) => cell.transcript_lines(width, expanded),
        };
        sanitize_lines_for_terminal(lines)
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

fn cell_expandable(cell: &dyn HistoryCell, width: u16) -> bool {
    if let Some(reasoning) = cell.as_any_ref().downcast_ref::<ReasoningCell>() {
        reasoning.has_transcript_details(width)
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
        reasoning.transcript_lines(width, expanded)
    } else if let Some(tool) = cell.as_any_ref().downcast_ref::<ToolCell>() {
        tool.transcript_lines(width, expanded)
    } else {
        cell.display_lines(width)
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
}

/// Full conversation transcript including thinking content and tool output.
pub(crate) struct TranscriptView {
    title: String,
    snapshot: TranscriptSnapshot,
    lines: Vec<Line<'static>>,
    row_items: Vec<Option<TranscriptItemId>>,
    expanded: HashSet<TranscriptItemId>,
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
        for item in &self.snapshot.items {
            lines.extend(item.render(self.width, true).iter().map(line_plain_text));
            lines.extend(std::iter::repeat_n(String::new(), item.separator_rows));
        }
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
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

    fn replace_snapshot(&mut self, snapshot: TranscriptSnapshot, width: u16) {
        let follow_tail = self.is_following_tail();
        let cursor_locator = self.row_locator(self.cursor);
        let anchor_locator = self
            .selection_anchor
            .and_then(|anchor| self.row_locator(anchor));
        self.snapshot = snapshot;
        self.width = width;
        self.expanded.retain(|id| {
            self.snapshot
                .items
                .iter()
                .find(|item| &item.id == id)
                .is_some_and(|item| item.is_expandable(width))
        });
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
        self.scroll = self.scroll.min(self.max_scroll());
        self.selection_anchor = anchor_locator
            .and_then(|locator| self.row_for_locator(locator))
            .or_else(|| {
                self.selection_anchor
                    .map(|anchor| anchor.min(self.lines.len() - 1))
            });
        self.ensure_cursor_visible();
    }

    fn rebuild_lines(&mut self) {
        self.lines.clear();
        self.row_items.clear();
        for item in &self.snapshot.items {
            if !item.matches_filter(self.filter) {
                continue;
            }
            let expanded = self.expanded.contains(&item.id);
            let item_lines = item.render(self.width, expanded);
            if item_lines.is_empty() {
                continue;
            }
            self.row_items
                .extend(std::iter::repeat_n(Some(item.id.clone()), item_lines.len()));
            self.lines.extend(item_lines);
            for _ in 0..item.separator_rows {
                self.lines.push(Line::default());
                self.row_items.push(None);
            }
        }
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
            self.row_items.pop();
        }
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
        let rows: Vec<usize> = self
            .row_items
            .iter()
            .enumerate()
            .filter_map(|(row, candidate)| (candidate.as_ref() == Some(&id)).then_some(row))
            .collect();
        rows.get(offset).copied().or_else(|| rows.last().copied())
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

    fn toggle_current_item(&mut self) {
        let Some(id) = self.item_at_or_near(self.cursor) else {
            self.status = Some("Nothing to expand".to_string());
            return;
        };
        let Some(item) = self.snapshot.items.iter().find(|item| item.id == id) else {
            self.status = Some("Selected item is no longer available".to_string());
            return;
        };
        if !item.is_expandable(self.width) {
            self.status = Some("This item has no hidden details".to_string());
            return;
        }
        let label = item.label();
        let expanded = if self.expanded.remove(&id) {
            false
        } else {
            self.expanded.insert(id.clone());
            true
        };
        self.rebuild_lines();
        if let Some(row) = self.row_for_locator((id, 0)) {
            self.cursor = row;
        }
        self.ensure_cursor_visible();
        self.status = Some(format!(
            "{} {label}",
            if expanded { "Expanded" } else { "Collapsed" }
        ));
    }

    /// Expand the selected content object without turning Right into a toggle.
    /// Returns `true` only when a collapsed expandable object changed state.
    pub(crate) fn expand_current_item(&mut self) -> bool {
        let Some(id) = self.item_at_or_near(self.cursor) else {
            return false;
        };
        let Some(item) = self.snapshot.items.iter().find(|item| item.id == id) else {
            return false;
        };
        if !item.is_expandable(self.width) || self.expanded.contains(&id) {
            return false;
        }
        let label = item.label();
        self.expanded.insert(id.clone());
        self.rebuild_lines();
        if let Some(row) = self.row_for_locator((id, 0)) {
            self.move_cursor_to(row);
        }
        self.ensure_cursor_visible();
        self.status = Some(format!("Expanded {label}"));
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
            return false;
        }
        let label = self
            .snapshot
            .items
            .iter()
            .find(|item| item.id == id)
            .map(TranscriptItem::label)
            .unwrap_or("item");
        self.rebuild_lines();
        if let Some(row) = self.row_for_locator((id, 0)) {
            self.move_cursor_to(row);
        }
        self.ensure_cursor_visible();
        self.status = Some(format!("Collapsed {label}"));
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
        let searchable =
            self.snapshot
                .items
                .iter()
                .filter(|item| item.matches_filter(self.filter))
                .flat_map(|item| {
                    item.render(self.width, true).into_iter().enumerate().map(
                        move |(offset, line)| (item.id.clone(), offset, line_plain_text(&line)),
                    )
                })
                .collect::<Vec<_>>();
        let matches = searchable
            .iter()
            .enumerate()
            .filter_map(|(index, (_, _, text))| {
                text.to_lowercase().contains(&needle).then_some(index)
            })
            .collect::<Vec<_>>();
        if matches.is_empty() {
            self.status = Some(format!("No matches for /{query}"));
            return;
        }

        let current = self.row_locator(self.cursor);
        let current_index = current
            .and_then(|locator| {
                searchable
                    .iter()
                    .position(|(id, offset, _)| id == &locator.0 && *offset == locator.1)
            })
            .unwrap_or_else(|| searchable.len().saturating_sub(1));
        let match_index = if forward {
            matches
                .iter()
                .copied()
                .find(|index| *index > current_index)
                .unwrap_or(matches[0])
        } else {
            matches
                .iter()
                .copied()
                .rev()
                .find(|index| *index < current_index)
                .unwrap_or_else(|| *matches.last().expect("matches is not empty"))
        };
        let (id, offset, _) = &searchable[match_index];
        if self
            .snapshot
            .items
            .iter()
            .find(|item| &item.id == id)
            .is_some_and(|item| item.is_expandable(self.width))
        {
            self.expanded.insert(id.clone());
            self.rebuild_lines();
        }
        if let Some(row) = self.row_for_locator((id.clone(), *offset)) {
            self.move_cursor_to(row);
        }
        let ordinal = matches
            .iter()
            .position(|index| *index == match_index)
            .unwrap_or(0)
            + 1;
        self.status = Some(format!("Match {ordinal}/{} · /{query}", matches.len()));
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
        self.lines[start..=end]
            .iter()
            .map(line_plain_text)
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
        // the top of the buffer. Same fix as C-TUI-1 in task_detail_view.
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
            let mut line = self.lines[i].clone();
            if self.is_selected_row(i) {
                line.style = selection_style;
            } else if i == self.cursor {
                line.style = cursor_style;
            }
            Widget::render(line, Rect::new(area.x, y, area.width, 1), buf);
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
            .map(super::line_plain_text)
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
}
