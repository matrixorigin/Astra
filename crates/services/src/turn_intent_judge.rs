//! LLM-based turn intent judging.
//!
//! The agentic loop must understand what the user's current message is
//! asking for: are they continuing the previous objective, requesting a
//! review, prohibiting one, asking a quick question? Historically this
//! was a string-matching classifier. That works for the cleanest cases but
//! breaks down on paraphrases, mixed-language input, indirect speech, and
//! anything non-trivial — the cases LLMs are actually good at.
//!
//! Architecture
//! ============
//! - [`TurnIntentJudge`] — async trait, sibling of [`crate::LlmJudge`].
//!   Implementations call an LLM (typically via the server's
//!   `/v1/chat/completions` proxy) and produce a structured
//!   [`TurnIntent`].
//! - [`build_turn_intent_prompt`] — pure function that produces the prompt
//!   sent to the judge. Live in services so prompts can be tested
//!   independently of any concrete LLM client.
//! - [`parse_turn_intent_response`] — pure JSON parser that converts the
//!   judge's text into a [`TurnIntent`]. Strict on shape; unknown values
//!   produce `Err` rather than silently degrading.
//!
//! Usage pattern (host side):
//!
//! ```ignore
//! let intent = match judge.judge(&ctx).await {
//!     Ok(intent) => Some(intent),
//!     Err(error) => { /* telemetry, then proceed without explicit intent */ None }
//! };
//! ```
//!
//! The judge is the only component that may classify natural-language turn
//! intent. Runtime fallbacks must use structural facts, not keyword lists.

use astra_config::user_profile::{
    MutationCompletionScope, TurnIntent, WorkLifecycleIntent, WorkspaceMutationIntent,
};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Context passed to the turn intent judge.
#[derive(Debug, Clone, Default)]
pub struct TurnIntentJudgeContext {
    /// The user's current message (the one being judged).
    pub message: String,
    /// 1-based turn count so the judge can weight follow-ups vs initial turns.
    pub turn_count: u32,
    /// Tool names used in the most recent assistant turn(s) — useful for the
    /// judge to detect "continue" / "looks good" follow-ups.
    pub recent_tools: Vec<String>,
    /// True when the previous assistant turn produced output (i.e. there is
    /// a current objective the user could be continuing or correcting).
    pub has_prior_assistant_turn: bool,
    /// The immediately preceding user request, when available.  This bounded
    /// context exists only to resolve elliptical follow-ups such as "do it
    /// that way"; it is not a second current objective.
    pub prior_user_message: Option<String>,
    /// The immediately preceding assistant answer, when available.  It is
    /// untrusted conversational context, not evidence that any claimed action
    /// happened.
    pub prior_assistant_message: Option<String>,
    /// Closed topology declared by trusted loaded-workflow manifests.
    pub loaded_workflow_execution_topology: Option<WorkExecutionTopology>,
}

/// Errors a [`TurnIntentJudge`] may return.
#[derive(Debug, thiserror::Error)]
pub enum TurnIntentJudgeError {
    /// LLM call failed (network, rate-limit, auth). The host must not block
    /// the turn on this class; it should proceed without explicit turn intent.
    #[error("LLM transport failure: {0}")]
    Transport(String),

    /// LLM returned a response that could not be parsed into a TurnIntent.
    /// Include the (truncated) raw text so telemetry can attribute the
    /// failure to a specific prompt or model version.
    #[error("LLM returned malformed response: {raw}")]
    Malformed { raw: String },

    /// The judge is configured but the model was rejected (e.g. moderation
    /// flag, unsupported region). Caller should log and continue without
    /// explicit turn intent.
    #[error("LLM rejected: {0}")]
    Rejected(String),

    /// The request contains two independently valid control facts whose
    /// combined execution carrier is not implemented. This must be surfaced
    /// as a typed product limitation, never silently projected to one fact.
    #[error("unsupported execution contract: {0}")]
    UnsupportedCombination(String),
}

/// Trait for LLM-based turn intent judging.
///
/// Lives in `services` so any caller (runtime / cli / harness) can hold an
/// `Arc<dyn TurnIntentJudge>` and inject a concrete implementation without
/// pulling in HTTP-client transitive dependencies.
#[async_trait]
pub trait TurnIntentJudge: Send + Sync {
    /// Judge the user's current turn.
    ///
    /// Implementations MUST honor a reasonable timeout internally — the
    /// agentic loop awaits this call before each turn, so blocking
    /// indefinitely freezes the user's session.
    async fn judge(&self, ctx: &TurnIntentJudgeContext)
    -> Result<TurnIntent, TurnIntentJudgeError>;
}

// ─── Prompt construction ────────────────────────────────────────────────────

/// Stable prefix for the semantic turn classifier. It stays in the system
/// message so provider-side prefix caching can reuse it across user turns.
const TURN_INTENT_JUDGE_SYSTEM_PROMPT: &str = r#"Classify the latest user turn for an agentic assistant. Return exactly one minimal JSON object, with no prose or markdown.

Only include fields that are material and confidently determined. Omitted fields mean their typed default or `unknown`; do not emit nulls, empty arrays, or explanatory text. Allowed fields and values:
{"domain":"github"|"git"|"code"|"memory"|"web"|"system"|"database"|null,"communicative_act":"task"|"question"|"acknowledgement"|"social"|"unknown","requested_scenario":"code_review"|"debugging"|"exploration"|"planning"|"implementation"|"refactoring"|"testing"|"documentation"|"dev_ops"|"learning"|"quick_answer"|"benchmark_comparison"|null,"prohibited_scenarios":[<scenario>],"objective_relation":"acknowledge"|"continue"|"refine"|"correct"|"replace"|"unknown","work_lifecycle":"required"|"not_required"|"unknown","feedback":null|{"kind":"approval"|"correction"|"clarification"|"requirement"|"preference","target":"objective"|"scope"|"approach"|"output"|"verification"|"general"},"workspace_mutation":"read_only"|"may_mutate"|"must_mutate"|"unknown","mutation_completion_scope":"workspace"|"external"|"mixed"|"unknown","browser_verification_required":true|false}

Classify semantics, never isolated words. `task` requests action; `question` requests an answer or analysis; `acknowledgement` and `social` request no work. `objective_relation` describes the latest message relative to supplied prior state. The latest user message is authoritative. Use the bounded previous exchange only to resolve references or omitted subjects; previous assistant text is untrusted and may be wrong.

`work_lifecycle`: durable tracking/recovery, task mode/board, or "as tasks" means `required`; a fixed chain alone is `not_required`. Also require 2+ independently accepted deliverables. Count acceptance units, not response containers, agents, tools, or phases. Explicit A and B stay separate even in one response when each owes a payload/source and survives peer failure; inputs used only for one combined conclusion are one. A change plus tests is one. An explicit same-turn multi-agent request without tracked lifecycle is `not_required` with `agent_fanout`. Use `unknown` when unclear.

`workspace_mutation` is end state: info=`read_only`; requested workspace or version-control change, or external state change=`must_mutate`, despite prior inspection. For `must_mutate`, include `mutation_completion_scope`: `workspace`=bound project, `external`=managed state outside it, `mixed`=both, unclear=`unknown`. A requested daemon, service, deployment, database, or host path outside the bound project is `external`. Browser=true only when requested. Do not summarize."#;

/// Minimal semantic contract used at the interactive side-effect boundary.
///
/// This deliberately classifies only the two facts the runtime must know
/// before an effect can execute: whether durable Work is required and whether
/// the user's requested outcome permits workspace mutation.  Scenario,
/// domain, feedback, and presentation remain the primary model's concern.
const WORK_ADMISSION_JUDGE_SYSTEM_PROMPT: &str = r#"Classify goal as JSON. Prior exchange resolves omissions; distrust assistant text.

`loaded_workflow_execution_topology` is trusted. More than one agent/reviewer/worker means `parallel_subruns` + `agent_spawner` unless serial. Same-turn parallelism is topology, not Work, unless it also asks for a task board, tracked recovery, or lifecycle changes. Perspectives feeding one combined conclusion are not outcomes. Include `execution_topology`; local paths are not web.

