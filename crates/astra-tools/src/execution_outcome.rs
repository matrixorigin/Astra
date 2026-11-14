use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct ToolExecutionOutcome {
    pub output: String,
    pub tool_result_fields: Option<serde_json::Map<String, serde_json::Value>>,
    pub is_error: bool,
    /// Local approval evidence. Never serialize or reconstruct from tool output.
    pub sandbox_approval_scope: Option<astra_sandbox::SandboxApprovalScope>,
}

impl ToolExecutionOutcome {
    /// Construct a successful outcome. `is_error` is ALWAYS `false`.
    ///
    /// Use this for any non-error output, even strings that happen to start
    /// with "Error" (e.g. `"Error code 0 (no change)"`, diff hunks quoting
    /// compiler errors, log lines, etc.). For error outcomes use [`Self::error`].
    pub fn ok(output: String) -> Self {
        Self {
            output,
            sandbox_approval_scope: None,
            tool_result_fields: None,
            is_error: false,
        }
    }

    pub fn error(output: String) -> Self {
        Self {
            output,
            sandbox_approval_scope: None,
            tool_result_fields: None,
            is_error: true,
        }
    }

    pub fn error_with_evidence(output: String, evidence: astra_core::ToolFailureEvidence) -> Self {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "error_kind".to_string(),
            serde_json::Value::String(evidence.kind.as_str().to_string()),
        );
        fields.insert(
            "disposition".to_string(),
            serde_json::Value::String("rejected".to_string()),
        );
        if let Ok(value) = serde_json::to_value(evidence) {
            fields.insert("recovery_evidence".to_string(), value);
        }
        Self {
            output,
            sandbox_approval_scope: None,
            tool_result_fields: Some(fields),
            is_error: true,
        }
    }

    /// A later subcommand can fail to start after an earlier step ran.
    pub fn after_prior_execution(mut self) -> Self {
        if self.is_error {
            let fields = self.tool_result_fields.get_or_insert_with(Default::default);
            fields.insert("disposition".into(), Value::String("executed".into()));
            fields.insert("process_started".into(), Value::Bool(true));
            if let Some(evidence) = fields
                .get_mut("recovery_evidence")
                .and_then(Value::as_object_mut)
            {
                evidence.insert("retryable".into(), Value::Bool(false));
                evidence.insert(
                    "recovery_actions".into(),
                    serde_json::json!(["inspect_structured_failure"]),
                );
            }
            self.output.push_str(" An earlier step of this tool already executed; inspect the resulting state before retrying.");
        }
        self
    }

    /// Mark a successful git operation whose owner has actually changed the
    /// bound repository/worktree.  This is consumed by the executor to mint
    /// the same typed mutation receipt as structured file writers.
    pub fn with_workspace_mutation_applied(mut self) -> Self {
        self.tool_result_fields
            .get_or_insert_with(serde_json::Map::new)
            .insert(
                "workspace_mutation_applied".to_string(),
                serde_json::Value::Bool(true),
            );
        self
    }
}

/// Carry a producer-owned control execution fact through a tool
/// boundary.  Control tools may reject a request after inspecting durable
/// state (for example, a duplicate fanout start) while returning a structured
/// error that still contains the existing group.  The JSON fact must reach the
/// journal as `Rejected`; otherwise the record fallback promotes the request
/// to `Executed` and completion policy treats a request that never ran as a
/// failed execution.
pub fn insert_producer_execution_fact(
    parsed: Option<&Value>,
    metadata: &mut serde_json::Map<String, Value>,
) {
    let Some(executed) = parsed.and_then(|value| {
        value
            .get("executed")
            .or_else(|| value.pointer("/advisory/executed"))
    }) else {
        return;
    };
    match executed {
        Value::Bool(false) => insert_not_executed_fact(metadata),
        Value::Bool(true) => {
            metadata.insert("execution_started".to_string(), Value::Bool(true));
            metadata.insert(
                "execution_fact".to_string(),
                Value::String("executed".to_string()),
            );
        }
        Value::Null => {
            // `null` is an explicit outcome-unknown fact. Do not turn it into
            // `execution_started=false`: the operation may already have had
            // side effects and must remain fail-closed for settlement.
            insert_unknown_execution_fact(metadata);
        }
        _ => {}
    }
}

/// Preserve known startup, but never let a stale rejection claim that an
/// invocation with an unknown outcome definitely did not start.
pub fn insert_unknown_execution_fact(metadata: &mut serde_json::Map<String, Value>) {
    if metadata.get("execution_started").and_then(Value::as_bool) == Some(false) {
        metadata.remove("execution_started");
    }
    if metadata.get("disposition").and_then(Value::as_str) == Some("rejected") {
        metadata.remove("disposition");
    }
    if metadata.get("execution_started").and_then(Value::as_bool) != Some(true)
        && metadata.get("blocked").and_then(Value::as_bool) == Some(true)
    {
        metadata.remove("blocked");
    }
    // The nested runtime error duplicates the top-level facts. A pre-start
    // error cannot accurately describe an outcome whose start is unknown.
    if metadata
        .get("runtime_error")
        .and_then(|value| value.get("execution_started"))
        .and_then(Value::as_bool)
        == Some(false)
    {
        metadata.remove("runtime_error");
    }
    metadata.insert("execution_fact".into(), Value::String("unknown".into()));
    metadata.insert("side_effects_maybe".into(), Value::Bool(true));
    metadata.insert("retryable".into(), Value::Bool(false));
}

