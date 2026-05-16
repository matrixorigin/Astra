use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget, Wrap},
};
use textwrap::wrap;
use tokio::sync::oneshot;

use super::{
    textarea::{TextArea, TextAreaAction},
    view::{BottomPaneView, CancellationEvent, ViewCompletion},
};
use crate::chat_stream::{
    AskUserAnnotation, AskUserAnswers, AskUserPrompt, AskUserQuestion, AskUserQuestionAnswer,
    AskUserResponse,
};

struct QuestionState {
    cursor_row: usize,
    selected: BTreeSet<usize>,
    custom_input: TextArea,
    notes_input: TextArea,
}

impl QuestionState {
    fn new() -> Self {
        Self {
            cursor_row: 0,
            selected: BTreeSet::new(),
            custom_input: TextArea::new(),
            notes_input: TextArea::new(),
        }
    }
}

pub(crate) struct AskUserView {
    prompt: AskUserPrompt,
    states: Vec<QuestionState>,
    current_tab: usize,
    notes_focus: bool,
    completed: bool,
    response_tx: Option<oneshot::Sender<AskUserResponse>>,
    validation: Option<String>,
}

impl AskUserView {
    pub fn new(prompt: AskUserPrompt, response_tx: oneshot::Sender<AskUserResponse>) -> Self {
        let states = prompt
            .questions
            .iter()
            .map(|_| QuestionState::new())
            .collect();
        Self {
            prompt,
            states,
            current_tab: 0,
            notes_focus: false,
            completed: false,
            response_tx: Some(response_tx),
            validation: None,
        }
    }

    fn submit_tab(&self) -> usize {
        self.prompt.questions.len()
    }

    fn current_question(&self) -> Option<&AskUserQuestion> {
        self.prompt.questions.get(self.current_tab)
    }

    fn current_state(&self) -> Option<&QuestionState> {
        self.states.get(self.current_tab)
    }

    fn current_state_mut(&mut self) -> Option<&mut QuestionState> {
        self.states.get_mut(self.current_tab)
    }

    fn question_has_preview(question: &AskUserQuestion) -> bool {
        !question.multi_select
            && question
                .options
                .iter()
                .any(|option| option.preview.is_some())
    }

    fn show_other(question: &AskUserQuestion) -> bool {
        question.allow_freeform && !Self::question_has_preview(question)
    }

    fn row_count(question: &AskUserQuestion) -> usize {
        question.options.len() + usize::from(Self::show_other(question))
    }

    fn other_row(question: &AskUserQuestion) -> Option<usize> {
        Self::show_other(question).then_some(question.options.len())
    }

    fn is_other_row(question: &AskUserQuestion, state: &QuestionState) -> bool {
        Self::other_row(question) == Some(state.cursor_row)
    }

    fn question_answers(&self, idx: usize) -> Vec<String> {
        let question = &self.prompt.questions[idx];
        let state = &self.states[idx];
        let mut answers: Vec<String> = state
            .selected
            .iter()
            .filter_map(|selected| question.options.get(*selected))
            .map(|choice| choice.label.clone())
            .collect();
        let custom = state.custom_input.text().trim();
        if !custom.is_empty() {
            if !question.multi_select {
                answers.clear();
            }
            answers.push(custom.to_string());
        }
        answers
    }

    fn selected_preview(&self, idx: usize) -> Option<String> {
        let question = &self.prompt.questions[idx];
        let state = &self.states[idx];
        state.selected.iter().next().and_then(|selected| {
            question
                .options
                .get(*selected)
                .and_then(|option| option.preview.clone())
        })
    }

    fn answer_annotation(&self, idx: usize) -> Option<AskUserAnnotation> {
        let notes = self.states[idx].notes_input.text().trim().to_string();
        let preview = self.selected_preview(idx);
        if notes.is_empty() && preview.is_none() {
            None
        } else {
            Some(AskUserAnnotation {
                notes: (!notes.is_empty()).then_some(notes),
                preview,
            })
        }
    }

    fn is_answered(&self, idx: usize) -> bool {
        !self.question_answers(idx).is_empty()
    }

