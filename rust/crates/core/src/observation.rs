//! Turn metrics: pure data collection for agent self-observation.
//!
//! This module collects orthogonal metric dimensions from each turn's tool
//! calls. The data is surfaced through `introspect` so the agent can observe
//! its own behavior and adjust — no intermediate layer pre-judges agent state.
//!
//! # Architecture
//!
//! ```text
//! ToolCallRecords → TurnMetrics → introspect exposure → Agent self-adjusts
//! ```
//!
//! All judgment ("am I stuck?", "should I change approach?") belongs to the
//! agent, not to the framework. TurnMetrics is a mirror, not a teacher.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

/// Lightweight sample representing a single tool call for metrics computation.
/// Decoupled from `astra_services::session_journal::ToolCallRecord` to avoid
/// cross-crate dependency from core → services.
#[derive(Debug, Clone)]
pub struct ToolCallSample<'a> {
    pub name: &'a str,
    pub ok: bool,
    pub round: Option<u32>,
    pub file_path: Option<&'a str>,
    pub error: Option<&'a str>,
}

/// A consecutive streak of failures for the same tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorStreak {
    /// Tool name that failed.
    pub tool_name: String,
    /// Number of consecutive failures.
    pub count: u32,
    /// First error message (truncated to 200 chars).
    pub first_error: String,
}

/// Orthogonal metric dimensions extracted from turn state.
///
/// These are intentionally coarse-grained to avoid overfitting to specific
/// tool names or scenarios. Tool families are abstract categories.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TurnMetrics {
    /// Total LLM rounds completed this turn.
    pub rounds_completed: u32,
    /// Total tool calls (successful + failed).
    pub tool_calls_total: u32,
    /// Tool calls grouped by abstract family.
    pub tool_calls_by_family: BTreeMap<ToolFamily, u32>,
    /// Number of distinct tool names used.
    pub unique_tools_used: u32,
    /// Workspace-mutating operations (writes, edits, git commits).
    pub mutation_count: u32,
    /// Failed tool calls.
    pub error_count: u32,
    /// Calls that hit idempotency cache or returned identical results.
    pub cache_hits: u32,
    /// Rounds since the last workspace mutation.
    pub rounds_since_last_mutation: u32,
    /// Tokens consumed this turn (approximate).
    pub tokens_consumed: u64,
    /// Top tools by frequency: (tool_name, call_count), sorted descending.
    /// Used for evidence-driven nudges.
    pub top_tools: Vec<(String, u32)>,
    /// File access counts: (file_path, access_count) for read/write tools.
    /// Used to detect excessive exploration of the same file.
    pub file_access_counts: BTreeMap<String, u32>,
    /// Consecutive failure streaks per tool, sorted by count descending.
    /// Used to detect spirals on the same tool (e.g., str_replace 3x old_str not found).
    pub error_streaks: Vec<ErrorStreak>,
}

/// Abstract tool families — coarser than individual tool names.
///
/// Each tool maps to exactly one family. Search tools (grep, glob) are
/// classified as `Read` since they are read-only inspection operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ToolFamily {
    /// Read-only inspection: read_file, grep, glob, list_dir, symbols.
    Read,
    /// Workspace mutation: str_replace, write_file, apply_patch.
    Write,
    /// Version control: git operations.
    Git,
    /// Shell execution: bash, shell.
    Shell,
    /// Other tools (memory, notify, etc.).
    Other,
}

impl std::fmt::Display for ToolFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read => write!(f, "read"),
            Self::Write => write!(f, "write"),
            Self::Git => write!(f, "git"),
            Self::Shell => write!(f, "shell"),
            Self::Other => write!(f, "other"),
        }
    }
}

/// Classify a tool name (lowercase) into an abstract family.
///
/// Search tools (grep, glob, list_dir) are classified as `Read` since they
/// are read-only inspection operations.
pub fn classify_tool_family(tool_name: &str) -> ToolFamily {
    match tool_name {
        // Read-only inspection
        "read_file" | "grep" | "glob" | "list_dir" | "symbols" | "read" => ToolFamily::Read,
        // Workspace mutation
        "str_replace" | "write_file" | "apply_patch" | "write" | "edit" => ToolFamily::Write,
        // Version control
        "git" | "git_commit" | "git_push" | "git_diff" | "git_log" | "git_blame" => ToolFamily::Git,
        // Shell execution
        "bash" | "shell" | "run_command" | "exec" => ToolFamily::Shell,
        // Everything else
        _ => ToolFamily::Other,
    }
}

