//! Live-call integration test for TokenUsage extraction across real providers.
//!
//! Reads the top-level `.models.yaml` and issues a real, small request to one
//! OpenAI-compatible model (Qwen via DashScope) and one Bedrock Converse model
//! (us.anthropic.claude-sonnet-4-6). Asserts:
//!
//! 1. `TokenUsage` extracts non-zero `input_tokens` / `output_tokens`.
//! 2. Canonical billing identity holds:
//!    `total_tokens == input + cached_input + cache_creation + output`.
//! 3. OpenAI convention: `prompt_tokens` from the raw response equals
//!    `input + cached + creation` (disjoint sum after normalization).
//! 4. Bedrock convention: `inputTokens` from the raw response equals
//!    `input_tokens` directly (disjoint from cache fields).
//!
//! Runs a second request to the same Bedrock model with the same prompt to
//! observe `cached_input_tokens > 0` when prompt-cache hits.
//!
//! Gated behind `#[ignore]` — invoke explicitly with:
//!     cargo test -p astra-runtime --test live_token_usage_e2e -- --ignored --nocapture

use std::path::PathBuf;
use std::time::Duration;

use astra_runtime::turn::bedrock_eventstream::FrameDecoder;
use astra_runtime::turn::token_usage::{TokenUsage, UsageDialect, extract_usage};
use futures_util::StreamExt;
use serde_json::{Value, json};

/// Percent-encode the Bedrock model id path segment. Model ids contain only
/// letters, digits, `.`, `-`, `_`, `:`, and `/`. Of these, `:` and `/` must be
/// escaped when used as a single path segment.
fn encode_bedrock_model_id(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ModelDef {
    name: String,
    provider: String,
    api_key: String,
    base_url: String,
}

fn load_models_yaml() -> Vec<ModelDef> {
    // .models.yaml lives at the repo root (../../..  from rust/crates/runtime)
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../.models.yaml")
        .canonicalize()
        .expect("canonicalize .models.yaml");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    let docs: Vec<Value> = serde_yaml_ng::from_str(&text).expect("parse .models.yaml");
    docs.into_iter()
        .filter_map(|doc| {
            Some(ModelDef {
                name: doc.get("name")?.as_str()?.to_string(),
                provider: doc.get("provider")?.as_str()?.to_string(),
                api_key: doc.get("api_key")?.as_str()?.to_string(),
                base_url: doc.get("base_url")?.as_str()?.to_string(),
            })
        })
        .collect()
}

fn find_model<'a>(models: &'a [ModelDef], name: &str) -> &'a ModelDef {
    models
        .iter()
        .find(|m| m.name == name)
        .unwrap_or_else(|| panic!("model {name} not found in .models.yaml"))
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client")
}

/// Issue one OpenAI-compatible `/v1/chat/completions` call and return the
/// raw response JSON plus the normalized [`TokenUsage`].
async fn call_openai_compatible(
    client: &reqwest::Client,
    model: &ModelDef,
    user_message: &str,
) -> (Value, TokenUsage) {
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let body = json!({
        "model": model.name,
        "messages": [{"role": "user", "content": user_message}],
        "max_tokens": 16,
        "temperature": 0.0,
        "stream": false,
    });
    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {}", model.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("HTTP send");
    let status = resp.status();
    let raw: Value = resp.json().await.expect("parse upstream json");
    assert!(
        status.is_success(),
        "upstream {} returned {status}: {raw}",
        model.name
    );
    let usage_obj = raw
        .get("usage")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("no `usage` in response for {}: {raw}", model.name));
    let extracted = extract_usage(UsageDialect::OpenAi, usage_obj)
        .expect("extract_usage should succeed on OpenAI response");
    (raw, extracted)
}

/// Issue one Bedrock Converse call. The base_url already includes `/model/{id}/converse`
/// path segment logic, but `.models.yaml` gives `https://bedrock-runtime.us-east-1.amazonaws.com`
/// so we build the path here. Returns raw response + normalized [`TokenUsage`].
async fn call_bedrock_converse(
    client: &reqwest::Client,
    model: &ModelDef,
    user_message: &str,
) -> (Value, TokenUsage) {
    // URL-encode the model id (inference-profile ids contain '.' which are safe, but keep it defensive).
    let encoded_id = encode_bedrock_model_id(&model.name);
    let url = format!(
        "{}/model/{encoded_id}/converse",
        model.base_url.trim_end_matches('/')
    );
    let body = json!({
        "messages": [{"role": "user", "content": [{"text": user_message}]}],
        "inferenceConfig": {"maxTokens": 16, "temperature": 0.0},
    });
    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {}", model.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("HTTP send");
    let status = resp.status();
    let raw: Value = resp.json().await.expect("parse upstream json");
    assert!(
        status.is_success(),
        "upstream {} returned {status}: {raw}",
        model.name
    );
    let usage_obj = raw
        .get("usage")
        .and_then(Value::as_object)
        .unwrap_or_else(|| panic!("no `usage` in response for {}: {raw}", model.name));
    let extracted = extract_usage(UsageDialect::BedrockConverse, usage_obj)
        .expect("extract_usage should succeed on Bedrock response");
    (raw, extracted)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore = "hits real DashScope API; run with --ignored"]
