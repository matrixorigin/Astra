//! `astra-cli` library crate.
//!
//! Provides the CLI application logic shared between the main `astra` binary
//! and external consumers (tests, sub-binaries like `mock_mcp_server`).
//!
//! Module layout:
//! - `cli/` — CLI-specific modules (slash commands, TUI, session mgmt, etc.)
//! - `edge_tools` / `edge_tools/` — edge tool definitions and execution
//! - `tui` — Ratatui terminal UI
//! - Top-level utility modules (delta_log, diff_utils, mcp_client, etc.)

// Clippy 1.94 — allow backlog; refine incrementally.
#![allow(
    dead_code,
    deprecated,
    clippy::collapsible_if,
    clippy::derivable_impls,
    clippy::field_reassign_with_default,
    clippy::items_after_test_module,
    clippy::let_unit_value,
    clippy::manual_strip,
    clippy::needless_borrow,
    clippy::redundant_closure,
    clippy::single_match,
    clippy::unnecessary_mut_passed
)]

// ═══════════════════════════ Top-level utility modules ═══════════════════
pub mod delta_log;
pub mod diff_utils;
pub mod edge_tools;
pub mod explain_dag;
pub mod git_branch_cache;
pub mod manifest_loader;
pub mod mcp_client;
pub mod sandbox_retry;
pub(crate) mod skill_instructions;
pub mod tool_safety_guard;

// ═══════════════════════════ CLI modules ═════════════════════════════════
pub mod cli;

// ═══════════════════════════ TUI ════════════════════════════════════════
pub mod tui;

// ═══════════════════════════ Crate-internal re-exports ═══════════════════
// These items are pub(crate) in cli submodules; we re-export them here so
// main.rs (and tests) can access via `astra_cli::` or `crate::`.

// Common external crate aliases — many cli/ files use bare `prompts::`,
// `session_journal::`, `tool_registry::` which were previously resolved
// via #[path]-based re-exports in main.rs.
pub(crate) use cli::*;

// SSE streaming types
pub(crate) use cli::streaming_types::{PartialTurnData, StreamResult, TurnFailure, VerdictEvent};

// Session state
pub(crate) use cli::plan_monitor::{format_duration_short, format_plan_progress};
pub(crate) use cli::session_state::{ExplainMode, SessionState, SkillDevState};

// Cloud sync
pub(crate) use cli::cloud_sync::post_auth_cloud_resync;

// Core command routing

#[cfg(test)]
pub(crate) mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    pub(crate) struct CredentialsGuard {
        _lock: MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for CredentialsGuard {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("ASTRA_CLI_CREDENTIALS_DIR");
            }
        }
    }

    fn creds_lock() -> MutexGuard<'static, ()> {
        static CREDS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        CREDS_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn isolate_credentials() -> CredentialsGuard {
        let lock = creds_lock();
        let dir = tempfile::tempdir().expect("create temp credentials dir");
        unsafe {
            std::env::set_var("ASTRA_CLI_CREDENTIALS_DIR", dir.path());
        }
        CredentialsGuard {
            _lock: lock,
            _dir: dir,
        }
    }

    pub(crate) struct HomeGuard {
        _lock: MutexGuard<'static, ()>,
        prev: Option<OsString>,
        current: PathBuf,
        _dir: Option<tempfile::TempDir>,
    }

    fn home_lock() -> MutexGuard<'static, ()> {
        static HOME_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        HOME_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    impl HomeGuard {
        fn set_impl(path: PathBuf, dir: Option<tempfile::TempDir>) -> Self {
            let lock = home_lock();
            let prev = std::env::var_os("HOME");
            unsafe {
                std::env::set_var("HOME", &path);
            }
            Self {
                _lock: lock,
                prev,
                current: path,
                _dir: dir,
            }
        }

        pub(crate) fn temp() -> Self {
            let dir = tempfile::tempdir().expect("create temp home dir");
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
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var("HOME", v),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }
}
