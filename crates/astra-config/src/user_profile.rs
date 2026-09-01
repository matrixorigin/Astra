//! M5: User Profile System
//!
//! Provides per-user preferences, learned patterns, and typed scenario state.
//!
//! Key features:
//! - User preferences (verbosity, language style, explicit tool blocks)
//! - Scenario strategy selected by the LLM-produced [`TurnIntent`]
//! - Config overrides per user
//! - A/B experiment enrollment

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use astra_turn_types::{ObjectiveRelation, UserFeedback};
use serde::{Deserialize, Serialize};

use crate::lock_ext::RwLockExt;
use crate::runtime_config::RuntimeConfig;

// ─── User Profile ───────────────────────────────────────────────────────────

/// Complete user profile including preferences and learned patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    /// Unique user identifier.
    pub user_id: String,

    /// User preferences.
    pub preferences: UserPreferences,

    /// Scenario selected from the latest accepted typed turn intent.
    pub current_scenario: Option<Scenario>,

    /// Active A/B experiments for this user.
    pub active_experiments: Vec<String>,

    /// Profile creation time.
    pub created_at: SystemTime,

    /// Last update time.
    pub updated_at: SystemTime,

    /// Session statistics.
    pub stats: UserStats,
}

impl UserProfile {
    /// Create a new user profile with defaults.
    pub fn new(user_id: impl Into<String>) -> Self {
        let now = SystemTime::now();
        Self {
            user_id: user_id.into(),
            preferences: UserPreferences::default(),
            current_scenario: None,
            active_experiments: Vec::new(),
            created_at: now,
            updated_at: now,
            stats: UserStats::default(),
        }
    }

    /// Update the last modified timestamp.
    pub fn touch(&mut self) {
        self.updated_at = SystemTime::now();
    }

    /// Apply scenario state from the typed LLM turn-intent contract.
    ///
    /// Query text and tool observations deliberately have no write path to
    /// `current_scenario`. A prohibited active scenario may be cleared even
    /// when the judge does not select a replacement.
    pub fn apply_judged_turn_intent(&mut self, intent: &TurnIntent) {
        if let Some(scenario) = intent
            .requested_scenario
            .filter(|scenario| intent.allows_scenario(*scenario))
        {
            self.current_scenario = Some(scenario);
            self.stats.record_scenario(scenario);
            self.touch();
        } else if self
            .current_scenario
            .is_some_and(|scenario| !intent.allows_scenario(scenario))
        {
            self.current_scenario = None;
            self.touch();
        }
    }

    /// Enroll in an A/B experiment.
    pub fn enroll_experiment(&mut self, experiment_id: impl Into<String>) {
        let id = experiment_id.into();
        if !self.active_experiments.contains(&id) {
            self.active_experiments.push(id);
            self.touch();
        }
    }

    /// Leave an A/B experiment.
    pub fn leave_experiment(&mut self, experiment_id: &str) {
        self.active_experiments.retain(|id| id != experiment_id);
        self.touch();
    }
}

// ─── User Preferences ───────────────────────────────────────────────────────

/// User-specific preferences that affect agent behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPreferences {
    /// Output verbosity level.
    pub verbosity: Verbosity,

    /// Blocked tools (never select).
    pub blocked_tools: Vec<String>,

    /// Language style preferences.
    pub language_style: LanguageStyle,

    /// Response length preference.
    pub response_length: ResponseLength,

    /// Runtime config overrides.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub config_overrides: HashMap<String, serde_json::Value>,

    /// Custom prompt additions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_prompt_suffix: Option<String>,
}

impl Default for UserPreferences {
    fn default() -> Self {
        Self {
            verbosity: Verbosity::Normal,
            blocked_tools: Vec::new(),
            language_style: LanguageStyle::default(),
            response_length: ResponseLength::Medium,
            config_overrides: HashMap::new(),
            custom_prompt_suffix: None,
        }
    }
}

impl UserPreferences {
    /// Apply config overrides to a RuntimeConfig.
    pub fn apply_to_config(&self, config: &mut RuntimeConfig) {
        for (key, value) in &self.config_overrides {
            apply_preference_override(config, key, value);
        }
    }

    /// Check if a tool is blocked.
    pub fn is_blocked_tool(&self, tool_name: &str) -> bool {
        self.blocked_tools
            .iter()
            .any(|t| t == tool_name || tool_name.starts_with(t))
    }
}

