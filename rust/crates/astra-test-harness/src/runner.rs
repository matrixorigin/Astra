//! Run data types + configuration.
//!
//! This module holds the shared data model — [`RunOutcome`] and
//! [`RunnerConfig`] — plus the pure helpers used to parse the
//! `astra chat --json` envelope and resolve the model matrix.
//!
//! The actual subprocess invocation lives in [`crate::exec`] behind
//! the `CaseExecutor` trait, so tests can substitute a fake executor.
//!
//! Timeouts on the real executor are encoded as synthetic outcomes
//! with `exit_code = 124` (POSIX `timeout` convention) so downstream
//! tooling can classify them alongside real exits.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::case::Case;

/// Captured outcome of one `astra chat` subprocess run. Fields
/// mirror what `astra chat --json` prints on stdout, plus captured
/// stderr and timing.
///
/// `#[non_exhaustive]` is load-bearing: this struct is serialized
/// into `--format json` reports, so external consumers pattern-match
/// and / or deserialize it. We reserve the right to add fields (new
/// token buckets, new timing breakdowns, new observability tags)
/// without a SemVer breakage. In-crate construction is unaffected;
/// downstream crates must use `..` in struct patterns.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[non_exhaustive]
pub struct RunOutcome {
    pub model: String,
    pub exit_code: i32,
    pub text: String,
    pub stderr: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    pub tool_calls_count: u32,
    pub tools_used: Vec<String>,
    pub completion_tokens: u64,
    /// Fresh prompt/input tokens, excluding cache reads and cache writes.
    pub prompt_tokens: u64,
    /// Prompt/input tokens served from provider prompt cache.
    pub cached_input_tokens: u64,
    /// Prompt/input tokens written into provider prompt cache.
    pub cache_creation_tokens: u64,
    pub duration_ms: u64,
    /// Number of LLM round-trips (StepStarted events in step_events).
    pub turn_rounds: u32,
    /// Number of tool calls that hit the idempotency cache.
    pub cache_hits: u32,
    /// Total tool calls (for computing cache rate = cache_hits / total).
    pub total_tool_calls: u32,
    /// Time to first token in ms (from JSON envelope).
    pub ttft_ms: u64,
}

impl RunOutcome {
    /// Public constructor that external callers (integration tests,
    /// embedders) can use without fighting `#[non_exhaustive]`.
    /// Starts from defaults and sets `model`. Override additional
    /// fields with the dedicated `with_*` setters since
    /// `#[non_exhaustive]` prevents struct-update syntax from other
    /// crates.
    ///
    /// ```
    /// use astra_test_harness::runner::RunOutcome;
    /// let out = RunOutcome::new("my-model")
    ///     .with_exit_code(42)
    ///     .with_session_id("sess-x");
    /// # let _ = out;
    /// ```
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    pub fn with_exit_code(mut self, code: i32) -> Self {
        self.exit_code = code;
        self
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self
    }

    pub fn with_stderr(mut self, stderr: impl Into<String>) -> Self {
        self.stderr = stderr.into();
        self
    }

    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    pub fn with_tools_used(mut self, tools: Vec<String>) -> Self {
        self.tool_calls_count = tools.len() as u32;
        self.tools_used = tools;
        self
    }
}

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// Path to the astra release binary.
    pub astra_bin: PathBuf,
    /// Models to run each case against when the case's `models:`
    /// field is empty. CLI `--models` provides this.
    pub fallback_models: Vec<String>,
    /// Optional shared working directory — harness runs from here
    /// so relative paths in cases (if ever added) are stable.
    pub working_dir: Option<PathBuf>,
    /// Optional profile name passed as `--profile <name>` to astra
    /// subprocesses. Set by preflight auto-register to isolate
    /// harness credentials from the user's active profile.
    pub profile: Option<String>,
}

impl RunnerConfig {
    pub fn new(astra_bin: impl Into<PathBuf>) -> Self {
        Self {
            astra_bin: astra_bin.into(),
            fallback_models: Vec::new(),
            working_dir: None,
            profile: None,
        }
    }
    pub fn with_fallback_models(mut self, models: Vec<String>) -> Self {
        self.fallback_models = models;
        self
    }
}

