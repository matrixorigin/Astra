use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinSet;

use astra_core::{MatrixOneSettings, SharedPool};
use astra_services::{
    AdminConfigService, FernetTokenEncryptor, SkillifyAgentDraft, SkillifyAgentExecutor,
    SkillifyAgentOutput, SkillifyAgentRequest, SkillifySourcePacket,
};
use astra_turn_core::thinking_config::ThinkingConfig;

use crate::turn::llm::{
    client::{LlmCall, LlmExecutionRoute, global_llm_client, llm_nonstream_timeout},
    durable::DurableInferenceLedger,
};

const SKILLIFY_EXTRACTION_OUTPUT_TOKENS: usize = 5000;
const SKILLIFY_SYNTHESIS_OUTPUT_TOKENS: usize = 7000;
const SKILLIFY_CHUNK_CHAR_BUDGET: usize = 28_000;
const SKILLIFY_MAX_CHUNKS: usize = 64;

#[derive(Clone)]
pub(super) struct RuntimeSkillifyAgentExecutor {
    matrixone: MatrixOneSettings,
    encryptor: Arc<FernetTokenEncryptor>,
    admin_config_service: Arc<dyn AdminConfigService>,
    pool: SharedPool,
}

#[derive(Clone)]
struct SkillifyInferenceExecution {
    admitted: astra_services::AdmittedModelExecution,
    ledger: DurableInferenceLedger,
}

impl RuntimeSkillifyAgentExecutor {
    pub(super) fn new(
        matrixone: MatrixOneSettings,
        encryptor: Arc<FernetTokenEncryptor>,
        admin_config_service: Arc<dyn AdminConfigService>,
        pool: SharedPool,
    ) -> Self {
        Self {
            matrixone,
            encryptor,
            admin_config_service,
            pool,
        }
    }

    async fn prepare_inference_execution(
        &self,
        user_id: &str,
    ) -> Result<SkillifyInferenceExecution, String> {
        let offering = astra_services::resolve_reasoning_offering(
            &self.matrixone,
            &self.encryptor,
            self.admin_config_service.as_ref(),
            Some(self.pool.get()),
        )
        .await
        .map_err(|error| format!("Offering resolution failed: {error}"))?;
        let admitted = astra_services::AdmittedModelExecution::from_offering(offering)
            .map_err(|error| format!("Offering execution configuration is invalid: {error}"))?;
        Ok(SkillifyInferenceExecution {
            ledger: DurableInferenceLedger::new(self.pool.clone(), user_id, admitted.clone()),
            admitted,
        })
    }

    async fn call_json_agent(
        execution: &SkillifyInferenceExecution,
        scope: astra_turn_types::InferenceInvocationScope,
        system_prompt: &str,
        user_prompt: &str,
        max_output_tokens: usize,
    ) -> Result<String, String> {
        let messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "user", "content": user_prompt}),
        ];
        let result = execution
            .ledger
            .execute_nonstream(
                global_llm_client(),
                scope,
                LlmCall {
                    purpose: astra_turn_types::InferencePurpose::SkillSynthesis,
                    messages: &messages,
                    tools: &[],
                    cache_capability: None,
                    route: LlmExecutionRoute::from_admitted(&execution.admitted),
                    max_output_tokens: Some(max_output_tokens),
                    temperature: None,
                    has_fallback: false,
                    thinking: &ThinkingConfig::Off,
                },
                llm_nonstream_timeout(),
            )
            .await
            .map_err(|error| {
                let message = crate::turn::llm::client::redact_provider_secrets(&error.message);
                format!(
                    "LLM call failed: {}",
                    astra_text_utils::str_preview::truncate_str(&message, 1_000)
                )
            })?;

        let text = result.full_text.trim();
        if text.is_empty() {
            return Err("LLM returned empty content".to_string());
        }
        Ok(text.to_string())
    }
}

