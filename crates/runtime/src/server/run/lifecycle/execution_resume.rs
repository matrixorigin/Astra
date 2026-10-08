//! Same-turn handoff consumption by the existing run lifecycle owner.

use super::*;
use astra_services::SessionContextCoordinator;

fn invalid_resume(detail: impl Into<String>) -> (StatusCode, Json<ErrorResponse>) {
    error_response_coded(StatusCode::CONFLICT, detail, "execution_resume_unavailable")
}

fn resume_custody_error(
    error: astra_services::SessionContextCoordinatorError,
) -> (StatusCode, Json<ErrorResponse>) {
    use astra_services::SessionContextCoordinatorError as Error;
    let (status, code) = match &error {
        Error::Database {
            operation: "commit_execution_resume",
            ..
        } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "execution_resume_outcome_unknown",
        ),
        Error::Database { .. } | Error::DatabaseJson { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            "execution_resume_storage_unavailable",
        ),
        Error::Unauthorized => (StatusCode::FORBIDDEN, "execution_resume_unauthorized"),
        _ => (StatusCode::CONFLICT, "execution_resume_fenced"),
    };
    error_response_coded(
        status,
        format!("execution custody was not confirmed: {error}"),
        code,
    )
}

fn admission_field<T: serde::de::DeserializeOwned>(
    admission: &Map<String, Value>,
    name: &str,
) -> Result<T, (StatusCode, Json<ErrorResponse>)> {
    let value = admission
        .get(name)
        .ok_or_else(|| invalid_resume(format!("original admission is missing {name}")))?;
    serde_json::from_value(value.clone())
        .map_err(|_| invalid_resume(format!("original admission has invalid {name}")))
}

