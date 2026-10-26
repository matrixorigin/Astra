//! Bounded, owner-bound Server evidence acquired before suite cleanup.

use crate::criteria::FanoutGroupSelector;
use crate::session_capture::{JournalToolCall, SessionCapture};
use astra_server_types::{SessionRunLifecycleStatus, SessionRunTreeSnapshot};
use astra_services::reflect::ReflectReport;
use astra_services::session_journal::ToolCallDisposition;
use astra_turn_core::orchestration::agent_result_wire::{
    AgentFanoutControlExecutionFact, agent_control_result_value,
    agent_fanout_control_execution_fact,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionOwner {
    pub account_id: String,
    pub profile_name: String,
    pub api_origin: String,
}

impl ExecutionOwner {
    pub(crate) fn is_valid(&self) -> bool {
        !self.account_id.is_empty()
            && self.account_id.len() <= 256
            && !self.profile_name.is_empty()
            && self.profile_name.len() <= 256
            && self.api_origin.len() <= 1024
            && reqwest::Url::parse(&self.api_origin).is_ok_and(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExecutionCapture {
    pub schema_version: u32,
    pub session_id: String,
    pub owner: ExecutionOwner,
    pub run_tree: SessionRunTreeSnapshot,
    pub reflection: ReflectReport,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript: Option<astra_thin_client::SessionTranscriptPage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_projections: Option<Vec<Value>>,
}

#[derive(Debug, Serialize)]
pub struct VerifiedChildEvidence<'a> {
    pub(crate) run_id: &'a str,
    pub(crate) agent_id: &'a str,
    pub(crate) model: &'a str,
    pub(crate) offering_id: Option<&'a str>,
    pub(crate) slot_index: Option<u32>,
    pub(crate) content: &'a str,
    pub(crate) result_sha256: String,
}

/// Closed public reason codes: never serialize evidence identities or payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChildProofReason {
    ChildCountMismatch,
    ChildNotCompleted,
    ModelMismatch,
    FanoutStartMissing,
    FanoutStartRepeated,
    FanoutStartUnsuccessful,
    FanoutRejectionExecuted,
    FanoutSelectionInvalid,
    FanoutTitleInvalid,
    FanoutRejectionExecutionUnknown,
    FanoutSelectionUnavailable,
    FanoutGroupMismatch,
    FanoutChildCountMismatch,
    FanoutLabelInvalid,
    FanoutIdentityMismatch,
    FanoutChildrenMismatch,
    SpawnIdentityMismatch,
    SlotMissing,
    ModelAmbiguous,
    ChildSelectionRepeated,
    OfferingMismatch,
    ThinkingMismatch,
    MutationMismatch,
    QuestionUnanswered,
    LogicalRoundsMismatch,
    ChildResultConflict,
    ExpectedResultMismatch,
    ResultDigestMismatch,
    FinalizationBeforeAdoption,
    RunTreeUnavailable,
    RootRunning,
    RootWaiting,
    RootPaused,
    RootDelegated,
    RootInterrupted,
    RootFailed,
    RootCancelled,
    FanoutReceiptUnavailable,
    ModelIdentityUnavailable,
    OfferingIdentityUnavailable,
    ThinkingUnavailable,
    MutationUnavailable,
    QuestionEvidenceUnavailable,
    LogicalRoundsUnavailable,
    ChildResultUnavailable,
    AdoptionEvidenceUnavailable,
    ResultWindowUnavailable,
    ResultWindowIdentityConflict,
    ResultWindowDigestConflict,
    ResultWindowInvalid,
    AgentIdentityUnavailable,
    FinalizationUnavailable,
}

#[derive(Debug)]
pub(crate) struct ChildProofFailure {
    pub reason: ChildProofReason,
    // Preserve existing private errors and classification. Public diagnostics
    // serialize only the reason enum, never this string.
    pub unavailable_detail: Option<&'static str>,
}

impl ChildProofFailure {
    fn into_detail(self) -> &'static str {
        self.unavailable_detail
            .unwrap_or("execution evidence contradicted")
    }

    fn contradicted(reason: ChildProofReason) -> Self {
        Self {
            reason,
            unavailable_detail: None,
        }
    }

    fn unavailable(reason: ChildProofReason, detail: &'static str) -> Self {
        Self {
            reason,
            unavailable_detail: Some(detail),
        }
    }
}

/// Full documents borrow the existing trace; parsed fields retain the shared reader's identity.
pub(crate) struct ExecutionToolCall<'a> {
    record: JournalToolCall,
    raw_result: &'a str,
}

impl std::ops::Deref for ExecutionToolCall<'_> {
    type Target = JournalToolCall;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}

impl ExecutionToolCall<'_> {
    pub(crate) fn raw_result(&self) -> &str {
        self.raw_result
    }

    pub(crate) fn execution_disposition(&self) -> Option<ToolCallDisposition> {
        serde_json::from_value(self.runtime_metadata["disposition"].clone()).ok()
    }

    /// Public diagnostics contain only recognized machine tags, never result prose.
    pub(crate) fn safe_outcome(&self) -> Value {
        let result = agent_control_result_value(self.raw_result());
        let status = match result.as_ref().and_then(|value| value["status"].as_str()) {
            Some(
                status @ ("completed"
                | "failed"
                | "rejected"
                | "blocked"
                | "started"
                | "unknown"
                | "waiting"
                | "paused"
                | "cancelled"
                | "interrupted"
                | "timeout"
                | "timed_out"
                | "still_running"
                | "launched"
                | "incomplete"
                | "completed_with_issues"),
            ) => status,
            Some(_) => "other",
            None => "not_reported",
        };
        let error_kind = result.as_ref().and_then(|value| value.get("error_kind"));
        let error_kind = match error_kind.and_then(Value::as_str) {
            Some(
                code @ ("delegation_model_scope_unresolved"
                | "delegation_model_unavailable"
                | "delegation_preparation_unavailable"
                | "delegation_model_assessment_unavailable"
                | "invalid_delegation_model_scope"
                | "execution_time_budget_exhausted"),
            ) => code,
            _ => error_kind.map_or("not_reported", |value| {
                serde_json::from_value::<astra_core::ErrorKind>(value.clone())
                    .map_or("other", astra_core::ErrorKind::as_str)
            }),
        };
        let execution = match agent_fanout_control_execution_fact(self.raw_result()) {
            Some(AgentFanoutControlExecutionFact::Executed) => "executed",
            Some(AgentFanoutControlExecutionFact::NotExecuted) => "not_executed",
            Some(AgentFanoutControlExecutionFact::Unknown) => "unknown",
            None => "not_reported",
        };
        let validation = (error_kind == "tool_invalid_args")
            .then(|| {
                result
                    .as_ref()
                    .and_then(|value| value.get("validation"))
                    .and_then(|value| {
                        astra_tools::schemas::validated_argument_diagnostic(&self.name, value)
                    })
            })
            .flatten();
        serde_json::json!({"disposition":self.execution_disposition(), "ok":self.ok, "status":status,
            "error_kind":error_kind, "execution":execution, "validation":validation})
    }
}

impl SessionExecutionCapture {
    fn validated_runs(
        &self,
        root: &str,
    ) -> Result<
        std::collections::BTreeMap<&str, &astra_server_types::SessionRunNode>,
        ChildProofFailure,
    > {
        let unavailable =
            |detail| ChildProofFailure::unavailable(ChildProofReason::RunTreeUnavailable, detail);
        if !self.matches(&self.session_id, &self.owner) || self.run_tree.truncated {
            return Err(unavailable(
                "execution identity or complete run tree unavailable",
            ));
        }
        let runs: std::collections::BTreeMap<_, _> = self
            .run_tree
            .runs
            .iter()
            .map(|run| (run.run_id.as_str(), run))
            .collect();
        if runs.len() != self.run_tree.runs.len()
            || !runs
                .get(root)
                .is_some_and(|run| run.parent_run_id.is_none())
        {
            return Err(unavailable("root or unique run identities unavailable"));
        }
        for run in runs.values() {
            if let Some(parent) = run.parent_run_id.as_deref() {
                let parent = runs
                    .get(parent)
                    .ok_or_else(|| unavailable("run parent missing from complete tree"))?;
                if parent.depth.checked_add(1) != Some(run.depth)
                    || run.root_run_id != parent.root_run_id
                {
                    return Err(unavailable("run lineage conflict"));
                }
            } else if run.depth != 0 || run.root_run_id.as_deref() != Some(run.run_id.as_str()) {
                return Err(unavailable("root lineage conflict"));
            }
        }
        // A lifecycle is diagnostic evidence only after identity and lineage
        // validation. Successful child certification still requires Completed.
        let reason = match runs[root].status {
            SessionRunLifecycleStatus::Completed => return Ok(runs),
            SessionRunLifecycleStatus::Running => ChildProofReason::RootRunning,
            SessionRunLifecycleStatus::Waiting => ChildProofReason::RootWaiting,
            SessionRunLifecycleStatus::Paused => ChildProofReason::RootPaused,
            SessionRunLifecycleStatus::Delegated => ChildProofReason::RootDelegated,
            SessionRunLifecycleStatus::Interrupted => ChildProofReason::RootInterrupted,
            SessionRunLifecycleStatus::Failed => ChildProofReason::RootFailed,
            SessionRunLifecycleStatus::Cancelled => ChildProofReason::RootCancelled,
        };
        Err(ChildProofFailure::unavailable(
            reason,
            "captured root run has not completed",
        ))
    }

    pub(crate) fn child_count(&self, root: &str) -> Option<usize> {
        Some(
            self.validated_runs(root)
                .ok()?
                .values()
                .filter(|run| run.parent_run_id.as_deref() == Some(root))
                .count(),
        )
    }