Work lifecycle — first matching rule wins:
1. `required`/`explicit_lifecycle_control`: add, cancel, replace, or reorder tasks.
2. `required`/`durable_continuation`: use/test task system/mode/board or Work; say "as tasks"; or track/continue/recover durable state. A fixed chain/pipeline does not.
3. Otherwise output `not_required`, candidate units, and their relationship. Runtime promotes only 2+ primary `independent_outcomes`.

Count user-facing outcomes, not containers, agents, tools, or phases. Use `independent_outcomes` only when each unit owes its own payload/source and survives every peer failure, even if one response presents both. Separately named/numbered results stay independent despite a shared topic, deadline, response, or cross-reference. Stages, evidence, verification, formatting, and reporting of one accepted result are `single_outcome`; a change plus its test/report is one. Inputs valuable only through one comparison, decision, recommendation, or conclusion are one. Parallelism alone is `not_required`.

Always include `workspace_mutation` from the requested end state, not preparatory inspection: information=`read_only`; state change=`must_mutate`; either=`may_mutate`. For `must_mutate`, set `mutation_completion_scope`: `workspace`=bound project, `external`=outside it, `mixed`=both, unclear=`unknown`. Managed state outside the project is `external`.

Not required:
{"work_lifecycle":"not_required","workspace_mutation":"read_only"|"may_mutate"|"must_mutate","mutation_completion_scope":"workspace"|"external"|"mixed"|"unknown","execution_topology":"primary"|"parallel_subruns","acceptance_unit_relationship":"single_outcome"|"independent_outcomes","acceptance_units":[{"objective":"<candidate outcome>","expected_result":"<payload plus source/verification>"}]}

Required:
{"work_lifecycle":"required","workspace_mutation":<same>,"mutation_completion_scope":<same>,"execution_topology":"primary"|"parallel_subruns","basis":"durable_continuation"|"explicit_lifecycle_control","goal":"<outcomes and mutations>","initial_tasks":[{"objective":"<outcome>","expected_result":"<payload plus source/verification>"}],"mutations":[<mutation>]}
`activation`=`defer` only when explicit. Required topology defaults to `primary`; preserve parallel conflicts.

At most 8 `initial_tasks`+`mutations`. Add has task; cancel has `target_initial_task`; replace with both. Cancel+add stay two mutations. Targets are 1-based; unnamed selects last. Never merge named outcomes. Counts/state are runtime-derived."#;

/// LLM-authored, bounded declaration of one initial canonical Work item.
///
/// The declaration contains only uncertain-language product intent. IDs,
/// ordering, state transitions, and delivery status remain server-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkAdmissionTask {
    pub objective: String,
    pub expected_result: String,
}

/// One atomic user-requested graph operation.
///
/// Keeping cancel, add, and replace distinct prevents a surface-level count
/// optimization from changing the operation the user asked to exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkAdmissionGraphMutation {
    Add {
        task: WorkAdmissionTask,
    },
    Cancel {
        target_initial_candidate: usize,
        target: WorkAdmissionTask,
    },
    Replace {
        target_initial_candidate: usize,
        target: WorkAdmissionTask,
        replacement: WorkAdmissionTask,
    },
}

impl WorkAdmissionGraphMutation {
    #[must_use]
    pub fn target_initial_candidate(&self) -> Option<usize> {
        match self {
            Self::Add { .. } => None,
            Self::Cancel {
                target_initial_candidate,
                ..
            }
            | Self::Replace {
                target_initial_candidate,
                ..
            } => Some(*target_initial_candidate),
        }
    }

    #[must_use]
    pub fn addition(&self) -> Option<&WorkAdmissionTask> {
        match self {
            Self::Add { task } => Some(task),
            Self::Replace { replacement, .. } => Some(replacement),
            Self::Cancel { .. } => None,
        }
    }

    #[must_use]
    pub fn retirement(&self) -> Option<&WorkAdmissionTask> {
        match self {
            Self::Add { .. } => None,
            Self::Cancel { target, .. } | Self::Replace { target, .. } => Some(target),
        }
    }

    #[must_use]
    pub fn required_declaration_state(&self) -> Option<&'static str> {
        match self {
            Self::Add { .. } => None,
            Self::Cancel { .. } => Some("cancelled"),
            Self::Replace { .. } => Some("superseded"),
        }
    }
}

/// Closed semantic reason that makes durable Work necessary.
///
/// Requiring this separately from candidate text prevents a model from
/// turning the internal phases of one deliverable into durable tasks merely
/// because the work is complex or may use several tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkAdmissionBasis {
    DurableContinuation,
    ExplicitLifecycleControl,
}

/// Relationship between candidate result descriptions in a non-explicit
/// lifecycle decision. Candidate count alone cannot distinguish independent
/// deliverables from the implementation, verification, and reporting stages
/// of one accepted outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum AcceptanceUnitRelationship {
    SingleOutcome,
    IndependentOutcomes,
}

/// Semantic execution topology chosen by the admission judge.
///
/// A graph may contain independent items while its primary session still runs
/// them sequentially. Parallel sub-runs are a separate user-facing execution
/// choice and can be projected without establishing a durable Work graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkExecutionTopology {
    #[default]
    Primary,
    ParallelSubruns,
}

/// Typed execution-surface hints returned by Work admission. These are a
/// bounded projection of uncertain-language intent, not authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkAdmissionCapability {
    Web,
    AgentSpawner,
}

/// Whether a newly admitted Work graph should dispatch its first item now or
/// remain a durable plan awaiting an explicit continuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkAdmissionActivation {
    #[default]
    Start,
    Defer,
}

/// Semantic admission result used before the primary agent receives a tool
/// surface. This is intentionally closed: an LLM can decide whether durable
/// Work is needed and describe the bounded outcomes, while all lifecycle
/// behavior after that decision is deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkAdmissionDecision {
    NotRequired {
        workspace_mutation: WorkspaceMutationIntent,
        mutation_completion_scope: MutationCompletionScope,
        execution_topology: WorkExecutionTopology,
        required_capabilities: Vec<WorkAdmissionCapability>,
    },
    Required {
        workspace_mutation: WorkspaceMutationIntent,
        mutation_completion_scope: MutationCompletionScope,
        goal: String,
        tasks: Vec<WorkAdmissionTask>,
        deferred_graph_mutations: Vec<WorkAdmissionGraphMutation>,
        activation: WorkAdmissionActivation,
        execution_topology: WorkExecutionTopology,
        required_capabilities: Vec<WorkAdmissionCapability>,
    },
}

impl WorkAdmissionDecision {
    #[must_use]
    pub fn turn_intent(&self) -> TurnIntent {
        TurnIntent {
            work_lifecycle: match self {
                Self::NotRequired { .. } => WorkLifecycleIntent::NotRequired,
                Self::Required { .. } => WorkLifecycleIntent::Required,
            },
            workspace_mutation: self.workspace_mutation(),
            mutation_completion_scope: self.mutation_completion_scope(),
            ..TurnIntent::default()
        }
    }

    #[must_use]
    pub fn workspace_mutation(&self) -> WorkspaceMutationIntent {
        match self {
            Self::NotRequired {
                workspace_mutation, ..
            }
            | Self::Required {
                workspace_mutation, ..
            } => *workspace_mutation,
        }
    }

    #[must_use]
    pub fn mutation_completion_scope(&self) -> MutationCompletionScope {
        match self {
            Self::NotRequired {
                mutation_completion_scope,
                ..
            }
            | Self::Required {
                mutation_completion_scope,
                ..
            } => *mutation_completion_scope,
        }
    }

    #[must_use]
    pub fn initial_work_plan(&self) -> Option<(&str, &[WorkAdmissionTask])> {
        match self {
            Self::NotRequired { .. } => None,
            Self::Required { goal, tasks, .. } => Some((goal, tasks)),
        }
    }

    /// User-requested graph changes that become actionable only after the
    /// initial Work graph exists. These are lifecycle obligations, not initial
    /// executable tasks: the runtime keeps them typed until an accepted plan
    /// proposal has crossed the tool boundary.
    #[must_use]
    pub fn deferred_graph_mutations(&self) -> &[WorkAdmissionGraphMutation] {
        match self {
            Self::NotRequired { .. } => &[],
            Self::Required {
                deferred_graph_mutations,
                ..
            } => deferred_graph_mutations,
        }
    }

