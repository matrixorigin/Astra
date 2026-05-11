//! Rendering layer for the `/context` panel.
//!
//! Mirrors Claude Code's `/context` view: a grid visualization on the
//! left (one glyph ≈ 2 % of the context window) paired with a
//! category legend on the right, then nested sub-sections below for
//! tools / memory / skills / system-prompt sections.  Everything
//! goes through `build_lines(breakdown, width)` which produces a
//! `Vec<Line<'static>>` — the wrapping view renders whatever slice
//! of that list fits the current area, offset by the scroll position
//! so users can page through the full breakdown on a small overlay.
//!
//! Keeping rendering line-oriented (rather than manual Rect layout)
//! means the tests can assert against `Vec<Line>` directly and the
//! scroll logic stays trivial.
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

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Widget, Wrap};

use super::model::{
    Category, CategoryKind, ContextBreakdown, HistorySummary, MemoryItem, PressureBand, Section,
    SkillItem, ToolItem, TurnDetail,
};

/// Grid geometry. The grid lives in the left column of the two-pane
/// top section. 5 rows × 10 cols = 50 glyphs — each glyph therefore
/// represents 2 % of the budget. Matches Claude Code's density.
pub(crate) const GRID_ROWS: usize = 5;
pub(crate) const GRID_COLS: usize = 10;
pub(crate) const GRID_CELLS: usize = GRID_ROWS * GRID_COLS;

/// View state for a single render pass. Carries the currently
/// focused section plus whether it's expanded to its detail view.
/// Defaults to "no focus, no expansion" which reproduces the
/// original flat render.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ViewState {
    pub focus: Option<Section>,
    pub expanded: Option<Section>,
}

impl ViewState {
    pub fn collapsed(focus: Option<Section>) -> Self {
        Self {
            focus,
            expanded: None,
        }
    }

