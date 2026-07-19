//! Rendering layer for the `/context` panel.
//!
//! Visual grammar: a grid on the left (one glyph ≈ 2 % of the
//! context window) paired with a category legend on the right,
//! then nested sub-sections below for tools / memory / skills /
//! system-prompt sections.  Everything
//! goes through `build_lines(breakdown, width)` which produces a
//! `Vec<Line<'static>>` — the wrapping view renders whatever slice
//! of that list fits the current area, offset by the scroll position
//! so users can page through the full breakdown on a small overlay.
//!
//! Keeping rendering line-oriented (rather than manual Rect layout)
//! keeps terminal drawing simple. Navigation, however, is driven by typed
//! layout metadata rather than by re-reading the rendered strings.
//!
//! Approximate shape:
//!
//! ```text
//! ┌ Context window (45% · low) ────────────────────────────────────┐
//! │ model · 45.2k / 100k tokens (45%)                              │
//! │                                                                │
//! │ ⛁ ⛁ ⛁ ⛁ ⛁ ⛁ ⛶ ⛶ ⛶ ⛶     ⛁ System         3.2k   (3.2%)      │
//! │ ⛁ ⛁ ⛁ ⛁ ⛁ ⛁ ⛁ ⛶ ⛶ ⛶     ⛁ Tools         14.1k  (14.1%)      │
//! │ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶     ⛁ Memory         2.0k   (2.0%)      │
//! │ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶     ⛁ History       24.9k  (24.9%)      │
//! │ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶ ⛶     ⛁ Current turn   1.0k   (1.0%)      │
//! │                           ⛶ Free          54.8k  (54.8%)      │
//! │                                                                │
//! │ Tools · /tool                                                  │
//! │   └ read_file           1.2k tokens                            │
//! │   └ write_file          0.9k tokens                            │
//! │                                                                │
//! │ Memory · /memory                                               │
//! │   └ "project memory…"   0.4k tokens  (rel 0.91)                │
//! └────────────────────────────────────────────────────────────────┘
//! ```

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::model::{
    Category, CategoryKind, ContextBreakdown, HistoryEvidenceSource, HistorySummary, MemoryItem,
    PressureBand, Section, SkillItem, ToolItem, TurnDetail,
};

/// Grid geometry. The grid lives in the left column of the two-pane
/// top section. 5 rows × 10 cols = 50 glyphs — each glyph therefore
/// represents 2 % of the budget.
pub(crate) const GRID_ROWS: usize = 5;
pub(crate) const GRID_COLS: usize = 10;
pub(crate) const GRID_CELLS: usize = GRID_ROWS * GRID_COLS;

/// Preview body rows rendered under each item when the section is
/// expanded (but not drilled). Kept stable across selection so
/// ↑/↓ is pure navigation — nothing jumps.  Drill mode renders
/// the full body instead, capped by the wrap_text budget.
pub(crate) const EXPANDED_PREVIEW_ROWS: usize = 3;

/// View state for a single render pass.
///
/// Three nested modes, picked by which fields are set:
/// 1. No focus → flat render, just the grid + legend + sections.
/// 2. `focus = Some(section)` → headings carry the ▶ / ▼ marker;
///    Tab cycles focus.
/// 3. `expanded = Some(section)` → the focused section renders
///    its detail form. `selected_item` picks one of the section's
///    items (↑/↓); that item gets a ▸ marker.
/// 4. `drilled = true` → render ONLY the selected item's full
///    content, replacing the normal section list. Esc backs out
///    one level at a time (drill → expanded → closed).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ViewState {
    pub focus: Option<Section>,
    pub expanded: Option<Section>,
    pub selected_item: usize,
    pub drilled: bool,
}

impl ViewState {
    pub fn collapsed(focus: Option<Section>) -> Self {
        Self {
            focus,
            expanded: None,
            selected_item: 0,
            drilled: false,
        }
    }

    pub fn is_expanded(&self, s: Section) -> bool {
        self.expanded == Some(s)
    }

    pub fn is_drilled(&self, s: Section) -> bool {
        self.drilled && self.expanded == Some(s)
    }
}

/// One render pass plus the semantic positions the interactive wrapper needs.
///
/// The visible text is presentation only: a section can have a dynamic
/// heading and item bodies can contain arbitrary model/user text.  Scrolling
/// and focus must therefore use this typed layout rather than scanning spans
/// for labels or marker glyphs after rendering.
#[derive(Debug)]
pub(crate) struct ContextLayout {
    pub lines: Vec<Line<'static>>,
    section_starts: Vec<(Section, u16)>,
    selected_item_lines: Option<Range<u16>>,
}

impl ContextLayout {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            section_starts: Vec::new(),
            selected_item_lines: None,
        }
    }

    fn record_section(&mut self, section: Section, start: usize) {
        self.section_starts.push((section, start as u16));
    }

    fn record_selected_item(&mut self, lines: Option<Range<usize>>) {
        if let Some(lines) = lines {
            self.selected_item_lines = Some(lines.start as u16..lines.end as u16);
        }
    }

    fn trim_trailing_blanks(&mut self) {
        while self
            .lines
            .last()
            .map(|line| line.spans.is_empty())
            .unwrap_or(false)
        {
            self.lines.pop();
        }
    }

    pub(crate) fn section_start(&self, section: Section) -> Option<u16> {
        self.section_starts
            .iter()
            .find_map(|(candidate, start)| (*candidate == section).then_some(*start))
    }

    pub(crate) fn selected_item_line_range(&self) -> Option<Range<u16>> {
        self.selected_item_lines.clone()
    }
}

/// Ratatui render shim used by `ContextPanelView` and tests.
pub(crate) fn render(b: &ContextBreakdown, area: Rect, buf: &mut Buffer) {
    render_with(b, area, buf, 0, ViewState::default())
}

/// Ratatui render shim with explicit scroll offset and view state.
/// Callers that own scroll + focus + expansion state (the BottomPane
/// view wrapper) use this; stateless callers go through [`render`].
pub(crate) fn render_with(
    b: &ContextBreakdown,
    area: Rect,
    buf: &mut Buffer,
    scroll: u16,
    state: ViewState,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let band = b.band();
    let theme = crate::tui::theme::current();
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.gutter_frozen))
        .title(title_line(b, band));
    let inner = outer.inner(area);
    outer.render(area, buf);

    if !b.has_observable_data() {
        let msg = Line::from(Span::styled(
            "  no context trace yet — run a turn first",
            Style::default().fg(theme.dim).add_modifier(Modifier::DIM),
        ));
        Paragraph::new(msg).render(inner, buf);
        return;
    }

    // Build the full logical line list once; the paragraph picks
    // the window based on the current scroll offset and draws it
    // with wrap disabled (lines are pre-sized for `inner.width`).
    let lines = build_lines_with(b, inner.width, state);
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0))
        .render(inner, buf);
}

/// Total logical line count of the breakdown at the given width
/// and view state. The view wrapper uses this to clamp the scroll
/// offset so the user can't scroll past the last line — when a
/// section expands, the count grows and the scroll clamp moves
/// with it.
///
/// Returns `0` for the empty-breakdown placeholder render path.
pub(crate) fn line_count(b: &ContextBreakdown, inner_width: u16) -> u16 {
    line_count_with(b, inner_width, ViewState::default())
}

pub(crate) fn line_count_with(b: &ContextBreakdown, inner_width: u16, state: ViewState) -> u16 {
    if !b.has_observable_data() {
        return 0;
    }
    build_layout_with(b, inner_width, state).lines.len() as u16
}

/// How many items in the given section are selectable when it's
/// expanded. Sections where item-level drill-in is meaningful
/// (History turns, Memories, Tools, Decisions) return their
/// element count; other sections return 0 to signal "nothing to
/// select".  The wrapper uses this to clamp `selected_item`.
pub(crate) fn section_item_count(b: &ContextBreakdown, section: Section) -> usize {
    match section {
        Section::History => b.history.turns.len(),
        Section::Memory => b.memories.len(),
        Section::Tools => b.tools.len(),
        Section::Decisions => b.decisions.len(),
        Section::Compaction => b.compaction.events.len(),
        Section::SystemPrompt | Section::PromptSignals | Section::Session | Section::Skills => 0,
    }
}

/// Line index of the given section's heading in the rendered line
/// list at the given width + state. Used by the BottomPaneView
/// wrapper to auto-scroll a newly-focused (or newly-expanded)
/// section into the visible window.
///
/// Returns `None` when the section has no content in the
/// breakdown (so it wasn't rendered).
pub(crate) fn section_line_index(
    b: &ContextBreakdown,
    inner_width: u16,
    state: ViewState,
    target: Section,
) -> Option<u16> {
    build_layout_with(b, inner_width, state).section_start(target)
}

/// Typed extent of the currently selected expanded item. The workbench uses
/// this to keep the whole item visible without treating a decorative marker
/// in arbitrary rendered content as a navigation signal.
pub(crate) fn selected_item_line_range(
    b: &ContextBreakdown,
    inner_width: u16,
    state: ViewState,
) -> Option<Range<u16>> {
    build_layout_with(b, inner_width, state).selected_item_line_range()
}

pub(crate) fn desired_height(b: &ContextBreakdown) -> u16 {
    // Overlay reserves 2 rows for the border. The content itself is
    // capped at 20 rows here; the view wrapper enables scrolling
    // when content exceeds that budget.  The empty-breakdown case
    // still needs a minimum of 3 (border + stub row).
    if !b.has_observable_data() {
        return 3;
    }
    // Top block: GRID_ROWS side-by-side with the legend, plus header,
    // blank, sections.  We want the full breakdown to be visible
    // where it fits without scrolling, capped so the composer stays
    // reachable on small terminals.
    const MIN: u16 = 12;
    const MAX: u16 = 24;
    let lines = build_lines(b, 80).len() as u16;
    (lines.saturating_add(2)).clamp(MIN, MAX)
}

// ─── Line builder ─────────────────────────────────────────────────

/// Convert the breakdown into a list of rendered lines with default view state.
pub(crate) fn build_lines(b: &ContextBreakdown, inner_width: u16) -> Vec<Line<'static>> {
    build_layout_with(b, inner_width, ViewState::default()).lines
}

/// State-aware version of [`build_lines`]. Honors the focus
/// highlight (bold section heading when focused) and expands the
/// currently expanded section to its full detail form.
pub(crate) fn build_lines_with(
    b: &ContextBreakdown,
    inner_width: u16,
    state: ViewState,
) -> Vec<Line<'static>> {
    build_layout_with(b, inner_width, state).lines
}

