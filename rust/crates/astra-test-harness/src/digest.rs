//! Journal digest collection for FAIL diagnostics.
//!
//! When a case fails and the outcome has a session_id, the harness
//! shells out to `astra journal digest --format json --focus summary`
//! to pull aggregate metrics (turns, tokens, tool_calls, errors, etc.)
//! and embeds the result in the report. Point: a developer reading a
//! FAIL no longer has to copy the session_id, open a terminal, and run
//! the digest themselves — it's already there.
//!
//! ## Why its own module
//!
//! The digest command is external (another subprocess). Trait-gating
//! it means:
//! - Tests inject a `FakeDigestCollector` instead of spawning astra.
//! - A future in-process digest can swap in without touching the
//!   suite runner.
//! - Failures of the digest call don't poison the report — they land
//!   in `digest_error` and PASS criteria keep their FAIL signal.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Collected digest blob. The `json` field is whatever
/// `astra journal digest --format json --focus summary` printed,
/// stored as raw JSON for downstream consumption. Stored as `Value`
/// so report formats can expose it without reshaping.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DigestArtifact {
    pub session_id: String,
    pub json: serde_json::Value,
}

/// Trait for collecting a journal digest for a failed case.
#[async_trait]
pub trait DigestCollector: Send + Sync {
    async fn collect(&self, session_id: &str) -> Result<DigestArtifact, String>;
}

/// Shell-out impl — runs `astra journal digest --format json
/// --focus summary <session_id>`. Summary focus keeps the embedded
/// blob small (turn-level metrics only, not per-line detail).
pub struct AstraCliDigestCollector {
    pub astra_bin: PathBuf,
    pub timeout_seconds: u64,
}

impl AstraCliDigestCollector {
    pub fn new(astra_bin: impl Into<PathBuf>) -> Self {
        Self {
            astra_bin: astra_bin.into(),
            // 15s default is generous — digest is offline JSON
            // aggregation of a local file. CI cold-start can burn
            // this when tokio's cargo-warmed target/debug isn't
            // cached; use `with_timeout` there.
            timeout_seconds: 15,
        }
    }

    /// Override the digest subprocess timeout. Useful on cold CI
    /// hosts where 15s is tight, or in tests that want a fast-fail.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_seconds = secs;
        self
    }
}

#[async_trait]
impl DigestCollector for AstraCliDigestCollector {
    async fn collect(&self, session_id: &str) -> Result<DigestArtifact, String> {
        let mut cmd = Command::new(&self.astra_bin);
        cmd.arg("journal")
            .arg("digest")
            .arg("--format")
            .arg("json")
            .arg("--focus")
            .arg("summary")
            .arg(session_id)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            // Capture stderr to avoid pipe backpressure stalling the
            // child — wait_with_output drains both concurrently.
            .stderr(Stdio::piped())
            // See exec.rs for the kill_on_drop rationale.
            .kill_on_drop(true);

        let child = cmd.spawn().map_err(|e| format!("spawn digest: {e}"))?;
        let timeout = Duration::from_secs(self.timeout_seconds);
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| format!("digest timeout after {}s", timeout.as_secs()))?
            .map_err(|e| format!("digest wait: {e}"))?;
        let stdout_body = String::from_utf8_lossy(&output.stdout).to_string();

        let trimmed = stdout_body.trim();
        if trimmed.is_empty() {
            return Err(format!(
                "digest produced empty stdout for session {session_id} \
                 (session file probably doesn't exist yet or was not flushed)"
            ));
        }
        let json: serde_json::Value = serde_json::from_str(trimmed).map_err(|e| {
            format!(
                "digest stdout is not valid JSON: {e}; raw={:?}",
                trimmed.chars().take(200).collect::<String>()
            )
        })?;
        Ok(DigestArtifact {
            session_id: session_id.to_string(),
            json,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Test helpers. Kept in lib so integration tests can reuse.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory digest collector. Tests seed an expected artifact
    /// or error per session_id; `collect()` returns it.
    pub struct FakeDigestCollector {
        pub results: Mutex<HashMap<String, Result<DigestArtifact, String>>>,
        pub calls: Mutex<Vec<String>>,
    }

    impl FakeDigestCollector {
        pub fn new() -> Self {
            Self {
                results: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
            }
        }
        pub fn seed_ok(&self, session_id: &str, json: serde_json::Value) {
            self.results.lock().unwrap().insert(
                session_id.to_string(),
                Ok(DigestArtifact {
                    session_id: session_id.to_string(),
                    json,
                }),
            );
        }
        pub fn seed_err(&self, session_id: &str, err: &str) {
            self.results
                .lock()
                .unwrap()
                .insert(session_id.to_string(), Err(err.to_string()));
        }
    }

    #[async_trait]
    impl DigestCollector for FakeDigestCollector {
        async fn collect(&self, session_id: &str) -> Result<DigestArtifact, String> {
            self.calls.lock().unwrap().push(session_id.to_string());
            self.results
                .lock()
                .unwrap()
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| Err(format!("fake: no seed for {session_id}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn astra_cli_digest_collector_fails_loudly_when_bin_missing() {
        // Missing binary → spawn error, not silent empty digest.
        let collector = AstraCliDigestCollector::new("/nonexistent/astra-binary");
        let res = collector.collect("abc").await;
        assert!(res.is_err());
        let err = res.unwrap_err();
        assert!(err.starts_with("spawn digest"), "unexpected err: {err}");
    }

    #[test]
    fn with_timeout_overrides_default() {
        let c = AstraCliDigestCollector::new("/nonexistent/astra").with_timeout(42);
        assert_eq!(c.timeout_seconds, 42);
    }

    #[tokio::test]
    async fn with_timeout_honored_when_subprocess_hangs() {
        // Need a bash shim that hangs so we can prove the override
        // actually caps elapsed time. Skipped on platforms without
        // /bin/sh.
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        // `astra journal digest …` args are swallowed; the shim just
        // sleeps indefinitely.
        std::fs::write(&shim, "#!/bin/sh\nsleep 30\n").expect("write shim");
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();

        let collector = AstraCliDigestCollector::new(shim).with_timeout(2);
        let start = std::time::Instant::now();
        let res = collector.collect("sess-hangs").await;
        let elapsed = start.elapsed();
        assert!(res.is_err(), "hanging subprocess should produce an error");
        let err = res.unwrap_err();
        assert!(
            err.contains("digest timeout after 2s"),
            "error should name the configured timeout, got: {err}"
        );
        assert!(
            elapsed.as_secs() <= 5,
            "with_timeout(2) didn't cap elapsed — ran {}s",
            elapsed.as_secs()
        );
    }

    #[tokio::test]
    async fn fake_digest_collector_returns_seeded_artifact() {
        let c = test_support::FakeDigestCollector::new();
        c.seed_ok("sess-1", serde_json::json!({"turns": 4}));
        let got = c.collect("sess-1").await.unwrap();
        assert_eq!(got.session_id, "sess-1");
        assert_eq!(got.json["turns"], 4);
        assert_eq!(c.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn fake_digest_collector_surfaces_seeded_error() {
        let c = test_support::FakeDigestCollector::new();
        c.seed_err("sess-broken", "flushed too late");
        let err = c.collect("sess-broken").await.unwrap_err();
        assert!(err.contains("flushed too late"));
    }
}
