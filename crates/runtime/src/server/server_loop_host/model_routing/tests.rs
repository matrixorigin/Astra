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
        .with_model_service(Some(catalog.clone()))
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
    state.context_manifest_user_id = Some("router-user".into());
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

#[derive(Default)]
struct RolloutMemory(std::sync::Mutex<RouterRolloutState>);
#[async_trait]
impl RouterRolloutStore for RolloutMemory {
    async fn load(&self, _: &str) -> Result<RouterRolloutState, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    async fn change(
        &self,
        owner: &str,
        expected: u64,
        _: &str,
        change: RolloutChange,
    ) -> Result<RouterRolloutState, String> {
        let mut state = self.0.lock().unwrap();
        if state.revision != expected {
            return Err("Rollout revision conflict".into());
        }
        *state = transition(state.clone(), owner, change, Utc::now())?;
        Ok(state.clone())
    }
}
fn deployment_for_test(catalog: &Catalog) -> RouterDeployment {
    use astra_services::model_routing::offline::{ModelProfile, content_sha256};
    use astra_services::tuning::{
        RouterQualificationProtocol, RouterQualificationStatus, RouterTuningRecord,
    };
    use astra_turn_core::model_routing::offline::{
        OutcomeBucket, OutcomeEstimate, RouterCandidate, RouterTrainingConfig,
    };
    let profile = |id: &str| ModelProfile {
        profile_id: id.into(),
        offering_id: id.into(),
        contract_root: crate::server::model_execution_admission::model_execution_contract_root(
            &catalog.models.read().unwrap()[id],
        ),
    };
    let features = astra_turn_types::model_routing::ModelRoutingFeatures::new(
        Some(astra_turn_types::TurnAssessment {
            difficulty: astra_turn_types::TaskDifficulty::Moderate,
            difficulty_confidence: astra_turn_types::AssessmentConfidence::High,
            ..Default::default()
        }),
        true,
        true,
    );
    let config = RouterTrainingConfig::default();
    let candidate = RouterCandidate {
        schema_version: 1,
        algorithm_version: "categorical-paired-outcomes-v1".into(),
        feature_version: 1,
        dataset_sha256: "data-digest".into(),
        economy: profile("economy"),
        strong: profile("strong"),
        config: config.clone(),
        threshold: Some(0.5),
        buckets: [(
            serde_json::to_string(&features).unwrap(),
            OutcomeBucket {
                economy: OutcomeEstimate {
                    groups: 100,
                    acceptable: 100,
                    mean_cost_usd: 0.01,
                },
                strong: OutcomeEstimate {
                    groups: 100,
                    acceptable: 100,
                    mean_cost_usd: 0.1,
                },
            },
        )]
        .into(),
        activation: "offline_only".into(),
    };
    let now = Utc::now();
    let expires = now + chrono::Duration::days(1);
    let protocol = RouterQualificationProtocol {
        schema_version: 1,
        job_id: "job-1".into(),
        owner_id: "router-user".into(),
        dataset_id: "dataset".into(),
        registered_at: now - chrono::Duration::days(1),
        training_config_sha256: content_sha256(&config).unwrap(),
        evaluation_plan_sha256: "plan-digest".into(),
        minimum_test_groups: 100,
        minimum_stratum_groups: 100,
        minimum_pair_coverage: 0.95,
        maximum_quality_regression: 0.01,
        minimum_cost_saving_fraction: 0.2,
        maximum_episode_cost_usd: 1.0,
        maximum_p95_latency_ratio: 1.1,
        confidence: 0.95,
        required_strata: vec![features],
    };
    let tuning = RouterTuningRecord {
        schema_version: 1,
        job_id: protocol.job_id.clone(),
        owner_id: "router-user".into(),
        dataset_sha256: candidate.dataset_sha256.clone(),
        candidate_sha256: content_sha256(&candidate).unwrap(),
        protocol_sha256: content_sha256(&protocol).unwrap(),
        evaluated_at: now,
        expires_at: expires,
        source_ids: vec!["source-1".into()],
        status: RouterQualificationStatus::ReadyForShadow,
        production_qualified: false,
    };
    let mut d = RouterDeployment {
        deployment_id: "deployment-1".into(),
        owner_id: "router-user".into(),
        policy_revision: policy().revision,
        rubric_version: "rubric-1".into(),
        candidate_json: serde_json::to_string(&candidate).unwrap(),
        tuning,
        protocol,
        review: RolloutReview {
            online_consent_reference: "consent-1".into(),
            verifier_review_reference: "verify-1".into(),
            safety_review_reference: "safety-1".into(),
            expires_at: expires,
            minimum_shadow_sessions: 1,
            maximum_routing_overhead_ms: 1000,
        },
        mode: RolloutMode::Shadow,
        canary_basis_points: 0,
        assignment_salt: "seed".into(),
        created_at: now,
        expires_at: expires,
        stop_reason: None,
    };
    // Deterministically find a test seed selecting the test session at 10%.
    for i in 0..1000 {
        d.assignment_salt = format!("seed-{i}");
        if astra_turn_core::model_routing::rollout::session_bucket(&d, "router-session") < 1000 {
            break;
        }
    }
    d
}

