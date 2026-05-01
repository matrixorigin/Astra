use std::io::{self, Stdout, stdout};

use crossterm::{
    SynchronizedUpdate,
    cursor,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::Print,
    terminal::{disable_raw_mode, enable_raw_mode},
};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;

use super::custom_terminal;

pub(crate) type CustomTerminal = custom_terminal::Terminal<CrosstermBackend<Stdout>>;

pub(crate) struct TerminalGuard {
    pub terminal: CustomTerminal,
    pub pending_history: Vec<Line<'static>>,
    is_zellij: bool,
}

struct RawModeGuard;
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableBracketedPaste, cursor::Show);
    }
}

impl TerminalGuard {
    pub fn init() -> io::Result<Self> {
        enable_raw_mode()?;
        execute!(stdout(), EnableBracketedPaste)?;

        let _early_guard = RawModeGuard;

        let backend = CrosstermBackend::new(stdout());
        let terminal = CustomTerminal::with_options(backend)?;

        std::mem::forget(_early_guard);

        let is_zellij = std::env::var("ZELLIJ_SESSION_NAME").is_ok();

        static PANIC_HOOK_INSTALLED: std::sync::Once = std::sync::Once::new();
        PANIC_HOOK_INSTALLED.call_once(|| {
            let original_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |panic_info| {
                let _ = disable_raw_mode();
                let _ = execute!(stdout(), DisableBracketedPaste, cursor::Show);
                original_hook(panic_info);
            }));
        });

        Ok(Self {
            terminal,
            pending_history: Vec::new(),
            is_zellij,
        })
    }

    pub fn queue_history_lines(&mut self, lines: Vec<Line<'static>>) {
        self.pending_history.extend(lines);
    }

    /// Draw the viewport — matches Codex tui.rs::draw() sequence:
    /// 1. update_inline_viewport (scroll if height changed, clear if viewport moved)
    /// 2. flush_pending_history (insert above viewport)
    /// 3. invalidate_viewport only when needed (viewport moved or Zellij flush)
    /// 4. terminal.draw (diff buffer + ClearToEnd handles stale content per-row)
    pub fn draw(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> io::Result<()> {
        stdout().sync_update(|_| {
            let terminal = &mut self.terminal;

            let mut needs_full_repaint =
                Self::update_inline_viewport(terminal, height, self.is_zellij)?;

            needs_full_repaint |= Self::flush_pending_history(
                terminal,
                &mut self.pending_history,
                self.is_zellij,
            )?;

            if needs_full_repaint {
                terminal.invalidate_viewport();
            }

            terminal.draw(draw_fn)
        })?
    }

    /// Force a clear+redraw of the viewport. Use after operations that
    /// modify terminal content outside ratatui's knowledge (e.g. insert_history
    /// in non-Zellij mode where we rely on ClearToEnd in the diff).
    #[allow(dead_code)]
    pub fn force_clear_viewport(&mut self) -> io::Result<()> {
        self.terminal.clear()
    }

    /// Matches Codex tui.rs::update_inline_viewport():
    /// If viewport would extend past screen bottom, scroll content above it up.
    /// If viewport area changed, clear old area and set new one.
    fn update_inline_viewport(
        terminal: &mut CustomTerminal,
        height: u16,
        is_zellij: bool,
    ) -> io::Result<bool> {
        let size = terminal.size()?;
        let mut area = terminal.viewport_area;
        area.height = height.min(size.height);
        area.width = size.width;
        let mut needs_full_repaint = false;

        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if is_zellij {
                queue!(
                    terminal.backend_mut(),
                    cursor::MoveTo(0, size.height.saturating_sub(1))
                )?;
                for _ in 0..scroll_by {
                    queue!(terminal.backend_mut(), Print("\n"))?;
                }
                needs_full_repaint = true;
            } else {
                let region_bottom = area.top();
                if region_bottom > 0 {
                    queue!(
                        terminal.backend_mut(),
                        Print(format!("\x1b[1;{}r", region_bottom)),
                        cursor::MoveTo(0, 0),
                        Print(format!("\x1b[{}S", scroll_by)),
                        Print("\x1b[r"),
                    )?;
                }
            }
            area.y = size.height - area.height;
        }

        if area != terminal.viewport_area {
            terminal.clear()?;
            terminal.set_viewport_area(area);
            needs_full_repaint = true;
        }

        Ok(needs_full_repaint)
    }

    fn flush_pending_history(
        terminal: &mut CustomTerminal,
        pending: &mut Vec<Line<'static>>,
        is_zellij: bool,
    ) -> io::Result<bool> {
        if pending.is_empty() {
            return Ok(false);
        }

        let lines = std::mem::take(pending);
        super::insert_history::insert_history_lines_with_terminal(
            terminal,
            &lines,
            is_zellij,
        )?;

        Ok(is_zellij)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let area = self.terminal.viewport_area;
        let _ = execute!(
            stdout(),
            cursor::MoveTo(0, area.bottom()),
            cursor::Show
        );
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableBracketedPaste);
        let _ = println!();
    }
}
