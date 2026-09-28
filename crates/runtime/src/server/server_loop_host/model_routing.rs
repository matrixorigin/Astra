use super::*;
use crate::server::run::engine::RunEngine;
use astra_services::{AdmittedModelExecution, ModelExecutionPlacement, ModelService};
use astra_turn_types::model_routing::{AutoModelRoutingPolicy, ModelRoutingReason};

use astra_services::model_routing::{DECISION_KEY, EVENT_TYPE, ModelRoutingDecision};

pub(super) struct AutoRoutingContext {
    policy: AutoModelRoutingPolicy,
    service: Arc<dyn ModelService>,
    engine: RunEngine,
    supported_input: bool,
    decision: Option<ModelRoutingDecision>,
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
        if decision.policy_version != astra_turn_core::model_routing::POLICY_VERSION {
            return Err(invalid("Unsupported Auto routing policy version"));
        }
        let admission = decision.work_admission.clone();
        let skill_revision = decision.work_admission_skill_revision;
        routing.decision = Some(decision);
        if let Some(admission) = admission {
            self.apply_work_admission_decision(admission);
            self.work_admission_attempted = true;
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
            self.resolve_pending_work_admission(true).await;
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
                self.pending_work_admission.as_ref(),
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
                let catalog = service.list_models(self.user_id.clone(), false).await;
                let same_access = catalog.as_ref().is_ok_and(|catalog| {
                    let strong = catalog.iter().find(|item| item.offering_id == policy.strong_offering_id && item.is_active);
                    let economy = catalog.iter().find(|item| item.offering_id == policy.economy_offering_id && item.is_active);
                    matches!((strong, economy), (Some(a), Some(b)) if a.access_id == b.access_id && a.access_kind == b.access_kind && a.execution_placement == b.execution_placement)
                });
                if same_access {
                    match crate::server::model_execution_admission::admit_model_execution(
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
                    {
                        Ok(candidate) if preserves_contract(&baseline, &candidate) => {
                            execution = candidate
                        }
                        Ok(_) => reason = ModelRoutingReason::IncompatibleCandidate,
                        Err(_) => reason = ModelRoutingReason::EconomyUnavailable,
                    }
                } else {
                    reason = ModelRoutingReason::EconomyUnavailable;
                }
            }
            let decision = ModelRoutingDecision {
                schema_version: 1,
                features: Some(features),
                work_admission: self.pending_work_admission.clone(),
                work_admission_skill_revision: self.work_admission_skill_revision,
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
        self.model_override = Some(execution.model_name.clone());
        state.context_manifest_model_name = Some(execution.model_name.clone());
        state.hooks.admitted_model_execution = Some(execution.clone());
        if let Some(executor) = state.runtime_tool_executor.as_ref() {
            executor.set_agent_model(&execution.model_name);
        }
        let tool_policy = astra_config::RuntimeConfig::cached()
            .tool_selection
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
