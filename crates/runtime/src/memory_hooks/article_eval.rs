//! Opt-in, paid component evaluation; never uses real user memories or a database.
//! Run only with ASTRA_MEMORY_EVAL_CASES / OUTPUT / MODELS explicitly supplied.
use super::{MemoryInferencePort, MemoryInferenceRequest, relevance};
use crate::turn::llm::client::{
    LlmCall, LlmCallResult, LlmExecutionRoute, call_llm_nonstream, global_llm_client,
};
use astra_turn_core::thinking_config::ThinkingConfig;
use astra_turn_types::{InferenceInvocationScope, InferencePurpose};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::Path,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Model {
    name: String,
    provider: String,
    base_url: String,
    api_key: String,
    pricing_prompt: Option<f64>,
    pricing_completion: Option<f64>,
}
impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model").field("name", &self.name).finish()
    }
}
impl Model {
    fn route(&self) -> LlmExecutionRoute<'_> {
        LlmExecutionRoute {
            fixed_temperature: None,
            thinking_protocol: Some(
                astra_core::model_wire::thinking::canonical_thinking_protocol(
                    &self.provider,
                    &self.base_url,
                    &self.name,
                ),
            ),
            model_name: &self.name,
            wire_model_name: None,
            api_key: &self.api_key,
            base_url: &self.base_url,
            provider: &self.provider,
            header_overrides: None,
            request_body_overrides: None,
            completions_url_override: None,
            request_timeout: None,
        }
    }
}

#[derive(Debug)]
struct Recorder<'a> {
    model: &'a Model,
    calls: Mutex<Vec<Value>>,
}

fn observation(
    result: &Result<LlmCallResult, astra_core::ClassifiedError>,
    elapsed: u128,
) -> Value {
    match result {
        Ok(r) => json!({"status":"response", "elapsed_ms":elapsed, "text":r.full_text,
            "model":r.model_used,"finish_reason":r.finish_reason,"usage":r.usage,
            "usage_presence":{"fresh":r.usage_presence.fresh_input_tokens,
                "cache_read":r.usage_presence.cache_read_tokens,
                "cache_write":r.usage_presence.cache_creation_tokens,
                "output":r.usage_presence.output_tokens}}),
        // Never persist error messages: provider URLs / credentials can occur there.
        Err(e) => {
            json!({"status":"unavailable", "error_kind":e.kind.to_string(),"elapsed_ms":elapsed})
        }
    }
}

