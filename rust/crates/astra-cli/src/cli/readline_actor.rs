//! Async readline actor — moves the rustyline `Editor` to a dedicated thread
//! so the main async loop can `tokio::select!` between readline and other
//! futures (e.g. background plan updates).
//!
//! # Architecture
//!
//! ```text
//!  Main async task                    Readline thread
//!  ───────────────                    ───────────────
//!  ReadlineActor                      std::thread
//!   ├─ req_tx ──────────────────────► req_rx
//!   │  (ReadlineRequest)              loop { recv → editor.readline() }
//!   └─ resp_rx ◄────────────────────  resp_tx
//!      (ReadlineResponse)             sends result back
//! ```
//!
//! The readline thread runs autonomously; plan updates are flushed
//! between prompts via `eprintln!` (not during active readline).

use std::path::PathBuf;

use rustyline::{Editor, error::ReadlineError, history::FileHistory};

use crate::repl_ui::ReplHelper;

/// Messages sent from the main async loop → readline thread.
enum ReadlineRequest {
    /// Read a line with the given prompt string.
    ReadLine(String),
    /// Add an entry to readline history (called after a successful read).
    AddHistory(String),
    /// Save history to disk and shut down the thread.
    Shutdown(PathBuf),
}

/// Messages sent from the readline thread → main async loop.
pub(super) enum ReadlineResponse {
    /// A line was read (or an error occurred).
    Line {
        result: Result<String, ReadlineError>,
        /// If the slash-command picker selected a command, it's here.
        pending_execute: Option<String>,
    },
}

/// Async handle to a readline thread.
///
/// The `Editor` lives exclusively on the spawned thread. The main async
/// task communicates via channels and can freely `tokio::select!` while
/// waiting for the next line.
pub(super) struct ReadlineActor {
    req_tx: std::sync::mpsc::Sender<ReadlineRequest>,
    resp_rx: tokio::sync::mpsc::UnboundedReceiver<ReadlineResponse>,
}

impl ReadlineActor {
    /// Spawn the readline thread and return the actor handle.
    ///
    /// ExternalPrinter is intentionally NOT created — its mere existence
    /// changes rustyline's internal rendering path, which breaks display of
    /// the last CJK (wide) character due to rustyline issue #826. Plan
    /// updates are flushed between prompts via eprintln! instead.
    pub fn spawn(editor: Editor<ReplHelper, FileHistory>) -> Result<Self, String> {
        // Channels: sync mpsc for requests (main→thread), tokio mpsc for responses (thread→main).
        let (req_tx, req_rx) = std::sync::mpsc::channel::<ReadlineRequest>();
        let (resp_tx, resp_rx) = tokio::sync::mpsc::unbounded_channel::<ReadlineResponse>();

        std::thread::Builder::new()
            .name("readline".into())
            .spawn(move || {
                readline_thread_main(editor, req_rx, resp_tx);
            })
            .map_err(|e| format!("failed to spawn readline thread: {e}"))?;

        Ok(Self { req_tx, resp_rx })
    }

    /// Request the thread to read a line with the given prompt.
    ///
    /// Returns immediately — the result will arrive via [`recv`].
    pub fn request_readline(&self, prompt: String) {
        let _ = self.req_tx.send(ReadlineRequest::ReadLine(prompt));
    }

    /// Wait for the next readline response.
    ///
    /// Returns `None` if the readline thread has exited.
    pub async fn recv(&mut self) -> Option<ReadlineResponse> {
        self.resp_rx.recv().await
    }

    /// Tell the readline thread to add a history entry.
    pub fn add_history(&self, entry: String) {
        let _ = self.req_tx.send(ReadlineRequest::AddHistory(entry));
    }

    /// Tell the readline thread to save history and shut down.
    pub fn shutdown(&self, hist_path: PathBuf) {
        let _ = self.req_tx.send(ReadlineRequest::Shutdown(hist_path));
    }
}

/// The readline thread's main loop.
fn readline_thread_main(
    mut editor: Editor<ReplHelper, FileHistory>,
    req_rx: std::sync::mpsc::Receiver<ReadlineRequest>,
    resp_tx: tokio::sync::mpsc::UnboundedSender<ReadlineResponse>,
) {
    use crate::repl_ui::take_slash_pending_execute;

    while let Ok(req) = req_rx.recv() {
        match req {
            ReadlineRequest::ReadLine(prompt) => {
                let result = editor.readline(&prompt);

                // Capture slash picker result on THIS thread (where the
                // event handler set it via global state).
                let pending = take_slash_pending_execute();

                let _ = resp_tx.send(ReadlineResponse::Line {
                    result,
                    pending_execute: pending,
                });
            }
            ReadlineRequest::AddHistory(entry) => {
                let _ = editor.add_history_entry(entry.as_str());
            }
            ReadlineRequest::Shutdown(path) => {
                let _ = editor.save_history(&path);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readline_request_variants_are_constructible() {
        let _ = ReadlineRequest::ReadLine("❯ ".into());
        let _ = ReadlineRequest::AddHistory("hello".into());
        let _ = ReadlineRequest::Shutdown(PathBuf::from("/tmp/hist"));
    }

    #[test]
    fn readline_response_variants_are_constructible() {
        let _ = ReadlineResponse::Line {
            result: Ok("hello".into()),
            pending_execute: None,
        };
        let _ = ReadlineResponse::Line {
            result: Err(ReadlineError::Interrupted),
            pending_execute: Some("/help".into()),
        };
    }
}