    #[must_use]
    pub fn execution_topology(&self) -> WorkExecutionTopology {
        match self {
            Self::NotRequired {
                execution_topology, ..
            } => *execution_topology,
            Self::Required {
                execution_topology, ..
            } => *execution_topology,
        }
    }

    #[must_use]
    pub fn required_capabilities(&self) -> &[WorkAdmissionCapability] {
        match self {
            Self::NotRequired {
                required_capabilities,
                ..
            } => required_capabilities,
            Self::Required {
                required_capabilities,
                ..
            } => required_capabilities,
        }
    }

    #[must_use]
    pub fn activation(&self) -> WorkAdmissionActivation {
        match self {
            Self::NotRequired { .. } => WorkAdmissionActivation::Start,
            Self::Required { activation, .. } => *activation,
        }
    }

    /// Return the same semantic admission with a reconciled execution mode.
    ///
    /// The admission judge owns the uncertain-language Work/decomposition
    /// decision, while a primary model may also emit the typed `start_work`
    /// activation after seeing the full runtime contract. Keeping this
    /// operation typed lets the runtime conservatively preserve an explicit
    /// `defer` without reparsing user prose or changing task identity.
    #[must_use]
    pub fn with_activation(self, activation: WorkAdmissionActivation) -> Self {
        match self {
            Self::Required {
                workspace_mutation,
                mutation_completion_scope,
                goal,
                tasks,
                deferred_graph_mutations,
                execution_topology,
                required_capabilities,
                ..
            } => Self::Required {
                workspace_mutation,
                mutation_completion_scope,
                goal,
                tasks,
                deferred_graph_mutations,
                activation,
                execution_topology,
                required_capabilities,
            },
            other => other,
        }
    }
}

/// Build the dynamic classifier context. It is a JSON value rather than
/// interpolated prose, so arbitrary user content cannot alter the contract.
#[must_use]
pub fn build_turn_intent_prompt(ctx: &TurnIntentJudgeContext) -> String {
    let recent_tools: Vec<&str> = ctx
        .recent_tools
        .iter()
        .take(8)
        .map(String::as_str)
        .collect();
    let mut prompt = serde_json::Map::from_iter([
        ("turn".to_string(), json!(ctx.turn_count)),
        (
            "has_prior_assistant_turn".to_string(),
            json!(ctx.has_prior_assistant_turn),
        ),
        ("recent_tools".to_string(), json!(recent_tools)),
        ("user_message".to_string(), json!(ctx.message)),
    ]);
    let mut previous_exchange = serde_json::Map::new();
    if let Some(message) = ctx.prior_user_message.as_deref() {
        previous_exchange.insert("user".to_string(), json!(truncate(message, 2_000)));
    }
    if let Some(message) = ctx.prior_assistant_message.as_deref() {
        previous_exchange.insert("assistant".to_string(), json!(truncate(message, 2_000)));
    }
    if !previous_exchange.is_empty() {
        prompt.insert(
            "immediate_previous_exchange".to_string(),
            Value::Object(previous_exchange),
        );
    }
    Value::Object(prompt).to_string()
}

fn build_work_admission_prompt(ctx: &TurnIntentJudgeContext) -> String {
    let mut prompt = serde_json::from_str::<Value>(&build_turn_intent_prompt(ctx))
        .expect("turn intent prompt is always valid JSON");
    // Workflow prose belongs to the primary agent's data plane.  Admission is
    // a control-plane decision, so it receives only the immutable topology
    // fact extracted from the trusted invocation ledger.  Otherwise a skill's
    // explanatory body can accidentally manufacture durable work units.
    if let Some(topology) = ctx.loaded_workflow_execution_topology {
        prompt["loaded_workflow_execution_topology"] = json!(match topology {
            WorkExecutionTopology::Primary => "primary",
            WorkExecutionTopology::ParallelSubruns => "parallel_subruns",
        });
    }
    prompt.to_string()
}

/// Build the chat messages sent to the turn-intent judge.
///
/// Keep this centralized so CLI/server judge implementations cannot drift in
/// system wording, prompt shape, or output contract.
#[must_use]
pub fn turn_intent_judge_messages(ctx: &TurnIntentJudgeContext) -> Vec<Value> {
    vec![
        json!({
            "role": "system",
            "content": TURN_INTENT_JUDGE_SYSTEM_PROMPT
        }),
        json!({
            "role": "user",
            "content": build_turn_intent_prompt(ctx),
        }),
    ]
}

/// Build the bounded, cacheable request for the Work-admission decision.
///
/// The dynamic context is intentionally shared with the broader judge so the
/// semantic basis stays the same, while the output contract remains a closed
/// lifecycle decision plus (only when needed) a small initial graph, rather
/// than an open-ended bundle of auxiliary hints.
#[must_use]
pub fn work_admission_judge_messages(ctx: &TurnIntentJudgeContext) -> Vec<Value> {
    vec![
        json!({
            "role": "system",
            "content": WORK_ADMISSION_JUDGE_SYSTEM_PROMPT,
        }),
        json!({
            "role": "user",
            "content": build_work_admission_prompt(ctx),
        }),
    ]
}

// ─── Response parser ────────────────────────────────────────────────────────

/// Parse the judge's JSON response into a [`TurnIntent`].
///
/// Strict: unknown fields or enum values produce `Err` so callers cannot
/// silently construct a degraded intent from an obsolete schema.
pub fn parse_turn_intent_response(raw: &str) -> Result<TurnIntent, TurnIntentJudgeError> {
    serde_json::from_str(json_object_payload(raw)).map_err(|_| TurnIntentJudgeError::Malformed {
        raw: truncate(raw, 256),
    })
}

/// Return the single JSON object carried by a model response.
///
/// The schema parser remains strict; this only removes presentation drift
/// around an otherwise valid object (most commonly a Markdown JSON fence).
/// Taking the first opening brace through the last closing brace also makes
/// multiple objects and malformed braces fail normal parsing instead of
/// guessing which object the model intended.
fn json_object_payload(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return trimmed;
    }
    match (trimmed.find('{'), trimmed.rfind('}')) {
        (Some(start), Some(end)) if start < end => &trimmed[start..=end],
        _ => trimmed,
    }
}