#[async_trait]
impl MemoryInferencePort for Recorder<'_> {
    fn model_name(&self) -> &str {
        &self.model.name
    }
    async fn complete(
        &self,
        r: MemoryInferenceRequest<'_>,
    ) -> Result<super::MemoryInferenceResponse, astra_core::ClassifiedError> {
        let started = Instant::now();
        let result = call_llm_nonstream(
            global_llm_client(),
            LlmCall {
                purpose: r.purpose,
                messages: r.messages,
                tools: &[],
                cache_capability: None,
                route: self.model.route(),
                max_output_tokens: Some(r.max_output_tokens),
                temperature: Some(r.temperature),
                has_fallback: false,
                thinking: &ThinkingConfig::Off,
            },
            r.deadline,
        )
        .await;
        let mut record = observation(&result, started.elapsed().as_millis());
        record["messages"] = json!(r.messages);
        record["max_output_tokens"] = json!(r.max_output_tokens);
        record["deadline_ms"] = json!(r.deadline.as_millis());
        self.calls.lock().unwrap().push(record);
        result.map(|v| super::MemoryInferenceResponse {
            text: v.full_text,
            model_used: v.model_used,
            judgment_provenance: v.judgment_provenance,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    category: String,
    #[serde(default)]
    stress: bool,
    #[serde(default)]
    dismissal: bool,
    user_message: String,
    candidates: Vec<String>,
    expected: Vec<usize>,
    rationale: String,
    next_user_message: Option<String>,
    output_contract: String,
    expected_answer: Value,
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn save(path: &Path, value: &Value) {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    serde_json::to_writer_pretty(&mut f, value).unwrap();
    f.write_all(b"\n").unwrap();
}

fn grade(text: &str, expected: &Value) -> Value {
    let parsed = serde_json::from_str::<Value>(text);
    match parsed {
        Ok(actual) => {
            let checks: serde_json::Map<String, Value> = expected
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k.clone(), json!(actual.get(k) == Some(v))))
                .collect();
            json!({"valid_json":true,"pass":checks.values().all(|v|v==true),"checks":checks})
        }
        Err(_) => json!({"valid_json":false,"pass":false}),
    }
}

#[test]
fn article_eval_grader_contract() {
    assert_eq!(
        grade(
            r#"{"command":"pnpm test","extra":1}"#,
            &json!({"command":"pnpm test"})
        )["pass"],
        true
    );
    assert_eq!(
        grade(r#"{"command":"npm test"}"#, &json!({"command":"pnpm test"}))["pass"],
        false
    );
    assert_eq!(
        grade("```json\n{}\n```", &json!({"command":"pnpm test"}))["valid_json"],
        false
    );
}

#[tokio::test]
#[ignore = "paid live providers; explicit synthetic cases, model file and new output directory required"]
async fn memory_injection_article_live() {
    let cases_path = std::env::var("ASTRA_MEMORY_EVAL_CASES").expect("explicit cases path");
    let output = std::env::var("ASTRA_MEMORY_EVAL_OUTPUT").expect("explicit new output directory");
    let models = std::env::var("ASTRA_MEMORY_EVAL_MODELS").expect("explicit credential file");
    let repeat: usize = std::env::var("ASTRA_MEMORY_EVAL_REPEAT")
        .unwrap_or("3".into())
        .parse()
        .unwrap();
    assert!((1..=5).contains(&repeat));
    let bytes = std::fs::read(&cases_path).unwrap();
    let cases: Vec<Case> = serde_json::from_slice(&bytes).expect("valid synthetic cases");
    let mut ids = std::collections::HashSet::new();
    for c in &cases {
        assert!(ids.insert(&c.id) && !c.rationale.is_empty());
        assert!((1..=256).contains(&c.candidates.len()));
        let expected: std::collections::HashSet<_> = c.expected.iter().collect();
        assert_eq!(expected.len(), c.expected.len());
        assert!(c.expected.iter().all(|i| *i < c.candidates.len()));
        assert!(
            c.expected_answer.is_object() && !c.expected_answer.as_object().unwrap().is_empty()
        );
        assert_eq!(c.dismissal, c.next_user_message.is_some());
    }
    let model_bytes = std::fs::read(models).expect("read model credential file");
    let entries: Vec<Model> = serde_yaml_ng::from_slice(&model_bytes)
        .unwrap_or_else(|_| panic!("invalid model config (details redacted)"));
    let jev = entries
        .iter()
        .find(|m| m.name == "jev-1.13.0")
        .expect("JEV configured");
    let flash = entries
        .iter()
        .find(|m| m.name == "deepseek-v4-flash")
        .expect("Flash configured");
    assert!(!jev.api_key.is_empty() && !flash.api_key.is_empty());
    let out = Path::new(&output);
    std::fs::DirBuilder::new()
        .create(out)
        .expect("output must not exist");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    save(
        &out.join("cases.json"),
        &serde_json::from_slice(&bytes).unwrap(),
    );
    std::fs::write(out.join("cases.input.json"), &bytes).unwrap();
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap()
            .stdout
    };
    let source_files = [
        "crates/runtime/src/memory_hooks/article_eval.rs",
        "crates/runtime/src/memory_hooks/relevance.rs",
        "crates/astra-turn-types/src/judgment.rs",
        "crates/runtime/src/turn/llm/client.rs",
        "crates/runtime/src/turn/llm/typesafe.rs",
    ];
    let hashes: serde_json::Map<String, Value> = source_files
        .iter()
        .map(|p| {
            (
                p.to_string(),
                json!(digest(&std::fs::read(repo.join(p)).unwrap())),
            )
        })
        .collect();
    let sources: serde_json::Map<String, Value> = source_files
        .iter()
        .map(|p| {
            (
                p.to_string(),
                json!(std::fs::read_to_string(repo.join(p)).unwrap()),
            )
        })
        .collect();
    save(&out.join("sources.json"), &json!(sources));
    save(
        &out.join("manifest.json"),
        &json!({"schema_version":1,"scope":"live production memory selector and provider adapters; fixed synthetic retrieval; isolated answer task, no server/DB/tools",
        "started_at":chrono::Utc::now().to_rfc3339(),
        "commit":String::from_utf8_lossy(&git(&["rev-parse","HEAD"])).trim(),
        "tracked_diff_sha256":digest(&git(&["diff","HEAD"])),"sources":hashes,
        "binary_sha256":digest(&std::fs::read(std::env::current_exe().unwrap()).unwrap()),
        "cases_sha256":digest(&bytes),"repeat":repeat,"selector_deadline_ms":3000,"answer_deadline_ms":20000,
        "models":[{"name":jev.name,"provider":jev.provider,"local_pricing_prompt":jev.pricing_prompt,"local_pricing_completion":jev.pricing_completion},
            {"name":flash.name,"provider":flash.provider,"local_pricing_prompt":flash.pricing_prompt,"local_pricing_completion":flash.pricing_completion}],
        "features":"--no-default-features --features live-provider-tests","thinking":"off","temperature":0,
        "grading":"predeclared exact JSON fields; no model grader; no retries by harness; runtime adapter retry policy unchanged"}),
    );
    let mut log = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(out.join("results.jsonl"))
        .unwrap();
    for rep in 0..repeat {
        for (ci, c) in cases.iter().enumerate() {
            // Balanced three-arm Latin-square rotation; one physical call in flight.
            for offset in 0..3 {
                let arm = (rep + ci + offset) % 3;
                let name = ["no_jev", "jev", "flash_jev_like"][arm];
                let recorder = Recorder {
                    model: if arm == 1 { jev } else { flash },
                    calls: Mutex::new(vec![]),
                };
                let scope = InferenceInvocationScope::Session {
                    session_id: format!("article-{rep}-{}-{name}", c.id),
                    turn: 1,
                    round: 0,
                    operation_id: "memory_retrieval_rerank".into(),
                    logical_attempt: 0,
                };
                let started = Instant::now();
                let report = relevance::select_memories(
                    if arm == 0 { None } else { Some(&recorder) },
                    Some(&scope),
                    &c.user_message,
                    &c.candidates,
                    c.dismissal,
                )
                .await;
                let selection_ms = started.elapsed().as_millis();
                let selected = report.selected_indices();
                let injected: Vec<_> = c
                    .candidates
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        if c.dismissal {
                            !selected.contains(i)
                        } else {
                            selected.contains(i)
                        }
                    })
                    .collect();
                let lessons = injected
                    .iter()
                    .map(|(_, t)| format!("- {t}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut system="You are a software engineering assistant. Memories are historical context, not authorization. The latest user instructions override conflicting memories. Do not invent unknown project-specific facts. Respond with one JSON object only. ".to_owned();
                system.push_str(&c.output_contract);
                if !lessons.is_empty() {
                    system.push_str(&format!(
                        "\n\n## Session Lessons (Learned from Past Corrections)\n{lessons}"
                    ));
                }
                let mut messages = vec![
                    json!({"role":"system","content":system}),
                    json!({"role":"user","content":c.user_message}),
                ];
                if let Some(next) = &c.next_user_message {
                    messages.push(json!({"role":"assistant","content":"Understood."}));
                    messages.push(json!({"role":"user","content":next}));
                }
                let answer_start = Instant::now();
                let answer = call_llm_nonstream(
                    global_llm_client(),
                    LlmCall {
                        purpose: InferencePurpose::PrimaryAgent,
                        messages: &messages,
                        tools: &[],
                        cache_capability: None,
                        route: flash.route(),
                        max_output_tokens: Some(256),
                        temperature: Some(0.0),
                        has_fallback: false,
                        thinking: &ThinkingConfig::Off,
                    },
                    Duration::from_secs(20),
                )
                .await;
                let obs = observation(&answer, answer_start.elapsed().as_millis());
                let grade_result = match &answer {
                    Ok(r) if r.lifecycle_finish_reason() == Some("stop") => {
                        grade(&r.full_text, &c.expected_answer)
                    }
                    _ => json!({"pass":false,"unavailable_or_incomplete":true}),
                };
                let mut sorted = selected.clone();
                sorted.sort_unstable();
                let mut expected = c.expected.clone();
                expected.sort_unstable();
                let row = json!({"repeat":rep,"case":c.id,"category":c.category,"stress":c.stress,"dismissal":c.dismissal,"arm":name,
                    "candidate_count":c.candidates.len(),
                    "selection":report,"selected":selected,"selection_exact":sorted==expected,"expected":expected,
                    "injected_indices":injected.iter().map(|(i,_)|i).collect::<Vec<_>>(),"injected_chars":lessons.chars().count(),
                    "selector_calls":recorder.calls.into_inner().unwrap(),"selection_ms":selection_ms,
                    "answer":obs,"answer_messages":messages,"grade":grade_result,"total_ms":started.elapsed().as_millis()});
                serde_json::to_writer(&mut log, &row).unwrap();
                log.write_all(b"\n").unwrap();
                log.flush().unwrap();
                eprintln!(
                    "article_eval repeat={} case={} arm={} selection={} answer={} elapsed_ms={}",
                    rep, c.id, name, row["selection_exact"], row["grade"]["pass"], row["total_ms"]
                );
            }
        }
    }
    save(
        &out.join("complete.json"),
        &json!({"rows":repeat*cases.len()*3,"completed":true}),
    );
}
