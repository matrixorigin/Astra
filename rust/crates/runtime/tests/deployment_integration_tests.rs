use astra_runtime::deployment::DeploymentProfile;
use astra_runtime::provider::types::ToolCapability;

fn tool_names(profile: &DeploymentProfile) -> Vec<String> {
    profile
        .providers
        .iter()
        .flat_map(|p| &p.capabilities)
        .filter_map(|c| match c {
            ToolCapability::Named(n) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

/// Verify that DeploymentProfile factories produce the correct tool lists.
/// The full E2E chain (disabled tools → dispatch reject → LLM surface) is
/// covered in unit tests inside the astra-runtime crate
/// (tool_execution_service and server_loop_host).
#[tokio::test]
async fn server_default_includes_web_tools() {
    let profile = DeploymentProfile::server_default();
    let names = tool_names(&profile);
    assert!(names.iter().any(|n| n == "web_search"));
    assert!(names.iter().any(|n| n == "web_fetch"));
}

#[tokio::test]
async fn server_without_excludes_tools() {
    let profile = DeploymentProfile::server_without(&["web_search", "web_fetch"]);
    let names = tool_names(&profile);
    assert!(!names.iter().any(|n| n == "web_search"));
    assert!(!names.iter().any(|n| n == "web_fetch"));
    assert!(names.iter().any(|n| n == "memory"));
    assert!(names.iter().any(|n| n == "agent"));
    assert!(!names.iter().any(|n| n == "bash"));
}

#[tokio::test]
async fn server_with_only_restricts_to_server_builtin_tools() {
    let profile = DeploymentProfile::server_with_only(&["bash", "read_file"]);
    let names = tool_names(&profile);
    assert!(
        names.is_empty(),
        "workspace/process executor tools require an explicit runtime provider, got {names:?}"
    );
}

#[tokio::test]
async fn server_with_only_keeps_server_service_tools() {
    let profile = DeploymentProfile::server_with_only(&["memory", "web_fetch"]);
    let names = tool_names(&profile);
    assert!(names.iter().any(|n| n == "memory"));
    assert!(names.iter().any(|n| n == "web_fetch"));
    assert!(!names.iter().any(|n| n == "web_search"));
    assert!(!names.iter().any(|n| n == "bash"));
    assert!(!names.iter().any(|n| n == "read_file"));
    assert!(!names.iter().any(|n| n == "grep"));
}