fn is_workspace_mutation_tool(tool_name: &str, family: ToolFamily) -> bool {
    match family {
        ToolFamily::Write => true,
        ToolFamily::Git => matches!(
            tool_name,
            "git_commit" | "git_push" | "git_merge" | "git_rebase" | "git_checkout" | "git_reset"
        ),
        _ => false,
    }
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

impl TurnMetrics {
    /// Build metrics from lightweight `ToolCallSample` records.
    pub fn from_samples(
        samples: &[ToolCallSample<'_>],
        rounds_completed: u32,
        tokens_consumed: u64,
    ) -> Self {
        let mut by_family: BTreeMap<ToolFamily, u32> = BTreeMap::new();
        let mut unique_tools = std::collections::HashSet::new();
        let mut mutation_count = 0u32;
        let mut error_count = 0u32;
        let mut last_mutation_round: Option<u32> = None;
        let mut file_access_counts: BTreeMap<String, u32> = BTreeMap::new();
        // Track consecutive failure streaks: tool_name → (count, first_error)
        let mut streak_map: BTreeMap<String, (u32, String)> = BTreeMap::new();
        let mut last_tool: Option<String> = None;

        for sample in samples {
            let lower = sample.name.to_lowercase();
            unique_tools.insert(lower.clone());

            let family = classify_tool_family(&lower);
            *by_family.entry(family).or_insert(0) += 1;

            if is_workspace_mutation_tool(&lower, family) {
                mutation_count += 1;
                if let Some(r) = sample.round {
                    last_mutation_round = Some(last_mutation_round.map_or(r, |prev| prev.max(r)));
                }
            }
            if !sample.ok {
                error_count += 1;
                // Extend or start a streak
                if let Some(ref last) = last_tool {
                    if *last == lower {
                        // Same tool failed consecutively — extend streak
                        if let Some(entry) = streak_map.get_mut(&lower) {
                            entry.0 += 1;
                        } else {
                            streak_map.insert(
                                lower.clone(),
                                (
                                    1,
                                    sample
                                        .error
                                        .map(|e| truncate_chars(e, 200))
                                        .unwrap_or_default(),
                                ),
                            );
                        }
                    } else {
                        // Different tool failed — start new streak for this tool
                        let first_err = sample
                            .error
                            .map(|e| truncate_chars(e, 200))
                            .unwrap_or_default();
                        streak_map.insert(lower.clone(), (1, first_err));
                    }
                } else {
                    // First failure in turn
                    let first_err = sample
                        .error
                        .map(|e| truncate_chars(e, 200))
                        .unwrap_or_default();
                    streak_map.insert(lower.clone(), (1, first_err));
                }
                last_tool = Some(lower);
            } else {
                // Success resets streak for this tool
                if let Some(ref last) = last_tool
                    && *last == lower
                {
                    streak_map.remove(&lower);
                }
                last_tool = Some(lower);
            }

            // Track file access for read/write tools
            if matches!(family, ToolFamily::Read | ToolFamily::Write)
                && let Some(path) = sample.file_path
            {
                *file_access_counts.entry(path.to_string()).or_insert(0) += 1;
            }
        }

        let rounds_since_last_mutation = match last_mutation_round {
            None => rounds_completed,
            Some(r) => rounds_completed.saturating_sub(r),
        };

        // Build top_tools: sorted by frequency descending, take top 5
        let mut tool_freq: Vec<(String, u32)> = unique_tools
            .iter()
            .map(|name| {
                let count = samples
                    .iter()
                    .filter(|s| s.name.to_lowercase() == *name)
                    .count() as u32;
                (name.clone(), count)
            })
            .collect();
        tool_freq.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let top_tools: Vec<(String, u32)> = tool_freq.into_iter().take(5).collect();

        // Convert streak map to sorted list
        let mut error_streaks: Vec<ErrorStreak> = streak_map
            .into_iter()
            .map(|(tool, (count, first_error))| ErrorStreak {
                tool_name: tool,
                count,
                first_error,
            })
            .collect();
        error_streaks.sort_by_key(|b| std::cmp::Reverse(b.count));

        Self {
            rounds_completed,
            tool_calls_total: samples.len() as u32,
            tool_calls_by_family: by_family,
            unique_tools_used: unique_tools.len() as u32,
            mutation_count,
            error_count,
            cache_hits: 0, // not tracked in journal records
            rounds_since_last_mutation,
            tokens_consumed,
            top_tools,
            file_access_counts,
            error_streaks,
        }
    }

    /// Build metrics from raw tool-call records (legacy tuple API).
    ///
    /// Each record is `(tool_name, ok, round, file_path)`. Surgically-removed
    /// placeholders should already be filtered out before calling this.
    #[deprecated(note = "Use from_samples with ToolCallSample for richer error tracking")]
    pub fn from_tool_records(
        records: &[(&str, bool, Option<u32>, Option<&str>)],
        rounds_completed: u32,
        tokens_consumed: u64,
    ) -> Self {
        // Sliding window: only count records from the last DEFAULT_WINDOW rounds.
        // Records without a round marker are always included (conservative).
        // e.g., rounds_completed=6, window=3 → rounds 4,5,6
        const DEFAULT_WINDOW: u32 = 3;
        let window_start = rounds_completed.saturating_sub(DEFAULT_WINDOW.saturating_sub(1));
        let filtered: Vec<&(_, _, _, _)> = records
            .iter()
            .filter(|r| r.2.is_none_or(|round| round >= window_start))
            .collect();
        let samples: Vec<ToolCallSample<'_>> = filtered
            .iter()
            .map(|&&(name, ok, round, file_path)| ToolCallSample {
                name,
                ok,
                round,
                file_path,
                error: None,
            })
            .collect();
        Self::from_samples(&samples, rounds_completed, tokens_consumed)
    }

    /// Compute derived metrics that are ratios or aggregates.
    pub fn read_write_ratio(&self) -> Option<f64> {
        let reads = self
            .tool_calls_by_family
            .get(&ToolFamily::Read)
            .copied()
            .unwrap_or(0);
        let writes = self
            .tool_calls_by_family
            .get(&ToolFamily::Write)
            .copied()
            .unwrap_or(0);
        if writes == 0 {
            None
        } else {
            Some(reads as f64 / writes as f64)
        }
    }

    pub fn repetition_ratio(&self) -> f64 {
        if self.tool_calls_total == 0 {
            0.0
        } else {
            self.unique_tools_used as f64 / self.tool_calls_total as f64
        }
    }

    pub fn error_rate(&self) -> f64 {
        if self.tool_calls_total == 0 {
            0.0
        } else {
            self.error_count as f64 / self.tool_calls_total as f64
        }
    }

    pub fn cache_hit_ratio(&self) -> f64 {
        if self.tool_calls_total == 0 {
            0.0
        } else {
            self.cache_hits as f64 / self.tool_calls_total as f64
        }
    }
}
/// Canonical observation facet vocabulary shared by `introspect` and `reflect`.
///
/// This enum defines the unified observation-plane taxonomy. Both tools map their
/// legacy facet names to these canonical variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObservationFacet {
    /// Session-level health: pressure, cache, turns, alerts.
    #[default]
    Session,
    /// Recent execution rounds and tool calls.
    Recent,
    /// Currently-pending volatile injections.
    Volatile,
    /// Stall / loop-guard state and circuit breaker telemetry.
    Stall,
    /// Per-channel freshness of runtime-injected signals.
    Noise,
    /// Recent tool errors and failure previews.
    Errors,
    /// Tool execution trace and outcome history.
    Trace,
    /// Overview: aggregated session summary.
    Overview,
    /// CLI/Edge-local prompt-cache captures and cache diagnosis.
    Cache,
    /// CLI/Edge-local session-memory extraction and injection artifacts.
    SessionMemory,
}

/// Error type for facet parsing failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationFacetError {
    unknown: String,
}

