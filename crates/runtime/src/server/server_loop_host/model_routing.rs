use super::*;
use crate::server::run::engine::RunEngine;
use astra_services::tuning::rollout::*;
use astra_services::{AdmittedModelExecution, ModelExecutionPlacement, ModelService};
use astra_turn_core::model_routing::rollout::{
    deployment_candidate, score_live, validate_pinned_treatment,
};
use astra_turn_types::model_routing::{AutoModelRoutingPolicy, ModelRoutingReason};
use chrono::Utc;

use astra_services::model_routing::{DECISION_KEY, EVENT_TYPE, ModelRoutingDecision};

pub(super) struct AutoRoutingContext {
    policy: AutoModelRoutingPolicy,
    service: Arc<dyn ModelService>,
    engine: RunEngine,
    supported_input: bool,
    decision: Option<ModelRoutingDecision>,
    applied: bool,
    rollout_store: Option<Arc<dyn RouterRolloutStore>>,
}

async fn load_live_rollout(
    store: &dyn RouterRolloutStore,
    owner: &str,
) -> Result<RouterRolloutState, String> {
    tokio::time::timeout(std::time::Duration::from_secs(1), store.load(owner))
        .await
        .map_err(|_| "Router deployment lookup timed out".to_string())?
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
        && baseline.thinking_capability == candidate.thinking_capability
        && baseline.thinking_protocol == candidate.thinking_protocol
        && baseline.fixed_temperature == candidate.fixed_temperature
        && baseline.request_body_overrides == candidate.request_body_overrides
        && baseline.cache_capability == candidate.cache_capability
        && matches!((baseline.context_window, candidate.context_window), (Some(a), Some(b)) if a > 0 && b >= a)
        && matches!((baseline.max_completion_tokens, candidate.max_completion_tokens), (Some(a), Some(b)) if a > 0 && b >= a)
}

