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

pub(crate) const PROTOCOL_ERROR_MARKER: &str = "[astra-test:protocol-error]";

#[derive(Debug, Clone)]
pub struct RunnerProfileIdentity {
    pub profile_name: String,
    pub local_owner_scope: astra_services::OwnerScope,
    pub artifact_owner_scopes: Vec<astra_services::OwnerScope>,
}

/// Resolve the CLI profile and both artifact namespaces used by one remote
/// harness run: profile-scoped local CLI events and account-scoped server
/// events. Verification must never guess either identity.
pub fn resolve_runner_profile_owner(
    requested_profile: Option<&str>,
) -> Result<RunnerProfileIdentity, String> {
    let credentials = astra_credentials::CredentialStore::new()
        .load()
        .map_err(|error| format!("load CLI credentials for harness session capture: {error}"))?;
    let profile_name = astra_credentials::CredentialStore::resolve_profile_name(
        requested_profile,
        credentials.current_profile.as_deref(),
    );
    let profile = credentials.profiles.get(&profile_name).ok_or_else(|| {
        format!(
            "credential profile `{profile_name}` is unavailable after preflight; authenticate that profile before running the harness"
        )
    })?;
    let account_id = profile.account_id.as_deref().ok_or_else(|| {
        format!(
            "credential profile `{profile_name}` has no server-issued account_id; log in again before running owner-scoped harness verification"
        )
    })?;
    let local_owner_id =
        astra_credentials::local_profile_owner_id(&profile_name, Some(account_id))?;
    let local_owner_scope = astra_services::OwnerScope::user(local_owner_id)?;
    let server_owner_scope = astra_services::OwnerScope::user(account_id)?;
    let mut artifact_owner_scopes = vec![local_owner_scope.clone()];
    if server_owner_scope != local_owner_scope {
        artifact_owner_scopes.push(server_owner_scope);
    }
    Ok(RunnerProfileIdentity {
        profile_name,
        local_owner_scope,
        artifact_owner_scopes,
    })
}

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
    /// Number of provider LLM round-trips from the typed terminal summary,
    /// with step-event evidence used when the terminal envelope is absent.
    pub turn_rounds: u32,
    /// Number of tool calls that hit the idempotency cache.
    pub cache_hits: u32,
    /// Total tool calls (for computing cache rate = cache_hits / total).
    pub total_tool_calls: u32,
    /// Time to first token in ms (from JSON envelope).
    pub ttft_ms: u64,
    /// Machine-readable terminal state from `astra chat --json`.
    #[serde(default)]
    pub final_state: Option<String>,
    /// Interruption kind label when final_state is interrupted.
    #[serde(default)]
    pub interruption_kind: Option<String>,
    /// Counts of tool result classes observed during the run.
    #[serde(default)]
    pub tool_result_class_counts: std::collections::BTreeMap<String, u32>,
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

    pub fn with_final_state(mut self, state: impl Into<String>) -> Self {
        self.final_state = Some(state.into());
        self
    }

    pub fn with_interruption_kind(mut self, kind: impl Into<String>) -> Self {
        self.interruption_kind = Some(kind.into());
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
    /// Authorized namespaces containing artifacts for this run. A remote run
    /// normally has a profile-scoped CLI journal and account-scoped server
    /// step events.
    pub artifact_owner_scopes: Vec<astra_services::OwnerScope>,
    /// Require every run to prove that typed asynchronous subsystem work
    /// remained healthy using its durable session journal.
    pub require_session_subsystem_health: bool,
    /// Maximum time to wait for the server's durable asynchronous settlement
    /// marker after the visible chat process exits.
    pub session_settle_timeout: std::time::Duration,
}

impl RunnerConfig {
    pub fn new(astra_bin: impl Into<PathBuf>) -> Self {
        Self {
            astra_bin: astra_bin.into(),
            fallback_models: Vec::new(),
            working_dir: None,
            profile: None,
            artifact_owner_scopes: Vec::new(),
            require_session_subsystem_health: false,
            session_settle_timeout: std::time::Duration::ZERO,
        }
    }
    pub fn with_fallback_models(mut self, models: Vec<String>) -> Self {
        self.fallback_models = models;
        self
    }

    pub fn with_required_session_subsystem_health(mut self) -> Self {
        self.require_session_subsystem_health = true;
        // A session owns one active extraction plus one bounded latest-wins
        // refresh. Each provider attempt has a 30-second deadline and the
        // runtime resets its drain window only after observable handoff to the
        // queued refresh. The harness watches the complete bounded contract;
        // it must not label healthy sequential progress as data loss.
        self.session_settle_timeout = std::time::Duration::from_secs(75);
        self
    }
}