/// Apply a single config override based on key path.
fn apply_preference_override(config: &mut RuntimeConfig, key: &str, value: &serde_json::Value) {
    match key {
        "token_budget.max_prompt_tokens" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.max_prompt_tokens = v as u32;
            }
        }
        "token_budget.system_prompt_reserve" => {
            if let Some(v) = value.as_u64() {
                config.token_budget.system_prompt_reserve = v as u32;
            }
        }
        _ => {
            // Unknown key - log or ignore
        }
    }
}

// ─── Verbosity ──────────────────────────────────────────────────────────────

/// Output verbosity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verbosity {
    /// Minimal output, just results.
    Quiet,
    /// Normal explanations.
    #[default]
    Normal,
    /// Detailed explanations with reasoning.
    Verbose,
    /// Debug-level output with all details.
    Debug,
}

impl Verbosity {
    /// Get prompt instruction for this verbosity level.
    pub fn prompt_instruction(&self) -> &'static str {
        match self {
            Verbosity::Quiet => "Be extremely concise. Output only essential information.",
            Verbosity::Normal => "Be concise but clear. Explain important decisions briefly.",
            Verbosity::Verbose => "Explain your reasoning in detail. Walk through each step.",
            Verbosity::Debug => {
                "Provide maximum detail including all reasoning, alternatives considered, and debugging information."
            }
        }
    }
}

// ─── Language Style ─────────────────────────────────────────────────────────

/// Language and communication style preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LanguageStyle {
    /// Preferred language code (e.g., "en", "zh", "ja").
    pub language: String,

    /// Formality level.
    pub formality: Formality,

    /// Use technical jargon freely.
    pub technical_jargon: bool,

    /// Include code comments in generated code.
    pub code_comments: CodeCommentStyle,

    /// Emoji usage preference.
    pub emoji_usage: EmojiUsage,
}

impl Default for LanguageStyle {
    fn default() -> Self {
        Self {
            language: "en".to_string(),
            formality: Formality::Neutral,
            technical_jargon: true,
            code_comments: CodeCommentStyle::Moderate,
            emoji_usage: EmojiUsage::Minimal,
        }
    }
}

impl LanguageStyle {
    /// Get prompt instruction for this style.
    pub fn prompt_instruction(&self) -> String {
        let mut parts = Vec::new();

        if self.language != "en" {
            parts.push(format!("Respond in {}", language_name(&self.language)));
        }

        match self.formality {
            Formality::Casual => parts.push("Use a casual, friendly tone".to_string()),
            Formality::Neutral => {}
            Formality::Formal => parts.push("Use a professional, formal tone".to_string()),
        }

        if !self.technical_jargon {
            parts.push("Avoid technical jargon, explain concepts simply".to_string());
        }

        match self.code_comments {
            CodeCommentStyle::None => {
                parts.push("Don't add comments to generated code".to_string())
            }
            CodeCommentStyle::Minimal => {
                parts.push("Only add comments for non-obvious code".to_string())
            }
            CodeCommentStyle::Moderate => {}
            CodeCommentStyle::Extensive => {
                parts.push("Add detailed comments to all generated code".to_string())
            }
        }

        if parts.is_empty() {
            String::new()
        } else {
            parts.join(". ") + "."
        }
    }
}

