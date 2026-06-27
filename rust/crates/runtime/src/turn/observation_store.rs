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
use astra_core::observation_journal::{JournalFacts, ObservationStore, StoredEntry};

// ── FileObservationStore ────────────────────────────────────────────────────

/// JSON-Lines file backend for [`ObservationStore`].
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
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => return 0,
        };
        content.lines().filter(|l| !l.trim().is_empty()).count()
    }

    fn delete_session(&self, session_id: &str) -> Result<(), String> {
        let path = self.session_path(session_id);
        if path.exists() {
            fs::remove_file(&path).map_err(|e| format!("cannot delete {:?}: {e}", path))?;
        }
        Ok(())
    }

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
}

// ── Default store factory ─────────────────────────────────────────────────

/// Create the default file-backed observation store rooted at
/// `~/.astra/observations/`.
///
/// Returns `None` when the home directory cannot be resolved (e.g. in a
/// container without `$HOME` set). In that case a warning is logged and
/// the caller should treat observation persistence as unavailable.
pub fn default_observation_store() -> Option<Arc<dyn ObservationStore>> {
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
pub fn test_store() -> Option<Arc<dyn ObservationStore>> {
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
            rounds_completed: turn,
            total_errors: errors,
            total_tool_calls: errors + writes + 3,
            consecutive_rounds_with_outcome: writes.min(1),
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
        assert_eq!(
            store.entry_count(sid),
            1,
            "entry_count should skip empty lines"
        );
    }

    #[test]
    fn default_store_with_home_dir() {
        let store = default_observation_store();
        // In any reasonable test environment, HOME is set.
        assert!(store.is_some(), "HOME should be set in test environment");
    }

    // ── save_tuning_entry tests ────────────────────────────────────────

    #[test]
    fn save_and_load_tuning_entry_persists_to_separate_file() {
        let dir = TempDir::new().expect("tempdir");
        let store = FileObservationStore::new(dir.path().to_path_buf());

        let json = r#"{"signal":"prompt_compaction","trigger_value":0.85,"reason":"test","created_at_ms":1700000000000,"turn_index":1,"session_id":"tune-sess","priority":7}"#;
        store
            .save_tuning_entry("tune-sess", 1, json)
            .expect("save tuning entry");

        // Verify file exists and is separate from observation entries
        let tuning_path = dir.path().join("tune-sess.tuning.jsonl");
        let obs_path = dir.path().join("tune-sess.jsonl");

        assert!(
            tuning_path.exists(),
            "tuning file should exist: {:?}",
            tuning_path
        );
        assert!(
            !obs_path.exists(),
            "observation file should NOT exist: {:?}",
            obs_path
        );

        let raw = std::fs::read_to_string(&tuning_path).expect("read tuning file");
        assert!(raw.contains("prompt_compaction"));
        assert!(raw.contains("0.85"));
    }

    #[test]
    fn save_tuning_entry_multiple_jobs_are_appended() {
        let dir = TempDir::new().expect("tempdir");
        let store = FileObservationStore::new(dir.path().to_path_buf());

        store
            .save_tuning_entry(
                "multi-tune",
                1,
                r#"{"signal":"prompt_compaction","trigger_value":0.80}"#,
            )
            .expect("save 1");
        store
            .save_tuning_entry(
                "multi-tune",
                3,
                r#"{"signal":"cache_warming","trigger_value":0.20}"#,
            )
            .expect("save 2");
        store
            .save_tuning_entry(
                "multi-tune",
                5,
                r#"{"signal":"circuit_breaker_tuning","trigger_value":0.40}"#,
            )
            .expect("save 3");

        let tuning_path = dir.path().join("multi-tune.tuning.jsonl");
        let raw = std::fs::read_to_string(&tuning_path).expect("read tuning file");
        let lines: Vec<_> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(raw.contains("prompt_compaction"));
        assert!(raw.contains("cache_warming"));
        assert!(raw.contains("circuit_breaker_tuning"));
    }

    #[test]
    fn save_tuning_entry_adds_trailing_newline() {
        let dir = TempDir::new().expect("tempdir");
        let store = FileObservationStore::new(dir.path().to_path_buf());

        store
            .save_tuning_entry(
                "nl-sess",
                1,
                r#"{"signal":"prompt_compaction","trigger_value":0.85}"#,
            )
            .expect("save");
        // Verify trailing newline
        let raw = std::fs::read_to_string(dir.path().join("nl-sess.tuning.jsonl")).expect("read");
        assert!(raw.ends_with('\n'));
    }

    #[test]
    fn save_tuning_entry_creates_directory_if_missing() {
        let dir = TempDir::new().expect("tempdir");
        let nested = dir.path().join("subdir").join("deep");
        let store = FileObservationStore::new(nested);

        store
            .save_tuning_entry(
                "auto-create",
                1,
                r#"{"signal":"prompt_compaction","trigger_value":0.85}"#,
            )
            .expect("save should auto-create dirs");

        assert!(dir
            .path()
            .join("subdir")
            .join("deep")
            .join("auto-create.tuning.jsonl")
            .exists());
    }

    #[test]
    fn test_store_returns_some_and_persists() {
        let store = test_store().expect("test_store should return Some");
        store
            .save_tuning_entry(
                "teststore",
                1,
                r#"{"signal":"cache_warming","trigger_value":0.15}"#,
            )
            .expect("save");
    }

    #[test]
    fn save_tuning_entry_session_id_is_sanitized() {
        let dir = TempDir::new().expect("tempdir");
        let store = FileObservationStore::new(dir.path().to_path_buf());

        // Session id with special characters
        store
            .save_tuning_entry(
                "a/b../c",
                1,
                r#"{"signal":"prompt_compaction","trigger_value":0.85}"#,
            )
            .expect("save");

        // Should NOT create subdirectories from path traversal
        let path = dir.path().join("a_b__c.tuning.jsonl");
        assert!(path.exists(), "expect sanitized path {:?}", path);

        // Should NOT have created original traversal paths
        assert!(
            !dir.path().join("a").exists(),
            "traversal dir should not exist"
        );
    }
}
