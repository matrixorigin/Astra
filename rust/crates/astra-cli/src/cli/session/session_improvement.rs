use crate::cli::session::session_input::detect_correction_signal;
use super::*;

/// Minimal async-capable chat completion abstraction so the skill-improvement
/// LLM path can be unit-tested without real HTTP.
#[async_trait::async_trait]
pub(crate) trait SkillImproveLlm: Send + Sync {
    async fn complete(&self, system: &str, user: &str) -> Result<String, String>;
}

/// Async variant of the skill-improvement loop with an optional LLM-driven
/// rewrite path.
pub(crate) async fn check_skill_improvement_async(state: &mut SessionState) {
    // LLM-driven skill improvement previously used CloudLlmJudge::from_env; after env cleanup
    // it falls through to the heuristic append path below. TODO: wire a server-proxy LLM client
    // here if skill auto-rewrite becomes a priority again.
    let llm: Option<Box<dyn SkillImproveLlm>> = None;

    if let Some(llm) = llm {
        match try_llm_skill_improvement(state, llm.as_ref()).await {
            Ok(true) => return,
            Ok(false) => {}
            Err(e) => {
                astra_core::agent_debug!(
                    "skill",
                    "LLM skill-improvement failed, falling back to heuristic: {}",
                    e
                );
            }
        }
    }

    check_skill_improvement_sync(state);
}

/// Periodically detect user corrections in conversation history and turn them
/// into skill-improvement proposals.
pub(crate) fn check_skill_improvement_sync(state: &mut SessionState) {
    check_skill_improvement_inner(state);
}

/// LLM-driven skill-improvement core.
///
/// Return codes:
/// - `Ok(true)`  — the LLM path handled this turn. The caller must NOT run the
///   heuristic fallback. This covers both successful SKILL.md rewrites and
///   deliberate no-ops (no filesystem skills, no queued corrections, empty or
///   structurally-invalid LLM responses).
/// - `Err(_)`    — an unexpected error occurred; the caller should log it and
///   run the heuristic fallback.
///
/// The shape of `Result<bool, _>` is retained so future versions can reintroduce
/// an `Ok(false)` "inapplicable, please retry via heuristic" path without a
/// breaking signature change. At the moment no code path returns `Ok(false)`.
pub(crate) async fn try_llm_skill_improvement(
    state: &mut SessionState,
    llm: &dyn SkillImproveLlm,
) -> Result<bool, String> {
    if !state.skill_improvement_tracker.should_analyze(state.turn) {
        return Ok(true);
    }

    let registry = state.unified_skill_registry.clone();
    let manifests = registry.all_manifests();
    let filesystem_skills: Vec<_> = manifests
        .iter()
        .filter(|m| matches!(m.source, astra_skills::manifest::SkillSourceKind::Local))
        .collect();
    if filesystem_skills.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    let recent: Vec<astra_skills::improvement::RecentMessage> = state
        .history
        .iter()
        .rev()
        .take(astra_skills::improvement::TURN_BATCH_SIZE as usize)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                astra_skills::improvement::RecentMessage {
                    role: "user".into(),
                    content: user.clone(),
                },
                astra_skills::improvement::RecentMessage {
                    role: "assistant".into(),
                    content: assistant.clone(),
                },
            ]
        })
        .collect();

    if recent.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    let has_correction = recent
        .iter()
        .any(|m| m.role == "user" && detect_correction_signal(&m.content));
    if !has_correction {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    let target = filesystem_skills
        .iter()
        .find(|m| state.recent_tools.iter().any(|t| t.contains(&m.name)))
        .copied()
        .or_else(|| filesystem_skills.first().copied())
        .ok_or_else(|| "no target skill".to_string())?;

    let loaded = registry.get_loaded_skill(&target.name);
    let skill_dir = loaded
        .as_ref()
        .and_then(|s| s.skill_dir.clone())
        .ok_or_else(|| format!("skill {} has no on-disk directory", target.name))?;
    let skill_md = skill_dir.join("SKILL.md");
    let current_content = std::fs::read_to_string(&skill_md)
        .map_err(|e| format!("failed to read {}: {}", skill_md.display(), e))?;

    let (analysis_system, analysis_user) =
        astra_skills::improvement::build_analysis_prompt(&target.name, &current_content, &recent);
    let analysis_resp = llm.complete(&analysis_system, &analysis_user).await?;
    let improvements = astra_skills::improvement::parse_improvements(&analysis_resp);
    if improvements.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return Ok(true);
    }

    let rewrite_prompt =
        astra_skills::improvement::build_rewrite_prompt(&current_content, &improvements);
    let rewrite_system =
        "You are editing a skill definition file. Output only the <updated_file> block.";
    let rewrite_resp = llm.complete(rewrite_system, &rewrite_prompt).await?;
    let new_content = astra_skills::improvement::extract_updated_content(&rewrite_resp)
        .ok_or_else(|| "LLM response missing <updated_file> block".to_string())?;

    astra_skills::improvement::apply_improvement(&skill_md, &new_content)
        .map_err(|e| format!("failed to write {}: {}", skill_md.display(), e))?;

    let proposal = astra_skills::improvement::ImprovementProposal {
        skill_name: target.name.clone(),
        skill_path: skill_md.clone(),
        improvements: improvements.clone(),
    };
    state.skill_improvement_tracker.propose(proposal);
    eprintln!(
        "  {}",
        format!(
            "✓ applied {} LLM-generated improvement(s) to skill '{}' ({})",
            improvements.len(),
            target.name,
            skill_md.display()
        )
        .dim()
    );
    state.skill_improvement_tracker.mark_analyzed(state.turn);
    Ok(true)
}