fn language_name(code: &str) -> &'static str {
    match code {
        "en" => "English",
        "zh" => "Chinese",
        "ja" => "Japanese",
        "ko" => "Korean",
        "es" => "Spanish",
        "fr" => "French",
        "de" => "German",
        "pt" => "Portuguese",
        "ru" => "Russian",
        _ => "the user's preferred language",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Formality {
    Casual,
    #[default]
    Neutral,
    Formal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CodeCommentStyle {
    None,
    Minimal,
    #[default]
    Moderate,
    Extensive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EmojiUsage {
    None,
    #[default]
    Minimal,
    Moderate,
    Frequent,
}

// ─── Response Length ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ResponseLength {
    /// Very short responses, under 100 words.
    Short,
    /// Medium-length responses.
    #[default]
    Medium,
    /// Detailed responses with full context.
    Long,
}

impl ResponseLength {
    /// Return the system-prompt directive for this response length.
    pub fn prompt_instruction(&self) -> &'static str {
        match self {
            ResponseLength::Short => "Keep responses under 100 words.",
            ResponseLength::Medium => "Keep responses concise but complete.",
            ResponseLength::Long => "Provide detailed, thorough responses.",
        }
    }
}

// ─── Scenario Detection ─────────────────────────────────────────────────────

/// Detected work scenario that affects strategy selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scenario {
    /// Code review: analyzing diffs, suggesting improvements.
    CodeReview,
    /// Debugging: finding and fixing bugs.
    Debugging,
    /// Exploration: understanding codebase, searching.
    Exploration,
    /// Planning: designing architecture, creating plans.
    Planning,
    /// Implementation: writing new code.
    Implementation,
    /// Refactoring: restructuring existing code.
    Refactoring,
    /// Testing: writing or running tests.
    Testing,
    /// Documentation: writing docs, comments.
    Documentation,
    /// DevOps: CI/CD, deployment, configuration.
    DevOps,
    /// Learning: tutorials, explanations.
    Learning,
    /// Quick conceptual Q&A — short questions about how something works, where
    /// something lives, or why a previous decision was made. Deliberately tight
    /// budget so a 37-token "why does X do Y?" cannot ratchet into a 23-tool
    /// exploration. This scenario is preferred over `Exploration` when the query
    /// is short and interrogative AND the workspace doesn't need to be mutated.
    QuickAnswer,
    /// Benchmark or artifact comparison that needs more tool-preview budget for
    /// side-by-side evidence inspection. This is intentionally judge-driven and
    /// has no keyword fallback.
    BenchmarkComparison,
}

/// Structured workspace-mutation intent for the current user turn.
///
/// This is a control-plane field, not a natural-language heuristic. Strong
/// runtime behaviors such as execution retry may only use this structured
/// value (or concrete tool evidence), never ad hoc keyword matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceMutationIntent {
    /// The judge could not determine whether mutation is requested.
    #[default]
    Unknown,
    /// The user is asking for read-only analysis, explanation, or review.
    ReadOnly,
    /// The task may lead to edits, but the current turn does not require them.
    MayMutate,
    /// The user explicitly wants files/workspace state changed in this turn.
    MustMutate,
}

/// Requested state boundary for a mutating turn's completion evidence.
///
/// This is semantic control-plane data produced alongside
/// [`WorkspaceMutationIntent`].  It prevents a task whose accepted outcome is
/// entirely outside the bound workspace (for example managed system state)
/// from being forced to manufacture a workspace edit.  Unknown remains
/// workspace-scoped for fail-closed completion, and mixed tasks still owe the
/// ordinary bound-workspace receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MutationCompletionScope {
    /// The judge omitted or could not determine the state boundary.
    #[default]
    Unknown,
    /// The requested mutation is confined to the bound workspace.
    Workspace,
    /// The requested mutation is confined to executor-managed external state.
    External,
    /// The requested outcome includes both workspace and external state.
    Mixed,
}

impl MutationCompletionScope {
    /// Whether a mutating turn must present a bound-workspace mutation receipt.
    ///
    /// Unknown is deliberately strict so a missing classifier field cannot
    /// weaken the established workspace completion contract.
    #[must_use]
    pub const fn requires_workspace_receipt(self) -> bool {
        !matches!(self, Self::External)
    }
}

/// Domain assigned to the current turn by the semantic LLM judge.
///
/// This value is intentionally part of [`TurnIntent`]: downstream routing
/// telemetry must not reconstruct a domain by matching words in the user's
/// message. `None` means the judge did not provide a reliable domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnIntentDomain {
    #[serde(rename = "github")]
    GitHub,
    Git,
    Code,
    Memory,
    Web,
    System,
    Database,
}

impl TurnIntentDomain {
    /// Stable label used by journal and observability projections.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::Git => "git",
            Self::Code => "code",
            Self::Memory => "memory",
            Self::Web => "web",
            Self::System => "system",
            Self::Database => "database",
        }
    }
}

/// LLM-judged communicative role of the current user turn.
///
/// Tool-surface policy consumes this typed value instead of attempting to
/// recognize greetings, acknowledgements, or tasks from user text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TurnCommunicativeAct {
    /// The user asks the agent to perform work or take an action.
    Task,
    /// The user asks a substantive question that may require tools.
    Question,
    /// The user only acknowledges prior output and requests no further work.
    Acknowledgement,
    /// The user makes a purely social utterance and requests no further work.
    Social,
    /// The judge could not reliably classify the communicative act.
    #[default]
    Unknown,
}