    fn answer_summary(&self, idx: usize) -> String {
        let answers = self.question_answers(idx);
        if answers.is_empty() {
            "Not answered yet".into()
        } else if self.prompt.questions[idx].multi_select {
            answers.join(", ")
        } else {
            answers[0].clone()
        }
    }

    fn next_tab(&mut self) {
        self.current_tab = (self.current_tab + 1).min(self.submit_tab());
        self.notes_focus = false;
        self.validation = None;
    }

    fn prev_tab(&mut self) {
        if self.current_tab > 0 {
            self.current_tab -= 1;
            self.notes_focus = false;
            self.validation = None;
        }
    }

    fn send(&mut self, response: AskUserResponse) {
        if let Some(tx) = self.response_tx.take() {
            let _ = tx.send(response);
        }
        self.completed = true;
    }

    fn submit_all(&mut self) {
        if let Some((idx, question)) = self
            .prompt
            .questions
            .iter()
            .enumerate()
            .find(|(idx, _)| !self.is_answered(*idx))
        {
            self.current_tab = idx;
            self.notes_focus = false;
            self.validation = Some(format!("Answer '{}' before submitting.", question.header));
            return;
        }

        let answers = self
            .prompt
            .questions
            .iter()
            .enumerate()
            .map(|(idx, question)| AskUserQuestionAnswer {
                question: question.question.clone(),
                answers: self.question_answers(idx),
                multi_select: question.multi_select,
                annotation: self.answer_annotation(idx),
            })
            .collect();
        self.send(AskUserResponse::Submitted(AskUserAnswers { answers }));
    }

    fn summary_line(&self) -> String {
        if self.current_tab == self.submit_tab() {
            format!(
                "Review {} answers, then press Enter to submit.",
                self.prompt.questions.len()
            )
        } else {
            format!(
                "Question {}/{}",
                self.current_tab + 1,
                self.prompt.questions.len()
            )
        }
    }

    fn wrap_count(text: &str, width: u16) -> u16 {
        if text.trim().is_empty() {
            return 0;
        }
        let width = width.max(1) as usize;
        let mut rows = 0u16;
        for logical in text.lines() {
            rows = rows.saturating_add(wrap(logical, width).len().max(1) as u16);
        }
        rows
    }

    fn preview_line_count(text: &str, width: u16) -> u16 {
        Self::wrap_count(text, width).clamp(3, 8)
    }

    fn current_input_height(&self, width: u16) -> u16 {
        let Some(question) = self.current_question() else {
            return 0;
        };
        let Some(state) = self.current_state() else {
            return 0;
        };
        if !Self::show_other(question)
            || (!Self::is_other_row(question, state) && state.custom_input.is_empty())
        {
            return 0;
        }
        state
            .custom_input
            .desired_height(width.saturating_sub(4).max(1))
            + 2
    }

    fn notes_height(&self, width: u16) -> u16 {
        let Some(question) = self.current_question() else {
            return 0;
        };
        let Some(state) = self.current_state() else {
            return 0;
        };
        if !Self::question_has_preview(question) {
            return 0;
        }
        state
            .notes_input
            .desired_height(width.saturating_sub(4).max(1))
            .max(1)
            + 2
    }

    fn current_choices_height(&self, width: u16) -> u16 {
        let Some(question) = self.current_question() else {
            return 0;
        };
        let desc_width = width.saturating_sub(10).max(1);
        let mut rows = 0u16;
        for choice in &question.options {
            rows = rows.saturating_add(1);
            if let Some(description) = &choice.description {
                rows = rows.saturating_add(Self::wrap_count(description, desc_width));
            }
        }
        if Self::show_other(question) {
            rows = rows.saturating_add(1);
        }
        rows
    }

