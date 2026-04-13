use astra_runtime::tool_registry::ToolChain;
use astra_runtime::turn::cloud_approval_policy::{
    CloudGatedToolKind, bash_command_is_read_only, cloud_gated_tool_kind,
};
use astra_runtime::turn::safety_middleware::{
    SafetyMiddlewareDecision, evaluate_tool_safety_request,
};
use astra_runtime::turn::stall::{
    SERVER_STALL_WINDOW, detect_server_stall, record_server_tool_signatures,
};
use serde_json::{Value, json};

pub(crate) const MAX_RUN_CHAIN_STEPS: usize = 16;
pub(crate) const MAX_RUN_CHAIN_MUTATING_STEPS: usize = 8;
pub(crate) use astra_runtime::turn::safety_middleware::check_sql_safety;
#[cfg(test)]
pub(crate) use astra_runtime::turn::safety_middleware::strip_sql_comments;

pub(crate) struct ToolSafetyGuard;

impl ToolSafetyGuard {
    pub(crate) fn check_request(
        perm_manager: Option<&mut crate::permission_manager::PermissionManager>,
        name: &str,
        args: &Value,
    ) -> crate::permission_manager::PermissionDecision {
        if let Err(error) = Self::check_dispatch(name, args) {
            return crate::permission_manager::PermissionDecision::Deny(error);
        }
        match perm_manager {
            Some(pm) => pm.check_nonblocking(name, args),
            None => crate::permission_manager::PermissionDecision::Allow,
        }
    }

    pub(crate) fn check_dispatch(name: &str, args: &Value) -> Result<(), String> {
        match evaluate_tool_safety_request(name, args) {
            SafetyMiddlewareDecision::Allow => Ok(()),
            SafetyMiddlewareDecision::Deny(reason) => Err(reason),
        }
    }

    pub(crate) fn check_chain(chain: &ToolChain) -> Result<(), String> {
        if chain.steps.len() > MAX_RUN_CHAIN_STEPS {
            return Err(format!(
                "Error: run_chain exceeds the safety limit of {MAX_RUN_CHAIN_STEPS} steps. Split it into smaller chains."
            ));
        }

        if chain.steps.iter().any(|step| step.tool == "run_chain") {
            return Err(
                "Error: recursive run_chain steps are blocked by the safety guard. Inline the child steps instead."
                    .to_string(),
            );
        }

        let mutating_steps = chain
            .steps
            .iter()
            .filter(|step| is_mutating_tool(&step.tool))
            .count();
        if mutating_steps > MAX_RUN_CHAIN_MUTATING_STEPS {
            return Err(format!(
                "Error: run_chain exceeds the safety limit of {MAX_RUN_CHAIN_MUTATING_STEPS} write/execute steps. Split it into smaller batches."
            ));
        }

        if chain.rollback_on_failure
            && let Some(command) = chain.steps.iter().find_map(|step| {
                (step.tool == "bash")
                    .then(|| step.args.get("command").and_then(Value::as_str))
                    .flatten()
                    .map(str::trim)
                    .filter(|command| !command.is_empty() && !bash_command_is_read_only(command))
            })
        {
            return Err(format!(
                "Error: run_chain rollback_on_failure only supports read-only bash steps. `{command}` is mutating. Use structured mutation tools (write_file, git_*, rollback-aware editors), use run_build_test for build/test loops when available, or keep bash read-only inside the chain."
            ));
        }

        let mut tool_sigs = Vec::new();
        for step in &chain.steps {
            let tool_call = json!({
                "name": step.tool,
                "arguments": step.args,
            });
            record_server_tool_signatures(
                &mut tool_sigs,
                std::slice::from_ref(&tool_call),
                SERVER_STALL_WINDOW,
            );
            if detect_server_stall(&tool_sigs, SERVER_STALL_WINDOW) {
                return Err(format!(
                    "Error: run_chain repeats the same step {SERVER_STALL_WINDOW} times in a row and was blocked as a likely stall."
                ));
            }
        }

        Ok(())
    }
}

