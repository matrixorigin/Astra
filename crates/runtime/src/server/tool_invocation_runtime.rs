//! Runtime adapter over the shared durable invocation ledger contract.
//!
//! This module is also the single projection boundary between ephemeral
//! `ToolResult` values and durable typed outcomes. Classification only uses
//! machine-readable fields; provider/user errors are never inferred from
//! human-readable prose.

use std::collections::BTreeMap;
use std::sync::Arc;

use astra_turn_core::invocation_ledger::{InMemoryInvocationLedger, InvocationLedgerError};
use astra_turn_types::{
    DispatchCertainty, ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationResultPayload,
    ToolInvocationState, ToolInvocationTerminalOutcome,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

pub(crate) enum InvocationBeginDisposition {
    Execute(ToolInvocationDecision),
    Return(astra_tools::ToolResult),
}

#[derive(Clone)]
pub(crate) enum RuntimeToolInvocationLedger {
    Database(astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger),
    InMemory(Arc<tokio::sync::Mutex<InMemoryInvocationLedger>>),
}

impl RuntimeToolInvocationLedger {
    pub(crate) fn new(pool: Option<astra_core::SharedPool>) -> Self {
        match pool {
            Some(pool) => Self::Database(
                astra_services::tool_invocation_ledger::DatabaseToolInvocationLedger::new(pool),
            ),
            None => Self::InMemory(Arc::new(tokio::sync::Mutex::new(
                InMemoryInvocationLedger::default(),
            ))),
        }
    }

    pub(crate) async fn prepare(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
    ) -> Result<ToolInvocationPrepareOutcome, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger.prepare(identity, fingerprint, decision).await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.prepare(
                identity.clone(),
                fingerprint.clone(),
                decision.clone(),
            )?),
        }
    }

    pub(crate) async fn dispatch(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .compare_and_transition(
                    identity,
                    ToolInvocationState::Prepared,
                    ToolInvocationState::Dispatched,
                    DispatchCertainty::Dispatched,
                )
                .await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.compare_and_transition(
                identity,
                ToolInvocationState::Prepared,
                ToolInvocationState::Dispatched,
                DispatchCertainty::Dispatched,
            )?),
        }
    }

    pub(crate) async fn get(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<Option<ToolInvocationRecord>, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger.get(identity).await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.get(identity).cloned()),
        }
    }

    pub(crate) async fn complete(
        &self,
        identity: &ToolInvocationIdentity,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .compare_and_complete(identity, ToolInvocationState::Dispatched, outcome)
                .await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.compare_and_complete(
                identity,
                ToolInvocationState::Dispatched,
                outcome.clone(),
            )?),
        }
    }

    pub(crate) async fn mark_outcome_unknown(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<ToolInvocationRecord, RuntimeInvocationLedgerError> {
        match self {
            Self::Database(ledger) => Ok(ledger
                .compare_and_transition(
                    identity,
                    ToolInvocationState::Dispatched,
                    ToolInvocationState::OutcomeUnknown,
                    DispatchCertainty::Unknown,
                )
                .await?),
            Self::InMemory(ledger) => Ok(ledger.lock().await.compare_and_transition(
                identity,
                ToolInvocationState::Dispatched,
                ToolInvocationState::OutcomeUnknown,
                DispatchCertainty::Unknown,
            )?),
        }
    }

    /// Prepare and atomically claim the route boundary. A terminal row is
    /// replayed; an acknowledged or uncertain in-flight row is never sent a
    /// second time.
    pub(crate) async fn begin(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
        validate_decision: impl FnOnce(&ToolInvocationDecision) -> Result<(), String>,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        let record = match self.prepare(identity, fingerprint, decision).await? {
            ToolInvocationPrepareOutcome::Prepared(record)
            | ToolInvocationPrepareOutcome::Existing(record) => record,
        };
        match record.state {
            ToolInvocationState::Prepared => {
                validate_decision(&record.decision)
                    .map_err(RuntimeInvocationLedgerError::InvalidDecision)?;
                match self.dispatch(identity).await {
                    Ok(record) => Ok(InvocationBeginDisposition::Execute(record.decision)),
                    Err(dispatch_error) => {
                        let Some(authoritative) = self.get(identity).await? else {
                            return Err(dispatch_error);
                        };
                        disposition_for_existing_record(authoritative)?.ok_or(dispatch_error)
                    }
                }
            }
            _ => disposition_for_existing_record(record)?.ok_or_else(|| {
                RuntimeInvocationLedgerError::InvalidRecord(
                    "existing prepared invocation was not dispatchable".to_string(),
                )
            }),
        }
    }

    /// Persist the execution result before exposing it. Ambiguous transport
    /// results become `OutcomeUnknown`; a persistence failure after execution
    /// is surfaced as a durability error instead of leaking an undurable
    /// success to the caller.
    pub(crate) async fn finish(
        &self,
        identity: &ToolInvocationIdentity,
        mut result: astra_tools::ToolResult,
    ) -> astra_tools::ToolResult {
        if result_side_effects_maybe(&result) {
            return match self.mark_outcome_unknown(identity).await {
                Ok(_) => {
                    annotate_durable_state(&mut result, ToolInvocationState::OutcomeUnknown, false);
                    let metadata = result.metadata.get_or_insert_with(Map::new);
                    metadata.insert("retryable".to_string(), Value::Bool(false));
                    metadata.insert("resumable".to_string(), Value::Bool(true));
                    result
                }
                Err(error) => durability_error_result(
                    identity,
                    ToolInvocationState::Dispatched,
                    format!("persist outcome-unknown state: {error}"),
                ),
            };
        }

        let outcome = terminal_outcome_from_result(&result);
        match self.complete(identity, &outcome).await {
            Ok(record) => project_terminal_outcome(&outcome, record.state, false),
            Err(complete_error) => {
                if let Ok(Some(record)) = self.get(identity).await
                    && record.state.is_terminal()
                {
                    return replay_terminal_record(&record).unwrap_or_else(|error| {
                        durability_error_result(identity, record.state, error.to_string())
                    });
                }
                match self.mark_outcome_unknown(identity).await {
                    Ok(_) => durability_error_result(
                        identity,
                        ToolInvocationState::OutcomeUnknown,
                        format!("persist terminal outcome: {complete_error}"),
                    ),
                    Err(unknown_error) => durability_error_result(
                        identity,
                        ToolInvocationState::Dispatched,
                        format!(
                            "persist terminal outcome: {complete_error}; mark outcome unknown: {unknown_error}"
                        ),
                    ),
                }
            }
        }
    }
}

