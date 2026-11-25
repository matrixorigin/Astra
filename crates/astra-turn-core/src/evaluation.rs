//! Tool outcome evidence, reconciliation, and behavioral diagnostics.

use crate::orchestration::agent_result_wire::{
    AGENT_RESULT_CLASS_AGENT_INCOMPLETE as RESULT_CLASS_AGENT_INCOMPLETE,
    AGENT_RESULT_CLASS_FANOUT_INCOMPLETE as RESULT_CLASS_FANOUT_INCOMPLETE,
    agent_fanout_result_looks_like, agent_fanout_structured_result_class,
    agent_tool_result_looks_like, agent_tool_structured_result_class,
};
use astra_services::session_journal::{JournalEvent, ToolCallRecord};
use serde::Deserialize;
use serde_json::Value;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};

pub const LLM_ROUND_CHURN_THRESHOLD: usize = 8;
pub const EXPLORATION_FAMILY_CHURN_THRESHOLD: usize = 3;

/// A known command exit is useful observation evidence. It cannot certify
/// validation or settle an earlier failure. Keep explicit validation strict.
fn record_is_observed_shell_exit(record: &ToolCallRecord) -> bool {
    !record.ok
        && record.execution_ok.is_none()
        && record.was_executed()
        && !record.outcome_is_unknown()
        && record.result_class.as_deref() == Some("execution_error")
        && matches!(
            record.error_kind,
            None | Some(astra_core::ErrorKind::Unknown)
        )
        && record
            .recovery_evidence
            .as_ref()
            .is_some_and(astra_core::ToolFailureEvidence::is_observed_command_exit)
        && crate::tool::categories::registry().is_shell(&record.name)
        && record
            .authoritative_args_full()
            .is_some_and(|args| normalize_validation_attempt_prefix(&record.name, args).is_none())
}

/// Whether a retained failed record represents a completion-critical outcome.
/// This shared semantic predicate is used by the CLI status surface and by
/// runtime lifecycle projection. A read-only probe stays advisory even when
/// it is the only failed record; mutating tools, recognized validation, and
/// unfinished child execution remain strict.
pub fn tool_outcome_requires_terminal_attention(
    record: &ToolCallRecord,
    result_class: &str,
) -> bool {
    if matches!(
        result_class,
        "test_failure" | RESULT_CLASS_AGENT_INCOMPLETE | RESULT_CLASS_FANOUT_INCOMPLETE
    ) || record.outcome_is_unknown()
        || (record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Rejected
            && result_class == "execution_error")
    {
        return true;
    }
    if record_is_observed_shell_exit(record) {
        return false;
    }
    let args = record
        .authoritative_args_full()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    if normalize_validation_attempt_prefix(
        &record.name,
        record.authoritative_args_full().unwrap_or(""),
    )
    .is_some()
    {
        return true;
    }
    // Observation failures inform the next decision without implying unfinished
    // task effects. Unknown tools/effects retain their execution obligation.
    !crate::tool::categories::registry().is_read_only_for_recovery(&record.name, args.as_ref())
}

fn result_class_is_outcome_failure(class: &str) -> bool {
    matches!(
        class,
        "test_failure"
            | "env_failure"
            | "execution_error"
            | RESULT_CLASS_AGENT_INCOMPLETE
            | RESULT_CLASS_FANOUT_INCOMPLETE
    )
}

fn result_class_resolves_outcome_failure(class: &str) -> bool {
    matches!(class, "success" | "domain_negative")
}

/// Whether a non-zero process result is explicitly a domain outcome rather
/// than an execution failure. `ToolCallRecord::ok` is intentionally not the
/// sole source of truth: nonzero shell exits can be useful observations. An
/// explicit typed failure class still wins over the process-level semantic.
fn record_is_non_failure_outcome(record: &ToolCallRecord, class: Option<&str>) -> bool {
    if let Some(class) = class {
        if result_class_is_outcome_failure(class) {
            return false;
        }
        if result_class_resolves_outcome_failure(class) {
            return true;
        }
    }
    if record.ok {
        return false;
    }
    matches!(
        record.exit_semantics.as_deref().and_then(|tag| {
            serde_json::from_value::<astra_tools::exit_semantics::ExitSemantics>(
                serde_json::Value::String(tag.to_string()),
            )
            .ok()
        }),
        Some(
            astra_tools::exit_semantics::ExitSemantics::Success
                | astra_tools::exit_semantics::ExitSemantics::EmptyResult
                | astra_tools::exit_semantics::ExitSemantics::DomainNegative
        )
    )
}

fn hash_bounded_key(value: &str) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Stable identity of the governed operation, independent of result class,
/// disposition, provider attempt id, or JSON object key order. Outcome
/// classifiers are ledger values; they must never change the operation key
/// used to correlate a failure with a later retry. Display previews and call
/// IDs cannot establish command equality after arguments have been redacted.
fn operation_identity_key(record: &ToolCallRecord) -> Option<String> {
    let args = record.authoritative_args_full()?.trim();
    // A validation prefix drops target details; preserve the complete request
    // rather than clearing an unrelated verification failure.
    (!args.is_empty()).then(|| operation_args_identity(&record.name, args))
}

fn operation_args_identity(name: &str, args: &str) -> String {
    format!(
        "tool::{name}::args::{}",
        hash_bounded_key(&crate::stall::canonical_tool_args(args))
    )
}

pub fn unresolved_tool_outcome_failure_counts(
    records: &[ToolCallRecord],
    resolved_children: &[astra_turn_types::task_resolution::ToolExecutionEvidenceRef],
) -> std::collections::BTreeMap<String, usize> {
    unresolved_tool_outcome_fact_counts(
        &records
            .iter()
            .map(|record| {
                let mut fact = ToolEvaluationFact::from_record(record);
                if matches!(
                    (record.name.as_str(), fact.effective_result_class.as_deref()),
                    ("agent", Some(RESULT_CLASS_AGENT_INCOMPLETE))
                        | ("agent_fanout", Some(RESULT_CLASS_FANOUT_INCOMPLETE))
                ) && record.ok
                    && record
                        .execution_completion
                        .as_ref()
                        .is_some_and(|reference| {
                            resolved_children
                                .iter()
                                .any(|resolved| resolved == reference)
                        })
                {
                    fact.effective_result_class = Some("success".into());
                    fact.non_failure_outcome = true;
                }
                fact
            })
            .collect::<Vec<_>>(),
    )
}

fn unresolved_tool_outcome_fact_counts(
    facts: &[ToolEvaluationFact],
) -> std::collections::BTreeMap<String, usize> {
    let mut counts = std::collections::BTreeMap::new();
    for failure in unresolved_execution_outcome_facts(facts).values() {
        *counts.entry(failure.result_class.clone()).or_insert(0) += 1;
    }
    counts
}

/// The last unresolved execution for an operation in the supplied fact window.
/// Indices are window-local; they confer no durable invocation authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedToolOutcome {
    pub fact_index: usize,
    pub result_class: String,
    pub invocation: Option<astra_turn_types::task_resolution::ToolExecutionEvidenceRef>,
}

/// Shared source for both failure counts and precise reconciliation coverage.
/// Opaque failures remain in this result and never match a later success.
pub fn unresolved_tool_outcome_facts(
    facts: &[ToolEvaluationFact],
) -> std::collections::BTreeMap<EvaluationOutcomeKey, UnresolvedToolOutcome> {
    reduce_tool_outcome_facts(facts, |_| true)
}

fn unresolved_execution_outcome_facts(
    facts: &[ToolEvaluationFact],
) -> std::collections::BTreeMap<EvaluationOutcomeKey, UnresolvedToolOutcome> {
    reduce_tool_outcome_facts(facts, |fact| fact.requires_terminal_attention)
}

fn reduce_tool_outcome_facts(
    facts: &[ToolEvaluationFact],
    include_failure: impl Fn(&ToolEvaluationFact) -> bool,
) -> std::collections::BTreeMap<EvaluationOutcomeKey, UnresolvedToolOutcome> {
    let mut unresolved_by_key = std::collections::BTreeMap::new();

    for (record_index, record) in facts.iter().enumerate() {
        let disposition = record.disposition;
        if !matches!(
            disposition,
            astra_services::session_journal::ToolCallDisposition::Executed
                | astra_services::session_journal::ToolCallDisposition::Unknown
                | astra_services::session_journal::ToolCallDisposition::Rejected
        ) {
            continue;
        }

        // A rejected domain carrier never began that domain execution. Its
        // rendered payload can still look like `agent_incomplete`; do not
        // charge the request once as blocked and again as unfinished work.
        // Preserve the one rejected class that is itself execution evidence:
        // `execution_error` means route/launch setup was attempted and failed
        // before the executor could report an Executed disposition.
        if disposition == astra_services::session_journal::ToolCallDisposition::Rejected
            && record.effective_result_class.as_deref() != Some("execution_error")
        {
            continue;
        }

        // Filter failure evidence before it can replace a prior obligation.
        // An observational failure neither resolves an unknown outcome nor
        // proves recovery. Typed domain failures still win over envelope ok.
        if record.has_failed_outcome() && !include_failure(record) {
            continue;
        }

        // A governed execution may fail before it can attach a structured
        // result class (for example a process launch, transport, or sandbox
        // failure). `ok=false` is already the typed terminal fact at this
        // boundary; do not silently lose it just because the optional
        // classifier was unavailable. Rejected requests remain a separate
        // signal and must not be double-counted as unresolved execution.
        if disposition == astra_services::session_journal::ToolCallDisposition::Executed
            && !record.ok
            && !record.non_failure_outcome
        {
            let key = record.outcome_key(record_index);
            let class = record
                .effective_result_class
                .clone()
                .unwrap_or_else(|| "tool_failure".to_string());
            unresolved_by_key.insert(
                key,
                UnresolvedToolOutcome {
                    fact_index: record_index,
                    result_class: class,
                    invocation: record.execution_completion.clone(),
                },
            );
            continue;
        }

        // A successful unclassified execution resolves an earlier fallback
        // failure for the same governed operation. Typed classes below still
        // provide the more precise resolution path when available.
        if record.ok {
            if let Some(key) = record.operation_identity {
                unresolved_by_key.remove(&EvaluationOutcomeKey::Stable(key));
            }
        }
        let Some(class) = &record.effective_result_class else {
            continue;
        };
        let key = record.outcome_key(record_index);

        if result_class_is_outcome_failure(class) {
            unresolved_by_key.insert(
                key,
                UnresolvedToolOutcome {
                    fact_index: record_index,
                    result_class: class.clone(),
                    invocation: record.execution_completion.clone(),
                },
            );
        } else if result_class_resolves_outcome_failure(class) {
            unresolved_by_key.remove(&key);
        }
    }

    unresolved_by_key
}

/// Return active failures that are strong enough to affect a terminal CLI
/// outcome, using the same reduction as runtime policy and evaluation counts.
/// Opaque failures remain distinct because no later operation can establish
/// their identity. Observation-only errors remain in the raw audit projection.
pub fn active_terminal_execution_failure_operation_keys(
    records: &[ToolCallRecord],
) -> std::collections::BTreeSet<String> {
    let facts: Vec<_> = records
        .iter()
        .map(ToolEvaluationFact::from_record)
        .collect();
    unresolved_execution_outcome_facts(&facts)
        .values()
        .map(|failure| {
            operation_identity_key(&records[failure.fact_index])
                .unwrap_or_else(|| format!("opaque-terminal::{}", failure.fact_index))
        })
        .collect()
}

/// A lost mutation result remains unresolved because repeating a mutation
/// cannot establish the first outcome. Reads can be retried or replaced by
/// another observation; leave that choice to the agent.
pub fn has_unresolved_tool_result_delivery_failure(records: &[ToolCallRecord]) -> bool {
    records.iter().any(|record| {
        if !record.was_executed() || record.ok || record.execution_ok.is_none() {
            return false;
        }
        let args = record
            .authoritative_args_full()
            .and_then(|args| serde_json::from_str(args).ok());
        !crate::tool::categories::registry().is_read_only_for_recovery(&record.name, args.as_ref())
    })
}

pub fn active_execution_failure_fact_keys(
    facts: &[ToolEvaluationFact],
) -> std::collections::BTreeSet<EvaluationIdentity> {
    unresolved_execution_outcome_facts(facts)
        .into_keys()
        .filter_map(|key| match key {
            EvaluationOutcomeKey::Stable(identity) => Some(identity),
            EvaluationOutcomeKey::Opaque { .. } => None,
        })
        .collect()
}

/// Return currently active rejected-operation obligations using the same
/// exact-argument resolution key as terminal evaluation.
pub fn active_rejected_operation_keys(
    records: &[ToolCallRecord],
) -> std::collections::BTreeSet<String> {
    active_rejected_keys(records.iter().enumerate().map(|(index, record)| {
        (
            record_is_rejected_attempt(record),
            record.was_executed() && record.ok,
            blocked_resolution_key(record, index),
        )
    }))
}

fn active_rejected_keys<K: Ord>(
    facts: impl IntoIterator<Item = (bool, bool, K)>,
) -> std::collections::BTreeSet<K> {
    let mut unresolved = std::collections::BTreeSet::new();
    for (rejected, executed_ok, key) in facts {
        if rejected {
            unresolved.insert(key);
        } else if executed_ok {
            unresolved.remove(&key);
        }
    }
    unresolved
}

/// Stable identity for a rejected request when its arguments or provider call
/// id make a later exact execution matchable. Opaque rejected requests remain
/// terminal evidence but cannot be safely used for cause-level persistence.
pub fn rejected_operation_key(record: &ToolCallRecord) -> Option<String> {
    operation_identity_key(record)
}

/// Number of currently unresolved, typed tool outcomes.
///
/// This is the online-safe projection of the same failure ledger used by
/// terminal turn evaluation. A later authoritative success/domain-negative
/// result for the same operation removes the earlier failure, so runtime
/// feedback does not keep advising about a problem the agent already fixed.
pub fn count_unresolved_tool_outcome_failures(records: &[ToolCallRecord]) -> usize {
    unresolved_tool_outcome_failure_counts(records, &[])
        .values()
        .copied()
        .sum()
}

/// Whether an executed operation carries affirmative typed success evidence.
///
/// `ToolCallRecord::ok` only proves that the tool transport/executor completed;
/// domain outcomes such as an empty search, a false predicate, or an
/// inconclusive pipeline are not positive validation receipts.  Callers that
/// authorize delivery must require this stronger fact instead of inferring
/// success from the absence of an unresolved failure.
pub fn tool_outcome_is_positive_success(record: &ToolCallRecord) -> bool {
    record.effective_disposition() == astra_services::session_journal::ToolCallDisposition::Executed
        && record.ok
        && effective_tool_result_class(record).as_deref() == Some("success")
        && record.exit_semantics.as_deref() == Some("success")
}

fn effective_tool_result_class(record: &ToolCallRecord) -> Option<String> {
    if record.outcome_is_unknown() {
        return Some("execution_error".into());
    }
    if matches!(record.name.as_str(), "agent" | "agent_fanout") {
        if let Some(class) = record
            .result_class
            .as_deref()
            .map(str::trim)
            .filter(|class| !class.is_empty() && *class != "success")
        {
            return Some(class.to_string());
        }
        return structured_tool_result_class(record).map(str::to_string);
    }
    if let Some(class) = record
        .result_class
        .as_deref()
        .map(str::trim)
        .filter(|class| !class.is_empty())
    {
        return Some(class.to_string());
    }

    structured_tool_result_class(record).map(str::to_string)
}

fn structured_tool_result_class(record: &ToolCallRecord) -> Option<&'static str> {
    let Some(value) = parse_structured_tool_result(record) else {
        return matches!(record.name.as_str(), "agent" | "agent_fanout")
            .then_some(RESULT_CLASS_AGENT_INCOMPLETE);
    };
    if value.get("result_family").is_some() {
        return agent_tool_structured_result_class(&value);
    }
    if record.name == "agent_fanout" || agent_fanout_result_looks_like(&value) {
        return agent_fanout_structured_result_class(&value);
    }
    if record.name == "agent" || agent_tool_result_looks_like(&value) {
        return agent_tool_structured_result_class(&value);
    }
    None
}

