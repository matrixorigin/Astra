use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::durable_task::{ContractStatus, SubtaskStage, TaskContract};
use crate::session_journal::{self, JournalEvent, JournalEventType};
use crate::session_restore::{HybridRestoreService, RestoredSession};
use crate::session_workspace::{self, ContextTraceSignal, WorkspaceMetadata};
use astra_core::canonical_names::normalize_name_list as normalized_names;

#[cfg(test)]
use chrono::Utc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelfSurfaceDimension {
    Snapshot,
    Profile,
    Goal,
    Trace,
    Budget,
    Signals,
    Health,
    Journal,
    Verify,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "surface", content = "body", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum SelfSurfaceResponse {
    Snapshot(PersistentSelfSnapshot),
    Profile(ProfileSurface),
    Goal(GoalSurface),
    Trace(TraceSurface),
    Budget(BudgetSurface),
    Signals(SignalsSurface),
    Health(HealthSurface),
    Journal(JournalSurface),
    Verify(VerificationSurface),
}

#[derive(Debug, Clone, Serialize)]
pub struct PersistentSelfSnapshot {
    pub run: RunSurface,
    pub environment: EnvironmentSurface,
    pub recent_steps: Vec<StepRecord>,
    pub recent_decisions: Vec<DecisionRecord>,
    pub evolution: EvolutionSurface,
    pub acceptance: AcceptanceSurface,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunSurface {
    pub session_id: String,
    pub status: String,
    pub phase: String,
    pub turn_count: u32,
    pub persistence_error: Option<String>,
    pub goal: Option<String>,
    pub active_skill: Option<String>,
    pub latest_user_request: Option<String>,
    pub latest_assistant_output: Option<String>,
    pub last_updated_at: Option<String>,
    pub budget: Option<BudgetState>,
    pub totals: RunTotals,
    pub pending_blockers: Vec<String>,
    pub risk_flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunTotals {
    pub total_events: usize,
    pub total_tool_calls: usize,
    pub failure_events: usize,
    pub total_tokens_in: u64,
    pub total_tokens_out: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetState {
    pub max_tokens: u32,
    pub total_used: u32,
    pub remaining: u32,
    pub pressure: f64,
    pub compression_triggered: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentSurface {
    pub session_id: String,
    pub cwd: Option<String>,
    pub git_root: Option<String>,
    pub git_branch: Option<String>,
    pub git_head: Option<String>,
    pub model: Option<String>,
    pub resolved_sources: Vec<&'static str>,
    pub available_tools: usize,
    pub tool_names: Vec<String>,
    pub health_avoidance_tools: Vec<String>,
    pub discovered_skills: Vec<String>,
    pub active_experiment_id: Option<String>,
    pub active_variant: Option<String>,
    pub tuned_config_present: bool,
    pub last_context_trace_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepRecord {
    pub id: String,
    pub turn: Option<u32>,
    pub ts: String,
    pub event_type: String,
    pub actor: String,
    pub phase: String,
    pub summary: String,
    pub visible_tools: Vec<String>,
    pub used_tools: Vec<String>,
    pub selected_skills: Vec<String>,
    pub tool_calls: Vec<ToolCallView>,
    pub duration_ms: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub budget_pressure: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallView {
    pub name: String,
    pub ok: bool,
    pub latency_ms: u64,
    pub error: Option<String>,
    pub args_preview: Option<String>,
    pub result_preview: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionRecord {
    pub id: String,
    pub turn: Option<u32>,
    pub ts: String,
    pub visible_tools: Vec<String>,
    pub selected_skills: Vec<String>,
    pub routing_domain_hint: Option<String>,
    pub source_step_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvolutionSurface {
    pub active_experiment_id: Option<String>,
    pub active_variant: Option<String>,
    pub records: Vec<EvolutionRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EvolutionRecord {
    pub id: String,
    pub turn: Option<u32>,
    pub ts: String,
    pub kind: String,
    pub status: String,
    pub summary: String,
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AcceptanceSurface {
    pub ok: bool,
    pub summary: String,
    pub failing_checks: Vec<String>,
    pub checks: Vec<SelfSurfaceCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelfSurfaceCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub goal: Option<String>,
    pub phase: String,
    pub capabilities: CapabilitySurface,
    pub constraints: SurfaceConstraints,
    pub risk_flags: Vec<String>,
    pub acceptance_ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CapabilitySurface {
    pub total_tools: usize,
    pub tool_names: Vec<String>,
    pub health_avoidance_tools: Vec<String>,
    pub skills: Vec<String>,
    pub tool_health: Vec<ToolHealthView>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SurfaceConstraints {
    pub max_mutations_per_turn: u32,
    pub config_drift_ceiling: f64,
    pub min_available_tool_count: usize,
    pub token_reserve_fraction: f64,
}

impl Default for SurfaceConstraints {
    fn default() -> Self {
        Self {
            max_mutations_per_turn: 2,
            config_drift_ceiling: 0.30,
            min_available_tool_count: 5,
            token_reserve_fraction: 0.20,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct GoalSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub goal: Option<String>,
    pub plan_goal: Option<String>,
    pub phase: String,
    pub plan_execution_rounds: usize,
    pub plan_corrections: Vec<String>,
    pub recent_goal_events: Vec<EventPreview>,
    pub pending_blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub recent_steps: Vec<StepRecord>,
    pub recent_decisions: Vec<DecisionRecord>,
    pub compact_trace: Option<ContextTraceSignal>,
    pub compact_preview: Option<String>,
    pub latest_full_context_trace: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetConfig {
    pub compression_threshold: f64,
    pub max_turn_input_tokens: u32,
    pub compression_threshold_min: f64,
    pub compression_threshold_max: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            compression_threshold: 0.0,
            max_turn_input_tokens: 0,
            compression_threshold_min: 0.0,
            compression_threshold_max: 0.0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub budget: Option<BudgetState>,
    pub compression_threshold: f64,
    pub max_turn_input_tokens: u32,
    pub compression_threshold_min: f64,
    pub compression_threshold_max: f64,
    pub risk_flags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignalsSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub risk_flags: Vec<String>,
    pub records: Vec<EvolutionRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub phase: String,
    pub risk_flags: Vec<String>,
    pub pending_blockers: Vec<String>,
    pub health_avoidance_tools: Vec<String>,
    pub tool_hotspots: Vec<ToolHealthView>,
    pub recent_failures: Vec<ToolFailureView>,
    pub acceptance_ok: bool,
    pub failing_checks: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct JournalSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub phase: String,
    pub total_events: usize,
    pub returned: usize,
    pub events: Vec<EventPreview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationSurface {
    pub session_id: String,
    pub persistence_error: Option<String>,
    pub ok: bool,
    pub acceptance_ok: bool,
    pub objective_ok: bool,
    pub summary: String,
    pub objective: ObjectiveVerificationSurface,
    pub checks: Vec<SelfSurfaceCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ObjectiveVerificationSurface {
    pub ok: bool,
    pub goal: Option<String>,
    pub plan_goal: Option<String>,
    pub contract_status: Option<String>,
    pub subtasks_total: usize,
    pub subtasks_satisfied: usize,
    pub subtasks_incomplete: usize,
    pub subtasks_failed: usize,
    pub subtasks_blocked: usize,
    pub global_checks_total: usize,
    pub global_checks_passed: usize,
    pub pending_blockers: Vec<String>,
    pub latest_verification: Option<VerificationEventView>,
    pub recent_verifications: Vec<VerificationEventView>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerificationEventView {
    pub ts: String,
    pub turn: Option<u32>,
    pub scope: Option<String>,
    pub target: Option<String>,
    pub passed: Option<bool>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolHealthView {
    pub name: String,
    pub total_calls: usize,
    pub total_failures: usize,
    pub success_rate: f64,
    pub avoidance_advised: bool,
    pub consecutive_failures: usize,
    pub rehabilitation_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFailureView {
    pub ts: String,
    pub tool: String,
    pub error: Option<String>,
    pub turn: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventPreview {
    pub event_type: String,
    pub ts: String,
    pub turn: Option<u32>,
    pub error: Option<String>,
    pub tools_used: Option<Vec<String>>,
    pub metadata: Option<serde_json::Value>,
    pub user_input_preview: Option<String>,
    pub assistant_output_preview: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LoadedSelfSurfaceArtifacts {
    pub session_id: String,
    pub workspace: Option<WorkspaceMetadata>,
    pub restored: Option<RestoredSession>,
    pub journal_events: Vec<JournalEvent>,
    pub latest_full_context_trace: Option<serde_json::Value>,
}

type SessionArtifacts = LoadedSelfSurfaceArtifacts;

#[async_trait]
pub trait SelfSurfaceService: Send + Sync {
    async fn snapshot(
        &self,
        session_id: &str,
        journal_limit: usize,
    ) -> Result<PersistentSelfSnapshot, String>;

    async fn surface(
        &self,
        session_id: &str,
        dimension: SelfSurfaceDimension,
        journal_limit: usize,
    ) -> Result<SelfSurfaceResponse, String>;
}

#[async_trait]
pub trait SelfSurfaceArtifactLoader: Send + Sync {
    async fn load_artifacts(&self, session_id: &str) -> Result<LoadedSelfSurfaceArtifacts, String>;
}

pub trait SelfSurfaceRuntimeSupport: Send + Sync {
    fn tool_names(&self) -> Vec<String>;
    fn constraints(&self) -> SurfaceConstraints {
        SurfaceConstraints::default()
    }
    fn budget_config(&self, tuned_config_json: Option<&str>) -> Result<BudgetConfig, String>;
    fn runtime_checks(&self, tuned_config_json: Option<&str>) -> Vec<SelfSurfaceCheck>;
}

pub struct NoopSelfSurfaceRuntimeSupport;

impl SelfSurfaceRuntimeSupport for NoopSelfSurfaceRuntimeSupport {
    fn tool_names(&self) -> Vec<String> {
        Vec::new()
    }

    fn budget_config(&self, _: Option<&str>) -> Result<BudgetConfig, String> {
        Ok(BudgetConfig::default())
    }

    fn runtime_checks(&self, _: Option<&str>) -> Vec<SelfSurfaceCheck> {
        Vec::new()
    }
}

#[derive(Default)]
struct LocalSelfSurfaceArtifactLoader;

#[async_trait]
impl SelfSurfaceArtifactLoader for LocalSelfSurfaceArtifactLoader {
    async fn load_artifacts(&self, session_id: &str) -> Result<LoadedSelfSurfaceArtifacts, String> {
        load_local_artifacts(session_id).await
    }
}

pub struct LocalSelfSurfaceService {
    runtime_support: Arc<dyn SelfSurfaceRuntimeSupport>,
    artifact_loader: Arc<dyn SelfSurfaceArtifactLoader>,
}

impl Default for LocalSelfSurfaceService {
    fn default() -> Self {
        Self {
            runtime_support: Arc::new(NoopSelfSurfaceRuntimeSupport),
            artifact_loader: Arc::new(LocalSelfSurfaceArtifactLoader),
        }
    }
}

impl LocalSelfSurfaceService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_runtime_support(
        mut self,
        runtime_support: Arc<dyn SelfSurfaceRuntimeSupport>,
    ) -> Self {
        self.runtime_support = runtime_support;
        self
    }

    pub fn with_artifact_loader(
        mut self,
        artifact_loader: Arc<dyn SelfSurfaceArtifactLoader>,
    ) -> Self {
        self.artifact_loader = artifact_loader;
        self
    }
}

#[async_trait]
impl SelfSurfaceService for LocalSelfSurfaceService {
    async fn snapshot(
        &self,
        session_id: &str,
        journal_limit: usize,
    ) -> Result<PersistentSelfSnapshot, String> {
        let artifacts = self.artifact_loader.load_artifacts(session_id).await?;
        build_persistent_snapshot(
            &artifacts,
            journal_limit.max(1),
            self.runtime_support.as_ref(),
        )
    }

    async fn surface(
        &self,
        session_id: &str,
        dimension: SelfSurfaceDimension,
        journal_limit: usize,
    ) -> Result<SelfSurfaceResponse, String> {
        let snapshot = self.snapshot(session_id, journal_limit).await?;
        let artifacts = self.artifact_loader.load_artifacts(session_id).await?;

        Ok(match dimension {
            SelfSurfaceDimension::Snapshot => SelfSurfaceResponse::Snapshot(snapshot),
            SelfSurfaceDimension::Profile => SelfSurfaceResponse::Profile(build_profile_surface(
                &snapshot,
                &artifacts,
                self.runtime_support.as_ref(),
            )),
            SelfSurfaceDimension::Goal => {
                SelfSurfaceResponse::Goal(build_goal_surface(&snapshot, &artifacts))
            }
            SelfSurfaceDimension::Trace => {
                SelfSurfaceResponse::Trace(build_trace_surface(&snapshot, &artifacts))
            }
            SelfSurfaceDimension::Budget => SelfSurfaceResponse::Budget(build_budget_surface(
                &snapshot,
                &artifacts,
                self.runtime_support.as_ref(),
            )?),
            SelfSurfaceDimension::Signals => SelfSurfaceResponse::Signals(SignalsSurface {
                session_id: artifacts.session_id.clone(),
                persistence_error: snapshot.run.persistence_error.clone(),
                risk_flags: snapshot.run.risk_flags.clone(),
                records: snapshot.evolution.records.clone(),
            }),
            SelfSurfaceDimension::Health => {
                SelfSurfaceResponse::Health(build_health_surface(&snapshot, &artifacts))
            }
            SelfSurfaceDimension::Journal => SelfSurfaceResponse::Journal(build_journal_surface(
                &snapshot,
                &artifacts,
                journal_limit.max(1),
            )),
            SelfSurfaceDimension::Verify => {
                SelfSurfaceResponse::Verify(build_verification_surface(&snapshot, &artifacts))
            }
        })
    }
}

async fn load_local_artifacts(session_id: &str) -> Result<LoadedSelfSurfaceArtifacts, String> {
    session_journal::validate_session_id(session_id)
        .map_err(|error| format!("invalid session id '{session_id}': {error}"))?;
    let workspace = session_workspace::read_workspace_optional(session_id)
        .map_err(|error| format!("failed to read workspace for session {session_id}: {error}"))?;
    let journal_events = read_journal_events(session_id)?;
    let restore_service = HybridRestoreService::local_only();
    let restored = restore_service.restore_local_session(session_id).await?;
    if workspace.is_none() && restored.is_none() && journal_events.is_empty() {
        return Err(format!(
            "no persistent local state found for session {session_id}"
        ));
    }
    let latest_full_context_trace = journal_events
        .iter()
        .rev()
        .find_map(|event| event.context_assembly_trace.clone());
    Ok(LoadedSelfSurfaceArtifacts {
        session_id: session_id.to_string(),
        workspace,
        restored,
        journal_events,
        latest_full_context_trace,
    })
}

fn read_journal_events(session_id: &str) -> Result<Vec<JournalEvent>, String> {
    session_journal::read_journal(session_id).map_err(|error| {
        format!("failed to read session journal for session {session_id}: {error}")
    })
}

fn build_persistent_snapshot(
    artifacts: &SessionArtifacts,
    journal_limit: usize,
    runtime_support: &dyn SelfSurfaceRuntimeSupport,
) -> Result<PersistentSelfSnapshot, String> {
    let health = build_health_data(artifacts);
    let recent_steps = build_recent_steps(&artifacts.journal_events, journal_limit);
    let recent_decisions = build_recent_decisions(artifacts, journal_limit);
    let evolution = build_evolution_surface(artifacts, journal_limit);
    let environment = build_environment_surface(artifacts, runtime_support);
    let runtime_checks = runtime_support.runtime_checks(
        artifacts
            .workspace
            .as_ref()
            .and_then(|ws| ws.tuned_config_json.as_deref()),
    );
    let provisional_run = build_run_surface(artifacts, &health, &runtime_checks, &evolution);
    let provisional_acceptance = build_acceptance_surface(
        artifacts,
        runtime_checks.clone(),
        &provisional_run,
        &environment,
        &recent_steps,
        &recent_decisions,
        &evolution,
    );
    let final_run = with_acceptance_context(provisional_run, &provisional_acceptance);
    let acceptance = build_acceptance_surface(
        artifacts,
        runtime_checks,
        &final_run,
        &environment,
        &recent_steps,
        &recent_decisions,
        &evolution,
    );

    Ok(PersistentSelfSnapshot {
        run: final_run,
        environment,
        recent_steps,
        recent_decisions,
        evolution,
        acceptance,
    })
}

fn build_environment_surface(
    artifacts: &SessionArtifacts,
    runtime_support: &dyn SelfSurfaceRuntimeSupport,
) -> EnvironmentSurface {
    let workspace = artifacts.workspace.as_ref();
    let tool_names = normalized_names(runtime_support.tool_names());
    let last_context_trace = latest_context_trace(artifacts);

    EnvironmentSurface {
        session_id: artifacts.session_id.clone(),
        cwd: workspace.map(|ws| ws.cwd.clone()),
        git_root: workspace.and_then(|ws| ws.git_root.clone()),
        git_branch: workspace.and_then(|ws| ws.git_branch.clone()).or_else(|| {
            artifacts
                .restored
                .as_ref()
                .and_then(|restored| restored.git_branch.clone())
        }),
        git_head: workspace.and_then(|ws| ws.git_head.clone()),
        model: workspace.and_then(|ws| ws.model.clone()).or_else(|| {
            artifacts
                .restored
                .as_ref()
                .and_then(|restored| restored.model.clone())
        }),
        resolved_sources: resolved_sources(artifacts),
        available_tools: tool_names.len(),
        tool_names,
        health_avoidance_tools: merged_health_avoidance_tools(artifacts),
        discovered_skills: merged_skills(workspace),
        active_experiment_id: workspace.and_then(|ws| ws.active_experiment_id.clone()),
        active_variant: workspace.and_then(|ws| ws.active_variant.clone()),
        tuned_config_present: workspace
            .and_then(|ws| ws.tuned_config_json.as_ref())
            .is_some(),
        last_context_trace_preview: last_context_trace.as_ref().map(|trace| trace.preview()),
    }
}

fn build_run_surface(
    artifacts: &SessionArtifacts,
    health: &HealthData,
    runtime_checks: &[SelfSurfaceCheck],
    evolution: &EvolutionSurface,
) -> RunSurface {
    let budget = budget_state_from_artifacts(artifacts);
    let persistence_error = session_persistence_error(artifacts);
    let risk_flags = build_risk_flags(
        &budget,
        health,
        runtime_checks,
        evolution,
        persistence_error,
    );
    let pending_blockers = build_pending_blockers(
        health,
        runtime_checks,
        &persistence_pending_blockers(persistence_error),
    );
    let effective_goal = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.plan_goal.clone());

    RunSurface {
        session_id: artifacts.session_id.clone(),
        status: artifacts
            .workspace
            .as_ref()
            .map(|ws| ws.status.clone())
            .or_else(|| {
                artifacts
                    .restored
                    .as_ref()
                    .map(|restored| restored.last_status.clone())
            })
            .unwrap_or_else(|| derived_status_from_events(&artifacts.journal_events)),
        phase: artifacts
            .journal_events
            .last()
            .map(|event| phase_for_event_type(&event.event_type).to_string())
            .unwrap_or_else(|| "observe".to_string()),
        turn_count: artifacts
            .workspace
            .as_ref()
            .map(|ws| ws.turn_count)
            .or_else(|| {
                artifacts
                    .restored
                    .as_ref()
                    .map(|restored| restored.turn_count)
            })
            .unwrap_or_default(),
        persistence_error: persistence_error.map(str::to_string),
        goal: effective_goal,
        active_skill: latest_active_skill(&artifacts.journal_events),
        latest_user_request: latest_event_text(&artifacts.journal_events, true),
        latest_assistant_output: latest_event_text(&artifacts.journal_events, false),
        last_updated_at: artifacts
            .workspace
            .as_ref()
            .map(|ws| ws.updated_at.clone())
            .or_else(|| {
                artifacts
                    .journal_events
                    .last()
                    .map(|event| event.ts.clone())
            }),
        budget,
        totals: RunTotals {
            total_events: artifacts.journal_events.len(),
            total_tool_calls: artifacts
                .journal_events
                .iter()
                .map(|event| {
                    event.tool_outcomes.as_ref().map_or_else(
                        || {
                            event
                                .tool_calls
                                .as_ref()
                                .map(|calls| {
                                    calls.iter().filter(|call| call.was_executed()).count()
                                })
                                .unwrap_or_default()
                        },
                        |outcomes| outcomes.executed as usize,
                    )
                })
                .sum(),
            failure_events: artifacts
                .journal_events
                .iter()
                .filter(|event| {
                    matches!(
                        event.event_type,
                        JournalEventType::TurnError
                            | JournalEventType::Error
                            | JournalEventType::StallDetected
                    ) || event.tool_calls.as_ref().is_some_and(|calls| {
                        calls.iter().any(|call| call.was_executed() && !call.ok)
                    })
                })
                .count(),
            total_tokens_in: artifacts
                .workspace
                .as_ref()
                .map(|ws| ws.total_tokens_in)
                .or_else(|| {
                    artifacts
                        .restored
                        .as_ref()
                        .map(|restored| restored.total_tokens_in)
                })
                .unwrap_or_default(),
            total_tokens_out: artifacts
                .workspace
                .as_ref()
                .map(|ws| ws.total_tokens_out)
                .or_else(|| {
                    artifacts
                        .restored
                        .as_ref()
                        .map(|restored| restored.total_tokens_out)
                })
                .unwrap_or_default(),
        },
        pending_blockers,
        risk_flags,
    }
}

fn with_acceptance_context(mut run: RunSurface, acceptance: &AcceptanceSurface) -> RunSurface {
    if !acceptance.ok {
        run.risk_flags.push("acceptance_gaps".to_string());
        run.risk_flags.sort();
        run.risk_flags.dedup();
        run.pending_blockers = build_pending_blockers_from_lists(
            &run.pending_blockers,
            &acceptance
                .failing_checks
                .iter()
                .map(|name| format!("acceptance:{name}"))
                .collect::<Vec<_>>(),
        );
    }
    run
}

fn build_profile_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
    runtime_support: &dyn SelfSurfaceRuntimeSupport,
) -> ProfileSurface {
    let health = build_health_data(artifacts);
    ProfileSurface {
        session_id: snapshot.run.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        goal: snapshot.run.goal.clone(),
        phase: snapshot.run.phase.clone(),
        capabilities: CapabilitySurface {
            total_tools: snapshot.environment.available_tools,
            tool_names: snapshot.environment.tool_names.clone(),
            health_avoidance_tools: snapshot.environment.health_avoidance_tools.clone(),
            skills: snapshot.environment.discovered_skills.clone(),
            tool_health: health.tool_health.into_iter().take(8).collect(),
        },
        constraints: runtime_support.constraints(),
        risk_flags: snapshot.run.risk_flags.clone(),
        acceptance_ok: snapshot.acceptance.ok,
    }
}

fn build_goal_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
) -> GoalSurface {
    let plan_goal = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.plan_goal.clone());

    GoalSurface {
        session_id: artifacts.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        goal: plan_goal.clone(),
        plan_goal,
        phase: snapshot.run.phase.clone(),
        plan_execution_rounds: artifacts
            .workspace
            .as_ref()
            .map(|ws| ws.plan_execution_rounds)
            .unwrap_or_default(),
        plan_corrections: artifacts
            .workspace
            .as_ref()
            .map(|ws| ws.plan_corrections.clone())
            .unwrap_or_default(),
        recent_goal_events: recent_event_previews(
            &artifacts.journal_events,
            10,
            &[
                JournalEventType::PlanProgress,
                JournalEventType::PlanEdit,
                JournalEventType::PlanLifecycle,
                JournalEventType::GoalSteered,
                JournalEventType::VerificationCompleted,
            ],
        ),
        pending_blockers: snapshot.run.pending_blockers.clone(),
    }
}

fn build_trace_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
) -> TraceSurface {
    TraceSurface {
        session_id: artifacts.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        recent_steps: snapshot.recent_steps.clone(),
        recent_decisions: snapshot.recent_decisions.clone(),
        compact_trace: latest_context_trace(artifacts).cloned(),
        compact_preview: latest_context_trace(artifacts).map(ContextTraceSignal::preview),
        latest_full_context_trace: artifacts.latest_full_context_trace.clone(),
    }
}

fn build_budget_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
    runtime_support: &dyn SelfSurfaceRuntimeSupport,
) -> Result<BudgetSurface, String> {
    let budget_config = runtime_support.budget_config(
        artifacts
            .workspace
            .as_ref()
            .and_then(|ws| ws.tuned_config_json.as_deref()),
    )?;
    Ok(BudgetSurface {
        session_id: artifacts.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        budget: snapshot.run.budget.clone(),
        compression_threshold: budget_config.compression_threshold,
        max_turn_input_tokens: budget_config.max_turn_input_tokens,
        compression_threshold_min: budget_config.compression_threshold_min,
        compression_threshold_max: budget_config.compression_threshold_max,
        risk_flags: snapshot.run.risk_flags.clone(),
    })
}

fn build_health_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
) -> HealthSurface {
    let health = build_health_data(artifacts);
    HealthSurface {
        session_id: artifacts.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        phase: snapshot.run.phase.clone(),
        risk_flags: snapshot.run.risk_flags.clone(),
        pending_blockers: snapshot.run.pending_blockers.clone(),
        health_avoidance_tools: health.health_avoidance_tools,
        tool_hotspots: health.tool_health.into_iter().take(10).collect(),
        recent_failures: health.recent_failures,
        acceptance_ok: snapshot.acceptance.ok,
        failing_checks: snapshot.acceptance.failing_checks.clone(),
    }
}

fn build_journal_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
    journal_limit: usize,
) -> JournalSurface {
    let events = artifacts
        .journal_events
        .iter()
        .rev()
        .take(journal_limit)
        .map(event_preview)
        .collect::<Vec<_>>();

    JournalSurface {
        session_id: artifacts.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        phase: snapshot.run.phase.clone(),
        total_events: artifacts.journal_events.len(),
        returned: events.len(),
        events,
    }
}

fn build_verification_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
) -> VerificationSurface {
    let objective = build_objective_verification_surface(snapshot, artifacts);
    let acceptance_ok = snapshot.acceptance.ok;
    let objective_ok = objective.ok;
    let ok = acceptance_ok && objective_ok;
    let summary = match (acceptance_ok, objective_ok) {
        (true, true) => "acceptance and objective verification passed".to_string(),
        (false, true) => snapshot.acceptance.summary.clone(),
        (true, false) => objective.summary.clone(),
        (false, false) => format!(
            "{}; {}",
            snapshot.acceptance.summary,
            objective.summary.to_lowercase()
        ),
    };

    VerificationSurface {
        session_id: artifacts.session_id.clone(),
        persistence_error: snapshot.run.persistence_error.clone(),
        ok,
        acceptance_ok,
        objective_ok,
        summary,
        objective,
        checks: snapshot.acceptance.checks.clone(),
    }
}

fn build_objective_verification_surface(
    snapshot: &PersistentSelfSnapshot,
    artifacts: &SessionArtifacts,
) -> ObjectiveVerificationSurface {
    let goal = snapshot.run.goal.clone();
    let plan_goal = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.plan_goal.clone());
    let pending_blockers = snapshot.run.pending_blockers.clone();
    let recent_verifications = recent_verification_events(&artifacts.journal_events, 8);
    let latest_verification = recent_verifications.first().cloned();

    if let Some(contract) = task_contract_from_artifacts(artifacts) {
        let subtasks_total = contract.subtasks.len();
        let subtasks_satisfied = contract
            .subtasks
            .iter()
            .filter(|subtask| subtask_stage_satisfied(&subtask.stage))
            .count();
        let subtasks_failed = contract
            .subtasks
            .iter()
            .filter(|subtask| subtask_stage_failed(&subtask.stage))
            .count();
        let subtasks_blocked = contract
            .subtasks
            .iter()
            .filter(|subtask| matches!(subtask.stage, SubtaskStage::Blocked { .. }))
            .count();
        let subtasks_incomplete = subtasks_total.saturating_sub(subtasks_satisfied);
        let global_checks_total = contract.global_verification.len();
        let global_checks_passed = contract
            .last_global_results
            .iter()
            .filter(|result| result.passed)
            .count();
        let global_ok = global_checks_total == 0
            || (contract.last_global_results.len() >= global_checks_total
                && global_checks_passed == global_checks_total);
        let objective_ok = pending_blockers.is_empty()
            && match contract.status {
                ContractStatus::Completed => true,
                _ if subtasks_total == 0 && global_checks_total == 0 => latest_verification
                    .as_ref()
                    .and_then(|event| event.passed)
                    .unwrap_or(false),
                _ => {
                    subtasks_incomplete == 0
                        && subtasks_failed == 0
                        && subtasks_blocked == 0
                        && global_ok
                }
            };
        let summary = if objective_ok {
            format!(
                "objective satisfied: {subtasks_satisfied}/{subtasks_total} subtasks complete, {global_checks_passed}/{global_checks_total} global checks passed"
            )
        } else if !pending_blockers.is_empty() {
            format!("objective blocked: {}", pending_blockers.join("; "))
        } else if subtasks_incomplete > 0 || subtasks_failed > 0 || subtasks_blocked > 0 {
            format!(
                "objective pending: {subtasks_satisfied}/{subtasks_total} subtasks satisfied, {subtasks_blocked} blocked, {subtasks_failed} failed"
            )
        } else if !global_ok {
            format!(
                "global verification incomplete: {global_checks_passed}/{global_checks_total} checks passed"
            )
        } else {
            "objective has not produced passing verification evidence yet".to_string()
        };

        return ObjectiveVerificationSurface {
            ok: objective_ok,
            goal,
            plan_goal,
            contract_status: Some(contract.status.as_str().to_string()),
            subtasks_total,
            subtasks_satisfied,
            subtasks_incomplete,
            subtasks_failed,
            subtasks_blocked,
            global_checks_total,
            global_checks_passed,
            pending_blockers,
            latest_verification,
            recent_verifications,
            summary,
        };
    }

    let has_objective = goal.is_some() || plan_goal.is_some();
    let latest_passed = latest_verification
        .as_ref()
        .and_then(|event| event.passed)
        .unwrap_or(false);
    let objective_ok = if has_objective {
        pending_blockers.is_empty() && latest_passed
    } else {
        true
    };
    let summary = if objective_ok {
        if has_objective {
            "goal has passing verification evidence".to_string()
        } else {
            "no explicit objective contract recorded".to_string()
        }
    } else if !pending_blockers.is_empty() {
        format!("objective blocked: {}", pending_blockers.join("; "))
    } else {
        "goal recorded but no passing verification evidence yet".to_string()
    };

    ObjectiveVerificationSurface {
        ok: objective_ok,
        goal,
        plan_goal,
        contract_status: None,
        subtasks_total: 0,
        subtasks_satisfied: 0,
        subtasks_incomplete: 0,
        subtasks_failed: 0,
        subtasks_blocked: 0,
        global_checks_total: 0,
        global_checks_passed: 0,
        pending_blockers,
        latest_verification,
        recent_verifications,
        summary,
    }
}

fn recent_verification_events(events: &[JournalEvent], limit: usize) -> Vec<VerificationEventView> {
    events
        .iter()
        .rev()
        .filter_map(verification_event_view_from_event)
        .take(limit)
        .collect()
}

fn verification_event_view_from_event(event: &JournalEvent) -> Option<VerificationEventView> {
    if !matches!(event.event_type, JournalEventType::VerificationCompleted) {
        return None;
    }

    let metadata = event.metadata.as_ref();
    let scope = metadata
        .and_then(|meta| meta.get("scope"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let target = metadata
        .and_then(|meta| meta.get("subtask_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let passed = metadata
        .and_then(|meta| meta.get("passed"))
        .and_then(serde_json::Value::as_bool);
    let summary = metadata
        .and_then(|meta| meta.get("results"))
        .map(compact_json_value)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| match passed {
            Some(true) => "verification passed".to_string(),
            Some(false) => "verification failed".to_string(),
            None => "verification recorded".to_string(),
        });

    Some(VerificationEventView {
        ts: event.ts.clone(),
        turn: event.turn,
        scope,
        target,
        passed,
        summary,
    })
}

fn task_contract_from_artifacts(artifacts: &SessionArtifacts) -> Option<TaskContract> {
    artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.contract_json.as_deref())
        .and_then(|json| serde_json::from_str::<TaskContract>(json).ok())
}

fn subtask_stage_satisfied(stage: &SubtaskStage) -> bool {
    matches!(
        stage,
        SubtaskStage::Verified | SubtaskStage::Completed | SubtaskStage::Skipped { .. }
    )
}

fn subtask_stage_failed(stage: &SubtaskStage) -> bool {
    matches!(
        stage,
        SubtaskStage::ExecutionFailed { .. }
            | SubtaskStage::VerificationFailed { .. }
            | SubtaskStage::Abandoned { .. }
    )
}

#[derive(Default)]
struct ToolHealthAccumulator {
    total_calls: usize,
    total_failures: usize,
    consecutive_failures: usize,
    rehabilitation_count: usize,
    avoidance_advised: bool,
}

struct HealthData {
    health_avoidance_tools: Vec<String>,
    tool_health: Vec<ToolHealthView>,
    recent_failures: Vec<ToolFailureView>,
}

fn build_health_data(artifacts: &SessionArtifacts) -> HealthData {
    let health_avoidance_tools = merged_health_avoidance_tools(artifacts);
    let mut by_tool: BTreeMap<String, ToolHealthAccumulator> = BTreeMap::new();

    for event in &artifacts.journal_events {
        let Some(tool_calls) = event.tool_calls.as_ref() else {
            continue;
        };
        for call in tool_calls {
            let entry = by_tool.entry(call.name.clone()).or_default();
            entry.total_calls += 1;
            if call.ok {
                if entry.consecutive_failures > 0 {
                    entry.rehabilitation_count += 1;
                }
                entry.consecutive_failures = 0;
            } else {
                entry.total_failures += 1;
                entry.consecutive_failures += 1;
            }
        }
    }

    for tool in &health_avoidance_tools {
        by_tool.entry(tool.clone()).or_default().avoidance_advised = true;
    }

    let mut tool_health = by_tool
        .into_iter()
        .map(|(name, acc)| ToolHealthView {
            name,
            total_calls: acc.total_calls,
            total_failures: acc.total_failures,
            success_rate: if acc.total_calls == 0 {
                1.0
            } else {
                (acc.total_calls.saturating_sub(acc.total_failures)) as f64 / acc.total_calls as f64
            },
            avoidance_advised: acc.avoidance_advised,
            consecutive_failures: acc.consecutive_failures,
            rehabilitation_count: acc.rehabilitation_count,
        })
        .collect::<Vec<_>>();
    tool_health.sort_by(|a, b| {
        b.total_calls
            .cmp(&a.total_calls)
            .then_with(|| a.name.cmp(&b.name))
    });

    HealthData {
        health_avoidance_tools,
        tool_health,
        recent_failures: recent_tool_failures(&artifacts.journal_events, 12),
    }
}

fn build_recent_steps(events: &[JournalEvent], journal_limit: usize) -> Vec<StepRecord> {
    events
        .iter()
        .rev()
        .take(journal_limit)
        .map(|event| StepRecord {
            id: step_id(event),
            turn: event.turn,
            ts: event.ts.clone(),
            event_type: event_type_name(&event.event_type),
            actor: actor_for_event(event).to_string(),
            phase: phase_for_event_type(&event.event_type).to_string(),
            summary: summarize_event(event),
            visible_tools: normalized_names(event.visible_tools.as_deref().unwrap_or(&[])),
            used_tools: normalized_names(event.tools_used.as_deref().unwrap_or(&[])),
            selected_skills: normalized_names(event.selected_skills.as_deref().unwrap_or(&[])),
            tool_calls: event
                .tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|call| ToolCallView {
                            name: call.name.clone(),
                            ok: call.ok,
                            latency_ms: call.ms,
                            error: call.error.clone(),
                            args_preview: call.args_preview.clone(),
                            result_preview: call.result_preview.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            duration_ms: event.duration_ms,
            tokens_in: event.tokens_in,
            tokens_out: event.tokens_out,
            budget_pressure: event.budget_pressure,
            error: event.error.clone(),
        })
        .collect()
}

fn build_recent_decisions(
    artifacts: &SessionArtifacts,
    journal_limit: usize,
) -> Vec<DecisionRecord> {
    let mut decisions = Vec::new();

    for event in artifacts.journal_events.iter().rev() {
        let Some(record) = decision_from_event(event) else {
            continue;
        };
        decisions.push(record);
        if decisions.len() >= journal_limit {
            break;
        }
    }

    if let Some(trace) = artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.last_context_trace.as_ref())
        .and_then(|trace| trace.tool_surface.as_ref())
    {
        decisions.push(DecisionRecord {
            id: format!(
                "decision:context-trace:{}",
                artifacts
                    .workspace
                    .as_ref()
                    .and_then(|ws| ws.last_context_trace.as_ref())
                    .map(|trace| trace.turn_id.clone())
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            turn: artifacts.workspace.as_ref().map(|ws| ws.turn_count),
            ts: artifacts
                .workspace
                .as_ref()
                .and_then(|ws| ws.last_context_trace.as_ref())
                .and_then(|trace| trace.captured_at.clone())
                .unwrap_or_else(|| "workspace".to_string()),
            visible_tools: normalized_names(&trace.visible_tools),
            selected_skills: artifacts
                .workspace
                .as_ref()
                .map(|workspace| merged_skills(Some(workspace)))
                .unwrap_or_default(),
            routing_domain_hint: None,
            source_step_id: None,
        });
    }

    decisions
}

fn decision_from_event(event: &JournalEvent) -> Option<DecisionRecord> {
    let visible_tools = normalized_names(event.visible_tools.as_deref().unwrap_or(&[]));
    let selected_skills = normalized_names(event.selected_skills.as_deref().unwrap_or(&[]));
    let has_decision = !visible_tools.is_empty() || !selected_skills.is_empty();
    if !has_decision {
        return None;
    }

    Some(DecisionRecord {
        id: format!("decision:{}", step_id(event)),
        turn: event.turn,
        ts: event.ts.clone(),
        visible_tools,
        selected_skills,
        routing_domain_hint: event.routing_domain_hint.clone(),
        source_step_id: Some(step_id(event)),
    })
}

fn build_evolution_surface(artifacts: &SessionArtifacts, journal_limit: usize) -> EvolutionSurface {
    let records = artifacts
        .journal_events
        .iter()
        .rev()
        .filter_map(evolution_record_from_event)
        .take(journal_limit)
        .collect();

    EvolutionSurface {
        active_experiment_id: artifacts
            .workspace
            .as_ref()
            .and_then(|ws| ws.active_experiment_id.clone()),
        active_variant: artifacts
            .workspace
            .as_ref()
            .and_then(|ws| ws.active_variant.clone()),
        records,
    }
}

fn evolution_record_from_event(event: &JournalEvent) -> Option<EvolutionRecord> {
    let (kind, status) = match event.event_type {
        JournalEventType::TurnError | JournalEventType::Error => ("failure", "observed"),
        JournalEventType::StallDetected => ("stall", "observed"),
        JournalEventType::TurnEvaluation => ("evaluation", "recorded"),
        JournalEventType::DriftDetected => ("drift", "observed"),
        JournalEventType::AdaptiveScenarioApplied => ("scenario", "applied"),
        JournalEventType::AdaptivePerTurnApplied => ("adaptation", "applied"),
        JournalEventType::ConfigChange => ("mutation", "applied"),
        JournalEventType::VerificationCompleted => ("verification", "recorded"),
        _ => return None,
    };

    let mut evidence_refs = Vec::new();
    if event.turn.is_some() {
        evidence_refs.push(step_id(event));
    }
    if let Some(config_key) = event.config_key.as_ref() {
        evidence_refs.push(format!("config:{config_key}"));
    }

    Some(EvolutionRecord {
        id: format!("evolution:{}", step_id(event)),
        turn: event.turn,
        ts: event.ts.clone(),
        kind: kind.to_string(),
        status: status.to_string(),
        summary: summarize_event(event),
        evidence_refs,
    })
}

fn build_acceptance_surface(
    artifacts: &SessionArtifacts,
    runtime_checks: Vec<SelfSurfaceCheck>,
    run: &RunSurface,
    environment: &EnvironmentSurface,
    recent_steps: &[StepRecord],
    recent_decisions: &[DecisionRecord],
    evolution: &EvolutionSurface,
) -> AcceptanceSurface {
    let mut checks = runtime_checks;
    let persistence_error = session_persistence_error(artifacts);
    checks.push(SelfSurfaceCheck {
        name: "workspace_or_restore_present".to_string(),
        ok: artifacts.workspace.is_some() || artifacts.restored.is_some(),
        detail: format!(
            "workspace={} restore={}",
            artifacts.workspace.is_some(),
            artifacts.restored.is_some()
        ),
    });
    checks.push(SelfSurfaceCheck {
        name: "run_session_id_present".to_string(),
        ok: !run.session_id.is_empty(),
        detail: format!("session_id={}", run.session_id),
    });
    checks.push(SelfSurfaceCheck {
        name: "environment_sources_present".to_string(),
        ok: !environment.resolved_sources.is_empty(),
        detail: format!("sources={}", environment.resolved_sources.join(",")),
    });
    checks.push(SelfSurfaceCheck {
        name: "journal_present".to_string(),
        ok: !artifacts.journal_events.is_empty(),
        detail: format!("journal_events={}", artifacts.journal_events.len()),
    });
    checks.push(SelfSurfaceCheck {
        name: "session_persistence_healthy".to_string(),
        ok: persistence_error.is_none(),
        detail: persistence_error
            .map(str::to_string)
            .unwrap_or_else(|| "healthy".to_string()),
    });
    checks.push(SelfSurfaceCheck {
        name: "steps_present_when_journal_present".to_string(),
        ok: artifacts.journal_events.is_empty() || !recent_steps.is_empty(),
        detail: format!(
            "journal_events={} recent_steps={}",
            artifacts.journal_events.len(),
            recent_steps.len()
        ),
    });
    checks.push(SelfSurfaceCheck {
        name: "decision_records_have_selected_targets".to_string(),
        ok: recent_decisions.iter().all(|decision| {
            !decision.visible_tools.is_empty() || !decision.selected_skills.is_empty()
        }),
        detail: format!("decision_records={}", recent_decisions.len()),
    });
    let step_ids = recent_steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<BTreeSet<_>>();
    checks.push(SelfSurfaceCheck {
        name: "decision_source_refs_resolve".to_string(),
        ok: recent_decisions
            .iter()
            .all(|decision| match decision.source_step_id.as_deref() {
                Some(source) => step_ids.contains(source),
                None => true,
            }),
        detail: format!(
            "step_refs={} decisions={}",
            step_ids.len(),
            recent_decisions.len()
        ),
    });
    checks.push(SelfSurfaceCheck {
        name: "evolution_records_identified".to_string(),
        ok: evolution.records.iter().all(|record| !record.id.is_empty()),
        detail: format!("records={}", evolution.records.len()),
    });
    checks.push(SelfSurfaceCheck {
        name: "risk_flags_deduped".to_string(),
        ok: run.risk_flags.len() == run.risk_flags.iter().collect::<BTreeSet<_>>().len(),
        detail: format!("risk_flags={}", run.risk_flags.join(",")),
    });

    let failing_checks = checks
        .iter()
        .filter(|check| !check.ok)
        .map(|check| check.name.clone())
        .collect::<Vec<_>>();
    let ok = failing_checks.is_empty();
    let summary = if ok {
        format!("{} acceptance checks passed", checks.len())
    } else {
        format!(
            "{} of {} acceptance checks failed",
            failing_checks.len(),
            checks.len()
        )
    };

    AcceptanceSurface {
        ok,
        summary,
        failing_checks,
        checks,
    }
}

fn budget_state_from_artifacts(artifacts: &SessionArtifacts) -> Option<BudgetState> {
    latest_context_trace(artifacts)
        .and_then(|trace| trace.budget.as_ref())
        .map(|budget| BudgetState {
            max_tokens: budget.max_tokens,
            total_used: budget.total_used,
            remaining: budget.max_tokens.saturating_sub(budget.total_used),
            pressure: budget.budget_pressure,
            compression_triggered: budget.compression_triggered,
        })
}

fn latest_context_trace(artifacts: &SessionArtifacts) -> Option<&ContextTraceSignal> {
    artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.last_context_trace.as_ref())
        .or_else(|| {
            artifacts
                .restored
                .as_ref()
                .and_then(|restored| restored.last_context_trace.as_ref())
        })
}

fn build_risk_flags(
    budget: &Option<BudgetState>,
    health: &HealthData,
    runtime_checks: &[SelfSurfaceCheck],
    evolution: &EvolutionSurface,
    persistence_error: Option<&str>,
) -> Vec<String> {
    let mut flags = Vec::new();

    if budget
        .as_ref()
        .is_some_and(|budget| budget.pressure >= 0.85)
    {
        flags.push("high_token_pressure".to_string());
    }
    if !health.recent_failures.is_empty() {
        flags.push("recent_tool_failures".to_string());
    }
    if !health.health_avoidance_tools.is_empty() {
        flags.push("health_avoidance_tools".to_string());
    }
    if evolution
        .records
        .iter()
        .any(|record| record.kind == "stall")
    {
        flags.push("recent_stall".to_string());
    }
    if evolution
        .records
        .iter()
        .any(|record| record.kind == "drift")
    {
        flags.push("recent_drift".to_string());
    }
    if runtime_checks.iter().any(|check| !check.ok) {
        flags.push("runtime_config_issues".to_string());
    }
    if persistence_error.is_some() {
        flags.push("session_persistence_degraded".to_string());
    }

    flags.sort();
    flags.dedup();
    flags
}

fn build_pending_blockers(
    health: &HealthData,
    runtime_checks: &[SelfSurfaceCheck],
    extra: &[String],
) -> Vec<String> {
    let base = health
        .recent_failures
        .iter()
        .map(|failure| {
            format!(
                "{}{}",
                failure.tool,
                failure
                    .error
                    .as_deref()
                    .map(|error| format!(": {}", truncate(error, 80)))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    let checks = runtime_checks
        .iter()
        .filter(|check| !check.ok)
        .map(|check| format!("{}: {}", check.name, truncate(&check.detail, 80)))
        .collect::<Vec<_>>();
    build_pending_blockers_from_lists(
        &base,
        &checks
            .into_iter()
            .chain(extra.iter().cloned())
            .collect::<Vec<_>>(),
    )
}

fn build_pending_blockers_from_lists(primary: &[String], extra: &[String]) -> Vec<String> {
    let mut blockers = primary.to_vec();
    blockers.extend(extra.iter().cloned());
    blockers.sort();
    blockers.dedup();
    blockers.truncate(8);
    blockers
}

fn session_persistence_error(artifacts: &SessionArtifacts) -> Option<&str> {
    artifacts
        .workspace
        .as_ref()
        .and_then(|ws| ws.last_persistence_error.as_deref())
        .map(str::trim)
        .filter(|error| !error.is_empty())
}

fn persistence_pending_blockers(persistence_error: Option<&str>) -> Vec<String> {
    persistence_error
        .map(|error| vec![format!("session_persistence: {}", truncate(error, 80))])
        .unwrap_or_default()
}

fn merged_health_avoidance_tools(artifacts: &SessionArtifacts) -> Vec<String> {
    // Use BTreeSet for deduplication and sorted output
    // Only scan the last 100 events for performance (avoid full scan on large sessions)
    let events_to_scan = artifacts.journal_events.len().saturating_sub(100)..;
    let mut health_avoidance_tools = BTreeSet::new();

    for event in &artifacts.journal_events[events_to_scan] {
        let Some(names) = event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("health_avoidance_tools"))
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for name in names {
            if let Some(name) = name.as_str().map(str::trim).filter(|name| !name.is_empty()) {
                health_avoidance_tools.insert(name.to_string());
            }
        }
    }
    health_avoidance_tools.into_iter().collect()
}

fn merged_skills(workspace: Option<&WorkspaceMetadata>) -> Vec<String> {
    workspace
        .map(|ws| normalized_names(&ws.discovered_skills))
        .unwrap_or_default()
}

fn resolved_sources(artifacts: &SessionArtifacts) -> Vec<&'static str> {
    let mut resolved_sources = Vec::new();
    if artifacts.workspace.is_some() {
        resolved_sources.push("workspace");
    }
    if artifacts.restored.is_some() {
        resolved_sources.push("restore");
    }
    if !artifacts.journal_events.is_empty() {
        resolved_sources.push("journal");
    }
    resolved_sources
}

fn latest_active_skill(events: &[JournalEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        event
            .selected_skills
            .as_ref()
            .and_then(|skills| normalized_names(skills).into_iter().next())
    })
}

fn latest_event_text(events: &[JournalEvent], user: bool) -> Option<String> {
    events.iter().rev().find_map(|event| {
        let candidate = if user {
            event.user_input.as_deref()
        } else {
            event.assistant_output.as_deref()
        }?;
        let trimmed = candidate.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(truncate(trimmed, 160))
        }
    })
}

fn derived_status_from_events(events: &[JournalEvent]) -> String {
    match events.last().map(|event| &event.event_type) {
        Some(JournalEventType::TurnError | JournalEventType::Error) => "error".to_string(),
        Some(JournalEventType::SessionEnd) => "completed".to_string(),
        Some(_) => "active".to_string(),
        None => "unknown".to_string(),
    }
}

fn step_id(event: &JournalEvent) -> String {
    format!(
        "step:{}:{}:{}",
        event.turn.unwrap_or_default(),
        event_type_name(&event.event_type),
        event.ts
    )
}

fn actor_for_event(event: &JournalEvent) -> &'static str {
    match event.event_type {
        JournalEventType::AdaptiveScenarioApplied | JournalEventType::AdaptivePerTurnApplied => {
            "adaptive_engine"
        }
        JournalEventType::VerificationCompleted => "verifier",
        JournalEventType::ConfigChange | JournalEventType::GoalSteered => "self_mutation",
        JournalEventType::Turn | JournalEventType::TurnError if event.tool_calls.is_some() => {
            "agentic_loop"
        }
        _ => "runtime",
    }
}

fn phase_for_event_type(event_type: &JournalEventType) -> &'static str {
    match event_type {
        JournalEventType::Turn
        | JournalEventType::PlanProgress
        | JournalEventType::PlanEdit
        | JournalEventType::GoalSteered => "execute",
        JournalEventType::TurnError
        | JournalEventType::Error
        | JournalEventType::StallDetected
        | JournalEventType::DriftDetected => "reflect",
        JournalEventType::VerificationCompleted | JournalEventType::TurnEvaluation => "evaluate",
        JournalEventType::AdaptiveScenarioApplied
        | JournalEventType::AdaptivePerTurnApplied
        | JournalEventType::ConfigChange => "adapt",
        _ => "observe",
    }
}

fn summarize_event(event: &JournalEvent) -> String {
    match event.event_type {
        JournalEventType::Turn => event
            .user_input
            .as_deref()
            .map(|input| format!("turn completed for '{}'", truncate(input, 80)))
            .unwrap_or_else(|| "turn completed".to_string()),
        JournalEventType::TurnError => event
            .error
            .as_deref()
            .map(|error| format!("turn failed: {}", truncate(error, 80)))
            .unwrap_or_else(|| "turn failed".to_string()),
        JournalEventType::ConfigChange => format!(
            "{} -> {}",
            event.config_key.as_deref().unwrap_or("config"),
            truncate(event.config_value.as_deref().unwrap_or("updated"), 80)
        ),
        JournalEventType::AdaptiveScenarioApplied => event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("scenario").and_then(serde_json::Value::as_str))
            .map(|scenario| format!("adaptive scenario applied: {scenario}"))
            .unwrap_or_else(|| "adaptive scenario applied".to_string()),
        JournalEventType::AdaptivePerTurnApplied => {
            if let Some(metadata) = event.metadata.as_ref() {
                if let Some(changes) = metadata
                    .get("changes")
                    .and_then(serde_json::Value::as_array)
                {
                    let details = changes
                        .iter()
                        .filter_map(|change| change.get("key").and_then(serde_json::Value::as_str))
                        .collect::<Vec<_>>();
                    if !details.is_empty() {
                        return format!("adaptive per-turn changes: {}", details.join(", "));
                    }
                }
                if let Some(triggers) = metadata
                    .get("triggers")
                    .and_then(serde_json::Value::as_array)
                {
                    let details = triggers
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .collect::<Vec<_>>();
                    if !details.is_empty() {
                        return format!("adaptive triggers: {}", details.join(", "));
                    }
                }
            }
            "adaptive per-turn changes applied".to_string()
        }
        JournalEventType::GoalSteered => event
            .metadata
            .as_ref()
            .map(|metadata| {
                let source = metadata
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let new_goal = metadata
                    .get("new_goal")
                    .and_then(serde_json::Value::as_str)
                    .map(|goal| truncate(goal, 80))
                    .unwrap_or_else(|| "updated goal".to_string());
                match metadata
                    .get("previous_goal")
                    .and_then(serde_json::Value::as_str)
                {
                    Some(previous) => format!(
                        "goal steered via {source}: {} -> {new_goal}",
                        truncate(previous, 40)
                    ),
                    None => format!("goal steered via {source}: {new_goal}"),
                }
            })
            .unwrap_or_else(|| "goal steered".to_string()),
        JournalEventType::VerificationCompleted => event
            .error
            .as_deref()
            .map(|error| format!("verification completed with error: {}", truncate(error, 80)))
            .unwrap_or_else(|| {
                verification_event_view_from_event(event)
                    .map(|view| {
                        let outcome = match view.passed {
                            Some(true) => "verification passed",
                            Some(false) => "verification failed",
                            None => "verification recorded",
                        };
                        let mut detail = outcome.to_string();
                        if let Some(scope) = view.scope.as_deref() {
                            detail.push(' ');
                            detail.push_str(scope);
                        }
                        if let Some(target) = view.target.as_deref() {
                            detail.push(' ');
                            detail.push_str(target);
                        }
                        detail.push_str(" — ");
                        detail.push_str(&truncate(&view.summary, 80));
                        detail
                    })
                    .unwrap_or_else(|| "verification completed".to_string())
            }),
        JournalEventType::StallDetected => format!(
            "stall detected{}",
            event
                .stall_type
                .as_deref()
                .map(|kind| format!(" ({kind})"))
                .unwrap_or_default()
        ),
        JournalEventType::TurnEvaluation => event
            .metadata
            .as_ref()
            .map(|metadata| {
                let source = metadata
                    .get("source")
                    .and_then(|value| value.as_str())
                    .unwrap_or("runtime");
                let quality = metadata
                    .get("quality")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0);
                let confidence = metadata
                    .get("confidence")
                    .and_then(|value| value.as_f64())
                    .unwrap_or(0.0);
                format!("turn evaluation ({source}): q={quality:.2}, conf={confidence:.2}")
            })
            .unwrap_or_else(|| "turn evaluation recorded".to_string()),
        JournalEventType::DriftDetected => event
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("detail"))
            .map(compact_json_value)
            .map(|detail| format!("drift detected: {detail}"))
            .unwrap_or_else(|| "drift detected".to_string()),
        JournalEventType::Error => event
            .error
            .as_deref()
            .map(|error| format!("error: {}", truncate(error, 80)))
            .unwrap_or_else(|| "runtime error".to_string()),
        _ => event_type_name(&event.event_type),
    }
}

fn recent_event_previews(
    events: &[JournalEvent],
    limit: usize,
    kinds: &[JournalEventType],
) -> Vec<EventPreview> {
    events
        .iter()
        .rev()
        .filter(|event| kinds.iter().any(|kind| kind == &event.event_type))
        .take(limit)
        .map(event_preview)
        .collect()
}

fn event_preview(event: &JournalEvent) -> EventPreview {
    EventPreview {
        event_type: event_type_name(&event.event_type),
        ts: event.ts.clone(),
        turn: event.turn,
        error: event.error.clone(),
        tools_used: event
            .tools_used
            .as_deref()
            .map(normalized_names)
            .filter(|tools| !tools.is_empty()),
        metadata: event.metadata.clone(),
        user_input_preview: event.user_input.as_deref().map(|s| truncate(s, 160)),
        assistant_output_preview: event.assistant_output.as_deref().map(|s| truncate(s, 160)),
    }
}

fn recent_tool_failures(events: &[JournalEvent], limit: usize) -> Vec<ToolFailureView> {
    let mut failures = Vec::new();
    for event in events.iter().rev() {
        if let Some(tool_calls) = event.tool_calls.as_ref() {
            for call in tool_calls
                .iter()
                .rev()
                .filter(|call| call.was_executed() && !call.ok)
            {
                let tool = call.name.trim();
                if tool.is_empty() {
                    continue;
                }
                failures.push(ToolFailureView {
                    ts: event.ts.clone(),
                    tool: tool.to_string(),
                    error: call.error.clone(),
                    turn: event.turn,
                });
                if failures.len() >= limit {
                    return failures;
                }
            }
        }
    }
    failures
}

fn event_type_name(event_type: &JournalEventType) -> String {
    match event_type {
        JournalEventType::SessionStart => "session_start",
        JournalEventType::Turn => "turn",
        JournalEventType::TurnError => "turn_error",
        JournalEventType::Compact => "compact",
        JournalEventType::ConfigChange => "config_change",
        JournalEventType::Error => "error",
        JournalEventType::SessionEnd => "session_end",
        JournalEventType::StallDetected => "stall_detected",
        JournalEventType::Checkpoint => "checkpoint",
        JournalEventType::TurnGuardVerdict => "turn_guard_verdict",
        JournalEventType::TurnEvaluation => "turn_evaluation",
        JournalEventType::PlanProgress => "plan_progress",
        JournalEventType::SessionFork => "session_fork",
        JournalEventType::SyncMarker => "sync_marker",
        JournalEventType::DelegationStarted => "delegation_started",
        JournalEventType::DelegationSubRunStarted => "delegation_sub_run_started",
        JournalEventType::DelegationSubRunCompleted => "delegation_sub_run_completed",
        JournalEventType::DelegationRetry => "delegation_retry",
        JournalEventType::DelegationCompleted => "delegation_completed",
        JournalEventType::AgentSpawned => "agent_spawned",
        JournalEventType::AgentTerminated => "agent_terminated",
        JournalEventType::TranscriptItem => "transcript_item",
        JournalEventType::VerificationCompleted => "verification_completed",
        JournalEventType::PlanEdit => "plan_edit",
        JournalEventType::PlanLifecycle => "plan_lifecycle",
        JournalEventType::TaskLifecycle => "task_lifecycle",
        JournalEventType::GoalSteered => "goal_steered",
        JournalEventType::ApprovalRequired => "approval_required",
        JournalEventType::ApprovalDecision => "approval_decision",
        JournalEventType::ApprovalTimeout => "approval_timeout",
        JournalEventType::AskUserPrompted => "ask_user_prompted",
        JournalEventType::AskUserResponse => "ask_user_response",
        JournalEventType::PermissionAudit => "permission_audit",
        JournalEventType::ExecutionBoundaryOpened => "execution_boundary_opened",
        JournalEventType::ExecutionBoundaryCommitted => "execution_boundary_committed",
        JournalEventType::ExecutionBoundaryAborted => "execution_boundary_aborted",
        JournalEventType::ContextAssemblyRecorded => "context_assembly_recorded",
        JournalEventType::DriftDetected => "drift_detected",
        JournalEventType::AdaptiveScenarioApplied => "adaptive_scenario_applied",
        JournalEventType::AdaptivePerTurnApplied => "adaptive_per_turn_applied",
        JournalEventType::InterruptionRecorded => "interruption_recorded",
        JournalEventType::CompactionRetry => "compaction_retry",
        JournalEventType::LlmRound => "llm_round",
        JournalEventType::LlmRequestFull => "llm_request_full",
        JournalEventType::LlmResponseFull => "llm_response_full",
        JournalEventType::SessionMemoryExtraction => "session_memory_extraction",
        JournalEventType::PipelineFeedback => "pipeline_feedback",
        JournalEventType::PipelineAlert => "pipeline_alert",
        JournalEventType::PipelineCompactionAudit => "pipeline_compaction_audit",
        JournalEventType::Bootstrap => "bootstrap",
        JournalEventType::TraceSpan => "trace_span",
        JournalEventType::ToolCallError => "tool_call_error",
    }
    .to_string()
}

fn truncate(input: &str, max_chars: usize) -> String {
    let head: String = input.chars().take(max_chars).collect();
    if input.chars().count() > max_chars {
        format!("{head}...")
    } else {
        head
    }
}

fn compact_json_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::String(inner) => truncate(inner, 80),
        _ => truncate(&value.to_string(), 80),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::{JournalDirGuard, JournalWriter, ToolCallRecord};

    const REAL_SESSION_0AC769_FIXTURE: &str =
        include_str!("../fixtures/real_session_0ac769_min.jsonl");

    struct StubRuntimeSupport;

    impl SelfSurfaceRuntimeSupport for StubRuntimeSupport {
        fn tool_names(&self) -> Vec<String> {
            vec![
                "".to_string(),
                "bash".to_string(),
                " bash ".to_string(),
                "rg".to_string(),
                "web_fetch".to_string(),
            ]
        }

        fn budget_config(&self, _: Option<&str>) -> Result<BudgetConfig, String> {
            Ok(BudgetConfig {
                compression_threshold: 0.7,
                max_turn_input_tokens: 120000,
                compression_threshold_min: 0.5,
                compression_threshold_max: 0.9,
            })
        }

        fn runtime_checks(&self, _: Option<&str>) -> Vec<SelfSurfaceCheck> {
            vec![SelfSurfaceCheck {
                name: "runtime_config_parse".to_string(),
                ok: true,
                detail: "stubbed".to_string(),
            }]
        }
    }

    #[derive(Clone)]
    struct StubArtifactLoader {
        artifacts: LoadedSelfSurfaceArtifacts,
    }

    #[async_trait]
    impl SelfSurfaceArtifactLoader for StubArtifactLoader {
        async fn load_artifacts(
            &self,
            _session_id: &str,
        ) -> Result<LoadedSelfSurfaceArtifacts, String> {
            Ok(self.artifacts.clone())
        }
    }

    fn append_turn_event(session_id: &str, turn: u32) {
        JournalWriter::new(session_id)
            .unwrap()
            .append(&JournalEvent::turn(
                Some(session_id),
                turn,
                Some("gpt-5.4"),
                "continue",
                "done",
                1,
                20,
                30,
                50,
            ))
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_rejects_invalid_session_id() {
        let service = LocalSelfSurfaceService::new();
        let error = service
            .snapshot("../bad", 5)
            .await
            .expect_err("invalid session id should error");
        assert!(error.contains("invalid session id"));
    }

    #[tokio::test]
    async fn snapshot_surfaces_session_journal_io_error() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-journal-error";
        let journal_path = crate::session_journal::journal_file_path(session_id);
        std::fs::create_dir_all(&journal_path).unwrap();

        let service = LocalSelfSurfaceService::new();
        let error = service
            .snapshot(session_id, 5)
            .await
            .expect_err("journal io error should surface");
        assert!(error.contains("failed to read session journal"));
    }

    fn sample_verification_criterion(id: &str) -> crate::verification::VerificationCriterion {
        crate::verification::VerificationCriterion {
            id: id.to_string(),
            description: format!("verify {id}"),
            verifier: crate::verification::VerifierKind::Command {
                cmd: "true".to_string(),
                expected_exit: 0,
            },
            required: true,
            timeout_sec: 30,
            global_only: false,
        }
    }

    fn sample_verification_result(
        criterion_id: &str,
        passed: bool,
    ) -> crate::verification::VerificationResult {
        crate::verification::VerificationResult {
            criterion_id: criterion_id.to_string(),
            passed,
            evidence: if passed {
                "passed".to_string()
            } else {
                "failed".to_string()
            },
            expected: "pass".to_string(),
            duration_ms: 10,
            error: None,
        }
    }

    fn sample_contract(status: ContractStatus, stage: SubtaskStage) -> TaskContract {
        TaskContract {
            contract_id: "contract-1".to_string(),
            task_id: "task-1".to_string(),
            goal: "ship self surface".to_string(),
            scope: crate::durable_task::TaskScope::default(),
            subtasks: vec![crate::durable_task::DurableSubtask {
                id: "subtask-1".to_string(),
                title: "finish verifier".to_string(),
                stage,
                criteria: vec![sample_verification_criterion("subtask-check")],
                ..Default::default()
            }],
            global_verification: vec![sample_verification_criterion("global-check")],
            version: 1,
            status,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            domain_hint: None,
            task_type: None,
            last_global_results: Vec::new(),
        }
    }

    #[tokio::test]
    async fn local_service_builds_snapshot_from_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-snapshot";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.plan_goal = Some("ship self surface".to_string());
        ws.discovered_skills = vec![
            " ".to_string(),
            " goal-driven-evolution ".to_string(),
            "goal-driven-evolution".to_string(),
        ];
        ws.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-2".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_surface: None,
            memory: None,
            history: None,
            budget: Some(crate::session_workspace::ContextTraceBudgetSignal {
                max_tokens: 10000,
                total_used: 8300,
                budget_pressure: 0.83,
                compression_triggered: false,
            }),
            timing: None,
            explanations: vec!["stable".to_string()],
        });
        session_workspace::write_workspace(&ws).unwrap();

        JournalWriter::new(session_id)
            .unwrap()
            .append(&JournalEvent {
                event_type: JournalEventType::Turn,
                ts: Utc::now().to_rfc3339(),
                session_id: Some(session_id.to_string()),
                producer_scope: None,
                turn: Some(2),
                agentic_step: None,
                model: Some("gpt-5.4".to_string()),
                user_input: Some("continue".to_string()),
                assistant_output: Some("done".to_string()),
                tool_count: Some(1),
                tokens_in: Some(20),
                tokens_out: Some(30),
                duration_ms: Some(50),
                error: None,
                config_key: None,
                config_value: None,
                turns_compacted: None,
                facts_stored: None,
                visible_tools: Some(vec!["bash".to_string()]),
                selected_skills: Some(vec![
                    " ".to_string(),
                    " goal-driven-evolution ".to_string(),
                    "goal-driven-evolution".to_string(),
                ]),
                tools_used: Some(vec!["bash".to_string()]),
                tool_calls: Some(vec![ToolCallRecord {
                    name: "bash".to_string(),
                    ok: true,
                    ms: 50,
                    error: None,
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                    result_preview: None,
                    file_path: None,
                    surgically_removed: None,
                    original_tool_name: None,
                    ..Default::default()
                }]),
                tool_outcomes: None,
                budget_used: Some(8300),
                budget_pressure: Some(0.83),
                stall_type: None,
                metadata: None,
                plan_subtask_id: None,
                ttft_ms: None,
                context_ms: None,
                cache_read_tokens: None,
                cache_creation_tokens: None,
                memoria_ms: None,
                session_lineage: None,
                coordination: None,
                transcript_item: None,
                edge_policy: None,
                context_assembly_trace: None,
                routing_domain_hint: Some("code".to_string()),
                entity_learn_skipped_no_domain: false,
                round: None,
                tool_calls_returned: None,
                offset_ms: None,
                llm_rounds: None,
                total_llm_ms: None,
                total_tool_ms: None,
                parent_event_id: None,
                git_head: None,
                git_branch: None,
            })
            .unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let snapshot = service.snapshot(session_id, 10).await.unwrap();

        assert_eq!(snapshot.run.goal.as_deref(), Some("ship self surface"));
        assert_eq!(
            snapshot.run.active_skill.as_deref(),
            Some("goal-driven-evolution")
        );
        assert_eq!(snapshot.environment.available_tools, 3);
        assert_eq!(
            snapshot.environment.tool_names,
            vec![
                "bash".to_string(),
                "rg".to_string(),
                "web_fetch".to_string()
            ]
        );
        assert_eq!(
            snapshot.environment.discovered_skills,
            vec!["goal-driven-evolution".to_string()]
        );
        assert_eq!(
            snapshot.run.totals.total_tool_calls, 1,
            "durable turn tool records must project into the self snapshot"
        );
        assert_eq!(snapshot.run.totals.failure_events, 0);
        assert_eq!(snapshot.recent_steps.len(), 2);
        assert_eq!(snapshot.recent_decisions.len(), 1);
        assert!(snapshot.acceptance.ok);
    }

    #[test]
    fn self_surface_normalizes_tool_and_skill_names() {
        let session_id = "svc-self-normalized-names";
        let mut event = JournalEvent::turn(
            Some(session_id),
            1,
            Some("gpt-5.4"),
            "continue",
            "done",
            1,
            20,
            30,
            50,
        );
        event.visible_tools = Some(vec![
            "".to_string(),
            " rg ".to_string(),
            "rg".to_string(),
            " bash".to_string(),
        ]);
        event.tools_used = Some(vec![
            " bash ".to_string(),
            "".to_string(),
            "bash".to_string(),
            "web_fetch".to_string(),
        ]);
        event.selected_skills = Some(vec![
            " review ".to_string(),
            "review".to_string(),
            " ".to_string(),
        ]);
        event.tool_calls = Some(vec![
            ToolCallRecord {
                name: " ".to_string(),
                ok: false,
                ms: 1,
                error: Some("blank name".to_string()),
                ..Default::default()
            },
            ToolCallRecord {
                name: " bash ".to_string(),
                ok: false,
                ms: 2,
                error: Some("failed".to_string()),
                ..Default::default()
            },
        ]);
        event.metadata = Some(serde_json::json!({
            "health_avoidance_tools": [" flaky_http ", "", "flaky_http"]
        }));

        let steps = build_recent_steps(std::slice::from_ref(&event), 5);
        assert_eq!(
            steps[0].visible_tools,
            vec!["rg".to_string(), "bash".to_string()]
        );
        assert_eq!(
            steps[0].used_tools,
            vec!["bash".to_string(), "web_fetch".to_string()]
        );
        assert_eq!(steps[0].selected_skills, vec!["review".to_string()]);

        let decision = decision_from_event(&event).expect("dirty names still contain targets");
        assert_eq!(
            decision.visible_tools,
            vec!["rg".to_string(), "bash".to_string()]
        );
        assert_eq!(decision.selected_skills, vec!["review".to_string()]);

        let preview = event_preview(&event);
        assert_eq!(
            preview.tools_used,
            Some(vec!["bash".to_string(), "web_fetch".to_string()])
        );
        assert_eq!(recent_tool_failures(&[event.clone()], 5)[0].tool, "bash");

        let mut workspace =
            WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        workspace.last_context_trace = Some(ContextTraceSignal {
            turn_id: "turn-1".to_string(),
            captured_at: Some(Utc::now().to_rfc3339()),
            tool_surface: Some(crate::session_workspace::ContextTraceToolSurface {
                tools_available: 4,
                visible_tools: vec![
                    " ".to_string(),
                    " rg ".to_string(),
                    "rg".to_string(),
                    "bash".to_string(),
                ],
                surface_scope: "latest_round".to_string(),
                latency_ms: 1,
            }),
            memory: None,
            history: None,
            budget: None,
            timing: None,
            explanations: Vec::new(),
        });
        let artifacts = LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace: Some(workspace),
            restored: None,
            journal_events: vec![event],
            latest_full_context_trace: None,
        };

        let decisions = build_recent_decisions(&artifacts, 5);
        assert_eq!(
            decisions.last().unwrap().visible_tools,
            vec!["rg".to_string(), "bash".to_string()]
        );
        assert_eq!(
            merged_health_avoidance_tools(&artifacts),
            vec!["flaky_http".to_string()]
        );
    }

    #[tokio::test]
    async fn self_surface_serializes_health_avoidance_tools_without_legacy_keys() {
        let session_id = "svc-self-health-surface";
        let artifacts = LoadedSelfSurfaceArtifacts {
            session_id: session_id.to_string(),
            workspace: None,
            restored: Some(RestoredSession {
                session_id: session_id.to_string(),
                blocked_tools: vec!["hard_restricted_tool".to_string()],
                ..Default::default()
            }),
            journal_events: vec![JournalEvent::turn_guard_verdict(
                Some(session_id),
                3,
                "warning",
                &[],
                &["flaky_http".to_string()],
                &["flaky_http".to_string()],
                false,
                0,
                3,
                0,
                &[],
                0,
                0,
            )],
            latest_full_context_trace: None,
        };
        let service = LocalSelfSurfaceService::new()
            .with_runtime_support(Arc::new(StubRuntimeSupport))
            .with_artifact_loader(Arc::new(StubArtifactLoader { artifacts }));

        let snapshot = service.snapshot(session_id, 10).await.unwrap();
        assert_eq!(
            snapshot.environment.health_avoidance_tools,
            vec!["flaky_http".to_string()]
        );
        assert!(
            !snapshot
                .environment
                .health_avoidance_tools
                .contains(&"hard_restricted_tool".to_string()),
            "checkpoint hard restrictions must not be reported as health-avoidance tools"
        );
        assert!(
            snapshot
                .run
                .risk_flags
                .contains(&"health_avoidance_tools".to_string())
        );

        let environment_json = serde_json::to_value(&snapshot.environment).unwrap();
        assert_eq!(
            environment_json["health_avoidance_tools"],
            serde_json::json!(["flaky_http"])
        );
        assert!(environment_json.get("health_avoidance_count").is_none());

        let health = service
            .surface(session_id, SelfSurfaceDimension::Health, 10)
            .await
            .unwrap();
        let health_json = serde_json::to_value(&health).unwrap();
        assert_eq!(
            health_json["body"]["health_avoidance_tools"],
            serde_json::json!(["flaky_http"])
        );
        assert!(health_json["body"].get("blocked_tools").is_none());
        assert!(health_json["body"].get("health_avoidance_count").is_none());
    }

    #[tokio::test]
    async fn snapshot_surfaces_real_session_fixture_turn_evaluation_and_prefetch_symptom() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = crate::session_journal::journal_file_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let snapshot = service.snapshot(session_id, 20).await.unwrap();

        assert_eq!(
            snapshot.run.latest_user_request.as_deref(),
            Some("review b273c589a73799070a71f4cfc6d55349b534d8d1")
        );
        assert!(
            snapshot
                .run
                .latest_assistant_output
                .as_deref()
                .unwrap_or("")
                .contains("not b273c589"),
            "snapshot should surface the wrong-prefetch symptom from the real session"
        );
        assert_eq!(snapshot.recent_steps.len(), 14);
        assert_eq!(
            snapshot
                .recent_steps
                .iter()
                .filter(|step| step.event_type == "llm_round")
                .count(),
            7,
            "snapshot should preserve the 7-round loop"
        );
        assert!(
            snapshot.recent_steps.iter().any(|step| {
                step.event_type == "turn_evaluation" && step.summary.contains("q=0.50, conf=0.70")
            }),
            "turn evaluation summary should be visible in recent steps"
        );
    }

    #[tokio::test]
    async fn journal_surface_exposes_real_session_fixture_previews() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "0ac7696c-8a67-4e9f-b7bb-88b3bf7b59a0";
        let path = crate::session_journal::journal_file_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, REAL_SESSION_0AC769_FIXTURE).unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let surface = service
            .surface(session_id, SelfSurfaceDimension::Journal, 20)
            .await
            .unwrap();

        let SelfSurfaceResponse::Journal(journal) = surface else {
            panic!("expected journal surface");
        };
        assert_eq!(journal.total_events, 14);
        assert_eq!(journal.returned, 14);
        assert_eq!(
            journal
                .events
                .iter()
                .filter(|event| event.event_type == "llm_round")
                .count(),
            7,
            "journal surface should preserve the 7-round loop"
        );
        assert!(
            journal.events.iter().any(|event| {
                event.event_type == "turn"
                    && event
                        .assistant_output_preview
                        .as_deref()
                        .unwrap_or("")
                        .contains("not b273c589")
            }),
            "journal surface should preview the wrong-prefetch symptom"
        );
        assert!(
            journal.events.iter().any(|event| {
                event.event_type == "turn_evaluation"
                    && event
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("tool_call_count"))
                        .and_then(|value| value.as_u64())
                        == Some(12)
            }),
            "journal surface should expose the persisted turn_evaluation metadata"
        );
    }

    #[tokio::test]
    async fn snapshot_run_goal_prefers_active_plan_goal() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-snapshot-plan-goal";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.plan_goal = Some("execute migration plan".to_string());
        session_workspace::write_workspace(&ws).unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let snapshot = service.snapshot(session_id, 10).await.unwrap();

        assert_eq!(snapshot.run.goal.as_deref(), Some("execute migration plan"));
    }

    #[tokio::test]
    async fn snapshot_surfaces_session_persistence_degradation_as_risk_and_blocker() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-persistence-degraded";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        append_turn_event(session_id, 1);

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let snapshot = service.snapshot(session_id, 10).await.unwrap();

        assert_eq!(
            snapshot.run.persistence_error.as_deref(),
            Some("failed to append turn event")
        );
        assert!(
            snapshot
                .run
                .risk_flags
                .contains(&"session_persistence_degraded".to_string())
        );
        assert!(snapshot
            .run
            .pending_blockers
            .iter()
            .any(|blocker| blocker.contains("session_persistence: failed to append turn event")));
        assert!(!snapshot.acceptance.ok);
        assert!(
            snapshot
                .acceptance
                .failing_checks
                .contains(&"session_persistence_healthy".to_string())
        );
    }

    #[tokio::test]
    async fn self_surface_variants_expose_persistence_error_field() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-surface-persistence-field";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.last_persistence_error = Some("failed to append turn event".to_string());
        session_workspace::write_workspace(&ws).unwrap();
        append_turn_event(session_id, 1);

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));

        for dimension in [
            SelfSurfaceDimension::Profile,
            SelfSurfaceDimension::Goal,
            SelfSurfaceDimension::Trace,
            SelfSurfaceDimension::Budget,
            SelfSurfaceDimension::Signals,
            SelfSurfaceDimension::Health,
            SelfSurfaceDimension::Journal,
            SelfSurfaceDimension::Verify,
        ] {
            let surface = service.surface(session_id, dimension, 10).await.unwrap();
            let persistence_error = match surface {
                SelfSurfaceResponse::Profile(body) => body.persistence_error,
                SelfSurfaceResponse::Goal(body) => body.persistence_error,
                SelfSurfaceResponse::Trace(body) => body.persistence_error,
                SelfSurfaceResponse::Budget(body) => body.persistence_error,
                SelfSurfaceResponse::Signals(body) => body.persistence_error,
                SelfSurfaceResponse::Health(body) => body.persistence_error,
                SelfSurfaceResponse::Journal(body) => body.persistence_error,
                SelfSurfaceResponse::Verify(body) => body.persistence_error,
                SelfSurfaceResponse::Snapshot(_) => unreachable!("snapshot not requested"),
            };
            assert_eq!(
                persistence_error.as_deref(),
                Some("failed to append turn event"),
                "dimension {dimension:?} should surface persistence degradation explicitly"
            );
        }
    }

    #[tokio::test]
    async fn verify_surface_exposes_runtime_checks() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-verify";
        let ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        session_workspace::write_workspace(&ws).unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let surface = service
            .surface(session_id, SelfSurfaceDimension::Verify, 10)
            .await
            .unwrap();

        let SelfSurfaceResponse::Verify(verify) = surface else {
            panic!("expected verify surface");
        };
        assert!(!verify.ok, "journal is missing so acceptance should fail");
        assert!(
            verify
                .checks
                .iter()
                .any(|check| check.name == "runtime_config_parse")
        );
    }

    #[tokio::test]
    async fn verify_surface_reports_pending_contract_objective() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-objective-pending";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.plan_goal = Some("ship self surface".to_string());
        ws.contract_json = Some(
            serde_json::to_string(&sample_contract(
                ContractStatus::Active,
                SubtaskStage::Pending,
            ))
            .unwrap(),
        );
        session_workspace::write_workspace(&ws).unwrap();
        append_turn_event(session_id, 2);

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let surface = service
            .surface(session_id, SelfSurfaceDimension::Verify, 10)
            .await
            .unwrap();

        let SelfSurfaceResponse::Verify(verify) = surface else {
            panic!("expected verify surface");
        };
        assert!(!verify.ok);
        assert!(verify.acceptance_ok);
        assert!(!verify.objective_ok);
        assert_eq!(verify.objective.contract_status.as_deref(), Some("active"));
        assert_eq!(verify.objective.subtasks_total, 1);
        assert_eq!(verify.objective.subtasks_satisfied, 0);
        assert_eq!(verify.objective.subtasks_incomplete, 1);
        assert_eq!(
            verify.objective.summary,
            "objective pending: 0/1 subtasks satisfied, 0 blocked, 0 failed"
        );
    }

    #[tokio::test]
    async fn verify_surface_reports_verified_contract_objective() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-objective-verified";
        let mut ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        ws.plan_goal = Some("ship self surface".to_string());
        let mut contract = sample_contract(ContractStatus::Completed, SubtaskStage::Verified);
        contract.last_global_results = vec![sample_verification_result("global-check", true)];
        ws.contract_json = Some(serde_json::to_string(&contract).unwrap());
        session_workspace::write_workspace(&ws).unwrap();
        append_turn_event(session_id, 2);
        JournalWriter::new(session_id)
            .unwrap()
            .append(&JournalEvent::verification_completed(
                Some(session_id),
                2,
                "subtask-1",
                "global",
                true,
                &serde_json::json!([sample_verification_result("global-check", true)]),
            ))
            .unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let surface = service
            .surface(session_id, SelfSurfaceDimension::Verify, 10)
            .await
            .unwrap();

        let SelfSurfaceResponse::Verify(verify) = surface else {
            panic!("expected verify surface");
        };
        assert!(verify.ok);
        assert!(verify.acceptance_ok);
        assert!(verify.objective_ok);
        assert_eq!(
            verify.objective.contract_status.as_deref(),
            Some("completed")
        );
        assert_eq!(verify.objective.global_checks_passed, 1);
        assert_eq!(
            verify
                .objective
                .latest_verification
                .as_ref()
                .and_then(|event| event.passed),
            Some(true)
        );
        assert_eq!(
            verify.objective.summary,
            "objective satisfied: 1/1 subtasks complete, 1/1 global checks passed"
        );
    }

    #[tokio::test]
    async fn evolution_surface_verification_records_include_pass_fail_detail() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = JournalDirGuard::new(temp.path());
        let session_id = "svc-self-evolution-verification";
        let ws = WorkspaceMetadata::with_context(session_id, "gpt-5.4", "/repo", Some("main"));
        session_workspace::write_workspace(&ws).unwrap();
        append_turn_event(session_id, 2);
        JournalWriter::new(session_id)
            .unwrap()
            .append(&JournalEvent::verification_completed(
                Some(session_id),
                3,
                "subtask-1",
                "global",
                false,
                &serde_json::json!([sample_verification_result("integration-tests", false)]),
            ))
            .unwrap();

        let service =
            LocalSelfSurfaceService::new().with_runtime_support(Arc::new(StubRuntimeSupport));
        let snapshot = service.snapshot(session_id, 10).await.unwrap();
        let verification = snapshot
            .evolution
            .records
            .iter()
            .find(|record| record.kind == "verification")
            .expect("verification evolution record");

        assert_eq!(verification.status, "recorded");
        assert!(
            verification
                .summary
                .contains("verification failed global subtask-1")
        );
        assert!(verification.summary.contains("integration-tests"));
    }
}