    fn run_projection(&self, root: &str, run_id: &str) -> Result<&Value, &'static str> {
        let runs = self
            .validated_runs(root)
            .map_err(ChildProofFailure::into_detail)?;
        if runs
            .get(run_id)
            .is_none_or(|run| run_id != root && run.parent_run_id.as_deref() != Some(root))
        {
            return Err("direct child identity conflict");
        }
        let mut projections = self
            .run_projections
            .as_ref()
            .ok_or("run projections not captured")?
            .iter()
            .filter(|projection| projection["run_id"].as_str() == Some(run_id));
        let projection = projections
            .next()
            .ok_or("child run projection unavailable")?;
        if projections.next().is_some()
            || projection["session_id"].as_str() != Some(self.session_id.as_str())
            || !projection["run_event_high_watermark"]
                .as_i64()
                .is_some_and(|index| index >= 0)
        {
            return Err("child run projection identity conflict");
        }
        Ok(projection)
    }

    pub(crate) fn run_initial_thinking(
        &self,
        root: &str,
        run_id: &str,
    ) -> Result<astra_turn_core::thinking_config::ThinkingConfig, &'static str> {
        let projection = self.run_projection(root, run_id)?;
        let mut starts = projection["recent_events"]
            .as_array()
            .ok_or("child run events unavailable")?
            .iter()
            .filter(|event| event["type"] == "run_started");
        let start = starts
            .next()
            .ok_or("initial admission outside captured tail")?;
        if starts.next().is_some()
            || start["run_id"].as_str() != Some(run_id)
            || start["index"].as_i64() != Some(0)
        {
            return Err("initial admission identity conflict");
        }
        let value = start
            .pointer("/generation_controls/thinking")
            .ok_or("initial thinking unavailable")?;
        let thinking: astra_turn_core::thinking_config::ThinkingConfig =
            serde_json::from_value(value.clone()).map_err(|_| "initial thinking malformed")?;
        if serde_json::to_value(&thinking).ok().as_ref() != Some(value) {
            return Err("initial thinking incomplete");
        }
        thinking
            .validate_output_budget(u64::MAX)
            .map_err(|_| "initial thinking malformed")?;
        Ok(thinking)
    }

    fn run_logical_rounds(&self, root: &str, run_id: &str) -> Result<u32, &'static str> {
        let projection = self.run_projection(root, run_id)?;
        let mut terminals = projection["recent_events"]
            .as_array()
            .ok_or("child run events unavailable")?
            .iter()
            .filter(|event| event["type"] == "run_finished" && event["status"] == "completed");
        let terminal = terminals
            .next()
            .ok_or("completed child terminal outside captured tail")?;
        if terminals.next().is_some()
            || terminal["run_id"] != run_id
            || terminal["index"].as_i64().is_none_or(|index| {
                index < 0
                    || index
                        > projection["run_event_high_watermark"]
                            .as_i64()
                            .unwrap_or(-1)
            })
            || terminal
                .pointer("/turn_evaluation/producer_scope/run_id")
                .and_then(Value::as_str)
                != Some(run_id)
            || terminal
                .pointer("/turn_evaluation/session_id")
                .and_then(Value::as_str)
                != Some(self.session_id.as_str())
            || terminal
                .pointer("/turn_evaluation/metadata/run_status")
                .and_then(Value::as_str)
                != Some("completed")
            || terminal["owner_generation"].as_u64().is_none()
            || terminal["owner_generation"]
                != terminal["turn_evaluation"]["metadata"]["execution_owner_generation"]
        {
            return Err("child logical-round terminal identity conflict");
        }
        terminal
            .pointer("/turn_evaluation/metadata/llm_rounds")
            .and_then(Value::as_u64)
            .and_then(|rounds| u32::try_from(rounds).ok())
            .ok_or("child logical rounds unavailable")
    }

    /// Select an unambiguous terminal document, not a display preview or the
    /// parent's serialization. The caller proves custody with its raw digest.
    pub(crate) fn run_terminal_content(
        &self,
        root: &str,
        run_id: &str,
    ) -> Result<Option<&str>, &'static str> {
        let runs = self
            .validated_runs(root)
            .map_err(ChildProofFailure::into_detail)?;
        let child = runs.get(run_id).ok_or("direct child unavailable")?;
        if child.parent_run_id.as_deref() != Some(root) {
            return Err("direct child identity conflict");
        }
        if child.status != SessionRunLifecycleStatus::Completed {
            return Ok(None);
        }
        let page = self
            .transcript
            .as_ref()
            .ok_or("child transcript not captured")?;
        if page.session_id != self.session_id || page.has_more {
            return Err("complete child transcript unavailable");
        }
        let mut identities = std::collections::BTreeSet::new();
        let mut content = None;
        for item in &page.items {
            if item.session_id != self.session_id
                || item.item_seq <= 0
                || !identities.insert(item.item_seq)
                || item
                    .run_id
                    .as_deref()
                    .is_some_and(|run| !runs.contains_key(run))
            {
                return Err("child transcript identity conflict");
            }
            if item.run_id.as_deref() == Some(child.run_id.as_str())
                && item.role == "assistant"
                && item.tool_calls.is_empty()
                && item.tool_result.is_none()
                && item.evidence.is_none()
                && (item.content.is_empty()
                    || item.source_event_id.as_deref().is_none_or(str::is_empty)
                    || content.replace(item.content.as_str()).is_some())
            {
                return Err("child terminal content unavailable or ambiguous");
            }
        }
        content.map(Some).ok_or("child terminal content missing")
    }

    pub(crate) fn tools<'trace>(
        &self,
        root: &str,
        trace: Option<&'trace SessionCapture>,
    ) -> Result<Vec<ExecutionToolCall<'trace>>, &'static str> {
        let runs = self
            .validated_runs(root)
            .map_err(ChildProofFailure::into_detail)?;
        let trace = trace.ok_or("authorized invocation trace not captured")?;
        if trace.session_id != self.session_id {
            return Err("execution trace belongs to another session");
        }
        let mut calls = trace
            .execution_tool_calls()?
            .into_iter()
            .map(|(record, _, raw_result)| ExecutionToolCall { record, raw_result })
            .collect::<Vec<_>>();
        for call in &calls {
            let run = call
                .run_id
                .as_deref()
                .ok_or("tool run identity unavailable")?;
            let owner = runs.get(run).ok_or("tool outside authoritative run tree")?;
            let _ = owner;
        }
        calls.retain(|call| {
            let run = call.run_id.as_deref().expect("validated trace identity");
            run == root || runs[run].root_run_id.as_deref() == Some(root)
        });
        for run in runs
            .values()
            .filter(|run| run.run_id == root || run.root_run_id.as_deref() == Some(root))
        {
            if calls
                .iter()
                .filter(|call| call.run_id.as_deref() == Some(run.run_id.as_str()))
                .count()
                != run.total_tool_calls as usize
            {
                return Err("trace tool count disagrees with authoritative run");
            }
        }
        Ok(calls)
    }
    /// Completed child execution can precede asynchronous trace ingestion.
    /// Wait only for the owner's terminal boundary, never for oracle success.
    pub(crate) fn awaiting_completion_evidence(&self, root_id: Option<&str>) -> bool {
        let Some(root_id) = root_id else {
            return false;
        };
        let Ok(runs) = self.validated_runs(root_id) else {
            return false;
        };
        let mut children = runs
            .values()
            .filter(|run| run.parent_run_id.as_deref() == Some(root_id));
        let Some(child) = children.next() else {
            return false;
        };
        if child.status != SessionRunLifecycleStatus::Completed
            || children.any(|child| child.status != SessionRunLifecycleStatus::Completed)
        {
            return false;
        }
        !self
            .reflection
            .graph_slice
            .nodes
            .iter()
            .filter_map(|node| {
                node.metadata
                    .as_ref()?
                    .pointer("/execution_spine/facts")?
                    .as_array()
            })
            .flatten()
            .any(|fact| {
                fact["metadata_available"] == true
                    && fact["metadata_omitted"] == false
                    && fact["kind"] == "agent_dependency_boundary"
                    && fact["run_id"] == root_id
                    && fact["parent_run_id"] == root_id
                    && matches!(
                        fact["outcome"].as_str(),
                        Some(
                            "finalization_accepted"
                                | "finalization_incomplete"
                                | "finalization_interrupted"
                                | "synthesis_budget_exhausted"
                                | "remote_owner_unsettled"
                        )
                    )
            })
    }

    fn matches(&self, session: &str, owner: &ExecutionOwner) -> bool {
        self.schema_version == 1
            && self.owner.is_valid()
            && &self.owner == owner
            && self.session_id == session
            && self.run_tree.session_id == session
            && self.run_tree.schema_version == astra_server_types::SESSION_RUN_TREE_SCHEMA_VERSION
            && self.reflection.session_id == session
            && self.reflection.schema_version == 2
            && self.reflection.topic == "execution"
            && self.reflection.facet == "trace"
            && self.reflection.horizon == "session"
    }

    /// Positive evidence may survive partial reflection. A truncated run tree
    /// cannot establish the required direct-child cardinality; missing terminal
    /// results or boundary facts cannot establish completion or adoption.
    pub(crate) fn verified_children<'a>(
        &'a self,
        root: &str,
        expected: &[crate::criteria::ChildExecutionExpectation],
        fanout_group: Option<&FanoutGroupSelector>,
        trace: Option<&SessionCapture>,
    ) -> Result<Option<Vec<VerifiedChildEvidence<'a>>>, &'static str> {
        match self.verify_children(root, expected, fanout_group, trace) {
            Ok(evidence) => Ok(Some(evidence)),
            Err(failure) => match failure.unavailable_detail {
                Some(detail) => Err(detail),
                None => Ok(None),
            },
        }
    }

    pub(crate) fn verify_children<'a>(
        &'a self,
        root: &str,
        expected: &[crate::criteria::ChildExecutionExpectation],
        fanout_group: Option<&FanoutGroupSelector>,
        trace: Option<&SessionCapture>,
    ) -> Result<Vec<VerifiedChildEvidence<'a>>, ChildProofFailure> {
        use crate::criteria::ChildThinkingExpectation;
        let runs = self.validated_runs(root)?;
        let children: Vec<_> = runs
            .values()
            .filter(|child| child.parent_run_id.as_deref() == Some(root))
            .copied()
            .collect();
        if children.len() != expected.len() || expected.is_empty() {
            return Err(ChildProofFailure::contradicted(
                ChildProofReason::ChildCountMismatch,
            ));
        }
        if children
            .iter()
            .any(|child| child.status != SessionRunLifecycleStatus::Completed)
        {
            return Err(ChildProofFailure::contradicted(
                ChildProofReason::ChildNotCompleted,
            ));
        }
        if fanout_group.is_none() {
            let mut remaining = std::collections::BTreeMap::new();
            for expectation in expected {
                *remaining
                    .entry(expectation.model.as_str())
                    .or_insert(0usize) += 1;
            }
            for model in children
                .iter()
                .filter_map(|child| child.runtime.model_name.as_deref())
            {
                let Some(count) = remaining.get_mut(model).filter(|count| **count > 0) else {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::ModelMismatch,
                    ));
                };
                *count -= 1;
            }
        }
        let mut slots = std::collections::BTreeMap::new();
        if let Some(group) = fanout_group {
            let tools = self.tools(root, trace).map_err(|detail| {
                ChildProofFailure::unavailable(ChildProofReason::FanoutReceiptUnavailable, detail)
            })?;
            let field = match group {
                FanoutGroupSelector::Id(_) => "group_id",
                FanoutGroupSelector::Title(_) => "title",
            };
            let mut start = None;
            let mut missing_selection = false;
            for call in tools.iter().filter(|call| {
                call.run_id.as_deref() == Some(root)
                    && call.name == "agent_fanout"
                    && call
                        .arguments
                        .as_ref()
                        .is_some_and(|args| args["action"] == "start")
            }) {
                if call.execution_disposition() == Some(ToolCallDisposition::Rejected) {
                    // A pre-execution rejection is not another launch, even
                    // when it references an existing group. Contradictory or
                    // explicitly unknown execution cannot be discarded.
                    if agent_control_result_value(call.raw_result())
                        .is_some_and(|value| value["status"] == "started")
                    {
                        return Err(ChildProofFailure::contradicted(
                            ChildProofReason::FanoutRejectionExecuted,
                        ));
                    }
                    match agent_fanout_control_execution_fact(call.raw_result()) {
                        Some(AgentFanoutControlExecutionFact::Executed) => {
                            return Err(ChildProofFailure::contradicted(
                                ChildProofReason::FanoutRejectionExecuted,
                            ));
                        }
                        Some(AgentFanoutControlExecutionFact::Unknown) => {
                            return Err(ChildProofFailure::unavailable(
                                ChildProofReason::FanoutRejectionExecutionUnknown,
                                "fanout rejection execution unknown",
                            ));
                        }
                        None | Some(AgentFanoutControlExecutionFact::NotExecuted) => continue,
                    }
                }
                match call.result.as_ref().and_then(|receipt| receipt.get(field)) {
                    None | Some(Value::Null) => missing_selection = true,
                    Some(Value::String(actual)) if actual == group.value().trim() => {
                        if start.replace(call).is_some() {
                            return Err(ChildProofFailure::contradicted(
                                ChildProofReason::FanoutStartRepeated,
                            ));
                        }
                    }
                    Some(Value::String(_)) => {}
                    _ => {
                        return Err(ChildProofFailure::contradicted(
                            ChildProofReason::FanoutSelectionInvalid,
                        ));
                    }
                }
            }
            if missing_selection {
                return Err(ChildProofFailure::unavailable(
                    ChildProofReason::FanoutSelectionUnavailable,
                    "fanout selection identity unavailable",
                ));
            }
            let Some(start) = start else {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::FanoutStartMissing,
                ));
            };
            if start.ok != Some(true) {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::FanoutStartUnsuccessful,
                ));
            }
            let receipt = start.result.as_ref().ok_or_else(|| {
                ChildProofFailure::unavailable(
                    ChildProofReason::FanoutReceiptUnavailable,
                    "fanout receipt unavailable",
                )
            })?;
            // Selection is not identity proof. Every current start receipt
            // carries the same canonical identity in these producer projections.
            let receipt_id = match &receipt["group_id"] {
                Value::Null => {
                    return Err(ChildProofFailure::unavailable(
                        ChildProofReason::FanoutReceiptUnavailable,
                        "fanout group identity unavailable",
                    ));
                }
                Value::String(id) if !id.trim().is_empty() && id.trim() == id => id.as_str(),
                _ => {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::FanoutGroupMismatch,
                    ));
                }
            };
            let args = start.arguments.as_ref().expect("selected start arguments");
            match &args["group_id"] {
                Value::Null => {}
                Value::String(id) if !id.trim().is_empty() && id.trim() == receipt_id => {}
                _ => {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::FanoutGroupMismatch,
                    ));
                }
            }
            let expected_title = match &args["title"] {
                Value::Null => receipt_id,
                Value::String(title) => {
                    if title.trim().is_empty() {
                        receipt_id
                    } else {
                        title.trim()
                    }
                }
                _ => {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::FanoutTitleInvalid,
                    ));
                }
            };
            let mut missing_identity = false;
            for (path, expected) in [
                ("/status", "started"),
                ("/title", expected_title),
                ("/fanout/group_id", receipt_id),
                ("/fanout/title", expected_title),
                ("/fanout/parent_run_id", root),
                ("/work_unit_observation/id", receipt_id),
                ("/work_unit_observation/kind", "agent_fanout"),
            ] {
                match receipt.pointer(path) {
                    None | Some(Value::Null) => missing_identity = true,
                    Some(Value::String(actual)) if actual == expected => {}
                    _ => {
                        return Err(ChildProofFailure::contradicted(
                            ChildProofReason::FanoutIdentityMismatch,
                        ));
                    }
                }
            }
            if missing_identity {
                return Err(ChildProofFailure::unavailable(
                    ChildProofReason::FanoutReceiptUnavailable,
                    "fanout receipt identity projections unavailable",
                ));
            }
            let agents = receipt["agents"].as_array().ok_or_else(|| {
                ChildProofFailure::unavailable(
                    ChildProofReason::FanoutReceiptUnavailable,
                    "fanout child identities unavailable",
                )
            })?;
            if agents.len() != children.len() {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::FanoutChildCountMismatch,
                ));
            }
            let mut returned_runs = std::collections::BTreeSet::new();
            let mut returned_agents = std::collections::BTreeSet::new();
            let mut returned_labels = std::collections::BTreeSet::new();
            for entry in agents {
                let slot = entry["slot_index"]
                    .as_u64()
                    .and_then(|slot| u32::try_from(slot).ok())
                    .ok_or_else(|| {
                        ChildProofFailure::unavailable(
                            ChildProofReason::FanoutReceiptUnavailable,
                            "fanout slot identity unavailable",
                        )
                    })?;
                let run_id = entry["run_id"].as_str().ok_or_else(|| {
                    ChildProofFailure::unavailable(
                        ChildProofReason::FanoutReceiptUnavailable,
                        "fanout run identity unavailable",
                    )
                })?;
                let agent_id = entry["agent_id"].as_str().ok_or_else(|| {
                    ChildProofFailure::unavailable(
                        ChildProofReason::FanoutReceiptUnavailable,
                        "fanout agent identity unavailable",
                    )
                })?;
                if !entry["id"].is_null()
                    && entry["id"]
                        .as_str()
                        .is_none_or(|label| label.is_empty() || !returned_labels.insert(label))
                {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::FanoutLabelInvalid,
                    ));
                }
                if run_id.is_empty()
                    || agent_id.is_empty()
                    || !returned_runs.insert(run_id)
                    || !returned_agents.insert(agent_id)
                    || slots.insert(slot, run_id.to_owned()).is_some()
                    || runs.get(run_id).is_none_or(|child| {
                        child.parent_run_id.as_deref() != Some(root)
                            || child.agent_id.as_deref() != Some(agent_id)
                    })
                {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::FanoutIdentityMismatch,
                    ));
                }
            }
            if returned_runs != children.iter().map(|child| child.run_id.as_str()).collect() {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::FanoutChildrenMismatch,
                ));
            }
        }
        let facts: Vec<_> = self
            .reflection
            .graph_slice
            .nodes
            .iter()
            .filter_map(|node| {
                node.metadata
                    .as_ref()?
                    .pointer("/execution_spine/facts")?
                    .as_array()
            })
            .flatten()
            .filter(|fact| fact["metadata_available"] == true && fact["metadata_omitted"] == false)
            .collect();
        if facts.iter().any(|fact| {
            children.iter().any(|child| {
                fact["kind"] == "agent_spawned"
                    && fact["run_id"] == child.run_id
                    && (fact["parent_run_id"] != root
                        || child
                            .agent_id
                            .as_deref()
                            .is_some_and(|agent| fact["agent_id"] != agent))
            })
        }) {
            return Err(ChildProofFailure::contradicted(
                ChildProofReason::SpawnIdentityMismatch,
            ));
        }
        let mut selected = std::collections::BTreeSet::new();
        let mut latest_adoption = None;
        let mut unavailable = None;
        let mut verified = Vec::with_capacity(expected.len());
        for expectation in expected {
            let child = if fanout_group.is_some() {
                let Some(run_id) = expectation.slot_index.and_then(|slot| slots.get(&slot)) else {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::SlotMissing,
                    ));
                };
                runs[run_id.as_str()]
            } else {
                let mut matching = children.iter().filter(|child| {
                    child.runtime.model_name.as_deref() == Some(expectation.model.as_str())
                });
                let Some(child) = matching.next() else {
                    if children
                        .iter()
                        .any(|child| child.runtime.model_name.is_none())
                    {
                        unavailable = Some(ChildProofFailure::unavailable(
                            ChildProofReason::ModelIdentityUnavailable,
                            "child model identity unavailable",
                        ));
                        continue;
                    }
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::ModelMismatch,
                    ));
                };
                if matching.next().is_some() {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::ModelAmbiguous,
                    ));
                }
                *child
            };
            if !selected.insert(child.run_id.as_str()) {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::ChildSelectionRepeated,
                ));
            }
            if child
                .runtime
                .model_name
                .as_deref()
                .is_some_and(|model| model != expectation.model)
            {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::ModelMismatch,
                ));
            }
            if child.runtime.model_name.is_none() {
                unavailable = Some(ChildProofFailure::unavailable(
                    ChildProofReason::ModelIdentityUnavailable,
                    "child model identity unavailable",
                ));
            }
            if let Some(thinking) = &expectation.initial_thinking {
                let expected = match thinking {
                    ChildThinkingExpectation::Exact { config } => Ok(config.clone()),
                    ChildThinkingExpectation::SameAsRoot => {
                        match (&runs[root].runtime.offering_id, &child.runtime.offering_id) {
                            (Some(parent), Some(current)) if parent != current => {
                                return Err(ChildProofFailure::contradicted(
                                    ChildProofReason::OfferingMismatch,
                                ));
                            }
                            (None, _) | (_, None) => {
                                unavailable = Some(ChildProofFailure::unavailable(
                                    ChildProofReason::OfferingIdentityUnavailable,
                                    "inherited offering identity unavailable",
                                ));
                            }
                            _ => {}
                        }
                        self.run_initial_thinking(root, root)
                    }
                };
                match expected.and_then(|expected| {
                    self.run_initial_thinking(root, &child.run_id)
                        .map(|actual| actual == expected)
                }) {
                    Ok(false) => {
                        return Err(ChildProofFailure::contradicted(
                            ChildProofReason::ThinkingMismatch,
                        ));
                    }
                    Err(reason) => {
                        unavailable = Some(ChildProofFailure::unavailable(
                            ChildProofReason::ThinkingUnavailable,
                            reason,
                        ))
                    }
                    _ => {}
                }
            }
            if let Some(expected) = expectation.workspace_mutation {
                let mut observed = false;
                for fact in facts.iter().filter(|fact| {
                    fact["kind"] == "agent_spawned" && fact["run_id"] == child.run_id
                }) {
                    if let Ok(actual) = serde_json::from_value::<
                        astra_config::user_profile::WorkspaceMutationIntent,
                    >(fact["workspace_mutation"].clone())
                    {
                        if actual != expected {
                            return Err(ChildProofFailure::contradicted(
                                ChildProofReason::MutationMismatch,
                            ));
                        }
                        observed = true;
                    }
                }
                if !observed {
                    unavailable = Some(ChildProofFailure::unavailable(
                        ChildProofReason::MutationUnavailable,
                        "recorded child mutation intent unavailable",
                    ));
                }
            }
            if expectation.answered_question {
                match self.proves_answered_question(root, child, trace) {
                    Ok(false) => {
                        return Err(ChildProofFailure::contradicted(
                            ChildProofReason::QuestionUnanswered,
                        ));
                    }
                    Err(reason) => {
                        unavailable = Some(ChildProofFailure::unavailable(
                            ChildProofReason::QuestionEvidenceUnavailable,
                            reason,
                        ))
                    }
                    _ => {}
                }
            }
            if let Some(rounds) = expectation.logical_rounds {
                match self.run_logical_rounds(root, &child.run_id) {
                    Ok(actual) if actual != rounds => {
                        return Err(ChildProofFailure::contradicted(
                            ChildProofReason::LogicalRoundsMismatch,
                        ));
                    }
                    Err(reason) => {
                        unavailable = Some(ChildProofFailure::unavailable(
                            ChildProofReason::LogicalRoundsUnavailable,
                            reason,
                        ))
                    }
                    _ => {}
                }
            }
            let content = match self.run_terminal_content(root, &child.run_id) {
                Ok(Some(raw)) => raw,
                Ok(None) => {
                    return Err(ChildProofFailure::contradicted(
                        ChildProofReason::ChildResultConflict,
                    ));
                }
                Err(reason) => {
                    unavailable = Some(ChildProofFailure::unavailable(
                        ChildProofReason::ChildResultUnavailable,
                        reason,
                    ));
                    continue;
                }
            };
            let result_matches =
                expectation
                    .expected_result
                    .as_ref()
                    .is_none_or(|expected| match expected {
                        crate::criteria::ChildResultExpectation::Text(text) => content == text,
                        crate::criteria::ChildResultExpectation::Contains(needle) => {
                            content.contains(needle)
                        }
                        crate::criteria::ChildResultExpectation::Json(value) => {
                            serde_json::from_str::<Value>(content).ok().as_ref() == Some(value)
                        }
                    });
            if !result_matches {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::ExpectedResultMismatch,
                ));
            }
            let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
            let observed_digests: Vec<_> = facts
                .iter()
                .filter(|fact| {
                    fact["kind"] == "agent_dependency_boundary"
                        && fact["run_id"] == root
                        && fact["parent_run_id"] == root
                        && fact["outcome"] == "results_adopted"
                })
                .filter_map(|fact| fact["children"].as_array())
                .flatten()
                .filter(|entry| {
                    entry["run_id"] == child.run_id
                        && entry["agent_id"] == child.agent_id.as_deref().unwrap_or_default()
                        && entry["status"] == "completed"
                        && entry["result_truncated"] == false
                })
                .filter_map(|entry| entry["result_sha256"].as_str())
                .collect();
            if !observed_digests.is_empty() && !observed_digests.contains(&digest.as_str()) {
                return Err(ChildProofFailure::contradicted(
                    ChildProofReason::ResultDigestMismatch,
                ));
            }
            match self.child_adoption_time(root, child, content, &facts) {
                Ok(time) => {
                    latest_adoption = Some(
                        latest_adoption
                            .map_or(time, |latest: chrono::NaiveDateTime| latest.max(time)),
                    )
                }
                Err(reason) => {
                    unavailable = Some(reason);
                    continue;
                }
            }
            let agent_id = child.agent_id.as_deref().ok_or_else(|| {
                ChildProofFailure::unavailable(
                    ChildProofReason::AgentIdentityUnavailable,
                    "child agent identity unavailable",
                )
            })?;
            let Some(model) = child.runtime.model_name.as_deref() else {
                // Keep checking later siblings: a missing model is an
                // unavailable witness, not permission to hide a known
                // contradiction in another requested child.
                unavailable = Some(ChildProofFailure::unavailable(
                    ChildProofReason::ModelIdentityUnavailable,
                    "child model identity unavailable",
                ));
                continue;
            };
            verified.push(VerifiedChildEvidence {
                run_id: child.run_id.as_str(),
                agent_id,
                model,
                offering_id: child.runtime.offering_id.as_deref(),
                slot_index: expectation.slot_index,
                content,
                result_sha256: digest,
            });
        }
        let finalizations: Vec<_> = facts
            .iter()
            .filter(|fact| {
                fact["kind"] == "agent_dependency_boundary"
                    && fact["run_id"] == root
                    && fact["parent_run_id"] == root
                    && fact["outcome"] == "finalization_accepted"
            })
            .filter_map(|fact| fact_time(fact))
            .collect();
        if latest_adoption.is_some_and(|latest| {
            !finalizations.is_empty() && finalizations.iter().all(|time| *time <= latest)
        }) {
            return Err(ChildProofFailure::contradicted(
                ChildProofReason::FinalizationBeforeAdoption,
            ));
        }
        if let Some(reason) = unavailable {
            return Err(reason);
        }
        let latest = latest_adoption.ok_or_else(|| {
            ChildProofFailure::unavailable(
                ChildProofReason::AdoptionEvidenceUnavailable,
                "child adoption unavailable",
            )
        })?;
        if finalizations.is_empty() {
            return Err(ChildProofFailure::unavailable(
                ChildProofReason::FinalizationUnavailable,
                "parent finalization unavailable",
            ));
        }
        if !finalizations.iter().any(|time| *time > latest) {
            return Err(ChildProofFailure::contradicted(
                ChildProofReason::FinalizationBeforeAdoption,
            ));
        }
        Ok(verified)
    }

    #[cfg(test)]
    fn fixture_tools(&self, root: &str) -> Result<Vec<JournalToolCall>, &'static str> {
        self.tools(root, Some(&tests::trace(self)))
            .map(|calls| calls.into_iter().map(|call| call.record).collect())
    }

    #[cfg(test)]
    fn fixture_verified_children<'a>(
        &'a self,
        root: &str,
        expected: &[crate::criteria::ChildExecutionExpectation],
        group: Option<&FanoutGroupSelector>,
    ) -> Result<Option<Vec<VerifiedChildEvidence<'a>>>, &'static str> {
        self.verified_children(root, expected, group, Some(&tests::trace(self)))
    }

    #[cfg(test)]
    pub(crate) fn proves_children(
        &self,
        root: &str,
        expected: &[crate::criteria::ChildExecutionExpectation],
        group: Option<&FanoutGroupSelector>,
    ) -> Result<bool, &'static str> {
        self.fixture_verified_children(root, expected, group)
            .map(|evidence| evidence.is_some())
    }

    /// A queued answer is not observation. Exact durable communication plus
    /// Completed closes the existing reply-obligation fence: its owner clears
    /// only after a successful provider attempt consumes the matching response.
    fn proves_answered_question(
        &self,
        root: &str,
        child: &astra_server_types::SessionRunNode,
        trace: Option<&SessionCapture>,
    ) -> Result<bool, &'static str> {
        use astra_turn_types::{
            AgentCommunicationDirection as Direction, AgentCommunicationPayloadKind as Kind,
            AgentCommunicationTarget as Target, AgentTranscriptEvidence,
        };
        let agent = child
            .agent_id
            .as_deref()
            .ok_or("question actor unavailable")?;
        let tools = self.tools(root, trace)?;
        let transcript = self
            .transcript
            .as_ref()
            .ok_or("communication transcript unavailable")?;
        // Bind the chain by actual root-owned answer calls, not by a second
        // transcript mirror of their send. Stable mailbox identity comes from
        // the question's canonical PendingReply and the child's delivery fact.
        let answered_requests: std::collections::HashSet<_> = tools
            .iter()
            .filter(|call| call.run_id.as_deref() == Some(root) && call.name == "agent")
            .filter_map(|call| call.arguments.as_ref())
            .filter(|args| args["action"] == "send_message" && args["message_type"] == "answer")
            .filter_map(|args| args["request_id"].as_str().filter(|id| !id.is_empty()))
            .collect();
        let mut selected = None;
        for question in tools.iter().filter(|call| {
            call.run_id.as_deref() == Some(child.run_id.as_str())
                && call.name == "agent"
                && call.arguments.as_ref().is_some_and(|args| {
                    args["action"] == "send_message" && args["message_type"] == "question"
                })
        }) {
            if agent_fanout_control_execution_fact(question.raw_result())
                == Some(AgentFanoutControlExecutionFact::NotExecuted)
            {
                continue;
            }
            let receipt = question
                .result
                .as_ref()
                .ok_or("question receipt unavailable")?;
            let request = receipt["message_id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .ok_or("question identity unavailable")?;
            if !answered_requests.contains(request) {
                continue;
            }
            let obligation: astra_turn_types::PendingReply =
                serde_json::from_value(receipt["reply_obligation"].clone())
                    .map_err(|_| "question responder unavailable")?;
            if selected.replace((question, obligation)).is_some() {
                return Ok(false);
            }
        }
        let Some((question, obligation)) = selected else {
            return Ok(false);
        };
        let receipt = question
            .result
            .as_ref()
            .ok_or("question receipt unavailable")?;
        if question.ok != Some(true)
            || receipt["run_id"] != child.run_id
            || receipt["status"] != "queued"
        {
            return Ok(false);
        }
        let request = receipt["message_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("question identity unavailable")?;
        if obligation.request_id != request
            || obligation.expected_responder.run_id.is_empty()
            || obligation.expected_responder.agent_id.is_empty()
        {
            return Ok(false);
        }
        let mut answers = tools.iter().filter(|call| {
            call.run_id.as_deref() == Some(root)
                && call.name == "agent"
                && call.arguments.as_ref().is_some_and(|args| {
                    args["action"] == "send_message"
                        && args["message_type"] == "answer"
                        && args["request_id"] == request
                })
        });
        let Some(answer) = answers.next() else {
            return Ok(false);
        };
        if answers.next().is_some() || answer.ok != Some(true) {
            return Ok(false);
        }
        let receipt = answer.result.as_ref().ok_or("answer receipt unavailable")?;
        if receipt["run_id"] != root || receipt["status"] != "queued" {
            return Ok(false);
        }
        let message = receipt["message_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("answer identity unavailable")?;
        let mut received = false;
        let mut unavailable = false;
        for item in &transcript.items {
            let Some(AgentTranscriptEvidence::AgentCommunication { event }) = &item.evidence else {
                continue;
            };
            if event.message_id != message && event.message_id != request {
                continue;
            }
            if event.schema_version != astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION
                || item.session_id != self.session_id
                || item.role != "event"
                || item.source_event_id.as_deref().is_none_or(str::is_empty)
                || item.run_id.as_deref() != Some(event.observed_by.run_id.as_str())
            {
                unavailable = true;
                continue;
            }
            if event.message_id == request {
                let observer_matches = match event.direction {
                    Direction::Sent => {
                        event.observed_by.run_id == child.run_id
                            && event.observed_by.agent_id == agent
                    }
                    Direction::Received => {
                        event.observed_by.run_id == root && !event.observed_by.agent_id.is_empty()
                    }
                };
                if event.from.run_id != child.run_id
                    || event.from.agent_id != agent
                    || event.payload_kind != Kind::Request
                    || event.related_message_id.is_some()
                    || event.response_accepted.is_some()
                    || !matches!(&event.to, Target::Direct { address }
                        if address == &obligation.expected_responder)
                    || !observer_matches
                {
                    return Ok(false);
                }
                continue;
            }
            if event.from != obligation.expected_responder
                || event.related_message_id.as_deref() != Some(request)
                || event.payload_kind != Kind::Response
                || event.response_accepted != Some(true)
                || !matches!(&event.to, Target::Direct { address }
                    if address.run_id == child.run_id && address.agent_id == agent)
            {
                return Ok(false);
            }
            match event.direction {
                Direction::Sent
                    if event.observed_by.run_id == root
                        && !event.observed_by.agent_id.is_empty() => {}
                Direction::Received
                    if event.observed_by.run_id == child.run_id
                        && event.observed_by.agent_id == agent =>
                {
                    received = true
                }
                _ => return Ok(false),
            }
        }
        if unavailable || !received {
            return Err("answer delivery observation unavailable");
        }
        Ok(true)
    }

    fn child_adoption_time(
        &self,
        root_id: &str,
        child: &astra_server_types::SessionRunNode,
        content: &str,
        facts: &[&Value],
    ) -> Result<chrono::NaiveDateTime, ChildProofFailure> {
        let (Some(agent), Some(offering)) = (
            child.agent_id.as_deref(),
            child.runtime.offering_id.as_deref(),
        ) else {
            return Err(ChildProofFailure::unavailable(
                if child.agent_id.as_deref().is_none_or(str::is_empty) {
                    ChildProofReason::AgentIdentityUnavailable
                } else {
                    ChildProofReason::OfferingIdentityUnavailable
                },
                "child agent or offering identity unavailable",
            ));
        };
        if agent.is_empty() || offering.is_empty() {
            return Err(ChildProofFailure::unavailable(
                if child.agent_id.as_deref().is_none_or(str::is_empty) {
                    ChildProofReason::AgentIdentityUnavailable
                } else {
                    ChildProofReason::OfferingIdentityUnavailable
                },
                "child agent or offering identity unavailable",
            ));
        }
        // Completion and model identity come from the validated Run tree;
        // result content comes from its terminal transcript. Adoption is a
        // separate parent-owned fact, bound below to that exact content.
        // Provider request diagnostics are not execution authority and may be
        // absent for Run-owned inference.
        let mut windows = Vec::new();
        for fact in facts.iter().filter(|fact| {
            fact["kind"] == "agent_dependency_boundary"
                && fact["run_id"] == root_id
                && fact["parent_run_id"] == root_id
                && fact["outcome"] == "results_adopted"
        }) {
            let Some(time) = fact_time(fact) else {
                continue;
            };
            for entry in fact["children"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|entry| {
                    entry["agent_id"] == agent
                        && entry["run_id"] == child.run_id
                        && entry["status"] == "completed"
                })
            {
                let Some(digest) = entry["result_sha256"].as_str() else {
                    continue;
                };
                let range = if entry["result_range"].is_null() {
                    if entry["result_truncated"] != false {
                        continue;
                    }
                    [0, content.len(), content.len()]
                } else {
                    serde_json::from_value::<[usize; 3]>(entry["result_range"].clone()).map_err(
                        |_| {
                            ChildProofFailure::unavailable(
                                ChildProofReason::ResultWindowUnavailable,
                                "child result window unavailable",
                            )
                        },
                    )?
                };
                let [start, end, total] = range;
                if total != content.len() {
                    return Err(ChildProofFailure::unavailable(
                        ChildProofReason::ResultWindowIdentityConflict,
                        "child result window identity conflict",
                    ));
                }
                let slice = content.get(start..end).ok_or_else(|| {
                    ChildProofFailure::unavailable(
                        ChildProofReason::ResultWindowInvalid,
                        "child result window invalid",
                    )
                })?;
                if digest != format!("{:x}", Sha256::digest(slice.as_bytes())) {
                    return Err(ChildProofFailure::unavailable(
                        ChildProofReason::ResultWindowDigestConflict,
                        "child result window digest conflict",
                    ));
                }
                windows.push((start, end, time));
            }
        }
        // Overlapping/repeated pages are fine; a gap, wrong payload or only a
        // prefix is not complete custody. No model text participates in this proof.
        windows.sort_unstable_by_key(|(_, _, time)| *time);
        let mut ranges = Vec::new();
        for (start, end, time) in windows {
            ranges.push((start, end));
            ranges.sort_unstable();
            let mut covered = 0;
            for &(start, end) in &ranges {
                if start > covered {
                    break;
                }
                covered = covered.max(end);
            }
            if covered == content.len() {
                return Ok(time);
            }
        }
        Err(ChildProofFailure::unavailable(
            ChildProofReason::AdoptionEvidenceUnavailable,
            "child result adoption unavailable",
        ))
    }
}

fn fact_time(fact: &Value) -> Option<chrono::NaiveDateTime> {
    chrono::NaiveDateTime::parse_from_str(fact["created_at"].as_str()?, "%Y-%m-%dT%H:%M:%S%.f").ok()
}

/// Reuse the same CLI binary, environment and profile that produced the run.
/// No harness credential resolver, direct database read, or local fallback.
pub(crate) async fn load(
    cfg: &crate::runner::RunnerConfig,
    case: &crate::case::Case,
    outcome: &crate::runner::RunOutcome,
) -> Result<SessionExecutionCapture, String> {
    load_until(
        cfg,
        case,
        outcome,
        tokio::time::Instant::now() + std::time::Duration::from_secs(20),
    )
    .await
}

pub(crate) async fn load_until(
    cfg: &crate::runner::RunnerConfig,
    case: &crate::case::Case,
    outcome: &crate::runner::RunOutcome,
    deadline: tokio::time::Instant,
) -> Result<SessionExecutionCapture, String> {
    use tokio::io::AsyncReadExt;
    const MAX_BYTES: u64 = 5 * 1024 * 1024;
    let stream = outcome
        .stream_capture
        .as_ref()
        .filter(|stream| stream.identity_verified)
        .ok_or("unverified execution scope")?;
    if stream
        .diagnostics
        .iter()
        .any(|code| code == "execution_owner_conflict")
    {
        return Err("conflicting execution owner".into());
    }
    let session = outcome
        .session_id
        .as_deref()
        .ok_or("missing session identity")?;
    let owner = stream.owner.as_ref().ok_or("missing execution owner")?;
    if !owner.is_valid()
        || cfg
            .profile
            .as_deref()
            .is_some_and(|profile| profile != owner.profile_name)
    {
        return Err("execution profile changed".into());
    }
    let account_scope = astra_services::OwnerScope::user(&owner.account_id)
        .map_err(|_| "invalid execution account")?;
    if !cfg.artifact_owner_scopes.contains(&account_scope) {
        return Err("execution account is outside the authorized capture scope".into());
    }
    let mut last_partial = None;
    loop {
        let mut command = crate::exec::configured_astra_command(cfg, case);
        command
            .args(["session", "show", session, "--execution"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if crate::criteria::requires_execution_transcript(&case.criteria) {
            command.arg("--transcript");
        }
        if crate::criteria::requires_execution_run_events(&case.criteria) {
            command.arg("--run-events");
        }
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command
            .spawn()
            .map_err(|_| "execution capture could not start")?;
        let group_id = child.id();
        let mut stdout = child
            .stdout
            .take()
            .ok_or("execution capture has no stdout")?
            .take(MAX_BYTES + 1);
        let mut bytes = Vec::new();
        let read = async {
            stdout
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| "execution capture read failed")?;
            if bytes.len() as u64 > MAX_BYTES {
                return Err("execution capture exceeded byte limit");
            }
            let status = child
                .wait()
                .await
                .map_err(|_| "execution capture did not settle")?;
            if !status.success() {
                return Err("execution capture command failed");
            }
            Ok(())
        };
        let result = tokio::time::timeout_at(deadline, read).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                crate::exec::kill_process_group_and_reap(&mut child, group_id).await;
                return Err("execution capture failed".into());
            }
            Err(_) => {
                crate::exec::kill_process_group_and_reap(&mut child, group_id).await;
                return last_partial
                    .ok_or_else(|| "execution capture exceeded its deadline".into());
            }
        }
        let capture: SessionExecutionCapture =
            serde_json::from_slice(&bytes).map_err(|_| "execution capture schema is invalid")?;
        if !capture.matches(session, owner) {
            return Err("execution capture owner or scope changed".into());
        }
        if !capture.awaiting_completion_evidence(outcome.run_id.as_deref())
            || tokio::time::Instant::now() >= deadline
        {
            return Ok(capture);
        }
        last_partial = Some(capture);
        let wake = (tokio::time::Instant::now() + std::time::Duration::from_secs(2)).min(deadline);
        tokio::time::sleep_until(wake).await;
        if tokio::time::Instant::now() >= deadline {
            return Ok(last_partial.expect("validated partial precedes observation wait"));
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Shared fixture producer for the existing explicit request/receipt data.
    /// Observation rounds preserve overlap: parallel slots never acquire an order.
    pub(crate) fn trace(capture: &SessionExecutionCapture) -> SessionCapture {
        use crate::session_capture::JournalEvent;
        let mut trace = SessionCapture {
            session_id: capture.session_id.clone(),
            ..Default::default()
        };
        let Some(page) = &capture.transcript else {
            return trace;
        };
        let mut groups = std::collections::BTreeMap::<(String, u32), Vec<Value>>::new();
        for run in &capture.run_tree.runs {
            let mut frontier = i64::MIN;
            let mut round = 0;
            for item in page
                .items
                .iter()
                .filter(|item| item.run_id.as_deref() == Some(run.run_id.as_str()))
            {
                if item.tool_calls.is_empty() {
                    continue;
                }
                if item.item_seq > frontier && frontier != i64::MIN {
                    round += 1;
                }
                for call in &item.tool_calls {
                    let result = page.items.iter().find(|result| {
                        result.run_id == item.run_id
                            && result
                                .tool_result
                                .as_ref()
                                .is_some_and(|r| r.tool_use_id == call.tool_use_id)
                    });
                    let terminal = result.and_then(|i| i.tool_result.as_ref());
                    let disposition = terminal.and_then(|r| match r.status.as_deref() {
                        Some("completed" | "failed") => Some("executed"),
                        Some("rejected") => Some("rejected"),
                        Some("reused") => Some("reused"),
                        Some("suppressed") => Some("suppressed"),
                        Some("deferred") => Some("deferred"),
                        _ => None,
                    });
                    let ok = terminal.and_then(|r| match r.status.as_deref() {
                        Some("completed" | "reused") => Some(true),
                        Some("failed" | "rejected") => Some(false),
                        Some("suppressed" | "deferred") => Some(false),
                        _ => None,
                    });
                    frontier = frontier.max(result.map_or(i64::MAX, |i| i.item_seq));
                    groups.entry((run.run_id.clone(), round)).or_default().push(serde_json::json!({
                        "name":call.name,"tool_call_id":call.tool_use_id,"args_full":call.arguments,
                        "result_full":result.map(|i| &i.content),"disposition":disposition,"ok":ok,"round":round,
                    }));
                }
            }
        }
        for ((run, round), records) in groups {
            trace.events.push(JournalEvent {event_type:"llm_round".into(), raw:serde_json::json!({
                "type":"llm_round","ts":chrono::Utc::now().to_rfc3339(),"turn":1,"round":round,"producer_scope":{"run_id":run},"tool_calls":records,
            })});
        }
        for run in &capture.run_tree.runs {
            if let Some(parent) = &run.parent_run_id {
                trace.events.push(JournalEvent { event_type:"agent_spawned".into(), raw:serde_json::json!({
                    "type":"agent_spawned","ts":chrono::Utc::now().to_rfc3339(),"session_id":capture.session_id,
                    "metadata":{"parent_run_id":parent,"run_id":run.run_id}
                }) });
            }
        }
        trace
    }

    fn expectation(
        text: &str,
        model: &str,
        slot_index: Option<u32>,
    ) -> crate::criteria::ChildExecutionExpectation {
        crate::criteria::ChildExecutionExpectation {
            expected_result: Some(crate::criteria::ChildResultExpectation::Text(text.into())),
            model: model.into(),
            initial_thinking: None,
            answered_question: false,
            workspace_mutation: None,
            logical_rounds: None,
            slot_index,
        }
    }

    fn singleton_proven(
        capture: &SessionExecutionCapture,
        root: Option<&str>,
        text: &str,
        model: &str,
    ) -> bool {
        root.is_some_and(|root| {
            capture.proves_children(root, &[expectation(text, model, None)], None) == Ok(true)
        })
    }

    fn proof_failure(
        capture: SessionExecutionCapture,
        children: Vec<crate::criteria::ChildExecutionExpectation>,
        group: Option<&FanoutGroupSelector>,
    ) -> Option<crate::classify::FailureClass> {
        let mut outcome = crate::runner::RunOutcome::new("test-model").with_exit_code(0);
        outcome.session_id = Some(capture.session_id.clone());
        outcome.run_id = Some("root".into());
        let mut stream = crate::runner::StreamCapture::default();
        stream.identity_verified = true;
        stream.execution = Some(capture);
        outcome.stream_capture = Some(stream);
        let trace = trace(
            outcome
                .stream_capture
                .as_ref()
                .unwrap()
                .execution
                .as_ref()
                .unwrap(),
        );
        let results = crate::criteria::evaluate_deterministic_with_session(
            &[crate::criteria::Criterion::ExecutionChildResultsAdopted {
                children,
                fanout_group: group.cloned(),
            }],
            &outcome,
            Some(&trace),
        );
        (!results[0].passed).then(|| crate::classify::classify(&outcome, &results))
    }

    /// Consumer fixture using the published Server wire shape. These tests
    /// prove oracle rejection, not production emission or live task quality.
    pub(crate) fn capture() -> SessionExecutionCapture {
        let child = serde_json::json!({
            "agent_id": "member", "run_id": "child", "status": "completed",
            "result_sha256": format!("{:x}", Sha256::digest(b"observed-value")),
            "result_truncated": false,
        });
        let fact = |kind: &str, run: &str, outcome: &str, time: &str, children: Value| {
            serde_json::json!({
                "kind": kind, "run_id": run, "parent_run_id": "root",
                "agent_id": "member", "outcome": outcome, "created_at": time,
                "metadata_available": true, "metadata_omitted": false,
                "children": children,
            })
        };
        let node = |run: &str, parent: Value, agent: Value, runtime: Value| {
            serde_json::json!({
                "run_id": run, "parent_run_id": parent, "root_run_id": "root", "depth": if parent.is_null() { 0 } else { 1 },
                "agent_id": agent, "status": "completed", "run_event_high_watermark": 1,
                "total_tool_calls": 0, "runtime": runtime, "available_actions": [],
                "created_at": "2026-10-07T00:00:00Z", "updated_at": "2026-10-07T00:00:03Z",
            })
        };
        let reflection = serde_json::json!({
            "schema_version": 2, "tool": "reflect", "session_id": "session",
            "analysis_view": "execution_trace", "topic": "execution", "facet": "trace",
            "depth": "forensic", "horizon": "session", "source_policy": "local_first",
            "include_context": false, "data_coverage": {"source":"server", "events":3, "decisions":0},
            "model_requests": astra_services::reflect::ModelRequestCapture::default(),
            "graph_slice": {"nodes":[{
                "ref_id":"spine", "layer":"runtime", "kind":"event", "label":"execution_spine",
                "metadata":{"execution_spine":{"coverage":"partial", "truncated":true, "omitted_facts":3,
                    "facts":[
                        fact("agent_spawned", "child", "running", "2026-10-07T00:00:00.000000", serde_json::json!([])),
                        fact("agent_dependency_boundary", "root", "results_adopted", "2026-10-07T00:00:01.000000", serde_json::json!([child])),
                        fact("agent_dependency_boundary", "root", "finalization_accepted", "2026-10-07T00:00:02.000000", serde_json::json!([])),
                    ]
                }}
            }]},
        });
        serde_json::from_value(serde_json::json!({
            "schema_version":1, "session_id":"session",
            "owner":{"account_id":"account", "profile_name":"profile", "api_origin":"https://example.invalid"},
            "run_tree":{"schema_version":astra_server_types::SESSION_RUN_TREE_SCHEMA_VERSION,
                "session_id":"session", "snapshot_revision":"revision", "observed_at":"2026-10-07T00:00:03Z",
                "node_limit":200, "truncated":false, "runs":[
                    node("root", Value::Null, Value::Null, serde_json::json!({})),
                    node("child", serde_json::json!("root"), serde_json::json!("member"), serde_json::json!({"offering_id":"offer","model_name":"test-model"})),
                ]},
            "reflection":reflection,
            "transcript":{"session_id":"session", "has_more":false,"next_before_seq":null,
                "items":[{"session_id":"session", "item_seq":3, "run_id":"child", "role":"assistant",
                    "content":"observed-value", "tool_calls":[], "tool_result":null,
                    "source_event_id":"child-terminal", "created_at":"2026-10-07T00:00:00Z"}]},
        })).unwrap()
    }

    pub(crate) fn tool_capture() -> SessionExecutionCapture {
        let mut original = capture();
        original.run_tree.runs[0].total_tool_calls = 1;
        let item = |seq: i64, role: &str, calls: Value, result: Value| {
            serde_json::json!({
                "session_id":"session", "item_seq":seq, "run_id":"root", "role":role,
                "content":"{\"value\":42}", "tool_calls":calls, "tool_result":result,
                "source_event_id":format!("source-{seq}"), "created_at":"2026-10-07T00:00:00Z",
            })
        };
        let page = serde_json::json!({"session_id":"session", "has_more":false,
        "next_before_seq":null, "items":[
            item(1, "assistant", serde_json::json!([{"tool_use_id":"call", "name":"agent",
                "arguments":"{\"action\":\"spawn\"}"}]), Value::Null),
            item(2, "tool", serde_json::json!([]), serde_json::json!({
                "tool_use_id":"call", "name":"agent", "status":"completed",
                "runtime_advisories":["Presentation guidance, not result evidence."]})),
        ]});
        let terminal = original.transcript.as_mut().unwrap().items.pop().unwrap();
        original.transcript = Some(serde_json::from_value(page).unwrap());
        original.transcript.as_mut().unwrap().items.push(terminal);
        original
    }

    #[test]
    fn child_custody_requires_matching_complete_windows_before_finalization() {
        let mut capture = tool_capture();
        let content = "界".repeat(3000);
        capture.transcript.as_mut().unwrap().items[2].content = content.clone();
        let facts = capture.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()
            .pointer_mut("/execution_spine/facts")
            .unwrap()
            .as_array_mut()
            .unwrap();
        let template = facts[1].clone();
        facts.remove(1);
        let window = |start: usize, end: usize| {
            let mut fact = template.clone();
            fact["children"][0]["result_range"] = serde_json::json!([start, end, content.len()]);
            fact["children"][0]["result_truncated"] = true.into();
            fact["children"][0]["result_sha256"] = serde_json::json!(format!(
                "{:x}",
                Sha256::digest(&content.as_bytes()[start..end])
            ));
            fact
        };
        facts.push(window(0, 4095));
        assert!(!singleton_proven(
            &capture,
            Some("root"),
            &content,
            "test-model"
        ));
        // Overlap and out-of-order reads are legitimate; skipped UTF-8 bytes,
        // wrong siblings and pages observed after finalization are not.
        let mut full = capture.clone();
        full.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()
            .pointer_mut("/execution_spine/facts")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .extend([window(6000, 9000), window(3000, 6000)]);
        assert!(singleton_proven(
            &full,
            Some("root"),
            &content,
            "test-model"
        ));
        let mut duplicate = window(0, 4095);
        duplicate["created_at"] = "2026-10-07T00:01:00.000000".into();
        full.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()
            .pointer_mut("/execution_spine/facts")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(
            singleton_proven(&full, Some("root"), &content, "test-model"),
            "later rereads do not erase earlier complete adoption"
        );
        for broken in ["gap", "digest", "identity", "late", "utf8", "total"] {
            let mut bad = capture.clone();
            let mut page = window(4095, 9000);
            match broken {
                "gap" => page = window(4098, 9000),
                "digest" => page["children"][0]["result_sha256"] = "0".repeat(64).into(),
                "identity" => page["children"][0]["run_id"] = "other-child".into(),
                "late" => page["created_at"] = "2026-10-07T00:01:00.000000".into(),
                "utf8" => page["children"][0]["result_range"][0] = 4096.into(),
                "total" => page["children"][0]["result_range"][2] = 8999.into(),
                _ => unreachable!(),
            }
            bad.reflection.graph_slice.nodes[0]
                .metadata
                .as_mut()
                .unwrap()
                .pointer_mut("/execution_spine/facts")
                .unwrap()
                .as_array_mut()
                .unwrap()
                .push(page);
            assert!(
                !singleton_proven(&bad, Some("root"), &content, "test-model"),
                "{broken}"
            );
        }
    }

    #[test]
    fn sibling_custody_requires_exact_children_before_one_root_finalization() {
        let group = FanoutGroupSelector::Id("group".into());
        let named = FanoutGroupSelector::Title("group".into());
        let mut capture = tool_capture();
        let mut sibling = capture.run_tree.runs[1].clone();
        sibling.run_id = "sibling".into();
        sibling.agent_id = Some("sibling-agent".into());
        capture.run_tree.runs.push(sibling);
        let page = capture.transcript.as_mut().unwrap();
        page.items[0].tool_calls[0].name = "agent_fanout".into();
        page.items[0].tool_calls[0].arguments =
            serde_json::json!({"action":"start", "group_id":"group"}).to_string();
        page.items[1].tool_result.as_mut().unwrap().name = Some("agent_fanout".into());
        page.items[1].content = serde_json::json!({
            "status":"started", "group_id":"group", "title":"group",
            "fanout":{"group_id":"group", "title":"group", "parent_run_id":"root"},
            "work_unit_observation":{"id":"group", "kind":"agent_fanout"},
            "agents":[
            {"slot_index":0,"id":"slot-0","run_id":"child","agent_id":"member"},
            {"slot_index":1,"id":"slot-1","run_id":"sibling","agent_id":"sibling-agent"},
        ]})
        .to_string();
        let mut sibling_terminal = page.items[2].clone();
        sibling_terminal.item_seq = 4;
        sibling_terminal.run_id = Some("sibling".into());
        sibling_terminal.source_event_id = Some("sibling-terminal".into());
        sibling_terminal.content = "second-value".into();
        page.items.push(sibling_terminal);
        let second = serde_json::json!({
            "kind":"agent_dependency_boundary", "run_id":"root", "parent_run_id":"root",
            "outcome":"results_adopted", "created_at":"2026-10-07T00:00:01.500000",
            "metadata_available":true, "metadata_omitted":false,
            "children":[{"agent_id":"sibling-agent", "run_id":"sibling", "status":"completed",
                "result_truncated":false, "result_sha256":format!("{:x}",Sha256::digest(b"second-value"))}],
        });
        capture.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()
            .pointer_mut("/execution_spine/facts")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(second);
        let expected = [
            expectation("observed-value", "test-model", Some(0)),
            expectation("second-value", "test-model", Some(1)),
        ];
        assert_eq!(
            capture.proves_children("root", &expected, Some(&group)),
            Ok(true)
        );
        // A requested title is independent of an optional explicit ID. All
        // launch forms must pass the same identity and custody proof.
        for (arguments, receipt_id, proven) in [
            (r#"{"action":"start","title":"group"}"#, "generated", true),
            (r#"{"action":"start","title":" group "}"#, "generated", true),
            (
                r#"{"action":"start","group_id":null,"title":"group"}"#,
                "generated",
                true,
            ),
            (r#"{"action":"start","group_id":" group "}"#, "group", true),
            (
                r#"{"action":"start","group_id":"group","title":""}"#,
                "group",
                true,
            ),
            (
                r#"{"action":"start","group_id":"group","title":" "}"#,
                "group",
                true,
            ),
            (r#"{"action":"start","group_id":"group"}"#, "other", false),
            (
                r#"{"action":"start","group_id":"other","title":"group"}"#,
                "other",
                true,
            ),
            (
                r#"{"action":"start","group_id":"","title":"group"}"#,
                "generated",
                false,
            ),
            (
                r#"{"action":"start","group_id":42,"title":"group"}"#,
                "generated",
                false,
            ),
            (r#"{"action":"start","title":"other"}"#, "generated", false),
        ] {
            let mut altered = capture.clone();
            let page = altered.transcript.as_mut().unwrap();
            page.items[0].tool_calls[0].arguments = arguments.into();
            let mut receipt: Value = serde_json::from_str(&page.items[1].content).unwrap();
            receipt["group_id"] = receipt_id.into();
            receipt["fanout"]["group_id"] = receipt_id.into();
            receipt["work_unit_observation"]["id"] = receipt_id.into();
            page.items[1].content = receipt.to_string();
            assert_eq!(
                altered.proves_children("root", &expected, Some(&named)),
                Ok(proven),
                "{arguments}"
            );
            let exact = FanoutGroupSelector::Id(receipt_id.into());
            assert_eq!(
                altered.proves_children("root", &expected, Some(&exact)),
                Ok(proven),
                "{arguments}"
            );
        }
        let mut ambiguous = capture.clone();
        ambiguous.run_tree.runs[0].total_tool_calls = 2;
        let page = ambiguous.transcript.as_mut().unwrap();
        page.items[0].tool_calls[0].arguments =
            serde_json::json!({"action":"start", "title":"group"}).to_string();
        let mut request = page.items[0].clone();
        request.item_seq = 5;
        request.tool_calls[0].tool_use_id = "second-call".into();
        request.tool_calls[0].arguments =
            serde_json::json!({"action":"start", "title":"other"}).to_string();
        let mut receipt = page.items[1].clone();
        receipt.item_seq = 6;
        receipt.tool_result.as_mut().unwrap().tool_use_id = "second-call".into();
        page.items.extend([request, receipt]);
        assert_eq!(
            ambiguous.proves_children("root", &expected, Some(&named)),
            Ok(false),
            "a display title cannot disambiguate multiple launches"
        );
        for (status, content, proof) in [
            ("rejected", "{}", Ok(true)),
            ("rejected", "not JSON", Ok(true)),
            (
                "rejected",
                r#"{"executed":false,"group_id":"group","title":"group"}"#,
                Ok(true),
            ),
            ("rejected", r#"{"executed":true}"#, Ok(false)),
            ("rejected", r#"{"status":"started"}"#, Ok(false)),
            (
                "rejected",
                "{\"status\":\"started\",\"group_id\":\"group\"}\nRecovery guidance",
                Ok(false),
            ),
            (
                "rejected",
                r#"{"executed":null}"#,
                Err("fanout rejection execution unknown"),
            ),
            ("failed", "{}", Err("fanout selection identity unavailable")),
            (
                "failed",
                "not JSON",
                Err("fanout selection identity unavailable"),
            ),
            (
                "failed",
                r#"{"executed":null}"#,
                Err("fanout selection identity unavailable"),
            ),
        ] {
            let mut altered = ambiguous.clone();
            let page = altered.transcript.as_mut().unwrap();
            // Move the second attempt before the successful launch, retaining
            // distinct, ordered request/result identities and both children.
            for item in &mut page.items {
                item.item_seq = if item.item_seq >= 5 {
                    item.item_seq - 4
                } else {
                    item.item_seq + 2
                };
            }
            let receipt = page.items.last_mut().unwrap();
            receipt.tool_result.as_mut().unwrap().status = Some(status.into());
            receipt.content = content.into();
            page.items.sort_by_key(|item| item.item_seq);
            assert_eq!(altered.fixture_tools("root").unwrap().len(), 2);
            for selector in [&group, &named] {
                assert_eq!(
                    altered.proves_children("root", &expected, Some(selector)),
                    proof,
                    "{status}: {content} / {selector:?}"
                );
            }
        }
        let mut missing_selection = ambiguous.clone();
        let page = missing_selection.transcript.as_mut().unwrap();
        let mut receipt: Value = serde_json::from_str(&page.items.last().unwrap().content).unwrap();
        receipt.as_object_mut().unwrap().remove("title");
        page.items.last_mut().unwrap().content = receipt.to_string();
        assert!(
            missing_selection
                .proves_children("root", &expected, Some(&named))
                .is_err()
        );
        for path in [
            "/group_id",
            "/status",
            "/title",
            "/fanout/group_id",
            "/fanout/title",
            "/fanout/parent_run_id",
            "/work_unit_observation/id",
            "/work_unit_observation/kind",
        ] {
            let mut altered = capture.clone();
            let mut receipt: Value =
                serde_json::from_str(&altered.transcript.as_ref().unwrap().items[1].content)
                    .unwrap();
            *receipt.pointer_mut(path).unwrap() = "wrong".into();
            altered.transcript.as_mut().unwrap().items[1].content = receipt.to_string();
            assert_eq!(
                altered.proves_children("root", &expected, Some(&named)),
                Ok(false),
                "{path}"
            );
            *receipt.pointer_mut(path).unwrap() = Value::Null;
            altered.transcript.as_mut().unwrap().items[1].content = receipt.to_string();
            assert!(
                altered
                    .proves_children("root", &expected, Some(&named))
                    .is_err(),
                "{path}"
            );
        }
        // A slot's optional display label is not its identity: the group and
        // slot index bind the authoritative child run even when labels are null.
        let mut unlabelled = capture.clone();
        let content = &mut unlabelled.transcript.as_mut().unwrap().items[1].content;
        let mut receipt: Value = serde_json::from_str(content).unwrap();
        for agent in receipt["agents"].as_array_mut().unwrap() {
            agent["id"] = Value::Null;
        }
        *content = receipt.to_string();
        assert_eq!(
            unlabelled.proves_children("root", &expected, Some(&group)),
            Ok(true)
        );
        let mut missing_model_for_order = capture.clone();
        missing_model_for_order.run_tree.runs[1].runtime.model_name = None;
        assert_eq!(
            proof_failure(
                missing_model_for_order.clone(),
                expected.to_vec(),
                Some(&group)
            ),
            Some(crate::classify::FailureClass::InfraVerificationUnavailable)
        );
        assert_eq!(
            capture.proves_children("root", &expected[..1], Some(&group)),
            Ok(false)
        );
        for expectations in [
            [
                expectation("observed-value", "test-model", Some(0)),
                expectation("wrong sibling output", "test-model", Some(1)),
            ],
            [
                expectation("wrong sibling output", "test-model", Some(1)),
                expectation("observed-value", "test-model", Some(0)),
            ],
        ] {
            assert!(
                matches!(
                    missing_model_for_order.fixture_verified_children(
                        "root",
                        &expectations,
                        Some(&group)
                    ),
                    Ok(None)
                ),
                "missing-model sibling must not hide a known contradiction"
            );
        }
        assert_eq!(
            capture.proves_children(
                "root",
                &[expected[0].clone(), expected[0].clone()],
                Some(&group)
            ),
            Ok(false)
        );
        assert_eq!(
            capture.proves_children(
                "root",
                &[
                    expectation("second-value", "test-model", Some(0)),
                    expectation("observed-value", "test-model", Some(1)),
                ],
                Some(&group)
            ),
            Ok(false)
        );
        for fault in ["late", "digest", "parent"] {
            let mut altered = capture.clone();
            match fault {
                "parent" => altered.run_tree.runs[2].parent_run_id = Some("child".into()),
                "late" | "digest" => {
                    let fact = &mut altered.reflection.graph_slice.nodes[0]
                        .metadata
                        .as_mut()
                        .unwrap()["execution_spine"]["facts"][3];
                    if fault == "late" {
                        fact["created_at"] = "2026-10-07T00:00:03.000000".into();
                    } else {
                        fact["children"][0]["result_sha256"] = "wrong".into();
                    }
                }
                _ => unreachable!(),
            }
            assert_ne!(
                altered.proves_children("root", &expected, Some(&group)),
                Ok(true),
                "{fault}"
            );
        }
        for fault in [
            "swapped-slots",
            "duplicate-run",
            "duplicate-agent",
            "duplicate-slot",
            "duplicate-label",
            "invalid-label",
            "empty-label",
            "omitted",
            "foreign-run",
            "extra-child",
        ] {
            let mut altered = capture.clone();
            if fault == "extra-child" {
                let mut extra = altered.run_tree.runs[2].clone();
                extra.run_id = "extra-child".into();
                altered.run_tree.runs.push(extra);
            } else {
                let content = &mut altered.transcript.as_mut().unwrap().items[1].content;
                let mut receipt: Value = serde_json::from_str(content).unwrap();
                match fault {
                    "swapped-slots" => {
                        receipt["agents"][0]["slot_index"] = 1.into();
                        receipt["agents"][1]["slot_index"] = 0.into();
                    }
                    "duplicate-run" => receipt["agents"][1]["run_id"] = "child".into(),
                    "duplicate-agent" => receipt["agents"][1]["agent_id"] = "member".into(),
                    "duplicate-slot" => receipt["agents"][1]["slot_index"] = 0.into(),
                    "duplicate-label" => receipt["agents"][1]["id"] = "slot-0".into(),
                    "invalid-label" => receipt["agents"][1]["id"] = 1.into(),
                    "empty-label" => receipt["agents"][1]["id"] = "".into(),
                    "omitted" => {
                        receipt["agents"].as_array_mut().unwrap().pop();
                    }
                    "foreign-run" => receipt["agents"][1]["run_id"] = "other-parent-child".into(),
                    _ => unreachable!(),
                }
                *content = receipt.to_string();
            }
            assert_eq!(
                altered.proves_children("root", &expected, Some(&group)),
                Ok(false),
                "{fault}"
            );
        }
        let mut missing = capture.clone();
        missing.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert!(
            missing
                .proves_children("root", &expected, Some(&group))
                .is_err(),
            "successful collection and parent finalization cannot replace one missing adoption"
        );
        let mut contradictions = expected.clone();
        contradictions[0].initial_thinking =
            Some(crate::criteria::ChildThinkingExpectation::Exact {
                config: astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
            });
        contradictions[1].expected_result = Some(crate::criteria::ChildResultExpectation::Text(
            "wrong sibling output".into(),
        ));
        for expectations in [
            contradictions.clone(),
            [contradictions[1].clone(), contradictions[0].clone()],
        ] {
            assert_eq!(
                capture.proves_children("root", &expectations, Some(&group)),
                Ok(false),
                "missing thinking on one child cannot hide another child's wrong output"
            );
        }
        let mut natural = capture.clone();
        natural.run_tree.runs[2].runtime.model_name = Some("other-model".into());
        let natural_expected = [
            expectation("observed-value", "test-model", None),
            expectation("second-value", "other-model", None),
        ];
        assert_eq!(
            natural.proves_children("root", &natural_expected, None),
            Ok(true)
        );
        natural.run_tree.runs[2].runtime.model_name = Some("wrong-model".into());
        assert_eq!(
            natural.proves_children("root", &natural_expected, None),
            Ok(false)
        );
        assert!(!capture.awaiting_completion_evidence(Some("root")));
        capture.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"]
            .as_array_mut()
            .unwrap()
            .remove(2);
        assert!(capture.awaiting_completion_evidence(Some("root")));
        capture.run_tree.runs[2].status = SessionRunLifecycleStatus::Paused;
        assert!(!capture.awaiting_completion_evidence(Some("root")));
    }

    #[test]
    fn semantic_child_evidence_keeps_prose_custody_and_rejects_identity_gaps() {
        let group = FanoutGroupSelector::Id("group".into());
        let prose = "The child returned observed-value with an explanation.";
        let mut capture = capture();
        capture.transcript.as_mut().unwrap().items[0].content = prose.into();
        capture.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"][1]["children"][0]["result_sha256"] =
            serde_json::json!(format!("{:x}", Sha256::digest(prose.as_bytes())));
        let mut expected = expectation("ignored", "test-model", None);
        expected.expected_result = None;

        let evidence = capture
            .fixture_verified_children("root", &[expected.clone()], None)
            .expect("complete semantic custody evidence")
            .expect("valid semantic custody evidence");
        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].content, prose);
        assert_eq!(evidence[0].model, "test-model");
        assert_eq!(
            evidence[0].result_sha256,
            format!("{:x}", Sha256::digest(prose.as_bytes()))
        );
        let serialized = serde_json::to_value(&evidence).unwrap();
        assert_eq!(serialized[0]["content"], prose);

        let mut wrong_model = capture.clone();
        wrong_model.run_tree.runs[1].runtime.model_name = Some("wrong-model".into());
        assert!(matches!(
            wrong_model.fixture_verified_children("root", &[expected.clone()], None),
            Ok(None)
        ));

        let mut wrong_digest = capture.clone();
        wrong_digest.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"][1]["children"][0]["result_sha256"] =
            serde_json::json!("wrong");
        assert!(matches!(
            wrong_digest.fixture_verified_children("root", &[expected.clone()], None),
            Ok(None)
        ));

        let mut missing_output = capture.clone();
        missing_output.transcript.as_mut().unwrap().items.clear();
        assert!(
            missing_output
                .fixture_verified_children("root", std::slice::from_ref(&expected), None)
                .is_err()
        );

        let mut fanout = tool_capture();
        fanout.transcript.as_mut().unwrap().items[0].tool_calls[0].name = "agent_fanout".into();
        fanout.transcript.as_mut().unwrap().items[0].tool_calls[0].arguments =
            serde_json::json!({"action":"start", "group_id":"group"}).to_string();
        fanout.transcript.as_mut().unwrap().items[1]
            .tool_result
            .as_mut()
            .unwrap()
            .name = Some("agent_fanout".into());
        fanout.transcript.as_mut().unwrap().items[1].content = serde_json::json!({
            "status":"started", "group_id":"group", "title":"group",
            "fanout":{"group_id":"group", "title":"group", "parent_run_id":"root"},
            "work_unit_observation":{"id":"group", "kind":"agent_fanout"},
            "agents":[{"slot_index":0,"id":"slot-0","run_id":"child","agent_id":"member"}]
        })
        .to_string();
        let mut wrong_slot = expected;
        wrong_slot.slot_index = Some(1);
        assert!(matches!(
            fanout.fixture_verified_children("root", &[wrong_slot], Some(&group)),
            Ok(None)
        ));
    }

    #[test]
    fn tool_proof_reads_complete_scoped_trace_without_raw_tool_mirrors() {
        let mut snapshot = tool_capture();
        let trace = trace(&snapshot);
        // The raw API retains the child answer but contains no tool mirror.
        snapshot
            .transcript
            .as_mut()
            .unwrap()
            .items
            .retain(|item| item.role == "assistant" && item.tool_calls.is_empty());
        let calls = snapshot.tools("root", Some(&trace)).unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].arguments.as_ref().unwrap()["action"], "spawn");
        assert_eq!(calls[0].result.as_ref().unwrap()["value"], 42);
        assert_eq!(calls[0].raw_result(), "{\"value\":42}");
        assert!(snapshot.tools("root", None).is_err());
        for fault in [
            "foreign-session",
            "unknown-run",
            "missing",
            "dropped",
            "skipped",
            "conflict",
            "preview",
            "disposition",
            "count",
        ] {
            let mut trace = trace.clone();
            let mut snapshot = snapshot.clone();
            match fault {
                "foreign-session" => trace.session_id = "foreign".into(),
                "unknown-run" => trace.events[0].raw["producer_scope"]["run_id"] = "unknown".into(),
                "missing" => trace.events.clear(),
                "dropped" => trace.dropped_lines = 1,
                "skipped" => trace.skipped_lines = 1,
                "conflict" => trace.integrity_errors = 1,
                "preview" => {
                    trace.events[0].raw["tool_calls"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("result_full");
                }
                "disposition" => {
                    trace.events[0].raw["tool_calls"][0]["disposition"] = "unknown".into()
                }
                "count" => snapshot.run_tree.runs[0].total_tool_calls += 1,
                _ => unreachable!(),
            }
            assert!(snapshot.tools("root", Some(&trace)).is_err(), "{fault}");
        }
    }

    #[test]
    fn execution_counts_use_round_owners_instead_of_mixed_turn_summaries() {
        let mut snapshot = tool_capture();
        snapshot.run_tree.runs[1].total_tool_calls = 1;
        let mut trace = trace(&snapshot);
        let mut child = trace.events[0].clone();
        child.raw["producer_scope"]["run_id"] = "child".into();
        child.raw["tool_calls"][0]["tool_call_id"] = "child-call".into();
        child.raw["tool_calls"][0]["ok"] = false.into();
        child.raw["tool_calls"][0]["disposition"] = "rejected".into();
        let mut summary = trace.events[0].clone();
        summary.event_type = "turn".into();
        summary.raw["tool_calls"]
            .as_array_mut()
            .unwrap()
            .push(child.raw["tool_calls"][0].clone());
        // This real CLI shape includes child observations under the root
        // summary without independent per-call owner identity.
        trace.events.push(summary);
        trace.events.push(child);
        let calls = snapshot.tools("root", Some(&trace)).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].run_id.as_deref(), Some("child"));
        assert_eq!(calls[1].ok, Some(false));
        assert_eq!(
            calls[1].execution_disposition(),
            Some(ToolCallDisposition::Rejected)
        );
        assert_eq!(trace.journal_tool_calls().len(), 3);
        trace.events.pop();
        assert!(
            snapshot.tools("root", Some(&trace)).is_err(),
            "the turn summary cannot replace a missing child round"
        );
    }

    #[test]
    fn trace_scope_ignores_other_valid_roots_and_rejects_document_drift() {
        let mut snapshot = tool_capture();
        let mut other = snapshot.run_tree.runs[0].clone();
        other.run_id = "other-root".into();
        other.root_run_id = Some("other-root".into());
        other.status = SessionRunLifecycleStatus::Paused;
        snapshot.run_tree.runs.push(other);
        let mut trace = trace(&snapshot);
        let mut other = trace.events[0].clone();
        other.raw["producer_scope"]["run_id"] = "other-root".into();
        trace.events.push(other);
        assert_eq!(snapshot.tools("root", Some(&trace)).unwrap().len(), 1);
        let mut drift = trace.events[0].clone();
        // Equal parsed JSON cannot excuse conflicting exact producer bytes.
        drift.raw["tool_calls"][0]["args_full"] = "{ \"action\": \"spawn\" }".into();
        trace.events.push(drift);
        assert!(snapshot.tools("root", Some(&trace)).is_err());
    }

    #[test]
    fn trace_terminal_dispositions_and_plain_text_stay_distinct() {
        let snapshot = tool_capture();
        let original = trace(&snapshot);
        for disposition in ["reused", "suppressed", "deferred"] {
            let mut trace = original.clone();
            trace.events[0].raw["tool_calls"][0]["disposition"] = disposition.into();
            let calls = snapshot.tools("root", Some(&trace)).unwrap();
            assert_eq!(calls[0].ok, None, "{disposition}");
        }
        let mut unknown = original.clone();
        unknown.events[0].raw["tool_calls"][0]["ok"] = false.into();
        unknown.events[0].raw["tool_calls"][0]["error_kind"] = "tool_outcome_unknown".into();
        assert_eq!(snapshot.tools("root", Some(&unknown)).unwrap()[0].ok, None);
        let mut trace = original;
        trace.events[0].raw["tool_calls"][0]["disposition"] = "rejected".into();
        trace.events[0].raw["tool_calls"][0]["ok"] = false.into();
        trace.events[0].raw["tool_calls"][0]["result_full"] =
            "a complete rejection, not JSON".into();
        // A full rejection needs no generic executor-completed event.
        let calls = snapshot.tools("root", Some(&trace)).unwrap();
        assert_eq!(
            calls[0].execution_disposition(),
            Some(ToolCallDisposition::Rejected)
        );
        assert_eq!(calls[0].ok, Some(false));
        assert!(calls[0].result.is_none());
        assert_eq!(calls[0].raw_result(), "a complete rejection, not JSON");
    }

    #[test]
    fn child_adoption_uses_run_and_result_custody_without_request_diagnostics() {
        let mut observed = capture();
        observed.reflection.model_requests = Default::default();
        let expected = [expectation("observed-value", "test-model", None)];
        assert_eq!(observed.proves_children("root", &expected, None), Ok(true));
        observed.run_tree.runs[1].runtime.model_name = Some("wrong-model".into());
        assert_eq!(observed.proves_children("root", &expected, None), Ok(false));
    }

    #[test]
    fn child_custody_classifies_observed_contradictions_before_missing_evidence() {
        use crate::classify::FailureClass::{
            BehaviorContractViolation, InfraVerificationUnavailable,
        };
        let expected = vec![expectation("observed-value", "test-model", None)];
        for (fault, classification) in [
            ("text", BehaviorContractViolation),
            ("missing-text", InfraVerificationUnavailable),
            ("spawn-parent", BehaviorContractViolation),
            ("spawn-agent", BehaviorContractViolation),
            ("not-completed", BehaviorContractViolation),
            ("missing-adoption", InfraVerificationUnavailable),
        ] {
            let mut altered = capture();
            match fault {
                "text" => {
                    altered.transcript.as_mut().unwrap().items[0].content =
                        "wrong actual terminal".into()
                }
                "missing-text" => altered.transcript = None,
                "spawn-parent" | "spawn-agent" => {
                    let fact = &mut altered.reflection.graph_slice.nodes[0]
                        .metadata
                        .as_mut()
                        .unwrap()["execution_spine"]["facts"][0];
                    let field = if fault == "spawn-parent" {
                        "parent_run_id"
                    } else {
                        "agent_id"
                    };
                    fact[field] = "foreign".into();
                }
                "not-completed" => {
                    altered.run_tree.runs[1].status = SessionRunLifecycleStatus::Failed
                }
                "missing-adoption" => {
                    altered.reflection.graph_slice.nodes[0]
                        .metadata
                        .as_mut()
                        .unwrap()["execution_spine"]["facts"]
                        .as_array_mut()
                        .unwrap()
                        .remove(1);
                }
                _ => unreachable!(),
            }
            assert_eq!(
                proof_failure(altered, expected.clone(), None),
                Some(classification),
                "{fault}"
            );
        }
        let mut impossible = capture();
        let mut unknown = impossible.run_tree.runs[1].clone();
        unknown.run_id = "unknown-child".into();
        unknown.agent_id = Some("unknown-agent".into());
        unknown.runtime.model_name = None;
        impossible.run_tree.runs[1].runtime.model_name = Some("wrong-model".into());
        impossible.run_tree.runs.push(unknown);
        let two = [
            expectation("observed-value", "model-A", None),
            expectation("other", "model-B", None),
        ];
        for children in [two.to_vec(), vec![two[1].clone(), two[0].clone()]] {
            assert_eq!(
                proof_failure(impossible.clone(), children, None),
                Some(BehaviorContractViolation)
            );
        }
    }

    #[test]
    fn inherited_controls_and_contains_require_actual_root_and_complete_result_custody() {
        use crate::classify::FailureClass::{
            BehaviorContractViolation, InfraVerificationUnavailable,
        };
        use crate::criteria::{ChildResultExpectation, ChildThinkingExpectation};
        let mut original = capture();
        original.run_tree.runs[0].runtime.offering_id = Some("offer".into());
        original.run_projections = Some(
            ["root", "child"]
                .into_iter()
                .map(|run| {
                    serde_json::json!({
                        "session_id":"session", "run_id":run, "run_event_high_watermark":0,
                        "recent_events":[{"type":"run_started", "run_id":run, "index":0,
                            "generation_controls":{"thinking":{"mode":"adaptive","effort":"high"}}}]
                    })
                })
                .collect(),
        );
        let mut expected = expectation("observed-value", "test-model", None);
        expected.expected_result = Some(ChildResultExpectation::Contains("observed".into()));
        expected.initial_thinking = Some(ChildThinkingExpectation::SameAsRoot);
        assert_eq!(
            proof_failure(original.clone(), vec![expected.clone()], None),
            None
        );
        for (fault, classification) in [
            ("offering", BehaviorContractViolation),
            ("thinking", BehaviorContractViolation),
            ("root-missing", InfraVerificationUnavailable),
            ("root-duplicate", InfraVerificationUnavailable),
            ("needle-digest", BehaviorContractViolation),
            ("wrong-body", BehaviorContractViolation),
        ] {
            let mut altered = original.clone();
            match fault {
                "offering" => altered.run_tree.runs[0].runtime.offering_id = Some("other".into()),
                "thinking" => {
                    altered.run_projections.as_mut().unwrap()[1]["recent_events"][0]["generation_controls"]
                        ["thinking"]["effort"] = serde_json::json!("medium")
                }
                "root-missing" => {
                    altered.run_projections.as_mut().unwrap().remove(0);
                }
                "root-duplicate" => {
                    let root = altered.run_projections.as_ref().unwrap()[0].clone();
                    altered.run_projections.as_mut().unwrap().push(root);
                }
                "needle-digest" => {
                    altered.reflection.graph_slice.nodes[0]
                        .metadata
                        .as_mut()
                        .unwrap()["execution_spine"]["facts"][1]["children"][0]["result_sha256"] =
                        serde_json::json!(format!("{:x}", Sha256::digest(b"observed")))
                }
                "wrong-body" => {
                    altered.transcript.as_mut().unwrap().items[0].content = "different".into()
                }
                _ => unreachable!(),
            }
            assert_eq!(
                proof_failure(altered, vec![expected.clone()], None),
                Some(classification),
                "{fault}"
            );
        }
        expected.workspace_mutation =
            Some(astra_config::user_profile::WorkspaceMutationIntent::ReadOnly);
        original.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"][0]["workspace_mutation"] =
            serde_json::json!("read_only");
        assert_eq!(
            proof_failure(original.clone(), vec![expected.clone()], None),
            None
        );
        for (intent, class) in [
            (serde_json::json!("may_mutate"), BehaviorContractViolation),
            (Value::Null, InfraVerificationUnavailable),
        ] {
            original.reflection.graph_slice.nodes[0]
                .metadata
                .as_mut()
                .unwrap()["execution_spine"]["facts"][0]["workspace_mutation"] = intent;
            assert_eq!(
                proof_failure(original.clone(), vec![expected.clone()], None),
                Some(class)
            );
        }
    }

    #[test]
    fn child_proof_distinguishes_captured_root_lifecycle_from_invalid_tree() {
        for (status, reason) in [
            (SessionRunLifecycleStatus::Running, "root_running"),
            (SessionRunLifecycleStatus::Waiting, "root_waiting"),
            (SessionRunLifecycleStatus::Paused, "root_paused"),
            (SessionRunLifecycleStatus::Delegated, "root_delegated"),
            (SessionRunLifecycleStatus::Interrupted, "root_interrupted"),
            (SessionRunLifecycleStatus::Failed, "root_failed"),
            (SessionRunLifecycleStatus::Cancelled, "root_cancelled"),
        ] {
            let mut captured = capture();
            captured.run_tree.runs[0].status = status;
            for fault in ["none", "truncated", "duplicate", "lineage", "owner"] {
                let mut observed = captured.clone();
                match fault {
                    "truncated" => observed.run_tree.truncated = true,
                    "duplicate" => observed
                        .run_tree
                        .runs
                        .push(observed.run_tree.runs[0].clone()),
                    "lineage" => observed.run_tree.runs[1].depth = 9,
                    "owner" => observed.owner.account_id.clear(),
                    _ => {}
                }
                let mut outcome = crate::runner::RunOutcome::new("test-model").with_exit_code(0);
                outcome.session_id = Some("session".into());
                outcome.run_id = Some("root".into());
                let mut stream = crate::runner::StreamCapture::default();
                stream.identity_verified = true;
                stream.execution = Some(observed);
                outcome.stream_capture = Some(stream);
                let criterion = crate::criteria::Criterion::ExecutionChildResultsAdopted {
                    children: vec![expectation("observed-value", "test-model", None)],
                    fanout_group: None,
                };
                let failure = crate::criteria::execution_contract_proof(&criterion, &outcome, None)
                    .unwrap_err();
                assert_eq!(
                    failure.diagnostic(),
                    serde_json::json!({
                        "kind":"child_adoption", "status":"unavailable",
                        "reason": if fault == "none" { reason } else { "run_tree_unavailable" }
                    }),
                    "{status:?}/{fault}"
                );
                let results = crate::criteria::evaluate_deterministic(&[criterion], &outcome);
                assert!(!results[0].passed);
                assert_eq!(
                    crate::classify::classify(&outcome, &results),
                    crate::classify::FailureClass::InfraVerificationUnavailable
                );
            }
        }
    }

    #[test]
    fn question_completion_requires_correlated_receipts_and_both_durable_observers() {
        use crate::classify::FailureClass::{
            BehaviorContractViolation, InfraVerificationUnavailable,
        };
        let mut original = capture();
        original.run_tree.runs[0].total_tool_calls = 1;
        original.run_tree.runs[1].total_tool_calls = 1;
        let mut terminal = original.transcript.as_mut().unwrap().items.pop().unwrap();
        terminal.item_seq = 9;
        let party =
            serde_json::json!({"run_id":"stable-parent-mailbox", "agent_id":"parent-agent"});
        let mut items = Vec::new();
        for (run, id, seq, args, receipt) in [
            (
                "child",
                "question-call",
                1,
                serde_json::json!({"action":"send_message","message_type":"question","to":"parent"}),
                serde_json::json!({"run_id":"child","status":"queued","message_id":"question-id",
                    "reply_obligation":{"request_id":"question-id","expected_responder":party}}),
            ),
            (
                "root",
                "answer-call",
                3,
                serde_json::json!({"action":"send_message","message_type":"answer","to":"member","request_id":"question-id"}),
                serde_json::json!({"run_id":"root","status":"queued","message_id":"answer-id"}),
            ),
        ] {
            for (offset, role, calls, result, content) in [
                (
                    0,
                    "assistant",
                    serde_json::json!([{"tool_use_id":id,"name":"agent","arguments":args.to_string()}]),
                    Value::Null,
                    String::new(),
                ),
                (
                    1,
                    "tool",
                    serde_json::json!([]),
                    serde_json::json!({"tool_use_id":id,"name":"agent","status":"completed"}),
                    receipt.to_string(),
                ),
            ] {
                items.push(serde_json::from_value(serde_json::json!({
                    "session_id":"session","run_id":run,"item_seq":seq+offset,"role":role,
                    "content":content,"tool_calls":calls,"tool_result":result,
                    "source_event_id":format!("{id}-{offset}"),"created_at":"2026-10-07T00:00:00Z"
                })).unwrap());
            }
        }
        for (run, agent, direction, seq) in [
            ("child", "member", "sent", 5),
            ("child", "member", "received", 6),
        ] {
            items.push(serde_json::from_value(serde_json::json!({
                "session_id":"session","run_id":run,"item_seq":seq,"role":"event",
                "content":"","tool_calls":[],"tool_result":null,
                "source_event_id":format!("message-{direction}"),"created_at":"2026-10-07T00:00:00Z",
                "evidence":{"kind":"agent_communication","event":{
                    "schema_version":astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION,
                    "observed_by":{"run_id":run,"agent_id":agent},"direction":direction,
                    "message_id":if direction=="sent" {"question-id"} else {"answer-id"},
                    "from":if direction=="sent" {serde_json::json!({"run_id":"child","agent_id":"member"})} else {party.clone()},
                    "to":{"kind":"direct","address":if direction=="sent" {party.clone()} else {serde_json::json!({"run_id":"child","agent_id":"member"})}},
                    "payload_kind":if direction=="sent" {"request"} else {"response"},
                    "response_accepted":if direction=="sent" {None} else {Some(true)},
                    "related_message_id":if direction=="sent" {None} else {Some("question-id")},"timestamp_ms":1
                }}
            })).unwrap());
        }
        items.push(terminal);
        original.transcript.as_mut().unwrap().items = items;
        let mut expected = expectation("observed-value", "test-model", None);
        expected.answered_question = true;
        assert_eq!(
            proof_failure(original.clone(), vec![expected.clone()], None),
            None
        );
        for recipient in [
            "parent",
            " PARENT ",
            "orchestrator",
            " OrChEsTrAtOr ",
            "root",
        ] {
            let mut addressed = original.clone();
            addressed.transcript.as_mut().unwrap().items[0].tool_calls[0].arguments =
                serde_json::json!({"action":"send_message","message_type":"question","to":recipient}).to_string();
            if recipient == "root" {
                // Direct addressing uses a registered parent execution, rather
                // than the separate stable-mailbox fixture used for aliases.
                let page = addressed.transcript.as_mut().unwrap();
                let mut receipt: Value = serde_json::from_str(&page.items[1].content).unwrap();
                receipt["reply_obligation"]["expected_responder"]["run_id"] = "root".into();
                page.items[1].content = receipt.to_string();
                for item in &mut page.items[5..6] {
                    let Some(astra_turn_types::AgentTranscriptEvidence::AgentCommunication {
                        event,
                    }) = &mut item.evidence
                    else {
                        unreachable!()
                    };
                    event.from.run_id = "root".into();
                }
                let Some(astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event }) =
                    &mut page.items[4].evidence
                else {
                    unreachable!()
                };
                let astra_turn_types::AgentCommunicationTarget::Direct { address } = &mut event.to
                else {
                    unreachable!()
                };
                address.run_id = "root".into();
            }
            assert_eq!(
                proof_failure(addressed, vec![expected.clone()], None),
                None,
                "{recipient}"
            );
        }
        for recipient in ["peer", "parent"] {
            let mut additional = original.clone();
            additional.run_tree.runs[1].total_tool_calls += 1;
            let page = additional.transcript.as_mut().unwrap();
            let mut request = page.items[0].clone();
            request.item_seq = 7;
            request.source_event_id = Some("second-question-request".into());
            request.tool_calls[0].tool_use_id = "second-question-call".into();
            request.tool_calls[0].arguments = serde_json::json!({"action":"send_message","message_type":"question","to":recipient}).to_string();
            let mut result = page.items[1].clone();
            result.item_seq = 8;
            result.source_event_id = Some("second-question-result".into());
            result.tool_result.as_mut().unwrap().tool_use_id = "second-question-call".into();
            let mut receipt: Value = serde_json::from_str(&result.content).unwrap();
            receipt["message_id"] = "second-question".into();
            receipt["reply_obligation"]["request_id"] = "second-question".into();
            if recipient == "peer" {
                receipt["reply_obligation"]["expected_responder"] =
                    serde_json::json!({"run_id":"peer","agent_id":"peer"});
            }
            result.content = receipt.to_string();
            page.items.splice(6..6, [request, result]);
            assert_eq!(
                proof_failure(additional, vec![expected.clone()], None),
                // A different request is not part of the verified reply chain.
                None,
                "additional {recipient}"
            );
        }
        let mut custom_root = original.clone();
        let mut sent = custom_root.transcript.as_ref().unwrap().items[5].clone();
        sent.run_id = Some("root".into());
        sent.source_event_id = Some("optional-root-send".into());
        sent.item_seq = 7;
        let Some(astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event }) =
            &mut sent.evidence
        else {
            unreachable!()
        };
        event.direction = astra_turn_types::AgentCommunicationDirection::Sent;
        event.observed_by.run_id = "root".into();
        // A custom execution profile is distinct from its stable mailbox agent.
        event.observed_by.agent_id = "planner".into();
        custom_root
            .transcript
            .as_mut()
            .unwrap()
            .items
            .insert(6, sent);
        assert_eq!(
            proof_failure(custom_root.clone(), vec![expected.clone()], None),
            None
        );
        let Some(astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event }) =
            &mut custom_root.transcript.as_mut().unwrap().items[6].evidence
        else {
            unreachable!()
        };
        event.from.agent_id = "conflicting-parent".into();
        assert_eq!(
            proof_failure(custom_root, vec![expected.clone()], None),
            Some(BehaviorContractViolation)
        );
        for before in [false, true] {
            let mut with_rejected_peer = original.clone();
            with_rejected_peer.run_tree.runs[1].total_tool_calls += 1;
            let page = with_rejected_peer.transcript.as_mut().unwrap();
            let mut request = page.items[0].clone();
            let mut result = page.items[1].clone();
            if before {
                for item in &mut page.items {
                    item.item_seq += 2;
                }
            }
            request.item_seq = if before { 1 } else { 7 };
            result.item_seq = request.item_seq + 1;
            request.source_event_id = Some("rejected-peer-request".into());
            result.source_event_id = Some("rejected-peer-result".into());
            request.tool_calls[0].tool_use_id = "rejected-peer-call".into();
            request.tool_calls[0].arguments =
                r#"{"action":"send_message","message_type":"question","to":"peer"}"#.into();
            result.tool_result.as_mut().unwrap().tool_use_id = "rejected-peer-call".into();
            result.content = serde_json::json!({"result_family":"control_receipt","action":"send_message","success":false,"executed":false,"status":"rejected","reason":"no peer"}).to_string();
            page.items
                .splice(if before { 0..0 } else { 6..6 }, [request, result]);
            assert_eq!(
                proof_failure(with_rejected_peer, vec![expected.clone()], None),
                None,
                "rejected peer before={before}"
            );
        }
        let mut run_addressed = original.clone();
        run_addressed.transcript.as_mut().unwrap().items[2].tool_calls[0].arguments =
            r#"{"action":"send_message","message_type":"answer","to":"child","request_id":"question-id"}"#.into();
        assert_eq!(
            proof_failure(run_addressed, vec![expected.clone()], None),
            None
        );
        for reverse in [false, true] {
            let mut mixed = original.clone();
            mixed.transcript.as_mut().unwrap().items[4].source_event_id = None;
            let evidence = mixed.transcript.as_mut().unwrap().items[5]
                .evidence
                .as_mut()
                .unwrap();
            let astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event } = evidence
            else {
                unreachable!()
            };
            event.related_message_id = Some("wrong-question".into());
            if reverse {
                mixed.transcript.as_mut().unwrap().items.swap(4, 5);
            }
            assert_eq!(
                proof_failure(mixed, vec![expected.clone()], None),
                Some(BehaviorContractViolation),
                "reverse={reverse}"
            );
        }
        for (fault, class) in [
            ("queued-only", InfraVerificationUnavailable),
            ("sent-only", InfraVerificationUnavailable),
            ("missing-provenance", InfraVerificationUnavailable),
            ("wrong-schema", InfraVerificationUnavailable),
            ("wrong-message", InfraVerificationUnavailable),
            ("rejected-answer", BehaviorContractViolation),
            ("unknown-answer", InfraVerificationUnavailable),
            ("duplicate-question", BehaviorContractViolation),
            ("duplicate-answer", BehaviorContractViolation),
            ("accepted-false", BehaviorContractViolation),
            ("wrong-question-sender", BehaviorContractViolation),
            ("wrong-request", BehaviorContractViolation),
            ("wrong-sender", BehaviorContractViolation),
            ("wrong-receiver", BehaviorContractViolation),
            ("unfinished", BehaviorContractViolation),
            ("reverse-question", BehaviorContractViolation),
        ] {
            let mut altered = original.clone();
            if fault == "queued-only" {
                altered
                    .transcript
                    .as_mut()
                    .unwrap()
                    .items
                    .retain(|item| item.role != "event");
            } else if matches!(fault, "duplicate-question" | "duplicate-answer") {
                let (index, run) = if fault == "duplicate-question" {
                    (0, 1)
                } else {
                    (2, 0)
                };
                altered.run_tree.runs[run].total_tool_calls += 1;
                let page = altered.transcript.as_mut().unwrap();
                let mut request = page.items[index].clone();
                let mut result = page.items[index + 1].clone();
                request.item_seq = 7;
                result.item_seq = 8;
                request.source_event_id = Some("duplicate-chain-request".into());
                result.source_event_id = Some("duplicate-chain-result".into());
                request.tool_calls[0].tool_use_id = "duplicate-chain-call".into();
                result.tool_result.as_mut().unwrap().tool_use_id = "duplicate-chain-call".into();
                page.items.splice(6..6, [request, result]);
            } else if matches!(fault, "rejected-answer" | "unknown-answer") {
                altered.transcript.as_mut().unwrap().items[3]
                    .tool_result
                    .as_mut()
                    .unwrap()
                    .status = (fault == "rejected-answer").then(|| "rejected".into());
            } else if fault == "unfinished" {
                altered.run_tree.runs[1].status = SessionRunLifecycleStatus::Running;
            } else if fault == "reverse-question" {
                altered.transcript.as_mut().unwrap().items[0].tool_calls[0].arguments =
                    r#"{"action":"send_message","message_type":"question","to":"member"}"#.into();
                let item = &mut altered.transcript.as_mut().unwrap().items[1];
                let mut receipt: Value = serde_json::from_str(&item.content).unwrap();
                receipt["reply_obligation"]["expected_responder"] =
                    serde_json::json!({"run_id":"peer","agent_id":"peer"});
                item.content = receipt.to_string();
            } else {
                let item = &mut altered.transcript.as_mut().unwrap().items[if fault
                    == "wrong-question-sender"
                {
                    4
                } else {
                    5
                }];
                if fault == "missing-provenance" {
                    item.source_event_id = None;
                }
                let evidence = item.evidence.as_mut().unwrap();
                let astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event } =
                    evidence
                else {
                    unreachable!()
                };
                match fault {
                    "missing-provenance" => {}
                    "wrong-schema" => event.schema_version = "unknown-version".into(),
                    "wrong-message" => event.message_id = "different-answer".into(),
                    "accepted-false" => event.response_accepted = Some(false),
                    "wrong-question-sender" => event.from.agent_id = "peer".into(),
                    "sent-only" => {
                        event.direction = astra_turn_types::AgentCommunicationDirection::Sent;
                        event.observed_by.run_id = "root".into();
                        event.observed_by.agent_id = "parent-agent".into();
                        item.run_id = Some("root".into());
                    }
                    "wrong-request" => event.related_message_id = Some("other-question".into()),
                    "wrong-sender" => event.from.agent_id = "other-parent".into(),
                    "wrong-receiver" => {
                        event.to = astra_turn_types::AgentCommunicationTarget::Parent
                    }
                    _ => unreachable!(),
                }
            }
            assert_eq!(
                proof_failure(altered, vec![expected.clone()], None),
                Some(class),
                "{fault}"
            );
        }
    }

    #[test]
    fn child_logical_rounds_use_exact_owner_terminal_not_physical_attempt_counts() {
        use crate::classify::FailureClass::{
            BehaviorContractViolation, InfraVerificationUnavailable,
        };
        let mut original = capture();
        original.run_projections = Some(vec![serde_json::json!({
            "session_id":"session","run_id":"child","run_event_high_watermark":5,
            "recent_events":[{"type":"run_finished","run_id":"child","index":5,
                "status":"completed","owner_generation":2,"turn_evaluation":{
                    "session_id":"session",
                    "producer_scope":{"run_id":"child"},"metadata":{
                        "run_status":"completed","execution_owner_generation":2,"llm_rounds":1
                    }
                }}]
        })]);
        let mut expected = expectation("observed-value", "test-model", None);
        expected.logical_rounds = Some(1);
        assert!(crate::criteria::requires_execution_run_events(&[
            crate::criteria::Criterion::ExecutionChildResultsAdopted {
                children: vec![expected.clone()],
                fanout_group: None,
            }
        ]));
        assert_eq!(
            proof_failure(original.clone(), vec![expected.clone()], None),
            None
        );
        for (path, value, classification) in [
            (
                "/recent_events/0/turn_evaluation/metadata/llm_rounds",
                serde_json::json!(2),
                BehaviorContractViolation,
            ),
            (
                "/recent_events/0/turn_evaluation/metadata/llm_rounds",
                Value::Null,
                InfraVerificationUnavailable,
            ),
            (
                "/recent_events/0/turn_evaluation/metadata/llm_rounds",
                serde_json::json!(4294967296u64),
                InfraVerificationUnavailable,
            ),
            (
                "/recent_events/0/turn_evaluation/producer_scope/run_id",
                serde_json::json!("foreign"),
                InfraVerificationUnavailable,
            ),
            (
                "/recent_events/0/turn_evaluation/session_id",
                serde_json::json!("foreign-session"),
                InfraVerificationUnavailable,
            ),
            (
                "/recent_events/0/turn_evaluation/metadata/execution_owner_generation",
                serde_json::json!(3),
                InfraVerificationUnavailable,
            ),
        ] {
            let mut altered = original.clone();
            *altered.run_projections.as_mut().unwrap()[0]
                .pointer_mut(path)
                .unwrap() = value;
            assert_eq!(
                proof_failure(altered, vec![expected.clone()], None),
                Some(classification),
                "{path}"
            );
        }
    }

    #[test]
    fn child_terminal_content_requires_exact_complete_unambiguous_identity() {
        let mut capture = tool_capture();
        let mut terminal = capture.transcript.as_ref().unwrap().items[0].clone();
        terminal.item_seq = 3;
        terminal.run_id = Some("child".into());
        terminal.tool_calls.clear();
        terminal.content = r#"{"value": 42}"#.into();
        terminal.source_event_id = Some("child-terminal".into());
        capture.transcript.as_mut().unwrap().items[2] = terminal.clone();
        assert_eq!(
            capture.run_terminal_content("root", "child").unwrap(),
            Some(terminal.content.as_str())
        );
        terminal.content = "x".repeat(4097);
        capture.transcript.as_mut().unwrap().items[2] = terminal.clone();
        assert_eq!(
            capture.run_terminal_content("root", "child").unwrap(),
            Some(terminal.content.as_str())
        );
        let mut siblings = capture.clone();
        let mut sibling = siblings.run_tree.runs[1].clone();
        sibling.run_id = "sibling".into();
        sibling.agent_id = Some("sibling-agent".into());
        siblings.run_tree.runs.push(sibling);
        let mut sibling_terminal = terminal.clone();
        sibling_terminal.item_seq = 4;
        sibling_terminal.run_id = Some("sibling".into());
        sibling_terminal.source_event_id = Some("sibling-terminal".into());
        sibling_terminal.content = "different sibling output".into();
        siblings
            .transcript
            .as_mut()
            .unwrap()
            .items
            .push(sibling_terminal);
        assert_eq!(
            siblings.run_terminal_content("root", "child").unwrap(),
            Some(terminal.content.as_str())
        );
        assert_eq!(
            siblings.run_terminal_content("root", "sibling").unwrap(),
            Some("different sibling output")
        );
        assert!(siblings.run_terminal_content("root", "root").is_err());
        assert!(siblings.run_terminal_content("root", "unknown").is_err());
        for change in ["foreign", "missing", "duplicate", "unidentified", "partial"] {
            let mut altered = capture.clone();
            let page = altered.transcript.as_mut().unwrap();
            match change {
                "foreign" => page.items[2].run_id = Some("foreign-run".into()),
                "missing" => {
                    page.items.pop();
                }
                "duplicate" => {
                    let mut duplicate = terminal.clone();
                    duplicate.item_seq = 4;
                    duplicate.source_event_id = Some("another-terminal".into());
                    page.items.push(duplicate);
                }
                "unidentified" => page.items[2].source_event_id = None,
                "partial" => page.has_more = true,
                _ => unreachable!(),
            }
            assert!(
                altered.run_terminal_content("root", "child").is_err(),
                "{change}"
            );
        }
    }

    #[test]
    fn canonical_child_oracle_requires_exact_identity_execution_and_ordered_adoption() {
        let original = capture();
        assert!(singleton_proven(
            &original,
            Some("root"),
            "observed-value",
            "test-model"
        ));
        let mut bounded = original.clone();
        bounded.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        assert!(
            singleton_proven(&bounded, Some("root"), "observed-value", "test-model"),
            "bounded event history is not the launch authority"
        );
        assert!(!original.awaiting_completion_evidence(Some("root")));
        let mut joined = original.clone();
        joined.reflection.graph_slice.nodes[0]
            .metadata
            .as_mut()
            .unwrap()["execution_spine"]["facts"] = serde_json::json!([]);
        assert!(joined.awaiting_completion_evidence(Some("root")));
        assert!(!singleton_proven(
            &joined,
            Some("root"),
            "observed-value",
            "test-model"
        ));
        assert!(!singleton_proven(
            &original,
            None,
            "observed-value",
            "test-model"
        ));
        assert!(!singleton_proven(
            &original,
            Some("foreign-root"),
            "observed-value",
            "test-model"
        ));
        assert!(!singleton_proven(
            &original,
            Some("root"),
            "guessed-value",
            "test-model"
        ));
        assert!(!singleton_proven(
            &original,
            Some("root"),
            "observed-value",
            "foreign-model"
        ));
        let baseline = serde_json::to_value(&original).unwrap();
        for (pointer, replacement) in [
            ("/schema_version", serde_json::json!(0)),
            ("/run_tree/session_id", serde_json::json!("foreign")),
            ("/run_tree/truncated", serde_json::json!(true)),
            (
                "/run_tree/runs/1/parent_run_id",
                serde_json::json!("foreign"),
            ),
            ("/run_tree/runs/1/status", serde_json::json!("paused")),
            ("/run_tree/runs/1/agent_id", serde_json::json!("foreign")),
            (
                "/run_tree/runs/1/runtime/model_name",
                serde_json::json!("foreign"),
            ),
            (
                "/run_tree/runs/1/runtime/offering_id",
                serde_json::Value::Null,
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/0/parent_run_id",
                serde_json::json!("foreign"),
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/1/metadata_available",
                serde_json::json!(false),
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/1/children/0/result_truncated",
                serde_json::json!(true),
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/1/children/0/run_id",
                serde_json::json!("foreign"),
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/1/created_at",
                serde_json::json!("2026-10-07T00:00:02.000000"),
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/1/created_at",
                serde_json::json!("2026-10-07T00:00:03.000000"),
            ),
            (
                "/reflection/graph_slice/nodes/0/metadata/execution_spine/facts/2/outcome",
                serde_json::json!("finalization_incomplete"),
            ),
        ] {
            let mut altered = baseline.clone();
            *altered.pointer_mut(pointer).unwrap() = replacement;
            let altered: SessionExecutionCapture = serde_json::from_value(altered).unwrap();
            assert!(
                !singleton_proven(&altered, Some("root"), "observed-value", "test-model"),
                "accepted {pointer}"
            );
            if pointer.ends_with("/outcome") {
                assert!(
                    !altered.awaiting_completion_evidence(Some("root")),
                    "terminal failure must not wait for success"
                );
            }
        }
        let mut foreign_owner = original.owner.clone();
        foreign_owner.account_id = "other-account".into();
        assert!(!original.matches("session", &foreign_owner));
        assert!(!original.matches("foreign-session", &original.owner));
    }
}
