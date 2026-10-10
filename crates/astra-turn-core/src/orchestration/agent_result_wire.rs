use super::types::{AgentStatus, agent_completion_is_interrupted, agent_finish_reason_text};
use astra_tools::agent_tool_contract::{
    AGENT_WAIT_MAX_MS, AgentControlOutcome, AgentToolResultFamily, AgentWaitReceipt,
    agent_action_from_args,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::str::FromStr;
use std::time::Duration;

/// Wire-level status of an agent tool result.
///
/// Serde round-trips to the lowercase wire strings (e.g. `"completed"`,
/// `"timeout"`, `"still_running"`) that LLMs see in JSON output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentToolResultStatusKind {
    Completed,
    Failed,
    #[serde(rename = "timeout")]
    TimedOut,
    Cancelled,
    Interrupted,
    Waiting,
    Paused,
    StillRunning,
    Launched,
    /// Catch-all for unknown wire statuses.
    #[serde(other)]
    Other,
}

impl AgentToolResultStatusKind {
    /// Return the serde wire name (the lowercase string used in JSON output).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::TimedOut => "timeout",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::StillRunning => "still_running",
            Self::Launched => "launched",
            Self::Other => "other",
        }
    }

    /// Wire-tolerant parser: any unrecognized status maps to `Other`.
    ///
    /// Use this from JSON-deserialization paths where dropping an unknown
    /// status would silently lose information from a peer running a different
    /// version. For typed call sites where you want to learn about a typo,
    /// use `FromStr::from_str` instead — it returns `Err` for unknowns.
    pub fn parse_wire(s: &str) -> Self {
        Self::from_str(s).unwrap_or(Self::Other)
    }
}

impl FromStr for AgentToolResultStatusKind {
    type Err = String;

    /// Parse a wire status string. Trims and lower-cases the input for
    /// resilience to upstream casing/whitespace variants. Truly unknown
    /// statuses produce an `Err` rather than silently mapping to `Other`,
    /// so callers must explicitly opt into the catch-all.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "timeout" | "timed_out" => Ok(Self::TimedOut),
            "cancelled" => Ok(Self::Cancelled),
            "interrupted" => Ok(Self::Interrupted),
            "waiting" => Ok(Self::Waiting),
            "paused" => Ok(Self::Paused),
            "still_running" => Ok(Self::StillRunning),
            "launched" => Ok(Self::Launched),
            "other" => Ok(Self::Other),
            _ => Err(format!("unknown agent tool status: '{s}'")),
        }
    }
}

pub const AGENT_RESULT_CLASS_SUCCESS: &str = "success";
pub const AGENT_RESULT_CLASS_AGENT_INCOMPLETE: &str = "agent_incomplete";
pub const AGENT_RESULT_CLASS_FANOUT_INCOMPLETE: &str = "fanout_incomplete";

pub fn agent_tool_status_needs_recovery(status: AgentToolResultStatusKind) -> bool {
    matches!(
        status,
        AgentToolResultStatusKind::Failed
            | AgentToolResultStatusKind::TimedOut
            | AgentToolResultStatusKind::Cancelled
            | AgentToolResultStatusKind::Interrupted
            | AgentToolResultStatusKind::Waiting
            | AgentToolResultStatusKind::Paused
            | AgentToolResultStatusKind::StillRunning
            | AgentToolResultStatusKind::Launched
            | AgentToolResultStatusKind::Other
    )
}

pub fn agent_tool_result_looks_like(value: &Value) -> bool {
    value.get("result_family").is_some()
        || (value.get("agent_id").is_some() && value.get("status").is_some())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentControlReceipt {
    Spawn,
    Wait(AgentWaitReceipt),
    SendMessage,
    List,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedAgentToolResult {
    ControlReceipt(AgentControlReceipt),
    ControlFailure(AgentFanoutControlExecutionFact),
    ChildResult(AgentToolResultStatusKind),
}

/// Decode producer-owned facts only. Missing/unknown families and malformed
/// receipts fail closed. The action/outcome pair must agree, and a control
/// receipt cannot carry a child-result payload.
pub fn decode_agent_tool_result(value: &Value) -> Option<DecodedAgentToolResult> {
    if value
        .get("result_family")
        .is_none_or(|family| family.as_str() == Some("control_receipt"))
        && value
            .get("success")
            .is_none_or(|success| success == &Value::Bool(false))
        && matches!(
            value.get("status").and_then(Value::as_str),
            Some("failed" | "rejected" | "blocked" | "unknown")
        )
        && let Some(
            fact @ (AgentFanoutControlExecutionFact::NotExecuted
            | AgentFanoutControlExecutionFact::Unknown),
        ) = execution_fact_from_receipt(value)
    {
        return Some(DecodedAgentToolResult::ControlFailure(fact));
    }
    let family: AgentToolResultFamily =
        serde_json::from_value(value.get("result_family")?.clone()).ok()?;
    let nonempty = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    match family {
        AgentToolResultFamily::ChildResult => {
            let status = value.get("status")?.as_str()?;
            nonempty("agent_id").then(|| {
                DecodedAgentToolResult::ChildResult(AgentToolResultStatusKind::parse_wire(status))
            })
        }
        AgentToolResultFamily::ControlReceipt => {
            if value.get("success").and_then(Value::as_bool) != Some(true)
                || value.get("error").is_some()
                || value.get("result").is_some()
                || value.get("finish_reason").is_some()
                || value.get("incomplete").is_some()
            {
                return None;
            }
            let outcome: AgentControlOutcome =
                serde_json::from_value(value.get("status")?.clone()).ok()?;
            if agent_action_from_args(value).ok()? != outcome.action() {
                return None;
            }
            let receipt = match outcome {
                AgentControlOutcome::SpawnLaunched
                    if nonempty("parent_run_id") && nonempty("agent_id") && nonempty("run_id") =>
                {
                    AgentControlReceipt::Spawn
                }
                AgentControlOutcome::WaitAdmitted => {
                    let receipt: AgentWaitReceipt =
                        serde_json::from_value(value.get("wait_request")?.clone()).ok()?;
                    if receipt.parent_run_id.trim().is_empty()
                        || receipt.tool_call_id.trim().is_empty()
                        || !(1..=AGENT_WAIT_MAX_MS).contains(&receipt.timeout_ms)
                    {
                        return None;
                    }
                    AgentControlReceipt::Wait(receipt)
                }
                AgentControlOutcome::MessageQueued
                    if nonempty("run_id")
                        && nonempty("message_id")
                        && nonempty("target")
                        && nonempty("message_type") =>
                {
                    // success=true is the sender's enqueue acknowledgement,
                    // not proof that the recipient applied or completed work.
                    match value.get("recipients")? {
                        Value::Null => {}
                        Value::Array(ids)
                            if !ids.is_empty()
                                && ids.iter().all(|id| {
                                    id.as_str().is_some_and(|id| !id.trim().is_empty())
                                }) => {}
                        _ => return None,
                    }
                    AgentControlReceipt::SendMessage
                }
                AgentControlOutcome::ListObserved if nonempty("parent_run_id") => {
                    let agents = value.get("agents")?.as_array()?;
                    if !agents.iter().all(|agent| {
                        ["agent_id", "run_id"].into_iter().all(|key| {
                            agent
                                .get(key)
                                .and_then(Value::as_str)
                                .is_some_and(|id| !id.trim().is_empty())
                        })
                    }) {
                        return None;
                    }
                    AgentControlReceipt::List
                }
                _ => return None,
            };
            Some(DecodedAgentToolResult::ControlReceipt(receipt))
        }
    }
}

pub fn agent_tool_structured_result_class(value: &Value) -> Option<&'static str> {
    let status = match decode_agent_tool_result(value) {
        Some(DecodedAgentToolResult::ControlReceipt(_)) => return None,
        Some(DecodedAgentToolResult::ChildResult(status))
            if !value
                .get("incomplete")
                .is_some_and(|v| v != &Value::Bool(false))
                && !value
                    .get("success")
                    .is_some_and(|v| v != &Value::Bool(true))
                && value.get("error").is_none() =>
        {
            status
        }
        _ => return Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE),
    };
    match status {
        AgentToolResultStatusKind::Completed
            if value
                .get("result")
                .and_then(Value::as_str)
                .is_some_and(|result| !result.trim().is_empty()) =>
        {
            Some(AGENT_RESULT_CLASS_SUCCESS)
        }
        AgentToolResultStatusKind::Completed => Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE),
        status if agent_tool_status_needs_recovery(status) => {
            Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
        }
        _ => None,
    }
}

