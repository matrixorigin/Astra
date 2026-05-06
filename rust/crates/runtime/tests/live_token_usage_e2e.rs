//! Live-call integration tests against real LLM providers.
//!
//! Philosophy: **enumerate, don't hard-code**. Model availability changes
//! frequently (keys rotate, models retire, new providers get added). Tests
//! read `.models.yaml`, group by `provider`, pick one model per provider at
//! runtime, and exercise the invariants. What's present gets tested, what's
//! missing is skipped — so a session trace regression on any configured
//! provider would catch it without needing a test-code change to pin a
//! specific model.
//!
//! # Invariants exercised
//!
//! Every provider (and in the streaming/cache subtests, only providers whose
//! dialect supports them) must satisfy:
//!
//! 1. `TokenUsage` extracts non-zero `input_tokens` + `output_tokens`.
//! 2. Disjoint-sum identity:
//!    `total_tokens == input + cached_input + cache_creation + output`
//! 3. Shape-specific check:
//!    - OpenAI-family (inclusive shape): raw `prompt_tokens == input + cached + creation`
//!    - Bedrock Converse (disjoint shape): raw `inputTokens == input_tokens`
//!
//! # Bedrock-only regression guards
//!
//! - Repeated identical prompt must not regress cache_read counts.
//! - `converse-stream` must yield `metadata` AFTER `messageStop` and the
//!   transport layer must drain to EOS (regression guard for the token=0
//!   Bedrock bug where the loop broke on `is_finished()`).
//!
//! # How to run
//!
//! ```sh
//! make test-live-llm        # Makefile target — only this file
//! cargo test -p astra-runtime --test live_token_usage_e2e -- --ignored --nocapture
//! ```
//!
//! Missing `.models.yaml`, no matching provider, or a non-2xx response on a
//! specific model results in a **per-model skip** (eprintln + return) rather
//! than a hard failure, so a broken API key in one model doesn't mask real
//! regressions in another.

use std::path::PathBuf;
use std::time::Duration;

use astra_runtime::turn::bedrock_eventstream::FrameDecoder;
use astra_runtime::turn::token_usage::{TokenUsage, UsageDialect, extract_usage};
use futures_util::StreamExt;
use serde_json::{Value, json};

// ── Opt-in env gate ──────────────────────────────────────────────────────────

/// Live tests cost money + time and depend on external provider availability.
/// Even though `#[ignore]` already keeps them out of normal `cargo test`, the
/// `make test-online` DB sweep runs `cargo test -- --ignored` which would pull
/// them in. Guard with a dedicated env var so the only way they execute is:
///
///   - `make test-live-llm` (sets the var for you), OR
///   - `ASTRA_LIVE_LLM=1 cargo test ... --ignored`
///
/// Returns `true` when the suite should SKIP (var not set). Prints a hint
/// so operators know the test is intentionally bypassed.
fn skip_if_not_opted_in(test_name: &str) -> bool {
    if std::env::var("ASTRA_LIVE_LLM").ok().as_deref() == Some("1") {
        return false;
    }
    eprintln!(
        "SKIP [{test_name}]: live-LLM suite gated behind ASTRA_LIVE_LLM=1 — \
         run `make test-live-llm` to execute"
    );
    true
}

// ── Model enumeration ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct ModelDef {
    name: String,
    provider: String,
    api_key: String,
    base_url: String,
}

/// Read `.models.yaml` from the repo root. Returns an empty vec if the file
/// is missing — tests should then soft-skip rather than panic.
fn load_models_yaml() -> Vec<ModelDef> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.models.yaml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("SKIP: cannot read .models.yaml at {path:?}: {e}");
            return Vec::new();
        }
    };
    let docs: Vec<Value> = match serde_yaml_ng::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("SKIP: cannot parse .models.yaml: {e}");
            return Vec::new();
        }
    };
    docs.into_iter()
        .filter_map(|doc| {
            Some(ModelDef {
                name: doc.get("name")?.as_str()?.to_string(),
                provider: doc.get("provider")?.as_str()?.to_string(),
                api_key: doc.get("api_key")?.as_str()?.to_string(),
                base_url: doc.get("base_url")?.as_str()?.to_string(),
            })
        })
        .filter(|m| !m.api_key.is_empty() && !m.base_url.is_empty())
        .collect()
}

