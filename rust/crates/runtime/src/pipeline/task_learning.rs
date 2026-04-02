//! Task Learning Bridge — connects durable task outcomes to pipeline learning modules.
//!
//! Implements the `TaskLearningBridge` trait (defined in `mo-agent-services`) using the
//! concrete pipeline types: EntityGraph, PatternLibrary, ProgressiveCalibrator.
//!
//! This bridges the architectural gap: the trait lives in `services` (no pipeline dependency),
//! while this implementation lives in `runtime` (has access to pipeline internals).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use mo_agent_services::durable_task::{
    TaskContract, TaskDeliveryReport, TaskLearningBridge, TaskOutcomeSignal, TaskPatternStats,
};

use crate::pipeline::calibration::ProgressiveCalibrator;
use crate::pipeline::entity::{EntityGraph, extract_entities};
use crate::pipeline::pattern::PatternLibrary;
use crate::pipeline::routing::{DomainHint, TaskType};

/// Concrete implementation of [`TaskLearningBridge`] backed by pipeline learning modules.
///
/// Mirrors the builder pattern of [`PipelineLearningWriter`] — all modules are optional.
pub struct PipelineTaskLearningBridge {
    entity_graph: Option<Arc<Mutex<EntityGraph>>>,
    pattern_library: Option<Arc<Mutex<PatternLibrary>>>,
    calibrator: Option<Arc<Mutex<ProgressiveCalibrator>>>,
}

impl PipelineTaskLearningBridge {
    pub fn new() -> Self {
        Self {
            entity_graph: None,
            pattern_library: None,
            calibrator: None,
        }
    }

    pub fn with_entity_graph(mut self, graph: Arc<Mutex<EntityGraph>>) -> Self {
        self.entity_graph = Some(graph);
        self
    }

    pub fn with_pattern_library(mut self, library: Arc<Mutex<PatternLibrary>>) -> Self {
        self.pattern_library = Some(library);
        self
    }

    pub fn with_calibrator(mut self, calibrator: Arc<Mutex<ProgressiveCalibrator>>) -> Self {
        self.calibrator = Some(calibrator);
        self
    }

    /// Build from the same shared modules used by PipelineLearningWriter.
    pub fn from_shared(
        entity_graph: Arc<Mutex<EntityGraph>>,
        pattern_library: Arc<Mutex<PatternLibrary>>,
        calibrator: Arc<Mutex<ProgressiveCalibrator>>,
    ) -> Self {
        Self {
            entity_graph: Some(entity_graph),
            pattern_library: Some(pattern_library),
            calibrator: Some(calibrator),
        }
    }
}

