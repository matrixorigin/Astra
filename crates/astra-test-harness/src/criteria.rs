//! Criterion evaluation.
//!
//! Criteria stack from cheap-to-expensive. Deterministic matchers
//! (tool_called, stderr_contains, exit_code, tools_count) run
//! against the captured `RunOutcome` locally — no provider calls.
//! The `Judger` variant calls into an LLM to score free-form
//! criteria like "did the agent understand the task?".
//!
//! Rationale: when a deterministic criterion fails, the case is
//! already known FAIL and the judger call would waste a provider
//! round-trip. The runner short-circuits accordingly (see
//! `evaluate_all`).

use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::case::PromptCacheReuseScope;
use crate::pipeline_analysis::analyze_pipeline_health;
use crate::runner::RunOutcome;
use crate::session_capture::SessionCapture;

/// One declarative success check. Serialized into YAML cases as
/// `type: <variant>` discriminator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Criterion {
    /// Passes if the tool with `name` appears in the run's
    /// `tools_used` list. Cheapest possible check.
    ToolCalled { name: String },

    /// Passes when the run's exit code equals `code`. Useful for
    /// pinning expected failures.
    ExitCode { code: i32 },

    /// Passes when `astra chat --json` reports the expected terminal state.
    FinalState { expect: String },

    /// Passes when the structured interruption kind matches.
    InterruptionKind { expect: String },

    /// Passes when a tool result class occurred within the expected count range.
    ToolResultClassCount { class: String, min: u32, max: u32 },

    /// Passes when the total tool_calls_count is within the range
    /// `min..=max`. Catches runaway loops or under-tool-use.
    ToolsCountBetween { min: u32, max: u32 },

    /// Regex match against the run's stderr. Intended for
    /// observability checks — `^\[fork-cache\]` / `^\[audit\]`.
    /// The regex is compiled per-evaluation; test stays
    /// robust across Rust regex version bumps.
    StderrMatches { pattern: String },

    /// Passes when the final assistant text contains `needle`
    /// (case-sensitive substring match). For simple yes/no
    /// checks without pulling in a judger.
    TextContains { needle: String },

    /// Passes when the final assistant text does not contain `needle`.
    /// Useful for deterministic stale-topic and provenance-contamination gates.
    TextNotContains { needle: String },

    /// Passes when the assistant text, after trimming outer whitespace, is
    /// exactly `expected`.
    TextEquals { expected: String },

    /// Parses the complete assistant text as one JSON value and requires the
    /// value at the RFC 6901 JSON pointer to equal `equals`. Markdown fences
    /// and surrounding prose are rejected: this is a wire-contract oracle,
    /// not a best-effort text extractor.
    TextJsonValue {
        path: String,
        equals: serde_json::Value,
    },

    /// Parses the complete assistant text as JSON and bounds the length of an
    /// array selected by an RFC 6901 JSON pointer.
    TextJsonArrayCount { path: String, min: u32, max: u32 },

    /// Requires that an RFC 6901 JSON pointer is absent from the complete
    /// assistant JSON value. A present `null` still counts as present.
    TextJsonPathAbsent { path: String },

    /// Validates a generic directed acyclic graph encoded in the complete
    /// assistant JSON value. Node and edge field pointers are relative to
    /// each element selected by `nodes_path` and `edges_path`.
    TextJsonDag {
        nodes_path: String,
        node_id_path: String,
        /// Additional non-empty string fields required on every node. Paths
        /// are relative RFC 6901 pointers, just like `node_id_path`.
        #[serde(default)]
        node_required_string_paths: Vec<String>,
        edges_path: String,
        predecessor_path: String,
        successor_path: String,
    },

    /// Passes when the session journal contains at least `min`
    /// events with `type == event_type`. Requires session capture.
    /// Use for structural checks ("at least one subagent_spawned
    /// event appears").
    ///
    /// FAILS when no session is loaded, UNLESS `optional: true` —
    /// in which case the criterion skip-passes with a note. Default
    /// strict semantics mean: if a case author wrote this criterion
    /// and the session isn't available, something is wrong (bad
    /// session_id, journal not flushed, loader misconfigured) and
    /// the case should surface that as a failure instead of silently
    /// passing.
    SessionEventCount {
        event_type: String,
        #[serde(default = "default_event_min")]
        min: u32,
        /// When true, skip-pass when session is unavailable. Use
        /// sparingly — only for cases that are meaningful even
        /// without the journal check.
        #[serde(default)]
        optional: bool,
    },

    /// Counts typed signal codes in durable `turn_evaluation` events.
    ///
    /// This matches the stable `metadata.signals[].kind` code, never the
    /// rendered diagnostic message. Pair an absence assertion with
    /// `SessionEventCount { event_type: "turn_evaluation", .. }` when a
    /// missing evaluation event must not be mistaken for a healthy turn.
    JournalTurnEvaluationSignalCount { kind: String, min: u32, max: u32 },

    /// Requires the terminal durable turn evaluation to report the requested
    /// product-success verdict. This prevents a harness from certifying a
    /// structurally shaped run whose own runtime evaluator detected unresolved
    /// failures or incomplete work.
    JournalTurnEvaluationSuccess { equals: bool },

    /// Requires complete durable session evidence and rejects asynchronous
    /// subsystem failures/degradation recorded during the case.
    ///
    /// This checks typed event fields only. Human-facing log/error text is
    /// deliberately irrelevant so changing copy cannot change product truth.
    SessionSubsystemHealthy {
        /// When set, require a matching `subsystem_settled` event before
        /// accepting the captured evidence as complete.
        #[serde(default)]
        settled_subsystem: Option<String>,
    },

    /// Passes when the given tool name appears in the journal's
    /// `tool_invocation` events. Journal is the source of truth
    /// for tool calls — `tools_used` from the CLI envelope may
    /// miss tools emitted inside sub-agent runs.
    ///
    /// FAILS when no session is loaded unless `optional: true`. See
    /// `SessionEventCount` for the rationale.
    JournalToolCalled {
        name: String,
        #[serde(default)]
        optional: bool,
    },

    /// Counts successful typed child tool completions causally descended from
    /// a parent dispatch (for example, `read_file` from `agent_fanout`).
    /// Same-run coordinator calls do not satisfy this criterion.
    JournalChildToolCallCount {
        parent: String,
        child: String,
        min: u32,
        max: u32,
    },

    /// Requires that a tool is absent from every canonical user-facing turn
    /// surface. This proves catalog authority at the product boundary; it
    /// intentionally does not inspect child tool surfaces, where an
    /// attempt-bound tool may be valid.
    JournalTurnToolHidden { name: String },

    /// Exact number of complete tool-call records in durable turn events.
    /// The optional document/path/equality triplet narrows the count by one
    /// structural JSON predicate (for example, only `action=start` calls).
    JournalToolCallCount {
        name: String,
        min: u32,
        max: u32,
        #[serde(default)]
        document: Option<JournalToolDocument>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        equals: Option<serde_json::Value>,
    },

    /// Exact number of complete durable tool-call records with an explicit
    /// typed success or failure outcome. This deliberately uses the journal's
    /// boolean `ok` field rather than inferring outcome from rendered text.
    JournalToolOutcomeCount {
        name: String,
        ok: bool,
        min: u32,
        max: u32,
    },

    /// Requires a minimum success ratio across durable tool outcomes. Calls
    /// without a terminal boolean outcome are excluded from both buckets;
    /// `min_calls` prevents an empty or trivial trace from looking healthy.
    JournalToolSuccessRatio {
        min: f64,
        #[serde(default = "default_event_min")]
        min_calls: u32,
        /// Explicit task-required negative probes (for example one failing
        /// baseline) excluded from the adjusted ratio. Raw health is always
        /// reported as well, so the allowance cannot hide trace evidence.
        #[serde(default)]
        allowed_failures: u32,
    },

    /// Exact JSON-pointer assertion against a complete durable tool call.
    JournalToolJson {
        name: String,
        document: JournalToolDocument,
        path: String,
        equals: serde_json::Value,
    },

    /// Requires a durable JSON-pointer value to be a string containing the
    /// supplied marker. This is useful for lossless provider-result checks
    /// where punctuation/formatting is provider data, but the semantic marker
    /// must still come from the child/tool result rather than final prose.
    JournalToolJsonContains {
        name: String,
        document: JournalToolDocument,
        path: String,
        contains: String,
    },

    /// Proves that a consumer tool used the exact logical artifact handle
    /// advertised by an earlier producer result. This is a structural
    /// provenance assertion over complete durable records; final assistant
    /// text and physical filesystem paths are not evidence.
    JournalArtifactConsumed { producer: String, consumer: String },

    /// Proves that a successful consumer used an exact scalar value emitted
    /// by a prior successful producer. JSON pointers select the producer value
    /// and the allowed consumer destinations; one destination must
    /// structurally contain the exact scalar. This is a generic durable
    /// value-flow contract, not a tool-specific or text-based assertion.
    JournalToolValueFlow {
        producer: String,
        producer_document: JournalToolDocument,
        producer_path: String,
        #[serde(default)]
        producer_filter: Option<JournalJsonPredicate>,
        consumer: String,
        consumer_document: JournalToolDocument,
        consumer_paths: Vec<String>,
        #[serde(default)]
        consumer_filter: Option<JournalJsonPredicate>,
    },

    /// Value-flow variant with conjunctive typed predicates on the exact
    /// successful producer and consumer calls. This is the relational form
    /// for lifecycle contracts whose identity and scope must travel with the
    /// same call (for example, remember(memory_type=working) ->
    /// recall(scope=session)).
    JournalToolValueFlowBound {
        producer: String,
        producer_document: JournalToolDocument,
        producer_path: String,
        producer_filters: Vec<JournalJsonPredicate>,
        consumer: String,
        consumer_document: JournalToolDocument,
        consumer_paths: Vec<String>,
        consumer_filters: Vec<JournalJsonPredicate>,
    },

    /// Passes when at least one `[fork-cache]` JSON event in stderr
    /// has its `outcome` field in `expect`. Pins the exact runtime
    /// contract (see `ForkCacheEvent` in astra-turn-core) — outcomes
    /// are one of `hit`, `partial_drift`, `miss`, `exceeded_expected`.
    ///
    /// Example:
    /// ```yaml
    /// - type: fork_cache_outcome
    ///   expect: [hit]
    /// ```
    ///
    /// Accepted field aliases: `outcome` (current wire name) and
    /// `class` (earlier harness-facing name; deprecated, still read so
    /// existing YAML doesn't silently break).
    ForkCacheOutcome {
        /// Accepted outcome names (snake_case — `hit`, `partial_drift`,
        /// `miss`, `exceeded_expected`, plus any future variant added
        /// to `astra_turn_core::fork_cache_event::ForkCacheOutcome`).
        /// A stderr event whose `outcome` equals any of these passes.
        #[serde(default)]
        expect: Vec<String>,
    },

    /// LLM judger — calls a scoring model with the prompt +
    /// context and expects a number in [0.0, 1.0]. Passes when
    /// score >= `threshold`. Most expensive; put last.
    Judger {
        /// Natural-language question the judger answers. Should be
        /// specific: "Did the agent correctly spawn a sub-agent
        /// and return the agent_id?" — not "Is the output good?"
        question: String,
        /// Score threshold in [0.0, 1.0]. Default 0.7 — tests
        /// that allow mild model drift while rejecting
        /// obvious failures.
        #[serde(default = "default_judger_threshold")]
        threshold: f64,
        /// Optional model override for the judger. Defaults to
        /// whatever the harness's `--judger-model` CLI flag says.
        #[serde(default)]
        model: Option<String>,
    },

    /// LLM judger whose result is a required product assertion rather than
    /// advisory quality feedback. Use this when a remote side effect has no
    /// deterministic receipt in the chat envelope (for example a memory
    /// purge) and a failed judgement must fail the case.
    HardJudger {
        question: String,
        #[serde(default = "default_judger_threshold")]
        threshold: f64,
        #[serde(default)]
        model: Option<String>,
    },

    /// Passes when total tokens (prompt + completion) is within range.
    /// Catches token efficiency regressions — a case that used to cost
    /// 500 tokens suddenly costing 5000 means something broke.
    TokensBetween { min: u64, max: u64 },

    /// Passes when wall-clock duration (ms) is within range.
    /// Catches latency regressions and hung subprocesses that
    /// complete just under the timeout.
    DurationBetween { min_ms: u64, max_ms: u64 },

    /// Passes when the tools_used list contains the given names
    /// as an ordered subsequence. Does NOT require exact match —
    /// extra tools between the expected ones are allowed.
    /// Example: `[read_file, str_replace]` passes for
    /// `[bash, read_file, bash, str_replace, bash]`.
    ToolSequence { tools: Vec<String> },

    /// Requires an ordered subsequence in complete durable journal records.
    /// Unlike [`ToolSequence`], this includes server-side and child calls that
    /// the CLI envelope can omit, so it proves lifecycle ordering rather than
    /// merely the client-visible summary.
    JournalToolSequence { tools: Vec<String> },

    /// Requires every durable invocation of `successor` to occur only after
    /// `predecessor` has appeared in the same session journal. This is a
    /// lifecycle invariant, not a textual final-answer assertion.
    JournalToolPrecedence {
        predecessor: String,
        successor: String,
    },

    /// Proves completed canonical Work executions carry server-selected
    /// `(item_id, item_revision)` identities issued by an earlier successful
    /// `start_work`. This checks durable ownership and sequential task
    /// progress, not model prose or fixture-specific task names.
    JournalWorkItemExecutionFromStart {
        #[serde(default = "default_min_distinct_work_items")]
        min_distinct_items: usize,
    },

    /// Proves a successful canonical graph patch was committed after Work was
    /// established. The requested mutation dimensions are read from typed tool
    /// arguments and the accepted receipt, never from model prose or item
    /// names. This covers user steering and evidence-driven replanning with
    /// the same durable graph contract.
    JournalWorkGraphPatch {
        #[serde(default)]
        require_addition: bool,
        #[serde(default)]
        require_active_revision: bool,
        #[serde(default)]
        require_retired_revision: bool,
        /// Require the exact cancellation state. A superseded revision does
        /// not satisfy this because cancel and replace are distinct user
        /// operations.
        #[serde(default)]
        require_cancelled_revision: bool,
        /// Require the exact replacement retirement state.
        #[serde(default)]
        require_superseded_revision: bool,
        #[serde(default)]
        require_dependency_change: bool,
        /// Require an addition and a retired revision in the same accepted
        /// proposal. When an exact cancellation or supersession state is also
        /// required, that state must occur in the same proposal. This proves
        /// atomic graph structure; it deliberately makes no claim that two
        /// model-authored descriptions are semantically equivalent.
        #[serde(default)]
        require_atomic_retire_and_add: bool,
    },

    /// Passes when the number of LLM round-trips (turns) is within range.
    /// Catches inefficient multi-turn loops where the agent should have
    /// completed in fewer rounds.
    TurnRoundsBetween { min: u32, max: u32 },

    /// Passes when the tool cache hit rate >= threshold (0.0 to 1.0).
    /// A high cache rate means the agent is efficiently reusing
    /// idempotent tool results. Requires at least one tool call.
    CacheRateAbove {
        /// Minimum cache hit rate (0.0 = no caching required, 1.0 = all cached).
        threshold: f64,
        /// Minimum number of tool calls required for the criterion to
        /// apply. When the agent makes fewer calls than this, the
        /// criterion FAILs instead of skip-passing. Default 1 — set
        /// higher if the case expects a specific tool-call volume.
        #[serde(default = "default_cache_min_calls")]
        min_calls: u32,
    },

    /// Passes when provider prompt-cache accounting reports the cache-read
    /// and cache-creation buckets within the expected bounds.
    ///
    /// - `min_read` — floor on cumulative `cached_input_tokens`. Fails when
    ///   the prefix doesn't hit enough (cache prefix broken).
    /// - `min_creation` — floor on `cache_creation_tokens`. Rarely used; set
    ///   to 0 for backward compatibility.
    /// - `max_creation` — ceiling on `cache_creation_tokens`. When set, fails
    ///   if cache is being rebuilt excessively (partial-hit regressions where
    ///   reads look healthy but creations explode).
    ///
    /// Distinct from `cache_rate_above`, which checks the local idempotent
    /// tool-result cache.
    PromptCacheTokens {
        min_read: u64,
        min_creation: u64,
        #[serde(default)]
        max_creation: Option<u64>,
    },

    /// Passes when the token-weighted provider prompt-cache read ratio,
    /// after discarding a configurable number of cold-start observations,
    /// is at least `min`.
    ///
    /// The denominator is all provider input:
    /// `fresh + cache_read + cache_creation`. This prevents small requests
    /// from dominating an average and prevents cache-write churn from being
    /// mistaken for a healthy read hit rate. The criterion reads canonical
    /// `turn` journal usage and falls back to legacy `llm_round` records.
    /// `warmup_rounds` is an explicit provider-boundary mode for journeys
    /// whose single user turn contains several model/tool rounds; it uses
    /// detailed `llm_round` records and leaves `warmup_turns` unchanged.
    ProviderPromptCacheReadRatio {
        min: f64,
        #[serde(default = "default_prompt_cache_warmup_turns")]
        warmup_turns: u32,
        #[serde(default)]
        warmup_rounds: u32,
    },

    /// Passes when provider cache reuse is healthy within stable Context
    /// Pipeline prefix epochs. The first request for each typed prefix
    /// identity is a cold boundary and is not scored; each later request with
    /// the same identity must read at least `min` of the preceding request's
    /// total input from the provider cache.
    ProviderPromptCacheStablePrefixReuseRatio {
        min: f64,
        #[serde(default = "default_stable_prefix_min_pairs")]
        min_pairs: u32,
        max_identity_transitions_per_run: u32,
    },

    /// Internal hard gate injected when a case declares
    /// `required_cache_scope`. It proves the requested reuse boundary from
    /// durable provider usage rather than trusting model metadata or a soft
    /// cache-quality criterion.
    PromptCacheReuseScope { scope: PromptCacheReuseScope },

    /// Passes when the session's pipeline alerts matching `rule`
    /// occur at most `max` times.
    PipelineAlertCount {
        rule: String,
        max: u32,
        #[serde(default)]
        optional: bool,
    },

    /// Passes when the average per-turn prompt cache hit ratio reported
    /// by pipeline feedback is at least `min`.
    PipelineAvgCacheHitRatio {
        min: f64,
        #[serde(default)]
        optional: bool,
    },

    /// Passes when any nested deterministic criterion passes.
    ///
    /// Use for cases with multiple acceptable high-quality behaviors, such
    /// as "called the requested tool" OR "safely refused a runaway prompt".
    AnyOf { criteria: Vec<Criterion> },

    /// Passes when every nested deterministic criterion passes.
    ///
    /// Useful for making a set of normally-soft metric bounds a hard case
    /// requirement without changing their default severity globally.
    AllOf { criteria: Vec<Criterion> },
}

fn default_cache_min_calls() -> u32 {
    1
}

fn default_judger_threshold() -> f64 {
    0.7
}

fn default_event_min() -> u32 {
    1
}

fn default_prompt_cache_warmup_turns() -> u32 {
    1
}

fn default_stable_prefix_min_pairs() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalToolDocument {
    Arguments,
    Result,
}

/// One structural predicate applied to the same durable tool call that
/// participates in a value-flow relation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalJsonPredicate {
    pub document: JournalToolDocument,
    pub path: String,
    pub equals: serde_json::Value,
}

/// How severe a criterion failure is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionSeverity {
    /// Hard requirement: exit_code, tool_called, text_contains.
    /// Failure means the case fundamentally didn't work.
    Hard,
    /// Soft bound: tokens_between, duration_between, turn_rounds, cache_rate.
    /// Failure means the case worked but outside acceptable efficiency bounds.
    Soft,
    /// Quality score: judger, session checks.
    /// Uses a 0-1 continuous score rather than binary pass/fail.
    Quality,
}

/// Result of evaluating a single criterion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CriterionResult {
    pub criterion: Criterion,
    pub passed: bool,
    /// Severity level — tells the frontend how to treat this result.
    #[serde(default = "default_severity")]
    pub severity: CriterionSeverity,
    /// Short human explanation (≤ 200 chars). Surfaces in report
    /// on FAIL; suppressed on PASS unless `--verbose`.
    pub detail: String,
    /// Optional untruncated diagnostic. The Judger path fills this
    /// with the full judge text (including all quorum votes) so a
    /// FAIL report can show everything without re-running. `None`
    /// when the short `detail` already contains the full story.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full_detail: Option<String>,
    /// For Judger only: the score the judger returned.
    #[serde(default)]
    pub score: Option<f64>,
}

fn default_severity() -> CriterionSeverity {
    CriterionSeverity::Hard
}

fn default_min_distinct_work_items() -> usize {
    1
}

/// Classify the severity of a criterion type.
pub fn criterion_severity(c: &Criterion) -> CriterionSeverity {
    match c {
        Criterion::ExitCode { .. }
        | Criterion::FinalState { .. }
        | Criterion::InterruptionKind { .. }
        | Criterion::ToolResultClassCount { .. }
        | Criterion::ToolCalled { .. }
        | Criterion::TextContains { .. }
        | Criterion::TextNotContains { .. }
        | Criterion::TextEquals { .. }
        | Criterion::TextJsonValue { .. }
        | Criterion::TextJsonArrayCount { .. }
        | Criterion::TextJsonPathAbsent { .. }
        | Criterion::TextJsonDag { .. }
        | Criterion::ToolSequence { .. }
        | Criterion::JournalToolSequence { .. }
        | Criterion::JournalToolPrecedence { .. }
        | Criterion::JournalTurnToolHidden { .. }
        | Criterion::JournalWorkItemExecutionFromStart { .. }
        | Criterion::JournalWorkGraphPatch { .. }
        | Criterion::ForkCacheOutcome { .. }
        | Criterion::HardJudger { .. }
        | Criterion::JournalTurnEvaluationSignalCount { .. }
        | Criterion::JournalTurnEvaluationSuccess { .. }
        | Criterion::JournalToolCallCount { .. }
        | Criterion::JournalToolOutcomeCount { .. }
        | Criterion::JournalToolSuccessRatio { .. }
        | Criterion::JournalToolJson { .. }
        | Criterion::JournalToolJsonContains { .. }
        | Criterion::JournalChildToolCallCount { .. }
        | Criterion::JournalArtifactConsumed { .. }
        | Criterion::JournalToolValueFlow { .. }
        | Criterion::JournalToolValueFlowBound { .. }
        | Criterion::SessionSubsystemHealthy { .. }
        | Criterion::PromptCacheReuseScope { .. }
        | Criterion::AnyOf { .. }
        | Criterion::AllOf { .. } => CriterionSeverity::Hard,

        Criterion::SessionEventCount {
            optional: false, ..
        }
        | Criterion::JournalToolCalled {
            optional: false, ..
        } => CriterionSeverity::Hard,

        Criterion::ToolsCountBetween { .. }
        | Criterion::TokensBetween { .. }
        | Criterion::DurationBetween { .. }
        | Criterion::TurnRoundsBetween { .. }
        | Criterion::CacheRateAbove { .. }
        | Criterion::PromptCacheTokens { .. }
        | Criterion::ProviderPromptCacheReadRatio { .. }
        | Criterion::ProviderPromptCacheStablePrefixReuseRatio { .. }
        | Criterion::StderrMatches { .. } => CriterionSeverity::Soft,

        Criterion::Judger { .. }
        | Criterion::SessionEventCount { optional: true, .. }
        | Criterion::JournalToolCalled { optional: true, .. }
        | Criterion::PipelineAlertCount { .. }
        | Criterion::PipelineAvgCacheHitRatio { .. } => CriterionSeverity::Quality,
    }
}

/// Evaluate every criterion against the outcome in list order.
/// Returns all results (not just first failure) so the report can
/// show which specific checks passed.
///
/// Session-dependent criteria (`SessionEventCount`, `JournalToolCalled`)
/// require the loaded session; pass `None` when session capture is off
/// and they'll auto-PASS with a clear "skipped" detail line.
///
/// For Judger criteria the runner calls into
/// [`crate::judger::Judger`] separately — this function is the
/// deterministic-only pass; see [`crate::suite::SuiteRunner`] for
/// the full orchestration.
pub fn evaluate_deterministic(
    criteria: &[Criterion],
    outcome: &RunOutcome,
) -> Vec<CriterionResult> {
    evaluate_deterministic_with_session(criteria, outcome, None)
}

/// Session-aware variant of [`evaluate_deterministic`]. The runner
/// calls this after loading the journal (if any).
pub fn evaluate_deterministic_with_session(
    criteria: &[Criterion],
    outcome: &RunOutcome,
    session: Option<&SessionCapture>,
) -> Vec<CriterionResult> {
    criteria
        .iter()
        .map(|c| evaluate_one(c, outcome, session))
        .collect()
}

/// Whether any criterion in the tree requires a loaded session capture.
pub fn requires_session_capture(criteria: &[Criterion]) -> bool {
    criteria.iter().any(criterion_requires_session_capture)
}

/// Whether a hard criterion will use durable session evidence to certify a
/// run. Optional/quality-only session projections may legitimately skip when
/// evidence is unavailable; hard evidence must additionally be bound to the
/// current terminal run identity.
pub fn requires_durable_run_binding(criteria: &[Criterion]) -> bool {
    criteria.iter().any(|criterion| {
        criterion_requires_session_capture(criterion)
            && criterion_severity(criterion) == CriterionSeverity::Hard
    })
}

fn criterion_requires_session_capture(c: &Criterion) -> bool {
    match c {
        Criterion::SessionEventCount { .. }
        | Criterion::JournalTurnEvaluationSignalCount { .. }
        | Criterion::JournalTurnEvaluationSuccess { .. }
        | Criterion::SessionSubsystemHealthy { .. }
        | Criterion::JournalToolCalled { .. }
        | Criterion::JournalTurnToolHidden { .. }
        | Criterion::JournalToolCallCount { .. }
        | Criterion::JournalToolOutcomeCount { .. }
        | Criterion::JournalToolSuccessRatio { .. }
        | Criterion::JournalToolJson { .. }
        | Criterion::JournalToolJsonContains { .. }
        | Criterion::JournalToolSequence { .. }
        | Criterion::JournalToolPrecedence { .. }
        | Criterion::JournalWorkItemExecutionFromStart { .. }
        | Criterion::JournalWorkGraphPatch { .. }
        | Criterion::JournalArtifactConsumed { .. }
        | Criterion::JournalToolValueFlow { .. }
        | Criterion::JournalToolValueFlowBound { .. }
        | Criterion::PipelineAlertCount { .. }
        | Criterion::PipelineAvgCacheHitRatio { .. }
        | Criterion::ProviderPromptCacheReadRatio { .. }
        | Criterion::ProviderPromptCacheStablePrefixReuseRatio { .. }
        | Criterion::PromptCacheReuseScope { .. } => true,
        Criterion::AnyOf { criteria } | Criterion::AllOf { criteria } => {
            requires_session_capture(criteria)
        }
        _ => false,
    }
}

