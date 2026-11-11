use super::*;
use crate::server::run::engine::RunEngine;
use crate::turn::agentic_loop::settings_boundary::{await_settings_control, settings_control_stop};
use astra_services::{AdmittedModelExecution, ModelExecutionPlacement, ModelService};
use astra_turn_types::model_routing::{AutoModelRoutingPolicy, ModelRoutingReason};

use astra_services::model_routing::{DECISION_KEY, EVENT_TYPE, ModelRoutingDecision};

pub(super) struct AutoRoutingContext {
    policy: AutoModelRoutingPolicy,
    service: Arc<dyn ModelService>,
    engine: RunEngine,
    supported_input: bool,
    decision: Option<ModelRoutingDecision>,
    // Immutable-decision absence is reusable only within this exact owned Run.
    // Publishing still checks the canonical owner fence before primary I/O.
    missing_decision_for: Option<(String, u64)>,
    applied: bool,
}

fn invalid(message: impl Into<String>) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(astra_core::ErrorKind::ContractViolation, message)
}

fn preserves_contract(
    baseline: &AdmittedModelExecution,
    candidate: &AdmittedModelExecution,
) -> bool {
    candidate.execution_placement == ModelExecutionPlacement::Server
        && baseline.execution_placement == candidate.execution_placement
        && baseline.access_kind == candidate.access_kind
        && baseline.provider == candidate.provider
        && baseline
            .inference_protocol
            .unwrap_or_else(|| astra_core::model_wire::llm_provider_protocol(&baseline.provider))
            == candidate.inference_protocol.unwrap_or_else(|| {
                astra_core::model_wire::llm_provider_protocol(&candidate.provider)
            })
        && baseline.thinking_capability == candidate.thinking_capability
        && baseline.thinking_protocol == candidate.thinking_protocol
        && baseline.supported_reasoning_efforts == candidate.supported_reasoning_efforts
        && baseline.fixed_temperature == candidate.fixed_temperature
        && baseline.request_body_overrides == candidate.request_body_overrides
        && baseline.cache_capability == candidate.cache_capability
        && matches!((baseline.context_window, candidate.context_window), (Some(a), Some(b)) if a > 0 && b >= a)
        && matches!((baseline.max_completion_tokens, candidate.max_completion_tokens), (Some(a), Some(b)) if a > 0 && b >= a)
}