impl fmt::Display for ObservationFacetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown observation facet: {}", self.unknown)
    }
}

impl std::error::Error for ObservationFacetError {}

impl std::str::FromStr for ObservationFacet {
    type Err = ObservationFacetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "session" | "runtime" => Ok(Self::Session),
            "recent" => Ok(Self::Recent),
            "volatile" => Ok(Self::Volatile),
            "stall" => Ok(Self::Stall),
            "noise" | "freshness" => Ok(Self::Noise),
            "errors" => Ok(Self::Errors),
            "trace" | "history" => Ok(Self::Trace),
            "overview" | "all" => Ok(Self::Overview),
            "cache" => Ok(Self::Cache),
            "session_memory" => Ok(Self::SessionMemory),
            unknown => Err(ObservationFacetError {
                unknown: unknown.to_string(),
            }),
        }
    }
}

impl ObservationFacet {
    /// Convert to canonical string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Recent => "recent",
            Self::Volatile => "volatile",
            Self::Stall => "stall",
            Self::Noise => "noise",
            Self::Errors => "errors",
            Self::Trace => "trace",
            Self::Overview => "overview",
            Self::Cache => "cache",
            Self::SessionMemory => "session_memory",
        }
    }
}

impl fmt::Display for ObservationFacet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

// ── Shared observation-plane types ──
// Used by both introspect and reflect for typed request/response fields.

/// Normalized top-level observation topic for both `introspect` and `reflect`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObservationTopic {
    Overview,
    #[default]
    Runtime,
    Execution,
    Knowledge,
}

impl ObservationTopic {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
            "overview" | "" => Self::Overview,
            "execution" => Self::Execution,
            "knowledge" => Self::Knowledge,
            "runtime" | "session" => Self::Runtime,
            _ => Self::Runtime,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Runtime => "runtime",
            Self::Execution => "execution",
            Self::Knowledge => "knowledge",
        }
    }
}

impl fmt::Display for ObservationTopic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Output depth for observation requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObservationDepth {
    Hint,
    #[default]
    Summary,
    Diagnostic,
    Forensic,
}

impl ObservationDepth {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
            "hint" => Self::Hint,
            "diagnostic" => Self::Diagnostic,
            "forensic" => Self::Forensic,
            "summary" | "" => Self::Summary,
            _ => Self::Summary,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hint => "hint",
            Self::Summary => "summary",
            Self::Diagnostic => "diagnostic",
            Self::Forensic => "forensic",
        }
    }
}

/// Time horizon for the observation request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ObservationHorizon {
    Now,
    #[default]
    CurrentTurn,
    Recent,
    Turn,
    Session,
    CrossSession,
}

impl ObservationHorizon {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
            "now" => Self::Now,
            "recent" => Self::Recent,
            "turn" => Self::Turn,
            "session" => Self::Session,
            "cross_session" | "cross-session" => Self::CrossSession,
            "current_turn" | "current-turn" | "" => Self::CurrentTurn,
            _ => Self::CurrentTurn,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::CurrentTurn => "current_turn",
            Self::Recent => "recent",
            Self::Turn => "turn",
            Self::Session => "session",
            Self::CrossSession => "cross_session",
        }
    }
}

/// Data source policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum SourcePolicy {
    #[default]
    Auto,
    LiveOnly,
    LiveFirst,
    DurableFirst,
    LocalOnly,
    CloudOnly,
}

impl SourcePolicy {
    pub fn from_arg(arg: &str) -> Self {
        match normalize_observation_arg(arg).as_str() {
            "live_only" => Self::LiveOnly,
            "live_first" => Self::LiveFirst,
            "durable_first" => Self::DurableFirst,
            "local_only" => Self::LocalOnly,
            "cloud_only" => Self::CloudOnly,
            "auto" | "" => Self::Auto,
            _ => Self::Auto,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::LiveOnly => "live_only",
            Self::LiveFirst => "live_first",
            Self::DurableFirst => "durable_first",
            Self::LocalOnly => "local_only",
            Self::CloudOnly => "cloud_only",
        }
    }

    pub fn allows_edge_local_artifacts(self) -> bool {
        matches!(
            self,
            Self::Auto | Self::LiveFirst | Self::DurableFirst | Self::LocalOnly
        )
    }
}

/// Shared observation-argument normalizer.
pub fn normalize_observation_arg(arg: &str) -> String {
    arg.trim().to_ascii_lowercase().replace('-', "_")
}

