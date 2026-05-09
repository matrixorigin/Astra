//! Append-only JSONL store for committed [`TurnEvent`]s.
//!
//! One line per event. Path:
//! `~/.astra/transcripts/<session_id>.jsonl`. Sibling of the agent
//! session journal (`~/.astra/sessions/<id>.jsonl`), deliberately
//! kept separate because they store different things (§3.4 of
//! `docs/design/tui-refactor.md`).
//!
//! ## Failure policy
//!
//! Every write is best-effort: an I/O error logs a warning via
//! `astra_core::agent_warn!` and returns, rather than propagating
//! up into the TUI event loop. Losing a transcript line is mildly
//! annoying; crashing the chat session is not.
//!
//! Reads return `Vec<TurnEvent>` and silently skip malformed lines
//! (with a warning each). This keeps resume robust against
//! partially-written trailing records from a crash mid-flush —
//! we'd rather show N-1 turns than refuse to open the session.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use super::turn_event::TurnEvent;

/// Resolve the on-disk path for a given session's transcript.
/// Returns `None` when `$HOME` is unresolvable — the caller should
/// treat it as "persistence disabled" and keep running.
pub(crate) fn transcript_path(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    dirs::home_dir().map(|h| {
        h.join(".astra")
            .join("transcripts")
            .join(format!("{session_id}.jsonl"))
    })
}

/// Append a single event to the session's transcript. Idempotent on
/// an existing file (opens in append mode), best-effort on errors.
///
/// The newline character is part of the record, not a terminator —
/// i.e. we always write `{json}\n`, never `\n{json}`. That matches
/// the standard JSONL convention and keeps `tail -f` behaviour
/// intuitive.
pub(crate) fn append(session_id: &str, event: &TurnEvent) {
    let Some(path) = transcript_path(session_id) else {
        return;
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            astra_core::agent_warn!("tui.transcript", "create_dir_all({parent:?}) failed: {e}");
            return;
        }
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        astra_core::agent_warn!("tui.transcript", "open-for-append failed: {path:?}");
        return;
    };
    let serialized = match serde_json::to_string(event) {
        Ok(s) => s,
        Err(e) => {
            astra_core::agent_warn!("tui.transcript", "serialize {event:?} failed: {e}");
            return;
        }
    };
    if let Err(e) = writeln!(f, "{serialized}") {
        astra_core::agent_warn!("tui.transcript", "write failed: {e}");
    }
}

