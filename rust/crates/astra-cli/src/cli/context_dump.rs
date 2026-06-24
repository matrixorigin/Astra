//! `/context dump` + `astra context dump` — persist a full JSON
//! snapshot of everything the `/context` panel can see.
//!
//! Two entry points share one writer:
//! - [`write_dump_for_repl`] pulls from the live `SessionState` +
//!   `ChatWidget` the TUI owns.  Used by the in-session
//!   `/context dump [path]` slash command.
//! - [`write_dump_from_journal`] rebuilds a dump from a persisted
//!   session JSONL journal on disk.  Used by the standalone
//!   `astra context dump --session <id>` CLI for forensic replay
//!   after the TUI has exited.
//!
//! Output format (Serde JSON, pretty-printed):
//!
//! ```json
//! {
//!   "schema": "astra.context_dump/v1",
//!   "captured_at": "2026-05-20T11:13:00Z",
//!   "session_id": "abc…",
//!   "turn": 5,
//!   "model": "<model-id>",
//!   "cwd": "~/github/astra",
//!   "git_branch": "improve_tui3",
//!   "trace": { … full ContextAssemblyTrace … },
//!   "chat_history": [ {"role": "user", "text": "…"}, … ],
//!   "totals": { "cost_usd": 0.12, "prompt_tokens": 12000, … }
//! }
//! ```
//!
//! The `trace` field serialises every byte the runtime recorded
//! (token budget, system-prompt breakdown, tool-surface, memory,
//! and decision traces).  `chat_history` captures
//! the same user/assistant/reasoning text the TUI shows, so the
//! dump reproduces what the model "saw" this turn.  This is the
//! truth file — paste it into a bug report and the maintainer
//! can replay the exact context you had.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use astra_services::session_journal;
use serde::Serialize;

use crate::cli::session::session_state::SessionState;

const SCHEMA_VERSION: &str = "astra.context_dump/v1";

