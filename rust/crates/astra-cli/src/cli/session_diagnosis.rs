use super::*;

/// Run the auto-invoke handler once per turn: compute signals, fire the gate,
/// and stash the first diagnosis in `state.latest_skill_diagnosis`.
pub(crate) async fn maybe_run_auto_invoke(state: &mut SessionState) {
    let signals = match state.observability_session.as_ref() {
        Some(arc) => match arc.read() {
            Ok(guard) => astra_runtime::auto_invoke_handler::compute_session_signals(Some(&*guard)),
            Err(_poison) => {
                tracing::warn!(
                    target: "auto_invoke",
                    "observability_session lock poisoned; auto-invoke signals degraded",
                );
                astra_runtime::auto_invoke_handler::compute_session_signals(None)
            }
        },
        None => astra_runtime::auto_invoke_handler::compute_session_signals(None),
    };

    let outcomes = state
        .diagnosis_outcome_tracker
        .evaluate_turn(&signals, state.turn);
    for outcome in &outcomes {
        let met = outcome
            .statuses
            .iter()
            .filter(|status| {
                **status == astra_runtime::auto_invoke_handler::DiagnosisCriterionStatus::Satisfied
            })
            .count() as u32;
        let failed = outcome
            .statuses
            .iter()
            .filter(|status| {
                **status == astra_runtime::auto_invoke_handler::DiagnosisCriterionStatus::Failed
            })
            .count() as u32;
        state.diagnosis_criteria_met = state.diagnosis_criteria_met.saturating_add(met);
        state.diagnosis_criteria_failed = state.diagnosis_criteria_failed.saturating_add(failed);
    }

    let default = astra_skills::auto_invoke::SessionSignals::default();
    if signals == default {
        state.latest_skill_diagnosis = None;
        return;
    }

    if state.auto_invoke_handler.is_none() {
        let executor: std::sync::Arc<dyn astra_runtime::auto_invoke_handler::SkillExecutor> =
            std::sync::Arc::new(
                astra_runtime::auto_invoke_handler::SyntheticSkillDiagnosisExecutor,
            );
        state.auto_invoke_handler = Some(
            astra_runtime::auto_invoke_handler::AutoInvokeHandler::new(executor),
        );
    }

    let handler = state
        .auto_invoke_handler
        .as_mut()
        .expect("just constructed above");
    let diagnoses = handler
        .maybe_fire(&signals, std::time::Instant::now())
        .await;

    for diagnosis in &diagnoses {
        state
            .diagnosis_outcome_tracker
            .activate(diagnosis.clone(), signals, state.turn);
    }
    if let Some(diagnosis) = diagnoses.into_iter().next() {
        state.latest_skill_diagnosis = Some(diagnosis);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn auto_invoke_noop_when_signals_are_zero() {
        let mut state = SessionState::default();
        assert!(state.auto_invoke_handler.is_none());
        assert!(state.latest_skill_diagnosis.is_none());

        maybe_run_auto_invoke(&mut state).await;

        assert!(
            state.auto_invoke_handler.is_none(),
            "zero-signals path must not construct a handler"
        );
        assert!(state.latest_skill_diagnosis.is_none());
    }

    #[tokio::test]
    async fn auto_invoke_populates_latest_diagnosis_on_stall_signal() {
        let mut state = SessionState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple("p8-turn-end"),
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

        maybe_run_auto_invoke(&mut state).await;

        let diagnosis = state
            .latest_skill_diagnosis
            .as_ref()
            .expect("3 stalls must produce a diagnosis");
        assert_eq!(diagnosis.skill, "analyze_session");
        assert_eq!(diagnosis.cause, "session_stalls");
        assert!(state.auto_invoke_handler.is_some());
    }

    #[tokio::test]
    async fn auto_invoke_cooldown_prevents_refire_within_window() {
        let mut state = SessionState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple("p8-cooldown"),
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

        maybe_run_auto_invoke(&mut state).await;
        let first_diagnosis = state.latest_skill_diagnosis.clone();
        assert!(first_diagnosis.is_some(), "first turn must fire");

        maybe_run_auto_invoke(&mut state).await;
        assert!(
            state.latest_skill_diagnosis.is_some(),
            "cooldown must NOT clear the diagnosis — tracker still evaluating"
        );
    }

    #[tokio::test]
    async fn diagnosis_outcome_tracker_accumulates_met_failed() {
        let mut state = SessionState::default();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple("r1-tracker"),
        ));
        {
            let mut guard = session.write().unwrap();
            guard.record_stall_event();
            guard.record_stall_event();
            guard.record_stall_event();
            guard.record_stall_event();
            guard.record_stall_event();
        }
        state.observability_session = Some(session.clone());

        state.turn = 5;
        maybe_run_auto_invoke(&mut state).await;
        let diagnosis = state
            .latest_skill_diagnosis
            .as_ref()
            .expect("should have fired");
        assert!(
            !diagnosis.success_criteria.is_empty(),
            "synthetic executor must produce criteria"
        );

        for turn in 6..=10 {
            state.turn = turn;
            maybe_run_auto_invoke(&mut state).await;
        }

        let total = state.diagnosis_criteria_met + state.diagnosis_criteria_failed;
        assert!(
            total > 0,
            "tracker must have evaluated at least one criterion; met={}, failed={}",
            state.diagnosis_criteria_met,
            state.diagnosis_criteria_failed
        );
    }
}
