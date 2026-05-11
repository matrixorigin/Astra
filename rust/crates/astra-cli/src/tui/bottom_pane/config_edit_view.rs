//! TUI-native `/config edit` — a two-column list+detail view with
//! inline per-kind editors pushed as a child view on Enter.
//!
//! Modelled after the reference implementation's Config.tsx:
//!   * flat catalog of `SettingItem` rows built from the resolved
//!     `RuntimeConfig` (see `astra_config::config_overlay`);
//!   * left column = searchable list with a live `filter` string that
//!     types/backspaces into the search state;
//!   * right column = detail pane showing label / id / kind metadata /
//!     current value for the highlighted row;
//!   * Enter opens an inline editor (Bool toggle, Number input, Enum
//!     selection) scoped to that row; the child lives inside this
//!     view (no extra push on the `BottomPane` stack) so cancel/accept
//!     routing is local and the parent keeps its filter/position;
//!   * Esc in a child cancels the child; Esc on a clean outer view
//!     exits cleanly; Esc on a dirty outer view surfaces a save prompt
//!     (SaveToUser / SaveToProject / Discard / Keep editing).
//!
//! Writing the resolved config back to disk is the caller's job — the
//! view merely reports a `ConfigEditAction` via `pending_action()`.
//! The routing layer in `tui/mod.rs` reads that, writes the TOML to
//! the chosen scope (via the same helper `slash_config` uses in
//! line-mode), and reloads the process-wide overlay.

use astra_config::config_overlay::{
    SettingItem, SettingKind, apply_edit, build_settings_catalog, filter_settings,
};
use astra_config::runtime_config::RuntimeConfig;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use serde_json::Value;

use super::view::{BottomPaneView, CancellationEvent, ViewCompletion};

// ─── Public API ──────────────────────────────────────────────────────────

/// What the caller (`tui::mod`) should do after this view completes.
/// The view never touches disk itself — it only decides the next step.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConfigEditAction {
    /// View still live, nothing to do yet.
    None,
    /// Outer Esc on a clean view — discard nothing, close the panel.
    Cancelled,
    /// Dirty save-prompt is currently on screen; caller should not
    /// pop the view yet. This mainly matters for the test surface.
    PromptingSave,
    /// User picked "Save to ~/.astra/config/runtime.toml".
    SaveToUser,
    /// User picked "Save to ./.astra/config/runtime.toml".
    SaveToProject,
    /// User picked "Discard changes" from the save prompt.
    Discarded,
}

/// Outer view. Owns the working RuntimeConfig, the original snapshot
/// (for revert), filter state, selection index, and an optional inner
/// editor view.
pub(crate) struct ConfigEditView {
    /// Pre-edit snapshot. Keeping this lets us expose a "Discard" path
    /// that doesn't require a reload pass on the caller side: we hand
    /// it back unchanged and the caller knows nothing shipped.
    original: RuntimeConfig,
    /// Current edited state. `apply_edit` writes into this; `is_dirty`
    /// compares it against `original`.
    working: RuntimeConfig,
    filter: String,
    selected: usize,
    /// Inline per-kind editor. `Some` means the parent passes keys
    /// down to the inner widget instead of handling them itself.
    inner: Option<Box<dyn InnerEditor>>,
    /// Dirty-esc save prompt. `Some` means the outer view is in
    /// "confirm what to do" mode; the list/detail drop and we render a
    /// 3-way choice instead.
    save_prompt: Option<SavePrompt>,
    completed: bool,
    action: ConfigEditAction,
}

impl ConfigEditView {
    pub(crate) fn new(config: RuntimeConfig) -> Self {
        Self {
            original: config.clone(),
            working: config,
            filter: String::new(),
            selected: 0,
            inner: None,
            save_prompt: None,
            completed: false,
            action: ConfigEditAction::None,
        }
    }

    /// Consume the view and yield the edited config. Callers use this
    /// after observing `pending_action() == SaveToUser | SaveToProject`
    /// to get the final snapshot to persist.
    pub(crate) fn into_working(self) -> RuntimeConfig {
        self.working
    }

    pub(crate) fn pending_action(&self) -> ConfigEditAction {
        self.action.clone()
    }