#[async_trait]
impl SkillifyAgentExecutor for RuntimeSkillifyAgentExecutor {
    async fn synthesize_skill_drafts(
        &self,
        request: SkillifyAgentRequest,
    ) -> Result<SkillifyAgentOutput, String> {
        let execution = self.prepare_inference_execution(&request.user_id).await?;
        let chunks = chunk_source_packets(&request.source_packets)?;
        let use_subagents = chunks.len() > 1;
        let mut extraction_results = Vec::with_capacity(chunks.len());

        if use_subagents {
            let mut tasks = JoinSet::new();
            for chunk in chunks {
                let executor = self.clone();
                let execution = execution.clone();
                let req = request.clone();
                tasks.spawn(async move {
                    let index = chunk.index;
                    let output = executor.extract_chunk(&execution, &req, &chunk).await;
                    (index, output)
                });
            }
            extraction_results = drain_skillify_extractions(tasks).await?;
        } else {
            for chunk in chunks {
                extraction_results.push((
                    chunk.index,
                    self.extract_chunk(&execution, &request, &chunk).await?,
                ));
            }
        }

        let extractions = extraction_results
            .into_iter()
            .map(|(_, output)| output)
            .collect::<Vec<_>>();
        let synthesis = self
            .synthesize_parent(&execution, &request, &extractions)
            .await?;

        Ok(SkillifyAgentOutput {
            extractor: "llm_skillify_agent".to_string(),
            subagent_strategy: json!({
                "enabled": use_subagents,
                "chunk_count": extractions.len(),
                "mode": if use_subagents { "parallel_chunk_extraction" } else { "single_chunk_extraction" },
            }),
            drafts: synthesis.drafts,
        })
    }
}

async fn drain_skillify_extractions(
    mut tasks: JoinSet<(usize, Result<ExtractionResponse, String>)>,
) -> Result<Vec<(usize, ExtractionResponse)>, String> {
    let mut extraction_results = Vec::new();
    let mut failures = Vec::new();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((index, Ok(output))) => extraction_results.push((index, output)),
            Ok((index, Err(error))) => failures.push(format!("chunk {index}: {error}")),
            Err(error) => failures.push(format!("extraction task failed to join: {error}")),
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "Skillify extraction failed after draining all chunks: {}",
            failures.join("; ")
        ));
    }
    extraction_results.sort_by_key(|(index, _)| *index);
    Ok(extraction_results)
}

impl RuntimeSkillifyAgentExecutor {
    async fn extract_chunk(
        &self,
        execution: &SkillifyInferenceExecution,
        request: &SkillifyAgentRequest,
        chunk: &SourceChunk,
    ) -> Result<ExtractionResponse, String> {
        let system_prompt = skillify_extraction_system_prompt();
        let user_prompt = skillify_extraction_user_prompt(request, chunk)?;
        let response = Self::call_json_agent(
            execution,
            astra_turn_types::InferenceInvocationScope::HarnessRun {
                harness_run_id: request.harness_run_id.clone(),
                operation_id: "skillify_extract".to_string(),
                logical_attempt: u32::try_from(chunk.index)
                    .map_err(|_| "Skillify chunk index exceeds u32".to_string())?,
            },
            system_prompt,
            &user_prompt,
            SKILLIFY_EXTRACTION_OUTPUT_TOKENS,
        )
        .await?;
        parse_json_response::<ExtractionResponse>(&response)
    }

    async fn synthesize_parent(
        &self,
        execution: &SkillifyInferenceExecution,
        request: &SkillifyAgentRequest,
        extractions: &[ExtractionResponse],
    ) -> Result<SynthesisResponse, String> {
        let system_prompt = skillify_synthesis_system_prompt();
        let user_prompt = skillify_synthesis_user_prompt(request, extractions)?;
        let response = Self::call_json_agent(
            execution,
            astra_turn_types::InferenceInvocationScope::HarnessRun {
                harness_run_id: request.harness_run_id.clone(),
                operation_id: "skillify_synthesize".to_string(),
                logical_attempt: 0,
            },
            system_prompt,
            &user_prompt,
            SKILLIFY_SYNTHESIS_OUTPUT_TOKENS,
        )
        .await?;
        parse_json_response::<SynthesisResponse>(&response)
    }
}

#[derive(Clone, Debug, Serialize)]
struct ChunkPacket {
    event_id: String,
    session_id: String,
    source_id: String,
    source_type: String,
    title: String,
    event_type: String,
    chunk_index: usize,
    content: String,
}