fn missing_required_session(c: &Criterion, label: &str) -> CriterionResult {
    CriterionResult {
        criterion: c.clone(),
        severity: criterion_severity(c),
        passed: false,
        detail: format!(
            "{label} FAILED: no session loaded (enable debug_log: true / --capture-session)"
        ),
        full_detail: None,
        score: None,
    }
}

const SESSION_TOOL_RESULT_ARTIFACT_PREFIX: &str = "artifact://session/tool-result/";

fn is_flow_scalar(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::String(value) => !value.trim().is_empty(),
        serde_json::Value::Number(_) | serde_json::Value::Bool(_) => true,
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            false
        }
    }
}

fn value_flows_into(source: &serde_json::Value, destination: &serde_json::Value) -> bool {
    if !is_flow_scalar(source) {
        return false;
    }
    match destination {
        value if value == source => true,
        serde_json::Value::Array(values) => {
            values.iter().any(|value| value_flows_into(source, value))
        }
        serde_json::Value::Object(values) => {
            values.values().any(|value| value_flows_into(source, value))
        }
        _ => false,
    }
}

/// Resolve a small, deliberately typed extension of RFC 6901 pointers for
/// durable evidence projections. A `*` segment walks every child of an array
/// or object, so a criterion can prove that a scalar appeared in any element
/// of a provider result without binding the oracle to ranking/index order.
fn flow_destinations_at_path<'a>(
    document: &'a serde_json::Value,
    path: &str,
) -> Vec<&'a serde_json::Value> {
    if path.is_empty() {
        return vec![document];
    }
    let Some(segments) = path.strip_prefix('/').map(|rest| rest.split('/')) else {
        return Vec::new();
    };
    let mut current = vec![document];
    for encoded in segments {
        let segment = encoded.replace("~1", "/").replace("~0", "~");
        let mut next = Vec::new();
        for value in current {
            match value {
                serde_json::Value::Array(values) if segment == "*" => {
                    next.extend(values.iter());
                }
                serde_json::Value::Object(values) if segment == "*" => {
                    next.extend(values.values());
                }
                serde_json::Value::Array(values) => {
                    if let Ok(index) = segment.parse::<usize>()
                        && let Some(value) = values.get(index)
                    {
                        next.push(value);
                    }
                }
                serde_json::Value::Object(values) => {
                    if let Some(value) = values.get(&segment) {
                        next.push(value);
                    }
                }
                _ => {}
            }
        }
        if next.is_empty() {
            return Vec::new();
        }
        current = next;
    }
    current
}

fn call_matches_predicate(
    call: &crate::session_capture::JournalToolCall,
    predicate: Option<&JournalJsonPredicate>,
) -> bool {
    let Some(predicate) = predicate else {
        return true;
    };
    let document = match predicate.document {
        JournalToolDocument::Arguments => call.arguments.as_ref(),
        JournalToolDocument::Result => call.result.as_ref(),
    };
    document.and_then(|value| value.pointer(&predicate.path)) == Some(&predicate.equals)
}

fn call_matches_predicates(
    call: &crate::session_capture::JournalToolCall,
    predicates: &[JournalJsonPredicate],
) -> bool {
    predicates
        .iter()
        .all(|predicate| call_matches_predicate(call, Some(predicate)))
}

fn collect_session_artifact_handles(
    value: &serde_json::Value,
    handles: &mut std::collections::HashSet<String>,
) {
    match value {
        serde_json::Value::String(text) => {
            for (start, _) in text.match_indices(SESSION_TOOL_RESULT_ARTIFACT_PREFIX) {
                let suffix = &text[start + SESSION_TOOL_RESULT_ARTIFACT_PREFIX.len()..];
                let token_len = suffix
                    .bytes()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                    .count();
                if token_len > 0 {
                    handles.insert(format!(
                        "{SESSION_TOOL_RESULT_ARTIFACT_PREFIX}{}",
                        &suffix[..token_len]
                    ));
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_session_artifact_handles(value, handles);
            }
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values() {
                collect_session_artifact_handles(value, handles);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn parse_complete_text_json(outcome: &RunOutcome) -> Result<serde_json::Value, String> {
    serde_json::from_str(outcome.text.trim())
        .map_err(|error| format!("assistant text is not exactly one JSON value: {error}"))
}

fn required_json_array<'a>(
    document: &'a serde_json::Value,
    path: &str,
    label: &str,
) -> Result<&'a Vec<serde_json::Value>, String> {
    document
        .pointer(path)
        .ok_or_else(|| format!("{label} pointer {path:?} is absent"))?
        .as_array()
        .ok_or_else(|| format!("{label} pointer {path:?} is not an array"))
}

fn required_relative_string<'a>(
    document: &'a serde_json::Value,
    path: &str,
    label: &str,
) -> Result<&'a str, String> {
    document
        .pointer(path)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{label} pointer {path:?} is not a non-empty string"))
}

fn validate_text_json_dag(
    document: &serde_json::Value,
    nodes_path: &str,
    node_id_path: &str,
    node_required_string_paths: &[String],
    edges_path: &str,
    predecessor_path: &str,
    successor_path: &str,
) -> Result<(usize, usize), String> {
    let nodes = required_json_array(document, nodes_path, "nodes")?;
    let edges = required_json_array(document, edges_path, "edges")?;
    if nodes.is_empty() {
        return Err("nodes array is empty".into());
    }

    let mut node_ids = BTreeSet::new();
    for (index, node) in nodes.iter().enumerate() {
        let id = required_relative_string(node, node_id_path, &format!("nodes[{index}]"))?;
        for path in node_required_string_paths {
            required_relative_string(node, path, &format!("nodes[{index}]"))?;
        }
        if !node_ids.insert(id.to_string()) {
            return Err(format!("duplicate node id {id:?}"));
        }
    }

    let mut outgoing: BTreeMap<String, Vec<String>> =
        node_ids.iter().map(|id| (id.clone(), Vec::new())).collect();
    let mut indegree: BTreeMap<String, usize> = node_ids.iter().map(|id| (id.clone(), 0)).collect();
    let mut unique_edges = BTreeSet::new();

    for (index, edge) in edges.iter().enumerate() {
        let predecessor = required_relative_string(
            edge,
            predecessor_path,
            &format!("edges[{index}].predecessor"),
        )?;
        let successor =
            required_relative_string(edge, successor_path, &format!("edges[{index}].successor"))?;
        if predecessor == successor {
            return Err(format!("edge {index} is a self-cycle on {predecessor:?}"));
        }
        if !node_ids.contains(predecessor) || !node_ids.contains(successor) {
            return Err(format!(
                "edge {index} references undeclared endpoint {predecessor:?} -> {successor:?}"
            ));
        }
        if !unique_edges.insert((predecessor.to_string(), successor.to_string())) {
            return Err(format!("duplicate edge {predecessor:?} -> {successor:?}"));
        }
        outgoing
            .get_mut(predecessor)
            .expect("declared predecessor has adjacency entry")
            .push(successor.to_string());
        *indegree
            .get_mut(successor)
            .expect("declared successor has indegree entry") += 1;
    }

    let mut ready: VecDeque<String> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut visited = 0usize;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for successor in outgoing.get(&id).into_iter().flatten() {
            let degree = indegree
                .get_mut(successor)
                .expect("adjacency endpoint has indegree entry");
            *degree -= 1;
            if *degree == 0 {
                ready.push_back(successor.clone());
            }
        }
    }
    if visited != node_ids.len() {
        return Err(format!(
            "graph contains a cycle (visited {visited} of {} nodes)",
            node_ids.len()
        ));
    }
    Ok((node_ids.len(), unique_edges.len()))
}

