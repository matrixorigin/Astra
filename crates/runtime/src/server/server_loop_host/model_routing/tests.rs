use super::super::tests::{
    admitted_test_offering, create_durable_execution_test_state, mock_encryptor, mock_matrixone,
};
use super::*;
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
        let (url, requests, server) = super::super::tests::spawn_gateway(
            StatusCode::OK,
            json!({
                "choices":[{"message":{"content":"done"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":12,"completion_tokens":24}
            }),
        )
        .await;
        for model in catalog.models.write().unwrap().values_mut() {
            model.completions_url_override = Some(url.clone());
        }
        let baseline = catalog.models.read().unwrap()["strong"].clone();
        let ledger = crate::turn::llm::durable::TestInferenceLedgerPersistence::default();
        let mut host = ServerAgenticLoopHostBuilder::new(
            mock_matrixone(),
            mock_encryptor(),
            "router-user".into(),
            "router-session".into(),
        )
        .with_admitted_model_execution(Some(baseline))
        .with_test_inference_ledger(ledger.clone())
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
        assert_eq!(recorded[0]["model"], expected_model);
        assert_eq!(
            judgment_requests.lock().unwrap().len(),
            expected_judgments,
            "later preparation and primary execution must not repeat the assessment"
        );
        ledger.assert_quiescent();
        server.abort();
    }
}

#[derive(Default)]
struct Catalog {
    models: RwLock<HashMap<String, AdmittedModelExecution>>,
    separate_billing: AtomicBool,
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
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    state.current_run_owner_generation = Some(authority.owner_generation);
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
    assert!(restored.revalidate_catalog_execution().await.is_err());
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
        "rotated"
    );
    catalog
        .models
        .write()
        .unwrap()
        .get_mut("economy")
        .unwrap()
        .wire_model_name = Some("different-revision".into());
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
    for case in ["explicit", "provider", "fixed"] {
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
            crate::server::run::engine::RunStartContext {
                model_selection: Some(astra_turn_types::ModelSelection {
                    offering_id: inherited.offering_id.clone(),
                }),
                resolved_model_selection: Some(astra_services::runs::ResolvedModelSelection {
                    offering_id: inherited.offering_id.clone(),
                    model_name: inherited.model_name.clone(),
                }),
                ..Default::default()
            },
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
    assert_eq!(restored.pending_work_admission.as_ref(), Some(&admission));
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