pub fn agent_tool_result_needs_recovery(value: &Value) -> bool {
    agent_tool_structured_result_class(value) == Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
}

pub fn agent_fanout_result_looks_like(value: &Value) -> bool {
    value.get("group_id").is_some() && value.get("results").is_some()
}

/// Authority carried by one structured `agent_fanout` control result.
///
/// A group identity proves that the fanout lifecycle exists, even when its
/// current status is failed or incomplete. A failure without a group identity
/// is authoritative only when it carries an explicit error proving that the
/// request was rejected before acceptance. Everything else is an observation
/// that still requires registry reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFanoutControlReceiptKind {
    Group,
    RejectedBeforeAcceptance,
    /// The control envelope reached a terminal boundary, but the producer
    /// could not prove whether execution began. This is authoritative for
    /// settlement; the host must not replay the action.
    ExecutionUnknown,
}

/// Typed execution fact carried by a control result.
///
/// `None` means that the producer did not publish an execution fact. That
/// absence is intentionally different from `Unknown`: an explicit unknown
/// terminal closes a physical call without granting permission to replay it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFanoutControlExecutionFact {
    NotExecuted,
    Executed,
    Unknown,
}

/// Parse the canonical structured record from an agent-control output.
///
/// The returned value comes only from a complete JSON document or its first
/// complete line. Consumers should derive lifecycle state from this value and
/// use any trailing text only as non-authoritative display guidance.
pub fn agent_control_result_value(output: &str) -> Option<Value> {
    let output = output.trim();
    if output.is_empty() {
        return None;
    }
    serde_json::from_str(output).ok().or_else(|| {
        // Some result surfaces append a human recovery hint after the
        // canonical single-line JSON record. Only that first complete record
        // is protocol authority; trailing prose must never create authority.
        let first_record = output.lines().next()?.trim();
        (!first_record.is_empty())
            .then(|| serde_json::from_str(first_record).ok())
            .flatten()
    })
}

fn execution_fact_from_receipt(receipt: &Value) -> Option<AgentFanoutControlExecutionFact> {
    let value = receipt
        .get("executed")
        .or_else(|| receipt.pointer("/advisory/executed"))?;
    match value {
        Value::Bool(false) => Some(AgentFanoutControlExecutionFact::NotExecuted),
        Value::Bool(true) => Some(AgentFanoutControlExecutionFact::Executed),
        Value::Null => Some(AgentFanoutControlExecutionFact::Unknown),
        _ => None,
    }
}

/// Read the producer-owned execution fact from a structured fanout result.
/// Only the exact boolean/null fields are recognized; display text and error
/// prose never participate in lifecycle authority.
pub fn agent_fanout_control_execution_fact(
    output: &str,
) -> Option<AgentFanoutControlExecutionFact> {
    execution_fact_from_receipt(&agent_control_result_value(output)?)
}

pub fn agent_fanout_control_receipt_kind(output: &str) -> Option<AgentFanoutControlReceiptKind> {
    let receipt = agent_control_result_value(output)?;
    let status = receipt
        .get("status")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())?;
    let execution_fact = execution_fact_from_receipt(&receipt);
    if execution_fact == Some(AgentFanoutControlExecutionFact::Unknown) {
        return Some(AgentFanoutControlReceiptKind::ExecutionUnknown);
    }
    let normalized_status = status.to_ascii_lowercase();
    if execution_fact == Some(AgentFanoutControlExecutionFact::NotExecuted)
        && matches!(
            normalized_status.as_str(),
            "failed" | "rejected" | "blocked"
        )
    {
        // A rejection can reference an existing group. Its identity is
        // context, not proof that this request created or executed it.
        return Some(AgentFanoutControlReceiptKind::RejectedBeforeAcceptance);
    }
    if receipt
        .get("group_id")
        .and_then(Value::as_str)
        .is_some_and(|group_id| !group_id.trim().is_empty())
    {
        return Some(AgentFanoutControlReceiptKind::Group);
    }
    if execution_fact == Some(AgentFanoutControlExecutionFact::Executed) {
        // An executed control action still needs its group receipt. Keeping
        // it provisional prevents duplicate child creation when a result was
        // flattened before the registry receipt arrived.
        return None;
    }
    None
}

/// Whether an agent-fanout control result is complete enough to cross an
/// execution boundary without host reconciliation.
///
/// A `start` call has side effects before its response is delivered.  Plain
/// text, an empty body, or ambiguous JSON without the canonical group identity
/// cannot prove whether children were accepted, so the host must reconcile it
/// from the fanout registry. A typed rejection with `executed=false` is
/// authoritative evidence that this call did not execute, even if it references
/// an existing group. An
/// explicit `executed=null` closes as an unknown terminal and must never be
/// replayed. A failure without an execution fact still requires reconciliation.
/// Arbitrary non-empty transport text is never a lifecycle result.
pub fn agent_fanout_control_result_is_usable(output: &str) -> bool {
    agent_fanout_control_receipt_kind(output).is_some()
}

pub fn agent_fanout_structured_result_class(value: &Value) -> Option<&'static str> {
    if agent_fanout_result_has_recoverable_issue(value) {
        return Some(AGENT_RESULT_CLASS_FANOUT_INCOMPLETE);
    }

    match value.get("status").and_then(Value::as_str)? {
        "completed" => Some(AGENT_RESULT_CLASS_SUCCESS),
        "incomplete"
        | "completed_with_issues"
        | "failed"
        | "failed_to_start"
        | "interrupted"
        | "timeout"
        | "timed_out"
        | "cancelled" => Some(AGENT_RESULT_CLASS_FANOUT_INCOMPLETE),
        _ => None,
    }
}

