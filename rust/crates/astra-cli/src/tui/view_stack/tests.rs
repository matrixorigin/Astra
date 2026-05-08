//! ViewStack behaviour contract (RED).

#![cfg(test)]

use std::cell::RefCell;
use std::rc::Rc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::{EventResult, View, ViewStack};

/// Journal of lifecycle calls made across all recorder views.
type Journal = Rc<RefCell<Vec<String>>>;

/// Test view that records every call with its name, drawing a single
/// recognisable character across its area so render order is visible.
struct Recorder {
    name: &'static str,
    glyph: char,
    journal: Journal,
    response: RecorderResponse,
}

#[derive(Clone)]
enum RecorderResponse {
    Handled,
    Unhandled,
    Close,
    Open {
        glyph: char,
        response: Box<RecorderResponse>,
    },
}

impl Recorder {
    fn new(name: &'static str, glyph: char, journal: Journal, response: RecorderResponse) -> Self {
        Self {
            name,
            glyph,
            journal,
            response,
        }
    }
}

impl View for Recorder {
    fn handle_key(&mut self, _event: KeyEvent) -> EventResult {
        self.journal
            .borrow_mut()
            .push(format!("handle_key:{}", self.name));
        match self.response.clone() {
            RecorderResponse::Handled => EventResult::Handled,
            RecorderResponse::Unhandled => EventResult::Unhandled,
            RecorderResponse::Close => EventResult::Close,
            RecorderResponse::Open { glyph, response } => {
                let j = self.journal.clone();
                EventResult::OpenView(Box::new(Recorder::new(
                    "opened",
                    glyph,
                    j,
                    (*response).clone(),
                )))
            }
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.journal
            .borrow_mut()
            .push(format!("render:{}", self.name));
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(self.glyph);
                }
            }
        }
    }

    fn on_enter(&mut self) {
        self.journal
            .borrow_mut()
            .push(format!("enter:{}", self.name));
    }

    fn on_exit(&mut self) {
        self.journal
            .borrow_mut()
            .push(format!("exit:{}", self.name));
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

fn j() -> Journal {
    Rc::new(RefCell::new(Vec::new()))
}

fn key() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn recorder(
    name: &'static str,
    glyph: char,
    journal: &Journal,
    response: RecorderResponse,
) -> Box<dyn View> {
    Box::new(Recorder::new(name, glyph, journal.clone(), response))
}

// ─── Stack mechanics ──────────────────────────────────────────────

#[test]
fn new_stack_is_empty() {
    let s = ViewStack::new();
    assert!(s.is_empty());
    assert_eq!(s.len(), 0);
    assert_eq!(s.top_name(), None);
}

#[test]
fn push_and_pop_update_length() {
    let journal = j();
    let mut s = ViewStack::new();

    s.push(recorder("a", 'a', &journal, RecorderResponse::Handled));
    assert_eq!(s.len(), 1);
    assert_eq!(s.top_name(), Some("a"));

    s.push(recorder("b", 'b', &journal, RecorderResponse::Handled));
    assert_eq!(s.len(), 2);
    assert_eq!(s.top_name(), Some("b"));

    let popped = s.pop().expect("pop b");
    assert_eq!(popped.name(), "b");
    assert_eq!(s.len(), 1);
    assert_eq!(s.top_name(), Some("a"));
}

#[test]
fn push_fires_on_enter_hook() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder("a", 'a', &journal, RecorderResponse::Handled));
    assert!(journal.borrow().iter().any(|e| e == "enter:a"));
}

#[test]
fn pop_fires_on_exit_hook() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder("a", 'a', &journal, RecorderResponse::Handled));
    journal.borrow_mut().clear();
    s.pop();
    assert!(journal.borrow().iter().any(|e| e == "exit:a"));
}

// ─── Event routing ────────────────────────────────────────────────

#[test]
fn handle_key_on_empty_returns_unhandled() {
    let mut s = ViewStack::new();
    match s.handle_key(key()) {
        EventResult::Unhandled => {}
        other => panic!("expected Unhandled, got {other:?}"),
    }
}

#[test]
fn top_view_handles_event_without_falling_through() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder("base", 'b', &journal, RecorderResponse::Unhandled));
    s.push(recorder("top", 't', &journal, RecorderResponse::Handled));
    journal.borrow_mut().clear();

    match s.handle_key(key()) {
        EventResult::Handled => {}
        other => panic!("expected Handled, got {other:?}"),
    }
    let log = journal.borrow();
    assert!(log.contains(&"handle_key:top".to_string()));
    assert!(
        !log.contains(&"handle_key:base".to_string()),
        "base should NOT have been called; log={:?}",
        *log
    );
}

#[test]
fn unhandled_event_falls_through_to_lower_view() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder("base", 'b', &journal, RecorderResponse::Handled));
    s.push(recorder("top", 't', &journal, RecorderResponse::Unhandled));
    journal.borrow_mut().clear();

    match s.handle_key(key()) {
        EventResult::Handled => {}
        other => panic!("expected Handled (after fall-through), got {other:?}"),
    }
    let log = journal.borrow().clone();
    let top_idx = log.iter().position(|e| e == "handle_key:top");
    let base_idx = log.iter().position(|e| e == "handle_key:base");
    assert!(top_idx.is_some() && base_idx.is_some(), "both called; log={log:?}");
    assert!(top_idx < base_idx, "top must fire before base");
}

#[test]
fn close_result_pops_the_top_view() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder("base", 'b', &journal, RecorderResponse::Handled));
    s.push(recorder("top", 't', &journal, RecorderResponse::Close));

    match s.handle_key(key()) {
        EventResult::Handled => {}
        other => panic!("expected Handled after close, got {other:?}"),
    }
    assert_eq!(s.len(), 1);
    assert_eq!(s.top_name(), Some("base"));
    assert!(
        journal.borrow().iter().any(|e| e == "exit:top"),
        "closing should fire exit hook"
    );
}

#[test]
fn open_view_result_pushes_a_new_view() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder(
        "base",
        'b',
        &journal,
        RecorderResponse::Open {
            glyph: 'o',
            response: Box::new(RecorderResponse::Handled),
        },
    ));

    match s.handle_key(key()) {
        EventResult::Handled => {}
        other => panic!("expected Handled after open, got {other:?}"),
    }
    assert_eq!(s.len(), 2);
    assert_eq!(s.top_name(), Some("opened"));
}

// ─── Rendering ────────────────────────────────────────────────────

#[test]
fn render_draws_bottom_up() {
    let journal = j();
    let mut s = ViewStack::new();
    s.push(recorder("base", 'b', &journal, RecorderResponse::Handled));
    s.push(recorder("top", 't', &journal, RecorderResponse::Handled));
    journal.borrow_mut().clear();

    let area = Rect::new(0, 0, 4, 1);
    let mut buf = Buffer::empty(area);
    s.render(area, &mut buf);

    let log = journal.borrow().clone();
    let base_idx = log.iter().position(|e| e == "render:base");
    let top_idx = log.iter().position(|e| e == "render:top");
    assert!(base_idx.is_some() && top_idx.is_some(), "both render; log={log:?}");
    assert!(base_idx < top_idx, "base must render before top (bottom-up)");

    // Final buffer shows top view's glyph because it overwrites base.
    let row: String = (area.left()..area.right())
        .map(|x| buf.cell((x, area.top())).unwrap().symbol().to_string())
        .collect();
    assert_eq!(row, "tttt", "top overlay should win the pixel");
}
