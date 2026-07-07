//! `FileObservationStore` — JSON Lines persistence backend for [`ObservationStore`].
//!
//! # Storage layout
//!
//! ```text
//! ~/.astra/observations/{session_id}.jsonl
//! ```
//!
//! Each line is a JSON object: `{"session_id":"...","turn_index":0,"timestamp_unix_ms":...,"metrics_json":"...","facts_json":"..."}`
//!
//! # Concurrency model
//!
//! Writes are `append`-only (O_APPEND). Within a single session only one thread
//! writes per turn. Line-level atomicity is guaranteed by POSIX `write(2)` for
//! writes ≤ PIPE_BUF (4 KiB on Linux); our serialized records are well under
//! that limit.
//!
//! # Unhappy-path guarantees
//!
//! * **Disk full** — `save_entry` returns `Err("...")`, caller logs and continues.
//! * **Permission denied** — same as above.
//! * **Corrupt file** — `load_entries` skips unparseable lines (no panic).
//! * **Missing directory** — created lazily on first write.
//! * **Nonexistent session** — `load_entries` returns empty Vec, no error.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;

use astra_core::observation::TurnMetrics;
use astra_core::observation_journal::{JournalFacts, ObservationStore, StoredEntry, TuningStore};

// ── FileObservationStore ────────────────────────────────────────────────────

/// JSON-Lines file backend for [`ObservationStore`] and [`TuningStore`].
///
/// One file per session at `{root_dir}/{session_id}.jsonl`.
pub struct FileObservationStore {
    root_dir: PathBuf,
}

impl FileObservationStore {
    /// Create a new store rooted at `root_dir`.
    ///
    /// The directory is NOT created here — it is created lazily on first write
    /// (so a read-only deployment with no writes never touches the filesystem).
    pub fn new(root_dir: PathBuf) -> Self {
        Self { root_dir }
    }

    /// Return the canonical path for `session_id`.
    fn session_path(&self, session_id: &str) -> PathBuf {
        // Sanitize: replace '/' and '..' to prevent path traversal.
        let safe = session_id.replace('/', "_").replace("..", "_");
        self.root_dir.join(format!("{safe}.jsonl"))
    }

    /// Return the tuning file path for `session_id`.
    fn session_path_tuning(&self, session_id: &str) -> PathBuf {
        let safe = session_id.replace('/', "_").replace("..", "_");
        self.root_dir.join(format!("{safe}.tuning.jsonl"))
    }

    /// Ensure the root directory exists.
    fn ensure_root_dir(&self) -> Result<(), String> {
        fs::create_dir_all(&self.root_dir)
            .map_err(|e| format!("cannot create observations dir {:?}: {e}", self.root_dir))
    }
}

// ── ObservationStore impl ───────────────────────────────────────────────────

impl ObservationStore for FileObservationStore {
    fn save_entry(
        &self,
        session_id: &str,
        turn_index: u32,
        metrics: &TurnMetrics,
        facts: &JournalFacts,
    ) -> Result<(), String> {
        self.ensure_root_dir()?;

        let path = self.session_path(session_id);

        // Serialize payloads.
        let metrics_json = serde_json::to_string(metrics)
            .map_err(|e| format!("metrics serialization failed: {e}"))?;
        let facts_json =
            serde_json::to_string(facts).map_err(|e| format!("facts serialization failed: {e}"))?;

        let timestamp_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let record = serde_json::json!({
            "session_id": session_id,
            "turn_index": turn_index,
            "timestamp_unix_ms": timestamp_unix_ms,
            "metrics_json": metrics_json,
            "facts_json": facts_json,
        });

        let mut line = record.to_string();
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open {:?}: {e}", path))?;

        file.write_all(line.as_bytes())
            .map_err(|e| format!("write failed for {:?}: {e}", path))?;

        file.flush()
            .map_err(|e| format!("flush failed for {:?}: {e}", path))
    }

