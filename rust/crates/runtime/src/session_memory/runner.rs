use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use astra_services::session_journal::{
    SessionMemoryExtractionErrorReason, SessionMemoryExtractionSource,
};
use astra_turn_core::cloud_session_memory_extract::{
    build_extraction_prompt, SESSION_MEMORY_TEMPLATE,
};

use crate::memory_relevance::LlmConnParams;
use crate::turn::cloud::memoria_compact::MemoriaClient;
use crate::turn::llm_client::{
    apply_provider_auth, build_provider_request_body, llm_request_url_for_provider,
    parse_nonstream_response_for_provider,
};

pub const SESSION_MEMORY_PREFIX: &str = "[@session/memory]";

/// What the worker produced.
pub enum ExtractionArtifacts {
    Persisted {
        source: SessionMemoryExtractionSource,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
    },
    LlmFailedPersistedFallback {
        error_reason: SessionMemoryExtractionErrorReason,
        bytes_written: u64,
        store_attempt: u32,
        content: String,
    },
    PersistFailed {
        error_reason: SessionMemoryExtractionErrorReason,
    },
}

#[allow(clippy::too_many_arguments)]
pub async fn run_extraction(
    memoria: &Arc<dyn MemoriaClient>,
    session_id: &str,
    messages: &[Value],
    turn_number: usize,
    current_tokens: usize,
    current_memory: &str,
    memory_model_params: Option<&LlmConnParams>,
    llm_timeout: Duration,
    max_output_tokens: usize,
) -> ExtractionArtifacts {
    let base_memory = if current_memory.trim().is_empty() {
        SESSION_MEMORY_TEMPLATE.to_string()
    } else {
        current_memory.to_string()
    };
    let fallback = build_rule_fallback_memory(&base_memory, messages, turn_number, current_tokens);

    let Some(params) = memory_model_params else {
        return match store_session_memory(memoria, session_id, &fallback).await {
            Ok((bytes_written, store_attempt)) => ExtractionArtifacts::Persisted {
                source: SessionMemoryExtractionSource::RuleFallback,
                bytes_written,
                store_attempt,
                content: fallback,
            },
            Err(error_reason) => ExtractionArtifacts::PersistFailed { error_reason },
        };
    };

    match update_memory_with_llm(
        &base_memory,
        messages,
        params,
        llm_timeout,
        max_output_tokens,
    )
    .await
    {
        Ok(updated) => match store_session_memory(memoria, session_id, &updated).await {
            Ok((bytes_written, store_attempt)) => ExtractionArtifacts::Persisted {
                source: SessionMemoryExtractionSource::Llm,
                bytes_written,
                store_attempt,
                content: updated,
            },
            Err(error_reason) => ExtractionArtifacts::PersistFailed { error_reason },
        },
        Err(error_reason) => match store_session_memory(memoria, session_id, &fallback).await {
            Ok((bytes_written, store_attempt)) => ExtractionArtifacts::LlmFailedPersistedFallback {
                error_reason,
                bytes_written,
                store_attempt,
                content: fallback,
            },
            Err(store_error) => ExtractionArtifacts::PersistFailed {
                error_reason: store_error,
            },
        },
    }
}

pub fn encode_session_memory_entry(session_id: &str, content: &str) -> String {
    format!(
        "{SESSION_MEMORY_PREFIX}\nsession_id={session_id}\n\n{}",
        content.trim()
    )
}

pub fn decode_session_memory_entry(raw: &str, session_id: &str) -> Option<String> {
    let trimmed = raw.trim();
    if !trimmed.starts_with(SESSION_MEMORY_PREFIX) {
        return None;
    }
    let rest = trimmed[SESSION_MEMORY_PREFIX.len()..].trim_start();
    let sid_line = rest.lines().next()?.trim();
    let encoded_sid = sid_line.strip_prefix("session_id=")?.trim();
    if encoded_sid != session_id {
        return None;
    }
    let content = rest[sid_line.len()..].trim();
    (!content.is_empty()).then(|| content.to_string())
}