/// Parse astra's `--json` stdout into a RunOutcome skeleton.
/// The executor fills the real process status and stderr afterward. Empty
/// stdout is deliberately left marker-free so an early non-zero process exit
/// remains classifiable; non-empty malformed/invalid envelopes carry
/// [`PROTOCOL_ERROR_MARKER`] and fail closed regardless of process status.
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
                     ({parse_err}); recording protocol failure. Preview: {preview:?}"
                );
                return invalid_json_envelope(
                    model,
                    trimmed,
                    &format!("stdout is not valid JSON: {parse_err}"),
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
                final_state: None,
                interruption_kind: None,
                tool_result_class_counts: Default::default(),
            };
        }
    };

    // A syntactically valid JSON object is not necessarily an Astra outcome.
    // Do not default missing protocol fields to zero: an executor that emits
    // `{}` must not satisfy an ExitCode(0) case merely because its process
    // happened to exit successfully. The CLI's headless contract always
    // carries these typed terminal fields.
    let Some(object) = v.as_object() else {
        return invalid_json_envelope(model, trimmed, "top-level value must be an object");
    };
    let has_text = object.get("text").is_some_and(serde_json::Value::is_string);
    let has_exit_code = object
        .get("exit_code")
        .is_some_and(serde_json::Value::is_i64);
    let has_tool_count = object
        .get("tool_calls_count")
        .is_some_and(serde_json::Value::is_u64);
    let has_tools = object
        .get("tools_used")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|tools| tools.iter().all(serde_json::Value::is_string));
    let has_completion_tokens = object
        .get("completion_tokens")
        .is_some_and(serde_json::Value::is_u64);
    let has_prompt_tokens = ["fresh_prompt_tokens", "prompt_tokens"]
        .iter()
        .any(|key| object.get(*key).is_some_and(serde_json::Value::is_u64));
    if !(has_text
        && has_exit_code
        && has_tool_count
        && has_tools
        && has_completion_tokens
        && has_prompt_tokens)
    {
        return invalid_json_envelope(
            model,
            trimmed,
            "missing or mistyped required fields: text:string, exit_code:integer, \
             tool_calls_count:integer, tools_used:array<string>, completion_tokens:integer, \
             fresh_prompt_tokens|prompt_tokens:integer",
        );
    }

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
            .and_then(|x| x.as_u64())
            .or_else(|| v.get("prompt_tokens").and_then(|x| x.as_u64()))
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
        turn_rounds: v
            .get("llm_rounds")
            .and_then(|x| x.as_u64())
            .unwrap_or(0)
            .min(u64::from(u32::MAX)) as u32,
        cache_hits: 0,
        total_tool_calls: 0,
        ttft_ms: v.get("ttft_ms").and_then(|x| x.as_u64()).unwrap_or(0),
        final_state: v
            .get("final_state")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        interruption_kind: v
            .get("interruption_kind")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        tool_result_class_counts: v
            .get("tool_result_class_counts")
            .and_then(|x| x.as_object())
            .map(|map| {
                map.iter()
                    .filter_map(|(key, value)| {
                        value
                            .as_u64()
                            .map(|count| (key.clone(), count.min(u32::MAX as u64) as u32))
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// Parse the current CLI terminal envelope at a trust boundary. The loose
/// field extraction above remains useful for rendering malformed evidence,
/// but no executor, preflight probe, or dashboard path may certify it.
pub(crate) fn parse_strict_cli_outcome(stdout: &str, model: &str) -> Result<RunOutcome, String> {
    let value: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("stdout is not valid JSON: {error}"))?;
    let object = value
        .as_object()
        .ok_or("terminal outcome must be a JSON object")?;
    let is_string_or_null = |key: &str| {
        object
            .get(key)
            .is_some_and(|value| value.is_null() || value.is_string())
    };
    let has_string_or_null = [
        "trace_id",
        "request_id",
        "run_id",
        "session_id",
        "interruption_kind",
        "persistence_error",
        "error_kind",
    ]
    .into_iter()
    .all(is_string_or_null);
    let cache = object
        .get("cache")
        .and_then(serde_json::Value::as_object)
        .ok_or("terminal outcome missing object 'cache'")?;
    let cache_valid = cache.get("hit").is_some_and(serde_json::Value::is_boolean)
        && cache
            .get("read_tokens")
            .is_some_and(serde_json::Value::is_u64)
        && cache
            .get("creation_tokens")
            .is_some_and(serde_json::Value::is_u64);
    let class_counts_valid = object
        .get("tool_result_class_counts")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|counts| counts.values().all(serde_json::Value::is_u64));
    let llm_rounds_valid = object
        .get("llm_rounds")
        .is_some_and(|value| value.is_null() || value.is_u64());
    let fields_valid = object.get("text").is_some_and(serde_json::Value::is_string)
        && object
            .get("final_state")
            .is_some_and(serde_json::Value::is_string)
        && object
            .get("interruption_kind")
            .is_some_and(|value| value.is_null() || value.is_string())
        && object
            .get("tool_calls_count")
            .is_some_and(serde_json::Value::is_u64)
        && object
            .get("tools_used")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|tools| tools.iter().all(serde_json::Value::is_string))
        && object
            .get("completion_tokens")
            .is_some_and(serde_json::Value::is_u64)
        && object
            .get("prompt_tokens")
            .is_some_and(serde_json::Value::is_u64)
        && object
            .get("fresh_prompt_tokens")
            .is_some_and(serde_json::Value::is_u64)
        && object
            .get("exit_code")
            .is_some_and(serde_json::Value::is_i64)
        && object
            .get("success")
            .is_some_and(serde_json::Value::is_boolean)
        && has_string_or_null
        && cache_valid
        && class_counts_valid
        && llm_rounds_valid;
    if !fields_valid {
        return Err(
            "terminal outcome missing or mistyping required typed fields: \
             trace_id/request_id/run_id/session_id, text, final_state, interruption_kind, \
             tool_result_class_counts, prompt_tokens/fresh_prompt_tokens, cache, \
             completion_tokens, llm_rounds, tool_calls_count, tools_used, exit_code, \
             success, persistence_error, error_kind"
                .to_string(),
        );
    }

    let exit_code = object["exit_code"]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok())
        .ok_or("terminal outcome exit_code is outside i32 range")?;
    if exit_code < 0 {
        return Err("terminal outcome exit_code must be non-negative".to_string());
    }
    let success = object["success"]
        .as_bool()
        .expect("validated success boolean");
    if success != (exit_code == 0) {
        return Err(format!(
            "terminal outcome success={} disagrees with exit_code={exit_code}",
            success
        ));
    }
    let final_state = object["final_state"]
        .as_str()
        .expect("validated final_state string");
    if !matches!(final_state, "" | "completed" | "interrupted") {
        return Err(format!(
            "terminal outcome final_state has unknown value {final_state:?}"
        ));
    }
    if success && final_state != "completed" {
        return Err(format!(
            "successful terminal outcome must have final_state=completed, got {final_state:?}"
        ));
    }
    if success && !object["error_kind"].is_null() {
        return Err("successful terminal outcome must not carry error_kind".to_string());
    }
    if success && !object["persistence_error"].is_null() {
        return Err("successful terminal outcome must not carry persistence_error".to_string());
    }
    if !success
        && object["error_kind"]
            .as_str()
            .is_none_or(|kind| kind.trim().is_empty())
    {
        return Err("failed terminal outcome must carry a non-empty error_kind".to_string());
    }
    if object["interruption_kind"].is_string() && final_state != "interrupted" {
        return Err("interruption_kind requires final_state=interrupted".to_string());
    }
    if final_state == "interrupted" && object["interruption_kind"].is_null() {
        return Err("interrupted terminal outcome is missing interruption_kind".to_string());
    }
    if let Some(session_id) = object["session_id"].as_str()
        && !crate::session_identity::is_valid_server_session_id(session_id)
    {
        return Err("terminal outcome session_id is not a server UUID".to_string());
    }
    if success && object["session_id"].is_null() {
        return Err("successful terminal outcome is missing session_id".to_string());
    }
    if success
        && object["run_id"]
            .as_str()
            .is_none_or(|run_id| run_id.trim().is_empty())
    {
        return Err("successful terminal outcome is missing run_id".to_string());
    }
    if let Some(run_id) = object["run_id"].as_str()
        && run_id.trim().is_empty()
    {
        return Err("terminal outcome run_id must not be empty".to_string());
    }
    let outcome = parse_json_outcome(stdout, model);
    if outcome.stderr.starts_with(PROTOCOL_ERROR_MARKER) {
        return Err(outcome.text);
    }
    Ok(outcome)
}