/// Trim the content so that at most `keep` `## Recent user feedback` sections
/// remain (the most-recent ones). A "section" is delimited by any top-level
/// `## ` heading.
fn trim_feedback_sections(content: &str, keep: usize) -> String {
    const HEADING: &str = "## Recent user feedback";
    if content.matches(HEADING).count() <= keep {
        return content.to_string();
    }

    let mut section_starts = Vec::new();
    let mut pos = 0usize;
    for line in content.split_inclusive('\n') {
        if line.trim_start().starts_with("## ") {
            section_starts.push(pos);
        }
        pos += line.len();
    }
    section_starts.push(content.len());

    let mut feedback_ranges = Vec::new();
    for window in section_starts.windows(2) {
        let (start, end) = (window[0], window[1]);
        if content[start..end].trim_start().starts_with(HEADING) {
            feedback_ranges.push((start, end));
        }
    }

    if feedback_ranges.len() <= keep {
        return content.to_string();
    }

    let drop_count = feedback_ranges.len() - keep;
    let drop_set: std::collections::BTreeSet<(usize, usize)> =
        feedback_ranges.iter().take(drop_count).copied().collect();

    let mut out = String::with_capacity(content.len());
    let mut cursor = 0usize;
    for (start, end) in &drop_set {
        if cursor < *start {
            out.push_str(&content[cursor..*start]);
        }
        cursor = *end;
    }
    if cursor < content.len() {
        out.push_str(&content[cursor..]);
    }
    out
}

