//! Search and edit user or project defaults. The existing config router
//! handles the typed save action and applies the effective settings.

use astra_config::config_overlay::{
    SettingItem, SettingKind, apply_edit, build_settings_catalog, filter_settings,
};
use astra_config::runtime_config::RuntimeConfig;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget, Wrap},
};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::tui::{event::is_text_key, history_cell::truncate_by_width};

use super::view::{
    BottomPaneView, CancellationEvent, ConfigEditDisposition, ViewCompletion, ViewResult,
};

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
    /// 4-way choice instead.
    save_prompt: Option<SavePrompt>,
    /// Save-prompt → Preview sub-state. `true` means we render the
    /// diff view instead of the prompt; any key (including Esc)
    /// returns to the prompt with state preserved.
    preview: bool,
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
            preview: false,
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
    pub(crate) fn selected_id_for_test(&self) -> Option<String> {
        self.current_item().map(|i| i.id)
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

    #[cfg(test)]
    pub(crate) fn save_prompt_open_for_test(&self) -> bool {
        self.save_prompt.is_some()
    }

    #[cfg(test)]
    pub(crate) fn preview_open_for_test(&self) -> bool {
        self.preview
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
            SettingKind::Number { .. } | SettingKind::Text => Box::new(ValueEditor::new(&item)),
            SettingKind::Enum { options } => Box::new(EnumEditor::new(&item, options.clone())),
        };
        self.inner = Some(editor);
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
                let previous_id = self.current_item().map(|i| i.id);
                self.filter.pop();
                let filtered = self.filtered_catalog();
                self.selected = previous_id
                    .and_then(|id| filtered.iter().position(|item| item.id == id))
                    .unwrap_or(0);
            }
            KeyCode::Char(c) if is_text_key(&key) => {
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

        // Preview layer — rendered on top of the (still-alive) save
        // prompt state so returning to the prompt is just a flag flip.
        if self.preview {
            render_preview(&self.original, &self.working, area, buf);
            return;
        }
        // Save prompt takes over the whole area when active.
        if let Some(ref prompt) = self.save_prompt {
            render_save_prompt(prompt, area, buf);
            return;
        }

        // Two-column split on sufficient width, single-column otherwise.
        let split_at_col = 74;
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

    fn handle_paste(&mut self, text: &str) -> bool {
        self.inner
            .as_mut()
            .is_some_and(|editor| editor.handle_paste(text))
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
        // Preview absorbs everything — any key returns to the prompt
        // without committing. This mirrors `context_panel_view`'s
        // "press anything to close" semantics and keeps the diff view
        // strictly read-only.
        if self.preview {
            self.preview = false;
            return;
        }

        // Save prompt takes precedence over the edit list.
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
                SavePromptOutcome::OpenPreview => {
                    // Prompt stays open under the preview layer;
                    // handle_key will flip `preview` back off on any
                    // key press.
                    self.preview = true;
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
                    match apply_edit(self.working.clone(), &id, value) {
                        Ok(next) => {
                            self.working = next;
                            self.inner = None;
                        }
                        Err(err) => {
                            inner.set_error(err.to_string());
                        }
                    }
                }
            }
            return;
        }

        self.handle_outer_key(key);
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
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
        // TOML stays inline because `completion()` only has `&self`; cloning
        // this small snapshot is preferable to inventing a textual protocol.
        let (disposition, want_toml) = match self.action {
            ConfigEditAction::SaveToUser => (ConfigEditDisposition::SaveUser, true),
            ConfigEditAction::SaveToProject => (ConfigEditDisposition::SaveProject, true),
            ConfigEditAction::Discarded => (ConfigEditDisposition::Discard, false),
            ConfigEditAction::Cancelled => (ConfigEditDisposition::Cancel, false),
            _ => return None,
        };
        let body = if want_toml {
            toml::to_string(&self.working).unwrap_or_default()
        } else {
            String::new()
        };
        Some(ViewCompletion {
            result: Some(ViewResult::ConfigEdit {
                disposition,
                toml_body: body,
            }),
            reopen: None,
        })
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        // We handle Esc with state (dirty vs clean vs in-child). The
        // BottomPane dispatcher must route Esc to us first.
        true
    }

    fn hint_keys(&self) -> Option<String> {
        if self.preview {
            return Some("Esc back".into());
        }
        if self.save_prompt.is_some() {
            return Some("↑↓ select · Enter confirm · Esc back".into());
        }
        if let Some(ref inner) = self.inner {
            return Some(inner.hint_keys().to_string());
        }
        Some("↑↓ select · Enter edit · Esc finish editing · Type to filter".into())
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

impl ConfigEditView {
    fn render_list(&self, area: Rect, buf: &mut Buffer) {
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(theme.dim);
        let selected = Style::default()
            .fg(theme.selected_fg)
            .bg(theme.selected_bg)
            .bold();
        let width = usize::from(area.width.saturating_sub(2));
        let mut y = area.y;
        if area.height >= 3 {
            let label = if self.filter.is_empty() {
                "  Settings · type to filter".to_string()
            } else {
                format!("  Filter · {}", self.filter)
            };
            Line::styled(label, dim).render(Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }

        let filtered = self.filtered_catalog();
        let visible = usize::from(area.bottom().saturating_sub(y)).min(12);
        let start = self.selected.saturating_add(1).saturating_sub(visible);
        for (offset, item) in filtered.iter().skip(start).take(visible).enumerate() {
            let focused = start + offset == self.selected;
            let value = render_value_short(&item.value);
            let available = width.saturating_sub(value.width() + 2);
            let label = truncate_by_width(&item.label, available);
            let gap = " ".repeat(width.saturating_sub(label.width() + value.width()));
            let marker = if focused { "› " } else { "  " };
            let style = if focused { selected } else { Style::default() };
            let line = Line::styled(format!("{marker}{label}{gap}{value}"), style);
            line.render(Rect::new(area.x, y, area.width, 1), buf);
            y += 1;
        }
        if filtered.is_empty() && y < area.bottom() {
            Line::styled("  No matching settings", dim)
                .render(Rect::new(area.x, y, area.width, 1), buf);
        }
    }

    fn render_detail(&self, area: Rect, buf: &mut Buffer) {
        let dim = Style::default().fg(crate::tui::theme::current().dim);
        let head = Style::default().add_modifier(Modifier::BOLD);

        let item = match self.current_item() {
            Some(i) => i,
            None => return,
        };

        let value = render_value_short(&item.value);
        let mut lines = vec![
            Line::styled(format!("  {}", item.label), head),
            Line::styled(
                format!("  {value}"),
                Style::default().fg(crate::tui::theme::current().accent),
            ),
            Line::styled(format!("  {}", item.id), dim),
        ];
        match item.kind {
            SettingKind::Number { min, max, .. } => {
                lines.push(Line::styled(format!("  Range {min}–{max}"), dim));
            }
            SettingKind::Enum { options } => {
                lines.push(Line::styled(format!("  {}", options.join(" · ")), dim));
            }
            SettingKind::Bool | SettingKind::Text => {}
        }
        if self.is_dirty() {
            lines.push(Line::styled(
                "  Unsaved changes",
                Style::default().fg(crate::tui::theme::current().warn),
            ));
        }

        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        Widget::render(para, area, buf);
    }
}

fn render_value_short(v: &Value) -> String {
    match v {
        Value::Bool(value) => (if *value { "On" } else { "Off" }).to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        other => other.to_string(),
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
    fn set_error(&mut self, _message: String) {}
    fn handle_paste(&mut self, _text: &str) -> bool {
        false
    }
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
        let hint = Style::default().fg(crate::tui::theme::current().dim);
        let sel = Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::from(Span::styled(
            format!("  ▶ {} ", self.label),
            Style::default().add_modifier(Modifier::BOLD),
        ))];
        for (i, label) in ["Off", "On"].iter().enumerate() {
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
        Widget::render(Paragraph::new(lines), area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        3
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
        "↑↓ select · Space toggle · Enter apply · Esc back"
    }
    fn kind_label(&self) -> &'static str {
        "bool"
    }
}

// ── Text and number ────────────────────────────────────────────────────────────

struct ValueEditor {
    id: String,
    label: String,
    buffer: String,
    error: Option<String>,
    number: Option<(f64, f64, bool)>,
}

impl ValueEditor {
    fn new(item: &SettingItem) -> Self {
        let number = match &item.kind {
            SettingKind::Number {
                min,
                max,
                allow_fraction,
            } => Some((*min, *max, *allow_fraction)),
            _ => None,
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
            .or_else(|| item.value_as_string())
            .unwrap_or_default();
        Self {
            id: item.id.clone(),
            label: item.label.clone(),
            buffer,
            error: None,
            number,
        }
    }

    fn try_commit(&mut self) -> Option<Value> {
        // Accept fractions only for knobs declared as fractional. Integer
        // knobs still reject values that would silently round.
        let trimmed = self.buffer.trim();
        let Some((min, max, allow_fraction)) = self.number else {
            return Some(Value::from(trimmed));
        };
        let parsed: Result<f64, _> = trimmed.parse();
        match parsed {
            Ok(n) if n.is_finite() => {
                if !allow_fraction && n.fract() != 0.0 {
                    self.error = Some(format!("Fractional values not allowed: {}", self.buffer));
                    return None;
                }
                if n < min || n > max {
                    self.error = Some(format!(
                        "Out of range: {} not in [{}, {}]",
                        self.buffer, min, max
                    ));
                    return None;
                }
                self.error = None;
                if allow_fraction {
                    Some(Value::from(n))
                } else {
                    Some(Value::from(n as i64))
                }
            }
            _ => {
                self.error = Some(self.parse_error_message(trimmed));
                None
            }
        }
    }

    fn refresh_inline_error(&mut self) {
        let trimmed = self.buffer.trim();
        self.error = if self.number.is_some() && Self::is_incomplete_number(trimmed) {
            Some(Self::incomplete_number_message())
        } else {
            None
        };
    }

    fn is_incomplete_number(trimmed: &str) -> bool {
        matches!(trimmed, "." | "-" | "-.")
    }

    fn incomplete_number_message() -> String {
        "Incomplete number: add a digit (try 0.5)".to_string()
    }

    fn parse_error_message(&self, trimmed: &str) -> String {
        if trimmed.is_empty() {
            "Enter a number before saving".to_string()
        } else if Self::is_incomplete_number(trimmed) {
            Self::incomplete_number_message()
        } else {
            format!("Not a number: {}", self.buffer)
        }
    }
}

impl InnerEditor for ValueEditor {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let hint = Style::default().fg(crate::tui::theme::current().dim);
        let val_style = Style::default()
            .fg(crate::tui::theme::current().accent)
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
                    match self.number {
                        Some((min, max, _)) => {
                            format!("    range {min}..={max}")
                        }
                        None => "    #RRGGBB or auto".to_string(),
                    },
                    hint,
                ),
            ]),
        ];
        if let Some(ref err) = self.error {
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(crate::tui::theme::current().error),
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
                self.refresh_inline_error();
                InnerOutcome::Pending
            }
            KeyCode::Char(c) if self.number.is_none() && is_text_key(&key) => {
                if self.buffer.len() < 64 {
                    self.buffer.push(c);
                    self.error = None;
                }
                InnerOutcome::Pending
            }
            KeyCode::Char(c) if is_text_key(&key) && (c.is_ascii_digit() || c == '-') => {
                if c != '-'
                    || (self.number.is_some_and(|(min, _, _)| min < 0.0) && self.buffer.is_empty())
                {
                    self.buffer.push(c);
                    self.refresh_inline_error();
                }
                InnerOutcome::Pending
            }
            KeyCode::Char('.')
                if is_text_key(&key)
                    && self.number.is_some_and(|(_, _, fraction)| fraction)
                    && !self.buffer.contains('.') =>
            {
                self.buffer.push('.');
                self.refresh_inline_error();
                InnerOutcome::Pending
            }
            _ => InnerOutcome::Pending,
        }
    }

    fn item_id(&self) -> &str {
        &self.id
    }
    fn hint_keys(&self) -> &'static str {
        "Enter apply · Esc back · Backspace erase"
    }
    fn kind_label(&self) -> &'static str {
        if self.number.is_some() {
            "number"
        } else {
            "text"
        }
    }
    fn handle_paste(&mut self, text: &str) -> bool {
        self.buffer = text
            .trim()
            .chars()
            .filter(|ch| !ch.is_control())
            .take(64)
            .collect();
        self.error = None;
        true
    }
    fn set_error(&mut self, message: String) {
        self.error = Some(message);
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
        let hint = Style::default().fg(crate::tui::theme::current().dim);
        let sel = Style::default()
            .fg(crate::tui::theme::current().accent)
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
        Widget::render(Paragraph::new(lines), area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        (1 + self.options.len() as u16).min(8)
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
        "↑↓ select · Enter apply · Esc back"
    }
    fn kind_label(&self) -> &'static str {
        "enum"
    }
}

