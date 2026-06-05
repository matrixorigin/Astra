//! Thin facade for one-shot/headless chat turns.
//!
//! This centralizes two invariants that used to be duplicated at multiple
//! call sites in `command_router.rs`:
//! 1. the first attempt may include preloaded continuation messages, but a
//!    session-not-found retry must preserve them so local continuation is not lost;
//! 2. a session-not-found retry must clear the persisted "last session"
//!    pointer before retrying without a session id.

use crate::cli::chat_stream::{ApprovalRequestTx, BasicCliChatContext, ChatTurnParams, stream_chat_sse};
use crate::cli::permission_manager::PermissionManager;
use crate::cli::stream::streaming_types::{StreamResult, TurnFailure};
use super::turn_session_retry::{
    clear_stale_last_session_pointer, should_retry_after_session_not_found,
};

#[derive(Clone, Default)]
pub(crate) struct BasicCliTurnOptions {
    pub(crate) pre_loaded_messages: Option<Vec<serde_json::Value>>,
    pub(crate) append_system_prompt: Option<String>,
    pub(crate) cancel_token: Option<std::sync::Arc<tokio_util::sync::CancellationToken>>,
    pub(crate) approval_request_tx: Option<ApprovalRequestTx>,
    pub(crate) disable_session_not_found_retry: bool,
}

fn build_basic_cli_turn_params<'a>(
    ctx: &'a BasicCliChatContext<'a>,
    token: &'a str,
    session_id: Option<&'a str>,
    perm_manager: &'a mut PermissionManager,
    skill_quality_tracker: &'a mut astra_skills::quality::SkillQualityTracker,
    options: &BasicCliTurnOptions,
    pre_loaded_messages: Option<Vec<serde_json::Value>>,
) -> ChatTurnParams<'a> {
    let mut params =
        ChatTurnParams::basic_cli(ctx, token, session_id, perm_manager, skill_quality_tracker);
    params.pre_loaded_messages = pre_loaded_messages;
    params.append_system_prompt = options.append_system_prompt.clone();
    params.cancel_token = options.cancel_token.clone();
    params.approval_request_tx = options.approval_request_tx.clone();
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
    pre_loaded_messages.clone()
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
    let params = build_basic_cli_turn_params(
        ctx,
        token,
        session_id,
        perm_manager,
        skill_quality_tracker,
        &options,
        pre_loaded_messages,
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
                retry_messages,
            ))
            .await
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