/// Semantic decision about whether this user turn must be represented as
/// canonical Work. It is produced by the turn-intent judge; runtime code must
/// never reconstruct it from user wording or a tool name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkLifecycleIntent {
    /// The judge cannot reliably decide. Runtime preserves ordinary tool
    /// availability rather than inventing a lifecycle requirement.
    #[default]
    Unknown,
    /// The requested outcome is genuinely one-shot and does not need durable
    /// task tracking.
    NotRequired,
    /// The requested outcome needs visible/durable decomposition or the user
    /// explicitly asks for canonical task tracking. Execution must establish
    /// Work before it delegates independent tasks.
    Required,
}

impl TurnCommunicativeAct {
    /// Whether this act may need a tool-bearing model surface.
    ///
    /// Unknown stays tool-capable so a judge failure cannot silently remove
    /// agent capabilities.
    #[must_use]
    pub const fn uses_tool_surface(self) -> bool {
        !matches!(self, Self::Acknowledgement | Self::Social)
    }
}

/// Semantic turn intent produced by an upstream judge/classifier.
///
/// This type is deliberately structural: the runtime policy consumes the
/// requested/prohibited scenarios deterministically and does not parse natural
/// language negations itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TurnIntent {
    /// Judge-owned work domain. Consumers must preserve `None` rather than
    /// infer a replacement from user text, memory snippets, or tool names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<TurnIntentDomain>,
    /// Judge-owned communicative role. Missing output defaults to `Unknown`:
    /// an incomplete auxiliary response must not erase another independently
    /// valid typed decision such as `work_lifecycle = Required`.
    #[serde(default)]
    pub communicative_act: TurnCommunicativeAct,
    /// Scenario the current user turn asks to enter, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_scenario: Option<Scenario>,
    /// Scenarios the current user turn explicitly forbids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub prohibited_scenarios: Vec<Scenario>,
    /// Judge-owned relationship between this message and the session
    /// objective. This replaces the former continuation + reanchor booleans,
    /// whose combinations could represent contradictory states.
    #[serde(default)]
    pub objective_relation: ObjectiveRelation,
    /// Judge-owned requirement for canonical Work lifecycle. This is kept
    /// separate from the broad `Task` communicative act: a small one-step task
    /// is still a task, but it need not create an enduring task graph.
    #[serde(default)]
    pub work_lifecycle: WorkLifecycleIntent,
    /// Optional typed feedback classification. The exact instruction stays in
    /// the canonical user message; this field supplies kind and target only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feedback: Option<UserFeedback>,
    /// Whether the current turn requires, permits, or forbids workspace
    /// mutation. Defaults to `Unknown` so judge failures fail closed.
    #[serde(default)]
    pub workspace_mutation: WorkspaceMutationIntent,
    /// State boundary whose mutation evidence can satisfy this turn.
    #[serde(default)]
    pub mutation_completion_scope: MutationCompletionScope,
    /// Whether the user explicitly requires browser-capable verification for
    /// this turn. Strong browser-verification retry consumes only this
    /// structured field plus tool evidence.
    #[serde(default)]
    pub browser_verification_required: bool,
}

impl TurnIntent {
    #[must_use]
    pub fn with_domain(mut self, domain: TurnIntentDomain) -> Self {
        self.domain = Some(domain);
        self
    }

    #[must_use]
    pub fn with_communicative_act(mut self, act: TurnCommunicativeAct) -> Self {
        self.communicative_act = act;
        self
    }

    #[must_use]
    pub fn with_requested_scenario(mut self, scenario: Scenario) -> Self {
        self.requested_scenario = Some(scenario);
        self
    }

    #[must_use]
    pub fn with_objective_relation(mut self, relation: ObjectiveRelation) -> Self {
        self.objective_relation = relation;
        self
    }

    #[must_use]
    pub fn with_work_lifecycle(mut self, lifecycle: WorkLifecycleIntent) -> Self {
        self.work_lifecycle = lifecycle;
        self
    }

    #[must_use]
    pub fn with_feedback(mut self, feedback: UserFeedback) -> Self {
        self.feedback = Some(feedback);
        self
    }

    #[must_use]
    pub fn with_workspace_mutation(mut self, mutation: WorkspaceMutationIntent) -> Self {
        self.workspace_mutation = mutation;
        self
    }

    #[must_use]
    pub fn with_mutation_completion_scope(mut self, scope: MutationCompletionScope) -> Self {
        self.mutation_completion_scope = scope;
        self
    }

    #[must_use]
    pub fn with_browser_verification_required(mut self, required: bool) -> Self {
        self.browser_verification_required = required;
        self
    }