/// Wire format for the dump.  All fields use plain owned types so
/// the file is round-trippable by other tooling (no Rc / Arc).
#[derive(Debug, Clone, Serialize)]
pub struct ContextDump {
    pub schema: String,
    pub captured_at_unix_millis: u128,
    pub session_id: Option<String>,
    pub turn: u32,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub persistence_error: Option<String>,
    /// Only `Some` when the caller could reach the latest trace
    /// (either via the live ObservabilitySession or a persisted
    /// journal).  Missing traces land here as `None` so the dump
    /// still records the chat history.
    pub trace: Option<serde_json::Value>,
    pub chat_history: Vec<ChatTurnDump>,
    pub totals: Totals,
    pub active_skills: Vec<ActiveSkillDump>,
    pub compressed_turns: Vec<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatTurnDump {
    /// `user`, `assistant`, `reasoning`, or `system`.
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub cost_usd: f64,
    pub max_budget_usd: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActiveSkillDump {
    pub name: String,
    pub description: String,
}

/// Write a dump built from live REPL state + pre-collected chat
/// history.  `arg` is the optional filesystem path the user typed
/// after `/context dump` — when `None`, we synthesize a default
/// path under `~/.astra/context-dumps/`.  Callers hand in the
/// chat history separately because `ChatWidget` lives in a
/// private module that `cli::` can't import.
pub(crate) fn write_dump_for_repl(
    state: &SessionState,
    chat_history: Vec<ChatTurnDump>,
    arg: Option<&str>,
) -> Result<PathBuf, String> {
    let dump = build_dump_from_repl(state, chat_history);
    let path = resolve_dump_path(arg, state.session_id.as_deref(), state.turn)?;
    write_json(&path, &dump)?;
    Ok(path)
}

fn build_dump_from_repl(state: &SessionState, chat_history: Vec<ChatTurnDump>) -> ContextDump {
    let trace_json = state.observability_session.as_ref().and_then(|session| {
        let guard = astra_core::sync_poison::recover_rwlock_read(&session);
        guard
            .context_traces
            .last()
            .map(|t| serde_json::to_value(t).unwrap_or(serde_json::Value::Null))
    });
    let compressed_turns = state
        .observability_session
        .as_ref()
        .map(|session| {
            let guard = astra_core::sync_poison::recover_rwlock_read(&session);
            guard.compressed_turns.clone()
        })
        .unwrap_or_default();

    ContextDump {
        schema: SCHEMA_VERSION.to_string(),
        captured_at_unix_millis: now_millis(),
        session_id: state.session_id.clone(),
        turn: state.turn,
        model: state.model.clone(),
        cwd: std::env::current_dir().ok().map(display_path),
        git_branch: detect_git_branch(),
        persistence_error: state.session_persistence_error.clone(),
        trace: trace_json,
        chat_history,
        totals: Totals {
            cost_usd: state.total_session_cost,
            max_budget_usd: state.max_budget_limit,
            prompt_tokens: state.total_prompt_tokens,
            completion_tokens: state.total_completion_tokens,
            cache_read_tokens: state.total_cache_read_tokens,
            cache_creation_tokens: state.total_cache_creation_tokens,
        },
        active_skills: state
            .active_system_skills
            .iter()
            .map(|s| ActiveSkillDump {
                name: s.name.clone(),
                description: s.description.clone(),
            })
            .collect(),
        compressed_turns,
    }
}

/// Resolve a user-supplied session id to a full UUID, with two
/// conveniences:
///   1. Prefix match — any unique prefix (e.g. first 8 chars) of
///      an on-disk session resolves to the full id. Ambiguous
///      prefixes error out with the candidate list so the user
///      can pick.
///   2. Default-latest — when `arg` is `None`, returns the most
///      recently modified owner-bound journal. Makes `astra context
///      dump` a zero-arg operation for bug reports.
pub fn resolve_session_id(arg: Option<&str>) -> Result<String, String> {
    let sessions_dir = session_journal::local_owner_sessions_dir();
    let entries = session_journal::list_sessions_by_time(usize::MAX)
        .map_err(|error| format!("read {}: {error}", sessions_dir.display()))?;
    if entries.is_empty() {
        return Err(format!(
            "no sessions found in {} — is ASTRA running?",
            sessions_dir.display()
        ));
    }
    match arg {
        None => {
            // Pick the most recently modified. `list_sessions_by_time`
            // returns entries sorted newest-first by mtime.
            Ok(entries[0].clone())
        }
        Some(raw) => {
            let needle = raw.trim();
            if needle.is_empty() {
                return Err("empty --session value".to_string());
            }
            // Full match wins first — a user who types the full
            // UUID should never get a "multiple matches" error.
            if entries.iter().any(|id| id == needle) {
                return Ok(needle.to_string());
            }
            let matches: Vec<&String> =
                entries.iter().filter(|id| id.starts_with(needle)).collect();
            match matches.len() {
                0 => Err(format!(
                    "no session matches prefix `{needle}` in {}",
                    sessions_dir.display()
                )),
                1 => Ok(matches[0].clone()),
                _ => {
                    // Ambiguous — list the first few so the user
                    // can copy-paste one.
                    let sample: Vec<&String> = matches.iter().take(5).copied().collect();
                    let mut msg = format!("prefix `{needle}` matches {} sessions:", matches.len());
                    for id in &sample {
                        msg.push_str(&format!("\n  • {id}"));
                    }
                    if matches.len() > sample.len() {
                        msg.push_str(&format!("\n  … and {} more", matches.len() - sample.len()));
                    }
                    Err(msg)
                }
            }
        }
    }
}

/// Plain-text summary of a session's latest context state.  Printed
/// to stdout by `astra context dump --summary`.  Intentionally
/// skips the full trace JSON and the chat bodies — the goal is
/// "what does /context's collapsed view look like?" readable in a
/// terminal.
pub fn print_summary(session_id: &str) -> Result<(), String> {
    let dump = build_dump_from_journal(session_id)?;
    println!(
        "Session {}  ·  turn {}",
        dump.session_id.as_deref().unwrap_or("?"),
        dump.turn
    );
    if let Some(m) = &dump.model {
        println!("  model: {m}");
    }
    if let Some(cwd) = &dump.cwd {
        println!("  cwd: {cwd}");
    }
    if let Some(git_branch) = &dump.git_branch {
        println!("  git: {git_branch}");
    }
    if let Some(error) = &dump.persistence_error {
        println!("  persistence: degraded · {error}");
    }
    println!(
        "  tokens: in {} · out {} · cache-read {} · cache-create {}",
        fmt_tokens_u64(dump.totals.prompt_tokens),
        fmt_tokens_u64(dump.totals.completion_tokens),
        fmt_tokens_u64(dump.totals.cache_read_tokens),
        fmt_tokens_u64(dump.totals.cache_creation_tokens),
    );
    println!("  chat turns: {} recorded", dump.chat_history.len());
    if !dump.compressed_turns.is_empty() {
        let rendered: Vec<String> = dump
            .compressed_turns
            .iter()
            .map(|t| t.to_string())
            .collect();
        println!("  compaction fired on turns: {}", rendered.join(", "));
    }
    if dump.trace.is_none() {
        println!("  trace: (none captured in journal)");
    }
    Ok(())
}

fn fmt_tokens_u64(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Rebuild a dump from a persisted session journal.  Used by the
/// standalone `astra context dump --session <id>` CLI.  Returns
/// an error when the journal can't be found or parsed.
pub fn write_dump_from_journal(session_id: &str, arg: Option<&str>) -> Result<PathBuf, String> {
    let dump = build_dump_from_journal(session_id)?;
    let path = resolve_dump_path(arg, Some(session_id), dump.turn)?;
    write_json(&path, &dump)?;
    Ok(path)
}

fn build_dump_from_journal(session_id: &str) -> Result<ContextDump, String> {
    use astra_services::session_journal::JournalEventType;
    let events = astra_services::session_journal::read_journal(session_id)
        .map_err(|e| format!("read journal: {e}"))?;
    if events.is_empty() {
        return Err(format!("session {session_id} has no journal events"));
    }

    let mut chat = Vec::<ChatTurnDump>::new();
    let mut last_turn: u32 = 0;
    let mut trace_json: Option<serde_json::Value> = None;
    let mut model: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut git_branch: Option<String> = None;
    let mut persistence_error: Option<String> = None;
    let mut compressed_turns: Vec<u32> = Vec::new();
    let mut prompt_tokens: u64 = 0;
    let mut completion_tokens: u64 = 0;
    let mut cache_read_tokens: u64 = 0;
    let mut cache_creation_tokens: u64 = 0;
    // Journal doesn't track per-event USD cost today; forensic
    // dumps report 0 here. Live dumps (the `/context dump` slash)
    // still carry the accumulated cost from SessionState.
    let cost_usd: f64 = 0.0;

    match astra_services::session_workspace::read_workspace_optional(session_id) {
        Ok(Some(workspace)) => {
            model = model.or(workspace.model.clone());
            cwd = Some(workspace.cwd);
            git_branch = workspace.git_branch;
            persistence_error = workspace
                .last_persistence_error
                .as_deref()
                .map(str::trim)
                .filter(|error| !error.is_empty())
                .map(str::to_string);
        }
        Ok(None) => {}
        Err(error) => {
            persistence_error = Some(format!("workspace metadata unreadable: {error}"));
        }
    }

    for ev in &events {
        if let Some(t) = ev.turn {
            last_turn = last_turn.max(t);
        }
        if let Some(m) = &ev.model {
            model = Some(m.clone());
        }
        if let Some(raw) = ev.tokens_in {
            prompt_tokens = prompt_tokens.saturating_add(raw);
        }
        if let Some(raw) = ev.tokens_out {
            completion_tokens = completion_tokens.saturating_add(raw);
        }
        if let Some(raw) = ev.cache_read_tokens {
            cache_read_tokens = cache_read_tokens.saturating_add(raw);
        }
        if let Some(raw) = ev.cache_creation_tokens {
            cache_creation_tokens = cache_creation_tokens.saturating_add(raw);
        }
        match ev.event_type {
            JournalEventType::Turn => {
                if let Some(user_text) = &ev.user_input {
                    chat.push(ChatTurnDump {
                        role: "user".into(),
                        text: user_text.clone(),
                    });
                }
                if let Some(assistant_text) = &ev.assistant_output {
                    chat.push(ChatTurnDump {
                        role: "assistant".into(),
                        text: assistant_text.clone(),
                    });
                }
            }
            JournalEventType::Compact => {
                if let Some(t) = ev.turn
                    && !compressed_turns.contains(&t)
                {
                    compressed_turns.push(t);
                }
            }
            JournalEventType::ContextAssemblyRecorded => {
                // Serialise the whole event so all the trace fields
                // come along for the ride — serde round-trips cleanly.
                if let Ok(v) = serde_json::to_value(ev) {
                    trace_json = Some(v);
                }
            }
            _ => {}
        }
    }

    Ok(ContextDump {
        schema: SCHEMA_VERSION.to_string(),
        captured_at_unix_millis: now_millis(),
        session_id: Some(session_id.to_string()),
        turn: last_turn,
        model,
        cwd,
        git_branch,
        persistence_error,
        trace: trace_json,
        chat_history: chat,
        totals: Totals {
            cost_usd,
            max_budget_usd: 0.0,
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        },
        active_skills: Vec::new(),
        compressed_turns,
    })
}

// ─── Path resolution + I/O ────────────────────────────────────────

fn resolve_dump_path(
    arg: Option<&str>,
    session_id: Option<&str>,
    turn: u32,
) -> Result<PathBuf, String> {
    if let Some(arg) = arg
        && !arg.is_empty()
    {
        // User-supplied path. Expand `~` prefix for convenience.
        return Ok(expand_tilde(arg));
    }
    let home = std::env::var("HOME").map_err(|_| "HOME env var is unset".to_string())?;
    let dir = Path::new(&home).join(".astra").join("context-dumps");
    fs::create_dir_all(&dir).map_err(|e| format!("create dump dir: {e}"))?;
    let sid = session_id
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "nosess".to_string());
    let ts = now_millis();
    let filename = format!("{sid}-t{turn}-{ts}.json");
    Ok(dir.join(filename))
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

fn write_json(path: &Path, dump: &ContextDump) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| format!("create parent dir: {e}"))?;
    }
    let body = serde_json::to_string_pretty(dump).map_err(|e| format!("serialize: {e}"))?;
    fs::write(path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

fn display_path(path: PathBuf) -> String {
    let s = path.display().to_string();
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    s
}

fn detect_git_branch() -> Option<String> {
    crate::git_branch_cache::detect_git_branch_cached()
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{
        ChatTurnDump, ContextDump, SCHEMA_VERSION, Totals, build_dump_from_journal, expand_tilde,
        resolve_dump_path, resolve_session_id, write_json,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn resolve_path_uses_arg_when_given() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("snap.json");
        let p = resolve_dump_path(Some(explicit.to_str().unwrap()), Some("sess1234"), 3).unwrap();
        assert_eq!(p, explicit);
    }

    #[serial_test::serial]
    #[test]
    fn resolve_path_expands_tilde() {
        let _g = crate::tests::HomeGuard::set("/tmp/fake-home");
        let p = resolve_dump_path(Some("~/snap.json"), Some("sess"), 0).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/fake-home/snap.json"));
    }

    #[serial_test::serial]
    #[test]
    fn resolve_path_synthesizes_under_home_dir_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let _g = crate::tests::HomeGuard::set(tmp.path());
        let p = resolve_dump_path(None, Some("abcdef12-full"), 7).unwrap();
        let parent = p.parent().unwrap();
        assert!(parent.ends_with(".astra/context-dumps"));
        let filename = p.file_name().unwrap().to_string_lossy();
        // Short sid + turn in filename so dumps sort sensibly.
        assert!(filename.starts_with("abcdef12-t7-"));
        assert!(filename.ends_with(".json"));
    }

    #[test]
    fn write_json_produces_valid_serde_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.json");
        let dump = ContextDump {
            schema: SCHEMA_VERSION.to_string(),
            captured_at_unix_millis: 12345,
            session_id: Some("s".into()),
            turn: 3,
            model: Some("m".into()),
            cwd: None,
            git_branch: None,
            persistence_error: None,
            trace: None,
            chat_history: vec![ChatTurnDump {
                role: "user".into(),
                text: "hi".into(),
            }],
            totals: Totals {
                cost_usd: 0.0,
                max_budget_usd: 0.0,
                prompt_tokens: 0,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            active_skills: Vec::new(),
            compressed_turns: Vec::new(),
        };
        write_json(&path, &dump).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["schema"], SCHEMA_VERSION);
        assert_eq!(value["turn"], 3);
        assert_eq!(value["chat_history"][0]["role"], "user");
    }

