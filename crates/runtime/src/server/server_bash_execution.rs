use std::path::Path;
use std::time::Duration;

use astra_tools::ToolExecutor;
use astra_tools::executor::DefaultToolExecutor;
use astra_tools::exit_semantics::{classify_command_result, classify_exit};
use serde_json::Value;

use super::tool_execution_binding::WorkspaceBinding;
use super::tool_execution_result::{tool_timeout_tool_result, workspace_path_mismatch_tool_result};
use super::tool_workspace_path_guard::server_sandbox_local_path_mismatch;
use crate::tool_sandbox::{
    IsolatedOutput, IsolationConfig, IsolationLevel, SandboxPolicy, execute_isolated,
    filter_environment, wrap_command_with_limits,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerBashExecutionMode {
    DelegateToDefaultExecutor,
    SandboxedProcess,
    IsolatedProcess,
}

/// Maximum allowed length for a bash command string (100 KB).
pub(crate) const MAX_COMMAND_LENGTH: usize = 100 * 1024;

pub(crate) fn server_bash_execution_mode(policy: &SandboxPolicy) -> ServerBashExecutionMode {
    match policy.isolation {
        IsolationLevel::Permissive => ServerBashExecutionMode::DelegateToDefaultExecutor,
        IsolationLevel::Standard => ServerBashExecutionMode::SandboxedProcess,
        IsolationLevel::Strict => ServerBashExecutionMode::IsolatedProcess,
    }
}

pub(crate) async fn execute_server_bash(
    default_executor: &DefaultToolExecutor,
    sandbox_policy: &SandboxPolicy,
    workspace_root: &Path,
    workspace_binding: &WorkspaceBinding,
    args: &Value,
) -> astra_tools::ToolResult {
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

    let timeout_secs = args
        .get("timeout")
        .and_then(|value| value.as_f64())
        .unwrap_or(30.0)
        .min(sandbox_policy.max_execution_secs);
    let wrapped_command = wrap_command_with_limits(sandbox_policy, command);

    match server_bash_execution_mode(sandbox_policy) {
        ServerBashExecutionMode::IsolatedProcess => {
            let mut config = IsolationConfig::strict(workspace_root.to_path_buf());
            apply_policy_limits_to_isolation_config(&mut config, sandbox_policy, timeout_secs);
            config.net_namespace = !sandbox_policy.network_allowed;
            let env = filter_environment(sandbox_policy);
            let output = execute_isolated(&wrapped_command, &env, &config).await;
            tool_result_from_server_bash_output(command, output, timeout_secs)
        }
        ServerBashExecutionMode::SandboxedProcess => {
            let mut config = IsolationConfig::sandboxed(workspace_root.to_path_buf());
            apply_policy_limits_to_isolation_config(&mut config, sandbox_policy, timeout_secs);
            let env = filter_environment(sandbox_policy);
            let output = execute_isolated(&wrapped_command, &env, &config).await;
            tool_result_from_server_bash_output(command, output, timeout_secs)
        }
        ServerBashExecutionMode::DelegateToDefaultExecutor => {
            default_executor.execute("bash", args).await
        }
    }
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
    let body = format_server_bash_output(&output, timeout_secs);
    if output.timed_out {
        return tool_timeout_tool_result(format!("Error: {body}"));
    }
    let semantics = output.exit_code.map(|code| classify_exit(command, code));
    let result_class =
        classify_command_result(command, &output.stdout, &output.stderr, output.exit_code);
    let mut result = if output.exit_code.is_some_and(|code| code != 0)
        && semantics.is_some_and(|semantics| semantics.is_tool_error())
        || result_class.is_tool_error()
        || output.exit_code.is_none() && output.stdout.is_empty() && !output.stderr.is_empty()
    {
        astra_tools::ToolResult::error(format!("Error: {body}"))
    } else {
        astra_tools::ToolResult::text(body)
    };
    if let Some(semantics) = semantics {
        result = result.with_exit_semantics(semantics);
    }
    result = result.with_result_class(result_class);
    if let Some(exit_code) = output.exit_code {
        result = result.with_exit_code(exit_code);
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
            ServerBashExecutionMode::DelegateToDefaultExecutor
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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
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
            stdout_capped: false,
            stderr_capped: false,
            namespace_active: false,
            cgroup_active: false,
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
    fn command_length_exceeds_limit_returns_error() {
        let command = "x".repeat(MAX_COMMAND_LENGTH + 1);
        assert!(command.len() > MAX_COMMAND_LENGTH);
        assert_eq!(MAX_COMMAND_LENGTH, 100 * 1024);
    }
}
