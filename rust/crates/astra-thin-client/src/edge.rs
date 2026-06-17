//! Lightweight **edge executor** helpers — transport + local tools only (design §5.5.2).
//!
//! An edge process should depend on [`crate::ThinClient`] and local tool execution, not on
//! `astra` / `runtime` / cognitive pipelines.

use crate::protocol::{ChatStreamRequest, EdgeRegisterRequest};
use astra_runtime_env::{
    ExecutorBinding, PolicyIntent, RunBinding, RuntimeBinding, RuntimeEnvironmentAdvertisement,
    ToolRegistry, WorkspaceAuthority, WorkspaceBinding,
};
use serde_json::Value;

/// HTTP header matching design doc §5.5 (`POST /tools/result`).
pub const ASTRA_EDGE_ID_HEADER: &str = "X-Astra-Edge-Id";

/// Default `capabilities` tags for a full local toolkit (coarse buckets; server may refine).
///
/// Aligns with `multi-agent-cloud-runtime.md` chat example
/// `["bash", "fs", "git", "code_intel"]`.
pub fn builtin_capability_preset() -> Vec<String> {
    vec![
        "bash".into(),
        "fs".into(),
        "git".into(),
        "code_intel".into(),
    ]
}

/// Set `edge_executor_id` and, if `capabilities` is empty, fill [`builtin_capability_preset`].
pub fn advertise_executor(req: &mut ChatStreamRequest, executor_id: impl Into<String>) {
    req.edge_executor_id = Some(executor_id.into());
    if req.capabilities.is_empty() {
        req.capabilities = builtin_capability_preset();
    }
}

/// Structured runtime-env advertisement for `POST /agents/edge`.
pub fn edge_runtime_environment_capabilities(
    executor_id: impl AsRef<str>,
    worktree_path: impl AsRef<str>,
) -> Value {
    let registry = ToolRegistry::builtins();
    let edge_agent_id = executor_id.as_ref();
    let binding = RunBinding::resolve(
        WorkspaceBinding::edge_workspace(
            worktree_path.as_ref().to_string(),
            WorkspaceAuthority::ReadWrite,
        ),
        ExecutorBinding::edge_agent(edge_agent_id.to_string()),
        RuntimeBinding::host_process(format!("edge-host:{edge_agent_id}")),
        PolicyIntent::local_developer(),
        &registry,
    );

    serde_json::to_value(RuntimeEnvironmentAdvertisement::new(binding))
        .expect("runtime environment advertisement serializes")
}

/// [`EdgeRegisterRequest`] with a structured runtime-env capability advertisement.
pub fn edge_register_with_capabilities(
    executor_id: impl Into<String>,
    worktree_path: impl Into<String>,
) -> EdgeRegisterRequest {
    let executor_id = executor_id.into();
    let worktree_path = worktree_path.into();
    let mut r = EdgeRegisterRequest::new(executor_id.clone());
    r.worktree_path = Some(worktree_path.clone());
    r.capabilities = Some(edge_runtime_environment_capabilities(
        &executor_id,
        &worktree_path,
    ));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertise_executor_fills_defaults() {
        let mut r = ChatStreamRequest::new("hi");
        advertise_executor(&mut r, "edge-test");
        assert_eq!(r.edge_executor_id.as_deref(), Some("edge-test"));
        assert_eq!(r.capabilities, builtin_capability_preset());
    }

    #[test]
    fn advertise_executor_respects_existing_capabilities() {
        let mut r = ChatStreamRequest::new("hi");
        r.capabilities = vec!["bash".into()];
        advertise_executor(&mut r, "e1");
        assert_eq!(r.capabilities, vec!["bash"]);
    }

    #[test]
    fn edge_register_with_capabilities_json() {
        let r = edge_register_with_capabilities("my-edge", "/workspace/app");
        assert_eq!(r.edge_agent_id, "my-edge");
        assert_eq!(r.worktree_path.as_deref(), Some("/workspace/app"));
        let capabilities = r.capabilities.as_ref().unwrap();
        assert_eq!(
            capabilities["schema_version"],
            RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        );
        assert_eq!(
            capabilities["binding"]["workspace"]["kind"],
            "edge_workspace"
        );
        assert_eq!(
            capabilities["binding"]["workspace"]["cwd"],
            "/workspace/app"
        );
        assert_eq!(
            capabilities["binding"]["executor"]["executor_id"],
            "my-edge"
        );
        assert_eq!(
            capabilities["binding"]["capabilities"]["runtime"]["runtime_has_git"],
            true
        );
    }
}
