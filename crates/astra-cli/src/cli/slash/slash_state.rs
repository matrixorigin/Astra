use crate::cli::{
    cli_config::cli_utils::{map_thin_err, urlencoding},
    session::session_state::SessionState,
};
/// Create a fresh server session without publishing it as the profile's
/// resumable session yet. The identity is provisional until its first turn is
/// durably admitted; this prevents an admission failure from making an empty
/// draft the next process's implicit recovery target.
async fn create_server_session_identity(
    api: &astra_thin_client::ThinClient,
    token: &str,
) -> Result<String, String> {
    let body = api
        .post_sessions_json(token, &serde_json::json!({}))
        .await
        .map_err(map_thin_err)?;
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let session_id = value
        .get("session_id")
        .or_else(|| value.get("id"))
        .and_then(|value| value.as_str())
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| "session service returned no session identity".to_string())?
        .to_string();

    Ok(session_id)
}

/// Bind the first authoritative identity to an otherwise sessionless runtime.
///
/// Startup may already have installed long-lived producers (notably the
/// dynamic-agent spawner) against the pristine session-scoped registries.
/// Initial identity discovery is not a session transition, so rotating those
/// registries here would split producers from their consumers. Actual session
/// transitions continue to go through [`start_fresh_session`].
pub(crate) async fn bind_initial_session(
    api: &astra_thin_client::ThinClient,
    _profile: Option<&str>,
    token: &str,
    state: &mut SessionState,
) -> Result<String, String> {
    if state.session_id.is_some()
        || state.run_id.is_some()
        || state.turn != 0
        || !state.history.is_empty()
    {
        return Err("initial session identity requires a pristine sessionless runtime".to_string());
    }
    let (config, version) =
        crate::cli::session::session_startup::prepare_session_runtime_config(state, None)?;
    let session_id = create_server_session_identity(api, token).await?;
    state.set_session_id(session_id.clone());
    crate::cli::session::session_startup::apply_session_runtime_config(state, config, version);
    crate::cli::session::session_startup::initialize_journal_pub(state, &session_id);
    Ok(session_id)
}

pub(crate) async fn start_fresh_session(
    api: &astra_thin_client::ThinClient,
    _profile: Option<&str>,
    token: &str,
    state: &mut SessionState,
) -> Result<String, String> {
    let (config, version) =
        crate::cli::session::session_startup::prepare_session_runtime_config(state, None)?;
    let session_id = create_server_session_identity(api, token).await?;
    state.prepare_for_session_rebind().await;
    state.reset_for_new_session();
    state.set_session_id(session_id.clone());
    crate::cli::session::session_startup::apply_session_runtime_config(state, config, version);
    crate::cli::session::session_startup::initialize_journal_pub(state, &session_id);
    Ok(session_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedReflectArgs {
    topic: String,
    facet: Option<String>,
    depth: String,
    question: Option<String>,
    diff: bool,
}

impl ParsedReflectArgs {
    fn request(&self, last_n: i32) -> astra_services::reflect::ReflectRequest {
        astra_services::reflect::ReflectRequest::from_observation_params(
            Some(self.topic.as_str()),
            self.facet.as_deref(),
            Some(self.depth.as_str()),
            None,
            last_n,
            self.question.as_deref().unwrap_or(""),
        )
    }
}

/// Authoritative source used to build a user-visible reflection. The source is
/// part of the result because an equivalent-looking report from a local journal
/// versus the server has different freshness and recovery guarantees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReflectEvidenceSource {
    LocalArtifacts,
    Server,
}

/// A read-only reflection result. It deliberately contains no apply/mutation
/// action: plan, task, memory, and permission changes remain separate typed
/// workflows that can consume its evidence later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReflectSurface {
    Diff {
        body: String,
    },
    Report {
        session_id: String,
        source: ReflectEvidenceSource,
        body: String,
    },
}