fn evaluate_one(
    c: &Criterion,
    outcome: &RunOutcome,
    session: Option<&SessionCapture>,
) -> CriterionResult {
    if let Some(capture) = session
        && (capture.skipped_lines > 0
            || capture.dropped_lines > 0
            || capture.has_integrity_errors())
        && criterion_requires_session_capture(c)
    {
        let incomplete = format!(
            "session evidence incomplete (malformed={}, dropped={}, integrity_errors={})",
            capture.skipped_lines, capture.dropped_lines, capture.integrity_errors
        );
        if criterion_allows_incomplete_capture(c) {
            return CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: true,
                detail: format!("{incomplete}; optional criterion skipped"),
                full_detail: None,
                score: Some(1.0),
            };
        }
        return CriterionResult {
            criterion: c.clone(),
            severity: criterion_severity(c),
            passed: false,
            detail: incomplete,
            full_detail: None,
            score: Some(0.0),
        };
    }

    match c {
        Criterion::ToolCalled { name } => {
            let hit = outcome.tools_used.iter().any(|t| t == name);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: hit,
                detail: if hit {
                    format!("tool {name} was called")
                } else {
                    format!(
                        "tool {name} NOT called (tools_used: {:?})",
                        outcome.tools_used
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::FinalState { expect } => {
            let actual = outcome.final_state.as_deref().unwrap_or("missing");
            let pass = actual == expect;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("final_state={actual} (expected {expect})"),
                full_detail: None,
                score: None,
            }
        }
        Criterion::InterruptionKind { expect } => {
            let actual = outcome.interruption_kind.as_deref().unwrap_or("missing");
            let pass = actual == expect;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("interruption_kind={actual} (expected {expect})"),
                full_detail: None,
                score: None,
            }
        }
        Criterion::ToolResultClassCount { class, min, max } => {
            let count = outcome
                .tool_result_class_counts
                .get(class)
                .copied()
                .unwrap_or(0);
            let pass = count >= *min && count <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("tool_result_class_count[{class}]={count}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }
        Criterion::ExitCode { code } => {
            let pass = outcome.exit_code == *code;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("exit_code {} (expected {})", outcome.exit_code, code),
                full_detail: None,
                score: None,
            }
        }
        Criterion::ToolsCountBetween { min, max } => {
            let n = outcome.tool_calls_count;
            let pass = n >= *min && n <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("tool_calls_count={n}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }
        Criterion::StderrMatches { pattern } => {
            // Multi-line mode so `^` / `$` anchor at line boundaries —
            // stderr is almost always a log stream, and users write
            // patterns like `^\[fork-cache\]` expecting per-line match.
            match Regex::new(&format!("(?m){pattern}")) {
                Ok(re) => {
                    let hit = re.is_match(&outcome.stderr);
                    CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        passed: hit,
                        detail: if hit {
                            format!("stderr matches /{pattern}/")
                        } else {
                            format!(
                                "stderr does NOT match /{pattern}/ (stderr len={})",
                                outcome.stderr.len()
                            )
                        },
                        full_detail: None,
                        score: None,
                    }
                }
                Err(e) => CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!("invalid regex /{pattern}/: {e}"),
                    full_detail: None,
                    score: None,
                },
            }
        }
        Criterion::TextContains { needle } => {
            let hit = outcome.text.contains(needle);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: hit,
                detail: if hit {
                    format!("text contains {needle:?}")
                } else {
                    format!(
                        "text does NOT contain {needle:?} (text len={})",
                        outcome.text.len()
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::TextNotContains { needle } => {
            let hit = outcome.text.contains(needle);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: !hit,
                detail: if hit {
                    format!("text unexpectedly contains {needle:?}")
                } else {
                    format!("text does not contain {needle:?}")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::TextEquals { expected } => {
            let actual = outcome.text.trim();
            let expected = expected.trim();
            let passed = actual == expected;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!("text exactly matches {expected:?}")
                } else {
                    format!("text did not exactly match {expected:?}; got {actual:?}")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::TextJsonValue { path, equals } => {
            let result = parse_complete_text_json(outcome).and_then(|document| {
                document
                    .pointer(path)
                    .ok_or_else(|| format!("JSON pointer {path:?} is absent"))
                    .and_then(|actual| {
                        if actual == equals {
                            Ok(())
                        } else {
                            Err(format!(
                                "JSON pointer {path:?} did not equal its expected value"
                            ))
                        }
                    })
            });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: result.is_ok(),
                detail: result.map_or_else(|error| error, |_| format!("JSON {path:?} matched")),
                full_detail: None,
                score: None,
            }
        }
        Criterion::TextJsonArrayCount { path, min, max } => {
            let result = parse_complete_text_json(outcome).and_then(|document| {
                let count = u32::try_from(required_json_array(&document, path, "array")?.len())
                    .map_err(|_| format!("JSON array {path:?} is too large to count"))?;
                if (*min..=*max).contains(&count) {
                    Ok(count)
                } else {
                    Err(format!(
                        "JSON array {path:?} has {count} items, expected {min}..={max}"
                    ))
                }
            });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: result.is_ok(),
                detail: result.map_or_else(
                    |error| error,
                    |count| format!("JSON array {path:?} has {count} items"),
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::TextJsonPathAbsent { path } => {
            let result = parse_complete_text_json(outcome).and_then(|document| {
                if document.pointer(path).is_none() {
                    Ok(())
                } else {
                    Err(format!("JSON pointer {path:?} is unexpectedly present"))
                }
            });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: result.is_ok(),
                detail: result.map_or_else(|error| error, |_| format!("JSON {path:?} is absent")),
                full_detail: None,
                score: None,
            }
        }
        Criterion::TextJsonDag {
            nodes_path,
            node_id_path,
            node_required_string_paths,
            edges_path,
            predecessor_path,
            successor_path,
        } => {
            let result = parse_complete_text_json(outcome).and_then(|document| {
                validate_text_json_dag(
                    &document,
                    nodes_path,
                    node_id_path,
                    node_required_string_paths,
                    edges_path,
                    predecessor_path,
                    successor_path,
                )
            });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: result.is_ok(),
                detail: result.map_or_else(
                    |error| format!("JSON DAG invalid: {error}"),
                    |(nodes, edges)| format!("JSON DAG is acyclic ({nodes} nodes, {edges} edges)"),
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::AnyOf { criteria } => {
            let mut nested = Vec::new();
            let mut passed = false;
            for criterion in criteria {
                let result = evaluate_one(criterion, outcome, session);
                passed = result.passed;
                nested.push(result);
                if passed {
                    break;
                }
            }
            let detail = nested
                .iter()
                .enumerate()
                .map(|(idx, result)| {
                    format!(
                        "#{idx}:{}:{}",
                        if result.passed { "pass" } else { "fail" },
                        result.detail
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!("any_of passed ({detail})")
                } else {
                    format!("any_of failed ({detail})")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::AllOf { criteria } => {
            let mut nested = Vec::new();
            let mut passed = true;
            for criterion in criteria {
                let result = evaluate_one(criterion, outcome, session);
                passed = result.passed;
                nested.push(result);
                if !passed {
                    break;
                }
            }
            let detail = nested
                .iter()
                .enumerate()
                .map(|(idx, result)| {
                    format!(
                        "#{idx}:{}:{}",
                        if result.passed { "pass" } else { "fail" },
                        result.detail
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!("all_of passed ({detail})")
                } else {
                    format!("all_of failed ({detail})")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::SessionEventCount {
            event_type,
            min,
            optional,
        } => {
            let Some(sess) = session else {
                // Session unavailable. Default (strict): FAIL with
                // an actionable detail so the reviewer knows the
                // criterion was requested but couldn't run. Setting
                // `optional: true` opts into skip-pass.
                let passed = *optional;
                let detail = if *optional {
                    format!(
                        "session_event_count {event_type} skipped (optional + no session capture)"
                    )
                } else {
                    format!(
                        "session_event_count {event_type} FAILED: no session \
                         loaded (enable debug_log: true in the case or \
                         --capture-session on the CLI; set optional: true on \
                         the criterion to skip-pass instead)"
                    )
                };
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail,
                    full_detail: None,
                    score: if passed { Some(1.0) } else { Some(0.0) },
                };
            };
            let n = sess.count_events(event_type);
            let pass = n as u32 >= *min;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: format!("session events type={event_type} count={n} (expected >= {min})"),
                full_detail: None,
                score: if pass { Some(1.0) } else { Some(0.0) },
            }
        }
        Criterion::JournalTurnEvaluationSignalCount { kind, min, max } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_turn_evaluation_signal_count");
            };
            let count = session
                .events
                .iter()
                .filter(|event| event.event_type == "turn_evaluation")
                .flat_map(|event| {
                    event
                        .raw
                        .get("metadata")
                        .and_then(|metadata| metadata.get("signals"))
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .filter(|signal| {
                    signal.get("kind").and_then(serde_json::Value::as_str) == Some(kind)
                })
                .count() as u32;
            let passed = count >= *min && count <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "journal turn-evaluation signal {kind} count={count}, expected {min}..={max}"
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalTurnEvaluationSuccess { equals } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_turn_evaluation_success");
            };
            let observed = session
                .events
                .iter()
                .filter(|event| event.event_type == "turn_evaluation")
                .max_by_key(|event| {
                    let timestamp = event
                        .raw
                        .get("ts")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                        .map(|ts| ts.timestamp_millis())
                        .unwrap_or(i64::MIN);
                    let turn = event
                        .raw
                        .get("turn")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    (timestamp, turn)
                })
                .and_then(|event| event.raw.pointer("/metadata/success"))
                .and_then(serde_json::Value::as_bool);
            let passed = observed == Some(*equals);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: match observed {
                    Some(value) => format!(
                        "terminal journal turn-evaluation success={value} (expected {equals})"
                    ),
                    None => "terminal journal turn-evaluation success is missing".to_string(),
                },
                full_detail: None,
                score: if passed { Some(1.0) } else { Some(0.0) },
            }
        }
        Criterion::SessionSubsystemHealthy { settled_subsystem } => {
            let Some(capture) = session else {
                return missing_required_session(c, "session_subsystem_healthy");
            };
            if capture.skipped_lines > 0
                || capture.dropped_lines > 0
                || capture.has_integrity_errors()
            {
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!(
                        "session health evidence incomplete (malformed={}, dropped={}, integrity_errors={})",
                        capture.skipped_lines, capture.dropped_lines, capture.integrity_errors
                    ),
                    full_detail: None,
                    score: None,
                };
            }
            let Some(latest_turn) = capture.latest_canonical_turn() else {
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: "session health evidence has no canonical turn".into(),
                    full_detail: None,
                    score: None,
                };
            };
            if let Some(expected) = settled_subsystem
                && !capture.subsystem_settled_for_latest_turn(expected)
            {
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!("asynchronous subsystem evidence did not settle: {expected}"),
                    full_detail: None,
                    score: None,
                };
            }

            let unhealthy = capture
                .events
                .iter()
                .filter(|event| {
                    event.raw.get("turn").and_then(|value| value.as_u64()) == Some(latest_turn)
                })
                .find_map(|event| match event.event_type.as_str() {
                    "session_memory_extraction" => {
                        let metadata = event.raw.get("metadata");
                        match metadata
                            .and_then(|value| value.get("outcome"))
                            .and_then(|value| value.as_str())
                        {
                            Some("extracted" | "skipped") => None,
                            Some("errored") => Some(format!(
                                "session_memory.extraction: {}",
                                metadata
                                    .and_then(|value| value.get("reason"))
                                    .and_then(|value| value.as_str())
                                    .unwrap_or("invalid_event")
                            )),
                            _ => Some("session_memory.extraction: invalid_event".into()),
                        }
                    }
                    "subsystem_diagnostic" => {
                        let metadata = event.raw.get("metadata");
                        Some(format!(
                            "{}.{}: {} ({})",
                            metadata
                                .and_then(|value| value.get("subsystem"))
                                .and_then(|value| value.as_str())
                                .unwrap_or("unknown"),
                            metadata
                                .and_then(|value| value.get("operation"))
                                .and_then(|value| value.as_str())
                                .unwrap_or("unknown"),
                            metadata
                                .and_then(|value| value.get("code"))
                                .and_then(|value| value.as_str())
                                .unwrap_or("invalid_event"),
                            metadata
                                .and_then(|value| value.get("severity"))
                                .and_then(|value| value.as_str())
                                .unwrap_or("invalid")
                        ))
                    }
                    _ => None,
                });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: unhealthy.is_none(),
                detail: unhealthy.map_or_else(
                    || "no durable asynchronous subsystem failures or degradation".into(),
                    |diagnostic| format!("asynchronous subsystem unhealthy: {diagnostic}"),
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolCalled { name, optional } => {
            let Some(sess) = session else {
                let passed = *optional;
                let detail = if *optional {
                    format!("journal_tool_called {name} skipped (optional + no session capture)")
                } else {
                    format!(
                        "journal_tool_called {name} FAILED: no session loaded \
                         (enable debug_log: true / --capture-session; or set \
                         optional: true on the criterion to skip-pass)"
                    )
                };
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail,
                    full_detail: None,
                    score: if passed { Some(1.0) } else { Some(0.0) },
                };
            };
            let tools = sess.tools_invoked();
            let hit = tools.iter().any(|t| t == name);
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: hit,
                detail: if hit {
                    format!("journal tool {name} was invoked")
                } else {
                    format!("journal tool {name} NOT invoked (journal tools: {tools:?})")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalChildToolCallCount {
            parent,
            child,
            min,
            max,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_child_tool_call_count");
            };
            let count = session.causal_child_tool_call_count(parent, child) as u32;
            let passed = count >= *min && count <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "causal child tool {child} under {parent} count={count}, expected {min}..={max}"
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalTurnToolHidden { name } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_turn_tool_hidden");
            };
            let (turns, exposed) = session.canonical_turn_tool_surface_count(name);
            let passed = turns > 0 && exposed == 0;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if turns == 0 {
                    "canonical turn evidence contains no persisted tool surface".to_string()
                } else {
                    format!(
                        "tool {name} exposed in {exposed}/{turns} canonical turn surfaces (expected 0/{turns})"
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolCallCount {
            name,
            min,
            max,
            document,
            path,
            equals,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_call_count");
            };
            let count = session
                .journal_tool_calls()
                .iter()
                .filter(|call| {
                    if call.name != *name {
                        return false;
                    }
                    let (Some(document), Some(path), Some(equals)) =
                        (document, path.as_deref(), equals)
                    else {
                        return true;
                    };
                    let value = match document {
                        JournalToolDocument::Arguments => call.arguments.as_ref(),
                        JournalToolDocument::Result => call.result.as_ref(),
                    };
                    value.and_then(|value| value.pointer(path)) == Some(equals)
                })
                .count() as u32;
            let passed = count >= *min && count <= *max;
            let predicate = match (document, path, equals) {
                (Some(document), Some(path), Some(equals)) => {
                    format!(" filtered by {document:?} {path:?} == {equals}")
                }
                _ => String::new(),
            };
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "journal tool {name}{predicate} full-call count={count}, expected {min}..={max}"
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolOutcomeCount { name, ok, min, max } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_outcome_count");
            };
            let count = session
                .journal_tool_calls()
                .iter()
                .filter(|call| call.name == *name && call.ok == Some(*ok))
                .count() as u32;
            let passed = count >= *min && count <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "journal tool {name} ok={ok} full-call count={count}, expected {min}..={max}"
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolSuccessRatio {
            min,
            min_calls,
            allowed_failures,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_success_ratio");
            };
            let calls = session.journal_tool_calls();
            let terminal = calls.iter().filter(|call| call.ok.is_some()).count() as u32;
            let succeeded = calls.iter().filter(|call| call.ok == Some(true)).count() as u32;
            let raw_ratio = if terminal == 0 {
                0.0
            } else {
                f64::from(succeeded) / f64::from(terminal)
            };
            let failures = terminal.saturating_sub(succeeded);
            let unexpected_failures = failures.saturating_sub(*allowed_failures);
            let adjusted_terminal = succeeded.saturating_add(unexpected_failures);
            let adjusted_ratio = if adjusted_terminal == 0 {
                0.0
            } else {
                f64::from(succeeded) / f64::from(adjusted_terminal)
            };
            let passed = terminal >= *min_calls && adjusted_ratio >= *min;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "journal tool success raw={succeeded}/{terminal} ({raw_ratio:.1}%), adjusted={succeeded}/{adjusted_terminal} ({adjusted_ratio:.1}%) with allowed_failures={allowed_failures}; expected calls>={min_calls} and adjusted ratio>={min:.1}%",
                    raw_ratio = raw_ratio * 100.0,
                    adjusted_ratio = adjusted_ratio * 100.0,
                    min = min * 100.0,
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolJson {
            name,
            document,
            path,
            equals,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_json");
            };
            let calls = session.journal_tool_calls();
            let passed = calls.iter().filter(|call| call.name == *name).any(|call| {
                let value = match document {
                    JournalToolDocument::Arguments => call.arguments.as_ref(),
                    JournalToolDocument::Result => call.result.as_ref(),
                };
                value.and_then(|value| value.pointer(path)) == Some(equals)
            });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "journal tool {name} {document:?} pointer {path:?} {} expected {}",
                    if passed { "matched" } else { "did not match" },
                    equals
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolJsonContains {
            name,
            document,
            path,
            contains,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_json_contains");
            };
            let calls = session.journal_tool_calls();
            let passed = calls
                .iter()
                .filter(|call| call.name == *name && call.ok == Some(true))
                .any(|call| {
                    let value = match document {
                        JournalToolDocument::Arguments => call.arguments.as_ref(),
                        JournalToolDocument::Result => call.result.as_ref(),
                    };
                    value
                        .and_then(|value| value.pointer(path))
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|text| text.contains(contains))
                });
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "journal tool {name} {document:?} pointer {path:?} {} substring {:?}",
                    if passed {
                        "contained"
                    } else {
                        "did not contain"
                    },
                    contains
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolSequence { tools } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_sequence");
            };
            let calls = session.journal_tool_calls();
            let mut calls = calls.iter();
            let mut matched = 0usize;
            for expected in tools {
                if calls.any(|call| call.name == *expected) {
                    matched += 1;
                }
            }
            let passed = matched == tools.len();
            let actual = session
                .journal_tool_calls()
                .into_iter()
                .map(|call| call.name)
                .collect::<Vec<_>>();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!("journal tool sequence {tools:?} found")
                } else {
                    format!(
                        "journal tool sequence {tools:?} NOT found (matched {matched}/{}, actual: {actual:?})",
                        tools.len()
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolPrecedence {
            predecessor,
            successor,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_precedence");
            };
            let calls = session.journal_tool_calls();
            let mut saw_predecessor = false;
            let mut successor_count = 0usize;
            let mut first_violation = None;
            for call in &calls {
                if call.name == *predecessor {
                    saw_predecessor = true;
                }
                if call.name == *successor {
                    successor_count += 1;
                    if !saw_predecessor {
                        first_violation =
                            Some(call.call_id.clone().unwrap_or_else(|| "<no-id>".into()));
                        break;
                    }
                }
            }
            let passed = successor_count > 0 && first_violation.is_none();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if let Some(call_id) = first_violation {
                    format!(
                        "journal precedence FAILED: {successor} call {call_id} occurred before {predecessor}"
                    )
                } else if successor_count == 0 {
                    format!(
                        "journal precedence FAILED: expected at least one {successor} after {predecessor}, but no {successor} call was recorded"
                    )
                } else {
                    format!(
                        "journal precedence satisfied: {successor_count} {successor} call(s) followed {predecessor}"
                    )
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalWorkItemExecutionFromStart { min_distinct_items } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_work_item_execution_from_start");
            };

            let mut runnable_items = std::collections::HashSet::new();
            let mut declared_task_refs = std::collections::HashSet::new();
            let mut checked_executions = 0usize;
            let mut owned_items = std::collections::HashSet::new();
            let mut pending_assignment: Option<(String, u64)> = None;
            let mut first_failure = session.first_work_assignment_surface_gap();
            for call in session.journal_tool_calls() {
                if call.name == "start_work"
                    && call.ok != Some(false)
                    && call
                        .result
                        .as_ref()
                        .and_then(|result| result.get("status"))
                        .and_then(serde_json::Value::as_str)
                        == Some("started")
                {
                    let Some(items) = call
                        .result
                        .as_ref()
                        .and_then(|result| result.get("declared_tasks"))
                        .and_then(serde_json::Value::as_array)
                    else {
                        first_failure.get_or_insert_with(|| {
                            "start_work omitted its server-issued declared task identities"
                                .to_string()
                        });
                        continue;
                    };
                    if items.is_empty() {
                        first_failure.get_or_insert_with(|| {
                            "start_work reported success with no declared tasks".to_string()
                        });
                        continue;
                    }
                    for item in items {
                        let declared = item.get("item_id").and_then(serde_json::Value::as_str).zip(
                            item.get("item_revision")
                                .and_then(serde_json::Value::as_u64),
                        );
                        match declared {
                            Some((item_id, revision)) => {
                                declared_task_refs.insert((item_id.to_string(), revision));
                            }
                            None => {
                                first_failure.get_or_insert_with(|| {
                                    "start_work declared a task without its exact server-issued identity"
                                        .to_string()
                                });
                            }
                        }
                    }
                    if let Some(items) = call
                        .result
                        .as_ref()
                        .and_then(|result| result.get("runnable_items"))
                        .and_then(serde_json::Value::as_array)
                    {
                        for item in items {
                            let Some(item_id) =
                                item.get("item_id").and_then(serde_json::Value::as_str)
                            else {
                                continue;
                            };
                            let Some(item_revision) = item
                                .get("item_revision")
                                .and_then(serde_json::Value::as_u64)
                            else {
                                continue;
                            };
                            let item = (item_id.to_string(), item_revision);
                            if !declared_task_refs.contains(&item) {
                                first_failure.get_or_insert_with(|| {
                                    format!(
                                        "start_work marked undeclared WorkItem {item_id}@{item_revision} runnable"
                                    )
                                });
                            } else {
                                runnable_items.insert(item);
                            }
                        }
                    }
                    if let Some(initial_task) = call
                        .result
                        .as_ref()
                        .and_then(|result| result.get("initial_task"))
                        .filter(|task| {
                            task.get("status").and_then(serde_json::Value::as_str)
                                == Some("assigned")
                        })
                    {
                        checked_executions += 1;
                        let assignment = initial_task
                            .get("item_id")
                            .and_then(serde_json::Value::as_str)
                            .zip(
                                initial_task
                                    .get("item_revision")
                                    .and_then(serde_json::Value::as_u64),
                            );
                        match assignment {
                            Some((item_id, revision))
                                if runnable_items.contains(&(item_id.to_string(), revision))
                                    && pending_assignment.is_none() =>
                            {
                                pending_assignment = Some((item_id.to_string(), revision));
                            }
                            Some((item_id, revision)) => {
                                first_failure.get_or_insert_with(|| {
                                    format!(
                                        "initial assignment selected {item_id}@{revision}, not an initially runnable WorkItem"
                                    )
                                });
                            }
                            None => {
                                first_failure.get_or_insert_with(|| {
                                    "start_work initial assignment omitted its exact WorkItem identity"
                                        .to_string()
                                });
                            }
                        }
                    }
                    continue;
                }

                if call.ok == Some(false) {
                    continue;
                }
                let status = call
                    .result
                    .as_ref()
                    .and_then(|result| result.get("status"))
                    .and_then(serde_json::Value::as_str);
                if call.name == "run_next_work_item" && status == Some("assigned") {
                    checked_executions += 1;
                    let assignment = call.result.as_ref().and_then(|result| {
                        Some((
                            result.get("item_id")?.as_str()?.to_string(),
                            result.get("item_revision")?.as_u64()?,
                        ))
                    });
                    let Some(assignment) = assignment else {
                        first_failure.get_or_insert_with(|| {
                            format!(
                                "run_next_work_item {} has no server-selected Work item identity",
                                call.call_id.as_deref().unwrap_or("<no-id>")
                            )
                        });
                        continue;
                    };
                    if let Some(active) = pending_assignment.as_ref() {
                        if active == &assignment {
                            // `run_next_work_item` is an idempotent recovery surface. The
                            // server may return the exact active assignment again when the
                            // coordinator lost or ignored its earlier receipt; this does not
                            // create a second execution and must not be projected as one.
                            continue;
                        }
                        first_failure.get_or_insert_with(|| {
                            format!(
                                "WorkItem {}@{} was assigned while {}@{} was still active",
                                assignment.0, assignment.1, active.0, active.1
                            )
                        });
                        continue;
                    }
                    if !declared_task_refs.contains(&assignment) {
                        first_failure.get_or_insert_with(|| {
                            format!(
                                "assignment selected undeclared WorkItem {}@{}",
                                assignment.0, assignment.1
                            )
                        });
                        continue;
                    }
                    if owned_items.is_empty() && !runnable_items.contains(&assignment) {
                        first_failure.get_or_insert_with(|| {
                            format!(
                                "first assignment selected {}@{}, not an initially runnable WorkItem",
                                assignment.0, assignment.1
                            )
                        });
                        continue;
                    }
                    pending_assignment = Some(assignment);
                    continue;
                }
                if call.name != "settle_work_item" || status != Some("recorded") {
                    continue;
                }
                let settlement = call.result.as_ref().and_then(|result| {
                    Some((
                        result.get("item_id")?.as_str()?.to_string(),
                        result.get("item_revision")?.as_u64()?,
                    ))
                });
                match (pending_assignment.take(), settlement) {
                    (Some(assigned), Some(settled)) if assigned == settled => {
                        owned_items.insert(settled);
                    }
                    (Some(assigned), Some(settled)) => {
                        first_failure.get_or_insert_with(|| {
                            format!(
                                "settlement for {}@{} does not match active assignment {}@{}",
                                settled.0, settled.1, assigned.0, assigned.1
                            )
                        });
                    }
                    (Some(_), None) => {
                        first_failure.get_or_insert_with(|| {
                            "settle_work_item omitted its exact WorkItem identity".to_string()
                        });
                    }
                    (None, _) => {
                        first_failure.get_or_insert_with(|| {
                            "settle_work_item recorded without a preceding active assignment"
                                .to_string()
                        });
                    }
                }
                if let Some(next_task) = call
                    .result
                    .as_ref()
                    .and_then(|result| result.get("next_task"))
                    .filter(|next| {
                        next.get("status").and_then(serde_json::Value::as_str) == Some("assigned")
                    })
                {
                    checked_executions += 1;
                    let assignment = next_task
                        .get("item_id")
                        .and_then(serde_json::Value::as_str)
                        .zip(
                            next_task
                                .get("item_revision")
                                .and_then(serde_json::Value::as_u64),
                        );
                    match assignment {
                        Some((item_id, revision))
                            if pending_assignment.is_none()
                                && declared_task_refs
                                    .contains(&(item_id.to_string(), revision)) =>
                        {
                            pending_assignment = Some((item_id.to_string(), revision));
                        }
                        Some((item_id, revision))
                            if !declared_task_refs.contains(&(item_id.to_string(), revision)) =>
                        {
                            first_failure.get_or_insert_with(|| {
                                format!(
                                    "settlement advanced to undeclared WorkItem {item_id}@{revision}"
                                )
                            });
                        }
                        Some(_) => {
                            first_failure.get_or_insert_with(|| {
                                "settlement advanced while another WorkItem assignment was active"
                                    .to_string()
                            });
                        }
                        None => {
                            first_failure.get_or_insert_with(|| {
                                "settlement successor omitted its exact WorkItem identity"
                                    .to_string()
                            });
                        }
                    }
                }
            }
            if let Some((item_id, revision)) = pending_assignment {
                first_failure.get_or_insert_with(|| {
                    format!("WorkItem {item_id}@{revision} was assigned but never settled")
                });
            }

            let passed = first_failure.is_none() && owned_items.len() >= *min_distinct_items;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if let Some(failure) = first_failure {
                    failure
                } else if owned_items.len() >= *min_distinct_items {
                    format!(
                        "{} distinct server-selected WorkItems were assigned and exactly settled",
                        owned_items.len()
                    )
                } else {
                    if runnable_items.is_empty() {
                        "no successful start_work runnable-item receipt was recorded".into()
                    } else if checked_executions == 0 {
                        "no server-selected WorkItem assignment was recorded".into()
                    } else {
                        format!(
                            "only {} distinct runnable WorkItems were executed; expected at least {min_distinct_items}",
                            owned_items.len()
                        )
                    }
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalWorkGraphPatch {
            require_addition,
            require_active_revision,
            require_retired_revision,
            require_cancelled_revision,
            require_superseded_revision,
            require_dependency_change,
            require_atomic_retire_and_add,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_work_graph_patch");
            };

            let mut work_established = false;
            let mut accepted_patches = 0usize;
            let mut accepted_addition = false;
            let mut accepted_active_revision = false;
            let mut accepted_retired_revision = false;
            let mut accepted_cancelled_revision = false;
            let mut accepted_superseded_revision = false;
            let mut accepted_dependency_change = false;
            let mut accepted_atomic_retire_and_add = false;
            let mut integrity_failure = false;
            let mut first_failure = None;
            for call in session.journal_tool_calls() {
                if call.name == "start_work"
                    && call.ok != Some(false)
                    && call
                        .result
                        .as_ref()
                        .and_then(|result| result.get("status"))
                        .and_then(serde_json::Value::as_str)
                        == Some("started")
                {
                    work_established = true;
                    continue;
                }
                if call.name != "propose_work_plan" {
                    continue;
                }
                if !work_established {
                    integrity_failure = true;
                    first_failure.get_or_insert_with(|| {
                        "propose_work_plan was invoked before canonical Work was established"
                            .to_string()
                    });
                    continue;
                }
                if call.ok == Some(false)
                    || call
                        .result
                        .as_ref()
                        .and_then(|result| result.get("status"))
                        .and_then(serde_json::Value::as_str)
                        != Some("accepted")
                {
                    first_failure.get_or_insert_with(|| {
                        "a Work graph patch did not produce an accepted durable receipt".to_string()
                    });
                    continue;
                }
                let Some(arguments) = call.arguments.as_ref() else {
                    integrity_failure = true;
                    first_failure.get_or_insert_with(|| {
                        "an accepted Work graph patch omitted typed arguments".to_string()
                    });
                    continue;
                };
                let additions = arguments
                    .get("additions")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|entries| !entries.is_empty());
                let revisions = arguments
                    .get("revisions")
                    .and_then(serde_json::Value::as_array);
                let active_revision = revisions.is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry
                            .get("declaration_state")
                            .and_then(serde_json::Value::as_str)
                            == Some("active")
                    })
                });
                let retired_revision = revisions.is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        matches!(
                            entry
                                .get("declaration_state")
                                .and_then(serde_json::Value::as_str),
                            Some("cancelled" | "superseded")
                        )
                    })
                });
                let cancelled_revision = revisions.is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry
                            .get("declaration_state")
                            .and_then(serde_json::Value::as_str)
                            == Some("cancelled")
                    })
                });
                let superseded_revision = revisions.is_some_and(|entries| {
                    entries.iter().any(|entry| {
                        entry
                            .get("declaration_state")
                            .and_then(serde_json::Value::as_str)
                            == Some("superseded")
                    })
                });
                let dependency_change =
                    ["dependencies", "dependency_removals"].iter().any(|field| {
                        arguments
                            .get(*field)
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|entries| !entries.is_empty())
                    });
                accepted_patches += 1;
                accepted_addition |= additions;
                accepted_active_revision |= active_revision;
                accepted_retired_revision |= retired_revision;
                accepted_cancelled_revision |= cancelled_revision;
                accepted_superseded_revision |= superseded_revision;
                accepted_dependency_change |= dependency_change;
                let required_atomic_retirement = retired_revision
                    && (!*require_cancelled_revision || cancelled_revision)
                    && (!*require_superseded_revision || superseded_revision);
                accepted_atomic_retire_and_add |= additions && required_atomic_retirement;
            }
            let missing = [
                (*require_addition && !accepted_addition).then_some("addition"),
                (*require_active_revision && !accepted_active_revision)
                    .then_some("active revision"),
                (*require_retired_revision && !accepted_retired_revision)
                    .then_some("retired revision"),
                (*require_cancelled_revision && !accepted_cancelled_revision)
                    .then_some("cancelled revision"),
                (*require_superseded_revision && !accepted_superseded_revision)
                    .then_some("superseded revision"),
                (*require_dependency_change && !accepted_dependency_change)
                    .then_some("dependency change"),
                (*require_atomic_retire_and_add && !accepted_atomic_retire_and_add)
                    .then_some("atomic retire-and-add proposal"),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
            let passed = work_established
                && accepted_patches > 0
                && missing.is_empty()
                && !integrity_failure;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!(
                        "{accepted_patches} accepted Work graph patch(es) collectively satisfied the typed mutation contract"
                    )
                } else if integrity_failure {
                    first_failure.unwrap_or_else(|| {
                        "Work graph patch evidence failed an integrity check".to_string()
                    })
                } else if !missing.is_empty() {
                    format!(
                        "accepted Work graph history omitted required mutation(s): {}",
                        missing.join(", ")
                    )
                } else {
                    first_failure.unwrap_or_else(|| {
                        "no accepted Work graph patch was recorded after Work establishment".into()
                    })
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalArtifactConsumed { producer, consumer } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_artifact_consumed");
            };
            let calls = session.journal_tool_calls();
            let mut advertised = std::collections::HashSet::new();
            let mut matched = None;
            for call in calls {
                if call.name == *consumer
                    && let Some(artifact) = call
                        .arguments
                        .as_ref()
                        .and_then(|arguments| arguments.get("artifact"))
                        .and_then(|artifact| artifact.as_str())
                    && advertised.contains(artifact)
                {
                    matched = Some(artifact.to_string());
                    break;
                }
                if call.name == *producer
                    && let Some(result) = call.result.as_ref()
                {
                    collect_session_artifact_handles(result, &mut advertised);
                }
            }
            let passed = matched.is_some();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: matched.map_or_else(
                    || {
                        format!(
                            "journal consumer {consumer} did not use a session artifact advertised by prior producer {producer}"
                        )
                    },
                    |artifact| {
                        format!(
                            "journal consumer {consumer} used producer {producer} artifact {artifact}"
                        )
                    },
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolValueFlow {
            producer,
            producer_document,
            producer_path,
            producer_filter,
            consumer,
            consumer_document,
            consumer_paths,
            consumer_filter,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_value_flow");
            };
            let mut produced = Vec::<serde_json::Value>::new();
            let mut matched = None;
            for call in session.journal_tool_calls() {
                if call.ok != Some(true) {
                    continue;
                }
                if call.name == *consumer && call_matches_predicate(&call, consumer_filter.as_ref())
                {
                    let consumer_document = match consumer_document {
                        JournalToolDocument::Arguments => call.arguments.as_ref(),
                        JournalToolDocument::Result => call.result.as_ref(),
                    };
                    let matched_value = consumer_document.and_then(|document| {
                        produced.iter().find(|value| {
                            consumer_paths.iter().any(|path| {
                                flow_destinations_at_path(document, path)
                                    .into_iter()
                                    .any(|destination| value_flows_into(value, destination))
                            })
                        })
                    });
                    if let Some(value) = matched_value {
                        matched = Some(value.clone());
                        break;
                    }
                }
                if call.name == *producer && call_matches_predicate(&call, producer_filter.as_ref())
                {
                    let producer_value = match producer_document {
                        JournalToolDocument::Arguments => call.arguments.as_ref(),
                        JournalToolDocument::Result => call.result.as_ref(),
                    }
                    .and_then(|document| document.pointer(producer_path));
                    if let Some(value) = producer_value.filter(|value| is_flow_scalar(value)) {
                        produced.push(value.clone());
                    }
                }
            }
            let passed = matched.is_some();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: matched.map_or_else(
                    || format!(
                        "journal consumer {consumer} {consumer_document:?} paths {consumer_paths:?} did not use a scalar emitted by prior producer {producer} {producer_document:?} {producer_path:?}"
                    ),
                    |value| format!(
                        "journal consumer {consumer} used prior producer {producer} value {value}"
                    ),
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::JournalToolValueFlowBound {
            producer,
            producer_document,
            producer_path,
            producer_filters,
            consumer,
            consumer_document,
            consumer_paths,
            consumer_filters,
        } => {
            let Some(session) = session else {
                return missing_required_session(c, "journal_tool_value_flow_bound");
            };
            let mut produced = Vec::<serde_json::Value>::new();
            let mut matched = None;
            for call in session.journal_tool_calls() {
                if call.ok != Some(true) {
                    continue;
                }
                if call.name == *consumer && call_matches_predicates(&call, consumer_filters) {
                    let consumer_document = match consumer_document {
                        JournalToolDocument::Arguments => call.arguments.as_ref(),
                        JournalToolDocument::Result => call.result.as_ref(),
                    };
                    let matched_value = consumer_document.and_then(|document| {
                        produced.iter().find(|value| {
                            consumer_paths.iter().any(|path| {
                                flow_destinations_at_path(document, path)
                                    .into_iter()
                                    .any(|destination| value_flows_into(value, destination))
                            })
                        })
                    });
                    if let Some(value) = matched_value {
                        matched = Some(value.clone());
                        break;
                    }
                }
                if call.name == *producer && call_matches_predicates(&call, producer_filters) {
                    let producer_value = match producer_document {
                        JournalToolDocument::Arguments => call.arguments.as_ref(),
                        JournalToolDocument::Result => call.result.as_ref(),
                    }
                    .and_then(|document| document.pointer(producer_path));
                    if let Some(value) = producer_value.filter(|value| is_flow_scalar(value)) {
                        produced.push(value.clone());
                    }
                }
            }
            let passed = matched.is_some();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: matched.map_or_else(
                    || format!(
                        "bounded journal consumer {consumer} {consumer_document:?} paths {consumer_paths:?} did not use a scalar emitted by prior producer {producer}"
                    ),
                    |value| format!(
                        "bounded journal consumer {consumer} used prior producer {producer} value {value}"
                    ),
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::ForkCacheOutcome { expect } => {
            let hits = parse_fork_cache_outcomes(&outcome.stderr);
            let pass = hits.iter().any(|c| expect.iter().any(|e| e == c));
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: pass,
                detail: if pass {
                    format!(
                        "fork-cache event with outcome in {expect:?} observed (all seen: {hits:?})"
                    )
                } else if hits.is_empty() {
                    "no [fork-cache] events observed in stderr".to_string()
                } else {
                    format!("no [fork-cache] event matched {expect:?}; seen outcomes: {hits:?}")
                },
                full_detail: None,
                score: None,
            }
        }
        Criterion::Judger { .. } | Criterion::HardJudger { .. } => CriterionResult {
            criterion: c.clone(),
            severity: criterion_severity(c),
            passed: false,
            detail: "judger not yet evaluated (handled by runner)".into(),
            full_detail: None,
            score: None,
        },

        Criterion::TokensBetween { min, max } => {
            let total = astra_turn_types::NormalizedPromptCacheUsage::new(
                outcome.prompt_tokens,
                outcome.cached_input_tokens,
                outcome.cache_creation_tokens,
            )
            .total_tokens_with_output(outcome.completion_tokens);
            let passed = total >= *min && total <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!("tokens_total={total}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }

        Criterion::DurationBetween { min_ms, max_ms } => {
            let dur = outcome.duration_ms;
            let passed = dur >= *min_ms && dur <= *max_ms;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!("duration={dur}ms, expected {min_ms}..={max_ms}ms"),
                full_detail: None,
                score: None,
            }
        }

        Criterion::ToolSequence { tools } => {
            // Check if `tools` is an ordered subsequence of `outcome.tools_used`.
            let mut iter = outcome.tools_used.iter();
            let mut matched = 0;
            for expected in tools {
                if iter.any(|t| t == expected) {
                    matched += 1;
                }
            }
            let passed = matched == tools.len();
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: if passed {
                    format!("tool sequence {:?} found", tools)
                } else {
                    format!(
                        "tool sequence {:?} NOT found (matched {}/{}, actual: {:?})",
                        tools,
                        matched,
                        tools.len(),
                        outcome.tools_used
                    )
                },
                full_detail: None,
                score: None,
            }
        }

        Criterion::TurnRoundsBetween { min, max } => {
            let rounds = outcome.turn_rounds;
            let passed = rounds >= *min && rounds <= *max;
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!("turn_rounds={rounds}, expected {min}..={max}"),
                full_detail: None,
                score: None,
            }
        }

        Criterion::CacheRateAbove {
            threshold,
            min_calls,
        } => {
            let effective_calls = if outcome.total_tool_calls > 0 {
                outcome.total_tool_calls
            } else {
                outcome.tool_calls_count
            };
            if effective_calls < *min_calls {
                return CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!(
                        "too few tool calls: {effective_calls} < min_calls={min_calls} \
                         (cache rate requires at least {min_calls} calls)"
                    ),
                    full_detail: None,
                    score: None,
                };
            }
            if outcome.total_tool_calls == 0 && outcome.tool_calls_count == 0 {
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: true,
                    detail: "no tool calls — cache rate N/A (skip-pass)".into(),
                    full_detail: None,
                    score: None,
                }
            } else if outcome.total_tool_calls == 0 {
                // tool_calls_count > 0 but step_events weren't parsed.
                // This means step_events.jsonl was missing or unreadable.
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail: format!(
                        "step_events missing: tool_calls_count={} but total_tool_calls=0 \
                         (cannot compute cache rate)",
                        outcome.tool_calls_count
                    ),
                    full_detail: None,
                    score: None,
                }
            } else {
                let rate = outcome.cache_hits as f64 / outcome.total_tool_calls as f64;
                let passed = rate >= *threshold;
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail: format!(
                        "cache_rate={:.1}% ({}/{}), threshold={:.0}%",
                        rate * 100.0,
                        outcome.cache_hits,
                        outcome.total_tool_calls,
                        threshold * 100.0
                    ),
                    full_detail: None,
                    score: None,
                }
            }
        }
        Criterion::PromptCacheTokens {
            min_read,
            min_creation,
            max_creation,
        } => {
            let read_ok = outcome.cached_input_tokens >= *min_read;
            let creation_floor_ok = outcome.cache_creation_tokens >= *min_creation;
            let creation_ceiling_ok = match max_creation {
                Some(max) => outcome.cache_creation_tokens <= *max,
                None => true,
            };
            let passed = read_ok && creation_floor_ok && creation_ceiling_ok;
            let ceiling_desc = match max_creation {
                Some(max) => format!(", creation<={max}"),
                None => String::new(),
            };
            CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed,
                detail: format!(
                    "prompt_cache read={} creation={}, expected read>={} creation>={}{}",
                    outcome.cached_input_tokens,
                    outcome.cache_creation_tokens,
                    min_read,
                    min_creation,
                    ceiling_desc
                ),
                full_detail: None,
                score: None,
            }
        }
        Criterion::PromptCacheReuseScope { scope } => {
            evaluate_prompt_cache_reuse_scope(c, *scope, session)
        }
        Criterion::ProviderPromptCacheReadRatio {
            min,
            warmup_turns,
            warmup_rounds,
        } => match session {
            None => missing_required_session(c, "provider prompt-cache read ratio"),
            Some(capture) => {
                let (usages, warmup, unit) = if *warmup_rounds > 0 {
                    (
                        provider_prompt_cache_round_usages(capture),
                        *warmup_rounds as usize,
                        "round",
                    )
                } else {
                    (
                        provider_prompt_cache_usages(capture),
                        *warmup_turns as usize,
                        "turn",
                    )
                };
                let measured = usages.iter().skip(warmup);
                let mut fresh = 0_u128;
                let mut read = 0_u128;
                let mut creation = 0_u128;
                let mut observations = 0_usize;
                for usage in measured {
                    fresh += u128::from(usage.fresh);
                    read += u128::from(usage.read);
                    creation += u128::from(usage.creation);
                    observations += 1;
                }
                let input = fresh + read + creation;
                let count_label = if unit == "turn" { "turns" } else { "rounds" };
                if observations == 0 || input == 0 {
                    CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        passed: false,
                        detail: format!(
                            "no provider usage after {} warmup {unit}(s) (usage observations={})",
                            warmup,
                            usages.len()
                        ),
                        full_detail: None,
                        score: None,
                    }
                } else {
                    let ratio = read as f64 / input as f64;
                    CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        // Token counts are exact integers; tolerate only the
                        // representational epsilon at an exact decimal
                        // boundary such as 9_800 / 10_000 == 0.98.
                        passed: ratio >= *min || (ratio - *min).abs() <= 1e-12,
                        detail: format!(
                            "provider_prompt_cache_read_ratio={:.2}% \
                             (read={read}, fresh={fresh}, creation={creation}, {count_label}={observations}, \
                             warmup_{unit}={warmup}), expected >= {:.2}%",
                            ratio * 100.0,
                            min * 100.0
                        ),
                        full_detail: None,
                        score: None,
                    }
                }
            }
        },
        Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min,
            min_pairs,
            max_identity_transitions_per_run,
        } => match session {
            None => missing_required_session(c, "stable-prefix provider cache reuse"),
            Some(capture) => match assess_stable_prefix_cache_reuse(capture) {
                Err(detail) => CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: false,
                    detail,
                    full_detail: None,
                    score: None,
                },
                Ok(assessment) => {
                    let enough_pairs = assessment.scored_pairs >= *min_pairs
                        && assessment
                            .minimum_pairs_per_multi_observation_run
                            .is_some_and(|pairs| pairs >= *min_pairs);
                    let transitions_ok = assessment.max_identity_transitions_per_run
                        <= *max_identity_transitions_per_run;
                    let ratio_ok = assessment
                        .worst_ratio
                        .is_some_and(|ratio| ratio >= *min || (ratio - *min).abs() <= 1e-12);
                    CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        passed: enough_pairs && transitions_ok && ratio_ok,
                        detail: format!(
                            "stable_prefix_cache_reuse worst={:.2}%, pairs={}, min_pairs_per_run={}, max_transitions_per_run={}, expected >= {:.2}%, pairs/run>={}, transitions/run<={}",
                            assessment.worst_ratio.unwrap_or_default() * 100.0,
                            assessment.scored_pairs,
                            assessment
                                .minimum_pairs_per_multi_observation_run
                                .unwrap_or_default(),
                            assessment.max_identity_transitions_per_run,
                            min * 100.0,
                            min_pairs,
                            max_identity_transitions_per_run,
                        ),
                        full_detail: None,
                        score: None,
                    }
                }
            },
        },
        Criterion::PipelineAlertCount {
            rule,
            max,
            optional,
        } => match session {
            None if *optional => CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: true,
                detail: "session unavailable — skipped pipeline alert check".into(),
                full_detail: None,
                score: None,
            },
            None => CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: false,
                detail: "session unavailable — cannot inspect pipeline alerts".into(),
                full_detail: None,
                score: None,
            },
            Some(capture) => {
                let report = analyze_pipeline_health(capture);
                if report.invalid_events > 0 {
                    if *optional {
                        return CriterionResult {
                            criterion: c.clone(),
                            severity: criterion_severity(c),
                            passed: true,
                            detail: format!(
                                "pipeline evidence incomplete: {} invalid event payload(s); optional criterion skipped",
                                report.invalid_events
                            ),
                            full_detail: None,
                            score: Some(1.0),
                        };
                    }
                    return CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        passed: false,
                        detail: format!(
                            "pipeline evidence incomplete: {} invalid event payload(s)",
                            report.invalid_events
                        ),
                        full_detail: None,
                        score: None,
                    };
                }
                let count = report
                    .alerts
                    .iter()
                    .filter(|alert| alert.rule == *rule)
                    .count() as u32;
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed: count <= *max,
                    detail: format!("pipeline_alert[{rule}]={count}, expected <= {max}"),
                    full_detail: None,
                    score: None,
                }
            }
        },
        Criterion::PipelineAvgCacheHitRatio { min, optional } => match session {
            None if *optional => CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: true,
                detail: "session unavailable — skipped pipeline cache ratio check".into(),
                full_detail: None,
                score: None,
            },
            None => CriterionResult {
                criterion: c.clone(),
                severity: criterion_severity(c),
                passed: false,
                detail: "session unavailable — cannot inspect pipeline cache ratio".into(),
                full_detail: None,
                score: None,
            },
            Some(capture) => {
                let report = analyze_pipeline_health(capture);
                if report.invalid_events > 0 {
                    if *optional {
                        return CriterionResult {
                            criterion: c.clone(),
                            severity: criterion_severity(c),
                            passed: true,
                            detail: format!(
                                "pipeline evidence incomplete: {} invalid event payload(s); optional criterion skipped",
                                report.invalid_events
                            ),
                            full_detail: None,
                            score: Some(1.0),
                        };
                    }
                    return CriterionResult {
                        criterion: c.clone(),
                        severity: criterion_severity(c),
                        passed: false,
                        detail: format!(
                            "pipeline evidence incomplete: {} invalid event payload(s)",
                            report.invalid_events
                        ),
                        full_detail: None,
                        score: Some(0.0),
                    };
                }
                if report.turns_with_feedback == 0 {
                    return if *optional {
                        CriterionResult {
                            criterion: c.clone(),
                            severity: criterion_severity(c),
                            passed: true,
                            detail:
                                "pipeline cache ratio skipped (optional + no pipeline feedback turns)"
                                    .into(),
                            full_detail: None,
                            score: None,
                        }
                    } else {
                        CriterionResult {
                            criterion: c.clone(),
                            severity: criterion_severity(c),
                            passed: false,
                            detail:
                                "no pipeline feedback turns available — cannot evaluate cache ratio"
                                    .into(),
                            full_detail: None,
                            score: None,
                        }
                    };
                }
                let passed = report.turns_with_feedback > 0 && report.avg_cache_hit_ratio >= *min;
                CriterionResult {
                    criterion: c.clone(),
                    severity: criterion_severity(c),
                    passed,
                    detail: format!(
                        "pipeline_avg_cache_hit_ratio={:.1}%, expected >= {:.1}%",
                        report.avg_cache_hit_ratio * 100.0,
                        min * 100.0
                    ),
                    full_detail: None,
                    score: None,
                }
            }
        },
    }
}