    #[must_use]
    pub fn prohibit_scenario(mut self, scenario: Scenario) -> Self {
        if !self.prohibited_scenarios.contains(&scenario) {
            self.prohibited_scenarios.push(scenario);
        }
        self
    }

    #[must_use]
    pub fn allows_scenario(&self, scenario: Scenario) -> bool {
        !self.prohibited_scenarios.contains(&scenario)
    }

    #[must_use]
    pub fn reanchors_current_objective(&self) -> bool {
        self.objective_relation.reanchors_current_objective()
    }

    #[must_use]
    pub fn requires_workspace_mutation(&self) -> bool {
        self.workspace_mutation == WorkspaceMutationIntent::MustMutate
            && self.mutation_completion_scope.requires_workspace_receipt()
    }
}

impl Scenario {
    /// Get suggested tool labels for this scenario.
    pub fn suggested_tools(&self) -> Vec<&'static str> {
        match self {
            Scenario::CodeReview => vec!["read_file", "grep", "github"],
            Scenario::Debugging => vec!["bash", "read_file", "grep", "glob"],
            Scenario::Exploration => vec!["glob", "grep", "read_file", "tool_search"],
            Scenario::Planning => vec!["read_file", "write_file", "mo_query"],
            Scenario::Implementation => vec!["str_replace", "write_file", "bash", "read_file"],
            Scenario::Refactoring => vec!["str_replace", "read_file", "grep", "bash"],
            Scenario::Testing => vec!["bash", "read_file", "str_replace", "write_file"],
            Scenario::Documentation => vec!["read_file", "str_replace", "write_file"],
            Scenario::DevOps => vec!["bash", "read_file", "str_replace", "write_file"],
            Scenario::Learning => vec!["read_file", "grep", "web_search"],
            Scenario::QuickAnswer => vec!["read_file", "grep"],
            Scenario::BenchmarkComparison => Vec::new(),
        }
    }

    /// Get strategy adjustments for this scenario.
    pub fn strategy_hints(&self) -> ScenarioStrategy {
        match self {
            Scenario::CodeReview => ScenarioStrategy {
                max_tools_per_turn: 80,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
                memory_top_k: Some(7),
                verification_strictness: Some(0.7),
            },
            Scenario::Debugging => ScenarioStrategy {
                max_tools_per_turn: 100,
                prefer_read_only: false,
                detail_level: Verbosity::Debug,
                memory_top_k: Some(8),
                verification_strictness: None,
            },
            Scenario::Exploration => ScenarioStrategy {
                max_tools_per_turn: 100,
                prefer_read_only: true,
                detail_level: Verbosity::Normal,
                memory_top_k: Some(10),
                verification_strictness: None,
            },
            Scenario::Planning => ScenarioStrategy {
                max_tools_per_turn: 60,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
                memory_top_k: None,
                verification_strictness: None,
            },
            Scenario::Implementation => ScenarioStrategy {
                max_tools_per_turn: 100,
                prefer_read_only: false,
                detail_level: Verbosity::Normal,
                memory_top_k: None,
                verification_strictness: Some(0.6),
            },
            Scenario::Refactoring => ScenarioStrategy {
                max_tools_per_turn: 100,
                prefer_read_only: false,
                detail_level: Verbosity::Verbose,
                memory_top_k: Some(7),
                verification_strictness: Some(0.65),
            },
            Scenario::Testing => ScenarioStrategy {
                max_tools_per_turn: 100,
                prefer_read_only: false,
                detail_level: Verbosity::Normal,
                memory_top_k: None,
                verification_strictness: Some(0.55),
            },
            Scenario::Documentation => ScenarioStrategy {
                max_tools_per_turn: 60,
                prefer_read_only: false,
                detail_level: Verbosity::Verbose,
                memory_top_k: None,
                verification_strictness: None,
            },
            Scenario::DevOps => ScenarioStrategy {
                max_tools_per_turn: 80,
                prefer_read_only: false,
                detail_level: Verbosity::Normal,
                memory_top_k: None,
                verification_strictness: Some(0.6),
            },
            Scenario::Learning => ScenarioStrategy {
                max_tools_per_turn: 80,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
                memory_top_k: Some(10),
                verification_strictness: None,
            },
            // QuickAnswer is intentionally the tightest profile in the set.
            // The execution cap keeps short factual questions from drifting into
            // long tool rounds without an explicit escalation.
            Scenario::QuickAnswer => ScenarioStrategy {
                max_tools_per_turn: 20,
                prefer_read_only: true,
                detail_level: Verbosity::Normal,
                memory_top_k: Some(5),
                verification_strictness: None,
            },
            Scenario::BenchmarkComparison => ScenarioStrategy {
                max_tools_per_turn: 80,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
                memory_top_k: None,
                verification_strictness: Some(0.6),
            },
        }
    }
}

