use std::collections::VecDeque;
use std::io::{self, Stdout, stdout};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossterm::{
    SynchronizedUpdate, cursor,
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute, queue,
    style::Print,
    terminal::{disable_raw_mode, enable_raw_mode, is_raw_mode_enabled},
};
use ratatui::backend::CrosstermBackend;
use ratatui::text::Line;

use super::custom_terminal;
use super::frame_requester::FrameRequester;
use super::history_cell::{HistoryCell, assistant::AssistantCell};
use super::render::line_utils::sanitize_lines_for_terminal;

pub(crate) type CustomTerminal = custom_terminal::Terminal<CrosstermBackend<Stdout>>;

pub(crate) struct TerminalGuard {
    pub terminal: CustomTerminal,
    pending_history: VecDeque<PendingHistory>,
    is_zellij: bool,
    /// Scrollback is deliberately drained over several frames for very long
    /// replies. This keeps terminal writes from monopolising the same event
    /// loop that owns keyboard input and the composer.
    history_drain_requester: Option<FrameRequester>,
}

// A terminal write can block on a slow terminal emulator or remote PTY. Keep
// each interactive draw intentionally small; the frame requester drains the
// rest without requiring a keypress.
const MAX_HISTORY_LINES_PER_DRAW: usize = 16;
const MAX_HISTORY_CHARS_PER_DRAW: usize = 4 * 1024;
const MAX_LAZY_ASSISTANT_LINES_PER_BATCH: usize = 8;

