//! Stub [`View`] / [`ViewStack`] — RED phase of TDD.

#![allow(dead_code)]

use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

/// Outcome of routing an event to a view.
pub(crate) enum EventResult {
    /// Event was consumed; do not fall through.
    Handled,
    /// View did not care; try the next view down.
    Unhandled,
    /// Pop this view off the stack.
    Close,
    /// Push a new view on top.
    OpenView(Box<dyn View>),
}

impl std::fmt::Debug for EventResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Handled => f.write_str("Handled"),
            Self::Unhandled => f.write_str("Unhandled"),
            Self::Close => f.write_str("Close"),
            Self::OpenView(v) => write!(f, "OpenView({})", v.name()),
        }
    }
}

pub(crate) trait View {
    fn handle_key(&mut self, event: KeyEvent) -> EventResult;

    fn render(&self, area: Rect, buf: &mut Buffer);

    fn on_enter(&mut self) {}

    fn on_exit(&mut self) {}

    /// Optional descriptive name for debugging / snapshotting.
    fn name(&self) -> &'static str {
        "view"
    }
}

#[derive(Default)]
pub(crate) struct ViewStack {
    stack: Vec<Box<dyn View>>,
}

impl ViewStack {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.stack.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stack.is_empty()
    }

    pub fn push(&mut self, mut view: Box<dyn View>) {
        view.on_enter();
        self.stack.push(view);
    }

    pub fn pop(&mut self) -> Option<Box<dyn View>> {
        let mut view = self.stack.pop()?;
        view.on_exit();
        Some(view)
    }

    pub fn top_name(&self) -> Option<&'static str> {
        self.stack.last().map(|v| v.name())
    }

    /// Route an event top-down. First view returning anything other than
    /// [`EventResult::Unhandled`] stops the walk. [`EventResult::Close`] pops
    /// the handling view; [`EventResult::OpenView`] pushes a new one. Both
    /// cases return [`EventResult::Handled`] to the caller since the stack
    /// resolved the event.
    pub fn handle_key(&mut self, event: KeyEvent) -> EventResult {
        // Walk top-down by index so we can mutate the matching view.
        for idx in (0..self.stack.len()).rev() {
            let result = self.stack[idx].handle_key(event);
            match result {
                EventResult::Unhandled => continue,
                EventResult::Handled => return EventResult::Handled,
                EventResult::Close => {
                    // Tear down every view above and including the closer,
                    // ensuring exit hooks fire in reverse push order.
                    while self.stack.len() > idx {
                        if let Some(mut v) = self.stack.pop() {
                            v.on_exit();
                        }
                    }
                    return EventResult::Handled;
                }
                EventResult::OpenView(new_view) => {
                    self.push(new_view);
                    return EventResult::Handled;
                }
            }
        }
        EventResult::Unhandled
    }

    /// Render bottom-up so overlays composite on top of base views.
    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        for view in &self.stack {
            view.render(area, buf);
        }
    }
}