impl ServerAgenticLoopHost {
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
            applied: false,
            rollout_store: self.shared_pool.clone().map(|pool| {
                Arc::new(DatabaseRouterRolloutStore(pool)) as Arc<dyn RouterRolloutStore>
            }),
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
        let Some(event) = routing
            .engine
            .load_run_event_by_idempotency_key(&self.user_id, run_id, EVENT_TYPE, DECISION_KEY)
            .await
            .map_err(invalid)?
        else {
            return Ok(false);
        };
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
        if decision.policy_version != astra_turn_core::model_routing::POLICY_VERSION
            && decision.policy_version
                != astra_turn_types::model_routing::LEARNED_CANARY_ROUTING_POLICY_VERSION
        {
            return Err(invalid("Unsupported Auto routing policy version"));
        }
        if let Some(pinned) = &decision.rollout
            && pinned.cohort == RolloutCohort::Treatment
        {
            let store = routing
                .rollout_store
                .as_ref()
                .ok_or_else(|| invalid("Router rollout store unavailable"))?;
            let current = load_live_rollout(store.as_ref(), &self.user_id)
                .await
                .map_err(invalid)?;
            validate_pinned_treatment(&current, pinned, Utc::now()).map_err(invalid)?;
        }
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
        let routing = self.model_routing.as_ref().expect("Auto context");
        let service = routing.service.clone();
        let engine = routing.engine.clone();
        let policy = routing.policy.clone();
        let supported_input = routing.supported_input;
        let saved = routing.decision.clone();
        let rollout_store = routing.rollout_store.clone();
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
            let routing_started = Instant::now();
            let rollout_state = match &rollout_store {
                Some(store) => load_live_rollout(store.as_ref(), &self.user_id)
                    .await
                    .map_err(invalid)?,
                None => RouterRolloutState::default(),
            };
            let mut rollout = score_live(
                &rollout_state,
                &self.user_id,
                &self.session_id,
                &policy,
                features,
                Utc::now(),
            )
            .map_err(invalid)?;
            let mut reason =
                astra_turn_core::model_routing::economy_eligibility_from_features(features);
            let mut execution = baseline.clone();
            let treatment = rollout
                .as_ref()
                .is_some_and(|r| r.cohort == RolloutCohort::Treatment);
            let wants_economy = if treatment {
                rollout
                    .as_ref()
                    .is_some_and(|r| r.proposed_offering_id == policy.economy_offering_id)
            } else {
                reason == ModelRoutingReason::EasyReadOnly
            };
            // Shadow checks the actual admission/contract boundary too, without
            // sending a second provider request or changing the baseline action.
            if wants_economy || rollout.is_some() {
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
                let pinned_pair = if let Some(d) = rollout_state
                    .deployment
                    .as_ref()
                    .filter(|_| rollout.is_some())
                {
                    let candidate = deployment_candidate(d).map_err(invalid)?;
                    candidate.strong.contract_root == crate::server::model_execution_admission::model_execution_contract_root(&baseline)
                        && admitted.as_ref().is_some_and(|e| candidate.economy.contract_root == crate::server::model_execution_admission::model_execution_contract_root(e))
                } else {
                    true
                };
                if compatible && (!treatment || pinned_pair) {
                    if wants_economy {
                        execution = admitted.expect("compatible candidate");
                    }
                } else if wants_economy {
                    reason = if admitted.is_none() {
                        ModelRoutingReason::EconomyUnavailable
                    } else {
                        ModelRoutingReason::IncompatibleCandidate
                    };
                }
                // Observational admission can reject a deployment without
                // changing deterministic Auto in either shadow or control.
                if (!compatible || !pinned_pair)
                    && let Some(r) = &mut rollout
                {
                    r.abstained = true;
                    r.admission_rejected = true;
                    r.proposed_offering_id = policy.strong_offering_id.clone();
                }
            }
            if treatment
                && !matches!(
                    reason,
                    ModelRoutingReason::EconomyUnavailable
                        | ModelRoutingReason::IncompatibleCandidate
                )
            {
                reason = if rollout.as_ref().is_some_and(|r| r.abstained) {
                    ModelRoutingReason::LearnedAbstention
                } else {
                    ModelRoutingReason::LearnedCanary
                };
            }
            if let Some(r) = &mut rollout {
                r.routing_overhead_us =
                    u64::try_from(routing_started.elapsed().as_micros()).unwrap_or(u64::MAX);
                if treatment {
                    let limit = rollout_state
                        .deployment
                        .as_ref()
                        .expect("deployment")
                        .review
                        .maximum_routing_overhead_ms
                        * 1000;
                    if r.routing_overhead_us > limit {
                        r.routing_failure = Some(RouterRoutingFailure::OverheadBudgetExceeded);
                    }
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
                rollout,
                features: Some(features),
                work_admission: self
                    .pending_work_admission
                    .as_ref()
                    .map(|assessment| assessment.decision.clone()),
                work_admission_skill_revision: self.work_admission_skill_revision,
                delegation_model_requirement: model_requirement,
                delegation_model_source: model_requirement.and(model_source),
                policy_version: if treatment {
                    astra_turn_types::model_routing::LEARNED_CANARY_ROUTING_POLICY_VERSION
                } else {
                    astra_turn_core::model_routing::POLICY_VERSION
                }
                .into(),
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

        // Persist failed treatment assignment as well as successful decisions.
        // Recovery must never turn a recorded routing failure into execution.
        if let Some(rollout) = &decision.rollout {
            rollout.ensure_dispatchable().map_err(invalid)?;
        }

        // Everything model-dependent is repinned before prompt construction or
        // compaction. No endpoint or credential is included in the saved fact.
        self.model_override = Some(execution.model_name.clone());
        state.context_manifest_model_name = Some(execution.model_name.clone());
        state.hooks.admitted_model_execution = Some(execution.clone());
        if let Some(executor) = state.runtime_tool_executor.as_ref() {
            executor.set_agent_model_execution(&execution);
        }
        let tool_policy = state
            .admitted_tool_policy
            .resolve_for_model(Some(&execution.model_name));
        state.max_identical_tool_calls = tool_policy.max_identical_tool_calls;
        state.max_tools_per_turn = tool_policy.max_tools_per_turn;
        state.max_consecutive_empty_name = tool_policy.max_consecutive_empty_name;
        if let Some(manifest) = state.runtime_manifest.as_mut() {
            manifest["model_selection"] = json!({"offering_id": execution.offering_id});
            manifest["selected_model"] = json!({"model": execution.model_name});
            manifest["model_resolution"] = json!({"source":"auto", "offering_id":execution.offering_id, "model":execution.model_name, "resolved":true});
            manifest["model_routing"] =
                serde_json::to_value(&decision).map_err(|error| invalid(error.to_string()))?;
        }
        self.admitted_model_execution = Some(execution);
        self.clear_resolved_llm_config();
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
        if let Some(pinned) = routing.decision.as_ref().and_then(|d| d.rollout.as_ref())
            && pinned.cohort == RolloutCohort::Treatment
        {
            let store = routing
                .rollout_store
                .as_ref()
                .ok_or("Router rollout store unavailable")?;
            validate_pinned_treatment(
                &load_live_rollout(store.as_ref(), &self.user_id).await?,
                pinned,
                Utc::now(),
            )?;
        }
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