fn check_skill_improvement_inner(state: &mut SessionState) {
    if !state.skill_improvement_tracker.should_analyze(state.turn) {
        return;
    }

    let registry = state.unified_skill_registry.clone();
    let manifests = registry.all_manifests();
    let filesystem_skills: Vec<_> = manifests
        .iter()
        .filter(|m| matches!(m.source, astra_skills::manifest::SkillSourceKind::Local))
        .collect();

    if filesystem_skills.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let recent: Vec<astra_skills::improvement::RecentMessage> = state
        .history
        .iter()
        .rev()
        .take(astra_skills::improvement::TURN_BATCH_SIZE as usize)
        .rev()
        .flat_map(|(user, assistant)| {
            vec![
                astra_skills::improvement::RecentMessage {
                    role: "user".into(),
                    content: user.clone(),
                },
                astra_skills::improvement::RecentMessage {
                    role: "assistant".into(),
                    content: assistant.clone(),
                },
            ]
        })
        .collect();

    if recent.is_empty() {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let corrections: Vec<String> = recent
        .iter()
        .filter(|m| m.role == "user" && detect_correction_signal(&m.content))
        .map(|m| m.content.clone())
        .collect();

    if corrections.is_empty() {
        astra_core::agent_debug!(
            "skill",
            "improvement check: {} filesystem skill(s) eligible, no user corrections in last {} messages",
            filesystem_skills.len(),
            recent.len(),
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let target = filesystem_skills
        .iter()
        .find(|m| state.recent_tools.iter().any(|t| t.contains(&m.name)))
        .copied()
        .or_else(|| filesystem_skills.first().copied());
    let Some(target) = target else {
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    };

    let loaded = registry.get_loaded_skill(&target.name);
    let skill_dir = loaded.as_ref().and_then(|s| s.skill_dir.clone());
    let Some(skill_dir) = skill_dir else {
        astra_core::agent_debug!(
            "skill",
            "improvement check: skill {} has no on-disk directory — skipping",
            target.name,
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    };
    let skill_md = skill_dir.join("SKILL.md");
    if !skill_md.exists() {
        astra_core::agent_debug!(
            "skill",
            "improvement check: {} not found — skipping",
            skill_md.display(),
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let improvements: Vec<astra_skills::improvement::SkillImprovement> = corrections
        .iter()
        .map(|correction| {
            let snippet: String = correction.chars().take(240).collect();
            astra_skills::improvement::SkillImprovement {
                section: "Recent user feedback".into(),
                change: format!("User correction: {}", snippet),
                reason: "Detected correction pattern in user message".into(),
            }
        })
        .collect();

    let proposal = astra_skills::improvement::ImprovementProposal {
        skill_name: target.name.clone(),
        skill_path: skill_md.clone(),
        improvements: improvements.clone(),
    };

    const MAX_FEEDBACK_SECTIONS: usize = 5;
    let existing = std::fs::read_to_string(&skill_md).unwrap_or_default();

    let novel_changes: Vec<&str> = improvements
        .iter()
        .map(|improvement| improvement.change.as_str())
        .filter(|change| !existing.contains(change))
        .collect();

    if novel_changes.is_empty() {
        astra_core::agent_debug!(
            "skill",
            "improvement check: all {} corrections already recorded in {} — skipping append",
            improvements.len(),
            skill_md.display(),
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut appended = String::new();
    appended.push_str("\n\n## Recent user feedback\n");
    appended.push_str(&format!("<!-- auto-recorded at t={} -->\n", now));
    for change in &novel_changes {
        appended.push_str(&format!("- {}\n", change));
    }

    let trimmed_existing = trim_feedback_sections(&existing, MAX_FEEDBACK_SECTIONS - 1);
    let new_content = format!("{}{}", trimmed_existing.trim_end(), appended);
    if let Err(error) = astra_skills::improvement::apply_improvement(&skill_md, &new_content) {
        eprintln!(
            "  {}",
            format!(
                "skill improvement: failed to write {}: {}",
                skill_md.display(),
                error
            )
            .yellow()
        );
        state.skill_improvement_tracker.mark_analyzed(state.turn);
        return;
    }

    state.skill_improvement_tracker.propose(proposal);
    eprintln!(
        "  {}",
        format!(
            "✓ recorded {} user correction(s) into skill '{}' ({})",
            improvements.len(),
            target.name,
            skill_md.display()
        )
        .dim()
    );

    state.skill_improvement_tracker.mark_analyzed(state.turn);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SessionState;
    use crate::lock_recovery::LockRecovery;

    struct FakeLlm {
        responses: std::sync::Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SkillImproveLlm for FakeLlm {
        async fn complete(&self, _system: &str, _user: &str) -> Result<String, String> {
            let mut responses = self.responses.lock_recover();
            if responses.is_empty() {
                Err("no canned response".into())
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    #[tokio::test]
    async fn skill_improvement_records_correction_on_filesystem_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        std::fs::write(
            &skill_md,
            "---\nname: my-skill\ndescription: test skill\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal instructions.\n",
        )
        .unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = SessionState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, that's wrong — please do it differently next time".to_string(),
                "(previous assistant response)".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };
        assert!(state.skill_improvement_tracker.should_analyze(state.turn));

        check_skill_improvement_sync(&mut state);

        let updated = std::fs::read_to_string(&skill_md).unwrap();
        assert!(updated.contains("Recent user feedback"));
        assert!(updated.contains("User correction:"));
        assert!(updated.contains("Original instructions."));

        let pending = state
            .skill_improvement_tracker
            .take_proposal()
            .expect("pending proposal should be recorded");
        assert_eq!(pending.skill_name, "my-skill");
        assert_eq!(pending.skill_path, skill_md);
        assert!(!pending.improvements.is_empty());
        assert!(!state.skill_improvement_tracker.should_analyze(state.turn));
    }

    #[tokio::test]
    async fn skill_improvement_noop_without_correction() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test skill\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = SessionState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "hello, please add a feature".to_string(),
                "sure thing".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
            ..Default::default()
        };

        check_skill_improvement_sync(&mut state);

        let unchanged = std::fs::read_to_string(&skill_md).unwrap();
        assert_eq!(unchanged, original);
        assert!(state.skill_improvement_tracker.pending_proposal.is_none());
    }

    #[tokio::test]
    async fn llm_skill_improvement_rewrites_skill_md_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal instructions.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = SessionState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, don't greet twice — skip the greeting on follow-ups".to_string(),
                "Hello again!".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };

        let analysis = r#"[
          {"section": "greeting", "change": "skip greeting on follow-ups", "reason": "user said don't greet twice"}
        ]"#;
        let rewritten = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal instructions.\nSkip greeting on follow-up turns per user preference.\n";
        let wrapped_rewrite = format!("<updated_file>\n{}\n</updated_file>", rewritten);

        let llm = FakeLlm {
            responses: std::sync::Mutex::new(vec![analysis.to_string(), wrapped_rewrite]),
        };

        let ok = try_llm_skill_improvement(&mut state, &llm)
            .await
            .expect("LLM path should succeed");
        assert!(ok);

        let updated = std::fs::read_to_string(&skill_md).unwrap();
        assert!(updated.contains("Skip greeting on follow-up turns"));
        assert!(updated.contains("name: my-skill"));

        let pending = state
            .skill_improvement_tracker
            .take_proposal()
            .expect("structured proposal should land in tracker");
        assert_eq!(pending.skill_name, "my-skill");
        assert_eq!(pending.improvements.len(), 1);
        assert_eq!(pending.improvements[0].section, "greeting");
        assert!(!state.skill_improvement_tracker.should_analyze(state.turn));
    }

    #[tokio::test]
    async fn llm_skill_improvement_empty_response_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = SessionState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![("no, that's wrong".to_string(), "sorry".to_string())],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };

        let llm = FakeLlm {
            responses: std::sync::Mutex::new(vec!["[]".to_string()]),
        };

        let ok = try_llm_skill_improvement(&mut state, &llm).await.unwrap();
        assert!(ok);
        assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), original);
        assert!(state.skill_improvement_tracker.take_proposal().is_none());
    }

    #[tokio::test]
    async fn llm_error_falls_back_to_heuristic() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let skill_md = skill_dir.join("SKILL.md");
        let original = "---\nname: my-skill\ndescription: test\nversion: \"0.1.0\"\n---\n\n# Body\nOriginal.\n";
        std::fs::write(&skill_md, original).unwrap();

        let mut registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        registry.add_provider(Box::new(
            astra_skills::providers::LocalSkillProvider::with_paths(vec![tmp.path().to_path_buf()]),
        ));
        registry.discover_all().await.unwrap();
        registry.load("my-skill").await.unwrap();

        let mut state = SessionState {
            unified_skill_registry: std::sync::Arc::new(registry),
            history: vec![(
                "no, that's wrong, do it differently".to_string(),
                "sorry".to_string(),
            )],
            turn: astra_skills::improvement::TURN_BATCH_SIZE + 1,
            recent_tools: vec!["my-skill".to_string()],
            ..Default::default()
        };

        let llm = FakeLlm {
            responses: std::sync::Mutex::new(vec![]),
        };

        let result = try_llm_skill_improvement(&mut state, &llm).await;
        assert!(result.is_err());

        check_skill_improvement_sync(&mut state);

        let updated = std::fs::read_to_string(&skill_md).unwrap();
        assert!(updated.contains("## Recent user feedback"));
        assert!(updated.contains("no, that's wrong"));
    }

    #[test]
    fn trim_feedback_sections_caps_at_keep() {
        let content = "# Body\ncontent\n\n## Recent user feedback\n- a\n\n\
                       ## Recent user feedback\n- b\n\n## Recent user feedback\n- c\n\n\
                       ## Recent user feedback\n- d\n";
        let trimmed = trim_feedback_sections(content, 2);
        assert_eq!(trimmed.matches("## Recent user feedback").count(), 2);
        assert!(!trimmed.contains("- a"));
        assert!(!trimmed.contains("- b"));
        assert!(trimmed.contains("- c"));
        assert!(trimmed.contains("- d"));
        assert!(trimmed.contains("# Body"));
    }

    #[test]
    fn trim_feedback_sections_noop_when_within_cap() {
        let content = "# Body\n\n## Recent user feedback\n- only\n";
        let trimmed = trim_feedback_sections(content, 5);
        assert_eq!(trimmed, content);
    }
}