/// One model per distinct provider — the first well-formed entry wins.
/// Stable ordering so logs are comparable across runs.
fn one_per_provider(models: &[ModelDef]) -> Vec<ModelDef> {
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<ModelDef> = Vec::new();
    for m in models {
        if seen.insert(m.provider.clone()) {
            out.push(m.clone());
        }
    }
    out
}

/// All models for a given provider (order preserved from yaml).
fn models_for_provider<'a>(models: &'a [ModelDef], provider: &str) -> Vec<&'a ModelDef> {
    models.iter().filter(|m| m.provider == provider).collect()
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("reqwest client")
}

/// URL-encode a Bedrock model id path segment.
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

// ── Shared call helpers ──────────────────────────────────────────────────────

/// Result of one live call. `None` means a soft skip (non-2xx, missing usage).
struct LiveResult {
    raw_usage: Value,
    normalized: TokenUsage,
}

async fn call_openai_compatible(
    client: &reqwest::Client,
    model: &ModelDef,
    user_message: &str,
) -> Option<LiveResult> {
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
    let body = json!({
        "model": model.name,
        "messages": [{"role": "user", "content": user_message}],
        "max_tokens": 16,
        "temperature": 0.0,
        "stream": false,
    });
    let resp = match client
        .post(&url)
        .header("authorization", format!("Bearer {}", model.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "SKIP [{}/{}]: HTTP send failed: {e}",
                model.provider, model.name
            );
            return None;
        }
    };
    let status = resp.status();
    let raw: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "SKIP [{}/{}]: json parse failed: {e}",
                model.provider, model.name
            );
            return None;
        }
    };
    if !status.is_success() {
        eprintln!(
            "SKIP [{}/{}]: upstream {status}: {raw}",
            model.provider, model.name
        );
        return None;
    }
    let Some(usage_obj) = raw.get("usage").and_then(Value::as_object) else {
        eprintln!(
            "SKIP [{}/{}]: response missing `usage` object: {raw}",
            model.provider, model.name
        );
        return None;
    };
    let Some(normalized) = extract_usage(UsageDialect::OpenAi, usage_obj) else {
        eprintln!(
            "SKIP [{}/{}]: extract_usage returned None for: {raw}",
            model.provider, model.name
        );
        return None;
    };
    Some(LiveResult {
        raw_usage: raw.get("usage").cloned().unwrap_or(Value::Null),
        normalized,
    })
}

async fn call_bedrock_converse(
    client: &reqwest::Client,
    model: &ModelDef,
    user_message: &str,
) -> Option<LiveResult> {
    let encoded_id = encode_bedrock_model_id(&model.name);
    let url = format!(
        "{}/model/{encoded_id}/converse",
        model.base_url.trim_end_matches('/')
    );
    let body = json!({
        "messages": [{"role": "user", "content": [{"text": user_message}]}],
        "inferenceConfig": {"maxTokens": 16, "temperature": 0.0},
    });
    let resp = match client
        .post(&url)
        .header("authorization", format!("Bearer {}", model.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "SKIP [{}/{}]: HTTP send failed: {e}",
                model.provider, model.name
            );
            return None;
        }
    };
    let status = resp.status();
    let raw: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "SKIP [{}/{}]: json parse failed: {e}",
                model.provider, model.name
            );
            return None;
        }
    };
    if !status.is_success() {
        eprintln!(
            "SKIP [{}/{}]: upstream {status}: {raw}",
            model.provider, model.name
        );
        return None;
    }
    let Some(usage_obj) = raw.get("usage").and_then(Value::as_object) else {
        eprintln!(
            "SKIP [{}/{}]: response missing `usage` object: {raw}",
            model.provider, model.name
        );
        return None;
    };
    let Some(normalized) = extract_usage(UsageDialect::BedrockConverse, usage_obj) else {
        eprintln!(
            "SKIP [{}/{}]: extract_usage returned None for: {raw}",
            model.provider, model.name
        );
        return None;
    };
    Some(LiveResult {
        raw_usage: raw.get("usage").cloned().unwrap_or(Value::Null),
        normalized,
    })
}

