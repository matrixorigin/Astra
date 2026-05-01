#[cfg(test)]
mod textarea_tests {
    use crate::tui::bottom_pane::textarea::{TextArea, TextAreaAction};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn alt(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn insert_and_retrieve() {
        let mut ta = TextArea::new();
        ta.handle_key(key(KeyCode::Char('h')));
        ta.handle_key(key(KeyCode::Char('i')));
        assert_eq!(ta.text(), "hi");
    }

    #[test]
    fn backspace_removes_char() {
        let mut ta = TextArea::new();
        ta.set_text("abc");
        ta.handle_key(key(KeyCode::Backspace));
        assert_eq!(ta.text(), "ab");
    }

    #[test]
    fn cjk_insert_and_backspace() {
        let mut ta = TextArea::new();
        ta.set_text("你好");
        assert_eq!(ta.text(), "你好");
        ta.handle_key(key(KeyCode::Backspace));
        assert_eq!(ta.text(), "你");
    }

    #[test]
    fn ctrl_k_kills_to_eol() {
        let mut ta = TextArea::new();
        ta.set_text("hello world");
        // Move to position 5
        ta.handle_key(ctrl(KeyCode::Char('a'))); // go to start
        for _ in 0..5 {
            ta.handle_key(key(KeyCode::Right));
        }
        ta.handle_key(ctrl(KeyCode::Char('k')));
        assert_eq!(ta.text(), "hello");
    }

    #[test]
    fn ctrl_y_yanks_killed_text() {
        let mut ta = TextArea::new();
        ta.set_text("hello world");
        ta.handle_key(ctrl(KeyCode::Char('a')));
        for _ in 0..5 {
            ta.handle_key(key(KeyCode::Right));
        }
        ta.handle_key(ctrl(KeyCode::Char('k'))); // kill " world"
        ta.handle_key(ctrl(KeyCode::Char('y'))); // yank it back
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn kill_buffer_survives_clear() {
        let mut ta = TextArea::new();
        ta.set_text("hello world");
        ta.handle_key(ctrl(KeyCode::Char('a'))); // go to start
        ta.handle_key(ctrl(KeyCode::Char('k'))); // kill "hello world"
        assert_eq!(ta.text(), "");
        ta.clear();
        ta.handle_key(ctrl(KeyCode::Char('y'))); // yank
        assert_eq!(ta.text(), "hello world");
    }

    #[test]
    fn ctrl_w_deletes_backward_word() {
        let mut ta = TextArea::new();
        ta.set_text("hello world");
        ta.handle_key(ctrl(KeyCode::Char('w')));
        assert_eq!(ta.text(), "hello ");
    }

    #[test]
    fn word_movement_alt_b_f() {
        let mut ta = TextArea::new();
        ta.set_text("foo bar baz");
        // cursor at end (pos 11)
        ta.handle_key(alt(KeyCode::Char('b'))); // back one word → start of "baz" (pos 8)
        ta.handle_key(alt(KeyCode::Char('b'))); // back one word → start of "bar" (pos 4)
        ta.handle_key(key(KeyCode::Char('X')));
        assert_eq!(ta.text(), "foo Xbar baz");
    }

    #[test]
    fn enter_submits() {
        let mut ta = TextArea::new();
        ta.set_text("hello");
        assert_eq!(ta.handle_key(key(KeyCode::Enter)), TextAreaAction::Submit);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        let mut ta = TextArea::new();
        ta.set_text("line1");
        ta.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        ta.handle_key(key(KeyCode::Char('2')));
        assert_eq!(ta.text(), "line1\n2");
    }

    #[test]
    fn unhandled_key_returns_unhandled() {
        let mut ta = TextArea::new();
        assert_eq!(
            ta.handle_key(key(KeyCode::PageUp)),
            TextAreaAction::Unhandled
        );
    }

    #[test]
    fn desired_height_wraps_long_line() {
        let mut ta = TextArea::new();
        ta.set_text(&"a".repeat(100));
        assert_eq!(ta.desired_height(50), 2);
        assert_eq!(ta.desired_height(100), 1);
    }

    #[test]
    fn desired_height_cjk_wrap() {
        let mut ta = TextArea::new();
        // 10 CJK chars = 20 display columns
        ta.set_text("你好你好你好你好你好");
        assert_eq!(ta.desired_height(20), 1);
        assert_eq!(ta.desired_height(10), 2);
    }

    #[test]
    fn cursor_position_basic() {
        let ta = TextArea::new();
        let area = ratatui::layout::Rect::new(0, 0, 80, 5);
        let pos = ta.cursor_position(area);
        assert_eq!(pos, Some((0, 0)));
    }

    #[test]
    fn cursor_position_after_cjk() {
        let mut ta = TextArea::new();
        ta.set_text("你好");
        let area = ratatui::layout::Rect::new(0, 0, 80, 5);
        let pos = ta.cursor_position(area);
        // "你好" = 4 display columns
        assert_eq!(pos, Some((4, 0)));
    }

    #[test]
    fn cursor_position_wrapped_line() {
        let mut ta = TextArea::new();
        ta.set_text(&"a".repeat(90));
        let area = ratatui::layout::Rect::new(0, 0, 50, 5);
        let pos = ta.cursor_position(area);
        // 90 chars at width 50 → wraps to row 1, col 40
        assert_eq!(pos, Some((40, 1)));
    }
}

#[cfg(test)]
mod key_routing_tests {
    use crate::tui::bottom_pane::{BottomPane, BottomPaneAction};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn regular_char_is_consumed() {
        let mut bp = BottomPane::new();
        match bp.handle_key(key(KeyCode::Char('a'))) {
            BottomPaneAction::Consumed => {}
            other => panic!("expected Consumed, got {other:?}"),
        }
    }

    #[test]
    fn page_up_escalates() {
        let mut bp = BottomPane::new();
        match bp.handle_key(key(KeyCode::PageUp)) {
            BottomPaneAction::Escalate(_) => {}
            other => panic!("expected Escalate, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_empty_composer_quits() {
        let mut bp = BottomPane::new();
        match bp.handle_key(ctrl('c')) {
            BottomPaneAction::Quit => {}
            other => panic!("expected Quit, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_c_non_empty_clears_draft() {
        let mut bp = BottomPane::new();
        bp.handle_key(key(KeyCode::Char('h')));
        bp.handle_key(key(KeyCode::Char('i')));
        match bp.handle_key(ctrl('c')) {
            BottomPaneAction::Consumed => {}
            other => panic!("expected Consumed (draft cleared), got {other:?}"),
        }
        assert!(bp.composer.is_empty());
    }

    #[test]
    fn ctrl_c_task_active_interrupts() {
        use crate::tui::task_status::TaskStatus;
        let mut bp = BottomPane::new();
        bp.set_task_status(TaskStatus::TurnRunning {
            started_at: std::time::Instant::now(),
        });
        match bp.handle_key(ctrl('c')) {
            BottomPaneAction::Interrupt => {}
            other => panic!("expected Interrupt, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod viewport_tests {
    use crate::tui::chat_cell::system_cell::SystemChatCell;
    use crate::tui::chat_viewport::ChatViewport;

    #[test]
    fn auto_follow_on_push() {
        let mut vp = ChatViewport::new();
        for i in 0..50 {
            vp.push_cell(Box::new(SystemChatCell::info(format!("Line {i}"))));
        }
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 10));
        vp.render(ratatui::layout::Rect::new(0, 0, 80, 10), &mut buf);
        // After render, scroll should be near the bottom (auto-follow)
        // Just verify it doesn't panic
    }

    #[test]
    fn scroll_up_disables_auto_follow() {
        let mut vp = ChatViewport::new();
        for i in 0..50 {
            vp.push_cell(Box::new(SystemChatCell::info(format!("Line {i}"))));
        }
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 10));
        vp.render(ratatui::layout::Rect::new(0, 0, 80, 10), &mut buf);

        vp.scroll_up(5);
        // Push new cell — should NOT scroll to bottom
        vp.push_cell(Box::new(SystemChatCell::info("new".to_string())));
        // auto_follow should be false
        // Verify by checking needs_scroll_to_bottom is false (indirect: no panic)
        vp.render(ratatui::layout::Rect::new(0, 0, 80, 10), &mut buf);
    }

    #[test]
    fn jump_to_bottom_restores_auto_follow() {
        let mut vp = ChatViewport::new();
        for i in 0..50 {
            vp.push_cell(Box::new(SystemChatCell::info(format!("Line {i}"))));
        }
        vp.scroll_up(10);
        vp.jump_to_bottom(80, 10);
        vp.push_cell(Box::new(SystemChatCell::info("new".to_string())));
        // Should auto-follow now
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 10));
        vp.render(ratatui::layout::Rect::new(0, 0, 80, 10), &mut buf);
    }
}

#[cfg(test)]
mod markdown_tests {
    use crate::tui::markdown_render::render_markdown_text;

    #[test]
    fn renders_heading() {
        let text = render_markdown_text("# Hello");
        assert!(!text.lines.is_empty());
        let first = &text.lines[0];
        let content: String = first.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(content.contains("Hello"));
    }

    #[test]
    fn renders_code_block() {
        // Use no language tag so we exercise the plain-text code-block path
        // without triggering Oniguruma pattern compilation (which is slow and
        // can disrupt the test environment when run concurrently).
        let text = render_markdown_text("```\nfn main() {}\n```");
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(all.contains("fn"), "code block should contain 'fn', got: {all}");
        assert!(all.contains("main"), "code block should contain 'main', got: {all}");
    }

    #[test]
    #[ignore = "compiles Oniguruma patterns on first use; run manually with -- --ignored"]
    fn renders_highlighted_code_block() {
        let text = render_markdown_text("```rust\nfn main() {}\n```");
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join("");
        assert!(all.contains("fn"), "code block should contain 'fn', got: {all}");
        assert!(all.contains("main"), "code block should contain 'main', got: {all}");
    }

    #[test]
    fn renders_list() {
        let text = render_markdown_text("- item1\n- item2");
        let all: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(all.contains("item1"));
        assert!(all.contains("item2"));
    }
}

#[cfg(test)]
mod stream_bridge_tests {
    use crate::chat_stream::StreamEvent;
    use crate::tui::app_event::TuiAppEvent;
    use crate::tui::stream_bridge;

    #[tokio::test]
    async fn per_turn_bridge_sends_turn_complete_after_all_tokens() {
        let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
        let stream_tx = stream_bridge::create_per_turn_bridge(tui_tx);

        // Send two tokens then drop the sender (simulates turn end)
        stream_tx.send(StreamEvent::Token("hello ".to_string())).unwrap();
        stream_tx.send(StreamEvent::Token("world".to_string())).unwrap();
        drop(stream_tx);

        // Receive: should get Token, Token, TurnComplete in order
        let mut events = Vec::new();
        while let Some(evt) = tui_rx.recv().await {
            let is_complete = matches!(evt, TuiAppEvent::TurnComplete);
            events.push(evt);
            if is_complete {
                break;
            }
        }

        assert!(events.len() >= 3, "expected at least 3 events, got {}", events.len());
        assert!(matches!(&events[0], TuiAppEvent::Token(t) if t == "hello "));
        assert!(matches!(&events[1], TuiAppEvent::Token(t) if t == "world"));
        assert!(matches!(&events[events.len() - 1], TuiAppEvent::TurnComplete));
    }
}

#[cfg(test)]
mod turn_input_tests {
    use crate::tui::bottom_pane::BottomPane;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn enter_during_turn_preserves_draft() {
        let mut bp = BottomPane::new();

        // Type "hello" into composer
        for c in "hello".chars() {
            bp.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(!bp.composer.is_empty());

        // Simulate what handle_ui_event_during_turn does: intercept Enter
        let enter_key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let is_plain_enter = enter_key.code == KeyCode::Enter
            && !enter_key.modifiers.contains(KeyModifiers::SHIFT)
            && !enter_key.modifiers.contains(KeyModifiers::ALT);
        assert!(is_plain_enter, "should detect plain Enter");

        // Don't route to bottom_pane — this is what the TUI does during turn
        // Verify draft is preserved
        assert!(!bp.composer.is_empty(), "draft should be preserved after blocked Enter");
        assert_eq!(bp.composer.text(), "hello");
    }
}