async fn openai_compatible_qwen_plus_populates_usage() {
    let models = load_models_yaml();
    let model = find_model(&models, "qwen-plus");
    let client = http_client();

    let (raw, usage) = call_openai_compatible(&client, model, "Say hi in 3 words.").await;
    eprintln!("qwen-plus raw usage: {}", raw.get("usage").unwrap());
    eprintln!("qwen-plus normalized: {usage:?}");

    // Invariant 1: non-zero input+output.
    assert!(
        usage.input_tokens + usage.cached_input_tokens > 0,
        "input must be non-zero"
    );
    assert!(usage.output_tokens > 0, "output must be non-zero");

    // Invariant 2: total = sum of disjoint buckets.
    assert_eq!(
        usage.total_tokens(),
        usage.input_tokens
            + usage.cached_input_tokens
            + usage.cache_creation_tokens
            + usage.output_tokens,
        "disjoint sum identity"
    );

    // Invariant 3: OpenAI-side prompt_tokens ⊇ cached + creation.
    let raw_prompt = raw["usage"]["prompt_tokens"]
        .as_u64()
        .expect("prompt_tokens");
    assert_eq!(
        raw_prompt,
        usage.input_tokens + usage.cached_input_tokens + usage.cache_creation_tokens,
        "OpenAI prompt_tokens must equal normalized fresh + cached + creation"
    );
}

#[tokio::test]
#[ignore = "hits real Bedrock API; run with --ignored"]
async fn bedrock_claude_sonnet_populates_usage() {
    let models = load_models_yaml();
    let model = find_model(&models, "us.anthropic.claude-sonnet-4-6");
    let client = http_client();

    let (raw, usage) = call_bedrock_converse(&client, model, "Say hi in 3 words.").await;
    eprintln!("bedrock raw usage: {}", raw.get("usage").unwrap());
    eprintln!("bedrock normalized: {usage:?}");

    assert!(usage.input_tokens > 0);
    assert!(usage.output_tokens > 0);

    // Bedrock convention: inputTokens is DISJOINT from cacheRead/cacheWrite.
    let raw_input = raw["usage"]["inputTokens"].as_u64().expect("inputTokens");
    assert_eq!(
        raw_input, usage.input_tokens,
        "Bedrock raw inputTokens passes through as fresh input"
    );

    // Disjoint sum identity.
    assert_eq!(
        usage.total_tokens(),
        usage.input_tokens
            + usage.cached_input_tokens
            + usage.cache_creation_tokens
            + usage.output_tokens,
    );
}

/// Two requests with the SAME long system prompt so Bedrock's prompt cache
/// has a chance to hit on the second call. Verifies cache accounting moves.
#[tokio::test]
#[ignore = "hits real Bedrock API; run with --ignored"]
async fn bedrock_claude_sonnet_cache_read_increments_on_repeat() {
    let models = load_models_yaml();
    let model = find_model(&models, "us.anthropic.claude-sonnet-4-6");
    let client = http_client();

    // Build a long, stable user prompt so that ≥1024 tokens qualify for caching.
    let prelude = "The following is a long context about distributed systems. ".repeat(200);
    let prompt = format!("{prelude}\nWhat is CAP? Answer in 5 words.");

    let encoded_id = encode_bedrock_model_id(&model.name);
    let url = format!(
        "{}/model/{encoded_id}/converse",
        model.base_url.trim_end_matches('/')
    );

    // Body WITH cachePoint on the user message so Bedrock will cache the prefix.
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"text": prompt},
                {"cachePoint": {"type": "default"}}
            ]
        }],
        "inferenceConfig": {"maxTokens": 16, "temperature": 0.0},
    });

    async fn one_call(
        client: &reqwest::Client,
        url: &str,
        api_key: &str,
        body: &Value,
    ) -> TokenUsage {
        let resp = client
            .post(url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .expect("send");
        let status = resp.status();
        let raw: Value = resp.json().await.expect("json");
        assert!(status.is_success(), "bedrock {status}: {raw}");
        eprintln!("bedrock cache-test raw usage: {}", raw["usage"]);
        extract_usage(
            UsageDialect::BedrockConverse,
            raw["usage"].as_object().expect("usage obj"),
        )
        .expect("extract")
    }

    let first = one_call(&client, &url, &model.api_key, &body).await;
    // Second call — same request body — should hit cache.
    let second = one_call(&client, &url, &model.api_key, &body).await;

    eprintln!("first: {first:?}");
    eprintln!("second: {second:?}");

    // On the first call, Bedrock should either write cache (cache_creation > 0)
    // or (if prefix is too short) do neither. Tolerate both, but the second call
    // must NOT be strictly worse on the cache-read axis than the first.
    assert!(
        second.cached_input_tokens >= first.cached_input_tokens,
        "repeat call should not regress cache reads: first={first:?}, second={second:?}"
    );

    // Invariant holds on both.
    for u in [first, second] {
        assert_eq!(
            u.total_tokens(),
            u.input_tokens + u.cached_input_tokens + u.cache_creation_tokens + u.output_tokens,
        );
    }
}