/// Reconcile the producer-declared exit code with the OS process status. A
/// valid non-empty envelope must agree with the process; otherwise the
/// evidence is a protocol failure and can never satisfy an exit criterion.
pub(crate) fn reconcile_process_exit(
    mut outcome: RunOutcome,
    stdout: &str,
    process_exit: i32,
) -> RunOutcome {
    let protocol_error = outcome.stderr.starts_with(PROTOCOL_ERROR_MARKER);
    let empty_stdout_success = stdout.trim().is_empty() && process_exit == 0;
    if protocol_error || empty_stdout_success {
        outcome.exit_code = -1;
    } else if !stdout.trim().is_empty() && outcome.exit_code != process_exit {
        let declared = outcome.exit_code;
        outcome.exit_code = -1;
        outcome.stderr = PROTOCOL_ERROR_MARKER.into();
        outcome.text = format!(
            "invalid terminal outcome: envelope exit_code {declared} disagrees with process exit {process_exit}"
        );
    } else {
        outcome.exit_code = process_exit;
    }
    outcome
}

fn invalid_json_envelope(model: &str, payload: &str, reason: &str) -> RunOutcome {
    let preview: String = payload.chars().take(512).collect();
    eprintln!(
        "[astra-test] WARNING: stdout from astra chat --json violated the outcome envelope: {reason}"
    );
    RunOutcome {
        model: model.into(),
        exit_code: -1,
        text: format!("invalid JSON outcome envelope: {reason}; payload={preview}"),
        stderr: PROTOCOL_ERROR_MARKER.into(),
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
        final_state: None,
        interruption_kind: None,
        tool_result_class_counts: Default::default(),
    }
}