fn disposition_for_existing_record(
    record: ToolInvocationRecord,
) -> Result<Option<InvocationBeginDisposition>, RuntimeInvocationLedgerError> {
    match record.state {
        ToolInvocationState::Prepared => Ok(None),
        ToolInvocationState::Dispatched => Ok(Some(InvocationBeginDisposition::Return(
            pending_result(&record.identity),
        ))),
        ToolInvocationState::OutcomeUnknown => Ok(Some(InvocationBeginDisposition::Return(
            outcome_unknown_result(&record.identity),
        ))),
        ToolInvocationState::Succeeded
        | ToolInvocationState::Failed
        | ToolInvocationState::Rejected => Ok(Some(InvocationBeginDisposition::Return(
            replay_terminal_record(&record)?,
        ))),
    }
}

fn terminal_outcome_from_result(result: &astra_tools::ToolResult) -> ToolInvocationTerminalOutcome {
    let payload = ToolInvocationResultPayload {
        output: result.output.clone(),
        metadata: result
            .metadata
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
        exit_semantics: result.exit_semantics.and_then(|semantics| {
            serde_json::to_value(semantics)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
        }),
    };
    if !result.is_error {
        return ToolInvocationTerminalOutcome::Succeeded { result: payload };
    }
    if let Some((rejection_code, retryable)) = rejection_evidence(result) {
        return ToolInvocationTerminalOutcome::Rejected {
            result: payload,
            rejection_code,
            retryable,
        };
    }
    ToolInvocationTerminalOutcome::Failed {
        result: payload,
        error_kind: structured_field(result, "error_kind")
            .and_then(|value| value.as_str().map(str::to_string)),
        retryable: structured_field(result, "retryable")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

fn rejection_evidence(result: &astra_tools::ToolResult) -> Option<(Option<String>, bool)> {
    if let Some(provider_rejection) = result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("providerRejection"))
        .and_then(Value::as_object)
    {
        return Some((
            provider_rejection
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string),
            provider_rejection
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        ));
    }

    let rejection_code = structured_field(result, "rejection_code")
        .and_then(|value| value.as_str().map(str::to_string));
    let error_kind =
        structured_field(result, "error_kind").and_then(|value| value.as_str().map(str::to_string));
    let reason_kind = structured_field(result, "reason_kind")
        .and_then(|value| value.as_str().map(str::to_string));
    let capability_denial = structured_field(result, "capability_denial").is_some();
    let explicitly_rejected = rejection_code.is_some()
        || capability_denial
        || matches!(
            error_kind.as_deref(),
            Some("approval_denied" | "approval_timeout" | "capability_denied" | "policy_denied")
        )
        || matches!(
            reason_kind.as_deref(),
            Some("policy_denied" | "runtime_surface_denied")
        );
    explicitly_rejected.then(|| {
        (
            rejection_code.or(error_kind),
            structured_field(result, "retryable")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        )
    })
}