fn is_mutating_tool(name: &str) -> bool {
    matches!(
        cloud_gated_tool_kind(name),
        Some(CloudGatedToolKind::Write | CloudGatedToolKind::Execute)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime::tool_registry::ToolChain;

    #[test]
    fn chain_guard_blocks_recursive_run_chain() {
        let chain = ToolChain::new("outer", "outer").step(
            "run_chain",
            json!({
                "name": "inner",
                "description": "inner",
                "steps": [],
            }),
        );

        let error = ToolSafetyGuard::check_chain(&chain).unwrap_err();
        assert!(error.contains("recursive run_chain"));
    }

    #[test]
    fn chain_guard_blocks_identical_step_stall() {
        let chain = ToolChain::new("stall", "stall")
            .step("read_file", json!({"path": "same.txt"}))
            .step("read_file", json!({"path": "same.txt"}))
            .step("read_file", json!({"path": "same.txt"}));

        let error = ToolSafetyGuard::check_chain(&chain).unwrap_err();
        assert!(error.contains("likely stall"));
    }

    #[test]
    fn chain_guard_blocks_mutating_step_burst() {
        let mut chain = ToolChain::new("writes", "writes");
        for idx in 0..=MAX_RUN_CHAIN_MUTATING_STEPS {
            chain = chain.step(
                "write_file",
                json!({"path": format!("file-{idx}.txt"), "content": "x"}),
            );
        }

        let error = ToolSafetyGuard::check_chain(&chain).unwrap_err();
        assert!(error.contains("write/execute steps"));
    }

    #[test]
    fn chain_guard_blocks_mutating_bash_inside_rollback_on_failure_chain() {
        let chain = ToolChain::new("rollback-bash", "rollback bash")
            .with_rollback_on_failure(true)
            .step("bash", json!({"command": "mkdir unsafe-dir"}));

        let error = ToolSafetyGuard::check_chain(&chain).unwrap_err();
        assert!(error.contains("rollback_on_failure only supports read-only bash steps"));
        assert!(error.contains("mkdir unsafe-dir"));
    }

    #[test]
    fn chain_guard_allows_read_only_bash_inside_rollback_on_failure_chain() {
        let chain = ToolChain::new("rollback-bash-ro", "rollback bash ro")
            .with_rollback_on_failure(true)
            .step("bash", json!({"command": "pwd"}));

        ToolSafetyGuard::check_chain(&chain).expect("read-only bash should be allowed");
    }

    #[test]
    fn check_request_static_denial_overrides_auto_mode() {
        let mut pm = crate::permission_manager::PermissionManager::new(true);
        let decision = ToolSafetyGuard::check_request(
            Some(&mut pm),
            "mo_query",
            &json!({"sql": "DROP TABLE users"}),
        );

        match decision {
            crate::permission_manager::PermissionDecision::Deny(reason) => {
                assert!(reason.contains("blocked by default"));
            }
            other => panic!("expected deny, got: {other:?}"),
        }
    }

    #[test]
    fn check_request_delegates_safe_write_tool_to_permission_manager() {
        let mut pm = crate::permission_manager::PermissionManager::new(false);
        let decision = ToolSafetyGuard::check_request(
            Some(&mut pm),
            "write_file",
            &json!({"path": "file.txt", "content": "x"}),
        );

        assert!(matches!(
            decision,
            crate::permission_manager::PermissionDecision::NeedApproval { .. }
        ));
    }

    #[test]
    fn check_request_blocks_obfuscated_shell_command() {
        let decision =
            ToolSafetyGuard::check_request(None, "bash", &json!({"command": "eval \"$PAYLOAD\""}));

        match decision {
            crate::permission_manager::PermissionDecision::Deny(reason) => {
                assert!(reason.contains("shell_obfuscation"));
                assert!(reason.contains("eval"));
            }
            other => panic!("expected deny, got: {other:?}"),
        }
    }
}
