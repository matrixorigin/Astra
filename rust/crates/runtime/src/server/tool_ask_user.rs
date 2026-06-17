use serde_json::{Value, json};
use uuid::Uuid;

use astra_tools::{
    AskUserAnswers, AskUserGate, AskUserPrompt, ToolProgressCallback,
    build_ask_user_prompt_telemetry, build_ask_user_tool_call_audit, normalize_ask_user_answers,
    parse_ask_user_prompt,
};

pub(crate) struct AskUserExecutionContext<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) gate: Option<&'a dyn AskUserGate>,
    pub(crate) progress_callback: Option<&'a dyn ToolProgressCallback>,
    pub(crate) auxiliary_event_writer: Option<&'a dyn crate::TurnAuxiliaryEventWriter>,
}

pub(crate) async fn execute_ask_user(
    context: AskUserExecutionContext<'_>,
    args: &Value,
) -> astra_tools::ToolResult {
    let prompt = match parse_ask_user_prompt(args) {
        Ok(prompt) => prompt,
        Err(error) => return astra_tools::ToolResult::error(error),
    };

    let Some(gate) = context.gate else {
        return astra_tools::ToolResult::error(
            "Error: ask_user requires an interactive client connection".into(),
        );
    };

    let request_id = format!("ask-{}-{}", context.session_id, Uuid::new_v4());
    let content = ask_user_content_preview(&prompt);
    persist_ask_user_auxiliary_event(
        &context,
        "ask_user_prompted",
        content.clone(),
        request_id.clone(),
        json!({
            "tool_name": "ask_user",
            "request_id": request_id.clone(),
            "ask_user": {
                "prompt": build_ask_user_prompt_telemetry(&prompt),
            },
        }),
    )
    .await;

    match gate.request_questionnaire(&request_id, &prompt).await {
        astra_tools::AskUserDecision::Submitted(submitted) => {
            let answers = match normalize_ask_user_answers(&prompt, &submitted) {
                Ok(answers) => answers,
                Err(error) => {
                    if let Some(cb) = context.progress_callback {
                        cb.ask_user_resolved(&request_id, "error", &[], None, Some(&error))
                            .await;
                    }
                    persist_ask_user_auxiliary_event(
                        &context,
                        "ask_user_error",
                        content,
                        request_id.clone(),
                        json!({
                            "tool_name": "ask_user",
                            "request_id": request_id.clone(),
                            "ask_user": build_ask_user_tool_call_audit(&prompt, "error", None, Some(&error)),
                        }),
                    )
                    .await;
                    return astra_tools::ToolResult::error(error);
                }
            };
            let flattened_answers = flatten_ask_user_answers(&answers);
            let was_custom = Some(ask_user_answers_use_freeform(&prompt, &answers));
            if let Some(cb) = context.progress_callback {
                cb.ask_user_resolved(
                    &request_id,
                    "submitted",
                    &flattened_answers,
                    was_custom,
                    None,
                )
                .await;
            }
            persist_ask_user_auxiliary_event(
                &context,
                "ask_user_submitted",
                content,
                request_id.clone(),
                json!({
                    "tool_name": "ask_user",
                    "request_id": request_id.clone(),
                    "ask_user": build_ask_user_tool_call_audit(
                        &prompt,
                        "submitted",
                        Some(&answers),
                        None,
                    ),
                }),
            )
            .await;
            astra_tools::ToolResult::text(answers.to_tool_result_value().to_string())
        }
        astra_tools::AskUserDecision::Cancelled => {
            let error = "Error: ask_user was cancelled by the user";
            if let Some(cb) = context.progress_callback {
                cb.ask_user_resolved(&request_id, "cancelled", &[], None, Some(error))
                    .await;
            }
            persist_ask_user_auxiliary_event(
                &context,
                "ask_user_cancelled",
                content,
                request_id.clone(),
                json!({
                    "tool_name": "ask_user",
                    "request_id": request_id.clone(),
                    "ask_user": build_ask_user_tool_call_audit(&prompt, "cancelled", None, Some(error)),
                }),
            )
            .await;
            astra_tools::ToolResult::error(error.into())
        }
        astra_tools::AskUserDecision::Timeout => {
            let error = "Error: ask_user timed out waiting for user response";
            if let Some(cb) = context.progress_callback {
                cb.ask_user_resolved(&request_id, "timeout", &[], None, Some(error))
                    .await;
            }
            persist_ask_user_auxiliary_event(
                &context,
                "ask_user_timeout",
                content,
                request_id.clone(),
                json!({
                    "tool_name": "ask_user",
                    "request_id": request_id.clone(),
                    "ask_user": build_ask_user_tool_call_audit(&prompt, "timeout", None, Some(error)),
                }),
            )
            .await;
            astra_tools::ToolResult::error(error.into())
        }
        astra_tools::AskUserDecision::Error(message) => {
            let error = format!("Error: ask_user failed: {message}");
            if let Some(cb) = context.progress_callback {
                cb.ask_user_resolved(&request_id, "error", &[], None, Some(&error))
                    .await;
            }
            persist_ask_user_auxiliary_event(
                &context,
                "ask_user_error",
                content,
                request_id.clone(),
                json!({
                    "tool_name": "ask_user",
                    "request_id": request_id.clone(),
                    "ask_user": build_ask_user_tool_call_audit(&prompt, "error", None, Some(&error)),
                }),
            )
            .await;
            astra_tools::ToolResult::error(format!("ask_user failed: {message}"))
        }
    }
}

fn ask_user_content_preview(prompt: &AskUserPrompt) -> String {
    prompt
        .questions
        .first()
        .map(|question| question.question.clone())
        .unwrap_or_else(|| "ask_user".to_string())
}

fn flatten_ask_user_answers(answers: &AskUserAnswers) -> Vec<String> {
    answers
        .answers
        .iter()
        .flat_map(|answer| answer.answers.iter().cloned())
        .collect()
}

fn ask_user_answers_use_freeform(prompt: &AskUserPrompt, answers: &AskUserAnswers) -> bool {
    answers.answers.iter().any(|answer| {
        prompt
            .questions
            .iter()
            .find(|question| question.question == answer.question)
            .map(|question| {
                let option_labels = question
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect::<std::collections::HashSet<_>>();
                answer
                    .answers
                    .iter()
                    .any(|item| !option_labels.contains(item.as_str()))
            })
            .unwrap_or(false)
    })
}

async fn persist_ask_user_auxiliary_event(
    context: &AskUserExecutionContext<'_>,
    event_type: &str,
    content: String,
    request_id: String,
    metadata: Value,
) {
    let Some(writer) = context.auxiliary_event_writer else {
        return;
    };
    let record = crate::TurnAuxiliaryEventRecord {
        event_id: Uuid::now_v7().to_string(),
        user_id: context.user_id.to_string(),
        session_id: context.session_id.to_string(),
        agent_id: None,
        event_type: event_type.to_string(),
        content,
        parent_event_id: None,
        parent_event_ids: Vec::new(),
        causal_chain_id: request_id,
        metadata: Some(metadata),
        reasoning_content: None,
    };
    if let Err(error) = writer.persist_events(vec![record]).await {
        tracing::warn!(
            session_id = %context.session_id,
            event_type,
            error = %error,
            "failed to persist ask_user auxiliary event"
        );
    }
}
