use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct ToolExecutionOutcome {
    pub output: String,
    pub tool_result_fields: Option<serde_json::Map<String, serde_json::Value>>,
    pub is_error: bool,
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
            tool_result_fields: None,
            is_error: false,
        }
    }

    pub fn error(output: String) -> Self {
        Self {
            output,
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
