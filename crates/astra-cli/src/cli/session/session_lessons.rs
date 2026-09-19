use crate::cli::session::session_state::SessionState;

/// Run the lesson checkpointer against the current session signals.
/// If new lessons are produced, fire-and-forget write them to Memoria.
pub(crate) fn checkpoint_lessons_from_runtime(state: &mut SessionState) {
    let summary = match state
        .observability_session
        .as_ref()
        .and_then(|arc| arc.read().ok())
    {
        Some(guard) => astra_runtime::learning::extractor::summarise_from_runtime(
            &state.tool_health_entries,
            Some(&*guard),
        ),
        None => astra_runtime::learning::extractor::summarise_from_runtime(
            &state.tool_health_entries,
            None,
        ),
    };

    let delta = state.lesson_checkpointer.maybe_checkpoint(
        &summary,
        state.turn,
        state.ingestion_user_id.as_deref().unwrap_or("unknown"),
        "generic",
        None,
    );
    if delta.is_empty() {
        return;
    }

    let memoria_lessons: Vec<astra_runtime::learning::synthesizer::ExtractedLesson> = delta
        .into_iter()
        .filter(|lesson| {
            astra_runtime::learning::synthesizer::is_high_quality_lesson(&lesson.action)
        })
        .map(
            |lesson| astra_runtime::learning::synthesizer::ExtractedLesson {
                memory_type: "working",
                content: format!("💡 LESSON: {}", lesson.action),
                trust_tier: "T4",
            },
        )
        .collect();
    if memoria_lessons.is_empty() {
        return;
    }

    let session_id = state.session_id.clone();
    tokio::spawn(
        crate::edge_tools::memoria::memoria_store_lessons_fire_and_forget(
            memoria_lessons,
            session_id,
        ),
    );
}

pub(crate) fn should_bootstrap_lessons(state: &SessionState) -> bool {
    !state.session_lessons_loaded
}

async fn filter_lessons_by_relevance(
    invocation_scope: Option<&astra_turn_types::InferenceInvocationScope>,
    user_message: &str,
    lessons: Vec<astra_services::LessonHint>,
    client: Option<&dyn astra_runtime::memory_hooks::MemoryInferencePort>,
) -> (
    Vec<astra_services::LessonHint>,
    astra_turn_types::MemorySelectionReport,
) {
    let texts: Vec<String> = lessons.iter().map(|lesson| lesson.action.clone()).collect();
    let report = astra_runtime::memory_hooks::relevance::select_memories(
        client,
        invocation_scope,
        user_message,
        &texts,
        false,
    )
    .await;
    let selected = lessons
        .into_iter()
        .enumerate()
        .filter_map(|(i, lesson)| report.candidates[i].selected.then_some(lesson))
        .collect();
    (selected, report)
}

async fn maybe_load_memory_inference_offering(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
) {
    if state.memory_inference_offering.is_some() {
        return;
    }
    match super::session_memory_inference::fetch_memory_judgment_offerings(api, token).await {
        Ok(offerings) => {
            state.memory_inference_offering = offerings.into_iter().next();
        }
        Err(error) => {
            tracing::debug!("memory inference Offering fetch skipped: {error}");
        }
    }
}

