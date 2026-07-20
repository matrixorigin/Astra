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
) -> Vec<astra_services::LessonHint> {
    let texts: Vec<String> = lessons.iter().map(|lesson| lesson.action.clone()).collect();
    let filtered = if let (Some(client), Some(invocation_scope)) = (client, invocation_scope) {
        astra_runtime::memory_hooks::relevance::filter_memories(
            client,
            invocation_scope,
            user_message,
            &texts,
        )
        .await
    } else {
        astra_runtime::memory_hooks::relevance::lexical_filter_memories(user_message, &texts)
    };
    if filtered.len() == texts.len() {
        return lessons;
    }

    let filtered_set: std::collections::HashSet<&str> =
        filtered.iter().map(|text| text.as_str()).collect();
    lessons
        .into_iter()
        .filter(|lesson| filtered_set.contains(lesson.action.as_str()))
        .collect()
}

async fn maybe_load_memory_inference_offering(
    state: &mut SessionState,
    api: &astra_thin_client::ThinClient,
    token: &str,
) {
    if state.memory_inference_offering.is_some() {
        return;
    }
    match super::session_memory_inference::fetch_memory_inference_offerings(api, token).await {
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
        if let Some(offering) = state.memory_inference_offering.as_ref() {
            let client = super::session_memory_inference::CliServerMemoryInferenceClient::new(
                api.clone(),
                token,
                &offering.offering_id,
                &offering.model_name,
            );
            let texts: Vec<String> = state
                .session_lessons
                .iter()
                .map(|lesson| lesson.action.clone())
                .collect();
            let dismissed = match session_scope("memory_feedback") {
                Some(scope) => {
                    astra_runtime::memory_hooks::relevance::select_dismissed_memory_indices(
                        &client,
                        &scope,
                        user_message,
                        &texts,
                    )
                    .await
                }
                None => Vec::new(),
            };
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
    }

    if !should_bootstrap_lessons(state) {
        return;
    }

    maybe_load_memory_inference_offering(state, api, token).await;

    let lessons = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        crate::edge_tools::memoria::memoria_retrieve_lessons(6, Some(user_message)),
    )
    .await
    .unwrap_or_default();

    let client = state.memory_inference_offering.as_ref().map(|offering| {
        super::session_memory_inference::CliServerMemoryInferenceClient::new(
            api.clone(),
            token,
            &offering.offering_id,
            &offering.model_name,
        )
    });
    state.session_lessons = filter_lessons_by_relevance(
        session_scope("memory_relevance").as_ref(),
        user_message,
        lessons,
        client
            .as_ref()
            .map(|client| client as &dyn astra_runtime::memory_hooks::MemoryInferencePort),
    )
    .await;
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

        let filtered =
            filter_lessons_by_relevance(None, "review Rust executor code", lessons, None).await;

        assert_eq!(filtered.len(), 1);
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
}
