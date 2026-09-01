use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use astra_tools::exit_semantics::{classify_command_result, classify_exit};
use serde_json::Value;

use super::tool_execution_binding::WorkspaceBinding;
use super::tool_execution_result::{tool_timeout_tool_result, workspace_path_mismatch_tool_result};
use super::tool_workspace_path_guard::server_sandbox_local_path_mismatch;
use crate::tool_sandbox::{
    IsolatedOutput, IsolationConfig, IsolationLevel, SandboxPolicy, filter_environment,
    wrap_command_with_limits,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerBashExecutionMode {
    SandboxedProcess,
    IsolatedProcess,
}

/// Maximum allowed length for a bash command string (100 KB).
pub(crate) const MAX_COMMAND_LENGTH: usize = 100 * 1024;

pub(crate) fn server_bash_execution_mode(policy: &SandboxPolicy) -> ServerBashExecutionMode {
    match policy.isolation {
        IsolationLevel::Permissive | IsolationLevel::Standard => {
            ServerBashExecutionMode::SandboxedProcess
        }
        IsolationLevel::Strict => ServerBashExecutionMode::IsolatedProcess,
    }
}

pub(crate) async fn execute_server_bash(
    sandbox_policy: &SandboxPolicy,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
    source_scope: Option<&str>,
    args: &Value,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> astra_tools::ToolResult {
    if args.get("run_in_background").is_some()
        || args.get("ready_check").is_some()
        || args.get("background_ttl").is_some()
    {
        return astra_tools::ToolResult::error(
            "Error: managed background fields are unavailable on this server Bash executor; no command was run"
                .to_string(),
        );
    }
    let command = match args.get("command").and_then(|value| value.as_str()) {
        Some(command) => command,
        None => {
            return astra_tools::ToolResult::error(
                "Error: Missing 'command' parameter".to_string(),
            );
        }
    };
    if let Err(reason) =
        astra_tools::shell_ops::validate_execute_bash_command_in_workspace(command, workspace_root)
    {
        return astra_tools::ToolResult::error(reason);
    }
    if command.len() > MAX_COMMAND_LENGTH {
        return astra_tools::ToolResult::error(format!(
            "Error: command exceeds maximum length of {} bytes",
            MAX_COMMAND_LENGTH
        ));
    }
    if let Some(reason) =
        server_sandbox_local_path_mismatch(command, workspace_root, workspace_binding)
    {
        return workspace_path_mismatch_tool_result(reason);
    }

    // Explicit source_artifacts is a transactional evidence-preservation
    // contract.  Validate and persist the preimage before any shell process
    // can open the workspace.  An absent declaration keeps ordinary bash
    // unchanged; a declaration without trusted invocation identity fails
    // closed instead of creating an unscoped receipt.
    let explicit_source_artifacts = args
        .get(astra_tools::source_preimage::SOURCE_ARTIFACTS_FIELD)
        .is_some();
    let mut source_preimages = match astra_tools::source_preimage::prepare(
        workspace_root,
        args,
        source_scope.unwrap_or_default(),
    ) {
        Ok(plan) => plan,
        Err(reason) => return astra_tools::ToolResult::error(format!("Error: {reason}")),
    };
    if source_preimages.is_none() && !explicit_source_artifacts {
        // This is intentionally advisory. A missing identity, ambiguous
        // operand, or unavailable durable store must never make ordinary
        // server bash unavailable; only an explicit declaration is fail-closed.
        if let Some(scope) = source_scope {
            source_preimages =
                astra_tools::source_preimage::prepare_inferred(workspace_root, command, scope)
                    .unwrap_or(None);
        }
    }

    let timeout_secs = args
        .get("timeout")
        .and_then(|value| value.as_f64())
        .unwrap_or(30.0)
        .min(sandbox_policy.max_execution_secs);
    // The authenticated run_script RPC bridge is the sole re-entrant writer
    // route. Its top-level script guard owns this whole callback, including
    // any direct Python writes concurrent with nested Bash, so the callback
    // must neither reacquire that exclusive generation nor mint its own
    // fingerprint receipt.
    let nested_run_script_callback = astra_tools::rpc_bridge::is_run_script_rpc_dispatch();
    if nested_run_script_callback
        && args
            .get(astra_tools::workspace_observation::EXTERNAL_STATE_PATHS_FIELD)
            .is_some()
    {
        return astra_tools::ToolResult::error(
            "Error: external_state_paths requires a top-level foreground executor-owned observation window"
                .to_string(),
        );
    }
    let observation_lease = if !command.trim().is_empty() && !nested_run_script_callback {
        match astra_tools::workspace_observation::acquire_workspace_observation_lease_with_options(
            workspace_root,
            cancel_token,
            Duration::from_secs_f64(timeout_secs.max(0.1)),
        )
        .await
        {
            Some(guard) => Some(guard),
            None => {
                if cancel_token.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                    return astra_tools::cancelled_tool_result("bash", false);
                }
                return astra_tools::ToolResult::error(
                    "Error: workspace observation lease was unavailable or timed out; no bash command was run".into(),
                );
            }
        }
    } else {
        None
    };
    if cancel_token.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return astra_tools::cancelled_tool_result("bash", false);
    }
    // Capture only commands outside the strict cache-safe allowlist.  This is
    // an executor-owned observation, not a parser for Python/SQLite/etc.; an
    // unknown or compound command simply enters the bounded snapshot lane.
    let workspace_before = if observation_lease.is_some() {
        let root = workspace_root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(&root)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let external_lease = match astra_tools::workspace_observation::acquire_external_effect_observation_lease_with_options(
        args, workspace_root, cancel_token, Duration::from_secs_f64(timeout_secs.max(0.1)),
    ).await {
        Ok(lease) => lease,
        Err(reason) => return astra_tools::ToolResult::error(format!("Error: external state observation was not admitted: {reason}")),
    };
    if args
        .get(astra_tools::workspace_observation::EXTERNAL_STATE_PATHS_FIELD)
        .is_some()
        && external_lease.is_none()
    {
        return astra_tools::ToolResult::error("Error: external state observation lease is contended or unavailable; no command was run.".to_string());
    }
    let external_before = {
        let root = workspace_root.to_path_buf();
        let args = args.clone();
        match tokio::task::spawn_blocking(move || {
            astra_tools::workspace_observation::ExternalEffectFingerprint::capture_from_args(
                &args, &root,
            )
        })
        .await
        {
            Ok(Ok(before)) => before,
            Ok(Err(reason)) => {
                return astra_tools::ToolResult::error(format!(
                    "Error: external state observation was not admitted: {reason}"
                ));
            }
            Err(error) => {
                return astra_tools::ToolResult::error(format!(
                    "Error: external state preimage worker failed: {error}"
                ));
            }
        }
    };
    if cancel_token.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return astra_tools::cancelled_tool_result("bash", false);
    }
    let wrapped_command = wrap_command_with_limits(sandbox_policy, command);

    let result = match server_bash_execution_mode(sandbox_policy) {
        ServerBashExecutionMode::IsolatedProcess => {
            let mut config = IsolationConfig::strict(workspace_root.to_path_buf());
            apply_policy_limits_to_isolation_config(&mut config, sandbox_policy, timeout_secs);
            config.net_namespace = !sandbox_policy.network_allowed;
            let env = server_process_environment(sandbox_policy, workspace_root);
            let output = crate::tool_sandbox::execute_isolated_with_cancel(
                &wrapped_command,
                &env,
                &config,
                cancel_token,
            )
            .await;
            let execution_started = output.execution_started;
            let scope_ownership = output.scope_ownership;
            if output.cancelled {
                astra_tools::cancelled_tool_result("bash", true)
                    .with_metadata_scope_settled(scope_ownership, execution_started)
            } else {
                tool_result_from_server_bash_output(command, output, timeout_secs)
                    .with_metadata_scope_settled(scope_ownership, execution_started)
            }
        }
        ServerBashExecutionMode::SandboxedProcess => {
            let mut config = IsolationConfig::sandboxed(workspace_root.to_path_buf());
            apply_policy_limits_to_isolation_config(&mut config, sandbox_policy, timeout_secs);
            let env = server_process_environment(sandbox_policy, workspace_root);
            let output = crate::tool_sandbox::execute_isolated_with_cancel(
                &wrapped_command,
                &env,
                &config,
                cancel_token,
            )
            .await;
            let execution_started = output.execution_started;
            let scope_ownership = output.scope_ownership;
            if output.cancelled {
                astra_tools::cancelled_tool_result("bash", true)
                    .with_metadata_scope_settled(scope_ownership, execution_started)
            } else {
                tool_result_from_server_bash_output(command, output, timeout_secs)
                    .with_metadata_scope_settled(scope_ownership, execution_started)
            }
        }
    };
    let workspace_after = if observation_lease.is_some() {
        let root = workspace_root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(&root)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    let scope_settled = result
        .metadata
        .as_ref()
        .and_then(|fields| fields.get("_astra_scope_settled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let scope_ownership = result
        .metadata
        .as_ref()
        .and_then(|fields| fields.get("_astra_scope_ownership"))
        .and_then(Value::as_str)
        .and_then(|value| match value {
            astra_tools::workspace_observation::INVOCATION_CGROUP_OWNERSHIP => {
                Some(astra_sandbox::ScopeOwnership::InvocationCgroup)
            }
            astra_tools::workspace_observation::FOREGROUND_PROCESS_GROUP_OWNERSHIP => {
                Some(astra_sandbox::ScopeOwnership::ForegroundProcessGroup)
            }
            astra_tools::workspace_observation::INVOCATION_SUPERVISOR_OWNERSHIP => {
                Some(astra_sandbox::ScopeOwnership::InvocationSupervisor)
            }
            _ => None,
        });
    let execution_started = result
        .metadata
        .as_ref()
        .and_then(|fields| fields.get("_astra_execution_started"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut result = result;
    if let Some(fields) = result.metadata.as_mut() {
        fields.remove("_astra_scope_settled");
        fields.remove("_astra_scope_ownership");
        fields.remove("_astra_execution_started");
    }
    let coordination_integrity_valid = observation_lease
        .as_ref()
        .is_none_or(astra_tools::workspace_observation::WorkspaceObservationLease::integrity_valid);
    let quarantine_weak_after_current = !nested_run_script_callback
        && astra_tools::shell_ops::bash_scope_requires_attribution_quarantine(
            command,
            scope_ownership,
        );
    let explicit_verification =
        astra_tools::workspace_observation::is_explicit_workspace_verification_request(
            "bash", args,
        );
    let mut result = attach_workspace_observation(
        result,
        workspace_root,
        workspace_before,
        workspace_after,
        execution_started,
        scope_settled,
        scope_ownership,
        coordination_integrity_valid,
        quarantine_weak_after_current,
        explicit_verification,
    );
    if let Some(receipt) = match external_before.as_ref() {
        Some(before)
            if external_lease
                .as_ref()
                .is_none_or(|lease| lease.integrity_valid()) =>
        {
            before
                .changed_receipt_async(scope_ownership.map(astra_sandbox::ScopeOwnership::as_str))
                .await
        }
        _ => None,
    } {
        result
            .metadata
            .get_or_insert_with(Default::default)
            .extend(receipt);
    }
    attach_source_preimage(result, source_preimages)
}

trait ScopeSettledResult {
    fn with_metadata_scope_settled(
        self,
        ownership: Option<astra_sandbox::ScopeOwnership>,
        execution_started: bool,
    ) -> Self;
}

impl ScopeSettledResult for astra_tools::ToolResult {
    fn with_metadata_scope_settled(
        mut self,
        ownership: Option<astra_sandbox::ScopeOwnership>,
        execution_started: bool,
    ) -> Self {
        let fields = self.metadata.get_or_insert_with(Default::default);
        fields.insert(
            "_astra_execution_started".to_string(),
            Value::Bool(execution_started),
        );
        fields.insert(
            "_astra_scope_settled".to_string(),
            Value::Bool(ownership.is_some()),
        );
        if let Some(ownership) = ownership {
            fields.insert(
                "_astra_scope_ownership".to_string(),
                Value::String(ownership.as_str().to_string()),
            );
        }
        self
    }
}

fn attach_workspace_observation(
    mut result: astra_tools::ToolResult,
    workspace_root: &std::path::Path,
    before: Option<astra_tools::workspace_observation::WorkspaceFingerprint>,
    after: Option<astra_tools::workspace_observation::WorkspaceFingerprint>,
    execution_started: bool,
    scope_settled: bool,
    scope_ownership: Option<astra_sandbox::ScopeOwnership>,
    coordination_integrity_valid: bool,
    quarantine_weak_after_current: bool,
    explicit_verification: bool,
) -> astra_tools::ToolResult {
    if !coordination_integrity_valid {
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(workspace_root);
        if let Some(fields) = result.metadata.as_mut() {
            fields.remove(astra_tools::workspace_observation::OBSERVED_FIELD);
            fields.remove(astra_tools::workspace_observation::SCOPE_FIELD);
            fields.remove(astra_tools::workspace_observation::RECEIPT_FIELD);
        }
        result.is_error = true;
        if !result.output.is_empty() {
            result.output.push_str("\n\n");
        }
        result.output.push_str(
            "Error: workspace binding or coordination generation changed during Bash execution; the command may have changed files, but no durable mutation receipt was issued. Re-bind and inspect the workspace before continuing.",
        );
        return result;
    }
    if execution_started && scope_ownership.is_none() {
        astra_tools::workspace_observation::mark_workspace_observation_unsettled(workspace_root);
        if let Some(fields) = result.metadata.as_mut() {
            fields.remove(astra_tools::workspace_observation::OBSERVED_FIELD);
            fields.remove(astra_tools::workspace_observation::SCOPE_FIELD);
            fields.remove(astra_tools::workspace_observation::RECEIPT_FIELD);
        }
        result.is_error = true;
        if !result.output.is_empty() {
            result.output.push_str("\n\n");
        }
        result.output.push_str(
            "Error: Bash execution started but its descendant ownership did not settle; the workspace is quarantined and no durable mutation receipt was issued.",
        );
        return result;
    }
    let before_available = before.is_some();
    let after_available = after.is_some();
    let workspace_changed = before
        .as_ref()
        .is_some_and(|before| before.changed_from(after));
    if before.filter(|_| scope_settled).is_some() && workspace_changed {
        if let Some(ownership) = scope_ownership {
            if ownership.is_authoritative() {
                result.metadata.get_or_insert_with(Default::default).extend(
                    astra_tools::workspace_observation::changed_receipt_with_ownership(
                        ownership.as_str(),
                    ),
                );
            } else {
                result.metadata.get_or_insert_with(Default::default).extend(
                    astra_tools::workspace_observation::changed_receipt_with_ownership(
                        ownership.as_str(),
                    ),
                );
                astra_tools::workspace_observation::quarantine_after_weak_receipt(
                    workspace_root,
                    Some(ownership.as_str()),
                );
            }
        } else {
            astra_tools::workspace_observation::quarantine_after_weak_receipt(workspace_root, None);
        }
    }
    // Preserve the current chain's truthful post-state before making weak
    // ownership quarantine sticky. A foreground process group cannot rule
    // out a setsid/double-fork descendant that writes after this function
    // returns, even when the immediate fingerprint delta is empty.
    if quarantine_weak_after_current {
        astra_tools::workspace_observation::quarantine_after_weak_receipt(
            workspace_root,
            scope_ownership.map(astra_sandbox::ScopeOwnership::as_str),
        );
    }
    let verify_receipt_valid = explicit_verification
        && !result.is_error
        && result
            .metadata
            .as_ref()
            .and_then(|fields| fields.get("exit_code"))
            .and_then(Value::as_i64)
            == Some(0)
        && before_available
        && after_available
        && !workspace_changed
        && scope_settled
        && scope_ownership.is_some_and(astra_sandbox::ScopeOwnership::is_authoritative);
    if verify_receipt_valid {
        result
            .metadata
            .get_or_insert_with(Default::default)
            .extend(astra_tools::workspace_observation::explicit_workspace_verification_receipt());
    } else if explicit_verification && !result.is_error {
        if !before_available || !after_available {
            result = result.with_failure_evidence(
                astra_tools::workspace_observation::explicit_workspace_verification_unavailable_evidence(),
            );
            result
                .metadata
                .get_or_insert_with(Default::default)
                .insert("retryable".to_string(), Value::Bool(false));
            result.metadata.get_or_insert_with(Default::default).insert(
                "workspace_observation_retry_scope".to_string(),
                Value::String("workspace_generation".to_string()),
            );
            if !result.output.is_empty() {
                result.output.push_str("\n\n");
            }
            result.output.push_str(
                astra_tools::workspace_observation::EXPLICIT_WORKSPACE_VERIFICATION_UNAVAILABLE_MESSAGE,
            );
        } else {
            result.is_error = true;
            result.output.push_str("\n\nError: verify-mode command did not produce an authoritative unchanged-workspace observation receipt.");
        }
    }
    result
}

fn attach_source_preimage(
    mut result: astra_tools::ToolResult,
    plan: Option<astra_tools::source_preimage::PreparedSourcePreimages>,
) -> astra_tools::ToolResult {
    if let Some(mut plan) = plan {
        let finished = plan.finish();
        if let Some(advisory) = astra_tools::source_preimage::advisory_text(&finished) {
            if !result.output.is_empty() {
                result.output.push_str("\n\n");
            }
            result.output.push_str(&advisory);
        }
        result
            .metadata
            .get_or_insert_with(Default::default)
            .extend(finished);
    }
    result
}

fn server_process_environment(
    sandbox_policy: &SandboxPolicy,
    workspace_root: &Path,
) -> HashMap<String, String> {
    let mut environment = filter_environment(sandbox_policy);
    harden_server_process_environment(&mut environment, workspace_root);
    environment
}

fn harden_server_process_environment(
    environment: &mut HashMap<String, String>,
    workspace_root: &Path,
) {
    // A multi-tenant Server process never lends its host login, SSH agent, or
    // credential-helper configuration to workspace commands. Owner-scoped
    // credentials must arrive through a separately bound provider.
    for key in [
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
        "SSH_ASKPASS",
        "GIT_ASKPASS",
        "GIT_SSH",
        "GIT_SSH_COMMAND",
        "GH_CONFIG_DIR",
        "GNUPGHOME",
        "GPG_AGENT_INFO",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_STATE_HOME",
    ] {
        environment.remove(key);
    }
    environment.retain(|key, _| {
        key != "GIT_CONFIG_COUNT"
            && !key.starts_with("GIT_CONFIG_KEY_")
            && !key.starts_with("GIT_CONFIG_VALUE_")
    });

    let isolated_home = workspace_root.join(".astra-server-home");
    environment.insert("HOME".to_string(), isolated_home.display().to_string());
    environment.insert(
        "XDG_CONFIG_HOME".to_string(),
        isolated_home.join(".config").display().to_string(),
    );
    environment.insert("GIT_CONFIG_GLOBAL".to_string(), "/dev/null".to_string());
    environment.insert("GIT_CONFIG_NOSYSTEM".to_string(), "1".to_string());
    environment.insert("GIT_TERMINAL_PROMPT".to_string(), "0".to_string());
    environment.insert("GCM_INTERACTIVE".to_string(), "never".to_string());
    environment.insert("GH_PROMPT_DISABLED".to_string(), "true".to_string());
    environment.insert(
        "GIT_SSH_COMMAND".to_string(),
        "ssh -o BatchMode=yes -o IdentityAgent=none -o IdentitiesOnly=yes".to_string(),
    );
}

fn apply_policy_limits_to_isolation_config(
    config: &mut IsolationConfig,
    sandbox_policy: &SandboxPolicy,
    timeout_secs: f64,
) {
    config.timeout = Duration::from_secs_f64(timeout_secs);
    config.max_output_bytes = sandbox_policy.max_output_bytes;
}

pub(crate) fn tool_result_from_server_bash_output(
    command: &str,
    output: IsolatedOutput,
    timeout_secs: f64,
) -> astra_tools::ToolResult {
    let descendants_terminated = output.descendants_terminated;
    let mut body = format_server_bash_output(&output, timeout_secs);
    if descendants_terminated {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(
            "\n⚠ Live descendant processes were terminated when this foreground bash call ended; \
             they are not running now. This executor provides no process-persistence guarantee.",
        );
    }
    if output.timed_out {
        return tool_timeout_tool_result(format!("Error: {body}"));
    }
    let semantics = output.exit_code.map(|code| classify_exit(command, code));
    let result_class =
        classify_command_result(command, &output.stdout, &output.stderr, output.exit_code);
    let command_failed = output.exit_code.is_some_and(|code| code != 0)
        && semantics.is_some_and(|semantics| semantics.is_tool_error())
        || result_class.is_tool_error()
        || output.exit_code.is_none() && output.stdout.is_empty() && !output.stderr.is_empty();
    let mut result = if command_failed {
        astra_tools::ToolResult::error(format!("Error: {body}"))
    } else {
        astra_tools::ToolResult::text(body)
    };
    if command_failed {
        result =
            result.with_failure_evidence(astra_tools::exit_semantics::command_failed_evidence());
    }
    if let Some(semantics) = semantics {
        result = result.with_exit_semantics(semantics);
    }
    result = result.with_result_class(result_class);
    if let Some(exit_code) = output.exit_code {
        result = result.with_exit_code(exit_code);
    }
    if descendants_terminated {
        let metadata = result.metadata.get_or_insert_with(serde_json::Map::new);
        metadata.insert("background_children_reaped".to_string(), Value::Bool(true));
        metadata.insert("descendant_persistence".to_string(), Value::Bool(false));
    }
    result
}

pub(crate) fn format_server_bash_output(output: &IsolatedOutput, timeout_secs: f64) -> String {
    let mut body = String::new();
    if !output.stdout.is_empty() {
        body.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str("stderr:\n");
        body.push_str(&output.stderr);
    }
    if let Some(code) = output.exit_code {
        if code != 0 {
            if !body.is_empty() && !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(&format!("(exit code: {code})"));
        }
    }
    if output.stdout_capped || output.stderr_capped {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!(
            "[output capped: {} limit reached]",
            capped_streams_label(output.stdout_capped, output.stderr_capped)
        ));
    }

    if output.timed_out {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str(&format!(
            "[bash timed out after {}; partial output shown]",
            format_timeout_seconds(timeout_secs)
        ));
    }

    body
}

fn capped_streams_label(stdout_capped: bool, stderr_capped: bool) -> &'static str {
    match (stdout_capped, stderr_capped) {
        (true, true) => "stdout, stderr",
        (true, false) => "stdout",
        (false, true) => "stderr",
        (false, false) => "output",
    }
}

fn format_timeout_seconds(timeout_secs: f64) -> String {
    let mut text = format!("{timeout_secs:.3}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    format!("{text}s")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::tool_transport_metadata::TOOL_ERROR_KIND_TOOL_TIMEOUT;

    #[test]
    fn server_bash_execution_mode_is_explicit_from_policy_isolation() {
        assert_eq!(
            server_bash_execution_mode(&SandboxPolicy::permissive("/workspace")),
            ServerBashExecutionMode::SandboxedProcess
        );
        assert_eq!(
            server_bash_execution_mode(&SandboxPolicy::for_project("/workspace")),
            ServerBashExecutionMode::SandboxedProcess
        );
        assert_eq!(
            server_bash_execution_mode(&SandboxPolicy::strict("/workspace")),
            ServerBashExecutionMode::IsolatedProcess
        );
    }

    #[tokio::test]
    async fn server_bash_rejects_unadvertised_managed_background_fields() {
        let workspace = tempfile::tempdir().expect("workspace");
        let policy = SandboxPolicy::permissive(workspace.path());
        let binding = WorkspaceBinding::server_sandbox(workspace.path());
        let result = execute_server_bash(
            &policy,
            workspace.path(),
            &binding,
            None,
            &serde_json::json!({
                "command": "printf should-not-run",
                "run_in_background": true,
                "ready_check": "true"
            }),
            None,
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("unavailable"));
        assert!(!result.output.contains("should-not-run"));
    }

    #[test]
    fn server_process_environment_never_inherits_host_vcs_identity() {
        let mut environment = HashMap::from([
            ("HOME".to_string(), "/home/server".to_string()),
            ("SSH_AUTH_SOCK".to_string(), "/run/host-agent".to_string()),
            ("SSH_AGENT_PID".to_string(), "41".to_string()),
            ("GIT_ASKPASS".to_string(), "/opt/host-askpass".to_string()),
            (
                "GH_CONFIG_DIR".to_string(),
                "/home/server/.config/gh".to_string(),
            ),
            ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
            (
                "GIT_CONFIG_KEY_0".to_string(),
                "credential.helper".to_string(),
            ),
            (
                "GIT_CONFIG_VALUE_0".to_string(),
                "host-keychain".to_string(),
            ),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ]);

        harden_server_process_environment(&mut environment, Path::new("/work/owner-session"));

        assert_eq!(environment["PATH"], "/usr/bin");
        assert_eq!(
            environment["HOME"],
            "/work/owner-session/.astra-server-home"
        );
        for key in [
            "SSH_AUTH_SOCK",
            "SSH_AGENT_PID",
            "GIT_ASKPASS",
            "GH_CONFIG_DIR",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_KEY_0",
            "GIT_CONFIG_VALUE_0",
        ] {
            assert!(
                !environment.contains_key(key),
                "{key} must not cross tenants"
            );
        }
        assert_eq!(environment["GIT_CONFIG_GLOBAL"], "/dev/null");
        assert_eq!(environment["GIT_CONFIG_NOSYSTEM"], "1");
        assert_eq!(environment["GIT_TERMINAL_PROMPT"], "0");
        assert!(environment["GIT_SSH_COMMAND"].contains("IdentityAgent=none"));
    }

    fn successful_bash_result() -> astra_tools::ToolResult {
        let mut result = astra_tools::ToolResult::text("ok".into());
        result.metadata = Some(serde_json::Map::from_iter([(
            "exit_code".to_string(),
            Value::from(0),
        )]));
        result
    }

    #[test]
    fn unavailable_server_verify_is_typed_and_never_mints_receipt() {
        let workspace = tempfile::tempdir().expect("workspace");
        let result = attach_workspace_observation(
            successful_bash_result(),
            workspace.path(),
            None,
            None,
            false,
            true,
            Some(astra_sandbox::ScopeOwnership::InvocationCgroup),
            true,
            false,
            true,
        );
        assert!(result.is_error);
        let fields = result.metadata.expect("typed failure");
        assert_eq!(fields["error_kind"], "tool_unavailable");
        assert_eq!(fields["retryable"], false);
        assert!(
            fields
                .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
                .is_none()
        );
    }

    #[test]
    fn stable_authoritative_server_verify_mints_v2_receipt() {
        let workspace = tempfile::tempdir().expect("workspace");
        let git_init = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()
            .expect("git installed");
        assert!(git_init.success(), "initialize workspace repository");
        let before =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("before");
        let after =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("after");
        let result = attach_workspace_observation(
            successful_bash_result(),
            workspace.path(),
            Some(before),
            Some(after),
            true,
            true,
            Some(astra_sandbox::ScopeOwnership::InvocationCgroup),
            true,
            false,
            true,
        );
        let fields = result.metadata.expect("receipt");
        let receipt = fields
            .get(astra_tools::workspace_observation::OBSERVATION_RECEIPT_FIELD)
            .unwrap_or_else(|| panic!("missing verify receipt: {fields:?}"));
        assert!(
            astra_tools::workspace_observation::is_explicit_workspace_verification_receipt(receipt)
        );
    }

    #[test]
    fn weak_mutation_capable_no_delta_quarantines_future_attribution() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("before fingerprint");
        let after =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("after fingerprint");
        let delayed_root = workspace.path().to_path_buf();
        let delayed_writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            std::fs::write(delayed_root.join("escaped-late.txt"), "late mutation")
                .expect("delayed escaped writer");
        });

        let result = attach_workspace_observation(
            astra_tools::ToolResult::text(
                "foreground command returned before escaped writer".into(),
            ),
            workspace.path(),
            Some(before),
            Some(after),
            true,
            true,
            Some(astra_sandbox::ScopeOwnership::ForegroundProcessGroup),
            true,
            true,
            false,
        );

        assert!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get(astra_tools::workspace_observation::RECEIPT_FIELD))
                .is_none(),
            "an unchanged current chain must not invent a mutation receipt"
        );
        assert_eq!(
            astra_tools::workspace_observation::workspace_observation_is_quarantined(
                workspace.path(),
            ),
            Some(true),
            "weak mutation-capable ownership must quarantine after the current observation even when its immediate delta is empty"
        );
        delayed_writer.join().expect("delayed writer thread");
        assert!(workspace.path().join("escaped-late.txt").is_file());
        assert!(
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .is_none(),
            "a later invocation must not misattribute an escaped post-return write to its own observation window"
        );
    }

    #[test]
    fn started_server_bash_without_settled_ownership_quarantines_before_delayed_daemon_write() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("before fingerprint");
        let after =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("unchanged immediate fingerprint");
        let delayed_root = workspace.path().to_path_buf();
        let delayed_daemon = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            std::fs::write(delayed_root.join("server-daemon-late.txt"), "late")
                .expect("delayed daemon write");
        });

        let result = attach_workspace_observation(
            astra_tools::ToolResult::text("supervisor disappeared after spawn".into()),
            workspace.path(),
            Some(before),
            Some(after),
            true,
            false,
            None,
            true,
            false,
            false,
        );

        assert!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get(astra_tools::workspace_observation::RECEIPT_FIELD))
                .is_none(),
            "unsettled ownership cannot mint a current receipt"
        );
        assert_eq!(
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(workspace.path()),
            Some(true),
            "a started process with no settled ownership must be terminally quarantined even before any delta"
        );
        delayed_daemon.join().expect("delayed daemon");
        assert!(
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .is_none(),
            "the daemon's delayed write cannot be attributed to a later invocation"
        );
    }

    #[test]
    fn pre_spawn_server_bash_refusal_does_not_quarantine_workspace() {
        let workspace = tempfile::tempdir().expect("workspace");
        let result = attach_workspace_observation(
            astra_tools::ToolResult::error("sandbox preflight refused execution".into()),
            workspace.path(),
            None,
            None,
            false,
            false,
            None,
            true,
            false,
            false,
        );

        assert!(result.is_error);
        assert_eq!(
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(workspace.path()),
            Some(false),
            "the executor-owned started bit must distinguish a safe pre-spawn refusal"
        );
    }

    #[test]
    fn bash_isolation_config_consumes_policy_limits() {
        let mut policy = SandboxPolicy::strict("/workspace");
        policy.max_execution_secs = 2.5;
        policy.max_output_bytes = 1234;
        let requested_timeout = 10.0_f64.min(policy.max_execution_secs);
        let mut config = IsolationConfig::strict(std::path::PathBuf::from("/workspace"));

        apply_policy_limits_to_isolation_config(&mut config, &policy, requested_timeout);

        assert_eq!(config.timeout, Duration::from_secs_f64(2.5));
        assert_eq!(config.max_output_bytes, 1234);
    }

    #[test]
    fn bash_timeout_returns_partial_output() {
        let output = IsolatedOutput {
            stdout: "start\n".into(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
        let result = format_server_bash_output(&output, 0.2);
        assert!(result.contains("start"), "got: {result}");
        assert!(result.contains("timed out after 0.2s"), "got: {result}");
        assert!(!result.contains("done"), "got: {result}");
    }

    #[test]
    fn bash_timeout_sets_error_metadata() {
        let output = IsolatedOutput {
            stdout: "start\n".into(),
            stderr: String::new(),
            exit_code: None,
            timed_out: true,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
        let result = tool_result_from_server_bash_output("sleep 10", output, 0.2);
        assert!(result.is_error, "got: {}", result.output);
        assert!(result.output.contains("start"), "got: {}", result.output);
        assert!(result.output.contains("timed out after 0.2s"));
        let metadata = result.metadata.expect("tool timeout metadata");
        assert_eq!(metadata["error_kind"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert_eq!(metadata["reason"], TOOL_ERROR_KIND_TOOL_TIMEOUT);
        assert!(metadata.get("blocked").is_none(), "{metadata:?}");
    }

    #[test]
    fn bash_domain_negative_exit_keeps_non_error_semantics() {
        let output = IsolatedOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(1),
            timed_out: false,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
        let result = tool_result_from_server_bash_output("test -f missing", output, 0.2);
        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.exit_semantics,
            Some(astra_tools::exit_semantics::ExitSemantics::DomainNegative)
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("exit_code"))
                .and_then(serde_json::Value::as_i64),
            Some(1)
        );
        assert!(result.output.contains("(exit code: 1)"), "{result:?}");
    }

    #[test]
    fn bash_command_failure_preserves_typed_recovery_evidence() {
        let output = IsolatedOutput {
            stdout: "partial output\n".into(),
            stderr: String::new(),
            exit_code: Some(2),
            timed_out: false,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: false,
            scope_ownership: None,
            descendants_terminated: false,
        };
        let result = tool_result_from_server_bash_output("ls missing", output, 0.2);
        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.expect("command failure metadata");
        assert_eq!(metadata["error_kind"], "unknown");
        assert_eq!(metadata["recovery_evidence"]["cause"], "command_failed");
        assert_eq!(metadata["exit_semantics"], "execution_error");
    }

    #[test]
    fn server_bash_surfaces_executor_observed_descendant_reaping() {
        let output = IsolatedOutput {
            stdout: "ready\n".into(),
            stderr: String::new(),
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            execution_started: true,
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
            scope_settled: true,
            scope_ownership: Some(astra_sandbox::ScopeOwnership::ForegroundProcessGroup),
            descendants_terminated: true,
        };
        let result = tool_result_from_server_bash_output("service-start", output, 1.0);
        assert!(!result.is_error, "{result:?}");
        assert!(result.output.contains("no process-persistence guarantee"));
        let metadata = result.metadata.expect("typed descendant settlement");
        assert_eq!(metadata["background_children_reaped"], true);
        assert_eq!(metadata["descendant_persistence"], false);
    }

    #[test]
    fn command_length_exceeds_limit_returns_error() {
        let command = "x".repeat(MAX_COMMAND_LENGTH + 1);
        assert!(command.len() > MAX_COMMAND_LENGTH);
        assert_eq!(MAX_COMMAND_LENGTH, 100 * 1024);
    }

    #[test]
    fn workspace_observation_attaches_receipt_even_after_partial_failure() {
        let workspace = tempfile::tempdir().expect("workspace");
        let before =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("workspace fingerprint");
        std::fs::write(workspace.path().join("generated.txt"), "partial").unwrap();
        let after =
            astra_tools::workspace_observation::WorkspaceFingerprint::capture(workspace.path())
                .expect("workspace fingerprint");

        let result = attach_workspace_observation(
            astra_tools::ToolResult::error("writer failed after partial output".into()),
            workspace.path(),
            Some(before),
            Some(after),
            true,
            true,
            Some(astra_sandbox::ScopeOwnership::InvocationCgroup),
            true,
            false,
            false,
        );
        let metadata = result.metadata.expect("typed receipt");
        assert!(result.is_error);
        assert_eq!(
            metadata[astra_tools::workspace_observation::OBSERVED_FIELD],
            true
        );
        assert_eq!(
            metadata[astra_tools::workspace_observation::SCOPE_FIELD],
            astra_tools::workspace_observation::BOUND_WORKSPACE_SCOPE
        );
    }
}