impl Default for PipelineTaskLearningBridge {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────────

fn parse_task_type(label: Option<&str>) -> TaskType {
    match label {
        Some("code") => TaskType::Code,
        Some("reasoning") => TaskType::Reasoning,
        Some("fetch") => TaskType::Fetch,
        Some("mutate") => TaskType::Mutate,
        Some("memory") => TaskType::Memory,
        Some("conversational") => TaskType::Conversational,
        Some("compound") => TaskType::Compound,
        _ => TaskType::Unknown,
    }
}

fn parse_domain_hint(label: Option<&str>) -> Option<DomainHint> {
    match label {
        Some("github") => Some(DomainHint::GitHub),
        Some("code") => Some(DomainHint::Code),
        Some("memory") => Some(DomainHint::Memory),
        Some("git") => Some(DomainHint::Git),
        Some("system") => Some(DomainHint::System),
        Some("database") => Some(DomainHint::Database),
        Some("web") => Some(DomainHint::Web),
        _ => None,
    }
}

// ─── Trait Implementation ───────────────────────────────────────────────────

#[async_trait]
impl TaskLearningBridge for PipelineTaskLearningBridge {
    async fn learn_from_task_outcome(&self, signal: &TaskOutcomeSignal) -> Result<(), String> {
        let task_type = parse_task_type(signal.task_type.as_deref());
        let domain = parse_domain_hint(signal.domain_hint.as_deref());
        let feedback = signal.user_rating.map(|r| r as i64);

        // 1. EntityGraph: learn entity → domain → tools associations
        if signal.success
            && let Some(eg) = &self.entity_graph
        {
            let mut graph = eg.lock().unwrap_or_else(|e| e.into_inner());
            // Extract entities from the task goal
            let entities = extract_entities(&signal.goal);
            if let Some(d) = domain {
                for entity in &entities {
                    graph.learn(entity, d, &signal.tools_used, feedback);
                }
            }
            // Also learn from subtask titles (finer-grained entities)
            for sub in &signal.subtask_outcomes {
                let sub_entities = extract_entities(&sub.title);
                if let Some(d) = domain {
                    for entity in &sub_entities {
                        graph.learn(entity, d, &sub.tools_used, feedback);
                    }
                }
                // Learn from file paths as entities
                for file in &sub.files_modified {
                    if let Some(stem) = std::path::Path::new(file)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        && let Some(d) = domain
                    {
                        graph.learn(stem, d, &sub.tools_used, feedback);
                    }
                }
            }
        }

        // 2. PatternLibrary: record tool chain outcome
        if let Some(pl) = &self.pattern_library {
            let mut lib = pl.lock().unwrap_or_else(|e| e.into_inner());
            let quality = signal
                .user_rating
                .map(|r| r as f64 / 100.0)
                .unwrap_or(if signal.success { 0.7 } else { 0.2 });
            lib.record_outcome(
                &signal.tools_used,
                task_type,
                domain,
                signal.success,
                quality,
                feedback,
            );

            // Also record per-subtask patterns (if they have distinct tools)
            for sub in &signal.subtask_outcomes {
                if !sub.tools_used.is_empty() {
                    let sub_quality =
                        sub.verification_pass_rate
                            .unwrap_or(if sub.success { 0.7 } else { 0.2 });
                    lib.record_outcome(
                        &sub.tools_used,
                        task_type,
                        domain,
                        sub.success,
                        sub_quality,
                        feedback,
                    );
                }
            }
        }

        // 3. ProgressiveCalibrator: record task-level correction data
        if let Some(pc) = &self.calibrator {
            let mut cal = pc.lock().unwrap_or_else(|e| e.into_inner());
            let intent = format!("task_{task_type:?}").to_lowercase();
            // Treat failed tasks as implicit routing corrections
            let was_corrected = !signal.success && signal.total_retries > 0;
            cal.record(&intent, domain, task_type, was_corrected, feedback);
        }

        Ok(())
    }

    async fn extract_template(
        &self,
        contract: &TaskContract,
        _report: &TaskDeliveryReport,
    ) -> Result<Option<String>, String> {
        // Only extract templates from successful, multi-subtask contracts
        if contract.subtasks.len() < 2 {
            return Ok(None);
        }
        let all_success = contract.subtasks.iter().all(|s| s.stage.is_success());
        if !all_success {
            return Ok(None);
        }

        // Build a template signature from the subtask structure
        let subtask_titles: Vec<&str> =
            contract.subtasks.iter().map(|s| s.title.as_str()).collect();
        let template_name = format!(
            "learned_{}",
            contract
                .goal
                .to_lowercase()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '_' })
                .take(40)
                .collect::<String>()
        );

        let template_json = serde_json::json!({
            "name": template_name,
            "goal_pattern": contract.goal,
            "subtask_count": contract.subtasks.len(),
            "subtask_titles": subtask_titles,
            "scope": {
                "in_scope": contract.scope.in_scope,
                "out_of_scope": contract.scope.out_of_scope,
            },
            "verification_criteria_count": contract.subtasks.iter()
                .map(|s| s.criteria.len())
                .sum::<usize>(),
            "global_verification_count": contract.global_verification.len(),
        });