    pub(crate) fn is_dirty(&self) -> bool {
        // Structural comparison via TOML serialization. Comparing the
        // struct itself would require `PartialEq` on RuntimeConfig and
        // all its sub-configs; serialising to TOML and comparing string
        // output is exact enough and avoids that blast radius.
        match (
            toml::to_string(&self.original),
            toml::to_string(&self.working),
        ) {
            (Ok(a), Ok(b)) => a != b,
            _ => true, // if either fails to serialise, play it safe
        }
    }

    // ── test-only accessors ────────────────────────────────────────

    #[cfg(test)]
    pub(crate) fn visible_ids(&self) -> Vec<String> {
        self.filtered_catalog()
            .iter()
            .map(|i| i.id.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn select_by_id(&mut self, id: &str) {
        let catalog = build_settings_catalog(&self.working);
        let target = catalog.iter().position(|i| i.id == id);
        if let Some(pos) = target {
            // The view's `selected` indexes the *filtered* list; clear
            // the filter so the catalog and the filtered list are 1:1.
            self.filter.clear();
            self.selected = pos;
        }
    }

    #[cfg(test)]
    pub(crate) fn has_inner_editor(&self) -> bool {
        self.inner.is_some()
    }

    #[cfg(test)]
    pub(crate) fn inner_editor_kind(&self) -> Option<&'static str> {
        self.inner.as_ref().map(|e| e.kind_label())
    }

    #[cfg(test)]
    pub(crate) fn working_config_for_test(&self) -> &RuntimeConfig {
        &self.working
    }

    // ── internal ───────────────────────────────────────────────────

    fn filtered_catalog(&self) -> Vec<SettingItem> {
        let catalog = build_settings_catalog(&self.working);
        filter_settings(&catalog, &self.filter)
    }

    fn current_item(&self) -> Option<SettingItem> {
        let filtered = self.filtered_catalog();
        filtered.get(self.selected).cloned()
    }

    fn open_editor_for(&mut self, item: SettingItem) {
        let editor: Box<dyn InnerEditor> = match &item.kind {
            SettingKind::Bool => Box::new(BoolEditor::new(&item)),
            SettingKind::Number { .. } => Box::new(NumberEditor::new(&item)),
            SettingKind::Enum { options } => Box::new(EnumEditor::new(&item, options.clone())),
        };
        self.inner = Some(editor);
    }

    fn apply_inner_result(&mut self, item_id: String, value: Value) {
        match apply_edit(self.working.clone(), &item_id, value) {
            Ok(next) => self.working = next,
            Err(_e) => {
                // The per-kind editor's validator should have caught
                // this; if apply_edit still rejects (type coercion
                // quirk, e.g. Number out of u32 range), we drop the
                // edit silently. A future refinement could surface a
                // toast via the save-prompt footer area.
            }
        }
    }

    fn handle_outer_key(&mut self, key: KeyEvent) {
        let filtered = self.filtered_catalog();
        let len = filtered.len().max(1);
        match key.code {
            KeyCode::Up => {
                self.selected = if self.selected == 0 {
                    len - 1
                } else {
                    self.selected - 1
                };
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % len;
            }
            KeyCode::Enter => {
                if let Some(item) = self.current_item() {
                    self.open_editor_for(item);
                }
            }
            KeyCode::Esc => {
                if self.is_dirty() {
                    self.save_prompt = Some(SavePrompt::new());
                    self.action = ConfigEditAction::PromptingSave;
                } else {
                    self.completed = true;
                    self.action = ConfigEditAction::Cancelled;
                }
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.selected = 0;
            }
            _ => {}
        }
    }
}

impl BottomPaneView for ConfigEditView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Save prompt takes over the whole area when active.
        if let Some(ref prompt) = self.save_prompt {
            render_save_prompt(prompt, area, buf);
            return;
        }