/// Strategy adjustments for a scenario.
#[derive(Debug, Clone)]
pub struct ScenarioStrategy {
    pub max_tools_per_turn: usize,
    pub prefer_read_only: bool,
    pub detail_level: Verbosity,
    /// Suggested memory retrieval top-k override (None = use default).
    pub memory_top_k: Option<u32>,
    /// Suggested verification strictness override (None = use default).
    pub verification_strictness: Option<f64>,
}

// ─── User Stats ─────────────────────────────────────────────────────────────

/// User session statistics for personalization.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserStats {
    /// Total sessions.
    pub total_sessions: u64,
    /// Total queries.
    pub total_queries: u64,
    /// Total tool calls.
    pub total_tool_calls: u64,
    /// Most used tools (tool_name -> count).
    pub tool_usage: HashMap<String, u64>,
    /// Scenario frequency (scenario -> count).
    pub scenario_frequency: HashMap<String, u64>,
    /// Average session duration in seconds.
    pub avg_session_duration_secs: f64,
}

impl UserStats {
    /// Record a tool usage.
    pub fn record_tool_use(&mut self, tool_name: &str) {
        self.total_tool_calls += 1;
        *self.tool_usage.entry(tool_name.to_string()).or_insert(0) += 1;
    }

    /// Record a scenario occurrence.
    pub fn record_scenario(&mut self, scenario: Scenario) {
        let key = format!("{:?}", scenario).to_lowercase();
        *self.scenario_frequency.entry(key).or_insert(0) += 1;
    }

    /// Get top N most used tools.
    pub fn top_tools(&self, n: usize) -> Vec<(&str, u64)> {
        let mut tools: Vec<_> = self
            .tool_usage
            .iter()
            .map(|(k, v)| (k.as_str(), *v))
            .collect();
        tools.sort_by_key(|b| std::cmp::Reverse(b.1));
        tools.truncate(n);
        tools
    }
}

// ─── Profile Store ──────────────────────────────────────────────────────────

/// Storage and retrieval of user profiles.
#[derive(Debug)]
pub struct UserProfileStore {
    /// In-memory profiles.
    profiles: RwLock<HashMap<String, UserProfile>>,
    /// Optional persistence path.
    storage_path: Option<PathBuf>,
}

impl Default for UserProfileStore {
    fn default() -> Self {
        Self::new()
    }
}

impl UserProfileStore {
    /// Create an in-memory store.
    pub fn new() -> Self {
        Self {
            profiles: RwLock::new(HashMap::new()),
            storage_path: None,
        }
    }

    /// Create a persistent store.
    pub fn with_storage(path: PathBuf) -> Self {
        let store = Self {
            profiles: RwLock::new(HashMap::new()),
            storage_path: Some(path.clone()),
        };

        // Load existing profiles
        if path.exists()
            && let Ok(data) = std::fs::read_to_string(&path)
            && let Ok(profiles) = serde_json::from_str::<HashMap<String, UserProfile>>(&data)
        {
            *store.profiles.write_or_recover() = profiles;
        }

        store
    }

    /// Get or create a user profile.
    pub fn get_or_create(&self, user_id: &str) -> UserProfile {
        let mut profiles = self.profiles.write_or_recover();
        if let Some(profile) = profiles.get(user_id) {
            return profile.clone();
        }

        let profile = UserProfile::new(user_id);
        profiles.insert(user_id.to_string(), profile.clone());
        drop(profiles); // release lock before I/O
        self.persist();
        profile
    }

    /// Update a user profile.
    pub fn update(&self, profile: UserProfile) {
        self.profiles
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(profile.user_id.clone(), profile);
        self.persist();
    }

    /// Get a profile if it exists.
    pub fn get(&self, user_id: &str) -> Option<UserProfile> {
        self.profiles
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(user_id)
            .cloned()
    }
    /// Delete a profile.
    pub fn delete(&self, user_id: &str) -> bool {
        let removed = self
            .profiles
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(user_id)
            .is_some();
        if removed {
            self.persist();
        }
        removed
    }