fn parse_structured_tool_result(record: &ToolCallRecord) -> Option<Value> {
    [
        record.result_full.as_deref(),
        record.result_preview.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(|raw| serde_json::from_str::<Value>(raw).ok())
}

fn blocked_resolution_key(record: &ToolCallRecord, record_index: usize) -> String {
    rejected_operation_key(record)
        .map(|key| key.to_string())
        .unwrap_or_else(|| format!("tool::{}::opaque::{record_index}", record.name))
}

/// Count rejected invocations that have not been resolved by a successful
/// execution of the same tool and arguments later in the ledger.
pub fn unresolved_blocked_attempt_count(records: &[ToolCallRecord]) -> usize {
    let mut unresolved = std::collections::BTreeMap::<String, usize>::new();
    for (record_index, record) in records.iter().enumerate() {
        if record_is_rejected_attempt(record) {
            *unresolved
                .entry(blocked_resolution_key(record, record_index))
                .or_insert(0) += 1;
        } else if !unresolved.is_empty() && record.was_executed() && record.ok {
            unresolved.remove(&blocked_resolution_key(record, record_index));
        }
    }
    unresolved.values().sum()
}

/// Count the consecutive rejected invocations at the end of the execution
/// ledger. A later executed call is evidence that the model continued through
/// a recovery path, even when the corrected call necessarily used different
/// arguments or a different tool. Terminal prose directly after a rejection
/// has no such boundary and needs one bounded recovery opportunity.
pub fn terminal_rejected_attempt_count(records: &[ToolCallRecord]) -> usize {
    records
        .iter()
        .rev()
        .take_while(|record| record_is_rejected_attempt(record))
        .count()
}

/// A rejected request is a user-visible failed attempt even though it never
/// reached an executor.  Keep it out of execution health/error-rate math, but
/// include it in the turn boundary so policy/admission failures cannot make a
/// turn look successful merely because the executor saw zero calls.
fn record_is_rejected_attempt(record: &ToolCallRecord) -> bool {
    !record.is_synthetic_placeholder()
        && (record.effective_disposition()
            == astra_services::session_journal::ToolCallDisposition::Rejected
            || record.was_blocked_by_policy())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum ExplorationFamily {
    Diff,
    Search,
    Read,
}

impl ExplorationFamily {
    fn as_str(self) -> &'static str {
        match self {
            Self::Diff => "diff",
            Self::Search => "search",
            Self::Read => "read",
        }
    }
}

fn classify_exploration_family(record: &ToolCallRecord) -> Option<ExplorationFamily> {
    let args = record.authoritative_args_full().unwrap_or("");
    match record.name.as_str() {
        name if is_diff_like_tool_call(name, args) => Some(ExplorationFamily::Diff),
        "read_file" => Some(ExplorationFamily::Read),
        "grep" | "rg" | "glob" => Some(ExplorationFamily::Search),
        "bash" if is_search_like_tool_call(&record.name, args) => Some(ExplorationFamily::Search),
        _ => None,
    }
}

fn is_diff_like_tool_call(name: &str, args: &str) -> bool {
    if name != "bash" {
        return false;
    }

    use regex::Regex;
    use std::sync::OnceLock;

    static DIFF_CMD: OnceLock<Regex> = OnceLock::new();
    let diff_re =
        DIFF_CMD.get_or_init(|| Regex::new(r#"(^|[;&|]\s*)git\s+(--no-pager\s+)?diff\b"#).unwrap());
    diff_re.is_match(&bash_command_text(args))
}

fn longest_exploration_family_fact_streak(
    facts: &[ToolEvaluationFact],
) -> Option<(ExplorationFamily, usize)> {
    use std::collections::BTreeMap;

    #[derive(Clone, Copy, Default)]
    struct RoundState {
        family: Option<ExplorationFamily>,
        homogeneous: bool,
        count: usize,
    }

    let mut per_round: BTreeMap<u32, RoundState> = BTreeMap::new();
    for record in facts {
        if !record.was_executed() {
            continue;
        }
        let Some(round) = record.round else { continue };
        let state = per_round.entry(round).or_insert(RoundState {
            family: None,
            homogeneous: true,
            count: 0,
        });
        state.count += 1;
        let Some(family) = record.exploration_family else {
            state.homogeneous = false;
            continue;
        };
        match state.family {
            None => state.family = Some(family),
            Some(existing) if existing == family && state.homogeneous => {}
            Some(_) => state.homogeneous = false,
        }
    }

    let mut best: Option<(ExplorationFamily, usize)> = None;
    let mut current_family: Option<ExplorationFamily> = None;
    let mut current_streak = 0usize;
    let mut prev_round: Option<u32> = None;

    for (&round, state) in &per_round {
        let round_family = if state.homogeneous && state.count >= 2 {
            state.family
        } else {
            None
        };
        let adjacent = prev_round.is_none_or(|prev| round == prev + 1);
        match round_family {
            Some(family) if adjacent && current_family == Some(family) => {
                current_streak += 1;
            }
            Some(family) => {
                current_family = Some(family);
                current_streak = 1;
            }
            None => {
                current_family = None;
                current_streak = 0;
            }
        }
        if let Some(family) = current_family {
            match best {
                Some((_, best_streak)) if best_streak >= current_streak => {}
                _ => best = Some((family, current_streak)),
            }
        }
        prev_round = Some(round);
    }

    best
}

/// Return the dominant exploratory-family streak across consecutive
/// multi-call rounds from the live tool records.
pub fn exploration_family_round_streak(
    records: &[ToolCallRecord],
) -> Option<(&'static str, usize)> {
    let facts: Vec<_> = records
        .iter()
        .map(ToolEvaluationFact::from_record)
        .collect();
    exploration_family_fact_streak(&facts)
}

pub fn exploration_family_fact_streak(
    facts: &[ToolEvaluationFact],
) -> Option<(&'static str, usize)> {
    longest_exploration_family_fact_streak(facts).map(|(family, streak)| (family.as_str(), streak))
}

fn bash_command_text(args: &str) -> String {
    let trimmed = args.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed)
        && let Some(command) = v.get("command").and_then(|c| c.as_str())
    {
        return command.trim().to_string();
    }
    trimmed.to_string()
}

fn is_search_like_tool_call(name: &str, args: &str) -> bool {
    use regex::Regex;
    use std::sync::OnceLock;

    if matches!(name, "grep" | "rg") {
        return true;
    }
    if name != "bash" {
        return false;
    }
    static SEARCH_CMD: OnceLock<Regex> = OnceLock::new();
    let search_re = SEARCH_CMD
        .get_or_init(|| Regex::new(r#"(^|[;&|]\s*|\s+)(git\s+grep|grep|rg|find)\b"#).unwrap());
    search_re.is_match(&bash_command_text(args))
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEvaluationFact {
    tool_name: String,
    /// Exact terminal execution provenance, retained through the bounded
    /// policy window. Absence never authorizes a guessed recovery reference.
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    execution_completion: Option<astra_turn_types::task_resolution::ToolExecutionEvidenceRef>,
    /// Present only when the source retained full authoritative arguments.
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    operation_identity: Option<EvaluationIdentity>,
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    effective_result_class: Option<String>,
    non_failure_outcome: bool,
    /// Frozen by the same effect classifier as terminal notices. Failed
    /// observations remain history without becoming unfinished task effects.
    requires_terminal_attention: bool,
    is_rejected_attempt: bool,
    /// A terminal rejection prevents repair; invalid arguments instead need
    /// a corrected invocation, even though the original call is not retryable.
    rejection_non_retryable: bool,
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    pub round: Option<u32>,
    pub disposition: astra_services::session_journal::ToolCallDisposition,
    pub ok: bool,
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    exploration_family: Option<ExplorationFamily>,
    is_search: bool,
    workspace_mutated: bool,
    /// Identity of a successful read-only request and the exact result
    /// delivered at the model boundary. This is equality-only evidence: it
    /// proves that the same observable result was delivered again, never that
    /// the underlying world stayed unchanged.
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    observation_request_identity: Option<EvaluationIdentity>,
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    observation_result_identity: Option<EvaluationIdentity>,
    /// Facets actually delivered by a live introspection snapshot. This is
    /// separate from the source-owned revision: a facet can be a redundant
    /// read even when the underlying snapshot has not changed.
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    observation_coverage: Option<Vec<String>>,
    /// Normalized scope of a live introspection request. Cross-facet reuse is
    /// allowed only when the earlier read was at least as detailed and used
    /// the same source/horizon/context boundary.
    #[serde(deserialize_with = "astra_turn_types::deserialize_required_option")]
    observation_scope: Option<ObservationScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationScope {
    topic: String,
    facet: String,
    depth: String,
    horizon: String,
    source_policy: String,
    include_context: bool,
    format: String,
}

impl ObservationScope {
    fn from_request(request: &crate::introspect::IntrospectRequest) -> Self {
        Self {
            topic: request.topic.as_str().to_string(),
            facet: request.facet.as_str().to_string(),
            depth: request.depth.as_str().to_string(),
            horizon: request.horizon.as_str().to_string(),
            source_policy: request.source_policy.as_str().to_string(),
            include_context: request.include_context,
            format: if request.format.is_json() {
                "json".to_string()
            } else {
                "text".to_string()
            },
        }
    }

    fn can_cover(
        &self,
        requested: &Self,
        delivered_facets: &[String],
        required_facets: &[String],
    ) -> bool {
        self.topic == requested.topic
            && self.horizon == requested.horizon
            && self.source_policy == requested.source_policy
            && self.include_context == requested.include_context
            && self.format == requested.format
            && depth_rank(&self.depth) >= depth_rank(&requested.depth)
            && required_facets
                .iter()
                .all(|facet| delivered_facets.iter().any(|delivered| delivered == facet))
    }
}

fn depth_rank(depth: &str) -> u8 {
    match depth {
        "hint" => 0,
        "summary" => 1,
        "diagnostic" => 2,
        "forensic" => 3,
        _ => 0,
    }
}

/// Opaque failures are local to the current evidence window and never match a
/// success. Their index is deliberately not a durable operation identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "identity", deny_unknown_fields)]
pub enum EvaluationOutcomeKey {
    Stable(EvaluationIdentity),
    Opaque { tool_name: String, index: usize },
}

/// Equality-only evidence identity. It is domain-separated from execution and
/// health identities and grants no permission to reuse a result.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(deny_unknown_fields)]
pub struct EvaluationIdentity {
    digest: [u8; 32],
}

impl EvaluationIdentity {
    fn new(domain: &[u8], value: &str) -> Self {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        hash.update(b"astra.evaluation.identity.v1\0");
        hash.update((domain.len() as u64).to_be_bytes());
        hash.update(domain);
        hash.update(value.as_bytes());
        Self {
            digest: hash.finalize().into(),
        }
    }
}

impl ToolEvaluationFact {
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.rejection_non_retryable && !self.is_rejected_attempt {
            return Err("non-retryable rejection without rejected attempt");
        }
        let class = self.effective_result_class.as_deref().unwrap_or("");
        let resolves = result_class_resolves_outcome_failure(class);
        if (result_class_is_outcome_failure(class) && self.non_failure_outcome)
            || (resolves && !self.non_failure_outcome)
            || (self.non_failure_outcome && self.ok && !resolves)
        {
            return Err("contradictory tool outcome classification");
        }
        Ok(())
    }

    pub fn from_record(record: &ToolCallRecord) -> Self {
        let args = record.authoritative_args_full().unwrap_or("");
        let workspace_mutated = invalidates_workspace_evidence(record);
        let (observation_request_identity, observation_result_identity) = if workspace_mutated {
            (None, None)
        } else {
            observation_evidence_identities(record)
        };
        let observation_coverage = observation_coverage(record);
        let observation_scope = observation_scope(record);
        let observed_shell_exit = record_is_observed_shell_exit(record);
        let effective_result_class = (!observed_shell_exit)
            .then(|| effective_tool_result_class(record))
            .flatten();
        let requires_terminal_attention = !observed_shell_exit
            && tool_outcome_requires_terminal_attention(
                record,
                effective_result_class.as_deref().unwrap_or("tool_failure"),
            );
        let non_failure_outcome = observed_shell_exit
            || record_is_non_failure_outcome(record, effective_result_class.as_deref());
        Self {
            tool_name: record.name.clone(),
            execution_completion: record.execution_completion.clone(),
            operation_identity: operation_identity_key(record)
                .map(|key| EvaluationIdentity::new(b"operation", &key)),
            effective_result_class,
            non_failure_outcome,
            requires_terminal_attention,
            is_rejected_attempt: record_is_rejected_attempt(record),
            rejection_non_retryable: record_is_rejected_attempt(record)
                && record.error_kind != Some(astra_core::ErrorKind::ToolInvalidArgs)
                && record
                    .runtime_model_result_full
                    .as_deref()
                    .or(record.result_full.as_deref())
                    .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                    .and_then(|result| {
                        result.get("retryable").and_then(serde_json::Value::as_bool)
                    })
                    == Some(false),
            round: record.round,
            disposition: record.effective_disposition(),
            ok: record.ok,
            exploration_family: classify_exploration_family(record),
            is_search: is_search_like_tool_call(&record.name, args),
            workspace_mutated,
            observation_request_identity,
            observation_result_identity,
            observation_coverage,
            observation_scope,
        }
    }

    pub fn was_executed(&self) -> bool {
        self.disposition == astra_services::session_journal::ToolCallDisposition::Executed
    }

    pub fn invalidates_workspace_evidence(&self) -> bool {
        self.workspace_mutated
    }

    /// A delivered envelope can still report a failed test or incomplete
    /// child. Domain-negative observations are not failed execution evidence.
    pub fn has_failed_outcome(&self) -> bool {
        (!self.ok && !self.non_failure_outcome)
            || self
                .effective_result_class
                .as_deref()
                .is_some_and(result_class_is_outcome_failure)
    }

    pub fn execution_completion(
        &self,
    ) -> Option<&astra_turn_types::task_resolution::ToolExecutionEvidenceRef> {
        self.execution_completion.as_ref()
    }

    fn outcome_key(&self, index: usize) -> EvaluationOutcomeKey {
        self.operation_identity.map_or_else(
            || EvaluationOutcomeKey::Opaque {
                tool_name: self.tool_name.clone(),
                index,
            },
            EvaluationOutcomeKey::Stable,
        )
    }

    /// Repetition requires full authoritative arguments, not a shared preview.
    pub fn operation_identity(&self) -> Option<EvaluationIdentity> {
        self.operation_identity
    }

    /// Equality-only identity for a read-only result that was actually
    /// delivered. `None` deliberately means that the runtime cannot prove
    /// what the model received.
    pub fn observation_evidence_identity(
        &self,
    ) -> Option<(EvaluationIdentity, EvaluationIdentity)> {
        self.observation_request_identity
            .zip(self.observation_result_identity)
    }

    pub fn observation_coverage(&self) -> Option<&[String]> {
        self.observation_coverage.as_deref()
    }
}

/// Build the conservative identity used by the online convergence feedback.
/// The request must have complete arguments and the result must have an exact
/// bounded model projection. A durable artifact hash identifies bytes stored
/// for later recovery, not evidence delivered in this call; a preview is never
/// enough to claim that no new evidence was delivered.
///
/// Live introspection is the one structured exception: its report carries a
/// source-owned evidence revision that excludes the diagnostic call's own
/// counters and latency. Comparing that revision avoids treating observation
/// overhead as new task evidence while still breaking the run when the
/// observed runtime facts actually change. If an older/foreign result lacks
/// the marker, the function fails closed.
fn observation_evidence_identities(
    record: &ToolCallRecord,
) -> (Option<EvaluationIdentity>, Option<EvaluationIdentity>) {
    if !record.ok
        || !matches!(
            record.effective_disposition(),
            astra_services::session_journal::ToolCallDisposition::Executed
                | astra_services::session_journal::ToolCallDisposition::Reused
        )
    {
        return (None, None);
    }
    let Some(args) = record.authoritative_args_full().map(str::trim) else {
        return (None, None);
    };
    if args.is_empty() {
        return (None, None);
    }
    let args_value = serde_json::from_str::<serde_json::Value>(args).ok();
    let classification = crate::tool::categories::classify(&record.name, args_value.as_ref());
    if !matches!(
        classification.category,
        crate::tool::categories::ToolCategory::ReadOnly
            | crate::tool::categories::ToolCategory::Consultative
    ) {
        return (None, None);
    }
    let Some(request_key) = observation_request_key(record, args_value.as_ref()) else {
        return (None, None);
    };
    let result_key = if record.name == "introspect"
        && args_value.as_ref().is_some_and(is_live_snapshot_request)
    {
        let result = delivered_model_result(record).filter(|value| !value.is_empty());
        let Some(result) = result else {
            return (None, None);
        };
        let Some(revision) = crate::introspect::extract_evidence_revision(result) else {
            return (None, None);
        };
        format!("introspect_evidence_revision:{revision}")
    } else {
        let Some(result) = delivered_model_result(record).filter(|value| !value.is_empty()) else {
            return (None, None);
        };
        result.to_string()
    };
    if result_key.is_empty() {
        return (None, None);
    }
    (
        Some(EvaluationIdentity::new(
            b"observation_request",
            &request_key,
        )),
        Some(EvaluationIdentity::new(b"observation_result", &result_key)),
    )
}

fn is_live_snapshot_request(args: &serde_json::Value) -> bool {
    args.get("artifact").is_none() && args.get("explain").is_none()
}

fn observation_coverage(record: &ToolCallRecord) -> Option<Vec<String>> {
    if record.name != "introspect"
        || !record.ok
        || !matches!(
            record.effective_disposition(),
            astra_services::session_journal::ToolCallDisposition::Executed
                | astra_services::session_journal::ToolCallDisposition::Reused
        )
    {
        return None;
    }
    let args = record.authoritative_args_full()?.trim();
    if args.is_empty() {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(args).ok()?;
    if !is_live_snapshot_request(&args) {
        return None;
    }
    let result = delivered_model_result(record).filter(|value| !value.is_empty())?;
    let facets = crate::introspect::extract_covered_facets(result)?;
    let mut normalized = std::collections::BTreeSet::new();
    for facet in facets {
        if !matches!(
            facet.as_str(),
            "overview"
                | "session"
                | "recent"
                | "errors"
                | "trace"
                | "volatile"
                | "stall"
                | "noise"
                | "cache"
                | "session_memory"
        ) {
            return None;
        }
        normalized.insert(facet);
    }
    Some(normalized.into_iter().collect())
}

/// Return the exact sanitized result projection available to the live
/// evaluator. An artifact replacement is durable storage presentation only;
/// without the runtime lane it is intentionally unmeasurable.
fn delivered_model_result(record: &ToolCallRecord) -> Option<&str> {
    record.runtime_model_result_full.as_deref().or_else(|| {
        record
            .result_artifact
            .is_none()
            .then_some(record.result_full.as_deref())
            .flatten()
    })
}

fn observation_scope(record: &ToolCallRecord) -> Option<ObservationScope> {
    if record.name != "introspect" {
        return None;
    }
    let args = record.authoritative_args_full()?.trim();
    if args.is_empty() {
        return None;
    }
    let args = serde_json::from_str::<serde_json::Value>(args).ok()?;
    if !is_live_snapshot_request(&args) {
        return None;
    }
    Some(ObservationScope::from_request(
        &crate::introspect::IntrospectRequest::from_args(&args),
    ))
}

/// Normalize the live introspection request using the parameters that actually
/// affect its projection. `max_bytes` is an artifact-window parameter and is
/// ignored by a live request; keeping it out of this identity prevents a
/// non-semantic retry from looking like a new diagnostic query. Artifact
/// recovery keeps the ordinary exact operation identity because offset and
/// window size do affect the delivered evidence.
fn observation_request_key(
    record: &ToolCallRecord,
    args: Option<&serde_json::Value>,
) -> Option<String> {
    if record.name != "introspect" {
        return operation_identity_key(record);
    }
    let Some(args) = args else {
        return operation_identity_key(record);
    };
    if !is_live_snapshot_request(args) {
        return operation_identity_key(record);
    }
    let request = crate::introspect::IntrospectRequest::from_args(args);
    Some(
        serde_json::json!({
            "topic": request.topic.as_str(),
            "facet": request.facet.as_str(),
            "depth": request.depth.as_str(),
            "horizon": request.horizon.as_str(),
            "source_policy": request.source_policy.as_str(),
            "include_context": request.include_context,
            "format": if request.format.is_json() { "json" } else { "text" },
        })
        .to_string(),
    )
}

/// Count the trailing run of successful read-only observations that delivered
/// the same request/result pair or, for live introspection, the same
/// source-owned snapshot without adding facet coverage. The scan stops at the
/// first different or unmeasurable observation, so an unrelated read,
/// mutation, failed call, or missing result never gets silently folded into a
/// "no new evidence" claim.
pub fn trailing_repeated_observation_evidence(facts: &[ToolEvaluationFact]) -> usize {
    trailing_exact_observation_evidence(facts).max(trailing_covered_observation_evidence(facts))
}

fn trailing_exact_observation_evidence(facts: &[ToolEvaluationFact]) -> usize {
    let mut iter = facts.iter().rev();
    let Some(first) = iter
        .next()
        .and_then(ToolEvaluationFact::observation_evidence_identity)
    else {
        return 0;
    };
    let first_coverage = facts
        .last()
        .and_then(|fact| fact.observation_coverage())
        .map(|coverage| coverage.to_vec());
    // A live introspection result must carry an explicit coverage marker. An
    // empty marker means bounded delivery and is still eligible for exact
    // repeat detection; a missing marker is an unmeasurable foreign/legacy
    // result and must fail closed.
    if facts
        .last()
        .is_some_and(|fact| fact.observation_scope.is_some() && first_coverage.is_none())
    {
        return 0;
    }
    let mut count = 1;
    for fact in iter {
        let same_coverage = if first_coverage.is_some() {
            fact.observation_coverage()
                .map(|coverage| coverage.to_vec())
                == first_coverage
        } else {
            true
        };
        if fact.observation_evidence_identity() == Some(first) && same_coverage {
            count += 1;
        } else {
            break;
        }
    }
    count
}

fn trailing_covered_observation_evidence(facts: &[ToolEvaluationFact]) -> usize {
    let Some(latest) = facts.last() else {
        return 0;
    };
    let Some((_, latest_result)) = latest.observation_evidence_identity() else {
        return 0;
    };
    let Some(latest_scope) = latest.observation_scope.as_ref() else {
        return 0;
    };
    let Some(latest_facets) = latest.observation_coverage() else {
        return 0;
    };
    // Empty coverage is a valid bounded-delivery marker, but it cannot prove
    // that one facet contains another. Exact equality detection above still
    // reports repeated delivery without upgrading it to cross-facet reuse.
    if latest_facets.is_empty() {
        return 0;
    }

    let mut count = 0;
    for fact in facts.iter().rev() {
        let Some((_, result)) = fact.observation_evidence_identity() else {
            break;
        };
        let Some(scope) = fact.observation_scope.as_ref() else {
            break;
        };
        let Some(facets) = fact.observation_coverage() else {
            break;
        };
        if result != latest_result || !scope.can_cover(latest_scope, facets, latest_facets) {
            break;
        }
        count += 1;
    }
    if count >= 2 { count } else { 0 }
}

pub fn active_rejected_fact_keys(
    facts: &[ToolEvaluationFact],
) -> std::collections::BTreeSet<EvaluationOutcomeKey> {
    active_rejected_keys(facts.iter().enumerate().map(|(index, fact)| {
        (
            fact.is_rejected_attempt,
            fact.was_executed() && fact.ok,
            fact.outcome_key(index),
        )
    }))
}

/// Retain explicit non-retryability through policy-window compaction and
/// recovery. Only an authoritative success for the same operation clears it.
pub fn has_active_non_retryable_rejection(facts: &[ToolEvaluationFact]) -> bool {
    !active_rejected_keys(facts.iter().enumerate().map(|(index, fact)| {
        (
            fact.is_rejected_attempt && fact.rejection_non_retryable,
            fact.was_executed() && fact.ok,
            fact.outcome_key(index),
        )
    }))
    .is_empty()
}

pub fn count_unresolved_tool_outcome_fact_failures(facts: &[ToolEvaluationFact]) -> usize {
    unresolved_tool_outcome_fact_counts(facts).values().sum()
}

pub fn count_search_fanout_facts(facts: &[ToolEvaluationFact]) -> usize {
    facts
        .iter()
        .filter(|fact| fact.was_executed() && fact.is_search)
        .count()
}

const MAX_SHELL_LEX_BYTES: usize = 32 * 1024;
const MAX_NESTED_SHELL_SCAN_BYTES: usize = 16 * 1024;
const MAX_NESTED_SHELL_DEPTH: usize = 16;

/// Top-level shell control-flow operator retained for evidence consumers.
/// This is metadata only; it never authorizes or blocks command execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellControlOp {
    And,
    Or,
    Sequence,
    Neutral,
}

/// Split a shell command at validated top-level sequencing operators.  The
/// lexer is shared by evidence consumers so they do not each invent a partial
/// quote/substitution parser.  Unsupported or malformed shell syntax returns
/// `None`; callers must then keep the command executable but decline to infer
/// a completion receipt.
pub fn split_shell_control_segments(command: &str) -> Option<Vec<&str>> {
    split_shell_segments(command, true, false)
}

/// Split a command while retaining the operator which followed each segment.
/// The ordinary public splitter intentionally exposes only segment text for
/// callers that need a best-effort read/mutation classification.  Completion
/// receipts additionally need the control-flow fact: a validator followed by
/// `;` or `||` is not a proof that the overall command succeeded, whereas a
/// validator followed by `&&` remains on the successful path.  Comments are
/// neutral; a newline after a comment is represented by the following empty
/// segment and therefore still applies its sequence semantics.
pub fn split_shell_control_segments_with_ops(command: &str) -> Option<Vec<(&str, ShellControlOp)>> {
    let delimiters = top_level_shell_delimiters(command, true, false)?;
    if delimiters.is_empty() {
        return Some(vec![(command, ShellControlOp::Neutral)]);
    }
    let bytes = command.as_bytes();
    let mut segments = Vec::with_capacity(delimiters.len() + 1);
    let mut start = 0usize;
    for (index, width) in delimiters {
        let op = match bytes.get(index..index + width) {
            Some(b"&&") => ShellControlOp::And,
            Some(b"||") => ShellControlOp::Or,
            Some(b";") | Some(b"\n") | Some(b"\r\n") => ShellControlOp::Sequence,
            // A comment delimiter consumes its text up to (but not including)
            // a following newline. It does not itself change command status.
            _ => ShellControlOp::Neutral,
        };
        segments.push((&command[start..index], op));
        start = index + width;
    }
    segments.push((&command[start..], ShellControlOp::Neutral));
    Some(segments)
}

/// Parse one already-segmented shell command into literal words.  This is a
/// deliberately static lexer, not a shell interpreter: it preserves quoted
/// whitespace, rejects expansion/operators/escapes, and returns `None` for
/// anything whose destination cannot be known without executing a shell.
pub fn split_static_shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut token_started = false;
    let chars = segment.chars().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                if active_quote == '"' && matches!(ch, '$' | '`' | '\\') {
                    return None;
                }
                current.push(ch);
            }
            token_started = true;
            index += 1;
            continue;
        }
        match ch {
            '\'' | '"' => {
                quote = Some(ch);
                token_started = true;
            }
            ch if ch.is_whitespace() => {
                if token_started {
                    words.push(std::mem::take(&mut current));
                    token_started = false;
                }
            }
            '>' => {
                if token_started {
                    words.push(std::mem::take(&mut current));
                    token_started = false;
                }
                if chars.get(index + 1) == Some(&'>') {
                    words.push(">>".to_string());
                    index += 1;
                } else {
                    words.push(">".to_string());
                }
            }
            '$' | '`' | '~' | '*' | '?' | '[' | ']' | '\\' | ';' | '|' | '&' | '<' | '(' | ')'
            | '\n' | '\r' => return None,
            _ => {
                current.push(ch);
                token_started = true;
            }
        }
        index += 1;
    }
    if quote.is_some() {
        return None;
    }
    if token_started {
        words.push(current);
    }
    Some(words)
}

fn split_top_level_pipeline_segments(command: &str) -> Option<Vec<&str>> {
    split_shell_segments(command, false, true)
}

/// Split a shell command at top-level pipelines using the same quote,
/// substitution, and malformed-input rules as the evidence evaluator.
/// Consumers may use this for scope/receipt analysis, but it never authorizes
/// execution and returns `None` for syntax it cannot prove.
pub fn split_shell_pipeline_segments(command: &str) -> Option<Vec<&str>> {
    split_top_level_pipeline_segments(command)
}

fn split_shell_segments(
    command: &str,
    include_control: bool,
    include_pipeline: bool,
) -> Option<Vec<&str>> {
    let delimiters = top_level_shell_delimiters(command, include_control, include_pipeline)?;
    if delimiters.is_empty() {
        return Some(vec![command]);
    }
    let mut segments = Vec::with_capacity(delimiters.len() + 1);
    let mut start = 0;
    for (index, width) in delimiters {
        segments.push(&command[start..index]);
        start = index + width;
    }
    segments.push(&command[start..]);
    Some(segments)
}

/// Find the closing parenthesis for a `$()` beginning at `open_index` and
/// return `(exclusive_end, body_end)`. This is a lexical matcher only; it
/// rejects unsupported backticks, here-docs, malformed quotes, and excessive
/// nesting rather than guessing at shell semantics.
fn command_substitution_end(command: &str, open_index: usize) -> Option<(usize, usize)> {
    let bytes = command.as_bytes();
    if open_index + 1 >= bytes.len() || bytes[open_index] != b'$' || bytes[open_index + 1] != b'(' {
        return None;
    }
    let mut depth = 1usize;
    let mut quote = None;
    let mut escaped = false;
    let mut index = open_index + 2;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if byte == b'`' && quote != Some(b'\'') {
            return None;
        }
        if let Some(active_quote) = quote {
            if active_quote == b'\'' {
                if byte == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if byte == b'"' {
                quote = None;
                index += 1;
                continue;
            }
            if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
                depth += 1;
                if depth > MAX_NESTED_SHELL_DEPTH {
                    return None;
                }
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        if byte == b'#' && shell_comment_starts(bytes, index) {
            index = shell_comment_end(bytes, index);
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte == b'`' || bytes.get(index..index + 2) == Some(b"<<") {
            return None;
        }
        if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
            depth += 1;
            if depth > MAX_NESTED_SHELL_DEPTH {
                return None;
            }
            index += 2;
            continue;
        }
        match byte {
            b'(' => {
                depth += 1;
                if depth > MAX_NESTED_SHELL_DEPTH {
                    return None;
                }
            }
            b')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some((index + 1, index));
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Lightweight shell lexer used only for evidence segmentation.  It tracks
/// quotes, escapes, and parenthesis depth so control operators inside `$()`
/// are never mistaken for outer sequencing.  It intentionally does not
/// execute or fully parse shell grammar; malformed/oversized input returns
/// `None` so callers fail closed rather than treating the whole input as one
/// apparently safe segment.
fn top_level_shell_delimiters(
    command: &str,
    include_control: bool,
    include_pipeline: bool,
) -> Option<Vec<(usize, usize)>> {
    if command.len() > MAX_SHELL_LEX_BYTES {
        return None;
    }
    let bytes = command.as_bytes();
    let mut delimiters = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if let Some(active_quote) = quote {
            if active_quote == b'\'' {
                if byte == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
            if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
                index = command_substitution_end(command, index)?.0;
                continue;
            }
            if byte == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'#' && shell_comment_starts(bytes, index) {
            // Preserve the comment as a lexical separator so the caller
            // cannot let an unknown comment prefix erase a receipt. The
            // newline, if any, is processed on the next iteration.
            let end = shell_comment_end(bytes, index);
            delimiters.push((index, end - index));
            index = end;
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte == b'`' || bytes.get(index..index + 2) == Some(b"<<") {
            return None;
        }
        if byte == b'$' && bytes.get(index + 1) == Some(&b'(') {
            index = command_substitution_end(command, index)?.0;
            continue;
        }
        if byte == b'(' || byte == b')' {
            // Shell grouping/arithmetic is intentionally outside this
            // evidence lexer. Unknown syntax must not be treated as a safe
            // separator; callers fail closed instead.
            return None;
        }
        if include_control && bytes.get(index..index + 2) == Some(b"&&") {
            delimiters.push((index, 2));
            index += 2;
            continue;
        }
        if include_control && bytes.get(index..index + 2) == Some(b"||") {
            delimiters.push((index, 2));
            index += 2;
            continue;
        }
        if include_control && byte == b';' {
            delimiters.push((index, 1));
            index += 1;
            continue;
        }
        if include_control && byte == b'\n' {
            delimiters.push((index, 1));
            index += 1;
            continue;
        }
        if include_control && byte == b'\r' && bytes.get(index + 1) == Some(&b'\n') {
            delimiters.push((index, 2));
            index += 2;
            continue;
        }
        if include_pipeline && byte == b'|' {
            delimiters.push((index, 1));
            index += 1;
            continue;
        }
        if byte == b'&' {
            // Preserve the two benign file-descriptor redirects used by
            // ordinary compiler/test commands. They are not sequencing or
            // background operators and are normalized by the caller.
            if index >= 2
                && bytes[index - 1] == b'>'
                && matches!(
                    (bytes[index - 2], bytes.get(index + 1).copied()),
                    (b'2', Some(b'1')) | (b'1', Some(b'2'))
                )
            {
                index += 2;
                continue;
            }
            // `&&` was consumed above; a standalone background operator is
            // not modeled and therefore cannot yield positive evidence.
            return None;
        }
        index += 1;
    }
    if quote.is_some() || escaped {
        return None;
    }
    Some(delimiters)
}

/// Return the canonical operation prefix for a known build/test validation
/// command.  This is intentionally a positive, narrow predicate: unlike the
/// permission classifier (which must fail closed for unknown shell syntax), it
/// only reports validation evidence when the command family is recognized.
///
/// The normalized prefix is shared by evaluation and completion settlement so
/// a successful `python3 -m pytest`/`cargo test` receipt cannot be mistaken for
/// an opaque, potentially mutating shell call.
pub fn normalize_validation_prefix(name: &str, args: &str) -> Option<String> {
    normalize_validation_prefix_kind(name, args, true)
}

/// Whether an attempted verification is a recognized project validator or
/// an opaque operation that can only be matched by its complete arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationAttempt {
    Recognized(String),
    Opaque(String),
}

impl ValidationAttempt {
    pub fn into_identity(self) -> String {
        match self {
            Self::Recognized(identity) | Self::Opaque(identity) => identity,
        }
    }
}

/// Classify a verification attempt without treating unsupported shell syntax
/// as evidence that a particular project validator ran. An opaque attempt
/// retains the existing bounded exact-argument identity for recovery.
pub fn classify_validation_attempt(name: &str, args: &str) -> Option<ValidationAttempt> {
    if let Some(identity) = normalize_validation_prefix_kind(name, args, false) {
        return Some(ValidationAttempt::Recognized(identity));
    }
    if name != "bash" {
        return None;
    }
    let command = bash_command_text(args);
    match bash_command_go_validation_presence(&command) {
        Some(true) => Some(ValidationAttempt::Recognized(operation_args_identity(
            name, args,
        ))),
        None => Some(ValidationAttempt::Opaque(operation_args_identity(
            name, args,
        ))),
        Some(false) => None,
    }
}

/// A failed verification retains debt even when its arguments cannot prove
/// that a successful invocation actually validated the project.
pub fn normalize_validation_attempt_prefix(name: &str, args: &str) -> Option<String> {
    classify_validation_attempt(name, args).map(ValidationAttempt::into_identity)
}

fn normalize_validation_prefix_kind(name: &str, args: &str, require_proof: bool) -> Option<String> {
    if name != "bash" {
        return None;
    }
    let command = bash_command_text(args);
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    let scan = has_go_validation(command, 0);
    let has_go_validation = scan != Some(false);
    // Every Go identity can enter recovery/checkpoint state, including the
    // ordinary validator path. Never retain sensitive invocation text there.
    if scan.is_none()
        || (has_go_validation
            && (astra_text_utils::credential_redaction::redact_credentials_for_display(command).1
                != 0
                || astra_text_utils::credential_redaction::redaction_marker_status(command).1))
    {
        return None;
    }
    let alternative_status = has_go_validation
        && split_shell_control_segments_with_ops(command)?
            .iter()
            .any(|(_, op)| matches!(op, ShellControlOp::Or));
    let operation = (!alternative_status)
        .then(|| bash_command_post_mutation_validation_prefix_kind(command, require_proof))
        .flatten()
        .or_else(|| {
            // A recognizable but unproven invocation still owns its attempt.
            // Dropping it would preserve older Work proof; using only its tail
            // would wrongly attribute a preparation failure to the validator.
            (!require_proof && has_go_validation).then(|| command.to_string())
        })?;
    // A failed prefix reader/output stage may prevent Go from running too.
    // Use one complete invocation identity for all Go checks, not just scratch.
    let operation = if has_go_validation {
        command.to_string()
    } else {
        operation
    };
    let arguments = serde_json::from_str::<serde_json::Value>(args).ok();
    match arguments.as_ref().and_then(|args| args.get("workdir")) {
        None => Some(operation),
        Some(serde_json::Value::String(workdir))
            if !workdir.trim().is_empty()
                && !workdir.contains(['\n', '\r'])
                && astra_text_utils::credential_redaction::redact_credentials_for_display(
                    workdir,
                )
                .1 == 0
                && !astra_text_utils::credential_redaction::redaction_marker_status(workdir).1 =>
        {
            if std::path::Path::new(workdir)
                .components()
                .all(|part| part == std::path::Component::CurDir)
            {
                return Some(operation);
            }
            Some(format!(
                "cd '{}' && {operation}",
                workdir.replace('\'', "'\\''")
            ))
        }
        _ => None,
    }
}

struct ValidationCandidate {
    operation: String,
    proof_eligible: bool,
}

fn typescript_option_may_skip_validation(word: &str) -> bool {
    let Some(name) = word.strip_prefix("--").or_else(|| word.strip_prefix('-')) else {
        return false;
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "h" | "?"
            | "help"
            | "v"
            | "version"
            | "all"
            | "showconfig"
            | "listfilesonly"
            | "init"
            | "nocheck"
            | "w"
            | "watch"
    )
}

/// TypeScript expands `@file` only at the top level of argv, after an option
/// has had the chance to consume its operand. These are the CLI-admitted
/// non-Boolean compiler/watch options; an unknown option does not grant proof.
fn typescript_option_value_kind(word: &str) -> Option<bool> {
    let name = word.strip_prefix("--").or_else(|| word.strip_prefix('-'))?;
    let name = name.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "lib"
            | "typeroots"
            | "types"
            | "modulesuffixes"
            | "customconditions"
            | "excludedirectories"
            | "excludefiles"
    ) {
        return Some(true); // List: a following switch is not consumed.
    }
    matches!(
        name.as_str(),
        "generatecpuprofile"
            | "generatetrace"
            | "locale"
            | "project"
            | "p"
            | "target"
            | "t"
            | "module"
            | "m"
            | "jsx"
            | "outfile"
            | "outdir"
            | "rootdir"
            | "tsbuildinfofile"
            | "importsnotusedasvalues"
            | "moduleresolution"
            | "baseurl"
            | "sourceroot"
            | "maproot"
            | "jsxfactory"
            | "jsxfragmentfactory"
            | "jsximportsource"
            | "out"
            | "reactnamespace"
            | "charset"
            | "newline"
            | "declarationdir"
            | "maxnodemodulejsdepth"
            | "moduledetection"
            | "ignoredeprecations"
            | "watchfile"
            | "watchdirectory"
            | "fallbackpolling"
    )
    .then_some(false)
}

fn typescript_args_prove_validation(words: &[String]) -> bool {
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if word.starts_with('@') {
            return false;
        }
        if let Some(is_list) = typescript_option_value_kind(word) {
            let Some(value) = words.get(index + 1) else {
                return false;
            };
            // TypeScript leaves an option-looking token after a list option
            // for the outer argv parser (e.g. `--types --help`).
            index += if is_list && value.starts_with('-') {
                1
            } else {
                2
            };
            continue;
        }
        if typescript_option_may_skip_validation(word) {
            if words.get(index + 1).is_none_or(|value| value != "false") {
                return false;
            }
            index += 1;
        }
        index += 1;
    }
    true
}

/// Normalize an absolute evidence path lexically, without accessing the filesystem.
pub fn normalize_absolute_path(path: &str) -> Option<PathBuf> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return None;
                }
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Some(normalized)
}

/// Whether a lexical scratch destination is outside a concrete workspace binding.
pub fn path_is_external_volatile_scratch(path: &str, workspace_root: Option<&str>) -> bool {
    let Some(path) = normalize_absolute_path(path) else {
        return false;
    };
    let workspace_root = workspace_root.and_then(normalize_absolute_path);
    if workspace_root
        .as_ref()
        .is_some_and(|root| path.starts_with(root) || root.starts_with(&path))
    {
        return false;
    }
    ["/tmp", "/var/tmp", "/dev/shm"]
        .into_iter()
        .filter_map(normalize_absolute_path)
        .any(|root| path == root || path.starts_with(root))
}

/// Prove a small class of shell-local setup mutations whose every destination
/// is already outside the bound workspace.  Reviewers commonly create a
/// scratch directory before redirecting `git show` output into it; treating
/// `mkdir -p /tmp/...` as an opaque writer recreates the same false mutation
/// epoch as an external redirect.  Unknown writers, relative paths, and any
/// workspace target remain barriers.
pub fn external_scratch_setup(segment: &str, workspace_root: Option<&str>) -> bool {
    let normalized = crate::cloud_approval_policy::strip_benign_fd_redirects(segment);
    let Some(args) = split_static_shell_words(&normalized) else {
        return false;
    };
    let Some(head) = args.first().map(String::as_str) else {
        return false;
    };
    if !matches!(head, "mkdir" | "touch" | "rm") {
        return false;
    }
    let paths = args
        .iter()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect::<Vec<_>>();
    !paths.is_empty()
        && paths.iter().all(|path| {
            Path::new(path).is_absolute() && path_is_external_volatile_scratch(path, workspace_root)
        })
}

fn go_args_prove_local_validation(args: &[String]) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index) {
        if arg.starts_with('-') {
            let (flag, inline) = arg
                .split_once('=')
                .map_or((arg.as_str(), None), |(flag, value)| (flag, Some(value)));
            let takes_value = matches!(
                flag,
                "-mod"
                    | "-count"
                    | "-timeout"
                    | "-run"
                    | "-skip"
                    | "-bench"
                    | "-benchtime"
                    | "-parallel"
                    | "-cpu"
                    | "-shuffle"
                    | "-tags"
                    | "-covermode"
                    | "-coverpkg"
                    | "-vet"
                    | "-p"
            );
            if takes_value {
                let value = match inline {
                    Some(value) => value,
                    None => {
                        index += 1;
                        let Some(value) = args.get(index) else {
                            return false;
                        };
                        value
                    }
                };
                if value.starts_with('-') || (flag == "-mod" && value != "readonly") {
                    return false;
                }
            } else if !matches!(
                flag,
                "-v" | "-race"
                    | "-short"
                    | "-failfast"
                    | "-cover"
                    | "-json"
                    | "-trimpath"
                    | "-x"
                    | "-a"
            ) || inline.is_some_and(|value| !matches!(value, "true" | "false"))
            {
                // Dry-run, alternate tools/scope and explicit output writers
                // are not observations of the final source state.
                return false;
            }
        } else if !(arg == "."
            || arg.starts_with("./")
            || arg.starts_with("../")
            || arg.starts_with('/')
            || arg.ends_with(".go"))
        {
            // Go import paths and std/all resolve outside the workspace.
            return false;
        }
        index += 1;
    }
    true
}

