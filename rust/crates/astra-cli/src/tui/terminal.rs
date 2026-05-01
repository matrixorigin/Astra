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

    /// Draw the viewport. Follows the Codex draw sequence:
    /// 1. Update inline viewport (scroll if needed)
    /// 2. Flush pending history lines above viewport
    /// 3. Render active UI into viewport
    pub fn draw(
        &mut self,
        height: u16,
        draw_fn: impl FnOnce(&mut custom_terminal::Frame),
    ) -> io::Result<()> {
        stdout().sync_update(|_| {
            let terminal = &mut self.terminal;

            // 1. Update viewport position
            let mut needs_repaint =
                Self::update_inline_viewport(terminal, height, self.is_zellij)?;

            // 2. Flush history
            needs_repaint |= Self::flush_pending_history(
                terminal,
                &mut self.pending_history,
                self.is_zellij,
            )?;

            if needs_repaint {
                terminal.invalidate_viewport();
                // Also clear any stale content below viewport on physical terminal
                let vp = terminal.viewport_area;
                queue!(
                    terminal.backend_mut(),
                    cursor::MoveTo(0, vp.bottom()),
                    Print("\x1b[J"), // ED: clear from cursor to end of screen
                )?;
                std::io::Write::flush(terminal.backend_mut())?;
            }

            // 3. Render
            terminal.draw(draw_fn)
        })?
    }

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


        // If viewport would extend past bottom, scroll content above it up
        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            if is_zellij {
                // Zellij: emit newlines at screen bottom
                queue!(
                    terminal.backend_mut(),
                    cursor::MoveTo(0, size.height.saturating_sub(1))
                )?;
                for _ in 0..scroll_by {
                    queue!(terminal.backend_mut(), Print("\n"))?;
                }
            } else {
                // Standard: scroll content above viewport UP to make room below
                // Set scroll region to rows above viewport, then Scroll Up
                let region_bottom = area.top(); // top of current viewport
                if region_bottom > 0 {
                    queue!(
                        terminal.backend_mut(),
                        Print(format!("\x1b[1;{}r", region_bottom)), // Set scroll region
                        cursor::MoveTo(0, 0),
                        Print(format!("\x1b[{}S", scroll_by)), // Scroll Up n lines
                        Print("\x1b[r"), // Reset scroll region
                    )?;
                }
            }
            area.y = size.height - area.height;
        }

        if area != terminal.viewport_area {
            // Clear from min(old_top, new_top) to screen bottom BEFORE moving viewport
            // This prevents stale viewport content from leaking into scrollback
            let previous_area = terminal.viewport_area;
            let clear_y = previous_area.y.min(area.y);
            queue!(
                terminal.backend_mut(),
                cursor::MoveTo(0, clear_y),
                Print("\x1b[J"), // ED: clear to end of screen
            )?;
            std::io::Write::flush(terminal.backend_mut())?;

            terminal.set_viewport_area(area);
            terminal.invalidate_viewport();
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
