//! M5: User Profile System
//!
//! Provides per-user preferences, learned patterns, and scenario detection.
//!
//! Key features:
//! - User preferences (verbosity, tools, language style)
//! - Scenario detection (code review, debugging, exploration, planning)
//! - Config overrides per user
//! - A/B experiment enrollment

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::runtime_config::RuntimeConfig;

// ─── User Profile ───────────────────────────────────────────────────────────

/// Complete user profile including preferences and learned patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserProfile {
    /// Unique user identifier.
    pub user_id: String,

    /// User preferences.
    pub preferences: UserPreferences,

    /// Detected scenario for current session.
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

    /// Set current scenario.
    pub fn set_scenario(&mut self, scenario: Scenario) {
        self.current_scenario = Some(scenario);
        self.touch();
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

    /// Preferred tools (boost in selection).
    pub preferred_tools: Vec<String>,

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
            preferred_tools: Vec::new(),
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

    /// Check if a tool is preferred.
    pub fn is_preferred_tool(&self, tool_name: &str) -> bool {
        self.preferred_tools
            .iter()
            .any(|t| t == tool_name || tool_name.starts_with(t))
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
        "tool_selection.confidence_threshold" => {
            if let Some(v) = value.as_f64() {
                config.tool_selection.confidence_threshold = v;
            }
        }
        "tool_selection.max_tools" => {
            if let Some(v) = value.as_u64() {
                config.tool_selection.max_tools = v as u32;
            }
        }
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
}

impl Scenario {
    /// Get recommended tool preferences for this scenario.
    pub fn recommended_tools(&self) -> Vec<&'static str> {
        match self {
            Scenario::CodeReview => vec!["view", "grep", "github-mcp-server-pull_request_read"],
            Scenario::Debugging => vec!["bash", "view", "grep", "glob"],
            Scenario::Exploration => vec!["glob", "grep", "view", "github-mcp-server-search_code"],
            Scenario::Planning => vec!["view", "create", "sql"],
            Scenario::Implementation => vec!["edit", "create", "bash", "view"],
            Scenario::Refactoring => vec!["edit", "view", "grep", "bash"],
            Scenario::Testing => vec!["bash", "view", "edit", "create"],
            Scenario::Documentation => vec!["view", "edit", "create"],
            Scenario::DevOps => vec!["bash", "view", "edit", "create"],
            Scenario::Learning => vec!["view", "grep", "web_search"],
        }
    }

    /// Get strategy adjustments for this scenario.
    pub fn strategy_hints(&self) -> ScenarioStrategy {
        match self {
            Scenario::CodeReview => ScenarioStrategy {
                max_tools_per_turn: 3,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
            },
            Scenario::Debugging => ScenarioStrategy {
                max_tools_per_turn: 5,
                prefer_read_only: false,
                detail_level: Verbosity::Debug,
            },
            Scenario::Exploration => ScenarioStrategy {
                max_tools_per_turn: 6,
                prefer_read_only: true,
                detail_level: Verbosity::Normal,
            },
            Scenario::Planning => ScenarioStrategy {
                max_tools_per_turn: 2,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
            },
            Scenario::Implementation => ScenarioStrategy {
                max_tools_per_turn: 4,
                prefer_read_only: false,
                detail_level: Verbosity::Normal,
            },
            Scenario::Refactoring => ScenarioStrategy {
                max_tools_per_turn: 4,
                prefer_read_only: false,
                detail_level: Verbosity::Verbose,
            },
            Scenario::Testing => ScenarioStrategy {
                max_tools_per_turn: 5,
                prefer_read_only: false,
                detail_level: Verbosity::Normal,
            },
            Scenario::Documentation => ScenarioStrategy {
                max_tools_per_turn: 3,
                prefer_read_only: false,
                detail_level: Verbosity::Verbose,
            },
            Scenario::DevOps => ScenarioStrategy {
                max_tools_per_turn: 4,
                prefer_read_only: false,
                detail_level: Verbosity::Normal,
            },
            Scenario::Learning => ScenarioStrategy {
                max_tools_per_turn: 4,
                prefer_read_only: true,
                detail_level: Verbosity::Verbose,
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
}

// ─── Scenario Detector ──────────────────────────────────────────────────────

/// Detects the current work scenario based on query patterns and tool usage.
#[derive(Debug, Default)]
pub struct ScenarioDetector {
    /// Recent queries for pattern matching.
    recent_queries: Vec<String>,
    /// Recent tool calls.
    recent_tools: Vec<String>,
    /// Detection confidence threshold.
    confidence_threshold: f64,
}

impl ScenarioDetector {
    pub fn new() -> Self {
        Self {
            recent_queries: Vec::new(),
            recent_tools: Vec::new(),
            confidence_threshold: 0.6,
        }
    }

    /// Add a query for analysis.
    pub fn observe_query(&mut self, query: &str) {
        self.recent_queries.push(query.to_lowercase());
        // Keep last 5 queries
        if self.recent_queries.len() > 5 {
            self.recent_queries.remove(0);
        }
    }

    /// Add a tool call for analysis.
    pub fn observe_tool(&mut self, tool_name: &str) {
        self.recent_tools.push(tool_name.to_string());
        // Keep last 10 tool calls
        if self.recent_tools.len() > 10 {
            self.recent_tools.remove(0);
        }
    }

    /// Detect the most likely scenario.
    pub fn detect(&self) -> Option<(Scenario, f64)> {
        let scores = self.score_scenarios();

        // Find the highest scoring scenario above threshold
        scores
            .into_iter()
            .filter(|(_, score)| *score >= self.confidence_threshold)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// Score all scenarios based on current evidence.
    fn score_scenarios(&self) -> Vec<(Scenario, f64)> {
        let scenarios = [
            Scenario::CodeReview,
            Scenario::Debugging,
            Scenario::Exploration,
            Scenario::Planning,
            Scenario::Implementation,
            Scenario::Refactoring,
            Scenario::Testing,
            Scenario::Documentation,
            Scenario::DevOps,
            Scenario::Learning,
        ];

        scenarios
            .into_iter()
            .map(|s| {
                let query_score = self.score_queries(s);
                let tool_score = self.score_tools(s);
                // Weight queries slightly more than tools
                let combined = query_score * 0.6 + tool_score * 0.4;
                (s, combined)
            })
            .collect()
    }

    fn score_queries(&self, scenario: Scenario) -> f64 {
        if self.recent_queries.is_empty() {
            return 0.0;
        }

        let keywords = scenario_keywords(scenario);
        let mut matches = 0;

        for query in &self.recent_queries {
            for keyword in &keywords {
                if query.contains(keyword) {
                    matches += 1;
                    break; // One match per query
                }
            }
        }

        matches as f64 / self.recent_queries.len() as f64
    }

    fn score_tools(&self, scenario: Scenario) -> f64 {
        if self.recent_tools.is_empty() {
            return 0.0;
        }

        let recommended = scenario.recommended_tools();
        let mut matches = 0;

        for tool in &self.recent_tools {
            if recommended.iter().any(|r| tool.contains(r)) {
                matches += 1;
            }
        }

        matches as f64 / self.recent_tools.len() as f64
    }

    /// Clear detection history.
    pub fn clear(&mut self) {
        self.recent_queries.clear();
        self.recent_tools.clear();
    }
}

fn scenario_keywords(scenario: Scenario) -> Vec<&'static str> {
    match scenario {
        Scenario::CodeReview => vec![
            "review",
            "pr",
            "pull request",
            "diff",
            "change",
            "feedback",
            "comment",
            "approve",
        ],
        Scenario::Debugging => vec![
            "bug", "fix", "error", "crash", "debug", "issue", "problem", "wrong", "fail", "broken",
        ],
        Scenario::Exploration => vec![
            "find",
            "search",
            "where",
            "what",
            "how",
            "explore",
            "look",
            "understand",
            "show",
        ],
        Scenario::Planning => vec![
            "plan",
            "design",
            "architect",
            "strategy",
            "approach",
            "think",
            "consider",
            "propose",
        ],
        Scenario::Implementation => vec![
            "create",
            "implement",
            "add",
            "build",
            "write",
            "new",
            "feature",
            "develop",
        ],
        Scenario::Refactoring => vec![
            "refactor",
            "restructure",
            "clean",
            "improve",
            "optimize",
            "reorganize",
            "simplify",
        ],
        Scenario::Testing => vec![
            "test",
            "spec",
            "assert",
            "verify",
            "coverage",
            "unit",
            "integration",
        ],
        Scenario::Documentation => vec![
            "doc", "comment", "readme", "explain", "describe", "document",
        ],
        Scenario::DevOps => vec![
            "deploy",
            "ci",
            "cd",
            "pipeline",
            "docker",
            "kubernetes",
            "config",
            "env",
        ],
        Scenario::Learning => vec![
            "learn", "tutorial", "example", "teach", "explain", "how does", "what is",
        ],
    }
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
        tools.sort_by(|a, b| b.1.cmp(&a.1));
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
        if path.exists() {
            if let Ok(data) = std::fs::read_to_string(&path) {
                if let Ok(profiles) = serde_json::from_str::<HashMap<String, UserProfile>>(&data) {
                    *store.profiles.write().unwrap() = profiles;
                }
            }
        }

        store
    }

    /// Get or create a user profile.
    pub fn get_or_create(&self, user_id: &str) -> UserProfile {
        let mut profiles = self.profiles.write().unwrap();
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
            .unwrap()
            .insert(profile.user_id.clone(), profile);
        self.persist();
    }

    /// Get a profile if it exists.
    pub fn get(&self, user_id: &str) -> Option<UserProfile> {
        self.profiles.read().unwrap().get(user_id).cloned()
    }

    /// List all user IDs.
    pub fn list_users(&self) -> Vec<String> {
        self.profiles.read().unwrap().keys().cloned().collect()
    }

    /// Delete a profile.
    pub fn delete(&self, user_id: &str) -> bool {
        let removed = self.profiles.write().unwrap().remove(user_id).is_some();
        if removed {
            self.persist();
        }
        removed
    }

    /// Persist to storage if configured. Uses atomic write (temp + rename)
    /// to avoid data loss on crash.
    fn persist(&self) {
        if let Some(ref path) = self.storage_path {
            let profiles = self.profiles.read().unwrap();
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

/// High-level manager for user profiles with scenario detection.
pub struct UserProfileManager {
    store: Arc<UserProfileStore>,
    detectors: RwLock<HashMap<String, ScenarioDetector>>,
}

impl UserProfileManager {
    pub fn new(store: Arc<UserProfileStore>) -> Self {
        Self {
            store,
            detectors: RwLock::new(HashMap::new()),
        }
    }

    /// Get the current profile for a user.
    pub fn get_profile(&self, user_id: &str) -> UserProfile {
        self.store.get_or_create(user_id)
    }

    /// Record a user query and update scenario detection.
    pub fn observe_query(&self, user_id: &str, query: &str) {
        let mut detectors = self.detectors.write().unwrap();
        let detector = detectors
            .entry(user_id.to_string())
            .or_insert_with(ScenarioDetector::new);
        detector.observe_query(query);

        // Update profile with new query count
        let mut profile = self.store.get_or_create(user_id);
        profile.stats.total_queries += 1;

        // Check for scenario detection
        if let Some((scenario, _confidence)) = detector.detect() {
            profile.set_scenario(scenario);
            profile.stats.record_scenario(scenario);
        }

        self.store.update(profile);
    }

    /// Record a tool call.
    pub fn observe_tool(&self, user_id: &str, tool_name: &str) {
        let mut detectors = self.detectors.write().unwrap();
        let detector = detectors
            .entry(user_id.to_string())
            .or_insert_with(ScenarioDetector::new);
        detector.observe_tool(tool_name);

        // Update profile stats
        let mut profile = self.store.get_or_create(user_id);
        profile.stats.record_tool_use(tool_name);
        self.store.update(profile);
    }

    /// Get the current detected scenario for a user.
    pub fn get_scenario(&self, user_id: &str) -> Option<Scenario> {
        self.store.get(user_id).and_then(|p| p.current_scenario)
    }

    /// Generate prompt instructions based on user profile.
    pub fn generate_prompt_instructions(&self, user_id: &str) -> String {
        let profile = self.store.get_or_create(user_id);
        let mut instructions = Vec::new();

        // Verbosity
        let verbosity_instr = profile.preferences.verbosity.prompt_instruction();
        if !verbosity_instr.is_empty() {
            instructions.push(verbosity_instr.to_string());
        }

        // Response length
        let length_instr = profile.preferences.response_length.prompt_instruction();
        if !length_instr.is_empty() {
            instructions.push(length_instr.to_string());
        }

        // Language style
        let style_instr = profile.preferences.language_style.prompt_instruction();
        if !style_instr.is_empty() {
            instructions.push(style_instr);
        }

        // Custom suffix
        if let Some(ref suffix) = profile.preferences.custom_prompt_suffix {
            instructions.push(suffix.clone());
        }

        // Scenario-specific hints
        if let Some(scenario) = profile.current_scenario {
            let strategy = scenario.strategy_hints();
            if strategy.prefer_read_only {
                instructions.push(
                    "Focus on reading and analysis, avoid modifications unless asked.".to_string(),
                );
            }
        }

        if instructions.is_empty() {
            String::new()
        } else {
            format!(
                "<user_preferences>\n{}\n</user_preferences>",
                instructions.join("\n")
            )
        }
    }

    /// Clear scenario detection history for a user.
    pub fn clear_detection(&self, user_id: &str) {
        if let Some(detector) = self.detectors.write().unwrap().get_mut(user_id) {
            detector.clear();
        }
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
    fn test_preference_tool_check() {
        let mut prefs = UserPreferences::default();
        prefs.preferred_tools = vec!["view".to_string(), "grep".to_string()];
        prefs.blocked_tools = vec!["bash".to_string()];

        assert!(prefs.is_preferred_tool("view"));
        assert!(prefs.is_preferred_tool("grep"));
        assert!(!prefs.is_preferred_tool("edit"));

        assert!(prefs.is_blocked_tool("bash"));
        assert!(!prefs.is_blocked_tool("view"));
    }

    #[test]
    fn test_scenario_detection() {
        let mut detector = ScenarioDetector::new();

        // Simulate debugging scenario
        detector.observe_query("there's a bug in the auth module");
        detector.observe_query("why is this test failing");
        detector.observe_tool("bash");
        detector.observe_tool("view");

        let result = detector.detect();
        assert!(result.is_some());
        let (scenario, confidence) = result.unwrap();
        assert_eq!(scenario, Scenario::Debugging);
        assert!(confidence >= 0.6);
    }

    #[test]
    fn test_scenario_keywords() {
        let keywords = scenario_keywords(Scenario::CodeReview);
        assert!(keywords.contains(&"review"));
        assert!(keywords.contains(&"pr"));

        let keywords = scenario_keywords(Scenario::Testing);
        assert!(keywords.contains(&"test"));
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
        stats.record_tool_use("view");
        stats.record_tool_use("view");
        stats.record_tool_use("grep");

        assert_eq!(stats.total_tool_calls, 3);
        assert_eq!(stats.tool_usage.get("view"), Some(&2));

        let top = stats.top_tools(2);
        assert_eq!(top[0], ("view", 2));
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
        let mut style = LanguageStyle::default();
        style.language = "zh".to_string();
        style.formality = Formality::Formal;

        let instruction = style.prompt_instruction();
        assert!(instruction.contains("Chinese"));
        assert!(instruction.contains("formal"));
    }

    #[test]
    fn test_profile_manager_observation() {
        let store = Arc::new(UserProfileStore::new());
        let manager = UserProfileManager::new(store);

        manager.observe_query("user1", "find all tests");
        manager.observe_tool("user1", "grep");

        let profile = manager.get_profile("user1");
        assert_eq!(profile.stats.total_queries, 1);
        assert_eq!(profile.stats.total_tool_calls, 1);
    }
}