/// Load a reflection through the same fallback order in every interactive
/// surface: local canonical artifacts first, then the durable server only when
/// local state is absent. Local corruption remains an error instead of being
/// hidden by a remote result.
pub(crate) async fn load_reflect_surface(
    state: &SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
    arg: &str,
) -> Result<ReflectSurface, String> {
    let session_id = state
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.trim().is_empty())
        .ok_or_else(|| "Reflect needs an active session.".to_string())?
        .to_string();
    let reflect_args = parse_reflect_args(arg);

    if reflect_args.diff {
        return Ok(ReflectSurface::Diff {
            body: render_reflect_diff(state),
        });
    }

    load_reflect_report_for_session_with_args(&session_id, api, profile, token, reflect_args).await
}

/// Whether a reflection request is the local, immediately available diff
/// variant. The workbench uses this to keep purely local inspection on the
/// input path while scheduling report retrieval as a background effect.
pub(crate) fn is_reflect_diff_request(arg: &str) -> bool {
    parse_reflect_args(arg).diff
}

/// Load a read-only reflection report for an already captured session
/// identity. This keeps local-artifact-first fallback identical across CLI,
/// Server Only and Edge+Server without retaining a mutable `SessionState`
/// borrow while filesystem or server evidence is pending.
pub(crate) async fn load_reflect_report_for_session(
    session_id: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
    arg: &str,
) -> Result<ReflectSurface, String> {
    let reflect_args = parse_reflect_args(arg);
    if reflect_args.diff {
        return Err("Reflection diff needs the live session state.".into());
    }
    load_reflect_report_for_session_with_args(session_id, api, profile, token, reflect_args).await
}

async fn load_reflect_report_for_session_with_args(
    session_id: &str,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    token: Option<&str>,
    reflect_args: ParsedReflectArgs,
) -> Result<ReflectSurface, String> {
    if let Some(body) =
        crate::cli::self_command::try_render_reflect_surface_for_session_with_profile(
            session_id,
            reflect_args.request(20),
            profile,
        )
        .await?
    {
        return Ok(ReflectSurface::Report {
            session_id: session_id.to_string(),
            source: ReflectEvidenceSource::LocalArtifacts,
            body,
        });
    }

    let token = token.ok_or_else(|| {
        "Reflect needs local session artifacts or a logged-in server session.".to_string()
    })?;
    let rel = reflect_request_path(session_id, &reflect_args);
    let body = api
        .get_authed_path_text(token, &rel)
        .await
        .map_err(|error| format!("Reflect failed: {error}"))?;
    Ok(ReflectSurface::Report {
        session_id: session_id.to_string(),
        source: ReflectEvidenceSource::Server,
        body,
    })
}

fn reflect_request_path(session_id: &str, reflect_args: &ParsedReflectArgs) -> String {
    let mut rel = astra_thin_client::paths::chat_session_reflect(session_id)
        .trim_start_matches('/')
        .to_string();
    let mut query_parts: Vec<String> = Vec::new();
    if reflect_args.topic != "overview" {
        query_parts.push(format!("topic={}", urlencoding(&reflect_args.topic)));
    }
    if let Some(facet) = reflect_args.facet.as_deref() {
        query_parts.push(format!("facet={}", urlencoding(facet)));
    }
    if reflect_args.depth != "diagnostic" {
        query_parts.push(format!("depth={}", urlencoding(&reflect_args.depth)));
    }
    if let Some(question) = reflect_args
        .question
        .as_deref()
        .filter(|question| !question.is_empty())
    {
        query_parts.push(format!("question={}", urlencoding(question)));
    }
    if !query_parts.is_empty() {
        rel = format!("{rel}?{}", query_parts.join("&"));
    }
    rel
}