/// Sanitize a string for use in a URN path component.
pub fn urn_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len().max(1));
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// Builder for `urn:astra:{kind}:{namespace}:{component}[:{segments}...]` references.
///
/// Every segment (including `kind`, `namespace`, `component`) is sanitized via
/// [`urn_component`]; indices appended via [`Urn::idx`] are decimal integers and
/// are **not** sanitized.
///
/// # Examples
///
/// ```
/// use astra_core::Urn;
/// let urn = Urn::new("observation", "local", "introspect")
///     .seg("execution")
///     .seg("error")
///     .idx(3)
///     .build();
/// assert_eq!(urn, "urn:astra:observation:local:introspect:execution:error:3");
/// ```
#[derive(Debug, Clone)]
pub struct Urn {
    base: String,
}

impl Urn {
    /// Start a URN with `urn:astra:{kind}:{namespace}:{component}`.
    pub fn new(kind: &str, namespace: &str, component: &str) -> Self {
        Self {
            base: format!(
                "urn:astra:{}:{}:{}",
                urn_component(kind),
                urn_component(namespace),
                urn_component(component),
            ),
        }
    }

    /// Append a sanitized segment.
    pub fn seg(mut self, value: &str) -> Self {
        self.base.push(':');
        self.base.push_str(&urn_component(value));
        self
    }

    /// Append a decimal-index segment (unsanitized).
    pub fn idx(mut self, idx: usize) -> Self {
        use std::fmt::Write;
        let _ = write!(self.base, ":{idx}");
        self
    }

    /// Consume the builder and return the URN string.
    pub fn build(self) -> String {
        self.base
    }
}

const ALLOWED_EVIDENCE_KINDS: &[&str] = &[
    "event",
    "decision",
    "trace",
    "observation",
    "signal",
    "memory",
    "artifact",
    "evaluation",
    "intervention",
    "spec",
    "job",
    "failure_cluster",
    "hypothesis",
    "candidate",
    "condition",
    "reconcile",
    "measurement",
    "context",
];

const ALLOWED_EVIDENCE_NAMESPACES: &[&str] =
    &["cloud", "edge", "local", "graph", "memory", "external"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceRef<'a> {
    raw: &'a str,
    kind: &'a str,
    namespace: &'a str,
    id: &'a str,
}

impl<'a> EvidenceRef<'a> {
    pub fn parse(raw: &'a str) -> Result<Self, EvidenceRefError> {
        let mut parts = raw.splitn(5, ':');
        let Some("urn") = parts.next() else {
            return Err(EvidenceRefError::InvalidPrefix);
        };
        let Some("astra") = parts.next() else {
            return Err(EvidenceRefError::InvalidPrefix);
        };
        let kind = parts.next().ok_or(EvidenceRefError::MissingKind)?;
        let namespace = parts.next().ok_or(EvidenceRefError::MissingNamespace)?;
        let id = parts.next().ok_or(EvidenceRefError::MissingId)?;
        if !ALLOWED_EVIDENCE_KINDS.contains(&kind) {
            return Err(EvidenceRefError::UnknownKind(kind.to_string()));
        }
        if !ALLOWED_EVIDENCE_NAMESPACES.contains(&namespace) {
            return Err(EvidenceRefError::UnknownNamespace(namespace.to_string()));
        }
        if id.trim().is_empty()
            || id.chars().any(char::is_whitespace)
            || id.split(':').any(str::is_empty)
        {
            return Err(EvidenceRefError::InvalidId);
        }
        Ok(Self {
            raw,
            kind,
            namespace,
            id,
        })
    }

    pub fn as_str(&self) -> &'a str {
        self.raw
    }

    pub fn kind(&self) -> &'a str {
        self.kind
    }

    pub fn namespace(&self) -> &'a str {
        self.namespace
    }

    pub fn id(&self) -> &'a str {
        self.id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvidenceRefError {
    InvalidPrefix,
    MissingKind,
    MissingNamespace,
    MissingId,
    UnknownKind(String),
    UnknownNamespace(String),
    InvalidId,
}

impl fmt::Display for EvidenceRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPrefix => write!(f, "evidence ref must start with urn:astra"),
            Self::MissingKind => write!(f, "evidence ref is missing kind"),
            Self::MissingNamespace => write!(f, "evidence ref is missing namespace"),
            Self::MissingId => write!(f, "evidence ref is missing id"),
            Self::UnknownKind(kind) => write!(f, "unknown evidence ref kind: {kind}"),
            Self::UnknownNamespace(namespace) => {
                write!(f, "unknown evidence ref namespace: {namespace}")
            }
            Self::InvalidId => write!(f, "evidence ref id must be non-empty and whitespace-free"),
        }
    }
}

