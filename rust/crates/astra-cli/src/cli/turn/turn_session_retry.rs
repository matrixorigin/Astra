use astra_turn_core::chat_turn_heuristics::is_session_not_found_error;

use super::cli_utils::clear_profile_last_session_if_matches;
use super::session_state::SessionState;

pub(crate) fn should_retry_after_session_not_found(error: &str, has_session: bool) -> bool {
    has_session && is_session_not_found_error(error)
}

pub(crate) fn clear_stale_last_session_pointer(
    profile: Option<&str>,
    session_id: &str,
) -> Result<(), String> {
    clear_profile_last_session_if_matches(profile, session_id).map(|_| ())
}

pub(crate) async fn prepare_session_not_found_retry(
    state: &mut SessionState,
    profile: Option<&str>,
) {
    let old_sid = state.session_id.clone();
    if let Some(ref session_id) = old_sid {
        if let Err(error) = clear_stale_last_session_pointer(profile, session_id) {
            tracing::warn!(
                %error,
                %session_id,
                "failed to clear stale last-session pointer before retrying without session id"
            );
        }
    }
    if let (Some(hub), Some(old_sid)) = (&state.observability_hub, &old_sid) {
        let _ = hub.end_session(old_sid);
    }
    state.clear_session_id();
    state.unregister_root_mailbox().await;
    state.observability_session = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::cli_utils::{CredentialsFile, Profile, load_credentials, save_credentials};

    #[test]
    fn should_retry_after_session_not_found_requires_live_session() {
        assert!(should_retry_after_session_not_found(
            "session not found: 1234",
            true,
        ));
        assert!(!should_retry_after_session_not_found(
            "session not found: 1234",
            false,
        ));
        assert!(!should_retry_after_session_not_found("rate limited", true));
    }

    #[tokio::test]
    async fn prepare_session_not_found_retry_clears_session_identity_and_resume_state() {
        let mut state = SessionState {
            session_id: Some("sess-stale".into()),
            resume_guidance: Some("resume".into()),
            resume_restricted_tools: vec!["read_file".into()],
            ..SessionState::default()
        };

        prepare_session_not_found_retry(&mut state, None).await;

        assert!(state.session_id.is_none());
        assert!(state.resume_guidance.is_none());
        assert!(state.resume_restricted_tools.is_empty());
        assert!(state.observability_session.is_none());
        assert!(state.root_mailbox.is_none());
    }

    #[serial_test::serial]
    #[test]
    fn clear_stale_last_session_pointer_clears_matching_pointer_across_profiles() {
        let _creds_guard = crate::tests::isolate_credentials();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("tok-default".to_string()),
                ..Default::default()
            },
        );
        creds.profiles.insert(
            "other".to_string(),
            Profile {
                last_session_id: Some("sess-stale".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        clear_stale_last_session_pointer(None, "sess-stale").unwrap();

        let creds = load_credentials();
        assert_eq!(creds.profiles["other"].last_session_id.as_deref(), None);
    }
}
