use crate::server::run::lifecycle::run_state::{
    RunControlAction, RunStatus, durable_run_owner_lease_is_live,
};
use crate::server::*;
use astra_server_types::{
    SESSION_RUN_TREE_SCHEMA_VERSION, SessionRunAction, SessionRunCapabilityServerRefs,
    SessionRunLifecycleStatus, SessionRunNode, SessionRunRuntimeFacts, SessionRunTreeSnapshot,
};
use astra_services::runs::{DurableRunRecord, validate_run_list_limit};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const DEFAULT_SESSION_RUN_NODE_LIMIT: u32 = 200;

#[derive(Debug, Deserialize)]
pub(crate) struct SessionRunTreeQuery {
    #[serde(default = "default_session_run_node_limit")]
    pub limit: u32,
}

fn default_session_run_node_limit() -> u32 {
    DEFAULT_SESSION_RUN_NODE_LIMIT
}

pub(crate) async fn get_session_run_tree_handler(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<SessionRunTreeQuery>,
) -> Result<Json<SessionRunTreeSnapshot>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id, user.user_id.clone())
        .await?;
    let page = state
        .execution
        .run_lifecycle_service
        .list_session_runs(
            user.user_id,
            session.session_id.clone(),
            validate_run_list_limit(query.limit),
        )
        .await?;
    Ok(Json(build_session_run_tree_snapshot(
        session.session_id,
        page,
    )?))
}

fn build_session_run_tree_snapshot(
    session_id: String,
    page: astra_services::runs::DurableSessionRunPage,
) -> Result<SessionRunTreeSnapshot, (StatusCode, Json<ErrorResponse>)> {
    let mut durable_runs = page.runs;
    durable_runs.sort_by(|a, b| {
        a.depth
            .cmp(&b.depth)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    let runs_by_id = durable_runs
        .iter()
        .map(|run| (run.run_id.as_str(), run))
        .collect::<HashMap<_, _>>();
    let mut effective_statuses = HashMap::new();
    let mut runs = Vec::with_capacity(durable_runs.len());
    for run in &durable_runs {
        let (status, inherited_control) = effective_run_status(
            run,
            &runs_by_id,
            &mut effective_statuses,
            &mut HashSet::new(),
        )?;
        runs.push(project_run_node_with_status(run, status, inherited_control));
    }
    let revision_payload = serde_json::json!({
        "schema_version": SESSION_RUN_TREE_SCHEMA_VERSION,
        "session_id": &session_id,
        "node_limit": page.limit,
        "truncated": page.truncated,
        "runs": &runs,
    });
    let canonical = astra_core::canonical_json_string(&revision_payload);
    let digest = Sha256::digest(canonical.as_bytes());

    Ok(SessionRunTreeSnapshot {
        schema_version: SESSION_RUN_TREE_SCHEMA_VERSION,
        session_id,
        snapshot_revision: format!("sha256:{digest:x}"),
        observed_at: chrono::Utc::now().to_rfc3339(),
        node_limit: page.limit,
        truncated: page.truncated,
        runs,
    })
}

/// Hierarchical controls are declarations, not replicated mutable child state.
/// A root pause/cancel therefore projects to every visible descendant even
/// before any child has reached its next execution boundary. Terminal child
/// facts remain terminal and are never visually rewritten by later ancestor
/// controls.
fn effective_run_status(
    run: &DurableRunRecord,
    runs_by_id: &HashMap<&str, &DurableRunRecord>,
    cache: &mut HashMap<String, (RunStatus, bool)>,
    visiting: &mut HashSet<String>,
) -> Result<(RunStatus, bool), (StatusCode, Json<ErrorResponse>)> {
    if let Some(status) = cache.get(&run.run_id) {
        return Ok(*status);
    }
    let local_status = RunStatus::from_durable_status(&run.status).ok_or_else(|| {
        error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "durable run {} has unsupported lifecycle status {:?}",
                run.run_id, run.status
            ),
        )
    })?;
    if local_status.is_terminal() {
        cache.insert(run.run_id.clone(), (local_status, false));
        return Ok((local_status, false));
    }

    let mut effective = (local_status, false);
    if let Some(parent_run_id) = run.parent_run_id.as_deref()
        && let Some(parent) = runs_by_id.get(parent_run_id)
    {
        if !visiting.insert(run.run_id.clone()) {
            return Err(error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "durable run tree contains a lineage cycle at {}",
                    run.run_id
                ),
            ));
        }
        let (parent_status, parent_inherited_control) =
            effective_run_status(parent, runs_by_id, cache, visiting)?;
        visiting.remove(&run.run_id);
        effective = match parent_status {
            RunStatus::Cancelled => (RunStatus::Cancelled, true),
            RunStatus::Paused if parent_inherited_control || parent.waiting_for.is_some() => {
                (RunStatus::Paused, true)
            }
            _ => effective,
        };
    }
    cache.insert(run.run_id.clone(), effective);
    Ok(effective)
}