enum PendingHistory {
    Lines(VecDeque<Line<'static>>),
    Assistant(QueuedAssistantHistory),
}

struct QueuedAssistantHistory {
    cell: Arc<dyn HistoryCell>,
    width: u16,
    next_line: usize,
    /// Final Markdown layout runs on the blocking pool. Until it is ready the
    /// queue remains ordered but must not make the input/render loop wait.
    layout_preparing: Arc<AtomicBool>,
    rendered_complete: bool,
    pending_separators: usize,
    ready: VecDeque<Line<'static>>,
}

/// Guarantees that a completed blocking layout cannot leave the ordered
/// scrollback queue permanently waiting, including if the renderer panics.
struct LayoutPreparationWake {
    preparing: Arc<AtomicBool>,
    requester: Option<FrameRequester>,
}

impl Drop for LayoutPreparationWake {
    fn drop(&mut self) {
        self.preparing.store(false, Ordering::Release);
        if let Some(requester) = self.requester.as_ref() {
            requester.schedule_frame();
        }
    }
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
        static PANIC_HOOK_INSTALLED: std::sync::Once = std::sync::Once::new();
        PANIC_HOOK_INSTALLED.call_once(|| {
            let original_hook = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |panic_info| {
                let _ = disable_raw_mode();
                let _ = execute!(stdout(), DisableBracketedPaste, cursor::Show);
                original_hook(panic_info);
            }));
        });

        enable_raw_mode()?;
        execute!(stdout(), EnableBracketedPaste)?;

        let early_guard = RawModeGuard;

        let backend = CrosstermBackend::new(stdout());
        let terminal = CustomTerminal::with_options(backend)?;

        let is_zellij = std::env::var("ZELLIJ_SESSION_NAME").is_ok();
        let guard = Self {
            terminal,
            pending_history: VecDeque::new(),
            is_zellij,
            history_drain_requester: None,
        };
        // Tell display_sixel the TUI owns the terminal, so it queues images for
        // the event loop to blit on a paused screen instead of writing bytes the
        // render loop would paint over. Cleared in Drop.
        astra_tools::display_sixel::set_tui_active(true);
        // Probe sixel support once, now — raw mode is on and the event-loop input
        // reader hasn't started, so it's safe to read the DA1 reply directly.
        // Cached so display_sixel skips the image (with a message) on terminals
        // that would only show a blank box.
        astra_tools::display_sixel::set_sixel_supported(
            astra_tools::display_sixel::probe_sixel_support(),
        );
        std::mem::forget(early_guard);
        Ok(guard)
    }

    /// Restore the TUI input contract if a slash fallback, tool path, or
    /// platform quirk left the terminal in cooked mode. Without this,
    /// subsequent `/` and Ctrl-C keystrokes are echoed by the terminal
    /// driver instead of reaching crossterm as key events.
    pub fn ensure_tui_modes(&mut self) -> io::Result<()> {
        let raw = is_raw_mode_enabled()?;
        if !raw {
            enable_raw_mode()?;
            execute!(stdout(), EnableBracketedPaste)?;
        }
        Ok(())
    }

    pub fn queue_history_lines(&mut self, lines: Vec<Line<'static>>) {
        let lines = sanitize_lines_for_terminal(lines);
        if !lines.is_empty() {
            self.pending_history
                .push_back(PendingHistory::Lines(lines.into()));
        }
    }

    /// Queue committed cells without eagerly expanding a final assistant
    /// response. The cell remains immutable in `ChatWidget`; this queue only
    /// owns presentation progress and can yield back to keyboard handling
    /// between small scrollback batches.
    pub fn queue_history_cells(&mut self, cells: Vec<Arc<dyn HistoryCell>>, width: u16) {
        for (index, cell) in cells.iter().enumerate() {
            let next = cells.get(index + 1).map(|next| next.as_ref());
            let separators = super::history_cell::separator_rows_after(cell.as_ref(), next);
            if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>()
                && !cell.is_live()
            {
                let layout_preparing = self.prepare_assistant_history_layout(
                    Arc::clone(cell),
                    assistant.has_scrollback_layout(width),
                    width,
                );
                self.pending_history
                    .push_back(PendingHistory::Assistant(QueuedAssistantHistory {
                        cell: Arc::clone(cell),
                        width,
                        next_line: 0,
                        layout_preparing,
                        rendered_complete: false,
                        pending_separators: separators,
                        ready: VecDeque::new(),
                    }));
                continue;
            }

            let mut lines = sanitize_lines_for_terminal(cell.display_lines(width));
            lines.extend(std::iter::repeat_n(Line::default(), separators));
            if !lines.is_empty() {
                self.pending_history
                    .push_back(PendingHistory::Lines(lines.into()));
            }
        }
    }

    pub fn set_history_drain_requester(&mut self, requester: FrameRequester) {
        self.history_drain_requester = Some(requester);
    }

    fn prepare_assistant_history_layout(
        &self,
        cell: Arc<dyn HistoryCell>,
        layout_is_ready: bool,
        width: u16,
    ) -> Arc<AtomicBool> {
        let preparing = Arc::new(AtomicBool::new(false));
        if layout_is_ready {
            return preparing;
        }
        let Some(handle) = tokio::runtime::Handle::try_current().ok() else {
            // Non-interactive callers and narrowly-scoped tests do not always
            // own a Tokio runtime. They retain the correct synchronous path;
            // the interactive TUI always has one and never reaches it.
            return preparing;
        };
        let requester = self.history_drain_requester.clone();
        preparing.store(true, Ordering::Release);
        let completion = LayoutPreparationWake {
            preparing: Arc::clone(&preparing),
            requester,
        };
        handle.spawn_blocking(move || {
            let _completion = completion;
            if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>() {
                assistant.prepare_scrollback_layout(width);
            }
        });
        preparing
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

            let mut needs_full_repaint = Self::update_inline_viewport(terminal, height)?;

            needs_full_repaint |=
                Self::flush_pending_history(terminal, &mut self.pending_history, self.is_zellij)?;

            if needs_full_repaint {
                terminal.invalidate_viewport();
            }

            terminal.draw(draw_fn)
        })??;

        // The frame scheduler intentionally coalesces ordinary redraws. A
        // large final response needs one additional wake for each bounded
        // scrollback batch, otherwise the tail would wait until an unrelated
        // keypress or runtime event arrives.
        if self.pending_history_has_ready_lines()
            && let Some(requester) = self.history_drain_requester.as_ref()
        {
            requester.schedule_frame();
        }
        Ok(())
    }

    /// If viewport would extend past screen bottom, add only the missing rows
    /// at the bottom so displaced content enters native terminal scrollback.
    /// Once enough space exists below a shrunken viewport, later growth reuses
    /// that space instead of printing more blank lines.
    /// If viewport area changed, clear old area and set new one.
    fn update_inline_viewport(terminal: &mut CustomTerminal, height: u16) -> io::Result<bool> {
        let size = terminal.size()?;
        let mut area = terminal.viewport_area;
        area.height = height.min(size.height);
        area.width = size.width;
        let mut needs_full_repaint = false;

        if area.bottom() > size.height {
            let scroll_by = area.bottom() - size.height;
            queue!(
                terminal.backend_mut(),
                cursor::MoveTo(0, size.height.saturating_sub(1))
            )?;
            for _ in 0..scroll_by {
                queue!(terminal.backend_mut(), Print("\n"))?;
            }
            needs_full_repaint = true;
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
        pending: &mut VecDeque<PendingHistory>,
        is_zellij: bool,
    ) -> io::Result<bool> {
        if pending.is_empty() {
            return Ok(false);
        }

        let lines = take_pending_history_batch(pending);
        super::insert_history::insert_history_lines_with_terminal(terminal, &lines, is_zellij)?;

        Ok(is_zellij)
    }

    fn pending_history_has_ready_lines(&self) -> bool {
        self.pending_history
            .front()
            .is_some_and(|pending| match pending {
                PendingHistory::Lines(lines) => !lines.is_empty(),
                PendingHistory::Assistant(queued) => {
                    !queued.layout_preparing.load(Ordering::Acquire)
                        && (!queued.ready.is_empty()
                            || !queued.rendered_complete
                            || queued.pending_separators > 0)
                }
            })
    }

    /// Temporarily leave TUI mode for an external interactive process, then
    /// restore. Workbench slash actions must not use this transition.
    pub async fn with_restored<F, Fut, T>(&mut self, f: F) -> io::Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = T>,
    {
        // Clear viewport area before leaving TUI so the old composer/footer
        // doesn't remain visible while the slash command runs.
        let area = self.terminal.viewport_area;
        if area.height > 0 {
            queue!(
                self.terminal.backend_mut(),
                cursor::MoveTo(0, area.top()),
                Print("\x1b[J"), // ED: clear from viewport top to screen bottom
            )?;
            std::io::Write::flush(self.terminal.backend_mut())?;
        }

        // Position cursor at viewport top and show it
        execute!(stdout(), cursor::MoveTo(0, area.top()), cursor::Show)?;

        // Leave TUI modes
        disable_raw_mode()?;
        execute!(stdout(), DisableBracketedPaste)?;

        // Run the user's closure
        let result = f().await;

        // Restore TUI modes
        self.ensure_tui_modes()?;

        // Flush stale terminal input
        #[cfg(unix)]
        {
            use std::io::IsTerminal;
            use std::os::unix::io::AsRawFd;
            let stdin = std::io::stdin();
            if stdin.is_terminal() {
                let rc = unsafe { nix::libc::tcflush(stdin.as_raw_fd(), nix::libc::TCIFLUSH) };
                if rc != 0 {
                    tracing::warn!(
                        error = %std::io::Error::last_os_error(),
                        "failed to flush stale terminal input"
                    );
                }
            }
        }

        // Clear the screen area where the slash command output was,
        // then force full repaint. The viewport position may have shifted
        // due to slash output scrolling the terminal.
        self.terminal.clear()?;
        self.terminal.invalidate_viewport();

        Ok(result)
    }
}