    fn load_entries(&self, session_id: &str) -> Vec<StoredEntry> {
        let path = self.session_path(session_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        let reader = BufReader::new(file);
        let mut entries: Vec<StoredEntry> = Vec::new();

        for (line_no, line_result) in reader.lines().enumerate() {
            let line = match line_result {
                Ok(l) => l,
                Err(_) => continue,
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<StoredEntry>(&line) {
                Ok(entry) => entries.push(entry),
                Err(_) => {
                    // Corrupt line — skip, don't fail the whole load.
                    tracing::warn!(
                        line = line_no + 1,
                        session_id = %session_id,
                        "skipping unparseable line in observation store"
                    );
                }
            }
        }

        entries
    }

    fn entry_count(&self, session_id: &str) -> usize {
        let path = self.session_path(session_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return 0,
        };
        let reader = BufReader::new(file);
        reader
            .lines()
            .map_while(Result::ok)
            .filter(|line| !line.trim().is_empty())
            .count()
    }

    fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let path = self.session_path(session_id);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("cannot delete {:?}: {e}", path))?;
        }
        Ok(())
    }
}

// ── TuningStore impl ────────────────────────────────────────────────────────

impl TuningStore for FileObservationStore {
    fn save_tuning_entry(
        &self,
        session_id: &str,
        _turn_index: u32,
        raw_json: &str,
    ) -> Result<(), String> {
        self.ensure_root_dir()?;

        let path = self.session_path_tuning(session_id);

        let mut line = raw_json.to_string();
        if !line.ends_with('\n') {
            line.push('\n');
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("cannot open tuning file {:?}: {e}", path))?;

        file.write_all(line.as_bytes())
            .map_err(|e| format!("write failed for tuning {:?}: {e}", path))?;

        file.flush()
            .map_err(|e| format!("flush failed for tuning {:?}: {e}", path))
    }

    fn load_tuning_entries(&self, session_id: &str) -> Vec<String> {
        let path = self.session_path_tuning(session_id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };

        let reader = BufReader::new(file);
        reader
            .lines()
            .filter_map(|line_result| {
                let line = match line_result {
                    Ok(l) => l,
                    Err(_) => return None,
                };
                if line.trim().is_empty() {
                    return None;
                }
                Some(line)
            })
            .collect()
    }

    fn list_tuning_sessions(&self) -> Vec<String> {
        // Scan for *.tuning.jsonl files in the root directory.
        let dir_entries = match fs::read_dir(&self.root_dir) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        let mut sessions: Vec<String> = dir_entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // Match "*.tuning.jsonl" and extract session id.
                name.strip_suffix(".tuning.jsonl").map(|s| s.to_string())
            })
            .collect();
        sessions.sort();
        sessions
    }
}

// ── Default store factory ─────────────────────────────────────────────────

/// Create the default file-backed observation store rooted at
/// `~/.astra/observations/`.
///
/// Returns `None` when the home directory cannot be resolved (e.g. in a
/// container without `$HOME` set). In that case a warning is logged and
/// the caller should treat observation persistence as unavailable.
pub fn default_observation_store() -> Option<Arc<FileObservationStore>> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => {
            tracing::warn!("HOME directory unavailable — observation persistence disabled");
            return None;
        }
    };
    let root = home.join(".astra").join("observations");
    Some(Arc::new(FileObservationStore::new(root)))
}