/// Record non-execution only at a producer boundary that has not started work.
pub fn insert_not_executed_fact(metadata: &mut serde_json::Map<String, Value>) {
    metadata.insert("execution_started".to_string(), Value::Bool(false));
    metadata.insert(
        "disposition".to_string(),
        Value::String("rejected".to_string()),
    );
    metadata.insert(
        "execution_fact".to_string(),
        Value::String("not_executed".to_string()),
    );
}

/// Record process startup at the executor that owns it. Later validation or
/// observation failure cannot turn an executed command into a preflight refusal.
pub fn insert_started_execution_fact(metadata: &mut serde_json::Map<String, Value>) {
    let unknown = outcome_is_unknown(Some(metadata));
    metadata.insert("execution_started".into(), Value::Bool(true));
    metadata.insert("disposition".into(), Value::String("executed".into()));
    if !unknown {
        metadata.insert("execution_fact".into(), Value::String("executed".into()));
    }
    metadata.insert("side_effects_maybe".into(), Value::Bool(true));
}

/// A known result may have effects. Only an explicit executor or transport
/// fact means that its outcome is unknown.
pub fn outcome_is_unknown(metadata: Option<&serde_json::Map<String, Value>>) -> bool {
    ["execution_fact", "outcome_certainty"].iter().any(|key| {
        metadata
            .and_then(|fields| fields.get(*key))
            .and_then(Value::as_str)
            == Some("unknown")
    })
}

/// Only consistent producer facts can prove that this invocation did not start.
/// Missing acknowledgements or conflicting/unknown effects are not non-execution.
pub fn execution_was_not_started(metadata: Option<&serde_json::Map<String, Value>>) -> bool {
    let Some(fields) = metadata else { return false };
    (fields.get("execution_started").and_then(Value::as_bool) == Some(false)
        || fields.get("execution_fact").and_then(Value::as_str) == Some("not_executed"))
        && fields
            .get("execution_started")
            .is_none_or(|v| v == &Value::Bool(false))
        && fields
            .get("execution_fact")
            .is_none_or(|v| v.as_str() == Some("not_executed"))
        && fields
            .get("disposition")
            .is_none_or(|v| matches!(v.as_str(), Some("rejected" | "suppressed")))
        && fields
            .get("executed")
            .is_none_or(|v| v == &Value::Bool(false))
        && fields
            .get("side_effects_maybe")
            .is_none_or(|v| v == &Value::Bool(false))
        && !outcome_is_unknown(Some(fields))
}

/// A suppressed duplicate is not eligible for a sandbox-denial retry.
pub fn execution_was_rejected(metadata: Option<&serde_json::Map<String, Value>>) -> bool {
    execution_was_not_started(metadata)
        && metadata
            .and_then(|fields| fields.get("disposition"))
            .is_none_or(|value| value.as_str() == Some("rejected"))
}

pub fn execution_error_kind(
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ErrorKind> {
    tool_result_fields
        .and_then(|fields| fields.get("error_kind"))
        .and_then(serde_json::Value::as_str)
        .and_then(parse_execution_error_kind_tag)
        .or_else(|| {
            outcome_is_unknown(tool_result_fields)
                .then_some(astra_core::ErrorKind::ToolOutcomeUnknown)
        })
}

pub fn execution_recovery_evidence(
    tool_result_fields: Option<&Map<String, Value>>,
) -> Option<astra_core::ToolFailureEvidence> {
    let fields = tool_result_fields?;
    let mut evidence = fields
        .get("recovery_evidence")
        .filter(|value| astra_turn_types::json_serialized_fits(value, 4096).unwrap_or(false))
        .cloned()
        .and_then(|value| serde_json::from_value::<astra_core::ToolFailureEvidence>(value).ok())
        .or_else(|| {
            // Compact transport facts use the same evidence type as local
            // tools, without classifying rendered banners.
            let kind = execution_error_kind(Some(fields))?;
            let mut evidence = astra_core::ToolFailureEvidence::from_error_kind(kind);
            evidence.retryable = fields
                .get("retryable")
                .and_then(Value::as_bool)
                .unwrap_or_else(|| kind.is_retryable());
            Some(evidence)
        })?;
    if outcome_is_unknown(Some(fields)) {
        // Keep the source timeout/cancellation kind; uncertainty describes the
        // outcome, not its transport cause. Both survive result compaction.
        evidence.cause = astra_core::ToolFailureCause::OutcomeUnknown;
        evidence.retryable = false;
        evidence.recovery_actions = vec![astra_core::ToolRecoveryAction::ReconcileOutcome];
    }
    Some(evidence)
}

fn parse_execution_error_kind_tag(tag: &str) -> Option<astra_core::ErrorKind> {
    astra_core::ErrorKind::parse_tag(tag).or(match tag {
        // These aliases are part of the existing Edge/Server wire contract;
        // normalize them at the boundary instead of scattering string checks
        // through recovery and turn-guard code.
        "capability_denied" | "approval_denied" | "sandbox_denied" => {
            Some(astra_core::ErrorKind::PolicyDenied)
        }
        "approval_timeout" => Some(astra_core::ErrorKind::ToolTimeout),
        "transport_unavailable" => Some(astra_core::ErrorKind::ToolUnavailable),
        _ => None,
    })
}