async fn call_anthropic_messages(
    client: &reqwest::Client,
    model: &ModelDef,
    user_message: &str,
) -> Option<LiveResult> {
    let base = model.base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    };
    let body = json!({
        "model": model.name,
        "messages": [{"role": "user", "content": user_message}],
        "max_tokens": 16,
        "temperature": 0.0,
        "stream": false,
    });
    let resp = match client
        .post(&url)
        .header("x-api-key", &model.api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!(
                "SKIP [{}/{}]: HTTP send failed: {e}",
                model.provider, model.name
            );
            return None;
        }
    };
    let status = resp.status();
    let raw: Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            eprintln!(
                "SKIP [{}/{}]: json parse failed: {e}",
                model.provider, model.name
            );
            return None;
        }
    };
    if !status.is_success() {
        eprintln!(
            "SKIP [{}/{}]: upstream {status}: {raw}",
            model.provider, model.name
        );
        return None;
    }
    let Some(usage_obj) = raw.get("usage").and_then(Value::as_object) else {
        eprintln!(
            "SKIP [{}/{}]: response missing `usage` object: {raw}",
            model.provider, model.name
        );
        return None;
    };
    let Some(normalized) = extract_usage(UsageDialect::AnthropicMessages, usage_obj) else {
        eprintln!(
            "SKIP [{}/{}]: extract_usage returned None for: {raw}",
            model.provider, model.name
        );
        return None;
    };
    Some(LiveResult {
        raw_usage: raw.get("usage").cloned().unwrap_or(Value::Null),
        normalized,
    })
}

// ── Shared invariant assertions ──────────────────────────────────────────────

fn assert_disjoint_sum_identity(u: &TokenUsage, tag: &str) {
    assert_eq!(
        u.total_tokens(),
        u.input_tokens + u.cached_input_tokens + u.cache_creation_tokens + u.output_tokens,
        "{tag}: disjoint-sum identity broken: {u:?}"
    );
}

fn assert_nonzero_input_and_output(u: &TokenUsage, tag: &str) {
    assert!(
        u.input_tokens + u.cached_input_tokens > 0,
        "{tag}: expected non-zero total input, got {u:?}"
    );
    assert!(
        u.output_tokens > 0,
        "{tag}: expected non-zero output, got {u:?}"
    );
}

/// OpenAI-family inclusive shape: raw `prompt_tokens ⊇ cached + creation`, so
/// `fresh = prompt_tokens - cached - creation`, and
/// `fresh + cached + creation == prompt_tokens`.
fn assert_openai_inclusive_identity(raw_usage: &Value, u: &TokenUsage, tag: &str) {
    let Some(raw_prompt) = raw_usage.get("prompt_tokens").and_then(Value::as_u64) else {
        eprintln!("{tag}: raw_usage has no prompt_tokens — skipping inclusive identity check");
        return;
    };
    assert_eq!(
        raw_prompt,
        u.input_tokens + u.cached_input_tokens + u.cache_creation_tokens,
        "{tag}: OpenAI inclusive identity broken: raw prompt_tokens={raw_prompt}, normalized={u:?}"
    );
}

