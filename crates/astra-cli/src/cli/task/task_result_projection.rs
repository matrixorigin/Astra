use crate::cli::stream::streaming_types::StreamResult;

pub(crate) fn task_checkpoint_state_from_result(
    sr: &StreamResult,
    output_path: Option<&str>,
    exit_code: crate::cli::exit_code::ExitCode,
) -> serde_json::Map<String, serde_json::Value> {
    let mut state_map = serde_json::Map::new();
    state_map.insert(
        "full_text".to_string(),
        serde_json::Value::String(sr.full_text.clone()),
    );
    if let Some(output_path) = output_path {
        state_map.insert(
            "output_file".to_string(),
            serde_json::Value::String(output_path.to_string()),
        );
    }
    state_map.insert("run_id".to_string(), serde_json::json!(sr.run_id));
    state_map.insert(
        "prompt_tokens".to_string(),
        serde_json::json!(sr.prompt_tokens),
    );
    state_map.insert(
        "completion_tokens".to_string(),
        serde_json::json!(sr.completion_tokens),
    );
    state_map.insert(
        "cache_read_tokens".to_string(),
        serde_json::json!(sr.cache_read_tokens),
    );
    state_map.insert(
        "cache_creation_tokens".to_string(),
        serde_json::json!(sr.cache_creation_tokens),
    );
    state_map.insert(
        "tool_calls_count".to_string(),
        serde_json::json!(sr.tool_calls_count),
    );
    state_map.insert(
        "background_agent_results".to_string(),
        serde_json::json!(
            sr.background_agent_results
                .iter()
                .map(|(id, text)| serde_json::json!({"agent_id": id, "result": text}))
                .collect::<Vec<_>>()
        ),
    );
    state_map.insert(
        "persistence_error".to_string(),
        serde_json::json!(sr.session_persistence_error),
    );
    state_map.insert(
        "exit_code".to_string(),
        serde_json::json!(i32::from(exit_code)),
    );
    state_map.insert(
        "error_kind".to_string(),
        serde_json::json!(crate::cli::command_router::error_kind_for_exit_code(
            exit_code
        )),
    );
    state_map.insert("final_state".to_string(), serde_json::json!(sr.final_state));
    state_map.insert(
        "interruption_kind".to_string(),
        serde_json::json!(sr.interruption_kind),
    );
    state_map
}

pub(crate) fn stream_result_completion_outcome(sr: &StreamResult) -> astra_services::TaskOutcome {
    if sr.final_state == "interrupted" {
        astra_services::TaskOutcome::Partial
    } else {
        astra_services::TaskOutcome::Success
    }
}

pub(crate) fn stream_result_exit_code(sr: &StreamResult) -> crate::cli::exit_code::ExitCode {
    // Forced terminal stop always wins over softer failures because the user or
    // turn guard explicitly ended execution.
    if sr.verdict_events.iter().any(|v| v.force_stop) {
        return crate::cli::exit_code::ExitCode::ForceStop;
    }

    // Honor explicit exit semantics when present; a failing shell command like
    // `grep` with no matches is not a task failure, but an execution error is.
    let is_error = |r: &astra_services::session_journal::ToolCallRecord| -> bool {
        match r
            .exit_semantics
            .as_deref()
            .and_then(parse_exit_semantics_tag)
        {
            Some(
                astra_tools::exit_semantics::ExitSemantics::Success
                | astra_tools::exit_semantics::ExitSemantics::EmptyResult
                | astra_tools::exit_semantics::ExitSemantics::DomainNegative
                | astra_tools::exit_semantics::ExitSemantics::PipelineTruncated,
            ) => false,
            None => !r.ok,
            Some(
                astra_tools::exit_semantics::ExitSemantics::TimedOut
                | astra_tools::exit_semantics::ExitSemantics::Cancelled
                | astra_tools::exit_semantics::ExitSemantics::Signaled
                | astra_tools::exit_semantics::ExitSemantics::ExecutionError,
            ) => true,
        }
    };

    let has_any_failure = sr.tool_call_records.iter().any(&is_error);
    if has_any_failure {
        let last_ok = sr
            .tool_call_records
            .iter()
            .rev()
            .find(|r| !is_error(r))
            .is_some();
        let last_ok_explicit = sr
            .tool_call_records
            .last()
            .map(|r| !is_error(r))
            .unwrap_or(true);
        if !last_ok || !last_ok_explicit {
            return crate::cli::exit_code::ExitCode::ToolFailure;
        }
    }

    if sr.session_persistence_error.is_some() {
        return crate::cli::exit_code::ExitCode::PersistenceError;
    }

    if sr.final_state == "interrupted" {
        return crate::cli::exit_code::ExitCode::Partial;
    }

    crate::cli::exit_code::ExitCode::Success
}