// ─── Save prompt ─────────────────────────────────────────────────────────
//
// Four-row picker: save to user / save to project / preview / discard.
// Arrow keys move the cursor (matches Bool/Enum editor UX); Enter commits
// the highlighted row; numeric/letter shortcuts bypass navigation for
// muscle memory.
//
// "Preview" doesn't commit — it flips an internal `preview` flag on the
// outer view that swaps the render to a diff list. Any key returns to
// the prompt with state intact (filter position, selection).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SavePromptRow {
    SaveUser,
    SaveProject,
    Preview,
    Discard,
}

const SAVE_PROMPT_ROWS: [SavePromptRow; 4] = [
    SavePromptRow::SaveUser,
    SavePromptRow::SaveProject,
    SavePromptRow::Preview,
    SavePromptRow::Discard,
];

struct SavePrompt {
    selected: usize,
}

impl SavePrompt {
    fn new() -> Self {
        Self { selected: 0 }
    }

    fn handle_key(&mut self, key: KeyEvent) -> SavePromptOutcome {
        match key.code {
            // Arrow navigation + Enter — primary UX.
            KeyCode::Up => {
                self.selected = if self.selected == 0 {
                    SAVE_PROMPT_ROWS.len() - 1
                } else {
                    self.selected - 1
                };
                SavePromptOutcome::Pending
            }
            KeyCode::Down => {
                self.selected = (self.selected + 1) % SAVE_PROMPT_ROWS.len();
                SavePromptOutcome::Pending
            }
            KeyCode::Enter => row_outcome(SAVE_PROMPT_ROWS[self.selected]),

            // Muscle-memory shortcuts — one key = one choice, no
            // navigation needed.
            KeyCode::Char('1') | KeyCode::Char('u') | KeyCode::Char('U') => {
                SavePromptOutcome::SaveToUser
            }
            KeyCode::Char('2') | KeyCode::Char('p') | KeyCode::Char('P') => {
                SavePromptOutcome::SaveToProject
            }
            KeyCode::Char('v') | KeyCode::Char('V') => SavePromptOutcome::OpenPreview,
            KeyCode::Char('d') | KeyCode::Char('D') => SavePromptOutcome::Discard,
            KeyCode::Esc | KeyCode::Char('b') | KeyCode::Char('B') => SavePromptOutcome::BackToEdit,
            _ => SavePromptOutcome::Pending,
        }
    }
}