        // Two-column split on sufficient width, single-column otherwise.
        let split_at_col = 45u16;
        let two_col = area.width >= split_at_col;
        let layout = if two_col {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(area)
        } else {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(100)])
                .split(area)
        };

        let list_area = layout[0];
        self.render_list(list_area, buf);

        if two_col && layout.len() > 1 {
            self.render_detail(layout[1], buf);
        }

        // Inner editor (if any) pins to the bottom of the list area,
        // overlaying its last rows. This matches the reference Pane
        // layout where the active editor sits in place of the footer.
        if let Some(ref inner) = self.inner {
            let h = inner.desired_height(list_area.width).min(list_area.height);
            let editor_area = Rect::new(
                list_area.x,
                list_area.y + list_area.height.saturating_sub(h),
                list_area.width,
                h,
            );
            // Clear the overlay region by overpainting blanks first.
            for y in editor_area.y..editor_area.y + editor_area.height {
                for x in editor_area.x..editor_area.x + editor_area.width {
                    buf[(x, y)].set_symbol(" ");
                }
            }
            inner.render(editor_area, buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // Enough for:
        //   1 search line
        //   12 list rows (same MAX_VISIBLE as ListSelectionView)
        //   1 blank
        //   1 hint
        // Total = 15. The save prompt needs only ~6 rows and reuses
        // this height; the difference is padding at the bottom, no
        // harm done.
        15
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Save prompt takes precedence.
        if let Some(ref mut prompt) = self.save_prompt {
            match prompt.handle_key(key) {
                SavePromptOutcome::Pending => {}
                SavePromptOutcome::SaveToUser => {
                    self.save_prompt = None;
                    self.action = ConfigEditAction::SaveToUser;
                    self.completed = true;
                }
                SavePromptOutcome::SaveToProject => {
                    self.save_prompt = None;
                    self.action = ConfigEditAction::SaveToProject;
                    self.completed = true;
                }
                SavePromptOutcome::Discard => {
                    self.working = self.original.clone();
                    self.save_prompt = None;
                    self.action = ConfigEditAction::Discarded;
                    self.completed = true;
                }
                SavePromptOutcome::BackToEdit => {
                    self.save_prompt = None;
                    self.action = ConfigEditAction::None;
                }
            }
            return;
        }

        // Inner editor absorbs keys while open.
        if let Some(ref mut inner) = self.inner {
            match inner.handle_key(key) {
                InnerOutcome::Pending => {}
                InnerOutcome::Cancel => {
                    self.inner = None;
                }
                InnerOutcome::Accept(value) => {
                    let id = inner.item_id().to_string();
                    self.inner = None;
                    self.apply_inner_result(id, value);
                }
            }
            return;
        }

        self.handle_outer_key(key);
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        // Only NumberEditor needs a cursor; a follow-up can compute
        // the inner editor's absolute position. Returning None here
        // hides the caret during editing, which is still usable.
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.working = self.original.clone();
        self.completed = true;
        self.action = ConfigEditAction::Cancelled;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        if !self.completed {
            return None;
        }
        // The token is `__config_edit__\n<action>\n<toml-body>`.
        // Callers split on the first two newlines; body is absent for
        // Cancelled / Discarded (nothing to persist). TOML is inline
        // because `completion()` is `&self` — we can't move `self.working`
        // out without a trait-level change, and cloning is cheap (~KBs).
        let (tag, want_toml) = match self.action {
            ConfigEditAction::SaveToUser => ("save_user", true),
            ConfigEditAction::SaveToProject => ("save_project", true),
            ConfigEditAction::Discarded => ("discard", false),
            ConfigEditAction::Cancelled => ("cancel", false),
            _ => return None,
        };
        let body = if want_toml {
            toml::to_string(&self.working).unwrap_or_default()
        } else {
            String::new()
        };
        Some(ViewCompletion {
            result: Some(format!("__config_edit__\n{tag}\n{body}")),
            reopen: None,
        })
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        // We handle Esc with state (dirty vs clean vs in-child). The
        // BottomPane dispatcher must route Esc to us first.
        true
    }

    fn hint_keys(&self) -> Option<String> {
        if self.save_prompt.is_some() {
            return Some("1 user · 2 project · d discard · Esc back".into());
        }
        if let Some(ref inner) = self.inner {
            return Some(inner.hint_keys().to_string());
        }
        Some("↑↓ navigate · Enter edit · type to search · Esc save/close".into())
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

impl ConfigEditView {
    fn render_list(&self, area: Rect, buf: &mut Buffer) {
        let dim = Style::default().fg(Color::DarkGray);
        let sel_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);

        let mut y = area.y;

        // Search line
        if y < area.bottom() {
            let label = if self.filter.is_empty() {
                "  Search: (type to filter) ".to_string()
            } else {
                format!("  Search: {} ", self.filter)
            };
            Widget::render(
                Line::from(Span::styled(label, dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        // List rows
        let filtered = self.filtered_catalog();
        const MAX_VISIBLE: usize = 12;
        let start = if self.selected >= MAX_VISIBLE {
            self.selected - MAX_VISIBLE + 1
        } else {
            0
        };
        let end = (start + MAX_VISIBLE).min(filtered.len());

        for (vi, item) in filtered[start..end].iter().enumerate() {
            if y >= area.bottom() {
                break;
            }
            let idx = start + vi;
            let is_sel = idx == self.selected;
            let marker = if is_sel { "› " } else { "  " };
            let current = render_value_short(&item.value);
            let raw = format!("{}{}  {} ", marker, short_id(&item.id), current);
            let line_text = truncate_line(&raw, area.width as usize);
            let span = if is_sel {
                Span::styled(line_text, sel_style)
            } else {
                Span::raw(line_text)
            };
            Widget::render(Line::from(span), Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }
        if filtered.is_empty() && y < area.bottom() {
            Widget::render(
                Line::from(Span::styled("  no matches", dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
        }
    }

    fn render_detail(&self, area: Rect, buf: &mut Buffer) {
        let dim = Style::default().fg(Color::DarkGray);
        let head = Style::default().add_modifier(Modifier::BOLD);

        let item = match self.current_item() {
            Some(i) => i,
            None => return,
        };

        let mut lines = vec![
            Line::from(Span::styled("  Details ", head)),
            Line::from(Span::raw("")),
            Line::from(vec![
                Span::styled("  ", dim),
                Span::styled(item.label.clone(), head),
            ]),
            Line::from(vec![
                Span::styled("  id:    ", dim),
                Span::raw(item.id.clone()),
            ]),
        ];
        let kind_desc = match &item.kind {
            SettingKind::Bool => "bool".to_string(),
            SettingKind::Number { min, max } => format!("number  range {min}..={max}"),
            SettingKind::Enum { options } => {
                let shown: String = options
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" / ");
                format!("enum  {shown}")
            }
        };
        lines.push(Line::from(vec![
            Span::styled("  kind:  ", dim),
            Span::raw(kind_desc),
        ]));
        lines.push(Line::from(vec![
            Span::styled("  value: ", dim),
            Span::styled(
                render_value_short(&item.value),
                Style::default().fg(Color::Yellow),
            ),
        ]));
        if self.is_dirty() {
            lines.push(Line::from(Span::raw("")));
            lines.push(Line::from(Span::styled(
                "  * unsaved changes",
                Style::default().fg(Color::Yellow),
            )));
        }

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        Widget::render(para, area, buf);
    }
}

fn render_value_short(v: &Value) -> String {
    match v {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// Drop the dotted prefix so the list row fits comfortably in the
/// 55% list column. Keep the tail segment, which is the human-readable
/// leaf name (`max_turn_input_tokens`, `adaptive_budget_reduction`).
fn short_id(id: &str) -> String {
    match id.rsplit_once('.') {
        Some((_, tail)) => tail.to_string(),
        None => id.to_string(),
    }
}

fn truncate_line(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ─── Inner editors ───────────────────────────────────────────────────────

trait InnerEditor: Send {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, _width: u16) -> u16 {
        3
    }
    fn handle_key(&mut self, key: KeyEvent) -> InnerOutcome;
    fn item_id(&self) -> &str;
    fn hint_keys(&self) -> &'static str;
    fn kind_label(&self) -> &'static str;
}

enum InnerOutcome {
    Pending,
    Cancel,
    Accept(Value),
}

// ── Bool ──────────────────────────────────────────────────────────────
//
// Two-option picker — same visual pattern as EnumEditor so users have
// a single mental model for "pick one of these". Space/Tab keeps the
// "flip" shortcut from the earlier single-value editor for muscle
// memory, but the primary UI is ↑↓ + Enter.

struct BoolEditor {
    id: String,
    label: String,
    /// 0 = false row, 1 = true row. Initialised to the current value so
    /// the cursor opens on whichever option is live.
    selected: usize,
}

impl BoolEditor {
    fn new(item: &SettingItem) -> Self {
        let current = item.value_as_bool().unwrap_or(false);
        Self {
            id: item.id.clone(),
            label: item.label.clone(),
            selected: if current { 1 } else { 0 },
        }
    }

    fn current_value(&self) -> bool {
        self.selected == 1
    }
}

impl InnerEditor for BoolEditor {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let hint = Style::default().fg(Color::DarkGray);
        let sel = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::from(Span::styled(
            format!("  ▶ {} ", self.label),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (i, label) in ["false", "true"].iter().enumerate() {
            let marker = if i == self.selected { "› " } else { "  " };
            let line = Line::from(vec![
                Span::styled("  ", hint),
                Span::styled(
                    format!("{}{}", marker, label),
                    if i == self.selected { sel } else { hint },
                ),
            ]);
            lines.push(line);
        }
        lines.push(Line::from(Span::styled(
            "  (↑↓ move, space toggle, Enter save, Esc cancel)",
            hint,
        )));
        Widget::render(Paragraph::new(lines), area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // label + 2 options + hint = 4 rows
        4
    }

    fn handle_key(&mut self, key: KeyEvent) -> InnerOutcome {
        match key.code {
            KeyCode::Up | KeyCode::Down | KeyCode::Char(' ') | KeyCode::Tab => {
                // All four keys swap the selection. Arrow keys match
                // EnumEditor (the norm); space/tab preserves the old
                // "flip" muscle memory.
                self.selected = 1 - self.selected;
                InnerOutcome::Pending
            }
            KeyCode::Enter => InnerOutcome::Accept(Value::Bool(self.current_value())),
            KeyCode::Esc => InnerOutcome::Cancel,
            _ => InnerOutcome::Pending,
        }
    }

    fn item_id(&self) -> &str {
        &self.id
    }
    fn hint_keys(&self) -> &'static str {
        "↑↓ move · space toggle · Enter save · Esc cancel"
    }
    fn kind_label(&self) -> &'static str {
        "bool"
    }
}

// ── Number ────────────────────────────────────────────────────────────

struct NumberEditor {
    id: String,
    label: String,
    buffer: String,
    error: Option<String>,
    min: i64,
    max: i64,
}

impl NumberEditor {
    fn new(item: &SettingItem) -> Self {
        let (min, max) = match &item.kind {
            SettingKind::Number { min, max } => (*min, *max),
            _ => (i64::MIN, i64::MAX),
        };
        let buffer = item
            .value_as_number()
            .map(|n| {
                if n.fract() == 0.0 {
                    format!("{}", n as i64)
                } else {
                    n.to_string()
                }
            })
            .unwrap_or_default();
        Self {
            id: item.id.clone(),
            label: item.label.clone(),
            buffer,
            error: None,
            min,
            max,
        }
    }

    fn try_commit(&mut self) -> Option<Value> {
        // Accept integer or integer-valued float; reject anything that
        // would silently round. Range check against declared min/max.
        let parsed: Result<f64, _> = self.buffer.trim().parse();
        match parsed {
            Ok(n) if n.is_finite() => {
                if n.fract() != 0.0 {
                    self.error = Some(format!("Fractional values not allowed: {}", self.buffer));
                    return None;
                }
                let as_i64 = n as i64;
                if as_i64 < self.min || as_i64 > self.max {
                    self.error = Some(format!(
                        "Out of range: {} not in [{}, {}]",
                        self.buffer, self.min, self.max
                    ));
                    return None;
                }
                self.error = None;
                Some(Value::from(as_i64 as u64))
            }
            _ => {
                self.error = Some(format!("Not a number: {}", self.buffer));
                None
            }
        }
    }
}

impl InnerEditor for NumberEditor {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let hint = Style::default().fg(Color::DarkGray);
        let val_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![
            Line::from(Span::styled(
                format!("  ▶ {} ", self.label),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(vec![
                Span::styled("  value: ", hint),
                Span::styled(self.buffer.clone(), val_style),
                Span::styled(
                    format!(
                        "    (range {}..={}, Enter saves, Esc cancels)",
                        self.min, self.max
                    ),
                    hint,
                ),
            ]),
        ];
        if let Some(ref err) = self.error {
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(Color::Red),
            )));
        }
        Widget::render(Paragraph::new(lines), area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        if self.error.is_some() { 4 } else { 3 }
    }

    fn handle_key(&mut self, key: KeyEvent) -> InnerOutcome {
        match key.code {
            KeyCode::Esc => InnerOutcome::Cancel,
            KeyCode::Enter => match self.try_commit() {
                Some(v) => InnerOutcome::Accept(v),
                None => InnerOutcome::Pending,
            },
            KeyCode::Backspace => {
                self.buffer.pop();
                self.error = None;
                InnerOutcome::Pending
            }
            KeyCode::Char(c) if c.is_ascii_digit() || c == '-' => {
                self.buffer.push(c);
                self.error = None;
                InnerOutcome::Pending
            }
            _ => InnerOutcome::Pending,
        }
    }

    fn item_id(&self) -> &str {
        &self.id
    }
    fn hint_keys(&self) -> &'static str {
        "digits to edit · Backspace erase · Enter save · Esc cancel"
    }
    fn kind_label(&self) -> &'static str {
        "number"
    }
}

// ── Enum ──────────────────────────────────────────────────────────────

struct EnumEditor {
    id: String,
    label: String,
    options: Vec<String>,
    selected: usize,
}

impl EnumEditor {
    fn new(item: &SettingItem, options: Vec<String>) -> Self {
        let current = item.value_as_string().unwrap_or_default();
        let selected = options.iter().position(|o| o == &current).unwrap_or(0);
        Self {
            id: item.id.clone(),
            label: item.label.clone(),
            options,
            selected,
        }
    }
}

impl InnerEditor for EnumEditor {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let hint = Style::default().fg(Color::DarkGray);
        let sel = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::from(Span::styled(
            format!("  ▶ {} ", self.label),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (i, o) in self.options.iter().enumerate() {
            let marker = if i == self.selected { "› " } else { "  " };
            let line = Line::from(vec![
                Span::styled("  ", hint),
                Span::styled(
                    format!("{}{}", marker, o),
                    if i == self.selected { sel } else { hint },
                ),
            ]);
            lines.push(line);
        }
        lines.push(Line::from(Span::styled(
            "  (↑↓ move, Enter saves, Esc cancels)",
            hint,
        )));
        Widget::render(Paragraph::new(lines), area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        (2 + self.options.len() as u16).min(8)
    }

    fn handle_key(&mut self, key: KeyEvent) -> InnerOutcome {
        let len = self.options.len().max(1);
        match key.code {
            KeyCode::Up => {
                self.selected = if self.selected == 0 {
                    len - 1
                } else {
                    self.selected - 1
                };
                InnerOutcome::Pending
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % len;
                InnerOutcome::Pending
            }
            KeyCode::Enter => {
                let v = self.options.get(self.selected).cloned().unwrap_or_default();
                InnerOutcome::Accept(Value::String(v))
            }
            KeyCode::Esc => InnerOutcome::Cancel,
            _ => InnerOutcome::Pending,
        }
    }

    fn item_id(&self) -> &str {
        &self.id
    }
    fn hint_keys(&self) -> &'static str {
        "↑↓ move · Enter save · Esc cancel"
    }
    fn kind_label(&self) -> &'static str {
        "enum"
    }
}

// ─── Save prompt ─────────────────────────────────────────────────────────

struct SavePrompt;

impl SavePrompt {
    fn new() -> Self {
        Self
    }
    fn handle_key(&mut self, key: KeyEvent) -> SavePromptOutcome {
        match key.code {
            KeyCode::Char('1') | KeyCode::Char('u') | KeyCode::Char('U') => {
                SavePromptOutcome::SaveToUser
            }
            KeyCode::Char('2') | KeyCode::Char('p') | KeyCode::Char('P') => {
                SavePromptOutcome::SaveToProject
            }
            KeyCode::Char('d') | KeyCode::Char('D') => SavePromptOutcome::Discard,
            KeyCode::Esc | KeyCode::Char('b') | KeyCode::Char('B') => SavePromptOutcome::BackToEdit,
            _ => SavePromptOutcome::Pending,
        }
    }
}

enum SavePromptOutcome {
    Pending,
    SaveToUser,
    SaveToProject,
    Discard,
    BackToEdit,
}

fn render_save_prompt(_p: &SavePrompt, area: Rect, buf: &mut Buffer) {
    let head = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(Span::styled("  Save changes?", head)),
        Line::from(Span::raw("")),
        Line::from(vec![
            Span::styled("  [1] ", dim),
            Span::raw("Save to ~/.astra/config/runtime.toml (user)"),
        ]),
        Line::from(vec![
            Span::styled("  [2] ", dim),
            Span::raw("Save to ./.astra/config/runtime.toml (project)"),
        ]),
        Line::from(vec![
            Span::styled("  [d] ", dim),
            Span::raw("Discard changes"),
        ]),
        Line::from(vec![
            Span::styled("  [Esc] ", dim),
            Span::raw("Back to edit"),
        ]),
    ];
    Widget::render(Paragraph::new(lines), area, buf);
}