    /// Persist to storage if configured. Uses atomic write (temp + rename)
    /// to avoid data loss on crash.
    fn persist(&self) {
        if let Some(ref path) = self.storage_path
            && let Some(parent) = path.parent()
        {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("[user-profile] failed to create storage directory: {e}");
                return;
            }
            let profiles = self.profiles.read_or_recover();
            if let Ok(data) = serde_json::to_string_pretty(&*profiles) {
                let tmp = path.with_extension("tmp");
                if let Err(e) = std::fs::write(&tmp, &data) {
                    eprintln!("[user-profile] failed to write temp file: {e}");
                    return;
                }
                if let Err(e) = std::fs::rename(&tmp, path) {
                    eprintln!("[user-profile] failed to rename temp file: {e}");
                }
            }
        }
    }
}

// ─── Profile Manager ────────────────────────────────────────────────────────

/// High-level manager for user profiles and non-semantic usage statistics.
pub struct UserProfileManager {
    store: Arc<UserProfileStore>,
}

impl UserProfileManager {
    /// Create a new profile manager backed by the given store.
    pub fn new(store: Arc<UserProfileStore>) -> Self {
        Self { store }
    }

    /// Get the current profile for a user.
    pub fn get_profile(&self, user_id: &str) -> UserProfile {
        self.store.get_or_create(user_id)
    }

    /// Update (and persist) a user profile.
    pub fn update_profile(&self, profile: UserProfile) {
        self.store.update(profile);
    }

    /// Record a query count without interpreting its natural-language text.
    pub fn observe_query(&self, user_id: &str, _query: &str) {
        let mut profile = self.store.get_or_create(user_id);
        profile.stats.total_queries += 1;
        self.store.update(profile);
    }