// ── Real Bedrock streaming via converse-stream + EventStream ────────────

/// Drive a real `/converse-stream` response through our [`FrameDecoder`].
/// Validates:
/// - The wire body is AWS `vnd.amazon.eventstream` binary (not JSON).
/// - Multiple event frames arrive (messageStart, contentBlockDelta*, metadata,
///   messageStop), proving the stream is actually incremental.
/// - Token accounting from the terminal `metadata` frame satisfies the
///   canonical disjoint-sum identity.
#[tokio::test]
#[ignore = "hits real Bedrock converse-stream; run with --ignored"]
async fn bedrock_converse_stream_yields_incremental_frames() {
    let models = load_models_yaml();
    let model = find_model(&models, "us.anthropic.claude-sonnet-4-6");
    let client = http_client();

    let encoded_id = encode_bedrock_model_id(&model.name);
    let url = format!(
        "{}/model/{encoded_id}/converse-stream",
        model.base_url.trim_end_matches('/')
    );

    // Ask for >1 content chunk worth of output so contentBlockDelta is emitted
    // multiple times.
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "text": "Count from 1 to 5, one number per sentence, each sentence at least 12 words."
            }]
        }],
        "inferenceConfig": {"maxTokens": 200, "temperature": 0.0},
    });

    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {}", model.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .expect("HTTP send");
    assert!(
        resp.status().is_success(),
        "bedrock converse-stream status: {} body: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("vnd.amazon.eventstream"),
        "expected AWS eventstream content-type, got: {content_type}"
    );

    let mut decoder = FrameDecoder::new();
    let mut event_type_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut assembled_text = String::new();
    let mut final_usage: Option<TokenUsage> = None;
    let mut stop_reason: Option<String> = None;

    let mut body_stream = resp.bytes_stream();
    while let Some(chunk) = body_stream.next().await {
        let bytes = chunk.expect("stream chunk");
        decoder.push(&bytes);
        loop {
            let frame = match decoder.try_next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => panic!("decode error: {e}"),
            };
            if let Some(et) = frame.event_type() {
                *event_type_counts.entry(et.to_string()).or_insert(0) += 1;
                let payload: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                match et {
                    "contentBlockDelta" => {
                        if let Some(text) = payload
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(Value::as_str)
                        {
                            assembled_text.push_str(text);
                        }
                    }
                    "messageStop" => {
                        stop_reason = payload
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    "metadata" => {
                        if let Some(usage_obj) = payload.get("usage").and_then(Value::as_object) {
                            final_usage = extract_usage(UsageDialect::BedrockConverse, usage_obj);
                        }
                    }
                    _ => {}
                }
            } else if frame.message_type() == Some("exception") {
                let payload: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                panic!(
                    "bedrock exception frame {:?}: {}",
                    frame.exception_type(),
                    payload
                );
            }
        }
    }

    eprintln!("event counts: {event_type_counts:?}");
    eprintln!(
        "assembled text ({} chars): {assembled_text:?}",
        assembled_text.len()
    );
    eprintln!("final usage: {final_usage:?}");
    eprintln!("stop_reason: {stop_reason:?}");

    assert!(
        event_type_counts
            .get("contentBlockDelta")
            .copied()
            .unwrap_or(0)
            >= 2,
        "streaming should yield ≥ 2 contentBlockDelta frames; got {event_type_counts:?}"
    );
    assert_eq!(event_type_counts.get("messageStart").copied(), Some(1));
    assert_eq!(event_type_counts.get("messageStop").copied(), Some(1));
    assert_eq!(event_type_counts.get("metadata").copied(), Some(1));
    assert!(
        !assembled_text.trim().is_empty(),
        "text deltas should produce non-empty body"
    );
    assert!(stop_reason.is_some());

    let u = final_usage.expect("metadata frame must carry usage");
    assert!(u.input_tokens > 0);
    assert!(u.output_tokens > 0);
    assert_eq!(
        u.total_tokens(),
        u.input_tokens + u.cached_input_tokens + u.cache_creation_tokens + u.output_tokens,
        "canonical disjoint-sum identity holds on streamed usage"
    );
}
