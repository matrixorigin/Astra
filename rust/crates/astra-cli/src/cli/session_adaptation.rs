use super::session_diagnosis::maybe_run_auto_invoke;
use super::session_lessons::{checkpoint_lessons_from_runtime, ensure_bootstrapped_lessons};
use super::*;

pub(crate) async fn prepare_turn_adaptation(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
    message: &str,
) {
    if state.drift_original_query.is_none() {
        state.drift_original_query = Some(message.to_string());
    }

    ensure_bootstrapped_lessons(state, api, token, message).await;

    if crate::cli::session_input::detect_correction_signal(message) {
        let correction_turn = state.history.len() as u32;
        state.drift_user_corrections.push(correction_turn);
        checkpoint_lessons_from_runtime(state);
    }
}

pub(crate) async fn finalize_turn_adaptation(state: &mut SessionState, interrupted: bool) {
    if !interrupted {
        maybe_run_auto_invoke(state).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prepare_turn_adaptation_sets_original_query_and_correction_turn() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState {
            session_lessons_loaded: true,
            history: vec![("u1".into(), "a1".into()), ("u2".into(), "a2".into())],
            ..SessionState::default()
        };

        prepare_turn_adaptation(&mut state, &api, "token", "不对，修这里").await;

        assert_eq!(state.drift_original_query.as_deref(), Some("不对，修这里"));
        assert_eq!(state.drift_user_corrections, vec![2]);
    }

    #[tokio::test]
    async fn finalize_turn_adaptation_skips_auto_invoke_when_interrupted() {
        let mut state = SessionState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple("p8-skip"),
        ));
        {
            let mut guard = session.write().unwrap();
            guard.record_stall_event();
            guard.record_stall_event();
            guard.record_stall_event();
            guard.record_stall_event();
            guard.record_stall_event();
        }
        state.observability_session = Some(session);

        finalize_turn_adaptation(&mut state, true).await;

        assert!(state.latest_skill_diagnosis.is_none());
        assert!(state.auto_invoke_handler.is_none());
    }
}
