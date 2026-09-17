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

/// The dashboard and report renderer consume the v2 summary wire, not an
/// arbitrary JSON object. Keep this literal aligned with the producer's
/// `astra-cli::journal_digest::SCHEMA_VERSION`.
pub const JOURNAL_DIGEST_SCHEMA_VERSION: &str = "astra-journal-digest-v2";

/// Collected, validated digest blob. The `json` field retains the producer's
/// exact v2 summary wire for downstream JSON reports without reshaping it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DigestArtifact {
    pub session_id: String,
    pub json: serde_json::Value,
}

/// Validate the stable summary contract emitted by `journal digest --focus
/// summary`. This is intentionally structural: the artifact is retained as
/// raw JSON for reporting, but no missing field may silently become a fake
/// zero and the payload must belong to the requested session.
pub(crate) fn validate_digest_json(
    json: &serde_json::Value,
    expected_session_id: &str,
) -> Result<(), String> {
    let object = json
        .as_object()
        .ok_or_else(|| "digest stdout must be a JSON object".to_string())?;
    let schema = object
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "digest is missing string schema_version".to_string())?;
    if schema != JOURNAL_DIGEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported digest schema_version {schema:?}, expected {JOURNAL_DIGEST_SCHEMA_VERSION:?}"
        ));
    }
    let session_id = object
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "digest is missing string session_id".to_string())?;
    if session_id != expected_session_id {
        return Err(format!(
            "digest session_id {session_id:?} does not match requested {expected_session_id:?}"
        ));
    }
    object
        .get("journal_file")
        .and_then(serde_json::Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| "digest is missing non-empty journal_file".to_string())?;
    for key in ["journal_lines_non_empty", "journal_lines_malformed"] {
        if object
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            return Err(format!("digest is missing non-negative integer {key}"));
        }
    }
    let aggregates = object
        .get("aggregates")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "digest is missing aggregates object".to_string())?;
    const COUNTS: &[&str] = &[
        "attempt_count",
        "turn_count",
        "turn_error_count",
        "compact_count",
        "stall_count",
        "error_event_count",
        "session_start_count",
        "session_end_count",
        "total_tokens_in",
        "total_tokens_out",
        "total_duration_ms",
        "total_tool_calls",
        "subrun_count",
        "subrun_total_tokens_in",
        "subrun_total_tokens_out",
        "subrun_total_duration_ms",
        "subrun_total_tool_calls",
        "inclusive_total_tokens_in",
        "inclusive_total_tokens_out",
        "inclusive_total_tool_calls",
        "total_fresh_tool_calls",
        "total_noop_or_cached_tool_calls",
        "tool_calls_failed",
        "safety_guard_blocks",
    ];
    for key in COUNTS {
        if aggregates
            .get(*key)
            .and_then(serde_json::Value::as_u64)
            .is_none()
        {
            return Err(format!(
                "digest aggregates is missing non-negative integer {key}"
            ));
        }
    }
    for key in [
        "avg_tokens_in",
        "avg_tokens_out",
        "avg_duration_ms",
        "avg_llm_rounds",
        "avg_tool_calls_per_round",
    ] {
        let value = aggregates
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .ok_or_else(|| format!("digest aggregates is missing numeric {key}"))?;
        if !value.is_finite() || value < 0.0 {
            return Err(format!(
                "digest aggregates {key} is not a finite non-negative number"
            ));
        }
    }
    Ok(())
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
    pub profile: Option<String>,
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
            profile: None,
        }
    }

    /// Override the digest subprocess timeout. Useful on cold CI
    /// hosts where 15s is tight, or in tests that want a fast-fail.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_seconds = secs;
        self
    }

    /// Bind digest lookup to the same credential profile that produced the
    /// session journal.
    pub fn with_profile(mut self, profile: Option<String>) -> Self {
        self.profile = profile;
        self
    }
}