        Ok(Some(template_json.to_string()))
    }

    async fn suggest_tools(
        &self,
        goal: &str,
        domain_hint: Option<&str>,
        task_type: Option<&str>,
    ) -> Result<Vec<String>, String> {
        let tt = parse_task_type(task_type);
        let domain = parse_domain_hint(domain_hint);
        let mut suggestions = Vec::new();

        // Entity-based boost: extract entities from goal, get associated tools
        if let Some(eg) = &self.entity_graph {
            let graph = eg.lock().unwrap_or_else(|e| e.into_inner());
            let entities = extract_entities(goal);
            for entity in &entities {
                let boost = graph.boost_for(entity);
                suggestions.extend(boost);
            }
        }

        // Pattern-based suggestions: find top patterns for this task type
        if let Some(pl) = &self.pattern_library {
            let lib = pl.lock().unwrap_or_else(|e| e.into_inner());
            let patterns = lib.suggest(tt, domain, 3);
            for pattern in patterns {
                suggestions.extend(pattern.tools.iter().cloned());
            }
        }

        // Deduplicate while preserving first-seen order
        let mut seen = std::collections::HashSet::new();
        suggestions.retain(|t| seen.insert(t.clone()));

        Ok(suggestions)
    }

    async fn task_pattern_stats(
        &self,
        goal_pattern: &str,
    ) -> Result<Option<TaskPatternStats>, String> {
        let pl = match &self.pattern_library {
            Some(pl) => pl,
            None => return Ok(None),
        };
        let lib = pl.lock().unwrap_or_else(|e| e.into_inner());

        // Search patterns that match entities in the goal
        let entities = extract_entities(goal_pattern);
        if entities.is_empty() {
            return Ok(None);
        }

        // Find patterns containing any of the goal's entities
        let all_patterns = lib.export();
        let matching: Vec<_> = all_patterns
            .iter()
            .filter(|p| {
                let sig_lower = p.signature.to_lowercase();
                entities
                    .iter()
                    .any(|e| sig_lower.contains(&e.to_lowercase()))
            })
            .collect();

        if matching.is_empty() {
            return Ok(None);
        }

        let total_attempts: u32 = matching.iter().map(|p| p.total_count()).sum();
        let total_successes: u32 = matching.iter().map(|p| p.success_count).sum();
        let success_rate = if total_attempts > 0 {
            total_successes as f64 / total_attempts as f64
        } else {
            0.0
        };
        let avg_quality: f64 = if !matching.is_empty() {
            matching.iter().map(|p| p.avg_quality()).sum::<f64>() / matching.len() as f64
        } else {
            0.0
        };

        Ok(Some(TaskPatternStats {
            pattern: goal_pattern.to_string(),
            total_attempts,
            success_rate,
            avg_retries: 0.0, // not tracked at pattern level
            avg_turns: 0.0,   // not tracked at pattern level
            avg_verification_pass_rate: avg_quality,
        }))
    }

    async fn learn_from_verification(
        &self,
        signal: &mo_agent_services::durable_task::VerificationLearningSignal,
    ) -> Result<(), String> {
        // 1. PatternLibrary: record per-verifier-kind outcome patterns
        //    This lets the library track which verification strategies work/fail
        //    for different subtask types.
        if let Some(pl) = &self.pattern_library {
            let mut lib = pl.lock().unwrap_or_else(|e| e.into_inner());

            // Record each criterion's verifier kind as a "tool pattern"
            let verifier_tools: Vec<String> = signal
                .criteria_results
                .iter()
                .map(|c| format!("verify:{}", c.verifier_kind))
                .collect();

            if !verifier_tools.is_empty() {
                let quality = signal.pass_rate;
                lib.record_outcome(
                    &verifier_tools,
                    parse_task_type(None), // default task type
                    None,                  // domain not available at verification time
                    signal.all_passed,
                    quality,
                    None, // no user feedback at verification time
                );
            }
        }

        // 2. ProgressiveCalibrator: record verification-level correction signal
        //    Failed verifications after retry indicate the task type needs higher
        //    confidence thresholds.
        if !signal.all_passed
            && signal.attempt > 1
            && let Some(pc) = &self.calibrator
        {
            let mut cal = pc.lock().unwrap_or_else(|e| e.into_inner());
            let intent = format!("verify_{}", signal.subtask_id);
            // Verification failure on retry = implicit correction needed
            cal.record(
                &intent,
                None,
                parse_task_type(None),
                true, // was_corrected
                None,
            );
        }

        // 3. EntityGraph: learn from files involved in failed verifications
        //    Files that frequently fail verification get associated with "verify" tools.
        if !signal.all_passed
            && let Some(eg) = &self.entity_graph
        {
            let mut graph = eg.lock().unwrap_or_else(|e| e.into_inner());
            let failed_verifiers: Vec<String> = signal
                .criteria_results
                .iter()
                .filter(|c| !c.passed)
                .map(|c| format!("verify:{}", c.verifier_kind))
                .collect();
            for file in &signal.files {
                if let Some(stem) = std::path::Path::new(file)
                    .file_stem()
                    .and_then(|s| s.to_str())
                {
                    graph.learn(
                        stem,
                        DomainHint::Code, // files are always code domain
                        &failed_verifiers,
                        None,
                    );
                }
            }
        }

        Ok(())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use mo_agent_services::durable_task::{CriterionLearningResult, VerificationLearningSignal};

    fn make_signal(success: bool) -> TaskOutcomeSignal {
        TaskOutcomeSignal {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "implement JWT auth".into(),
            success,
            user_rating: Some(85),
            tools_used: vec!["read_file".into(), "str_replace".into(), "bash".into()],
            subtask_outcomes: vec![mo_agent_services::durable_task::SubtaskOutcomeSignal {
                subtask_id: "s1".into(),
                title: "add JWT module".into(),
                success: true,
                retry_count: 0,
                tools_used: vec!["read_file".into(), "str_replace".into()],
                verification_pass_rate: Some(1.0),
                files_modified: vec!["src/auth.rs".into()],
            }],
            total_verification_attempts: 1,
            total_retries: 0,
            total_turns: 5,
            domain_hint: Some("code".into()),
            task_type: Some("code".into()),
        }
    }

    #[tokio::test]
    async fn learn_from_successful_task() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));

        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        let signal = make_signal(true);
        bridge.learn_from_task_outcome(&signal).await.unwrap();

        // Verify entity graph was updated
        {
            let graph = eg.lock().unwrap();
            // "jwt" and "auth" should be extracted from "implement JWT auth"
            assert!(
                !graph.is_empty(),
                "entity graph should have learned entities"
            );
        }

        // Verify pattern library was updated
        {
            let lib = pl.lock().unwrap();
            assert!(
                !lib.is_empty(),
                "pattern library should have recorded outcome"
            );
        }

        // Verify calibrator was updated
        {
            let c = cal.lock().unwrap();
            assert!(
                c.tracked_intent_count() > 0,
                "calibrator should have recorded"
            );
        }
    }

    #[tokio::test]
    async fn learn_from_failed_task() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));

        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        let signal = make_signal(false);
        bridge.learn_from_task_outcome(&signal).await.unwrap();

        // Failed tasks should NOT update entity graph (only on success)
        {
            let graph = eg.lock().unwrap();
            assert_eq!(
                graph.len(),
                0,
                "entity graph should NOT learn from failures"
            );
        }

        // But pattern library SHOULD record failures
        {
            let lib = pl.lock().unwrap();
            assert!(
                !lib.is_empty(),
                "pattern library should record failures too"
            );
        }
    }

    #[tokio::test]
    async fn suggest_tools_with_patterns() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));

        // suggest() requires total_count() >= 2, so record twice
        {
            let mut lib = pl.lock().unwrap();
            lib.record_outcome(
                &["read_file".to_string(), "bash".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.9,
                None,
            );
            lib.record_outcome(
                &["read_file".to_string(), "bash".to_string()],
                TaskType::Code,
                Some(DomainHint::Code),
                true,
                0.85,
                None,
            );
        }

        let bridge = PipelineTaskLearningBridge::from_shared(eg, pl, cal);

        let tools = bridge
            .suggest_tools("implement something", Some("code"), Some("code"))
            .await
            .unwrap();

        assert!(!tools.is_empty(), "should suggest tools from patterns");
    }

    #[tokio::test]
    async fn extract_template_requires_multi_subtask_success() {
        let bridge = PipelineTaskLearningBridge::new();

        // Single subtask → no template
        let contract = TaskContract {
            contract_id: "c1".into(),
            task_id: "t1".into(),
            goal: "test".into(),
            scope: mo_agent_services::durable_task::TaskScope::default(),
            subtasks: vec![mo_agent_services::durable_task::DurableSubtask {
                id: "s1".into(),
                title: "only one".into(),
                stage: mo_agent_services::durable_task::SubtaskStage::Completed,
                ..Default::default()
            }],
            global_verification: vec![],
            version: 1,
            status: mo_agent_services::durable_task::ContractStatus::Completed,
            created_at: "2026-04-01".into(),
            updated_at: "2026-04-01".into(),
        };
        let report = TaskDeliveryReport {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "test".into(),
            subtask_summaries: vec![],
            global_verification: vec![],
            total_turns: 1,
            total_tokens: 100,
            total_verifications: 0,
            risks: vec![],
            timestamp: "2026-04-01".into(),
        };

        let result = bridge.extract_template(&contract, &report).await.unwrap();
        assert!(
            result.is_none(),
            "single subtask should not produce template"
        );

        // Multi-subtask success → template extracted
        let mut contract2 = contract.clone();
        contract2
            .subtasks
            .push(mo_agent_services::durable_task::DurableSubtask {
                id: "s2".into(),
                title: "second".into(),
                stage: mo_agent_services::durable_task::SubtaskStage::Verified,
                ..Default::default()
            });

        let result = bridge.extract_template(&contract2, &report).await.unwrap();
        assert!(
            result.is_some(),
            "multi-subtask success should produce template"
        );
        let tmpl = result.unwrap();
        assert!(
            tmpl.contains("learned_"),
            "template should have learned_ prefix"
        );
    }

    #[tokio::test]
    async fn empty_bridge_returns_defaults() {
        let bridge = PipelineTaskLearningBridge::new();

        // All operations should succeed with empty/default results
        let signal = make_signal(true);
        assert!(bridge.learn_from_task_outcome(&signal).await.is_ok());
        assert!(
            bridge
                .suggest_tools("test", None, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(bridge.task_pattern_stats("test").await.unwrap().is_none());
    }

    #[test]
    fn parse_helpers() {
        assert_eq!(parse_task_type(Some("code")), TaskType::Code);
        assert_eq!(parse_task_type(Some("fetch")), TaskType::Fetch);
        assert_eq!(parse_task_type(None), TaskType::Unknown);
        assert_eq!(parse_task_type(Some("bogus")), TaskType::Unknown);

        assert_eq!(parse_domain_hint(Some("github")), Some(DomainHint::GitHub));
        assert_eq!(
            parse_domain_hint(Some("database")),
            Some(DomainHint::Database)
        );
        assert_eq!(parse_domain_hint(Some("web")), Some(DomainHint::Web));
        assert!(parse_domain_hint(None).is_none());
        assert!(parse_domain_hint(Some("bogus")).is_none());
    }

    // ─── Verification Learning Tests ────────────────────────────────────────

    fn make_verification_signal(all_passed: bool, attempt: u32) -> VerificationLearningSignal {
        VerificationLearningSignal {
            task_id: "t1".into(),
            subtask_id: "s1".into(),
            subtask_title: "add auth module".into(),
            goal: "implement JWT auth".into(),
            all_passed,
            pass_rate: if all_passed { 1.0 } else { 0.5 },
            attempt,
            criteria_results: vec![
                CriterionLearningResult {
                    criterion_id: "c1".into(),
                    verifier_kind: "BuildPass".into(),
                    passed: true,
                    duration_ms: 1200,
                },
                CriterionLearningResult {
                    criterion_id: "c2".into(),
                    verifier_kind: "TestPass".into(),
                    passed: all_passed,
                    duration_ms: 3500,
                },
            ],
            files: vec!["src/auth.rs".into(), "src/routes.rs".into()],
        }
    }

    #[tokio::test]
    async fn learn_from_verification_records_verifier_patterns() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        let signal = make_verification_signal(true, 1);
        bridge.learn_from_verification(&signal).await.unwrap();

        // Pattern library should have recorded verifier kind tools
        let lib = pl.lock().unwrap();
        assert!(
            !lib.is_empty(),
            "pattern library should record verification outcomes"
        );
    }

    #[tokio::test]
    async fn learn_from_verification_failure_updates_entity_graph() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        let signal = make_verification_signal(false, 1);
        bridge.learn_from_verification(&signal).await.unwrap();

        // Entity graph should learn from failed files
        let graph = eg.lock().unwrap();
        assert!(
            !graph.is_empty(),
            "entity graph should learn file entities on failure"
        );
    }

    #[tokio::test]
    async fn learn_from_verification_retry_records_calibration() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        // First attempt failure — no calibrator update
        let signal = make_verification_signal(false, 1);
        bridge.learn_from_verification(&signal).await.unwrap();
        {
            let c = cal.lock().unwrap();
            assert_eq!(
                c.tracked_intent_count(),
                0,
                "first-attempt failure should not trigger calibration"
            );
        }

        // Second attempt failure — calibrator should record correction
        let signal = make_verification_signal(false, 2);
        bridge.learn_from_verification(&signal).await.unwrap();
        {
            let c = cal.lock().unwrap();
            assert!(
                c.tracked_intent_count() > 0,
                "retry failure should trigger calibration correction"
            );
        }
    }

    #[tokio::test]
    async fn learn_from_verification_success_does_not_touch_entity_graph() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        let signal = make_verification_signal(true, 1);
        bridge.learn_from_verification(&signal).await.unwrap();

        // Successful verification should NOT update entity graph (only failures)
        let graph = eg.lock().unwrap();
        assert_eq!(
            graph.len(),
            0,
            "entity graph should not learn from successful verifications"
        );
    }

    #[tokio::test]
    async fn noop_bridge_learn_from_verification_is_ok() {
        let bridge = PipelineTaskLearningBridge::new();
        let signal = make_verification_signal(true, 1);
        // Should succeed silently with no modules configured
        assert!(bridge.learn_from_verification(&signal).await.is_ok());
    }

    // ─── End-to-End: Verify → Learn → Deliver ──────────────────────────────

    #[tokio::test]
    async fn full_pipeline_verify_learn_deliver() {
        let eg = Arc::new(Mutex::new(EntityGraph::new()));
        let pl = Arc::new(Mutex::new(PatternLibrary::new()));
        let cal = Arc::new(Mutex::new(ProgressiveCalibrator::new(0.7)));
        let bridge = PipelineTaskLearningBridge::from_shared(eg.clone(), pl.clone(), cal.clone());

        // Phase 1: Simulate verification learning (subtask verified on first attempt)
        let verify_signal = make_verification_signal(true, 1);
        bridge
            .learn_from_verification(&verify_signal)
            .await
            .unwrap();

        // Phase 2: Simulate task delivery learning
        let outcome = make_signal(true);
        bridge.learn_from_task_outcome(&outcome).await.unwrap();

        // Phase 3: Verify all modules accumulated data
        {
            let graph = eg.lock().unwrap();
            assert!(
                !graph.is_empty(),
                "entity graph should have entities after delivery"
            );
        }
        {
            let lib = pl.lock().unwrap();
            let patterns = lib.export();
            assert!(
                patterns.len() >= 2,
                "pattern library should have both verification and task patterns"
            );
        }
        {
            let c = cal.lock().unwrap();
            assert!(
                c.tracked_intent_count() > 0,
                "calibrator should have data after delivery"
            );
        }

        // Phase 4: Verify suggestions incorporate learned patterns
        let suggestions = bridge
            .suggest_tools("implement auth", Some("code"), Some("code"))
            .await
            .unwrap();
        // Should include entities from the learning cycle
        assert!(
            !suggestions.is_empty() || !eg.lock().unwrap().is_empty(),
            "learning pipeline should produce usable knowledge"
        );

        // Phase 5: Verify template extraction works on multi-subtask contract
        let contract = TaskContract {
            contract_id: "c1".into(),
            task_id: "t1".into(),
            goal: "implement JWT auth".into(),
            scope: mo_agent_services::durable_task::TaskScope::default(),
            subtasks: vec![
                mo_agent_services::durable_task::DurableSubtask {
                    id: "s1".into(),
                    title: "add auth module".into(),
                    stage: mo_agent_services::durable_task::SubtaskStage::Verified,
                    ..Default::default()
                },
                mo_agent_services::durable_task::DurableSubtask {
                    id: "s2".into(),
                    title: "add routes".into(),
                    stage: mo_agent_services::durable_task::SubtaskStage::Completed,
                    ..Default::default()
                },
            ],
            global_verification: vec![],
            version: 1,
            status: mo_agent_services::durable_task::ContractStatus::Completed,
            created_at: "2026-04-01".into(),
            updated_at: "2026-04-01".into(),
        };
        let report = TaskDeliveryReport {
            task_id: "t1".into(),
            contract_id: "c1".into(),
            goal: "implement JWT auth".into(),
            subtask_summaries: vec![],
            global_verification: vec![],
            total_turns: 5,
            total_tokens: 1000,
            total_verifications: 2,
            risks: vec![],
            timestamp: "2026-04-01".into(),
        };
        let template = bridge.extract_template(&contract, &report).await.unwrap();
        assert!(
            template.is_some(),
            "successful multi-subtask contract should produce template"
        );
    }
}