/// Which models to run a case on: case-level override if set,
/// else config fallback list. Errors when both are empty.
pub fn resolve_models(case: &Case, cfg: &RunnerConfig) -> Result<Vec<String>, anyhow::Error> {
    let selected = match &case.models {
        Some(m) => {
            if m.is_empty() {
                return Err(anyhow::anyhow!(
                    "case {:?} has an explicitly empty models list",
                    case.name
                ));
            }
            m
        }
        None => {
            if cfg.fallback_models.is_empty() {
                return Err(anyhow::anyhow!(
                    "case {:?} has no `models:` list and --models CLI flag was not set",
                    case.name
                ));
            } else {
                &cfg.fallback_models
            }
        }
    };
    crate::case::canonicalize_model_ids(selected).map_err(anyhow::Error::msg)
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
            "llm_rounds": 3,
            "cache": {"read_tokens": 7, "creation_tokens": 3}
        }"#;
        let out = parse_json_outcome(stdout, "m");
        assert_eq!(out.model, "m");
        assert_eq!(out.session_id.as_deref(), Some("s1"));
        assert_eq!(out.tool_calls_count, 2);
        assert_eq!(out.tools_used, vec!["a", "b"]);
        assert_eq!(out.prompt_tokens, 20);
        assert_eq!(out.turn_rounds, 3);
        assert_eq!(out.cached_input_tokens, 7);
        assert_eq!(out.cache_creation_tokens, 3);
    }

    #[test]
    fn parse_json_outcome_uses_prompt_tokens_when_fresh_bucket_missing() {
        let stdout = r#"{
            "text": "legacy",
            "exit_code": 0,
            "tool_calls_count": 0,
            "tools_used": [],
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
        assert!(out.text.contains("not json"));
        assert_eq!(out.tool_calls_count, 0);
        assert_eq!(
            out.exit_code, -1,
            "fallback must signal anomaly via exit_code=-1"
        );
    }

    #[test]
    fn parse_json_outcome_rejects_valid_json_with_missing_protocol_fields() {
        let out = parse_json_outcome("{}", "m");
        assert_eq!(out.exit_code, -1);
        assert!(out.text.contains("invalid JSON outcome envelope"));

        let out = parse_json_outcome(
            r#"{"text":"ok","exit_code":0,"tool_calls_count":0,"tools_used":[],"completion_tokens":0,"prompt_tokens":"zero"}"#,
            "m",
        );
        assert_eq!(out.exit_code, -1);
        assert!(out.text.contains("prompt_tokens:integer"));
    }

    #[test]
    fn parse_json_outcome_rejects_mistyped_tools_used_and_preserves_valid_prompt_alias() {
        let out = parse_json_outcome(
            r#"{"text":"ok","exit_code":0,"tool_calls_count":0,"tools_used":["Read",1],"completion_tokens":0,"prompt_tokens":0}"#,
            "m",
        );
        assert_eq!(out.exit_code, -1);
        assert!(out.text.contains("tools_used:array<string>"));

        let out = parse_json_outcome(
            r#"{"text":"ok","exit_code":0,"tool_calls_count":0,"tools_used":[],"completion_tokens":0,"fresh_prompt_tokens":"bad","prompt_tokens":9}"#,
            "m",
        );
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.prompt_tokens, 9);
    }

    #[test]
    fn parse_json_outcome_merges_background_agent_results() {
        let stdout = r#"{
            "text": "parent output",
            "exit_code": 0,
            "tool_calls_count": 1,
            "tools_used": ["spawn_agent"],
            "completion_tokens": 0,
            "prompt_tokens": 0,
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

    fn strict_outcome_fixture() -> serde_json::Value {
        serde_json::json!({
            "trace_id": null,
            "request_id": null,
            "run_id": "run-1",
            "session_id": "550e8400-e29b-41d4-a716-446655440000",
            "text": "ok",
            "final_state": "completed",
            "interruption_kind": null,
            "tool_result_class_counts": {},
            "prompt_tokens": 0,
            "fresh_prompt_tokens": 0,
            "cache": {"hit": false, "read_tokens": 0, "creation_tokens": 0},
            "completion_tokens": 0,
            "llm_rounds": 0,
            "tool_calls_count": 0,
            "tools_used": [],
            "persistence_error": null,
            "exit_code": 0,
            "success": true,
            "error_kind": null
        })
    }

    #[test]
    fn strict_terminal_validator_rejects_missing_and_contradictory_fields() {
        let valid = strict_outcome_fixture().to_string();
        assert!(parse_strict_cli_outcome(&valid, "m").is_ok());

        let mut missing = strict_outcome_fixture();
        missing.as_object_mut().unwrap().remove("success");
        assert!(parse_strict_cli_outcome(&missing.to_string(), "m").is_err());

        let mut contradictory = strict_outcome_fixture();
        contradictory["success"] = serde_json::json!(false);
        assert!(parse_strict_cli_outcome(&contradictory.to_string(), "m").is_err());

        let mut persistence_contradiction = strict_outcome_fixture();
        persistence_contradiction["persistence_error"] = serde_json::json!("write failed");
        assert!(parse_strict_cli_outcome(&persistence_contradiction.to_string(), "m").is_err());
    }

    #[test]
    fn process_exit_mismatch_is_protocol_failure_in_both_directions() {
        let outcome = RunOutcome {
            exit_code: 0,
            ..RunOutcome::new("m")
        };
        let forward = reconcile_process_exit(outcome.clone(), "{}", 42);
        assert_eq!(forward.exit_code, -1);
        assert!(forward.text.contains("disagrees"));

        let reverse = reconcile_process_exit(
            RunOutcome {
                exit_code: 42,
                ..RunOutcome::new("m")
            },
            "{}",
            0,
        );
        assert_eq!(reverse.exit_code, -1);
        assert!(reverse.text.contains("disagrees"));
    }

    #[test]
    fn resolve_models_uses_case_override() {
        let case = Case {
            name: "c".into(),
            description: None,
            prompt: "p".into(),
            prompt_variants: vec![],
            models: Some(vec!["opus".into()]),
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
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
            prompt_variants: vec![],
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
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
            prompt_variants: vec![],
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        };
        let cfg = RunnerConfig::new("astra");
        assert!(resolve_models(&case, &cfg).is_err());
    }

    #[test]
    fn resolve_models_rejects_blank_and_duplicate_matrix_ids() {
        let mut case = Case {
            name: "c".into(),
            description: None,
            prompt: "p".into(),
            prompt_variants: vec![],
            models: None,
            criteria: vec![],
            debug_log: false,
            extra_cli_args: vec![],
            timeout_seconds: 180,
            capability: None,
            required_cache_scope: None,
            difficulty: None,
            weight: 1.0,
            steps: vec![],
            cli_env: std::collections::HashMap::new(),
            setup_cmd: None,
            teardown_cmd: None,
            cleanup_memory_records: false,
        };
        case.models = Some(vec!["m".into(), "m".into()]);
        let cfg = RunnerConfig::new("astra");
        assert!(resolve_models(&case, &cfg).is_err());
        case.models = Some(vec!["  ".into()]);
        assert!(resolve_models(&case, &cfg).is_err());
        case.models = Some(vec![" m ".into()]);
        assert!(resolve_models(&case, &cfg).is_err());
    }
}