    pub fn is_expanded(&self, s: Section) -> bool {
        self.expanded == Some(s)
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
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(title_line(b, band));
    let inner = outer.inner(area);
    outer.render(area, buf);

    if b.limit == 0 && b.categories.is_empty() {
        let msg = Line::from(Span::styled(
            "  no context trace yet — run a turn first",
            Style::default().add_modifier(Modifier::DIM),
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

/// Backwards-compat alias — some callsites still use the older
/// "scroll-only" signature.
pub(crate) fn render_with_scroll(b: &ContextBreakdown, area: Rect, buf: &mut Buffer, scroll: u16) {
    render_with(b, area, buf, scroll, ViewState::default())
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
    if b.limit == 0 && b.categories.is_empty() {
        return 0;
    }
    build_lines_with(b, inner_width, state).len() as u16
}

pub(crate) fn desired_height(b: &ContextBreakdown) -> u16 {
    // Overlay reserves 2 rows for the border. The content itself is
    // capped at 20 rows here; the view wrapper enables scrolling
    // when content exceeds that budget.  The empty-breakdown case
    // still needs a minimum of 3 (border + stub row).
    if b.limit == 0 && b.categories.is_empty() {
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

/// Convert the breakdown into a list of rendered lines — collapsed
/// view, no focus highlight. Retained for stateless callers and
/// legacy tests. Defers to [`build_lines_with`] under the hood.
pub(crate) fn build_lines(b: &ContextBreakdown, inner_width: u16) -> Vec<Line<'static>> {
    build_lines_with(b, inner_width, ViewState::default())
}

/// State-aware version of [`build_lines`]. Honors the focus
/// highlight (bold section heading when focused) and expands the
/// currently expanded section to its full detail form.
pub(crate) fn build_lines_with(
    b: &ContextBreakdown,
    inner_width: u16,
    state: ViewState,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();

    // Header — model token counts + compression hint.
    out.push(header_line(b));
    if b.compression_triggered {
        out.push(Line::from(Span::styled(
            "  ⚠ compression triggered on the last turn",
            Style::default().fg(Color::Yellow),
        )));
    }
    if let Some(focused) = state.focus {
        let hint = if state.expanded.is_some() {
            "  Tab next · Esc collapse · j/k scroll"
        } else {
            "  Tab next · Enter expand · j/k scroll"
        };
        let _ = focused;
        out.push(Line::from(Span::styled(
            hint,
            Style::default().add_modifier(Modifier::DIM),
        )));
    }
    out.push(Line::default());

    // Top block: grid on the left, category legend on the right.
    // Computed together so both columns stay aligned even when the
    // legend has more rows than the grid (5) — we pad whichever
    // side is shorter with blank spans.
    out.extend(top_block_lines(b, inner_width));
    out.push(Line::default());

    // Nested sub-sections. Only rendered when non-empty.
    render_section(&mut out, b, state, Section::SystemPrompt);
    render_section(&mut out, b, state, Section::Tools);
    render_section(&mut out, b, state, Section::Skills);
    render_section(&mut out, b, state, Section::Memory);
    render_section(&mut out, b, state, Section::History);

    // Drop the last blank line if we pushed one — trailing blanks
    // render as empty lines at the bottom of the scroll view which
    // feels unfinished.
    while out.last().map(|l| l.spans.is_empty()).unwrap_or(false) {
        out.pop();
    }

    out
}

fn render_section(
    out: &mut Vec<Line<'static>>,
    b: &ContextBreakdown,
    state: ViewState,
    section: Section,
) {
    if !b.section_non_empty(section) {
        return;
    }
    let focused = state.focus == Some(section);
    let expanded = state.is_expanded(section);
    match section {
        Section::SystemPrompt => {
            out.push(section_heading_for(Section::SystemPrompt, focused, expanded));
            for s in &b.system_sections {
                out.push(section_row(&format!(" {}", s.name), s.tokens));
            }
            out.push(Line::default());
        }
        Section::Tools => {
            out.push(section_heading_for(Section::Tools, focused, expanded));
            if expanded {
                append_tools_expanded(out, &b.tools);
            } else {
                for t in &b.tools {
                    out.push(section_row(&format!(" {}", t.name), t.tokens));
                }
            }
            out.push(Line::default());
        }
        Section::Skills => {
            append_skill_section(out, &b.skills, focused, expanded);
        }
        Section::Memory => {
            append_memory_section(out, &b.memories, focused, expanded);
        }
        Section::History => {
            append_history_section(out, &b.history, focused, expanded);
        }
    }
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
            format!("  ({pct:.1}%)"),
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

/// A single grid cell (glyph + trailing space). Glyph choice mimics
/// Claude Code: filled block `⛁` for consumed tokens, empty `⛶` for
/// free space. Coloured by the category that owns the cell.
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
    for _ in 0..remaining_cells {
        out.push(grid_glyph(false, Color::DarkGray));
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
        Style::default().fg(Color::DarkGray),
    );
    Line::from(vec![mark, label, tokens, pct])
}

fn free_space_row(free_tokens: u32, limit: u32, label_width: usize) -> Line<'static> {
    let pct = if limit == 0 {
        0.0
    } else {
        free_tokens as f64 / limit as f64 * 100.0
    };
    let mark = Span::styled("⛶ ", Style::default().fg(Color::DarkGray));
    let label = Span::styled(
        format!("{:<w$}", "Free space", w = label_width),
        Style::default().add_modifier(Modifier::DIM),
    );
    let tokens = Span::styled(
        format!("  {:>7}", fmt_tokens(free_tokens)),
        Style::default().add_modifier(Modifier::DIM),
    );
    let pct_span = Span::styled(
        format!("  ({pct:>4.1}%)"),
        Style::default().fg(Color::DarkGray),
    );
    Line::from(vec![mark, label, tokens, pct_span])
}

// ─── Sub-sections ─────────────────────────────────────────────────

/// Skills sub-section. Skills have a `tokens=0` fallback: when the
/// runtime only records a selector shortlist (no per-skill token
/// counts), we still want to list the skill names. When the
/// section is expanded we also surface the shortlist description
/// and source.
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
        "Skills · /skills (shortlist)"
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
) {
    if h.is_empty() {
        return;
    }
    out.push(section_heading_for(Section::History, focused, expanded));
    // Collapsed view: just the aggregate counts.
    let mut turn_spans: Vec<Span<'static>> = vec![Span::raw("    └ ")];
    turn_spans.push(Span::raw(format!("{} turns", h.total_turns)));
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
        // Per-turn detail. Retained turns, then compressed, then a
        // terse list of dropped turn indices.
        let retained: Vec<&TurnDetail> =
            h.turns.iter().filter(|t| t.compressed_from.is_none()).collect();
        let compressed: Vec<&TurnDetail> =
            h.turns.iter().filter(|t| t.compressed_from.is_some()).collect();
        if !retained.is_empty() {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled("Retained", Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for t in retained {
                out.push(turn_detail_line(t, false));
            }
        }
        if !compressed.is_empty() {
            out.push(Line::from(vec![
                Span::raw("    └ "),
                Span::styled("Compressed", Style::default().add_modifier(Modifier::BOLD)),
            ]));
            for t in compressed {
                out.push(turn_detail_line(t, true));
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
}

fn turn_detail_line(t: &TurnDetail, compressed: bool) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = vec![Span::raw("        └ ")];
    spans.push(Span::raw(format!("#{} {}", t.index, t.role)));
    if compressed {
        if let Some((orig, method)) = &t.compressed_from {
            spans.push(Span::styled(
                format!(
                    "   {} → {} tokens",
                    fmt_tokens(*orig),
                    fmt_tokens(t.tokens)
                ),
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
                Style::default().fg(Color::Magenta),
            ));
        }
    }
    Line::from(spans)
}

fn append_tools_expanded(out: &mut Vec<Line<'static>>, tools: &[ToolItem]) {
    for t in tools {
        out.push(section_row(&format!(" {}", t.name), t.tokens));
        // Score + top-ranked factors. Each on its own indented line
        // so long factor names don't wrap weirdly.
        let score_span = Span::styled(
            format!("        score {:.2}", t.score),
            Style::default().fg(Color::DarkGray),
        );
        out.push(Line::from(vec![score_span]));
        for (name, weight) in t.factors.iter().take(3) {
            out.push(Line::from(vec![
                Span::raw("        · "),
                Span::raw(name.clone()),
                Span::styled(
                    format!("   {weight:+.2}"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
}

fn append_memory_section(
    out: &mut Vec<Line<'static>>,
    memories: &[MemoryItem],
    focused: bool,
    expanded: bool,
) {
    if memories.is_empty() {
        return;
    }
    out.push(section_heading_for(Section::Memory, focused, expanded));
    for m in memories {
        // Collapsed: truncated preview. Expanded: full preview on
        // its own line so the user can read without wrapping.
        let preview_len = if expanded { 160 } else { 60 };
        let preview = truncate_preview(&m.preview, preview_len);
        out.push(Line::from(vec![
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
        ]));
        if expanded {
            out.push(Line::from(vec![
                Span::raw("        "),
                Span::styled(
                    format!("{} · {}", m.memory_type, m.source),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }
    }
    out.push(Line::default());
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
            .fg(Color::Cyan)
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
    let pct = b.usage_percent();
    let headline = format!(" Context window ({pct:.0}% · {}) ", band.label());
    Line::from(vec![Span::styled(
        headline,
        Style::default()
            .fg(band.color())
            .add_modifier(Modifier::BOLD),
    )])
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
    use super::super::model::ContextBreakdown;
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use astra_turn_core::context_assembly_trace::{
        CompressionMethod, ContextAssemblyTrace, HistorySelectionTrace, MemorySelection,
        MemorySource, SkillInjection, SystemPromptBreakdown, TokenBudgetTrace, ToolSelected,
        TurnCompression, TurnRetention,
    };
    use astra_turn_core::skill_selector_metrics::{
        SkillSelectorShortlistEntry, SkillSelectorShortlistTrace,
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
        let b =
            ContextBreakdown::from_trace(&trace(100_000, 2_000, 15_000, 500, 4_000, 200));
        insta::assert_snapshot!("context_panel_low_80x14", render_panel(&b, 80, 14));
    }

    #[test]
    fn snapshot_warning_pressure_80x14() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 8_000, 50_000, 1_000, 10_000, 1_500));
        insta::assert_snapshot!("context_panel_warn_80x14", render_panel(&b, 80, 14));
    }

    #[test]
    fn snapshot_critical_pressure_80x14() {
        let b =
            ContextBreakdown::from_trace(&trace(100_000, 12_000, 70_000, 2_000, 10_000, 1_500));
        insta::assert_snapshot!("context_panel_critical_80x14", render_panel(&b, 80, 14));
    }

    #[test]
    fn snapshot_empty_no_trace_80x3() {
        let b = ContextBreakdown::empty();
        insta::assert_snapshot!("context_panel_empty_80x3", render_panel(&b, 80, 3));
    }

    #[test]
    fn snapshot_with_history_and_shortlist_80x28() {
        // Mirrors a real runtime trace: the selector recorded a
        // shortlist (no per-skill tokens) and the compactor trimmed
        // the history aggressively. Both sections must render.
        let mut t = trace(102_400, 6_000, 22_000, 0, 6_300, 227);
        t.skill_selector = Some(SkillSelectorShortlistTrace {
            open_catalog: false,
            visible_skill_count: 3,
            skills: vec![
                SkillSelectorShortlistEntry {
                    rank: 1,
                    skill_name: "review_changes".into(),
                    aliases: Vec::new(),
                    description: String::new(),
                    source: "built-in".into(),
                    category: None,
                },
                SkillSelectorShortlistEntry {
                    rank: 2,
                    skill_name: "verify_task".into(),
                    aliases: Vec::new(),
                    description: String::new(),
                    source: "built-in".into(),
                    category: None,
                },
            ],
            telemetry: Default::default(),
        });
        t.history = HistorySelectionTrace {
            total_turns_available: 8,
            turns_retained: vec![TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 300,
                has_tool_calls: false,
            }],
            turns_compressed: vec![TurnCompression {
                turn_index: 1,
                role: "assistant".into(),
                original_tokens: 20_000,
                compressed_tokens: 5_000,
                compression_method: CompressionMethod::ReactiveCompact,
                information_lost: Vec::new(),
            }],
            turns_dropped: vec![2, 3],
            compression_ratio: 0.25,
            tokens_before: 32_000,
            tokens_after: 22_000,
        };
        t.tools.tools_selected = vec![
            ToolSelected {
                tool_name: "bash".into(),
                score: 0.9,
                tokens: 189,
                selection_factors: Vec::new(),
            },
            ToolSelected {
                tool_name: "read_file".into(),
                score: 0.8,
                tokens: 152,
                selection_factors: Vec::new(),
            },
        ];
        let b = ContextBreakdown::from_trace(&t);
        insta::assert_snapshot!("context_panel_history_shortlist_80x28", render_panel(&b, 80, 28));
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
    fn build_lines_renders_shortlist_skills_without_tokens() {
        let mut t = trace(100_000, 1_000, 1_000, 0, 0, 0);
        t.skill_selector = Some(SkillSelectorShortlistTrace {
            open_catalog: false,
            visible_skill_count: 1,
            skills: vec![SkillSelectorShortlistEntry {
                rank: 1,
                skill_name: "my_skill".into(),
                aliases: Vec::new(),
                description: String::new(),
                source: "built-in".into(),
                category: None,
            }],
            telemetry: Default::default(),
        });
        let b = ContextBreakdown::from_trace(&t);
        let text: String = build_lines(&b, 80)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("shortlist"), "shortlist label missing: {text}");
        assert!(text.contains("my_skill"), "skill name missing: {text}");
        // No fake "0 tokens" noise for shortlist entries.
        assert!(
            !text.contains("my_skill   0 tokens"),
            "shortlist row should not show 0-token count: {text}"
        );
    }

    #[test]
    fn snapshot_with_nested_sections_80x26() {
        let mut t = trace(100_000, 4_000, 20_000, 2_000, 6_000, 500);
        t.tools.tools_selected = vec![
            ToolSelected {
                tool_name: "read_file".into(),
                score: 0.9,
                tokens: 1_200,
                selection_factors: Vec::new(),
            },
            ToolSelected {
                tool_name: "write_file".into(),
                score: 0.8,
                tokens: 900,
                selection_factors: Vec::new(),
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
        insta::assert_snapshot!("context_panel_nested_80x26", render_panel(&b, 80, 26));
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
        t.tools.tools_selected = (0..30)
            .map(|i| ToolSelected {
                tool_name: format!("t{i}"),
                score: 0.5,
                tokens: 10,
                selection_factors: Vec::new(),
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
        assert!(text.contains("Free space"), "free space row missing: {text}");
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
    fn expanded_history_section_shows_per_turn_detail() {
        let mut t = trace(100_000, 2_000, 8_000, 0, 0, 0);
        t.history.total_turns_available = 4;
        t.history.turns_retained = vec![
            TurnRetention {
                turn_index: 0,
                role: "user".into(),
                tokens: 180,
                has_tool_calls: false,
            },
            TurnRetention {
                turn_index: 2,
                role: "assistant".into(),
                tokens: 4_200,
                has_tool_calls: true,
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
        assert!(
            text.contains("Dropped: #3"),
            "dropped turn missing: {text}"
        );
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
        t.tools.tools_selected = vec![ToolSelected {
            tool_name: "bash".into(),
            score: 0.9,
            tokens: 100,
            selection_factors: Vec::new(),
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
    fn expanded_section_heading_has_expand_marker() {
        let mut t = trace(100_000, 2_000, 0, 0, 1_000, 0);
        t.tools.tools_selected = vec![ToolSelected {
            tool_name: "bash".into(),
            score: 0.9,
            tokens: 100,
            selection_factors: Vec::new(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        let state = ViewState {
            focus: Some(Section::Tools),
            expanded: Some(Section::Tools),
        };
        let text: String = build_lines_with(&b, 80, state)
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(text.contains("▼"), "expand marker missing: {text}");
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
        t.tools.tools_selected = vec![ToolSelected {
            tool_name: "x".into(),
            score: 0.5,
            tokens: 100,
            selection_factors: Vec::new(),
        }];
        let b = ContextBreakdown::from_trace(&t);
        assert_eq!(line_count(&b, 80) as usize, build_lines(&b, 80).len());
    }

    #[test]
    fn grid_uses_fifty_cells() {
        let b = ContextBreakdown::from_trace(&trace(100_000, 10_000, 0, 0, 0, 0));
        let cells = render_grid_cells(&b);
        assert_eq!(cells.len(), GRID_CELLS);
        assert_eq!(cells.len(), 50, "5 × 10 grid is Claude Code's density");
    }
}