fn criterion_allows_incomplete_capture(c: &Criterion) -> bool {
    matches!(
        c,
        Criterion::SessionEventCount { optional: true, .. }
            | Criterion::JournalToolCalled { optional: true, .. }
            | Criterion::PipelineAlertCount { optional: true, .. }
            | Criterion::PipelineAvgCacheHitRatio { optional: true, .. }
    )
}

#[derive(Debug, Clone, Copy)]
struct ProviderPromptCacheUsage {
    fresh: u64,
    read: u64,
    creation: u64,
}

#[derive(Debug, Clone, Copy)]
struct StablePrefixCacheAssessment {
    scored_pairs: u32,
    minimum_pairs_per_multi_observation_run: Option<u32>,
    max_identity_transitions_per_run: u32,
    worst_ratio: Option<f64>,
}

fn assess_stable_prefix_cache_reuse(
    capture: &SessionCapture,
) -> Result<StablePrefixCacheAssessment, String> {
    use astra_turn_core::pipeline_journal::{PipelineEventKind, PipelineJournalEvent};

    type ObservationKey = (String, String, u32, u32);
    let mut observations = std::collections::BTreeMap::<
        ObservationKey,
        astra_turn_core::context_feedback::RuntimeFeedbackFrame,
    >::new();
    let mut feedback_events = 0_u32;

    for event in capture
        .events
        .iter()
        .filter(|event| event.event_type == "pipeline_feedback")
    {
        feedback_events = feedback_events.saturating_add(1);
        let metadata = event
            .raw
            .get("metadata")
            .ok_or_else(|| "stable-prefix cache evidence missing typed metadata".to_string())?;
        let feedback: PipelineJournalEvent = serde_json::from_value(metadata.clone())
            .map_err(|_| "stable-prefix cache evidence has invalid typed metadata".to_string())?;
        let frame = feedback.runtime_feedback.ok_or_else(|| {
            "stable-prefix cache evidence missing canonical runtime feedback".to_string()
        })?;
        let outer_turn = event.raw.get("turn").and_then(serde_json::Value::as_u64);
        if feedback.kind != PipelineEventKind::Feedback
            || feedback.turn != frame.progress.session_turn
            || outer_turn != Some(u64::from(frame.progress.session_turn))
            || !frame.is_valid()
        {
            return Err("stable-prefix cache evidence failed typed identity validation".into());
        }
        if frame.context.prompt_cache_identity.is_none() || frame.request_usage.is_none() {
            return Err(
                "stable-prefix cache evidence requires prompt_cache_identity and request_usage"
                    .into(),
            );
        }
        let key = (
            frame.identity.session_id.clone(),
            frame.identity.run_id.clone(),
            frame.progress.session_turn,
            frame.progress.llm_rounds_completed,
        );
        if let Some(existing) = observations.get(&key) {
            if existing != &frame {
                return Err(
                    "stable-prefix cache evidence contains conflicting mirrored observations"
                        .into(),
                );
            }
        } else {
            observations.insert(key, frame);
        }
    }

    if feedback_events == 0 {
        return Err("no canonical pipeline feedback for stable-prefix cache reuse".into());
    }

    let mut runs = std::collections::BTreeMap::<
        (String, String),
        Vec<astra_turn_core::context_feedback::RuntimeFeedbackFrame>,
    >::new();
    for ((session_id, run_id, _, _), frame) in observations {
        runs.entry((session_id, run_id)).or_default().push(frame);
    }

    let mut scored_pairs = 0_u32;
    let mut minimum_pairs_per_multi_observation_run: Option<u32> = None;
    let mut max_identity_transitions_per_run = 0_u32;
    let mut worst_ratio: Option<f64> = None;
    for frames in runs.values_mut() {
        frames.sort_by_key(|frame| {
            (
                frame.progress.session_turn,
                frame.progress.llm_rounds_completed,
            )
        });
        let mut run_pairs = 0_u32;
        let mut run_identity_transitions = 0_u32;
        // A zero cache-read observation can establish the single cold
        // anchor for an identity epoch.  Once that anchor has been consumed,
        // another zero is evidence of a reuse failure, not another boundary.
        let mut skipped_cold_boundary = false;
        for pair in frames.windows(2) {
            let previous = &pair[0];
            let current = &pair[1];
            if previous.context.prompt_cache_identity != current.context.prompt_cache_identity {
                run_identity_transitions = run_identity_transitions.saturating_add(1);
                skipped_cold_boundary = false;
                continue;
            }
            let previous_cached_prefix = previous
                .request_usage
                .expect("validated request usage")
                .cache_read;
            // The first observation after a cold boundary establishes the
            // provider's cached-prefix anchor but is not itself a reuse pair.
            // Dynamic prompt/tool-result tails are intentionally excluded from
            // this ratio; they are not part of the typed stable identity.
            // Only that one boundary is exempt: a later zero must remain a
            // scored 0% observation and cannot be erased by later recovery.
            if previous_cached_prefix == 0 && !skipped_cold_boundary {
                skipped_cold_boundary = true;
                continue;
            }
            let current_cached_prefix = current
                .request_usage
                .expect("validated request usage")
                .cache_read;
            let ratio = if previous_cached_prefix == 0 {
                0.0
            } else {
                current_cached_prefix as f64 / previous_cached_prefix as f64
            };
            scored_pairs = scored_pairs.saturating_add(1);
            run_pairs = run_pairs.saturating_add(1);
            worst_ratio = Some(worst_ratio.map_or(ratio, |worst| worst.min(ratio)));
        }
        if frames.len() >= 2 {
            minimum_pairs_per_multi_observation_run = Some(
                minimum_pairs_per_multi_observation_run
                    .map_or(run_pairs, |minimum| minimum.min(run_pairs)),
            );
        }
        max_identity_transitions_per_run =
            max_identity_transitions_per_run.max(run_identity_transitions);
    }

    Ok(StablePrefixCacheAssessment {
        scored_pairs,
        minimum_pairs_per_multi_observation_run,
        max_identity_transitions_per_run,
        worst_ratio,
    })
}

fn provider_cache_ratio_cmp(
    left: ProviderPromptCacheUsage,
    right: ProviderPromptCacheUsage,
) -> std::cmp::Ordering {
    let left_total = u128::from(left.fresh) + u128::from(left.read) + u128::from(left.creation);
    let right_total = u128::from(right.fresh) + u128::from(right.read) + u128::from(right.creation);
    (u128::from(left.read) * right_total).cmp(&(u128::from(right.read) * left_total))
}

fn evaluate_prompt_cache_reuse_scope(
    criterion: &Criterion,
    scope: PromptCacheReuseScope,
    session: Option<&SessionCapture>,
) -> CriterionResult {
    let Some(capture) = session else {
        return missing_required_session(criterion, "required prompt-cache reuse scope");
    };

    let (observations, reuse_reads, passed, boundary) = match scope {
        PromptCacheReuseScope::ConversationTurns => {
            let Some(usages) = required_conversation_prompt_cache_usages(capture) else {
                return CriterionResult {
                    criterion: criterion.clone(),
                    severity: criterion_severity(criterion),
                    passed: false,
                    detail: "required prompt-cache scope=conversation_turns: FAILED: every provider usage observation must carry a typed turn identity".into(),
                    full_detail: None,
                    score: None,
                };
            };
            let reuse_reads: u64 = usages.iter().skip(1).map(|usage| usage.read).sum();
            let observations = usages.len();
            (
                observations,
                reuse_reads,
                observations >= 2 && reuse_reads > 0,
                "conversation_turns",
            )
        }
        PromptCacheReuseScope::IntraTurnRounds => {
            let Some(groups) = required_intra_turn_rounds(capture) else {
                return CriterionResult {
                    criterion: criterion.clone(),
                    severity: criterion_severity(criterion),
                    passed: false,
                    detail: "required prompt-cache scope=intra_turn_rounds: FAILED: every provider round must carry typed turn, round, and producer run identities".into(),
                    full_detail: None,
                    score: None,
                };
            };
            let observations = groups.iter().map(Vec::len).max().unwrap_or(0);
            let reuse_reads = groups
                .iter()
                .filter(|rounds| rounds.len() >= 2)
                .map(|rounds| rounds.iter().skip(1).map(|usage| usage.read).sum::<u64>())
                .max()
                .unwrap_or(0);
            (
                observations,
                reuse_reads,
                observations >= 2 && reuse_reads > 0,
                "intra_turn_rounds",
            )
        }
    };
    CriterionResult {
        criterion: criterion.clone(),
        severity: criterion_severity(criterion),
        passed,
        detail: format!(
            "required prompt-cache scope={boundary}: observations={observations}, \
             post-cold cache_read_tokens={reuse_reads}, expected >=2 observations and read>0"
        ),
        full_detail: None,
        score: None,
    }
}

fn provider_usage_from_event(
    event: &crate::session_capture::JournalEvent,
) -> Option<ProviderPromptCacheUsage> {
    let fresh = event.raw.get("tokens_in").and_then(|value| value.as_u64());
    let read = event
        .raw
        .get("cache_read_tokens")
        .and_then(|value| value.as_u64());
    let creation = event
        .raw
        .get("cache_creation_tokens")
        .and_then(|value| value.as_u64());
    if fresh.is_none() && read.is_none() && creation.is_none() {
        return None;
    }
    let usage = ProviderPromptCacheUsage {
        fresh: fresh.unwrap_or_default(),
        read: read.unwrap_or_default(),
        creation: creation.unwrap_or_default(),
    };
    (usage.fresh != 0 || usage.read != 0 || usage.creation != 0).then_some(usage)
}

/// Strict conversation-boundary evidence. Unlike the advisory cache-ratio
/// extractor, this path refuses identity-less observations and collapses
/// mirrored records by their typed turn id. A duplicate turn can therefore
/// never manufacture a second cold/warm observation.
fn required_conversation_prompt_cache_usages(
    capture: &SessionCapture,
) -> Option<Vec<ProviderPromptCacheUsage>> {
    let canonical = capture
        .events
        .iter()
        .any(|event| event.event_type == "turn" && provider_usage_from_event(event).is_some());
    let event_type = if canonical { "turn" } else { "llm_round" };
    let mut usages = Vec::new();
    let mut turn_positions = std::collections::HashMap::<u64, usize>::new();
    for event in capture
        .events
        .iter()
        .filter(|event| event.event_type == event_type)
    {
        let Some(usage) = provider_usage_from_event(event) else {
            continue;
        };
        let turn = event.raw.get("turn").and_then(|value| value.as_u64())?;
        if let Some(index) = turn_positions.get(&turn).copied() {
            // Mirrored records for one turn are one observation. Keep the
            // lower ratio so disagreement cannot create a false pass.
            if provider_cache_ratio_cmp(usage, usages[index]).is_lt() {
                usages[index] = usage;
            }
        } else {
            turn_positions.insert(turn, usages.len());
            usages.push(usage);
        }
    }
    Some(usages)
}

/// Strict intra-turn evidence. Every provider round must have a typed
/// `(turn, round, producer_scope.run_id)` identity. Records are grouped by
/// the same `(turn, run_id)` so two rounds from different turns/runs cannot
/// be mistaken for one intra-turn reuse boundary; mirrored identities are
/// deduplicated conservatively.
fn required_intra_turn_rounds(
    capture: &SessionCapture,
) -> Option<Vec<Vec<ProviderPromptCacheUsage>>> {
    let mut groups = std::collections::HashMap::<
        (u64, String),
        std::collections::HashMap<u64, ProviderPromptCacheUsage>,
    >::new();
    for event in capture
        .events
        .iter()
        .filter(|event| event.event_type == "llm_round")
    {
        let Some(usage) = provider_usage_from_event(event) else {
            continue;
        };
        let turn = event.raw.get("turn").and_then(|value| value.as_u64())?;
        let round = event.raw.get("round").and_then(|value| value.as_u64())?;
        let run_id = event
            .raw
            .pointer("/producer_scope/run_id")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())?
            .to_owned();
        let rounds = groups.entry((turn, run_id)).or_default();
        if let Some(existing) = rounds.get_mut(&round) {
            if provider_cache_ratio_cmp(usage, *existing).is_lt() {
                *existing = usage;
            }
        } else {
            rounds.insert(round, usage);
        }
    }
    Some(
        groups
            .into_values()
            .map(|rounds| {
                let mut rounds: Vec<_> = rounds.into_iter().collect();
                rounds.sort_by_key(|(round, _)| *round);
                rounds.into_iter().map(|(_, usage)| usage).collect()
            })
            .collect(),
    )
}

/// Extract provider usage observations in journal order. Canonical `turn`
/// records are authoritative; legacy `llm_round` records are considered only
/// when the session has no canonical usage records, avoiding double-counting
/// sessions that contain both schemas.
fn provider_prompt_cache_usages(capture: &SessionCapture) -> Vec<ProviderPromptCacheUsage> {
    fn extract(
        capture: &SessionCapture,
        event_type: &str,
        aggregate_rounds: bool,
    ) -> Vec<ProviderPromptCacheUsage> {
        let mut usages = Vec::<ProviderPromptCacheUsage>::new();
        let mut turn_positions = std::collections::HashMap::<u64, usize>::new();
        for event in capture
            .events
            .iter()
            .filter(|event| event.event_type == event_type)
        {
            let fresh = event.raw.get("tokens_in").and_then(|value| value.as_u64());
            let read = event
                .raw
                .get("cache_read_tokens")
                .and_then(|value| value.as_u64());
            let creation = event
                .raw
                .get("cache_creation_tokens")
                .and_then(|value| value.as_u64());
            if fresh.is_none() && read.is_none() && creation.is_none() {
                continue;
            }
            let usage = ProviderPromptCacheUsage {
                fresh: fresh.unwrap_or_default(),
                read: read.unwrap_or_default(),
                creation: creation.unwrap_or_default(),
            };
            if usage.fresh == 0 && usage.read == 0 && usage.creation == 0 {
                continue;
            }

            let Some(turn) = event.raw.get("turn").and_then(|value| value.as_u64()) else {
                usages.push(usage);
                continue;
            };
            if let Some(index) = turn_positions.get(&turn).copied() {
                if aggregate_rounds {
                    let current = &mut usages[index];
                    current.fresh = current.fresh.saturating_add(usage.fresh);
                    current.read = current.read.saturating_add(usage.read);
                    current.creation = current.creation.saturating_add(usage.creation);
                } else if provider_cache_ratio_cmp(usage, usages[index]).is_lt() {
                    // Mirrored canonical journals should agree. If they do
                    // not, retain the lower cache-read ratio so a path-order
                    // change cannot turn conflicting evidence into a false
                    // pass.
                    usages[index] = usage;
                }
                continue;
            }
            turn_positions.insert(turn, usages.len());
            usages.push(usage);
        }
        usages
    }

    // Canonical turn records already aggregate every LLM round. Mirrored
    // records for one turn are de-duplicated; disagreement is resolved
    // conservatively and independently of artifact path ordering.
    let canonical = extract(capture, "turn", false);
    if canonical.is_empty() {
        // Legacy journals expose one usage record per LLM round. Aggregate
        // those by user turn so `warmup_turns` never removes only half a turn.
        extract(capture, "llm_round", true)
    } else {
        canonical
    }
}

/// Extract provider-boundary usage from detailed `llm_round` records.
///
/// A canonical `turn` event intentionally aggregates all model calls in one
/// user turn, which is the right source for `warmup_turns` but cannot express
/// "skip the first cold provider call" for a one-turn agentic journey.  This
/// explicit round-level path is only selected by `warmup_rounds`; it never
/// silently changes the meaning of the turn-level criterion.
fn provider_prompt_cache_round_usages(capture: &SessionCapture) -> Vec<ProviderPromptCacheUsage> {
    let mut usages = Vec::new();
    let mut seen = std::collections::HashMap::<String, usize>::new();

    for event in capture
        .events
        .iter()
        .filter(|event| event.event_type == "llm_round")
    {
        let fresh = event.raw.get("tokens_in").and_then(|value| value.as_u64());
        let read = event
            .raw
            .get("cache_read_tokens")
            .and_then(|value| value.as_u64());
        let creation = event
            .raw
            .get("cache_creation_tokens")
            .and_then(|value| value.as_u64());
        let usage = ProviderPromptCacheUsage {
            fresh: fresh.unwrap_or_default(),
            read: read.unwrap_or_default(),
            creation: creation.unwrap_or_default(),
        };
        if fresh.is_none() && read.is_none() && creation.is_none()
            || (usage.fresh == 0 && usage.read == 0 && usage.creation == 0)
        {
            continue;
        }

        // Server/account journals can be mirrored into more than one owner
        // artifact. Deduplicate only when the runtime supplied a stable
        // provider-boundary identity; records without one remain observable
        // instead of being guessed together by position or text.
        if let (Some(turn), Some(round), Some(run_id)) = (
            event.raw.get("turn"),
            event.raw.get("round"),
            event
                .raw
                .pointer("/producer_scope/run_id")
                .and_then(|value| value.as_str()),
        ) {
            let identity = serde_json::json!({
                "turn": turn,
                "round": round,
                "run_id": run_id,
                "agentic_step": event.raw.get("agentic_step"),
            });
            if let Ok(identity) = serde_json::to_string(&identity) {
                if let Some(index) = seen.get(&identity).copied() {
                    // Mirrored provider-boundary records should agree. On a
                    // conflict retain the lower cache-read ratio so artifact
                    // ordering cannot turn inconsistent evidence into a
                    // false pass.
                    if provider_cache_ratio_cmp(usage, usages[index]).is_lt() {
                        usages[index] = usage;
                    }
                    continue;
                }
                seen.insert(identity, usages.len());
            }
        }
        usages.push(usage);
    }
    usages
}

/// Scan stderr for `[fork-cache] {...}` JSON lines and return the
/// `outcome` field from each. Silently skips malformed lines and
/// lines where the outcome can't be found — a single corrupt event
/// should not hide the valid ones.
///
/// Field precedence: `outcome` (current wire name as serialized by
/// `astra_turn_core::fork_cache_event::ForkCacheEvent`) then `class`
/// (earlier harness-facing name; kept for YAML backward-compat). No
/// positional / first-key fallback — that was a schema-churn footgun
/// that would misclassify a re-tagged event.
fn parse_fork_cache_outcomes(stderr: &str) -> Vec<String> {
    let mut outcomes = Vec::new();
    for line in stderr.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("[fork-cache]") else {
            continue;
        };
        let rest = rest.trim_start();
        let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) else {
            continue;
        };
        // Only accept the two named fields. Unknown shapes are
        // skipped entirely — loud missing-outcome surface is
        // preferable to a silent misclassification.
        let name = v
            .get("outcome")
            .and_then(|c| c.as_str())
            .or_else(|| v.get("class").and_then(|c| c.as_str()));
        if let Some(s) = name {
            outcomes.push(s.to_string());
        }
    }
    outcomes
}

/// Reject a criterion whose bounds are internally inconsistent —
/// typos in YAML (`min: 5, max: 2`, `threshold: 2.0`, empty expect
/// list) otherwise turn into permanent-FAIL or permanent-PASS cases
/// that look like real bugs. Return `Err` with the precise field
/// name so the case author sees exactly what to change.
///
/// Called by `Case::from_path` at load time so the whole suite fails
/// fast on a typo rather than at runtime.
const MAX_COMPOSITE_DEPTH: usize = 4;

fn validate_json_pointer(label: &str, path: &str) -> Result<(), String> {
    // RFC 6901 defines the empty string as the pointer to the whole document.
    if !path.is_empty() && !path.starts_with('/') {
        return Err(format!(
            "{label} must be an RFC 6901 JSON pointer; got {path:?}"
        ));
    }
    let bytes = path.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] == b'~' && !matches!(bytes.get(index + 1), Some(b'0' | b'1')) {
            return Err(format!(
                "{label} contains an invalid RFC 6901 escape at byte {index}; only ~0 and ~1 are valid"
            ));
        }
    }
    Ok(())
}

pub fn validate_criterion(c: &Criterion) -> Result<(), String> {
    validate_criterion_at_depth(c, 0)
}