pub(crate) async fn ensure_bootstrapped_lessons(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
    user_message: &str,
) {
    state.memory_selection_reports.clear();
    let turn = state.turn.saturating_add(1);
    let session_id_for_scope = state.session_id.clone();
    let session_scope = |operation_id: &str| {
        session_id_for_scope.as_ref().map(|session_id| {
            astra_turn_types::InferenceInvocationScope::Session {
                session_id: session_id.clone(),
                turn,
                round: 0,
                operation_id: operation_id.to_string(),
                logical_attempt: 0,
            }
        })
    };
    if !state.session_lessons.is_empty() {
        maybe_load_memory_inference_offering(state, api, token).await;
        let client = state.memory_inference_offering.as_ref().map(|offering| {
            super::session_memory_inference::CliServerMemoryInferenceClient::new(
                api.clone(),
                token,
                &offering.offering_id,
                &offering.model_name,
            )
        });
        let texts: Vec<String> = state
            .session_lessons
            .iter()
            .map(|lesson| lesson.action.clone())
            .collect();
        let mut report = astra_runtime::memory_hooks::relevance::select_memories(
            client
                .as_ref()
                .map(|c| c as &dyn astra_runtime::memory_hooks::MemoryInferencePort),
            session_scope("memory_feedback").as_ref(),
            user_message,
            &texts,
            true,
        )
        .await;
        report.session_id = session_id_for_scope.clone().unwrap_or_default();
        report.turn = turn;
        let indices = report.selected_indices();
        state.memory_selection_reports.push(report);
        let dismissed = indices;
        if !dismissed.is_empty() {
            let dismissed: std::collections::HashSet<usize> = dismissed.into_iter().collect();
            state.session_lessons = state
                .session_lessons
                .drain(..)
                .enumerate()
                .filter_map(|(idx, lesson)| (!dismissed.contains(&idx)).then_some(lesson))
                .collect();
        }
    }

    if !should_bootstrap_lessons(state) {
        if state.memory_selection_reports.is_empty() {
            state
                .memory_selection_reports
                .push(astra_turn_types::MemorySelectionReport {
                    session_id: session_id_for_scope.clone().unwrap_or_default(),
                    turn,
                    operation: astra_turn_types::MemorySelectionOperation::Reuse,
                    method: astra_turn_types::MemorySelectionMethod::Reuse,
                    selection_order: (0..state.session_lessons.len() as u32).collect(),
                    reason: astra_turn_types::MemorySelectionReason::Reused,
                    model: None,
                    candidates: state
                        .session_lessons
                        .iter()
                        .enumerate()
                        .map(|(i, _)| astra_turn_types::MemoryCandidateDecision {
                            index: i as u32,
                            selected: true,
                            probability_bps: None,
                        })
                        .collect(),
                    elapsed_ms: 0,
                });
        }
        return;
    }

    maybe_load_memory_inference_offering(state, api, token).await;

    let retrieval_started = std::time::Instant::now();
    let retrieval = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::edge_tools::memoria::memoria_retrieve_lessons(6, Some(user_message)),
    )
    .await;
    let lessons = match retrieval {
        Ok(Ok(lessons)) => lessons,
        failure => {
            state
                .memory_selection_reports
                .push(astra_turn_types::MemorySelectionReport {
                    session_id: session_id_for_scope.clone().unwrap_or_default(),
                    turn,
                    operation: astra_turn_types::MemorySelectionOperation::Relevance,
                    method: astra_turn_types::MemorySelectionMethod::None,
                    selection_order: Vec::new(),
                    reason: if failure.is_err() {
                        astra_turn_types::MemorySelectionReason::RetrievalTimeout
                    } else {
                        astra_turn_types::MemorySelectionReason::RetrievalUnavailable
                    },
                    model: None,
                    candidates: Vec::new(),
                    elapsed_ms: retrieval_started.elapsed().as_millis() as u64,
                });
            state.session_lessons_loaded = true;
            return;
        }
    };

    let client = state.memory_inference_offering.as_ref().map(|offering| {
        super::session_memory_inference::CliServerMemoryInferenceClient::new(
            api.clone(),
            token,
            &offering.offering_id,
            &offering.model_name,
        )
    });
    let (lessons, mut report) = filter_lessons_by_relevance(
        session_scope("memory_relevance").as_ref(),
        user_message,
        lessons,
        client
            .as_ref()
            .map(|client| client as &dyn astra_runtime::memory_hooks::MemoryInferencePort),
    )
    .await;
    report.session_id = session_id_for_scope.clone().unwrap_or_default();
    report.turn = turn;
    state.session_lessons = lessons;
    state.memory_selection_reports.push(report);
    state.session_lessons_loaded = true;
}