fn parse_reflect_args(arg: &str) -> ParsedReflectArgs {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return ParsedReflectArgs {
            topic: "overview".to_string(),
            facet: None,
            depth: "diagnostic".to_string(),
            question: None,
            diff: false,
        };
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let first = normalize_reflect_token(tokens[0]);
    if first == "diff" {
        return ParsedReflectArgs {
            topic: "overview".to_string(),
            facet: None,
            depth: "diagnostic".to_string(),
            question: None,
            diff: true,
        };
    }

    let mut topic = "overview".to_string();
    let mut facet = None;
    let mut depth = "diagnostic".to_string();
    let mut question_start = 0usize;
    let mut parsed_topic = false;

    if let Some((head, tail)) = first.split_once('/')
        && is_reflect_topic(head)
    {
        topic = head.to_string();
        parsed_topic = true;
        if !tail.is_empty() {
            facet = Some(tail.to_string());
        }
        question_start = 1;
    } else if is_reflect_topic(&first) {
        topic = first.clone();
        parsed_topic = true;
        question_start = 1;
    }

    if parsed_topic && facet.is_none() && question_start < tokens.len() {
        let candidate = normalize_reflect_token(tokens[question_start]);
        if is_reflect_facet(&candidate) {
            facet = Some(candidate);
            question_start += 1;
        }
    }

    if question_start < tokens.len() {
        let candidate = normalize_reflect_token(tokens[question_start]);
        if is_reflect_depth(&candidate) {
            depth = candidate;
            question_start += 1;
        }
    } else if !parsed_topic && is_reflect_depth(&first) {
        depth = first;
        question_start = 1;
    }

    let question = (question_start < tokens.len()).then(|| tokens[question_start..].join(" "));

    ParsedReflectArgs {
        topic,
        facet,
        depth,
        question,
        diff: false,
    }
}

fn normalize_reflect_token(token: &str) -> String {
    token.trim().to_ascii_lowercase().replace('-', "_")
}

fn is_reflect_topic(token: &str) -> bool {
    matches!(token, "overview" | "runtime" | "execution" | "knowledge")
}

fn is_reflect_depth(token: &str) -> bool {
    matches!(token, "hint" | "summary" | "diagnostic" | "forensic")
}

fn is_reflect_facet(token: &str) -> bool {
    matches!(
        token,
        "overview"
            | "summary"
            | "question"
            | "errors"
            | "failures"
            | "tools"
            | "trace"
            | "performance"
            | "latency"
            | "cost"
            | "context"
            | "memory"
            | "progress"
            | "loop"
            | "cache"
    )
}

/// Render a compact diff view of what the agent has learned this session
/// vs the last cloud-synced baseline. Auto-populated — reads directly
/// from `SessionState` without any new plumbing.
///
/// Output enumerates tool-health entries whose execution failure rate, call
/// count, input-validation rejects, or presence changed since last sync.
/// When nothing changed (e.g. fresh session) the output is an explicit
/// "no delta" line.
pub(crate) fn render_reflect_diff(state: &SessionState) -> String {
    use std::collections::HashMap;
    use std::fmt::Write;

    let mut out = String::new();
    let sep = "─".repeat(38);
    let _ = writeln!(out, "\n  ─── reflect diff {sep}");

    let synced: HashMap<&str, &astra_turn_core::tool_health_persistence::ToolHealthEntry> = state
        .synced_tool_health_entries
        .iter()
        .map(|e| (e.name.as_str(), e))
        .collect();

    let mut rows: Vec<String> = Vec::new();
    for cur in &state.tool_health_entries {
        match synced.get(cur.name.as_str()) {
            None => {
                rows.push(format!(
                    "  + {name:20}  new · {calls} calls · {rate:.0}% fail · {invalid} input rejects",
                    name = cur.name,
                    calls = cur.total_calls,
                    rate = cur.failure_rate * 100.0,
                    invalid = cur.input_validation_failures,
                ));
            }
            Some(prev) => {
                let rate_delta = cur.failure_rate - prev.failure_rate;
                let call_delta = cur.total_calls as i64 - prev.total_calls as i64;
                let invalid_delta =
                    cur.input_validation_failures as i64 - prev.input_validation_failures as i64;
                if call_delta == 0 && rate_delta.abs() < 0.005 && invalid_delta == 0 {
                    continue;
                }
                let sign = if rate_delta >= 0.0 { "+" } else { "" };
                rows.push(format!(
                    "  ~ {name:20}  Δcalls {call_delta:+} · Δfail {sign}{rate:.0}% (now {now:.0}%) · Δinput {invalid_delta:+}",
                    name = cur.name,
                    rate = rate_delta * 100.0,
                    now = cur.failure_rate * 100.0,
                    invalid_delta = invalid_delta,
                ));
            }
        }
    }

    if rows.is_empty() {
        let _ = writeln!(
            out,
            "  no delta since last sync · {} tools tracked",
            state.tool_health_entries.len()
        );
    } else {
        for row in rows {
            let _ = writeln!(out, "{row}");
        }
    }
    out
}