fn structured_field(result: &astra_tools::ToolResult, key: &str) -> Option<Value> {
    result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(key))
        .cloned()
        .or_else(|| {
            serde_json::from_str::<Value>(&result.output)
                .ok()
                .and_then(|value| value.get(key).cloned())
        })
}

fn result_side_effects_maybe(result: &astra_tools::ToolResult) -> bool {
    structured_field(result, "side_effects_maybe")
        .and_then(|value| value.as_bool())
        .unwrap_or(false)
}

fn replay_terminal_record(
    record: &ToolInvocationRecord,
) -> Result<astra_tools::ToolResult, RuntimeInvocationLedgerError> {
    let outcome = record.outcome.as_ref().ok_or_else(|| {
        RuntimeInvocationLedgerError::InvalidRecord(format!(
            "terminal invocation {:?} has no typed outcome",
            record.identity
        ))
    })?;
    Ok(project_terminal_outcome(outcome, record.state, true))
}

fn project_terminal_outcome(
    outcome: &ToolInvocationTerminalOutcome,
    state: ToolInvocationState,
    replay: bool,
) -> astra_tools::ToolResult {
    let payload = outcome.result();
    let mut result = astra_tools::ToolResult {
        output: payload.output.clone(),
        metadata: (!payload.metadata.is_empty()).then(|| {
            payload
                .metadata
                .clone()
                .into_iter()
                .collect::<Map<String, Value>>()
        }),
        is_error: !matches!(outcome, ToolInvocationTerminalOutcome::Succeeded { .. }),
        exit_semantics: payload
            .exit_semantics
            .as_ref()
            .and_then(|semantics| serde_json::from_value(Value::String(semantics.clone())).ok()),
    };
    let metadata = result.metadata.get_or_insert_with(Map::new);
    match outcome {
        ToolInvocationTerminalOutcome::Succeeded { .. } => {}
        ToolInvocationTerminalOutcome::Failed {
            error_kind,
            retryable,
            ..
        } => {
            if let Some(error_kind) = error_kind {
                metadata
                    .entry("error_kind".to_string())
                    .or_insert_with(|| Value::String(error_kind.clone()));
            }
            metadata
                .entry("retryable".to_string())
                .or_insert(Value::Bool(*retryable));
        }
        ToolInvocationTerminalOutcome::Rejected {
            rejection_code,
            retryable,
            ..
        } => {
            if let Some(rejection_code) = rejection_code {
                metadata
                    .entry("rejection_code".to_string())
                    .or_insert_with(|| Value::String(rejection_code.clone()));
            }
            metadata
                .entry("retryable".to_string())
                .or_insert(Value::Bool(*retryable));
        }
    }
    annotate_durable_state(&mut result, state, replay);
    result
}

