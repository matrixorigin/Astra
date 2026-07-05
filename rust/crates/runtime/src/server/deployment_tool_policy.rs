use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use crate::server::tool_transport::ToolExecutionServiceBuilder;

#[derive(Clone, Default)]
pub(crate) struct DeploymentToolPolicy {
    pub disabled_tool_offers: Vec<String>,
    pub disabled_tool_names: Vec<String>,
    pub provider_allowed_tools: HashMap<String, HashSet<String>>,
}

pub(crate) fn load_deployment_tool_policy() -> DeploymentToolPolicy {
    static TOOL_POLICY: OnceLock<DeploymentToolPolicy> = OnceLock::new();
    TOOL_POLICY
        .get_or_init(|| {
            astra_core::ServerConfig::load()
                .map(|sc| DeploymentToolPolicy {
                    disabled_tool_offers: sc.deployment.disabled_tool_offers,
                    disabled_tool_names: sc.deployment.disabled_tool_names,
                    provider_allowed_tools: provider_allowed_tools(
                        sc.deployment.provider_allowed_tools,
                    ),
                })
                .unwrap_or_default()
        })
        .clone()
}

pub(crate) fn apply_deployment_tool_policy(
    mut builder: ToolExecutionServiceBuilder,
    policy: &DeploymentToolPolicy,
) -> ToolExecutionServiceBuilder {
    if !policy.disabled_tool_offers.is_empty() {
        builder = builder.initial_disabled_tool_offers(&policy.disabled_tool_offers);
    }
    if !policy.disabled_tool_names.is_empty() {
        builder = builder.initial_disabled_tool_names(&policy.disabled_tool_names);
    }
    if !policy.provider_allowed_tools.is_empty() {
        builder = builder.initial_provider_allowed_tools(policy.provider_allowed_tools.clone());
    }
    builder
}

fn provider_allowed_tools(
    configured: HashMap<String, Vec<String>>,
) -> HashMap<String, HashSet<String>> {
    configured
        .into_iter()
        .map(|(provider, tools)| (provider, tools.into_iter().collect()))
        .collect()
}