/// Group termination and delivery of its complete result set are distinct facts.
pub fn agent_fanout_results_delivered(value: &Value) -> bool {
    let Some(results) = value.get("results").and_then(Value::as_array) else {
        return false;
    };
    !results.is_empty()
        && value.get("target_count").and_then(Value::as_u64) == Some(results.len() as u64)
        && value.pointer("/provenance/all_slots_delivered") == Some(&Value::Bool(true))
        && agent_fanout_structured_result_class(value) == Some(AGENT_RESULT_CLASS_SUCCESS)
}

pub fn agent_fanout_result_has_recoverable_issue(value: &Value) -> bool {
    const ISSUE_COUNT_FIELDS: &[&str] = &[
        "failed",
        "interrupted",
        "timed_out",
        "cancelled_by_user",
        "cancelled_by_runtime",
        "spawn_rejected",
        "incomplete_results",
    ];
    if ISSUE_COUNT_FIELDS
        .iter()
        .any(|field| value.get(*field).and_then(Value::as_u64).unwrap_or(0) > 0)
    {
        return true;
    }

    value
        .get("results")
        .and_then(Value::as_array)
        .is_some_and(|results| {
            results.iter().any(|item| {
                item.get("status")
                    .and_then(Value::as_str)
                    .is_some_and(fanout_slot_status_is_recoverable_issue)
                    || item
                        .get("result")
                        .is_some_and(agent_tool_result_needs_recovery)
            })
        })
}

pub fn fanout_slot_status_is_recoverable_issue(status: &str) -> bool {
    matches!(
        status,
        "failed"
            | "failed_to_start"
            | "interrupted"
            | "timeout"
            | "timed_out"
            | "cancelled"
            | "cancelled_by_user"
            | "cancelled_by_runtime"
            | "spawn_rejected"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentToolWireOutcomeKind {
    Completed,
    Failed,
    TimedOut,
    Cancelled,
    Interrupted,
    Running,
    NoChange,
}

#[derive(Debug, Clone, Copy)]
pub struct AgentToolWireProjection<'a> {
    pub outcome: AgentToolWireOutcomeKind,
    /// Observation/control failures do not establish child termination.
    pub child_terminal: bool,
    pub finish_reason: Option<&'a str>,
    pub agent_id: Option<&'a str>,
    pub display_name_hint: Option<&'a str>,
    pub cancelled_reason: Option<&'a str>,
    pub has_result: bool,
}

pub const AGENT_RESULT_INTERRUPTED_ERROR: &str =
    "Agent did not return a final result before it was interrupted.";

pub fn project_agent_tool_wire<'a>(
    _action: &str,
    outer_tool_success: bool,
    parsed: Option<&'a Value>,
) -> AgentToolWireProjection<'a> {
    let decoded = parsed.and_then(decode_agent_tool_result);
    let finish_reason = parsed
        .and_then(|value| value.get("finish_reason"))
        .and_then(Value::as_str);
    let has_result = parsed
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .is_some_and(|result| !result.trim().is_empty());
    let child_terminal = matches!(
        &decoded,
        Some(DecodedAgentToolResult::ChildResult(
            AgentToolResultStatusKind::Completed
                | AgentToolResultStatusKind::Failed
                | AgentToolResultStatusKind::Cancelled
                | AgentToolResultStatusKind::Interrupted
        ))
    );
    let outcome = match decoded {
        Some(DecodedAgentToolResult::ControlReceipt(AgentControlReceipt::Spawn))
            if outer_tool_success =>
        {
            AgentToolWireOutcomeKind::Running
        }
        Some(DecodedAgentToolResult::ControlReceipt(_)) if outer_tool_success => {
            AgentToolWireOutcomeKind::NoChange
        }
        Some(DecodedAgentToolResult::ChildResult(AgentToolResultStatusKind::Completed)) => {
            if parsed.and_then(agent_tool_structured_result_class)
                == Some(AGENT_RESULT_CLASS_SUCCESS)
            {
                AgentToolWireOutcomeKind::Completed
            } else {
                AgentToolWireOutcomeKind::Failed
            }
        }
        Some(DecodedAgentToolResult::ChildResult(AgentToolResultStatusKind::Failed)) => {
            AgentToolWireOutcomeKind::Failed
        }
        Some(DecodedAgentToolResult::ChildResult(AgentToolResultStatusKind::TimedOut)) => {
            AgentToolWireOutcomeKind::TimedOut
        }
        Some(DecodedAgentToolResult::ChildResult(AgentToolResultStatusKind::Cancelled)) => {
            AgentToolWireOutcomeKind::Cancelled
        }
        Some(DecodedAgentToolResult::ChildResult(AgentToolResultStatusKind::Interrupted)) => {
            AgentToolWireOutcomeKind::Interrupted
        }
        Some(DecodedAgentToolResult::ChildResult(
            AgentToolResultStatusKind::Waiting
            | AgentToolResultStatusKind::Paused
            | AgentToolResultStatusKind::StillRunning
            | AgentToolResultStatusKind::Launched,
        )) => AgentToolWireOutcomeKind::Running,
        _ if parsed.is_some() || !outer_tool_success => AgentToolWireOutcomeKind::Failed,
        _ => AgentToolWireOutcomeKind::NoChange,
    };

    AgentToolWireProjection {
        outcome,
        child_terminal,
        finish_reason,
        agent_id: parsed
            .and_then(|value| value.get("agent_id"))
            .and_then(Value::as_str),
        display_name_hint: parsed.and_then(|value| {
            value
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| value.get("description").and_then(Value::as_str))
        }),
        cancelled_reason: parsed
            .and_then(|value| value.get("reason"))
            .and_then(Value::as_str),
        has_result,
    }
}

pub fn agent_tool_interrupted_message(is_result_wait: bool, finish_reason: Option<&str>) -> String {
    if let Some(kind) = finish_reason.and_then(crate::interruption::InterruptionKind::from_label) {
        return format!("{}: {}", kind.user_status(), kind.user_description());
    }
    if is_result_wait {
        AGENT_RESULT_INTERRUPTED_ERROR.to_string()
    } else {
        "Agent stopped before completing its result.".to_string()
    }
}