fn go_validation_candidate(segment: &str) -> Option<ValidationCandidate> {
    let head = segment.split_whitespace().next()?;
    if head != "go" && !head.contains('=') {
        return None;
    }
    let words = split_static_shell_words(segment)?;
    let command_start = words.iter().position(|word| word == "go")?;
    let command = &words[command_start..];
    if !matches!(
        command.get(1).map(String::as_str),
        Some("test" | "build" | "vet")
    ) {
        return None;
    }
    // Parse assignment words without executing a shell. Quoted command names
    // and expanded/unsupported prefixes must not become environment wrappers.
    // GOWORK=off explicitly selects the local module. Other environment
    // values may redirect execution or contain secrets and remain opaque.
    for (raw, parsed) in segment.split_whitespace().zip(&words).take(command_start) {
        let (name, _) = raw.split_once('=')?;
        if split_static_shell_words(raw)?.first() != Some(parsed)
            || name != "GOWORK"
            || parsed.split_once('=')?.1 != "off"
        {
            return None;
        }
    }
    Some(ValidationCandidate {
        // Configuration is part of operation equality. A differently
        // configured success must not erase an earlier validation failure.
        operation: segment.to_string(),
        proof_eligible: go_args_prove_local_validation(&command[2..]),
    })
}

/// Recognize a Go validation stage independently of whether its surrounding
/// invocation is eligible to prove completion. An inconclusive bounded scan
/// also requires the strict validation path; it must not grant a generic
/// verify receipt authority by pretending that no validator was present.
pub fn bash_command_has_go_validation(command: &str) -> bool {
    bash_command_go_validation_presence(command).unwrap_or(true)
}