impl std::error::Error for EvidenceRefError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationView {
    pub topic: String,
    pub facet: String,
    pub depth: String,
    pub horizon: String,
    pub data_coverage: ObservationDataCoverage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationDataCoverage {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub overall: String,
    pub source: String,
    pub events: i64,
    pub decisions: i64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ObservationProviderCoverage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationProviderCoverage {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freshness_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObservationBudgetResult {
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    #[serde(default, skip_serializing_if = "ObservationBudgetOmitted::is_empty")]
    pub omitted: ObservationBudgetOmitted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ObservationBudgetOmitted {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub nodes: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub evidence_previews: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub observations: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub action_hints: i64,
}

impl ObservationBudgetOmitted {
    pub fn is_empty(&self) -> bool {
        self.nodes == 0
            && self.evidence_previews == 0
            && self.observations == 0
            && self.action_hints == 0
    }
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationRecord {
    pub ref_id: String,
    pub topic: String,
    pub facet: String,
    pub kind: String,
    pub severity: String,
    pub summary: String,
    pub confidence: ObservationConfidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationConfidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causal: Option<f64>,
}

impl ObservationConfidence {
    pub fn complete(classification: f64, evidence: f64, causal: f64) -> Self {
        Self {
            classification: Some(clamp_confidence(classification)),
            evidence: Some(clamp_confidence(evidence)),
            causal: Some(clamp_confidence(causal)),
        }
    }

    pub fn evidence(evidence: f64) -> Self {
        Self {
            classification: None,
            evidence: Some(clamp_confidence(evidence)),
            causal: None,
        }
    }

    pub fn classification_evidence(classification: f64, evidence: f64) -> Self {
        Self {
            classification: Some(clamp_confidence(classification)),
            evidence: Some(clamp_confidence(evidence)),
            causal: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationEvidence {
    pub ref_id: String,
    pub evidence_class: String,
    pub source: String,
    pub summary: String,
    pub confidence: ObservationConfidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationActionHint {
    pub target_type: String,
    pub summary: String,
    pub confidence: ObservationConfidence,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observation_refs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationFailureCluster {
    pub cluster_ref: String,
    pub label: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observation_refs: Vec<String>,
    pub evidence_class: String,
    pub confidence: ObservationConfidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGraphLayer {
    Runtime,
    Observation,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGraphNodeKind {
    Event,
    Decision,
    Outcome,
    Observation,
    FailureCluster,
    Evidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ObservationGraphEdgeKind {
    Causes,
    Supports,
    Contradicts,
    DerivedFrom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObservationGraphNode {
    pub ref_id: String,
    pub layer: ObservationGraphLayer,
    pub kind: ObservationGraphNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObservationGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: ObservationGraphEdgeKind,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ObservationGraphSlice {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<ObservationGraphNode>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges: Vec<ObservationGraphEdge>,
    #[serde(default)]
    pub budget_result: ObservationBudgetResult,
}

// ── Shared graph builders ─────────────────────────────────────────────────

/// Deduplicated node insertion by `ref_id`. Returns `true` if the node was
/// added (i.e. the `ref_id` was not already present).
pub fn push_graph_node(
    nodes: &mut Vec<ObservationGraphNode>,
    node_refs: &mut BTreeSet<String>,
    node: ObservationGraphNode,
) -> bool {
    if node_refs.insert(node.ref_id.clone()) {
        nodes.push(node);
        true
    } else {
        false
    }
}

/// Deduplicated edge insertion by `(from, to, kind)`. Returns `true` if the
/// edge was added.
pub fn push_graph_edge(
    edges: &mut Vec<ObservationGraphEdge>,
    edge_keys: &mut BTreeSet<(String, String, ObservationGraphEdgeKind)>,
    from: String,
    to: String,
    kind: ObservationGraphEdgeKind,
) -> bool {
    if edge_keys.insert((from.clone(), to.clone(), kind)) {
        edges.push(ObservationGraphEdge { from, to, kind });
        true
    } else {
        false
    }
}

/// Trim and truncate a label or summary for graph node display.
/// Returns `None` when the value is empty after trimming.
pub fn truncate_graph_summary(value: &str, max_chars: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let snippet: String = trimmed.chars().take(max_chars).collect();
    Some(snippet)
}

/// Classify an event-type label into an [`ObservationGraphNodeKind`].
/// Error-like and failure-like events map to `Outcome`; everything
/// else maps to `Event`.
pub fn classify_event_kind(event_type: &str) -> ObservationGraphNodeKind {
    if matches!(
        event_type,
        "tool_result" | "tool_error" | "error" | "stall_detected"
    ) || event_type.contains("error")
        || event_type.contains("fail")
    {
        ObservationGraphNodeKind::Outcome
    } else {
        ObservationGraphNodeKind::Event
    }
}

fn clamp_confidence(value: f64) -> f64 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

// ── TuningJob ────────────────────────────────────────────────────────────────

/// A tuning signal emitted when the Observation Plane detects adaptation
/// triggers. TuningJobs are fire-and-forget — they are written to a sink
/// (file, cloud queue) without blocking the agentic loop.
///
/// # When TuningJobs are generated
///
/// | Trigger | Generated Signal |
/// |---------|-----------------|
/// | Token pressure > 0.8 | `PromptCompaction` |
/// | Token pressure > 0.95 | `AggressiveCompaction` |
/// | Error rate > 0.3 | `CircuitBreakerTuning` |
/// | Frequent compaction (≥3 in recent window) | `CompactionPolicyTuning` |
/// | Cache hit ratio < 0.3 after 10+ turns | `CacheWarming` |
/// | Task completion stalled (ratio < 1.0 for 5+ turns) | `TaskDecomposition` |

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningJob {
    /// Type of tuning signal.
    pub signal: TuningSignalType,
    /// The current value that triggered this signal.
    pub trigger_value: f64,
    /// Brief diagnostic summary (max 200 chars).
    pub reason: String,
    /// Unix timestamp in milliseconds.
    pub created_at_ms: u64,
    /// Turn index when the signal was generated.
    pub turn_index: u32,
    /// Session id for traceability.
    pub session_id: String,
    /// Priority: 0 (advisory) to 10 (critical).
    pub priority: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TuningSignalType {
    /// Token pressure crossed normal threshold → suggest compaction.
    PromptCompaction,
    /// Token pressure critical → urgent aggressive compaction.
    AggressiveCompaction,
    /// Error rate too high → tighten circuit breaker thresholds.
    CircuitBreakerTuning,
    /// Compaction triggered too frequently → adjust compaction policy.
    CompactionPolicyTuning,
    /// Cache hit ratio too low → warm cache or adjust prefetch.
    CacheWarming,
    /// Task board stalled for many turns → suggest decomposition.
    TaskDecomposition,
}

impl std::fmt::Display for TuningSignalType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PromptCompaction => write!(f, "prompt_compaction"),
            Self::AggressiveCompaction => write!(f, "aggressive_compaction"),
            Self::CircuitBreakerTuning => write!(f, "circuit_breaker_tuning"),
            Self::CompactionPolicyTuning => write!(f, "compaction_policy_tuning"),
            Self::CacheWarming => write!(f, "cache_warming"),
            Self::TaskDecomposition => write!(f, "task_decomposition"),
        }
    }
}

// ── Tuning Consumer Types ──────────────────────────────────────────────────

/// Aggregated statistics for a single [`TuningSignalType`] across sessions.
///
/// Produced by [`TuningConsumer::aggregate`]; consumed to generate
/// [`OptimizationSuggestion`] entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningAggregation {
    /// The signal type being aggregated.
    pub signal_type: TuningSignalType,
    /// Total count of this signal across all sessions.
    pub total_count: u64,
    /// Number of distinct sessions where this signal appeared.
    pub session_count: u32,
    /// Average priority (0-10) across occurrences.
    pub avg_priority: f64,
    /// Average trigger value across occurrences.
    pub avg_trigger_value: f64,
    /// Most recent timestamp (unix ms) when this signal was emitted.
    pub latest_at_ms: u64,
    /// Sample reasons (up to 3, for diagnostic display).
    pub sample_reasons: Vec<String>,
}

/// A concrete optimization recommendation produced by [`TuningConsumer`].
///
/// Suggestions are advisory and human-readable; they describe *what* to change
/// and *why*, but do not apply changes automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptimizationSuggestion {
    /// Human-readable title (e.g. "Increase compaction pressure threshold").
    pub title: String,
    /// The signal type that triggered this suggestion.
    pub source_signal: TuningSignalType,
    /// What parameter or behavior to change.
    pub target: String,
    /// Recommended new value (as a human-readable string).
    pub recommended_value: String,
    /// Current observed value (if measurable).
    pub current_value: Option<String>,
    /// Evidence summary — why this change is recommended.
    pub reason: String,
    /// Confidence in this recommendation (0.0–1.0).
    pub confidence: f64,
    /// Priority: 0 (advisory) to 10 (critical).
    pub priority: u8,
    /// Number of tuning signals backing this suggestion.
    pub signal_count: u64,
}

/// Overall summary of tuning data across all sessions.
///
/// The top-level output of [`TuningConsumer::summarize`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningSummary {
    /// Number of sessions scanned.
    pub sessions_scanned: u32,
    /// Total tuning job entries found.
    pub total_jobs: u64,
    /// Per-signal-type aggregations.
    pub aggregations: Vec<TuningAggregation>,
    /// Generated optimization suggestions.
    pub suggestions: Vec<OptimizationSuggestion>,
    /// Human-readable summary text.
    pub summary_text: String,
    /// Unix timestamp when this summary was produced.
    pub generated_at_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_facet_parses_advertised_edge_local_facets() {
        assert_eq!(
            "cache".parse::<ObservationFacet>().unwrap(),
            ObservationFacet::Cache
        );
        assert_eq!(
            "session-memory".parse::<ObservationFacet>().unwrap(),
            ObservationFacet::SessionMemory
        );
        assert_eq!(ObservationFacet::Cache.as_str(), "cache");
        assert_eq!(ObservationFacet::SessionMemory.as_str(), "session_memory");
    }

    #[test]
    fn observation_record_wire_shape_roundtrips() {
        let record = ObservationRecord {
            ref_id: "urn:astra:observation:graph:test:1".into(),
            topic: "execution".into(),
            facet: "errors".into(),
            kind: "diagnosis:tool_timeout".into(),
            severity: "warning".into(),
            summary: "tool timed out".into(),
            confidence: ObservationConfidence::complete(0.9, 0.8, 0.7),
            evidence_refs: vec!["urn:astra:event:local:test:1".into()],
        };

        let json = serde_json::to_string(&record).expect("serialize observation");
        let parsed: ObservationRecord =
            serde_json::from_str(&json).expect("deserialize observation");
        assert_eq!(record, parsed);
    }

    #[test]
    fn confidence_wire_shape_names_available_dimensions() {
        let evidence = ObservationEvidence {
            ref_id: "urn:astra:event:local:test:1".into(),
            evidence_class: "observed_evidence".into(),
            source: "test".into(),
            summary: "test evidence".into(),
            confidence: ObservationConfidence::evidence(1.5),
        };
        let json = serde_json::to_value(&evidence).expect("serialize evidence");
        assert_eq!(json["confidence"]["evidence"], 1.0);
        assert!(json["confidence"].get("classification").is_none());
        assert!(json["confidence"].get("causal").is_none());

        let hint = ObservationActionHint {
            target_type: "user_guidance".into(),
            summary: "inspect the failure".into(),
            confidence: ObservationConfidence::classification_evidence(0.8, f64::NAN),
            observation_refs: vec![],
        };
        let json = serde_json::to_value(&hint).expect("serialize action hint");
        assert_eq!(json["confidence"]["classification"], 0.8);
        assert_eq!(json["confidence"]["evidence"], 0.0);
        assert!(json["confidence"].get("causal").is_none());
    }

    #[test]
    fn read_only_git_tools_do_not_count_as_workspace_mutations() {
        let samples = [
            ToolCallSample {
                name: "git_diff",
                ok: true,
                round: Some(1),
                file_path: None,
                error: None,
            },
            ToolCallSample {
                name: "git_log",
                ok: true,
                round: Some(1),
                file_path: None,
                error: None,
            },
            ToolCallSample {
                name: "git_blame",
                ok: true,
                round: Some(1),
                file_path: None,
                error: None,
            },
        ];
        let metrics = TurnMetrics::from_samples(&samples, 1, 100);
        assert_eq!(metrics.tool_calls_by_family.get(&ToolFamily::Git), Some(&3));
        assert_eq!(
            metrics.mutation_count, 0,
            "read-only git inspection must not fabricate workspace progress"
        );
    }

    #[test]
    fn mutating_git_tools_count_as_workspace_mutations() {
        let samples = [
            ToolCallSample {
                name: "git_commit",
                ok: true,
                round: Some(2),
                file_path: None,
                error: None,
            },
            ToolCallSample {
                name: "git_push",
                ok: true,
                round: Some(2),
                file_path: None,
                error: None,
            },
        ];
        let metrics = TurnMetrics::from_samples(&samples, 2, 100);
        assert_eq!(metrics.mutation_count, 2);
        assert_eq!(metrics.rounds_since_last_mutation, 0);
    }

    #[test]
    fn error_streak_preview_truncates_on_utf8_char_boundary() {
        let error = format!("{}⚠ task returned an error", "x".repeat(199));
        let samples = [ToolCallSample {
            name: "task.create",
            ok: false,
            round: Some(1),
            file_path: None,
            error: Some(&error),
        }];

        let metrics = TurnMetrics::from_samples(&samples, 1, 100);

        assert_eq!(metrics.error_streaks.len(), 1);
        let first_error = &metrics.error_streaks[0].first_error;
        assert_eq!(first_error.chars().count(), 200);
        assert!(first_error.ends_with('⚠'));
    }

    #[test]
    fn failure_cluster_wire_shape_is_ref_based() {
        let cluster = ObservationFailureCluster {
            cluster_ref: "urn:astra:failure_cluster:graph:reflect:test:tool_timeout".into(),
            label: "tool_timeout".into(),
            summary: "bash timed out repeatedly".into(),
            observation_refs: vec!["urn:astra:observation:graph:reflect:test:diagnosis:0".into()],
            evidence_class: "inferred_evidence".into(),
            confidence: ObservationConfidence::classification_evidence(0.82, 0.76),
        };

        EvidenceRef::parse(&cluster.cluster_ref).expect("cluster ref must be canonical");
        let cluster_json = serde_json::to_value(&cluster).expect("serialize cluster");
        assert_eq!(cluster_json["confidence"]["classification"], 0.82);
        assert!(cluster_json["confidence"].get("causal").is_none());
    }

    #[test]
    fn graph_slice_wire_shape_keeps_layers_and_edges_explicit() {
        let slice = ObservationGraphSlice {
            nodes: vec![
                ObservationGraphNode {
                    ref_id: "urn:astra:event:cloud:evt-1".into(),
                    layer: ObservationGraphLayer::Runtime,
                    kind: ObservationGraphNodeKind::Event,
                    label: "tool_error".into(),
                    summary: Some("bash timed out".into()),
                    metadata: None,
                },
                ObservationGraphNode {
                    ref_id: "urn:astra:observation:graph:obs-1".into(),
                    layer: ObservationGraphLayer::Observation,
                    kind: ObservationGraphNodeKind::Observation,
                    label: "diagnosis:tool_timeout".into(),
                    summary: None,
                    metadata: None,
                },
            ],
            edges: vec![ObservationGraphEdge {
                from: "urn:astra:observation:graph:obs-1".into(),
                to: "urn:astra:event:cloud:evt-1".into(),
                kind: ObservationGraphEdgeKind::DerivedFrom,
            }],
            budget_result: ObservationBudgetResult::default(),
        };

        for node in &slice.nodes {
            EvidenceRef::parse(&node.ref_id).expect("graph node refs must be canonical");
        }
        let json = serde_json::to_value(&slice).expect("serialize graph slice");
        assert_eq!(json["nodes"][0]["layer"], "runtime");
        assert_eq!(json["nodes"][1]["kind"], "observation");
        assert_eq!(json["edges"][0]["kind"], "derived_from");
    }

    #[test]
    fn evidence_ref_parser_accepts_canonical_refs() {
        let parsed = EvidenceRef::parse("urn:astra:event:cloud:event_01H00000000000000000000001")
            .expect("valid evidence ref");
        assert_eq!(parsed.kind(), "event");
        assert_eq!(parsed.namespace(), "cloud");
        assert_eq!(parsed.id(), "event_01H00000000000000000000001");
        assert_eq!(
            parsed.as_str(),
            "urn:astra:event:cloud:event_01H00000000000000000000001"
        );
    }

    #[test]
    fn evidence_ref_parser_accepts_hierarchical_ids() {
        let edge_event = EvidenceRef::parse("urn:astra:event:edge:session_abc:seq_42")
            .expect("edge event evidence ref should allow hierarchical ids");
        assert_eq!(edge_event.id(), "session_abc:seq_42");

        let reflect_observation =
            EvidenceRef::parse("urn:astra:observation:graph:reflect:test-sess:diagnosis:0")
                .expect("reflect observation ref should allow generated hierarchical ids");
        assert_eq!(reflect_observation.kind(), "observation");
        assert_eq!(reflect_observation.namespace(), "graph");
        assert_eq!(reflect_observation.id(), "reflect:test-sess:diagnosis:0");
    }

    #[test]
    fn evidence_ref_parser_rejects_private_namespaces() {
        let err = EvidenceRef::parse("urn:astra:event:server:evt-1").unwrap_err();
        assert_eq!(err, EvidenceRefError::UnknownNamespace("server".into()));
    }

    #[test]
    fn evidence_ref_parser_rejects_malformed_refs() {
        assert_eq!(
            EvidenceRef::parse("event:cloud:evt-1").unwrap_err(),
            EvidenceRefError::InvalidPrefix
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra").unwrap_err(),
            EvidenceRefError::MissingKind
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event").unwrap_err(),
            EvidenceRefError::MissingNamespace
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud").unwrap_err(),
            EvidenceRefError::MissingId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:unknown:cloud:evt-1").unwrap_err(),
            EvidenceRefError::UnknownKind("unknown".into())
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:event 1").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud::evt").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:evt:").unwrap_err(),
            EvidenceRefError::InvalidId
        );
        assert_eq!(
            EvidenceRef::parse("urn:astra:event:cloud:evt::seq").unwrap_err(),
            EvidenceRefError::InvalidId
        );
    }

    #[test]
    fn observation_view_can_report_provider_coverage_and_budget() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "live_runtime".to_string(),
            ObservationProviderCoverage {
                status: "fresh".to_string(),
                freshness_ms: Some(0),
                reason: None,
            },
        );
        let coverage = ObservationDataCoverage {
            overall: "partial".to_string(),
            source: "live_runtime_snapshot".to_string(),
            events: 2,
            decisions: 0,
            providers,
            warnings: vec!["memory_backend_unavailable".to_string()],
        };
        let budget = ObservationBudgetResult {
            truncated: true,
            next_cursor: None,
            omitted: ObservationBudgetOmitted {
                nodes: 12,
                evidence_previews: 12,
                ..Default::default()
            },
        };

        let coverage_json = serde_json::to_value(&coverage).expect("serialize coverage");
        assert_eq!(
            coverage_json["providers"]["live_runtime"]["status"],
            "fresh"
        );
        let budget_json = serde_json::to_value(&budget).expect("serialize budget");
        assert_eq!(budget_json["truncated"], true);
        assert_eq!(budget_json["omitted"]["nodes"], 12);
    }

    // ── graph builder tests ──

    #[test]
    fn push_graph_node_inserts_unique_ref_ids() {
        let mut nodes = Vec::new();
        let mut node_refs = BTreeSet::new();

        let added = push_graph_node(
            &mut nodes,
            &mut node_refs,
            ObservationGraphNode {
                ref_id: "urn:astra:event:cloud:evt-1".into(),
                layer: ObservationGraphLayer::Runtime,
                kind: ObservationGraphNodeKind::Event,
                label: "test".into(),
                summary: None,
                metadata: None,
            },
        );
        assert!(added);
        assert_eq!(nodes.len(), 1);
        assert_eq!(node_refs.len(), 1);
    }

    #[test]
    fn push_graph_node_dedup_rejects_duplicate_ref_ids() {
        let mut nodes = Vec::new();
        let mut node_refs = BTreeSet::new();

        let node = ObservationGraphNode {
            ref_id: "urn:astra:event:cloud:evt-1".into(),
            layer: ObservationGraphLayer::Runtime,
            kind: ObservationGraphNodeKind::Event,
            label: "test".into(),
            summary: None,
            metadata: None,
        };
        assert!(push_graph_node(&mut nodes, &mut node_refs, node.clone()));
        assert!(!push_graph_node(&mut nodes, &mut node_refs, node.clone()));
        assert_eq!(nodes.len(), 1);
    }

    #[test]
    fn push_graph_edge_inserts_unique_triples() {
        let mut edges = Vec::new();
        let mut edge_keys = BTreeSet::new();

        let added = push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Causes,
        );
        assert!(added);
        assert_eq!(edges.len(), 1);
    }

    #[test]
    fn push_graph_edge_dedup_rejects_duplicate_triples() {
        let mut edges = Vec::new();
        let mut edge_keys = BTreeSet::new();

        assert!(push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Causes,
        ));
        assert!(!push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Causes,
        ));
        // Different kind = different triple → allowed
        assert!(push_graph_edge(
            &mut edges,
            &mut edge_keys,
            "from".into(),
            "to".into(),
            ObservationGraphEdgeKind::Supports,
        ));
        assert_eq!(edges.len(), 2);
    }

    #[test]
    fn truncate_graph_summary_trims_and_truncates() {
        assert_eq!(
            truncate_graph_summary("  hello world  ", 5),
            Some("hello".into())
        );
        assert_eq!(truncate_graph_summary("   ", 10), None);
        assert_eq!(truncate_graph_summary("", 10), None);
        assert_eq!(truncate_graph_summary("hi", 100), Some("hi".into()));
    }

    #[test]
    fn classify_event_kind_maps_errors_to_outcome() {
        assert_eq!(
            classify_event_kind("tool_error"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("tool_result"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("stall_detected"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("error"),
            ObservationGraphNodeKind::Outcome
        );
        assert_eq!(
            classify_event_kind("bash_failed"),
            ObservationGraphNodeKind::Outcome
        );
    }

    #[test]
    fn classify_event_kind_maps_normal_events_to_event() {
        assert_eq!(
            classify_event_kind("tool_call"),
            ObservationGraphNodeKind::Event
        );
        assert_eq!(
            classify_event_kind("user_message"),
            ObservationGraphNodeKind::Event
        );
        assert_eq!(
            classify_event_kind("skill_started"),
            ObservationGraphNodeKind::Event
        );
    }
}