impl ServerAgenticLoopHost {
    fn check_model_control(
        &self,
        state: &AgenticLoopState,
    ) -> Result<(), astra_core::ClassifiedError> {
        let remaining = self
            .execution_time_budget_remaining()
            .map(|budget| budget.total_remaining);
        match settings_control_stop(&state.cancellation, remaining) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Install one admitted model in the existing execution owner. Credentials
    /// stay in memory; all derived policy follows the same selected model.
    fn install_model_execution(
        &mut self,
        state: &mut AgenticLoopState,
        execution: AdmittedModelExecution,
        source: &str,
    ) {
        self.model_override = Some(execution.model_name.clone());
        state.policy_model_name = Some(execution.model_name.clone());
        state.hooks.admitted_model_execution = Some(execution.clone());
        let tool_policy = state
            .admitted_tool_policy
            .resolve_for_model(Some(&execution.model_name));
        state.max_identical_tool_calls = tool_policy.max_identical_tool_calls;
        state.max_tools_per_turn = tool_policy.max_tools_per_turn;
        state.max_consecutive_empty_name = tool_policy.max_consecutive_empty_name;
        if let Some(manifest) = state.runtime_manifest.as_mut() {
            manifest["model_selection"] = json!({"offering_id": execution.offering_id});
            manifest["selected_model"] = json!({"model": execution.model_name});
            manifest["model_resolution"] = json!({"source":source, "offering_id":execution.offering_id, "model":execution.model_name, "resolved":true});
        }
        self.admitted_model_execution = Some(execution);
        self.clear_resolved_llm_config();
    }

    fn publish_model_execution_to_tools(&self, state: &AgenticLoopState) {
        if let (Some(executor), Some(execution)) = (
            state.runtime_tool_executor.as_ref(),
            self.admitted_model_execution.as_ref(),
        ) {
            executor.set_agent_model_execution(execution, &state.thinking);
        }
    }

    async fn admit_explicit_model(
        &self,
        selection: &astra_turn_types::RunModelSelection,
    ) -> Result<AdmittedModelExecution, (astra_core::ErrorKind, &'static str)> {
        use astra_core::ErrorKind;
        let execution = if let Some(current) =
            self.admitted_model_execution.as_ref().filter(|current| {
                current.execution_placement == ModelExecutionPlacement::Edge
                    && current.offering_id == selection.model.offering_id
            }) {
            // An authenticated Edge descriptor grants only this exact Offering.
            current.clone()
        } else if let Some(service) = &self.model_service {
            crate::server::model_execution_admission::admit_model_execution(
                service,
                astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
                &self.user_id,
                &selection.model,
                None,
                None,
                None,
            )
            .await
            .map_err(|(status, _)| match status.as_u16() {
                401 => (ErrorKind::Auth, "model_authentication_failed"),
                403 => (ErrorKind::ProviderForbidden, "model_access_denied"),
                404 => (ErrorKind::InvalidRequest, "model_not_found"),
                429 => (ErrorKind::RateLimit, "model_rate_limited"),
                _ => (ErrorKind::ServerError, "model_admission_unavailable"),
            })?
        } else {
            astra_services::revalidate_admitted_model_execution(
                &self.matrixone,
                self.encryptor.as_ref(),
                &self.user_id,
                &selection.model.offering_id,
                self.shared_pool.as_ref().map(SharedPool::get),
            )
            .await
            .map_err(|_| (ErrorKind::ServerError, "model_admission_unavailable"))?
        };
        astra_services::models::validate_model_execution_purpose(
            &execution,
            astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
        )
        .map_err(|_| (ErrorKind::InvalidRequest, "model_purpose_unsupported"))?;
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::from(&selection.reasoning);
        crate::server::model_execution_admission::validate_reasoning_control(&execution, &thinking)
            .map_err(|_| (ErrorKind::InvalidRequest, "model_reasoning_unsupported"))?;
        Ok(execution)
    }

    async fn admit_explicit_model_at_boundary(
        &self,
        state: &AgenticLoopState,
        selection: &astra_turn_types::RunModelSelection,
    ) -> Result<
        Result<AdmittedModelExecution, (astra_core::ErrorKind, &'static str)>,
        astra_core::ClassifiedError,
    > {
        let remaining = self
            .execution_time_budget_remaining()
            .map(|budget| budget.total_remaining);
        await_settings_control(
            self.admit_explicit_model(selection),
            &state.cancellation,
            remaining,
        )
        .await
    }

    fn install_explicit_model(
        &mut self,
        state: &mut AgenticLoopState,
        execution: AdmittedModelExecution,
        reasoning: &astra_turn_types::ReasoningRequirement,
    ) {
        state.thinking = astra_turn_core::thinking_config::ThinkingConfig::from(reasoning);
        self.preserve_thinking =
            state.thinking != astra_turn_core::thinking_config::ThinkingConfig::ModelDefault;
        state.sticky_tool_schemas.clear();
        self.model_routing = None;
        self.install_model_execution(state, execution, "user");
    }

    async fn ensure_applied_model(
        &mut self,
        state: &mut AgenticLoopState,
        applied: &astra_services::runs::RunModelApplied,
    ) -> Result<(), astra_core::ClassifiedError> {
        let thinking = astra_turn_core::thinking_config::ThinkingConfig::from(&applied.reasoning);
        let already_installed = self
            .admitted_model_execution
            .as_ref()
            .is_some_and(|current| {
                current.offering_id == applied.resolved.offering_id
                    && current.model_name == applied.resolved.model_name
                    && current.source_identity == applied.resolved.source_identity
                    && state.thinking == thinking
            });
        if already_installed {
            self.check_model_control(state)?;
            self.preserve_thinking =
                thinking != astra_turn_core::thinking_config::ThinkingConfig::ModelDefault;
            self.model_routing = None;
            self.publish_model_execution_to_tools(state);
            return Ok(());
        }
        let mut selection = applied.selection.clone();
        selection.reasoning = applied.reasoning.clone();
        let execution = self
            .admit_explicit_model_at_boundary(state, &selection)
            .await?
            .map_err(|_| {
                invalid(
                    "The selected model is no longer available; select another model to continue",
                )
            })?;
        if execution.offering_id != applied.resolved.offering_id
            || execution.model_name != applied.resolved.model_name
            || execution.source_identity != applied.resolved.source_identity
        {
            return Err(invalid(
                "The selected model identity changed; select the updated model to continue",
            ));
        }
        self.check_model_control(state)?;
        self.install_explicit_model(state, execution, &applied.reasoning);
        self.publish_model_execution_to_tools(state);
        Ok(())
    }

    pub(super) async fn apply_explicit_model_settings(
        &mut self,
        state: &mut AgenticLoopState,
        mut settings: astra_services::runs::RunModelSettingsSnapshot,
    ) -> Result<(), astra_core::ClassifiedError> {
        use astra_turn_types::{RunModelOutcome, RunModelOutcomeKind};
        // A newer choice may arrive while the current one is being admitted.
        // Consume the latest observed choice at this boundary before starting
        // provider I/O. Bound retries under continuous user changes.
        let mut speculative_install = false;
        for _ in 0..8 {
            let pending = settings
                .requested
                .as_ref()
                .filter(|selection| {
                    settings
                        .outcome
                        .as_ref()
                        .is_none_or(|outcome| &outcome.selection != *selection)
                })
                .cloned();
            let Some(selection) = pending else {
                if let Some(applied) = settings.applied.as_ref() {
                    self.ensure_applied_model(state, applied).await?;
                    speculative_install = false;
                }
                if speculative_install {
                    return Err(invalid(
                        "The model change could not be confirmed; select a model to continue",
                    ));
                }
                self.check_model_control(state)?;
                return Ok(());
            };
            let (Some(control), Some(user_id), Some(session_id), Some(run_id), Some(generation)) = (
                state.run_control.clone(),
                state.owner_user_id.clone(),
                state.current_session_id.clone(),
                state.current_run_id.clone(),
                state.current_run_owner_generation,
            ) else {
                return Err(invalid("Model change has no active execution owner"));
            };
            if user_id != self.user_id || session_id != self.session_id {
                return Err(invalid("Model change does not belong to this execution"));
            }
            let admission = self
                .admit_explicit_model_at_boundary(state, &selection)
                .await?
                .and_then(|execution| {
                    if state.current_round_index == 0
                        && let Some(limit) = self.initial_output_limit
                    {
                        astra_turn_core::thinking_config::ThinkingConfig::from(
                            &selection.reasoning,
                        )
                        .validate_output_budget(u64::from(limit))
                        .map_err(|_| {
                            (
                                astra_core::ErrorKind::InvalidRequest,
                                "model_reasoning_unsupported",
                            )
                        })?;
                    }
                    let route = self.model_replay_route_for(&execution);
                    let scope = crate::turn::llm::model_origin::ReplayScope {
                        encryptor: &self.encryptor,
                        user_id: &user_id,
                        session_id: &session_id,
                        allowed_session_ids: self.private_reasoning_origins.as_ref(),
                        run_id: &run_id,
                        selection_revision: Some(selection.revision),
                        route: &route,
                    };
                    crate::turn::llm::model_origin::project(&scope, &state.messages).map_err(
                        |_| {
                            (
                                astra_core::ErrorKind::InvalidRequest,
                                "model_history_incomplete",
                            )
                        },
                    )?;
                    Ok(execution)
                });
            self.check_model_control(state)?;
            // Install in the execution owner before publishing an Applied
            // outcome. The child-facing tool context remains unchanged until
            // the durable decision is confirmed below.
            if let Ok(execution) = &admission {
                self.install_explicit_model(state, execution.clone(), &selection.reasoning);
                speculative_install = true;
            }
            let (result, resolved) = match &admission {
                Ok(execution) => (
                    RunModelOutcomeKind::Applied {
                        model: selection.model.clone(),
                        reasoning: selection.reasoning.clone(),
                    },
                    Some(astra_services::runs::ResolvedModelSelection {
                        offering_id: execution.offering_id.clone(),
                        model_name: execution.model_name.clone(),
                        source_identity: execution.source_identity.clone(),
                    }),
                ),
                Err((kind, code)) => (
                    RunModelOutcomeKind::Rejected {
                        kind: *kind,
                        error_code: Some((*code).into()),
                    },
                    None,
                ),
            };
            let outcome = RunModelOutcome {
                selection: selection.clone(),
                owner_generation: generation,
                round_index: state.current_round_index,
                result,
            };
            let remaining = self
                .execution_time_budget_remaining()
                .map(|budget| budget.total_remaining);
            let committed = await_settings_control(
                control.finish_run_model(
                    &user_id,
                    &session_id,
                    &run_id,
                    &outcome,
                    resolved.as_ref(),
                ),
                &state.cancellation,
                remaining,
            )
            .await;
            state.run_settings_observation.invalidate();
            let committed = committed?;
            if matches!(committed, Ok(None)) {
                return Err(invalid("The model change lost its execution owner"));
            }
            let remaining = self
                .execution_time_budget_remaining()
                .map(|budget| budget.total_remaining);
            let fresh = await_settings_control(
                state.run_settings_observation.read(
                    control.as_ref(),
                    &user_id,
                    &session_id,
                    &run_id,
                ),
                &state.cancellation,
                remaining,
            )
            .await;
            if fresh.is_err() {
                state.run_settings_observation.invalidate();
            }
            let fresh = fresh?
                .map_err(|_| invalid("The model change could not be confirmed; continue this task to reconcile it"))?
                .ok_or_else(|| invalid("The model change no longer belongs to this task"))?;
            self.check_model_control(state)?;
            let newer_pending = fresh.model.requested.as_ref().is_some_and(|requested| {
                fresh
                    .model
                    .outcome
                    .as_ref()
                    .is_none_or(|outcome| &outcome.selection != requested)
            });
            let decided = match committed {
                Ok(Some(decided)) => decided,
                Ok(None) => return Err(invalid("The model change lost its execution owner")),
                Err(_) => match fresh
                    .model
                    .outcome
                    .clone()
                    .filter(|outcome| outcome.selection == selection)
                {
                    Some(outcome) => outcome,
                    None if newer_pending => {
                        // Keep any speculative installation private while the
                        // newer choice reaches the same durable fence.
                        settings = fresh.model;
                        continue;
                    }
                    None => {
                        return Err(invalid(
                            "The model change could not be confirmed; continue this task to reconcile it",
                        ));
                    }
                },
            };
            if matches!(decided.result, RunModelOutcomeKind::Applied { .. }) {
                let confirmed = match (&admission, fresh.model.applied.as_ref()) {
                    (Ok(execution), Some(applied)) => {
                        applied.selection == selection
                            && execution.offering_id == applied.resolved.offering_id
                            && execution.model_name == applied.resolved.model_name
                            && execution.source_identity == applied.resolved.source_identity
                    }
                    _ => false,
                };
                if !confirmed {
                    return Err(invalid("The applied model could not be confirmed"));
                }
                speculative_install = false;
            }
            if newer_pending {
                self.emit_committed_lifecycle_projection(
                    json!({"type":"run_model_outcome", "run_id":run_id,
                "session_id":session_id, "outcome":decided}),
                )
                .await;
                settings = fresh.model;
                continue;
            }
            if let Some(applied) = fresh.model.applied.as_ref() {
                self.ensure_applied_model(state, applied).await?;
                speculative_install = false;
            }
            if speculative_install {
                return Err(invalid(
                    "The model change could not be confirmed; select a model to continue",
                ));
            }
            self.emit_committed_lifecycle_projection(
                json!({"type":"run_model_outcome", "run_id":run_id,
            "session_id":session_id, "outcome":decided}),
            )
            .await;
            self.check_model_control(state)?;
            return Ok(());
        }
        Err(invalid(
            "Model selection changed repeatedly; continue the task after choosing a model",
        ))
    }

    pub(crate) fn configure_model_routing(
        &mut self,
        policy: AutoModelRoutingPolicy,
        service: Arc<dyn ModelService>,
        engine: RunEngine,
        supported_input: bool,
    ) {
        self.model_routing = Some(AutoRoutingContext {
            policy,
            service,
            engine,
            supported_input,
            decision: None,
            missing_decision_for: None,
            applied: false,
        });
    }

    /// Read the immutable decision before judging or dispatching a recovered run.
    pub(super) async fn restore_model_routing(
        &mut self,
        state: &AgenticLoopState,
    ) -> Result<bool, astra_core::ClassifiedError> {
        let Some(routing) = self.model_routing.as_mut() else {
            return Ok(false);
        };
        if routing.decision.is_some() {
            return Ok(true);
        }
        let run_id = state
            .current_run_id
            .as_deref()
            .ok_or_else(|| invalid("Auto routing requires a run identity"))?;
        let restore_key = state
            .current_run_owner_generation
            .map(|generation| (run_id.to_owned(), generation));
        if restore_key
            .as_ref()
            .is_some_and(|key| routing.missing_decision_for.as_ref() == Some(key))
        {
            // Consume this preflight result once. A retry after uncertain
            // decision publication must re-read durable storage.
            routing.missing_decision_for = None;
            return Ok(false);
        }
        let Some(event) = routing
            .engine
            .load_run_event_by_idempotency_key(&self.user_id, run_id, EVENT_TYPE, DECISION_KEY)
            .await
            .map_err(invalid)?
        else {
            routing.missing_decision_for = restore_key;
            return Ok(false);
        };
        routing.missing_decision_for = None;
        let decision: ModelRoutingDecision = serde_json::from_value(
            event
                .get("data")
                .cloned()
                .ok_or_else(|| invalid("Auto routing decision has no payload"))?,
        )
        .map_err(|_| invalid("Invalid durable Auto routing decision"))?;
        decision
            .validate_identity(run_id, &self.session_id)
            .map_err(invalid)?;
        let rebound_source = if let Some(saved) = decision.delegation_model_source.as_ref() {
            let current = delegation_intent_source_from_state(state)
                .ok_or_else(|| invalid("Auto child-model requirement lost its user intent"))?;
            // A new run owner has a new generation. Every field identifying
            // the user's instruction must still match before reusing its
            // semantic judgment; a new instruction needs a new judgment.
            if saved.user_id != self.user_id
                || saved.user_id != current.user_id
                || saved.session_id != current.session_id
                || saved.run_id != current.run_id
                || saved.turn_chain_id != current.turn_chain_id
                || saved.applied_intent_id != current.applied_intent_id
                || saved.session_turn != current.session_turn
                || saved.user_intent_digest != current.user_intent_digest
            {
                return Err(invalid(
                    "Auto child-model requirement belongs to a different user intent",
                ));
            }
            Some(current)
        } else {
            None
        };
        let admission = decision.work_admission.clone();
        let skill_revision = decision.work_admission_skill_revision;
        let model_requirement = decision.delegation_model_requirement;
        routing.decision = Some(decision);
        if let Some(admission) = admission {
            self.apply_classified_work_admission(ClassifiedWorkAdmission {
                // Restored routing/control facts are not a new judgment of
                // the current human message and must not replay feedback.
                user_turn_semantics: None,
                decision: project_complete_admission_effect(admission),
                delegation_model_requirement: model_requirement,
                source: rebound_source,
                work_handoff_pending: true,
            });
            self.semantic_judgment_attempted = Some(SemanticJudgmentPurpose::WorkAdmission);
            self.work_admission_skill_revision = skill_revision;
        }
        Ok(true)
    }

    pub(super) async fn prepare_auto_model_selection(
        &mut self,
        state: &mut AgenticLoopState,
    ) -> Result<(), astra_core::ClassifiedError> {
        let Some(routing) = self.model_routing.as_ref() else {
            return Ok(());
        };
        if routing.applied {
            return Ok(());
        }
        self.restore_model_routing(state).await?;
        // Preparation can publish a decision, including an uncertain commit.
        // Never carry an absence snapshot across that write boundary.
        self.model_routing
            .as_mut()
            .expect("Auto context")
            .missing_decision_for = None;
        let routing = self.model_routing.as_ref().expect("Auto context");
        let service = routing.service.clone();
        let engine = routing.engine.clone();
        let policy = routing.policy.clone();
        let supported_input = routing.supported_input;
        let saved = routing.decision.clone();
        let run_id = state
            .current_run_id
            .clone()
            .ok_or_else(|| invalid("Auto routing requires a run identity"))?;
        let generation = state
            .current_run_owner_generation
            .ok_or_else(|| invalid("Auto routing requires execution ownership"))?;

        let (decision, execution) = if let Some(decision) = saved {
            let execution = crate::server::model_execution_admission::admit_model_execution(
                &service,
                astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
                &self.user_id,
                &astra_turn_types::ModelSelection {
                    offering_id: decision.selected_offering_id.clone(),
                },
                None,
                None,
                None,
            )
            .await
            .map_err(|_| invalid("The pinned Auto Offering is no longer available"))?;
            if crate::server::model_execution_admission::model_execution_contract_root(&execution)
                != decision.selected_contract_root
            {
                return Err(invalid(
                    "The pinned Auto model contract changed; start a new turn",
                ));
            }
            // Restored semantic authority must reach the shared loop even when
            // its first primary request/Work operation had not been persisted.
            self.project_work_admission_semantics(state, false);
            (decision, execution)
        } else {
            if state.llm_rounds_completed != 0 {
                return Err(invalid("A resumed Auto run is missing its model decision"));
            }
            // Opted-in model selection consumes semantic admission before any
            // primary request, so it is an explicit boundary even under the
            // default capacity policy. The shared preflight reuses an existing
            // decision/task and continues to honor the disabled policy.
            self.start_work_admission_preflight(state, true, false)
                .await;
            self.resolve_pending_work_admission(Some(state), true).await;
            self.flush_completed_work_admission_phase(state);
            if let Some(error) = self.work_admission_terminal_error() {
                return Err(error);
            }
            let baseline = self
                .admitted_model_execution
                .clone()
                .ok_or_else(|| invalid("Auto baseline is missing"))?;
            if baseline.offering_id != policy.strong_offering_id {
                return Err(invalid("Auto baseline differs from the admitted policy"));
            }
            let context = crate::turn::agentic::turn_intent::context_for_state(state);
            let input_reference = context.source.as_ref().and_then(|source| {
                let source_message = state.messages.get(source.message_index)?;
                if astra_turn_core::prompt_facing::extract_text_content(source_message).as_deref() != Some(source.message_text.as_str()) { return None }
                let prefix = astra_turn_core::prompt_facing::sanitize_canonical_continuation_messages_with_turn_semantics(state.messages[..=source.message_index].to_vec()).ok()?;
                Some(astra_turn_types::FeedbackResponseReference::from_canonical_prefix(prefix))
            });
            let assessment = state
                .turn_intent
                .as_ref()
                .and_then(|intent| intent.assessment);
            let read_only = astra_services::model_routing::routing_read_only_primary(
                self.pending_work_admission
                    .as_ref()
                    .map(|assessment| &assessment.decision),
            );
            let text_only_history = state.messages.iter().all(|message| {
                message
                    .get("content")
                    .is_none_or(|content| content.is_string() || content.is_null())
                    && message.get("reasoning_signature").is_none()
                    && message.get("encrypted_content").is_none()
            });
            let features = astra_turn_types::model_routing::ModelRoutingFeatures::new(
                assessment,
                read_only,
                supported_input && text_only_history && input_reference.is_some(),
            );
            let mut reason =
                astra_turn_core::model_routing::economy_eligibility_from_features(features);
            let mut execution = baseline.clone();
            if reason == ModelRoutingReason::EasyReadOnly {
                // The catalog is owner-scoped; retaining the same access id
                // prevents a policy from silently changing billing ownership.
                let catalog = self.read_authorized_model_catalog().await;
                let same_access = catalog.as_ref().is_some_and(|catalog| {
                    let strong = catalog.iter().find(|item| item.offering_id == policy.strong_offering_id && item.is_active);
                    let economy = catalog.iter().find(|item| item.offering_id == policy.economy_offering_id && item.is_active);
                    matches!((strong, economy), (Some(a), Some(b)) if a.access_id == b.access_id && a.access_kind == b.access_kind && a.execution_placement == b.execution_placement)
                });
                let admitted = if same_access {
                    crate::server::model_execution_admission::admit_model_execution(
                        &service,
                        astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
                        &self.user_id,
                        &astra_turn_types::ModelSelection {
                            offering_id: policy.economy_offering_id.clone(),
                        },
                        None,
                        None,
                        None,
                    )
                    .await
                    .ok()
                } else {
                    None
                };
                let compatible = admitted
                    .as_ref()
                    .is_some_and(|candidate| preserves_contract(&baseline, candidate));
                if compatible {
                    execution = admitted.expect("compatible candidate");
                } else {
                    reason = if admitted.is_none() {
                        ModelRoutingReason::EconomyUnavailable
                    } else {
                        ModelRoutingReason::IncompatibleCandidate
                    };
                }
            }
            let classified = self.pending_work_admission.as_ref();
            let model_requirement =
                classified.and_then(|assessment| assessment.delegation_model_requirement);
            let model_source = classified.and_then(|assessment| assessment.source.clone());
            if model_requirement.is_some() && model_source.is_none() {
                return Err(invalid(
                    "Auto child-model judgment has no authenticated user intent",
                ));
            }
            let decision = ModelRoutingDecision {
                schema_version: 2,
                features: Some(features),
                work_admission: self
                    .pending_work_admission
                    .as_ref()
                    .map(|assessment| assessment.decision.clone()),
                work_admission_skill_revision: self.work_admission_skill_revision,
                delegation_model_requirement: model_requirement,
                delegation_model_source: model_requirement.and(model_source),
                policy_version: astra_turn_core::model_routing::POLICY_VERSION.into(),
                policy,
                run_id: run_id.clone(),
                session_id: self.session_id.clone(),
                selected_offering_id: execution.offering_id.clone(),
                selected_model: execution.model_name.clone(),
                selected_contract_root:
                    crate::server::model_execution_admission::model_execution_contract_root(
                        &execution,
                    ),
                input_reference,
                reason,
                assessment,
            };
            let event = json!({"event_type": EVENT_TYPE, "idempotency_key": DECISION_KEY, "data": decision});
            if !engine
                .append_events_if_current_generation_and_status(
                    &self.user_id,
                    &self.session_id,
                    &run_id,
                    generation,
                    &["running"],
                    &[event],
                )
                .await
                .map_err(invalid)?
            {
                return Err(invalid("Auto model decision lost execution ownership"));
            }
            (decision, execution)
        };

        // Everything model-dependent is repinned before prompt construction or
        // compaction. No endpoint or credential is included in the saved fact.
        self.install_model_execution(state, execution, "auto");
        self.publish_model_execution_to_tools(state);
        if let Some(manifest) = state.runtime_manifest.as_mut() {
            manifest["model_routing"] =
                serde_json::to_value(&decision).map_err(|error| invalid(error.to_string()))?;
        }
        let routing = self.model_routing.as_mut().expect("Auto context");
        routing.decision = Some(decision);
        routing.applied = true;
        Ok(())
    }

    pub(super) async fn revalidate_auto_execution(
        &self,
    ) -> Result<Option<AdmittedModelExecution>, String> {
        let Some(routing) = self.model_routing.as_ref() else {
            return Ok(None);
        };
        let offering_id = routing
            .decision
            .as_ref()
            .map(|decision| &decision.selected_offering_id)
            .unwrap_or(&routing.policy.strong_offering_id);
        let execution = crate::server::model_execution_admission::admit_model_execution(
            &routing.service,
            astra_core::model_wire::purpose::ModelRequestPurpose::Chat,
            &self.user_id,
            &astra_turn_types::ModelSelection {
                offering_id: offering_id.clone(),
            },
            None,
            None,
            None,
        )
        .await
        .map_err(|_| "Auto Offering is no longer authorized or available".to_string())?;
        if let Some(decision) = &routing.decision
            && crate::server::model_execution_admission::model_execution_contract_root(&execution)
                != decision.selected_contract_root
        {
            return Err("Pinned Auto model contract changed".into());
        }
        Ok(Some(execution))
    }
}

#[cfg(test)]
mod tests;
