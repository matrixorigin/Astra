/// Offline layout test — renders each state to a TestBackend and dumps to file.
/// Run with: cargo test -p astra-cli -- tui::layout_test::test_layout_states --nocapture
#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use ratatui::widgets::Clear;

    use crate::tui::render::renderable::{FlexRenderable, RenderableItem, Renderable, RenderableExt};
    use crate::tui::render::Insets;
    use crate::tui::chat_cell::assistant_cell::AssistantChatCell;
    use crate::tui::chat_cell::ChatCell;
    use crate::tui::bottom_pane::BottomPane;

    fn render_state(
        label: &str,
        active_cell: &Option<Box<dyn ChatCell>>,
        bottom_pane: &mut BottomPane,
        width: u16,
    ) -> String {
        let ac_renderable: RenderableItem<'_> = match active_cell {
            Some(cell) => {
                let lines = cell.display_lines(width);
                let text = ratatui::text::Text::from(lines);
                let para = ratatui::widgets::Paragraph::new(text);
                RenderableItem::Owned(Box::new(para))
                    .inset(Insets::tlbr(1, 0, 0, 0))
            }
            None => RenderableItem::Owned(Box::new(())),
        };

        let bp_h = bottom_pane.desired_height(width);
        let bp_renderable = SimpleRenderable { height: bp_h, label: "bottom_pane".into() };
        let bp_item = RenderableItem::Owned(Box::new(bp_renderable) as Box<dyn Renderable>)
            .inset(Insets::tlbr(1, 0, 0, 0));

        let mut flex = FlexRenderable::new();
        flex.push(1, ac_renderable);
        flex.push(0, bp_item);

        let total_h = flex.desired_height(width);

        let backend = TestBackend::new(width, total_h.max(1));
        let mut terminal = Terminal::with_options(
            backend,
            ratatui::TerminalOptions {
                viewport: ratatui::Viewport::Fixed(ratatui::layout::Rect::new(0, 0, width, total_h.max(1))),
            },
        ).unwrap();

        terminal.draw(|frame| {
            let area = frame.area();
            Clear.render(area, frame.buffer_mut());
            flex.render(area, frame.buffer_mut());
        }).unwrap();

        let buf = terminal.backend().buffer().clone();
        let mut output = format!("=== {} (total_h={}, width={}) ===\n", label, total_h, width);
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                line.push_str(cell.symbol());
            }
            output.push_str(&format!("{:2}|{}|\n", y, line.trim_end()));
        }
        output
    }

    struct SimpleRenderable {
        height: u16,
        label: String,
    }

    impl Renderable for SimpleRenderable {
        fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
            if area.height > 0 {
                buf.set_string(area.x, area.y, &self.label, ratatui::style::Style::default());
            }
        }
        fn desired_height(&self, _width: u16) -> u16 {
            self.height
        }
    }

    use ratatui::widgets::Widget;

    #[test]
    fn test_layout_states() {
        let width = 40u16;
        let mut bp = BottomPane::new();
        let mut results = String::new();

        // State 1: Idle (no active cell)
        let state1 = render_state("IDLE", &None, &mut bp, width);
        results.push_str(&state1);
        results.push('\n');

        // State 2: Working (thinking, no content)
        let mut working_cell = AssistantChatCell::from_rendered(vec![]);
        working_cell.start_thinking();
        let ac2: Option<Box<dyn ChatCell>> = Some(Box::new(working_cell));
        let state2 = render_state("WORKING", &ac2, &mut bp, width);
        results.push_str(&state2);
        results.push('\n');

        // State 3: Streaming (has content)
        let mut streaming_cell = AssistantChatCell::from_rendered(vec![
            ratatui::text::Line::raw("Hello world"),
            ratatui::text::Line::raw("Second line of response"),
        ]);
        let ac3: Option<Box<dyn ChatCell>> = Some(Box::new(streaming_cell));
        let state3 = render_state("STREAMING", &ac3, &mut bp, width);
        results.push_str(&state3);
        results.push('\n');

        // State 4: Long response
        let mut long_lines = vec![];
        for i in 0..8 {
            long_lines.push(ratatui::text::Line::raw(format!("Line {} of long response", i)));
        }
        let long_cell = AssistantChatCell::from_rendered(long_lines);
        let ac4: Option<Box<dyn ChatCell>> = Some(Box::new(long_cell));
        let state4 = render_state("LONG_RESPONSE", &ac4, &mut bp, width);
        results.push_str(&state4);

        // Write to file
        std::fs::write("/tmp/tui_layout_test.txt", &results).unwrap();
        println!("{results}");
    }
}