#[derive(Clone, Debug)]
struct SourceChunk {
    index: usize,
    packets: Vec<ChunkPacket>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractionResponse {
    signals: Vec<ExtractedSignal>,
    conflicts: Vec<SkillifyConflict>,
    dropped_signals: Vec<DroppedSignal>,
    source_summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractedSignal {
    signal_type: String,
    statement: String,
    rationale: String,
    confidence: f64,
    citations: Vec<ExtractedCitation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtractedCitation {
    source_id: String,
    source_excerpt: String,
    source_locator_json: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillifyConflict {
    summary: String,
    source_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DroppedSignal {
    summary: String,
    reason: String,
    source_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SynthesisResponse {
    drafts: Vec<SkillifyAgentDraft>,
}

fn chunk_source_packets(packets: &[SkillifySourcePacket]) -> Result<Vec<SourceChunk>, String> {
    let mut chunks = Vec::new();
    let mut current = Vec::new();
    let mut current_chars = 0usize;

    for packet in packets {
        let pieces = split_packet(packet, SKILLIFY_CHUNK_CHAR_BUDGET);
        for piece in pieces {
            let piece_chars = piece.content.chars().count();
            if !current.is_empty()
                && current_chars.saturating_add(piece_chars) > SKILLIFY_CHUNK_CHAR_BUDGET
            {
                chunks.push(SourceChunk {
                    index: chunks.len(),
                    packets: std::mem::take(&mut current),
                });
                current_chars = 0;
            }
            current_chars = current_chars.saturating_add(piece_chars);
            current.push(piece);
        }
    }

    if !current.is_empty() {
        chunks.push(SourceChunk {
            index: chunks.len(),
            packets: current,
        });
    }

    if chunks.len() > SKILLIFY_MAX_CHUNKS {
        return Err(format!(
            "normalized Skillify sources produced {} chunks, exceeding the limit of {SKILLIFY_MAX_CHUNKS}; narrow the selected sessions or files",
            chunks.len()
        ));
    }

    Ok(chunks)
}

fn split_packet(packet: &SkillifySourcePacket, char_budget: usize) -> Vec<ChunkPacket> {
    let content = packet.content.trim();
    if content.is_empty() {
        return Vec::new();
    }

    let chars = content.chars().collect::<Vec<_>>();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut chunk_index = 0usize;
    while start < chars.len() {
        let end = start.saturating_add(char_budget).min(chars.len());
        let slice = chars[start..end].iter().collect::<String>();
        out.push(ChunkPacket {
            event_id: packet.event_id.clone(),
            session_id: packet.session_id.clone(),
            source_id: packet.source_id.clone(),
            source_type: packet.source_type.clone(),
            title: packet.title.clone(),
            event_type: packet.event_type.clone(),
            chunk_index,
            content: slice,
        });
        start = end;
        chunk_index += 1;
    }
    out
}

fn skillify_extraction_system_prompt() -> &'static str {
    r#"You are a Skillify extraction subagent.

Your job is only to extract evidence-backed signals from the assigned source chunk.
Do not write final SKILL.md content.
Do not summarize each source independently.
Prefer reusable user preferences, workflows, conventions, tool habits, constraints, and style rules.
Every signal must include at least one citation that preserves the original source_id and a short exact source excerpt.
If evidence is weak, ambiguous, one-off, or merely task content, put it in dropped_signals instead of signals.
Return only valid JSON. Do not wrap it in Markdown.
"#
}

fn skillify_extraction_user_prompt(
    request: &SkillifyAgentRequest,
    chunk: &SourceChunk,
) -> Result<String, String> {
    let source_json = serde_json::to_string_pretty(&chunk.packets)
        .map_err(|error| format!("failed to serialize source chunk: {error}"))?;
    Ok(r##"Skillify target:
- requested skill_name: __SKILL_NAME__
- topic: __TOPIC__
- target_scope: __TARGET_SCOPE__
- chunk_index: __CHUNK_INDEX__

Source packets:
__SOURCE_JSON__

Return this JSON shape:
{
  "signals": [
    {
      "signal_type": "preference | workflow | convention | tool_habit | constraint | style",
      "statement": "one reusable instruction",
      "rationale": "why this is a stable skill rule",
      "confidence": 0.0,
      "citations": [
        {
          "source_id": "must match a SourcePacket source_id",
          "source_excerpt": "short exact evidence excerpt",
          "source_locator_json": {
            "event_id": "...",
            "session_id": "...",
            "source_type": "...",
            "title": "...",
            "chunk_index": 0
          }
        }
      ]
    }
  ],
  "conflicts": [
    {"summary": "conflict or tension", "source_ids": ["..."]}
  ],
  "dropped_signals": [
    {"summary": "weak or local observation", "reason": "why it was dropped", "source_ids": ["..."]}
  ],
  "source_summary": "brief summary of what this chunk contributes"
}"##
    .replace(
        "__SKILL_NAME__",
        request.skill_name.as_deref().unwrap_or("(none)"),
    )
    .replace(
        "__TOPIC__",
        request
            .topic
            .as_deref()
            .unwrap_or("(all skill-relevant signals)"),
    )
    .replace("__TARGET_SCOPE__", &request.target_scope)
    .replace("__CHUNK_INDEX__", &chunk.index.to_string())
    .replace("__SOURCE_JSON__", &source_json))
}

fn skillify_synthesis_system_prompt() -> &'static str {
    r#"You are the parent Skillify agent.

Create a small set of coherent, reviewable draft skills from extracted signals.
You own global synthesis: cluster across chunks, remove duplicates, expose conflicts, decide how many skills are justified, and draft final SkillDraft objects.
Do not preserve chunk boundaries or produce one skill per source.
Prefer fewer, stronger skills over many source-shaped skills.
Subagent outputs are evidence, not final wording. Preserve citations on every rule.
Do not invent rules that are not supported by extracted citations.
Return only valid JSON. Do not wrap it in Markdown.
"#
}

fn skillify_synthesis_user_prompt(
    request: &SkillifyAgentRequest,
    extractions: &[ExtractionResponse],
) -> Result<String, String> {
    let extraction_json = serde_json::to_string_pretty(extractions)
        .map_err(|error| format!("failed to serialize extraction outputs: {error}"))?;
    Ok(r##"Skillify target:
- requested skill_name: __SKILL_NAME__
- topic: __TOPIC__
- target_scope: __TARGET_SCOPE__

Extraction outputs:
__EXTRACTION_JSON__

Return this JSON shape:
{
  "drafts": [
    {
      "candidate_name": "lowercase-kebab-case-or-underscore skill id, max 80 chars",
      "description": "what this skill helps the agent do",
      "target_scope": "personal | project",
      "publish_visibility": "private | public",
      "content_markdown": "# Skill Name\n\n## When To Use\n...\n\n## Rules\n- ...\n\n## Guardrails\n- ...",
      "source_summary_json": {
        "source_count": 0,
        "source_summary": "why these sources justify this skill",
        "conflicts": []
      },
      "confidence": 0.0,
      "rules": [
        {
          "rule_type": "preference | workflow | convention | tool_habit | constraint | style",
          "statement": "one reusable instruction",
          "rationale": "why this belongs in the skill",
          "confidence": 0.0,
          "citations": [
            {
              "source_id": "must match an original SourcePacket source_id",
              "source_excerpt": "preserved evidence excerpt",
              "source_locator_json": {"event_id": "...", "session_id": "...", "source_type": "...", "title": "...", "chunk_index": 0}
            }
          ]
        }
      ]
    }
  ]
}"##
    .replace(
        "__SKILL_NAME__",
        request
            .skill_name
            .as_deref()
            .unwrap_or("(generate from evidence)"),
    )
    .replace(
        "__TOPIC__",
        request
            .topic
            .as_deref()
            .unwrap_or("(all skill-relevant signals)"),
    )
    .replace("__TARGET_SCOPE__", &request.target_scope)
    .replace("__EXTRACTION_JSON__", &extraction_json))
}

fn parse_json_response<T: for<'de> Deserialize<'de>>(text: &str) -> Result<T, String> {
    serde_json::from_str(text.trim())
        .map_err(|error| format!("failed to parse JSON response: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(id: &str, content: &str) -> SkillifySourcePacket {
        SkillifySourcePacket {
            event_id: format!("event-{id}"),
            session_id: "session-1".to_string(),
            source_id: id.to_string(),
            source_type: "session".to_string(),
            title: format!("source {id}"),
            event_type: "user_query".to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn parse_json_response_accepts_exact_contract() {
        let parsed: SynthesisResponse =
            parse_json_response(r#"{"drafts":[]}"#).expect("parse exact JSON response");
        assert!(parsed.drafts.is_empty());
    }

    #[test]
    fn parse_json_response_rejects_prose_fences_and_unknown_fields() {
        assert!(parse_json_response::<SynthesisResponse>(r#"```json{"drafts":[]}```"#).is_err());
        assert!(parse_json_response::<SynthesisResponse>(r#"Result: {"drafts":[]}"#).is_err());
        assert!(
            parse_json_response::<SynthesisResponse>(r#"{"drafts":[],"status":"ok"}"#).is_err()
        );
    }

    #[tokio::test]
    async fn parallel_extraction_drains_siblings_before_returning_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (error_ready_tx, error_ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let sibling_completed = Arc::new(AtomicBool::new(false));
        let sibling_completed_in_task = Arc::clone(&sibling_completed);
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _ = error_ready_tx.send(());
            (0, Err("provider rejected chunk".to_string()))
        });
        tasks.spawn(async move {
            release_rx.await.expect("release sibling");
            sibling_completed_in_task.store(true, Ordering::Release);
            (
                1,
                Ok(ExtractionResponse {
                    signals: Vec::new(),
                    conflicts: Vec::new(),
                    dropped_signals: Vec::new(),
                    source_summary: "drained".to_string(),
                }),
            )
        });

        let drain = tokio::spawn(drain_skillify_extractions(tasks));
        error_ready_rx.await.expect("first task started");
        tokio::task::yield_now().await;
        release_tx
            .send(())
            .expect("failed sibling must not cancel the remaining extraction");
        let error = drain
            .await
            .expect("drain task joins")
            .expect_err("one failed chunk fails synthesis");

        assert!(error.contains("chunk 0"));
        assert!(sibling_completed.load(Ordering::Acquire));
    }

    #[test]
    fn chunk_source_packets_splits_large_packet() {
        let content = "a".repeat(SKILLIFY_CHUNK_CHAR_BUDGET + 10);
        let chunks = chunk_source_packets(&[packet("s1", &content)]).expect("chunks");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].packets[0].source_id, "s1");
        assert_eq!(chunks[1].packets[0].chunk_index, 1);
    }

    #[test]
    fn synthesis_prompt_keeps_parent_ownership_rule() {
        let prompt = skillify_synthesis_system_prompt();
        assert!(prompt.contains("parent Skillify agent"));
        assert!(prompt.contains("You own global synthesis"));
        assert!(prompt.contains("Do not preserve chunk boundaries"));
    }

    #[tokio::test]
    #[ignore = "requires live MatrixOne: run with ASTRA_TEST_DB_IT=1"]
    async fn skillify_provider_call_is_attributed_to_its_durable_harness_owner() {
        use axum::{Json, Router, routing::post};
        use sqlx::Row;

        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let mut settings = astra_core::MatrixOneSettings::from_env();
        settings.db_pool_max_connections = settings.db_pool_max_connections.min(4);
        settings.db_pool_min_connections = settings.db_pool_min_connections.min(1);
        let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
            .unwrap_or_else(|_| "mysql".to_string());
        astra_services::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure inference schema");
        let pool = astra_core::SharedPool::new(&settings)
            .await
            .expect("connect MatrixOne");
        let user_id = format!("skillify-user-{}", uuid::Uuid::new_v4().simple());
        let harness_run_id = format!("harness-run-{}", uuid::Uuid::new_v4().simple());
        sqlx::query(
            "INSERT INTO harness_runs
             (harness_run_id, harness_id, version_id, user_id, session_id, status,
              input_json, output_json, created_at, updated_at)
             VALUES (?, 'skillify', 'skillify.v1', ?, NULL, 'running', '{}', '{}', NOW(6), NOW(6))",
        )
        .bind(&harness_run_id)
        .bind(&user_id)
        .execute(pool.get())
        .await
        .expect("seed harness owner");

        let provider = Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                Json(json!({
                    "id": "provider-skillify",
                    "choices": [{
                        "message": {"role": "assistant", "content": "{\"signals\":[]}"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 11, "completion_tokens": 3}
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider");
        let provider_address = listener.local_addr().expect("provider address");
        let provider_task = tokio::spawn(async move {
            axum::serve(listener, provider)
                .await
                .expect("serve provider");
        });
        let admitted = astra_services::AdmittedModelExecution::from_endpoint(
            "offer-skillify".to_string(),
            "skillify-model".to_string(),
            "openai".to_string(),
            format!("http://{provider_address}/v1/chat/completions"),
            "Bearer test-key".to_string(),
            Some(2_000),
        );
        let execution = SkillifyInferenceExecution {
            ledger: DurableInferenceLedger::new(pool.clone(), &user_id, admitted.clone()),
            admitted,
        };

        let output = RuntimeSkillifyAgentExecutor::call_json_agent(
            &execution,
            astra_turn_types::InferenceInvocationScope::HarnessRun {
                harness_run_id: harness_run_id.clone(),
                operation_id: "skillify_extract".to_string(),
                logical_attempt: 0,
            },
            "Extract typed signals.",
            "Use this source.",
            256,
        )
        .await
        .expect("durable Skillify inference");
        assert_eq!(output, "{\"signals\":[]}");

        let row = sqlx::query(
            "SELECT r.scope_kind, r.session_id, r.run_id, r.harness_run_id, r.purpose,
                    i.status AS invocation_status, i.input_tokens, i.output_tokens,
                    a.status AS attempt_status
             FROM inference_invocations i
             JOIN inference_routes r
               ON r.user_id = i.user_id AND r.route_id = i.route_id
             JOIN inference_provider_attempts a
               ON a.user_id = i.user_id AND a.invocation_id = i.invocation_id
             WHERE i.user_id = ? AND i.harness_run_id = ? AND i.operation_id = ?",
        )
        .bind(&user_id)
        .bind(&harness_run_id)
        .bind("skillify_extract")
        .fetch_one(pool.get())
        .await
        .expect("load Skillify inference facts");
        assert_eq!(row.get::<String, _>("scope_kind"), "harness_run");
        assert_eq!(row.get::<Option<String>, _>("session_id"), None);
        assert_eq!(row.get::<Option<String>, _>("run_id"), None);
        assert_eq!(
            row.get::<Option<String>, _>("harness_run_id").as_deref(),
            Some(harness_run_id.as_str())
        );
        assert_eq!(row.get::<String, _>("purpose"), "skill_synthesis");
        assert_eq!(row.get::<String, _>("invocation_status"), "succeeded");
        assert_eq!(row.get::<String, _>("attempt_status"), "succeeded");
        assert_eq!(row.get::<i64, _>("input_tokens"), 11);
        assert_eq!(row.get::<i64, _>("output_tokens"), 3);

        for table in [
            "inference_invocation_settlement_debts",
            "inference_provider_attempts",
            "inference_invocations",
            "inference_routes",
        ] {
            let statement = format!("DELETE FROM {table} WHERE user_id = ? AND harness_run_id = ?");
            sqlx::query(&statement)
                .bind(&user_id)
                .bind(&harness_run_id)
                .execute(pool.get())
                .await
                .unwrap_or_else(|error| panic!("cleanup `{statement}`: {error}"));
        }
        sqlx::query("DELETE FROM harness_runs WHERE user_id = ? AND harness_run_id = ?")
            .bind(&user_id)
            .bind(&harness_run_id)
            .execute(pool.get())
            .await
            .expect("cleanup harness owner");
        provider_task.abort();
        assert!(
            provider_task
                .await
                .expect_err("provider should be cancelled")
                .is_cancelled()
        );
        pool.close().await;
    }
}