    /// Record exact tool-use statistics without treating tool names as intent.
    pub fn observe_tool(&self, user_id: &str, tool_name: &str) {
        let mut profile = self.store.get_or_create(user_id);
        profile.stats.record_tool_use(tool_name);
        self.store.update(profile);
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_profile_creation() {
        let profile = UserProfile::new("user123");
        assert_eq!(profile.user_id, "user123");
        assert!(profile.current_scenario.is_none());
        assert!(profile.active_experiments.is_empty());
    }

    #[test]
    fn test_experiment_enrollment() {
        let mut profile = UserProfile::new("user123");
        profile.enroll_experiment("exp-001");
        profile.enroll_experiment("exp-002");
        profile.enroll_experiment("exp-001"); // Duplicate

        assert_eq!(profile.active_experiments.len(), 2);

        profile.leave_experiment("exp-001");
        assert_eq!(profile.active_experiments.len(), 1);
        assert_eq!(profile.active_experiments[0], "exp-002");
    }

    #[test]
    fn turn_intent_tracks_requested_and_prohibited_scenarios() {
        let intent = TurnIntent::default()
            .with_requested_scenario(Scenario::Implementation)
            .prohibit_scenario(Scenario::CodeReview)
            .prohibit_scenario(Scenario::CodeReview);

        assert_eq!(intent.requested_scenario, Some(Scenario::Implementation));
        assert!(intent.allows_scenario(Scenario::Implementation));
        assert!(!intent.allows_scenario(Scenario::CodeReview));
        assert_eq!(intent.prohibited_scenarios, vec![Scenario::CodeReview]);

        let mut profile = UserProfile::new("typed-intent-owner");
        profile.apply_judged_turn_intent(&intent);
        assert_eq!(
            profile.current_scenario,
            Some(Scenario::Implementation),
            "typed turn intent is the scenario state owner"
        );
    }

    #[test]
    fn turn_intent_relation_derives_reanchor_without_a_second_boolean() {
        let continued = TurnIntent::default().with_objective_relation(ObjectiveRelation::Continue);
        assert!(!continued.reanchors_current_objective());

        let reanchored = TurnIntent::default().with_objective_relation(ObjectiveRelation::Correct);
        assert!(reanchored.reanchors_current_objective());

        let replaced = TurnIntent::default().with_objective_relation(ObjectiveRelation::Replace);
        assert!(replaced.reanchors_current_objective());
    }

    #[test]
    fn turn_intent_workspace_mutation_defaults_fail_closed() {
        assert_eq!(TurnIntent::default().domain, None);
        assert_eq!(
            TurnIntent::default().workspace_mutation,
            WorkspaceMutationIntent::Unknown
        );
        assert!(!TurnIntent::default().requires_workspace_mutation());
        assert_eq!(
            TurnIntent::default().work_lifecycle,
            WorkLifecycleIntent::Unknown
        );

        let mutating =
            TurnIntent::default().with_workspace_mutation(WorkspaceMutationIntent::MustMutate);
        assert!(mutating.requires_workspace_mutation());

        let external = mutating
            .clone()
            .with_mutation_completion_scope(MutationCompletionScope::External);
        assert!(
            !external.requires_workspace_mutation(),
            "an explicit external-only outcome must not manufacture a workspace edit"
        );
        for scope in [
            MutationCompletionScope::Unknown,
            MutationCompletionScope::Workspace,
            MutationCompletionScope::Mixed,
        ] {
            assert!(
                mutating
                    .clone()
                    .with_mutation_completion_scope(scope)
                    .requires_workspace_mutation(),
                "{scope:?} must retain the bound-workspace receipt gate"
            );
        }
    }

    #[test]
    fn turn_intent_domain_has_stable_typed_labels() {
        let intent = TurnIntent::default().with_domain(TurnIntentDomain::GitHub);
        assert_eq!(intent.domain.map(TurnIntentDomain::as_str), Some("github"));
        assert_eq!(
            serde_json::to_value(intent).unwrap()["domain"],
            "github",
            "the strict judge schema and journal projection share one label"
        );
    }

    #[test]
    fn communicative_act_tool_surface_policy_is_structural() {
        for act in [
            TurnCommunicativeAct::Task,
            TurnCommunicativeAct::Question,
            TurnCommunicativeAct::Unknown,
        ] {
            assert!(act.uses_tool_surface(), "{act:?} must stay tool-capable");
        }
        for act in [
            TurnCommunicativeAct::Acknowledgement,
            TurnCommunicativeAct::Social,
        ] {
            assert!(
                !act.uses_tool_surface(),
                "{act:?} must produce a tool-free base surface"
            );
        }
    }

    #[test]
    fn test_blocked_tool_check() {
        let prefs = UserPreferences {
            blocked_tools: vec!["bash".to_string()],
            ..UserPreferences::default()
        };

        assert!(prefs.is_blocked_tool("bash"));
        assert!(!prefs.is_blocked_tool("read_file"));
    }

    #[test]
    fn test_verbosity_prompt() {
        assert!(Verbosity::Quiet.prompt_instruction().contains("concise"));
        assert!(Verbosity::Debug.prompt_instruction().contains("maximum"));
    }

    #[test]
    fn test_scenario_strategy() {
        let strategy = Scenario::Debugging.strategy_hints();
        assert_eq!(strategy.detail_level, Verbosity::Debug);
        assert!(!strategy.prefer_read_only);

        let strategy = Scenario::CodeReview.strategy_hints();
        assert!(strategy.prefer_read_only);
    }

    #[test]
    fn test_user_stats() {
        let mut stats = UserStats::default();
        stats.record_tool_use("read_file");
        stats.record_tool_use("read_file");
        stats.record_tool_use("grep");

        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.tool_usage.get("read_file"), Some(&2));

        let top = stats.top_tools(2);
        assert_eq!(top[0], ("read_file", 2));
    }

    #[test]
    fn test_profile_store() {
        let store = UserProfileStore::new();

        let profile1 = store.get_or_create("user1");
        assert_eq!(profile1.user_id, "user1");

        let mut modified = profile1.clone();
        modified.enroll_experiment("exp1");
        store.update(modified);

        let retrieved = store.get("user1").unwrap();
        assert_eq!(retrieved.active_experiments.len(), 1);
    }

    #[test]
    fn test_language_style_instruction() {
        let style = LanguageStyle {
            language: "zh".to_string(),
            formality: Formality::Formal,
            ..LanguageStyle::default()
        };

        let instruction = style.prompt_instruction();
        assert!(instruction.contains("Chinese"));
        assert!(instruction.contains("formal"));
    }

    #[test]
    fn test_profile_manager_observation() {
        let store = Arc::new(UserProfileStore::new());
        let manager = UserProfileManager::new(store);

        manager.observe_query("user1", "opaque query");
        manager.observe_tool("user1", "grep");

        let profile = manager.get_profile("user1");
        assert_eq!(profile.stats.total_queries, 1);
        assert_eq!(profile.stats.total_tool_calls, 1);
        assert_eq!(profile.current_scenario, None);
    }
}