    #[test]
    fn write_json_creates_parent_directories_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("a/b/c/out.json");
        let dump = ContextDump {
            schema: SCHEMA_VERSION.to_string(),
            captured_at_unix_millis: 0,
            session_id: None,
            turn: 0,
            model: None,
            cwd: None,
            git_branch: None,
            persistence_error: None,
            trace: None,
            chat_history: Vec::new(),
            totals: Totals {
                cost_usd: 0.0,
                max_budget_usd: 0.0,
                prompt_tokens: 0,
                completion_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
            active_skills: Vec::new(),
            compressed_turns: Vec::new(),
        };
        assert!(write_json(&nested, &dump).is_ok());
        assert!(nested.exists());
    }

    #[serial_test::serial]
    #[test]
    fn expand_tilde_only_matches_leading_slash_prefix() {
        let _g = crate::tests::HomeGuard::set("/tmp/home");
        // `~foo` (no slash) isn't a tilde expansion — leave alone.
        assert_eq!(expand_tilde("~foo"), PathBuf::from("~foo"));
        assert_eq!(expand_tilde("~/foo"), PathBuf::from("/tmp/home/foo"));
    }

    #[serial_test::serial]
    #[test]
    fn build_dump_from_journal_rejects_unknown_session() {
        let err = build_dump_from_journal("nonexistent-session-xyz").unwrap_err();
        assert!(
            err.contains("no journal events") || err.contains("read journal"),
            "expected journal error, got {err}"
        );
    }

