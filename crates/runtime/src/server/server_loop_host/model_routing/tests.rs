use super::super::tests::{
    admitted_test_offering, create_durable_execution_test_state, mock_encryptor, mock_matrixone,
};
use super::*;
use crate::server::provider_test_support::{ProviderGateway, ProviderResponse, ProviderScript};
use astra_services::{
    ModelCreateRequestData, ModelListItem, ModelRecord, ModelUpdateRequestData,
    ResolvedModelOffering,
};
use axum::{Json, http::StatusCode};

#[tokio::test]
#[serial_test::serial(auxiliary_llm_capacity_policy_env)]
async fn auto_entrypoint_reuses_builtin_judge_and_dispatches_the_selected_model() {
    for (auxiliary_policy, malformed) in [
        (None, false),
        (Some("capacity_aware"), false),
        (Some("boundary_only"), false),
        (Some("always"), false),
        (Some("disabled"), false),
        (None, true),
    ] {
        let _policy = match auxiliary_policy {
            Some(value) => super::super::tests::EnvVarGuard::set(AUX_LLM_POLICY_ENV, value),
            None => super::super::tests::EnvVarGuard::remove(AUX_LLM_POLICY_ENV),
        };
        let expected_judgments = usize::from(auxiliary_policy != Some("disabled"));
        let economy = expected_judgments == 1 && !malformed;
        let expected_model = if economy {
            "wire-economy"
        } else {
            "wire-strong"
        };
        let (catalog, engine, _, mut state) = fixture().await;
        let mut response: Value =
            serde_json::from_str(&super::super::tests::classification_response(false)).unwrap();
        response["assessment"] = json!({"difficulty":"easy","difficulty_confidence":"high"});
        if malformed {
            response = json!({"invalid": "classification"});
        }
        let judgment_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let classification_client = super::super::tests::SequencedSummaryClient {
            provenance: astra_turn_types::JudgmentResponseProvenance::ProviderProbability,
            responses: std::sync::Mutex::new([response.to_string()].into()),
            requests: judgment_requests.clone(),
        };
        let planner_client = super::super::tests::SequencedSummaryClient {
            provenance: astra_turn_types::JudgmentResponseProvenance::ProviderProbability,
            responses: std::sync::Mutex::new(Default::default()),
            requests: judgment_requests.clone(),
        };
        let response = json!({
            "choices":[{"message":{"content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12,"completion_tokens":24}
        });
        let provider = ProviderGateway::start(vec![ProviderScript::new(
            "auto_entrypoint_reuses_builtin_judge_and_dispatches_the_selected_model",
            |request| request.path == "/v1/chat/completions",
            (0..1)
                .map(|_| ProviderResponse::OpenAi(response.clone()))
                .collect(),
        )])
        .await;
        let url = format!("{}/v1/chat/completions", provider.base_url);
        let requests = provider.requests.clone();
        for model in catalog.models.write().unwrap().values_mut() {
            model.completions_url_override = Some(url.clone());
        }
        let baseline = catalog.models.read().unwrap()["strong"].clone();
        let ledger = crate::server::provider_test_support::ProviderObservationsFixture::default();
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "router-user".into(),
            "router-session".into(),
        )
        .with_admitted_model_execution(Some(baseline))
        .with_model_service(Some(catalog.clone()))
        .with_provider_observations(ledger.clone())
        .with_test_judgment_clients([
            Box::new(classification_client)
                as Box<dyn astra_turn_core::cloud_summary::SummaryLlmClient>,
            Box::new(planner_client),
        ])
        .build();
        host.configure_model_routing(policy(), catalog, engine.clone(), true);
        crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        assert_eq!(
            judgment_requests.lock().unwrap().len(),
            expected_judgments,
            "routing policy {auxiliary_policy:?} must respect the shared judgment boundary"
        );
        assert!(
            requests.lock().await.is_empty(),
            "no primary call before routing"
        );
        let decision = engine
            .load_run_event_by_idempotency_key(
                "router-user",
                state.current_run_id.as_deref().unwrap(),
                EVENT_TYPE,
                DECISION_KEY,
            )
            .await
            .unwrap()
            .expect("decision is durable before primary I/O");
        assert_eq!(
            decision["data"]["reason"],
            if economy {
                "easy_read_only"
            } else {
                "assessment_unavailable"
            }
        );
        let frozen: ModelRoutingDecision =
            serde_json::from_value(decision["data"].clone()).unwrap();
        let features = frozen
            .features
            .expect("decision-time features must be durable before I/O");
        assert!(features.supported_input);
        if economy {
            assert!(features.read_only_primary);
        }
        assert_eq!(
            astra_turn_core::model_routing::economy_eligibility_from_features(features),
            if economy {
                ModelRoutingReason::EasyReadOnly
            } else {
                ModelRoutingReason::AssessmentUnavailable
            }
        );
        crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 1)
            .await
            .unwrap();
        host.execute_turn(&mut state).await.unwrap();
        let recorded = requests.lock().await.clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].body["model"], expected_model);
        assert_eq!(
            judgment_requests.lock().unwrap().len(),
            expected_judgments,
            "later preparation and primary execution must not repeat the assessment"
        );
        ledger.assert_quiescent();
        provider.assert_complete();
    }
}

#[derive(Default)]
struct Catalog {
    models: RwLock<HashMap<String, AdmittedModelExecution>>,
    separate_billing: AtomicBool,
    admission_gate: RwLock<Option<AdmissionGate>>,
}