fn project_run_node_with_status(
    run: &DurableRunRecord,
    lifecycle_status: RunStatus,
    inherited_control: bool,
) -> SessionRunNode {
    let partial_interruption = lifecycle_status == RunStatus::Failed
        && astra_services::coordination::durable_agent_result_is_partial(
            run.error_code.as_deref(),
            run.error_message.as_deref(),
        );
    let status = match lifecycle_status {
        RunStatus::Running => SessionRunLifecycleStatus::Running,
        RunStatus::Waiting => SessionRunLifecycleStatus::Waiting,
        RunStatus::Paused => SessionRunLifecycleStatus::Paused,
        RunStatus::Completed => SessionRunLifecycleStatus::Completed,
        RunStatus::Delegated => SessionRunLifecycleStatus::Delegated,
        RunStatus::Failed if partial_interruption => SessionRunLifecycleStatus::Interrupted,
        RunStatus::Failed => SessionRunLifecycleStatus::Failed,
        RunStatus::Cancelled => SessionRunLifecycleStatus::Cancelled,
    };
    let capability_server_refs = run.capability_server_refs_json.as_deref().and_then(|raw| {
        match serde_json::from_str::<astra_services::runs::CapabilityServerRefs>(raw) {
            Ok(refs) => Some(SessionRunCapabilityServerRefs {
                mcp: refs.mcp,
                skills: refs.skills,
            }),
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::session_run_tree",
                    run_id = %run.run_id,
                    %error,
                    "durable run capability refs are malformed; omitting inspector fact"
                );
                None
            }
        }
    });

    let available_actions = if inherited_control {
        matches!(lifecycle_status, RunStatus::Paused)
            .then_some(SessionRunAction::Cancel)
            .into_iter()
            .collect()
    } else if lifecycle_status == RunStatus::Paused && !durable_run_owner_lease_is_live(run) {
        vec![SessionRunAction::ContinueSession, SessionRunAction::Cancel]
    } else {
        lifecycle_status
            .available_control_actions()
            .map(|action| match action {
                RunControlAction::Pause => SessionRunAction::Pause,
                RunControlAction::Resume => SessionRunAction::Resume,
                RunControlAction::Cancel => SessionRunAction::Cancel,
            })
            .collect()
    };
    SessionRunNode {
        run_id: run.run_id.clone(),
        parent_run_id: run.parent_run_id.clone(),
        root_run_id: run.root_run_id.clone(),
        depth: run.depth,
        agent_id: run.agent_id.clone(),
        agent_name: run.agent_binding_name.clone(),
        status,
        waiting_for: (inherited_control && lifecycle_status == RunStatus::Paused)
            .then(|| "ancestor_paused".to_string())
            .or_else(|| run.waiting_for.clone()),
        error_code: run.error_code.clone(),
        error_message: if partial_interruption {
            astra_services::coordination::durable_agent_partial_reason(
                run.error_code.as_deref(),
                run.error_message.as_deref(),
            )
            .map(ToString::to_string)
        } else {
            run.error_message.clone()
        },
        run_event_high_watermark: run.last_event_idx,
        total_tool_calls: run.total_tool_calls,
        runtime: SessionRunRuntimeFacts {
            runtime_profile: run.runtime_profile.clone(),
            model_name: run.selected_model_name.clone(),
            model_gateway: run.selected_model_gateway.clone(),
            agent_binding_id: run.agent_binding_id.clone(),
            agent_binding_name: run.agent_binding_name.clone(),
            agent_binding_schema_version: run.agent_binding_schema_version.clone(),
            capability_server_refs,
            background: None,
            permission: None,
        },
        available_actions,
        created_at: run.created_at.clone(),
        updated_at: run.updated_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_core::{STATUS_CANCELLED, STATUS_COMPLETED, STATUS_DELEGATED, STATUS_PAUSED};
    use astra_services::auth::{
        SessionActivityCursor, SessionActivityRecord, SessionCreateRequestData, SessionListFilter,
        SessionListRecord, SessionRecord, SessionService, SessionUpdateRequestData,
    };
    use async_trait::async_trait;
    use axum::body::{self, Body};
    use axum::http::Request;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;
    use tower::util::ServiceExt;

    fn run(run_id: &str, status: &str, depth: u32) -> DurableRunRecord {
        DurableRunRecord {
            run_id: run_id.into(),
            user_id: "user-1".into(),
            session_id: "session-1".into(),
            parent_run_id: (depth > 0).then(|| "root".into()),
            root_run_id: Some("root".into()),
            ancestor_path: None,
            depth,
            delegation_id: None,
            agent_id: (depth > 0).then(|| "reviewer".into()),
            retry_of: None,
            retry_scope: None,
            status: status.into(),
            waiting_for: (status == STATUS_PAUSED).then(|| "user_resume".into()),
            owner_pod_id: None,
            owner_lease_expires_at: None,
            run_generation: 0,
            last_event_idx: 4,
            checkpoint_version: None,
            checkpoint_json: None,
            error_code: None,
            error_message: None,
            retry_count: 0,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_tool_calls: depth,
            agent_binding_id: (depth > 0).then(|| "reviewer-v2".into()),
            agent_binding_name: (depth > 0).then(|| "Reviewer".into()),
            agent_binding_schema_version: (depth > 0).then(|| "2".into()),
            selected_model_json: None,
            selected_model_name: (depth > 0).then(|| "gpt-5".into()),
            selected_model_gateway: (depth > 0).then(|| "primary".into()),
            capability_server_refs_json: (depth > 0)
                .then(|| r#"{"mcp":"mcp-main","skills":"skills-main"}"#.into()),
            runtime_profile: Some("server".into()),
            events: Vec::new(),
            created_at: format!("2026-07-11T00:00:0{depth}Z"),
            updated_at: format!("2026-07-11T00:01:0{depth}Z"),
        }
    }

    #[derive(Clone)]
    struct AlwaysHealthy;

    #[async_trait]
    impl HealthChecker for AlwaysHealthy {
        async fn database_healthy(&self) -> bool {
            true
        }
    }

    struct OwnedSessionService {
        allow: bool,
    }

    #[async_trait]
    impl SessionService for OwnedSessionService {
        async fn create_session(
            &self,
            _user_id: String,
            _request: SessionCreateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn list_sessions(
            &self,
            _filter: SessionListFilter,
        ) -> Result<SessionListRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn get_session(
            &self,
            session_id: String,
            user_id: String,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            if !self.allow || user_id != "test-user" {
                return Err(error_response(
                    StatusCode::FORBIDDEN,
                    "session access denied",
                ));
            }
            Ok(SessionRecord {
                session_id,
                user_id,
                agent_id: None,
                title: None,
                metadata: serde_json::Map::new(),
                status: "active".into(),
                event_count: 0,
                created_at: "2026-07-11T00:00:00Z".into(),
                updated_at: None,
                ended_at: None,
            })
        }

        async fn update_session(
            &self,
            _session_id: String,
            _user_id: String,
            _request: SessionUpdateRequestData,
        ) -> Result<SessionRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn delete_session(
            &self,
            _session_id: String,
            _user_id: String,
        ) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }

        async fn get_session_activity(
            &self,
            _session_id: String,
            _user_id: String,
            _limit: u32,
            _cursor: Option<SessionActivityCursor>,
        ) -> Result<SessionActivityRecord, (StatusCode, Json<ErrorResponse>)> {
            unreachable!()
        }
    }

    async fn session_run_tree_test_app(allow_session: bool) -> axum::Router {
        let engine = crate::server::run::engine::RunEngine::new(Arc::new(
            astra_services::runs::InMemoryRunStateStore::new(),
        ));
        engine
            .start_run("root-run", "test-user", "session-1")
            .await
            .unwrap();
        engine
            .start_run_ext(
                "child-run",
                "test-user",
                "session-1",
                Some("root-run"),
                Some("delegation-1"),
                Some("reviewer"),
                None,
            )
            .await
            .unwrap();
        let lifecycle = crate::server::run::lifecycle::AgenticRunLifecycleService::new(
            astra_core::MatrixOneSettings::mock(),
            Arc::new(crate::FernetTokenEncryptor::new("0123456789abcdef").expect("test encryptor")),
            Arc::new(TokioMutex::new(HashMap::new())),
            engine,
        );
        let state = AppState::new(ServiceInfo::default(), Arc::new(AlwaysHealthy))
            .with_auth_service(Arc::new(astra_services::auth::StubAuthService))
            .with_session_service(Arc::new(OwnedSessionService {
                allow: allow_session,
            }))
            .with_run_lifecycle_service(Arc::new(lifecycle));
        crate::server::build_test_router(state)
    }

    #[test]
    fn projection_is_hierarchical_typed_and_revision_ignores_observation_time() {
        let page = astra_services::runs::DurableSessionRunPage {
            runs: vec![
                run("child", STATUS_PAUSED, 1),
                run("root", STATUS_COMPLETED, 0),
            ],
            limit: 200,
            truncated: false,
        };
        let first = build_session_run_tree_snapshot("session-1".into(), page.clone()).unwrap();
        let second = build_session_run_tree_snapshot("session-1".into(), page).unwrap();

        assert_eq!(first.snapshot_revision, second.snapshot_revision);
        assert_eq!(first.runs[0].run_id, "root");
        assert_eq!(first.runs[1].parent_run_id.as_deref(), Some("root"));
        assert_eq!(first.runs[1].status, SessionRunLifecycleStatus::Paused);
        assert_eq!(
            first.runs[1].available_actions,
            vec![SessionRunAction::ContinueSession, SessionRunAction::Cancel]
        );
        let runtime = &first.runs[1].runtime;
        assert_eq!(runtime.runtime_profile.as_deref(), Some("server"));
        assert_eq!(runtime.model_name.as_deref(), Some("gpt-5"));
        assert_eq!(runtime.model_gateway.as_deref(), Some("primary"));
        assert_eq!(runtime.agent_binding_id.as_deref(), Some("reviewer-v2"));
        assert_eq!(runtime.agent_binding_schema_version.as_deref(), Some("2"));
        let capability_refs = runtime.capability_server_refs.as_ref().unwrap();
        assert_eq!(capability_refs.mcp, "mcp-main");
        assert_eq!(capability_refs.skills, "skills-main");
        assert!(runtime.permission.is_none());
    }

    #[test]
    fn delegated_projection_preserves_terminal_handoff_semantics() {
        let page = astra_services::runs::DurableSessionRunPage {
            runs: vec![run("delegated", STATUS_DELEGATED, 0)],
            limit: 200,
            truncated: false,
        };

        let snapshot = build_session_run_tree_snapshot("session-1".into(), page).unwrap();
        assert_eq!(
            snapshot.runs[0].status,
            SessionRunLifecycleStatus::Delegated
        );
        assert!(snapshot.runs[0].available_actions.is_empty());
        assert!(snapshot.runs[0].status.is_terminal());
    }

    #[test]
    fn delegated_descendant_is_not_reopened_by_ancestor_control() {
        for ancestor_status in [STATUS_PAUSED, STATUS_CANCELLED] {
            let ancestor = run("root", ancestor_status, 0);
            let descendant = run("child", STATUS_DELEGATED, 1);
            let page = astra_services::runs::DurableSessionRunPage {
                runs: vec![descendant, ancestor],
                limit: 200,
                truncated: false,
            };

            let snapshot = build_session_run_tree_snapshot("session-1".into(), page).unwrap();
            let descendant = snapshot
                .runs
                .iter()
                .find(|run| run.run_id == "child")
                .unwrap();
            assert_eq!(descendant.status, SessionRunLifecycleStatus::Delegated);
            assert!(descendant.available_actions.is_empty());
        }
    }

    #[test]
    fn projection_derives_parent_control_for_every_visible_descendant() {
        let mut root = run("root", STATUS_PAUSED, 0);
        root.waiting_for = Some("user_resume".into());
        let child = run("child", "running", 1);
        let mut grandchild = run("grandchild", "running", 2);
        grandchild.parent_run_id = Some("child".into());
        let page = astra_services::runs::DurableSessionRunPage {
            runs: vec![grandchild, child, root],
            limit: 200,
            truncated: false,
        };

        let snapshot = build_session_run_tree_snapshot("session-1".into(), page).unwrap();
        assert_eq!(snapshot.runs[0].status, SessionRunLifecycleStatus::Paused);
        for node in &snapshot.runs[1..] {
            assert_eq!(node.status, SessionRunLifecycleStatus::Paused);
            assert_eq!(node.waiting_for.as_deref(), Some("ancestor_paused"));
            assert_eq!(node.available_actions, vec![SessionRunAction::Cancel]);
        }
    }

    #[test]
    fn projection_does_not_spread_nonblocking_parent_pause_to_descendants() {
        let mut root = run("root", STATUS_PAUSED, 0);
        root.waiting_for = None;
        let child = run("child", "running", 1);
        let page = astra_services::runs::DurableSessionRunPage {
            runs: vec![child, root],
            limit: 200,
            truncated: false,
        };

        let snapshot = build_session_run_tree_snapshot("session-1".into(), page).unwrap();
        let root = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == "root")
            .unwrap();
        let child = snapshot
            .runs
            .iter()
            .find(|run| run.run_id == "child")
            .unwrap();
        assert_eq!(root.status, SessionRunLifecycleStatus::Paused);
        assert_eq!(child.status, SessionRunLifecycleStatus::Running);
        assert!(
            child.waiting_for.is_none(),
            "a resumable parent checkpoint is not a pause command for its children"
        );
    }

    #[test]
    fn projection_recovers_terminal_partial_child_as_interrupted() {
        let mut child = run("child", STATUS_FAILED, 1);
        child.error_code =
            Some(astra_services::coordination::AGENT_RESULT_PARTIAL_DURABLE_ERROR_CODE.to_string());
        child.error_message =
            Some("budget_exhausted: adaptive hard turn limit reached".to_string());
        let page = astra_services::runs::DurableSessionRunPage {
            runs: vec![child],
            limit: 200,
            truncated: false,
        };

        let snapshot = build_session_run_tree_snapshot("session-1".into(), page).unwrap();
        assert_eq!(
            snapshot.runs[0].status,
            SessionRunLifecycleStatus::Interrupted
        );
        assert!(snapshot.runs[0].available_actions.is_empty());
        assert_eq!(
            snapshot.runs[0].error_message.as_deref(),
            Some("budget_exhausted: adaptive hard turn limit reached")
        );
    }

    #[test]
    fn invalid_durable_status_is_not_downgraded_to_a_display_guess() {
        let page = astra_services::runs::DurableSessionRunPage {
            runs: vec![run("broken", "mystery", 1)],
            limit: 200,
            truncated: false,
        };
        let error = build_session_run_tree_snapshot("session-1".into(), page).unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn session_run_tree_route_returns_owned_typed_hierarchy() {
        let response = session_run_tree_test_app(true)
            .await
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/sessions/session-1/runs?limit=50")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let snapshot: SessionRunTreeSnapshot = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(snapshot.session_id, "session-1");
        assert_eq!(snapshot.runs.len(), 2);
        assert_eq!(snapshot.runs[0].run_id, "root-run");
        assert_eq!(snapshot.runs[1].parent_run_id.as_deref(), Some("root-run"));
        assert_eq!(
            snapshot.runs[1].available_actions,
            vec![SessionRunAction::Pause, SessionRunAction::Cancel]
        );
    }

    #[tokio::test]
    async fn session_run_tree_route_rejects_cross_session_access_before_listing_runs() {
        let response = session_run_tree_test_app(false)
            .await
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/sessions/session-1/runs")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