/// Decode the current reflection contract. Callers retain this typed payload
/// until presentation so evidence, scope, and advisory proposals cannot be
/// mistaken for a flat diagnostic text blob.
pub(crate) fn parse_reflection_report(
    body: &str,
) -> Result<astra_services::reflect::ReflectReport, String> {
    serde_json::from_str(body)
        .map_err(|error| format!("Reflection returned an invalid typed report: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{parse_reflect_args, parse_reflection_report, render_reflect_diff};

    #[test]
    fn parse_reflect_args_recognises_diff_branch() {
        let args = parse_reflect_args("diff");
        assert!(args.diff);
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question, None);
    }

    #[test]
    fn reflection_parser_preserves_typed_evidence_and_advisory_proposals() {
        let body = serde_json::json!({
            "schema_version": 1,
            "tool": "reflect",
            "session_id": "74eff903-30bb-45df-b36d-5826f1a4638c",
            "analysis_view": "overview",
            "topic": "execution",
            "facet": "errors",
            "depth": "diagnostic",
            "horizon": "session",
            "source_policy": "auto",
            "include_context": false,
            "data_coverage": {
                "overall": "fresh",
                "source": "session_journal",
                "events": 9,
                "decisions": 2
            },
            "summary": "One repeated failure is supported by local evidence.",
            "model_requests": astra_services::reflect::ModelRequestCapture::default(),
            "observations": [{
                "ref_id": "urn:astra:observation:local:reflect:session:diagnosis:0",
                "topic": "execution",
                "facet": "errors",
                "kind": "diagnosis:permission",
                "severity": "warning",
                "summary": "Repeated command failure",
                "confidence": {"evidence": 0.9},
                "evidence_refs": ["urn:astra:artifact:local:reflect:session:sample:0"]
            }],
            "evidence": [{
                "ref_id": "urn:astra:artifact:local:reflect:session:sample:0",
                "evidence_class": "observed_evidence",
                "source": "session_journal",
                "summary": "permission denied",
                "confidence": {"evidence": 0.9}
            }],
            "action_hints": [{
                "target_type": "user_guidance",
                "summary": "Narrow the command scope",
                "confidence": {"evidence": 0.9},
                "observation_refs": ["urn:astra:observation:local:reflect:session:diagnosis:0"]
            }]
        })
        .to_string();

        let report = parse_reflection_report(&body).expect("current reflect payload");
        assert_eq!(report.data_coverage.events, 9);
        assert_eq!(
            report.observations[0].evidence_refs[0],
            report.evidence[0].ref_id
        );
        assert_eq!(report.evidence[0].source, "session_journal");
        assert_eq!(report.action_hints[0].target_type, "user_guidance");
        assert_eq!(
            report.action_hints[0].observation_refs[0],
            report.observations[0].ref_id
        );
    }

    #[test]
    fn render_reflect_diff_reports_no_delta_on_fresh_session() {
        let state = crate::cli::session::session_state::SessionState::default();
        let out = render_reflect_diff(&state);
        assert!(out.contains("reflect diff"), "header present: {out}");
        assert!(
            out.contains("no delta since last sync"),
            "fresh session should say no delta: {out}"
        );
    }

    #[test]
    fn render_reflect_diff_surfaces_new_and_drifting_tools() {
        use astra_turn_core::tool_health_persistence::ToolHealthEntry;
        let mut state = crate::cli::session::session_state::SessionState::default();
        // Baseline had "grep" at 10 calls / 10% fail.
        state.synced_tool_health_entries = vec![ToolHealthEntry {
            name: "grep".into(),
            total_calls: 10,
            total_failures: 1,
            input_validation_failures: 0,
            failure_rate: 0.10,
            last_updated_epoch: 0,
            recent_outcomes: vec![],
        }];
        // Now grep has drifted up, and "glob" is new.
        state.tool_health_entries = vec![
            ToolHealthEntry {
                name: "grep".into(),
                total_calls: 14,
                total_failures: 5,
                input_validation_failures: 2,
                failure_rate: 0.36,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
            ToolHealthEntry {
                name: "glob".into(),
                total_calls: 3,
                total_failures: 0,
                input_validation_failures: 0,
                failure_rate: 0.0,
                last_updated_epoch: 0,
                recent_outcomes: vec![],
            },
        ];
        let out = render_reflect_diff(&state);
        assert!(out.contains("grep"), "drifting tool shown: {out}");
        assert!(out.contains("glob"), "new tool shown: {out}");
        assert!(out.contains("new"), "new marker: {out}");
        assert!(out.contains("Δcalls +4"), "grep call delta surfaced: {out}");
        assert!(
            out.contains("Δinput +2"),
            "caller misuse delta surfaced: {out}"
        );
    }

    #[test]
    fn parse_reflect_args_splits_topic_facet_and_question() {
        let args = parse_reflect_args("execution/errors why did bash fail");
        assert!(!args.diff);
        assert_eq!(args.topic, "execution");
        assert_eq!(args.facet.as_deref(), Some("errors"));
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question.as_deref(), Some("why did bash fail"));
    }

    #[test]
    fn parse_reflect_args_accepts_depth_after_topic_facet() {
        let args = parse_reflect_args("execution/trace forensic why did it fail");
        assert_eq!(args.topic, "execution");
        assert_eq!(args.facet.as_deref(), Some("trace"));
        assert_eq!(args.depth, "forensic");
        assert_eq!(args.question.as_deref(), Some("why did it fail"));
    }

    #[test]
    fn parse_reflect_args_accepts_depth_without_topic() {
        let args = parse_reflect_args("summary what happened");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "summary");
        assert_eq!(args.question.as_deref(), Some("what happened"));
    }

    #[test]
    fn parse_reflect_args_accepts_separate_topic_and_facet() {
        let args = parse_reflect_args("runtime performance why was bash slow");
        assert_eq!(args.topic, "runtime");
        assert_eq!(args.facet.as_deref(), Some("performance"));
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question.as_deref(), Some("why was bash slow"));
    }

    #[test]
    fn parse_reflect_args_treats_freeform_as_question() {
        let args = parse_reflect_args("performance why was bash slow");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(
            args.question.as_deref(),
            Some("performance why was bash slow")
        );
    }

    #[test]
    fn parse_reflect_args_does_not_accept_removed_focus_shortcuts() {
        let args = parse_reflect_args("skill_failure why did bash fail");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(
            args.question.as_deref(),
            Some("skill_failure why did bash fail")
        );
    }

    #[test]
    fn parse_reflect_args_does_not_accept_unimplemented_adaptation_topic() {
        let args = parse_reflect_args("adaptation/signals forensic");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(
            args.question.as_deref(),
            Some("adaptation/signals forensic")
        );
    }

    #[test]
    fn parse_reflect_args_empty_defaults_to_overview() {
        let args = parse_reflect_args("");
        assert_eq!(args.topic, "overview");
        assert_eq!(args.facet, None);
        assert_eq!(args.depth, "diagnostic");
        assert_eq!(args.question, None);
    }
}