/// Parse the semantic Work admission and its bounded initial graph.
///
/// Unknown, omitted, and extra values are rejected rather than silently
/// widening the admission boundary. The caller can then proceed with its
/// explicit unavailable policy; it cannot manufacture a Work transition from
/// user text.
pub fn parse_work_admission_response(
    raw: &str,
) -> Result<WorkAdmissionDecision, TurnIntentJudgeError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WorkAdmissionResponse {
        work_lifecycle: WorkLifecycleIntent,
        #[serde(default)]
        workspace_mutation: WorkspaceMutationIntent,
        #[serde(default)]
        mutation_completion_scope: MutationCompletionScope,
        #[serde(default)]
        basis: Option<WorkAdmissionBasis>,
        #[serde(default)]
        goal: Option<String>,
        acceptance_unit_relationship: Option<AcceptanceUnitRelationship>,
        acceptance_units: Option<Vec<WorkAdmissionTaskWire>>,
        #[serde(default)]
        initial_tasks: Option<Vec<WorkAdmissionTaskWire>>,
        #[serde(default)]
        mutations: Vec<WorkAdmissionMutationWire>,
        #[serde(default)]
        activation: Option<WorkAdmissionActivation>,
        execution_topology: WorkExecutionTopology,
        #[serde(default)]
        required_capabilities: Vec<WorkAdmissionCapability>,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct WorkAdmissionTaskWire {
        objective: String,
        expected_result: String,
    }

    #[derive(serde::Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum WorkAdmissionMutationWire {
        Add {
            task: WorkAdmissionTaskWire,
        },
        Cancel {
            #[serde(default)]
            target_initial_task: Option<usize>,
        },
        Replace {
            #[serde(default)]
            target_initial_task: Option<usize>,
            task: WorkAdmissionTaskWire,
        },
    }

    let response: WorkAdmissionResponse =
        serde_json::from_str(json_object_payload(raw)).map_err(|_| {
            TurnIntentJudgeError::Malformed {
                raw: truncate(raw, 256),
            }
        })?;
    let malformed = || TurnIntentJudgeError::Malformed {
        raw: truncate(raw, 256),
    };
    match response.work_lifecycle {
        WorkLifecycleIntent::NotRequired
            if response.basis.is_none()
                && response.initial_tasks.is_none()
                && response.mutations.is_empty()
                && response.activation.is_none() =>
        {
            // `goal` is descriptive when durable Work is not requested. It
            // has no lifecycle authority here, so preserving the otherwise
            // valid mutation/topology facts is safer than discarding the
            // whole primary-turn decision for an inert annotation.
            let topology = response.execution_topology;
            if response.required_capabilities.len() > 2
                || response
                    .required_capabilities
                    .windows(2)
                    .any(|pair| pair[0] == pair[1])
            {
                return Err(malformed());
            }
            let mut required_capabilities = response.required_capabilities;
            if topology == WorkExecutionTopology::ParallelSubruns
                && !required_capabilities.contains(&WorkAdmissionCapability::AgentSpawner)
            {
                // The topology is the semantic authority. Requiring the LLM
                // to repeat its implied capability made an otherwise valid
                // minimal response fail stochastically at admission.
                required_capabilities.push(WorkAdmissionCapability::AgentSpawner);
            }
            let acceptance_units = response.acceptance_units.ok_or_else(malformed)?;
            let acceptance_unit_relationship = response
                .acceptance_unit_relationship
                .ok_or_else(malformed)?;
            let valid_task = |task: &WorkAdmissionTaskWire| {
                valid_work_text(&task.objective, 1_024)
                    && valid_work_text(&task.expected_result, 1_024)
            };
            if acceptance_units.len() > 8 || acceptance_units.iter().any(|task| !valid_task(task)) {
                return Err(malformed());
            }
            if acceptance_unit_relationship == AcceptanceUnitRelationship::IndependentOutcomes
                && acceptance_units.len() < 2
            {
                return Err(malformed());
            }
            if topology == WorkExecutionTopology::Primary
                && acceptance_unit_relationship == AcceptanceUnitRelationship::IndependentOutcomes
                && acceptance_units.len() >= 2
            {
                let tasks = acceptance_units
                    .into_iter()
                    .map(|task| WorkAdmissionTask {
                        objective: task.objective,
                        expected_result: task.expected_result,
                    })
                    .collect::<Vec<_>>();
                let goal = response
                    .goal
                    .filter(|goal| valid_work_text(goal, 1_024))
                    .unwrap_or_else(|| {
                        format!(
                            "Complete the {} independently accepted user outcomes",
                            tasks.len()
                        )
                    });
                return Ok(WorkAdmissionDecision::Required {
                    workspace_mutation: response.workspace_mutation,
                    mutation_completion_scope: response.mutation_completion_scope,
                    goal,
                    tasks,
                    deferred_graph_mutations: Vec::new(),
                    activation: WorkAdmissionActivation::Start,
                    execution_topology: topology,
                    required_capabilities,
                });
            }
            Ok(WorkAdmissionDecision::NotRequired {
                workspace_mutation: response.workspace_mutation,
                mutation_completion_scope: response.mutation_completion_scope,
                execution_topology: topology,
                required_capabilities,
            })
        }
        WorkLifecycleIntent::Required => {
            if response.acceptance_units.is_some()
                || response.acceptance_unit_relationship.is_some()
            {
                return Err(malformed());
            }
            let basis = response.basis.ok_or_else(malformed)?;
            let goal = response.goal.ok_or_else(malformed)?;
            let initial_tasks = response.initial_tasks.ok_or_else(malformed)?;
            if !valid_work_text(&goal, 1_024)
                || !(1..=8).contains(&initial_tasks.len())
                || initial_tasks.len() + response.mutations.len() > 8
            {
                return Err(malformed());
            }
            if response.required_capabilities.len() > 2
                || response
                    .required_capabilities
                    .windows(2)
                    .any(|pair| pair[0] == pair[1])
            {
                return Err(malformed());
            }
            if response.execution_topology == WorkExecutionTopology::ParallelSubruns {
                return Err(TurnIntentJudgeError::UnsupportedCombination(
                    "durable Work and parallel sub-runs require a task-to-slot settlement protocol"
                        .to_string(),
                ));
            }
            let topology = response.execution_topology;
            let valid_task = |task: &WorkAdmissionTaskWire| {
                valid_work_text(&task.objective, 1_024)
                    && valid_work_text(&task.expected_result, 1_024)
            };
            if initial_tasks.iter().any(|task| !valid_task(task))
                || response.mutations.iter().any(|mutation| match mutation {
                    WorkAdmissionMutationWire::Add { task }
                    | WorkAdmissionMutationWire::Replace { task, .. } => !valid_task(task),
                    WorkAdmissionMutationWire::Cancel { .. } => false,
                })
            {
                return Err(malformed());
            }
            match basis {
                WorkAdmissionBasis::ExplicitLifecycleControl if response.mutations.is_empty() => {
                    return Err(malformed());
                }
                WorkAdmissionBasis::DurableContinuation if !response.mutations.is_empty() => {
                    return Err(malformed());
                }
                _ => {}
            }
            let project_task = |task: WorkAdmissionTaskWire| WorkAdmissionTask {
                objective: task.objective,
                expected_result: task.expected_result,
            };
            let tasks = initial_tasks
                .into_iter()
                .map(project_task)
                .collect::<Vec<_>>();
            let initial_count = tasks.len();
            let resolve_target = |target: Option<usize>| {
                let target = target.unwrap_or(initial_count);
                (target > 0 && target <= initial_count).then_some(target)
            };
            let deferred_graph_mutations = response
                .mutations
                .into_iter()
                .map(|mutation| match mutation {
                    WorkAdmissionMutationWire::Add { task } => {
                        Ok(WorkAdmissionGraphMutation::Add {
                            task: project_task(task),
                        })
                    }
                    WorkAdmissionMutationWire::Cancel {
                        target_initial_task,
                    } => {
                        let target_initial_candidate =
                            resolve_target(target_initial_task).ok_or_else(malformed)?;
                        Ok(WorkAdmissionGraphMutation::Cancel {
                            target_initial_candidate,
                            target: tasks[target_initial_candidate - 1].clone(),
                        })
                    }
                    WorkAdmissionMutationWire::Replace {
                        target_initial_task,
                        task,
                    } => {
                        let target_initial_candidate =
                            resolve_target(target_initial_task).ok_or_else(malformed)?;
                        Ok(WorkAdmissionGraphMutation::Replace {
                            target_initial_candidate,
                            target: tasks[target_initial_candidate - 1].clone(),
                            replacement: project_task(task),
                        })
                    }
                })
                .collect::<Result<Vec<_>, TurnIntentJudgeError>>()?;
            Ok(WorkAdmissionDecision::Required {
                workspace_mutation: response.workspace_mutation,
                mutation_completion_scope: response.mutation_completion_scope,
                goal,
                tasks,
                deferred_graph_mutations,
                activation: response.activation.unwrap_or_default(),
                execution_topology: topology,
                required_capabilities: response.required_capabilities,
            })
        }
        WorkLifecycleIntent::Unknown | WorkLifecycleIntent::NotRequired => Err(malformed()),
    }
}

