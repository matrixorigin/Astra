//! Bounded, owner-bound Server evidence acquired before suite cleanup.

use astra_server_types::{SessionRunLifecycleStatus, SessionRunTreeSnapshot};
use astra_services::reflect::ReflectReport;
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

/// Joined canonical transcript evidence, not a reconstructed journal event.
pub(crate) struct ExecutionToolCall<'a> {
    pub run_id: &'a str,
    pub name: &'a str,
    pub arguments: Option<Value>,
    pub result: Option<Value>,
    pub raw_result: &'a str,
    pub ok: Option<bool>,
}

impl SessionExecutionCapture {
    fn validated_runs(
        &self,
        root: &str,
    ) -> Result<std::collections::BTreeMap<&str, &astra_server_types::SessionRunNode>, &'static str>
    {
        if !self.matches(&self.session_id, &self.owner) || self.run_tree.truncated {
            return Err("execution identity or complete run tree unavailable");
        }
        let runs: std::collections::BTreeMap<_, _> = self
            .run_tree
            .runs
            .iter()
            .map(|run| (run.run_id.as_str(), run))
            .collect();
        if runs.len() != self.run_tree.runs.len()
            || !runs.get(root).is_some_and(|run| {
                run.parent_run_id.is_none() && run.status == SessionRunLifecycleStatus::Completed
            })
        {
            return Err("root or unique run identities unavailable");
        }
        for run in runs.values() {
            if let Some(parent) = run.parent_run_id.as_deref() {
                let parent = runs
                    .get(parent)
                    .ok_or("run parent missing from complete tree")?;
                if parent.depth.checked_add(1) != Some(run.depth)
                    || run.root_run_id != parent.root_run_id
                {
                    return Err("run lineage conflict");
                }
            } else if run.depth != 0 || run.root_run_id.as_deref() != Some(run.run_id.as_str()) {
                return Err("root lineage conflict");
            }
        }
        Ok(runs)
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
        let runs = self.validated_runs(root)?;
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
        let runs = self.validated_runs(root)?;
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
                    || item.content.len() > 4096
                    || item.source_event_id.as_deref().is_none_or(str::is_empty)
                    || content.replace(item.content.as_str()).is_some())
            {
                return Err("child terminal content unavailable or ambiguous");
            }
        }
        content.map(Some).ok_or("child terminal content missing")
    }

    pub(crate) fn tools(&self, root: &str) -> Result<Vec<ExecutionToolCall<'_>>, &'static str> {
        let runs = self.validated_runs(root)?;
        let page = self.transcript.as_ref().ok_or("transcript not captured")?;
        if page.session_id != self.session_id || page.has_more {
            return Err("complete transcript unavailable");
        }
        let mut requests = std::collections::BTreeMap::new();
        let mut results = std::collections::BTreeMap::new();
        let mut sequences = std::collections::BTreeSet::new();
        for item in &page.items {
            if item.session_id != self.session_id || !sequences.insert(item.item_seq) {
                return Err("transcript identity conflict");
            }
            if item.tool_calls.is_empty() && item.tool_result.is_none() {
                continue;
            }
            let run = item
                .run_id
                .as_deref()
                .ok_or("tool run identity unavailable")?;
            let owner = runs.get(run).ok_or("tool outside authoritative run tree")?;
            if run != root && owner.root_run_id.as_deref() != Some(root) {
                continue;
            }
            if item.source_event_id.as_deref().is_none_or(str::is_empty) {
                return Err("tool producer identity unavailable");
            }
            for call in &item.tool_calls {
                if item.role != "assistant"
                    || call.tool_use_id.is_empty()
                    || call.name.is_empty()
                    || requests
                        .insert((run, call.tool_use_id.as_str()), (item.item_seq, call))
                        .is_some()
                {
                    return Err("tool request identity conflict");
                }
            }
            if let Some(result) = &item.tool_result
                && (item.role != "tool"
                    || result.tool_use_id.is_empty()
                    || results
                        .insert((run, result.tool_use_id.as_str()), (item, result))
                        .is_some())
            {
                return Err("tool result identity conflict");
            }
        }
        if requests.len() != results.len() {
            return Err("tool request/result coverage incomplete");
        }
        let mut ordered = Vec::new();
        for ((run, call_id), (seq, call)) in requests {
            let (item, result) = results.get(&(run, call_id)).ok_or("tool result missing")?;
            if item.item_seq <= seq || result.name.as_deref() != Some(call.name.as_str()) {
                return Err("tool result is not bound to its request");
            }
            // The transcript producer uses ToolCallDisposition, not the Edge
            // callback vocabulary. Non-executed dispositions do not carry ok.
            let ok = match result.status.as_deref() {
                Some("completed") => Some(true),
                Some("failed" | "rejected") => Some(false),
                Some("reused" | "suppressed" | "deferred") => None,
                _ => return Err("tool terminal disposition unavailable"),
            };
            ordered.push((
                seq,
                ExecutionToolCall {
                    run_id: run,
                    name: &call.name,
                    arguments: serde_json::from_str(&call.arguments).ok(),
                    result: serde_json::from_str(&item.content).ok(),
                    raw_result: &item.content,
                    ok,
                },
            ));
        }
        for run in runs
            .values()
            .filter(|run| run.run_id == root || run.root_run_id.as_deref() == Some(root))
        {
            if ordered
                .iter()
                .filter(|(_, call)| call.run_id == run.run_id)
                .count()
                != run.total_tool_calls as usize
            {
                return Err("transcript tool count disagrees with authoritative run");
            }
        }
        ordered.sort_by_key(|(seq, _)| *seq);
        Ok(ordered.into_iter().map(|(_, call)| call).collect())
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
    /// cannot establish the required direct-child cardinality; missing request
    /// groups or boundary facts cannot establish execution or adoption.
    pub(crate) fn proves_children(
        &self,
        root: &str,
        expected: &[crate::criteria::ChildExecutionExpectation],
        fanout_group: Option<&str>,
    ) -> Result<bool, &'static str> {
        use crate::criteria::{ChildResultExpectation, ChildThinkingExpectation};
        let runs = self.validated_runs(root)?;
        let children: Vec<_> = runs
            .values()
            .filter(|child| child.parent_run_id.as_deref() == Some(root))
            .copied()
            .collect();
        if children.len() != expected.len() || expected.is_empty() {
            return Ok(false);
        }
        if children
            .iter()
            .any(|child| child.status != SessionRunLifecycleStatus::Completed)
        {
            return Ok(false);
        }
        if self
            .reflection
            .model_requests
            .terminal
            .as_ref()
            .is_some_and(|summary| {
                summary.groups.iter().any(|group| {
                    children.iter().any(|child| {
                        group.run_id.as_deref() == Some(child.run_id.as_str())
                            && (group
                                .parent_run_id
                                .as_deref()
                                .is_some_and(|parent| parent != root)
                                || group.agent_id.as_deref().is_some_and(|agent| {
                                    child
                                        .agent_id
                                        .as_deref()
                                        .is_some_and(|actual| actual != agent)
                                }))
                    })
                })
            })
        {
            return Ok(false);
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
                    return Ok(false);
                };
                *count -= 1;
            }
        }
        let mut slots = std::collections::BTreeMap::new();
        if let Some(group) = fanout_group {
            let tools = self.tools(root)?;
            let mut starts = tools.iter().filter(|call| {
                call.run_id == root
                    && call.name == "agent_fanout"
                    && call
                        .arguments
                        .as_ref()
                        .is_some_and(|args| args["action"] == "start" && args["group_id"] == group)
            });
            let Some(start) = starts.next() else {
                return Ok(false);
            };
            if starts.next().is_some() || start.ok != Some(true) {
                return Ok(false);
            }
            let receipt = start.result.as_ref().ok_or("fanout receipt unavailable")?;
            if receipt["group_id"] != group {
                return Ok(false);
            }
            let agents = receipt["agents"]
                .as_array()
                .ok_or("fanout child identities unavailable")?;
            if agents.len() != children.len() {
                return Ok(false);
            }
            let mut returned_runs = std::collections::BTreeSet::new();
            let mut returned_agents = std::collections::BTreeSet::new();
            let mut returned_labels = std::collections::BTreeSet::new();
            for entry in agents {
                let slot = entry["slot_index"]
                    .as_u64()
                    .and_then(|slot| u32::try_from(slot).ok())
                    .ok_or("fanout slot identity unavailable")?;
                let run_id = entry["run_id"]
                    .as_str()
                    .ok_or("fanout run identity unavailable")?;
                let agent_id = entry["agent_id"]
                    .as_str()
                    .ok_or("fanout agent identity unavailable")?;
                if !entry["id"].is_null()
                    && entry["id"]
                        .as_str()
                        .is_none_or(|label| label.is_empty() || !returned_labels.insert(label))
                {
                    return Ok(false);
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
                    return Ok(false);
                }
            }
            if returned_runs != children.iter().map(|child| child.run_id.as_str()).collect() {
                return Ok(false);
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
            return Ok(false);
        }
        let mut selected = std::collections::BTreeSet::new();
        let mut latest_adoption = None;
        let mut unavailable = None;
        for expectation in expected {
            let child = if fanout_group.is_some() {
                let Some(run_id) = expectation.slot_index.and_then(|slot| slots.get(&slot)) else {
                    return Ok(false);
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
                        unavailable = Some("child model identity unavailable");
                        continue;
                    }
                    return Ok(false);
                };
                if matching.next().is_some() {
                    return Ok(false);
                }
                *child
            };
            if !selected.insert(child.run_id.as_str())
                || child
                    .runtime
                    .model_name
                    .as_deref()
                    .is_some_and(|model| model != expectation.model)
            {
                return Ok(false);
            }
            if child.runtime.model_name.is_none() {
                unavailable = Some("child model identity unavailable");
            }
            if let Some(thinking) = &expectation.initial_thinking {
                let expected = match thinking {
                    ChildThinkingExpectation::Exact { config } => Ok(config.clone()),
                    ChildThinkingExpectation::SameAsRoot => {
                        match (&runs[root].runtime.offering_id, &child.runtime.offering_id) {
                            (Some(parent), Some(current)) if parent != current => return Ok(false),
                            (None, _) | (_, None) => {
                                unavailable = Some("inherited offering identity unavailable");
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
                    Ok(false) => return Ok(false),
                    Err(reason) => unavailable = Some(reason),
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
                            return Ok(false);
                        }
                        observed = true;
                    }
                }
                if !observed {
                    unavailable = Some("recorded child mutation intent unavailable");
                }
            }
            if expectation.answered_question {
                match self.proves_answered_question(root, child) {
                    Ok(false) => return Ok(false),
                    Err(reason) => unavailable = Some(reason),
                    _ => {}
                }
            }
            if let Some(rounds) = expectation.logical_rounds {
                match self.run_logical_rounds(root, &child.run_id) {
                    Ok(actual) if actual != rounds => return Ok(false),
                    Err(reason) => unavailable = Some(reason),
                    _ => {}
                }
            }
            let content = match self.run_terminal_content(root, &child.run_id) {
                Ok(Some(raw)) => raw,
                Ok(None) => return Ok(false),
                Err(reason) => {
                    unavailable = Some(reason);
                    continue;
                }
            };
            let result_matches = match &expectation.expected_result {
                ChildResultExpectation::Text(text) => content == text,
                ChildResultExpectation::Contains(needle) => content.contains(needle),
                ChildResultExpectation::Json(value) => {
                    serde_json::from_str::<Value>(content).ok().as_ref() == Some(value)
                }
            };
            if !result_matches {
                return Ok(false);
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
                return Ok(false);
            }
            match self.child_adoption_time(root, child, &digest, &expectation.model, &facts) {
                Ok(time) => {
                    latest_adoption = Some(
                        latest_adoption
                            .map_or(time, |latest: chrono::NaiveDateTime| latest.max(time)),
                    )
                }
                Err(reason) => unavailable = Some(reason),
            }
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
            return Ok(false);
        }
        if let Some(reason) = unavailable {
            return Err(reason);
        }
        let latest = latest_adoption.ok_or("child adoption unavailable")?;
        if finalizations.is_empty() {
            return Err("parent finalization unavailable");
        }
        Ok(finalizations.iter().any(|time| *time > latest))
    }

    /// A queued answer is not observation. Exact durable communication plus
    /// Completed closes the existing reply-obligation fence: its owner clears
    /// only after a successful provider attempt consumes the matching response.
    fn proves_answered_question(
        &self,
        root: &str,
        child: &astra_server_types::SessionRunNode,
    ) -> Result<bool, &'static str> {
        use astra_turn_types::{
            AgentCommunicationDirection as Direction, AgentCommunicationPayloadKind as Kind,
            AgentCommunicationTarget as Target, AgentTranscriptEvidence,
        };
        let agent = child
            .agent_id
            .as_deref()
            .ok_or("question actor unavailable")?;
        let tools = self.tools(root)?;
        let mut questions = tools.iter().filter(|call| {
            call.run_id == child.run_id
                && call.name == "agent"
                && call.arguments.as_ref().is_some_and(|args| {
                    args["action"] == "send_message"
                        && args["message_type"] == "question"
                        && args["to"] == "parent"
                })
        });
        let Some(question) = questions.next() else {
            return Ok(false);
        };
        if questions.next().is_some() || question.ok != Some(true) {
            return Ok(false);
        }
        let receipt = question
            .result
            .as_ref()
            .ok_or("question receipt unavailable")?;
        if receipt["run_id"] != child.run_id || receipt["status"] != "queued" {
            return Ok(false);
        }
        let request = receipt["message_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("question identity unavailable")?;
        let obligation: astra_turn_types::PendingReply =
            serde_json::from_value(receipt["reply_obligation"].clone())
                .map_err(|_| "question responder unavailable")?;
        if obligation.request_id != request
            || obligation.expected_responder.run_id.is_empty()
            || obligation.expected_responder.agent_id.is_empty()
        {
            return Ok(false);
        }
        let mut answers = tools.iter().filter(|call| {
            call.run_id == root
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
        let transcript = self
            .transcript
            .as_ref()
            .ok_or("communication transcript unavailable")?;
        let mut sent = false;
        let mut received = false;
        let mut unavailable = false;
        for item in &transcript.items {
            let Some(AgentTranscriptEvidence::AgentCommunication { event }) = &item.evidence else {
                continue;
            };
            if event.message_id != message {
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
                        && !event.observed_by.agent_id.is_empty() =>
                {
                    sent = true
                }
                Direction::Received
                    if event.observed_by.run_id == child.run_id
                        && event.observed_by.agent_id == agent =>
                {
                    received = true
                }
                _ => return Ok(false),
            }
        }
        if unavailable || !sent || !received {
            return Err("answer delivery observation unavailable");
        }
        Ok(true)
    }

    fn child_adoption_time(
        &self,
        root_id: &str,
        child: &astra_server_types::SessionRunNode,
        digest: &str,
        model: &str,
        facts: &[&Value],
    ) -> Result<chrono::NaiveDateTime, &'static str> {
        let (Some(agent), Some(offering)) = (
            child.agent_id.as_deref(),
            child.runtime.offering_id.as_deref(),
        ) else {
            return Err("child agent or offering identity unavailable");
        };
        if agent.is_empty() || offering.is_empty() {
            return Err("child agent or offering identity unavailable");
        }
        let summary = self
            .reflection
            .model_requests
            .terminal
            .as_ref()
            .ok_or("child physical execution unavailable")?;
        let physical = matches!(
            self.reflection.model_requests.coverage,
            astra_services::reflect::ModelRequestCaptureCoverage::WindowObserved
                | astra_services::reflect::ModelRequestCaptureCoverage::WindowLimitReached
        ) && summary.conflicting_requests == 0
            && summary.groups.iter().any(|group| {
                group.run_id.as_deref() == Some(child.run_id.as_str())
                    && group.offering_id == offering
                    && group.model == model
                    && group.terminal_statuses.succeeded > 0
                    && group.provider_response_id_observations > 0
            });
        if !physical {
            return Err("child physical execution unavailable");
        }
        let adopted = facts
            .iter()
            .filter(|fact| {
                fact["kind"] == "agent_dependency_boundary"
                    && fact["run_id"] == root_id
                    && fact["parent_run_id"] == root_id
                    && fact["outcome"] == "results_adopted"
                    && fact["children"].as_array().is_some_and(|children| {
                        children.iter().any(|entry| {
                            entry["agent_id"] == agent
                                && entry["run_id"] == child.run_id
                                && entry["status"] == "completed"
                                && entry["result_truncated"] == false
                                && entry["result_sha256"] == digest
                        })
                    })
            })
            .filter_map(|adoption| fact_time(adoption))
            .min()
            .ok_or("child result adoption unavailable")?;
        Ok(adopted)
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

    fn expectation(
        text: &str,
        model: &str,
        slot_index: Option<u32>,
    ) -> crate::criteria::ChildExecutionExpectation {
        crate::criteria::ChildExecutionExpectation {
            expected_result: crate::criteria::ChildResultExpectation::Text(text.into()),
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
        group: Option<&str>,
    ) -> Option<crate::classify::FailureClass> {
        let mut outcome = crate::runner::RunOutcome::new("test-model").with_exit_code(0);
        outcome.session_id = Some(capture.session_id.clone());
        outcome.run_id = Some("root".into());
        let mut stream = crate::runner::StreamCapture::default();
        stream.identity_verified = true;
        stream.execution = Some(capture);
        outcome.stream_capture = Some(stream);
        let results = crate::criteria::evaluate_deterministic(
            &[crate::criteria::Criterion::ExecutionChildResultsAdopted {
                children,
                fanout_group: group.map(str::to_owned),
            }],
            &outcome,
        );
        (!results[0].passed).then(|| crate::classify::classify(&outcome, &results))
    }
    use astra_services::reflect::{ModelRequestCapture, ModelRequestGroup, ModelRequestSummary};

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
        let mut group = ModelRequestGroup {
            run_id: Some("child".into()),
            parent_run_id: Some("root".into()),
            agent_id: Some("member".into()),
            offering_id: "offer".into(),
            model: "test-model".into(),
            terminal_requests: 1,
            provider_response_id_observations: 1,
            ..Default::default()
        };
        group.terminal_statuses.succeeded = 1;
        let reflection = serde_json::json!({
            "schema_version": 2, "tool": "reflect", "session_id": "session",
            "analysis_view": "execution_trace", "topic": "execution", "facet": "trace",
            "depth": "forensic", "horizon": "session", "source_policy": "local_first",
            "include_context": false, "data_coverage": {"source":"server", "events":3, "decisions":0},
            "model_requests": ModelRequestCapture {
                coverage: astra_services::reflect::ModelRequestCaptureCoverage::WindowObserved,
                terminal: Some(ModelRequestSummary { terminal_requests:1, groups:vec![group], ..Default::default() }),
                ..Default::default()
            },
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
    fn sibling_custody_requires_exact_children_before_one_root_finalization() {
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
        page.items[1].content = serde_json::json!({"group_id":"group", "agents":[
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
        let summary = capture.reflection.model_requests.terminal.as_mut().unwrap();
        let mut physical = summary.groups[0].clone();
        physical.run_id = Some("sibling".into());
        physical.agent_id = Some("sibling-agent".into());
        summary.groups.push(physical);
        summary.terminal_requests += 1;
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
            capture.proves_children("root", &expected, Some("group")),
            Ok(true)
        );
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
            unlabelled.proves_children("root", &expected, Some("group")),
            Ok(true)
        );
        let mut missing_model = capture.clone();
        missing_model.run_tree.runs[1].runtime.model_name = None;
        assert_eq!(
            proof_failure(missing_model, expected.to_vec(), Some("group")),
            Some(crate::classify::FailureClass::InfraVerificationUnavailable)
        );
        assert_eq!(
            capture.proves_children("root", &expected[..1], Some("group")),
            Ok(false)
        );
        assert_eq!(
            capture.proves_children(
                "root",
                &[expected[0].clone(), expected[0].clone()],
                Some("group")
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
                Some("group")
            ),
            Ok(false)
        );
        for fault in ["late", "physical", "digest", "parent"] {
            let mut altered = capture.clone();
            match fault {
                "physical" => {
                    altered
                        .reflection
                        .model_requests
                        .terminal
                        .as_mut()
                        .unwrap()
                        .groups
                        .pop();
                }
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
                altered.proves_children("root", &expected, Some("group")),
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
                altered.proves_children("root", &expected, Some("group")),
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
                .proves_children("root", &expected, Some("group"))
                .is_err(),
            "successful collection and parent finalization cannot replace one missing adoption"
        );
        let mut contradictions = expected.clone();
        contradictions[0].initial_thinking =
            Some(crate::criteria::ChildThinkingExpectation::Exact {
                config: astra_turn_core::thinking_config::ThinkingConfig::ModelDefault,
            });
        contradictions[1].expected_result =
            crate::criteria::ChildResultExpectation::Text("wrong sibling output".into());
        for expectations in [
            contradictions.clone(),
            [contradictions[1].clone(), contradictions[0].clone()],
        ] {
            assert_eq!(
                capture.proves_children("root", &expectations, Some("group")),
                Ok(false),
                "missing thinking on one child cannot hide another child's wrong output"
            );
        }
        let mut natural = capture.clone();
        natural.run_tree.runs[2].runtime.model_name = Some("other-model".into());
        natural
            .reflection
            .model_requests
            .terminal
            .as_mut()
            .unwrap()
            .groups[1]
            .model = "other-model".into();
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
    fn selected_root_proofs_ignore_other_valid_roots_not_identity_conflicts() {
        let mut capture = tool_capture();
        let mut other = capture.run_tree.runs[0].clone();
        other.run_id = "other-root".into();
        other.root_run_id = Some("other-root".into());
        other.status = SessionRunLifecycleStatus::Paused;
        capture.run_tree.runs.push(other);
        let page = capture.transcript.as_mut().unwrap();
        let mut request = page.items[0].clone();
        request.item_seq = 4;
        request.run_id = Some("other-root".into());
        let mut response = page.items[1].clone();
        response.item_seq = 5;
        response.run_id = Some("other-root".into());
        page.items.extend([request, response]);
        assert_eq!(capture.tools("root").unwrap().len(), 1);
        assert_eq!(capture.child_count("root"), Some(1));
        assert!(singleton_proven(
            &capture,
            Some("root"),
            "observed-value",
            "test-model"
        ));
        for fault in ["unknown-run", "foreign-session", "lineage"] {
            let mut altered = capture.clone();
            match fault {
                "unknown-run" => {
                    altered.transcript.as_mut().unwrap().items[3].run_id = Some("unknown".into())
                }
                "foreign-session" => {
                    altered.transcript.as_mut().unwrap().items[3].session_id = "foreign".into()
                }
                "lineage" => altered.run_tree.runs[2].root_run_id = Some("root".into()),
                _ => unreachable!(),
            }
            assert!(altered.tools("root").is_err(), "{fault}");
            if fault == "lineage" {
                assert!(!singleton_proven(
                    &altered,
                    Some("root"),
                    "observed-value",
                    "test-model"
                ));
            }
        }
    }

    #[test]
    fn complete_transcript_joins_tool_identity_and_rejects_missing_evidence() {
        let original = tool_capture();
        let page = serde_json::to_value(original.transcript.as_ref().unwrap()).unwrap();
        let calls = original.tools("root").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].ok, Some(true));
        assert_eq!(calls[0].arguments.as_ref().unwrap()["action"], "spawn");
        assert_eq!(calls[0].result.as_ref().unwrap()["value"], 42);
        let mut polluted = original.clone();
        polluted.transcript.as_mut().unwrap().items[1]
            .content
            .push_str("\n\n[Runtime tool guidance]\nNot part of the result document.");
        assert!(polluted.tools("root").unwrap()[0].result.is_none());
        assert_eq!(original.child_count("root"), Some(1));
        for status in ["reused", "suppressed", "deferred"] {
            let mut altered = original.clone();
            altered.transcript.as_mut().unwrap().items[1]
                .tool_result
                .as_mut()
                .unwrap()
                .status = Some(status.into());
            assert_eq!(altered.tools("root").unwrap()[0].ok, None);
        }
        for (pointer, value) in [
            ("/session_id", serde_json::json!("foreign")),
            ("/has_more", serde_json::json!(true)),
            ("/items/1/run_id", serde_json::json!("child")),
            (
                "/items/1/tool_result/tool_use_id",
                serde_json::json!("other"),
            ),
            ("/items/1/tool_result/name", serde_json::json!("bash")),
            ("/items/1/tool_result/status", serde_json::json!("unknown")),
            ("/items/1/source_event_id", Value::Null),
            ("/items/1/item_seq", serde_json::json!(1)),
        ] {
            let mut modified = page.clone();
            *modified.pointer_mut(pointer).unwrap() = value;
            let mut altered = original.clone();
            altered.transcript = Some(serde_json::from_value(modified).unwrap());
            assert!(altered.tools("root").is_err(), "{pointer}");
        }
        let mut altered = original.clone();
        altered.transcript.as_mut().unwrap().items.remove(1);
        assert!(altered.tools("root").is_err());
        altered = original.clone();
        altered.run_tree.runs[0].total_tool_calls = 2;
        assert!(altered.tools("root").is_err());
        altered.transcript = None;
        assert!(altered.tools("root").is_err());
        altered.run_tree.truncated = true;
        assert_eq!(altered.child_count("root"), None);
        altered = original.clone();
        altered.run_tree.runs[1].root_run_id = Some("foreign".into());
        assert!(altered.tools("root").is_err());
        assert_eq!(altered.child_count("root"), None);
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
            ("physical-parent", BehaviorContractViolation),
            ("physical-agent", BehaviorContractViolation),
            ("mixed-physical", BehaviorContractViolation),
            ("missing-physical", InfraVerificationUnavailable),
        ] {
            let mut altered = capture();
            match fault {
                "text" => {
                    altered.transcript.as_mut().unwrap().items[0].content =
                        "wrong actual terminal".into()
                }
                "missing-text" => altered.transcript = None,
                "physical-parent" => {
                    altered
                        .reflection
                        .model_requests
                        .terminal
                        .as_mut()
                        .unwrap()
                        .groups[0]
                        .parent_run_id = Some("wrong-parent".into())
                }
                "physical-agent" => {
                    altered
                        .reflection
                        .model_requests
                        .terminal
                        .as_mut()
                        .unwrap()
                        .groups[0]
                        .agent_id = Some("wrong-agent".into())
                }
                "mixed-physical" => {
                    let summary = altered.reflection.model_requests.terminal.as_mut().unwrap();
                    let mut wrong = summary.groups[0].clone();
                    wrong.parent_run_id = Some("wrong-parent".into());
                    summary.groups.push(wrong);
                    summary.terminal_requests += 1;
                }
                "missing-physical" => altered.reflection.model_requests.terminal = None,
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
        expected.expected_result = ChildResultExpectation::Contains("observed".into());
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
            ("root", "parent-agent", "sent", 5),
            ("child", "member", "received", 6),
        ] {
            items.push(serde_json::from_value(serde_json::json!({
                "session_id":"session","run_id":run,"item_seq":seq,"role":"event",
                "content":"","tool_calls":[],"tool_result":null,
                "source_event_id":format!("message-{direction}"),"created_at":"2026-10-07T00:00:00Z",
                "evidence":{"kind":"agent_communication","event":{
                    "schema_version":astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION,
                    "observed_by":{"run_id":run,"agent_id":agent},"direction":direction,
                    "message_id":"answer-id","from":party,
                    "to":{"kind":"direct","address":{"run_id":"child","agent_id":"member"}},
                    "payload_kind":"response","response_accepted":true,
                    "related_message_id":"question-id","timestamp_ms":1
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
            } else if fault == "unfinished" {
                altered.run_tree.runs[1].status = SessionRunLifecycleStatus::Running;
            } else if fault == "reverse-question" {
                altered.transcript.as_mut().unwrap().items[0].tool_calls[0].arguments =
                    r#"{"action":"send_message","message_type":"question","to":"member"}"#.into();
            } else {
                let evidence = altered.transcript.as_mut().unwrap().items[5]
                    .evidence
                    .as_mut()
                    .unwrap();
                let astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event } =
                    evidence
                else {
                    unreachable!()
                };
                match fault {
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
        for change in [
            "foreign",
            "missing",
            "duplicate",
            "unidentified",
            "oversized",
            "partial",
        ] {
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
                "oversized" => page.items[2].content = "x".repeat(4097),
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
        let group = &mut joined
            .reflection
            .model_requests
            .terminal
            .as_mut()
            .unwrap()
            .groups[0];
        group.parent_run_id = None;
        group.agent_id = None;
        assert!(singleton_proven(
            &joined,
            Some("root"),
            "observed-value",
            "test-model"
        ));
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
            (
                "/reflection/model_requests/coverage",
                serde_json::json!("source_unavailable"),
            ),
            (
                "/reflection/model_requests/terminal/groups/0/parent_run_id",
                serde_json::json!("foreign"),
            ),
            (
                "/reflection/model_requests/terminal/groups/0/agent_id",
                serde_json::json!("foreign"),
            ),
            (
                "/reflection/model_requests/terminal/groups/0/offering_id",
                serde_json::json!("foreign"),
            ),
            (
                "/reflection/model_requests/terminal/groups/0/provider_response_id_observations",
                serde_json::json!(0),
            ),
            (
                "/reflection/model_requests/terminal/groups/0/terminal_statuses/succeeded",
                serde_json::json!(0),
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