#[tokio::test]
async fn live_rollout_shadow_canary_recovery_and_kill_switch_share_auto_entrypoint() {
    for mode in [RolloutMode::Shadow, RolloutMode::Canary] {
        let (catalog, engine, mut host, mut state) = fixture().await;
        let mut d = deployment_for_test(&catalog);
        d.mode = mode;
        d.canary_basis_points = if mode == RolloutMode::Canary { 1000 } else { 0 };
        let store = Arc::new(RolloutMemory(std::sync::Mutex::new(RouterRolloutState {
            revision: 2,
            deployment: Some(d),
            ..Default::default()
        })));
        host.model_routing.as_mut().unwrap().rollout_store = Some(store.clone());
        let admission = astra_services::parse_work_admission_response(r#"{"work_lifecycle":"not_required","workspace_mutation":"read_only","execution_topology":"primary","assessment":{"difficulty":"moderate","difficulty_confidence":"high"}}"#).unwrap();
        host.apply_work_admission_decision(admission);
        crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .unwrap();
        let expected = if mode == RolloutMode::Canary {
            "economy"
        } else {
            "strong"
        };
        assert_eq!(
            host.admitted_model_execution.as_ref().unwrap().offering_id,
            expected
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
        assert_eq!(saved["data"]["rollout"]["proposed_offering_id"], "economy");
        assert_eq!(saved["data"]["selected_offering_id"], expected);
        let mut recovered = self::host(catalog.clone(), engine.clone());
        recovered.model_routing.as_mut().unwrap().rollout_store = Some(store.clone());
        recovered
            .prepare_auto_model_selection(&mut state)
            .await
            .unwrap();
        assert_eq!(
            recovered
                .admitted_model_execution
                .as_ref()
                .unwrap()
                .offering_id,
            expected
        );
        recovered.revalidate_auto_execution().await.unwrap();
        store
            .change(
                "router-user",
                2,
                "admin",
                RolloutChange::Rollback {
                    reason: "manual_kill".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            recovered.revalidate_auto_execution().await.is_err(),
            mode == RolloutMode::Canary
        );
        let mut after_kill = self::host(catalog.clone(), engine.clone());
        after_kill.model_routing.as_mut().unwrap().rollout_store = Some(store);
        assert_eq!(
            after_kill
                .prepare_auto_model_selection(&mut state)
                .await
                .is_err(),
            mode == RolloutMode::Canary
        );
    }
}

#[tokio::test]
async fn live_shadow_contract_drift_never_changes_deterministic_auto() {
    let (catalog, _, mut host, mut state) = fixture().await;
    let mut d = deployment_for_test(&catalog);
    let mut candidate = deployment_candidate(&d).unwrap();
    candidate.strong.contract_root = "old-strong-contract".into();
    d.candidate_json = serde_json::to_string(&candidate).unwrap();
    d.tuning.candidate_sha256 = candidate_json_sha256(&d.candidate_json);
    host.model_routing.as_mut().unwrap().rollout_store = Some(Arc::new(RolloutMemory(
        std::sync::Mutex::new(RouterRolloutState {
            revision: 1,
            deployment: Some(d),
            ..Default::default()
        }),
    )));
    crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
        .await
        .unwrap();
    assert_eq!(
        host.admitted_model_execution.as_ref().unwrap().offering_id,
        "economy"
    );
    let decision = host
        .model_routing
        .as_ref()
        .unwrap()
        .decision
        .as_ref()
        .unwrap();
    assert_eq!(decision.reason, ModelRoutingReason::EasyReadOnly);
    let shadow = decision.rollout.as_ref().unwrap();
    assert!(shadow.admission_rejected && shadow.abstained);
}

#[tokio::test]
async fn over_budget_treatment_is_durable_reportable_and_cannot_resume() {
    struct SlowRegistry(RouterRolloutState);
    #[async_trait::async_trait]
    impl RouterRolloutStore for SlowRegistry {
        async fn load(&self, _: &str) -> Result<RouterRolloutState, String> {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(self.0.clone())
        }
        async fn change(
            &self,
            _: &str,
            _: u64,
            _: &str,
            _: RolloutChange,
        ) -> Result<RouterRolloutState, String> {
            Err("read-only test registry".into())
        }
    }
    let (catalog, engine, mut host, mut state) = fixture().await;
    let mut d = deployment_for_test(&catalog);
    d.mode = RolloutMode::Canary;
    d.canary_basis_points = 1000;
    d.review.maximum_routing_overhead_ms = 1;
    let store = Arc::new(SlowRegistry(RouterRolloutState {
        revision: 2,
        deployment: Some(d),
        ..Default::default()
    }));
    host.model_routing.as_mut().unwrap().rollout_store = Some(store.clone());
    let admission = astra_services::parse_work_admission_response(r#"{"work_lifecycle":"not_required","workspace_mutation":"read_only","execution_topology":"primary","assessment":{"difficulty":"moderate","difficulty_confidence":"high"}}"#).unwrap();
    host.apply_work_admission_decision(admission);
    assert!(
        crate::turn::agentic_loop::lifecycle::prepare_turn_iteration(&mut host, &mut state, 0)
            .await
            .is_err()
    );
    assert!(!host.model_routing.as_ref().unwrap().applied);
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
    let decision: ModelRoutingDecision = serde_json::from_value(saved["data"].clone()).unwrap();
    let rollout = decision.rollout.unwrap();
    assert_eq!(
        rollout.routing_failure,
        Some(RouterRoutingFailure::OverheadBudgetExceeded)
    );
    assert!(rollout.routing_overhead_us > 1000);
    let report = dashboard(
        &store.0,
        &[RolloutRun {
            run_id: decision.run_id,
            session_id: decision.session_id,
            status: "failed".into(),
            selected_offering_id: decision.selected_offering_id,
            economy_offering_id: decision.policy.economy_offering_id,
            reason: decision.reason,
            rollout,
            outcome: None,
        }],
        false,
    );
    let cohort = &report.cohorts["treatment"];
    assert_eq!(
        (cohort.sessions, cohort.completed, cohort.routing_failures),
        (1, 0, 1)
    );
    assert_eq!(cohort.known_quality, 0);
    assert!(cohort.p95_routing_overhead_us.unwrap() > 1000);
    assert!(cohort.cost_per_acceptable_task.is_none());
    let mut recovered = self::host(catalog, engine);
    recovered.model_routing.as_mut().unwrap().rollout_store = Some(store);
    assert!(
        recovered
            .prepare_auto_model_selection(&mut state)
            .await
            .is_err()
    );
    assert!(!recovered.model_routing.as_ref().unwrap().applied);
}

#[tokio::test]
async fn auto_recovery_retains_source_bound_child_model_requirement() {
    let (catalog, engine, mut first, mut state) = fixture().await;
    let source = delegation_intent_source_from_state(&state).expect("authenticated user intent");
    let admission = first
        .pending_work_admission
        .take()
        .expect("fixture Work decision")
        .decision;
    first.apply_classified_work_admission(ClassifiedWorkAdmission {
        decision: admission,
        delegation_model_requirement: Some(astra_services::WorkAdmissionTruth::Yes),
        source: Some(source.clone()),
        work_handoff_pending: true,
    });
    first
        .prepare_auto_model_selection(&mut state)
        .await
        .unwrap();

    // A new execution owner can recover the same authenticated instruction,
    // but must not turn a positive model requirement into Unconstrained.
    state.current_run_owner_generation = Some(source.owner_generation + 1);
    state.user_intents.set_user_intent_cursor_for_test(42);
    let mut restored = host(catalog.clone(), engine.clone());
    assert!(restored.restore_model_routing(&state).await.unwrap());
    let classified = restored.pending_work_admission.as_ref().unwrap();
    assert_eq!(
        classified.delegation_model_requirement,
        Some(astra_services::WorkAdmissionTruth::Yes)
    );
    assert_eq!(
        classified.source.as_ref().unwrap().owner_generation,
        source.owner_generation + 1
    );
    assert_eq!(classified.source.as_ref().unwrap().control_epoch, 42);
    let absent = r#"{"disposition":"not_applicable"}"#;
    assert!(
        astra_services::delegation_model_requirement::parse_delegation_intent_requirements(
            absent,
            &state.message,
            &[],
            None,
            classified.delegation_model_requirement
                == Some(astra_services::WorkAdmissionTruth::Yes),
        )
        .is_err(),
        "a recovered positive model requirement must reject no-requirement output"
    );

    state.message = "a different instruction".into();
    state.user_intent = state.message.clone();
    state.messages = vec![json!({"role":"user", "content":state.message})];
    let mut stale = host(catalog, engine);
    assert!(stale.restore_model_routing(&state).await.is_err());
}