/// Parse astra's `--json` stdout into a RunOutcome skeleton.
/// Fills stderr/exit_code elsewhere. On parse failure, returns a
/// RunOutcome with the raw stdout stuffed into `text` so reports
/// still surface the payload for debugging.
pub(crate) fn parse_json_outcome(stdout: &str, model: &str) -> RunOutcome {
    let trimmed = stdout.trim();
    let v: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(parse_err) => {
            // `astra chat --json` printed something that isn't JSON.
            // Usual causes: the child printed a panic backtrace, a
            // deprecation notice, or hit an error before the JSON
            // envelope. Returning `exit_code: 0` on top of garbage
            // would let that garbage sail past every criterion, so
            // surface the anomaly on stderr where the reviewer can
            // see it — the caller subsequently overwrites exit_code
            // with the real process status.
            //
            // Skip the warning when stdout is empty: the usual cause
            // is a child that errored early and printed only to
            // stderr (credential failure, missing binary). Printing
            // "parse fallback" on every such run drowns the real
            // stderr error in harness noise.
            if !trimmed.is_empty() {
                let preview: String = trimmed.chars().take(160).collect();
                eprintln!(
                    "[astra-test] WARNING: stdout from astra chat --json was not valid JSON \
                     ({parse_err}); falling back to raw-text mode. Preview: {preview:?}"
                );
            }
            return RunOutcome {
                model: model.into(),
                exit_code: -1,
                text: trimmed.to_string(),
                stderr: String::new(),
                session_id: None,
                run_id: None,
                tool_calls_count: 0,
                tools_used: vec![],
                completion_tokens: 0,
                prompt_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_tokens: 0,
                duration_ms: 0,
                turn_rounds: 0,
                cache_hits: 0,
                total_tool_calls: 0,
                ttft_ms: 0,
            };
        }
    };
    // Merge background agent results into the visible text so
    // criteria (text_contains, judger) can see child output.
    let mut text = v
        .get("text")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if let Some(bg) = v.get("background_agent_results").and_then(|x| x.as_array()) {
        for entry in bg {
            let agent_id = entry
                .get("agent_id")
                .and_then(|x| x.as_str())
                .unwrap_or("?");
            let result = entry.get("result").and_then(|x| x.as_str()).unwrap_or("");
            text.push_str(&format!("\n[background:{agent_id}]: {result}"));
        }
    }
    RunOutcome {
        model: model.into(),
        exit_code: v.get("exit_code").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
        text,
        stderr: String::new(),
        session_id: v
            .get("session_id")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        run_id: v.get("run_id").and_then(|x| x.as_str()).map(str::to_string),
        tool_calls_count: {
            // The JSON envelope reports tool_calls_count as u64 but
            // `ToolsCountBetween` compares against u32. A pathological
            // runaway agent exceeding u32::MAX would wrap silently;
            // saturate + warn so the anomaly is visible instead of
            // quietly causing a "tool_calls_count=0" FAIL message
            // that reads like a missing-tool bug.
            let raw = v
                .get("tool_calls_count")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            if raw > u32::MAX as u64 {
                eprintln!(
                    "[astra-test] WARNING: tool_calls_count {raw} exceeds u32::MAX \
                     ({}); saturating. Almost certainly a runaway loop.",
                    u32::MAX
                );
                u32::MAX
            } else {
                raw as u32
            }
        },
        tools_used: v
            .get("tools_used")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        completion_tokens: v
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        prompt_tokens: v
            .get("fresh_prompt_tokens")
            .or_else(|| v.get("prompt_tokens"))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        cached_input_tokens: v
            .get("cached_input_tokens")
            .or_else(|| v.get("cache").and_then(|cache| cache.get("read_tokens")))
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        cache_creation_tokens: v
            .get("cache_creation_tokens")
            .or_else(|| {
                v.get("cache")
                    .and_then(|cache| cache.get("creation_tokens"))
            })
            .and_then(|x| x.as_u64())
            .unwrap_or(0),
        duration_ms: 0,
        turn_rounds: 0,
        cache_hits: 0,
        total_tool_calls: 0,
        ttft_ms: v.get("ttft_ms").and_then(|x| x.as_u64()).unwrap_or(0),
    }
}