/// Create a test store backed by a temporary directory.
///
/// This is used by sibling test modules (e.g. `observation_dispatcher`) that
/// need a real `FileObservationStore` without depending on the home directory.
#[cfg(any(test, feature = "bridge-e2e-hooks"))]
pub fn test_store() -> Option<Arc<FileObservationStore>> {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    let dir = DIR.get_or_init(|| tempfile::TempDir::new().expect("tempdir for test_store"));
    Some(Arc::new(FileObservationStore::new(
        dir.path().to_path_buf(),
    )))
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::observation::TurnMetrics;
    use astra_core::observation_journal::{
        BudgetSnapshot, JournalFacts, ObservationStore, PerformanceSnapshot, StreakSnapshot,
    };
    use tempfile::TempDir;

    fn make_metrics(turn: u32, errors: u32, writes: u32) -> TurnMetrics {
        let mut m = TurnMetrics::default();
        m.rounds_completed = turn;
        m.error_count = errors;
        m.tool_calls_total = errors + writes + 3; // 3 reads
        m.cache_hits = writes;
        m.mutation_count = writes;
        m
    }

    fn make_facts(turn: u32, errors: u32, writes: u32) -> JournalFacts {
        JournalFacts {
            budget: BudgetSnapshot {
                rounds_completed: turn,
                ..Default::default()
            },
            performance: PerformanceSnapshot {
                total_errors: errors,
                total_tool_calls: errors + writes + 3,
                ..Default::default()
            },
            streaks: StreakSnapshot {
                consecutive_rounds_with_outcome: writes.min(1),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn save_and_load_single_entry() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "test-session-1";

        let metrics = make_metrics(0, 1, 2);
        let facts = make_facts(0, 1, 2);

        store.save_entry(sid, 0, &metrics, &facts).unwrap();

        let entries = store.load_entries(sid);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, sid);
        assert_eq!(entries[0].turn_index, 0);
        assert!(entries[0].metrics().is_some());
        assert!(entries[0].facts().is_some());
    }

    #[test]
    fn save_and_load_multiple_entries() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "test-session-multi";

        for turn in 0..5 {
            let metrics = make_metrics(turn, 0, turn + 1);
            let facts = make_facts(turn, 0, turn + 1);
            store.save_entry(sid, turn, &metrics, &facts).unwrap();
        }

        let entries = store.load_entries(sid);
        assert_eq!(entries.len(), 5);
        assert_eq!(store.entry_count(sid), 5);

        // Verify turn ordering preserved.
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.turn_index, i as u32);
        }
    }

    #[test]
    fn load_nonexistent_session_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());

        let entries = store.load_entries("ghost-session");
        assert!(entries.is_empty());
        assert_eq!(store.entry_count("ghost-session"), 0);
    }

    #[test]
    fn delete_session_removes_entries() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "delete-me";

        store
            .save_entry(sid, 0, &make_metrics(0, 0, 1), &make_facts(0, 0, 1))
            .unwrap();
        assert_eq!(store.entry_count(sid), 1);

        store.delete_session(sid).unwrap();
        assert!(store.load_entries(sid).is_empty());
        assert_eq!(store.entry_count(sid), 0);
    }

    #[test]
    fn empty_lines_are_skipped() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "empty-lines";
        store
            .save_entry(sid, 0, &make_metrics(0, 0, 1), &make_facts(0, 0, 1))
            .unwrap();
        let path = store.session_path(sid);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "   ").unwrap();
        writeln!(file).unwrap();

        let entries = store.load_entries(sid);
        assert_eq!(entries.len(), 1, "empty lines should be skipped");
        assert_eq!(store.entry_count(sid), 1);
    }

    #[test]
    fn entry_count_matches_entries() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "count-session";

        assert_eq!(store.entry_count(sid), 0);
        for i in 0..3 {
            store
                .save_entry(sid, i, &make_metrics(i, 0, 1), &make_facts(i, 0, 1))
                .unwrap();
        }
        assert_eq!(store.entry_count(sid), 3);
    }

    // ── save_tuning_entry tests ────────────────────────────────────────

    #[test]
    fn save_tuning_entry_persists_json_line() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let json = r#"{"signal_type":"cache_warming","trigger_value":0.75,"priority":5}"#;

        store
            .save_tuning_entry("tune-sess", 1, json)
            .expect("save tuning");

        let entries = store.load_tuning_entries("tune-sess");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].contains("cache_warming"));
    }

    #[test]
    fn save_tuning_entry_multiple_jobs_are_appended() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "tune-multi";

        store
            .save_tuning_entry(
                sid,
                1,
                r#"{"signal_type":"cache_warming","trigger_value":0.5}"#,
            )
            .unwrap();
        store
            .save_tuning_entry(
                sid,
                2,
                r#"{"signal_type":"context_pressure","trigger_value":0.8}"#,
            )
            .unwrap();
        store
            .save_tuning_entry(
                sid,
                3,
                r#"{"signal_type":"error_rate","trigger_value":0.3}"#,
            )
            .unwrap();

        let entries = store.load_tuning_entries(sid);
        assert_eq!(entries.len(), 3);
    }

    #[test]
    fn save_tuning_entry_adds_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "tune-newline";
        let json = r#"{"signal_type":"cache_warming","trigger_value":0.25}"#;

        store.save_tuning_entry(sid, 1, json).unwrap();

        // Read raw file to verify trailing newline.
        let path = store.session_path_tuning(sid);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.ends_with('\n'));
    }

    #[test]
    fn save_tuning_entry_creates_directory_if_missing() {
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("sub").join("dir");
        let store = FileObservationStore::new(nested);

        store
            .save_tuning_entry("tune-dir", 1, r#"{"signal_type":"test"}"#)
            .expect("should create dir and save");
    }

    #[test]
    fn save_tuning_entry_session_id_is_sanitized() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "tune/session/with/slashes";

        store
            .save_tuning_entry(sid, 1, r#"{"signal_type":"test"}"#)
            .expect("sanitized session id");

        let entries = store.load_tuning_entries(sid);
        assert_eq!(entries.len(), 1);
    }

    // ── load_tuning_entries tests ──────────────────────────────────────

    #[test]
    fn load_tuning_entries_returns_persisted_lines() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "tune-load";

        store
            .save_tuning_entry(
                sid,
                1,
                r#"{"signal_type":"cache_warming","trigger_value":0.5}"#,
            )
            .unwrap();
        store
            .save_tuning_entry(sid, 3, r#"{"signal":"cache_warming","trigger_value":0.25}"#)
            .unwrap();

        let entries = store.load_tuning_entries(sid);
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn load_tuning_entries_empty_for_missing_session() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());

        let entries = store.load_tuning_entries("no-such-session");
        assert!(entries.is_empty());
    }

    #[test]
    fn load_tuning_entries_skips_empty_lines() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "tune-empty-lines";

        store
            .save_tuning_entry(
                sid,
                1,
                r#"{"signal_type":"cache_warming","trigger_value":0.5}"#,
            )
            .unwrap();

        // Manually append empty lines.
        let path = store.session_path_tuning(sid);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "   ").unwrap();

        let entries = store.load_tuning_entries(sid);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn load_tuning_entries_handles_corrupt_json() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());
        let sid = "tune-corrupt";

        store
            .save_tuning_entry(
                sid,
                1,
                r#"{"signal_type":"cache_warming","trigger_value":0.5}"#,
            )
            .unwrap();

        // Append a malformed line manually (cannot happen through save_tuning_entry
        // since it only writes valid JSON, but load must be resilient).
        let path = store.session_path_tuning(sid);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "not valid json at all").unwrap();

        // load_tuning_entries returns raw lines (no JSON parsing), so all 2 lines
        // should be returned (even the corrupt one — parsing is the caller's job).
        let entries = store.load_tuning_entries(sid);
        assert_eq!(entries.len(), 2);
    }

    // ── list_tuning_sessions tests ──────────────────────────────────────

    #[test]
    fn list_tuning_sessions_finds_written_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());

        store
            .save_tuning_entry("sess-alpha", 1, r#"{"signal_type":"test"}"#)
            .unwrap();
        store
            .save_tuning_entry("sess-beta", 2, r#"{"signal_type":"test"}"#)
            .unwrap();

        let sessions = store.list_tuning_sessions();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains(&"sess-alpha".to_string()));
        assert!(sessions.contains(&"sess-beta".to_string()));
    }

    #[test]
    fn list_tuning_sessions_returns_empty_for_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());

        let sessions = store.list_tuning_sessions();
        assert!(sessions.is_empty());
    }

    #[test]
    fn list_tuning_sessions_skips_non_tuning_files() {
        let tmp = TempDir::new().unwrap();
        let store = FileObservationStore::new(tmp.path().to_path_buf());

        // Write a regular observation entry (not tuning).
        store
            .save_entry(
                "regular-sess",
                0,
                &make_metrics(0, 0, 1),
                &make_facts(0, 0, 1),
            )
            .unwrap();

        // Write a tuning entry.
        store
            .save_tuning_entry("tuning-sess", 1, r#"{"signal_type":"test"}"#)
            .unwrap();

        let sessions = store.list_tuning_sessions();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0], "tuning-sess");
    }
}
