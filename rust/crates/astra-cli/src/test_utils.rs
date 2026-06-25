// Test utilities shared between lib.rs and main.rs.
// Both targets compile the same cli/ source files, so both need
// access to `HomeGuard`, `CredentialsGuard`, and other helpers
// under `crate::tests`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};

// ── Safe env helpers ───────────────────────────────────────────────────
//
// Centralised wrappers for `std::env::set_var` and `std::env::remove_var`.
// These functions are marked `unsafe` because they mutate process-global
// state, which is UB when concurrent with any other std::env read/write
// (including `var`/`var_os`/`vars`).
//
// SAFETY: Callers must ensure single-threaded access to the environment.
// All tests using these helpers MUST be annotated with
// `#[serial_test::serial]` so that only one test mutates the environment
// at a time (single-SUP safety net). Production guards (EnvGuard,
// ChatTurnSnapshotGuard, etc.) enforce this contract via their own
// scoped guard + Drop patterns.
//
// This is the single source of truth for all SAFETY documentation
// around env mutation in the astra-cli crate.

/// # Safety
///
/// Caller must ensure no concurrent access to the environment.
/// See module-level SAFETY note above.
pub(crate) unsafe fn set_var_serial<K, V>(key: K, value: V)
where
    K: AsRef<std::ffi::OsStr>,
    V: AsRef<std::ffi::OsStr>,
{
    unsafe {
        std::env::set_var(key, value);
    }
}

/// # Safety
///
/// Caller must ensure no concurrent access to the environment.
/// See module-level SAFETY note above.
pub(crate) unsafe fn remove_var_serial<K>(key: K)
where
    K: AsRef<std::ffi::OsStr>,
{
    unsafe {
        std::env::remove_var(key);
    }
}

// ── HomeGuard ──────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct HomeGuard {
    prev: Option<OsString>,
    current: PathBuf,
    _dir: Option<tempfile::TempDir>,
}

impl HomeGuard {
    fn set_impl(path: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
        let prev = std::env::var_os("HOME");
        // SAFETY: Tests using HomeGuard are #[serial_test::serial] —
        // only one test manipulates HOME at a time.
        unsafe {
            set_var_serial("HOME", &path);
        }
        Self {
            prev,
            current: path,
            _dir: dir,
        }
    }

    pub(crate) fn temp() -> Self {
        let dir = test_temp_dir();
        Self::set_impl(dir.path().to_path_buf(), Some(dir))
    }

    pub(crate) fn set(path: impl AsRef<Path>) -> Self {
        Self::set_impl(path.as_ref().to_path_buf(), None)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.current
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        // SAFETY: Tests using HomeGuard are #[serial_test::serial].
        unsafe {
            match &self.prev {
                Some(v) => set_var_serial("HOME", v),
                None => remove_var_serial("HOME"),
            }
        }
    }
}

// ── CredentialsGuard ───────────────────────────────────────────────────

pub(crate) struct CredentialsGuard {
    prev: Option<OsString>,
    _dir: tempfile::TempDir,
}

impl Drop for CredentialsGuard {
    fn drop(&mut self) {
        // SAFETY: Tests using CredentialsGuard are #[serial_test::serial].
        unsafe {
            match &self.prev {
                Some(v) => set_var_serial("ASTRA_CLI_CREDENTIALS_DIR", v),
                None => remove_var_serial("ASTRA_CLI_CREDENTIALS_DIR"),
            }
        }
    }
}

pub(crate) fn isolate_credentials() -> CredentialsGuard {
    let dir = test_temp_dir();
    let prev = std::env::var_os("ASTRA_CLI_CREDENTIALS_DIR");
    // SAFETY: Tests using CredentialsGuard are #[serial_test::serial].
    unsafe {
        set_var_serial("ASTRA_CLI_CREDENTIALS_DIR", dir.path());
    }
    CredentialsGuard { prev, _dir: dir }
}

// ── Session Journal Isolation ──────────────────────────────────────────