/// Which models to run a case on: case-level override if set,
/// else config fallback list. Errors when both are empty.
pub fn resolve_models(case: &Case, cfg: &RunnerConfig) -> Result<Vec<String>, anyhow::Error> {
    match &case.models {
        Some(m) if !m.is_empty() => Ok(m.clone()),
        _ => {
            if cfg.fallback_models.is_empty() {
                Err(anyhow::anyhow!(
                    "case {:?} has no `models:` list and --models CLI flag was not set",
                    case.name
                ))
            } else {
                Ok(cfg.fallback_models.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_outcome_happy_path() {
        let stdout = r#"{
            "session_id": "s1",
            "run_id": "r1",
            "text": "hi",
            "exit_code": 0,
            "tool_calls_count": 2,
            "tools_used": ["a", "b"],
            "completion_tokens": 10,
            "prompt_tokens": 30,
            "fresh_prompt_tokens": 20,
            "cache": {"read_tokens": 7, "creation_tokens": 3}
        }"#;
        let out = parse_json_outcome(stdout, "m");
        assert_eq!(out.model, "m");
        assert_eq!(out.session_id.as_deref(), Some("s1"));
        assert_eq!(out.tool_calls_count, 2);
        assert_eq!(out.tools_used, vec!["a", "b"]);
        assert_eq!(out.prompt_tokens, 20);
        assert_eq!(out.cached_input_tokens, 7);
        assert_eq!(out.cache_creation_tokens, 3);
    }

    #[test]
    fn parse_json_outcome_uses_prompt_tokens_when_fresh_bucket_missing() {
        let stdout = r#"{
            "text": "legacy",
            "exit_code": 0,
            "completion_tokens": 5,
            "prompt_tokens": 20,
            "cache": {"read_tokens": 7, "creation_tokens": 3}
        }"#;
        let out = parse_json_outcome(stdout, "m");
        assert_eq!(out.prompt_tokens, 20);
        assert_eq!(out.cached_input_tokens, 7);
        assert_eq!(out.cache_creation_tokens, 3);
    }

    #[test]
    fn parse_json_outcome_fallback_on_invalid_json() {
        let out = parse_json_outcome("not json", "m");
        assert_eq!(out.text, "not json");
        assert_eq!(out.tool_calls_count, 0);
        assert_eq!(
            out.exit_code, -1,
            "fallback must signal anomaly via exit_code=-1"
        );
    }

    #[test]
    fn parse_json_outcome_merges_background_agent_results() {
        let stdout = r#"{
            "text": "parent output",
            "exit_code": 0,
            "tool_calls_count": 1,
            "tools_used": ["spawn_agent"],
            "background_agent_results": [
                {"agent_id": "child-G1", "result": "inherited-ok"},
                {"agent_id": "child-G2", "result": "delegate-G2-ok"}
            ]
        }"#;
        let out = parse_json_outcome(stdout, "m");
        assert!(
            out.text.contains("inherited-ok"),
            "background result must appear in text: {}",
            out.text
        );
        assert!(
            out.text.contains("delegate-G2-ok"),
            "second background result must appear in text: {}",
            out.text
        );
        assert!(
            out.text.starts_with("parent output"),
            "parent text must come first"
        );
    }

    #[test]
    fn resolve_models_uses_case_override() {
        let case = Case {
            name: "c".into(),
            description: None,
            prompt: "p".into(),
            models: Some(vec!["opus".into()]),
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            setup_cmd: None,
            teardown_cmd: None,
        };
        let cfg = RunnerConfig::new("astra").with_fallback_models(vec!["sonnet".into()]);
        assert_eq!(resolve_models(&case, &cfg).unwrap(), vec!["opus"]);
    }

    #[test]
    fn resolve_models_falls_back_to_cli_list() {
        let case = Case {
            name: "c".into(),
            description: None,
            prompt: "p".into(),
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            setup_cmd: None,
            teardown_cmd: None,
        };
        let cfg = RunnerConfig::new("astra")
            .with_fallback_models(vec!["sonnet".into(), "minimax".into()]);
        let m = resolve_models(&case, &cfg).unwrap();
        assert_eq!(m, vec!["sonnet", "minimax"]);
    }

    #[test]
    fn resolve_models_errors_when_no_source() {
        let case = Case {
            name: "c".into(),
            description: None,
            prompt: "p".into(),
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            setup_cmd: None,
            teardown_cmd: None,
        };
        let cfg = RunnerConfig::new("astra");
        assert!(resolve_models(&case, &cfg).is_err());
    }
}