pub fn agent_tool_completed_result_text(parsed: &Value) -> Option<String> {
    match decode_agent_tool_result(parsed) {
        Some(DecodedAgentToolResult::ChildResult(
            AgentToolResultStatusKind::Completed | AgentToolResultStatusKind::Interrupted,
        )) => parsed
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

pub fn agent_tool_result_output_summary(
    parsed: Option<&Value>,
    raw_output: Option<&str>,
) -> Option<String> {
    parsed
        .and_then(|value| value.get("result"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or(raw_output)
        .map(agent_tool_result_preview)
}

pub fn agent_tool_error_message(parsed: Option<&Value>, fallback: &str) -> String {
    parsed
        .and_then(|value| value.get("error"))
        .and_then(Value::as_str)
        .unwrap_or(fallback)
        .to_string()
}

pub fn agent_tool_incomplete_reason(parsed: &Value) -> Option<String> {
    match parsed
        .get("status")
        .and_then(Value::as_str)
        .map(AgentToolResultStatusKind::parse_wire)
    {
        Some(AgentToolResultStatusKind::StillRunning) => {
            let detail = parsed
                .get("current_status")
                .and_then(Value::as_str)
                .unwrap_or("still running");
            Some(format!(
                "still running when the wait window expired ({detail})"
            ))
        }
        Some(AgentToolResultStatusKind::Launched) => {
            Some("launched and has not produced a child result yet".to_string())
        }
        Some(AgentToolResultStatusKind::TimedOut) => Some(
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("timed out while waiting for the child result")
                .to_string(),
        ),
        Some(AgentToolResultStatusKind::Failed) => Some(
            parsed
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("child result retrieval failed")
                .to_string(),
        ),
        Some(AgentToolResultStatusKind::Waiting | AgentToolResultStatusKind::Paused) => {
            Some(format!(
                "child agent is waiting ({})",
                parsed
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("waiting")
            ))
        }
        Some(AgentToolResultStatusKind::Cancelled) => Some(
            parsed
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("child agent was cancelled")
                .to_string(),
        ),
        Some(AgentToolResultStatusKind::Other) => Some(
            parsed
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("child agent returned an unknown status")
                .to_string(),
        ),
        _ => None,
    }
}

pub fn agent_tool_running_preview(parsed: &Value) -> Option<String> {
    match parsed
        .get("status")
        .and_then(Value::as_str)
        .map(AgentToolResultStatusKind::parse_wire)
    {
        Some(AgentToolResultStatusKind::StillRunning) => {
            let current_status = parsed
                .get("current_status")
                .and_then(Value::as_str)
                .unwrap_or("running");
            let waited = parsed
                .get("waited_secs")
                .and_then(Value::as_u64)
                .map(|secs| format!(" after {secs}s"))
                .unwrap_or_default();
            let hint = parsed
                .get("hint")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            Some(match hint {
                Some(hint) => format!("Agent is {current_status}{waited}. {hint}"),
                None => format!("Agent is {current_status}{waited}."),
            })
        }
        Some(AgentToolResultStatusKind::Launched) => {
            Some("Agent launched; waiting for get_result output.".to_string())
        }
        _ => None,
    }
}

pub fn agent_tool_status_summary(parsed: &Value) -> Option<String> {
    if let Some(result) = agent_tool_result_output_summary(Some(parsed), None) {
        return Some(one_line_preview(&result, 72));
    }
    if let Some(preview) = agent_tool_running_preview(parsed) {
        return Some(one_line_preview(&preview, 72));
    }
    if let Some(reason) = agent_tool_incomplete_reason(parsed) {
        return Some(one_line_preview(&reason, 72));
    }
    Some(one_line_preview(
        &agent_tool_error_message(Some(parsed), "agent result unavailable"),
        72,
    ))
}

pub fn render_completed_agent_result(
    agent_id: &str,
    result: &str,
    finish_reason: Option<&str>,
) -> String {
    render_child_agent_result(completed_agent_result_body(agent_id, result, finish_reason))
}

fn completed_agent_result_body(agent_id: &str, result: &str, finish_reason: Option<&str>) -> Value {
    let reason = agent_finish_reason_text(finish_reason);
    // This wire payload only reports whether the child result is complete.
    // Resume behavior belongs to the structured InterruptionRecord, where
    // `resume_mode` can distinguish Continue from Settle.
    let interrupted = agent_completion_is_interrupted(Some(reason));
    let mut body = json!({
        "status": if interrupted {
            AgentToolResultStatusKind::Interrupted.as_str()
        } else {
            AgentToolResultStatusKind::Completed.as_str()
        },
        "agent_id": agent_id,
        "result": result,
        "finish_reason": reason,
        "incomplete": interrupted,
    });
    if interrupted {
        body["hint"] = json!(
            "The child agent stopped before fully finishing. Treat this as incomplete and either continue it or report the interruption explicitly."
        );
    }
    body
}

fn render_child_agent_result(mut body: Value) -> String {
    body["result_family"] = json!(AgentToolResultFamily::ChildResult);
    body.to_string()
}

pub const CHILD_OUTCOME_GUIDANCE: &str = concat!(
    astra_tools::agent_parent_scope_guidance!(),
    " While owned children remain pending and no independent work remains, use agent(action='wait') or propose an answer for runtime completion waiting. After sufficient terminal outcomes arrive, finish the request; wait for future input only when requested. Terminal child outcomes are delivered automatically. Do not re-fetch an already observed, sufficient terminal result. get_result/get_results remain available for inspection, missing or truncated output, pagination, and recovery. Do not busy-poll or use shell sleep. A wait timeout does not cancel children or authorize completion."
);

pub const PENDING_CHILD_RUNTIME_WAIT_GUIDANCE: &str = "This observation is not a terminal result. Continue only work needed for the user's request. For a pending direct child owned by this run, use agent(action='wait') for input, or propose a final answer so the runtime waits and resumes when continuation is available. Otherwise inspect the child's status or waiting reason. Do not busy-poll or use shell sleep solely to wait.";

pub fn render_wait_timeout_outcome(
    agent_id: &str,
    live_status: Option<&AgentStatus>,
    timeout: Duration,
) -> String {
    render_child_agent_result(match live_status {
        Some(status) if !status.is_terminal() => {
            let mut body = wait_for_agent_status_body(agent_id, status);
            body["waited_secs"] = json!(timeout.as_secs());
            body["observation_timed_out"] = json!(true);
            body
        }
        _ => json!({
            "status": AgentToolResultStatusKind::TimedOut.as_str(),
            "agent_id": agent_id,
            "error": format!(
                "Agent '{agent_id}' did not complete within {}s and has no live state",
                timeout.as_secs()
            ),
        }),
    })
}

pub fn render_wait_for_agent_status(agent_id: &str, status: &AgentStatus) -> String {
    render_child_agent_result(wait_for_agent_status_body(agent_id, status))
}

fn wait_for_agent_status_body(agent_id: &str, status: &AgentStatus) -> Value {
    match status {
        AgentStatus::Completed {
            result,
            finish_reason,
        } => completed_agent_result_body(agent_id, result, finish_reason.as_deref()),
        AgentStatus::Interrupted {
            partial_result,
            finish_reason,
        } => {
            let reason = agent_finish_reason_text(Some(finish_reason));
            json!({
                "status": AgentToolResultStatusKind::Interrupted.as_str(),
                "agent_id": agent_id,
                "result": partial_result,
                "finish_reason": reason,
                "incomplete": true,
                "hint": "The child agent stopped before fully finishing. Treat this as incomplete and either continue it or report the interruption explicitly.",
            })
        }
        AgentStatus::Failed {
            error,
            finish_reason,
        } => {
            let reason = finish_reason
                .as_deref()
                .unwrap_or(AgentToolResultStatusKind::Failed.as_str());
            json!({
                "status": AgentToolResultStatusKind::Failed.as_str(),
                "agent_id": agent_id,
                "error": error,
                "finish_reason": reason,
            })
        }
        AgentStatus::Waiting { reason } => json!({
            "status": AgentToolResultStatusKind::Waiting.as_str(),
            "agent_id": agent_id,
            "reason": if reason.trim().is_empty() {
                "waiting".to_string()
            } else {
                reason.clone()
            },
            "hint": "The child agent is waiting for external input or executor recovery. Do not fabricate its result.",
        }),
        AgentStatus::Paused { reason } => json!({
            "status": AgentToolResultStatusKind::Paused.as_str(),
            "agent_id": agent_id,
            "reason": reason,
            "resumable": true,
            "hint": "The child agent has a committed execution pause. Preserve its run and resume it when the blocker is resolved; do not fabricate completion.",
        }),
        AgentStatus::Cancelled { by_user, reason } => {
            let mut payload = json!({
                "status": AgentToolResultStatusKind::Cancelled.as_str(),
                "agent_id": agent_id,
                "reason": if reason.is_empty() {
                    "cancelled".to_string()
                } else {
                    reason.clone()
                },
                "cancelled_by_user": *by_user,
            });
            if *by_user {
                // Make it explicit so the LLM doesn't dutifully respawn
                // the work the user just killed. Without this, the LLM
                // observes "cancelled" and most models treat it as a
                // transient failure → immediately re-spawns, defeating
                // the user's intent.
                payload["instruction"] = json!(
                    "The user explicitly cancelled this sub-agent. \
                     Do NOT respawn it or retry the same work; treat \
                     this turn as the user's signal to change direction. \
                     If the original objective still needs attention, \
                     ask the user what to do next."
                );
            }
            payload
        }
        AgentStatus::Initializing => json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": agent_id,
        }),
        AgentStatus::Running { activity } => json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "agent_id": agent_id,
            "current_status": "running",
            "activity": activity,
            "hint": PENDING_CHILD_RUNTIME_WAIT_GUIDANCE,
        }),
        AgentStatus::Idle => json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "agent_id": agent_id,
            "current_status": "idle",
            "hint": PENDING_CHILD_RUNTIME_WAIT_GUIDANCE,
        }),
    }
}