    fn render_tabs(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
        let mut line_width = 0usize;
        for idx in 0..=self.submit_tab() {
            let (label, active, answered) = if idx == self.submit_tab() {
                ("Submit".to_string(), self.current_tab == idx, false)
            } else {
                (
                    self.prompt.questions[idx].header.clone(),
                    self.current_tab == idx,
                    self.is_answered(idx),
                )
            };
            let prefix = if answered { "✓ " } else { "" };
            let chip = format!("[{prefix}{label}] ");
            if line_width + chip.len() > area.width as usize && !lines.last().unwrap().is_empty() {
                lines.push(Vec::new());
                line_width = 0;
            }
            let style = if active {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if answered {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            lines
                .last_mut()
                .expect("tab line")
                .push(Span::styled(chip.clone(), style));
            line_width += chip.len();
        }

        for (row, spans) in lines.into_iter().enumerate() {
            if row as u16 >= area.height {
                break;
            }
            Widget::render(
                Line::from(spans),
                Rect::new(area.x, area.y + row as u16, area.width, 1),
                buf,
            );
        }
    }

    fn tab_height(&self, width: u16) -> u16 {
        let mut rows = 1u16;
        let mut line_width = 0usize;
        for idx in 0..=self.submit_tab() {
            let label = if idx == self.submit_tab() {
                "Submit".to_string()
            } else {
                self.prompt.questions[idx].header.clone()
            };
            let prefix = if idx < self.submit_tab() && self.is_answered(idx) {
                "✓ "
            } else {
                ""
            };
            let chip_len = format!("[{prefix}{label}] ").len();
            if line_width + chip_len > width as usize && line_width > 0 {
                rows += 1;
                line_width = 0;
            }
            line_width += chip_len;
        }
        rows
    }

    fn render_standard_question(&self, area: Rect, buf: &mut Buffer) {
        let Some(question) = self.current_question() else {
            return;
        };
        let Some(state) = self.current_state() else {
            return;
        };
        let text_width = area.width.saturating_sub(4).max(1);
        let question_h = Self::wrap_count(&question.question, text_width);
        let choices_h = self.current_choices_height(text_width);
        let input_h = self.current_input_height(text_width);
        let chunks = Layout::vertical([
            Constraint::Length(question_h),
            Constraint::Length(choices_h),
            Constraint::Length(input_h),
            Constraint::Min(0),
        ])
        .split(area);

        Paragraph::new(question.question.as_str())
            .style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )
            .wrap(Wrap { trim: false })
            .render(chunks[0], buf);

        let mut y = chunks[1].y;
        let desc_width = chunks[1].width.saturating_sub(10).max(1) as usize;
        for (idx, choice) in question.options.iter().enumerate() {
            if y >= chunks[1].bottom() {
                break;
            }
            let focused = state.cursor_row == idx;
            let selected = state.selected.contains(&idx);
            let selector = if question.multi_select {
                if selected { "[x]" } else { "[ ]" }
            } else if selected {
                "(*)"
            } else {
                "( )"
            };
            let pointer = if focused { "›" } else { " " };
            let style = if focused {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Widget::render(
                Line::from(vec![
                    Span::styled(format!("  {pointer} {selector} "), style),
                    Span::styled(choice.label.clone(), style),
                ]),
                Rect::new(chunks[1].x, y, chunks[1].width, 1),
                buf,
            );
            y += 1;
            if let Some(description) = &choice.description {
                for line in wrap(description, desc_width) {
                    if y >= chunks[1].bottom() {
                        break;
                    }
                    Widget::render(
                        Line::from(Span::styled(
                            format!("        {line}"),
                            Style::default().fg(Color::DarkGray),
                        )),
                        Rect::new(chunks[1].x, y, chunks[1].width, 1),
                        buf,
                    );
                    y += 1;
                }
            }
        }

        if Self::show_other(question) && y < chunks[1].bottom() {
            let focused = Self::is_other_row(question, state);
            let style = if focused {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Widget::render(
                Line::from(vec![
                    Span::styled(if focused { "  › " } else { "    " }, style),
                    Span::styled("Other", style),
                ]),
                Rect::new(chunks[1].x, y, chunks[1].width, 1),
                buf,
            );
        }

        if chunks[2].height > 0 {
            let mut block = Block::default()
                .borders(Borders::ALL)
                .title(" Your answer ");
            if Self::is_other_row(question, state) {
                block = block.border_style(Style::default().fg(Color::Green));
            }
            let inner = block.inner(chunks[2]);
            Widget::render(block, chunks[2], buf);
            if state.custom_input.is_empty() {
                Widget::render(
                    Line::from(Span::styled(
                        "Type your answer here",
                        Style::default().fg(Color::DarkGray),
                    )),
                    inner,
                    buf,
                );
            } else {
                state.custom_input.render(inner, buf);
            }
        }
    }

    fn render_preview_box(&self, area: Rect, buf: &mut Buffer, content: &str) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let block = Block::default().borders(Borders::ALL).title(" Preview ");
        let inner = block.inner(area);
        Widget::render(block, area, buf);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let mut y = inner.y;
        for logical in content.lines() {
            for line in wrap(logical, inner.width.max(1) as usize) {
                if y >= inner.bottom() {
                    return;
                }
                Widget::render(
                    Line::from(Span::raw(line.to_string())),
                    Rect::new(inner.x, y, inner.width, 1),
                    buf,
                );
                y += 1;
            }
        }
    }

    fn render_notes_box(&self, area: Rect, buf: &mut Buffer) {
        let Some(state) = self.current_state() else {
            return;
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let mut block = Block::default().borders(Borders::ALL).title(" Notes ");
        if self.notes_focus {
            block = block.border_style(Style::default().fg(Color::Green));
        }
        let inner = block.inner(area);
        Widget::render(block, area, buf);
        if state.notes_input.is_empty() {
            Widget::render(
                Line::from(vec![
                    Span::styled("Notes: ", Style::default().fg(Color::DarkGray)),
                    Span::styled("press n to add notes", Style::default().fg(Color::DarkGray)),
                ]),
                inner,
                buf,
            );
        } else {
            state.notes_input.render(inner, buf);
        }
    }

    fn render_preview_question(&self, area: Rect, buf: &mut Buffer) {
        let Some(question) = self.current_question() else {
            return;
        };
        let Some(state) = self.current_state() else {
            return;
        };
        let text_width = area.width.saturating_sub(4).max(1);
        let question_h = Self::wrap_count(&question.question, text_width);
        let notes_h = self.notes_height(text_width);
        let body = Layout::vertical([
            Constraint::Length(question_h),
            Constraint::Min(6),
            Constraint::Length(notes_h),
            Constraint::Min(0),
        ])
        .split(area);

        Paragraph::new(question.question.as_str())
            .style(
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )
            .wrap(Wrap { trim: false })
            .render(body[0], buf);

        let side = Layout::horizontal([
            Constraint::Length(body[1].width.min(32)),
            Constraint::Min(24),
        ])
        .split(body[1]);

        let mut y = side[0].y;
        for (idx, choice) in question.options.iter().enumerate() {
            if y >= side[0].bottom() {
                break;
            }
            let focused = state.cursor_row == idx;
            let selected = state.selected.contains(&idx);
            let pointer = if focused { "›" } else { " " };
            let style = if focused {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Widget::render(
                Line::from(vec![
                    Span::styled(format!("  {pointer} {}. ", idx + 1), style),
                    Span::styled(choice.label.clone(), style),
                    Span::styled(
                        if selected { " ✓" } else { "" },
                        Style::default().fg(Color::Green),
                    ),
                ]),
                Rect::new(side[0].x, y, side[0].width, 1),
                buf,
            );
            y += 1;
            if let Some(description) = &choice.description {
                for line in wrap(description, side[0].width.saturating_sub(8).max(1) as usize) {
                    if y >= side[0].bottom() {
                        break;
                    }
                    Widget::render(
                        Line::from(Span::styled(
                            format!("       {line}"),
                            Style::default().fg(Color::DarkGray),
                        )),
                        Rect::new(side[0].x, y, side[0].width, 1),
                        buf,
                    );
                    y += 1;
                }
            }
        }

        let preview = state
            .selected
            .iter()
            .next()
            .and_then(|selected| question.options.get(*selected))
            .and_then(|choice| choice.preview.as_deref())
            .or_else(|| {
                question
                    .options
                    .first()
                    .and_then(|choice| choice.preview.as_deref())
            })
            .unwrap_or("No preview available");
        self.render_preview_box(side[1], buf, preview);
        self.render_notes_box(body[2], buf);
    }

    fn render_submit(&self, area: Rect, buf: &mut Buffer) {
        let mut y = area.y;
        Widget::render(
            Line::from(Span::styled(
                "Review answers",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )),
            Rect::new(area.x, y, area.width, 1),
            buf,
        );
        y += 1;
        for (idx, question) in self.prompt.questions.iter().enumerate() {
            if y >= area.bottom() {
                break;
            }
            Widget::render(
                Line::from(vec![
                    Span::styled(
                        format!("  {} ", question.header),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(self.answer_summary(idx)),
                ]),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
            if let Some(annotation) = self.answer_annotation(idx) {
                if let Some(notes) = annotation.notes {
                    if y >= area.bottom() {
                        break;
                    }
                    Widget::render(
                        Line::from(Span::styled(
                            format!("      notes: {notes}"),
                            Style::default().fg(Color::DarkGray),
                        )),
                        Rect::new(area.x, y, area.width, 1),
                        buf,
                    );
                    y += 1;
                }
            }
        }
    }

    fn content_area_for_cursor(&self, area: Rect) -> Rect {
        if self.hint_keys().is_some() && area.height > 0 {
            Rect::new(area.x, area.y, area.width, area.height - 1)
        } else {
            area
        }
    }

    fn cursor_target_area(&self, area: Rect) -> Option<Rect> {
        let question = self.current_question()?;
        let state = self.current_state()?;
        let content_area = self.content_area_for_cursor(area);
        let outer = Block::default().borders(Borders::ALL).title(" ask_user ");
        let inner = outer.inner(content_area);
        let text_width = inner.width.saturating_sub(4).max(1);
        let context_h = self
            .prompt
            .context
            .as_deref()
            .map(|c| Self::wrap_count(c, text_width))
            .unwrap_or(0);
        let body = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(context_h),
            Constraint::Length(self.tab_height(text_width)),
            Constraint::Min(0),
            Constraint::Length(u16::from(self.validation.is_some())),
        ])
        .split(inner);

        if Self::question_has_preview(question) && self.notes_focus {
            let question_h = Self::wrap_count(&question.question, text_width);
            let notes_h = self.notes_height(text_width);
            let chunks = Layout::vertical([
                Constraint::Length(question_h),
                Constraint::Min(6),
                Constraint::Length(notes_h),
                Constraint::Min(0),
            ])
            .split(body[3]);
            return Some(chunks[2]);
        }

        if !Self::show_other(question) || !Self::is_other_row(question, state) {
            return None;
        }
        let question_h = Self::wrap_count(&question.question, text_width);
        let choices_h = self.current_choices_height(text_width);
        let input_h = self.current_input_height(text_width);
        let chunks = Layout::vertical([
            Constraint::Length(question_h),
            Constraint::Length(choices_h),
            Constraint::Length(input_h),
            Constraint::Min(0),
        ])
        .split(body[3]);
        Some(chunks[2])
    }

    fn activate_option(&mut self, idx: usize) {
        let submit_tab = self.submit_tab();
        let Some(question) = self.current_question() else {
            return;
        };
        if idx >= question.options.len() {
            return;
        }
        let preview_mode = Self::question_has_preview(question);
        let multi_select = question.multi_select;
        let question_count = self.prompt.questions.len();
        let state = self.current_state_mut().expect("state for active question");
        state.cursor_row = idx;
        if multi_select {
            if !state.selected.insert(idx) {
                state.selected.remove(&idx);
            }
        } else {
            state.selected.clear();
            state.selected.insert(idx);
            if !preview_mode {
                if question_count == 1 {
                    self.submit_all();
                } else {
                    self.current_tab = (self.current_tab + 1).min(submit_tab);
                }
            }
        }
        self.validation = None;
    }

    fn focus_other_and_handle(&mut self, key: KeyEvent) {
        let Some(question) = self.current_question() else {
            return;
        };
        let Some(other_row) = Self::other_row(question) else {
            return;
        };
        let is_multi = question.multi_select;
        let question_count = self.prompt.questions.len();
        let submit_tab = self.submit_tab();
        let state = self.current_state_mut().expect("state for active question");
        state.cursor_row = other_row;
        if !is_multi {
            state.selected.clear();
        }
        match state.custom_input.handle_key(key) {
            TextAreaAction::Submit => {
                if state.custom_input.text().trim().is_empty() {
                    self.validation =
                        Some("Type an answer in the box below, then press Enter.".into());
                    return;
                }
                if question_count == 1 {
                    self.submit_all();
                } else {
                    self.current_tab = (self.current_tab + 1).min(submit_tab);
                }
            }
            TextAreaAction::Cancel | TextAreaAction::Quit => self.send(AskUserResponse::Cancelled),
            TextAreaAction::Changed
            | TextAreaAction::Consumed
            | TextAreaAction::Unhandled
            | TextAreaAction::HistoryPrev
            | TextAreaAction::HistoryNext => {
                self.validation = None;
            }
        }
    }

    fn handle_notes_key(&mut self, key: KeyEvent) {
        let state = self.current_state_mut().expect("state for active question");
        match state.notes_input.handle_key(key) {
            TextAreaAction::Submit | TextAreaAction::Cancel | TextAreaAction::Quit => {
                self.notes_focus = false;
                self.validation = None;
            }
            TextAreaAction::Changed
            | TextAreaAction::Consumed
            | TextAreaAction::Unhandled
            | TextAreaAction::HistoryPrev
            | TextAreaAction::HistoryNext => {
                self.validation = None;
            }
        }
    }
}

impl BottomPaneView for AskUserView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let outer = Block::default().borders(Borders::ALL).title(" ask_user ");
        let inner = outer.inner(area);
        Widget::render(outer, area, buf);

        let text_width = inner.width.saturating_sub(4).max(1);
        let context_h = self
            .prompt
            .context
            .as_deref()
            .map(|context| Self::wrap_count(context, text_width))
            .unwrap_or(0);
        let tab_h = self.tab_height(text_width);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(context_h),
            Constraint::Length(tab_h),
            Constraint::Min(0),
            Constraint::Length(u16::from(self.validation.is_some())),
        ])
        .split(inner);

        Widget::render(
            Line::from(Span::styled(
                format!("  {}", self.summary_line()),
                Style::default().fg(Color::DarkGray),
            )),
            chunks[0],
            buf,
        );

        if let Some(context) = &self.prompt.context {
            Paragraph::new(context.as_str())
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false })
                .render(chunks[1], buf);
        }

        self.render_tabs(chunks[2], buf);

        if self.current_tab == self.submit_tab() {
            self.render_submit(chunks[3], buf);
        } else if self
            .current_question()
            .is_some_and(Self::question_has_preview)
        {
            self.render_preview_question(chunks[3], buf);
        } else {
            self.render_standard_question(chunks[3], buf);
        }

        if let Some(validation) = &self.validation {
            if chunks[4].height > 0 {
                Widget::render(
                    Line::from(Span::styled(
                        format!("  {validation}"),
                        Style::default().fg(Color::Yellow),
                    )),
                    Rect::new(chunks[4].x, chunks[4].y, chunks[4].width, 1),
                    buf,
                );
            }
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        let text_width = width.saturating_sub(6).max(1);
        let context_h = self
            .prompt
            .context
            .as_deref()
            .map(|context| Self::wrap_count(context, text_width))
            .unwrap_or(0);
        let tab_h = self.tab_height(text_width);
        let body_h = if self.current_tab == self.submit_tab() {
            (self.prompt.questions.len() as u16 + 4).max(8)
        } else {
            let question = self.current_question().expect("question exists");
            if Self::question_has_preview(question) {
                Self::wrap_count(&question.question, text_width)
                    + Self::preview_line_count(
                        question
                            .options
                            .first()
                            .and_then(|option| option.preview.as_deref())
                            .unwrap_or("No preview available"),
                        text_width.saturating_sub(38).max(20),
                    )
                    + self.notes_height(text_width)
                    + 4
            } else {
                Self::wrap_count(&question.question, text_width)
                    + self.current_choices_height(text_width)
                    + self.current_input_height(text_width)
            }
        };
        (2 + 1 + context_h + tab_h + body_h + u16::from(self.validation.is_some())).clamp(12, 24)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.current_tab == self.submit_tab() {
            match key.code {
                KeyCode::Esc => self.send(AskUserResponse::Cancelled),
                KeyCode::Enter => self.submit_all(),
                KeyCode::Left | KeyCode::BackTab => self.prev_tab(),
                _ => {}
            }
            return;
        }

        let question = match self.current_question() {
            Some(question) => question.clone(),
            None => return,
        };
        if self.notes_focus {
            match key.code {
                KeyCode::Esc => self.notes_focus = false,
                _ => self.handle_notes_key(key),
            }
            return;
        }

        let state = self.current_state().expect("state for active question");
        let other_focused = Self::is_other_row(&question, state);

        match key.code {
            KeyCode::Esc => self.send(AskUserResponse::Cancelled),
            KeyCode::Left | KeyCode::BackTab => self.prev_tab(),
            KeyCode::Right | KeyCode::Tab => self.next_tab(),
            KeyCode::Up => {
                let state = self.current_state_mut().expect("state for active question");
                if state.cursor_row > 0 {
                    state.cursor_row -= 1;
                    self.validation = None;
                }
            }
            KeyCode::Down => {
                let state = self.current_state_mut().expect("state for active question");
                let last = Self::row_count(&question).saturating_sub(1);
                if state.cursor_row < last {
                    state.cursor_row += 1;
                    self.validation = None;
                }
            }
            KeyCode::Char('n') if Self::question_has_preview(&question) => {
                self.notes_focus = true;
                self.validation = None;
            }
            KeyCode::Char(_) if other_focused => self.focus_other_and_handle(key),
            KeyCode::Backspace | KeyCode::Delete if other_focused => {
                self.focus_other_and_handle(key)
            }
            KeyCode::Enter if other_focused => self.focus_other_and_handle(key),
            KeyCode::Char(c) if c.is_ascii_digit() && !other_focused => {
                let idx = c.to_digit(10).unwrap_or(0) as usize;
                if idx > 0 && idx <= question.options.len() {
                    self.activate_option(idx - 1);
                }
            }
            KeyCode::Char(' ') if question.multi_select && !other_focused => {
                let idx = self
                    .current_state()
                    .expect("state for active question")
                    .cursor_row;
                self.activate_option(idx);
            }
            KeyCode::Enter if !question.multi_select && !other_focused => {
                let idx = self
                    .current_state()
                    .expect("state for active question")
                    .cursor_row;
                self.activate_option(idx);
            }
            KeyCode::Enter if question.multi_select && !other_focused => {
                let idx = self
                    .current_state()
                    .expect("state for active question")
                    .cursor_row;
                self.activate_option(idx);
            }
            KeyCode::Char(_) if Self::show_other(&question) => self.focus_other_and_handle(key),
            _ => {}
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let question = self.current_question()?;
        let state = self.current_state()?;
        let target_area = self.cursor_target_area(area)?;
        let inner = Block::default().borders(Borders::ALL).inner(target_area);
        if self.notes_focus {
            return state.notes_input.cursor_position(inner);
        }
        if !Self::is_other_row(question, state) {
            return None;
        }
        state.custom_input.cursor_position(inner)
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.send(AskUserResponse::Cancelled);
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.completed.then_some(ViewCompletion {
            result: None,
            reopen: None,
        })
    }

    fn hint_keys(&self) -> Option<String> {
        if self.current_tab == self.submit_tab() {
            return Some("← switch question · Enter submit · Esc cancel".into());
        }
        let question = self.current_question()?;
        if self.notes_focus {
            return Some("Type notes · Enter finish notes · Esc back".into());
        }
        Some(if Self::question_has_preview(question) {
            "Tab switch question · ↑↓ choose · 1-9 quick select · n notes · Enter select · Esc cancel"
                .into()
        } else if question.multi_select {
            "Tab switch question · ↑↓ choose · Space/Enter toggle · type for Other · Esc cancel"
                .into()
        } else {
            "Tab switch question · ↑↓ choose · 1-9 quick select · Enter select · type for Other · Esc cancel"
                .into()
        })
    }
}