async fn update_memory_with_llm(
    current_memory: &str,
    messages: &[Value],
    params: &LlmConnParams,
    llm_timeout: Duration,
    max_output_tokens: usize,
) -> Result<String, SessionMemoryExtractionErrorReason> {
    let prompt = build_extraction_prompt(current_memory, messages);
    let body = build_provider_request_body(
        &prompt,
        &[],
        &params.model_name,
        &params.provider,
        Some(max_output_tokens),
        Some(0.0),
        false,
        &astra_turn_core::thinking_config::ThinkingConfig::Off,
    );
    let url = llm_request_url_for_provider(
        &params.base_url,
        &params.provider,
        &params.model_name,
        false,
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(llm_timeout)
        .build()
        .map_err(|_| SessionMemoryExtractionErrorReason::LlmError)?;
    let request = client.post(url).header("content-type", "application/json");
    let request = apply_provider_auth(request, &params.provider, &params.api_key, None).json(&body);

    let response = match tokio::time::timeout(llm_timeout, request.send()).await {
        Ok(Ok(resp)) => resp,
        Ok(Err(error)) if error.is_timeout() => {
            return Err(SessionMemoryExtractionErrorReason::LlmTimeout);
        }
        Ok(Err(_)) => return Err(SessionMemoryExtractionErrorReason::LlmError),
        Err(_) => return Err(SessionMemoryExtractionErrorReason::LlmTimeout),
    };

    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .map_err(|_| SessionMemoryExtractionErrorReason::LlmError)?;
    if !status.is_success() {
        return Err(SessionMemoryExtractionErrorReason::LlmError);
    }

    let parsed = parse_nonstream_response_for_provider(
        &payload,
        &params.provider,
        &params.model_name,
        Instant::now(),
    );
    let content = parsed.full_text.trim();
    if content.is_empty() {
        return Err(SessionMemoryExtractionErrorReason::EmptyResponse);
    }
    Ok(content.to_string())
}

async fn store_session_memory(
    memoria: &Arc<dyn MemoriaClient>,
    session_id: &str,
    content: &str,
) -> Result<(u64, u32), SessionMemoryExtractionErrorReason> {
    let encoded = encode_session_memory_entry(session_id, content);
    for attempt in 1..=2 {
        match memoria
            .store(&encoded, "working", Some(session_id), Some("T3"))
            .await
        {
            Ok(_) => return Ok((encoded.len() as u64, attempt)),
            Err(_) if attempt == 1 => continue,
            Err(_) => return Err(SessionMemoryExtractionErrorReason::WriteFailed),
        }
    }
    Err(SessionMemoryExtractionErrorReason::WriteFailed)
}

fn build_rule_fallback_memory(
    current_memory: &str,
    messages: &[Value],
    turn_number: usize,
    current_tokens: usize,
) -> String {
    let recent = render_recent_messages(messages, 8, 240);
    let first_user = first_user_message(messages).unwrap_or("Current session");
    let last_user = last_user_message(messages).unwrap_or(first_user);
    let last_assistant = last_assistant_message(messages).unwrap_or("No assistant summary yet.");
    let errors = collect_error_lines(messages);
    format!(
        "# Session Memory\n\n## Session Title\n{first_user}\n\n## Active Goals\n- {last_user}\n\n## Pending Todos\n- Continue the current session task.\n\n## Completed\n- Session memory updated through rule fallback.\n\n## Current State\n- Turn {turn_number}\n- Approximate context size: {current_tokens} tokens\n- Latest assistant state: {last_assistant}\n\n## Task Specification\n{first_user}\n\n## Files and Functions\n{files}\n\n## Workflow\n{workflow}\n\n## Errors & Corrections\n{errors}\n\n## Learnings\n- Keep session memory synchronized when the session grows.\n\n## Worklog\n{worklog}",
        files = detect_file_mentions(messages),
        workflow = if recent.is_empty() {
            "- No recent conversation captured.".to_string()
        } else {
            recent
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        errors = if errors.is_empty() {
            "- No explicit errors captured.".to_string()
        } else {
            errors
                .iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        },
        worklog = if current_memory.trim().is_empty() {
            "- Initialized session memory document.".to_string()
        } else {
            format!(
                "- Refreshed existing session memory.\n- {}",
                recent
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "No recent conversation captured.".to_string())
            )
        },
    )
}

fn render_recent_messages(messages: &[Value], take: usize, max_len: usize) -> Vec<String> {
    messages
        .iter()
        .rev()
        .filter_map(|msg| {
            let role = msg.get("role").and_then(Value::as_str)?;
            match role {
                "user" | "assistant" => {
                    let text = message_text(msg)?;
                    Some(format!("{role}: {}", truncate(&text, max_len)))
                }
                _ => None,
            }
        })
        .take(take)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn first_user_message(messages: &[Value]) -> Option<&str> {
    messages.iter().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("user"))
            .then(|| message_text(msg))
            .flatten()
    })
}

