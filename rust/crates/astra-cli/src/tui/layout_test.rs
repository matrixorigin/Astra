/// Offline layout verification using TestBackend.
/// Simulates full conversation flow and dumps each frame as text.
/// Run: cargo test -p astra-cli -- layout_test --nocapture
#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use ratatui::widgets::Clear;
    use ratatui::layout::Rect;
    use ratatui::buffer::Buffer;

    use crate::tui::render::renderable::{
        FlexRenderable, Renderable, RenderableExt, RenderableItem,
    };
    use crate::tui::render::Insets;
    use crate::tui::chat_cell::assistant_cell::AssistantChatCell;
    use crate::tui::chat_cell::user_cell::UserChatCell;
    use crate::tui::chat_cell::tool_cell::ToolChatCell;
    use crate::tui::chat_cell::ChatCell;

    const W: u16 = 60;

    /// Renders the viewport (active_cell + bottom_pane) into a TestBackend buffer
    /// and returns the text representation.
    fn render_viewport(
        label: &str,
        active_cell: &Option<Box<dyn ChatCell>>,
        bp_lines: &[&str], // simplified bottom pane content
    ) -> String {
        // Build active cell renderable
        let ac_renderable: RenderableItem<'_> = match active_cell {
            Some(cell) => {
                let lines = cell.display_lines(W);
                let text = ratatui::text::Text::from(lines);
                let para = ratatui::widgets::Paragraph::new(text);
                RenderableItem::Owned(Box::new(para)).inset(Insets::tlbr(1, 0, 0, 0))
            }
            None => RenderableItem::Owned(Box::new(())),
        };

        // Simplified bottom pane
        let bp = SimpleBP(bp_lines.iter().map(|s| s.to_string()).collect());
        let bp_item = RenderableItem::Owned(Box::new(bp) as Box<dyn Renderable>)
            .inset(Insets::tlbr(1, 0, 0, 0));

        let mut flex = FlexRenderable::new();
        flex.push(1, ac_renderable);
        flex.push(0, bp_item);

        let total_h = flex.desired_height(W);
        let h = total_h.max(1);

        let backend = TestBackend::new(W, h);
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(Rect::new(0, 0, W, h)),
            },
        )
        .unwrap();

        terminal
            .draw(|frame| {
                let area = frame.area();
                Clear.render(area, frame.buffer_mut());
                flex.render(area, frame.buffer_mut());
            })
            .unwrap();

        // Extract text from buffer
        let buf = terminal.backend().buffer().clone();
        let mut output = format!("=== {} (h={}) ===\n", label, h);
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                line.push_str(cell.symbol());
            }
            let trimmed = line.trim_end();
            output.push_str(&format!("{:2}|{}\n", y, trimmed));
        }
        output
    }

    /// Simulates scrollback: returns what would be written to terminal history
    fn scrollback_lines(cell: &dyn ChatCell) -> String {
        let lines = cell.display_lines(W);
        let mut out = String::new();
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            out.push_str(&format!("  |{}\n", text));
        }
        out
    }

    struct SimpleBP(Vec<String>);
    impl Renderable for SimpleBP {
        fn render(&self, area: Rect, buf: &mut Buffer) {
            for (i, line) in self.0.iter().enumerate() {
                if i < area.height as usize {
                    buf.set_string(area.x, area.y + i as u16, line, ratatui::style::Style::default());
                }
            }
        }
        fn desired_height(&self, _w: u16) -> u16 {
            self.0.len() as u16
        }
    }

    use ratatui::widgets::Widget;

    #[test]
    fn test_full_conversation_flow() {
        let bp = vec!["› Ask astra to do anything", "  ? for shortcuts"];
        let mut results = String::new();

        // ── State 1: IDLE ──
        results.push_str(&render_viewport("1. IDLE", &None, &bp));
        results.push('\n');

        // ── State 2: User sends "hi" → user message goes to scrollback ──
        let user1 = UserChatCell::new("hi".to_string());
        results.push_str("--- User message 'hi' → scrollback ---\n");
        results.push_str(&scrollback_lines(&user1));
        results.push('\n');

        // ── State 3: Working (thinking started, no content) ──
        let mut working = AssistantChatCell::from_rendered(vec![]);
        working.start_thinking();
        let ac3: Option<Box<dyn ChatCell>> = Some(Box::new(working));
        let bp_active = vec!["› Ask astra to do anything", "  ⏹ interrupt"];
        results.push_str(&render_viewport("3. WORKING", &ac3, &bp_active));
        results.push('\n');

        // ── State 4: Response streaming ──
        let streaming = AssistantChatCell::from_rendered(vec![
            ratatui::text::Line::raw("Hello! How can I help you?"),
        ]);
        let ac4: Option<Box<dyn ChatCell>> = Some(Box::new(streaming));
        results.push_str(&render_viewport("4. STREAMING", &ac4, &bp_active));
        results.push('\n');

        // ── State 5: Response complete → flush to scrollback, back to idle ──
        let response1 = AssistantChatCell::from_rendered(vec![
            ratatui::text::Line::raw("Hello! How can I help you?"),
        ]);
        results.push_str("--- Response → scrollback ---\n");
        results.push_str(&scrollback_lines(&response1));
        results.push('\n');

        // ── State 6: User sends "what is 2+2" ──
        let user2 = UserChatCell::new("what is 2+2".to_string());
        results.push_str("--- User message 'what is 2+2' → scrollback ---\n");
        results.push_str(&scrollback_lines(&user2));
        results.push('\n');

        // ── State 7: Working again ──
        let mut working2 = AssistantChatCell::from_rendered(vec![]);
        working2.start_thinking();
        let ac7: Option<Box<dyn ChatCell>> = Some(Box::new(working2));
        results.push_str(&render_viewport("7. WORKING (2nd)", &ac7, &bp_active));
        results.push('\n');

        // ── State 8: Short response ──
        let short = AssistantChatCell::from_rendered(vec![
            ratatui::text::Line::raw("4"),
        ]);
        let ac8: Option<Box<dyn ChatCell>> = Some(Box::new(short));
        results.push_str(&render_viewport("8. SHORT RESPONSE", &ac8, &bp_active));
        results.push('\n');

        // ── State 9: Tool call ──
        let tool = ToolChatCell::new_running("bash".to_string(), "ls -la".to_string());
        let ac9: Option<Box<dyn ChatCell>> = Some(Box::new(tool));
        results.push_str(&render_viewport("9. TOOL RUNNING", &ac9, &bp_active));
        results.push('\n');

        // ── State 10: Tool completed ──
        let mut tool_done = ToolChatCell::new_running("bash".to_string(), "ls -la".to_string());
        tool_done.complete("success", 52, Some("file1.rs\nfile2.rs\nfile3.rs".to_string()));
        let ac10: Option<Box<dyn ChatCell>> = Some(Box::new(tool_done));
        results.push_str(&render_viewport("10. TOOL COMPLETED", &ac10, &bp_active));

        // Write and print
        std::fs::write("/tmp/tui_layout_test.txt", &results).unwrap();
        println!("{results}");
    }
}