fn annotate_durable_state(
    result: &mut astra_tools::ToolResult,
    state: ToolInvocationState,
    replay: bool,
) {
    let metadata = result.metadata.get_or_insert_with(Map::new);
    metadata.insert(
        "durable_invocation_state".to_string(),
        Value::String(state_label(state).to_string()),
    );
    metadata.insert("invocation_replay".to_string(), Value::Bool(replay));
}

fn pending_result(identity: &ToolInvocationIdentity) -> astra_tools::ToolResult {
    invocation_state_result(
        identity,
        ToolInvocationState::Dispatched,
        "tool_invocation_in_progress",
        false,
        "The same logical tool invocation is already dispatched; Astra will not send it again.",
    )
}

fn outcome_unknown_result(identity: &ToolInvocationIdentity) -> astra_tools::ToolResult {
    invocation_state_result(
        identity,
        ToolInvocationState::OutcomeUnknown,
        "tool_invocation_outcome_unknown",
        true,
        "The provider may have applied this logical tool invocation, but no acknowledged outcome is durable; Astra will not retry it automatically.",
    )
}

fn durability_error_result(
    identity: &ToolInvocationIdentity,
    state: ToolInvocationState,
    detail: String,
) -> astra_tools::ToolResult {
    invocation_state_result(
        identity,
        state,
        "tool_invocation_durability",
        true,
        &format!(
            "Tool execution crossed the dispatch boundary but its durable outcome could not be committed: {detail}"
        ),
    )
}

pub(crate) fn ledger_unavailable_result(
    identity: &ToolInvocationIdentity,
    detail: impl std::fmt::Display,
) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(format!(
        "The logical tool invocation could not enter its durable dispatch ledger: {detail}"
    ));
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String("tool_invocation_ledger".to_string()),
        ),
        ("invocation_identity".to_string(), json!(identity)),
        ("side_effects_maybe".to_string(), Value::Bool(false)),
        ("retryable".to_string(), Value::Bool(true)),
        ("resumable".to_string(), Value::Bool(true)),
    ]));
    result
}

fn invocation_state_result(
    identity: &ToolInvocationIdentity,
    state: ToolInvocationState,
    error_kind: &str,
    side_effects_maybe: bool,
    message: &str,
) -> astra_tools::ToolResult {
    let mut result = astra_tools::ToolResult::error(message.to_string());
    result.metadata = Some(Map::from_iter([
        (
            "error_kind".to_string(),
            Value::String(error_kind.to_string()),
        ),
        (
            "durable_invocation_state".to_string(),
            Value::String(state_label(state).to_string()),
        ),
        ("invocation_identity".to_string(), json!(identity)),
        (
            "side_effects_maybe".to_string(),
            Value::Bool(side_effects_maybe),
        ),
        ("retryable".to_string(), Value::Bool(false)),
        ("resumable".to_string(), Value::Bool(true)),
    ]));
    result
}

fn state_label(state: ToolInvocationState) -> &'static str {
    match state {
        ToolInvocationState::Prepared => "prepared",
        ToolInvocationState::Dispatched => "dispatched",
        ToolInvocationState::Succeeded => "succeeded",
        ToolInvocationState::Failed => "failed",
        ToolInvocationState::Rejected => "rejected",
        ToolInvocationState::OutcomeUnknown => "outcome_unknown",
    }
}

#[derive(Debug, Error)]
pub(crate) enum RuntimeInvocationLedgerError {
    #[error(transparent)]
    Database(Box<astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError>),
    #[error(transparent)]
    InMemory(Box<InvocationLedgerError>),
    #[error("invalid durable invocation record: {0}")]
    InvalidRecord(String),
    #[error("invalid frozen tool invocation decision: {0}")]
    InvalidDecision(String),
}