fn last_user_message(messages: &[Value]) -> Option<&str> {
    messages.iter().rev().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("user"))
            .then(|| message_text(msg))
            .flatten()
    })
}

fn last_assistant_message(messages: &[Value]) -> Option<&str> {
    messages.iter().rev().find_map(|msg| {
        (msg.get("role").and_then(Value::as_str) == Some("assistant"))
            .then(|| message_text(msg))
            .flatten()
    })
}

fn collect_error_lines(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|msg| {
            let text = message_text(msg)?;
            let lower = text.to_ascii_lowercase();
            (lower.contains("error") || lower.contains("fail") || lower.contains("panic"))
                .then(|| truncate(text, 200))
        })
        .map(ToString::to_string)
        .collect()
}

fn detect_file_mentions(messages: &[Value]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    for msg in messages {
        let Some(text) = message_text(msg) else {
            continue;
        };
        for token in text.split_whitespace() {
            let cleaned = token.trim_matches(|c: char| {
                matches!(c, ',' | '.' | ':' | ';' | '(' | ')' | '[' | ']' | '{' | '}')
            });
            if cleaned.contains('/')
                || cleaned.ends_with(".rs")
                || cleaned.ends_with(".ts")
                || cleaned.ends_with(".tsx")
                || cleaned.ends_with(".py")
            {
                seen.insert(cleaned.to_string());
            }
        }
    }
    if seen.is_empty() {
        "- No file paths referenced.".to_string()
    } else {
        seen.into_iter()
            .take(8)
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn message_text(msg: &Value) -> Option<&str> {
    msg.get("content").and_then(Value::as_str)
}

fn truncate(text: &str, max_chars: usize) -> &str {
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut end = 0usize;
    for (count, (idx, ch)) in text.char_indices().enumerate() {
        if count == max_chars {
            break;
        }
        end = idx + ch.len_utf8();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::turn::cloud::memoria_compact::MemoriaMemory;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct CapturingMemoria {
        stored: Mutex<Vec<(String, String, Option<String>)>>,
    }

    #[async_trait::async_trait]
    impl MemoriaClient for CapturingMemoria {
        async fn retrieve_ext(
            &self,
            _query: &str,
            _session_id: Option<&str>,
            _top_k: usize,
            _filter_session: bool,
        ) -> Result<Vec<MemoriaMemory>, String> {
            Ok(Vec::new())
        }

        async fn store(
            &self,
            content: &str,
            memory_type: &str,
            session_id: Option<&str>,
            _trust_tier: Option<&str>,
        ) -> Result<String, String> {
            self.stored.lock().unwrap().push((
                content.to_string(),
                memory_type.to_string(),
                session_id.map(str::to_string),
            ));
            Ok("mem-1".to_string())
        }

        async fn purge_working(&self, _session_id: &str) -> Result<u64, String> {
            Ok(0)
        }
    }

    async fn spawn_json_server(
        assert_request: Arc<dyn Fn(&str) + Send + Sync>,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body_text = body.to_string();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 32 * 1024];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            assert_request(&request);
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body_text.len(),
                body_text
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        (format!("http://{addr}"), handle)
    }

    fn sample_messages() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "Fix rust/crates/runtime/src/session_memory/runner.rs"}),
            json!({"role": "assistant", "content": "Investigating the session memory runner."}),
        ]
    }

    #[tokio::test]
    async fn run_extraction_without_selector_persists_rule_fallback() {
        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let artifacts = run_extraction(
            &memoria,
            "sess-1",
            &sample_messages(),
            3,
            12_345,
            "",
            None,
            Duration::from_secs(3),
            256,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::RuleFallback);
                assert!(content.contains("# Session Memory"));
            }
            _ => panic!("expected fallback persistence"),
        }
    }

    #[tokio::test]
    async fn run_extraction_openai_selector_persists_llm_content() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/chat/completions HTTP/1.1"));
                assert!(request.contains("authorization: Bearer test-key"));
            }),
            json!({
                "choices": [{
                    "message": {
                        "content": "# Session Memory\n\n## Session Title\nLLM Result"
                    }
                }]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: format!("{server_url}/v1"),
            api_key: "test-key".to_string(),
            model_name: "selector-openai".to_string(),
            provider: "openai".to_string(),
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-openai",
            &sample_messages(),
            1,
            20_000,
            "",
            Some(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted {
                source, content, ..
            } => {
                assert_eq!(source, SessionMemoryExtractionSource::Llm);
                assert!(content.contains("LLM Result"));
            }
            _ => panic!("expected llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_anthropic_selector_uses_native_endpoint() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /v1/messages HTTP/1.1"));
                assert!(request.contains("x-api-key: anthropic-key"));
                assert!(request.contains("anthropic-version: 2023-06-01"));
            }),
            json!({
                "content": [
                    { "type": "text", "text": "# Session Memory\n\n## Session Title\nAnthropic Result" }
                ]
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: server_url,
            api_key: "anthropic-key".to_string(),
            model_name: "selector-anthropic".to_string(),
            provider: "anthropic".to_string(),
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-anthropic",
            &sample_messages(),
            1,
            20_000,
            "",
            Some(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted { content, .. } => {
                assert!(content.contains("Anthropic Result"));
            }
            _ => panic!("expected anthropic llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[tokio::test]
    async fn run_extraction_bedrock_selector_uses_converse_endpoint() {
        let (server_url, server_handle) = spawn_json_server(
            Arc::new(|request: &str| {
                assert!(request.starts_with("POST /model/anthropic.claude/converse HTTP/1.1"));
                assert!(request.contains("authorization: Bearer bedrock-key"));
            }),
            json!({
                "output": {
                    "message": {
                        "content": [
                            { "text": "# Session Memory\n\n## Session Title\nBedrock Result" }
                        ]
                    }
                }
            }),
        )
        .await;

        let memoria = Arc::new(CapturingMemoria::default()) as Arc<dyn MemoriaClient>;
        let params = LlmConnParams {
            base_url: server_url,
            api_key: "bedrock-key".to_string(),
            model_name: "anthropic.claude".to_string(),
            provider: "bedrock".to_string(),
        };
        let artifacts = run_extraction(
            &memoria,
            "sess-bedrock",
            &sample_messages(),
            1,
            20_000,
            "",
            Some(&params),
            Duration::from_secs(3),
            512,
        )
        .await;

        match artifacts {
            ExtractionArtifacts::Persisted { content, .. } => {
                assert!(content.contains("Bedrock Result"));
            }
            _ => panic!("expected bedrock llm persistence"),
        }
        server_handle.await.unwrap();
    }

    #[test]
    fn session_memory_entry_roundtrips() {
        let encoded = encode_session_memory_entry("sess-42", "# Session Memory\n\nhello");
        let decoded = decode_session_memory_entry(&encoded, "sess-42").unwrap();
        assert_eq!(decoded, "# Session Memory\n\nhello");
        assert!(decode_session_memory_entry(&encoded, "other").is_none());
    }
}