fn assert_bedrock_disjoint_identity(raw_usage: &Value, u: &TokenUsage, tag: &str) {
    let Some(raw_input) = raw_usage.get("inputTokens").and_then(Value::as_u64) else {
        eprintln!("{tag}: raw_usage has no inputTokens — skipping disjoint identity check");
        return;
    };
    assert_eq!(
        raw_input, u.input_tokens,
        "{tag}: Bedrock disjoint identity broken: raw inputTokens={raw_input}, normalized={u:?}"
    );
}

fn assert_anthropic_disjoint_identity(raw_usage: &Value, u: &TokenUsage, tag: &str) {
    let Some(raw_input) = raw_usage.get("input_tokens").and_then(Value::as_u64) else {
        eprintln!("{tag}: raw_usage has no input_tokens — skipping disjoint identity check");
        return;
    };
    assert_eq!(
        raw_input, u.input_tokens,
        "{tag}: Anthropic disjoint identity broken: raw input_tokens={raw_input}, normalized={u:?}"
    );
    // Verify cache fields round-trip correctly (Anthropic reports them disjointly).
    let raw_cached = raw_usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert_eq!(
        raw_cached, u.cached_input_tokens,
        "{tag}: Anthropic cache_read mismatch: raw={raw_cached}, normalized={u:?}"
    );
    let raw_creation = raw_usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    assert_eq!(
        raw_creation, u.cache_creation_tokens,
        "{tag}: Anthropic cache_creation mismatch: raw={raw_creation}, normalized={u:?}"
    );
}

// ── Tests ────────────────────────────────────────────────────────────────

/// Per-provider invariant sweep. Picks one model per distinct provider from
/// `.models.yaml`, issues a small request, and asserts the canonical token
/// invariants. A provider whose test model hard-fails (bad key, provider
/// offline) prints SKIP but does not fail the test — **unless every model
/// skipped**, in which case we fail to avoid a silent no-op run.
///
/// This is the main regression guard for CLI display: if a provider's usage
/// shape changes and the extractor stops returning the right disjoint
/// buckets, the displayed `↑`/`cache%` on every turn with that provider
/// would be wrong, and this test would catch it.
#[tokio::test]
#[ignore = "hits real provider APIs; run with `make test-live-llm` or --ignored"]
async fn per_provider_token_usage_invariants() {
    if skip_if_not_opted_in("per_provider_token_usage_invariants") {
        return;
    }
    let models = load_models_yaml();
    if models.is_empty() {
        eprintln!("SKIP: no usable models in .models.yaml");
        return;
    }
    let candidates = one_per_provider(&models);
    eprintln!(
        "Testing {} providers: {:?}",
        candidates.len(),
        candidates
            .iter()
            .map(|m| format!("{}/{}", m.provider, m.name))
            .collect::<Vec<_>>()
    );

    let client = http_client();
    let mut any_succeeded = false;
    for model in &candidates {
        let tag = format!("{}/{}", model.provider, model.name);
        let dialect = UsageDialect::for_provider(&model.provider);
        let res = match dialect {
            UsageDialect::BedrockConverse => {
                call_bedrock_converse(&client, model, "Say hi in 3 words.").await
            }
            UsageDialect::AnthropicMessages => {
                call_anthropic_messages(&client, model, "Say hi in 3 words.").await
            }
            UsageDialect::OpenAi => {
                call_openai_compatible(&client, model, "Say hi in 3 words.").await
            }
        };
        let Some(r) = res else { continue };
        eprintln!("[{tag}] raw usage: {}", r.raw_usage);
        eprintln!("[{tag}] normalized: {:?}", r.normalized);

        assert_nonzero_input_and_output(&r.normalized, &tag);
        assert_disjoint_sum_identity(&r.normalized, &tag);
        match dialect {
            UsageDialect::OpenAi => {
                assert_openai_inclusive_identity(&r.raw_usage, &r.normalized, &tag)
            }
            UsageDialect::BedrockConverse => {
                assert_bedrock_disjoint_identity(&r.raw_usage, &r.normalized, &tag)
            }
            UsageDialect::AnthropicMessages => {
                assert_anthropic_disjoint_identity(&r.raw_usage, &r.normalized, &tag)
            }
        }
        any_succeeded = true;
    }
    assert!(
        any_succeeded,
        "no provider could be reached — live test would be a silent no-op. \
         Fix at least one model in .models.yaml or skip the whole suite."
    );
}

