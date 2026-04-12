use astra_runtime::tool_registry::ToolChain;
use astra_runtime::turn::cloud_approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind};
use astra_runtime::turn::stall::{
    SERVER_STALL_WINDOW, detect_server_stall, record_server_tool_signatures,
};
use serde_json::{Value, json};

pub(crate) const MAX_RUN_CHAIN_STEPS: usize = 16;
pub(crate) const MAX_RUN_CHAIN_MUTATING_STEPS: usize = 8;
const DESTRUCTIVE_KEYWORDS: &[&str] = &["DROP", "DELETE", "TRUNCATE", "ALTER", "GRANT", "REVOKE"];

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
        match name {
            "mo_query" => Self::check_mo_query(args),
            _ => Ok(()),
        }
    }

    pub(crate) fn check_mo_query(args: &Value) -> Result<(), String> {
        let Some(sql) = args.get("sql").and_then(Value::as_str) else {
            return Ok(());
        };
        let allow_destructive = args
            .get("allow_destructive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allow_destructive && let Some(kind) = check_sql_safety(sql) {
            return Err(format!(
                "Error: {kind} statements are blocked by default. Pass \"allow_destructive\": true to confirm execution."
            ));
        }
        Ok(())
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

pub(crate) fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            for ch in chars.by_ref() {
                if ch == '\n' {
                    out.push(' ');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut depth = 1u32;
            while depth > 0 {
                match chars.next() {
                    Some('/') if chars.peek() == Some(&'*') => {
                        chars.next();
                        depth += 1;
                    }
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        depth -= 1;
                    }
                    None => break,
                    _ => {}
                }
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

pub(crate) fn check_sql_safety(sql: &str) -> Option<&'static str> {
    let stripped = strip_sql_comments(sql).to_uppercase();
    for stmt in stripped.split(';') {
        let first_word = stmt.split_whitespace().next().unwrap_or("");
        for &kw in DESTRUCTIVE_KEYWORDS {
            if first_word == kw {
                return Some(kw);
            }
        }
    }
    None
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
}