fn row_outcome(row: SavePromptRow) -> SavePromptOutcome {
    match row {
        SavePromptRow::SaveUser => SavePromptOutcome::SaveToUser,
        SavePromptRow::SaveProject => SavePromptOutcome::SaveToProject,
        SavePromptRow::Preview => SavePromptOutcome::OpenPreview,
        SavePromptRow::Discard => SavePromptOutcome::Discard,
    }
}

enum SavePromptOutcome {
    Pending,
    SaveToUser,
    SaveToProject,
    OpenPreview,
    Discard,
    BackToEdit,
}

/// Show the same resolved paths used by configuration loading and saving.
fn user_path_label() -> String {
    astra_config::runtime_config::user_runtime_config_path()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "~/.astra/config/runtime.toml".to_string())
}

fn project_path_label() -> String {
    std::env::current_dir()
        .map(|d| d.join(".astra/config/runtime.toml").display().to_string())
        .unwrap_or_else(|_| "./.astra/config/runtime.toml".to_string())
}

fn render_save_prompt(p: &SavePrompt, area: Rect, buf: &mut Buffer) {
    let theme = crate::tui::theme::current();
    let head = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(theme.dim);
    let sel = Style::default()
        .fg(theme.selected_fg)
        .bg(theme.selected_bg)
        .add_modifier(Modifier::BOLD);

    let rows: [(&str, String); 4] = [
        ("[1]", format!("Save for user · {}", user_path_label())),
        (
            "[2]",
            format!("Save for project · {}", project_path_label()),
        ),
        ("[v]", "Preview changes".to_string()),
        ("[d]", "Discard changes".to_string()),
    ];

    let mut lines = vec![
        Line::from(Span::styled("  Save changes?", head)),
        Line::from(Span::raw("")),
    ];
    for (idx, (key_hint, label)) in rows.iter().enumerate() {
        let is_sel = idx == p.selected;
        let marker = if is_sel { "› " } else { "  " };
        let row_style = if is_sel { sel } else { dim };
        let label_style = if is_sel { sel } else { Style::default() };
        lines.push(Line::from(vec![
            Span::styled(format!("  {marker}"), row_style),
            Span::styled(format!("{key_hint} "), dim),
            Span::styled(label.clone(), label_style),
        ]));
    }
    Widget::render(Paragraph::new(lines), area, buf);
}