pub fn render_unknown_agent_result(agent_id: &str, message: &str) -> String {
    render_agent_tool_error(Some(agent_id), message)
}

pub fn render_agent_tool_error(agent_id: Option<&str>, message: &str) -> String {
    render_agent_tool_error_with_kind(agent_id, message, None)
}

/// A rejected request that provably never launched child execution.
/// Do not use for launch failures or partially started fanout groups.
pub fn render_agent_tool_admission_error(message: &str) -> String {
    render_agent_tool_admission_error_with_kind(message, None)
}

pub fn render_agent_tool_admission_error_with_kind(
    message: &str,
    error_kind: Option<astra_core::ErrorKind>,
) -> String {
    let mut body = agent_tool_error_body(None, message, error_kind);
    body["executed"] = json!(false);
    body.to_string()
}

pub fn render_agent_tool_error_with_kind(
    agent_id: Option<&str>,
    message: &str,
    error_kind: Option<astra_core::ErrorKind>,
) -> String {
    agent_tool_error_body(agent_id, message, error_kind).to_string()
}

fn agent_tool_error_body(
    agent_id: Option<&str>,
    message: &str,
    error_kind: Option<astra_core::ErrorKind>,
) -> Value {
    let mut body = json!({
        "result_family": AgentToolResultFamily::ControlReceipt,
        "success": false,
        "status": AgentToolResultStatusKind::Failed.as_str(),
        "error": message,
    });
    if let Some(error_kind) = error_kind {
        body["error_kind"] = json!(error_kind.as_str());
    }
    if let Some(agent_id) = agent_id {
        body["agent_id"] = json!(agent_id);
    }
    body
}

/// A malformed provider tool-call is not a failed sub-agent run. It is an
/// unexecuted boundary failure, so preserve that distinction for the model,
/// transcript, and TUI without echoing the corrupt argument bytes.
pub fn render_agent_tool_malformed_arguments_error(
    tool_name: &str,
    parse_error: Option<&Value>,
) -> String {
    let mut body = json!({
        "status": AgentToolResultStatusKind::Failed.as_str(),
        "error_kind": astra_core::ErrorKind::ToolInvalidArgs.as_str(),
        "error": "Tool arguments were not valid JSON; no agent was started.",
        "advisory": {
            "kind": "malformed_tool_arguments",
            "tool": tool_name,
            "executed": false,
            "next_step": "If the task permits another call, retry with one complete JSON argument object matching this invocation's advertised schema; do not write function-call text or markup in the arguments field.",
        },
    });
    if let Some(parse_error) = parse_error.and_then(sanitized_parse_error_metadata) {
        body["advisory"]["parse_error"] = parse_error;
    }
    body.to_string()
}

fn sanitized_parse_error_metadata(parse_error: &Value) -> Option<Value> {
    let mut metadata = serde_json::Map::new();
    if let Some(kind @ ("invalid_json" | "truncated")) =
        parse_error.get("kind").and_then(Value::as_str)
    {
        metadata.insert("kind".into(), json!(kind));
    }
    if let Some(category @ ("io" | "syntax" | "data" | "eof")) =
        parse_error.get("category").and_then(Value::as_str)
    {
        metadata.insert("category".into(), json!(category));
    }
    for field in ["argument_bytes", "line", "column"] {
        if let Some(value) = parse_error.get(field).and_then(Value::as_u64) {
            metadata.insert(field.into(), json!(value));
        }
    }
    (!metadata.is_empty()).then_some(Value::Object(metadata))
}