/// Build the visible lines and their semantic navigation metadata together.
/// This is the single source of truth for context panel geometry.
pub(crate) fn build_layout_with(
    b: &ContextBreakdown,
    inner_width: u16,
    state: ViewState,
) -> ContextLayout {
    let mut layout = ContextLayout::new();

    if b.limit == 0 && b.categories.is_empty() {
        layout.lines.push(Line::from(Span::styled(
            "  Exact prompt composition is not available yet; session state below is current local evidence.",
            Style::default()
                .fg(crate::tui::theme::current().dim)
                .add_modifier(Modifier::DIM),
        )));
        layout.lines.push(Line::default());
        render_section(&mut layout, b, state, Section::Session);
        render_section(&mut layout, b, state, Section::Skills);
        render_section(&mut layout, b, state, Section::History);
        render_section(&mut layout, b, state, Section::Compaction);
        layout.trim_trailing_blanks();
        return layout;
    }

    // Header — model token counts + compression hint.
    layout.lines.push(header_line(b));
    if b.compression_triggered {
        layout.lines.push(Line::from(Span::styled(
            "  ⚠ compression triggered on the last turn",
            Style::default().fg(crate::tui::theme::current().warn),
        )));
    }
    // Top-of-panel hint tracks the current interaction mode so the
    // user always knows which keys do what RIGHT NOW.  The hint in
    // the bottom-pane footer repeats this, but having it inline
    // keeps the info visible while the user scrolls.
    let hint = if let Some(focused) = state.focus {
        let selectable = section_item_count(b, focused) > 0;
        if state.drilled {
            "  Esc back · j/k scroll"
        } else if state.expanded.is_some() && selectable {
            "  ↑/↓ select · Enter drill · Tab next · Esc back"
        } else if state.expanded.is_some() {
            // Expanded section has no drillable items — Enter just
            // collapses (and so does Esc).  Spell that out.
            "  Tab next · Enter/Esc collapse · j/k scroll"
        } else {
            "  Tab next · Enter expand · j/k scroll · Esc close"
        }
    } else {
        "  Tab focus · Enter close · j/k scroll · Esc close"
    };
    layout.lines.push(Line::from(Span::styled(
        hint,
        Style::default().add_modifier(Modifier::DIM),
    )));
    layout.lines.push(Line::default());

    // Top block: grid on the left, category legend on the right.
    // Computed together so both columns stay aligned even when the
    // legend has more rows than the grid (5) — we pad whichever
    // side is shorter with blank spans.
    layout.lines.extend(top_block_lines(b, inner_width));
    layout.lines.push(Line::default());

    // Nested sub-sections. Only rendered when non-empty.
    render_section(&mut layout, b, state, Section::Session);
    render_section(&mut layout, b, state, Section::SystemPrompt);
    render_section(&mut layout, b, state, Section::PromptSignals);
    render_section(&mut layout, b, state, Section::Tools);
    render_section(&mut layout, b, state, Section::Skills);
    render_section(&mut layout, b, state, Section::Memory);
    render_section(&mut layout, b, state, Section::History);
    render_section(&mut layout, b, state, Section::Compaction);
    render_section(&mut layout, b, state, Section::Decisions);

    // Drop the last blank line if we pushed one — trailing blanks
    // render as empty lines at the bottom of the scroll view which
    // feels unfinished.
    layout.trim_trailing_blanks();

    layout
}

fn render_section(
    layout: &mut ContextLayout,
    b: &ContextBreakdown,
    state: ViewState,
    section: Section,
) {
    if !b.section_non_empty(section) {
        return;
    }
    let start = layout.lines.len();
    layout.record_section(section, start);
    let out = &mut layout.lines;
    let focused = state.focus == Some(section);
    let expanded = state.is_expanded(section);
    let selected_item = match section {
        Section::SystemPrompt => {
            out.push(section_heading_for(
                Section::SystemPrompt,
                focused,
                expanded,
            ));
            for s in &b.system_sections {
                out.push(section_row(&format!(" {}", s.name), s.tokens));
                if expanded && let Some(preview) = &s.preview {
                    out.push(Line::from(vec![
                        Span::raw("        "),
                        Span::styled(
                            truncate_preview(preview, 120),
                            Style::default().add_modifier(Modifier::DIM),
                        ),
                    ]));
                }
            }
            out.push(Line::default());
            None
        }
        Section::Tools => {
            out.push(section_heading_for(Section::Tools, focused, expanded));
            let selected = if state.is_drilled(Section::Tools) {
                render_tool_drill(out, &b.tools, state.selected_item);
                None
            } else if expanded {
                append_tools_expanded(out, &b.tools, state.selected_item)
            } else {
                for t in &b.tools {
                    out.push(section_row(&format!(" {}", t.name), t.tokens));
                }
                None
            };
            out.push(Line::default());
            selected
        }
        Section::Skills => {
            append_skill_section(out, &b.skills, focused, expanded);
            None
        }
        Section::Memory => {
            if state.is_drilled(Section::Memory) {
                out.push(section_heading_for(Section::Memory, focused, expanded));
                render_memory_drill(out, &b.memories, state.selected_item);
                out.push(Line::default());
                None
            } else if !b.memories.is_empty() {
                let selected =
                    append_memory_section(out, &b.memories, focused, expanded, state.selected_item);
                if expanded && !b.memory_focus.is_empty() {
                    append_memory_focus(out, &b.memory_focus);
                    out.push(Line::default());
                }
                selected
            } else if !b.memory_focus.is_empty() {
                // No selected memories this turn but retrieval
                // or prompt injection still happened. Show the
                // heading plus a collapsed summary that preserves
                // the distinction between retrieved and injected.
                out.push(section_heading_for(Section::Memory, focused, expanded));
                if expanded {
                    append_memory_focus(out, &b.memory_focus);
                } else {
                    append_memory_focus_collapsed(out, &b.memory_focus);
                }
                out.push(Line::default());
                None
            } else {
                None
            }
        }
        Section::History => {
            if state.is_drilled(Section::History) {
                out.push(section_heading_for(Section::History, focused, expanded));
                render_history_drill(out, &b.history.turns, state.selected_item);
                out.push(Line::default());
                None
            } else {
                append_history_section(out, &b.history, focused, expanded, state.selected_item)
            }
        }
        Section::Session => {
            if let Some(s) = b.session_summary.as_ref() {
                append_session_section(out, s, focused, expanded);
            }
            None
        }
        Section::PromptSignals => {
            append_prompt_signals_section(out, &b.prompt_signals, focused, expanded);
            None
        }
        Section::Decisions => {
            if state.is_drilled(Section::Decisions) {
                out.push(section_heading_for(Section::Decisions, focused, expanded));
                render_decision_drill(out, &b.decisions, state.selected_item);
                out.push(Line::default());
                None
            } else {
                append_decisions_section(out, &b.decisions, focused, expanded, state.selected_item)
            }
        }
        Section::Compaction => {
            if state.is_drilled(Section::Compaction) {
                out.push(section_heading_for(Section::Compaction, focused, expanded));
                render_compaction_drill(out, &b.compaction, state.selected_item);
                out.push(Line::default());
                None
            } else {
                append_compaction_section(
                    out,
                    &b.compaction,
                    focused,
                    expanded,
                    state.selected_item,
                )
            }
        }
    };
    layout.record_selected_item(selected_item);
}

fn header_line(b: &ContextBreakdown) -> Line<'static> {
    let pct = b.usage_percent();
    let used = fmt_tokens(b.total_used);
    let limit = fmt_tokens(b.limit);
    Line::from(vec![
        Span::styled(
            format!("  {used} / {limit} tokens"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  current prompt ({pct:.1}%)"),
            Style::default().fg(b.band().color()),
        ),
    ])
}

// ─── Top block: grid + legend ─────────────────────────────────────

/// Build the side-by-side grid+legend rows.
///
/// The grid column is 2 × GRID_COLS display cells wide (each glyph
/// is one char + one space, leaving a visible gap between cells).
/// The legend column takes whatever remains and right-pads with
/// blanks so lines stay the exact inner width — otherwise Ratatui's
/// Paragraph would interpret the shorter line as wrapped content
/// and re-layout on resize.
fn top_block_lines(b: &ContextBreakdown, inner_width: u16) -> Vec<Line<'static>> {
    let grid_width: usize = GRID_COLS * 2;
    let legend_gap: usize = 2;
    let legend_width = (inner_width as usize)
        .saturating_sub(grid_width + legend_gap + 2 /* leading indent */)
        .max(24);

    let grid_cells = render_grid_cells(b);
    let legend_rows = legend_lines(b, legend_width);

    let row_count = GRID_ROWS.max(legend_rows.len());
    let mut out = Vec::with_capacity(row_count);
    for row_idx in 0..row_count {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(GRID_COLS + 4);
        spans.push(Span::raw("  "));
        if row_idx < GRID_ROWS {
            for col in 0..GRID_COLS {
                let cell = &grid_cells[row_idx * GRID_COLS + col];
                spans.push(cell.clone());
            }
        } else {
            // Pad out the space the grid would have occupied so
            // later rows still line up under the legend.
            spans.push(Span::raw(" ".repeat(grid_width)));
        }
        spans.push(Span::raw("  "));
        if row_idx < legend_rows.len() {
            spans.extend(legend_rows[row_idx].spans.iter().cloned());
        }
        out.push(Line::from(spans));
    }
    out
}

/// A single grid cell (glyph + trailing space). Glyph choice: a
/// filled block `⛁` for consumed tokens, empty `⛶` for free
/// space. Coloured by the category that owns the cell.
fn render_grid_cells(b: &ContextBreakdown) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(GRID_CELLS);
    // Fill the cells category-by-category proportionally. Rounding
    // matters on small panels — we use a running "emitted" counter
    // and compute each category's share relative to what's left so
    // the totals always add up to GRID_CELLS without drift.
    let mut remaining_cells = GRID_CELLS;
    let mut remaining_tokens: u64 = b.limit as u64;
    for cat in &b.categories {
        if remaining_cells == 0 {
            break;
        }
        let share = (cat.tokens as u64 * remaining_cells as u64)
            .checked_div(remaining_tokens)
            .unwrap_or(0)
            .min(remaining_cells as u64) as usize;
        for _ in 0..share {
            out.push(grid_glyph(true, cat.kind.color()));
        }
        remaining_cells -= share;
        remaining_tokens = remaining_tokens.saturating_sub(cat.tokens as u64);
    }
    // Remaining cells are free space.
    let theme = crate::tui::theme::current();
    for _ in 0..remaining_cells {
        out.push(grid_glyph(false, theme.dim));
    }
    out
}

fn grid_glyph(filled: bool, color: Color) -> Span<'static> {
    let ch = if filled { "⛁ " } else { "⛶ " };
    Span::styled(ch, Style::default().fg(color))
}

fn legend_lines(b: &ContextBreakdown, width: usize) -> Vec<Line<'static>> {
    // Label width: widest category label, capped so narrow terminals
    // still fit a reasonable token column.
    let label_width = CategoryKind::System.label().len().max(
        b.categories
            .iter()
            .map(|c| c.kind.label().len())
            .max()
            .unwrap_or(10),
    );
    let label_width = label_width.min(width.saturating_sub(18).max(8));

    let mut out = Vec::with_capacity(b.categories.len() + 1);
    for cat in &b.categories {
        out.push(legend_row(cat, label_width));
    }
    if b.free_space_tokens > 0 {
        out.push(free_space_row(b.free_space_tokens, b.limit, label_width));
    }
    out
}

fn legend_row(cat: &Category, label_width: usize) -> Line<'static> {
    let theme = crate::tui::theme::current();
    let mark = Span::styled("⛁ ", Style::default().fg(cat.kind.color()));
    let label = Span::styled(
        format!("{:<w$}", cat.kind.label(), w = label_width),
        Style::default().fg(cat.kind.color()),
    );
    let tokens = Span::styled(
        format!("  {:>7}", fmt_tokens(cat.tokens)),
        Style::default().add_modifier(Modifier::BOLD),
    );
    let pct = Span::styled(
        format!("  ({:>4.1}%)", cat.pct_of_limit),
        Style::default().fg(theme.dim),
    );
    Line::from(vec![mark, label, tokens, pct])
}