/// Preserve unknown syntax separately from a positively recognized Go stage.
pub fn bash_command_go_validation_presence(command: &str) -> Option<bool> {
    has_go_validation(command, 0)
}

fn has_go_validation(command: &str, depth: usize) -> Option<bool> {
    if depth >= MAX_NESTED_SHELL_DEPTH {
        return None;
    }
    for (segment, _) in split_shell_control_segments_with_ops(command)? {
        for part in split_top_level_pipeline_segments(segment)? {
            let part = part.trim();
            let part = part
                .strip_suffix("2>&1")
                .or_else(|| part.strip_suffix("1>&2"))
                .unwrap_or(part)
                .trim();
            let (words, literal) = match split_static_shell_words(part) {
                Some(words) => (words, true),
                None => (shell_outer_words(part)?, false),
            };
            let mut command = words.iter().skip_while(|word| shell_assignment(word));
            if command.next().is_some_and(|word| word.as_str() == "go")
                && command
                    .next()
                    .is_some_and(|word| matches!(word.as_str(), "test" | "build" | "vet"))
            {
                return Some(true);
            }
            if !literal {
                for body in command_substitution_bodies(part)? {
                    if has_go_validation(body, depth + 1)? {
                        return Some(true);
                    }
                }
            }
        }
    }
    Some(false)
}

fn normalized_validation_segment(segment: &str, original: &str) -> Option<ValidationCandidate> {
    if let Some(candidate) = go_validation_candidate(original) {
        return Some(candidate);
    }
    // Shell command names, subcommands and module names are case-sensitive.
    // TypeScript's --noEmit option is not: canonicalize only that option for
    // exact-operation recovery, preserving paths and other arguments.
    let typescript = ["tsc ", "npx tsc "].into_iter().find_map(|prefix| {
        let args = segment.strip_prefix(prefix)?;
        let option_end = args.find(' ').unwrap_or(args.len());
        if !args[..option_end].eq_ignore_ascii_case("--noEmit") {
            return None;
        }
        let words = split_static_shell_words(args)?;
        Some(ValidationCandidate {
            operation: format!("{prefix}--noEmit{}", &args[option_end..]),
            proof_eligible: typescript_args_prove_validation(&words[1..]),
        })
    });
    if typescript.is_some() {
        return typescript;
    }
    if validation_segment_has_meta_option(segment) {
        return None;
    }
    let recognized = segment.starts_with("cargo check ")
        || segment == "cargo check"
        || segment.starts_with("cargo test ")
        || segment == "cargo test"
        || segment.starts_with("cargo build ")
        || segment == "cargo build"
        || segment == "pytest"
        || segment.starts_with("pytest ")
        || segment == "python -m pytest"
        || segment.starts_with("python -m pytest ")
        || segment == "python3 -m pytest"
        || segment.starts_with("python3 -m pytest ")
        || segment == "python -m unittest"
        || segment.starts_with("python -m unittest ")
        || segment == "python3 -m unittest"
        || segment.starts_with("python3 -m unittest ")
        || segment == "python -m build"
        || segment.starts_with("python -m build ")
        || segment == "python3 -m build"
        || segment.starts_with("python3 -m build ")
        || segment.starts_with("python setup.py build ")
        || segment == "python setup.py build"
        || segment.starts_with("python setup.py build_ext ")
        || segment == "python setup.py build_ext"
        || segment.starts_with("python3 setup.py build ")
        || segment == "python3 setup.py build"
        || segment.starts_with("python3 setup.py build_ext ")
        || segment == "python3 setup.py build_ext"
        || segment == "npm test"
        || segment.starts_with("npm test ")
        || segment == "npm run build"
        || segment.starts_with("npm run build ");
    recognized.then(|| ValidationCandidate {
        operation: segment.to_string(),
        proof_eligible: true,
    })
}

/// A framework command's help/version mode is an informational query, not a
/// validation receipt. Keep this check provider- and task-neutral: it applies
/// to the common validation command families, while leaving ordinary reader
/// option semantics to the shell-observation classifier.
fn validation_segment_has_meta_option(segment: &str) -> bool {
    // Use the same static shell lexer as the path/redirect classifiers.  A
    // completion receipt must never be inferred from a command whose argv is
    // dynamic or malformed: `cargo test "$MODE"` could resolve to a metadata
    // query, and quoted spellings such as `cargo test '--help'` must retain
    // their option semantics.  This remains evidence-only; the command is
    // still executable under the normal permission policy.
    let Some(words) = split_static_shell_words(segment) else {
        // An opaque outer wrapper (for example an assignment containing a
        // command substitution) is handled by the nested-observation
        // classifier.  Only reject an unparseable command when its first
        // static token is itself a known validation family; otherwise this
        // check would turn every dynamic reader into a metadata failure.
        let first = segment
            .split_whitespace()
            .next()
            .map(|word| word.trim_matches(['\'', '"']))
            .map(str::to_ascii_lowercase);
        return first.is_some_and(|first| {
            matches!(
                first.as_str(),
                "cargo" | "go" | "npm" | "npx" | "pytest" | "python" | "python3" | "tsc"
            )
        });
    };
    let Some(first) = words.first().map(|word| word.to_ascii_lowercase()) else {
        return false;
    };
    if !matches!(
        first.as_str(),
        "cargo" | "go" | "npm" | "npx" | "pytest" | "python" | "python3" | "tsc"
    ) {
        return false;
    }
    words.iter().skip(1).any(|word| match word.as_str() {
        "-h" | "-?" | "--help" | "--version" | "--usage" => true,
        // Cargo/Go/Pytest use lower-case -v for verbose output, while the
        // TypeScript/npm frontends use it as a version query. Cargo's -V is
        // the version query; keep short-option meaning family-specific.
        "-v" => matches!(first.as_str(), "tsc" | "npx" | "npm"),
        "-V" => true,
        _ => false,
    })
}

/// Return the canonical build/test receipt that occurs after the final
/// possible mutation barrier in a shell command.  A validator before a later
/// writer is deliberately not returned; callers must not combine an old
/// project-wide prefix with a new, weaker observation.
pub fn bash_command_post_mutation_validation_prefix(command: &str) -> Option<String> {
    bash_command_post_mutation_validation_prefix_kind(command, true)
}

fn bash_command_post_mutation_validation_prefix_kind(
    command: &str,
    require_proof: bool,
) -> Option<String> {
    if !shell_command_preserves_pipeline_status(command) {
        return None;
    }
    let mut latest = Vec::new();
    let mut has_validator = false;
    let mut opaque_predecessor = false;
    let mut configuration_changed = false;
    let mut has_scratch_setup = false;
    let has_go_validation = bash_command_has_go_validation(command);
    let control_segments = split_shell_control_segments_with_ops(command)?;
    for (segment_index, (raw_segment, op_after)) in control_segments.iter().enumerate() {
        let pipeline_segments = split_top_level_pipeline_segments(raw_segment)?;
        let is_pipeline = pipeline_segments.len() > 1;
        let mut pipeline_latest = Vec::new();
        let mut pipeline_blocked = false;
        for pipeline_segment in pipeline_segments {
            let segment = pipeline_segment.trim();
            if segment.is_empty() {
                continue;
            }
            let segment = segment.strip_suffix("2>&1").unwrap_or(segment).trim();
            let segment = segment.strip_suffix("1>&2").unwrap_or(segment).trim();
            let normalized = segment.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            // `cd`/`set` are neutral only when they are pure control
            // statements. A redirect on a control segment is still a write
            // and, in a pipeline, runs concurrently with the validator.
            if shell_segment_has_non_benign_redirect(segment) {
                pipeline_blocked = true;
                break;
            }
            if is_shell_control_segment(&normalized) {
                if normalized.starts_with("cd ") && !is_pipeline {
                    if !shell_control_segment_is_static_and_neutral(segment) {
                        pipeline_blocked = true;
                        break;
                    }
                    latest.push(segment.to_string());
                }
                continue;
            }
            if has_go_validation
                && !is_pipeline
                && !has_validator
                && !opaque_predecessor
                && matches!(op_after, ShellControlOp::And)
                && external_scratch_setup(segment, None)
            {
                has_scratch_setup = true;
                continue;
            }
            if let Some(candidate) = normalized_validation_segment(&normalized, segment) {
                if (require_proof && !candidate.proof_eligible)
                    || !pipeline_latest.is_empty()
                    || configuration_changed
                    || (opaque_predecessor && go_validation_candidate(segment).is_some())
                {
                    pipeline_blocked = true;
                    break;
                }
                pipeline_latest.push(candidate.operation);
                continue;
            }
            if let Some(presentation) = shell_presentation(segment) {
                if matches!(presentation, ShellPresentation::Configuration { .. }) {
                    configuration_changed = true;
                    pipeline_blocked = true;
                    break;
                }
                continue;
            }
            if is_positive_validation_segment(&normalized)
                || is_status_neutral_segment(&normalized)
                || crate::cloud_approval_policy::bash_command_is_read_only(&normalized)
            {
                continue;
            }
            // Pipeline stages execute concurrently. An unknown or mutating
            // stage cannot be ordered before a validator merely because the
            // validator appears later in the text; treat the whole pipeline
            // as a barrier instead of manufacturing a post-mutation receipt.
            pipeline_blocked = true;
            break;
        }
        if pipeline_blocked {
            if require_proof && has_go_validation {
                return None;
            }
            latest.clear();
            has_validator = false;
            opaque_predecessor = true;
        } else {
            has_validator |= !pipeline_latest.is_empty();
            latest.extend(pipeline_latest);
        }
        // A successful `&&` continuation is conditional on the validator
        // succeeding, so it does not mask that receipt. `;` and `||` both
        // permit a later command to run after a failed validator and must
        // close the evidence epoch. Empty segments are significant here: a
        // newline following a comment appears as an empty segment.
        let sequence_has_rhs = control_segments[segment_index + 1..]
            .iter()
            .any(|(segment, _)| {
                let segment = segment.trim();
                !segment.is_empty() && !segment.starts_with('#')
            });
        if matches!(op_after, ShellControlOp::Or)
            || (matches!(op_after, ShellControlOp::Sequence) && sequence_has_rhs)
        {
            if require_proof && has_go_validation && has_validator {
                return None;
            }
            latest.clear();
            has_validator = false;
        }
    }
    if has_scratch_setup {
        // Setup belongs to the failed operation too: an overall failure may
        // have stopped before the validator. Never let a bare check discharge
        // that debt. Configuration changes are rejected by the shared
        // segment classifier, independently of whether setup was present.
        if !has_validator
            || opaque_predecessor
            || control_segments
                .iter()
                .any(|(_, op)| !matches!(op, ShellControlOp::And | ShellControlOp::Neutral))
            || astra_text_utils::credential_redaction::redact_credentials_for_display(command).1
                != 0
            || astra_text_utils::credential_redaction::redaction_marker_status(command).1
        {
            return None;
        }
        return Some(command.to_string());
    }
    has_validator.then(|| latest.join(" && "))
}

/// Return whether a compound shell command contains a positive validation
/// receipt after its last potentially state-changing segment.
///
/// Permission classification intentionally treats unknown shell syntax as
/// unsafe.  Completion evidence has a different question: a command such as
/// `prepare; deploy; cat result; curl endpoint` can still provide a useful
/// post-change observation, while `cat result; deploy` cannot.  Keep this
/// small and provider-neutral: unknown segments are barriers, known
/// validation/read commands are receipts, and shell control wrappers are
/// neutral.  This is evidence classification only; it never grants
/// execution authority or changes the shell safety policy.
pub fn bash_command_has_post_mutation_validation(command: &str) -> bool {
    if !shell_command_preserves_pipeline_status(command) {
        return false;
    }
    let mut validation_after_barrier = false;
    let mut strict_receipt = false;
    let mut configuration_changed = false;
    let mut opaque_predecessor = false;
    let Some(control_segments) = split_shell_control_segments_with_ops(command) else {
        return false;
    };
    for (segment_index, (raw_segment, op_after)) in control_segments.iter().enumerate() {
        let Some(pipeline_segments) = split_top_level_pipeline_segments(raw_segment) else {
            return false;
        };
        let mut pipeline_validation = None;
        let mut pipeline_validator_count = 0;
        let mut pipeline_blocked = false;
        let is_pipeline = pipeline_segments.len() > 1;
        for pipeline_segment in pipeline_segments {
            let segment = pipeline_segment.trim();
            if segment.is_empty() {
                continue;
            }
            let segment = segment.strip_suffix("2>&1").unwrap_or(segment).trim();
            let segment = segment.strip_suffix("1>&2").unwrap_or(segment).trim();
            let normalized = segment.split_whitespace().collect::<Vec<_>>().join(" ");
            if normalized.is_empty() {
                continue;
            }
            // Keep control-wrapper handling behind the raw redirect barrier;
            // otherwise `cd ... > out | cargo test` or `set -e > out | ...`
            // could manufacture a validation receipt.
            if shell_segment_has_non_benign_redirect(segment) {
                pipeline_blocked = true;
                break;
            }
            if is_shell_control_segment(&normalized) {
                continue;
            }
            if let Some(candidate) = normalized_validation_segment(&normalized, segment) {
                pipeline_validator_count += 1;
                if !candidate.proof_eligible
                    || pipeline_validator_count > 1
                    || configuration_changed
                    || (opaque_predecessor && go_validation_candidate(segment).is_some())
                {
                    pipeline_blocked = true;
                    break;
                }
                pipeline_validation = Some(true);
                continue;
            }
            if let Some(presentation) = shell_presentation(segment) {
                match presentation {
                    ShellPresentation::Output => {}
                    ShellPresentation::Observation => pipeline_validation = Some(false),
                    ShellPresentation::Configuration { observation } => {
                        configuration_changed = true;
                        if observation && !is_pipeline {
                            pipeline_validation = Some(false);
                        } else {
                            pipeline_blocked = true;
                            break;
                        }
                    }
                }
                continue;
            }
            if is_positive_validation_segment(&normalized) {
                pipeline_validation = Some(is_strict_local_validation_segment(&normalized));
                continue;
            }

            if is_status_neutral_segment(&normalized) {
                continue;
            }

            // A command not proven read-only is a possible mutation barrier.
            // It includes unknown Python/Perl helpers and writes to external
            // systems such as `git push`; a later receipt is still useful, but
            // an earlier receipt must not survive it.
            if !crate::cloud_approval_policy::bash_command_is_read_only(&normalized) {
                pipeline_blocked = true;
                break;
            }
        }
        if pipeline_blocked {
            // Pipeline stages execute concurrently. A later validator cannot
            // prove that it ran after an earlier writer in the same pipeline.
            validation_after_barrier = false;
            strict_receipt = false;
            opaque_predecessor = true;
        } else if let Some(strict) = pipeline_validation {
            validation_after_barrier = true;
            strict_receipt = strict;
        }
        let sequence_has_rhs = control_segments[segment_index + 1..]
            .iter()
            .any(|(segment, _)| {
                let segment = segment.trim();
                !segment.is_empty() && !segment.starts_with('#')
            });
        if strict_receipt
            && (matches!(op_after, ShellControlOp::Or)
                || (matches!(op_after, ShellControlOp::Sequence) && sequence_has_rhs))
        {
            validation_after_barrier = false;
            strict_receipt = false;
        }
    }
    validation_after_barrier
}