pub(crate) fn stream_result_failure_reason(
    exit_code: crate::cli::exit_code::ExitCode,
    sr: &StreamResult,
) -> String {
    if exit_code == crate::cli::exit_code::ExitCode::PersistenceError {
        sr.session_persistence_error
            .clone()
            .unwrap_or_else(|| "session persistence degraded".to_string())
    } else if exit_code == crate::cli::exit_code::ExitCode::Partial {
        sr.interruption_kind
            .clone()
            .map(|kind| format!("turn interrupted before completion ({kind})"))
            .unwrap_or_else(|| "turn interrupted before completion".to_string())
    } else {
        crate::cli::command_router::error_kind_for_exit_code(exit_code)
            .unwrap_or("task failed")
            .to_string()
    }
}

fn parse_exit_semantics_tag(tag: &str) -> Option<astra_tools::exit_semantics::ExitSemantics> {
    serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(serde_json::Value::String(
        tag.to_string(),
    ))
    .ok()
}

#[cfg(test)]
mod tests {
    use super::{
        stream_result_completion_outcome, stream_result_exit_code, stream_result_failure_reason,
        task_checkpoint_state_from_result,
    };
    use crate::cli::exit_code::ExitCode;
    use crate::cli::stream::streaming_types::StreamResult;

    fn interrupted_result() -> StreamResult {
        StreamResult {
            session_id: Some("session-1".into()),
            run_id: Some("run-1".into()),
            session_persistence_error: Some("journal append failed".into()),
            full_text: "partial".into(),
            prompt_tokens: 10,
            completion_tokens: 4,
            cache_read_tokens: 3,
            cache_creation_tokens: 1,
            tool_calls_count: 2,
            final_state: "interrupted".into(),
            interruption_kind: Some("budget_exhausted".into()),
            background_agent_results: vec![("agent-1".into(), "done".into())],
            ..Default::default()
        }
    }

    #[test]
    fn task_checkpoint_state_captures_terminal_stream_metadata() {
        let state = task_checkpoint_state_from_result(
            &interrupted_result(),
            Some("/tmp/out.txt"),
            crate::cli::exit_code::ExitCode::Partial,
        );

        assert_eq!(state["run_id"], "run-1");
        assert_eq!(state["output_file"], "/tmp/out.txt");
        assert_eq!(state["cache_read_tokens"], 3);
        assert_eq!(state["cache_creation_tokens"], 1);
        assert_eq!(state["exit_code"], 5);
        assert_eq!(state["error_kind"], "partial");
        assert_eq!(state["final_state"], "interrupted");
        assert_eq!(state["interruption_kind"], "budget_exhausted");
        assert_eq!(state["persistence_error"], "journal append failed");
    }

    #[test]
    fn interrupted_stream_result_maps_to_partial_outcome() {
        assert_eq!(
            stream_result_completion_outcome(&interrupted_result()),
            astra_services::TaskOutcome::Partial
        );
    }

    #[test]
    fn stream_result_exit_code_prefers_persistence_error_over_partial() {
        let mut result = interrupted_result();
        assert_eq!(stream_result_exit_code(&result), ExitCode::PersistenceError);
        result.session_persistence_error = None;
        assert_eq!(stream_result_exit_code(&result), ExitCode::Partial);
    }

    #[test]
    fn stream_result_exit_code_treats_terminal_exit_semantics_as_tool_failure() {
        for semantics in ["timed_out", "cancelled", "signaled", "execution_error"] {
            let result = StreamResult {
                tool_call_records: vec![astra_services::session_journal::ToolCallRecord {
                    name: "bash".into(),
                    ok: false,
                    exit_semantics: Some(semantics.into()),
                    ..Default::default()
                }],
                ..Default::default()
            };

            assert_eq!(
                stream_result_exit_code(&result),
                ExitCode::ToolFailure,
                "{semantics}"
            );
        }
    }

    #[test]
    fn stream_result_exit_code_treats_non_error_exit_semantics_as_success() {
        for semantics in [
            "success",
            "empty_result",
            "domain_negative",
            "pipeline_truncated",
        ] {
            let result = StreamResult {
                tool_call_records: vec![astra_services::session_journal::ToolCallRecord {
                    name: "bash".into(),
                    ok: false,
                    exit_semantics: Some(semantics.into()),
                    ..Default::default()
                }],
                ..Default::default()
            };

            assert_eq!(
                stream_result_exit_code(&result),
                ExitCode::Success,
                "{semantics}"
            );
        }
    }

    #[test]
    fn stream_result_failure_reason_prefers_persistence_detail() {
        let result = interrupted_result();
        assert_eq!(
            stream_result_failure_reason(ExitCode::PersistenceError, &result),
            "journal append failed"
        );
        assert_eq!(
            stream_result_failure_reason(ExitCode::Partial, &result),
            "turn interrupted before completion (budget_exhausted)"
        );
    }
}