impl From<astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError>
    for RuntimeInvocationLedgerError
{
    fn from(error: astra_services::tool_invocation_ledger::ToolInvocationLedgerStoreError) -> Self {
        Self::Database(Box::new(error))
    }
}

impl From<InvocationLedgerError> for RuntimeInvocationLedgerError {
    fn from(error: InvocationLedgerError) -> Self {
        Self::InMemory(Box::new(error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_tools::exit_semantics::ExitSemantics;
    use astra_turn_types::DurableToolReference;
    use serde_json::json;

    fn identity(invocation_id: &str) -> ToolInvocationIdentity {
        ToolInvocationIdentity::new("user", "session", "run", "turn", invocation_id).unwrap()
    }

    fn fingerprint(arguments: &Value) -> ToolInvocationFingerprint {
        let decision = decision("decision-v1");
        fingerprint_for(arguments, &decision)
    }

    fn fingerprint_for(
        arguments: &Value,
        decision: &ToolInvocationDecision,
    ) -> ToolInvocationFingerprint {
        ToolInvocationFingerprint::new(
            DurableToolReference::built_in("bash", "contract-v1").unwrap(),
            arguments,
            &decision.decision_id,
        )
        .unwrap()
    }

    fn decision(label: &str) -> ToolInvocationDecision {
        ToolInvocationDecision::new(&json!({"decision": label})).unwrap()
    }

    async fn begin(
        ledger: &RuntimeToolInvocationLedger,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
    ) -> Result<InvocationBeginDisposition, RuntimeInvocationLedgerError> {
        ledger
            .begin(identity, fingerprint, &decision("decision-v1"), |_| Ok(()))
            .await
    }

    #[tokio::test]
    async fn logical_identity_not_semantic_arguments_controls_replay() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let args = json!({"command": "deploy"});
        let first = identity("call-1");
        let second = identity("call-2");
        let fingerprint = fingerprint(&args);

        assert!(matches!(
            begin(&ledger, &first, &fingerprint).await.unwrap(),
            InvocationBeginDisposition::Execute(_)
        ));
        let first_result = astra_tools::ToolResult::text("deployed".to_string());
        let committed = ledger.finish(&first, first_result).await;
        assert!(!committed.is_error);
        assert_eq!(
            committed.metadata.as_ref().unwrap()["durable_invocation_state"],
            "succeeded"
        );

        let replay = match begin(&ledger, &first, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute(_) => panic!("terminal identity must replay"),
        };
        assert_eq!(replay.output, "deployed");
        assert_eq!(replay.metadata.as_ref().unwrap()["invocation_replay"], true);

        assert!(matches!(
            begin(&ledger, &second, &fingerprint).await.unwrap(),
            InvocationBeginDisposition::Execute(_)
        ));
    }

    #[tokio::test]
    async fn same_identity_with_changed_arguments_fails_instead_of_replaying() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        begin(
            &ledger,
            &identity,
            &fingerprint(&json!({"command": "deploy"})),
        )
        .await
        .unwrap();

        assert!(matches!(
            begin(&ledger, &identity, &fingerprint(&json!({"command": "destroy"}))).await,
            Err(RuntimeInvocationLedgerError::InMemory(error))
                if matches!(*error, InvocationLedgerError::IdentityConflict { .. })
        ));
    }

    #[tokio::test]
    async fn prepared_resume_uses_original_decision_when_live_policy_changed() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        let args = json!({"command": "deploy"});
        let original = decision("original-policy");
        ledger
            .prepare(&identity, &fingerprint_for(&args, &original), &original)
            .await
            .unwrap();
        let changed = decision("changed-live-policy");

        let resumed = ledger
            .begin(
                &identity,
                &fingerprint_for(&args, &changed),
                &changed,
                |_| Ok(()),
            )
            .await
            .unwrap();
        match resumed {
            InvocationBeginDisposition::Execute(authoritative) => {
                assert_eq!(authoritative, original);
            }
            InvocationBeginDisposition::Return(_) => panic!("prepared invocation should resume"),
        }
    }