fn free_space_row(free_tokens: u32, limit: u32, label_width: usize) -> Line<'static> {
    let theme = crate::tui::theme::current();
    let pct = if limit == 0 {
        0.0
    } else {
        free_tokens as f64 / limit as f64 * 100.0
    };
    let mark = Span::styled("⛶ ", Style::default().fg(theme.dim));
    let label = Span::styled(
        format!("{:<w$}", "Free space", w = label_width),
        Style::default().add_modifier(Modifier::DIM),
    );
    let tokens = Span::styled(
        format!("  {:>7}", fmt_tokens(free_tokens)),
        Style::default().add_modifier(Modifier::DIM),
    );
    let pct_span = Span::styled(format!("  ({pct:>4.1}%)"), Style::default().fg(theme.dim));
    Line::from(vec![mark, label, tokens, pct_span])
}

// ─── Sub-sections ─────────────────────────────────────────────────

/// Skills sub-section. Skills can fall back to a tokenless form when
/// the runtime did not record a per-skill cost breakdown. We still
/// surface the names, and expanded rows can include any description or
/// provenance the model supplied.
fn append_skill_section(
    out: &mut Vec<Line<'static>>,
    skills: &[SkillItem],
    focused: bool,
    expanded: bool,
) {
    if skills.is_empty() {
        return;
    }
    let all_zero = skills.iter().all(|s| s.tokens == 0);
    let heading = if all_zero {
        "Skills · /skills (no token breakdown)"
    } else {
        "Skills · /skills"
    };
    out.push(section_heading_raw(heading, focused, expanded));
    for s in skills {
        if s.tokens == 0 {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::raw(s.name.clone()),
            ]));
        } else {
            out.push(section_row(&format!(" {}", s.name), s.tokens));
        }
        if expanded {
            if let Some(desc) = &s.description {
                let preview = truncate_preview(desc, 70);
                out.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(preview, Style::default().add_modifier(Modifier::DIM)),
                ]));
            }
            if let Some(source) = &s.source {
                out.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(
                        format!("source: {source}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
    }
    out.push(Line::default());
}

fn append_history_section(
    out: &mut Vec<Line<'static>>,
    h: &HistorySummary,
    focused: bool,
    expanded: bool,
    selected_item: usize,
) -> Option<Range<usize>> {
    if h.is_empty() {
        return None;
    }
    let mut selected_lines = None;
    out.push(section_heading_for(Section::History, focused, expanded));
    // Collapsed view: just the aggregate counts.
    let mut turn_spans: Vec<Span<'static>> = vec![Span::raw("    └ ")];
    match h.evidence_source {
        HistoryEvidenceSource::PromptTrace => {
            turn_spans.push(Span::raw(format!("{} turns", h.total_turns)));
        }
        HistoryEvidenceSource::LocalVisibleConversation => {
            turn_spans.push(Span::raw(format!(
                "{} visible conversation items",
                h.total_turns
            )));
            turn_spans.push(Span::styled(
                "  (prompt trace unavailable)",
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    if h.retained > 0 || h.compressed > 0 || h.dropped > 0 {
        turn_spans.push(Span::styled(
            format!(
                "  ({} retained · {} compressed · {} dropped)",
                h.retained, h.compressed, h.dropped
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    out.push(Line::from(turn_spans));
    if h.tokens_before > 0 && h.tokens_before != h.tokens_after {
        let pct_saved = (1.0 - h.tokens_after as f64 / h.tokens_before as f64) * 100.0;
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::raw(format!(
                "{} → {} tokens",
                fmt_tokens(h.tokens_before),
                fmt_tokens(h.tokens_after)
            )),
            Span::styled(
                format!("  (−{pct_saved:.0}%)"),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    if expanded {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "↑/↓ select · Enter drill · Tab next · Esc back",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
        // Walk turns in the model's order (sorted ascending by
        // turn index); item selection uses that same flat index.
        for (i, t) in h.turns.iter().enumerate() {
            let item_start = out.len();
            let selected = i == selected_item;
            let compressed = t.compressed_from.is_some();
            out.extend(turn_detail_lines(t, compressed, selected));
            if selected {
                selected_lines = Some(item_start..out.len());
            }
        }
        if !h.dropped_indices.is_empty() {
            let rendered_indices: Vec<String> =
                h.dropped_indices.iter().map(|i| format!("#{i}")).collect();
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::raw(format!("Dropped: {}", rendered_indices.join(", "))),
            ]));
        }
    }
    out.push(Line::default());
    selected_lines
}

fn turn_detail_lines(t: &TurnDetail, compressed: bool, selected: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // Leading marker reserves two columns: `▸ ` when this row is
    // the ↑/↓-selected item, two spaces otherwise. Keeps column
    // alignment stable as selection moves.
    let marker: Span<'static> = if selected {
        Span::styled(
            "▸ ",
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    };
    let mut spans: Vec<Span<'static>> = vec![Span::raw("      "), marker.clone(), Span::raw("└ ")];
    let id_style = if selected {
        Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    spans.push(Span::styled(format!("#{} {}", t.index, t.role), id_style));
    if compressed {
        if let Some((orig, method)) = &t.compressed_from {
            spans.push(Span::styled(
                format!("   {} → {} tokens", fmt_tokens(*orig), fmt_tokens(t.tokens)),
                Style::default().add_modifier(Modifier::DIM),
            ));
            spans.push(Span::styled(
                format!("  via {method}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
    } else {
        spans.push(Span::styled(
            format!("   {} tokens", fmt_tokens(t.tokens)),
            Style::default().add_modifier(Modifier::DIM),
        ));
        if t.has_tool_calls {
            spans.push(Span::styled(
                "  [tools]".to_string(),
                Style::default().fg(crate::tui::theme::current().command),
            ));
        }
    }
    out.push(Line::from(spans));
    // Multi-line wrapped preview under each turn row. Width
    // accounts for the deep indent used below (13 cols). Selected
    // rows get a taller window (up to 6 lines) since they're the
    // "active" item the user is scanning.
    let preview_source = if !t.body.is_empty() {
        t.body.as_str()
    } else {
        t.preview.as_str()
    };
    if !preview_source.is_empty() {
        // Stable preview height regardless of selection. Variable
        // heights made selection cause layout to shift underneath
        // the user — items below the selected one disappeared off
        // the bottom as the preview grew. Drill mode is where the
        // full body appears; the expanded-list view keeps rows
        // predictable so ↑/↓ feel like pure navigation.
        let lines = wrap_text(preview_source.trim(), 64, EXPANDED_PREVIEW_ROWS);
        for line in lines {
            out.push(Line::from(vec![
                Span::raw("             "),
                Span::styled(line, Style::default().add_modifier(Modifier::DIM)),
            ]));
        }
    }
    out
}

fn append_tools_expanded(
    out: &mut Vec<Line<'static>>,
    tools: &[ToolItem],
    selected_item: usize,
) -> Option<Range<usize>> {
    out.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            "↑/↓ select · Enter drill · Tab next · Esc back",
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));
    let mut selected_lines = None;
    for (i, t) in tools.iter().enumerate() {
        let item_start = out.len();
        let selected = i == selected_item;
        let marker: Span<'static> = if selected {
            Span::styled(
                "▸ ",
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        out.push(Line::from(vec![
            Span::raw("    "),
            marker,
            Span::raw("└ "),
            Span::raw(t.name.clone()),
            Span::styled(
                format!("   {} tokens", fmt_tokens(t.tokens)),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
        if selected {
            selected_lines = Some(item_start..out.len());
        }
    }
    selected_lines
}

/// Drill view for a single visible tool.
fn render_tool_drill(out: &mut Vec<Line<'static>>, tools: &[ToolItem], selected_item: usize) {
    let Some(t) = tools.get(selected_item) else {
        return;
    };
    out.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            "Esc back · drill: tool ",
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            t.name.clone(),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    out.push(Line::from(vec![
        Span::raw("        "),
        Span::raw(format!("{} tokens", fmt_tokens(t.tokens))),
    ]));
}

/// Drill view for a single selected history turn — render the
/// full body text wrapped at the inner width.
fn render_history_drill(out: &mut Vec<Line<'static>>, turns: &[TurnDetail], selected_item: usize) {
    let Some(t) = turns.get(selected_item) else {
        return;
    };
    out.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            "Esc back · drill: turn ",
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("#{} {}", t.index, t.role),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   {} tokens", fmt_tokens(t.tokens)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            if t.has_tool_calls { "  [tools]" } else { "" }.to_string(),
            Style::default().fg(crate::tui::theme::current().command),
        ),
    ]));
    if let Some((orig, method)) = &t.compressed_from {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!(
                    "compressed {} → {} tokens  via {method}",
                    fmt_tokens(*orig),
                    fmt_tokens(t.tokens)
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    // Body — wrap to a generous window so the user can read in
    // one pass. Fall back to preview when body wasn't captured.
    let body = if !t.body.is_empty() {
        t.body.as_str()
    } else {
        t.preview.as_str()
    };
    if body.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "(turn body not captured in this snapshot)".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
        return;
    }
    for line in wrap_text(body.trim(), 70, 40) {
        out.push(Line::from(vec![Span::raw("        "), Span::raw(line)]));
    }
}

/// Drill view for a single memory — full preview + metadata.
fn render_memory_drill(
    out: &mut Vec<Line<'static>>,
    memories: &[MemoryItem],
    selected_item: usize,
) {
    let Some(m) = memories.get(selected_item) else {
        return;
    };
    out.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            "Esc back · drill: memory ",
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("{} · {}", m.memory_type, m.source),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    out.push(Line::from(vec![
        Span::raw("        "),
        Span::styled(
            format!("{} tokens   rel {:.2}", fmt_tokens(m.tokens), m.relevance),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    for line in wrap_text(m.preview.trim(), 70, 40) {
        out.push(Line::from(vec![Span::raw("        "), Span::raw(line)]));
    }
}

/// Drill view for a single decision — full reasoning + ALL
/// alternatives (the collapsed view only showed the first three).
fn render_decision_drill(
    out: &mut Vec<Line<'static>>,
    decisions: &[super::model::DecisionItem],
    selected_item: usize,
) {
    let Some(d) = decisions.get(selected_item) else {
        return;
    };
    out.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            "Esc back · drill: ",
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            d.label.clone(),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("   conf {:.2}", d.confidence),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if !d.reasoning.is_empty() {
        out.push(Line::from(vec![
            Span::raw("        "),
            Span::styled("Reasoning", Style::default().add_modifier(Modifier::BOLD)),
        ]));
        for line in wrap_text(d.reasoning.trim(), 68, 20) {
            out.push(Line::from(vec![Span::raw("          "), Span::raw(line)]));
        }
    }
    if !d.alternatives.is_empty() {
        out.push(Line::from(vec![
            Span::raw("        "),
            Span::styled(
                format!("Alternatives ({})", d.alternatives.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        for a in &d.alternatives {
            out.push(Line::from(vec![
                Span::raw("          ~ "),
                Span::raw(a.description.clone()),
                Span::styled(
                    format!("   score {:.2}", a.score),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            if !a.why_not_chosen.is_empty() {
                for line in wrap_text(a.why_not_chosen.trim(), 60, 6) {
                    out.push(Line::from(vec![
                        Span::raw("               "),
                        Span::styled(
                            format!("rejected: {line}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
        }
    }
}

fn append_memory_section(
    out: &mut Vec<Line<'static>>,
    memories: &[MemoryItem],
    focused: bool,
    expanded: bool,
    selected_item: usize,
) -> Option<Range<usize>> {
    if memories.is_empty() {
        return None;
    }
    let mut selected_lines = None;
    out.push(section_heading_for(Section::Memory, focused, expanded));
    if expanded {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "↑/↓ select · Enter drill · Tab next · Esc back",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    for (i, m) in memories.iter().enumerate() {
        let item_start = out.len();
        let selected = expanded && i == selected_item;
        let marker: Span<'static> = if selected {
            Span::styled(
                "▸ ",
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        // Header row — metadata only; preview text goes under it.
        let header = if expanded {
            vec![
                Span::raw("    "),
                marker,
                Span::raw("└ "),
                Span::styled(
                    format!("{} · {}", m.memory_type, m.source),
                    if selected {
                        Style::default()
                            .fg(crate::tui::theme::current().accent)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::styled(
                    format!("   {} tokens", fmt_tokens(m.tokens)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  (rel {:.2})", m.relevance),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        } else {
            // Collapsed: single-line truncated preview.
            let preview = astra_turn_core::context_assembly_trace::preview_snippet(&m.preview, 60);
            vec![
                Span::raw("    └ "),
                Span::raw(format!("\"{preview}\"")),
                Span::styled(
                    format!("   {} tokens", fmt_tokens(m.tokens)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  (rel {:.2})", m.relevance),
                    Style::default().fg(Color::DarkGray),
                ),
            ]
        };
        out.push(Line::from(header));
        if expanded {
            let normalized =
                astra_turn_core::context_assembly_trace::normalize_content_preview(&m.preview);
            for line in wrap_text(&normalized, 66, EXPANDED_PREVIEW_ROWS) {
                out.push(Line::from(vec![
                    Span::raw("          "),
                    Span::styled(line, Style::default().add_modifier(Modifier::DIM)),
                ]));
            }
        }
        if selected {
            selected_lines = Some(item_start..out.len());
        }
    }
    out.push(Line::default());
    selected_lines
}

/// When the Memory section is expanded, render the richer
/// retrieval-pipeline detail the trace carries: the query that
/// drove retrieval, how many candidates were considered, the
/// rejected list with reasons, and memory injected directly into
/// the system prompt (distinct from selected memories — they live
/// outside the retrieval output).
fn append_memory_focus(out: &mut Vec<Line<'static>>, focus: &super::model::MemoryFocus) {
    if focus.is_empty() {
        return;
    }
    if !focus.has_retrieval_activity() && focus.has_prompt_injections() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                "No retrieval-selected memories this turn.".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    if !focus.query.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled("query: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("\"{}\"", truncate_preview(&focus.query, 120))),
        ]));
    }
    if focus.candidates_considered > 0 || focus.retrieval_latency_ms > 0 {
        let mut spans = vec![Span::raw("    └ ")];
        if focus.candidates_considered > 0 {
            spans.push(Span::raw(format!(
                "{} candidates",
                focus.candidates_considered
            )));
        }
        if focus.retrieval_latency_ms > 0 {
            if !spans.last().unwrap().content.is_empty() {
                spans.push(Span::raw("  ·  "));
            }
            spans.push(Span::styled(
                format!("{}ms retrieval", focus.retrieval_latency_ms),
                Style::default().fg(Color::DarkGray),
            ));
        }
        out.push(Line::from(spans));
    }
    if !focus.rejected.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                format!("Rejected ({})", focus.rejected.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        for r in focus.rejected.iter().take(8) {
            out.push(Line::from(vec![
                Span::raw("        └ "),
                Span::raw(truncate_preview(&r.memory_id, 18)),
                Span::styled(
                    format!("   rel {:.2}", r.relevance),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  ({})", r.reason),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
        if focus.rejected.len() > 8 {
            out.push(Line::from(vec![
                Span::raw("        └ "),
                Span::styled(
                    format!("… {} more rejected", focus.rejected.len() - 8),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    if !focus.repository_injected.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                format!(
                    "Prompt-injected repository memories ({})",
                    focus.repository_injected.len()
                ),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        for r in &focus.repository_injected {
            out.push(Line::from(vec![
                Span::raw("        └ "),
                Span::styled(
                    format!("{}: ", r.source_label),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "\"{}\"",
                    astra_turn_core::context_assembly_trace::preview_snippet(&r.preview, 100)
                )),
                Span::styled(
                    format!("   {} tokens", fmt_tokens(r.tokens)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  (rel {:.2})", r.relevance),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    if let Some(r) = focus.session_injected.as_ref() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                "Prompt-injected session snapshot".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        out.push(Line::from(vec![
            Span::raw("        └ "),
            Span::styled(
                format!("{}: ", r.source_label),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                "\"{}\"",
                astra_turn_core::context_assembly_trace::preview_snippet(&r.preview, 100)
            )),
            Span::styled(
                format!("   {} tokens", fmt_tokens(r.tokens)),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("  (rel {:.2})", r.relevance),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
}

fn append_memory_focus_collapsed(out: &mut Vec<Line<'static>>, focus: &super::model::MemoryFocus) {
    if focus.has_retrieval_activity() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                "no retrieved memories selected this turn".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    let session_count = usize::from(focus.session_injected.is_some());
    let repository_count = focus.repository_injected.len();
    if session_count > 0 || repository_count > 0 {
        let mut parts = Vec::new();
        if session_count > 0 {
            parts.push("1 session snapshot".to_string());
        }
        if repository_count > 0 {
            parts.push(format!(
                "{repository_count} repository {}",
                if repository_count == 1 {
                    "memory"
                } else {
                    "memories"
                }
            ));
        }
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                "prompt injections:".to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(parts.join(" + "), Style::default().fg(Color::DarkGray)),
        ]));
    }
}

fn append_session_section(
    out: &mut Vec<Line<'static>>,
    s: &super::model::SessionSummary,
    focused: bool,
    expanded: bool,
) {
    out.push(section_heading_for(Section::Session, focused, expanded));
    // Collapsed view: id + turn + cost/budget on one line, token
    // totals on a second.  Enough for an at-a-glance read.
    let sid_short = if s.session_id.len() > 8 {
        &s.session_id[..8]
    } else {
        s.session_id.as_str()
    };
    let mut line1: Vec<Span<'static>> = vec![Span::raw("    └ ")];
    line1.push(Span::styled(
        format!("sid {sid_short}"),
        Style::default().fg(Color::DarkGray),
    ));
    line1.push(Span::raw("  ·  "));
    line1.push(Span::raw(format!("turn {}", s.turn)));
    if let Some(model) = &s.model {
        line1.push(Span::raw("  ·  "));
        line1.push(Span::styled(
            format!("model {model}"),
            Style::default().fg(crate::tui::theme::current().accent),
        ));
    }
    out.push(Line::from(line1));

    let mut line2: Vec<Span<'static>> = vec![Span::raw("    └ ")];
    line2.push(Span::raw(format!("cost ${:.4}", s.total_cost)));
    if s.max_budget > 0.0 {
        let pct = s.total_cost / s.max_budget * 100.0;
        line2.push(Span::styled(
            format!(" / ${:.2}  ({:.0}%)", s.max_budget, pct),
            Style::default().fg(Color::DarkGray),
        ));
    }
    out.push(Line::from(line2));

    out.push(Line::from(vec![
        Span::raw("    └ "),
        Span::styled(
            format!(
                "session totals · in {}  ·  out {}  ·  cache-read {}  ·  cache-create {}",
                fmt_tokens_u64(s.prompt_tokens),
                fmt_tokens_u64(s.completion_tokens),
                fmt_tokens_u64(s.cache_read_tokens),
                fmt_tokens_u64(s.cache_creation_tokens),
            ),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ]));

    if let Some(request) = s.request_context {
        let usage = request.usage;
        let pct = if usage.limit_tokens == 0 {
            0.0
        } else {
            usage.used_tokens as f64 / usage.limit_tokens as f64 * 100.0
        };
        let provenance = match usage.source {
            astra_turn_types::ContextWindowUsageSource::Estimated => "estimated",
            astra_turn_types::ContextWindowUsageSource::ProviderReported => "provider measured",
        };
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                format!(
                    "{} context · {:.0}% · {}/{} · {provenance}",
                    request.scope.label(),
                    pct,
                    fmt_tokens_u64(usage.used_tokens),
                    fmt_tokens_u64(usage.limit_tokens),
                ),
                Style::default().fg(crate::tui::theme::current().accent),
            ),
        ]));
    }

    append_read_activity(out, &s.read_activity, expanded);

    if expanded {
        if let Some(a) = &s.continuation_anchor {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled(
                    "continuation anchor",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(vec![
                Span::raw("        "),
                Span::styled(
                    format!("\"{}\"", truncate_preview(a, 140)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
        }
        if let Some(q) = &s.queued_message {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled(
                    "queued message",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(vec![
                Span::raw("        "),
                Span::styled(
                    format!("\"{}\"", truncate_preview(q, 140)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
        }
        if let Some(d) = &s.diagnostics_context {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled(
                    "diagnostics context",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]));
            out.push(Line::from(vec![
                Span::raw("        "),
                Span::styled(
                    format!("\"{}\"", truncate_preview(d, 140)),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }

    out.push(Line::default());
}

fn append_read_activity(
    out: &mut Vec<Line<'static>>,
    activity: &super::model::ReadActivity,
    expanded: bool,
) {
    use super::model::ReadActivity;

    match activity {
        ReadActivity::Loading => out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                "read_file activity · loading durable session evidence…",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])),
        ReadActivity::Unavailable(reason) => out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                format!("read_file activity unavailable · {reason}"),
                Style::default().fg(crate::tui::theme::current().dim),
            ),
        ])),
        ReadActivity::Available(summary) if !summary.has_activity() => out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                "read_file activity · no recorded requests in this session",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ])),
        ReadActivity::Available(summary) => {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled(
                    format!(
                        "read_file activity · {} requests · {} executed · {} files",
                        summary.requested, summary.executed, summary.distinct_files
                    ),
                    Style::default().add_modifier(Modifier::DIM),
                ),
            ]));
            if expanded {
                out.push(Line::from(vec![
                    Span::raw("        "),
                    Span::styled(
                        format!(
                            "{} reused/suppressed · {} rejected/deferred · {} exact repeats from {} fully identified requests",
                            summary.reused_or_suppressed,
                            summary.other_not_executed,
                            summary.exact_repeat_requests,
                            summary.requests_with_exact_identity,
                        ),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
                if summary.repeats_after_recorded_compaction > 0 {
                    out.push(Line::from(vec![
                        Span::raw("        "),
                        Span::styled(
                            format!(
                                "{} exact repeat(s) followed a recorded compaction · correlation only, not proof of cause",
                                summary.repeats_after_recorded_compaction
                            ),
                            Style::default().fg(crate::tui::theme::current().dim),
                        ),
                    ]));
                }
            }
        }
    }
}

fn append_prompt_signals_section(
    out: &mut Vec<Line<'static>>,
    signals: &[super::model::SignalItem],
    focused: bool,
    expanded: bool,
) {
    if signals.is_empty() {
        return;
    }
    out.push(section_heading_for(
        Section::PromptSignals,
        focused,
        expanded,
    ));
    // Collapsed: one row listing all active names separated by `·`.
    // Expanded: one row per signal with a description.
    if expanded {
        let (ctx_group, guide_group): (Vec<_>, Vec<_>) = signals
            .iter()
            .partition(|s| matches!(s.kind, super::model::SignalKind::Context));
        if !ctx_group.is_empty() {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled("Context", Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for s in ctx_group {
                out.push(Line::from(vec![
                    Span::raw("        └ "),
                    Span::raw(s.name.to_string()),
                    Span::styled(
                        format!("   {}", s.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        if !guide_group.is_empty() {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled("Guidance", Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for s in guide_group {
                out.push(Line::from(vec![
                    Span::raw("        └ "),
                    Span::raw(s.name.to_string()),
                    Span::styled(
                        format!("   {}", s.description),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
    } else {
        let names: Vec<String> = signals.iter().map(|s| s.name.to_string()).collect();
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                format!("{} active", signals.len()),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::styled(
                format!("  ·  {}", truncate_preview(&names.join(" · "), 90)),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    out.push(Line::default());
}

fn append_decisions_section(
    out: &mut Vec<Line<'static>>,
    decisions: &[super::model::DecisionItem],
    focused: bool,
    expanded: bool,
    selected_item: usize,
) -> Option<Range<usize>> {
    if decisions.is_empty() {
        return None;
    }
    let mut selected_lines = None;
    out.push(section_heading_for(Section::Decisions, focused, expanded));
    if expanded {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "↑/↓ select · Enter drill · Tab next · Esc back",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
    for (i, d) in decisions.iter().enumerate() {
        let item_start = out.len();
        let selected = expanded && i == selected_item;
        let marker: Span<'static> = if selected {
            Span::styled(
                "▸ ",
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let name_style = if selected {
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        out.push(Line::from(vec![
            Span::raw("    "),
            marker,
            Span::raw("└ "),
            Span::styled(d.label.clone(), name_style),
            Span::styled(
                format!("   conf {:.2}", d.confidence),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        if expanded {
            if !d.reasoning.is_empty() {
                for line in wrap_text(&d.reasoning, 66, EXPANDED_PREVIEW_ROWS) {
                    out.push(Line::from(vec![
                        Span::raw("          "),
                        Span::styled(line, Style::default().add_modifier(Modifier::DIM)),
                    ]));
                }
            }
            for a in d.alternatives.iter().take(3) {
                out.push(Line::from(vec![
                    Span::raw("          ~ "),
                    Span::raw(truncate_preview(&a.description, 60)),
                    Span::styled(
                        format!("   score {:.2}", a.score),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                if !a.why_not_chosen.is_empty() {
                    out.push(Line::from(vec![
                        Span::raw("             "),
                        Span::styled(
                            format!("rejected: {}", truncate_preview(&a.why_not_chosen, 100)),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
        }
        if selected {
            selected_lines = Some(item_start..out.len());
        }
    }
    out.push(Line::default());
    selected_lines
}

fn append_compaction_section(
    out: &mut Vec<Line<'static>>,
    c: &super::model::CompactionSummary,
    focused: bool,
    expanded: bool,
    selected_item: usize,
) -> Option<Range<usize>> {
    if c.is_empty() {
        return None;
    }
    let mut selected_lines = None;
    out.push(section_heading_for(Section::Compaction, focused, expanded));
    // Aggregate lines (always shown).
    let trig = if c.triggered_this_turn {
        "⚠ fired this turn"
    } else {
        "not triggered this turn"
    };
    out.push(Line::from(vec![
        Span::raw("    └ "),
        Span::styled(
            trig.to_string(),
            if c.triggered_this_turn {
                Style::default().fg(crate::tui::theme::current().warn)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ),
    ]));
    if c.tokens_before > 0 && c.tokens_before != c.tokens_after {
        let pct_saved = (1.0 - c.tokens_after as f64 / c.tokens_before as f64) * 100.0;
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::raw(format!(
                "{} → {} tokens",
                fmt_tokens(c.tokens_before),
                fmt_tokens(c.tokens_after)
            )),
            Span::styled(
                format!(
                    "  (saved {}, −{:.0}%)",
                    fmt_tokens(c.tokens_saved()),
                    pct_saved
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }
    if !c.compressed_turns.is_empty() {
        let rendered: Vec<String> = c.compressed_turns.iter().map(|i| i.to_string()).collect();
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::raw(format!(
                "{} compaction{} in session — turns: {}",
                c.compressed_turns.len(),
                if c.compressed_turns.len() == 1 {
                    ""
                } else {
                    "s"
                },
                rendered.join(", "),
            )),
        ]));
    }
    if !c.stages.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    └ "),
            Span::styled(
                format!(
                    "{} pipeline stage{} freed {}",
                    c.stages.len(),
                    if c.stages.len() == 1 { "" } else { "s" },
                    fmt_tokens(c.stages.iter().map(|stage| stage.tokens_freed).sum()),
                ),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
        if expanded {
            for stage in &c.stages {
                out.push(Line::from(vec![
                    Span::raw("        └ "),
                    Span::raw(stage.stage.clone()),
                    Span::styled(
                        format!(
                            " · {} · freed {}",
                            stage.method,
                            fmt_tokens(stage.tokens_freed)
                        ),
                        Style::default().add_modifier(Modifier::DIM),
                    ),
                ]));
            }
        }
    }
    // Per-event detail on expand.
    if expanded && !c.events.is_empty() {
        out.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "↑/↓ select · Enter drill · Tab next · Esc back",
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
        for (i, e) in c.events.iter().enumerate() {
            let item_start = out.len();
            let selected = i == selected_item;
            let marker: Span<'static> = if selected {
                Span::styled(
                    "▸ ",
                    Style::default()
                        .fg(crate::tui::theme::current().accent)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("  ")
            };
            let name_style = if selected {
                Style::default()
                    .fg(crate::tui::theme::current().accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            out.push(Line::from(vec![
                Span::raw("    "),
                marker,
                Span::raw("└ "),
                Span::styled(format!("#{} {}", e.turn_index, e.role), name_style),
                Span::styled(
                    format!(
                        "   {} → {} tokens",
                        fmt_tokens(e.original_tokens),
                        fmt_tokens(e.compressed_tokens)
                    ),
                    Style::default().add_modifier(Modifier::DIM),
                ),
                Span::styled(
                    format!("  via {}", e.method),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            if selected && !e.information_lost.is_empty() {
                for lost in e.information_lost.iter().take(3) {
                    for line in wrap_text(lost, 66, 2) {
                        out.push(Line::from(vec![
                            Span::raw("          · "),
                            Span::styled(line, Style::default().add_modifier(Modifier::DIM)),
                        ]));
                    }
                }
            }
            if selected {
                selected_lines = Some(item_start..out.len());
            }
        }
    }
    out.push(Line::default());
    selected_lines
}

fn render_compaction_drill(
    out: &mut Vec<Line<'static>>,
    c: &super::model::CompactionSummary,
    selected_item: usize,
) {
    let Some(event) = c.events.get(selected_item) else {
        return;
    };
    out.push(Line::from(vec![
        Span::raw("    "),
        Span::styled(
            "Esc back · drill: compaction ",
            Style::default().add_modifier(Modifier::DIM),
        ),
        Span::styled(
            format!("#{} {}", event.turn_index, event.role),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let pct_saved = if event.original_tokens > 0 {
        (1.0 - event.compressed_tokens as f64 / event.original_tokens as f64) * 100.0
    } else {
        0.0
    };
    out.push(Line::from(vec![
        Span::raw("        "),
        Span::raw(format!(
            "{} → {} tokens",
            fmt_tokens(event.original_tokens),
            fmt_tokens(event.compressed_tokens)
        )),
        Span::styled(
            format!(
                "  (saved {}, −{:.0}%)",
                fmt_tokens(
                    event
                        .original_tokens
                        .saturating_sub(event.compressed_tokens)
                ),
                pct_saved
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    out.push(Line::from(vec![
        Span::raw("        "),
        Span::styled(
            format!("method: {}", event.method),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    if !event.information_lost.is_empty() {
        out.push(Line::from(vec![
            Span::raw("        "),
            Span::styled(
                format!("Information lost ({})", event.information_lost.len()),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]));
        for lost in &event.information_lost {
            for line in wrap_text(lost, 66, 4) {
                out.push(Line::from(vec![Span::raw("          · "), Span::raw(line)]));
            }
        }
    } else {
        out.push(Line::from(vec![
            Span::raw("        "),
            Span::styled(
                "(no information-lost notes recorded)".to_string(),
                Style::default().add_modifier(Modifier::DIM),
            ),
        ]));
    }
}

fn fmt_tokens_u64(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn section_heading(text: &str) -> Line<'static> {
    section_heading_raw(text, false, false)
}

/// Render a section heading for a known [`Section`]. Adds a marker
/// glyph so focused / expanded state is visible at a glance.
fn section_heading_for(section: Section, focused: bool, expanded: bool) -> Line<'static> {
    section_heading_raw(section.label(), focused, expanded)
}

fn section_heading_raw(text: &str, focused: bool, expanded: bool) -> Line<'static> {
    // Unicode markers: ▼ when the section is expanded, ▶ when it's
    // focused-but-collapsed (there's detail to see), and a plain
    // space otherwise. Keeps column alignment stable across states.
    let marker = if expanded {
        "▼"
    } else if focused {
        "▶"
    } else {
        " "
    };
    let style = if focused {
        Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Line::from(vec![
        Span::raw(" "),
        Span::styled(marker.to_string(), style),
        Span::raw(" "),
        Span::styled(text.to_string(), style),
    ])
}

fn section_row(label: &str, tokens: u32) -> Line<'static> {
    Line::from(vec![
        Span::raw("    └"),
        Span::raw(label.to_string()),
        Span::styled(
            format!("   {} tokens", fmt_tokens(tokens)),
            Style::default().add_modifier(Modifier::DIM),
        ),
    ])
}

fn title_line(b: &ContextBreakdown, band: PressureBand) -> Line<'static> {
    if !b.has_observable_data() {
        return Line::from(Span::styled(
            " Context · no evidence yet ",
            Style::default()
                .fg(crate::tui::theme::current().dim)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if b.limit == 0 && b.categories.is_empty() {
        return Line::from(Span::styled(
            " Context · partial evidence ",
            Style::default()
                .fg(crate::tui::theme::current().warn)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let pct = b.usage_percent();
    let headline = format!(" Context window ({pct:.0}% · {}) ", band.label());
    Line::from(vec![Span::styled(
        headline,
        Style::default()
            .fg(band.color())
            .add_modifier(Modifier::BOLD),
    )])
}

/// Soft-wrap `text` into up to `max_rows` lines that fit within
/// `width` display columns, prefixed with `indent` so each line
/// lands under the same guide column.  Produces plain-text lines
/// (callers wrap them in Styled spans).
///
/// Uses character boundaries not grapheme clusters — fine for
/// the ASCII-heavy content the panel renders, and avoids pulling
/// `unicode-segmentation` into this module's dep list.
fn wrap_text(text: &str, width: usize, max_rows: usize) -> Vec<String> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }
    // Break into logical lines first so explicit newlines in the
    // source text land on their own row.  Empty lines between
    // paragraphs become a single blank row.
    let mut out: Vec<String> = Vec::new();
    'outer: for logical in text.lines() {
        let logical = logical.trim_end();
        if logical.is_empty() {
            if out.last().map(|s| !s.is_empty()).unwrap_or(false) {
                out.push(String::new());
                if out.len() >= max_rows {
                    break;
                }
            }
            continue;
        }
        // Word-wrap within the logical line.  Long words get hard-
        // broken at the width boundary.
        let mut current = String::new();
        for word in logical.split_whitespace() {
            if word.chars().count() > width {
                // Flush current, then hard-break the giant word.
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    if out.len() >= max_rows {
                        break 'outer;
                    }
                }
                let mut rest = word.to_string();
                while rest.chars().count() > width {
                    let head: String = rest.chars().take(width).collect();
                    out.push(head);
                    if out.len() >= max_rows {
                        break 'outer;
                    }
                    rest = rest.chars().skip(width).collect();
                }
                if !rest.is_empty() {
                    current.push_str(&rest);
                }
                continue;
            }
            let would_len = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if would_len > width {
                out.push(std::mem::take(&mut current));
                if out.len() >= max_rows {
                    break 'outer;
                }
                current.push_str(word);
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            out.push(current);
            if out.len() >= max_rows {
                break;
            }
        }
    }
    // If the original text was longer than `max_rows` could fit,
    // mark the last line with an ellipsis so the user knows more
    // content exists beyond the drill-out.
    if out.len() == max_rows {
        let more_rows_exist = text.lines().count() > max_rows
            || text.chars().count() > out.iter().map(|s| s.chars().count()).sum::<usize>();
        if more_rows_exist && let Some(last) = out.last_mut() {
            let max_last = width.saturating_sub(1);
            while last.chars().count() > max_last {
                last.pop();
            }
            last.push('…');
        }
    }
    out
}

fn truncate_preview(s: &str, max_chars: usize) -> String {
    let trimmed: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

fn fmt_tokens(n: u32) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{ContextBreakdown, ContextSnapshot, SessionSummary};
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use astra_turn_core::context_assembly_trace::{
        Alternative, CompressionMethod, ContextAssemblyTrace, DecisionExplanation, DecisionType,
        MemoryInjection, MemoryRejection, MemorySelection, MemorySource, PromptContextSignals,
        PromptGuidanceSignals, RejectionReason, SkillInjection, SystemPromptBreakdown,
        TokenBudgetTrace, TurnCompression, TurnRetention, VisibleTool,
    };

    fn trace(
        max: u32,
        sys: u32,
        hist: u32,
        mem: u32,
        tools: u32,
        user: u32,
    ) -> ContextAssemblyTrace {
        let total = sys + hist + mem + tools + user;
        let pressure = if max == 0 {
            0.0
        } else {
            total as f64 / max as f64
        };
        let mut t = ContextAssemblyTrace::default();
        t.token_budget = TokenBudgetTrace {
            max_tokens: max,
            system_prompt_tokens: sys,
            history_tokens: hist,
            memory_tokens: mem,
            tool_schema_tokens: tools,
            user_message_tokens: user,
            total_used: total,
            usage_source: Default::default(),
            budget_pressure: pressure,
            compression_triggered: false,
        };
        t
    }

    struct PanelWidget<'a>(&'a ContextBreakdown);
    impl Widget for PanelWidget<'_> {
        fn render(self, area: Rect, buf: &mut Buffer) {
            render(self.0, area, buf);
        }
    }

    fn render_panel(b: &ContextBreakdown, w: u16, h: u16) -> String {
        let buf = draw_widget(PanelWidget(b), w, h);
        buffer_to_string(&buf)
    }

    // ─── Snapshot tests ──────────────────────────────────────────

    #[test]
    fn snapshot_low_pressure_80x14() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 15_000, 500, 4_000, 200));
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_low_80x14",
            render_panel(&b, 80, 14)
        );
    }

    #[test]
    fn snapshot_warning_pressure_80x14() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 8_000, 50_000, 1_000, 10_000, 1_500));
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_warn_80x14",
            render_panel(&b, 80, 14)
        );
    }

    #[test]
    fn snapshot_critical_pressure_80x14() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 12_000, 70_000, 2_000, 10_000, 1_500));
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_critical_80x14",
            render_panel(&b, 80, 14)
        );
    }

    #[test]
    fn snapshot_empty_no_trace_80x3() {
        let b = ContextBreakdown::empty();
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_empty_80x3",
            render_panel(&b, 80, 3)
        );
    }

    #[test]
    fn snapshot_history_expanded_with_wrapped_previews_100x30() {
        let mut t = trace(100_000, 2_000, 0, 0, 0, 0);
        t.history.total_turns_available = 3;
        t.history.turns_retained = vec![
            TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 80,
                has_tool_calls: false,
                content_preview: "user: Can you refactor the auth module so the session validator lives in its own file?".into(),
            },
            TurnRetention {
                turn_index: 1,
                role: "assistant".into(),
                tokens: 4_200,
                has_tool_calls: true,
                content_preview: "assistant: I'll start by reading auth.rs and mapping out every caller. Let me look at the module structure first, then identify what to move. The validate_session function has three call sites that I'll need to update.".into(),
            },
            TurnRetention {
                turn_index: 2,
                role: "user".into(),
                tokens: 40,
                has_tool_calls: false,
                content_preview: "user: Thanks — now make the helper private.".into(),
            },
        ];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::History),
            expanded: Some(Section::History),
            selected_item: 1,
            drilled: false,
        };
        let lines = build_lines_with(&b, 100, state);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        let buf = draw_widget(p, 100, 30);
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_history_expanded_100x30",
            buffer_to_string(&buf)
        );
    }

    #[test]
    fn snapshot_history_drill_100x30() {
        let mut t = trace(100_000, 2_000, 0, 0, 0, 0);
        t.history.total_turns_available = 1;
        t.history.turns_retained = vec![TurnRetention {
            turn_index: 0,
            role: "assistant".into(),
            tokens: 4_200,
            has_tool_calls: true,
            content_preview:
                "assistant: I'll start by reading auth.rs and mapping out every caller.".into(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::History),
            expanded: Some(Section::History),
            selected_item: 0,
            drilled: true,
        };
        let lines = build_lines_with(&b, 100, state);
        let p =
            ratatui::widgets::Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false });
        let buf = draw_widget(p, 100, 30);
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_history_drill_100x30",
            buffer_to_string(&buf)
        );
    }

    #[test]
    fn build_lines_renders_history_section_when_populated() {
        let mut t = trace(100_000, 2_000, 8_000, 0, 0, 0);
        t.history.total_turns_available = 5;
        t.history.turns_retained = vec![TurnRetention {
            turn_index: 0,
            role: "user".into(),
            tokens: 50,
            has_tool_calls: false,
            content_preview: String::new(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        let text: String = build_lines(&b, 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("History"), "history header missing: {text}");
        assert!(text.contains("5 turns"), "turn count missing: {text}");
    }

    #[test]
    fn snapshot_with_nested_sections_80x26() {
        let mut t = trace(100_000, 4_000, 20_000, 2_000, 6_000, 500);
        t.tools.visible_tools = vec![
            VisibleTool {
                tool_name: "read_file".into(),
                tokens: 1_200,
            },
            VisibleTool {
                tool_name: "write_file".into(),
                tokens: 900,
            },
        ];
        t.memory.memories_selected = vec![MemorySelection {
            memory_id: "m1".into(),
            memory_type: "semantic".into(),
            content_preview: "User prefers terse answers".into(),
            relevance_score: 0.91,
            tokens: 400,
            source: MemorySource::Memoria,
        }];
        t.system_prompt = SystemPromptBreakdown {
            base_persona_tokens: 1_500,
            environment_tokens: 800,
            user_preferences_tokens: 200,
            skills_injected: vec![SkillInjection {
                skill_name: "review_changes".into(),
                skill_version: None,
                tokens: 650,
                selection_reason: String::new(),
            }],
            ..SystemPromptBreakdown::default()
        };
        let b = ContextBreakdown::from_trace(&t);
        crate::tui::testing::assert_tui_snapshot!(
            "context_panel_nested_80x26",
            render_panel(&b, 80, 26)
        );
    }

    // ─── Pure helpers ─────────────────────────────────────────────

    #[test]
    fn fmt_tokens_handles_all_magnitudes() {
        assert_eq!(fmt_tokens(42), "42");
        assert_eq!(fmt_tokens(1_200), "1.2k");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn desired_height_empty_is_three_rows() {
        assert_eq!(desired_height(&ContextBreakdown::empty()), 3);
    }

    #[test]
    fn desired_height_clamps_to_min_and_max() {
        // Tiny breakdown: still at least MIN rows so the border
        // doesn't crush the content.
        let small = ContextBreakdown::from_trace(&trace(100_000, 1_000, 0, 0, 0, 0));
        assert!(desired_height(&small) >= 12);

        // Huge breakdown with lots of tools: clamped at MAX so the
        // overlay never swallows the composer.
        let mut t = trace(100_000, 1_000, 1_000, 500, 500, 0);
        t.tools.visible_tools = (0..30)
            .map(|i| VisibleTool {
                tool_name: format!("t{i}"),
                tokens: 10,
            })
            .collect();
        let huge = ContextBreakdown::from_trace(&t);
        assert!(desired_height(&huge) <= 24);
    }

    #[test]
    fn build_lines_includes_free_space_when_budget_remains() {
        // System/Tools/History consume a fraction of the budget —
        // the legend must include a "Free space" row covering the
        // remainder so the user sees how much headroom they have.
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 8_000, 0, 1_000, 200));
        let lines = build_lines(&b, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("Free space"),
            "free space row missing: {text}"
        );
    }

    #[test]
    fn build_lines_omits_free_space_when_over_budget() {
        // total_used > max: free_space_tokens clamps at 0 which
        // means the legend skips the row (model invariant).
        let mut t = trace(100_000, 50_000, 60_000, 10_000, 10_000, 1_000);
        t.token_budget.total_used = 150_000;
        let b = ContextBreakdown::from_trace(&t);
        let lines = build_lines(&b, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("Free space"), "should be hidden: {text}");
    }

    #[test]
    fn expanded_history_uses_prompt_trace_previews() {
        let mut t = trace(100_000, 2_000, 8_000, 0, 0, 0);
        t.history.total_turns_available = 2;
        t.history.turns_retained = vec![
            TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 50,
                has_tool_calls: false,
                content_preview: "user: can you refactor the auth module".into(),
            },
            TurnRetention {
                turn_index: 1,
                role: "assistant".into(),
                tokens: 1_200,
                has_tool_calls: true,
                content_preview: "assistant: I'll start by reading auth.rs…".into(),
            },
        ];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::History),
            expanded: Some(Section::History),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("can you refactor the auth module"),
            "user preview missing: {text}"
        );
        assert!(
            text.contains("I'll start by reading auth.rs"),
            "assistant preview missing: {text}"
        );
    }

    #[test]
    fn memory_section_renders_rejected_and_repository_on_expand() {
        let mut t = trace(100_000, 1_000, 0, 500, 0, 0);
        t.memory.memories_selected = vec![MemorySelection {
            memory_id: "m1".into(),
            memory_type: "semantic".into(),
            content_preview: "kept".into(),
            relevance_score: 0.9,
            tokens: 100,
            source: MemorySource::Memoria,
        }];
        t.memory.query = "retrieval bug".into();
        t.memory.candidates_considered = 7;
        t.memory.retrieval_latency_ms = 42;
        t.memory.memories_rejected = vec![MemoryRejection {
            memory_id: "m-low".into(),
            relevance_score: 0.3,
            rejection_reason: RejectionReason::BelowThreshold {
                threshold: 0.5,
                score: 0.3,
            },
        }];
        t.system_prompt.repository_memories = vec![MemoryInjection {
            memory_id: "repo".into(),
            memory_type: "repository".into(),
            tokens: 80,
            relevance_score: 0.85,
            content_preview: "# Project rules".into(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Memory),
            expanded: Some(Section::Memory),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("retrieval bug"), "query missing: {text}");
        assert!(text.contains("7 candidates"), "candidates missing: {text}");
        assert!(text.contains("42ms"), "latency missing: {text}");
        assert!(text.contains("Rejected (1)"), "rejected header: {text}");
        assert!(text.contains("below threshold"), "reason: {text}");
        t.system_prompt.session_memory_injected = Some(MemoryInjection {
            memory_id: "sess".into(),
            memory_type: "working".into(),
            tokens: 120,
            relevance_score: 1.0,
            content_preview: "# Session Memory\n## Session Title\nAstra self reflection".into(),
        });
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Memory),
            expanded: Some(Section::Memory),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("retrieval bug"), "query missing: {text}");
        assert!(text.contains("7 candidates"), "candidates missing: {text}");
        assert!(text.contains("42ms"), "latency missing: {text}");
        assert!(text.contains("Rejected (1)"), "rejected header: {text}");
        assert!(text.contains("below threshold"), "reason: {text}");
        assert!(
            text.contains("Prompt-injected repository memories (1)"),
            "repo header: {text}"
        );
        assert!(text.contains("Repository memory"), "repo label: {text}");
        assert!(text.contains("# Project rules"), "repo preview: {text}");
        assert!(
            text.contains("Prompt-injected session snapshot"),
            "session header: {text}"
        );
        assert!(text.contains("Session memory"), "session label: {text}");
        assert!(
            text.contains("# Session Memory ⏎ ## Session Title"),
            "session preview: {text}"
        );
    }

    #[test]
    fn memory_section_collapsed_distinguishes_prompt_only_injection() {
        let mut t = trace(100_000, 0, 0, 0, 0, 0);
        t.system_prompt.session_memory_injected = Some(MemoryInjection {
            memory_id: "sess".into(),
            memory_type: "working".into(),
            tokens: 120,
            relevance_score: 1.0,
            content_preview: "# Session Memory".into(),
        });
        t.system_prompt.repository_memories = vec![MemoryInjection {
            memory_id: "repo".into(),
            memory_type: "repository".into(),
            tokens: 80,
            relevance_score: 0.85,
            content_preview: "# Project rules".into(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Memory),
            expanded: None,
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !text.contains("no memories selected this turn"),
            "collapsed summary regressed: {text}"
        );
        assert!(
            text.contains("prompt injections:")
                && text.contains("1 session snapshot + 1 repository memory"),
            "prompt-only summary missing: {text}"
        );
    }

    #[test]
    fn prompt_signals_section_collapsed_lists_names_expanded_describes() {
        use super::super::model::ContextSnapshot;
        let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
        t.system_prompt.context_signals = PromptContextSignals {
            memory_signal_detected: true,
            self_awareness: true,
            ..PromptContextSignals::default()
        };
        t.system_prompt.guidance_signals = PromptGuidanceSignals {
            parallel_batching_nudge: true,
            ..PromptGuidanceSignals::default()
        };
        let b = ContextBreakdown::from_trace_with(&t, &ContextSnapshot::default());
        let focus = ViewState::collapsed(Some(Section::PromptSignals));
        let collapsed: String = build_lines_with(&b, 80, focus)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(collapsed.contains("3 active"));
        assert!(collapsed.contains("memory_signal_detected"));
        assert!(
            !collapsed.contains("Retrieval found"),
            "description must stay hidden when collapsed: {collapsed}"
        );

        let expanded_state = ViewState {
            focus: Some(Section::PromptSignals),
            expanded: Some(Section::PromptSignals),
            selected_item: 0,
            drilled: false,
        };
        let expanded: String = build_lines_with(&b, 80, expanded_state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(expanded.contains("Context"), "sub-header: {expanded}");
        assert!(expanded.contains("Guidance"), "sub-header: {expanded}");
        assert!(expanded.contains("Retrieval found"), "desc: {expanded}");
    }

    #[test]
    fn decisions_section_renders_reasoning_and_alternatives_on_expand() {
        let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
        t.explanations = vec![DecisionExplanation {
            decision_type: DecisionType::StrategyChoice {
                strategy: "code-intel".into(),
            },
            reasoning: "Need symbol-aware context.".into(),
            alternatives_considered: vec![Alternative {
                description: "grep-only".into(),
                score: 0.4,
                why_not_chosen: "would miss imports".into(),
            }],
            confidence: 0.9,
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Decisions),
            expanded: Some(Section::Decisions),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("code-intel"));
        assert!(text.contains("Need symbol-aware context"));
        assert!(text.contains("grep-only"));
        assert!(text.contains("would miss imports"));
    }

    #[test]
    fn compaction_section_renders_counts_and_timeline_when_collapsed() {
        use super::super::model::ContextSnapshot;
        let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
        t.token_budget.compression_triggered = true;
        t.history.tokens_before = 30_000;
        t.history.tokens_after = 12_000;
        t.history.turns_compressed = vec![TurnCompression {
            turn_index: 2,
            role: "assistant".into(),
            original_tokens: 18_000,
            compressed_tokens: 1_000,
            compression_method: CompressionMethod::TieredCompaction,
            information_lost: Vec::new(),
        }];
        let mut snap = ContextSnapshot::default();
        snap.compressed_turns = vec![2, 5];
        let b = ContextBreakdown::from_trace_with(&t, &snap);
        let state = ViewState::collapsed(Some(Section::Compaction));
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("fired this turn"));
        assert!(text.contains("30.0k → 12.0k"));
        assert!(text.contains("turns: 2, 5"));
    }

    #[test]
    fn compaction_drill_shows_information_lost_bullets() {
        let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
        t.history.turns_compressed = vec![TurnCompression {
            turn_index: 4,
            role: "assistant".into(),
            original_tokens: 8_000,
            compressed_tokens: 500,
            compression_method: CompressionMethod::LlmSummarization,
            information_lost: vec![
                "Tool result for read_file truncated (1.2k → 0.1k chars)".into(),
                "Assistant draft #2 dropped (superseded by #3)".into(),
            ],
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Compaction),
            expanded: Some(Section::Compaction),
            selected_item: 0,
            drilled: true,
        };
        let text: String = build_lines_with(&b, 100, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("LlmSummarization"));
        assert!(text.contains("Tool result for read_file"));
        assert!(text.contains("Assistant draft #2 dropped"));
    }

    #[test]
    fn session_section_renders_when_snapshot_carries_summary() {
        use super::super::model::{
            ContextSnapshot, RequestContextEvidence, RequestContextScope, SessionSummary,
        };
        let t = trace(100_000, 1_000, 0, 0, 0, 0);
        let mut snap = ContextSnapshot::default();
        snap.session = Some(SessionSummary {
            session_id: "abcdef12-full".into(),
            turn: 5,
            model: Some("test-model-x".into()),
            total_cost: 0.12,
            max_budget: 1.0,
            prompt_tokens: 1200,
            completion_tokens: 300,
            cache_read_tokens: 800,
            cache_creation_tokens: 0,
            request_context: Some(RequestContextEvidence {
                usage: astra_turn_types::ContextWindowUsage::provider_reported(25_000, 100_000),
                scope: RequestContextScope::LastCompletedRequest,
            }),
            continuation_anchor: Some("refactoring auth".into()),
            queued_message: None,
            diagnostics_context: None,
            read_activity: Default::default(),
        });
        let b = ContextBreakdown::from_trace_with(&t, &snap);
        let state = ViewState {
            focus: Some(Section::Session),
            expanded: Some(Section::Session),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("sid abcdef12"), "short sid: {text}");
        assert!(text.contains("turn 5"));
        assert!(text.contains("test-model-x"));
        assert!(text.contains("$0.1200"));
        assert!(text.contains("/ $1.00"));
        assert!(
            text.contains(
                "last completed request context · 25% · 25.0k/100.0k · provider measured"
            ),
            "request context is distinct from session totals: {text}"
        );
        assert!(text.contains("refactoring auth"));
    }

    #[test]
    fn partial_evidence_view_keeps_known_session_and_history_visible() {
        use super::super::model::{ContextSnapshot, SessionSummary, VisibleConversationItem};
        let mut snap = ContextSnapshot::default();
        snap.session = Some(SessionSummary {
            session_id: "session-known".into(),
            turn: 4,
            model: Some("model-known".into()),
            total_cost: 0.02,
            max_budget: 1.0,
            prompt_tokens: 1_000,
            completion_tokens: 200,
            cache_read_tokens: 300,
            cache_creation_tokens: 0,
            request_context: None,
            continuation_anchor: None,
            queued_message: None,
            diagnostics_context: None,
            read_activity: Default::default(),
        });
        snap.visible_conversation.push(VisibleConversationItem {
            role: "user".into(),
            preview: "keep this task moving".into(),
            body: "keep this task moving".into(),
        });
        let breakdown = ContextBreakdown::from_snapshot_without_trace(&snap);

        let text: String = build_lines_with(
            &breakdown,
            80,
            ViewState {
                focus: Some(Section::History),
                expanded: Some(Section::History),
                selected_item: 0,
                drilled: false,
            },
        )
        .iter()
        .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
        .collect::<Vec<_>>()
        .join(" ");
        assert!(text.contains("Exact prompt composition is not available yet"));
        assert!(text.contains("Session · budget & state"));
        assert!(text.contains("History · conversation turns"));
        assert!(text.contains("prompt trace unavailable"));
        assert!(text.contains("keep this task moving"));
    }

    #[test]
    fn expanded_system_prompt_shows_env_preview() {
        use super::super::model::ContextSnapshot;
        let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
        t.system_prompt.environment_tokens = 500;
        t.system_prompt.base_persona_tokens = 400;
        let mut snap = ContextSnapshot::default();
        snap.cwd = Some("~/github/astra".into());
        snap.git_branch = Some("improve_tui3".into());
        snap.model = Some("test-model-x");
        let b = ContextBreakdown::from_trace_with(&t, &snap);
        let state = ViewState {
            focus: Some(Section::SystemPrompt),
            expanded: Some(Section::SystemPrompt),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("~/github/astra"),
            "cwd preview missing: {text}"
        );
        assert!(text.contains("improve_tui3"), "git branch missing: {text}");
        assert!(
            text.contains("test-model-x"),
            "model persona missing: {text}"
        );
    }

    #[test]
    fn collapsed_system_prompt_omits_env_preview() {
        use super::super::model::ContextSnapshot;
        let mut t = trace(100_000, 1_000, 0, 0, 0, 0);
        t.system_prompt.environment_tokens = 500;
        let mut snap = ContextSnapshot::default();
        snap.cwd = Some("~/code".into());
        let b = ContextBreakdown::from_trace_with(&t, &snap);
        let state = ViewState::collapsed(Some(Section::SystemPrompt));
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !text.contains("~/code"),
            "env preview must stay hidden until expansion: {text}"
        );
    }

    #[test]
    fn expanded_history_section_shows_per_turn_detail() {
        let mut t = trace(100_000, 2_000, 8_000, 0, 0, 0);
        t.history.total_turns_available = 4;
        t.history.turns_retained = vec![
            TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 180,
                has_tool_calls: false,
                content_preview: String::new(),
            },
            TurnRetention {
                turn_index: 2,
                role: "assistant".into(),
                tokens: 4_200,
                has_tool_calls: true,
                content_preview: String::new(),
            },
        ];
        t.history.turns_compressed = vec![TurnCompression {
            turn_index: 1,
            role: "assistant".into(),
            original_tokens: 800,
            compressed_tokens: 120,
            compression_method: CompressionMethod::ReactiveCompact,
            information_lost: Vec::new(),
        }];
        t.history.turns_dropped = vec![3];
        t.history.tokens_before = 5_180;
        t.history.tokens_after = 4_500;
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::History),
            expanded: Some(Section::History),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        // Per-turn detail appears only on expansion.
        assert!(text.contains("#0 user"), "retained turn missing: {text}");
        assert!(
            text.contains("#2 assistant"),
            "retained turn missing: {text}"
        );
        assert!(text.contains("[tools]"), "tool marker missing: {text}");
        assert!(
            text.contains("#1 assistant"),
            "compressed turn missing: {text}"
        );
        assert!(text.contains("via"), "compression method missing: {text}");
        assert!(text.contains("Dropped: #3"), "dropped turn missing: {text}");
    }

    #[test]
    fn expanded_memory_section_shows_type_and_source() {
        let mut t = trace(100_000, 1_000, 1_000, 500, 0, 0);
        t.memory.memories_selected = vec![MemorySelection {
            memory_id: "m1".into(),
            memory_type: "semantic".into(),
            content_preview: "short".into(),
            relevance_score: 0.9,
            tokens: 120,
            source: MemorySource::Memoria,
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Memory),
            expanded: Some(Section::Memory),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("semantic"), "memory type missing: {text}");
        assert!(text.contains("Memoria"), "memory source missing: {text}");
    }

    #[test]
    fn collapsed_history_section_omits_per_turn_detail() {
        let mut t = trace(100_000, 2_000, 8_000, 0, 0, 0);
        t.history.total_turns_available = 2;
        t.history.turns_retained = vec![TurnRetention {
            turn_index: 0,
            role: "user".into(),
            tokens: 100,
            has_tool_calls: false,
            content_preview: String::new(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState::collapsed(Some(Section::History));
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            !text.contains("#0 user"),
            "per-turn detail must stay hidden when collapsed: {text}"
        );
    }

    #[test]
    fn focused_section_heading_has_focus_marker() {
        // The ▶ marker appears only on the focused section heading.
        let mut t = trace(100_000, 2_000, 0, 0, 1_000, 0);
        t.tools.visible_tools = vec![VisibleTool {
            tool_name: "bash".into(),
            tokens: 100,
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState::collapsed(Some(Section::Tools));
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("▶"), "focus marker missing: {text}");
    }

    #[test]
    fn wrap_text_splits_long_line_into_rows() {
        let rows = wrap_text(&"word ".repeat(30), 20, 6);
        assert!(rows.len() >= 2);
        for row in &rows {
            assert!(row.chars().count() <= 20, "row exceeded width: {row}");
        }
    }

    #[test]
    fn wrap_text_appends_ellipsis_when_truncating() {
        let rows = wrap_text(&"abc\n".repeat(20), 10, 3);
        assert_eq!(rows.len(), 3);
        assert!(
            rows[2].ends_with('…'),
            "last row should be ellipsis-terminated when more rows exist: {rows:?}"
        );
    }

    #[test]
    fn wrap_text_respects_explicit_paragraphs() {
        let rows = wrap_text("one\n\ntwo", 30, 10);
        // Middle blank row preserved as a paragraph break.
        assert_eq!(
            rows,
            vec!["one".to_string(), String::new(), "two".to_string()]
        );
    }

    #[test]
    fn expanded_history_exposes_typed_selected_item_geometry() {
        let mut t = trace(100_000, 2_000, 0, 0, 0, 0);
        t.history.total_turns_available = 3;
        t.history.turns_retained = vec![
            TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 80,
                has_tool_calls: false,
                content_preview: String::new(),
            },
            TurnRetention {
                turn_index: 1,
                role: "assistant".into(),
                tokens: 200,
                has_tool_calls: false,
                content_preview: String::new(),
            },
            TurnRetention {
                turn_index: 2,
                role: "user".into(),
                tokens: 40,
                has_tool_calls: false,
                content_preview: String::new(),
            },
        ];
        let b = ContextBreakdown::from_trace(&t);
        let first = ViewState {
            focus: Some(Section::History),
            expanded: Some(Section::History),
            selected_item: 0,
            drilled: false,
        };
        let second = ViewState {
            selected_item: 1,
            ..first
        };
        let first_range = selected_item_line_range(&b, 80, first)
            .expect("first retained turn has a typed layout range");
        let second_range = selected_item_line_range(&b, 80, second)
            .expect("second retained turn has a typed layout range");

        assert!(first_range.start < first_range.end);
        assert!(second_range.start < second_range.end);
        assert!(
            first_range.end <= second_range.start,
            "item ranges follow the model order without inspecting rendered text"
        );
    }

    #[test]
    fn local_context_evidence_has_real_scroll_geometry_without_a_prompt_trace() {
        let mut snapshot = ContextSnapshot::default();
        snapshot.session = Some(SessionSummary {
            session_id: "session-local".into(),
            turn: 4,
            model: Some("model-local".into()),
            total_cost: 0.02,
            max_budget: 1.0,
            prompt_tokens: 1_200,
            completion_tokens: 800,
            cache_read_tokens: 300,
            cache_creation_tokens: 0,
            request_context: None,
            continuation_anchor: None,
            queued_message: None,
            diagnostics_context: None,
            read_activity: Default::default(),
        });
        snapshot.visible_conversation = vec![
            super::super::model::VisibleConversationItem {
                role: "user".into(),
                preview: "first request".into(),
                body: "first request".into(),
            },
            super::super::model::VisibleConversationItem {
                role: "assistant".into(),
                preview: "follow-up request".into(),
                body: "follow-up request".into(),
            },
        ];

        let breakdown = ContextBreakdown::from_snapshot_without_trace(&snapshot);
        let state = ViewState::default();
        assert!(line_count_with(&breakdown, 80, state) > 0);
        assert!(section_line_index(&breakdown, 80, state, Section::Session).is_some());
        assert!(section_line_index(&breakdown, 80, state, Section::History).is_some());
    }

    #[test]
    fn prompt_trace_history_drill_does_not_join_an_unrelated_local_body() {
        use super::super::model::VisibleConversationItem;
        let mut t = trace(100_000, 2_000, 0, 0, 0, 0);
        t.history.total_turns_available = 1;
        t.history.turns_retained = vec![TurnRetention {
            turn_index: 0,
            role: "user".into(),
            tokens: 300,
            has_tool_calls: false,
            content_preview: "user: First prompt-history item.".into(),
        }];
        let mut snap = ContextSnapshot::default();
        snap.visible_conversation.push(VisibleConversationItem {
            role: "user".into(),
            preview: "local transcript item".into(),
            body: "local transcript body that must not be attributed to the prompt trace".into(),
        });
        let b = ContextBreakdown::from_trace_with(&t, &snap);
        let state = ViewState {
            focus: Some(Section::History),
            expanded: Some(Section::History),
            selected_item: 0,
            drilled: true,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("First prompt-history item"), "{text}");
        assert!(!text.contains("local transcript body"), "{text}");
        assert!(text.contains("Esc back"), "drill hint missing: {text}");
    }

    #[test]
    fn memory_drill_replaces_list_with_single_item() {
        let mut t = trace(100_000, 1_000, 0, 500, 0, 0);
        t.memory.memories_selected = vec![
            MemorySelection {
                memory_id: "a".into(),
                memory_type: "semantic".into(),
                content_preview: "alpha entry with some detail".into(),
                relevance_score: 0.9,
                tokens: 100,
                source: MemorySource::Memoria,
            },
            MemorySelection {
                memory_id: "b".into(),
                memory_type: "procedural".into(),
                content_preview: "bravo entry — distinct from alpha".into(),
                relevance_score: 0.7,
                tokens: 80,
                source: MemorySource::Session,
            },
        ];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Memory),
            expanded: Some(Section::Memory),
            selected_item: 1,
            drilled: true,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            text.contains("bravo entry"),
            "drilled item content missing: {text}"
        );
        assert!(
            !text.contains("alpha entry"),
            "non-selected item must not appear in drill: {text}"
        );
    }

    #[test]
    fn tool_drill_lists_tool_tokens() {
        let mut t = trace(100_000, 1_000, 0, 0, 2_000, 0);
        t.tools.visible_tools = vec![VisibleTool {
            tool_name: "bash".into(),
            tokens: 200,
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Tools),
            expanded: Some(Section::Tools),
            selected_item: 0,
            drilled: true,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("bash"));
        assert!(text.contains("200 tokens"));
    }

    #[test]
    fn section_item_count_reflects_each_section_type() {
        let mut t = trace(100_000, 1_000, 0, 500, 1_000, 0);
        t.history.total_turns_available = 2;
        t.history.turns_retained = vec![TurnRetention {
            turn_index: 0,
            role: "user".into(),
            tokens: 50,
            has_tool_calls: false,
            content_preview: String::new(),
        }];
        t.memory.memories_selected = vec![MemorySelection {
            memory_id: "m".into(),
            memory_type: "semantic".into(),
            content_preview: "x".into(),
            relevance_score: 0.5,
            tokens: 100,
            source: MemorySource::Memoria,
        }];
        t.tools.visible_tools = vec![VisibleTool {
            tool_name: "bash".into(),
            tokens: 200,
        }];
        let b = ContextBreakdown::from_trace(&t);
        assert_eq!(section_item_count(&b, Section::History), 1);
        assert_eq!(section_item_count(&b, Section::Memory), 1);
        assert_eq!(section_item_count(&b, Section::Tools), 1);
        assert_eq!(section_item_count(&b, Section::SystemPrompt), 0);
        assert_eq!(section_item_count(&b, Section::Session), 0);
    }

    #[test]
    fn expanded_section_heading_has_expand_marker() {
        let mut t = trace(100_000, 2_000, 0, 0, 1_000, 0);
        t.tools.visible_tools = vec![VisibleTool {
            tool_name: "bash".into(),
            tokens: 100,
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Tools),
            expanded: Some(Section::Tools),
            selected_item: 0,
            drilled: false,
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("▼"), "expand marker missing: {text}");
    }

    #[test]
    fn expanded_selectable_section_hints_include_tab_navigation() {
        let mut t = trace(100_000, 2_000, 0, 0, 1_000, 0);
        t.tools.visible_tools = vec![VisibleTool {
            tool_name: "bash".into(),
            tokens: 100,
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Tools),
            expanded: Some(Section::Tools),
            selected_item: 0,
            drilled: false,
        };

        let hint_lines: Vec<String> = build_lines_with(&b, 80, state)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .filter(|text: &String| text.contains("↑/↓ select") && text.contains("Enter drill"))
            .collect();

        assert!(
            !hint_lines.is_empty(),
            "expanded selectable section should render keyboard hints"
        );
        assert!(
            hint_lines.iter().all(|text| text.contains("Tab next")),
            "all visible selectable-section hints should mention Tab navigation: {hint_lines:?}"
        );
    }

    #[test]
    fn initial_context_panel_shows_inline_tab_focus_hint() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 8_000, 0, 0, 500));
        let text: String = build_lines_with(&b, 80, ViewState::default())
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");

        assert!(
            text.contains("Tab focus"),
            "initial no-focus panel should show the same inline Tab focus hint as the footer: {text}"
        );
    }

    #[test]
    fn build_lines_sections_render_only_when_non_empty() {
        // Vanilla trace has no tools/memory/skills → no sub-section
        // headings should appear.
        let b = ContextBreakdown::from_trace(&trace(100_000, 2_000, 8_000, 0, 0, 500));
        let lines = build_lines(&b, 80);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!text.contains("Tools · /tool"));
        assert!(!text.contains("Memory · /memory"));
        assert!(!text.contains("Skills"));
    }

    #[test]
    fn line_count_matches_build_lines_len() {
        let mut t = trace(100_000, 2_000, 8_000, 0, 1_000, 500);
        t.tools.visible_tools = vec![VisibleTool {
            tool_name: "x".into(),
            tokens: 100,
        }];
        let b = ContextBreakdown::from_trace(&t);
        assert_eq!(line_count(&b, 80) as usize, build_lines(&b, 80).len());
    }

    #[test]
    fn grid_uses_fifty_cells() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 10_000, 0, 0, 0, 0));
        let cells = render_grid_cells(&b);
        assert_eq!(cells.len(), GRID_CELLS);
        assert_eq!(cells.len(), 50, "5 × 10 grid: one glyph ≈ 2% of budget");
    }
}