impl AgenticRunLifecycleService {
    pub(super) async fn resume_execution_handoff(
        &self,
        durable: &DurableRunRecord,
    ) -> Result<bool, (StatusCode, Json<ErrorResponse>)> {
        let checkpoint = self
            .run_engine
            .load_latest_checkpoint(&durable.user_id, &durable.run_id, Some("execution_handoff"))
            .await
            .map_err(|error| Self::durable_persist_error("load execution handoff", error))?;
        let Some(checkpoint) = checkpoint else {
            return Ok(false);
        };
        if self.execution_handoff_requested.is_cancelled() {
            return Err(invalid_resume(
                "server is draining; resume on a live execution owner",
            ));
        }
        // A saved profile has no credentials. Routes whose authority was only
        // request-scoped must supply current runtime context rather than silently
        // acquiring the Server's file or network capabilities.
        let Some(astra_services::runs::DurableAdmissionSource::V1 {
            model_source: astra_services::runs::ModelAdmissionSource::CatalogOffering,
            capability_source: astra_services::runs::RuntimeCapabilitySource::ServerManaged,
        }) = durable
            .admission_source()
            .map_err(|_| invalid_resume("invalid original admission source"))?
        else {
            return Err(invalid_resume(
                "this handoff requires renewed runtime credentials and bindings",
            ));
        };
        if durable.depth != 0 || durable.work_binding.is_some() {
            return Err(invalid_resume(
                "this handoff requires its original parent or Work authority",
            ));
        }
        astra_services::runs::validate_run_checkpoint_size(&checkpoint.checkpoint_json)
            .map_err(invalid_resume)?;
        let astra_services::runs::DurableExecutionHandoff::V1 { heavy: parked, .. } =
            serde_json::from_str::<
                astra_services::runs::DurableExecutionHandoff<
                    server_loop_host::RuntimeExecutionHandoff,
                >,
            >(&checkpoint.checkpoint_json)
            .map_err(|_| invalid_resume("invalid execution handoff"))?;
        if !matches!(
            parked.primary_work,
            runtime_tool_executor::PrimaryWorkHandoff::NoBinding
        ) {
            return Err(invalid_resume(
                "this checkpoint requires its exact primary Work authority",
            ));
        }
        let pool = self
            .shared_pool
            .as_ref()
            .ok_or_else(|| invalid_resume("durable execution requires a database"))?;
        let coordinator = Arc::new(astra_services::DatabaseSessionContextCoordinator::new(
            pool.clone(),
        ));
        let snapshot = coordinator
            .load_admission_snapshot(&parked.reservation.key)
            .await
            .map_err(|error| {
                invalid_resume(format!("canonical authority is unavailable: {error}"))
            })?;
        let owner = self
            .run_engine
            .execution_owner_pod_id()
            .ok_or_else(|| invalid_resume("execution owner identity is unavailable"))?;
        let actor = astra_turn_types::ActorContextV1::owner_user(
            &durable.user_id,
            format!("server-run:{}:resume", durable.run_id),
            astra_turn_types::ActorKindV1::Server,
            astra_turn_types::SessionSurfaceV1::Server,
            None,
            snapshot.authority_epochs,
        );
        let bytes = checkpoint.checkpoint_json.len() as u64;
        let work = astra_services::AdmissionWork {
            resident_bytes: bytes.saturating_mul(2),
            context_tokens: bytes.saturating_add(3) / 4,
            provider_slots: 1,
            cpu_units: bytes,
            io_bytes: bytes,
        };
        let weighted = self
            .weighted_admission
            .try_admit(&durable.user_id, work)
            .map_err(|error| {
                error_response_coded(
                    StatusCode::TOO_MANY_REQUESTS,
                    format!("execution capacity is unavailable: {error}"),
                    "weighted_session_admission_rejected",
                )
            })?;
        let distributed = self
            .distributed_weighted_admission
            .as_ref()
            .ok_or_else(|| invalid_resume("distributed execution capacity is unavailable"))?;
        let capacity = distributed
            .try_reserve(
                &parked.reservation.key,
                work,
                Duration::from_secs(15 * 60),
                &format!("server-resume:{}:{}", durable.run_id, Uuid::new_v4()),
            )
            .await
            .map_err(|error| {
                invalid_resume(format!(
                    "distributed execution capacity is unavailable: {error}"
                ))
            })?;
        let (mut tracked, cancel, pause, token, lease_lost) = Self::build_tracked_run_state(
            durable.run_id.clone(),
            durable.session_id.clone(),
            durable.user_id.clone(),
        );
        // No synthetic run_started or fresh user input is introduced by resume.
        tracked.events = durable.events.clone();
        {
            let mut runs = self.runs.write().await;
            if runs
                .get(&durable.run_id)
                .is_some_and(|run| run.execution_live)
            {
                return Err(invalid_resume("execution is already being resumed"));
            }
            runs.insert(durable.run_id.clone(), tracked);
        }
        let resume_request =
            || astra_services::session_context_coordinator::ResumedExecutionTurnRequest {
                user_id: &durable.user_id,
                session_id: &durable.session_id,
                run_id: &durable.run_id,
                expected_owner_generation: durable.run_generation,
                checkpoint_id: &checkpoint.checkpoint_id,
                source: &parked.reservation,
                actor: &actor,
                owner_pod_id: owner,
                ttl: Duration::from_secs(15 * 60),
            };
        let mut acquired = coordinator.resume_execution_turn(resume_request()).await;
        if matches!(
            acquired,
            Err(astra_services::SessionContextCoordinatorError::Database {
                operation: "commit_execution_resume",
                ..
            })
        ) {
            // Resolve one unknown acknowledgement with the exact existing
            // receipt identity. Never issue a new generation/request to retry.
            acquired = coordinator.resume_execution_turn(resume_request()).await;
        }
        let proof = match acquired {
            Ok(proof) => proof,
            Err(error) => {
                let mut runs = self.runs.write().await;
                if runs
                    .get(&durable.run_id)
                    .is_some_and(|run| Arc::ptr_eq(&run.cancel_flag, &cancel))
                {
                    runs.remove(&durable.run_id);
                }
                return Err(resume_custody_error(error));
            }
        };
        // The immutable proof is the only source of the recovered state. The
        // preflight checkpoint is no longer used after committed custody.
        drop(parked);
        let generation = proof.run().run_generation;
        let mut canonical = CanonicalTurnAdmission {
            coordinator,
            inference_pool: pool.clone(),
            lease: proof.receipt().writer_lease.clone(),
            reservation: proof.receipt().turn_reservation.clone(),
            prior_messages: Vec::new(),
            had_canonical_head: snapshot.head.is_some(),
            release_writer_on_finish: true,
            release_started: Arc::new(AtomicBool::new(false)),
            renewal_cancel: CancellationToken::new(),
            _weighted_permit: weighted,
            distributed_permit: capacity,
        };
        let mut heartbeat = None;
        let preparation = async {
            self.start_canonical_turn_renewal(&canonical, (*token).clone())?;
            let confirmed = self
                .run_engine
                .confirm_execution_authority(
                    &durable.user_id,
                    &durable.session_id,
                    &durable.run_id,
                    generation,
                    &token,
                )
                .await
                .map_err(|error| {
                    invalid_resume(format!("execution authority is unavailable: {error}"))
                })?;
            let ExecutionAuthorityConfirmation::Confirmed(confirmed) = confirmed else {
                lease_lost.store(true, Ordering::Release);
                return Err(invalid_resume("execution authority was superseded"));
            };
            heartbeat = self.run_engine.start_owner_lease_heartbeat(
                durable.user_id.clone(),
                durable.session_id.clone(),
                durable.run_id.clone(),
                generation,
                confirmed,
                lease_lost.clone(),
                token.clone(),
            );
            if let Some(head) = snapshot.head.as_ref() {
                if Some(&head.cursor) != canonical.reservation.expected_cursor.as_ref() {
                    return Err(invalid_resume("canonical base changed before recovery"));
                }
                canonical.prior_messages = canonical
                    .coordinator
                    .materialize(head)
                    .await
                    .map_err(|error| invalid_resume(error.to_string()))?
                    .messages;
            }
            self.prepare_resumed_execution(&proof, &canonical, &cancel, &pause, &token, &lease_lost)
                .await
        }
        .await;
        let mut execution = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut cleanup_error = None;
                if !lease_lost.load(Ordering::Acquire) {
                    // User cancellation has its own durable control owner.
                    // A cancelled preparation token is not evidence of it.
                    let unknown = |storage: String| {
                        error_response_coded(
                            StatusCode::SERVICE_UNAVAILABLE,
                            format!(
                                "execution recovery preparation failed; checkpoint custody is unconfirmed: {storage}"
                            ),
                            "execution_resume_outcome_unknown",
                        )
                    };
                    cleanup_error = match self.run_engine.park_resumed_execution(&proof).await {
                        Ok(Some(_)) => None,
                        Ok(None) => {
                            Some(unknown("checkpoint parking was not confirmed".to_string()))
                        }
                        Err(storage) => Some(unknown(storage)),
                    };
                }
                let mut runs = self.runs.write().await;
                if runs
                    .get(&durable.run_id)
                    .is_some_and(|run| Arc::ptr_eq(&run.cancel_flag, &cancel))
                {
                    runs.remove(&durable.run_id);
                }
                return Err(cleanup_error.unwrap_or(error));
            }
        };
        execution.owner_lease_heartbeat = heartbeat;
        execution.canonical_turn = Some(canonical);
        self.launch_owned_background_execution(execution).await;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_resumed_execution(
        &self,
        proof: &astra_services::session_context_coordinator::ResumedExecutionTurn,
        canonical: &CanonicalTurnAdmission,
        cancel: &Arc<AtomicBool>,
        pause: &Arc<AtomicBool>,
        token: &Arc<CancellationToken>,
        lease_lost: &Arc<AtomicBool>,
    ) -> Result<OwnedBackgroundExecution, (StatusCode, Json<ErrorResponse>)> {
        let run = proof.run();
        let principal = self
            .auth_service
            .reauthorize_execution_handoff(proof)
            .await?;
        let admission = run
            .original_admission_data()
            .map_err(|_| invalid_resume("invalid original admission"))?;
        let selection: ModelSelection = admission_field(admission, "model_selection")?;
        let resolved: ResolvedModelSelection =
            admission_field(admission, "resolved_model_selection")?;
        let execution = self
            .model_service
            .admit_model_offering(run.user_id.clone(), selection.offering_id.clone())
            .await?;
        if run.model_offering_id.as_deref() != Some(selection.offering_id.as_str())
            || run.resolved_model_name.as_deref() != Some(execution.model_name.as_str())
            || resolved.offering_id != execution.offering_id
            || resolved.model_name != execution.model_name
            || resolved.source_identity != execution.source_identity
        {
            return Err(invalid_resume(
                "the original model identity is no longer available",
            ));
        }
        let catalog = Some(
            astra_services::models::AuthorizedModelCatalogReader::with_cache(
                self.model_service.clone(),
                self.auth_service.clone(),
                principal,
                self.model_catalog_cache.clone(),
            ),
        );
        let delegation_authority =
            crate::server::run::engine::durable_run_delegation_authority(run, &run.user_id)
                .map_err(invalid_resume)?;
        let interaction_mode =
            crate::server::run::engine::durable_run_effective_interaction_mode(run)
                .map_err(invalid_resume)?;
        let interactive: bool = admission_field(admission, "interactive_client")?;
        let controls = crate::server::run::engine::durable_run_generation_controls(run)
            .map_err(invalid_resume)?;
        let handoff = server_loop_host::RuntimeExecutionHandoff::from_adopted(proof)
            .map_err(|error| invalid_resume(error.message))?;
        let constraints = RequestConstraints::from_durable_run(
            run,
            handoff.original_facts.delegated_model_requirements.clone(),
        )
        .map_err(invalid_resume)?;
        let (bindings, _) =
            crate::server::run::binding_resolution::durable_run_execution_contract(run)
                .map_err(invalid_resume)?;
        let original_workspace = &bindings.workspace;
        let workspace = if original_workspace.kind == WorkspaceBindingKind::ServerSandbox {
            let store = self
                .workspace_record_store
                .as_ref()
                .ok_or_else(|| invalid_resume("the original workspace owner is unavailable"))?;
            let entry = store
                .load_workspace_record(&run.user_id, &run.session_id)
                .await
                .map_err(workspace_record_store_error)?
                .ok_or_else(|| invalid_resume("the original workspace record is missing"))?;
            let root = self
                .server_workspace_provider()?
                .resolve_existing(&entry.record)
                .map_err(server_workspace_provision_error)?;
            if original_workspace.cwd.as_deref() != root.to_str() {
                return Err(invalid_resume("the original workspace identity changed"));
            }
            root
        } else if original_workspace.kind == WorkspaceBindingKind::None {
            // Internal scratch does not acquire user-visible workspace authority.
            self.provision_server_workspace(&run.session_id)?
        } else {
            return Err(invalid_resume(
                "the original workspace needs its selected execution provider",
            ));
        };
        if bindings.executor.kind != ExecutorBindingKind::ServerLocal
            || bindings.executor.transport != ToolTransportKind::ServerLocal
        {
            return Err(invalid_resume(
                "the original executor requires a live registered connection",
            ));
        }
        let mut inherited =
            InheritedPermissions::new(if interaction_mode == RequestedTurnInteractionMode::Deny {
                PermissionMode::Deny
            } else {
                PermissionMode::Auto
            });
        inherited.allowed_tools = constraints.allowed_tools.clone();
        inherited.read_only_execution =
            bindings.workspace.authority == astra_runtime_env::WorkspaceAuthority::ReadOnly;
        let mut permissions = PermissionSyncContext::new(inherited);
        let hook_root =
            (bindings.workspace.kind != WorkspaceBindingKind::None).then(|| workspace.clone());
        let (tool_hooks, session_hooks) = hook_root
            .as_ref()
            .map(|root| crate::skills::hooks::load_all_hooks(root))
            .unwrap_or_default();
        let facts = LoopExecutionFacts::from_handoff(
            proof,
            StopHookState {
                workspace_root_hint: hook_root.map(|root| root.to_string_lossy().into_owned()),
                admitted_model_execution: Some(execution.clone()),
                ..Default::default()
            },
            tool_hooks,
            session_hooks,
            &mut permissions,
        )
        .map_err(|error| invalid_resume(error.message))?;
        let authorization = LoopEnvironmentAuthorization {
            agent_id: run.agent_id.clone().unwrap_or_else(|| "root-agent".into()),
            model_name: Some(execution.model_name.clone()),
            model_execution: Some(execution.clone()),
            model_catalog_reader: catalog.clone(),
            permissions,
            workspace_record: None,
            runtime_process_authorization: None,
            runtime_edge_dispatch_authorization: None,
            forward_headers: HashMap::new(),
            thinking: controls.thinking.clone(),
            interaction_mode,
        };
        let runtime = self
            .prepare_authorized_runtime_capabilities(
                &run.user_id,
                &[],
                &[],
                None,
                None,
                None,
                &constraints,
            )
            .await?;
        self.validate_optional_tool_availability(&run.user_id, &constraints, Some(&bindings))
            .await?;
        let now_unix_ms = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| invalid_resume("execution clock is unavailable"))?
            .as_millis() as u64;
        let deadline = handoff
            .execution_deadline
            .map(|snapshot| {
                astra_services::runs::ExecutionDeadlineAuthority::from_snapshot_at(
                    snapshot,
                    now_unix_ms,
                )
            })
            .transpose()
            .map_err(invalid_resume)?;
        let mut host = self.build_authorized_host(
            &run.user_id,
            &run.session_id,
            &run.run_id,
            &authorization,
            &controls,
            false,
            deadline,
            false,
            admission_field(admission, "turn_intent_policy")?,
            admission_field(admission, "skill_auto_route_policy")?,
            interactive,
            handoff.edge_provider_tool_schemas.clone(),
            handoff.edge_profile.clone(),
            true,
            true,
            Some(&bindings),
            None,
            false,
            None,
        );
        host.restore_handoff_contracts(&handoff);
        let interaction_sink: Arc<dyn server_loop_host::HostInteractionSink> =
            Arc::new(DurableHostInteractionSink {
                run_engine: self.run_engine.clone(),
                user_id: run.user_id.clone(),
                run_id: run.run_id.clone(),
                session_id: run.session_id.clone(),
                agent_id: run.agent_id.clone(),
                event_tx: None,
            });
        host.set_interaction_sink(interaction_sink.clone());
        // Preserve the restored permission owner for descendants; the assembler
        // takes ownership of the root context without recreating session decisions.
        let child_permissions = authorization.permissions.for_child(false);
        let environment = self.assemble_loop_environment(
            &run.user_id,
            authorization,
            &run.session_id,
            &run.run_id,
            Some(&bindings),
            Some(token.clone()),
            Some(lease_lost.clone()),
            Some(interaction_sink),
            &constraints,
            &EdgeContext::default(),
            Some(&handoff.edge_profile),
            &runtime,
            Some(run.run_generation),
            &facts.hooks,
        );
        let mut state = Self::assemble_loop_state(environment, facts);
        state.skills.client_pipeline_skill_names = handoff
            .client_pipeline_skill_names
            .iter()
            .cloned()
            .collect();
        bind_execution_owner_generation(&mut state, run.run_generation);
        if let Some(cursor) = canonical.reservation.expected_cursor.as_ref() {
            state.initialize_canonical_rewrite_proof(
                &canonical.prior_messages,
                &cursor.canonical_root_hash,
                cursor.compaction_generation,
            );
        }
        host.restore_handoff_wal(&mut state, &handoff, &canonical.prior_messages)
            .await
            .map_err(|error| invalid_resume(error.message))?;
        host.bind_execution_handoff(
            self.execution_handoff_requested.clone(),
            self.run_engine.clone(),
            canonical.reservation.clone(),
            handoff.original_user_message.clone(),
        );
        self.configure_host_approval_audit_context(
            &mut host,
            &run.user_id,
            &run.session_id,
            &run.run_id,
            state.session_turn,
        );
        let entry = self
            .server_agent_spawner_for_session(&run.user_id, &run.session_id)
            .await;
        self.configure_loop_state_runtime_controls(
            &mut state,
            cancel,
            pause,
            (**token).clone(),
            lease_lost.clone(),
        );
        configure_runtime_controllers(
            &self.matrixone,
            self.shared_pool.as_ref(),
            &mut state,
            &run.user_id,
            &run.session_id,
            self.trace_ingestion.clone(),
        )
        .await;
        let mut executor = self.build_root_runtime_tool_executor(
            workspace.clone(),
            &run.user_id,
            &run.session_id,
            &run.run_id,
            catalog.clone(),
            None,
            None,
            deadline,
            &state,
            &host,
            &runtime,
            None,
        );
        executor.set_execution_binding_snapshot(bindings.clone());
        host.set_execution_metadata(executor.binding_metadata());
        let _fanout = entry.spawner.fanout_parent(&run.run_id);
        let restored = self
            .restore_server_dynamic_agents(&entry, &run.user_id, &run.session_id)
            .await;
        restored
            .as_ref()
            .map_err(|error| invalid_resume(error.clone()))?;
        let runtime_context = ServerSpawnRuntimeContext {
            model_catalog_reader: catalog,
            parent_run_id: run.run_id.clone(),
            runtime_context_id: Uuid::new_v4().to_string(),
            publication_capability: entry.executor.publication_capability_for_run(&run.run_id),
            cancellation_binding_id: None,
            user_id: run.user_id.clone(),
            session_id: run.session_id.clone(),
            trace_context: server_trace_context(
                &run.user_id,
                &run.session_id,
                &run.run_id,
                state.session_turn,
            ),
            forward_headers: HashMap::new(),
            admitted_model_execution: Some(execution),
            interaction_mode,
            edge_tools: Arc::new(handoff.edge_provider_tool_schemas.clone()),
            request_constraints: constraints.clone(),
            execution_contract: Some((
                bindings.clone(),
                astra_turn_types::StopHookObligations {
                    declarations: state.hooks.declarations.clone(),
                    phase: state.hooks.phase,
                },
            )),
            execution_metadata: None,
            provider_run_owner: None,
            spawner: Arc::downgrade(&entry.spawner),
            pause_flag: Some(pause.clone()),
            cancel_token: Some(token.clone()),
            execution_owner_generation: Arc::new(ExecutionOwnerGenerationSink::preparing(
                run.run_generation,
            )),
            #[cfg(feature = "harness")]
            harness_sink: state.harness.sink.clone(),
        };
        runtime_context
            .execution_owner_generation
            .publish(run.run_generation);
        let agent_context = AgentToolContext {
            parent_delegation_authority: delegation_authority,
            fanout_admission: entry.spawner.fanout_parent(&run.run_id),
            reply_obligations: state.messaging.reply_obligations.clone(),
            delegation_model_admission: None,
            run_id: run.run_id.clone(),
            agent_id: state.self_agent_id.clone(),
            delegation_chain: Vec::new(),
            current_model: run.resolved_model_name.clone(),
            current_model_selection: Some(selection.clone()),
            parent_model_reasoning: Some(
                astra_turn_core::orchestration_spawn_tool::ParentModelReasoning {
                    selection,
                    resolved_model_name: run.resolved_model_name.clone(),
                    thinking: controls.thinking,
                },
            ),
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: workspace.clone(),
            spawner: entry.spawner.clone(),
            inherited_permissions: child_permissions,
            enabled_tools: constraints.enabled_tools,
            active_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: Some(server_trace_context(
                &run.user_id,
                &run.session_id,
                &run.run_id,
                state.session_turn,
            )),
            execution_metadata: None,
            execution_deadline: deadline,
            workspace_mutation: crate::orchestration::WorkspaceMutationAuthority::default(),
            transcript_location: AgentTranscriptLocation::DurableServer,
        };
        let wiring = self
            .wire_authorized_server_dynamic_agent_tools(
                &entry,
                restored,
                &mut executor,
                runtime_context,
                agent_context,
                None,
                None,
            )
            .await
            .map_err(invalid_resume)?;
        state.attach_active_work_registry(wiring.active_work_registry);
        self.install_background_interaction_gates(
            &mut executor,
            &run.user_id,
            &run.session_id,
            &run.run_id,
            state.session_turn,
            token.clone(),
            interactive,
            None,
        )
        .await;
        wire_executor_into_state(executor, &mut state);
        Ok(OwnedBackgroundExecution {
            resumed: true,
            user_id: run.user_id.clone(),
            session_id: run.session_id.clone(),
            run_id: run.run_id.clone(),
            explain: admission
                .get("explain_analyze_requested")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            agent_id: run.agent_id.clone(),
            model_name: run.resolved_model_name.clone(),
            user_message: handoff.original_user_message.clone(),
            #[cfg(feature = "e2e-hooks")]
            test_post_loop_settlement_delay_ms: 0,
            host,
            loop_state: state,
            execution_owner_generation: run.run_generation,
            owner_lease_heartbeat: None,
            canonical_turn: None,
            root_runtime_context_guard: Some(wiring.root_runtime_context_guard),
            work_runtime_binding: None,
            tool_runtime_workspace: Some(workspace),
            cloud_workspace_record: None,
            csl_manager: None,
            cancel_flag: cancel.clone(),
            pause_flag: pause.clone(),
            llm_cancel_token: token.clone(),
            execution_lease_lost: lease_lost.clone(),
            descendant_spawner: entry.spawner.clone(),
            delivery: None,
        })
    }
}