/// Bedrock-specific regression: repeating the SAME long prompt must not make
/// `cached_input_tokens` decrease. Uses the first Bedrock model in the yaml.
#[tokio::test]
#[ignore = "hits real Bedrock API; run with `make test-live-llm` or --ignored"]
async fn bedrock_cache_read_does_not_regress_on_repeat() {
    if skip_if_not_opted_in("bedrock_cache_read_does_not_regress_on_repeat") {
        return;
    }
    let models = load_models_yaml();
    let bedrock = models_for_provider(&models, "bedrock");
    let Some(model) = bedrock.first() else {
        eprintln!("SKIP: no Bedrock model in .models.yaml");
        return;
    };
    let tag = format!("{}/{}", model.provider, model.name);
    eprintln!("[{tag}] cache repeat test");

    let client = http_client();
    let prelude = "The following is a long context about distributed systems. ".repeat(200);
    let prompt = format!("{prelude}\nWhat is CAP? Answer in 5 words.");
    let encoded_id = encode_bedrock_model_id(&model.name);
    let url = format!(
        "{}/model/{encoded_id}/converse",
        model.base_url.trim_end_matches('/')
    );
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
        tag: &str,
    ) -> Option<TokenUsage> {
        let resp = client
            .post(url)
            .header("authorization", format!("Bearer {api_key}"))
            .header("content-type", "application/json")
            .json(body)
            .send()
            .await
            .ok()?;
        let status = resp.status();
        let raw: Value = resp.json().await.ok()?;
        if !status.is_success() {
            eprintln!("[{tag}] SKIP: {status}: {raw}");
            return None;
        }
        eprintln!("[{tag}] raw usage: {}", raw["usage"]);
        extract_usage(UsageDialect::BedrockConverse, raw["usage"].as_object()?)
    }

    let Some(first) = one_call(&client, &url, &model.api_key, &body, &tag).await else {
        eprintln!("[{tag}] SKIP: first call failed");
        return;
    };
    let Some(second) = one_call(&client, &url, &model.api_key, &body, &tag).await else {
        eprintln!("[{tag}] SKIP: second call failed");
        return;
    };
    eprintln!("[{tag}] first: {first:?}");
    eprintln!("[{tag}] second: {second:?}");

    assert!(
        second.cached_input_tokens >= first.cached_input_tokens,
        "[{tag}] repeat call regressed cache reads: first={first:?}, second={second:?}"
    );
    assert_disjoint_sum_identity(&first, &tag);
    assert_disjoint_sum_identity(&second, &tag);
}

