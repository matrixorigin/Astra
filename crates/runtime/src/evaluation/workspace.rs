//! Shared authenticated workspace capability checks for prepare and Run admission.

pub(crate) fn validate_frozen_workspace_capability(
    owner_user_id: &str,
    frozen: &astra_services::evaluation::FrozenWorkspaceExecution,
    connected: &astra_server_types::edge_connection_pool::EdgeConnectionInfo,
    record: &astra_services::multi_agent::EdgeAgentRecord,
) -> Result<(), String> {
    let intent = astra_services::evaluation::EvaluationPrepareWorkspace {
        edge_executor_id: frozen.edge_executor_id.clone(),
        source_commit: frozen.source_commit.clone(),
        tool_names: frozen.tool_names.clone(),
    };
    let actual = freeze_workspace_capability(owner_user_id, &intent, connected, record)?;
    if actual != *frozen {
        return Err("live workspace confinement differs from the frozen contract".into());
    }
    Ok(())
}

/// Match the authenticated registry publication to the currently connected
/// provider. A profile name is never synthesized into a capability claim.
pub(crate) fn freeze_workspace_capability(
    owner_user_id: &str,
    intent: &astra_services::evaluation::EvaluationPrepareWorkspace,
    connected: &astra_server_types::edge_connection_pool::EdgeConnectionInfo,
    record: &astra_services::multi_agent::EdgeAgentRecord,
) -> Result<astra_services::evaluation::FrozenWorkspaceExecution, String> {
    let root = record
        .worktree_path
        .as_deref()
        .filter(|root| !root.is_empty() && root.trim() == *root)
        .ok_or("selected Edge has no canonical workspace root")?;
    let materialization = record
        .materialization_id
        .as_deref()
        .filter(|id| !id.is_empty() && id.trim() == *id)
        .ok_or("selected Edge has no materialization identity")?;
    if record.user_id != owner_user_id
        || record.edge_agent_id != intent.edge_executor_id
        || connected.edge_agent_id != intent.edge_executor_id
        || record.workspace_id.is_some()
        || connected.workspace_id.is_some()
        || record.registry_id.is_empty()
        || connected.registry_id.as_deref() != Some(record.registry_id.as_str())
        || connected.materialization_id.as_deref() != Some(materialization)
        || connected.workspace_dir.as_deref() != Some(root)
        || connected.capabilities != record.capabilities
    {
        return Err("selected Edge connection and authenticated registry identity disagree".into());
    }
    let advertisement: astra_runtime_env::RuntimeEnvironmentAdvertisement = serde_json::from_value(
        record
            .capabilities
            .clone()
            .ok_or("selected Edge has no capability advertisement")?,
    )
    .map_err(|_| "selected Edge capability advertisement is invalid")?;
    let source = advertisement
        .workspace_source
        .as_ref()
        .ok_or("selected Edge has no source identity")?;
    if advertisement.schema_version
        != astra_runtime_env::RuntimeEnvironmentAdvertisement::SCHEMA_VERSION
        || !advertisement.binding.executor.is_edge_agent()
        || advertisement.binding.executor.executor_id != intent.edge_executor_id
        || advertisement.binding.workspace.kind
            != astra_runtime_env::WorkspaceBindingKind::EdgeWorkspace
        || advertisement.binding.workspace.cwd.as_deref() != Some(root)
        || advertisement.binding.workspace.authority
            != astra_runtime_env::WorkspaceAuthority::ReadWrite
        || !source.is_valid()
        || !source.clean
        || !source.commit.eq_ignore_ascii_case(&intent.source_commit)
        || !intent
            .tool_names
            .iter()
            .all(|tool| advertisement.binding.tool_surface.tool_names.contains(tool))
    {
        return Err("selected Edge capability does not match workspace intent".into());
    }
    astra_services::evaluation::FrozenWorkspaceExecution {
        edge_executor_id: intent.edge_executor_id.clone(),
        source_commit: intent.source_commit.clone(),
        tool_names: intent.tool_names.clone(),
        confinement: advertisement
            .workspace_confinement
            .ok_or("selected Edge does not provide workspace confinement")?,
    }
    .normalized()
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{
        RunBinding, RuntimeEnvironmentAdvertisement, ToolRegistry, WorkspaceSourceIdentity,
    };
    use astra_server_types::edge_connection_pool::EdgeConnectionInfo;
    use astra_services::evaluation::EvaluationPrepareWorkspace;
    use astra_services::multi_agent::EdgeAgentRecord;

    fn capability_fixture() -> (
        EvaluationPrepareWorkspace,
        EdgeConnectionInfo,
        EdgeAgentRecord,
    ) {
        let mut advertisement = RuntimeEnvironmentAdvertisement::new(RunBinding::edge_developer(
            "/workspace/project",
            &ToolRegistry::builtins(),
        ));
        advertisement.binding.executor.executor_id = "edge-a".into();
        advertisement.workspace_source = Some(WorkspaceSourceIdentity {
            commit: "a".repeat(40),
            tree: "b".repeat(40),
            clean: true,
        });
        let capability = serde_json::to_value(advertisement).unwrap();
        let connected = EdgeConnectionInfo {
            generation: 1,
            edge_agent_id: "edge-a".into(),
            hostname: None,
            workspace_dir: Some("/workspace/project".into()),
            capabilities: Some(capability.clone()),
            connected_at: std::time::Instant::now(),
            workspace_id: None,
            registry_id: Some("registration".into()),
            materialization_id: Some("checkout".into()),
        };
        let record = EdgeAgentRecord {
            registry_id: "registration".into(),
            user_id: "owner".into(),
            edge_agent_id: "edge-a".into(),
            edge_id: "device".into(),
            hostname: None,
            worktree_path: connected.workspace_dir.clone(),
            capabilities: Some(capability),
            workspace_id: None,
            materialization_id: connected.materialization_id.clone(),
            registered_at: String::new(),
            last_heartbeat_at: String::new(),
        };
        let intent = EvaluationPrepareWorkspace {
            edge_executor_id: "edge-a".into(),
            source_commit: "a".repeat(40),
            tool_names: vec!["read_file".into()],
        };
        (intent, connected, record)
    }

    #[test]
    fn ordinary_workspace_capability_cannot_freeze_confinement() {
        let (intent, connected, record) = capability_fixture();
        assert!(
            freeze_workspace_capability("owner", &intent, &connected, &record)
                .unwrap_err()
                .contains("does not provide workspace confinement")
        );
    }

    #[test]
    fn only_matching_authenticated_capability_can_be_frozen() {
        let (intent, mut connected, mut record) = capability_fixture();
        let contract = serde_json::json!({
            "profile_id": astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE,
            "toolchain_manifest": {
                "schema_version": 1,
                "inputs": [{"guest_mount_path": "/usr/bin", "content_digest": format!("sha256:{}", "a".repeat(64))}],
                "launcher_digest": format!("sha256:{}", "b".repeat(64)),
                "supervisor_digest": format!("sha256:{}", "c".repeat(64))
            }
        });
        record.capabilities.as_mut().unwrap()["workspace_confinement"] = contract.clone();
        connected.capabilities = record.capabilities.clone();
        let frozen = freeze_workspace_capability("owner", &intent, &connected, &record).unwrap();
        assert_eq!(serde_json::to_value(&frozen.confinement).unwrap(), contract);
        validate_frozen_workspace_capability("owner", &frozen, &connected, &record).unwrap();
        assert!(freeze_workspace_capability("other-owner", &intent, &connected, &record).is_err());
        let mut wrong = connected.clone();
        wrong.registry_id = Some("old-registration".into());
        assert!(freeze_workspace_capability("owner", &intent, &wrong, &record).is_err());
        wrong = connected.clone();
        wrong.materialization_id = Some("other-checkout".into());
        assert!(freeze_workspace_capability("owner", &intent, &wrong, &record).is_err());
        wrong = connected.clone();
        wrong.capabilities.as_mut().unwrap()["workspace_confinement"]["toolchain_manifest"]["launcher_digest"] =
            serde_json::json!(format!("sha256:{}", "d".repeat(64)));
        assert!(freeze_workspace_capability("owner", &intent, &wrong, &record).is_err());
        let mut changed_record = record.clone();
        changed_record.capabilities = wrong.capabilities.clone();
        assert!(freeze_workspace_capability("owner", &intent, &wrong, &changed_record).is_ok());
        assert!(
            validate_frozen_workspace_capability("owner", &frozen, &wrong, &changed_record)
                .unwrap_err()
                .contains("differs from the frozen contract")
        );
        let mut changed = intent;
        changed.source_commit = "b".repeat(40);
        assert!(freeze_workspace_capability("owner", &changed, &connected, &record).is_err());
    }
}