fn valid_work_text(value: &str, max_chars: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max_chars
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use astra_config::user_profile::{
        MutationCompletionScope, Scenario, TurnCommunicativeAct, TurnIntentDomain,
        WorkLifecycleIntent, WorkspaceMutationIntent,
    };
    use astra_turn_types::{ObjectiveRelation, UserFeedback, UserFeedbackKind, UserFeedbackTarget};

    #[test]
    fn prompt_encodes_only_dynamic_context() {
        let ctx = TurnIntentJudgeContext {
            message: "please inspect the current changes".into(),
            turn_count: 3,
            recent_tools: vec!["read_file".into(), "bash".into()],
            has_prior_assistant_turn: true,
            ..Default::default()
        };
        let prompt = build_turn_intent_prompt(&ctx);
        let dynamic: Value = serde_json::from_str(&prompt).expect("dynamic context JSON");
        assert_eq!(dynamic["turn"], 3);
        assert_eq!(dynamic["has_prior_assistant_turn"], true);
        assert_eq!(dynamic["recent_tools"], json!(["read_file", "bash"]));
        assert_eq!(
            dynamic["user_message"],
            "please inspect the current changes"
        );
        assert!(
            !prompt.contains("objective_relation"),
            "static classifier rules belong to the cacheable system prefix"
        );
    }

    #[test]
    fn prompt_json_encodes_user_message_without_mutating_it() {
        let ctx = TurnIntentJudgeContext {
            message: "quote: \"x\"\nrun `literal`".into(),
            turn_count: 1,
            recent_tools: vec![],
            has_prior_assistant_turn: false,
            ..Default::default()
        };
        let prompt = build_turn_intent_prompt(&ctx);
        let dynamic: Value = serde_json::from_str(&prompt).expect("dynamic context JSON");
        assert_eq!(dynamic["user_message"], "quote: \"x\"\nrun `literal`");
    }

    #[test]
    fn work_admission_prompt_uses_typed_workflow_topology_not_workflow_body() {
        let ctx = TurnIntentJudgeContext {
            message: "review this change".into(),
            loaded_workflow_execution_topology: Some(WorkExecutionTopology::ParallelSubruns),
            ..Default::default()
        };

        let messages = work_admission_judge_messages(&ctx);
        let dynamic: Value = serde_json::from_str(
            messages[1]["content"]
                .as_str()
                .expect("dynamic admission context"),
        )
        .unwrap();

        assert!(dynamic.get("loaded_workflow_directives").is_none());
        assert_eq!(
            dynamic["loaded_workflow_execution_topology"],
            "parallel_subruns"
        );
        let system = messages[0]["content"].as_str().unwrap();
        assert!(system.contains("More than one agent/reviewer/worker"));
        assert!(system.contains("unless serial"));
        assert!(system.contains("Perspectives feeding one combined conclusion"));
        assert!(system.contains("Include `execution_topology`"));
    }

    #[test]
    fn prompt_carries_one_bounded_exchange_for_reference_resolution() {
        let ctx = TurnIntentJudgeContext {
            message: "use two agents for that".into(),
            turn_count: 2,
            recent_tools: vec![],
            has_prior_assistant_turn: true,
            prior_user_message: Some("compare the two implementations".into()),
            prior_assistant_message: Some("I compared them serially".into()),
            ..Default::default()
        };

        let dynamic: Value =
            serde_json::from_str(&build_turn_intent_prompt(&ctx)).expect("dynamic context JSON");

        assert_eq!(
            dynamic["immediate_previous_exchange"]["user"],
            "compare the two implementations"
        );
        assert_eq!(
            dynamic["immediate_previous_exchange"]["assistant"],
            "I compared them serially"
        );
    }

    #[test]
    fn prompt_caps_recent_tools_to_eight() {
        let ctx = TurnIntentJudgeContext {
            message: "hi".into(),
            turn_count: 1,
            recent_tools: (0..16).map(|i| format!("tool_{i}")).collect(),
            has_prior_assistant_turn: false,
            ..Default::default()
        };
        let prompt = build_turn_intent_prompt(&ctx);
        assert!(prompt.contains("tool_0"));
        assert!(prompt.contains("tool_7"));
        assert!(
            !prompt.contains("tool_8"),
            "recent tools must be capped at 8 entries: {prompt}"
        );
    }

    #[test]
    fn messages_keep_the_contract_in_a_bounded_cacheable_system_prefix() {
        let ctx = TurnIntentJudgeContext {
            message: "do the work".into(),
            turn_count: 1,
            recent_tools: vec![],
            has_prior_assistant_turn: false,
            ..Default::default()
        };
        let messages = turn_intent_judge_messages(&ctx);
        assert_eq!(messages.len(), 2);
        let system = messages[0]["content"].as_str().expect("system content");
        assert!(system.contains("work_lifecycle"));
        assert!(system.contains("browser_verification_required"));
        assert!(system.contains("independently accepted deliverables"));
        assert!(system.contains("Count acceptance units"));
        assert!(system.contains("Explicit A and B stay separate"));
        assert!(system.contains("explicit same-turn multi-agent request"));
        assert!(system.contains("not response containers, agents, tools"));
        assert!(system.contains("task mode/board"));
        assert!(system.contains("fixed chain"));
        assert!(system.contains("is end state"));
        assert!(system.contains("version-control change"));
        assert!(system.contains("daemon, service, deployment"));
        assert!(
            system.len() < 2_800,
            "the stable semantic prefix must stay small enough to cache cheaply: {} bytes",
            system.len()
        );
        assert_eq!(messages[1]["content"], build_turn_intent_prompt(&ctx));
    }

    #[test]
    fn work_admission_messages_keep_only_the_latency_critical_contract() {
        let ctx = TurnIntentJudgeContext {
            message: "independently verify the CI command and its local equivalent".into(),
            turn_count: 1,
            recent_tools: vec![],
            has_prior_assistant_turn: false,
            ..Default::default()
        };
        let messages = work_admission_judge_messages(&ctx);
        assert_eq!(messages.len(), 2);
        let system = messages[0]["content"].as_str().expect("system content");
        assert!(system.contains("work_lifecycle"));
        assert!(!system.contains("multiple_explicit_outcomes"));
        assert!(system.contains("durable_continuation"));
        assert!(system.contains("task system/mode/board"));
        assert!(system.contains("fixed chain/pipeline does not"));
        assert!(system.contains("explicit_lifecycle_control"));
        assert!(system.contains("replace with both"));
        assert!(system.contains("Cancel+add stay two mutations"));
        assert!(system.contains("first matching rule wins"));
        assert!(system.contains("Count user-facing outcomes"));
        assert!(system.contains("one comparison"));
        assert!(system.contains("survives every peer failure"));
        assert!(system.contains("Separately named/numbered results"));
        assert!(system.contains("shared topic, deadline"));
        assert!(system.contains("one response presents both"));
        assert!(system.contains("Parallelism alone"));
        assert!(system.contains("change plus its test/report"));
        assert!(system.contains("payload plus source/verification"));
        assert!(system.contains("Never merge named outcomes"));
        assert!(system.contains("parallel_subruns"));
        assert!(system.contains("agent_spawner"));
        assert!(system.contains("local paths are not web"));
        assert!(system.contains("expected_result"));
        assert!(system.contains("activation"));
        assert!(system.contains("At most 8"));
        assert!(system.contains("initial_tasks"));
        assert!(system.contains("mutations"));
        assert!(system.contains("outcomes and mutations"));
        assert!(system.contains("target_initial_task"));
        assert!(system.contains("runtime-derived"));
        assert!(system.contains("requested end state"));
        assert!(system.contains("preparatory inspection"));
        assert!(system.contains("mutation_completion_scope"));
        assert!(system.contains("Managed state outside the project"));
        assert!(!system.contains("initial_outcome_count"));
        assert!(!system.contains("final_outcome_count"));
        assert!(
            system.len() < 3_000,
            "Work admission must remain a small interactive request: {} bytes",
            system.len()
        );
        assert_eq!(messages[1]["content"], build_work_admission_prompt(&ctx));
    }

    #[test]
    fn work_admission_parser_accepts_only_a_decisive_closed_contract() {
        let required = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"explicit_lifecycle_control","goal":"Verify two independent facts and later add one outcome","initial_tasks":[{"objective":"Verify source A","expected_result":"One direct citation"},{"objective":"Verify source B","expected_result":"One direct citation"}],"mutations":[{"kind":"add","task":{"objective":"Verify source C","expected_result":"One direct citation"}}]}"#,
        )
        .expect("required work admission");
        assert_eq!(
            required.turn_intent().work_lifecycle,
            WorkLifecycleIntent::Required
        );
        let (goal, tasks) = required.initial_work_plan().expect("required graph");
        assert_eq!(
            goal,
            "Verify two independent facts and later add one outcome"
        );
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].objective, "Verify source A");
        assert_eq!(tasks[1].objective, "Verify source B");
        assert_eq!(required.deferred_graph_mutations().len(), 1);
        assert_eq!(
            required.deferred_graph_mutations()[0]
                .addition()
                .expect("add mutation")
                .objective,
            "Verify source C"
        );
        assert_eq!(
            required.execution_topology(),
            WorkExecutionTopology::Primary
        );
        assert!(required.required_capabilities().is_empty());
        assert_eq!(required.activation(), WorkAdmissionActivation::Start);

        let single = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"durable_continuation","goal":"Keep one retrieval task recoverable","initial_tasks":[{"objective":"Fetch the source","expected_result":"One cited result"}]}"#,
        )
        .expect("a durable single-task Work request is valid");
        assert_eq!(single.initial_work_plan().expect("single graph").1.len(), 1);

        let repeated = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"durable_continuation","goal":"Use the task system to keep two lifecycle probes recoverable","initial_tasks":[{"objective":"Run lifecycle probe","expected_result":"One independently accepted probe result"},{"objective":"Run lifecycle probe","expected_result":"One independently accepted probe result"}]}"#,
        )
        .expect("task identity comes from Work item ids, not content uniqueness");
        assert_eq!(
            repeated
                .initial_work_plan()
                .expect("repeated graph")
                .1
                .len(),
            2
        );

        let lifecycle_control = parse_work_admission_response(
            r#"{"work_lifecycle":"required","workspace_mutation":"read_only","basis":"explicit_lifecycle_control","goal":"Run two serial tasks, cancel one, then add one","execution_topology":"primary","initial_tasks":[{"objective":"Inspect source A","expected_result":"One cited result from A"},{"objective":"Inspect source B","expected_result":"One cited result from B"}],"mutations":[{"kind":"cancel","target_initial_task":2},{"kind":"add","task":{"objective":"Inspect source B","expected_result":"One cited result from B"}}]}"#,
        )
        .expect("explicit lifecycle control requires canonical Work");
        assert_eq!(
            lifecycle_control
                .initial_work_plan()
                .expect("initial lifecycle graph")
                .1
                .len(),
            2
        );
        assert_eq!(lifecycle_control.deferred_graph_mutations().len(), 2);
        assert_eq!(
            lifecycle_control.deferred_graph_mutations()[0].target_initial_candidate(),
            Some(2)
        );
        assert!(matches!(
            lifecycle_control.deferred_graph_mutations(),
            [
                WorkAdmissionGraphMutation::Cancel { .. },
                WorkAdmissionGraphMutation::Add { .. }
            ]
        ));
        assert_eq!(
            lifecycle_control.turn_intent().workspace_mutation,
            WorkspaceMutationIntent::ReadOnly
        );

        let implicit_target = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"explicit_lifecycle_control","goal":"Replace one task with a newly named outcome","initial_tasks":[{"objective":"Outcome A","expected_result":"Evidence A"},{"objective":"Outcome B","expected_result":"Evidence B"}],"mutations":[{"kind":"replace","task":{"objective":"Invented guess","expected_result":"Invented evidence"}}]}"#,
        )
        .expect("an unnamed target is normalized by product policy");
        let replacement = &implicit_target.deferred_graph_mutations()[0];
        assert_eq!(replacement.target_initial_candidate(), Some(2));
        assert_eq!(
            replacement.addition().expect("replacement task").objective,
            "Invented guess"
        );

        let guessed_explicit_target = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"explicit_lifecycle_control","goal":"Replace B with C","initial_tasks":[{"objective":"Outcome A","expected_result":"Evidence A"},{"objective":"Outcome B","expected_result":"Evidence B"}],"mutations":[{"kind":"replace","target_initial_task":2,"task":{"objective":"Invented C","expected_result":"Evidence C"}}]}"#,
        )
        .expect("a guessed divergent target cannot escape deterministic normalization");
        let replacement = &guessed_explicit_target.deferred_graph_mutations()[0];
        assert_eq!(replacement.target_initial_candidate(), Some(2));
        assert_eq!(
            replacement.addition().expect("replacement task").objective,
            "Invented C"
        );
        assert_eq!(
            lifecycle_control.execution_topology(),
            WorkExecutionTopology::Primary
        );

        let deferred = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"durable_continuation","activation":"defer","goal":"Prepare two recoverable task-system investigations","initial_tasks":[{"objective":"Define source A","expected_result":"A durable assignment"},{"objective":"Define source B","expected_result":"A durable assignment"}]}"#,
        )
        .expect("deferred Work admission");
        assert_eq!(deferred.activation(), WorkAdmissionActivation::Defer);

        let web_and_agents = parse_work_admission_response(
            r#"{"work_lifecycle":"required","execution_topology":"primary","basis":"durable_continuation","required_capabilities":["web","agent_spawner"],"goal":"Track two recoverable task-system investigations","initial_tasks":[{"objective":"Inspect source A","expected_result":"One direct citation"},{"objective":"Inspect source B","expected_result":"One direct citation"}]}"#,
        )
        .expect("typed execution-surface capabilities");
        assert_eq!(
            web_and_agents.required_capabilities(),
            &[
                WorkAdmissionCapability::Web,
                WorkAdmissionCapability::AgentSpawner
            ]
        );

        let direct = parse_work_admission_response(
            r#"{"work_lifecycle":"not_required","execution_topology":"primary","acceptance_unit_relationship":"single_outcome","acceptance_units":[{"objective":"Answer the question","expected_result":"One direct answer"}]}"#,
        )
        .expect("direct work admission");
        assert_eq!(
            direct.turn_intent().work_lifecycle,
            WorkLifecycleIntent::NotRequired
        );
        assert!(direct.initial_work_plan().is_none());
        assert_eq!(direct.execution_topology(), WorkExecutionTopology::Primary);
        assert!(direct.required_capabilities().is_empty());

        let explicit_primary_web = parse_work_admission_response(
            r#"{"work_lifecycle":"not_required","execution_topology":"primary","required_capabilities":["web"],"acceptance_unit_relationship":"single_outcome","acceptance_units":[{"objective":"Answer from the web","expected_result":"One cited answer"}]}"#,
        )
        .expect("the documented explicit primary/web projection must parse");
        assert_eq!(
            explicit_primary_web.execution_topology(),
            WorkExecutionTopology::Primary
        );
        assert_eq!(
            explicit_primary_web.required_capabilities(),
            &[WorkAdmissionCapability::Web]
        );

        let parallel_direct = parse_work_admission_response(
            r#"{"work_lifecycle":"not_required","execution_topology":"parallel_subruns","acceptance_unit_relationship":"independent_outcomes","acceptance_units":[{"objective":"Return result A","expected_result":"Payload A"},{"objective":"Return result B","expected_result":"Payload B"}]}"#,
        )
        .expect("same-turn fanout must derive its typed agent capability without durable Work");
        assert_eq!(
            parallel_direct.execution_topology(),
            WorkExecutionTopology::ParallelSubruns
        );
        assert_eq!(
            parallel_direct.required_capabilities(),
            &[WorkAdmissionCapability::AgentSpawner]
        );
        assert_eq!(
            parallel_direct.turn_intent().work_lifecycle,
            WorkLifecycleIntent::NotRequired
        );

        let derived_multiple = parse_work_admission_response(
            r#"{"work_lifecycle":"not_required","workspace_mutation":"read_only","execution_topology":"primary","acceptance_unit_relationship":"independent_outcomes","acceptance_units":[{"objective":"Verify outcome A","expected_result":"Payload and source A"},{"objective":"Verify outcome B","expected_result":"Payload and source B"}]}"#,
        )
        .expect("runtime derives durable Work from independently accepted primary outcomes");
        assert_eq!(
            derived_multiple.turn_intent().work_lifecycle,
            WorkLifecycleIntent::Required
        );
        assert_eq!(
            derived_multiple
                .initial_work_plan()
                .expect("derived Work graph")
                .1
                .len(),
            2
        );

        let cohesive_stages = parse_work_admission_response(
            r#"{"work_lifecycle":"not_required","workspace_mutation":"must_mutate","mutation_completion_scope":"workspace","execution_topology":"primary","acceptance_unit_relationship":"single_outcome","acceptance_units":[{"objective":"Apply the requested change","expected_result":"Changed file"},{"objective":"Run its regression check","expected_result":"Passing verification"},{"objective":"Report the result","expected_result":"One final summary"}]}"#,
        )
        .expect("candidate stages of one accepted change remain one-shot");
        assert_eq!(
            cohesive_stages.turn_intent().work_lifecycle,
            WorkLifecycleIntent::NotRequired
        );
        assert!(cohesive_stages.initial_work_plan().is_none());

        for invalid in [
            r#"{}"#,
            r#"{"work_lifecycle":"unknown"}"#,
            r#"{"work_lifecycle":"required"}"#,
            r#"{"work_lifecycle":"not_required"}"#,
            r#"{"work_lifecycle":"not_required","initial_tasks":[]}"#,
            r#"{"work_lifecycle":"required","scenario":"testing"}"#,
            r#"{"work_lifecycle":"not_required","activation":"defer"}"#,
            r#"{"work_lifecycle":"not_required","basis":"durable_continuation"}"#,
            r#"{"work_lifecycle":"not_required","execution_topology":"primary","acceptance_unit_relationship":"independent_outcomes","acceptance_units":[{"objective":"Only one","expected_result":"One result"}]}"#,
            r#"{"work_lifecycle":"required","basis":"explicit_lifecycle_control","deferred_outcome_count":0,"goal":"x","candidates":[{"availability":"at_work_start","objective":"a","expected_result":"b"}]}"#,
            r#"{"work_lifecycle":"required","basis":"explicit_lifecycle_control","initial_outcome_count":2,"deferred_outcome_count":1,"final_outcome_count":2,"goal":"add an outcome","candidates":[{"availability":"at_work_start","objective":"a","expected_result":"evidence a"},{"availability":"at_work_start","objective":"b","expected_result":"evidence b"},{"availability":"after_graph_mutation","mutation_kind":"add","objective":"c","expected_result":"evidence c"}]}"#,
            r#"{"work_lifecycle":"required","goal":"two outcomes","candidates":[{"availability":"at_work_start","objective":"a","expected_result":"b"},{"availability":"at_work_start","objective":"c","expected_result":"d"}]}"#,
            r#"{"work_lifecycle":"required","basis":"multiple_explicit_outcomes","goal":"x","candidates":[{"availability":"at_work_start","objective":"a","expected_result":"b"}]}"#,
            r#"{"work_lifecycle":"required","basis":"multiple_explicit_outcomes","goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"},{"objective":"c","expected_result":"d"}]}"#,
            r#"{"work_lifecycle":"required","goal":"x","candidates":[{"objective":"a","expected_result":"b"}]}"#,
            r#"{"work_lifecycle":"required","goal":"x","candidates":[{"availability":"after_graph_mutation","objective":"a","expected_result":"b"}]}"#,
            r#"{"work_lifecycle":"required","basis":"durable_continuation","required_capabilities":["web","web"],"goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"},{"objective":"c","expected_result":"d"}]}"#,
            r#"{"work_lifecycle":"required","basis":"explicit_lifecycle_control","goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"}],"mutations":[]}"#,
            r#"{"work_lifecycle":"required","basis":"explicit_lifecycle_control","goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"}],"mutations":[{"kind":"cancel","target_initial_task":0}]}"#,
            r#"{"work_lifecycle":"required","basis":"explicit_lifecycle_control","goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"}],"mutations":[{"kind":"cancel","target_initial_task":2}]}"#,
            r#"{"work_lifecycle":"required","basis":"explicit_lifecycle_control","goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"}],"mutations":[{"kind":"add","target_initial_task":1,"task":{"objective":"c","expected_result":"d"}}]}"#,
            r#"{"work_lifecycle":"maybe"}"#,
        ] {
            assert!(
                matches!(
                    parse_work_admission_response(invalid),
                    Err(TurnIntentJudgeError::Malformed { .. })
                ),
                "must reject non-contract response {invalid}"
            );
        }
    }

    #[test]
    fn durable_work_plus_parallel_topology_is_a_typed_product_conflict() {
        let error = parse_work_admission_response(
            r#"{"work_lifecycle":"required","basis":"durable_continuation","execution_topology":"parallel_subruns","required_capabilities":["agent_spawner"],"goal":"x","initial_tasks":[{"objective":"a","expected_result":"b"},{"objective":"c","expected_result":"d"}]}"#,
        )
        .expect_err("the runtime has no task-to-fanout-slot settlement carrier");

        assert!(matches!(
            error,
            TurnIntentJudgeError::UnsupportedCombination(_)
        ));
    }

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"domain":"github","communicative_act":"task","requested_scenario":"code_review","prohibited_scenarios":[],"objective_relation":"replace"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.domain, Some(TurnIntentDomain::GitHub));
        assert_eq!(intent.communicative_act, TurnCommunicativeAct::Task);
        assert_eq!(intent.requested_scenario, Some(Scenario::CodeReview));
        assert!(intent.prohibited_scenarios.is_empty());
        assert_eq!(intent.objective_relation, ObjectiveRelation::Replace);
    }

    #[test]
    fn parses_external_and_mixed_mutation_completion_scopes() {
        let external = parse_turn_intent_response(
            r#"{"communicative_act":"task","workspace_mutation":"must_mutate","mutation_completion_scope":"external"}"#,
        )
        .expect("typed external completion scope");
        assert_eq!(
            external.mutation_completion_scope,
            MutationCompletionScope::External
        );
        assert!(!external.requires_workspace_mutation());

        let mixed = parse_work_admission_response(
            r#"{"work_lifecycle":"not_required","workspace_mutation":"must_mutate","mutation_completion_scope":"mixed","execution_topology":"primary","acceptance_unit_relationship":"single_outcome","acceptance_units":[{"objective":"Apply the requested change","expected_result":"Changed workspace and external state"}]}"#,
        )
        .expect("typed mixed completion scope");
        assert_eq!(
            mixed.turn_intent().mutation_completion_scope,
            MutationCompletionScope::Mixed
        );
        assert!(mixed.turn_intent().requires_workspace_mutation());
    }

    #[test]
    fn parses_work_lifecycle_as_a_typed_contract() {
        let required = parse_turn_intent_response(
            r#"{"communicative_act":"task","objective_relation":"replace","work_lifecycle":"required"}"#,
        )
        .unwrap();
        assert_eq!(required.work_lifecycle, WorkLifecycleIntent::Required);

        let omitted = parse_turn_intent_response(
            r#"{"communicative_act":"question","objective_relation":"unknown"}"#,
        )
        .unwrap();
        assert_eq!(omitted.work_lifecycle, WorkLifecycleIntent::Unknown);

        let error = parse_turn_intent_response(
            r#"{"communicative_act":"task","objective_relation":"replace","work_lifecycle":"tracked"}"#,
        )
        .unwrap_err();
        assert!(matches!(error, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn parses_every_communicative_act_as_a_typed_value() {
        for (wire, expected) in [
            ("task", TurnCommunicativeAct::Task),
            ("question", TurnCommunicativeAct::Question),
            ("acknowledgement", TurnCommunicativeAct::Acknowledgement),
            ("social", TurnCommunicativeAct::Social),
            ("unknown", TurnCommunicativeAct::Unknown),
        ] {
            let raw = format!(r#"{{"communicative_act":"{wire}","objective_relation":"unknown"}}"#);
            let intent = parse_turn_intent_response(&raw).unwrap();
            assert_eq!(intent.communicative_act, expected);
        }
    }

    #[test]
    fn parses_refinement_with_prohibition_and_feedback() {
        let raw = r#"{
          "communicative_act": "task",
          "requested_scenario": "implementation",
          "prohibited_scenarios": ["code_review"],
          "objective_relation": "refine",
          "feedback": {"kind": "requirement", "target": "approach"}
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Implementation));
        assert_eq!(intent.prohibited_scenarios, vec![Scenario::CodeReview]);
        assert_eq!(intent.objective_relation, ObjectiveRelation::Refine);
        assert!(!intent.reanchors_current_objective());
        assert_eq!(
            intent.feedback,
            Some(UserFeedback {
                kind: UserFeedbackKind::Requirement,
                target: UserFeedbackTarget::Approach,
            })
        );
        assert_eq!(
            intent.workspace_mutation,
            WorkspaceMutationIntent::Unknown,
            "missing workspace_mutation must fail closed"
        );
        assert!(!intent.browser_verification_required);
    }

    #[test]
    fn parses_benchmark_comparison_scenario() {
        let raw = r#"{"communicative_act":"task","requested_scenario":"benchmark_comparison","prohibited_scenarios":[],"objective_relation":"replace"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(
            intent.requested_scenario,
            Some(Scenario::BenchmarkComparison)
        );
    }

    #[test]
    fn parses_structured_correction_as_one_relation() {
        let raw = r#"{
          "communicative_act": "task",
          "requested_scenario": "refactoring",
          "prohibited_scenarios": [],
          "objective_relation": "correct",
          "feedback": {"kind": "correction", "target": "approach"}
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Refactoring));
        assert_eq!(intent.objective_relation, ObjectiveRelation::Correct);
        assert!(intent.reanchors_current_objective());
    }

    #[test]
    fn parses_null_requested_scenario_as_none() {
        let raw = r#"{"domain":null,"communicative_act":"task","requested_scenario":null,"prohibited_scenarios":[],"objective_relation":"continue"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.domain, None);
        assert_eq!(intent.requested_scenario, None);
    }

    #[test]
    fn missing_domain_stays_unknown_instead_of_inferred_from_text() {
        let raw = r#"{"communicative_act":"task","requested_scenario":"implementation","objective_relation":"replace"}"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.domain, None);
    }

    #[test]
    fn unknown_domain_returns_malformed() {
        let raw =
            r#"{"domain":"frontend","communicative_act":"task","objective_relation":"replace"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn accepts_strict_payload_inside_markdown_fence() {
        let raw = "```json\n{\"communicative_act\":\"task\",\"requested_scenario\":\"debugging\",\"prohibited_scenarios\":[],\"objective_relation\":\"replace\"}\n```";
        let intent = parse_turn_intent_response(raw).expect("strict fenced payload");
        assert_eq!(intent.communicative_act, TurnCommunicativeAct::Task);
    }

    #[test]
    fn accepts_one_strict_payload_with_surrounding_prose() {
        let raw = "Here is the classification:\n{\"communicative_act\":\"question\",\"requested_scenario\":\"quick_answer\",\"prohibited_scenarios\":[],\"objective_relation\":\"unknown\"}\nLet me know if you need more.";
        let intent = parse_turn_intent_response(raw).expect("strict wrapped payload");
        assert_eq!(intent.communicative_act, TurnCommunicativeAct::Question);
    }

    #[test]
    fn work_admission_accepts_strict_payload_inside_markdown_fence() {
        let raw = "```json\n{\"work_lifecycle\":\"not_required\",\"workspace_mutation\":\"must_mutate\",\"mutation_completion_scope\":\"workspace\",\"execution_topology\":\"primary\",\"acceptance_unit_relationship\":\"single_outcome\",\"acceptance_units\":[{\"objective\":\"Apply the change\",\"expected_result\":\"Changed workspace\"}]}\n```";
        let decision = parse_work_admission_response(raw).expect("strict fenced admission");
        let intent = decision.turn_intent();
        assert_eq!(intent.work_lifecycle, WorkLifecycleIntent::NotRequired);
        assert_eq!(
            intent.workspace_mutation,
            WorkspaceMutationIntent::MustMutate
        );
        assert_eq!(
            intent.mutation_completion_scope,
            MutationCompletionScope::Workspace
        );
    }

    #[test]
    fn not_required_admission_preserves_mutation_intent_with_descriptive_goal() {
        let raw = r#"{"work_lifecycle":"not_required","workspace_mutation":"must_mutate","mutation_completion_scope":"workspace","execution_topology":"primary","goal":"Build the requested compiler in the bound workspace.","acceptance_unit_relationship":"single_outcome","acceptance_units":[{"objective":"Build the compiler","expected_result":"One working compiler artifact"}]}"#;

        let decision = parse_work_admission_response(raw)
            .expect("a non-durable descriptive goal must not erase typed primary intent");
        let intent = decision.turn_intent();
        assert_eq!(intent.work_lifecycle, WorkLifecycleIntent::NotRequired);
        assert_eq!(
            intent.workspace_mutation,
            WorkspaceMutationIntent::MustMutate
        );
        assert_eq!(
            intent.mutation_completion_scope,
            MutationCompletionScope::Workspace
        );
    }

    #[test]
    fn wrapped_multiple_objects_remain_malformed() {
        let raw = "first {\"work_lifecycle\":\"not_required\",\"execution_topology\":\"primary\"} second {\"work_lifecycle\":\"not_required\",\"execution_topology\":\"primary\"}";
        assert!(matches!(
            parse_work_admission_response(raw),
            Err(TurnIntentJudgeError::Malformed { .. })
        ));
    }

    #[test]
    fn unknown_scenario_returns_malformed() {
        let raw = r#"{"communicative_act":"task","requested_scenario":"mystery","prohibited_scenarios":[],"objective_relation":"unknown"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn unknown_objective_relation_returns_malformed() {
        let raw = r#"{"communicative_act":"question","requested_scenario":null,"prohibited_scenarios":[],"objective_relation":"sometimes"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn malformed_json_returns_malformed_error() {
        let err = parse_turn_intent_response("not json at all").unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn malformed_unicode_response_is_truncated_without_panicking() {
        let raw = "坏".repeat(100);
        let err = parse_turn_intent_response(&raw).unwrap_err();
        match err {
            TurnIntentJudgeError::Malformed { raw } => {
                assert_eq!(raw, "坏".repeat(100));
            }
            other => panic!("expected malformed, got {other:?}"),
        }

        let raw = "坏".repeat(300);
        let err = parse_turn_intent_response(&raw).unwrap_err();
        match err {
            TurnIntentJudgeError::Malformed { raw } => {
                assert!(raw.ends_with("..."));
                assert_eq!(raw.trim_end_matches("...").chars().count(), 256);
            }
            other => panic!("expected malformed, got {other:?}"),
        }
    }

    #[test]
    fn omitted_optional_control_fields_default_without_erasing_work_requirement() {
        // This is a valid minimal classifier response. The omitted relations
        // are safe defaults, while the explicit lifecycle decision remains
        // authoritative for deterministic Work admission.
        let intent = parse_turn_intent_response(
            r#"{"domain":"code","communicative_act":"task","requested_scenario":"exploration","work_lifecycle":"required","feedback":{"kind":"preference","target":"approach"},"workspace_mutation":"read_only"}"#,
        )
        .expect("partial typed response must preserve its valid Work decision");
        assert_eq!(intent.work_lifecycle, WorkLifecycleIntent::Required);
        assert_eq!(intent.objective_relation, ObjectiveRelation::Unknown);
        assert!(!intent.browser_verification_required);
    }

    #[test]
    fn omitted_communicative_act_defaults_to_unknown() {
        let intent = parse_turn_intent_response(r#"{"work_lifecycle":"not_required"}"#)
            .expect("minimal typed response");
        assert_eq!(intent.communicative_act, TurnCommunicativeAct::Unknown);
        assert_eq!(intent.work_lifecycle, WorkLifecycleIntent::NotRequired);
    }

    #[test]
    fn unknown_communicative_act_is_malformed() {
        let err = parse_turn_intent_response(
            r#"{"communicative_act":"conversation","objective_relation":"unknown"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn malformed_feedback_returns_malformed() {
        let raw = r#"{"communicative_act":"task","objective_relation":"correct","feedback":{"kind":"correction","target":"unknown_target"}}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn parses_workspace_mutation_and_browser_requirement() {
        let raw = r#"{
          "communicative_act": "task",
          "requested_scenario": "testing",
          "prohibited_scenarios": [],
          "objective_relation": "replace",
          "workspace_mutation": "read_only",
          "browser_verification_required": true
        }"#;
        let intent = parse_turn_intent_response(raw).unwrap();
        assert_eq!(intent.requested_scenario, Some(Scenario::Testing));
        assert_eq!(intent.workspace_mutation, WorkspaceMutationIntent::ReadOnly);
        assert!(intent.browser_verification_required);
    }

    #[test]
    fn unknown_workspace_mutation_returns_malformed() {
        let raw = r#"{"communicative_act":"unknown","objective_relation":"unknown","workspace_mutation":"sometimes"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn non_boolean_browser_requirement_returns_malformed() {
        let raw = r#"{"communicative_act":"unknown","objective_relation":"unknown","browser_verification_required":"yes"}"#;
        let err = parse_turn_intent_response(raw).unwrap_err();
        assert!(matches!(err, TurnIntentJudgeError::Malformed { .. }));
    }

    #[test]
    fn schema_rejects_scenario_aliases() {
        for alias in ["review", "debug", "impl", "quick"] {
            let raw = format!(
                r#"{{"communicative_act":"task","requested_scenario":"{alias}","prohibited_scenarios":[],"objective_relation":"unknown"}}"#
            );
            assert!(
                matches!(
                    parse_turn_intent_response(&raw),
                    Err(TurnIntentJudgeError::Malformed { .. })
                ),
                "non-schema alias {alias:?} must not be normalized"
            );
        }
    }
}