/// Bedrock streaming regression guard: `metadata` (carrying usage) arrives
/// AFTER `messageStop`. The transport must drain to EOS — this test would
/// have caught the `tokens:0 (↑0 ↓0)` bug where the loop broke on
/// `is_finished()` once messageStop fired.
#[tokio::test]
#[ignore = "hits real Bedrock converse-stream; run with `make test-live-llm` or --ignored"]
async fn bedrock_converse_stream_yields_metadata_after_message_stop() {
    if skip_if_not_opted_in("bedrock_converse_stream_yields_metadata_after_message_stop") {
        return;
    }
    let models = load_models_yaml();
    let bedrock = models_for_provider(&models, "bedrock");
    let Some(model) = bedrock.first() else {
        eprintln!("SKIP: no Bedrock model in .models.yaml");
        return;
    };
    let tag = format!("{}/{}", model.provider, model.name);

    let client = http_client();
    let encoded_id = encode_bedrock_model_id(&model.name);
    let url = format!(
        "{}/model/{encoded_id}/converse-stream",
        model.base_url.trim_end_matches('/')
    );
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "text": "Count from 1 to 5, one number per sentence, each sentence at least 12 words."
            }]
        }],
        "inferenceConfig": {"maxTokens": 200, "temperature": 0.0},
    });

    let resp = match client
        .post(&url)
        .header("authorization", format!("Bearer {}", model.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[{tag}] SKIP: HTTP send failed: {e}");
            return;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eprintln!("[{tag}] SKIP: {status}: {body}");
        return;
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        content_type.contains("vnd.amazon.eventstream"),
        "[{tag}] expected AWS eventstream content-type, got: {content_type}"
    );

    let mut decoder = FrameDecoder::new();
    let mut event_type_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut saw_message_stop_at: Option<usize> = None;
    let mut saw_metadata_at: Option<usize> = None;
    let mut assembled_text = String::new();
    let mut final_usage: Option<TokenUsage> = None;
    let mut stop_reason: Option<String> = None;
    let mut frame_index = 0usize;

    let mut body_stream = resp.bytes_stream();
    while let Some(chunk) = body_stream.next().await {
        let bytes = chunk.expect("stream chunk");
        decoder.push(&bytes);
        loop {
            let frame = match decoder.try_next_frame() {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(e) => panic!("[{tag}] decode error: {e}"),
            };
            frame_index += 1;
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
                        saw_message_stop_at = Some(frame_index);
                        stop_reason = payload
                            .get("stopReason")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                    }
                    "metadata" => {
                        saw_metadata_at = Some(frame_index);
                        if let Some(usage_obj) = payload.get("usage").and_then(Value::as_object) {
                            final_usage = extract_usage(UsageDialect::BedrockConverse, usage_obj);
                        }
                    }
                    _ => {}
                }
            } else if frame.message_type() == Some("exception") {
                let payload: Value = serde_json::from_slice(&frame.payload).unwrap_or(Value::Null);
                panic!(
                    "[{tag}] bedrock exception frame {:?}: {}",
                    frame.exception_type(),
                    payload
                );
            }
        }
    }

    eprintln!("[{tag}] event counts: {event_type_counts:?}");
    eprintln!(
        "[{tag}] messageStop at frame {saw_message_stop_at:?}, metadata at {saw_metadata_at:?}"
    );
    eprintln!(
        "[{tag}] assembled text ({} chars): {assembled_text:?}",
        assembled_text.len()
    );
    eprintln!("[{tag}] final usage: {final_usage:?}");

    assert!(
        event_type_counts
            .get("contentBlockDelta")
            .copied()
            .unwrap_or(0)
            >= 2,
        "[{tag}] streaming should yield ≥ 2 contentBlockDelta frames; got {event_type_counts:?}"
    );
    assert_eq!(
        event_type_counts.get("messageStop").copied(),
        Some(1),
        "[{tag}]"
    );
    assert_eq!(
        event_type_counts.get("metadata").copied(),
        Some(1),
        "[{tag}]"
    );
    assert!(!assembled_text.trim().is_empty(), "[{tag}]");
    assert!(stop_reason.is_some(), "[{tag}]");

    // Frame-order guard: metadata arrives AFTER messageStop in the stream.
    // This is the core invariant that the early-break bug violated.
    let stop_at = saw_message_stop_at.expect("messageStop seen");
    let meta_at = saw_metadata_at.expect("metadata seen");
    assert!(
        meta_at > stop_at,
        "[{tag}] metadata must arrive AFTER messageStop (stop at frame {stop_at}, metadata at {meta_at})"
    );

    let u = final_usage.expect("metadata frame must carry usage");
    assert_nonzero_input_and_output(&u, &tag);
    assert_disjoint_sum_identity(&u, &tag);
}