    // ─── Session resolver ────────────────────────────────────────

    /// Seed fake owner-bound session journals under a tempdir-backed HOME and
    /// return the guarded `HomeGuard` alongside the tempdir handle (so the
    /// tempdir outlives the test).
    ///
    /// `ids` are written IN ORDER. On filesystems with per-file
    /// mtime granularity (nsec on Linux) this gives ascending
    /// mtimes; the last one written is the "newest". That's
    /// enough to cover the default-latest resolver path.
    fn seed_sessions_tmp(ids: &[&str]) -> (crate::tests::HomeGuard, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let g = crate::tests::HomeGuard::set(tmp.path());
        for (i, id) in ids.iter().enumerate() {
            let path = astra_services::session_journal::journal_file_path(id);
            fs::create_dir_all(path.parent().expect("journal parent")).unwrap();
            fs::write(&path, "{}\n").unwrap();
            // Some filesystems coalesce mtimes written in the same
            // tick. A 10ms nap between writes keeps ordering
            // deterministic across Linux/macOS without pulling in
            // an mtime-setting crate.
            if i + 1 < ids.len() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        (g, tmp)
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_returns_most_recent_when_arg_none() {
        let (_g, _tmp) = seed_sessions_tmp(&[
            "01010101-aaaa-bbbb-cccc-dddddddddddd",
            "03030303-aaaa-bbbb-cccc-dddddddddddd",
            // Written last → highest mtime → resolver picks this.
            "02020202-aaaa-bbbb-cccc-dddddddddddd",
        ]);
        let id = resolve_session_id(None).unwrap();
        assert_eq!(id, "02020202-aaaa-bbbb-cccc-dddddddddddd");
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_matches_unique_prefix() {
        let (_g, _tmp) = seed_sessions_tmp(&[
            "01010101-aaaa-bbbb-cccc-dddddddddddd",
            "02020202-aaaa-bbbb-cccc-dddddddddddd",
        ]);
        let id = resolve_session_id(Some("0101")).unwrap();
        assert_eq!(id, "01010101-aaaa-bbbb-cccc-dddddddddddd");
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_errors_on_ambiguous_prefix() {
        let (_g, _tmp) = seed_sessions_tmp(&[
            "ab111111-aaaa-bbbb-cccc-dddddddddddd",
            "ab222222-aaaa-bbbb-cccc-dddddddddddd",
            "ab333333-aaaa-bbbb-cccc-dddddddddddd",
        ]);
        let err = resolve_session_id(Some("ab")).unwrap_err();
        assert!(
            err.contains("matches 3 sessions"),
            "expected ambiguity message, got {err}"
        );
        assert!(err.contains("ab111111"));
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_errors_on_unknown_prefix() {
        let (_g, _tmp) = seed_sessions_tmp(&["01010101-aaaa-bbbb-cccc-dddddddddddd"]);
        let err = resolve_session_id(Some("zzzzzz")).unwrap_err();
        assert!(err.contains("no session matches prefix"));
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_errors_when_sessions_dir_is_empty() {
        let (_g, _tmp) = seed_sessions_tmp(&[]);
        let err = resolve_session_id(None).unwrap_err();
        assert!(err.contains("no sessions found"));
    }

    #[serial_test::serial]
    #[test]
    fn resolve_session_accepts_full_uuid_even_if_overlapping_prefixes_exist() {
        // If a full UUID is typed it should never trigger the
        // ambiguous-prefix guard — full matches win.
        let full = "01010101-aaaa-bbbb-cccc-dddddddddddd";
        let (_g, _tmp) = seed_sessions_tmp(&[full, "01010101-extra-overlap-prefix"]);
        let id = resolve_session_id(Some(full)).unwrap();
        assert_eq!(id, full);
    }

    #[test]
    fn build_dump_from_journal_surfaces_workspace_persistence_and_metadata() {
        use astra_services::session_journal::{
            JournalDirGuard, JournalEvent, JournalEventType, JournalWriter,
        };
        use astra_services::session_workspace::WorkspaceMetadata;

        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "context-dump-persistence";

        let mut workspace =
            WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        workspace.last_persistence_error = Some("failed to append turn event".to_string());
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let writer = JournalWriter::new(session_id).unwrap();
        writer
            .append(&JournalEvent {
                event_type: JournalEventType::Turn,
                ts: "2026-01-15T10:31:00Z".to_string(),
                session_id: Some(session_id.to_string()),
                turn: Some(3),
                agentic_step: None,
                model: Some("gpt-5.4".to_string()),
                user_input: Some("hello".to_string()),
                assistant_output: Some("world".to_string()),
                tool_count: Some(0),
                tokens_in: Some(12),
                tokens_out: Some(7),
                duration_ms: Some(50),
                error: None,
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                visible_tools: None,
                selected_skills: None,
                tools_used: None,
                tool_calls: None,
                budget_used: None,
                budget_pressure: None,
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                edge_policy: None,
                context_assembly_trace: None,
                routing_domain_hint: None,
                entity_learn_skipped_no_domain: false,
                round: None,
                tool_calls_returned: None,
                offset_ms: None,
                llm_rounds: None,
                total_llm_ms: None,
                total_tool_ms: None,
                parent_event_id: None,
                git_head: None,
                git_branch: None,
            })
            .unwrap();

        let dump = build_dump_from_journal(session_id).unwrap();

        assert_eq!(dump.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(dump.cwd.as_deref(), Some("/repo"));
        assert_eq!(dump.git_branch.as_deref(), Some("main"));
        assert_eq!(
            dump.persistence_error.as_deref(),
            Some("failed to append turn event")
        );
    }
}