    #[test]
    fn terminal_classification_requires_structured_rejection_evidence() {
        let prose_only = astra_tools::ToolResult::error("request denied".to_string());
        assert!(matches!(
            terminal_outcome_from_result(&prose_only),
            ToolInvocationTerminalOutcome::Failed { .. }
        ));

        let mut approval = astra_tools::ToolResult::error("request denied".to_string());
        approval.metadata = Some(Map::from_iter([
            (
                "rejection_code".to_string(),
                Value::String("approval_denied".to_string()),
            ),
            ("retryable".to_string(), Value::Bool(false)),
        ]));
        assert!(matches!(
            terminal_outcome_from_result(&approval),
            ToolInvocationTerminalOutcome::Rejected {
                rejection_code: Some(code),
                retryable: false,
                ..
            } if code == "approval_denied"
        ));

        let mut provider = astra_tools::ToolResult::error("provider rejected".to_string());
        provider.metadata = Some(Map::from_iter([(
            "providerRejection".to_string(),
            json!({"code": "tenant_policy", "retryable": true}),
        )]));
        assert!(matches!(
            terminal_outcome_from_result(&provider),
            ToolInvocationTerminalOutcome::Rejected {
                rejection_code: Some(code),
                retryable: true,
                ..
            } if code == "tenant_policy"
        ));
    }

    #[tokio::test]
    async fn ambiguous_execution_becomes_outcome_unknown_and_is_not_redispatched() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        let fingerprint = fingerprint(&json!({"command": "deploy"}));
        assert!(matches!(
            begin(&ledger, &identity, &fingerprint).await.unwrap(),
            InvocationBeginDisposition::Execute(_)
        ));
        let mut ambiguous = astra_tools::ToolResult::error("connection lost".to_string());
        ambiguous.metadata = Some(Map::from_iter([
            ("execution_started".to_string(), Value::Bool(true)),
            ("side_effects_maybe".to_string(), Value::Bool(true)),
        ]));

        let result = ledger.finish(&identity, ambiguous).await;
        assert_eq!(
            result.metadata.as_ref().unwrap()["durable_invocation_state"],
            "outcome_unknown"
        );
        assert_eq!(result.metadata.as_ref().unwrap()["retryable"], false);
        let resumed = match begin(&ledger, &identity, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute(_) => {
                panic!("uncertain invocation must not retry")
            }
        };
        let metadata = resumed.metadata.unwrap();
        assert_eq!(metadata["error_kind"], "tool_invocation_outcome_unknown");
        assert_eq!(metadata["side_effects_maybe"], true);
        assert_eq!(metadata["retryable"], false);
    }

    #[tokio::test]
    async fn replay_preserves_payload_and_exit_semantics() {
        let ledger = RuntimeToolInvocationLedger::new(None);
        let identity = identity("call-1");
        let fingerprint = fingerprint(&json!({"command": "test -f missing"}));
        begin(&ledger, &identity, &fingerprint).await.unwrap();
        let result = astra_tools::ToolResult {
            output: "not found".to_string(),
            metadata: Some(Map::from_iter([("exit_code".to_string(), json!(1))])),
            is_error: false,
            exit_semantics: Some(ExitSemantics::DomainNegative),
        };
        ledger.finish(&identity, result).await;

        let replay = match begin(&ledger, &identity, &fingerprint).await.unwrap() {
            InvocationBeginDisposition::Return(result) => result,
            InvocationBeginDisposition::Execute(_) => panic!("terminal identity must replay"),
        };
        assert_eq!(replay.output, "not found");
        assert_eq!(replay.exit_semantics, Some(ExitSemantics::DomainNegative));
        assert_eq!(replay.metadata.as_ref().unwrap()["exit_code"], 1);
    }
}