/// Bash executors enable pipefail for real pipelines. Commands that can
/// override that setting cannot use the final stage's success as evidence.
/// This is an evidence check only, never shell execution authorization.
pub fn shell_command_preserves_pipeline_status(command: &str) -> bool {
    if !command.contains('|') {
        return true;
    }
    let Some(controls) = split_shell_control_segments_with_ops(command) else {
        return false;
    };
    if !controls.iter().any(|(segment, _)| {
        split_top_level_pipeline_segments(segment).is_some_and(|parts| parts.len() > 1)
    }) {
        return true;
    }
    controls.iter().all(|(segment, _)| {
        let Some(parts) = split_top_level_pipeline_segments(segment) else {
            return false;
        };
        parts.iter().all(|part| {
            let part = part.trim();
            let part = part
                .strip_suffix("2>&1")
                .or_else(|| part.strip_suffix("1>&2"))
                .unwrap_or(part)
                .trim();
            let bracket = part
                .strip_prefix('[')
                .and_then(|body| body.strip_suffix(']'))
                .map(|body| format!("test {}", body.trim()));
            let Some(words) = split_static_shell_words(bracket.as_deref().unwrap_or(part)) else {
                return false;
            };
            match words.first().map(String::as_str) {
                Some(
                    "source" | "." | "eval" | "builtin" | "command" | "shopt" | "trap" | "enable",
                ) => false,
                Some("set") => {
                    shell_control_segment_is_static_and_neutral(part)
                        && !words.iter().skip(1).any(|word| word.starts_with('+'))
                }
                _ => true,
            }
        })
    })
}

fn is_status_neutral_segment(segment: &str) -> bool {
    let normalized = segment.trim().to_ascii_lowercase();
    normalized == "true" || normalized == ":"
}

fn is_strict_local_validation_segment(segment: &str) -> bool {
    let Some(first) = segment.split_whitespace().next() else {
        return false;
    };
    matches!(first.to_ascii_lowercase().as_str(), "cmp" | "diff")
        || (matches!(
            first.to_ascii_lowercase().as_str(),
            "sha256sum" | "sha512sum"
        ) && segment.split_whitespace().any(|word| word == "-c"))
}

/// Return whether a shell segment contains a positive validation receipt in
/// command substitution(s) under a presentation-only wrapper.  This is a
/// deliberately small lexical recognizer, not a shell interpreter: it tracks
/// balanced `$()` and quotes, delegates each body back to the same
/// provider-neutral evidence predicate, and refuses writes/unknown wrappers.
fn nested_shell_validation_is_positive(segment: &str) -> bool {
    if segment.contains('>') {
        return false;
    }
    let Some(bodies) = command_substitution_bodies(segment) else {
        return false;
    };
    if bodies.is_empty() || !observation_wrapper_prefix(segment) {
        return false;
    }

    let mut saw_receipt = false;
    for body in bodies {
        if bash_command_has_post_mutation_validation(body) {
            saw_receipt = true;
        } else if !crate::cloud_approval_policy::bash_command_is_read_only(body) {
            // A nested command that is neither a known receipt nor a known
            // read-only formatter is a possible post-receipt mutation.
            return false;
        }
    }
    saw_receipt
}

#[derive(Clone, Copy)]
enum ShellPresentation {
    Output,
    Observation,
    Configuration { observation: bool },
}

/// Inspect the outer invocation without mistaking words inside substitutions
/// for its command or arguments. The existing lexer still bounds nested work.
fn shell_outer_words(segment: &str) -> Option<Vec<String>> {
    let bodies = command_substitution_bodies(segment)?;
    let mut outer = String::with_capacity(segment.len());
    let mut start = 0;
    for body in bodies {
        let offset = body.as_ptr() as usize - segment.as_ptr() as usize;
        outer.push_str(&segment[start..offset - 2]);
        outer.push_str("nested_output");
        start = offset + body.len() + 1;
    }
    outer.push_str(&segment[start..]);
    // Shape recognition must not require static trailing arguments. This
    // tokenizer does not execute expansions; proof still uses the static lexer.
    shell_words::split(&outer).ok()
}

fn shell_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && name.bytes().enumerate().all(|(index, byte)| {
                byte == b'_' || byte.is_ascii_alphabetic() || (index > 0 && byte.is_ascii_digit())
            })
    })
}

/// Permission to print is not proof of shell-state neutrality. Apply this
/// classification before read-only fallbacks in every evidence consumer.
fn shell_presentation(segment: &str) -> Option<ShellPresentation> {
    let words = shell_outer_words(segment);
    let raw_head = segment.split_whitespace().next().unwrap_or_default();
    let head_words = split_static_shell_words(raw_head);
    let head = words
        .as_ref()
        .and_then(|words| words.first())
        .or_else(|| head_words.as_ref().and_then(|words| words.first()))
        .map(String::as_str)
        .unwrap_or(raw_head);
    if !matches!(head, "echo" | "printf" | "test" | "[") && !shell_assignment(head) {
        return None;
    }
    if (segment.contains("${") || segment.contains("$((") || segment.contains('`'))
        && split_static_shell_words(segment).is_none()
    {
        return Some(ShellPresentation::Configuration { observation: false });
    }
    let assignment = shell_assignment(head);
    let safe_printf = head != "printf"
        || split_static_shell_words(segment).is_some_and(|words| {
            let format = words.get(if words.get(1).is_some_and(|word| word == "--") {
                2
            } else {
                1
            });
            format.is_some_and(|format| {
                !format.starts_with('-')
                    && !format.replace("%%", "").replace("%s", "").contains('%')
            })
        });
    let observation = safe_printf && nested_shell_validation_is_positive(segment);
    if assignment || !safe_printf {
        return Some(ShellPresentation::Configuration { observation });
    }
    if observation {
        return Some(ShellPresentation::Observation);
    }
    (matches!(head, "echo" | "printf") && !segment.contains("$(") && !segment.contains('>'))
        .then_some(ShellPresentation::Output)
}

/// Presentation-only shell forms for which nested command output is itself
/// an observation.  Keep this structural (assignments and standard output),
/// rather than matching task words, paths, or frameworks.
fn observation_wrapper_prefix(segment: &str) -> bool {
    let Some(words) = shell_outer_words(segment) else {
        return false;
    };
    let Some(first) = words.first().map(String::as_str) else {
        return false;
    };
    if matches!(first, "echo" | "printf" | "test" | "[") {
        return true;
    }
    // A plain shell assignment (`name=$(...)`) does not execute an outer
    // command and is safe to use as an observation carrier.  Reject `$` in
    // the variable name so arbitrary command prefixes cannot masquerade as
    // assignments.
    words.iter().all(|word| shell_assignment(word))
}

/// Extract top-level command-substitution bodies while respecting quotes,
/// escapes, and nested parentheses. `None` means an unterminated `$()` was
/// found; an empty vector means the segment has no command substitutions.
fn command_substitution_bodies(command: &str) -> Option<Vec<&str>> {
    if command.len() > MAX_NESTED_SHELL_SCAN_BYTES {
        return None;
    }
    let bytes = command.as_bytes();
    let mut bodies = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if escaped {
            escaped = false;
            index += 1;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            index += 1;
            continue;
        }
        if quote.is_none() && byte == b'#' && shell_comment_starts(bytes, index) {
            index = shell_comment_end(bytes, index);
            continue;
        }
        if (byte == b'`' && quote != Some(b'\''))
            || (quote.is_none() && bytes.get(index..index + 2) == Some(b"<<"))
        {
            return None;
        }
        if let Some(active_quote) = quote {
            // `$()` is evaluated inside double quotes, but not inside single
            // quotes. Let the normal substitution branch below handle it.
            if active_quote == b'\'' || (byte != b'$' || bytes.get(index + 1) != Some(&b'(')) {
                if byte == active_quote {
                    quote = None;
                }
                index += 1;
                continue;
            }
        }
        if byte == b'\'' || byte == b'"' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte != b'$' || bytes.get(index + 1) != Some(&b'(') {
            index += 1;
            continue;
        }

        let start = index + 2;
        let (end, body_end) = command_substitution_end(command, index)?;
        bodies.push(&command[start..body_end]);
        if bodies.len() > MAX_NESTED_SHELL_DEPTH {
            return None;
        }
        index = end;
    }
    quote.is_none().then_some(bodies)
}

fn shell_comment_starts(bytes: &[u8], index: usize) -> bool {
    if bytes.get(index) != Some(&b'#') {
        return false;
    }
    index == 0
        || matches!(
            bytes[index - 1],
            b' ' | b'\t' | b'\n' | b'\r' | b';' | b'|' | b'&' | b'('
        )
}

fn shell_comment_end(bytes: &[u8], mut index: usize) -> usize {
    while index < bytes.len() && bytes[index] != b'\n' && bytes[index] != b'\r' {
        index += 1;
    }
    index
}

fn is_shell_control_segment(segment: &str) -> bool {
    let lower = segment.to_ascii_lowercase();
    lower == "set -e"
        || lower == "set -u"
        || lower == "set -o pipefail"
        || lower.starts_with("set -e ")
        || lower.starts_with("set -u ")
        || lower.starts_with("set -o pipefail ")
        || lower.starts_with("cd ")
        || lower == "cd"
}

/// A known shell control with no dynamic expansion or output redirection.
/// Consumers may treat this as neutral while checking a compound diagnostic,
/// but an unchecked `set`/`cd` prefix is not itself proof of no side effect.
pub fn shell_control_segment_is_static_and_neutral(segment: &str) -> bool {
    let segment = segment.trim();
    let Some(words) = split_static_shell_words(segment) else {
        return false;
    };
    words
        .first()
        .is_some_and(|command| command.chars().all(|ch| !ch.is_ascii_uppercase()))
        && is_shell_control_segment(&words.join(" "))
        && !shell_segment_has_non_benign_redirect(segment)
}

fn is_positive_validation_segment(segment: &str) -> bool {
    if validation_segment_has_meta_option(segment) || shell_segment_has_non_benign_redirect(segment)
    {
        return false;
    }
    let lower = segment.to_ascii_lowercase();
    let mut words = lower.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    match first {
        "cat" | "head" | "tail" | "grep" | "rg" | "find" | "ls" | "stat" | "file" | "sha256sum"
        | "sha512sum" | "cmp" | "diff" | "test" | "[" | "curl" | "wget" => {
            // Network clients are receipts only for read requests.  Do not
            // treat a POST/upload form as observation of the workspace.
            if matches!(first, "curl" | "wget")
                && lower.split_whitespace().any(|word| {
                    matches!(
                        word,
                        "-d" | "--data"
                            | "--data-raw"
                            | "--data-binary"
                            | "--post-data"
                            | "--upload-file"
                            | "-t"
                            | "--method=post"
                            | "--method=put"
                            | "--method=patch"
                            | "--method=delete"
                    ) || (word == "-x" || word == "--request")
                })
            {
                return false;
            }
            true
        }

        "openssl" => matches!(words.next(), Some("x509" | "verify" | "s_client")),
        "nginx" => words.next() == Some("-t"),
        "systemctl" => matches!(words.next(), Some("status" | "is-active" | "is-enabled")),
        "cargo" => matches!(words.next(), Some("check" | "test" | "build")),
        "pytest" => true,
        "python" | "python3" => {
            let Some(second) = words.next() else {
                return false;
            };
            match second {
                "-m" => matches!(words.next(), Some("pytest" | "unittest" | "build")),
                // Building a local extension is also a validation receipt:
                // the command must successfully compile the changed source
                // before the completion ledger can settle it.
                "setup.py" => {
                    matches!(words.next(), Some("build" | "build_ext" | "check" | "test"))
                }
                _ => false,
            }
        }
        "npm" => {
            matches!(words.next(), Some("test"))
                || (matches!(words.next(), Some("run"))
                    && matches!(words.next(), Some("test" | "build")))
        }
        _ => false,
    }
}

/// Output redirection changes state even when the command name is a reader.
/// The two benign descriptor-forwarding forms are stripped by callers before
/// this predicate is used; an unparsed stage is conservatively treated as a
/// possible redirect/mutation rather than a receipt.
pub fn shell_segment_has_non_benign_redirect(segment: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    for byte in segment.trim().bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if byte == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if let Some(active) = quote {
            if byte == active {
                quote = None;
            }
            continue;
        }
        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'>' => return true,
            _ => {}
        }
    }
    false
}

/// Whether a tool call can change the program being validated. A possible
/// workspace mutation starts a new observation window.
fn invalidates_workspace_evidence(record: &ToolCallRecord) -> bool {
    use astra_tools::workspace_observation::{
        BOUND_WORKSPACE_SCOPE, is_changed_receipt,
        typed_workspace_desired_state_convergence_evidence_for_invocation,
    };

    if !record.was_executed() && !record.outcome_is_unknown() {
        return false;
    }
    if record.name == "write_file"
        && record.ok
        && record.execution_succeeded()
        && !record.outcome_is_unknown()
        && record.workspace_mutation_observed != Some(true)
        && record.workspace_writer_applied_bound != Some(true)
        && record.workspace_mutation_partial != Some(true)
        && record.workspace_mutation_scope.as_deref() == Some(BOUND_WORKSPACE_SCOPE)
        && let Some(receipt) = record.workspace_mutation_receipt.as_ref()
        && let Some(args) = record.authoritative_args_full()
        && let Ok(args) = serde_json::from_str::<serde_json::Value>(args)
        && typed_workspace_desired_state_convergence_evidence_for_invocation(receipt, &args)
            .is_some()
    {
        return false;
    }

    if record.workspace_mutation_observed == Some(true)
        && record.workspace_mutation_scope.as_deref() == Some(BOUND_WORKSPACE_SCOPE)
        && record
            .workspace_mutation_receipt
            .as_ref()
            .is_some_and(is_changed_receipt)
    {
        return true;
    }
    if crate::tool::categories::registry().is_shell(&record.name)
        || record.name == "run_script"
        || record.name.starts_with("mcp_")
    {
        // Neither a command shape nor a successful result attests that an
        // opaque invocation left the workspace unchanged.
        return true;
    }
    if !astra_tools::executor::may_require_workspace_serialization(&record.name) {
        return false;
    }
    let args = record
        .authoritative_args_full()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    args.as_ref().is_none_or(|args| {
        if record.name == "rollback_file_edits" && args["scope"] == "list" {
            return false;
        }
        astra_tools::executor::is_workspace_mutation_tool(&record.name, args)
    })
}