fn validate_criterion_at_depth(c: &Criterion, composite_depth: usize) -> Result<(), String> {
    match c {
        Criterion::ToolsCountBetween { min, max } => {
            if min > max {
                return Err(format!(
                    "ToolsCountBetween: min ({min}) > max ({max}); case will always FAIL"
                ));
            }
            Ok(())
        }
        Criterion::ProviderPromptCacheStablePrefixReuseRatio { min, min_pairs, .. } => {
            if !min.is_finite() || *min < 0.0 || *min > 1.0 {
                return Err(format!(
                    "ProviderPromptCacheStablePrefixReuseRatio.min must be finite in [0.0, 1.0]; got {min}"
                ));
            }
            if *min_pairs == 0 {
                return Err(
                    "ProviderPromptCacheStablePrefixReuseRatio.min_pairs must be >= 1".into(),
                );
            }
            Ok(())
        }
        Criterion::JournalChildToolCallCount {
            parent,
            child,
            min,
            max,
        } => {
            if parent.trim().is_empty() || child.trim().is_empty() {
                return Err("JournalChildToolCallCount tool names must not be empty".into());
            }
            if min > max {
                return Err(format!(
                    "JournalChildToolCallCount: min ({min}) > max ({max})"
                ));
            }
            Ok(())
        }
        Criterion::JournalToolSequence { tools } => {
            if tools.is_empty() {
                return Err("JournalToolSequence.tools must not be empty".into());
            }
            for (i, tool) in tools.iter().enumerate() {
                if tool.trim().is_empty() {
                    return Err(format!("JournalToolSequence.tools[{i}] must not be empty"));
                }
            }
            Ok(())
        }
        Criterion::JournalToolPrecedence {
            predecessor,
            successor,
        } => {
            if predecessor.trim().is_empty() || successor.trim().is_empty() {
                return Err("JournalToolPrecedence tool names must not be empty".into());
            }
            if predecessor == successor {
                return Err("JournalToolPrecedence requires distinct tool names".into());
            }
            Ok(())
        }
        Criterion::JournalWorkItemExecutionFromStart { min_distinct_items } => {
            if *min_distinct_items == 0 {
                return Err(
                    "JournalWorkItemExecutionFromStart.min_distinct_items must be at least 1"
                        .into(),
                );
            }
            Ok(())
        }
        Criterion::JournalWorkGraphPatch {
            require_addition,
            require_active_revision,
            require_retired_revision,
            require_cancelled_revision,
            require_superseded_revision,
            require_dependency_change,
            require_atomic_retire_and_add,
        } => {
            if !require_addition
                && !require_active_revision
                && !require_retired_revision
                && !require_cancelled_revision
                && !require_superseded_revision
                && !require_dependency_change
                && !require_atomic_retire_and_add
            {
                return Err(
                    "JournalWorkGraphPatch requires at least one mutation dimension".into(),
                );
            }
            Ok(())
        }
        Criterion::JournalToolCallCount {
            name,
            min,
            max,
            document,
            path,
            equals,
        } => {
            if name.trim().is_empty() {
                return Err("JournalToolCallCount.name must not be empty".into());
            }
            if min > max {
                return Err(format!(
                    "JournalToolCallCount: min ({min}) > max ({max}); case will always FAIL"
                ));
            }
            let predicate_fields = [document.is_some(), path.is_some(), equals.is_some()];
            if predicate_fields.iter().any(|present| *present)
                && !predicate_fields.iter().all(|present| *present)
            {
                return Err(
                    "JournalToolCallCount structural filtering requires document, path, and equals together"
                        .into(),
                );
            }
            if let Some(path) = path
                && !path.is_empty()
                && !path.starts_with('/')
            {
                return Err(
                    "JournalToolCallCount.path must be empty or a JSON pointer beginning with '/'"
                        .into(),
                );
            }
            Ok(())
        }
        Criterion::JournalToolOutcomeCount { name, min, max, .. } => {
            if name.trim().is_empty() {
                return Err("JournalToolOutcomeCount.name must not be empty".into());
            }
            if min > max {
                return Err(format!(
                    "JournalToolOutcomeCount: min ({min}) > max ({max}); case will always FAIL"
                ));
            }
            Ok(())
        }
        Criterion::JournalToolSuccessRatio {
            min,
            min_calls,
            allowed_failures,
        } => {
            if !min.is_finite() || !(0.0..=1.0).contains(min) {
                return Err(format!(
                    "JournalToolSuccessRatio.min must be finite in [0.0, 1.0]; got {min}"
                ));
            }
            if *min_calls == 0 {
                return Err("JournalToolSuccessRatio.min_calls must be >= 1".into());
            }
            if allowed_failures >= min_calls {
                return Err(
                    "JournalToolSuccessRatio.allowed_failures must be less than min_calls".into(),
                );
            }
            Ok(())
        }
        Criterion::JournalToolJson {
            name,
            path,
            document: _,
            equals: _,
        } => {
            if name.trim().is_empty() {
                return Err("JournalToolJson.name must not be empty".into());
            }
            if !path.is_empty() && !path.starts_with('/') {
                return Err(format!(
                    "JournalToolJson.path must be an RFC 6901 JSON pointer; got {path:?}"
                ));
            }
            Ok(())
        }
        Criterion::JournalToolJsonContains {
            name,
            path,
            contains,
            document: _,
        } => {
            if name.trim().is_empty() {
                return Err("JournalToolJsonContains.name must not be empty".into());
            }
            if contains.is_empty() {
                return Err("JournalToolJsonContains.contains must not be empty".into());
            }
            if !path.is_empty() && !path.starts_with('/') {
                return Err(format!(
                    "JournalToolJsonContains.path must be an RFC 6901 JSON pointer; got {path:?}"
                ));
            }
            Ok(())
        }
        Criterion::JournalArtifactConsumed { producer, consumer } => {
            if producer.trim().is_empty() || consumer.trim().is_empty() {
                return Err(
                    "JournalArtifactConsumed producer and consumer must not be empty".into(),
                );
            }
            Ok(())
        }
        Criterion::JournalToolValueFlow {
            producer,
            producer_path,
            producer_filter,
            consumer,
            consumer_paths,
            consumer_filter,
            ..
        } => {
            if producer.trim().is_empty() || consumer.trim().is_empty() {
                return Err("JournalToolValueFlow producer and consumer must not be empty".into());
            }
            if consumer_paths.is_empty() {
                return Err("JournalToolValueFlow.consumer_paths must not be empty".into());
            }
            if consumer_paths.iter().any(String::is_empty) {
                return Err("JournalToolValueFlow.consumer_paths entries must not be empty".into());
            }
            for (label, path) in std::iter::once(("producer_path", producer_path))
                .chain(consumer_paths.iter().map(|path| ("consumer_paths", path)))
                .chain(
                    producer_filter
                        .iter()
                        .map(|predicate| ("producer_filter.path", &predicate.path)),
                )
                .chain(
                    consumer_filter
                        .iter()
                        .map(|predicate| ("consumer_filter.path", &predicate.path)),
                )
            {
                if !path.is_empty() && !path.starts_with('/') {
                    return Err(format!(
                        "JournalToolValueFlow.{label} must be an RFC 6901 JSON pointer; got {path:?}"
                    ));
                }
            }
            Ok(())
        }
        Criterion::JournalToolValueFlowBound {
            producer,
            producer_path,
            producer_filters,
            consumer,
            consumer_paths,
            consumer_filters,
            ..
        } => {
            if producer.trim().is_empty() || consumer.trim().is_empty() {
                return Err(
                    "JournalToolValueFlowBound producer and consumer must not be empty".into(),
                );
            }
            if producer_filters.is_empty() || consumer_filters.is_empty() {
                return Err(
                    "JournalToolValueFlowBound producer_filters and consumer_filters must not be empty"
                        .into(),
                );
            }
            if consumer_paths.is_empty() || consumer_paths.iter().any(String::is_empty) {
                return Err(
                    "JournalToolValueFlowBound.consumer_paths must contain non-empty pointers"
                        .into(),
                );
            }
            let filter_paths = producer_filters
                .iter()
                .map(|predicate| (&predicate.path, "producer_filters.path"))
                .chain(
                    consumer_filters
                        .iter()
                        .map(|predicate| (&predicate.path, "consumer_filters.path")),
                );
            for (path, label) in std::iter::once((producer_path, "producer_path"))
                .chain(consumer_paths.iter().map(|path| (path, "consumer_paths")))
                .chain(filter_paths)
            {
                if path.is_empty() || !path.starts_with('/') {
                    return Err(format!(
                        "JournalToolValueFlowBound.{label} must be a non-empty RFC 6901 JSON pointer; got {path:?}"
                    ));
                }
            }
            Ok(())
        }
        Criterion::Judger { threshold, .. } | Criterion::HardJudger { threshold, .. } => {
            if !threshold.is_finite() || *threshold < 0.0 || *threshold > 1.0 {
                return Err(format!(
                    "Judger.threshold must be finite in [0.0, 1.0]; got {threshold}"
                ));
            }
            Ok(())
        }
        Criterion::PipelineAvgCacheHitRatio { min, .. } => {
            if !min.is_finite() || *min < 0.0 || *min > 1.0 {
                return Err(format!(
                    "PipelineAvgCacheHitRatio.min must be finite in [0.0, 1.0]; got {min}"
                ));
            }
            Ok(())
        }
        Criterion::ProviderPromptCacheReadRatio {
            min,
            warmup_turns,
            warmup_rounds,
        } => {
            if !min.is_finite() || *min < 0.0 || *min > 1.0 {
                return Err(format!(
                    "ProviderPromptCacheReadRatio.min must be finite in [0.0, 1.0]; got {min}"
                ));
            }
            if *warmup_turns > 0 && *warmup_rounds > 0 {
                return Err(
                    "ProviderPromptCacheReadRatio may set either warmup_turns or warmup_rounds, not both"
                        .into(),
                );
            }
            Ok(())
        }
        Criterion::SessionEventCount {
            min, event_type, ..
        } => {
            if *min == 0 {
                return Err(format!(
                    "SessionEventCount.min must be >= 1 (min=0 is trivially-true for \
                     event_type={event_type:?}; did you mean >= 1?)"
                ));
            }
            Ok(())
        }
        Criterion::JournalTurnEvaluationSignalCount { kind, min, max } => {
            if kind.trim().is_empty() {
                return Err("JournalTurnEvaluationSignalCount.kind must not be empty".into());
            }
            if min > max {
                return Err(format!(
                    "JournalTurnEvaluationSignalCount: min ({min}) > max ({max}); case will always FAIL"
                ));
            }
            Ok(())
        }
        Criterion::JournalTurnEvaluationSuccess { .. } => Ok(()),
        Criterion::SessionSubsystemHealthy { settled_subsystem } => {
            if settled_subsystem
                .as_ref()
                .is_some_and(|value| value.trim().is_empty())
            {
                Err("SessionSubsystemHealthy.settled_subsystem must not be empty".into())
            } else {
                Ok(())
            }
        }
        Criterion::ForkCacheOutcome { expect } => {
            if expect.is_empty() {
                return Err(
                    "ForkCacheOutcome.expect must not be empty (no outcome would ever match)"
                        .into(),
                );
            }
            Ok(())
        }
        Criterion::StderrMatches { pattern } => {
            // Compile-check the regex at load so a bad pattern fails
            // parse, not every per-case evaluation.
            Regex::new(pattern)
                .map(|_| ())
                .map_err(|e| format!("StderrMatches.pattern: invalid regex {pattern:?}: {e}"))
        }
        Criterion::ToolCalled { name }
        | Criterion::JournalToolCalled { name, .. }
        | Criterion::JournalTurnToolHidden { name } => {
            if name.trim().is_empty() {
                return Err("tool name must not be empty".into());
            }
            Ok(())
        }
        Criterion::PipelineAlertCount { rule, .. } => {
            if rule.trim().is_empty() {
                return Err("PipelineAlertCount.rule must not be empty".into());
            }
            Ok(())
        }
        Criterion::TextContains { needle } => {
            if needle.is_empty() {
                return Err("TextContains.needle must not be empty".into());
            }
            Ok(())
        }
        Criterion::TextNotContains { needle } => {
            if needle.is_empty() {
                return Err("TextNotContains.needle must not be empty".into());
            }
            Ok(())
        }
        Criterion::TextEquals { expected } => {
            if expected.trim().is_empty() {
                return Err("TextEquals.expected must not be empty".into());
            }
            Ok(())
        }
        Criterion::TextJsonValue { path, .. } => validate_json_pointer("TextJsonValue.path", path),
        Criterion::TextJsonArrayCount { path, min, max } => {
            validate_json_pointer("TextJsonArrayCount.path", path)?;
            if min > max {
                return Err(format!(
                    "TextJsonArrayCount: min ({min}) > max ({max}); case will always FAIL"
                ));
            }
            Ok(())
        }
        Criterion::TextJsonPathAbsent { path } => {
            if path.is_empty() {
                return Err(
                    "TextJsonPathAbsent.path cannot select the document root, which is always present"
                        .into(),
                );
            }
            validate_json_pointer("TextJsonPathAbsent.path", path)
        }
        Criterion::TextJsonDag {
            nodes_path,
            node_id_path,
            node_required_string_paths,
            edges_path,
            predecessor_path,
            successor_path,
        } => {
            for (label, path) in [
                ("TextJsonDag.nodes_path", nodes_path),
                ("TextJsonDag.node_id_path", node_id_path),
                ("TextJsonDag.edges_path", edges_path),
                ("TextJsonDag.predecessor_path", predecessor_path),
                ("TextJsonDag.successor_path", successor_path),
            ] {
                validate_json_pointer(label, path)?;
            }
            let mut unique_required_paths = BTreeSet::new();
            for path in node_required_string_paths {
                validate_json_pointer("TextJsonDag.node_required_string_paths[]", path)?;
                if !unique_required_paths.insert(path) {
                    return Err(format!(
                        "TextJsonDag.node_required_string_paths contains duplicate path {path:?}"
                    ));
                }
            }
            Ok(())
        }
        Criterion::ExitCode { .. } => Ok(()),
        Criterion::FinalState { expect } => {
            if expect.trim().is_empty() {
                return Err("FinalState.expect must not be empty".into());
            }
            Ok(())
        }
        Criterion::InterruptionKind { expect } => {
            if expect.trim().is_empty() {
                return Err("InterruptionKind.expect must not be empty".into());
            }
            Ok(())
        }
        Criterion::ToolResultClassCount { class, min, max } => {
            if class.trim().is_empty() {
                return Err("ToolResultClassCount.class must not be empty".into());
            }
            if min > max {
                return Err(format!("ToolResultClassCount: min ({min}) > max ({max})"));
            }
            Ok(())
        }
        Criterion::TokensBetween { min, max } => {
            if min > max {
                return Err(format!("TokensBetween: min ({min}) > max ({max})"));
            }
            Ok(())
        }
        Criterion::DurationBetween { min_ms, max_ms } => {
            if min_ms > max_ms {
                return Err(format!(
                    "DurationBetween: min_ms ({min_ms}) > max_ms ({max_ms})"
                ));
            }
            Ok(())
        }
        Criterion::ToolSequence { tools } => {
            if tools.is_empty() {
                return Err("ToolSequence.tools must not be empty".into());
            }
            for (i, t) in tools.iter().enumerate() {
                if t.trim().is_empty() {
                    return Err(format!("ToolSequence.tools[{i}] must not be empty"));
                }
            }
            Ok(())
        }
        Criterion::TurnRoundsBetween { min, max } => {
            if min > max {
                return Err(format!("TurnRoundsBetween: min ({min}) > max ({max})"));
            }
            Ok(())
        }
        Criterion::CacheRateAbove { threshold, .. } => {
            if !threshold.is_finite() || *threshold < 0.0 || *threshold > 1.0 {
                return Err(format!(
                    "CacheRateAbove.threshold must be in [0.0, 1.0]; got {threshold}"
                ));
            }
            Ok(())
        }
        Criterion::PromptCacheTokens {
            min_read,
            min_creation,
            max_creation,
        } => {
            if *min_read == 0 && *min_creation == 0 && max_creation.is_none() {
                return Err(
                    "PromptCacheTokens requires min_read, min_creation, or max_creation to be set"
                        .into(),
                );
            }
            if let Some(max) = max_creation
                && *max < *min_creation
            {
                return Err(format!(
                    "PromptCacheTokens.max_creation ({max}) must be >= min_creation ({min_creation})"
                ));
            }
            Ok(())
        }
        Criterion::PromptCacheReuseScope { .. } => Ok(()),
        Criterion::AnyOf { criteria } => {
            validate_composite_criteria("AnyOf", criteria, composite_depth + 1)
        }
        Criterion::AllOf { criteria } => {
            validate_composite_criteria("AllOf", criteria, composite_depth + 1)
        }
    }
}

fn validate_composite_criteria(
    label: &str,
    criteria: &[Criterion],
    composite_depth: usize,
) -> Result<(), String> {
    if composite_depth > MAX_COMPOSITE_DEPTH {
        return Err(format!(
            "{label}.criteria exceeds max composite depth {MAX_COMPOSITE_DEPTH}"
        ));
    }
    if criteria.is_empty() {
        return Err(format!("{label}.criteria must not be empty"));
    }
    for (idx, criterion) in criteria.iter().enumerate() {
        if matches!(
            criterion,
            Criterion::Judger { .. } | Criterion::HardJudger { .. }
        ) {
            return Err(format!(
                "{label}.criteria[{idx}]: Judger is evaluated by the runner and cannot be nested"
            ));
        }
        validate_criterion_at_depth(criterion, composite_depth)
            .map_err(|err| format!("{label}.criteria[{idx}]: {err}"))?;
    }
    Ok(())
}