/// Create a temp dir on the workspace target filesystem, avoiding
/// ENOSPC from small `/tmp` tmpfs mounts during highly-parallel test
/// runs. `ASTRA_TEST_TMPDIR` is the explicit override; otherwise use
/// Cargo's test temp root or `target/test-tmp` under the workspace.
pub(crate) fn test_temp_dir() -> tempfile::TempDir {
    use std::sync::OnceLock;

    static BASES: OnceLock<Vec<PathBuf>> = OnceLock::new();
    let bases = BASES.get_or_init(|| {
        let pid = std::process::id();
        let process_base = |root: PathBuf| root.join(format!("astra-cli-{pid}"));
        let mut bases = Vec::new();
        // 1. explicit override
        if let Ok(d) = std::env::var("ASTRA_TEST_TMPDIR") {
            let p = PathBuf::from(d);
            if p.exists() || std::fs::create_dir_all(&p).is_ok() {
                bases.push(process_base(p));
            }
        }
        // 2. Cargo-provided target temp root when available.
        if let Ok(d) = std::env::var("CARGO_TARGET_TMPDIR") {
            let p = PathBuf::from(d);
            if p.exists() || std::fs::create_dir_all(&p).is_ok() {
                bases.push(process_base(p));
            }
        }
        // 3. Locate workspace root from crate directory.
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        for ancestor in manifest.ancestors().skip(1) {
            if ancestor.join("Cargo.lock").exists() || ancestor.join("Cargo.toml").exists() {
                bases.push(process_base(ancestor.join("target").join("test-tmp")));
                break;
            }
        }
        // 4. Last resort.
        bases.push(process_base(std::env::temp_dir().join("astra-test-tmp")));
        bases
    });

    let mut errors = Vec::new();
    for base in bases {
        if let Err(error) = std::fs::create_dir_all(base) {
            errors.push(format!("{}: create base failed: {error}", base.display()));
            continue;
        }
        for attempt in 0..5 {
            match tempfile::Builder::new()
                .prefix("astra-test-")
                .tempdir_in(base)
            {
                Ok(dir) => return dir,
                Err(error) => {
                    let retryable = error.kind() == std::io::ErrorKind::StorageFull;
                    errors.push(format!(
                        "{}: create temp dir attempt {} failed: {error}",
                        base.display(),
                        attempt + 1
                    ));
                    if retryable {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    break;
                }
            }
        }
    }
    panic!(
        "create test temp dir failed in all candidate roots:\n{}",
        errors.join("\n")
    );
}

static ISOLATED_SESSIONS_DIR_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(crate) struct IsolatedSessionsGuard {
    // Drop the journal override before releasing the serial lock.
    _journal_guard: astra_services::session_journal::JournalDirGuard,
    _serial_guard: MutexGuard<'static, ()>,
}

pub(crate) fn isolated_sessions_dir() -> (tempfile::TempDir, IsolatedSessionsGuard) {
    let serial_guard = ISOLATED_SESSIONS_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tmp = test_temp_dir();
    let sessions = tmp.path().join("sessions");
    std::fs::create_dir_all(&sessions).expect("create isolated sessions root");
    let journal_guard = astra_services::session_journal::JournalDirGuard::new(&sessions);
    let guard = IsolatedSessionsGuard {
        _journal_guard: journal_guard,
        _serial_guard: serial_guard,
    };
    (tmp, guard)
}

// ── Stream Result Fixtures ─────────────────────────────────────────────

pub(crate) fn stub_stream_result(
    full_text: &str,
) -> crate::cli::stream::streaming_types::StreamResult {
    crate::cli::stream::streaming_types::StreamResult {
        full_text: full_text.to_string(),
        ..Default::default()
    }
}

pub(crate) fn stub_stream_result_with_records(
    full_text: &str,
    tool_call_records: Vec<astra_services::session_journal::ToolCallRecord>,
) -> crate::cli::stream::streaming_types::StreamResult {
    crate::cli::stream::streaming_types::StreamResult {
        full_text: full_text.to_string(),
        tool_calls_count: tool_call_records.len() as u32,
        tools_used: tool_call_records
            .iter()
            .map(|record| record.name.clone())
            .collect(),
        tool_call_records,
        ..Default::default()
    }
}

pub(crate) fn heavy_checkpoint_with_runtime_state(
    pipeline_state: serde_json::Value,
    compaction_state: serde_json::Value,
    consecutive_context_window_errors: u32,
) -> astra_pipeline::step_protocol::StepCheckpoint {
    use astra_pipeline::step_protocol::{ExecutionCursor, StepCheckpoint};

    let mut heavy = match StepCheckpoint::heavy(
        "session-turn-1".to_string(),
        "task-1".to_string(),
        "agent-1".to_string(),
        ExecutionCursor::default(),
    ) {
        StepCheckpoint::Heavy(heavy) => *heavy,
        StepCheckpoint::Light(_) => unreachable!("heavy checkpoint constructor returned light"),
    };
    heavy.pipeline_state = Some(pipeline_state);
    heavy.compaction_state = Some(compaction_state);
    heavy.consecutive_context_window_errors = consecutive_context_window_errors;
    StepCheckpoint::Heavy(Box::new(heavy))
}

// ── Async Test Waits ─────────────────────────────────────────────────

pub(crate) async fn wait_until(
    timeout: std::time::Duration,
    interval: std::time::Duration,
    mut condition: impl FnMut() -> bool,
) -> Result<(), ()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if condition() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(());
        }
        tokio::time::sleep(interval).await;
    }
}