fn agent_tool_result_preview(result: &str) -> String {
    const MAX_LINES: usize = 80;
    const MAX_CHARS: usize = 8_000;
    let mut out = String::new();
    for (idx, line) in result.lines().enumerate() {
        if idx >= MAX_LINES || out.len() > MAX_CHARS {
            out.push_str("\n…");
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    if out.is_empty() {
        result.chars().take(MAX_CHARS).collect()
    } else {
        out
    }
}

fn one_line_preview(text: &str, max_chars: usize) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    let mut out: String = first_line.chars().take(max_chars).collect();
    if first_line.chars().count() > max_chars || text.lines().nth(1).is_some() {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn child(mut value: Value) -> Value {
        value["result_family"] = json!(AgentToolResultFamily::ChildResult);
        value
    }

    #[test]
    fn wire_projection_covers_children_and_rejects_untyped_results() {
        let interrupted = child(json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "agent_id": "a1",
            "finish_reason": "budget_exhausted",
            "result": "partial"
        }));
        let projection = project_agent_tool_wire("get_result", true, Some(&interrupted));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Interrupted);
        assert_eq!(projection.agent_id, Some("a1"));
        assert_eq!(projection.finish_reason, Some("budget_exhausted"));
        assert!(projection.has_result);

        let launched = child(json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "a1"
        }));
        let projection = project_agent_tool_wire("get_result", true, Some(&launched));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Running);

        let untyped = json!({"agent_id": "a1", "result": "done"});
        let projection = project_agent_tool_wire("get_result", true, Some(&untyped));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Failed);

        let empty_success = json!({"agent_id": "a1"});
        let projection = project_agent_tool_wire("get_result", true, Some(&empty_success));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Failed);

        let unknown_status = json!({"status": "mystery", "agent_id": "a1"});
        let projection = project_agent_tool_wire("get_result", true, Some(&unknown_status));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Failed);

        let tool_failed = project_agent_tool_wire("spawn", false, None);
        assert_eq!(tool_failed.outcome, AgentToolWireOutcomeKind::Failed);

        for result in [Value::Null, json!(""), json!("  ")] {
            let incomplete = child(json!({
                "status": "completed", "agent_id": "a1", "result": result
            }));
            let projection = project_agent_tool_wire("get_result", true, Some(&incomplete));
            assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Failed);
            assert!(projection.child_terminal);
            assert!(!projection.has_result);
            assert!(agent_tool_result_needs_recovery(&incomplete));
        }
    }

    #[test]
    fn waiting_status_projects_as_incomplete_running_wire() {
        let waiting = child(json!({
            "status": AgentToolResultStatusKind::Waiting.as_str(),
            "agent_id": "a1",
            "reason": "executor_offline"
        }));

        let projection = project_agent_tool_wire("get_result", true, Some(&waiting));
        assert_eq!(projection.outcome, AgentToolWireOutcomeKind::Running);
        assert_eq!(projection.agent_id, Some("a1"));
        assert_eq!(
            agent_tool_incomplete_reason(&waiting).as_deref(),
            Some("child agent is waiting (executor_offline)")
        );
    }

    #[test]
    fn structured_result_classification_is_shared_for_agent_and_fanout() {
        let active_agent = child(json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "agent_id": "a1"
        }));
        assert_eq!(
            agent_tool_structured_result_class(&active_agent),
            Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
        );
        assert!(agent_tool_result_needs_recovery(&active_agent));

        let timeout_agent = child(json!({
            "status": "timed_out",
            "agent_id": "a1"
        }));
        assert_eq!(
            agent_tool_structured_result_class(&timeout_agent),
            Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
        );

        let completed_agent = child(json!({
            "status": AgentToolResultStatusKind::Completed.as_str(),
            "agent_id": "a1",
            "result": "done"
        }));
        assert_eq!(
            agent_tool_structured_result_class(&completed_agent),
            Some(AGENT_RESULT_CLASS_SUCCESS)
        );
        assert!(!agent_tool_result_needs_recovery(&completed_agent));

        let empty_completed_agent = child(json!({
            "status": AgentToolResultStatusKind::Completed.as_str(),
            "agent_id": "a2",
            "result": "   "
        }));
        assert_eq!(
            agent_tool_structured_result_class(&empty_completed_agent),
            Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
        );
        assert!(agent_tool_result_needs_recovery(&empty_completed_agent));

        let fanout = json!({
            "status": "completed",
            "group_id": "review",
            "results": [{
                "slot_index": 0,
                "agent_id": "a1",
                "result": active_agent
            }]
        });
        assert_eq!(
            agent_fanout_structured_result_class(&fanout),
            Some(AGENT_RESULT_CLASS_FANOUT_INCOMPLETE)
        );
        assert!(agent_fanout_result_has_recoverable_issue(&fanout));
        assert!(fanout_slot_status_is_recoverable_issue("spawn_rejected"));
        assert!(fanout_slot_status_is_recoverable_issue("timed_out"));
        assert!(fanout_slot_status_is_recoverable_issue(
            "cancelled_by_runtime"
        ));
    }

    #[test]
    fn control_receipts_are_neutral_only_with_complete_action_specific_facts() {
        let controls = [
            (
                json!({"action":"spawn", "status":"launched", "parent_run_id":"p", "agent_id":"a", "run_id":"r"}),
                "agent_id",
            ),
            (
                json!({"action":"wait", "status":"wait_admitted", "wait_request":{"parent_run_id":"p", "tool_call_id":"call", "timeout_ms":1}}),
                "wait_request",
            ),
            (
                json!({"action":"send_message", "status":"queued", "run_id":"r", "message_id":"m", "target":"parent", "message_type":"text", "recipients":null}),
                "message_id",
            ),
            (
                json!({"action":"list", "status":"ok", "parent_run_id":"p", "agents":[]}),
                "agents",
            ),
        ];
        for (mut receipt, required_field) in controls {
            receipt["result_family"] = json!(AgentToolResultFamily::ControlReceipt);
            receipt["success"] = json!(true);
            assert!(matches!(
                decode_agent_tool_result(&receipt),
                Some(DecodedAgentToolResult::ControlReceipt(_))
            ));
            assert_eq!(agent_tool_structured_result_class(&receipt), None);
            assert!(!agent_tool_result_needs_recovery(&receipt));
            // Metadata cannot promote a valid control to child success.
            receipt["result_class"] = json!("success");
            assert_eq!(agent_tool_structured_result_class(&receipt), None);
            assert!(agent_tool_completed_result_text(&receipt).is_none());
            for field in [
                "result_family",
                "action",
                "status",
                "success",
                required_field,
            ] {
                let mut malformed = receipt.clone();
                malformed.as_object_mut().unwrap().remove(field);
                assert!(
                    decode_agent_tool_result(&malformed).is_none(),
                    "{malformed}"
                );
                assert_eq!(
                    agent_tool_structured_result_class(&malformed),
                    Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
                );
            }
            for (field, invalid) in [
                ("result_family", json!("unknown")),
                ("action", json!("unknown")),
                ("status", json!("unknown")),
                ("status", json!("failed")),
                ("status", json!("delivery_unknown")),
                ("status", json!("completed")),
                ("status", Value::Null),
                ("success", json!(false)),
                ("incomplete", json!(true)),
                ("incomplete", json!(false)),
                ("result", json!("")),
                ("result", json!("child text")),
                ("result", Value::Null),
                ("finish_reason", json!("stop")),
                ("error", json!("delivery uncertain")),
            ] {
                let mut malformed = receipt.clone();
                malformed[field] = invalid;
                assert!(
                    decode_agent_tool_result(&malformed).is_none(),
                    "{malformed}"
                );
                assert_eq!(
                    agent_tool_structured_result_class(&malformed),
                    Some(AGENT_RESULT_CLASS_AGENT_INCOMPLETE)
                );
            }
            // A valid outcome for another action is not this action's receipt.
            for status in ["launched", "wait_admitted", "queued", "ok"] {
                if receipt["status"] == status {
                    continue;
                }
                let mut wrong_action = receipt.clone();
                wrong_action["status"] = json!(status);
                assert!(
                    decode_agent_tool_result(&wrong_action).is_none(),
                    "{wrong_action}"
                );
            }
        }
        for request in [
            json!({"parent_run_id":"p", "tool_call_id":"call", "timeout_ms":0}),
            json!({"parent_run_id":"p", "tool_call_id":"call", "timeout_ms":AGENT_WAIT_MAX_MS + 1}),
            json!({"parent_run_id":"p", "tool_call_id":"", "timeout_ms":1}),
            json!({"parent_run_id":"", "tool_call_id":"call", "timeout_ms":1}),
        ] {
            assert!(decode_agent_tool_result(&json!({"result_family":"control_receipt", "action":"wait", "status":"wait_admitted", "success":true, "wait_request":request})).is_none());
        }
    }

    #[test]
    fn fanout_start_requires_typed_identity_before_crossing_the_boundary() {
        for (fact, expected) in [
            (
                Value::Bool(false),
                Some(DecodedAgentToolResult::ControlFailure(
                    AgentFanoutControlExecutionFact::NotExecuted,
                )),
            ),
            (
                Value::Null,
                Some(DecodedAgentToolResult::ControlFailure(
                    AgentFanoutControlExecutionFact::Unknown,
                )),
            ),
            (Value::Bool(true), None),
            (json!("false"), None),
        ] {
            let receipt = json!({"status":"rejected", "executed":fact});
            assert_eq!(decode_agent_tool_result(&receipt), expected, "{receipt}");
        }
        assert!(
            decode_agent_tool_result(&json!({"status":"failed", "error":"transport failed"}))
                .is_none()
        );
        assert!(
            decode_agent_tool_result(
                &json!({"status":"rejected", "executed":false, "success":true})
            )
            .is_none()
        );
        assert_eq!(
            agent_fanout_control_receipt_kind(
                r#"{"status":"failed","error_kind":"fanout_group_already_started","executed":false,"group_id":"existing-group"}"#
            ),
            Some(AgentFanoutControlReceiptKind::RejectedBeforeAcceptance)
        );
        assert!(!agent_fanout_control_result_is_usable(""));
        assert!(!agent_fanout_control_result_is_usable(
            r#"{"status":"completed"}"#
        ));
        assert!(agent_fanout_control_result_is_usable(
            r#"{"status":"completed","group_id":"review"}"#
        ));
        assert!(!agent_fanout_control_result_is_usable(
            r#"{"status":"failed","error":"invalid target_count"}"#
        ));
        assert_eq!(
            agent_fanout_control_receipt_kind(
                "{\"status\":\"failed\",\"error\":\"invalid target_count\"}\nRetry with valid arguments."
            ),
            None
        );
        assert_eq!(
            agent_fanout_control_execution_fact(
                r#"{"status":"rejected","error_kind":"deferred_tool_descriptor_stale","advisory":{"executed":false}}"#
            ),
            Some(AgentFanoutControlExecutionFact::NotExecuted)
        );
        assert_eq!(
            agent_fanout_control_receipt_kind(
                r#"{"status":"rejected","error_kind":"deferred_tool_descriptor_stale","advisory":{"executed":false}}"#
            ),
            Some(AgentFanoutControlReceiptKind::RejectedBeforeAcceptance)
        );
        let skipped = r#"{"status":"rejected","outcome":"delegation_skipped","reason_code":"insufficient_time_to_delegate","executed":false}"#;
        assert_eq!(
            agent_fanout_control_receipt_kind(skipped),
            Some(AgentFanoutControlReceiptKind::RejectedBeforeAcceptance)
        );
        assert!(agent_fanout_control_result_is_usable(skipped));
        assert_eq!(
            agent_fanout_control_receipt_kind(
                r#"{"status":"unknown","error_kind":"action_outcome_unknown","advisory":{"executed":null}}"#
            ),
            Some(AgentFanoutControlReceiptKind::ExecutionUnknown)
        );
        assert!(!agent_fanout_control_result_is_usable(
            r#"{"status":"failed","error":"a later registry receipt is required","advisory":{"executed":true}}"#
        ));
        assert!(!agent_fanout_control_result_is_usable(
            r#"{"status":"failed"}"#
        ));
        assert!(!agent_fanout_control_result_is_usable("bounded result"));
        assert!(agent_fanout_control_result_is_usable(
            r#"{"status":"running","group_id":"review","results":[]}"#
        ));
    }

    #[test]
    fn render_wait_for_agent_status_preserves_waiting_reason_and_hint() {
        let rendered = render_wait_for_agent_status(
            "a1",
            &AgentStatus::Waiting {
                reason: "executor_offline".to_string(),
            },
        );
        let parsed: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(
            parsed["status"],
            AgentToolResultStatusKind::Waiting.as_str()
        );
        assert_eq!(parsed["agent_id"], "a1");
        assert_eq!(parsed["reason"], "executor_offline");
        assert!(
            parsed["hint"]
                .as_str()
                .is_some_and(|hint| hint.contains("Do not fabricate"))
        );
    }

    #[test]
    fn paused_child_wire_preserves_machine_status_without_terminal_success() {
        let rendered = render_wait_for_agent_status(
            "paused-child",
            &AgentStatus::Paused {
                reason: "executor_offline".into(),
            },
        );
        let parsed: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["status"], "paused");
        assert_eq!(parsed["reason"], "executor_offline");
        assert_eq!(parsed["resumable"], true);
        assert_eq!(
            decode_agent_tool_result(&parsed),
            Some(DecodedAgentToolResult::ChildResult(
                AgentToolResultStatusKind::Paused
            ))
        );
        assert!(agent_tool_result_needs_recovery(&parsed));
        let wire = project_agent_tool_wire("get_result", true, Some(&parsed));
        assert!(!wire.child_terminal);
        assert!(!wire.has_result);
        assert_eq!(wire.outcome, AgentToolWireOutcomeKind::Running);
    }

    #[test]
    fn interrupted_status_remains_incomplete_for_non_enum_finish_reason() {
        let rendered = render_wait_for_agent_status(
            "a1",
            &AgentStatus::Interrupted {
                partial_result: String::new(),
                finish_reason: "durable_result_unavailable".to_string(),
            },
        );
        let parsed: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(
            parsed["status"],
            AgentToolResultStatusKind::Interrupted.as_str()
        );
        assert_eq!(parsed["finish_reason"], "durable_result_unavailable");
        assert_eq!(parsed["incomplete"], true);
        assert!(agent_tool_result_needs_recovery(&parsed));
    }

    #[test]
    fn completed_agent_result_only_marks_known_interruptions_incomplete() {
        let warning = render_completed_agent_result(
            "a1",
            "done with warning",
            Some("completed_with_warnings"),
        );
        let parsed: Value = serde_json::from_str(&warning).unwrap();
        assert_eq!(
            parsed["status"],
            AgentToolResultStatusKind::Completed.as_str()
        );
        assert_eq!(parsed["finish_reason"], "completed_with_warnings");
        assert_eq!(parsed["incomplete"], false);

        let interrupted = render_completed_agent_result("a1", "partial", Some("empty_completion"));
        let parsed: Value = serde_json::from_str(&interrupted).unwrap();
        assert_eq!(
            parsed["status"],
            AgentToolResultStatusKind::Interrupted.as_str()
        );
        assert_eq!(parsed["finish_reason"], "empty_completion");
        assert_eq!(parsed["incomplete"], true);

        let safety_redacted = render_completed_agent_result(
            "a1",
            crate::response_guard::INTERNAL_PROTOCOL_FALLBACK,
            Some(crate::response_guard::RESPONSE_GUARD_REDACTED_FINISH_REASON),
        );
        let parsed: Value = serde_json::from_str(&safety_redacted).unwrap();
        assert_eq!(
            parsed["status"],
            AgentToolResultStatusKind::Completed.as_str()
        );
        assert_eq!(
            parsed["finish_reason"],
            crate::response_guard::RESPONSE_GUARD_REDACTED_FINISH_REASON
        );
        assert_eq!(parsed["incomplete"], false);
    }

    #[test]
    fn running_preview_covers_still_running_and_launched() {
        let still_running = json!({
            "status": AgentToolResultStatusKind::StillRunning.as_str(),
            "current_status": "running",
            "waited_secs": 120,
            "hint": "call again"
        });
        assert_eq!(
            agent_tool_running_preview(&still_running).as_deref(),
            Some("Agent is running after 120s. call again")
        );

        let launched = json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "a"
        });
        assert_eq!(
            agent_tool_running_preview(&launched).as_deref(),
            Some("Agent launched; waiting for get_result output.")
        );
    }

    #[test]
    fn live_wait_timeout_preserves_execution_state_without_promising_transport() {
        let status = AgentStatus::Running {
            activity: "reviewing".to_string(),
        };
        let rendered =
            render_wait_timeout_outcome("reviewer", Some(&status), Duration::from_secs(1));
        let parsed: Value = serde_json::from_str(&rendered).unwrap();

        assert_eq!(parsed["status"], "still_running");
        assert_eq!(parsed["waited_secs"], 1);
        assert!(parsed.get("delivery").is_none());
        assert_eq!(parsed["observation_timed_out"], true);
        assert_eq!(parsed["hint"], PENDING_CHILD_RUNTIME_WAIT_GUIDANCE);
        assert!(PENDING_CHILD_RUNTIME_WAIT_GUIDANCE.contains("when continuation is available"));
        assert!(PENDING_CHILD_RUNTIME_WAIT_GUIDANCE.contains("Do not busy-poll"));
        assert!(PENDING_CHILD_RUNTIME_WAIT_GUIDANCE.contains("shell sleep"));
        let idle: Value = serde_json::from_str(&render_wait_for_agent_status(
            "reviewer",
            &AgentStatus::Idle,
        ))
        .unwrap();
        assert_eq!(idle["hint"], PENDING_CHILD_RUNTIME_WAIT_GUIDANCE);
        let waiting: Value = serde_json::from_str(&render_wait_timeout_outcome(
            "reviewer",
            Some(&AgentStatus::Waiting {
                reason: "needs user input".to_string(),
            }),
            Duration::from_secs(1),
        ))
        .unwrap();
        assert_eq!(waiting["status"], "waiting");
        assert_eq!(waiting["reason"], "needs user input");
        assert_eq!(waiting["observation_timed_out"], true);
        let paused: Value = serde_json::from_str(&render_wait_timeout_outcome(
            "reviewer",
            Some(&AgentStatus::Paused {
                reason: "approval needed".into(),
            }),
            Duration::from_secs(1),
        ))
        .unwrap();
        assert_eq!(paused["status"], "paused");
        assert_eq!(paused["reason"], "approval needed");
        assert_eq!(paused["resumable"], true);
    }

    #[test]
    fn unknown_status_via_from_str_is_error_but_via_serde_is_other() {
        // First-principles split: `from_str` is the typed call path — caller
        // is supposed to know what statuses exist, so unknown is a bug and
        // surfaces as `Err`. Serde wire deserialization (e.g. JSON arriving
        // from an out-of-version peer) routes unknowns to `Other` to keep
        // the wire backwards-tolerant rather than dropping the message.
        assert!(AgentToolResultStatusKind::from_str("mystery").is_err());
        let kind: AgentToolResultStatusKind =
            serde_json::from_value(serde_json::Value::String("mystery".into())).unwrap();
        assert_eq!(kind, AgentToolResultStatusKind::Other);
    }

    #[test]
    fn from_str_normalizes_case_and_whitespace() {
        assert_eq!(
            AgentToolResultStatusKind::from_str("Completed").unwrap(),
            AgentToolResultStatusKind::Completed
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("  STILL_RUNNING  ").unwrap(),
            AgentToolResultStatusKind::StillRunning
        );
        assert_eq!(
            AgentToolResultStatusKind::from_str("TIMEOUT").unwrap(),
            AgentToolResultStatusKind::TimedOut
        );
    }

    #[test]
    fn interrupted_message_uses_shared_wait_copy_for_get_result() {
        assert_eq!(
            agent_tool_interrupted_message(true, Some("budget_exhausted")),
            "Needs continuation: The run reached its execution budget."
        );
        assert_eq!(
            agent_tool_interrupted_message(false, Some("context_overflow")),
            "Needs compaction: The conversation exceeded the model context window."
        );
        assert_eq!(
            agent_tool_interrupted_message(false, None),
            "Agent stopped before completing its result."
        );
    }

    #[test]
    fn status_summary_prefers_result_then_running_then_reason() {
        let completed = json!({
            "status": AgentToolResultStatusKind::Interrupted.as_str(),
            "result": "partial draft\nmore",
            "finish_reason": "budget_exhausted"
        });
        assert_eq!(
            agent_tool_status_summary(&completed).as_deref(),
            Some("partial draft…")
        );

        let launched = json!({
            "status": AgentToolResultStatusKind::Launched.as_str(),
            "agent_id": "a"
        });
        assert_eq!(
            agent_tool_status_summary(&launched).as_deref(),
            Some("Agent launched; waiting for get_result output.")
        );

        let failed = json!({
            "status": AgentToolResultStatusKind::Failed.as_str(),
            "error": "child result retrieval failed"
        });
        assert_eq!(
            agent_tool_status_summary(&failed).as_deref(),
            Some("child result retrieval failed")
        );
    }

    #[test]
    fn malformed_argument_receipt_exposes_only_safe_parse_metadata() {
        let rendered = render_agent_tool_malformed_arguments_error(
            "agent_fanout",
            Some(&json!({
                "kind": "invalid_json",
                "category": "eof",
                "argument_bytes": 2048,
                "line": 1,
                "column": 2049,
                "raw": "do-not-echo"
            })),
        );
        let value: Value = serde_json::from_str(&rendered).expect("structured receipt");
        assert_eq!(value["error_kind"], "tool_invalid_args");
        assert_eq!(value["advisory"]["executed"], false);
        assert!(
            value["advisory"]["next_step"]
                .as_str()
                .unwrap()
                .starts_with("If the task permits another call")
        );
        assert_eq!(
            value["advisory"]["parse_error"],
            json!({
                "kind": "invalid_json",
                "category": "eof",
                "argument_bytes": 2048,
                "line": 1,
                "column": 2049
            })
        );
        assert!(!rendered.contains("do-not-echo"));
    }
}