// ─── Preview ────────────────────────────────────────────────────────────
//
// Rendered when `preview` is on. Walks the catalog comparing the
// `original` snapshot against `working`, lists each differing row as
// `id: old → new`. Any key returns to the prompt; no commit, no I/O.

fn render_preview(original: &RuntimeConfig, working: &RuntimeConfig, area: Rect, buf: &mut Buffer) {
    let head = Style::default()
        .fg(crate::tui::theme::current().accent)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(crate::tui::theme::current().dim);
    let before = Style::default().fg(crate::tui::theme::current().dim);
    let after = Style::default()
        .fg(crate::tui::theme::current().accent)
        .add_modifier(Modifier::BOLD);

    let before_cat = build_settings_catalog(original);
    let after_cat = build_settings_catalog(working);
    // Catalogs are built from the same function so the id order matches;
    // zip is safe. Guard anyway.
    let mut lines = vec![
        Line::from(Span::styled("  Preview changes", head)),
        Line::from(Span::raw("")),
    ];
    let mut any = false;
    for (a, b) in before_cat.iter().zip(after_cat.iter()) {
        if a.id != b.id {
            continue; // shape drift, skip row
        }
        if a.value != b.value {
            any = true;
            lines.push(Line::from(vec![
                Span::styled("  ", dim),
                Span::raw(a.id.clone()),
                Span::styled(": ", dim),
                Span::styled(render_value_short(&a.value), before),
                Span::styled(" → ", dim),
                Span::styled(render_value_short(&b.value), after),
            ]));
        }
    }
    if !any {
        lines.push(Line::from(Span::styled("  (no changes)", dim)));
    }
    lines.push(Line::from(Span::raw("")));
    lines.push(Line::from(Span::styled("  any key returns to prompt", dim)));
    Widget::render(Paragraph::new(lines), area, buf);
}