// ── UI Adapter Fixtures ────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct TestUi {
    pub(crate) errors: Vec<String>,
    pub(crate) warnings: Vec<String>,
    pub(crate) infos: Vec<String>,
    pub(crate) statuses: Vec<String>,
    pub(crate) blank_lines: usize,
}

impl crate::cli::ui_adapter::ReplUiAdapter for TestUi {
    fn show_error(&mut self, msg: &str) {
        self.errors.push(msg.to_string());
    }

    fn show_warning(&mut self, msg: &str) {
        self.warnings.push(msg.to_string());
    }

    fn show_info(&mut self, msg: &str) {
        self.infos.push(msg.to_string());
    }

    fn show_status(&mut self, msg: &str) {
        self.statuses.push(msg.to_string());
    }

    fn blank_line(&mut self) {
        self.blank_lines += 1;
    }
}

// ── Unit tests ─────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{HomeGuard, isolate_credentials, test_temp_dir};

    // ── HomeGuard ──────────────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn home_guard_temp_creates_temp_dir() {
        let guard = HomeGuard::temp();
        let path = guard.path().to_path_buf();
        assert!(path.exists(), "temp home dir must exist");
        assert_eq!(
            std::env::var_os("HOME").as_deref(),
            Some(path.as_os_str()),
            "HOME must be set to temp dir while guard is alive"
        );
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn home_guard_set_redirects_home() {
        let tmp = test_temp_dir();
        let guard = HomeGuard::set(tmp.path());
        assert_eq!(guard.path(), tmp.path(), "guard path must match set path");
        assert_eq!(
            std::env::var_os("HOME").as_deref(),
            Some(tmp.path().as_os_str()),
            "HOME env must be set while guard is alive"
        );
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn home_guard_drop_restores_previous_home() {
        let original = std::env::var_os("HOME");
        {
            let _guard = HomeGuard::temp();
            // HOME is redirected inside the block
            assert_ne!(
                std::env::var_os("HOME"),
                original,
                "HOME must differ while guard is alive"
            );
        }
        // After drop, HOME is restored
        assert_eq!(
            std::env::var_os("HOME"),
            original,
            "HOME must be restored after guard drop"
        );
    }

    #[test]
    #[serial_test::serial]
    fn home_guard_nested_restores_correctly() {
        let original = std::env::var_os("HOME");
        let _outer = HomeGuard::temp();
        let outer_home = std::env::var_os("HOME");
        {
            let _inner = HomeGuard::set("/tmp/nested-test");
            let inner_home = std::env::var_os("HOME");
            assert!(
                inner_home
                    .as_ref()
                    .map(|v| v.to_string_lossy().contains("nested-test"))
                    .unwrap_or(false),
                "inner guard must redirect HOME to nested-test"
            );
        }
        // After inner drops, HOME must be restored to outer's value
        assert_eq!(
            std::env::var_os("HOME"),
            outer_home,
            "HOME must be restored to outer after inner drop"
        );
        drop(_outer);
        assert_eq!(
            std::env::var_os("HOME"),
            original,
            "HOME must be restored to original after outer drop"
        );
    }

    #[test]
    #[serial_test::serial]
    fn home_guard_path_returns_current() {
        let tmp = test_temp_dir();
        let guard = HomeGuard::set(tmp.path());
        assert_eq!(guard.path(), tmp.path());
        drop(guard);
    }

    // ── CredentialsGuard ───────────────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn credentials_guard_sets_env_var() {
        let guard = isolate_credentials();
        assert!(
            std::env::var_os("ASTRA_CLI_CREDENTIALS_DIR").is_some(),
            "ASTRA_CLI_CREDENTIALS_DIR must be set while guard is alive"
        );
        drop(guard);
    }

    #[test]
    #[serial_test::serial]
    fn credentials_guard_drop_removes_env_var() {
        {
            let _guard = isolate_credentials();
            assert!(std::env::var_os("ASTRA_CLI_CREDENTIALS_DIR").is_some());
        }
        assert!(
            std::env::var_os("ASTRA_CLI_CREDENTIALS_DIR").is_none(),
            "ASTRA_CLI_CREDENTIALS_DIR must be removed after guard drop"
        );
    }
}
