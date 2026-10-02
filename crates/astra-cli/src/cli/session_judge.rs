//! One auxiliary judgment, using the canonical Server session and inference owners.

use astra_thin_client::{
    CompletionOperation, CompletionRequest, CompletionResponse, SessionCreateRequest, ThinClient,
    ThinClientError,
};
use astra_turn_types::{JudgmentRequest, judgment_messages, normalize_judgment_response};
use serde_json::{Value, json};

/// The command's model selector uses the same catalog resolver as chat, with
/// the typed judgment purpose retained on every catalog page.
pub(crate) async fn execute_for_model(
    api: &ThinClient,
    token: &str,
    model: &str,
    message: &str,
    timeout_seconds: u64,
) -> Result<Value, String> {
    let selection = super::session::session_runtime::resolve_server_model_selection(
        api,
        token,
        model,
        astra_core::model_wire::purpose::ModelCatalogPurpose::TypedJudgment,
    )
    .await?;
    Ok(execute(api, token, &selection.offering_id, message, timeout_seconds).await)
}

pub(crate) async fn execute(
    api: &ThinClient,
    token: &str,
    offering_id: &str,
    message: &str,
    timeout_seconds: u64,
) -> Value {
    let judgment: JudgmentRequest = match serde_json::from_str(message) {
        Ok(request) => request,
        Err(error) => {
            return json!({"ok":false,"stage":"validate_request","error":error.to_string()});
        }
    };
    if let Err(error) = judgment.validate() {
        return json!({"ok":false,"stage":"validate_request","error":error});
    }
    let max_tokens = match u32::try_from(judgment.output_token_budget()) {
        Ok(budget) => budget,
        Err(_) => {
            return json!({"ok":false,"stage":"validate_request","error":"judgment output budget exceeds transport limits"});
        }
    };
    // A distinct real session keeps evaluation usage out of the measured agent
    // session. There is no agent run, local workspace, tool set, or chat turn.
    let session = match api
        .create_session(
            Some(token),
            &SessionCreateRequest {
                title: Some("Evidence judgment".into()),
                metadata: Some(serde_json::Map::from_iter([
                    ("purpose".into(), json!("verification_judge")),
                    ("source".into(), json!("cli_session_judge")),
                ])),
                ..Default::default()
            },
        )
        .await
    {
        Ok(session) => session,
        Err(error) => {
            return json!({"ok": false, "stage": "create_session", "error": error.to_string()});
        }
    };
    let Some(session_id) = session
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| uuid::Uuid::parse_str(id).is_ok())
    else {
        return json!({"ok": false, "stage": "create_session", "error": "Server omitted a valid session_id"});
    };
    // Every invocation (including a quorum vote) has its own
    // session, so these coordinates identify exactly one auxiliary operation.
    let mut request = CompletionRequest::new(
        CompletionOperation::VerificationJudge,
        session_id,
        1,
        1,
        1,
        judgment_messages(&judgment),
    )
    .with_offering_id(offering_id)
    .with_timeout(std::time::Duration::from_secs(timeout_seconds));
    request.max_tokens = max_tokens;
    let (mut result, close) = match api.post_completions(token, &request).await {
        Ok(response) => {
            let text = response.first_text();
            let finish_reason = response
                .choices
                .first()
                .map(|choice| choice.finish_reason.as_str());
            let normalized = if finish_reason == Some("stop") {
                text.map(|text| {
                    normalize_judgment_response(
                        &judgment,
                        text,
                        &response.model,
                        response.judgment_provenance,
                    )
                })
            } else {
                None
            };
            let error = if text.is_none() {
                Some("completion omitted text".to_string())
            } else if finish_reason != Some("stop") {
                Some(
                    "completion did not finish normally; incomplete judgments cannot be scored"
                        .to_string(),
                )
            } else {
                normalized
                    .as_ref()
                    .and_then(|result| result.as_ref().err())
                    .map(ToString::to_string)
            };
            let (answers, provenance) = match normalized {
                Some(Ok(normalized)) => (Some(normalized.response), Some(normalized.provenance)),
                _ => (None, None),
            };
            (
                json!({
                    "ok": error.is_none(), "stage": "completion", "session_id": session_id,
                    "completion_id": response.id, "offering_id": response.offering_id,
                    "model": response.model, "text": text, "usage": response.usage,
                    "judgment": answers, "provenance": provenance,
                    "finish_reason": finish_reason, "error": error,
                }),
                true,
            )
        }
        Err(error) => {
            // A client-error rejection is explicit. A gateway, transport or
            // decode failure can leave delivery uncertain: retain identity
            // without retrying or treating closure as inference cancellation.
            let close =
                matches!(&error, ThinClientError::Api { status, .. } if status.is_client_error());
            (
                json!({"ok": false, "stage": "completion", "session_id": session_id,
                "offering_id": offering_id, "error": error.to_string(),
                "delivery_uncertain": !close}),
                close,
            )
        }
    };
    if close {
        match tokio::time::timeout(
            std::time::Duration::from_secs(10),
            api.post_session_close_text(token, session_id),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => result["cleanup_warning"] = json!(error.to_string()),
            Err(_) => result["cleanup_warning"] = json!("session close timed out after 10s"),
        }
    }
    result
}