#[cfg(test)]
mod tests {
    use super::{filter_lessons_by_relevance, should_bootstrap_lessons};
    use crate::cli::session::session_state::SessionState;

    fn lesson(action: &str) -> astra_services::LessonHint {
        astra_services::LessonHint {
            kind: astra_services::LessonKind::PromptShape,
            trigger_signal: "memoria".into(),
            action: action.into(),
            compact: None,
            workload_tag: None,
        }
    }

    #[test]
    fn should_bootstrap_lessons_true_on_fresh_state() {
        let state = SessionState::default();
        assert!(
            should_bootstrap_lessons(&state),
            "fresh state should bootstrap from Memoria"
        );
    }

    #[test]
    fn should_bootstrap_lessons_skips_when_already_loaded() {
        let mut state = SessionState::default();
        state.session_lessons_loaded = true;
        assert!(
            !should_bootstrap_lessons(&state),
            "loaded flag must prevent re-bootstrap"
        );
    }

    #[tokio::test]
    async fn filter_lessons_without_inference_client_uses_local_relevance() {
        let lessons = vec![
            lesson("Do not treat curl checks as browser verification"),
            lesson("Prefer cargo test for Rust executor changes"),
        ];

        let (filtered, report) =
            filter_lessons_by_relevance(None, "review Rust executor code", lessons, None).await;

        assert_eq!(filtered.len(), 1);
        assert_eq!(report.candidates.len(), 2);
        assert_eq!(
            filtered[0].action,
            "Prefer cargo test for Rust executor changes"
        );
    }

    #[test]
    fn memory_offering_starts_unresolved_without_provider_material() {
        let state = SessionState::default();
        assert!(
            state.memory_inference_offering.is_none(),
            "memory Offering should start unresolved"
        );
    }

    #[derive(Debug)]
    struct SelectSecond;
    #[async_trait::async_trait]
    impl astra_runtime::memory_hooks::MemoryInferencePort for SelectSecond {
        fn model_name(&self) -> &str {
            "test-selector"
        }
        async fn complete(
            &self,
            _: astra_runtime::memory_hooks::MemoryInferenceRequest<'_>,
        ) -> Result<String, astra_core::ClassifiedError> {
            Ok(r#"{"true":["1"],"uncertain":[]}"#.into())
        }
    }

    #[tokio::test]
    async fn repeated_text_does_not_select_another_candidate() {
        let scope = astra_turn_types::InferenceInvocationScope::Session {
            session_id: "test".into(),
            turn: 1,
            round: 0,
            operation_id: "memory_relevance".into(),
            logical_attempt: 0,
        };
        let first = lesson("Same text");
        let mut second = first.clone();
        second.trigger_signal = "second".into();
        let (selected, report) = filter_lessons_by_relevance(
            Some(&scope),
            "test",
            vec![first, second],
            Some(&SelectSecond),
        )
        .await;
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].trigger_signal, "second");
        assert_eq!(report.selected_indices(), vec![1]);
        assert!(report.is_valid());
    }

    #[tokio::test]
    async fn reuse_replaces_previous_turn_decision() {
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:9", None).unwrap();
        let mut state = SessionState {
            session_id: Some("scope".into()),
            session_lessons_loaded: true,
            ..SessionState::default()
        };
        super::ensure_bootstrapped_lessons(&mut state, &api, "fake", "hi").await;
        state.turn = 1;
        super::ensure_bootstrapped_lessons(&mut state, &api, "fake", "next").await;
        assert_eq!(state.memory_selection_reports.len(), 1);
        assert_eq!(state.memory_selection_reports[0].turn, 2);
        assert_eq!(
            state.memory_selection_reports[0].method,
            astra_turn_types::MemorySelectionMethod::Reuse
        );
    }
}
