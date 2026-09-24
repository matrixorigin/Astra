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

fn ensure_reuse_selection_report(state: &mut SessionState, session_id: String, turn: u32) {
    if state.memory_selection_reports.iter().any(|report| {
        matches!(
            report.operation,
            astra_turn_types::MemorySelectionOperation::Relevance
                | astra_turn_types::MemorySelectionOperation::Reuse
        )
    }) {
        return;
    }
    state
        .memory_selection_reports
        .push(astra_turn_types::MemorySelectionReport {
            session_id,
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
            candidate_coverage: None,
            prompt_projection: None,
        });
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
    let selected = astra_runtime::memory_hooks::relevance::filter_by_indices(
        &lessons,
        &report.selected_indices(),
    );
    (selected, report)
}

async fn maybe_load_memory_inference_offering(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
) {
    use super::session_memory_inference::MemoryJudgmentOffering;
    if !state.memory_inference_offering.should_resolve() {
        return;
    }
    match super::session_memory_inference::fetch_memory_judgment_offerings(api, token).await {
        Ok(offerings) => {
            state.memory_inference_offering = match offerings.into_iter().next() {
                Some(offering) => MemoryJudgmentOffering::Available(offering),
                None => MemoryJudgmentOffering::RetryAfter(
                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                ),
            };
        }
        Err(error) => {
            tracing::debug!("memory inference Offering fetch skipped: {error}");
            state.memory_inference_offering = MemoryJudgmentOffering::RetryAfter(
                std::time::Instant::now() + std::time::Duration::from_secs(3),
            );
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
        if session_id_for_scope.is_some() {
            maybe_load_memory_inference_offering(state, api, token).await;
        }
        let client = state.memory_inference_offering.offering().map(|offering| {
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
        ensure_reuse_selection_report(
            state,
            session_id_for_scope.clone().unwrap_or_default(),
            turn,
        );
        return;
    }

    let retrieval_started = std::time::Instant::now();
    let retrieval = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::edge_tools::memoria::memoria_retrieve_lessons(6, Some(user_message)),
    )
    .await;
    let retrieval = match retrieval {
        Ok(Ok(retrieval)) => retrieval,
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
                    candidate_coverage: None,
                    prompt_projection: None,
                });
            state.session_lessons_loaded = true;
            return;
        }
    };

    if !retrieval.lessons.is_empty() && session_id_for_scope.is_some() {
        maybe_load_memory_inference_offering(state, api, token).await;
    }
    let client = state.memory_inference_offering.offering().map(|offering| {
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
        retrieval.lessons,
        client
            .as_ref()
            .map(|client| client as &dyn astra_runtime::memory_hooks::MemoryInferencePort),
    )
    .await;
    report.session_id = session_id_for_scope.clone().unwrap_or_default();
    report.turn = turn;
    report.candidate_coverage = Some(astra_turn_types::MemoryCandidateCoverage {
        source_items: retrieval.source_items,
        evaluated_candidates: report.candidates.len() as u32,
        truncated: retrieval.truncated,
    });
    state.session_lessons = lessons;
    state.memory_selection_reports.push(report);
    state.session_lessons_loaded = true;
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_reuse_selection_report, filter_lessons_by_relevance, should_bootstrap_lessons,
    };
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

    #[test]
    fn dismissal_is_followed_by_a_receipt_for_the_retained_set() {
        for retained in [2_usize, 1, 0] {
            let mut state = SessionState::default();
            state.session_lessons = (0..retained)
                .map(|index| lesson(&format!("retained lesson number {index}")))
                .collect();
            state
                .memory_selection_reports
                .push(astra_turn_types::MemorySelectionReport {
                    session_id: "session-1".into(),
                    turn: 2,
                    operation: astra_turn_types::MemorySelectionOperation::Dismissal,
                    method: astra_turn_types::MemorySelectionMethod::None,
                    reason: astra_turn_types::MemorySelectionReason::NoSelector,
                    model: None,
                    candidates: vec![astra_turn_types::MemoryCandidateDecision {
                        index: 0,
                        selected: false,
                        probability_bps: None,
                    }],
                    selection_order: vec![],
                    elapsed_ms: 0,
                    candidate_coverage: None,
                    prompt_projection: None,
                });

            ensure_reuse_selection_report(&mut state, "session-1".into(), 2);

            assert_eq!(state.memory_selection_reports.len(), 2);
            let receipt = &state.memory_selection_reports[1];
            assert_eq!(
                receipt.operation,
                astra_turn_types::MemorySelectionOperation::Reuse
            );
            assert_eq!(receipt.selected_indices().len(), retained);
            assert!(receipt.is_valid());
        }
    }

    #[tokio::test]
    async fn filter_lessons_without_inference_client_ranks_without_dropping() {
        let lessons = vec![
            lesson("Do not treat curl checks as browser verification"),
            lesson("Prefer cargo test for Rust executor changes"),
        ];

        let (filtered, report) =
            filter_lessons_by_relevance(None, "review Rust executor code", lessons, None).await;

        assert_eq!(filtered.len(), 2);
        assert_eq!(report.candidates.len(), 2);
        assert_eq!(
            filtered[0].action,
            "Prefer cargo test for Rust executor changes"
        );
        assert_eq!(
            filtered[1].action,
            "Do not treat curl checks as browser verification"
        );
    }

    #[test]
    fn memory_offering_starts_unresolved_without_provider_material() {
        let state = SessionState::default();
        assert!(
            state.memory_inference_offering.offering().is_none(),
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
        ) -> Result<astra_runtime::memory_hooks::MemoryInferenceResponse, astra_core::ClassifiedError>
        {
            Ok(astra_runtime::memory_hooks::MemoryInferenceResponse {
                text: r#"{"answers":{"0":{"type":"discrete_noul","decision":"no"},"1":{"type":"discrete_noul","decision":"yes"}}}"#.into(),
                model_used: self.model_name().into(),
                judgment_provenance: Some(
                    astra_turn_types::JudgmentResponseProvenance::DiscreteDecision,
                ),
            })
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

    #[tokio::test]
    async fn judgment_offering_lookup_caches_absence_and_retries_failure() {
        use super::super::session_memory_inference::MemoryJudgmentOffering;
        use axum::{Json, Router, http::StatusCode, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route(
            "/models/memory",
            get(move || {
                let seen = seen.clone();
                async move {
                    let call = seen.fetch_add(1, Ordering::SeqCst);
                    if call == 0 {
                        (StatusCode::OK, Json(serde_json::json!({"offerings": []})))
                    } else if call == 1 {
                        (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(serde_json::json!({"error": "temporary"})),
                        )
                    } else {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({"offerings": [{
                                "offering_id": "offer-test",
                                "model_name": "test-judge",
                                "thinking_capability": null
                            }]})),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = astra_thin_client::ThinClient::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            None,
        )
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut state = SessionState::default();

        super::maybe_load_memory_inference_offering(&mut state, &api, "token").await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(state.memory_inference_offering.offering().is_none());
        super::maybe_load_memory_inference_offering(&mut state, &api, "token").await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "empty catalog is cached");

        state.memory_inference_offering =
            MemoryJudgmentOffering::RetryAfter(std::time::Instant::now());
        super::maybe_load_memory_inference_offering(&mut state, &api, "token").await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(state.memory_inference_offering.offering().is_none());
        super::maybe_load_memory_inference_offering(&mut state, &api, "token").await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "failure is rate-limited");

        state.memory_inference_offering =
            MemoryJudgmentOffering::RetryAfter(std::time::Instant::now());
        super::maybe_load_memory_inference_offering(&mut state, &api, "token").await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            state
                .memory_inference_offering
                .offering()
                .unwrap()
                .model_name,
            "test-judge"
        );
        super::maybe_load_memory_inference_offering(&mut state, &api, "token").await;
        assert_eq!(calls.load(Ordering::SeqCst), 3, "success is cached");
        server.abort();
    }

    #[test]
    fn judgment_offering_retry_deadline_is_demand_driven() {
        use super::super::session_memory_inference::MemoryJudgmentOffering;
        let now = std::time::Instant::now();
        assert!(MemoryJudgmentOffering::Unresolved.should_resolve());
        assert!(
            !MemoryJudgmentOffering::RetryAfter(now + std::time::Duration::from_secs(3))
                .should_resolve()
        );
        assert!(MemoryJudgmentOffering::RetryAfter(now).should_resolve());
    }

    #[tokio::test]
    async fn missing_session_scope_does_not_resolve_memory_judge() {
        use axum::{Json, Router, routing::get};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = calls.clone();
        let app = Router::new().route(
            "/models/memory",
            get(move || {
                let seen = seen.clone();
                async move {
                    seen.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({"offerings": []}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = astra_thin_client::ThinClient::new(
            &format!("http://{}", listener.local_addr().unwrap()),
            None,
        )
        .unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut state = SessionState {
            session_lessons: vec![lesson("Keep this applicable lesson")],
            session_lessons_loaded: true,
            ..SessionState::default()
        };
        super::ensure_bootstrapped_lessons(&mut state, &api, "token", "hi").await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