/// Run one bounded model-requirement stage for a direct Team command.
///
/// The caller supplies the already authenticated session and one command UUID.
/// This deliberately does not create a second session, retry a request, or
/// accept an arbitrary operation identity.
pub(crate) async fn execute_delegation_requirement_stage(
    api: &ThinClient,
    token: &str,
    session_id: &str,
    turn: u32,
    command_intent_id: &str,
    messages: Vec<Value>,
    max_tokens: u32,
) -> Result<CompletionResponse, String> {
    let mut request = CompletionRequest::new(
        CompletionOperation::DelegationIntentExtraction,
        session_id,
        turn,
        0,
        0,
        messages,
    )
    .with_command_intent_id(command_intent_id)
    .with_timeout(std::time::Duration::from_secs(30));
    request.max_tokens = max_tokens;
    request.temperature = 0.0;
    request.validate()?;
    api.post_completions(token, &request)
        .await
        .map_err(|error| error.to_string())
}

/// Interpret one direct `/team run` command and freeze its exact ordered slot
/// constraints. One candidate-aware judgment sees the authenticated command,
/// authorized Chat catalog, and complete ordered slot plan; no serial scope
/// or model-resolution judgment follows it.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn assess_direct_team_model_plan(
    api: &ThinClient,
    token: &str,
    journal: Option<&astra_services::session_journal::JournalWriter>,
    user_id: &str,
    session_id: &str,
    command_identity: &astra_turn_types::DirectDelegationCommandIdentity,
    task: &str,
    request: &astra_services::coordination::DelegationRequest,
    profiles: &[astra_services::coordination::AgentProfile],
) -> Result<astra_turn_types::DirectDelegationModelPlan, String> {
    use astra_services::delegation_model_requirement::{
        bind_delegation_requirements_to_slots, canonical_team_delegation_slot_plan,
        delegation_intent_assessment_request, delegation_model_candidates,
        parse_delegation_intent_requirements_with_request,
    };
    use astra_turn_types::{
        DelegationIntentRequirements, DelegationModelAdmissionOutcome,
        DelegationUserRequirementSource,
    };
    use sha2::Digest;
    let started_at = std::time::Instant::now();
    let record_assessment = |outcome: &str,
                             extraction: Option<&CompletionResponse>,
                             summary: Option<
        &astra_services::delegation_model_requirement::DelegationCandidateJudgmentSummary,
    >| {
        let Some(journal) = journal else { return };
        use astra_services::session_journal::{JournalEvent, TraceSpanBuilder};
        let finished_us = chrono::Utc::now().timestamp_micros().max(0) as u64;
        let attrs = std::collections::HashMap::from([(
            "delegation_model_assessment".to_string(),
            json!({
                "schema_version": 1,
                "command_intent_id": command_identity.command_intent_id,
                "operation_id": format!("{}:{}", CompletionOperation::DelegationIntentExtraction.operation_id(), command_identity.command_intent_id),
                "outcome": outcome,
                "summary": summary,
                "completion_id": extraction.map(|response| &response.id),
                "usage": extraction.map(|response| &response.usage),
            })
            .to_string(),
        )]);
        let event = JournalEvent::trace_span_v2(
            TraceSpanBuilder::default()
                .session_id(Some(session_id))
                .turn(Some(command_identity.session_turn))
                .span_id(format!(
                    "delegation_model_assessment_{}",
                    command_identity.command_intent_id
                ))
                .name("delegation_model_assessment".into())
                .trace_id(Some(command_identity.command_intent_id.clone()))
                .start_us(finished_us.saturating_sub(started_at.elapsed().as_micros() as u64))
                .end_us(finished_us)
                .attrs(Some(&attrs)),
        );
        super::cli_config::cli_utils::append_journal_event_or_warn(
            journal,
            Some(session_id),
            &event,
            "direct_team:delegation_model_assessment",
        );
    };

    if request.user_id != user_id || request.session_id != session_id || request.task != task {
        return Err("Team command identity or task changed before model assessment".into());
    }
    let slot_plan = canonical_team_delegation_slot_plan(request, profiles)?;
    let task_digest = format!("sha256:{:x}", sha2::Sha256::digest(task.as_bytes()));
    let source = DelegationUserRequirementSource {
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        session_turn: command_identity.session_turn,
        applied_intent_id: None,
        command_intent_id: Some(command_identity.command_intent_id.clone()),
        user_intent_digest: task_digest,
    };
    source.validate().map_err(str::to_string)?;

    let (items, _) = super::session::session_runtime::load_server_model_catalog(
        api,
        token,
        astra_core::model_wire::purpose::ModelCatalogPurpose::Chat,
    )
    .await?;
    let catalog = items
        .into_iter()
        .map(astra_services::ModelListItem::from)
        .collect::<Vec<_>>();
    let candidates = delegation_model_candidates(&catalog);
    let assessment_request =
        delegation_intent_assessment_request(task, &candidates, Some(&slot_plan.briefs))?;
    let extraction = execute_delegation_requirement_stage(
        api,
        token,
        session_id,
        command_identity.session_turn,
        &command_identity.command_intent_id,
        assessment_request.messages.clone(),
        u32::try_from(assessment_request.max_output_tokens)
            .map_err(|_| "Delegated model assessment exceeds its output budget")?,
    )
    .await;
    let extraction = match extraction {
        Ok(extraction) => extraction,
        Err(error) => {
            record_assessment("unavailable", None, None);
            return Err(error);
        }
    };
    let extracted = if extraction
        .choices
        .first()
        .is_none_or(|choice| choice.finish_reason != "stop")
    {
        Err("Delegated model requirements did not finish normally; no child was started.".into())
    } else if let Some(text) = extraction.first_text() {
        parse_delegation_intent_requirements_with_request(
            text,
            // The direct CLI command has no separate WorkAdmission fact. The
            // structured extraction contract is the sole authority here; do not
            // replace it with a keyword heuristic that can misread task prose.
            false,
            &assessment_request,
        )
    } else {
        Err("Delegated model assessment omitted its response".into())
    };
    record_assessment(
        if extracted.is_ok() {
            "parsed"
        } else {
            "invalid"
        },
        Some(&extraction),
        extracted
            .as_ref()
            .ok()
            .map(|assessment| &assessment.summary),
    );
    let extracted = extracted?;
    let assessed =
        astra_services::delegation_model_requirement::materialize_delegation_intent_requirements(
            &extracted,
            source.clone(),
        )?;
    if let DelegationIntentRequirements::Unresolved { reason, .. }
    | DelegationIntentRequirements::Unavailable { reason, .. } = &assessed
    {
        return Err(reason.clone());
    }

    let binding = extracted.scope_binding.as_ref();
    if let Some(binding) = binding {
        let scoped = match &assessed {
            DelegationIntentRequirements::Requirements { requirements, .. } => requirements
                .iter()
                .filter(|requirement| requirement.task_scope_quote.is_some())
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        let unmatched_direct_scope = scoped.iter().any(|requirement| {
            requirement.propagation
                == astra_turn_types::DelegationRequirementPropagation::DirectChildren
                && binding.assignments.iter().any(|assignment| {
                    assignment.requirement_id == requirement.requirement_id
                        && assignment.slot_indices.is_empty()
                })
        });
        if unmatched_direct_scope {
            return Err(
                "A direct-child model requirement does not match any Team task; no child was started."
                    .into(),
            );
        }
    }
    let (slots, child_requirements) =
        bind_delegation_requirements_to_slots(&assessed, binding, &slot_plan.briefs)?;
    let outcome = if slots.iter().all(|slot| {
        slot.model_selection.is_none()
            && slot.requested_model_policy.is_none()
            && slot.reasoning.is_none()
    }) {
        DelegationModelAdmissionOutcome::ExplicitlyUnconstrained {
            slot_count: slots.len() as u32,
        }
    } else {
        DelegationModelAdmissionOutcome::Constrained { slots }
    };
    let plan = astra_turn_types::DirectDelegationModelPlan {
        source,
        slot_plan_digest: slot_plan.digest,
        outcome,
        child_requirements,
    };
    plan.validate_identity(
        command_identity,
        user_id,
        session_id,
        &plan.source.user_intent_digest,
        &plan.slot_plan_digest,
        slot_plan.briefs.len(),
    )
    .map_err(str::to_string)?;
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::JUDGMENT_SCHEMA_VERSION;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, header, method, path},
    };

    const SESSION: &str = "6bca9f9c-6d18-4579-bce1-2b45f573a098";

    #[tokio::test]
    async fn session_judge_resolves_typesafe_through_typed_catalog_before_execution() {
        let native =
            json!({"schema_version":JUDGMENT_SCHEMA_VERSION,"model":"jev","answers":{"0":{"type":"noul","noul":0.9}}})
                .to_string();
        let server = session_server(ResponseTemplate::new(200).set_body_json(json!({
            "id":"completion-1", "object":"chat.completion", "offering_id":"offering-1", "model":"jev",
            "judgment_provenance":"provider_probability",
            "choices":[{"index":0,"message":{"role":"assistant","content":native},"finish_reason":"stop"}]
        })), Some(200)).await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(wiremock::matchers::query_param("purpose", "typed_judgment"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{"offering_id":"offering-1","access_id":"self-hosted",
                    "access_kind":"self_hosted","access_label":"Self-hosted",
                    "execution_placement":"server","name":"jev","provider":"typesafe",
                    "description":null,"is_active":true,"context_window":64000,
                    "max_completion_tokens":512,"architecture":null,"thinking_capability":null}],
                "total":1,"limit":50,"next_cursor":null,"catalog_revision":"judgment-only"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute_for_model(&api, "test-token", "jev", &judgment_input(), 37)
            .await
            .expect("CLI judge model selection");
        assert_eq!(result["ok"], true, "{result}");
        let requests = server.received_requests().await.unwrap();
        let completion = requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
            .unwrap();
        let body: Value = serde_json::from_slice(&completion.body).unwrap();
        assert_eq!(body["model_selection"]["offering_id"], "offering-1");
        assert!(
            !requests
                .iter()
                .any(|request| request.url.path() == "/chat/stream")
        );
    }

    async fn session_server(completion: ResponseTemplate, close_status: Option<u16>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/sessions"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({"session_id": SESSION})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(completion)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(format!("/sessions/{SESSION}/close")))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(
                ResponseTemplate::new(close_status.unwrap_or(200)).set_body_json(json!({})),
            )
            .expect(u64::from(close_status.is_some()))
            .mount(&server)
            .await;
        server
    }

    fn completion_with_finish_reason(reason: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "completion-1", "object": "chat.completion", "offering_id": "offering-1", "model": "judge",
            "judgment_provenance": "discrete_decision",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "{\"answers\":{\"0\":{\"type\":\"discrete_noul\",\"decision\":\"no\"}}}"}, "finish_reason": reason}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110},
        }))
    }

    fn judgment_input() -> String {
        json!({"schema_version":JUDGMENT_SCHEMA_VERSION,"state":{"evidence":"quoted evidence"},"questions":{"0":{"type":"noul","instructions":"Evidence supports the criterion."}}}).to_string()
    }

    fn completion() -> ResponseTemplate {
        completion_with_finish_reason("stop")
    }

    #[tokio::test]
    async fn nonterminal_judgment_is_rejected_even_with_valid_decisions() {
        for reason in ["length", "content_filter", "unknown"] {
            let server = session_server(completion_with_finish_reason(reason), Some(200)).await;
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
            assert_eq!(result["ok"], false);
            assert_eq!(result["finish_reason"], reason);
            assert_eq!(result["completion_id"], "completion-1");
            assert!(result["judgment"].is_null());
        }
    }

    #[tokio::test]
    async fn direct_team_requirement_stage_uses_bound_session_and_never_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"completion-team-1", "object":"chat.completion",
                "offering_id":"offering-1", "model":"flash",
                "choices":[{"index":0,"message":{"role":"assistant","content":"{}"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":40,"completion_tokens":5,"total_tokens":45}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let command_id = "eb1b8c4a-4fc0-4a56-86e8-c1fc36d0d21a";
        let response = execute_delegation_requirement_stage(
            &api,
            "test-token",
            SESSION,
            7,
            command_id,
            vec![json!({"role":"user","content":"use flash"})],
            768,
        )
        .await
        .expect("one bounded extraction completion");

        assert_eq!(response.usage.as_ref().unwrap().total_tokens, 45);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["operation"], "delegation_intent_extraction");
        assert_eq!(body["command_intent_id"], command_id);
        assert_eq!(body["session_id"], SESSION);
        assert_eq!(body["turn"], 7);
        assert_eq!(body["max_tokens"], 768);
        assert_eq!(body["round"], 0);
        assert_eq!(body["logical_attempt"], 0);
        assert!(
            !requests
                .iter()
                .any(|request| request.url.path() == "/sessions")
        );
    }

    #[tokio::test]
    async fn direct_team_keeps_unmatched_descendant_scope_for_nested_tasks() {
        use astra_services::coordination::{
            AgentProfile, AgentTier, AggregationStrategy, CoordinationPattern, DelegationRequest,
        };

        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_scope =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let journal = astra_services::session_journal::JournalWriter::new(SESSION).unwrap();
        let server = MockServer::start().await;
        let extracted = json!({
            "disposition": "resolved",
            "requirements": [{
                "candidate_index": 0,
                "model_quote": "flash",
                "source_quote": null,
                "scope_quote": "nested reviewers",
                "slots": [],
                "reasoning": null,
                "reasoning_quote": null,
                "automatic_strategy": null,
                "propagation": "descendants",
                "strength": "hard"
            }]
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-token"))
            .and(body_partial_json(json!({
                "operation": "delegation_intent_extraction"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "completion-extraction",
                "object": "chat.completion",
                "offering_id": "offering-judge",
                "model": "judge",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": extracted.to_string()},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 40, "completion_tokens": 20, "total_tokens": 60}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [{
                    "offering_id": "offering-flash",
                    "access_id": "self-hosted",
                    "access_kind": "self_hosted",
                    "access_label": "Self-hosted",
                    "execution_placement": "server",
                    "name": "flash",
                    "provider": "test",
                    "description": null,
                    "is_active": true,
                    "context_window": 64000,
                    "max_completion_tokens": 512,
                    "architecture": null,
                    "thinking_capability": null
                }],
                "total": 1,
                "limit": 50,
                "next_cursor": null,
                "catalog_revision": "sha256:direct-team-test"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let request = DelegationRequest {
            session_id: SESSION.into(),
            delegation_id: "delegation-1".into(),
            parent_run_id: "parent-1".into(),
            task: "Use flash for nested reviewers".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: std::collections::HashMap::new(),
            execution_metadata: None,
        };
        let profiles = vec![
            AgentProfile::new("coder", "Coder", AgentTier::User),
            AgentProfile::new("reviewer", "Reviewer", AgentTier::User),
        ];
        let identity = astra_turn_types::DirectDelegationCommandIdentity {
            command_intent_id: "eb1b8c4a-4fc0-4a56-86e8-c1fc36d0d21a".into(),
            session_turn: 7,
        };
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let plan = assess_direct_team_model_plan(
            &api,
            "test-token",
            Some(&journal),
            "user-1",
            SESSION,
            &identity,
            &request.task,
            &request,
            &profiles,
        )
        .await
        .expect("descendant scope may have no current direct slot");

        assert!(matches!(
            plan.outcome,
            astra_turn_types::DelegationModelAdmissionOutcome::ExplicitlyUnconstrained {
                slot_count: 2
            }
        ));
        assert_eq!(plan.child_requirements.len(), 2);
        for child in plan.child_requirements {
            let astra_turn_types::DelegationIntentRequirements::Requirements {
                requirements, ..
            } = child
            else {
                panic!("descendant requirement was dropped");
            };
            assert_eq!(requirements.len(), 1);
            assert_eq!(
                requirements[0]
                    .model_selection
                    .as_ref()
                    .unwrap()
                    .offering_id,
                "offering-flash"
            );
            assert_eq!(
                requirements[0].propagation,
                astra_turn_types::DelegationRequirementPropagation::Descendants
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        let events = astra_services::session_journal::read_journal(SESSION).unwrap();
        let assessment = events
            .iter()
            .find(|event| {
                event
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata["name"].as_str())
                    == Some("delegation_model_assessment")
            })
            .expect("candidate decision must be in the existing session trace");
        let evidence: Value = serde_json::from_str(
            assessment.metadata.as_ref().unwrap()["attrs"]["delegation_model_assessment"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            evidence["summary"]["candidate_snapshot_digest"].is_string(),
            true
        );
        assert_eq!(
            evidence["summary"]["selections"][0]["candidate_id"],
            "offering-flash"
        );
        assert_eq!(
            evidence["summary"]["selections"][0]["slot_indices"],
            json!([])
        );
        assert_eq!(evidence["usage"]["total_tokens"], 60);
        assert_eq!(
            evidence["operation_id"],
            format!("delegation_intent:{}", identity.command_intent_id)
        );
        assert!(!format!("{evidence}").contains("nested reviewers"));
    }

    #[tokio::test]
    async fn direct_team_rejects_unmatched_direct_scope_before_children() {
        use astra_services::coordination::{
            AgentProfile, AgentTier, AggregationStrategy, CoordinationPattern, DelegationRequest,
        };

        let journal_dir = tempfile::tempdir().unwrap();
        let _journal_scope =
            astra_services::session_journal::JournalDirGuard::new(journal_dir.path());
        let journal = astra_services::session_journal::JournalWriter::new(SESSION).unwrap();
        let server = MockServer::start().await;
        let extracted = json!({
            "disposition": "resolved",
            "requirements": [{
                "candidate_index": null,
                "model_quote": null,
                "source_quote": null,
                "scope_quote": "nested reviewers",
                "slots": [],
                "reasoning": {"mode": "effort", "effort": "high"},
                "reasoning_quote": "high reasoning",
                "automatic_strategy": null,
                "propagation": "direct_children",
                "strength": "hard"
            }]
        });
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-token"))
            .and(body_partial_json(json!({
                "operation": "delegation_intent_extraction"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "completion-extraction",
                "object": "chat.completion",
                "offering_id": "offering-judge",
                "model": "judge",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": extracted.to_string()},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 40, "completion_tokens": 20, "total_tokens": 60}
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/models"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "items": [], "total": 0, "limit": 50,
                "next_cursor": null, "catalog_revision": "sha256:empty-direct-team-test"
            })))
            .expect(1)
            .mount(&server)
            .await;

        let request = DelegationRequest {
            session_id: SESSION.into(),
            delegation_id: "delegation-direct-scope-1".into(),
            parent_run_id: "parent-direct-scope-1".into(),
            task: "Use high reasoning for nested reviewers".into(),
            pattern: CoordinationPattern::FanOut {
                agent_ids: vec!["coder".into(), "reviewer".into()],
                aggregation: AggregationStrategy::AllResults,
                timeout_sec: 60,
            },
            user_id: "user-1".into(),
            depth: 0,
            delegation_chain: Vec::new(),
            context: std::collections::HashMap::new(),
            execution_metadata: None,
        };
        let profiles = vec![
            AgentProfile::new("coder", "Coder", AgentTier::User),
            AgentProfile::new("reviewer", "Reviewer", AgentTier::User),
        ];
        let identity = astra_turn_types::DirectDelegationCommandIdentity {
            command_intent_id: "91e5a4fd-ea43-4d87-9eb9-bc5a9bb3d6a2".into(),
            session_turn: 8,
        };
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let error = assess_direct_team_model_plan(
            &api,
            "test-token",
            Some(&journal),
            "user-1",
            SESSION,
            &identity,
            &request.task,
            &request,
            &profiles,
        )
        .await
        .expect_err("an unmatched direct-child scope must fail closed");

        assert!(error.contains("direct-child model requirement"), "{error}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "assessment stops before any child admission or execution"
        );
        assert!(
            astra_services::session_journal::read_journal(SESSION)
                .unwrap()
                .iter()
                .any(|event| event
                    .metadata
                    .as_ref()
                    .is_some_and(|metadata| metadata["name"] == "delegation_model_assessment"))
        );
    }

    #[tokio::test]
    async fn uncertain_direct_team_requirement_completion_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer test-token"))
            .respond_with(ResponseTemplate::new(504))
            .expect(1)
            .mount(&server)
            .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();

        let error = execute_delegation_requirement_stage(
            &api,
            "test-token",
            SESSION,
            7,
            "eb1b8c4a-4fc0-4a56-86e8-c1fc36d0d21a",
            vec![json!({"role":"user","content":"use flash"})],
            768,
        )
        .await
        .expect_err("uncertain delivery fails closed");

        assert!(!error.is_empty());
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn gateway_failure_retains_uncertain_delivery_without_closing_session() {
        let server = session_server(ResponseTemplate::new(504), None).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["delivery_uncertain"], true);
    }

    #[tokio::test]
    async fn judgment_uses_only_governed_auxiliary_inference_and_closes_owned_session() {
        let server = session_server(completion(), Some(200)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], true);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["usage"]["total_tokens"], 110);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests.len(),
            3,
            "no chat/run/tool HTTP requests permitted"
        );
        let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
        assert_eq!(body["operation"], "verification_judge");
        assert_eq!(body["session_id"], SESSION);
        assert_eq!(body["model_selection"]["offering_id"], "offering-1");
        let judgment: JudgmentRequest = serde_json::from_str(&judgment_input()).unwrap();
        assert_eq!(body["max_tokens"], judgment.output_token_budget());
        assert_eq!(body["messages"], json!(judgment_messages(&judgment)));
        assert_eq!(result["provenance"], "discrete_decision");
        assert_eq!(result["judgment"]["answers"]["0"]["decision"], "no");
        assert_eq!(body["timeout_ms"], 37_000);
        assert!(body.get("tools").is_none());
        let metadata: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(!metadata.to_string().contains("quoted evidence"));
    }

    #[tokio::test]
    async fn session_close_failure_preserves_completed_judgment_with_warning() {
        let server = session_server(completion(), Some(503)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], true);
        assert!(result["cleanup_warning"].as_str().unwrap().contains("503"));
    }

    #[tokio::test]
    async fn completion_rejection_preserves_identity_and_does_not_retry() {
        let server = session_server(
            ResponseTemplate::new(429).set_body_string("capacity exhausted"),
            Some(200),
        )
        .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert!(
            result["error"]
                .as_str()
                .unwrap()
                .contains("capacity exhausted")
        );
    }

    #[tokio::test]
    async fn malformed_completion_keeps_session_for_uncertain_delivery_diagnosis() {
        let server = session_server(
            ResponseTemplate::new(200).set_body_string("broken transport body"),
            None,
        )
        .await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["session_id"], SESSION);
        assert_eq!(result["delivery_uncertain"], true);
    }
    #[tokio::test]
    async fn invalid_typed_request_does_not_create_session() {
        let server = MockServer::start().await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        for message in [
            "free form rubric",
            r#"{"schema_version":1,"state":null,"questions":{}}"#,
        ] {
            let result = execute(&api, "test-token", "offering-1", message, 37).await;
            assert_eq!(result["ok"], false);
            assert_eq!(result["stage"], "validate_request");
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_decisions_close_completed_session_without_retry() {
        let body = json!({"id":"completion-1","object":"chat.completion","offering_id":"offering-1","model":"judge","judgment_provenance":"discrete_decision","choices":[{"index":0,"message":{"role":"assistant","content":"{\"true\":[\"unknown\"],\"uncertain\":[]}"},"finish_reason":"stop"}]});
        let server =
            session_server(ResponseTemplate::new(200).set_body_json(body), Some(200)).await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
        assert_eq!(result["ok"], false);
        assert_eq!(result["finish_reason"], "stop");
        assert!(result["judgment"].is_null());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }
    #[tokio::test]
    async fn judgment_source_and_identity_come_from_server_execution_metadata() {
        for (provenance, valid) in [
            (Some("provider_probability"), true),
            (Some("discrete_decision"), false),
            (None, false),
        ] {
            let text = json!({"schema_version":JUDGMENT_SCHEMA_VERSION,"model":"untrusted-answer-model","answers":{"0":{"type":"noul","noul":0.93}}}).to_string();
            let body = json!({"id":"completion-1","object":"chat.completion","offering_id":"offering-1","model":"native-judge","judgment_provenance":provenance,"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]});
            let server =
                session_server(ResponseTemplate::new(200).set_body_json(body), Some(200)).await;
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let result = execute(&api, "test-token", "offering-1", &judgment_input(), 37).await;
            assert_eq!(result["ok"], valid, "{result}");
            if valid {
                assert_eq!(result["provenance"], "provider_probability");
                assert_eq!(result["judgment"]["model"], "native-judge");
                assert_eq!(result["judgment"]["answers"]["0"]["noul"], 0.93);
            } else {
                assert!(result["judgment"].is_null());
            }
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                3,
                "one completion and owned session create/close; no repair inference"
            );
        }
    }
}