/// Validate every criterion in a list. Returns the first offender's
/// error with a 1-based index so the case author can find the line.
pub fn validate_criteria(criteria: &[Criterion]) -> Result<(), String> {
    for (i, c) in criteria.iter().enumerate() {
        validate_criterion(c).map_err(|e| format!("criteria[{}]: {e}", i + 1))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunOutcome;

    fn outcome_with_tools(tools: &[&str]) -> RunOutcome {
        RunOutcome {
            model: "m".into(),
            exit_code: 0,
            text: "ok".into(),
            stderr: String::new(),
            session_id: None,
            run_id: None,
            tool_calls_count: tools.len() as u32,
            tools_used: tools.iter().map(|s| s.to_string()).collect(),
            completion_tokens: 0,
            prompt_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_tokens: 0,
            duration_ms: 0,
            turn_rounds: 0,
            cache_hits: 0,
            total_tool_calls: 0,
            ttft_ms: 0,
            final_state: None,
            interruption_kind: None,
            tool_result_class_counts: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn tool_called_pass_fail() {
        let out = outcome_with_tools(&["spawn_agent", "read_file"]);
        let hit = evaluate_deterministic(
            &[Criterion::ToolCalled {
                name: "spawn_agent".into(),
            }],
            &out,
        );
        assert!(hit[0].passed);
        let miss = evaluate_deterministic(
            &[Criterion::ToolCalled {
                name: "nonexistent".into(),
            }],
            &out,
        );
        assert!(!miss[0].passed);
    }

    #[test]
    fn text_equals_rejects_a_contradictory_answer_that_contains_expected_text() {
        let mut out = outcome_with_tools(&[]);
        out.text = "ECHO_COUNT: 8; actually ECHO_COUNT: 6".into();

        let result = evaluate_deterministic(
            &[Criterion::TextEquals {
                expected: "ECHO_COUNT: 6".into(),
            }],
            &out,
        );

        assert!(!result[0].passed);
    }

    #[test]
    fn text_json_criteria_validate_structure_without_matching_prose() {
        let mut out = outcome_with_tools(&[]);
        out.text = serde_json::json!({
            "context_id": "ctx-1",
            "nodes": [{"id": "prepare"}, {"id": "verify"}],
            "edges": [{"from": "prepare", "to": "verify"}]
        })
        .to_string();

        let results = evaluate_deterministic(
            &[
                Criterion::TextJsonValue {
                    path: "/context_id".into(),
                    equals: serde_json::json!("ctx-1"),
                },
                Criterion::TextJsonArrayCount {
                    path: "/nodes".into(),
                    min: 2,
                    max: 3,
                },
                Criterion::TextJsonPathAbsent {
                    path: "/session_id".into(),
                },
                Criterion::TextJsonDag {
                    nodes_path: "/nodes".into(),
                    node_id_path: "/id".into(),
                    node_required_string_paths: vec![],
                    edges_path: "/edges".into(),
                    predecessor_path: "/from".into(),
                    successor_path: "/to".into(),
                },
            ],
            &out,
        );

        assert!(results.iter().all(|result| result.passed), "{results:#?}");
        assert!(
            results
                .iter()
                .all(|result| result.severity == CriterionSeverity::Hard)
        );
    }

    #[test]
    fn text_json_dag_rejects_cycles_and_undeclared_endpoints() {
        let criterion = Criterion::TextJsonDag {
            nodes_path: "/nodes".into(),
            node_id_path: "/id".into(),
            node_required_string_paths: vec!["/result".into()],
            edges_path: "/edges".into(),
            predecessor_path: "/from".into(),
            successor_path: "/to".into(),
        };
        for text in [
            r#"{"nodes":[{"id":"a","result":"A"},{"id":"b","result":"B"}],"edges":[{"from":"a","to":"b"},{"from":"b","to":"a"}]}"#,
            r#"{"nodes":[{"id":"a","result":"A"}],"edges":[{"from":"a","to":"missing"}]}"#,
            r#"{"nodes":[{"id":"a","result":"A"},{"id":"b","result":"B"}],"edges":[{"from":"a","to":"b"},{"from":"a","to":"b"}]}"#,
            r#"{"nodes":[{"id":"a","result":""}],"edges":[]}"#,
        ] {
            let mut out = outcome_with_tools(&[]);
            out.text = text.into();
            let result = evaluate_deterministic(std::slice::from_ref(&criterion), &out);
            assert!(!result[0].passed, "{text}");
        }
    }

    #[test]
    fn text_json_contract_rejects_markdown_fences_and_surrounding_prose() {
        let mut out = outcome_with_tools(&[]);
        out.text = "```json\n{\"context_id\":\"ctx-1\"}\n```".into();
        let result = evaluate_deterministic(
            &[Criterion::TextJsonValue {
                path: "/context_id".into(),
                equals: serde_json::json!("ctx-1"),
            }],
            &out,
        );
        assert!(!result[0].passed);
        assert!(result[0].detail.contains("exactly one JSON value"));
    }

    #[test]
    fn text_json_criteria_reject_invalid_pointer_escape_at_load_time() {
        let error = validate_criterion(&Criterion::TextJsonValue {
            path: "/invalid~2escape".into(),
            equals: serde_json::Value::Null,
        })
        .expect_err("invalid RFC 6901 escape must fail before execution");
        assert!(error.contains("only ~0 and ~1"));

        validate_criterion(&Criterion::TextJsonArrayCount {
            path: String::new(),
            min: 1,
            max: 2,
        })
        .expect("the RFC 6901 root pointer must remain available");

        let error = validate_criterion(&Criterion::TextJsonPathAbsent {
            path: String::new(),
        })
        .expect_err("the whole document cannot be absent");
        assert!(error.contains("always present"));
    }

    #[test]
    fn text_not_contains_rejects_stale_topic_contamination() {
        let mut outcome = RunOutcome::new("model");
        outcome.text = "CURSOR-BETA without LEGACY-ALPHA drift".into();
        let results = evaluate_deterministic(
            &[Criterion::TextNotContains {
                needle: "LEGACY-ALPHA".into(),
            }],
            &outcome,
        );
        assert!(!results[0].passed);

        outcome.text = "CURSOR-BETA: cursor mismatch and missing negative test".into();
        let results = evaluate_deterministic(
            &[Criterion::TextNotContains {
                needle: "LEGACY-ALPHA".into(),
            }],
            &outcome,
        );
        assert!(results[0].passed);
    }

    #[test]
    fn any_of_passes_when_one_nested_criterion_passes() {
        let out = outcome_with_tools(&["read_file"]);
        let r = evaluate_deterministic(
            &[Criterion::AnyOf {
                criteria: vec![
                    Criterion::ToolCalled {
                        name: "bash".into(),
                    },
                    Criterion::ToolCalled {
                        name: "read_file".into(),
                    },
                ],
            }],
            &out,
        );

        assert!(r[0].passed);
    }

    #[test]
    fn any_of_short_circuits_after_first_passing_criterion() {
        let out = outcome_with_tools(&["bash"]);
        let r = evaluate_deterministic(
            &[Criterion::AnyOf {
                criteria: vec![
                    Criterion::ToolCalled {
                        name: "bash".into(),
                    },
                    Criterion::TextContains {
                        needle: "sentinel-not-evaluated".into(),
                    },
                ],
            }],
            &out,
        );

        assert!(r[0].passed);
        assert!(
            !r[0].detail.contains("sentinel-not-evaluated"),
            "AnyOf should not evaluate criteria after the first pass"
        );
    }

    #[test]
    fn any_of_fails_when_all_nested_criteria_fail() {
        let out = outcome_with_tools(&["read_file"]);
        let r = evaluate_deterministic(
            &[Criterion::AnyOf {
                criteria: vec![
                    Criterion::ToolCalled {
                        name: "bash".into(),
                    },
                    Criterion::TextContains {
                        needle: "busy-loop".into(),
                    },
                ],
            }],
            &out,
        );

        assert!(!r[0].passed);
    }

    #[test]
    fn all_of_passes_only_when_every_nested_criterion_passes() {
        let mut out = outcome_with_tools(&["bash"]);
        out.turn_rounds = 4;
        out.duration_ms = 20_000;
        out.prompt_tokens = 40_000;
        let r = evaluate_deterministic(
            &[Criterion::AllOf {
                criteria: vec![
                    Criterion::ToolCalled {
                        name: "bash".into(),
                    },
                    Criterion::TurnRoundsBetween { min: 1, max: 6 },
                    Criterion::DurationBetween {
                        min_ms: 1,
                        max_ms: 60_000,
                    },
                    Criterion::TokensBetween {
                        min: 1,
                        max: 100_000,
                    },
                ],
            }],
            &out,
        );

        assert!(r[0].passed);

        out.turn_rounds = 14;
        let r = evaluate_deterministic(
            &[Criterion::AllOf {
                criteria: vec![Criterion::TurnRoundsBetween { min: 1, max: 6 }],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert_eq!(r[0].severity, CriterionSeverity::Hard);
    }

    #[test]
    fn all_of_short_circuits_after_first_failing_criterion() {
        let out = outcome_with_tools(&["read_file"]);
        let r = evaluate_deterministic(
            &[Criterion::AllOf {
                criteria: vec![
                    Criterion::ToolCalled {
                        name: "bash".into(),
                    },
                    Criterion::TextContains {
                        needle: "sentinel-not-evaluated".into(),
                    },
                ],
            }],
            &out,
        );

        assert!(!r[0].passed);
        assert!(
            !r[0].detail.contains("sentinel-not-evaluated"),
            "AllOf should not evaluate criteria after the first failure"
        );
    }

    #[test]
    fn stderr_matches_uses_regex() {
        let mut out = outcome_with_tools(&[]);
        out.stderr = "some noise\n[fork-cache] {...}\nmore noise".into();
        let r = evaluate_deterministic(
            &[Criterion::StderrMatches {
                pattern: r"^\[fork-cache\]".into(),
            }],
            &out,
        );
        assert!(r[0].passed);
        assert_eq!(r[0].severity, CriterionSeverity::Soft);
    }

    #[test]
    fn stderr_matches_invalid_regex_fails_safely() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::StderrMatches {
                pattern: "(".into(),
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("invalid regex"));
    }

    #[test]
    fn tools_count_range_inclusive() {
        let out = outcome_with_tools(&["a", "b", "c"]);
        let inside =
            evaluate_deterministic(&[Criterion::ToolsCountBetween { min: 1, max: 3 }], &out);
        assert!(inside[0].passed);
        let outside =
            evaluate_deterministic(&[Criterion::ToolsCountBetween { min: 5, max: 10 }], &out);
        assert!(!outside[0].passed);
    }

    #[test]
    fn final_state_and_interruption_criteria() {
        let out = RunOutcome::new("m")
            .with_final_state("interrupted")
            .with_interruption_kind("budget_exhausted");
        let results = evaluate_deterministic(
            &[
                Criterion::FinalState {
                    expect: "interrupted".into(),
                },
                Criterion::InterruptionKind {
                    expect: "budget_exhausted".into(),
                },
            ],
            &out,
        );
        assert!(results.iter().all(|result| result.passed));
    }

    #[test]
    fn tool_result_class_count_criterion() {
        let mut out = RunOutcome::new("m");
        out.tool_result_class_counts.insert("env_failure".into(), 2);
        let results = evaluate_deterministic(
            &[Criterion::ToolResultClassCount {
                class: "env_failure".into(),
                min: 1,
                max: 2,
            }],
            &out,
        );
        assert!(results[0].passed);
    }

    #[test]
    fn judger_variant_not_evaluated_synchronously() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::Judger {
                question: "ok?".into(),
                threshold: 0.7,
                model: None,
            }],
            &out,
        );
        // Placeholder-failed; caller runs the async judger separately.
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("judger"));
    }

    #[test]
    fn hard_judger_is_a_required_assertion() {
        let criterion = Criterion::HardJudger {
            question: "did the remote purge succeed?".into(),
            threshold: 0.8,
            model: None,
        };
        assert_eq!(criterion_severity(&criterion), CriterionSeverity::Hard);
        let results = evaluate_deterministic(&[criterion], &outcome_with_tools(&[]));
        assert!(!results[0].passed, "runner must evaluate required judges");
        assert_eq!(results[0].severity, CriterionSeverity::Hard);
    }

    #[test]
    fn criterion_rejects_unknown_fields_instead_of_silently_weakening_a_case() {
        let error = serde_yaml_ng::from_str::<Criterion>(
            "type: judger\nquestion: did it work?\nthreshold: 0.7\nseverity: Hard\n",
        )
        .expect_err("unsupported criterion fields must fail at suite load");
        assert!(error.to_string().contains("severity"), "{error}");
    }

    fn mk_session(events: &[(&str, serde_json::Value)]) -> SessionCapture {
        use crate::session_capture::JournalEvent;
        SessionCapture {
            session_id: "s".into(),
            journal_path: std::path::PathBuf::from("/x"),
            skipped_lines: 0,
            dropped_lines: 0,
            integrity_errors: 0,
            events: events
                .iter()
                .map(|(t, raw)| JournalEvent {
                    event_type: (*t).to_string(),
                    raw: raw.clone(),
                })
                .collect(),
        }
    }

    fn pipeline_feedback_event(
        round: u32,
        cache_identity: &str,
        prompt: u64,
        cache_read: u64,
    ) -> serde_json::Value {
        pipeline_feedback_event_for_run("run-1", round, cache_identity, prompt, cache_read)
    }

    fn pipeline_feedback_event_for_run(
        run_id: &str,
        round: u32,
        cache_identity: &str,
        prompt: u64,
        cache_read: u64,
    ) -> serde_json::Value {
        let cache_identity = astra_turn_types::PromptCacheIdentityV1::from_prefixes(
            &[serde_json::json!(cache_identity)],
            &[],
            "openai-stable-prefix-v1",
        )
        .expect("valid cache identity fixture");
        serde_json::json!({
            "type": "pipeline_feedback",
            "turn": 1,
            "metadata": {
                "kind": "Feedback",
                "turn": 1,
                "runtime_feedback": {
                    "schema_version": astra_turn_core::context_feedback::RuntimeFeedbackFrame::SCHEMA_VERSION,
                    "identity": {
                        "session_id": "s",
                        "run_id": run_id,
                        "agent_id": "agent-1",
                        "model_id": "deepseek-v4-flash",
                        "topology": "server_only"
                    },
                    "progress": {
                        "session_turn": 1,
                        "agentic_round_index": round.saturating_sub(1),
                        "llm_rounds_completed": round,
                        "slice_round_limit": 20,
                        "slice_rounds_remaining": 20_u32.saturating_sub(round)
                    },
                    "context": {
                        "prompt_cache_identity": cache_identity,
                        "token_pressure": 0.1,
                        "compaction_tier": "normal"
                    },
                    "request_usage": {
                        "prompt": prompt,
                        "cache_read": cache_read,
                        "cache_creation": 0,
                        "completion": 10
                    },
                    "run_usage": {
                        "prompt": prompt,
                        "cache_read": cache_read,
                        "cache_creation": 0,
                        "completion": 10
                    },
                    "policy_feedback": {"state": "not_evaluated"},
                    "was_truncated": false
                }
            }
        })
    }

    #[test]
    fn session_event_count_passes_when_min_met() {
        let sess = mk_session(&[
            ("llm_round", serde_json::json!({})),
            ("llm_round", serde_json::json!({})),
            ("tool_invocation", serde_json::json!({})),
        ]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(r[0].passed);
        assert_eq!(r[0].severity, CriterionSeverity::Hard);
    }

    #[test]
    fn session_event_count_fails_by_default_when_no_capture() {
        // Pre-fix semantics: this used to skip-pass, which masked
        // genuine session-capture failures. New default: FAIL with
        // an actionable hint. `optional: true` opts back into skip.
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
                optional: false,
            }],
            &out,
        );
        assert!(!r[0].passed, "default must FAIL, not skip");
        assert!(r[0].detail.contains("FAILED"));
        assert!(r[0].detail.contains("optional: true"));
    }

    #[test]
    fn session_event_count_optional_skips_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::SessionEventCount {
                event_type: "llm_round".into(),
                min: 2,
                optional: true,
            }],
            &out,
        );
        assert!(r[0].passed, "optional=true must skip-pass");
        assert!(r[0].detail.contains("skipped"));
    }

    #[test]
    fn session_subsystem_health_uses_typed_durable_evidence() {
        let healthy = mk_session(&[
            (
                "session_memory_extraction",
                serde_json::json!({"turn": 4, "metadata": {"outcome": "extracted", "source": "llm"}}),
            ),
            (
                "turn",
                serde_json::json!({"turn": 4, "text": "Error words in user-visible content are irrelevant"}),
            ),
            (
                "subsystem_diagnostic",
                serde_json::json!({
                    "turn": 3,
                    "metadata": {
                        "severity": "warning",
                        "subsystem": "old_run",
                        "operation": "cleanup",
                        "code": "failed"
                    }
                }),
            ),
        ]);
        let errored = mk_session(&[(
            "session_memory_extraction",
            serde_json::json!({"turn": 4, "metadata": {"outcome": "errored", "reason": "write_failed"}}),
        )]);
        let degraded = mk_session(&[
            ("turn", serde_json::json!({"turn": 4})),
            (
                "subsystem_diagnostic",
                serde_json::json!({
                    "turn": 4,
                    "metadata": {
                        "severity": "warning",
                        "subsystem": "session_memory",
                        "operation": "stale_snapshot_cleanup",
                        "code": "purge_failed"
                    }
                }),
            ),
        ]);
        let outcome = outcome_with_tools(&[]);

        let evaluate = |session: &SessionCapture| {
            evaluate_deterministic_with_session(
                &[Criterion::SessionSubsystemHealthy {
                    settled_subsystem: None,
                }],
                &outcome,
                Some(session),
            )
            .remove(0)
        };
        assert!(evaluate(&healthy).passed);
        assert!(!evaluate(&errored).passed);
        assert!(!evaluate(&degraded).passed);
    }

    #[test]
    fn session_subsystem_health_requires_complete_capture() {
        let outcome = outcome_with_tools(&[]);
        let missing = evaluate_deterministic(
            &[Criterion::SessionSubsystemHealthy {
                settled_subsystem: None,
            }],
            &outcome,
        );
        assert!(!missing[0].passed);

        let mut incomplete = mk_session(&[]);
        incomplete.dropped_lines = 1;
        let result = evaluate_deterministic_with_session(
            &[Criterion::SessionSubsystemHealthy {
                settled_subsystem: None,
            }],
            &outcome,
            Some(&incomplete),
        );
        assert!(!result[0].passed);
        assert!(result[0].detail.contains("incomplete"));
    }

    #[test]
    fn session_criteria_fail_closed_on_malformed_or_truncated_capture() {
        let mut incomplete = mk_session(&[(
            "tool_invocation",
            serde_json::json!({"metadata": {"tool_name": "Read"}}),
        )]);
        incomplete.skipped_lines = 1;
        let outcome = outcome_with_tools(&[]);

        let required = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: false,
            }],
            &outcome,
            Some(&incomplete),
        );
        assert!(!required[0].passed);
        assert!(required[0].detail.contains("evidence incomplete"));

        let optional = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: true,
            }],
            &outcome,
            Some(&incomplete),
        );
        assert!(optional[0].passed);
        assert!(optional[0].detail.contains("optional criterion skipped"));
    }

    #[test]
    fn journal_tool_called_reads_from_session_not_envelope() {
        // RunOutcome.tools_used is empty but journal shows Read was
        // invoked — journal is the source of truth (subagent calls
        // may not flow back through the envelope).
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "type": "turn",
                "ts": "2026-08-09T00:00:01Z",
                "session_id": "s",
                "turn": 1,
                "tool_calls": [{"name": "Read", "ok": true, "ms": 1}]
            }),
        )]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(r[0].passed);
    }

    #[test]
    fn journal_tool_called_fails_when_tool_missing() {
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "type": "turn",
                "ts": "2026-08-09T00:00:01Z",
                "session_id": "s",
                "turn": 1,
                "tool_calls": [{"name": "Read", "ok": true, "ms": 1}]
            }),
        )]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::JournalToolCalled {
                name: "Grep".into(),
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("NOT invoked"));
    }

    #[test]
    fn journal_tool_called_fails_by_default_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: false,
            }],
            &out,
        );
        assert!(!r[0].passed, "default must FAIL");
        assert!(r[0].detail.contains("FAILED"));
        assert_eq!(r[0].severity, CriterionSeverity::Hard);
    }

    #[test]
    fn journal_tool_called_optional_skips_when_no_capture() {
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic(
            &[Criterion::JournalToolCalled {
                name: "Read".into(),
                optional: true,
            }],
            &out,
        );
        assert!(r[0].passed);
        assert!(r[0].detail.contains("skipped"));
        assert_eq!(r[0].severity, CriterionSeverity::Quality);
    }

    #[test]
    fn journal_turn_tool_hidden_checks_only_the_canonical_coordinator_surface() {
        let outcome = outcome_with_tools(&[]);
        let hidden = mk_session(&[
            (
                "turn",
                serde_json::json!({"visible_tools": ["start_work", "tool_search"]}),
            ),
            // A child may legitimately use the attempt-owned tool; this is
            // not evidence that the coordinator surface leaked it.
            (
                "ToolCallCompleted",
                serde_json::json!({"payload": {"tool_name": "settle_work_item"}}),
            ),
        ]);
        let leaked = mk_session(&[(
            "turn",
            serde_json::json!({"visible_tools": ["start_work", "settle_work_item"]}),
        )]);
        let criterion = Criterion::JournalTurnToolHidden {
            name: "settle_work_item".into(),
        };

        let hidden_result = evaluate_deterministic_with_session(
            std::slice::from_ref(&criterion),
            &outcome,
            Some(&hidden),
        );
        assert!(hidden_result[0].passed, "{hidden_result:?}");
        assert_eq!(hidden_result[0].severity, CriterionSeverity::Hard);

        let leaked_result =
            evaluate_deterministic_with_session(&[criterion], &outcome, Some(&leaked));
        assert!(!leaked_result[0].passed, "{leaked_result:?}");
        assert!(leaked_result[0].detail.contains("1/1"));
    }

    #[test]
    fn durable_tool_json_proves_exact_fanout_contract() {
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [{
                    "tool_call_id": "fanout-call",
                    "name": "agent_fanout",
                    "ok": true,
                    "args_full": r#"{"action":"start","target_count":3}"#,
                    "result_full": r#"{"fanout":{"terminal":3},"provenance":{"all_slots_delivered":true}}"#
                }, {
                    "tool_call_id": "fanout-results-call",
                    "name": "agent_fanout",
                    "ok": true,
                    "args_full": r#"{"action":"get_results","group_id":"review"}"#,
                    "result_full": r#"{"fanout":{"terminal":3},"provenance":{"all_slots_delivered":true}}"#
                }]
            }),
        )]);
        let criteria = [
            Criterion::JournalToolCallCount {
                name: "agent_fanout".into(),
                min: 1,
                max: 1,
                document: Some(JournalToolDocument::Arguments),
                path: Some("/action".into()),
                equals: Some(serde_json::json!("start")),
            },
            Criterion::JournalToolJson {
                name: "agent_fanout".into(),
                document: JournalToolDocument::Arguments,
                path: "/target_count".into(),
                equals: serde_json::json!(3),
            },
            Criterion::JournalToolJson {
                name: "agent_fanout".into(),
                document: JournalToolDocument::Result,
                path: "/provenance/all_slots_delivered".into(),
                equals: serde_json::json!(true),
            },
        ];
        let results =
            evaluate_deterministic_with_session(&criteria, &outcome_with_tools(&[]), Some(&sess));
        assert!(results.iter().all(|result| result.passed), "{results:?}");
        assert!(
            results
                .iter()
                .all(|result| result.severity == CriterionSeverity::Hard)
        );

        let wrong = evaluate_deterministic_with_session(
            &[Criterion::JournalToolJson {
                name: "agent_fanout".into(),
                document: JournalToolDocument::Result,
                path: "/fanout/terminal".into(),
                equals: serde_json::json!(2),
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(!wrong[0].passed, "partial settlement must fail");
    }

    #[test]
    fn durable_tool_outcome_count_uses_typed_success_state() {
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [{
                    "tool_call_id": "failed-start",
                    "name": "start_work",
                    "ok": false,
                    "args_full": r#"{\"activation\":\"start\"}"#,
                    "result_full": r#"{\"error\":\"invalid graph\"}"#
                }, {
                    "tool_call_id": "started",
                    "name": "start_work",
                    "ok": true,
                    "args_full": r#"{\"activation\":\"start\"}"#,
                    "result_full": r#"{\"status\":\"started\"}"#
                }]
            }),
        )]);
        let no_failed_start = Criterion::JournalToolOutcomeCount {
            name: "start_work".into(),
            ok: false,
            min: 0,
            max: 0,
        };
        let one_success = Criterion::JournalToolOutcomeCount {
            name: "start_work".into(),
            ok: true,
            min: 1,
            max: 1,
        };
        let results = evaluate_deterministic_with_session(
            &[no_failed_start, one_success],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(!results[0].passed, "the typed failed call must be counted");
        assert!(
            results[1].passed,
            "the typed successful call must be counted"
        );
        assert!(
            results
                .iter()
                .all(|result| result.severity == CriterionSeverity::Hard)
        );
    }

    #[test]
    fn durable_tool_success_ratio_reports_raw_execution_health() {
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {"tool_call_id":"baseline","name":"bash","ok":false},
                    {"tool_call_id":"edit","name":"str_replace","ok":true},
                    {"tool_call_id":"verify","name":"bash","ok":true},
                    {"tool_call_id":"inspect","name":"read_file","ok":true},
                    {"tool_call_id":"pending","name":"read_file"}
                ]
            }),
        )]);
        let results = evaluate_deterministic_with_session(
            &[Criterion::JournalToolSuccessRatio {
                min: 1.0,
                min_calls: 4,
                allowed_failures: 1,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(results[0].passed, "{results:?}");
        assert!(results[0].detail.contains("raw=3/4 (75.0%)"));
        assert!(results[0].detail.contains("adjusted=3/3 (100.0%)"));

        let strict = evaluate_deterministic_with_session(
            &[Criterion::JournalToolSuccessRatio {
                min: 0.76,
                min_calls: 4,
                allowed_failures: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(!strict[0].passed, "{strict:?}");
    }

    #[test]
    fn turn_evaluation_signal_count_uses_typed_signal_codes() {
        let sess = mk_session(&[(
            "turn_evaluation",
            serde_json::json!({
                "metadata": {
                    "signals": [
                        {"kind": "search_fanout", "message": "rendered copy is irrelevant"},
                        {"kind": "llm_round_churn", "rounds": 16},
                        {"kind": "search_fanout", "message": "copy may change"}
                    ]
                }
            }),
        )]);
        let no_search_churn = Criterion::JournalTurnEvaluationSignalCount {
            kind: "search_fanout".into(),
            min: 0,
            max: 0,
        };
        let one_round_churn = Criterion::JournalTurnEvaluationSignalCount {
            kind: "llm_round_churn".into(),
            min: 1,
            max: 1,
        };
        let results = evaluate_deterministic_with_session(
            &[no_search_churn, one_round_churn],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(!results[0].passed, "both typed search signals must count");
        assert!(results[1].passed, "the exact typed signal code must count");
        assert!(
            results
                .iter()
                .all(|result| result.severity == CriterionSeverity::Hard)
        );
    }

    #[test]
    fn terminal_turn_evaluation_success_cannot_be_masked_by_structural_tool_calls() {
        let sess = mk_session(&[
            (
                "turn_evaluation",
                serde_json::json!({
                    "ts": "2026-08-23T00:00:02Z",
                    "turn": 2,
                    "metadata": {"success": true}
                }),
            ),
            (
                "turn_evaluation",
                serde_json::json!({
                    "ts": "2026-08-23T00:00:01Z",
                    "turn": 1,
                    "metadata": {"success": false}
                }),
            ),
        ]);
        let results = evaluate_deterministic_with_session(
            &[Criterion::JournalTurnEvaluationSuccess { equals: true }],
            &outcome_with_tools(&["agent_fanout"]),
            Some(&sess),
        );
        assert!(
            results[0].passed,
            "the latest timestamped evaluation is authoritative even when owner artifacts are merged out of order"
        );
        assert_eq!(results[0].severity, CriterionSeverity::Hard);

        let reversed = mk_session(&[
            (
                "turn_evaluation",
                serde_json::json!({
                    "ts": "2026-08-23T00:00:02Z",
                    "turn": 2,
                    "metadata": {"success": false}
                }),
            ),
            (
                "turn_evaluation",
                serde_json::json!({
                    "ts": "2026-08-23T00:00:01Z",
                    "turn": 1,
                    "metadata": {"success": true}
                }),
            ),
        ]);
        let reversed_results = evaluate_deterministic_with_session(
            &[Criterion::JournalTurnEvaluationSuccess { equals: true }],
            &outcome_with_tools(&["agent_fanout"]),
            Some(&reversed),
        );
        assert!(!reversed_results[0].passed);
    }

    #[test]
    fn durable_artifact_provenance_requires_exact_prior_handle() {
        let handle = "artifact://session/tool-result/Y2FsbC0x";
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [{
                    "tool_call_id": "call-1",
                    "name": "bash",
                    "ok": true,
                    "args_full": r#"{"command":"produce evidence"}"#,
                    "result_full": format!("evidence omitted; recover with {handle}")
                }, {
                    "tool_call_id": "call-2",
                    "name": "introspect",
                    "ok": true,
                    "args_full": serde_json::json!({
                        "artifact": handle,
                        "offset": 0,
                        "max_bytes": 256
                    }).to_string(),
                    "result_full": "RECOVERED_EVIDENCE_OK"
                }]
            }),
        )]);
        let criterion = Criterion::JournalArtifactConsumed {
            producer: "bash".into(),
            consumer: "introspect".into(),
        };
        let result = evaluate_deterministic_with_session(
            std::slice::from_ref(&criterion),
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(result[0].passed, "{}", result[0].detail);

        let reversed = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [{
                    "tool_call_id": "call-2",
                    "name": "introspect",
                    "args_full": serde_json::json!({"artifact": handle}).to_string()
                }, {
                    "tool_call_id": "call-1",
                    "name": "bash",
                    "result_full": handle
                }]
            }),
        )]);
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &outcome_with_tools(&[]),
            Some(&reversed),
        );
        assert!(
            !result[0].passed,
            "a future producer cannot justify an earlier consumer"
        );
    }

    #[test]
    fn durable_value_flow_requires_exact_successful_prior_scalar() {
        let call = |id: &str, ok: bool, args: serde_json::Value, result: serde_json::Value| {
            serde_json::json!({
                "tool_call_id": id,
                "name": "memory",
                "ok": ok,
                "args_full": args.to_string(),
                "result_full": result.to_string(),
            })
        };
        let criterion = Criterion::JournalToolValueFlow {
            producer: "memory".into(),
            producer_document: JournalToolDocument::Result,
            producer_path: "/memory_id".into(),
            producer_filter: Some(JournalJsonPredicate {
                document: JournalToolDocument::Arguments,
                path: "/action".into(),
                equals: serde_json::json!("remember"),
            }),
            consumer: "memory".into(),
            consumer_document: JournalToolDocument::Arguments,
            consumer_paths: vec![
                "/memory_id".into(),
                "/memory_ids".into(),
                "/request/memory_id".into(),
            ],
            consumer_filter: Some(JournalJsonPredicate {
                document: JournalToolDocument::Arguments,
                path: "/action".into(),
                equals: serde_json::json!("forget"),
            }),
        };
        let mut empty_destination = criterion.clone();
        let Criterion::JournalToolValueFlow { consumer_paths, .. } = &mut empty_destination else {
            unreachable!()
        };
        *consumer_paths = vec![String::new()];
        assert!(
            validate_criterion(&empty_destination).is_err(),
            "consumer destinations must be explicit nonempty pointers"
        );
        let evaluate = |tool_calls: Vec<serde_json::Value>| {
            let session = mk_session(&[("turn", serde_json::json!({"tool_calls": tool_calls}))]);
            evaluate_deterministic_with_session(
                std::slice::from_ref(&criterion),
                &outcome_with_tools(&[]),
                Some(&session),
            )[0]
            .clone()
        };

        let producer = call(
            "produce",
            true,
            serde_json::json!({"action": "remember"}),
            serde_json::json!({"memory_id": "memory-123"}),
        );
        let consumer = call(
            "consume",
            true,
            serde_json::json!({"action": "forget", "memory_ids": ["memory-123"]}),
            serde_json::json!({"purged": 1}),
        );
        assert!(evaluate(vec![producer.clone(), consumer.clone()]).passed);
        assert!(
            evaluate(vec![
                producer.clone(),
                call(
                    "consume-nested",
                    true,
                    serde_json::json!({
                        "action": "forget",
                        "request": {"memory_id": "memory-123"}
                    }),
                    serde_json::json!({"purged": 1}),
                ),
            ])
            .passed,
            "the exact scalar may flow through a nested typed argument subtree"
        );
        assert!(
            !evaluate(vec![consumer.clone(), producer.clone()]).passed,
            "a future producer cannot justify an earlier consumer"
        );
        assert!(
            !evaluate(vec![
                producer.clone(),
                call(
                    "wrong",
                    true,
                    serde_json::json!({"action": "forget", "memory_ids": ["other"]}),
                    serde_json::json!({}),
                ),
            ])
            .passed,
            "a different scalar is not provenance"
        );
        assert!(
            !evaluate(vec![
                producer.clone(),
                call(
                    "mentioned-only",
                    true,
                    serde_json::json!({
                        "action": "forget",
                        "memory_id": "other",
                        "reason": "memory-123"
                    }),
                    serde_json::json!({}),
                ),
            ])
            .passed,
            "an exact value in an unrelated argument is not consumption"
        );
        assert!(
            !evaluate(vec![
                producer.clone(),
                call(
                    "intermediate",
                    true,
                    serde_json::json!({
                        "action": "expand",
                        "memory_id": "memory-123"
                    }),
                    serde_json::json!({}),
                ),
                call(
                    "wrong-forget",
                    true,
                    serde_json::json!({
                        "action": "forget",
                        "memory_id": "other"
                    }),
                    serde_json::json!({}),
                ),
            ])
            .passed,
            "an intermediate consumer and unrelated terminal action cannot be spliced"
        );
        assert!(
            !evaluate(vec![
                call(
                    "failed-producer",
                    false,
                    serde_json::json!({"action": "remember"}),
                    serde_json::json!({"memory_id": "memory-123"}),
                ),
                consumer,
            ])
            .passed,
            "failed producer output cannot certify value flow"
        );

        let mut any_result_item = criterion.clone();
        let Criterion::JournalToolValueFlow {
            consumer_document,
            consumer_paths,
            consumer_filter,
            ..
        } = &mut any_result_item
        else {
            unreachable!()
        };
        *consumer_document = JournalToolDocument::Result;
        *consumer_paths = vec!["/*/memory_id".into()];
        *consumer_filter = None;
        let session = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    producer,
                    call(
                        "recall",
                        true,
                        serde_json::json!({"action": "recall"}),
                        serde_json::json!([
                            {"memory_id": "other"},
                            {"memory_id": "memory-123"}
                        ]),
                    )
                ]
            }),
        )]);
        let result = evaluate_deterministic_with_session(
            &[any_result_item],
            &outcome_with_tools(&[]),
            Some(&session),
        );
        assert!(
            result[0].passed,
            "wildcard projection must prove a scalar in any result item"
        );

        let bounded = Criterion::JournalToolValueFlowBound {
            producer: "memory".into(),
            producer_document: JournalToolDocument::Result,
            producer_path: "/memory_id".into(),
            producer_filters: vec![
                JournalJsonPredicate {
                    document: JournalToolDocument::Arguments,
                    path: "/action".into(),
                    equals: serde_json::json!("remember"),
                },
                JournalJsonPredicate {
                    document: JournalToolDocument::Arguments,
                    path: "/memory_type".into(),
                    equals: serde_json::json!("working"),
                },
            ],
            consumer: "memory".into(),
            consumer_document: JournalToolDocument::Result,
            consumer_paths: vec!["/*/memory_id".into()],
            consumer_filters: vec![
                JournalJsonPredicate {
                    document: JournalToolDocument::Arguments,
                    path: "/action".into(),
                    equals: serde_json::json!("recall"),
                },
                JournalJsonPredicate {
                    document: JournalToolDocument::Arguments,
                    path: "/scope".into(),
                    equals: serde_json::json!("session"),
                },
            ],
        };
        let splice_session = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    call(
                        "failed-producer",
                        false,
                        serde_json::json!({"action":"remember", "memory_type":"working"}),
                        serde_json::json!({"memory_id":"memory-123"}),
                    ),
                    call(
                        "session-recall",
                        true,
                        serde_json::json!({"action":"recall", "scope":"session"}),
                        serde_json::json!([]),
                    ),
                    call(
                        "unscoped-recall",
                        true,
                        serde_json::json!({"action":"recall"}),
                        serde_json::json!([{"memory_id":"memory-123"}]),
                    ),
                ]
            }),
        )]);
        let bounded_result = evaluate_deterministic_with_session(
            std::slice::from_ref(&bounded),
            &outcome_with_tools(&[]),
            Some(&splice_session),
        );
        assert!(
            !bounded_result[0].passed,
            "failed producer and unscoped consumer must not be spliced into a bound flow"
        );

        let good_session = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    call(
                        "good-producer",
                        true,
                        serde_json::json!({"action":"remember", "memory_type":"working"}),
                        serde_json::json!({"memory_id":"memory-123"}),
                    ),
                    call(
                        "good-consumer",
                        true,
                        serde_json::json!({"action":"recall", "scope":"session"}),
                        serde_json::json!([{"memory_id":"memory-123"}]),
                    ),
                ]
            }),
        )]);
        let bounded_result = evaluate_deterministic_with_session(
            &[bounded],
            &outcome_with_tools(&[]),
            Some(&good_session),
        );
        assert!(
            bounded_result[0].passed,
            "bound filters should accept one valid flow"
        );
    }

    #[test]
    fn pipeline_avg_cache_hit_ratio_optional_skips_when_no_feedback_turns() {
        let sess = mk_session(&[]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::PipelineAvgCacheHitRatio {
                min: 0.8,
                optional: true,
            }],
            &out,
            Some(&sess),
        );
        assert!(r[0].passed, "optional=true must skip-pass");
        assert!(r[0].detail.contains("skipped"));
    }

    #[test]
    fn pipeline_avg_cache_hit_ratio_rejects_invalid_feedback_payload() {
        let capture = mk_session(&[(
            "pipeline_feedback",
            serde_json::json!({
                "metadata": {"cache_hit_ratio": 2.0}
            }),
        )]);
        let criterion = Criterion::PipelineAvgCacheHitRatio {
            min: 0.9,
            optional: false,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &RunOutcome::new("m"),
            Some(&capture),
        );
        assert!(!result[0].passed);
        assert!(result[0].detail.contains("evidence incomplete"));
    }

    #[test]
    fn optional_pipeline_ratio_skips_invalid_feedback_payload() {
        let capture = mk_session(&[(
            "pipeline_feedback",
            serde_json::json!({
                "metadata": {"cache_hit_ratio": 2.0}
            }),
        )]);
        let criterion = Criterion::PipelineAvgCacheHitRatio {
            min: 0.9,
            optional: true,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &RunOutcome::new("m"),
            Some(&capture),
        );
        assert!(result[0].passed);
        assert!(result[0].detail.contains("optional criterion skipped"));
    }

    #[test]
    fn pipeline_avg_cache_hit_ratio_fails_when_no_feedback_turns_and_required() {
        let sess = mk_session(&[]);
        let out = outcome_with_tools(&[]);
        let r = evaluate_deterministic_with_session(
            &[Criterion::PipelineAvgCacheHitRatio {
                min: 0.8,
                optional: false,
            }],
            &out,
            Some(&sess),
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("no pipeline feedback turns"));
    }

    #[test]
    fn provider_prompt_cache_read_ratio_excludes_configured_warmup() {
        let sess = mk_session(&[
            (
                "turn",
                serde_json::json!({
                    "turn": 1,
                    "tokens_in": 10_000,
                    "cache_read_tokens": 0,
                    "cache_creation_tokens": 2_000
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800,
                    "cache_creation_tokens": 0
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "turn": 3,
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800,
                    "cache_creation_tokens": 0
                }),
            ),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.98,
                warmup_turns: 1,
                warmup_rounds: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(result[0].passed, "{:?}", result[0]);
        assert!(result[0].detail.contains("98.00%"), "{}", result[0].detail);
        assert!(result[0].detail.contains("turns=2"), "{}", result[0].detail);
    }

    #[test]
    fn stable_prefix_cache_reuse_scores_each_typed_epoch_after_its_cold_boundary() {
        let sess = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-a", 100, 29_900),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(3, "prefix-a", 100, 29_900),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(4, "prefix-b", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(5, "prefix-b", 100, 29_900),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(6, "prefix-b", 100, 29_900),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 2,
            max_identity_transitions_per_run: 1,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(result[0].passed, "{:?}", result[0]);
        assert!(result[0].detail.contains("pairs=2"));
        assert!(result[0].detail.contains("max_transitions_per_run=1"));
    }

    #[test]
    fn stable_prefix_cache_reuse_ignores_dynamic_prompt_tail() {
        let sess = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 36_096),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-a", 3_000, 36_224),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 1,
            max_identity_transitions_per_run: 0,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(result[0].passed, "{}", result[0].detail);
    }

    #[test]
    fn stable_prefix_cache_reuse_rejects_repeated_cold_observation_after_recovery() {
        let sess = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(3, "prefix-a", 100, 36_000),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(4, "prefix-a", 100, 36_000),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 1,
            max_identity_transitions_per_run: 0,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(!result[0].passed, "{}", result[0].detail);
        assert!(
            result[0].detail.contains("worst=0.00%"),
            "{}",
            result[0].detail
        );
    }

    #[test]
    fn stable_prefix_cache_reuse_rejects_cached_prefix_regression() {
        let sess = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 100, 36_096),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-a", 100, 32_000),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 1,
            max_identity_transitions_per_run: 0,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &outcome_with_tools(&[]),
            Some(&sess),
        );
        assert!(!result[0].passed, "{}", result[0].detail);
    }

    #[test]
    fn stable_prefix_cache_reuse_fails_closed_without_identity_or_pair() {
        let mut missing_identity = pipeline_feedback_event(1, "prefix-a", 30_000, 0);
        missing_identity
            .pointer_mut("/metadata/runtime_feedback/context")
            .and_then(serde_json::Value::as_object_mut)
            .expect("context")
            .remove("prompt_cache_identity");
        let missing = mk_session(&[("pipeline_feedback", missing_identity)]);
        let no_pair = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-b", 30_000, 0),
            ),
        ]);
        let low_reuse = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-a", 10_000, 20_000),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 1,
            max_identity_transitions_per_run: 10,
        };

        for capture in [&missing, &no_pair, &low_reuse] {
            let result = evaluate_deterministic_with_session(
                std::slice::from_ref(&criterion),
                &outcome_with_tools(&[]),
                Some(capture),
            );
            assert!(!result[0].passed, "{:?}", result[0]);
        }
    }

    #[test]
    fn stable_prefix_cache_reuse_rejects_conflicting_mirrors_and_excess_churn() {
        let conflicting = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 100, 29_900),
            ),
        ]);
        let churn = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event(1, "prefix-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(2, "prefix-a", 100, 29_900),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(3, "prefix-b", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(4, "prefix-b", 100, 29_900),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(5, "prefix-c", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event(6, "prefix-c", 100, 29_900),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 1,
            max_identity_transitions_per_run: 1,
        };

        for capture in [&conflicting, &churn] {
            let result = evaluate_deterministic_with_session(
                std::slice::from_ref(&criterion),
                &outcome_with_tools(&[]),
                Some(capture),
            );
            assert!(!result[0].passed, "{:?}", result[0]);
        }
    }

    #[test]
    fn stable_prefix_cache_reuse_requires_evidence_from_every_multi_round_run() {
        let sess = mk_session(&[
            (
                "pipeline_feedback",
                pipeline_feedback_event_for_run("root", 1, "root-a", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event_for_run("root", 2, "root-b", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event_for_run("root", 3, "root-c", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event_for_run("child", 1, "child-x", 30_000, 0),
            ),
            (
                "pipeline_feedback",
                pipeline_feedback_event_for_run("child", 2, "child-x", 100, 29_900),
            ),
        ]);
        let criterion = Criterion::ProviderPromptCacheStablePrefixReuseRatio {
            min: 0.95,
            min_pairs: 1,
            // Keep the transition ceiling out of this counterexample: the
            // root must fail because it has no comparable pair, even though
            // the child supplies a healthy one.
            max_identity_transitions_per_run: 10,
        };
        let result = evaluate_deterministic_with_session(
            &[criterion],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(!result[0].passed, "{:?}", result[0]);
        assert!(result[0].detail.contains("min_pairs_per_run=0"));
    }

    #[test]
    fn required_prompt_cache_scope_is_hard_and_scope_specific() {
        let one_turn = mk_session(&[(
            "turn",
            serde_json::json!({
                "turn": 1,
                "tokens_in": 10_000,
                "cache_creation_tokens": 2_000
            }),
        )]);
        let one_turn_result = evaluate_deterministic_with_session(
            &[Criterion::PromptCacheReuseScope {
                scope: PromptCacheReuseScope::ConversationTurns,
            }],
            &outcome_with_tools(&[]),
            Some(&one_turn),
        );
        assert!(!one_turn_result[0].passed);
        assert_eq!(one_turn_result[0].severity, CriterionSeverity::Hard);

        let two_turns = mk_session(&[
            (
                "turn",
                serde_json::json!({
                    "turn": 1,
                    "tokens_in": 10_000,
                    "cache_creation_tokens": 2_000
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800
                }),
            ),
        ]);
        let conversation_result = evaluate_deterministic_with_session(
            &[Criterion::PromptCacheReuseScope {
                scope: PromptCacheReuseScope::ConversationTurns,
            }],
            &outcome_with_tools(&[]),
            Some(&two_turns),
        );
        assert!(conversation_result[0].passed, "{conversation_result:?}");

        let one_turn_rounds = mk_session(&[
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 0,
                    "agentic_step": 1,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 10_000,
                    "cache_creation_tokens": 2_000
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 1,
                    "agentic_step": 2,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800
                }),
            ),
        ]);
        let intra_result = evaluate_deterministic_with_session(
            &[Criterion::PromptCacheReuseScope {
                scope: PromptCacheReuseScope::IntraTurnRounds,
            }],
            &outcome_with_tools(&[]),
            Some(&one_turn_rounds),
        );
        assert!(intra_result[0].passed, "{intra_result:?}");

        let identityless_mirrors = mk_session(&[
            (
                "turn",
                serde_json::json!({
                    "tokens_in": 10_000,
                    "cache_creation_tokens": 2_000
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800
                }),
            ),
        ]);
        let identityless_result = evaluate_deterministic_with_session(
            &[Criterion::PromptCacheReuseScope {
                scope: PromptCacheReuseScope::ConversationTurns,
            }],
            &outcome_with_tools(&[]),
            Some(&identityless_mirrors),
        );
        assert!(
            !identityless_result[0].passed,
            "missing turn IDs must fail closed"
        );

        let missing_run_id = mk_session(&[
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 0,
                    "tokens_in": 10_000,
                    "cache_creation_tokens": 2_000
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 1,
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800
                }),
            ),
        ]);
        let missing_run_result = evaluate_deterministic_with_session(
            &[Criterion::PromptCacheReuseScope {
                scope: PromptCacheReuseScope::IntraTurnRounds,
            }],
            &outcome_with_tools(&[]),
            Some(&missing_run_id),
        );
        assert!(
            !missing_run_result[0].passed,
            "missing run identity must fail closed"
        );

        let separate_turns = mk_session(&[
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 0,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 10_000,
                    "cache_creation_tokens": 2_000
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 2,
                    "round": 0,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 200,
                    "cache_read_tokens": 9_800
                }),
            ),
        ]);
        let separate_turn_result = evaluate_deterministic_with_session(
            &[Criterion::PromptCacheReuseScope {
                scope: PromptCacheReuseScope::IntraTurnRounds,
            }],
            &outcome_with_tools(&[]),
            Some(&separate_turns),
        );
        assert!(
            !separate_turn_result[0].passed,
            "cross-turn rounds are not intra-turn reuse"
        );
    }

    #[test]
    fn provider_prompt_cache_read_ratio_is_token_weighted_and_counts_creation() {
        let sess = mk_session(&[
            (
                "turn",
                serde_json::json!({
                    "tokens_in": 20,
                    "cache_read_tokens": 980,
                    "cache_creation_tokens": 0
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "tokens_in": 0,
                    "cache_read_tokens": 9_800,
                    "cache_creation_tokens": 220
                }),
            ),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.98,
                warmup_turns: 0,
                warmup_rounds: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(!result[0].passed, "creation tokens are non-read input");
        assert!(result[0].detail.contains("97.82%"), "{}", result[0].detail);
    }

    #[test]
    fn provider_prompt_cache_read_ratio_fails_without_post_warmup_usage() {
        let sess = mk_session(&[(
            "turn",
            serde_json::json!({
                "tokens_in": 100,
                "cache_read_tokens": 0
            }),
        )]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.98,
                warmup_turns: 1,
                warmup_rounds: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(!result[0].passed);
        assert!(
            result[0]
                .detail
                .contains("no provider usage after 1 warmup"),
            "{}",
            result[0].detail
        );
    }

    #[test]
    fn provider_prompt_cache_read_ratio_falls_back_to_legacy_llm_round_events() {
        let sess = mk_session(&[(
            "llm_round",
            serde_json::json!({
                "tokens_in": 10,
                "cache_read_tokens": 990
            }),
        )]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.99,
                warmup_turns: 0,
                warmup_rounds: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(result[0].passed, "{:?}", result[0]);
    }

    #[test]
    fn provider_prompt_cache_round_warmup_measures_intra_turn_reuse() {
        let round = |index: u64, fresh: u64, read: u64| {
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": index,
                    "agentic_step": index + 1,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": fresh,
                    "cache_read_tokens": read,
                }),
            )
        };
        let sess = mk_session(&[
            round(0, 10_000, 0),
            round(1, 500, 9_500),
            // A mirrored copy of the second boundary must not inflate the
            // measured read ratio or make results depend on owner ordering.
            round(1, 500, 9_500),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.95,
                warmup_turns: 0,
                warmup_rounds: 1,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(result[0].passed, "{:?}", result[0]);
        assert!(
            result[0].detail.contains("rounds=1"),
            "{}",
            result[0].detail
        );
        assert!(
            result[0].detail.contains("warmup_round=1"),
            "{}",
            result[0].detail
        );
    }

    #[test]
    fn provider_prompt_cache_round_mirrors_fail_closed_on_conflict() {
        let sess = mk_session(&[
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 0,
                    "agentic_step": 1,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 10_000,
                    "cache_read_tokens": 0,
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 1,
                    "agentic_step": 2,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 10_000,
                    "cache_read_tokens": 9_900,
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 1,
                    "agentic_step": 2,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 10_000,
                    "cache_read_tokens": 0,
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "round": 2,
                    "agentic_step": 3,
                    "producer_scope": {"run_id": "run-1"},
                    "tokens_in": 500,
                    "cache_read_tokens": 500,
                }),
            ),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.95,
                warmup_turns: 0,
                warmup_rounds: 1,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(!result[0].passed, "conflicting mirrors must fail closed");
        assert!(result[0].detail.contains("4.55%"), "{}", result[0].detail);
    }

    #[test]
    fn provider_prompt_cache_ratio_rejects_ambiguous_warmup_units() {
        let criterion = Criterion::ProviderPromptCacheReadRatio {
            min: 0.95,
            warmup_turns: 1,
            warmup_rounds: 1,
        };
        let error = validate_criterion(&criterion).expect_err("warmup units must be explicit");
        assert!(
            error.contains("either warmup_turns or warmup_rounds"),
            "{error}"
        );
    }

    #[test]
    fn provider_prompt_cache_read_ratio_conflicting_mirrors_fail_closed_in_either_order() {
        let evaluate = |second_turn_first_is_cached: bool| {
            let cached = (
                "turn",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 20,
                    "cache_read_tokens": 980
                }),
            );
            let uncached = (
                "turn",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 1_000,
                    "cache_read_tokens": 0
                }),
            );
            let (first, second) = if second_turn_first_is_cached {
                (cached, uncached)
            } else {
                (uncached, cached)
            };
            let sess = mk_session(&[
                (
                    "turn",
                    serde_json::json!({
                        "turn": 1,
                        "tokens_in": 1_000,
                        "cache_read_tokens": 0
                    }),
                ),
                first,
                second,
            ]);
            evaluate_deterministic_with_session(
                &[Criterion::ProviderPromptCacheReadRatio {
                    min: 0.98,
                    warmup_turns: 1,
                    warmup_rounds: 0,
                }],
                &outcome_with_tools(&[]),
                Some(&sess),
            )
        };

        for cached_first in [true, false] {
            let result = evaluate(cached_first);
            assert!(
                !result[0].passed,
                "conflicting mirrors must not produce an order-dependent false pass: {:?}",
                result[0]
            );
            assert!(result[0].detail.contains("turns=1"), "{}", result[0].detail);
        }
    }

    #[test]
    fn provider_prompt_cache_read_ratio_deduplicates_identical_canonical_mirrors() {
        let sess = mk_session(&[
            (
                "turn",
                serde_json::json!({
                    "turn": 1,
                    "tokens_in": 1_000,
                    "cache_read_tokens": 0
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 20,
                    "cache_read_tokens": 980
                }),
            ),
            (
                "turn",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 20,
                    "cache_read_tokens": 980
                }),
            ),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.98,
                warmup_turns: 1,
                warmup_rounds: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(
            result[0].passed,
            "identical mirrors of one canonical turn must be counted once: {:?}",
            result[0]
        );
        assert!(result[0].detail.contains("turns=1"), "{}", result[0].detail);
    }

    #[test]
    fn provider_prompt_cache_warmup_skips_whole_legacy_turn_not_one_round() {
        let sess = mk_session(&[
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "tokens_in": 500,
                    "cache_read_tokens": 0
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 1,
                    "tokens_in": 500,
                    "cache_read_tokens": 0
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "turn": 2,
                    "tokens_in": 20,
                    "cache_read_tokens": 980
                }),
            ),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::ProviderPromptCacheReadRatio {
                min: 0.98,
                warmup_turns: 1,
                warmup_rounds: 0,
            }],
            &outcome_with_tools(&[]),
            Some(&sess),
        );

        assert!(result[0].passed, "{:?}", result[0]);
        assert!(result[0].detail.contains("turns=1"), "{}", result[0].detail);
    }

    // ── validate_criterion / validate_criteria (R3 #2) ──

    #[test]
    fn validate_tools_count_between_rejects_inverted_range() {
        let err = validate_criterion(&Criterion::ToolsCountBetween { min: 5, max: 2 })
            .expect_err("min>max should fail");
        assert!(err.contains("min (5) > max (2)"), "err = {err}");
    }

    #[test]
    fn validate_tools_count_between_accepts_equal_bounds() {
        // min == max means "exactly N calls" — a legitimate assertion.
        assert!(validate_criterion(&Criterion::ToolsCountBetween { min: 3, max: 3 }).is_ok());
    }

    #[test]
    fn validate_judger_rejects_out_of_range_threshold() {
        for t in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
            let c = Criterion::Judger {
                question: "q".into(),
                threshold: t,
                model: None,
            };
            assert!(
                validate_criterion(&c).is_err(),
                "threshold {t} should fail validation"
            );
        }
    }

    #[test]
    fn validate_judger_accepts_boundary_thresholds() {
        for t in [0.0, 0.5, 1.0] {
            let c = Criterion::Judger {
                question: "q".into(),
                threshold: t,
                model: None,
            };
            assert!(validate_criterion(&c).is_ok(), "threshold {t} should pass");
        }
    }

    #[test]
    fn validate_composite_criteria_reject_nested_judger() {
        let judger = Criterion::Judger {
            question: "q".into(),
            threshold: 0.7,
            model: None,
        };

        let any_err = validate_criterion(&Criterion::AnyOf {
            criteria: vec![judger.clone()],
        })
        .expect_err("judger is evaluated by the runner and cannot be nested");
        assert!(any_err.contains("Judger"));

        let all_err = validate_criterion(&Criterion::AllOf {
            criteria: vec![judger],
        })
        .expect_err("judger is evaluated by the runner and cannot be nested");
        assert!(all_err.contains("Judger"));
    }

    #[test]
    fn validate_composite_criteria_rejects_excessive_depth() {
        let mut criterion = Criterion::ExitCode { code: 0 };
        for _ in 0..=MAX_COMPOSITE_DEPTH {
            criterion = Criterion::AnyOf {
                criteria: vec![criterion],
            };
        }

        let err = validate_criterion(&criterion).expect_err("over-depth composite should fail");
        assert!(err.contains("max composite depth"), "err = {err}");
    }

    #[test]
    fn validate_session_event_count_rejects_min_zero() {
        let err = validate_criterion(&Criterion::SessionEventCount {
            event_type: "llm_round".into(),
            min: 0,
            optional: false,
        })
        .expect_err("min=0 is trivially-true — should reject");
        assert!(err.contains("min must be >= 1"));
    }

    #[test]
    fn validate_fork_cache_outcome_rejects_empty_expect() {
        let err = validate_criterion(&Criterion::ForkCacheOutcome { expect: vec![] })
            .expect_err("empty expect should fail");
        assert!(err.contains("must not be empty"));
    }

    #[test]
    fn validate_stderr_matches_rejects_bad_regex() {
        let err = validate_criterion(&Criterion::StderrMatches {
            pattern: "(".into(),
        })
        .expect_err("bad regex should fail at load");
        assert!(err.contains("invalid regex"));
    }

    #[test]
    fn validate_journal_tool_count_rejects_partial_or_invalid_structural_filter() {
        let partial = Criterion::JournalToolCallCount {
            name: "agent_fanout".into(),
            min: 1,
            max: 1,
            document: Some(JournalToolDocument::Arguments),
            path: Some("/action".into()),
            equals: None,
        };
        assert!(
            validate_criterion(&partial)
                .expect_err("partial predicate must fail closed")
                .contains("requires document, path, and equals together")
        );

        let invalid_pointer = Criterion::JournalToolCallCount {
            name: "agent_fanout".into(),
            min: 1,
            max: 1,
            document: Some(JournalToolDocument::Arguments),
            path: Some("action".into()),
            equals: Some(serde_json::json!("start")),
        };
        assert!(
            validate_criterion(&invalid_pointer)
                .expect_err("non-pointer path must fail closed")
                .contains("JSON pointer")
        );
    }

    #[test]
    fn validate_journal_tool_outcome_count_rejects_invalid_bounds() {
        let err = validate_criterion(&Criterion::JournalToolOutcomeCount {
            name: "start_work".into(),
            ok: false,
            min: 2,
            max: 1,
        })
        .expect_err("inverted typed-outcome bounds must fail at case load");
        assert!(err.contains("min (2) > max (1)"), "{err}");
    }

    #[test]
    fn validate_journal_tool_success_ratio_rejects_invalid_contract() {
        assert!(
            validate_criterion(&Criterion::JournalToolSuccessRatio {
                min: 1.01,
                min_calls: 1,
                allowed_failures: 0,
            })
            .is_err()
        );
        assert!(
            validate_criterion(&Criterion::JournalToolSuccessRatio {
                min: 0.8,
                min_calls: 0,
                allowed_failures: 0,
            })
            .is_err()
        );
        assert!(
            validate_criterion(&Criterion::JournalToolSuccessRatio {
                min: 0.8,
                min_calls: 1,
                allowed_failures: 1,
            })
            .is_err()
        );
    }

    #[test]
    fn validate_turn_evaluation_signal_count_rejects_invalid_contract() {
        let empty_kind = Criterion::JournalTurnEvaluationSignalCount {
            kind: " ".into(),
            min: 0,
            max: 0,
        };
        assert!(
            validate_criterion(&empty_kind)
                .expect_err("empty typed signal code must fail at case load")
                .contains("kind must not be empty")
        );

        let inverted_bounds = Criterion::JournalTurnEvaluationSignalCount {
            kind: "search_fanout".into(),
            min: 2,
            max: 1,
        };
        assert!(
            validate_criterion(&inverted_bounds)
                .expect_err("inverted typed-signal bounds must fail at case load")
                .contains("min (2) > max (1)")
        );
    }

    #[test]
    fn validate_rejects_empty_tool_name_and_empty_needle() {
        assert!(validate_criterion(&Criterion::ToolCalled { name: "   ".into() }).is_err());
        assert!(
            validate_criterion(&Criterion::JournalToolCalled {
                name: "".into(),
                optional: false
            })
            .is_err()
        );
        assert!(validate_criterion(&Criterion::TextContains { needle: "".into() }).is_err());
    }

    #[test]
    fn validate_criteria_reports_one_based_index_for_first_offender() {
        let criteria = vec![
            Criterion::ExitCode { code: 0 },
            Criterion::ToolsCountBetween { min: 5, max: 2 },
            Criterion::ExitCode { code: 1 }, // would also pass validation
        ];
        let err = validate_criteria(&criteria).expect_err("second criterion is bad");
        assert!(err.contains("criteria[2]"), "1-based index expected: {err}");
        assert!(err.contains("min (5) > max (2)"));
    }

    // ── ForkCacheOutcome tests ──

    fn outcome_with_stderr(stderr: &str) -> RunOutcome {
        let mut out = outcome_with_tools(&[]);
        out.stderr = stderr.to_string();
        out
    }

    #[test]
    fn fork_cache_outcome_passes_on_real_wire_shape() {
        // Real wire shape emitted by
        // `astra_turn_core::fork_cache_event::StderrForkCacheSink`.
        // The field is `outcome`, rename_all = snake_case.
        let out = outcome_with_stderr(
            "[fork-cache] {\"prefix_id\":\"pfx-1\",\"outcome\":\"hit\",\"ratio\":0.9}",
        );
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn fork_cache_outcome_accepts_legacy_class_alias() {
        // Back-compat: older harness tooling + a brief YAML window
        // used `class` as the field name. The parser still accepts
        // it so a rename on the consumer side doesn't silently
        // break cases that predate the rename.
        let out = outcome_with_stderr("[fork-cache] {\"class\":\"hit\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn fork_cache_outcome_rejects_unknown_shape_instead_of_guessing() {
        // Regression: the previous implementation fell back to "first
        // object key" for events it couldn't parse, which silently
        // misclassified `{"metadata":{...}}` or similar as a valid
        // class. Now: unknown shapes produce NO outcome, and the
        // criterion reports zero events seen rather than guessing.
        let out = outcome_with_stderr("[fork-cache] {\"prefix_id\":\"x\",\"metadata\":{}}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into(), "partial_drift".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(
            r[0].detail.contains("no [fork-cache]"),
            "unknown-shape events must not be fabricated into outcomes; detail = {:?}",
            r[0].detail
        );
    }

    #[test]
    fn fork_cache_outcome_fails_when_only_other_outcomes_seen() {
        let out = outcome_with_stderr("[fork-cache] {\"outcome\":\"miss\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("miss"));
    }

    #[test]
    fn fork_cache_outcome_fails_when_no_events_seen() {
        let out = outcome_with_stderr("unrelated noise");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("no [fork-cache]"));
    }

    #[test]
    fn fork_cache_outcome_ignores_malformed_event_and_uses_good_ones() {
        let out = outcome_with_stderr(
            "[fork-cache] this is not json\n[fork-cache] {\"outcome\":\"hit\"}\n",
        );
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into()],
            }],
            &out,
        );
        assert!(
            r[0].passed,
            "one malformed event must not mask a later valid hit"
        );
    }

    #[test]
    fn fork_cache_outcome_accepts_any_of_multiple_expected_values() {
        let out = outcome_with_stderr("[fork-cache] {\"outcome\":\"partial_drift\"}");
        let r = evaluate_deterministic(
            &[Criterion::ForkCacheOutcome {
                expect: vec!["hit".into(), "partial_drift".into()],
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn tokens_between_passes_in_range() {
        let mut out = RunOutcome::new("m");
        out.prompt_tokens = 100;
        out.completion_tokens = 200;
        let r = evaluate_deterministic(&[Criterion::TokensBetween { min: 200, max: 400 }], &out);
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn tokens_between_fails_over_max() {
        let mut out = RunOutcome::new("m");
        out.prompt_tokens = 5000;
        out.completion_tokens = 5000;
        let r = evaluate_deterministic(&[Criterion::TokensBetween { min: 0, max: 1000 }], &out);
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("10000"));
    }

    #[test]
    fn duration_between_passes_in_range() {
        let mut out = RunOutcome::new("m");
        out.duration_ms = 5000;
        let r = evaluate_deterministic(
            &[Criterion::DurationBetween {
                min_ms: 1000,
                max_ms: 10000,
            }],
            &out,
        );
        assert!(r[0].passed);
    }

    #[test]
    fn duration_between_fails_too_slow() {
        let mut out = RunOutcome::new("m");
        out.duration_ms = 60000;
        let r = evaluate_deterministic(
            &[Criterion::DurationBetween {
                min_ms: 0,
                max_ms: 30000,
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("60000"));
    }

    #[test]
    fn tool_sequence_passes_subsequence() {
        let out = RunOutcome::new("m").with_tools_used(vec![
            "bash".into(),
            "read_file".into(),
            "bash".into(),
            "str_replace".into(),
        ]);
        let r = evaluate_deterministic(
            &[Criterion::ToolSequence {
                tools: vec!["read_file".into(), "str_replace".into()],
            }],
            &out,
        );
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn tool_sequence_fails_wrong_order() {
        let out =
            RunOutcome::new("m").with_tools_used(vec!["str_replace".into(), "read_file".into()]);
        let r = evaluate_deterministic(
            &[Criterion::ToolSequence {
                tools: vec!["read_file".into(), "str_replace".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
    }

    #[test]
    fn tool_sequence_fails_missing_tool() {
        let out = RunOutcome::new("m").with_tools_used(vec!["bash".into()]);
        let r = evaluate_deterministic(
            &[Criterion::ToolSequence {
                tools: vec!["read_file".into(), "str_replace".into()],
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("matched 0/2"));
    }

    #[test]
    fn journal_tool_sequence_proves_durable_lifecycle_order() {
        let session = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {"tool_call_id": "work", "name": "start_work", "args": "{}"},
                    {"tool_call_id": "delegate", "name": "agent", "args": "{}"}
                ]
            }),
        )]);
        let outcome = outcome_with_tools(&[]);
        let required = Criterion::JournalToolSequence {
            tools: vec!["start_work".into(), "agent".into()],
        };
        let passed = evaluate_deterministic_with_session(
            std::slice::from_ref(&required),
            &outcome,
            Some(&session),
        );
        assert!(passed[0].passed, "{}", passed[0].detail);

        let reversed = evaluate_deterministic_with_session(
            &[Criterion::JournalToolSequence {
                tools: vec!["agent".into(), "start_work".into()],
            }],
            &outcome,
            Some(&session),
        );
        assert!(!reversed[0].passed, "{}", reversed[0].detail);

        let missing_capture = evaluate_deterministic(std::slice::from_ref(&required), &outcome);
        assert!(!missing_capture[0].passed);
    }

    #[test]
    fn journal_tool_precedence_rejects_delegation_before_work_exists() {
        let ordered = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {"tool_call_id": "work", "name": "start_work", "args": "{}"},
                    {"tool_call_id": "delegate", "name": "agent", "args": "{}"}
                ]
            }),
        )]);
        let unordered = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {"tool_call_id": "delegate", "name": "agent", "args": "{}"},
                    {"tool_call_id": "work", "name": "start_work", "args": "{}"}
                ]
            }),
        )]);
        let criterion = Criterion::JournalToolPrecedence {
            predecessor: "start_work".into(),
            successor: "agent".into(),
        };
        let outcome = outcome_with_tools(&[]);
        assert!(
            evaluate_deterministic_with_session(
                std::slice::from_ref(&criterion),
                &outcome,
                Some(&ordered),
            )[0]
            .passed
        );
        let rejected = evaluate_deterministic_with_session(
            std::slice::from_ref(&criterion),
            &outcome,
            Some(&unordered),
        );
        assert!(!rejected[0].passed);
        assert!(rejected[0].detail.contains("before start_work"));
    }

    #[test]
    fn journal_work_item_execution_proves_durable_ownership_not_prose() {
        let owned = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "ok": true,
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "inspect", "item_revision": 3}],
                            "runnable_items": [{"item_id": "inspect", "item_revision": 3}]
                        }
                    },
                    {
                        "tool_call_id": "execute",
                        "name": "run_next_work_item",
                        "ok": true,
                        "args": {},
                        "result": {
                            "status": "assigned",
                            "item_id": "inspect", "item_revision": 3
                        }
                    },
                    {
                        "tool_call_id": "settle",
                        "name": "settle_work_item",
                        "ok": true,
                        "result": {
                            "status": "recorded",
                            "item_id": "inspect", "item_revision": 3,
                            "outcome": "delivered"
                        }
                    }
                ]
            }),
        )]);
        let criterion = Criterion::JournalWorkItemExecutionFromStart {
            min_distinct_items: 1,
        };
        let outcome = outcome_with_tools(&[]);
        let passed = evaluate_deterministic_with_session(
            std::slice::from_ref(&criterion),
            &outcome,
            Some(&owned),
        );
        assert!(passed[0].passed, "{}", passed[0].detail);

        let requires_two = Criterion::JournalWorkItemExecutionFromStart {
            min_distinct_items: 2,
        };
        let insufficient =
            evaluate_deterministic_with_session(&[requires_two], &outcome, Some(&owned));
        assert!(!insufficient[0].passed, "{}", insufficient[0].detail);
        assert!(insufficient[0].detail.contains("only 1 distinct"));

        let missing_server_receipt = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "runnable_items": [{"item_id": "task-1", "item_revision": 1}],
                            "initial_task": {
                                "status": "assigned",
                                "item_id": "task-1", "item_revision": 1
                            }
                        }
                    },
                    {
                        "tool_call_id": "settle",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "task-1", "item_revision": 1
                        }
                    }
                ]
            }),
        )]);
        let rejected = evaluate_deterministic_with_session(
            std::slice::from_ref(&criterion),
            &outcome,
            Some(&missing_server_receipt),
        );
        assert!(!rejected[0].passed, "{}", rejected[0].detail);
        assert!(
            rejected[0]
                .detail
                .contains("omitted its server-issued declared task identities")
        );

        let atomically_advanced = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "declared_tasks": [
                                {"item_id": "task-1", "item_revision": 1},
                                {"item_id": "task-2", "item_revision": 1}
                            ],
                            "runnable_items": [{"item_id": "task-1", "item_revision": 1}],
                            "initial_task": {
                                "status": "assigned",
                                "item_id": "task-1", "item_revision": 1
                            }
                        }
                    },
                    {
                        "tool_call_id": "settle-and-advance",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "task-1", "item_revision": 1,
                            "next_task": {
                                "status": "assigned",
                                "item_id": "task-2", "item_revision": 1
                            }
                        }
                    },
                    {
                        "tool_call_id": "settle-final",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "task-2", "item_revision": 1,
                            "next_task": null
                        }
                    }
                ]
            }),
        )]);
        let advanced = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkItemExecutionFromStart {
                min_distinct_items: 2,
            }],
            &outcome,
            Some(&atomically_advanced),
        );
        assert!(advanced[0].passed, "{}", advanced[0].detail);

        let idempotently_resumed = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "task-1", "item_revision": 1}],
                            "runnable_items": [{"item_id": "task-1", "item_revision": 1}],
                            "initial_task": {
                                "status": "assigned",
                                "item_id": "task-1", "item_revision": 1
                            }
                        }
                    },
                    {
                        "tool_call_id": "resume",
                        "name": "run_next_work_item",
                        "result": {
                            "status": "assigned",
                            "execution": "primary_session_resumed",
                            "item_id": "task-1", "item_revision": 1
                        }
                    },
                    {
                        "tool_call_id": "settle",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "task-1", "item_revision": 1
                        }
                    }
                ]
            }),
        )]);
        let resumed = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkItemExecutionFromStart {
                min_distinct_items: 1,
            }],
            &outcome,
            Some(&idempotently_resumed),
        );
        assert!(resumed[0].passed, "{}", resumed[0].detail);

        let undeclared_successor = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "task-1", "item_revision": 1}],
                            "runnable_items": [{"item_id": "task-1", "item_revision": 1}],
                            "initial_task": {
                                "status": "assigned",
                                "item_id": "task-1", "item_revision": 1
                            }
                        }
                    },
                    {
                        "tool_call_id": "settle-and-invent",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "task-1", "item_revision": 1,
                            "next_task": {
                                "status": "assigned",
                                "item_id": "invented", "item_revision": 1
                            }
                        }
                    },
                    {
                        "tool_call_id": "settle-invented",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "invented", "item_revision": 1
                        }
                    }
                ]
            }),
        )]);
        let rejected = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkItemExecutionFromStart {
                min_distinct_items: 1,
            }],
            &outcome,
            Some(&undeclared_successor),
        );
        assert!(!rejected[0].passed, "{}", rejected[0].detail);
        assert!(
            rejected[0]
                .detail
                .contains("undeclared WorkItem invented@1")
        );

        let wrong_revision = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "inspect", "item_revision": 3}],
                            "runnable_items": [{"item_id": "inspect", "item_revision": 3}]
                        }
                    },
                    {
                        "tool_call_id": "execute",
                        "name": "run_next_work_item",
                        "args": {},
                        "result": {
                            "status": "assigned",
                            "item_id": "inspect", "item_revision": 2
                        },
                    },
                    {
                        "tool_call_id": "settle",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "inspect", "item_revision": 2,
                            "outcome": "delivered"
                        }
                    }
                ]
            }),
        )]);
        let rejected =
            evaluate_deterministic_with_session(&[criterion], &outcome, Some(&wrong_revision));
        assert!(!rejected[0].passed, "{}", rejected[0].detail);
        assert!(rejected[0].detail.contains("inspect@2"));

        let abandoned = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "inspect", "item_revision": 3}],
                            "runnable_items": [{"item_id": "inspect", "item_revision": 3}]
                        }
                    },
                    {
                        "tool_call_id": "execute",
                        "name": "run_next_work_item",
                        "result": {
                            "status": "assigned",
                            "item_id": "inspect", "item_revision": 3
                        }
                    }
                ]
            }),
        )]);
        let rejected = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkItemExecutionFromStart {
                min_distinct_items: 1,
            }],
            &outcome,
            Some(&abandoned),
        );
        assert!(!rejected[0].passed, "{}", rejected[0].detail);
        assert!(rejected[0].detail.contains("assigned but never settled"));

        let mismatched_settlement = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "work",
                        "name": "start_work",
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "inspect", "item_revision": 3}],
                            "runnable_items": [{"item_id": "inspect", "item_revision": 3}]
                        }
                    },
                    {
                        "tool_call_id": "execute",
                        "name": "run_next_work_item",
                        "result": {
                            "status": "assigned",
                            "item_id": "inspect", "item_revision": 3
                        }
                    },
                    {
                        "tool_call_id": "settle",
                        "name": "settle_work_item",
                        "result": {
                            "status": "recorded",
                            "item_id": "other", "item_revision": 1
                        }
                    }
                ]
            }),
        )]);
        let rejected = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkItemExecutionFromStart {
                min_distinct_items: 1,
            }],
            &outcome,
            Some(&mismatched_settlement),
        );
        assert!(!rejected[0].passed, "{}", rejected[0].detail);
        assert!(
            rejected[0]
                .detail
                .contains("does not match active assignment")
        );
    }

    #[test]
    fn journal_work_item_execution_reports_post_start_wire_surface_gap() {
        let capture = mk_session(&[
            (
                "llm_round",
                serde_json::json!({
                    "round": 0,
                    "metadata": {
                        "purpose": "primary_agent",
                        "visible_tools": ["start_work", "web_fetch"]
                    },
                    "tool_calls": [{
                        "tool_call_id": "start",
                        "name": "start_work",
                        "ok": true,
                        "result": {
                            "status": "started",
                            "declared_tasks": [{"item_id": "task-1", "item_revision": 1}],
                            "runnable_items": [{"item_id": "task-1", "item_revision": 1}],
                            "initial_task": {
                                "status": "assigned",
                                "item_id": "task-1",
                                "item_revision": 1
                            }
                        }
                    }]
                }),
            ),
            (
                "llm_round",
                serde_json::json!({
                    "round": 1,
                    "metadata": {
                        "purpose": "primary_agent",
                        "visible_tools": ["web_fetch", "tool_search"]
                    },
                    "tool_calls": []
                }),
            ),
        ]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkItemExecutionFromStart {
                min_distinct_items: 1,
            }],
            &outcome_with_tools(&[]),
            Some(&capture),
        );

        assert!(!result[0].passed, "{result:?}");
        assert!(
            result[0]
                .detail
                .contains("wire surface omitted settle_work_item"),
            "the harness must report the first broken provider boundary, not only the later abandoned assignment: {result:?}"
        );
    }

    #[test]
    fn journal_work_graph_patch_proves_accepted_structural_replan() {
        let capture = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "start",
                        "name": "start_work",
                        "ok": true,
                        "result": {"status": "started"}
                    },
                    {
                        "tool_call_id": "replan",
                        "name": "propose_work_plan",
                        "ok": true,
                        "args": {
                            "additions": [{"item_id": "replacement"}],
                            "revisions": [
                                {"item_id": "obsolete", "declaration_state": "cancelled"}
                            ],
                            "dependencies": [],
                            "dependency_removals": [{"predecessor_item_id": "obsolete"}]
                        },
                        "result": {"status": "accepted", "result_graph_revision": 2}
                    }
                ]
            }),
        )]);
        let criterion = Criterion::JournalWorkGraphPatch {
            require_addition: true,
            require_active_revision: false,
            require_retired_revision: true,
            require_cancelled_revision: false,
            require_superseded_revision: false,
            require_dependency_change: true,
            require_atomic_retire_and_add: true,
        };
        let outcome = outcome_with_tools(&[]);
        let passed = evaluate_deterministic_with_session(
            std::slice::from_ref(&criterion),
            &outcome,
            Some(&capture),
        );
        assert!(passed[0].passed, "{}", passed[0].detail);

        let before_work = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [{
                    "tool_call_id": "replan",
                    "name": "propose_work_plan",
                    "ok": true,
                    "args": {
                        "additions": [{"item_id": "replacement"}],
                        "revisions": [],
                        "dependencies": [],
                        "dependency_removals": []
                    },
                    "result": {"status": "accepted"}
                }]
            }),
        )]);
        let rejected = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkGraphPatch {
                require_addition: true,
                require_active_revision: false,
                require_retired_revision: false,
                require_cancelled_revision: false,
                require_superseded_revision: false,
                require_dependency_change: false,
                require_atomic_retire_and_add: false,
            }],
            &outcome,
            Some(&before_work),
        );
        assert!(!rejected[0].passed, "{}", rejected[0].detail);
        assert!(rejected[0].detail.contains("before canonical Work"));
    }

    #[test]
    fn journal_work_graph_patch_can_match_collective_mutation_sequence() {
        let capture = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "start",
                        "name": "start_work",
                        "ok": true,
                        "result": {"status": "started"}
                    },
                    {
                        "tool_call_id": "invalid-combined-attempt",
                        "name": "propose_work_plan",
                        "ok": false,
                        "args": {
                            "additions": [{"item_id": "obsolete"}],
                            "revisions": [
                                {"item_id": "obsolete", "declaration_state": "superseded"}
                            ],
                            "dependencies": [],
                            "dependency_removals": []
                        },
                        "result": {"status": "error"}
                    },
                    {
                        "tool_call_id": "retire",
                        "name": "propose_work_plan",
                        "ok": true,
                        "args": {
                            "additions": [],
                            "revisions": [
                                {"item_id": "obsolete", "declaration_state": "superseded"}
                            ],
                            "dependencies": [],
                            "dependency_removals": []
                        },
                        "result": {"status": "accepted", "result_graph_revision": 2}
                    },
                    {
                        "tool_call_id": "replace",
                        "name": "propose_work_plan",
                        "ok": true,
                        "args": {
                            "additions": [{"item_id": "replacement"}],
                            "revisions": [],
                            "dependencies": [],
                            "dependency_removals": []
                        },
                        "result": {"status": "accepted", "result_graph_revision": 3}
                    }
                ]
            }),
        )]);
        let result = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkGraphPatch {
                require_addition: true,
                require_active_revision: false,
                require_retired_revision: true,
                require_cancelled_revision: false,
                require_superseded_revision: false,
                require_dependency_change: false,
                require_atomic_retire_and_add: false,
            }],
            &outcome_with_tools(&[]),
            Some(&capture),
        );
        assert!(result[0].passed, "{}", result[0].detail);
        assert!(
            result[0].detail.contains("2 accepted"),
            "{}",
            result[0].detail
        );

        let atomic = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkGraphPatch {
                require_addition: true,
                require_active_revision: false,
                require_retired_revision: true,
                require_cancelled_revision: false,
                require_superseded_revision: false,
                require_dependency_change: false,
                require_atomic_retire_and_add: true,
            }],
            &outcome_with_tools(&[]),
            Some(&capture),
        );
        assert!(!atomic[0].passed, "split proposals are not atomic");
        assert!(atomic[0].detail.contains("atomic retire-and-add"));
    }

    #[test]
    fn journal_work_graph_patch_keeps_exact_retirement_state_atomic() {
        let capture = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "start",
                        "name": "start_work",
                        "ok": true,
                        "result": {"status": "started"}
                    },
                    {
                        "tool_call_id": "cancel-separately",
                        "name": "propose_work_plan",
                        "ok": true,
                        "args": {
                            "additions": [],
                            "revisions": [
                                {"item_id": "cancelled", "declaration_state": "cancelled"}
                            ],
                            "dependencies": [],
                            "dependency_removals": []
                        },
                        "result": {"status": "accepted", "result_graph_revision": 2}
                    },
                    {
                        "tool_call_id": "add-with-different-retirement",
                        "name": "propose_work_plan",
                        "ok": true,
                        "args": {
                            "additions": [{"item_id": "fresh"}],
                            "revisions": [
                                {"item_id": "superseded", "declaration_state": "superseded"}
                            ],
                            "dependencies": [],
                            "dependency_removals": []
                        },
                        "result": {"status": "accepted", "result_graph_revision": 3}
                    }
                ]
            }),
        )]);

        let result = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkGraphPatch {
                require_addition: true,
                require_active_revision: false,
                require_retired_revision: true,
                require_cancelled_revision: true,
                require_superseded_revision: false,
                require_dependency_change: false,
                require_atomic_retire_and_add: true,
            }],
            &outcome_with_tools(&[]),
            Some(&capture),
        );

        assert!(
            !result[0].passed,
            "cancellation must be atomic with addition"
        );
        assert!(result[0].detail.contains("atomic retire-and-add"));
    }

    #[test]
    fn journal_work_graph_patch_treats_supersession_as_a_retired_revision() {
        let capture = mk_session(&[(
            "turn",
            serde_json::json!({
                "tool_calls": [
                    {
                        "tool_call_id": "start",
                        "name": "start_work",
                        "ok": true,
                        "result": {"status": "started"}
                    },
                    {
                        "tool_call_id": "replan",
                        "name": "propose_work_plan",
                        "ok": true,
                        "args": {
                            "additions": [],
                            "revisions": [
                                {"item_id": "narrowed", "declaration_state": "superseded"}
                            ],
                            "dependencies": [],
                            "dependency_removals": []
                        },
                        "result": {"status": "accepted", "result_graph_revision": 2}
                    }
                ]
            }),
        )]);
        let outcome = outcome_with_tools(&[]);
        let results = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkGraphPatch {
                require_addition: false,
                require_active_revision: false,
                require_retired_revision: true,
                require_cancelled_revision: false,
                require_superseded_revision: true,
                require_dependency_change: false,
                require_atomic_retire_and_add: false,
            }],
            &outcome,
            Some(&capture),
        );
        assert!(results[0].passed, "{}", results[0].detail);

        let cancellation = evaluate_deterministic_with_session(
            &[Criterion::JournalWorkGraphPatch {
                require_addition: false,
                require_active_revision: false,
                require_retired_revision: false,
                require_cancelled_revision: true,
                require_superseded_revision: false,
                require_dependency_change: false,
                require_atomic_retire_and_add: false,
            }],
            &outcome,
            Some(&capture),
        );
        assert!(
            !cancellation[0].passed,
            "supersession must not satisfy exact cancellation: {}",
            cancellation[0].detail
        );
    }

    #[test]
    fn journal_work_graph_patch_rejects_empty_requirement() {
        let error = validate_criterion(&Criterion::JournalWorkGraphPatch {
            require_addition: false,
            require_active_revision: false,
            require_retired_revision: false,
            require_cancelled_revision: false,
            require_superseded_revision: false,
            require_dependency_change: false,
            require_atomic_retire_and_add: false,
        })
        .expect_err("an unconstrained graph patch assertion would prove nothing");
        assert!(error.contains("at least one mutation dimension"));
    }

    #[test]
    fn turn_rounds_between_passes() {
        let mut out = RunOutcome::new("m");
        out.turn_rounds = 2;
        let r = evaluate_deterministic(&[Criterion::TurnRoundsBetween { min: 1, max: 3 }], &out);
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn turn_rounds_between_fails_too_many() {
        let mut out = RunOutcome::new("m");
        out.turn_rounds = 10;
        let r = evaluate_deterministic(&[Criterion::TurnRoundsBetween { min: 1, max: 3 }], &out);
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("10"));
    }

    #[test]
    fn cache_rate_above_passes() {
        let mut out = RunOutcome::new("m");
        out.total_tool_calls = 10;
        out.cache_hits = 8;
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.5,
                min_calls: 1,
            }],
            &out,
        );
        assert!(r[0].passed, "{}", r[0].detail);
    }

    #[test]
    fn cache_rate_above_fails() {
        let mut out = RunOutcome::new("m");
        out.total_tool_calls = 10;
        out.cache_hits = 1;
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.5,
                min_calls: 1,
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("10.0%"));
    }

    #[test]
    fn cache_rate_above_fails_when_no_tools_and_min_calls_set() {
        let out = RunOutcome::new("m");
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.9,
                min_calls: 1,
            }],
            &out,
        );
        assert!(!r[0].passed, "min_calls=1 with 0 tool calls must FAIL");
        assert!(r[0].detail.contains("too few tool calls"));
    }

    #[test]
    fn cache_rate_above_skip_passes_no_tools_when_min_calls_zero() {
        let out = RunOutcome::new("m");
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.9,
                min_calls: 0,
            }],
            &out,
        );
        assert!(r[0].passed, "min_calls=0 with no tools should skip-pass");
    }

    #[test]
    fn cache_rate_above_fails_when_step_events_missing() {
        let mut out = RunOutcome::new("m");
        out.tool_calls_count = 3; // envelope says tools were called
        out.total_tool_calls = 0; // but step_events weren't parsed
        let r = evaluate_deterministic(
            &[Criterion::CacheRateAbove {
                threshold: 0.5,
                min_calls: 1,
            }],
            &out,
        );
        assert!(!r[0].passed);
        assert!(r[0].detail.contains("step_events missing"));
    }

    #[test]
    fn prompt_cache_tokens_requires_read_and_creation_buckets() {
        let c = Criterion::PromptCacheTokens {
            min_read: 10,
            min_creation: 5,
            max_creation: None,
        };
        let mut out = RunOutcome::new("m");
        out.cached_input_tokens = 12;
        out.cache_creation_tokens = 5;
        let r = evaluate_one(&c, &out, None);
        assert!(r.passed, "{r:?}");

        out.cached_input_tokens = 9;
        let r = evaluate_one(&c, &out, None);
        assert!(!r.passed, "{r:?}");
    }

    /// `max_creation` flags excessive cache-rebuild. Catches the failure mode
    /// where cache_read is healthy but cache_creation is also huge — i.e. the
    /// prefix is hitting partially but something after the marker is forcing
    /// re-creation every turn (silent 40-60% hit-rate regressions).
    #[test]
    fn prompt_cache_tokens_max_creation_catches_churn() {
        let c = Criterion::PromptCacheTokens {
            min_read: 10000,
            min_creation: 0,
            max_creation: Some(15_000),
        };
        let mut out = RunOutcome::new("m");

        // Healthy: read well past min, creation within ceiling.
        out.cached_input_tokens = 30_000;
        out.cache_creation_tokens = 4_000;
        let r = evaluate_one(&c, &out, None);
        assert!(r.passed, "healthy cache should pass: {r:?}");

        // Regression: plenty of reads, but creation explodes — partial cache hit.
        out.cached_input_tokens = 30_000;
        out.cache_creation_tokens = 25_000;
        let r = evaluate_one(&c, &out, None);
        assert!(
            !r.passed,
            "max_creation must fire when cache creation exceeds the ceiling: {r:?}"
        );
        assert!(
            r.detail.contains("25000") && r.detail.contains("15000"),
            "detail must surface both observed and ceiling: {}",
            r.detail
        );
    }

    #[test]
    fn prompt_cache_tokens_max_creation_defaults_to_unbounded() {
        // YAML case omitting `max_creation` must remain backward-compatible.
        let c = Criterion::PromptCacheTokens {
            min_read: 10,
            min_creation: 0,
            max_creation: None,
        };
        let mut out = RunOutcome::new("m");
        out.cached_input_tokens = 100;
        out.cache_creation_tokens = 999_999;
        let r = evaluate_one(&c, &out, None);
        assert!(
            r.passed,
            "unlimited creation must pass when max_creation is None: {r:?}"
        );
    }

    #[test]
    fn prompt_cache_tokens_validation_catches_inverted_bounds() {
        // min_creation > max_creation is unreachable — validation must reject
        // it so YAML authors notice typos at parse time, not run time.
        let c = Criterion::PromptCacheTokens {
            min_read: 100,
            min_creation: 5_000,
            max_creation: Some(1_000),
        };
        let err = validate_criterion(&c).unwrap_err();
        assert!(
            err.contains("max_creation"),
            "validate must mention the offending field: {err}"
        );
    }

    #[test]
    fn provider_prompt_cache_read_ratio_rejects_invalid_min() {
        for min in [-0.01, 1.01, f64::NAN, f64::INFINITY] {
            let criterion = Criterion::ProviderPromptCacheReadRatio {
                min,
                warmup_turns: 1,
                warmup_rounds: 0,
            };
            let error = validate_criterion(&criterion).expect_err("invalid ratio must fail");
            assert!(error.contains("finite in [0.0, 1.0]"), "{error}");
        }
    }
}
