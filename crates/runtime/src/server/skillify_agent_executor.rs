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

use crate::turn::llm::client::{LlmCall, LlmCancel, LlmExecutionRoute, call_llm_and_collect};

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

    async fn call_json_agent(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        max_output_tokens: usize,
    ) -> Result<String, String> {
        let resolved = astra_services::resolve_reasoning_model(
            &self.matrixone,
            &self.encryptor,
            self.admin_config_service.as_ref(),
            Some(self.pool.get()),
        )
        .await
        .map_err(|error| format!("model resolution failed: {error}"))?;

        let messages = vec![
            json!({"role": "system", "content": system_prompt}),
            json!({"role": "user", "content": user_prompt}),
        ];
        let result = call_llm_and_collect(
            LlmCall {
                purpose: astra_turn_types::InferencePurpose::SubAgent,
                messages: &messages,
                tools: &[],
                route: LlmExecutionRoute {
                    model_name: &resolved.model_name,
                    wire_model_name: resolved.wire_model_name.as_deref(),
                    api_key: &resolved.api_key,
                    base_url: &resolved.base_url,
                    provider: &resolved.provider,
                    header_overrides: None,
                    request_body_overrides: resolved.request_body_overrides.as_ref(),
                    completions_url_override: None,
                    request_timeout: None,
                },
                max_output_tokens: Some(max_output_tokens),
                temperature: None,
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            LlmCancel::None,
        )
        .await
        .map_err(|error| format!("LLM call failed: {error}"))?;

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
        let chunks = chunk_source_packets(&request.source_packets)?;
        let use_subagents = chunks.len() > 1;
        let mut extraction_results = Vec::with_capacity(chunks.len());

        if use_subagents {
            let mut tasks = JoinSet::new();
            for chunk in chunks {
                let executor = self.clone();
                let req = request.clone();
                tasks.spawn(async move {
                    let index = chunk.index;
                    let output = executor.extract_chunk(&req, &chunk).await;
                    (index, output)
                });
            }
            while let Some(joined) = tasks.join_next().await {
                let (index, output) = joined
                    .map_err(|error| format!("Skillify extraction task panicked: {error}"))?;
                extraction_results.push((index, output?));
            }
            extraction_results.sort_by_key(|(index, _)| *index);
        } else {
            for chunk in chunks {
                extraction_results.push((chunk.index, self.extract_chunk(&request, &chunk).await?));
            }
        }

        let extractions = extraction_results
            .into_iter()
            .map(|(_, output)| output)
            .collect::<Vec<_>>();
        let synthesis = self.synthesize_parent(&request, &extractions).await?;

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

impl RuntimeSkillifyAgentExecutor {
    async fn extract_chunk(
        &self,
        request: &SkillifyAgentRequest,
        chunk: &SourceChunk,
    ) -> Result<ExtractionResponse, String> {
        let system_prompt = skillify_extraction_system_prompt();
        let user_prompt = skillify_extraction_user_prompt(request, chunk)?;
        let response = self
            .call_json_agent(
                system_prompt,
                &user_prompt,
                SKILLIFY_EXTRACTION_OUTPUT_TOKENS,
            )
            .await?;
        parse_json_response::<ExtractionResponse>(&response)
    }

    async fn synthesize_parent(
        &self,
        request: &SkillifyAgentRequest,
        extractions: &[ExtractionResponse],
    ) -> Result<SynthesisResponse, String> {
        let system_prompt = skillify_synthesis_system_prompt();
        let user_prompt = skillify_synthesis_user_prompt(request, extractions)?;
        let response = self
            .call_json_agent(
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
struct ExtractionResponse {
    signals: Vec<ExtractedSignal>,
    conflicts: Vec<SkillifyConflict>,
    dropped_signals: Vec<DroppedSignal>,
    source_summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExtractedSignal {
    signal_type: String,
    statement: String,
    rationale: String,
    confidence: f64,
    citations: Vec<ExtractedCitation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ExtractedCitation {
    source_id: String,
    source_excerpt: String,
    source_locator_json: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SkillifyConflict {
    summary: String,
    source_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct DroppedSignal {
    summary: String,
    reason: String,
    source_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
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
    let trimmed = text.trim();
    let json_text = strip_json_fence(trimmed);
    let start = json_text
        .find('{')
        .ok_or_else(|| "response did not contain a JSON object".to_string())?;
    let end = json_text
        .rfind('}')
        .ok_or_else(|| "response did not contain a complete JSON object".to_string())?;
    serde_json::from_str(&json_text[start..=end])
        .map_err(|error| format!("failed to parse JSON response: {error}"))
}

fn strip_json_fence(text: &str) -> &str {
    let trimmed = text.trim();
    if let Some(rest) = trimmed.strip_prefix("```json") {
        return rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        return rest.strip_suffix("```").unwrap_or(rest).trim();
    }
    trimmed
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
    fn parse_json_response_accepts_fenced_json() {
        let parsed: SynthesisResponse =
            parse_json_response(r#"```json{"drafts":[]}```"#).expect("parse");
        assert!(parsed.drafts.is_empty());
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
}