fn take_history_batch(pending: &mut VecDeque<Line<'static>>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut chars = 0usize;
    while lines.len() < MAX_HISTORY_LINES_PER_DRAW {
        let Some(next) = pending.front() else {
            break;
        };
        let next_chars = next
            .spans
            .iter()
            .map(|span| span.content.len())
            .sum::<usize>();
        if !lines.is_empty() && chars.saturating_add(next_chars) > MAX_HISTORY_CHARS_PER_DRAW {
            break;
        }
        chars = chars.saturating_add(next_chars);
        // Always make progress for one unusually long code/output line. The
        // markdown and terminal wrappers handle its visual rows; retaining it
        // indefinitely would be worse than a bounded exceptional write.
        lines.push(
            pending
                .pop_front()
                .expect("front item exists until this queue is mutated"),
        );
    }
    lines
}

fn pending_line_chars(line: &Line<'static>) -> usize {
    line.spans.iter().map(|span| span.content.len()).sum()
}

enum PendingHistoryLine {
    Ready(Line<'static>),
    Waiting,
    Exhausted,
}

fn take_next_pending_history_line(pending: &mut VecDeque<PendingHistory>) -> PendingHistoryLine {
    loop {
        let Some(front) = pending.front_mut() else {
            return PendingHistoryLine::Exhausted;
        };
        let next = match front {
            PendingHistory::Lines(lines) => lines.pop_front().map(PendingHistoryLine::Ready),
            PendingHistory::Assistant(queued) => {
                if queued.layout_preparing.load(Ordering::Acquire) {
                    return PendingHistoryLine::Waiting;
                }
                if queued.ready.is_empty() && !queued.rendered_complete {
                    let assistant = queued
                        .cell
                        .as_any_ref()
                        .downcast_ref::<AssistantCell>()
                        .expect("only final assistant cells enter the lazy history queue");
                    let (lines, next_line, complete) = assistant.scrollback_lines_chunk(
                        queued.width,
                        queued.next_line,
                        MAX_LAZY_ASSISTANT_LINES_PER_BATCH,
                    );
                    queued.ready.extend(sanitize_lines_for_terminal(lines));
                    queued.next_line = next_line;
                    queued.rendered_complete = complete;
                }
                queued
                    .ready
                    .pop_front()
                    .map(PendingHistoryLine::Ready)
                    .or_else(|| {
                        if queued.rendered_complete && queued.pending_separators > 0 {
                            queued.pending_separators -= 1;
                            Some(PendingHistoryLine::Ready(Line::default()))
                        } else {
                            None
                        }
                    })
            }
        };
        if let Some(next) = next {
            return next;
        }
        pending.pop_front();
    }
}

fn return_pending_history_line(pending: &mut VecDeque<PendingHistory>, line: Line<'static>) {
    match pending.front_mut() {
        Some(PendingHistory::Lines(lines)) => lines.push_front(line),
        Some(PendingHistory::Assistant(queued)) => queued.ready.push_front(line),
        None => unreachable!("a consumed pending history line keeps its queue entry"),
    }
}

fn take_pending_history_batch(pending: &mut VecDeque<PendingHistory>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut chars = 0usize;
    while lines.len() < MAX_HISTORY_LINES_PER_DRAW {
        let next = match take_next_pending_history_line(pending) {
            PendingHistoryLine::Ready(line) => line,
            PendingHistoryLine::Waiting | PendingHistoryLine::Exhausted => break,
        };
        let next_chars = pending_line_chars(&next);
        if !lines.is_empty() && chars.saturating_add(next_chars) > MAX_HISTORY_CHARS_PER_DRAW {
            return_pending_history_line(pending, next);
            break;
        }
        chars = chars.saturating_add(next_chars);
        lines.push(next);
    }
    lines
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Interactive rendering is deliberately time-sliced, but terminal
        // shutdown is a terminal boundary: flush the remaining canonical
        // scrollback rather than silently discarding a reply tail when the
        // user exits immediately after it starts painting.
        while !self.pending_history.is_empty() {
            let lines = take_pending_history_batch(&mut self.pending_history);
            if super::insert_history::insert_history_lines_with_terminal(
                &mut self.terminal,
                &lines,
                self.is_zellij,
            )
            .is_err()
            {
                break;
            }
        }
        astra_tools::display_sixel::set_tui_active(false);
        let area = self.terminal.viewport_area;
        let _ = execute!(stdout(), cursor::MoveTo(0, area.bottom()), cursor::Show);
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), DisableBracketedPaste);
        let _ = println!();
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Arc};

    use ratatui::text::Line;

    use super::{
        MAX_HISTORY_CHARS_PER_DRAW, MAX_HISTORY_LINES_PER_DRAW, PendingHistory,
        QueuedAssistantHistory, take_history_batch, take_pending_history_batch,
    };
    use crate::tui::history_cell::{HistoryCell, assistant::AssistantCell};

    #[test]
    fn history_batch_is_bounded_and_preserves_fifo_order() {
        let mut pending = (0..MAX_HISTORY_LINES_PER_DRAW + 2)
            .map(|index| Line::raw(format!("line-{index}")))
            .collect::<VecDeque<_>>();

        let first = take_history_batch(&mut pending);
        assert_eq!(first.len(), MAX_HISTORY_LINES_PER_DRAW);
        assert_eq!(first[0].spans[0].content, "line-0");
        assert_eq!(
            first.last().expect("bounded batch has content").spans[0].content,
            format!("line-{}", MAX_HISTORY_LINES_PER_DRAW - 1)
        );
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending.front().expect("tail remains").spans[0].content,
            format!("line-{MAX_HISTORY_LINES_PER_DRAW}")
        );
    }

    #[test]
    fn history_batch_never_starves_one_oversized_line() {
        let mut pending = VecDeque::from([Line::raw("x".repeat(32 * 1024)), Line::raw("tail")]);

        let first = take_history_batch(&mut pending);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].spans[0].content.len(), 32 * 1024);
        assert_eq!(
            pending.front().expect("tail remains").spans[0].content,
            "tail"
        );
    }

    #[test]
    fn history_batch_stops_at_the_character_budget_between_lines() {
        let line_size = MAX_HISTORY_CHARS_PER_DRAW / 2 + 1;
        let mut pending = VecDeque::from([
            Line::raw("x".repeat(line_size)),
            Line::raw("y".repeat(line_size)),
        ]);

        let first = take_history_batch(&mut pending);
        assert_eq!(first.len(), 1);
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending.front().expect("second line remains").spans[0]
                .content
                .len(),
            line_size
        );
    }

    #[test]
    fn final_assistant_history_is_materialized_lazily_in_bounded_batches() {
        let cell: Arc<dyn HistoryCell> = Arc::new(AssistantCell::from_markdown(
            (0..80)
                .map(|index| format!("paragraph {index}"))
                .collect::<Vec<_>>()
                .join("\n\n"),
        ));
        let mut pending = VecDeque::from([PendingHistory::Assistant(QueuedAssistantHistory {
            cell,
            width: 80,
            next_line: 0,
            layout_preparing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            rendered_complete: false,
            pending_separators: 0,
            ready: VecDeque::new(),
        })]);

        let first = take_pending_history_batch(&mut pending);
        assert_eq!(first.len(), MAX_HISTORY_LINES_PER_DRAW);
        assert!(!pending.is_empty(), "long reply tail remains queued");
        assert!(first.iter().all(|line| line.spans[0].content == "█ "));
    }
}