/// Read all events for a session in append order. Malformed lines
/// are skipped with a warning (see module doc); missing files
/// return an empty vec (first-run case).
pub(crate) fn load(session_id: &str) -> Vec<TurnEvent> {
    let Some(path) = transcript_path(session_id) else {
        return Vec::new();
    };
    let Ok(file) = std::fs::File::open(&path) else {
        // NOT found is the common case for a new session.
        return Vec::new();
    };
    let mut out = Vec::new();
    for (lineno, line) in BufReader::new(file).lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                astra_core::agent_warn!(
                    "tui.transcript",
                    "read error in {path:?} at line {lineno}: {e}"
                );
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<TurnEvent>(&line) {
            Ok(ev) => out.push(ev),
            Err(e) => {
                astra_core::agent_warn!(
                    "tui.transcript",
                    "skipping malformed line {lineno} in {path:?}: {e}"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::turn_event::{SystemLevel, ToolStatus, TurnEvent};

    /// Run `f` with `$HOME` pointing at a fresh tempdir so
    /// `dirs::home_dir()` resolves there. Ensures tests don't scribble
    /// into the developer's real `~/.astra/`.
    fn with_tmp_home<F: FnOnce(&std::path::Path)>(f: F) {
        use std::env;
        let tmp = tempfile::tempdir().expect("tempdir");
        let prev = env::var("HOME").ok();
        // SAFETY: tests in this module are `#[serial]` so no other
        // thread is reading/writing HOME concurrently.
        unsafe {
            env::set_var("HOME", tmp.path());
        }
        f(tmp.path());
        match prev {
            Some(v) => unsafe { env::set_var("HOME", v) },
            None => unsafe { env::remove_var("HOME") },
        }
    }

    #[test]
    #[serial_test::serial]
    fn empty_session_id_returns_none_path() {
        assert!(transcript_path("").is_none());
    }

    #[test]
    #[serial_test::serial]
    fn append_and_load_roundtrip_preserves_order() {
        with_tmp_home(|_tmp| {
            let sid = "sess_order";
            let events = vec![
                TurnEvent::User {
                    ts: None,
                    text: "hi".into(),
                },
                TurnEvent::Assistant {
                    ts: None,
                    markdown: "hello".into(),
                },
                TurnEvent::Tool {
                    ts: None,
                    name: "bash".into(),
                    description: "ls".into(),
                    status: ToolStatus::Success,
                    duration_ms: 42,
                    output_summary: Some("3 entries".into()),
                    output: None,
                },
                TurnEvent::TurnSummary {
                    ts: None,
                    elapsed_ms: Some(1500),
                    ttft_ms: Some(300),
                    tokens_in: Some(500),
                    tokens_out: Some(200),
                    tools: 1,
                    cumulative_tokens: Some(700),
                    cumulative_cost_usd: Some(0.0012),
                },
            ];
            for e in &events {
                append(sid, e);
            }
            let back = load(sid);
            assert_eq!(back, events, "order + content must survive round-trip");
        });
    }

    #[test]
    #[serial_test::serial]
    fn load_missing_session_is_empty_not_error() {
        with_tmp_home(|_tmp| {
            assert!(load("does_not_exist").is_empty());
        });
    }

    #[test]
    #[serial_test::serial]
    fn malformed_line_is_skipped_good_lines_survive() {
        // Simulates a partial crash: a valid line, a half-written
        // trailing line, then a recovered run appending a new valid
        // line. Reader must return the two good ones, drop the
        // broken one.
        with_tmp_home(|tmp| {
            let sid = "sess_malformed";
            append(
                sid,
                &TurnEvent::User {
                    ts: None,
                    text: "first".into(),
                },
            );
            // Poke a corrupt line directly onto the file.
            let path = tmp
                .join(".astra")
                .join("transcripts")
                .join("sess_malformed.jsonl");
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{{not valid json").unwrap();
            drop(f);

            append(
                sid,
                &TurnEvent::System {
                    ts: None,
                    level: SystemLevel::Info,
                    text: "after crash".into(),
                },
            );

            let back = load(sid);
            assert_eq!(back.len(), 2, "malformed middle line should be skipped");
            assert!(matches!(&back[0], TurnEvent::User { text, .. } if text == "first"));
            assert!(matches!(&back[1], TurnEvent::System { text, .. } if text == "after crash"));
        });
    }

    #[test]
    #[serial_test::serial]
    fn empty_session_id_does_not_touch_filesystem() {
        with_tmp_home(|tmp| {
            append(
                "",
                &TurnEvent::User {
                    ts: None,
                    text: "ignored".into(),
                },
            );
            // The transcripts directory should not even exist.
            let dir = tmp.join(".astra").join("transcripts");
            assert!(
                !dir.exists(),
                "empty sid must be a total no-op; dir was created at {dir:?}"
            );
            assert!(load("").is_empty());
        });
    }

    #[test]
    #[serial_test::serial]
    fn blank_lines_in_file_are_ignored() {
        // Guards against a file that was touched by a tool that
        // added stray newlines. We don't care about them; only
        // structured lines count.
        with_tmp_home(|tmp| {
            let sid = "sess_blanks";
            let path = tmp
                .join(".astra")
                .join("transcripts")
                .join("sess_blanks.jsonl");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let payload = r#"
{"kind":"user","text":"one"}

{"kind":"user","text":"two"}
"#;
            std::fs::write(&path, payload).unwrap();
            let back = load(sid);
            assert_eq!(back.len(), 2);
        });
    }
}
