//! Resolve execution settings from captured admission inputs.
//!
//! Consumers receive the resolved values; resolution never reads ambient state.

pub(crate) fn resolve_context_budget(
    runtime: &astra_config::runtime_config::RuntimeConfig,
    context_window: Option<u32>,
    max_completion_tokens: Option<u32>,
    compact: astra_turn_types::context_execution::CompactConfig,
) -> astra_turn_types::context_execution::ContextBudget {
    astra_turn_types::context_execution::ContextBudget::resolve(
        context_window,
        max_completion_tokens,
        runtime.compression.compression_threshold,
        runtime.compression.preserve_recent_turns as usize,
        (runtime.memory.max_memory_tokens as usize).saturating_mul(4),
        compact,
    )
}

/// Process-local material captured during root Run preparation. Persisted
/// Evaluation identity must be projected from these exact consumer inputs.
#[derive(Clone, Debug)]
pub(crate) struct PreparedExecutionInputs {
    pub runtime: astra_config::runtime_config::RuntimeConfig,
    pub static_sections: std::sync::Arc<astra_turn_types::prompt_sections::StaticSections>,
    pub summary_templates: astra_turn_types::summary_prompts::SummaryPromptTemplates,
    pub session_current_date: String,
}

impl PreparedExecutionInputs {
    pub(crate) fn capture(
        runtime: astra_config::runtime_config::RuntimeConfig,
        user_id: &str,
        session_id: &str,
    ) -> Self {
        Self {
            runtime,
            static_sections: std::sync::Arc::new(crate::prompts::build_pipeline_static_sections()),
            summary_templates: astra_turn_core::cloud_summary::canonical_summary_prompt_templates(),
            session_current_date:
                crate::turn::session_current_date::resolve_session_current_date_for_user(
                    user_id, session_id,
                ),
        }
    }

    pub(crate) fn new_pipeline_session(
        &self,
    ) -> astra_turn_core::pipeline_session::PipelineSession {
        let mut session = astra_turn_core::pipeline_session::PipelineSession::new_with_current_date(
            astra_turn_core::pipeline_config::PipelineConfig::default(),
            self.session_current_date.clone(),
        );
        session.static_sections_or_init(|| self.static_sections.as_ref().clone());
        session
    }
}

#[cfg(test)]
mod tests {
    use super::PreparedExecutionInputs;
    use astra_services::session_journal::{
        JournalDirGuard, JournalEvent, JournalWriter, journal_file_path_for_user,
    };
    use astra_turn_types::prompt_sections::{CacheScope, PromptTokenBucket};

    #[test]
    fn captured_date_and_sections_survive_owner_journal_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let user = "captured-input-owner";
        let session_id = "00000000-0000-0000-0000-000000000193";
        let writer = JournalWriter::for_user(user, session_id).unwrap();
        let mut event = JournalEvent::session_start(Some(session_id), None);
        event.ts = "2026-05-24T23:59:50Z".into();
        writer.append(&event).unwrap();
        drop(writer);

        let captured = PreparedExecutionInputs::capture(
            astra_config::runtime_config::RuntimeConfig::default(),
            user,
            session_id,
        );
        assert_eq!(captured.session_current_date, "2026-05-24");
        let expected_sections = captured.static_sections.as_ref().clone();

        // Replacing the first event makes a fresh resolver observably disagree;
        // merely appending an event would still return the original anchor.
        event.ts = "2026-05-26T00:10:00Z".into();
        std::fs::write(
            journal_file_path_for_user(user, session_id).unwrap(),
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        assert_eq!(
            crate::turn::session_current_date::resolve_session_current_date_for_user(
                user, session_id,
            ),
            "2026-05-26"
        );

        // Each reconstruction must consume the capture, not reread the journal.
        for _ in 0..2 {
            let mut session = captured.new_pipeline_session();
            assert_eq!(session.current_date(), "2026-05-24");
            let sections = session.static_sections_or_init(|| {
                panic!("captured sections must already populate the pipeline cache")
            });
            assert_eq!(sections.as_ref(), &expected_sections);
        }
        assert_eq!(captured.session_current_date, "2026-05-24");
    }

    #[test]
    fn supplied_capture_populates_pipeline_after_override_source_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path().join("sessions"));
        let mut captured = PreparedExecutionInputs::capture(
            astra_config::runtime_config::RuntimeConfig::default(),
            "override-owner",
            "00000000-0000-0000-0000-000000000194",
        );
        let overrides_dir = temp.path().join("prompts");
        std::fs::create_dir(&overrides_dir).unwrap();
        let override_path = overrides_dir.join("core_rules.txt");
        let exact_text = "captured rules\n保留原文及空白  \n";
        std::fs::write(&override_path, exact_text).unwrap();
        let overrides = crate::prompts::load_overrides(&overrides_dir);
        let section = &mut std::sync::Arc::make_mut(&mut captured.static_sections).core_rules;
        section.text = overrides["core_rules"].clone();
        section.scope = CacheScope::Session;
        section.token_bucket = PromptTokenBucket::UserPreferences;
        section.trace_signals.context_signals.system_prompt_override = true;
        captured.session_current_date = "1999-12-31".into();
        let expected_sections = captured.static_sections.as_ref().clone();
        std::fs::remove_file(override_path).unwrap();
        assert!(crate::prompts::load_overrides(&overrides_dir).is_empty());

        let mut session = captured.new_pipeline_session();
        assert_eq!(session.current_date(), "1999-12-31");
        let sections = session.static_sections_or_init(|| {
            panic!("supplied capture must prepopulate sections without loading overrides")
        });
        assert_eq!(sections.core_rules.text, exact_text);
        assert_eq!(sections.as_ref(), &expected_sections);
    }
}