/// Accept diagnostics only from their matching root terminal. This
/// validates the existing JournalEvent wire format, without evaluating partial
/// client observations or constructing another representation of the outcome.
pub fn turn_diagnostics_from_terminal(
    value: &Value,
    session_id: Option<&str>,
    run_id: &str,
    owner_generation: Option<u64>,
    status: &str,
) -> Result<JournalEvent, &'static str> {
    let event: JournalEvent =
        serde_json::from_value(value.clone()).map_err(|_| "malformed turn diagnostics event")?;
    let session_id = session_id
        .filter(|id| !id.is_empty())
        .ok_or("turn diagnostics have no stream session identity")?;
    let generation =
        owner_generation.ok_or("turn diagnostics have no terminal execution generation")?;
    if event.event_type != astra_services::session_journal::JournalEventType::TurnEvaluation
        || event.session_id.as_deref() != Some(session_id)
        || event
            .producer_scope
            .as_ref()
            .map(|scope| scope.run_id.as_str())
            != Some(run_id)
        || event.turn.is_none()
        || event.ts.is_empty()
    {
        return Err("turn diagnostics identity does not match the root terminal");
    }
    let diagnostics = astra_services::session_journal::TurnDiagnostics::deserialize(
        event.metadata.as_ref().ok_or("missing turn diagnostics")?,
    )
    .map_err(|_| "malformed turn diagnostics")?;
    if diagnostics.execution_owner_generation != Some(generation)
        || diagnostics.run_status != status
        || !matches!(
            diagnostics.source.as_str(),
            "server_runtime" | "server_subrun"
        )
    {
        return Err("turn diagnostics authority does not match the root terminal");
    }
    Ok(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn journal_ok_call(name: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 100,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: None,
            result_preview: Some("ok".to_string()),
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        }
    }

    #[test]
    fn read_delivery_failure_does_not_permanently_block_the_turn() {
        let mut failed = journal_ok_call("read_file");
        failed.ok = false;
        failed.execution_ok = Some(true);
        failed.args_full = Some(r#"{"path":"one.txt"}"#.into());
        let mut other = journal_ok_call("read_file");
        other.args_full = Some(r#"{"path":"two.txt"}"#.into());
        assert!(!has_unresolved_tool_result_delivery_failure(&[
            failed.clone(),
            other
        ]));
        let mut recovered = journal_ok_call("read_file");
        recovered.args_full = failed.args_full.clone();
        assert!(!has_unresolved_tool_result_delivery_failure(&[
            failed.clone(),
            recovered.clone()
        ]));
        failed.name = "write_file".into();
        recovered.name = "write_file".into();
        assert!(has_unresolved_tool_result_delivery_failure(&[
            failed,
            recovered.clone()
        ]));
        assert!(!has_unresolved_tool_result_delivery_failure(&[recovered]));

        let mut database_write = journal_ok_call("mo_query");
        database_write.ok = false;
        database_write.execution_ok = Some(true);
        database_write.args_full = Some(r#"{"sql":"DELETE FROM example"}"#.into());
        assert!(has_unresolved_tool_result_delivery_failure(&[
            database_write
        ]));
        let mut rename = journal_ok_call("lsp");
        rename.ok = false;
        rename.execution_ok = Some(true);
        rename.args_full = Some(r#"{"action":"rename","dry_run":false}"#.into());
        assert!(has_unresolved_tool_result_delivery_failure(&[rename]));
        let mut symbols = journal_ok_call("symbols");
        symbols.ok = false;
        symbols.execution_ok = Some(true);
        assert!(!has_unresolved_tool_result_delivery_failure(&[symbols]));
    }

    #[test]
    fn unclassified_failed_execution_is_feedback_and_matching_success_resolves_it() {
        let args = serde_json::json!({"command": "cargo test"}).to_string();
        let mut failed = journal_ok_call("bash");
        failed.ok = false;
        failed.args_full = Some(args.clone());
        failed.result_class = None;

        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&failed)),
            1,
            "a governed failure must remain actionable even without optional classification"
        );

        let mut recovered = journal_ok_call("bash");
        recovered.args_full = Some(args);
        recovered.result_class = None;
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed, recovered]),
            0,
            "a later successful execution of the same operation resolves the fallback failure"
        );
    }

    #[test]
    fn domain_negative_exit_does_not_create_unresolved_obligation() {
        let mut record = journal_ok_call("bash");
        record.ok = false;
        record.args_full = Some(serde_json::json!({"command": "grep missing file"}).to_string());
        record.exit_semantics = Some("domain_negative".to_string());
        assert_eq!(count_unresolved_tool_outcome_failures(&[record.clone()]), 0);
        assert!(active_terminal_execution_failure_operation_keys(&[record]).is_empty());
    }

    #[test]
    fn terminal_failure_projection_keeps_diagnostic_probe_advisory() {
        let mut probe = journal_ok_call("bash");
        probe.ok = false;
        probe.args_full = Some(serde_json::json!({"command": "lsof -p 1"}).to_string());
        probe.result_class = Some("execution_error".into());
        assert!(active_terminal_execution_failure_operation_keys(&[probe]).is_empty());

        let mut validation = journal_ok_call("bash");
        validation.ok = false;
        validation.args_full = Some(serde_json::json!({"command": "cargo test"}).to_string());
        validation.result_class = Some("test_failure".into());
        assert_eq!(
            active_terminal_execution_failure_operation_keys(&[validation]).len(),
            1
        );
    }

    #[test]
    fn rejected_typed_agent_result_is_not_an_unresolved_execution() {
        let mut record = journal_ok_call("agent");
        record.ok = false;
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        record.args_full =
            Some(serde_json::json!({"action":"spawn","prompt":"review"}).to_string());
        record.result_class = Some("agent_incomplete".to_string());

        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&record)),
            0
        );
        assert!(active_terminal_execution_failure_operation_keys(&[record]).is_empty());
    }

    #[test]
    fn unknown_start_remains_unresolved_without_counting_as_executed() {
        let mut record = journal_ok_call("write_file");
        record.ok = false;
        record.error_kind = Some(astra_core::ErrorKind::ToolUnavailable);
        record.disposition = Some(astra_services::session_journal::ToolCallDisposition::Unknown);
        let restored: ToolCallRecord =
            serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap();
        assert!(!restored.was_executed());
        assert!(restored.outcome_is_unknown());
        assert_eq!(count_unresolved_tool_outcome_failures(&[restored]), 1);
    }

    #[test]
    fn admitted_coordination_is_not_a_failed_child_execution_or_validation() {
        let mut record = journal_ok_call("agent");
        record.tool_call_id = Some("wait-call".into());
        record.args_full =
            Some(serde_json::json!({"action":"wait","timeout_ms":120000}).to_string());
        record.result_full = Some(
            serde_json::json!({
                "result_family":"control_receipt", "action":"wait", "success":true,
                "status":"wait_admitted",
                "wait_request":{"parent_run_id":"parent","tool_call_id":record.tool_call_id,
                    "timeout_ms":120000}
            })
            .to_string(),
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&record)),
            0
        );
        assert!(
            !tool_outcome_is_positive_success(&record),
            "coordination admission is not evidence that child work or validation succeeded"
        );
        let mut annotated = record.clone();
        annotated.result_class = Some("success".into());
        annotated.exit_semantics = Some("success".into());
        assert!(
            !tool_outcome_is_positive_success(&annotated),
            "generic success metadata cannot promote a coordination receipt to validation"
        );
        let receipt: Value = serde_json::from_str(record.result_full.as_deref().unwrap()).unwrap();
        let mut opaque = record.clone();
        opaque.result_full = Some("opaque executor failure".into());
        opaque.result_class = Some("execution_error".into());
        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&opaque)),
            1
        );
        opaque.result_class = None;
        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&opaque)),
            1
        );
        opaque.ok = false;
        opaque.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        opaque.result_class = Some("execution_error".into());
        assert_eq!(
            effective_tool_result_class(&opaque).as_deref(),
            Some("execution_error"),
            "a rejected non-JSON executor error must retain its failure classification"
        );
        assert_eq!(terminal_rejected_attempt_count(&[opaque]), 1);
        let mut disguised = record.clone();
        let mut envelope = receipt.clone();
        envelope["result_family"] = serde_json::json!("unknown");
        envelope["group_id"] = serde_json::json!("group");
        envelope["results"] = serde_json::json!([]);
        disguised.result_full = Some(envelope.to_string());
        assert_eq!(
            count_unresolved_tool_outcome_failures(std::slice::from_ref(&disguised)),
            1
        );
        for invalid_field in ["result_family", "success", "wait_request"] {
            let mut malformed = receipt.clone();
            malformed.as_object_mut().unwrap().remove(invalid_field);
            let mut rejected = record.clone();
            rejected.result_full = Some(malformed.to_string());
            assert_eq!(
                count_unresolved_tool_outcome_failures(std::slice::from_ref(&rejected)),
                1,
                "missing {invalid_field} cannot acquire coordination authority"
            );
            assert!(!tool_outcome_is_positive_success(&rejected));
        }
        for (field, value) in [
            ("result_family", serde_json::json!("unknown")),
            ("success", serde_json::json!(false)),
            ("incomplete", serde_json::json!(true)),
        ] {
            let mut malformed = receipt.clone();
            malformed[field] = value;
            let mut rejected = record.clone();
            rejected.result_full = Some(malformed.to_string());
            assert_eq!(
                count_unresolved_tool_outcome_failures(std::slice::from_ref(&rejected)),
                1
            );
        }
        let mut verifier = journal_ok_call("bash");
        verifier.ok = false;
        verifier.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
        verifier.result_class = Some("test_failure".into());
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[verifier, record.clone()]),
            1,
            "a successful coordination receipt must not erase a failed verifier"
        );
        for output in [
            serde_json::json!({"status":"completed","agent_id":"child","result":""}),
            serde_json::json!({"status":"interrupted","agent_id":"child"}),
            serde_json::json!({"status":"unknown_status","agent_id":"child"}),
        ] {
            let mut incomplete = record.clone();
            incomplete.args_full =
                Some(serde_json::json!({"action":"get_result","agent_id":"child"}).to_string());
            let mut output = output;
            output["result_family"] = serde_json::json!("child_result");
            incomplete.result_full = Some(output.to_string());
            assert_eq!(
                count_unresolved_tool_outcome_failures(std::slice::from_ref(&incomplete)),
                1
            );
            assert!(!tool_outcome_is_positive_success(&incomplete));
        }
    }

    #[test]
    fn observed_exits_and_failed_validation_have_distinct_outcomes() {
        use astra_core::ErrorKind;
        use astra_services::session_journal::ToolCallDisposition;

        let command = |text: &str, kind: ErrorKind| ToolCallRecord {
            name: "bash".into(),
            ok: false,
            disposition: Some(ToolCallDisposition::Executed),
            args_full: Some(serde_json::json!({"command": text}).to_string()),
            error_kind: Some(kind),
            recovery_evidence: Some(astra_tools::exit_semantics::command_failed_evidence(kind)),
            exit_semantics: Some(
                if kind == ErrorKind::ToolTimeout {
                    "timed_out"
                } else {
                    "execution_error"
                }
                .into(),
            ),
            result_class: Some("execution_error".into()),
            ..Default::default()
        };
        let grep = command("grep missing /dev/null", ErrorKind::Unknown);
        let diff = command("diff -u before after", ErrorKind::Unknown);
        let timeout = command("diff -u before after", ErrorKind::ToolTimeout);
        let validation = command("cargo test", ErrorKind::Unknown);
        let executor_failure = command("diff -u before after", ErrorKind::ToolUnavailable);

        for (record, expected_observation) in [
            (&grep, true),
            (&diff, true),
            (&timeout, false),
            (&validation, false),
            (&executor_failure, false),
        ] {
            let records = std::slice::from_ref(record);
            assert_eq!(
                count_unresolved_tool_outcome_failures(records),
                usize::from(!expected_observation)
            );
            assert!(!tool_outcome_is_positive_success(record));
            assert!(!record.ok);
        }
        assert_eq!(count_unresolved_tool_outcome_failures(&[timeout, diff]), 1);
    }

    #[test]
    fn positive_success_is_stricter_than_completed_domain_outcome() {
        let mut record = journal_ok_call("bash");
        record.args_full =
            Some(serde_json::json!({"command": "cargo test | grep PASSED"}).to_string());
        record.result_class = Some("empty_result".to_string());
        record.exit_semantics = Some("empty_result".to_string());
        assert!(record.ok, "the shell transport itself completed");
        assert!(!tool_outcome_is_positive_success(&record));

        record.result_class = Some("success".to_string());
        record.exit_semantics = Some("success".to_string());
        assert!(tool_outcome_is_positive_success(&record));

        record.exit_semantics = Some("execution_error".to_string());
        assert!(
            !tool_outcome_is_positive_success(&record),
            "a failed pipeline is not an affirmative validation receipt"
        );

        record.exit_semantics = None;
        assert!(
            !tool_outcome_is_positive_success(&record),
            "missing process semantics must remain fail-closed"
        );
    }

    #[test]
    fn typed_failure_overrides_domain_negative_process_semantics() {
        let mut record = journal_ok_call("bash");
        record.ok = false;
        record.args_full = Some(serde_json::json!({"command": "cargo test"}).to_string());
        record.result_class = Some("test_failure".to_string());
        record.exit_semantics = Some("domain_negative".to_string());
        assert_eq!(count_unresolved_tool_outcome_failures(&[record.clone()]), 1);
        assert_eq!(
            active_terminal_execution_failure_operation_keys(&[record]).len(),
            1
        );
    }

    #[test]
    fn unclassified_failure_without_identity_is_not_erased_by_same_named_success() {
        let mut failed = journal_ok_call("bash");
        failed.ok = false;
        failed.result_class = None;

        let recovered = journal_ok_call("bash");
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed, recovered]),
            1,
            "without an operation identity, an unrelated success must not erase feedback"
        );

        let mut failed_with_id = journal_ok_call("bash");
        failed_with_id.ok = false;
        failed_with_id.result_class = None;
        failed_with_id.tool_call_id = Some("call-1".to_string());
        failed_with_id.round = Some(1);
        let mut recovered_with_id = journal_ok_call("bash");
        recovered_with_id.tool_call_id = Some("call-1".to_string());
        recovered_with_id.round = Some(2);
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed_with_id, recovered_with_id]),
            1,
            "an unscoped provider call ID does not establish that two commands match"
        );

        let args = serde_json::json!({"command": "cargo test"}).to_string();
        let mut failed_retry = journal_ok_call("bash");
        failed_retry.ok = false;
        failed_retry.args_full = Some(args.clone());
        failed_retry.tool_call_id = Some("failed-attempt".to_string());
        let mut recovered_retry = journal_ok_call("bash");
        recovered_retry.args_full = Some(args);
        recovered_retry.tool_call_id = Some("new-attempt".to_string());
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed_retry, recovered_retry]),
            0,
            "stable operation arguments must resolve a retry even when provider ids rotate"
        );
    }

    #[test]
    fn active_operation_projection_matches_terminal_recovery_identity() {
        let mut failed = journal_ok_call("bash");
        failed.ok = false;
        failed.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
        failed.result_class = Some("test_failure".into());

        let mut unrelated_success = journal_ok_call("bash");
        unrelated_success.args_full =
            Some(serde_json::json!({"command":"cargo build"}).to_string());
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[failed.clone(), unrelated_success]),
            1
        );

        let mut matching_success = journal_ok_call("bash");
        matching_success.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
        assert!(
            active_terminal_execution_failure_operation_keys(&[failed, matching_success.clone()])
                .is_empty()
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[
                {
                    let mut f = journal_ok_call("bash");
                    f.ok = false;
                    f.args_full = Some(serde_json::json!({"command":"cargo test"}).to_string());
                    f.result_class = Some("test_failure".into());
                    f
                },
                matching_success,
            ]),
            0
        );
    }

    #[test]
    fn operation_identity_is_stable_across_result_shape_and_json_key_order() {
        let mut typed_failure = journal_ok_call("bash");
        typed_failure.ok = false;
        typed_failure.result_class = Some("execution_error".into());
        typed_failure.args_full = Some(r#"{"command":"probe","options":{"z":1,"a":2}}"#.into());

        let mut untyped_retry = journal_ok_call("bash");
        untyped_retry.result_class = None;
        untyped_retry.args_full = Some(r#"{"options":{"a":2,"z":1},"command":"probe"}"#.into());

        assert_eq!(
            operation_identity_key(&typed_failure),
            operation_identity_key(&untyped_retry),
            "result classification and JSON object ordering must not change operation identity"
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[typed_failure, untyped_retry]),
            0,
            "a successful untyped retry must resolve a typed failure for the same operation"
        );
    }

    #[test]
    fn operation_identity_keeps_distinct_arguments_isolated() {
        let mut first = journal_ok_call("bash");
        first.ok = false;
        first.result_class = Some("execution_error".into());
        first.args_full = Some(r#"{"command":"probe-a"}"#.into());

        let mut different = journal_ok_call("bash");
        different.args_full = Some(r#"{"command":"probe-b"}"#.into());

        assert_ne!(
            operation_identity_key(&first),
            operation_identity_key(&different)
        );
        assert_eq!(
            count_unresolved_tool_outcome_failures(&[first, different]),
            1,
            "a different operation must not clear the active failure"
        );
    }

    #[test]
    fn repeated_observation_evidence_requires_same_read_and_delivered_result() {
        for selector in [
            serde_json::json!({"artifact": "artifact://session/explain-analyze/report-1", "offset": 0}),
            serde_json::json!({"explain": {"target": "previous"}}),
            serde_json::json!({"explain": {"target": "run", "run_id": "run-1"}}),
        ] {
            let mut report = journal_ok_call("introspect");
            report.args_full = Some(selector.to_string());
            report.result_full = Some("bounded report window".into());
            let fact = ToolEvaluationFact::from_record(&report);
            assert!(observation_scope(&report).is_none());
            assert!(fact.observation_coverage().is_none());
            assert_eq!(
                trailing_repeated_observation_evidence(&[fact.clone(), fact]),
                2
            );

            let mut next_window = report.clone();
            let mut args = selector;
            args["max_bytes"] = serde_json::json!(1024);
            next_window.args_full = Some(args.to_string());
            assert_eq!(
                trailing_repeated_observation_evidence(&[
                    ToolEvaluationFact::from_record(&report),
                    ToolEvaluationFact::from_record(&next_window),
                ]),
                1,
                "report requests retain exact identities, even when their returned bytes match"
            );
        }

        let read = || {
            let mut record = journal_ok_call("read_file");
            record.disposition =
                Some(astra_services::session_journal::ToolCallDisposition::Executed);
            record.args_full = Some(serde_json::json!({"path": "a.txt"}).to_string());
            record.result_full = Some("snapshot".into());
            record
        };

        let first = read();
        let second = read();
        let third = read();
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&first),
                ToolEvaluationFact::from_record(&second),
                ToolEvaluationFact::from_record(&third),
            ],),
            3
        );

        let mut changed_result = read();
        changed_result.result_full = Some("new snapshot".into());
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&first),
                ToolEvaluationFact::from_record(&changed_result),
            ]),
            1,
            "a changed delivered result is new evidence"
        );

        let descriptor = || astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind: Default::default(),
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: "call-1".into(),
            run_id: "run-1".into(),
            byte_len: 8,
            content_sha256: "same-artifact".into(),
        };
        let mut stored_first = read();
        stored_first.result_artifact = Some(descriptor());
        let mut stored_changed = read();
        stored_changed.result_artifact = Some(descriptor());
        stored_changed.result_full = Some("recovered window".into());
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&stored_first),
                ToolEvaluationFact::from_record(&stored_changed),
            ]),
            0,
            "artifact-only records do not prove any model-visible projection"
        );

        let mut live_artifact_first = read();
        live_artifact_first.result_full =
            Some("<persisted-output>artifact handle</persisted-output>".into());
        live_artifact_first.result_artifact = Some(descriptor());
        live_artifact_first.runtime_model_result_full = Some("snapshot".into());
        let mut live_artifact_second = live_artifact_first.clone();
        live_artifact_second.round = Some(2);
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&live_artifact_first),
                ToolEvaluationFact::from_record(&live_artifact_second),
            ]),
            2,
            "the live model projection remains usable even when the durable result is an artifact"
        );

        let mut overview = journal_ok_call("introspect");
        overview.args_full =
            Some(serde_json::json!({"facet": "overview", "format": "json"}).to_string());
        overview.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "covered_facets": ["overview"],
                "summary": "bounded composite overview"
            })
            .to_string(),
        );
        let mut overview_with_artifact_window = overview.clone();
        overview_with_artifact_window.args_full = Some(
            serde_json::json!({
                "facet": "overview",
                "format": "json",
                "max_bytes": 16384
            })
            .to_string(),
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&overview_with_artifact_window),
            ]),
            2,
            "live introspection ignores artifact-only max_bytes for request identity"
        );

        let mut trace = overview.clone();
        trace.args_full = Some(serde_json::json!({"facet": "trace", "format": "json"}).to_string());
        trace.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "covered_facets": ["trace"],
                "summary": "the same snapshot, a different facet"
            })
            .to_string(),
        );
        assert!(
            ToolEvaluationFact::from_record(&overview)
                .observation_coverage()
                .is_some()
        );
        assert!(
            ToolEvaluationFact::from_record(&trace)
                .observation_coverage()
                .is_some()
        );
        assert_eq!(
            ToolEvaluationFact::from_record(&overview)
                .observation_evidence_identity()
                .map(|(_, result)| result),
            ToolEvaluationFact::from_record(&trace)
                .observation_evidence_identity()
                .map(|(_, result)| result),
            "facet-specific reads should share the source snapshot revision"
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&trace),
            ]),
            1,
            "a bounded JSON overview does not claim to have delivered trace detail"
        );

        let mut errors = trace.clone();
        errors.args_full =
            Some(serde_json::json!({"facet": "errors", "format": "json"}).to_string());
        errors.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "covered_facets": ["errors"],
                "summary": "another facet already covered by overview"
            })
            .to_string(),
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&trace),
                ToolEvaluationFact::from_record(&errors),
            ]),
            1,
            "different facets must not be merged without explicit containment"
        );

        let mut expanded_overview = overview.clone();
        expanded_overview.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "covered_facets": ["overview", "trace"],
                "summary": "the same snapshot with additional delivered detail"
            })
            .to_string(),
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&expanded_overview),
            ]),
            1,
            "same revision with expanded delivered coverage is new evidence"
        );

        let mut missing_coverage = overview.clone();
        missing_coverage.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "summary": "coverage marker omitted"
            })
            .to_string(),
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&missing_coverage),
            ]),
            0,
            "an introspection result without delivered coverage must fail closed"
        );

        let mut bounded_overview = overview.clone();
        bounded_overview.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "covered_facets": [],
                "summary": "the report was bounded before facet coverage could be claimed"
            })
            .to_string(),
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&bounded_overview),
                ToolEvaluationFact::from_record(&bounded_overview),
            ]),
            2,
            "an explicit empty coverage marker still proves identical delivery"
        );
        let mut reused_bounded_overview = bounded_overview.clone();
        reused_bounded_overview.disposition =
            Some(astra_services::session_journal::ToolCallDisposition::Reused);
        reused_bounded_overview.result_class =
            Some(astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.into());
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&bounded_overview),
                ToolEvaluationFact::from_record(&reused_bounded_overview),
            ]),
            2,
            "a cache-reused result with full delivery identity participates in repetition evidence"
        );

        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&trace),
                ToolEvaluationFact::from_record(&overview),
            ]),
            1,
            "a broader facet that adds coverage must reset the redundant suffix"
        );

        let mut changed_overview = overview.clone();
        changed_overview.result_full = Some(
            serde_json::json!({
                "evidence_revision": "v1:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "covered_facets": ["overview"],
                "summary": "the target evidence changed"
            })
            .to_string(),
        );
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&changed_overview),
            ]),
            1,
            "a source-owned evidence revision change is new evidence"
        );

        let mut unmarked_overview = overview.clone();
        unmarked_overview.result_full = Some("same overview without a marker".into());
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&overview),
                ToolEvaluationFact::from_record(&unmarked_overview),
            ]),
            0,
            "unmarked introspection output must fail closed"
        );

        let mut mutation = read();
        mutation.name = "write_file".into();
        mutation.args_full =
            Some(serde_json::json!({"path": "a.txt", "content": "changed"}).to_string());
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&first),
                ToolEvaluationFact::from_record(&mutation),
            ]),
            0,
            "a mutation must break an observation-only run"
        );

        let mut unknown_result = read();
        unknown_result.result_full = None;
        assert_eq!(
            trailing_repeated_observation_evidence(&[
                ToolEvaluationFact::from_record(&first),
                ToolEvaluationFact::from_record(&unknown_result),
            ]),
            0,
            "a missing result must not be treated as unchanged evidence"
        );
    }

    #[test]
    fn model_catalog_pages_do_not_collapse_into_one_live_snapshot_request() {
        let mut first = journal_ok_call("model_catalog");
        first.args_full = Some(serde_json::json!({}).to_string());
        first.result_full = Some("first complete JSON page".into());
        let mut second = first.clone();
        second.args_full =
            Some(serde_json::json!({"cursor":"next","catalog_revision":"revision"}).to_string());
        second.result_full = Some("second complete JSON page".into());
        assert!(observation_scope(&first).is_none());
        assert!(observation_scope(&second).is_none());
        let a: serde_json::Value = serde_json::from_str(first.args_full.as_ref().unwrap()).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(second.args_full.as_ref().unwrap()).unwrap();
        assert_ne!(
            observation_request_key(&first, Some(&a)),
            observation_request_key(&second, Some(&b))
        );
    }

    #[test]
    fn validation_recovery_does_not_cross_working_directories() {
        for (failed_args, success_args) in [
            (
                serde_json::json!({"command": "cd a && cargo test"}),
                serde_json::json!({"command": "cd b && cargo test"}),
            ),
            (
                serde_json::json!({"command": "cargo test", "working_dir": "a"}),
                serde_json::json!({"command": "cargo test", "working_dir": "b"}),
            ),
        ] {
            let mut failed = journal_ok_call("bash");
            failed.ok = false;
            failed.result_class = Some("test_failure".into());
            failed.args_full = Some(failed_args.to_string());
            let mut success = journal_ok_call("bash");
            success.args_full = Some(success_args.to_string());
            success.result_class = Some("success".into());
            let mut matching_success = success.clone();
            matching_success.args_full = failed.args_full.clone();
            let records = [failed.clone(), success];
            assert_eq!(count_unresolved_tool_outcome_failures(&records), 1);
            assert_eq!(
                active_terminal_execution_failure_operation_keys(&records).len(),
                1
            );
            assert_eq!(
                count_unresolved_tool_outcome_failures(&[failed.clone(), matching_success.clone()]),
                0,
                "an exact retry at the original target must still resolve its failure"
            );
            assert!(
                active_terminal_execution_failure_operation_keys(&[failed, matching_success])
                    .is_empty()
            );
        }
    }

    #[test]
    fn rejected_operation_identity_is_cleared_by_canonical_success() {
        let mut rejected = journal_ok_call("bash");
        rejected.ok = false;
        rejected.disposition = Some(astra_services::session_journal::ToolCallDisposition::Rejected);
        rejected.args_full = Some(r#"{"command":"probe","options":{"z":1,"a":2}}"#.into());

        let mut recovered = journal_ok_call("bash");
        recovered.args_full = Some(r#"{"options":{"a":2,"z":1},"command":"probe"}"#.into());

        assert!(active_rejected_operation_keys(&[rejected, recovered]).is_empty());
        assert_eq!(
            unresolved_blocked_attempt_count(&[
                {
                    let mut record = journal_ok_call("bash");
                    record.ok = false;
                    record.disposition =
                        Some(astra_services::session_journal::ToolCallDisposition::Rejected);
                    record.args_full =
                        Some(r#"{"command":"probe","options":{"z":1,"a":2}}"#.into());
                    record
                },
                {
                    let mut record = journal_ok_call("bash");
                    record.args_full =
                        Some(r#"{"options":{"a":2,"z":1},"command":"probe"}"#.into());
                    record
                },
            ]),
            0
        );
    }

    // ── evaluate_turn quality levels ──

    #[test]
    fn is_synthetic_placeholder_via_flag() {
        use astra_services::session_journal::{SURGICAL_REMOVAL_TOOL_NAME, ToolCallRecord};

        // Current contract: surgically_removed flag set
        let flagged = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: Some(true),
            original_tool_name: Some("bash".to_string()),
            ..Default::default()
        };
        assert!(flagged.is_synthetic_placeholder());

        // Sentinel name alone is not a supported synthetic marker.
        let unflagged_sentinel = ToolCallRecord {
            name: SURGICAL_REMOVAL_TOOL_NAME.to_string(),
            ok: true,
            ms: 0,
            error: None,
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(!unflagged_sentinel.is_synthetic_placeholder());

        // Normal tool call: neither flag nor sentinel name
        let normal = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: None,
            output_bytes: Some(200),
            args_preview: None,
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            ..Default::default()
        };
        assert!(!normal.is_synthetic_placeholder());
    }

    // ─── Sequential read-churn (real-session-shaped fixtures) ───────────
    //
    // Real sessions inspected to calibrate the threshold:
    //   - 6566d6a8 turn 1: 10 consecutive single-tool read rounds → wasted
    //   - bbae8641 turn 3: 11 consecutive single-tool read rounds → wasted
    //   - 6da9cf8f turn 6: 16 consecutive single-tool read rounds → wasted
    //   - 03945541 turn 1: 6 single-tool rounds (locate→read chain) → healthy
    //   - 03945541 turn 2: max 4 single-tool rounds (mutate→verify) → healthy
    // Threshold of 8 cleanly separates these populations.

    fn record_with_args(name: &str, round: u32, args_full: &str) -> ToolCallRecord {
        ToolCallRecord {
            name: name.to_string(),
            ok: true,
            ms: 50,
            error: None,
            input_bytes: Some(12),
            output_bytes: Some(500),
            args_preview: Some(args_full.chars().take(80).collect()),
            result_preview: None,
            file_path: None,
            surgically_removed: None,
            original_tool_name: None,
            batch_id: None,
            parallel: Some(false),
            round: Some(round),
            args_full: Some(args_full.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn requested_read_ranges_do_not_imply_redundancy_or_delivery() {
        let mut records = vec![record_with_args(
            "read_file",
            0,
            r#"{"path":"review.rs","start_line":50,"end_line":400}"#,
        )];
        records[0].result_full =
            Some("50\tfirst line\n114\tlast delivered line\n[truncated; continue at 115]".into());
        for (round, (start, end)) in [(115, 145), (146, 200), (201, 250)].into_iter().enumerate() {
            let args = serde_json::json!({"path":"review.rs", "start_line":start, "end_line":end});
            let mut record = record_with_args("read_file", round as u32 + 1, &args.to_string());
            record.result_full = Some(format!("{start}\tnew evidence\n{end}\tlast line"));
            records.push(record);
        }
        let facts: Vec<_> = records
            .iter()
            .map(ToolEvaluationFact::from_record)
            .collect();
        assert_eq!(trailing_repeated_observation_evidence(&facts), 1);
    }

    #[test]
    fn corrected_exploratory_read_preserves_history_without_execution_obligation() {
        let mut failed = record_with_args("read_file", 0, r#"{"path":"wrong.rs"}"#);
        failed.ok = false;
        failed.error_kind = Some(astra_core::ErrorKind::ToolNotFound);
        failed.error = Some("missing resource".into());
        let recovered = record_with_args("read_file", 1, r#"{"path":"correct.rs"}"#);
        let records = [failed, recovered];
        let facts: Vec<_> = records
            .iter()
            .map(ToolEvaluationFact::from_record)
            .collect();
        assert_eq!(
            unresolved_tool_outcome_facts(&facts).len(),
            1,
            "changed arguments do not erase historical failures"
        );
        assert_eq!(count_unresolved_tool_outcome_failures(&records), 0);
        assert!(active_terminal_execution_failure_operation_keys(&records).is_empty());
        assert!(active_execution_failure_fact_keys(&facts).is_empty());
    }

    #[test]
    fn observation_success_does_not_resolve_validation_mutation_or_unknown_effects() {
        for (name, args, class, kind) in [
            (
                "bash",
                r#"{"command":"cargo test -p example"}"#,
                Some("test_failure"),
                None,
            ),
            (
                "write_file",
                r#"{"path":"result.txt","content":"new"}"#,
                None,
                None,
            ),
            ("foreign_tool", r#"{"path":"result.txt"}"#, None, None),
            (
                "read_file",
                r#"{"path":"result.txt"}"#,
                Some("test_failure"),
                None,
            ),
            (
                "read_file",
                r#"{"path":"result.txt"}"#,
                None,
                Some(astra_core::ErrorKind::ToolOutcomeUnknown),
            ),
        ] {
            let mut failed = record_with_args(name, 0, args);
            failed.ok = false;
            failed.result_class = class.map(str::to_string);
            failed.error_kind = kind;
            let unrelated = record_with_args("read_file", 1, r#"{"path":"other.txt"}"#);
            let records = [failed.clone(), unrelated];
            assert_eq!(
                count_unresolved_tool_outcome_failures(&records),
                1,
                "{name}/{class:?}"
            );
            let facts: Vec<_> = records
                .iter()
                .map(ToolEvaluationFact::from_record)
                .collect();
            assert_eq!(
                active_execution_failure_fact_keys(&facts).len(),
                1,
                "{name}/{class:?}"
            );
            let matching = record_with_args(name, 2, args);
            assert_eq!(
                count_unresolved_tool_outcome_failures(&[failed, matching]),
                0,
                "exact success remains valid recovery evidence"
            );
        }
    }

    #[test]
    fn restored_redacted_arguments_neither_merge_nor_resolve_failures() {
        let records: Vec<_> = ["fixture-alpha", "fixture-beta", "fixture-gamma"]
            .into_iter()
            .enumerate()
            .map(|(index, target)| ToolCallRecord {
                name: "bash".into(),
                args_full: Some(r#"{"command":"cargo test --token [REDACTED:secret]"}"#.into()),
                runtime_args_full: Some(
                    json!({"command":format!("cargo test --token {target}")}).to_string(),
                ),
                args_preview: Some("cargo test --token ...".into()),
                tool_call_id: Some(format!("call-{index}")),
                ok: index == 2,
                result_class: Some(
                    if index == 2 {
                        "success"
                    } else {
                        "test_failure"
                    }
                    .into(),
                ),
                ..Default::default()
            })
            .collect();
        assert_eq!(count_unresolved_tool_outcome_failures(&records), 2);
        let restored: Vec<ToolCallRecord> =
            serde_json::from_value(serde_json::to_value(records).unwrap()).unwrap();
        assert!(
            restored
                .iter()
                .all(|record| record.authoritative_args_full().is_none())
        );
        let facts: Vec<_> = restored
            .iter()
            .map(ToolEvaluationFact::from_record)
            .collect();
        assert!(facts.iter().all(|fact| fact.operation_identity().is_none()));
        assert_eq!(unresolved_tool_outcome_facts(&facts).len(), 2);
        assert_eq!(count_unresolved_tool_outcome_failures(&restored), 2);
        assert_eq!(
            active_terminal_execution_failure_operation_keys(&restored).len(),
            2
        );
        assert!(
            active_execution_failure_fact_keys(&facts).is_empty(),
            "opaque history cannot authorize policy recovery"
        );
    }

    #[test]
    fn delivered_typed_failures_remain_execution_evidence() {
        for (name, class) in [
            ("read_file", "test_failure"),
            ("agent", RESULT_CLASS_AGENT_INCOMPLETE),
            ("agent_fanout", RESULT_CLASS_FANOUT_INCOMPLETE),
        ] {
            let mut failed = record_with_args(name, 0, r#"{"target":"requested-work"}"#);
            assert!(failed.ok, "the tool envelope was delivered successfully");
            failed.result_class = Some(class.into());
            let unrelated = record_with_args("read_file", 1, r#"{"path":"other.txt"}"#);
            let records = [failed, unrelated];
            let facts: Vec<_> = records
                .iter()
                .map(ToolEvaluationFact::from_record)
                .collect();
            assert_eq!(
                count_unresolved_tool_outcome_failures(&records),
                1,
                "{class}"
            );
            assert_eq!(
                active_execution_failure_fact_keys(&facts).len(),
                1,
                "{class}"
            );
            assert_eq!(
                active_terminal_execution_failure_operation_keys(&records).len(),
                1,
                "{class} must not become successful because its envelope arrived"
            );
        }
    }

    #[test]
    fn observational_failure_does_not_replace_unknown_outcome() {
        let args = r#"{"path":"requested.txt"}"#;
        let mut unknown = record_with_args("read_file", 0, args);
        unknown.ok = false;
        unknown.error_kind = Some(astra_core::ErrorKind::ToolOutcomeUnknown);
        let mut missing = record_with_args("read_file", 1, args);
        missing.ok = false;
        missing.error_kind = Some(astra_core::ErrorKind::ToolNotFound);
        let records = [unknown.clone(), missing.clone()];
        let facts: Vec<_> = records
            .iter()
            .map(ToolEvaluationFact::from_record)
            .collect();
        assert_eq!(
            unresolved_tool_outcome_facts(&facts)
                .values()
                .next()
                .unwrap()
                .fact_index,
            1,
            "the raw audit still retains the latest failed observation"
        );
        assert_eq!(
            unresolved_execution_outcome_facts(&facts)
                .values()
                .next()
                .unwrap()
                .fact_index,
            0,
            "a different failure cannot establish the unknown operation's outcome"
        );
        assert_eq!(count_unresolved_tool_outcome_failures(&records), 1);
        assert_eq!(active_execution_failure_fact_keys(&facts).len(), 1);
        assert_eq!(
            active_terminal_execution_failure_operation_keys(&records).len(),
            1
        );

        let recovered = record_with_args("read_file", 2, args);
        let records = [unknown, missing, recovered];
        assert_eq!(count_unresolved_tool_outcome_failures(&records), 0);
        assert!(active_terminal_execution_failure_operation_keys(&records).is_empty());
    }

    #[test]
    fn compound_shell_receipt_must_follow_the_last_possible_mutation() {
        assert!(bash_command_has_post_mutation_validation(
            "rm -rf build && git push origin main && cat dist/index.html && curl -sk https://localhost/"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "set -e; cargo test -p example; test -f target/debug/example"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "sed -i 's/np.int/np.intp/' source.py && python setup.py build_ext --inplace 2>&1 | tail -20"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "python3 -m build --wheel"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "python3 -m pytest 2>&1"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "python3 -m pytest 1>&2"
        ));
        // A validator followed by an alternate/fallback command is not a
        // status-determining receipt: the shell can report success even when
        // the validator failed.
        assert!(!bash_command_has_post_mutation_validation(
            "cargo test || true"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cargo test; true"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "cargo test && true"
        ));
        assert!(bash_command_post_mutation_validation_prefix("cargo test || true").is_none());
        assert!(bash_command_post_mutation_validation_prefix("cargo test; true").is_none());
        assert!(bash_command_post_mutation_validation_prefix("cargo test && true").is_some());
        assert!(bash_command_has_post_mutation_validation("cargo test;"));
        assert!(bash_command_has_post_mutation_validation("cargo test\n"));
        assert!(bash_command_has_post_mutation_validation(
            "cargo test # comment\n"
        ));
        assert!(bash_command_post_mutation_validation_prefix("cargo test;").is_some());
        assert!(bash_command_post_mutation_validation_prefix("cargo test\n").is_some());
        assert!(bash_command_post_mutation_validation_prefix("cargo test # comment\n").is_some());
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest 1>&1"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest 2>&2"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest 3>&1"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "python3 -m pytest &"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cat dist/index.html && rm -rf build"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "rm -rf build && echo done"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "curl -X POST https://localhost/deploy"
        ));
        assert_eq!(
            bash_command_post_mutation_validation_prefix(
                "sed -i 's/a/b/' src/lib.rs && cargo test"
            ),
            Some("cargo test".into())
        );
        assert_eq!(
            bash_command_post_mutation_validation_prefix(
                "cargo test && sed -i 's/a/b/' src/lib.rs && test -e src/lib.rs"
            ),
            None
        );
        assert!(bash_command_post_mutation_validation_prefix("npx tsc --noEmit").is_some());
        for command in [
            "CARGO TEST",
            "cargo TEST",
            "python3 -m PYTEST",
            "npm run BUILD",
        ] {
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_none(),
                "case-sensitive executable or subcommand cannot prove validation: {command}"
            );
        }
        assert!(bash_command_post_mutation_validation_prefix("npm run build").is_some());
        assert!(
            bash_command_post_mutation_validation_prefix("python setup.py build_ext --inplace")
                .is_some()
        );
        for command in [
            "cargo test --help",
            "cargo test --version",
            "cargo test '--help'",
            "cargo test \"$MODE\"",
            "pytest --help",
            "pytest \"$MODE\"",
            "python3 -m pytest -h",
            "npx tsc --noEmit --help",
        ] {
            assert!(
                !bash_command_has_post_mutation_validation(command),
                "metadata mode is not validation evidence: {command}"
            );
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_none(),
                "metadata mode is not a strict validation receipt: {command}"
            );
        }
        for command in ["cargo test -v", "go test -v", "pytest -v"] {
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_some(),
                "verbose validation must remain a receipt: {command}"
            );
        }
        for command in [
            "touch /workspace/out | cargo test",
            "cargo test | touch /workspace/out",
            "time cp /workspace/source /workspace/out | cargo test",
            "time cat /workspace/source > /workspace/out | cargo test",
            "cd /workspace > /workspace/out | cargo test",
            "set -e > /workspace/out | cargo test",
            "cat /workspace/source > /workspace/output",
        ] {
            assert!(
                !bash_command_has_post_mutation_validation(command),
                "a concurrent writer cannot be ordered around a validator: {command}"
            );
            assert!(
                bash_command_post_mutation_validation_prefix(command).is_none(),
                "a concurrent writer cannot produce a strict receipt: {command}"
            );
        }
        // A `>` inside a quoted argument is data, not shell redirection. The
        // same raw-syntax predicate is consumed by runtime lifecycle checks.
        assert!(bash_command_has_post_mutation_validation(
            "grep '>' /workspace/file"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "cat /workspace/source > /workspace/output && cat /workspace/output"
        ));
        assert_eq!(
            split_static_shell_words("cp src '/tmp/volatile file'"),
            Some(vec!["cp".into(), "src".into(), "/tmp/volatile file".into()])
        );
        assert_eq!(
            split_static_shell_words("printf x > '/workspace/out file'"),
            Some(vec![
                "printf".into(),
                "x".into(),
                ">".into(),
                "/workspace/out file".into()
            ])
        );
        assert_eq!(
            split_static_shell_words("sed -i 's/$/x/' '/workspace/out file'"),
            Some(vec![
                "sed".into(),
                "-i".into(),
                "s/$/x/".into(),
                "/workspace/out file".into()
            ])
        );
        assert!(split_static_shell_words("cp $DEST source").is_none());
        assert!(split_static_shell_words("cp src 'unterminated").is_none());
        assert!(split_static_shell_words("cp src | tee out").is_none());
    }

    #[test]
    fn configuration_writes_keep_attempt_identity_without_validation_proof() {
        for command in [
            "GOFLAGS=-n; GOWORK=off go test ./...",
            "GOFLAGS=-n && GOWORK=off go test ./... || true",
            "export GOFLAGS=-n && GOWORK=off go test \"${PKGS:-./...}\"",
            "GOFLAGS=\"$FLAGS\" go test \"$PKGS\"",
            "GOFLAGS=-n go test -run '<<' ./...",
            "GOWORK=off go test -n ./... && cargo test",
        ] {
            let args = json!({"command":command}).to_string();
            assert_eq!(normalize_validation_prefix("bash", &args), None);
            assert_eq!(
                normalize_validation_attempt_prefix("bash", &args).as_deref(),
                Some(command)
            );
        }
        for preparation in [
            "printf -v GOFLAGS %s -n",
            "printf '-v' GOFLAGS %s -n",
            "printf '%n' GOFLAGS",
            "printf '%5n' GOFLAGS",
            "GOFLAGS=$(cat /tmp/flags)",
            "GOFLAGS=-n",
            "echo ${GOFLAGS:=-n}",
        ] {
            for scratch in ["", "mkdir -p /tmp/astra-cache && "] {
                let command = format!("{scratch}{preparation} && GOWORK=off go test ./...");
                let args = json!({"command":command}).to_string();
                assert_eq!(
                    normalize_validation_prefix("bash", &args),
                    None,
                    "{command}"
                );
                assert_eq!(
                    normalize_validation_attempt_prefix("bash", &args).as_deref(),
                    Some(command.as_str()),
                    "{command}"
                );
                assert!(
                    !bash_command_has_post_mutation_validation(&command),
                    "{command}"
                );
            }
        }
        for output in ["echo ready", "printf '%s\\n' ready", "printf -- '%s' ready"] {
            for scratch in ["", "mkdir -p /tmp/astra-cache && "] {
                let command = format!("{scratch}{output} && GOWORK=off go test ./...");
                assert!(
                    normalize_validation_prefix("bash", &json!({"command":command}).to_string())
                        .is_some(),
                    "{command}"
                );
            }
        }
        assert!(bash_command_has_go_validation(
            "result=$(GOWORK=off go test ./...)"
        ));
        assert!(bash_command_has_go_validation("GOFLAGS=-n go test ./..."));
        assert!(!bash_command_has_go_validation("echo 'go test ./...'"));
        assert!(!bash_command_has_post_mutation_validation(
            "value=$(cat result) unknown_command"
        ));
        for command in [
            "GOWORK=off go test -run password=synthetic-secret-value-123456 ./...",
            "GOWORK=off go test -run '<<password=synthetic-secret-value-123456' ./...",
            "export GOFLAGS=-n && GOWORK=off go test ./... && echo password=synthetic-secret-value-123456",
        ] {
            let args = json!({"command":command}).to_string();
            assert!(bash_command_has_go_validation(command));
            assert_eq!(normalize_validation_prefix("bash", &args), None);
            let identity = normalize_validation_attempt_prefix("bash", &args).unwrap();
            assert_eq!(identity, operation_args_identity("bash", &args));
            assert!(!identity.contains("synthetic-secret-value"));
        }
        let command = "cat /tmp/missing && GOWORK=off go vet ./...";
        let args = json!({"command":command}).to_string();
        assert_eq!(
            normalize_validation_prefix("bash", &args).as_deref(),
            Some(command)
        );
        assert_eq!(
            normalize_validation_attempt_prefix("bash", &args).as_deref(),
            Some(command)
        );
        let oversized = format!(
            "GOFLAGS=-n go test -run '{}' \"$PKGS\"",
            "x".repeat(MAX_NESTED_SHELL_SCAN_BYTES)
        );
        let args = json!({"command":oversized}).to_string();
        assert!(bash_command_has_go_validation(&oversized));
        assert!(normalize_validation_prefix("bash", &args).is_none());
        assert_eq!(
            normalize_validation_attempt_prefix("bash", &args),
            Some(operation_args_identity("bash", &args))
        );
    }

    #[test]
    fn go_validation_preserves_static_configuration_and_rejects_opaque_scope() {
        for command in [
            "go test ./...",
            "go build -mod=readonly ./...",
            "go vet ./...",
            "GOWORK=off go test ./...",
            "GOWORK='off' go vet ./...",
            "set -e; GOWORK=off go test ./...",
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert_eq!(
                normalize_validation_prefix("bash", &args).as_deref(),
                Some(command)
            );
            assert!(
                bash_command_has_post_mutation_validation(command),
                "{command}"
            );
            assert!(!crate::cloud_approval_policy::bash_command_is_read_only(
                command
            ));
        }
        for command in [
            "GOWORK=/tmp/other/go.work go test ./...",
            "GOFLAGS='-C /tmp/other' go test ./...",
            "PATH=/tmp go test ./...",
            "SECRET=value go test ./...",
            "NO_COLOR=secret GOWORK=off go test ./...",
            "GOWORK=$(printf off) go test ./...",
            "env GOWORK=off go test ./...",
            "GOWORK=off go test --help",
            "GOWORK=off go test -n ./...",
            "go vet -C /tmp/other ./...",
            "go test -C=/tmp/other ./...",
            "go test -overlay=/tmp/other.json ./...",
            "go test -exec=echo ./...",
            "go test ./... || true",
            "go test ./...; echo ok",
            "go test ./... && touch source.go",
            "set -e +o pipefail; GOWORK=off go test ./... | tail -10",
            "source options.sh; go test ./... | tail -10",
            "shopt -ou pipefail && GOWORK=off go test ./... | tail -10",
            "shopt -u -o pipefail && GOWORK=off go test ./... | tail -10",
            "GOWORK=off go vet ./... | go test ./...",
            "GOWORK=off go vet fmt",
            "GOWORK=off go test encoding/json",
            "GOWORK=off go test -c ./...",
            "GOWORK=off go test -list Test ./...",
            "GOWORK=off go build -o ./source.go ./cmd/tool",
            "GOWORK=off go test -coverprofile=./source.go ./...",
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert!(
                normalize_validation_prefix("bash", &args).is_none(),
                "{command}"
            );
            assert!(
                !bash_command_has_post_mutation_validation(command),
                "{command}"
            );
        }
    }

    #[test]
    fn external_scratch_scope_excludes_workspace_overlap_in_both_directions() {
        for (root, external) in [
            (None, true),
            (Some("/workspace"), true),
            (Some("/tmp/sibling"), true),
            (Some("/tmp/astra-cache"), false),
            (Some("/tmp/astra-cache/repo"), false),
            (Some("/tmp"), false),
            (Some("/"), false),
        ] {
            assert_eq!(
                external_scratch_setup("rm -rf /tmp/astra-cache", root),
                external,
                "{root:?}"
            );
        }
    }

    #[test]
    fn ordered_scratch_setup_retains_the_whole_go_validation_operation() {
        for command in [
            "mkdir -p /tmp/astra-cache && go test ./...",
            "cd /workspace && mkdir -p /tmp/astra-cache && GOWORK=off go build ./... && GOWORK=off go vet ./... && echo OK",
            "rm -f /var/tmp/astra-log && touch /tmp/astra-log && GOWORK=off go test ./...",
            "mkdir -p /tmp/astra-cache && GOWORK=off go test ./... 2>&1 | tail -10",
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert_eq!(
                normalize_validation_attempt_prefix("bash", &args).as_deref(),
                Some(command)
            );
            assert_eq!(
                normalize_validation_prefix("bash", &args).as_deref(),
                Some(command)
            );
            assert!(!crate::cloud_approval_policy::bash_command_is_read_only(
                command
            ));
        }
        for command in [
            "mkdir -p /tmp/astra-cache; go test ./...",
            "mkdir -p /tmp/astra-cache || go test ./...",
            "mkdir -p /tmp/astra-cache | cat && go test ./...",
            "mkdir -p relative-cache && go test ./...",
            "mkdir -p /workspace/cache && go test ./...",
            "mkdir -p /tmp/astra-cache && export GOFLAGS=-n && go test ./...",
            "go env -w GOFLAGS=-n && mkdir -p /tmp/astra-cache && go test ./...",
            "go generate ./... && mkdir -p /tmp/astra-cache && go test ./...",
            "GOFLAGS=$(cat /tmp/flags) && mkdir -p /tmp/astra-cache && go test ./...",
            "GOFLAGS=-n && mkdir -p /tmp/astra-cache && go test ./...",
            "printf -v PATH /tmp/fakebin && mkdir -p /tmp/astra-cache && go test ./...",
            "printf -vPATH /tmp/fakebin && mkdir -p /tmp/astra-cache && go test ./...",
            "printf '%n' PATH && mkdir -p /tmp/astra-cache && go test ./...",
            "mkdir -p /tmp/astra-cache && export PATH=/tmp/fakebin && go test ./... && cargo test",
            "mkdir -p /tmp/astra-cache && go test ./... && touch /tmp/late",
            "mkdir -p /tmp/astra-cache && go test ./... && echo password=synthetic-secret-value-123456",
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert!(
                normalize_validation_prefix("bash", &args).is_none(),
                "{command}"
            );
        }
        // The preparation rule is shared, but other validation families keep
        // their existing operation identity and behavior.
        let args =
            serde_json::json!({"command":"mkdir -p /tmp/astra-cache && cargo test"}).to_string();
        assert_eq!(
            normalize_validation_prefix("bash", &args).as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn validation_chain_identity_does_not_attribute_failure_to_the_last_stage() {
        let command =
            "cd /workspace && GOWORK=off go build ./... && GOWORK=off go vet ./... && echo OK";
        let args = serde_json::json!({"command": command}).to_string();
        assert_eq!(
            normalize_validation_attempt_prefix("bash", &args).as_deref(),
            Some(command)
        );
        assert_eq!(
            normalize_validation_prefix("bash", &args).as_deref(),
            Some(command)
        );
        assert!(bash_command_has_post_mutation_validation(command));
    }

    #[test]
    fn go_operation_identity_preserves_workdir_cwd_and_quoted_arguments() {
        for command in ["go test ./...", "cargo test", "python3 -m pytest"] {
            let implicit = serde_json::json!({"command": command}).to_string();
            let explicit = serde_json::json!({"command": command, "workdir": "."}).to_string();
            assert_eq!(
                normalize_validation_attempt_prefix("bash", &implicit),
                normalize_validation_prefix("bash", &explicit)
            );
        }
        let sensitive = serde_json::json!({"command": "GOWORK=off go test ./...", "workdir": "password=synthetic-secret-value-123456"}).to_string();
        assert_eq!(
            normalize_validation_attempt_prefix("bash", &sensitive),
            Some(operation_args_identity("bash", &sensitive))
        );
        for (first, second) in [
            (
                serde_json::json!({"command": "GOWORK=off go test ./...", "workdir": "pkg/a"}),
                serde_json::json!({"command": "GOWORK=off go test ./...", "workdir": "pkg/b"}),
            ),
            (
                serde_json::json!({"command": "GOWORK=off go test -run 'Test  Name' ./..."}),
                serde_json::json!({"command": "GOWORK=off go test -run 'Test Name' ./..."}),
            ),
            (
                serde_json::json!({"command": "GOWORK=off go test ./... && cd pkg/b && GOWORK=off go vet ./..."}),
                serde_json::json!({"command": "GOWORK=off go test ./... && GOWORK=off go vet ./..."}),
            ),
        ] {
            let first = normalize_validation_attempt_prefix("bash", &first.to_string()).unwrap();
            let second = normalize_validation_prefix("bash", &second.to_string()).unwrap();
            assert_ne!(first, second);
        }
        let args =
            serde_json::json!({"command": "prepare_step && GOWORK=off go vet ./..."}).to_string();
        assert_eq!(
            normalize_validation_attempt_prefix("bash", &args).as_deref(),
            Some("prepare_step && GOWORK=off go vet ./...")
        );
        assert!(normalize_validation_prefix("bash", &args).is_none());
    }

    #[test]
    fn typescript_validation_separates_attempt_from_positive_proof() {
        for (command, operation) in [
            ("tsc --noEmit", "tsc --noEmit"),
            ("tsc --noemit false", "tsc --noEmit false"),
            (
                "npx tsc --NOEMIT src/CaseSensitive.ts",
                "npx tsc --noEmit src/CaseSensitive.ts",
            ),
            ("tsc --noEmit --project .", "tsc --noEmit --project ."),
            ("tsc --noEmit --strict", "tsc --noEmit --strict"),
            ("tsc --noEmit --pretty false", "tsc --noEmit --pretty false"),
            (
                "tsc --noEmit --noCheck false",
                "tsc --noEmit --noCheck false",
            ),
            ("tsc --noEmit --watch false", "tsc --noEmit --watch false"),
            ("tsc --noEmit --HELP false", "tsc --noEmit --HELP false"),
            (
                "tsc --noEmit --types @scope/custom index.ts",
                "tsc --noEmit --types @scope/custom index.ts",
            ),
            (
                "npx tsc --NOEMIT --typeRoots @scope/types index.ts",
                "npx tsc --noEmit --typeRoots @scope/types index.ts",
            ),
            (
                "tsc --noEmit --types --help false",
                "tsc --noEmit --types --help false",
            ),
            (
                "tsc --noEmit -p @config.json",
                "tsc --noEmit -p @config.json",
            ),
            (
                "tsc --noEmit --outDir --help false",
                "tsc --noEmit --outDir --help false",
            ),
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert_eq!(
                normalize_validation_attempt_prefix("bash", &args),
                Some(operation.into())
            );
            assert_eq!(
                normalize_validation_prefix("bash", &args),
                Some(operation.into())
            );
        }
        for (command, operation) in [
            ("tsc --noemit --HELP null", "tsc --noEmit --HELP null"),
            (
                "tsc --noemit --help '-?' false",
                "tsc --noEmit --help '-?' false",
            ),
            (
                "tsc --noEmit --help --outDir --help false",
                "tsc --noEmit --help --outDir --help false",
            ),
            ("tsc --noEmit --noCheck", "tsc --noEmit --noCheck"),
            ("tsc --noEmit --types --help", "tsc --noEmit --types --help"),
            ("tsc --noEmit @args.rsp", "tsc --noEmit @args.rsp"),
            (
                "tsc --noEmit --strict @args.rsp",
                "tsc --noEmit --strict @args.rsp",
            ),
            (
                "tsc --noEmit --types @scope/custom @args.rsp",
                "tsc --noEmit --types @scope/custom @args.rsp",
            ),
            (
                "tsc --noEmit --types --strict @args.rsp",
                "tsc --noEmit --types --strict @args.rsp",
            ),
            (
                "tsc --noEmit --types --outDir --types @args.rsp",
                "tsc --noEmit --types --outDir --types @args.rsp",
            ),
            ("tsc --noEmit --types", "tsc --noEmit --types"),
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert_eq!(
                normalize_validation_attempt_prefix("bash", &args),
                Some(operation.into())
            );
            assert!(
                normalize_validation_prefix("bash", &args).is_none(),
                "{command}"
            );
            assert!(
                !bash_command_has_post_mutation_validation(command),
                "{command}"
            );
        }
        for command in [
            "TSC --noemit",
            "npx TSC --noemit",
            "tsc --noEmitExtra",
            "tsc --noemit $FLAGS",
        ] {
            let args = serde_json::json!({"command": command}).to_string();
            assert!(
                normalize_validation_attempt_prefix("bash", &args).is_none(),
                "{command}"
            );
            assert!(
                normalize_validation_prefix("bash", &args).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn nested_read_receipts_inside_assignments_and_output_are_evidence() {
        assert!(bash_command_has_post_mutation_validation(
            r#"rm -rf work; deploy-command; value=$(curl -fsS https://localhost/); echo "state=$value""#
        ));
        assert!(bash_command_has_post_mutation_validation(
            r#"git push origin main; echo "state=$(curl -fsS https://localhost/ | tr -d '\n')""#
        ));
        assert!(bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(test -f result.json && echo PASS || echo FAIL)""#
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\nvalue=$(curl -fsS https://service/ | tr -d '\\n')\necho \"status=$([ \\\"$value\\\" = expected ] && echo PASS || echo FAIL)\"\n"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\ncat result.json # ; $(unknown_probe) `unknown_probe` &\n"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\necho \"state=#not-a-comment\"\ncat result.json # preserve receipt\n"
        ));
        assert!(bash_command_has_post_mutation_validation(
            "mutate-command\nvalue=$(curl -fsS https://service/#fragment)\n"
        ));
    }

    #[test]
    fn nested_unknown_or_post_receipt_writes_do_not_fake_validation() {
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(unknown_probe)""#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(cat result.json)" > report.txt"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; value=$(cat result.json; unknown_probe)"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(curl -X POST https://localhost/deploy)""#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(cat result.json)" | tee changed.txt"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo '$(cat result.json)'"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; echo "$(cat result.json""#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"write-command; value=$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(echo "$(cat result.json)"))))))))))))))))")"#
        ));
        let oversized = format!(
            "cat result.json; {}unknown-write",
            "x".repeat(MAX_SHELL_LEX_BYTES)
        );
        assert!(!bash_command_has_post_mutation_validation(&oversized));
        assert!(!bash_command_has_post_mutation_validation(
            r#"cat result.json; echo "unterminated"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"cat result.json; echo "$(cat result.json"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            r#"cat result.json; echo `unknown_probe`"#
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cat result.json; echo \"$(cat result.json)\" & unknown-write"
        ));
        assert!(!bash_command_has_post_mutation_validation(
            "cat result.json <<EOF\nunknown-write\nEOF"
        ));
    }
}