#[async_trait]
impl DigestCollector for AstraCliDigestCollector {
    async fn collect(&self, session_id: &str) -> Result<DigestArtifact, String> {
        let mut cmd = Command::new(&self.astra_bin);
        if let Some(profile) = &self.profile {
            cmd.arg("--profile").arg(profile);
        }
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
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "digest exited {}: {}",
                output.status,
                stderr.chars().take(500).collect::<String>()
            ));
        }
        let stdout_body = String::from_utf8_lossy(&output.stdout).into_owned();

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
        validate_digest_json(&json, session_id)?;
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
            self.results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
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
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.to_string(), Err(err.to_string()));
        }
    }

    #[async_trait]
    impl DigestCollector for FakeDigestCollector {
        async fn collect(&self, session_id: &str) -> Result<DigestArtifact, String> {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(session_id.to_string());
            self.results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| Err(format!("fake: no seed for {session_id}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_digest_json(session_id: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version": JOURNAL_DIGEST_SCHEMA_VERSION,
            "session_id": session_id,
            "journal_file": format!("/tmp/{session_id}.jsonl"),
            "journal_lines_non_empty": 1,
            "journal_lines_malformed": 0,
            "aggregates": {
                "attempt_count": 1, "turn_count": 1, "turn_error_count": 0,
                "compact_count": 0, "stall_count": 0, "error_event_count": 0,
                "session_start_count": 1, "session_end_count": 1,
                "total_tokens_in": 10, "total_tokens_out": 5, "total_duration_ms": 20,
                "total_tool_calls": 1, "subrun_count": 0,
                "subrun_total_tokens_in": 0, "subrun_total_tokens_out": 0,
                "subrun_total_duration_ms": 0, "subrun_total_tool_calls": 0,
                "inclusive_total_tokens_in": 10, "inclusive_total_tokens_out": 5,
                "inclusive_total_tool_calls": 1, "total_fresh_tool_calls": 1,
                "total_noop_or_cached_tool_calls": 0, "tool_calls_failed": 0,
                "safety_guard_blocks": 0, "avg_tokens_in": 10.0,
                "avg_tokens_out": 5.0, "avg_duration_ms": 20.0,
                "avg_llm_rounds": 1.0, "avg_tool_calls_per_round": 1.0
            }
        })
    }

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
    async fn digest_subprocess_uses_the_session_credential_profile() {
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let args_path = tmp.path().join("args");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(
            &shim,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s\\n' '{}'\n",
                args_path.display(),
                valid_digest_json("session-a")
            ),
        )
        .expect("write shim");

        AstraCliDigestCollector::new(shim)
            .with_profile(Some("profile-a".to_string()))
            .collect("session-a")
            .await
            .expect("digest");

        let args = std::fs::read_to_string(args_path).expect("read args");
        assert!(
            args.starts_with("--profile\nprofile-a\njournal\ndigest\n"),
            "{args}"
        );
        assert!(args.ends_with("session-a\n"), "{args}");
    }

    #[tokio::test]
    async fn digest_rejects_foreign_or_partial_success_payloads() {
        use crate::test_support::write_executable_shim;
        for (payload, expected) in [
            (valid_digest_json("other"), "does not match requested"),
            (
                serde_json::json!({
                    "schema_version": JOURNAL_DIGEST_SCHEMA_VERSION,
                    "session_id": "session-a",
                    "journal_file": "/tmp/session-a.jsonl",
                    "journal_lines_non_empty": 1,
                    "journal_lines_malformed": 0,
                    "aggregates": {}
                }),
                "missing non-negative integer attempt_count",
            ),
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let shim = tmp.path().join("fake-astra");
            write_executable_shim(&shim, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", payload))
                .expect("write shim");
            let error = AstraCliDigestCollector::new(shim)
                .collect("session-a")
                .await
                .expect_err("invalid digest must fail closed");
            assert!(error.contains(expected), "{error}");
        }
    }

    #[tokio::test]
    async fn with_timeout_honored_when_subprocess_hangs() {
        // Need a bash shim that hangs so we can prove the override
        // actually caps elapsed time. Skipped on platforms without
        // /bin/sh.
        if !std::path::Path::new("/bin/sh").exists() {
            return;
        }
        use crate::test_support::write_executable_shim;
        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        // `astra journal digest …` args are swallowed; the shim just
        // sleeps indefinitely.
        write_executable_shim(&shim, "#!/bin/sh\nsleep 30\n").expect("write shim");

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
        assert_eq!(c.calls.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    }

    #[tokio::test]
    async fn fake_digest_collector_surfaces_seeded_error() {
        let c = test_support::FakeDigestCollector::new();
        c.seed_err("sess-broken", "flushed too late");
        let err = c.collect("sess-broken").await.unwrap_err();
        assert!(err.contains("flushed too late"));
    }

    #[tokio::test]
    async fn digest_rejects_json_emitted_by_failed_subprocess() {
        use crate::test_support::write_executable_shim;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(
            &shim,
            "#!/bin/sh\necho '{\"turns\":999}'\necho 'digest backend failed' 1>&2\nexit 23\n",
        )
        .expect("write shim");

        let error = AstraCliDigestCollector::new(shim)
            .collect("session-a")
            .await
            .expect_err("non-zero digest must never become evidence");
        assert!(error.contains("digest exited"), "{error}");
        assert!(error.contains("digest backend failed"), "{error}");
    }

    #[tokio::test]
    async fn digest_rejects_non_object_success_payload() {
        use crate::test_support::write_executable_shim;

        let tmp = tempfile::tempdir().expect("tempdir");
        let shim = tmp.path().join("fake-astra");
        write_executable_shim(&shim, "#!/bin/sh\nprintf '%s\\n' 'null'\n").expect("write shim");

        let error = AstraCliDigestCollector::new(shim)
            .collect("session-a")
            .await
            .expect_err("digest evidence must be an object");
        assert!(error.contains("JSON object"), "{error}");
    }
}
