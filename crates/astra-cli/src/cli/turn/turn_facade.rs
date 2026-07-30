//! Thin facade for one-shot/headless chat turns.
//!
//! This centralizes two invariants that used to be duplicated at multiple
//! call sites in `command_router.rs`:
//! 1. the first attempt may include preloaded continuation messages, but a
//!    session-not-found retry must preserve them so local continuation is not lost;
//! 2. a session-not-found retry must clear the persisted "last session"
//!    pointer before retrying without a session id.

use super::turn_session_retry::{
    clear_stale_last_session_pointer, should_retry_after_session_not_found,
};
use crate::cli::chat_stream::{
    ApprovalRequestTx, BasicCliChatContext, ChatTurnParams, stream_chat_sse,
};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::stream::streaming_types::{StreamResult, TurnFailure};

#[derive(Clone, Default)]
pub(crate) struct BasicCliTurnOptions {
    pub(crate) pre_loaded_messages: Option<Vec<serde_json::Value>>,
    pub(crate) activated_deferred_tool_names: Vec<String>,
    pub(crate) append_system_prompt: Option<String>,
    pub(crate) cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    pub(crate) approval_request_tx: Option<ApprovalRequestTx>,
    pub(crate) disable_session_not_found_retry: bool,
    /// Authoritative 1-based outer-session turn restored before any auxiliary
    /// inference or main bridge request is admitted.
    pub(crate) turn_index: Option<u32>,
}

struct BasicCliTurnAttempt<'a> {
    pre_loaded_messages: Option<Vec<serde_json::Value>>,
    activated_deferred_tool_names: &'a mut Vec<String>,
}

fn build_basic_cli_turn_params<'a>(
    ctx: &'a BasicCliChatContext<'a>,
    token: &'a str,
    session_id: Option<&'a str>,
    perm_manager: &'a mut PermissionManager,
    skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    options: &BasicCliTurnOptions,
    attempt: BasicCliTurnAttempt<'a>,
) -> ChatTurnParams<'a> {
    let mut params =
        ChatTurnParams::basic_cli(ctx, token, session_id, perm_manager, skill_quality_tracker);
    params.pre_loaded_messages = attempt.pre_loaded_messages;
    params.activated_deferred_tool_names = Some(attempt.activated_deferred_tool_names);
    params.append_system_prompt = options.append_system_prompt.clone();
    params.cancel_token = options.cancel_token.clone();
    params.approval_request_tx = options.approval_request_tx.clone();
    if let Some(turn_index) = options.turn_index {
        params.turn_index = turn_index.max(1);
    }
    params
}

fn should_retry_without_session(
    error: &str,
    session_id: Option<&str>,
    retry_disabled: bool,
) -> bool {
    !retry_disabled && should_retry_after_session_not_found(error, session_id.is_some())
}

fn retry_pre_loaded_messages(
    pre_loaded_messages: &Option<Vec<serde_json::Value>>,
) -> Option<Vec<serde_json::Value>> {
    pre_loaded_messages.as_deref().map(|messages| {
        crate::cli::history_work::clone_json_history(
            astra_core::history_work::HistoryWorkSite::CliTurnRetryHistoryClone,
            messages,
        )
    })
}

pub(crate) async fn execute_basic_cli_turn<'a>(
    ctx: &'a BasicCliChatContext<'a>,
    token: &'a str,
    session_id: Option<&'a str>,
    profile: Option<&str>,
    perm_manager: &'a mut PermissionManager,
    skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    mut options: BasicCliTurnOptions,
) -> Result<StreamResult, TurnFailure> {
    let pre_loaded_messages = options.pre_loaded_messages.take();
    let retry_messages = retry_pre_loaded_messages(&pre_loaded_messages);
    let mut activated_deferred_tool_names =
        std::mem::take(&mut options.activated_deferred_tool_names);
    let params = build_basic_cli_turn_params(
        ctx,
        token,
        session_id,
        perm_manager,
        skill_quality_tracker,
        &options,
        BasicCliTurnAttempt {
            pre_loaded_messages,
            activated_deferred_tool_names: &mut activated_deferred_tool_names,
        },
    );
    match stream_chat_sse(params).await {
        Err(err)
            if should_retry_without_session(
                &err.error,
                session_id,
                options.disable_session_not_found_retry,
            ) =>
        {
            if let Some(stale_session_id) = session_id
                && let Err(clear_error) =
                    clear_stale_last_session_pointer(profile, stale_session_id)
            {
                tracing::warn!(
                    error = %clear_error,
                    session_id = ?stale_session_id,
                    "failed to clear stale last-session pointer before retrying without session id"
                );
            }
            stream_chat_sse(build_basic_cli_turn_params(
                ctx,
                token,
                None,
                perm_manager,
                skill_quality_tracker,
                &options,
                BasicCliTurnAttempt {
                    pre_loaded_messages: retry_messages,
                    activated_deferred_tool_names: &mut activated_deferred_tool_names,
                },
            ))
            .await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::{BasicCliTurnOptions, retry_pre_loaded_messages, should_retry_without_session};

    #[test]
    fn first_attempt_consumes_preloaded_messages_once() {
        let mut options = BasicCliTurnOptions {
            pre_loaded_messages: Some(vec![serde_json::json!({"role": "user", "content": "hi"})]),
            ..Default::default()
        };

        let first = options.pre_loaded_messages.take();
        let second = options.pre_loaded_messages.take();

        assert_eq!(first.as_ref().map(Vec::len), Some(1));
        assert!(
            second.is_none(),
            "preloaded messages should only be sent once"
        );
    }

    #[test]
    fn retry_without_session_requires_not_found_error_and_session_id() {
        assert!(should_retry_without_session(
            "session not found: 1234",
            Some("1234"),
            false
        ));
        assert!(!should_retry_without_session(
            "session not found: 1234",
            None,
            false
        ));
        assert!(!should_retry_without_session(
            "session not found: 1234",
            Some("1234"),
            true
        ));
        assert!(!should_retry_without_session(
            "rate limited",
            Some("1234"),
            false
        ));
    }

    #[test]
    fn session_not_found_retry_replays_preloaded_messages() {
        let original = Some(vec![
            serde_json::json!({"role": "assistant", "content": "previous answer"}),
        ]);

        let retry = retry_pre_loaded_messages(&original);

        assert_eq!(retry, original);
    }
}