#[derive(Clone)]
struct AdmissionGate {
    model: String,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

struct LostModelAck {
    engine: RunEngine,
    lose_once: AtomicBool,
}

#[async_trait]
impl crate::turn::run_control::RunStatusProvider for LostModelAck {
    async fn run_settings_snapshot(
        &self,
        user: &str,
        session: &str,
        run: &str,
    ) -> Result<Option<astra_services::runs::RunSettingsSnapshot>, String> {
        self.engine.run_settings_snapshot(user, session, run).await
    }
    async fn finish_run_model(
        &self,
        user: &str,
        session: &str,
        run: &str,
        outcome: &astra_turn_types::RunModelOutcome,
        resolved: Option<&astra_services::runs::ResolvedModelSelection>,
    ) -> Result<Option<astra_turn_types::RunModelOutcome>, String> {
        let result = self
            .engine
            .finish_run_model(user, session, run, outcome, resolved)
            .await?;
        if self.lose_once.swap(false, Ordering::SeqCst) {
            Err("ack lost after commit".into())
        } else {
            Ok(result)
        }
    }
    async fn control_status(
        &self,
        _: &str,
        _: &str,
    ) -> Result<Option<crate::turn::run_control::RunControlStatus>, String> {
        Ok(None)
    }
}

#[async_trait]
impl crate::turn::run_control::UserIntentProvider for LostModelAck {
    async fn poll_user_intents(
        &self,
        _: &str,
        _: &str,
        _: usize,
    ) -> crate::turn::run_control::UserIntentPoll {
        Default::default()
    }
    async fn mark_user_intents_applied(
        &self,
        _: &str,
        _: &str,
        _: &str,
        _: &[usize],
        _: crate::turn::run_control::UserIntentAdmissionAuthority,
        _: Option<crate::turn::run_control::UserIntentInvocationApplication<'_>>,
    ) -> Result<crate::turn::run_control::UserIntentApplyAck, String> {
        Ok(crate::turn::run_control::UserIntentApplyAck::Applied)
    }
}

fn unsupported<T>() -> Result<T, (StatusCode, Json<astra_core::ErrorResponse>)> {
    Err(crate::error_response_coded(
        StatusCode::FORBIDDEN,
        "unavailable",
        "model_unavailable",
    ))
}

#[async_trait]
impl ModelService for Catalog {
    async fn admit_model_offering(
        &self,
        user: String,
        id: String,
    ) -> Result<AdmittedModelExecution, (StatusCode, Json<astra_core::ErrorResponse>)> {
        if user != "router-user" {
            return unsupported();
        }
        let gate = self.admission_gate.read().unwrap().clone();
        if let Some(gate) = gate.filter(|gate| gate.model == id) {
            gate.entered.notify_one();
            gate.release.notified().await;
        }
        self.models
            .read()
            .unwrap()
            .get(&id)
            .cloned()
            .ok_or_else(|| {
                crate::error_response_coded(
                    StatusCode::FORBIDDEN,
                    "unavailable",
                    "model_unavailable",
                )
            })
    }
    async fn list_models(
        &self,
        user: String,
        _: bool,
    ) -> Result<Vec<ModelListItem>, (StatusCode, Json<astra_core::ErrorResponse>)> {
        if user != "router-user" {
            return unsupported();
        }
        Ok(self
            .models
            .read()
            .unwrap()
            .values()
            .map(|model| ModelListItem {
                supported_reasoning_efforts: None,
                thinking_protocol: None,
                offering_id: model.offering_id.clone(),
                access_id: if self.separate_billing.load(Ordering::SeqCst) {
                    model.offering_id.clone()
                } else {
                    "same-access".into()
                },
                access_kind: model.access_kind,
                access_label: "test".into(),
                execution_placement: model.execution_placement,
                name: model.model_name.clone(),
                provider: model.provider.clone(),
                description: None,
                is_active: true,
                context_window: model.context_window.unwrap_or(0) as i32,
                max_completion_tokens: model.max_completion_tokens.map(|value| value as i32),
                architecture: None,
                thinking_capability: None,
                pricing: None,
            })
            .collect())
    }
    async fn resolve_model_offering(
        &self,
        _: String,
    ) -> Result<ResolvedModelOffering, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unsupported()
    }
    async fn create_model(
        &self,
        _: String,
        _: ModelCreateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unsupported()
    }
    async fn get_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unsupported()
    }
    async fn update_model(
        &self,
        _: String,
        _: ModelUpdateRequestData,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unsupported()
    }
    async fn delete_model(
        &self,
        _: String,
    ) -> Result<(), (StatusCode, Json<astra_core::ErrorResponse>)> {
        unsupported()
    }
    async fn check_model(
        &self,
        _: String,
    ) -> Result<ModelRecord, (StatusCode, Json<astra_core::ErrorResponse>)> {
        unsupported()
    }
}

fn policy() -> AutoModelRoutingPolicy {
    AutoModelRoutingPolicy {
        revision: "qualified-pair-v1".into(),
        economy_offering_id: "economy".into(),
        strong_offering_id: "strong".into(),
    }
}

fn catalog() -> Arc<Catalog> {
    let catalog = Arc::new(Catalog::default());
    for id in ["economy", "strong"] {
        let mut model = AdmittedModelExecution::from_offering(admitted_test_offering()).unwrap();
        model.offering_id = id.into();
        model.model_name = id.into();
        model.wire_model_name = Some(format!("wire-{id}"));
        catalog.models.write().unwrap().insert(id.into(), model);
    }
    catalog
}

fn host(catalog: Arc<Catalog>, engine: RunEngine) -> ServerAgenticLoopHost {
    let baseline = catalog.models.read().unwrap()["strong"].clone();
    let mut host = ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "router-user".into(),
        "router-session".into(),
    )
    .with_admitted_model_execution(Some(baseline))
    .with_model_service(Some(catalog.clone()))
    .build();
    host.configure_model_routing(policy(), catalog, engine, true);
    host
}

async fn fixture() -> (
    Arc<Catalog>,
    RunEngine,
    ServerAgenticLoopHost,
    AgenticLoopState,
) {
    let catalog = catalog();
    let engine = RunEngine::new(Arc::new(astra_services::runs::InMemoryRunStateStore::new()));
    let mut state = create_durable_execution_test_state("router-session");
    let authority = engine
        .start_run_ext_with_context(
            state.current_run_id.as_deref().unwrap(),
            "router-user",
            "router-session",
            None,
            None,
            None,
            None,
            crate::server::run::engine::RunStartContext {
                model_selection: Some(astra_turn_types::ModelSelection {
                    offering_id: "strong".into(),
                }),
                resolved_model_selection: Some(astra_services::runs::ResolvedModelSelection {
                    offering_id: "strong".into(),
                    model_name: "strong".into(),
                    source_identity: None,
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state.current_run_owner_generation = Some(authority.owner_generation);
    state.owner_user_id = Some("router-user".into());
    state.canonical_turn_chain_id = Some("router-turn".into());
    state.message = "explain this result".into();
    state.user_intent = state.message.clone();
    state.messages = vec![json!({"role":"user", "content":state.message})];
    state.runtime_manifest = Some(json!({}));
    crate::turn::agentic::turn_intent::capture_turn_intent_context(&mut state);
    let mut host = host(catalog.clone(), engine.clone());
    let decision = astra_services::parse_work_admission_response(r#"{"work_lifecycle":"not_required","workspace_mutation":"read_only","execution_topology":"primary","assessment":{"difficulty":"easy","difficulty_confidence":"high"}}"#).unwrap();
    host.apply_work_admission_decision(decision);
    host.completed_work_admission_phase =
        Some((Instant::now(), Instant::now(), 0, TurnPhaseOutcome::Decided));
    (catalog, engine, host, state)
}

#[tokio::test]
async fn auto_choice_is_durable_idempotent_and_restored_after_policy_change() {
    let (catalog, engine, mut first, mut state) = fixture().await;
    state.admitted_tool_policy.model_profiles.push(
        astra_config::runtime_config::ModelPolicyProfile {
            model_match: "economy".into(),
            max_identical_tool_calls: 7,
            max_tools_per_turn: 9,
            max_consecutive_empty_name: 4,
            ..Default::default()
        },
    );
    state.evaluation_thresholds.search_fanout = 37;
    // Repinning must replace the previous model's limits, not retain them.
    state.max_identical_tool_calls = 71;
    state.max_tools_per_turn = 73;
    state.max_consecutive_empty_name = 79;
    crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut first, &mut state, 0)
        .await
        .unwrap();
    assert_eq!(
        first.admitted_model_execution.as_ref().unwrap().offering_id,
        "economy"
    );
    assert_eq!(
        state
            .hooks
            .admitted_model_execution
            .as_ref()
            .unwrap()
            .offering_id,
        "economy"
    );
    assert_eq!(
        state.runtime_manifest.as_ref().unwrap()["model_resolution"]["model"],
        "economy"
    );
    assert_eq!(
        (
            state.max_identical_tool_calls,
            state.max_tools_per_turn,
            state.max_consecutive_empty_name
        ),
        (7, 9, 4)
    );
    assert_eq!(state.evaluation_thresholds.search_fanout, 37);
    let saved = engine
        .load_run_event_by_idempotency_key(
            "router-user",
            state.current_run_id.as_deref().unwrap(),
            EVENT_TYPE,
            DECISION_KEY,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved["data"]["reason"], "easy_read_only");
    assert!(!saved.to_string().contains("provider-secret"));
    first
        .prepare_auto_model_selection(&mut state)
        .await
        .unwrap();

    let mut restored = host(catalog.clone(), engine.clone());
    restored
        .model_routing
        .as_mut()
        .unwrap()
        .policy
        .economy_offering_id = "new-policy-candidate".into();
    restored.model_routing.as_mut().unwrap().policy.revision = "new-revision".into();
    state.llm_rounds_completed = 2;
    state.turn_intent = None;
    restored
        .prepare_auto_model_selection(&mut state)
        .await
        .unwrap();
    assert_eq!(
        restored
            .admitted_model_execution
            .as_ref()
            .unwrap()
            .offering_id,
        "economy"
    );
    assert_eq!(
        engine
            .load_run_event_by_idempotency_key(
                "router-user",
                state.current_run_id.as_deref().unwrap(),
                EVENT_TYPE,
                DECISION_KEY
            )
            .await
            .unwrap()
            .unwrap(),
        saved
    );

    catalog.models.write().unwrap().remove("economy");
    restored.revalidate_catalog_execution().await.unwrap();
    restored
        .admitted_model_execution
        .as_mut()
        .unwrap()
        .admitted_at = Instant::now()
        - super::super::RESOLVED_TURN_LLM_CONFIG_CACHE_TTL
        - std::time::Duration::from_secs(1);
    assert!(restored.revalidate_catalog_execution().await.is_err());
}

#[tokio::test]
async fn explicit_model_change_applies_at_the_next_round_after_auto() {
    use astra_turn_types::{ReasoningRequirement, RunModelOutcomeKind, RunModelRequest};
    let (catalog, engine, mut host, mut state) = fixture().await;
    state.run_control = Some(Arc::new(engine.clone()));
    crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
        .await
        .unwrap();
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().offering_id,
        "economy"
    );
    let run_id = state.current_run_id.clone().unwrap();
    let request = RunModelRequest {
        expected_session_id: "router-session".into(),
        request_id: "switch-to-strong".into(),
        model: astra_turn_types::ModelSelection {
            offering_id: "strong".into(),
        },
        reasoning: ReasoningRequirement::ModelDefault,
    };
    engine
        .request_run_model("router-user", &run_id, &request)
        .await
        .unwrap();
    // The currently admitted request remains untouched until another round.
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().offering_id,
        "economy"
    );
    crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 1)
        .await
        .unwrap();
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().offering_id,
        "strong"
    );
    let snapshot = engine
        .run_settings_snapshot("router-user", "router-session", &run_id)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        snapshot.model.outcome.unwrap().result,
        RunModelOutcomeKind::Applied { .. }
    ));
    assert_eq!(
        snapshot.model.applied.unwrap().resolved.offering_id,
        "strong"
    );
    assert!(host.model_routing.is_none());
    let current = engine
        .load_run("router-user", &run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.model_offering_id.as_deref(), Some("strong"));
    // Auto revalidation must not repin economy after the user choice.
    assert!(host.revalidate_auto_execution().await.unwrap().is_none());
    // Both delegated agents and forked skills use this shared resolver; the
    // earlier Auto decision must not override the later explicit choice.
    let inherited = crate::server::model_execution_admission::inherit_routed_execution(
        &engine,
        "router-user",
        "router-session",
        &run_id,
        None,
        |id| async {
            catalog
                .admit_model_offering("router-user".into(), id)
                .await
                .map_err(|_| "not authorized".into())
        },
    )
    .await
    .unwrap()
    .0
    .unwrap();
    assert_eq!(inherited.offering_id, "strong");
    catalog.models.write().unwrap().remove("strong");
    assert!(
        crate::server::model_execution_admission::inherit_routed_execution(
            &engine,
            "router-user",
            "router-session",
            &run_id,
            Some(&inherited),
            |id| async {
                catalog
                    .admit_model_offering("router-user".into(), id)
                    .await
                    .map_err(|_| "not authorized".into())
            },
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn newer_model_choice_wins_before_the_next_provider_request() {
    use astra_turn_types::{ReasoningRequirement, RunModelOutcomeKind, RunModelRequest};
    for lose_ack in [false, true] {
        let (catalog, engine, mut host, mut state) = fixture().await;
        let response = json!({
            "choices":[{"message":{"content":"done"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":12,"completion_tokens":24}
        });
        let provider = ProviderGateway::start(vec![ProviderScript::new(
            "newer_model_choice_wins_before_the_next_provider_request",
            |request| request.path == "/v1/chat/completions",
            vec![ProviderResponse::OpenAi(response)],
        )])
        .await;
        let url = format!("{}/v1/chat/completions", provider.base_url);
        for model in catalog.models.write().unwrap().values_mut() {
            model.completions_url_override = Some(url.clone());
        }
        state.run_control = Some(Arc::new(engine.clone()));
        crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        if lose_ack {
            state.run_control = Some(Arc::new(LostModelAck {
                engine: engine.clone(),
                lose_once: AtomicBool::new(true),
            }));
        }
        let run_id = state.current_run_id.clone().unwrap();
        let request = |request_id: &str, offering_id: &str| RunModelRequest {
            expected_session_id: "router-session".into(),
            request_id: request_id.into(),
            model: astra_turn_types::ModelSelection {
                offering_id: offering_id.into(),
            },
            reasoning: ReasoningRequirement::ModelDefault,
        };
        engine
            .request_run_model("router-user", &run_id, &request("first", "strong"))
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *catalog.admission_gate.write().unwrap() = Some(AdmissionGate {
            model: "strong".into(),
            entered: entered.clone(),
            release: release.clone(),
        });
        let (prepared, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(
                    &mut host, &mut state, 1
                ),
                async {
                    entered.notified().await;
                    engine
                        .request_run_model("router-user", &run_id, &request("second", "economy"))
                        .await
                        .unwrap();
                    release.notify_one();
                },
            )
        })
        .await
        .expect("the paused admission must resume");
        prepared.unwrap();
        *catalog.admission_gate.write().unwrap() = None;
        let snapshot = engine
            .run_settings_snapshot("router-user", "router-session", &run_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            snapshot
                .model
                .applied
                .as_ref()
                .unwrap()
                .resolved
                .offering_id,
            "economy"
        );
        assert!(matches!(
            snapshot.model.outcome.unwrap().result,
            RunModelOutcomeKind::Applied { .. }
        ));
        let run = engine
            .load_run("router-user", &run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            run.events
                .iter()
                .any(|event| event["data"]["change"]["kind"] == "model_outcome"
                    && event["data"]["change"]["value"]["selection"]["request_id"] == "first"
                    && event["data"]["change"]["value"]["result"]["status"] == "superseded")
        );
        state.run_control = None;
        host.inference_owner_pod_id = Some("test-inference-owner".into());
        host.provider_observations =
            Some(crate::server::provider_test_support::ProviderObservationsFixture::default());
        host.execute_turn(&mut state).await.unwrap();
        let recorded = provider.requests.lock().await.clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].body["model"], "wire-economy");
        provider.assert_complete();
    }
}

#[tokio::test]
async fn active_model_change_applies_after_the_current_tool_round() {
    use crate::server::tool_transport::{
        ExecutionBindingSnapshot, ExecutorBinding, WorkspaceBinding,
    };
    use astra_turn_types::{ReasoningRequirement, RunModelRequest};

    let (catalog, engine, _, mut state) = fixture().await;
    let workspace = tempfile::tempdir().unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let entered_provider = entered.clone();
    let chunks = [
        json!({"choices":[{"delta":{"reasoning_content":"private to the first model"}}]}),
        json!({"choices":[{"delta":{"content":concat!(
            "<｜DSML｜ calls><｜DSML｜ invoke name=\"tool_search\">",
            "<｜DSML｜ parameter name=\"query\" string=\"true\">select:glob</｜DSML｜ parameter>",
            "</｜DSML｜ invoke></｜DSML｜ calls>"
        )},"finish_reason":"stop"}]}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n").into_bytes())
    .chain(std::iter::once(b"data: [DONE]\n\n".to_vec()))
    .collect();
    let provider = ProviderGateway::start(vec![
        ProviderScript::new(
            "first model finishes its in-flight tool call",
            move |request| {
                let matches = request.path == "/v1/chat/completions"
                    && request.body["model"] == "wire-strong";
                if matches {
                    entered_provider.notify_one();
                }
                matches
            },
            vec![ProviderResponse::Stream {
                content_type: "text/event-stream",
                chunks,
                release_before_chunk: Some((1, release.clone())),
            }],
        ),
        ProviderScript::new(
            "new model continues with the tool result",
            |request| request.path == "/v1/chat/completions" && request.body["model"] == "wire-economy",
            vec![ProviderResponse::OpenAi(json!({
                "choices":[{"message":{"content":"continued with the selected model"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":24}
            }))],
        ),
    ]).await;
    let url = format!("{}/v1/chat/completions", provider.base_url);
    for model in catalog.models.write().unwrap().values_mut() {
        model.completions_url_override = Some(url.clone());
    }
    let baseline = catalog.models.read().unwrap()["strong"].clone();
    let observations = crate::server::provider_test_support::ProviderObservationsFixture::default();
    let mut host = ServerAgenticLoopHostBuilder::new(
        mock_matrixone(),
        mock_encryptor(),
        "router-user".into(),
        "router-session".into(),
    )
    .with_admitted_model_execution(Some(baseline))
    .with_model_service(Some(catalog))
    .with_provider_observations(observations.clone())
    .with_execution_binding_snapshot(ExecutionBindingSnapshot::inferred(
        WorkspaceBinding::server_sandbox(workspace.path()),
        ExecutorBinding::server_local(),
    ))
    .with_turn_intent_policy(TurnIntentExecutionPolicy::FixedDefault)
    .build();
    let decision = astra_services::parse_work_admission_response(r#"{"work_lifecycle":"not_required","workspace_mutation":"read_only","execution_topology":"primary","assessment":{"difficulty":"easy","difficulty_confidence":"high"}}"#).unwrap();
    host.apply_work_admission_decision(decision);
    host.completed_work_admission_phase =
        Some((Instant::now(), Instant::now(), 0, TurnPhaseOutcome::Decided));
    state.run_control = Some(Arc::new(engine.clone()));
    state.skills.request_constraints.allowed_tools = Some(
        ["glob".to_string(), "tool_search".to_string()]
            .into_iter()
            .collect(),
    );
    state.permission_context = Some(crate::orchestration::PermissionSyncContext::shared_root(
        crate::orchestration::PermissionMode::Auto,
    ));
    let ledger =
        crate::server::tool_invocation_runtime::RuntimeToolInvocationLedger::new_process_local(
            engine.clone(),
        )
        .unwrap();
    let mut executor = crate::server::runtime_tool_executor::RuntimeToolExecutor::new(
        workspace.path().to_path_buf(),
        "router-user".into(),
        "router-session".into(),
        None,
        None,
    );
    executor.set_invocation_ledger(ledger);
    executor.set_execution_bindings(
        WorkspaceBinding::server_sandbox(workspace.path()),
        ExecutorBinding::server_local(),
    );
    state.runtime_tool_executor = Some(Arc::new(executor));
    let run_id = state.current_run_id.clone().unwrap();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            crate::turn::agentic_loop::host::run_agentic_loop_with_host(&mut host, &mut state),
            async {
                entered.notified().await;
                engine
                    .request_run_model(
                        "router-user",
                        &run_id,
                        &RunModelRequest {
                            expected_session_id: "router-session".into(),
                            request_id: "switch-during-tool-round".into(),
                            model: astra_turn_types::ModelSelection {
                                offering_id: "economy".into(),
                            },
                            reasoning: ReasoningRequirement::ModelDefault,
                        },
                    )
                    .await
                    .unwrap();
                release.notify_one();
            }
        )
    })
    .await
    .expect("the active model change and the tool round must complete");
    result.expect("the same run must continue after changing model");
    assert_eq!(state.final_text, "continued with the selected model");
    assert_eq!(
        state
            .stall
            .tool_call_records
            .iter()
            .filter(|record| record.name == "tool_search" && record.was_executed())
            .count(),
        1
    );
    let requests = provider.requests.lock().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body["model"], "wire-strong");
    assert_eq!(requests[1].body["model"], "wire-economy");
    let messages = requests[1].body["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .expect("the next model receives the tool call");
    let call = assistant["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|call| call["function"]["name"] == "tool_search")
        .expect("the original tool call remains available");
    let arguments: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(arguments["query"], "select:glob");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == call["id"])
        .expect("the tool result remains paired with its call");
    assert!(result["content"].to_string().contains("glob"));
    assert!(!assistant["content"].to_string().contains("DSML"));
    assert!(
        !requests[1]
            .body
            .to_string()
            .contains("private to the first model")
    );
    drop(requests);
    observations.assert_quiescent();
    provider.assert_complete();
}

#[tokio::test(start_paused = true)]
async fn cancellation_or_deadline_interrupts_model_admission_without_an_outcome() {
    use astra_turn_types::{ReasoningRequirement, RunModelRequest};
    for expire in [false, true] {
        let (catalog, engine, mut host, mut state) = fixture().await;
        state.run_control = Some(Arc::new(engine.clone()));
        crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        let run_id = state.current_run_id.clone().unwrap();
        engine
            .request_run_model(
                "router-user",
                &run_id,
                &RunModelRequest {
                    expected_session_id: "router-session".into(),
                    request_id: "cancelled-choice".into(),
                    model: astra_turn_types::ModelSelection {
                        offering_id: "strong".into(),
                    },
                    reasoning: ReasoningRequirement::ModelDefault,
                },
            )
            .await
            .unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        *catalog.admission_gate.write().unwrap() = Some(AdmissionGate {
            model: "strong".into(),
            entered: entered.clone(),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        state.cancellation.token = Some(cancel.clone());
        if expire {
            host.execution_time_budget = Some(
                ExecutionDeadlineAuthority::from_snapshot_at(
                    astra_services::runs::ExecutionDeadlineSnapshot {
                        deadline_unix_ms: 100,
                        work_deadline_unix_ms: 100,
                    },
                    0,
                )
                .unwrap(),
            );
        }
        let (prepared, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(
                    &mut host, &mut state, 1
                ),
                async {
                    entered.notified().await;
                    if !expire {
                        cancel.cancel();
                    }
                },
            )
        })
        .await
        .expect("cancellation must not wait for model admission");
        let expected = if expire {
            astra_core::ErrorKind::BudgetExhausted
        } else {
            astra_core::ErrorKind::Cancelled
        };
        assert!(matches!(prepared, Err(error) if error.kind == expected));
        let snapshot = engine
            .run_settings_snapshot("router-user", "router-session", &run_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            snapshot.model.outcome.is_none(),
            "cancelled admission must not publish an outcome"
        );
        assert_eq!(
            snapshot.model.requested.unwrap().request_id,
            "cancelled-choice"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_or_deadline_interrupts_restoring_an_applied_model() {
    use astra_turn_types::{ReasoningRequirement, RunModelSelection};
    for expire in [false, true] {
        let (catalog, _, mut host, mut state) = fixture().await;
        host.admitted_model_execution = Some(catalog.models.read().unwrap()["economy"].clone());
        let selection = RunModelSelection {
            request_id: "applied".into(),
            revision: 1,
            model: astra_turn_types::ModelSelection {
                offering_id: "strong".into(),
            },
            reasoning: ReasoningRequirement::ModelDefault,
        };
        let applied = astra_services::runs::RunModelApplied {
            selection,
            resolved: astra_services::runs::ResolvedModelSelection {
                offering_id: "strong".into(),
                model_name: "strong".into(),
                source_identity: None,
            },
            reasoning: ReasoningRequirement::ModelDefault,
            owner_generation: 0,
            round_index: 0,
        };
        let entered = Arc::new(tokio::sync::Notify::new());
        *catalog.admission_gate.write().unwrap() = Some(AdmissionGate {
            model: "strong".into(),
            entered: entered.clone(),
            release: Arc::new(tokio::sync::Notify::new()),
        });
        let cancel = Arc::new(tokio_util::sync::CancellationToken::new());
        state.cancellation.token = Some(cancel.clone());
        if expire {
            host.execution_time_budget = Some(
                ExecutionDeadlineAuthority::from_snapshot_at(
                    astra_services::runs::ExecutionDeadlineSnapshot {
                        deadline_unix_ms: 100,
                        work_deadline_unix_ms: 100,
                    },
                    0,
                )
                .unwrap(),
            );
        }
        let (result, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(host.ensure_applied_model(&mut state, &applied), async {
                entered.notified().await;
                if !expire {
                    cancel.cancel();
                }
            })
        })
        .await
        .expect("restoring an applied model must be interruptible");
        let expected = if expire {
            astra_core::ErrorKind::BudgetExhausted
        } else {
            astra_core::ErrorKind::Cancelled
        };
        assert!(matches!(result, Err(error) if error.kind == expected));
        assert_eq!(
            host.admitted_model_execution.as_ref().unwrap().offering_id,
            "economy"
        );
    }
}

#[tokio::test]
async fn rotated_catalog_route_preserves_public_tool_history_on_the_wire() {
    let (catalog, _engine, mut host, mut state) = fixture().await;
    host.model_routing = None;
    host.inference_owner_pod_id = Some("test-inference-owner".into());
    host.provider_observations =
        Some(crate::server::provider_test_support::ProviderObservationsFixture::default());
    let response = json!({"choices":[{"message":{"content":"done"},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":12,"completion_tokens":24}});
    let provider = ProviderGateway::start(vec![ProviderScript::new(
        "rotated_catalog_route_preserves_public_tool_history_on_the_wire",
        |request| request.path == "/v1/chat/completions",
        vec![ProviderResponse::OpenAi(response)],
    )])
    .await;
    let mut initial = catalog.models.read().unwrap()["strong"].clone();
    initial.api_key = "old-account".into();
    initial.base_url = format!("{}/old", provider.base_url);
    initial.completions_url_override =
        Some(format!("{}/old/v1/chat/completions", provider.base_url));
    catalog
        .models
        .write()
        .unwrap()
        .insert("strong".into(), initial.clone());
    host.admitted_model_execution = Some(initial);
    host.resolve_llm_config_for_state(&state).await.unwrap();
    let mut assistant = json!({"role":"assistant", "content":"working", "reasoning_content":"old-private-reasoning",
        "tool_calls":[{"id":"rotation-call", "type":"function", "function":{"name":"read_file", "arguments":"{}"}}]});
    host.attest_current_model_message(&state, &mut assistant);
    state.messages = vec![
        json!({"role":"user","content":"inspect"}),
        assistant,
        json!({"role":"tool", "tool_call_id":"rotation-call", "content":"public-tool-result"}),
    ];
    {
        let mut models = catalog.models.write().unwrap();
        let updated = models.get_mut("strong").unwrap();
        updated.api_key = "new-account".into();
        updated.base_url = provider.base_url.clone();
        updated.completions_url_override =
            Some(format!("{}/v1/chat/completions", provider.base_url));
    }
    host.admitted_model_execution.as_mut().unwrap().admitted_at =
        Instant::now() - RESOLVED_TURN_LLM_CONFIG_CACHE_TTL;
    let rotated = host.resolve_llm_config_for_state(&state).await.unwrap();
    assert_eq!(rotated.api_key, "new-account");
    host.execute_turn(&mut state).await.unwrap();
    let requests = provider.requests.lock().await.clone();
    assert_eq!(requests.len(), 1);
    let wire = requests[0].body.to_string();
    assert!(wire.contains("public-tool-result"), "{wire}");
    assert!(!wire.contains("old-private-reasoning"), "{wire}");
    provider.assert_complete();
}

#[tokio::test]
async fn unavailable_incompatible_and_cross_billing_economy_retain_strong() {
    for case in [
        "unavailable",
        "context",
        "output",
        "protocol",
        "billing",
        "multimodal",
        "assessment",
    ] {
        let (catalog, _, mut host, mut state) = fixture().await;
        match case {
            "unavailable" => {
                catalog.models.write().unwrap().remove("economy");
            }
            "context" => {
                catalog
                    .models
                    .write()
                    .unwrap()
                    .get_mut("economy")
                    .unwrap()
                    .context_window = Some(4_000)
            }
            "output" => {
                catalog
                    .models
                    .write()
                    .unwrap()
                    .get_mut("economy")
                    .unwrap()
                    .max_completion_tokens = None
            }
            "protocol" => {
                catalog
                    .models
                    .write()
                    .unwrap()
                    .get_mut("economy")
                    .unwrap()
                    .provider = "anthropic".into()
            }
            "billing" => catalog.separate_billing.store(true, Ordering::SeqCst),
            "multimodal" => host.model_routing.as_mut().unwrap().supported_input = false,
            "assessment" => {
                host.pending_work_admission = None;
                host.completed_work_admission_phase = None;
            }
            _ => unreachable!(),
        }
        host.prepare_auto_model_selection(&mut state).await.unwrap();
        assert_eq!(
            host.admitted_model_execution.as_ref().unwrap().offering_id,
            "strong",
            "{case}"
        );
        assert_ne!(
            host.model_routing
                .as_ref()
                .unwrap()
                .decision
                .as_ref()
                .unwrap()
                .reason,
            ModelRoutingReason::EasyReadOnly
        );
    }
}

#[tokio::test]
async fn persistence_failure_and_missing_resume_decision_stop_selection() {
    let (_, engine, mut host, mut state) = fixture().await;
    state.current_run_owner_generation = Some(900);
    assert!(host.prepare_auto_model_selection(&mut state).await.is_err());
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().offering_id,
        "strong"
    );
    assert!(
        engine
            .load_run_event_by_idempotency_key(
                "router-user",
                state.current_run_id.as_deref().unwrap(),
                EVENT_TYPE,
                DECISION_KEY
            )
            .await
            .unwrap()
            .is_none()
    );
    state.current_run_owner_generation = Some(0);
    state.llm_rounds_completed = 1;
    assert!(host.prepare_auto_model_selection(&mut state).await.is_err());
}

#[tokio::test]
async fn changed_contract_is_rejected_but_rotated_credentials_are_reauthorized() {
    let (catalog, engine, mut host, mut state) = fixture().await;
    host.prepare_auto_model_selection(&mut state).await.unwrap();
    catalog
        .models
        .write()
        .unwrap()
        .get_mut("economy")
        .unwrap()
        .api_key = "rotated".into();
    host.revalidate_catalog_execution().await.unwrap();
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().api_key,
        "provider-secret"
    );
    host.admitted_model_execution.as_mut().unwrap().admitted_at = Instant::now()
        - super::super::RESOLVED_TURN_LLM_CONFIG_CACHE_TTL
        - std::time::Duration::from_secs(1);
    host.revalidate_catalog_execution().await.unwrap();
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().api_key,
        "rotated"
    );
    catalog
        .models
        .write()
        .unwrap()
        .get_mut("economy")
        .unwrap()
        .wire_model_name = Some("different-revision".into());
    host.admitted_model_execution.as_mut().unwrap().admitted_at = Instant::now()
        - super::super::RESOLVED_TURN_LLM_CONFIG_CACHE_TTL
        - std::time::Duration::from_secs(1);
    assert!(host.revalidate_catalog_execution().await.is_err());
    let mut restored = self::host(catalog, engine);
    assert!(
        restored
            .prepare_auto_model_selection(&mut state)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn http_auto_admission_requires_opt_in_config_and_rejects_explicit_or_provider_models() {
    let catalog: Arc<dyn ModelService> = catalog();
    let parse = |value| {
        astra_server_types::chat_request_into_data(
            serde_json::from_value::<astra_server_types::ChatRequest>(value).unwrap(),
        )
    };
    let wire = json!({"message":"explain this", "execution_policy":{"model_routing":"auto"}});
    let admitted = crate::server::model_execution_admission::admit_auto_model_request(
        &catalog,
        "router-user",
        parse(wire.clone()),
        Some(policy()),
    )
    .await
    .unwrap();
    assert_eq!(admitted.model.as_deref(), Some("strong"));
    assert!(matches!(
        admitted.model_selection_mode,
        astra_services::runs::ModelSelectionMode::Auto(_)
    ));
    assert!(
        crate::server::model_execution_admission::admit_auto_model_request(
            &catalog,
            "router-user",
            parse(wire.clone()),
            None
        )
        .await
        .is_err()
    );
    assert!(
        crate::server::model_execution_admission::admit_auto_model_request(
            &catalog,
            "other-user",
            parse(wire.clone()),
            Some(policy())
        )
        .await
        .is_err()
    );
    for case in ["explicit", "provider", "fixed", "expected"] {
        let mut request = parse(wire.clone());
        match case {
            "explicit" => {
                request.model_selection = Some(astra_turn_types::ModelSelection {
                    offering_id: "strong".into(),
                })
            }
            "provider" => request.provider_runtime_authorized = true,
            "fixed" => {
                request.execution_policy.turn_intent =
                    astra_services::runs::TurnIntentExecutionPolicy::FixedDefault
            }
            "expected" => request.expected_model_name = Some("strong-model".into()),
            _ => unreachable!(),
        }
        assert!(
            crate::server::model_execution_admission::admit_auto_model_request(
                &catalog,
                "router-user",
                request,
                Some(policy())
            )
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn auto_children_inherit_the_committed_model_instead_of_cached_baseline() {
    let (catalog, engine, mut first, mut state) = fixture().await;
    let baseline = first.admitted_model_execution.clone();
    first
        .prepare_auto_model_selection(&mut state)
        .await
        .unwrap();
    let run_id = state.current_run_id.as_deref().unwrap();
    let parent = engine
        .load_run("router-user", run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(parent.model_offering_id.as_deref(), Some("economy"));
    assert_eq!(parent.resolved_model_name.as_deref(), Some("economy"));
    // The same resolver is used by dynamic-agent factories and forked skills,
    // both of which were constructed before selection completed.
    let inherited = crate::server::model_execution_admission::inherit_routed_execution(
        &engine,
        "router-user",
        "router-session",
        run_id,
        baseline.as_ref(),
        |id| async {
            catalog
                .admit_model_offering("router-user".into(), id)
                .await
                .map_err(|_| "not authorized".into())
        },
    )
    .await
    .unwrap()
    .0
    .unwrap();
    assert_eq!(inherited.offering_id, "economy");
    engine
        .start_run_ext_with_context(
            "routed-child",
            "router-user",
            "router-session",
            Some(run_id),
            None,
            None,
            None,
            // Let the existing child-admission owner inherit the committed
            // parent identity; this fixture must not forge admission authority.
            Default::default(),
        )
        .await
        .unwrap();
    // DelegationEngine pre-creates children without model material. It must
    // inherit exactly the same identity before its executor starts.
    engine
        .start_run_ext_with_context(
            "routed-delegation",
            "router-user",
            "router-session",
            Some(run_id),
            None,
            None,
            None,
            Default::default(),
        )
        .await
        .unwrap();
    let child = engine
        .load_run("router-user", "routed-delegation")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(child.model_offering_id.as_deref(), Some("economy"));
    catalog.models.write().unwrap().remove("economy");
    assert!(
        crate::server::model_execution_admission::inherit_routed_execution(
            &engine,
            "router-user",
            "router-session",
            run_id,
            baseline.as_ref(),
            |id| async {
                catalog
                    .admit_model_offering("router-user".into(), id)
                    .await
                    .map_err(|_| "not authorized".into())
            },
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn auto_recovery_restores_required_work_before_first_primary_request() {
    let (catalog, engine, mut first, mut state) = fixture().await;
    let admission = astra_services::WorkAdmissionDecision::Required {
        assessment: None,
        domain: None,
        workspace_mutation: astra_config::user_profile::WorkspaceMutationIntent::ReadOnly,
        mutation_completion_scope: astra_config::user_profile::MutationCompletionScope::Unknown,
        goal: "Inspect the source".into(),
        tasks: vec![astra_services::WorkAdmissionTask {
            after_initial_tasks: vec![],
            objective: "Inspect source".into(),
            expected_result: "Cited finding".into(),
        }],
        deferred_graph_mutations: vec![],
        activation: astra_services::WorkAdmissionActivation::Start,
        execution_topology: astra_services::WorkExecutionTopology::Primary,
        required_capabilities: vec![astra_services::WorkAdmissionCapability::Web],
    };
    first.apply_work_admission_decision(admission.clone());
    first
        .prepare_auto_model_selection(&mut state)
        .await
        .unwrap();
    assert!(
        first
            .take_admitted_work_establishment_call(&state)
            .is_some()
    );
    let mut restored = host(catalog, engine);
    state.turn_intent = None;
    crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut restored, &mut state, 0)
        .await
        .unwrap();
    assert_eq!(
        restored
            .pending_work_admission
            .as_ref()
            .map(|assessment| &assessment.decision),
        Some(&admission)
    );
    assert_eq!(
        restored.work_admission_capabilities,
        admission.required_capabilities()
    );
    assert_eq!(
        state.turn_intent.as_ref().unwrap().work_lifecycle,
        WorkLifecycleIntent::Required
    );
    assert!(
        restored.pending_work_admission_judge.is_none(),
        "no second judge request"
    );
    let call = restored
        .take_admitted_work_establishment_call(&state)
        .unwrap();
    let args: Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["goal"], "Inspect the source");
    assert_eq!(args["tasks"][0]["objective"], "Inspect source");
}

#[tokio::test]
async fn retired_decisions_fail_before_recovery_or_child_resolution() {
    use astra_services::runs::RunStateStore;
    let (catalog, engine, mut first, mut state) = fixture().await;
    let run_id = state.current_run_id.clone().unwrap();
    let original = engine
        .load_run("router-user", &run_id)
        .await
        .unwrap()
        .unwrap();
    first
        .prepare_auto_model_selection(&mut state)
        .await
        .unwrap();
    let event = engine
        .load_run_event_by_idempotency_key("router-user", &run_id, EVENT_TYPE, DECISION_KEY)
        .await
        .unwrap()
        .unwrap();
    for change in [
        json!({"rollout": null}),
        json!({"rollout": {"cohort": "control"}}),
        json!({"rollout": {"cohort": "shadow"}}),
        json!({"rollout": {"cohort": "treatment"}}),
        json!({"policy_version": "learned-canary-v1"}),
        json!({"policy_version": "unknown-policy"}),
    ] {
        // Simulate persisted historical input at the storage boundary. New
        // append admission is separately exercised for both database/memory stores.
        let mut historic = original.clone();
        let mut saved = event.clone();
        saved["data"]
            .as_object_mut()
            .unwrap()
            .extend(change.as_object().unwrap().clone());
        historic.events.push(saved);
        let store = Arc::new(astra_services::runs::InMemoryRunStateStore::new());
        store.insert_run(historic).await.unwrap();
        let engine = RunEngine::new(store);
        let before = engine
            .load_run("router-user", &run_id)
            .await
            .unwrap()
            .unwrap();
        let mut recovered = host(catalog.clone(), engine.clone());
        assert!(recovered.restore_model_routing(&state).await.is_err());
        assert!(recovered.model_routing.as_ref().unwrap().decision.is_none());
        assert!(
            crate::server::model_execution_admission::inherit_routed_execution(
                &engine,
                "router-user",
                "router-session",
                &run_id,
                None,
                |_| async { panic!("invalid persisted decision must not resolve an Offering") },
            )
            .await
            .is_err()
        );
        assert_eq!(
            engine
                .load_run("router-user", &run_id)
                .await
                .unwrap()
                .unwrap(),
            before
        );
    }
}
