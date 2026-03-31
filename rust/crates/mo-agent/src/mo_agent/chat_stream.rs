use super::*;
use std::sync::OnceLock;

use crate::stream_render::consume_turn_sse;
use mo_agent_core::{RuntimeLimits, agent_warn};
use mo_agent_runtime::pipeline::step_protocol::{
    CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache,
};
use mo_agent_runtime::turn::chat_turn_heuristics::{
    extract_repos_from_memory, factual_tool_retry_message, should_force_factual_tool_retry,
};
use mo_agent_runtime::turn::edge_prompt_context::{
    detect_project_languages, detect_workspace_context, make_args_preview,
};
use mo_agent_runtime::turn::tool_result_semantics::{
    is_resource_limit_output, is_tool_error, tool_dedup_signature,
};

/// Stable id for this process (§5.5 `edge_executor_id`). Override with `MO_EDGE_EXECUTOR_ID`.
static EDGE_EXECUTOR_INSTANCE_ID: OnceLock<String> = OnceLock::new();

pub(crate) fn edge_executor_instance_id() -> &'static str {
    EDGE_EXECUTOR_INSTANCE_ID
        .get_or_init(|| {
            std::env::var("MO_EDGE_EXECUTOR_ID").unwrap_or_else(|_| {
                format!("edge-{}", uuid::Uuid::new_v4())
            })
        })
        .as_str()
}

/// Tools that are idempotent reads — safe to cache across turns.
/// Side-effectful tools (bash, write_file, mo_query, etc.) must NOT be in this list.
const CACHEABLE_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
    "symbols",
    "find_definition",
    "find_references",
    "git_status",
    "git_diff",
    "git_log",
    "git_blame",
    "git_file_history",
    "git_contributors",
    "git_log_search",
    "github_list_prs",
    "github_get_pr",
    "github_ci_status",
    "github_list_issues",
    "github_get_issue",
    "get_agent_info",
];

fn take_edge_output_for_tool_call(
    name: &str,
    args: &serde_json::Value,
    round: &[crate::stream_render::EdgeToolRoundEntry],
    consumed: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> String {
    let sig = tool_dedup_signature(name, args);
    for (i, e) in round.iter().enumerate() {
        if consumed[i] {
            continue;
        }
        if tool_dedup_signature(&e.tool, &e.args) == sig {
            consumed[i] = true;
            return e.output.clone();
        }
    }
    by_sig.get(&sig).cloned().unwrap_or_else(|| {
        format!(
            "Error: headless edge protocol — expected SSE `tool_request` before assistant `tool_call` for `{name}` (no matching edge execution in this turn)."
        )
    })
}

async fn hydrate_reflect_placeholder_if_needed(
    api: &mo_thin_client::ThinClient,
    token: &str,
    current_session_id: Option<&String>,
    name: &str,
    args: &serde_json::Value,
    mut result_str: String,
) -> String {
    if name == "reflect"
        && result_str.contains("reflect_requires_session")
        && let Some(sid) = current_session_id
    {
        let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
        let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
        let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
        let mut qp: Vec<String> = Vec::new();
        if !focus.is_empty() && focus != "auto" {
            qp.push(format!("focus={focus}"));
        }
        if !question.is_empty() {
            qp.push(format!("question={}", urlencoding(question)));
        }
        qp.push(format!("last_n={last_n}"));
        let rel = format!(
            "{}?{}",
            mo_thin_client::paths::chat_session_reflect(sid).trim_start_matches('/'),
            qp.join("&")
        );
        match api.get_authed_path_text(token, &rel).await {
            Ok(text) => {
                result_str = text;
            }
            Err(mo_thin_client::ThinClientError::Api { status, .. }) => {
                result_str = format!("{{\"error\": \"reflect HTTP {status}\"}}");
            }
            Err(e) => {
                result_str = format!("{{\"error\": \"reflect failed: {e}\"}}");
            }
        }
    }
    result_str
}

fn print_explain_report(turns: &[serde_json::Value], verbose: bool) {
    eprintln!("\n{}", "── EXPLAIN ─────────────────────────────".dim());
    let mut total_ms = 0i64;
    let mut total_prompt = 0i64;
    let mut total_completion = 0i64;
    let mut total_prompt_known = true;
    let mut total_completion_known = true;
    for (idx, turn) in turns.iter().enumerate() {
        let ms = turn.get("total_ms").and_then(|v| v.as_i64()).unwrap_or(0);
        let prompt = turn.get("prompt_tokens").and_then(|v| v.as_i64());
        let completion = turn.get("completion_tokens").and_then(|v| v.as_i64());
        total_ms += ms;
        if let Some(value) = prompt {
            total_prompt += value;
        } else {
            total_prompt_known = false;
        }
        if let Some(value) = completion {
            total_completion += value;
        } else {
            total_completion_known = false;
        }

        let selected = turn
            .get("tools_selected")
            .and_then(|v| v.as_u64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let selected_skills = turn
            .get("selected_skills")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let available = turn
            .get("tools_available")
            .and_then(|v| v.as_u64())
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let prompt_s = prompt
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let completion_s = completion
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".to_string());
        let mut tool_info = format!("tools: {selected}/{available}");
        if let Some(selection) = turn.get("tool_selection").filter(|value| !value.is_null()) {
            tool_info.push_str(&format!(" → {selection}"));
        }
        if let Some(fallback) = turn
            .get("tool_selection_fallback")
            .filter(|value| !value.is_null())
        {
            tool_info.push_str(&format!(" ⚠fallback:{fallback}"));
        }
        if !selected_skills.is_empty() {
            tool_info.push_str(&format!("  skills=[{selected_skills}]"));
        }
        eprintln!(
            "{}",
            format!(
                "Turn {}  {}ms  tokens: {}→{}  {}",
                idx + 1,
                ms,
                prompt_s,
                completion_s,
                tool_info
            )
            .dim()
        );

        if let Some(routing) = turn.get("routing").and_then(|v| v.as_object()) {
            if routing.get("skipped").and_then(|v| v.as_bool()) == Some(true) {
                let reason = routing
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                eprintln!("{}", format!("  ├─ routing  skipped ({reason})").dim());
            } else {
                let intent = routing
                    .get("intent")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let confidence = routing.get("confidence").and_then(|v| v.as_f64());
                let tier = routing
                    .get("tier")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let latency_ms = routing
                    .get("latency_ms")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let est = routing
                    .get("estimated_tokens")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let confidence_s = if intent == "default" {
                    "-".to_string()
                } else {
                    confidence
                        .map(|v| format!("{v:.2}"))
                        .unwrap_or_else(|| "?".to_string())
                };
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ routing  {}  conf={}  tier={}  {:.0}ms  ~{}tok",
                        intent, confidence_s, tier, latency_ms, est
                    )
                    .dim()
                );
            }
        }

        if let Some(memory) = turn.get("memory").and_then(|v| v.as_object()) {
            if let Some(l0) = memory.get("l0").and_then(|v| v.as_object()) {
                let loaded = if l0.get("loaded").and_then(|v| v.as_bool()) == Some(true) {
                    "✓"
                } else {
                    "✗"
                };
                let l0_tokens = l0.get("tokens").and_then(|v| v.as_i64()).unwrap_or(0);
                let l0_ms = l0.get("ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ L0 profile  {}  {} tokens  {:.0}ms",
                        loaded, l0_tokens, l0_ms
                    )
                    .dim()
                );
            }
            if let Some(ret) = memory.get("retrieval").and_then(|v| v.as_object()) {
                let kw_hit = if ret.get("keyword_hit").and_then(|v| v.as_bool()) == Some(true) {
                    "✓"
                } else {
                    "✗"
                };
                let vec_hit = if ret.get("vector_hit").and_then(|v| v.as_bool()) == Some(true) {
                    "✓"
                } else {
                    "✗"
                };
                let p1 = ret
                    .get("phase1_candidates")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let p2 = ret
                    .get("phase2_candidates")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let merged = ret
                    .get("merged_candidates")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let final_count = ret.get("final_count").and_then(|v| v.as_i64()).unwrap_or(0);
                let ret_ms = ret.get("total_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
                let l1_tokens = memory
                    .get("l1")
                    .and_then(|v| v.as_object())
                    .and_then(|l1| l1.get("tokens"))
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ L1 retrieval  {:.0}ms  kw={}({}) vec={}({}) → {} → {}  {} tokens",
                        ret_ms, kw_hit, p1, vec_hit, p2, merged, final_count, l1_tokens
                    )
                    .dim()
                );
            } else if let Some(mem_ms) = memory.get("total_ms").and_then(|v| v.as_f64()) {
                eprintln!("{}", format!("  └─ memory total  {:.0}ms", mem_ms).dim());
            }
        }

        if let Some(steps) = turn.get("steps").and_then(|v| v.as_array()) {
            for step in steps {
                let label = step.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                let dur = step
                    .get("duration_ms")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if label == "llm" {
                    let sin = step
                        .get("in")
                        .and_then(|v| v.as_i64())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let sout = step
                        .get("out")
                        .and_then(|v| v.as_i64())
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "?".to_string());
                    let tc = step.get("tool_calls").and_then(|v| v.as_u64()).unwrap_or(0);
                    let suffix = if tc > 0 {
                        format!("in={} out={} tool_calls={}", sin, sout, tc)
                    } else {
                        format!("in={} out={}", sin, sout)
                    };
                    eprintln!("{}", format!("  └─ LLM  {}ms  {}", dur, suffix).dim());
                } else {
                    eprintln!("{}", format!("  └─ {}  {}ms", label, dur).dim());
                }
            }
        }

        if let Some(aux) = turn.get("auxiliary_llm_calls").and_then(|v| v.as_array()) {
            let mut aux_tokens_known = true;
            let aux_tokens = aux
                .iter()
                .map(|item| {
                    let tin = item.get("tokens_in").and_then(|v| v.as_i64());
                    let tout = item.get("tokens_out").and_then(|v| v.as_i64());
                    if tin.is_none() || tout.is_none() {
                        aux_tokens_known = false;
                    }
                    tin.unwrap_or(0) + tout.unwrap_or(0)
                })
                .sum::<i64>();
            eprintln!(
                "{}",
                format!(
                    "  ├─ auxiliary LLM  {} calls  {} tokens",
                    aux.len(),
                    if aux_tokens_known {
                        aux_tokens.to_string()
                    } else {
                        "?".to_string()
                    }
                )
                .dim()
            );
            for call in aux {
                let purpose = call.get("purpose").and_then(|v| v.as_str()).unwrap_or("?");
                let ms = call.get("ms").and_then(|v| v.as_i64()).unwrap_or(0);
                let tin = call
                    .get("tokens_in")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                let tout = call
                    .get("tokens_out")
                    .and_then(|v| v.as_i64())
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".to_string());
                eprintln!(
                    "{}",
                    format!("  │    {}  {}ms  {}→{}", purpose, ms, tin, tout).dim()
                );
            }
        }
        if verbose {
            if let Some(preview) = turn.get("content_preview").and_then(|v| v.as_str()) {
                eprintln!("{}", format!("  ├─ content  {}", preview).dim());
            }
            if let Some(phase_timing) = turn.get("phase_timing").and_then(|v| v.as_array()) {
                for entry in phase_timing {
                    let step = entry.get("step").and_then(|v| v.as_str()).unwrap_or("?");
                    let ms = entry.get("ms").and_then(|v| v.as_i64()).unwrap_or(0);
                    eprintln!("{}", format!("  ├─ phase  {}  {}ms", step, ms).dim());
                }
            }
            if let Some(candidates) = turn
                .get("memory")
                .and_then(|v| v.get("retrieval"))
                .and_then(|v| v.get("candidates"))
                .and_then(|v| v.as_array())
            {
                for cand in candidates {
                    let score = cand.get("score").and_then(|v| v.as_f64()).unwrap_or(0.0);
                    let id = cand.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    eprintln!(
                        "{}",
                        format!("  ├─ candidate  {}  score={:.3}", id, score).dim()
                    );
                }
            }
        }
    }
    let total_prompt_s = if total_prompt_known {
        total_prompt.to_string()
    } else {
        "?".to_string()
    };
    let total_completion_s = if total_completion_known {
        total_completion.to_string()
    } else {
        "?".to_string()
    };
    eprintln!(
        "{}",
        format!(
            "Total: {}ms  tokens: {}→{}",
            total_ms, total_prompt_s, total_completion_s
        )
        .dim()
    );
    eprintln!("{}", "─────────────────────────────────────────────".dim());
}

/// Print TurnGuard verdict details in explain mode.
fn print_verdict_report(verdict_events: &[VerdictEvent], verbose: bool) {
    if verdict_events.is_empty() {
        return;
    }
    eprintln!("\n{}", "── TURN GUARD ──────────────────────────".dim());
    for ve in verdict_events {
        let icon = match ve.severity.as_str() {
            "critical" => "🛑",
            "warning" => "⚠",
            _ => "ℹ",
        };
        eprintln!(
            "{}",
            format!(
                "T{} {} {}  nudges={}  errors={}  deprioritized={}{}",
                ve.turn,
                icon,
                ve.severity,
                ve.nudge_count,
                ve.total_errors,
                ve.deprioritized_count,
                if ve.force_stop { "  FORCE_STOP" } else { "" },
            )
            .dim()
        );
        if !ve.avoid_tools.is_empty() {
            eprintln!(
                "{}",
                format!("  ├─ avoid: [{}]", ve.avoid_tools.join(", ")).dim()
            );
        }
        if verbose {
            for (i, inj) in ve.injections.iter().enumerate() {
                let preview: String = inj.chars().take(120).collect();
                eprintln!("{}", format!("  ├─ injection[{}]: {}…", i, preview).dim());
            }
        } else if !ve.injections.is_empty() {
            eprintln!(
                "{}",
                format!("  └─ {} injection(s)", ve.injections.len()).dim()
            );
        }
    }
    eprintln!("{}", "─────────────────────────────────────────────".dim());
}

/// Parameters for a single agentic chat turn — groups the many arguments
/// to `stream_chat_sse` into a named struct to reduce cognitive load.
pub(super) struct ChatTurnParams<'a> {
    pub(super) api: &'a mo_thin_client::ThinClient,
    pub(super) token: &'a str,
    pub(super) message: &'a str,
    pub(super) session_id: Option<&'a str>,
    pub(super) model: Option<&'a str>,
    pub(super) explain: ExplainMode,
    pub(super) render_md: bool,
    pub(super) history: &'a [(String, String)],
    pub(super) perm_manager: &'a mut PermissionManager,
    pub(super) verbose_mode: bool,
    pub(super) quiet: bool,
    pub(super) selector: &'a dyn tool_selector::ToolSelector,
    pub(super) recent_tools: &'a [String],
    pub(super) tool_health_entries:
        &'a [mo_agent_runtime::pipeline::persistence::ToolHealthEntry],
    /// Skill registry for loading instructions when LLM selects a skill.
    pub(super) skill_registry: &'a crate::skill_instructions::SharedSkillRegistry,
}

/// Full edge-cloud agentic loop: sends message, executes tools, loops until done.
pub(super) async fn stream_chat_sse(p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    // Destructure for readability within the function body
    let ChatTurnParams {
        api,
        token,
        message,
        session_id,
        model,
        explain,
        render_md,
        history,
        perm_manager,
        verbose_mode,
        quiet,
        selector,
        recent_tools,
        tool_health_entries,
        skill_registry,
    } = p;
    let start = Instant::now();
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let executor = edge_tools::ToolExecutor::new(&project_root).with_cloud(api.api_origin(), token);
    let all_schemas = edge_tools::all_tool_schemas();
    let registry = tool_registry::ToolRegistry::new(all_schemas.clone());
    let valid_tool_names: HashSet<String> = all_schemas
        .iter()
        .filter_map(|s| {
            s.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(String::from)
        })
        .collect();

    let mut current_session_id: Option<String> = session_id.map(|s| s.to_string());
    // Build messages: history + current user message
    let mut messages: Vec<serde_json::Value> = history
        .iter()
        .flat_map(|(u, a)| {
            if u.is_empty() {
                // Compacted context: only include the summary as assistant message
                vec![serde_json::json!({"role": "assistant", "content": a})]
            } else {
                vec![
                    serde_json::json!({"role": "user", "content": u}),
                    serde_json::json!({"role": "assistant", "content": a}),
                ]
            }
        })
        .collect();
    messages.push(serde_json::json!({"role": "user", "content": message}));

    let mut tool_results: Vec<serde_json::Value> = Vec::new();
    let mut final_text = String::new();
    let mut total_prompt = 0u64;
    let mut total_completion = 0u64;
    let mut total_tool_calls = 0u32;
    let mut has_any_usage = false;
    let mut explain_turns: Vec<serde_json::Value> = Vec::new();
    // Track first-turn selection report and all unique tools actually used
    let mut first_selection_report: Option<tool_registry::SelectionReport> = None;
    let mut first_budget_pressure: f64 = 0.0;
    let mut all_tools_used: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut turn_sigs: Vec<std::collections::BTreeSet<String>> = Vec::new();
    let mut turn_tool_names: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut forced_factual_retry = false;
    const TOOL_NAME_STALL_WINDOW: usize = 3;
    let mut current_run_id: Option<String> = None;
    let mut stall_events: Vec<(String, u32)> = Vec::new();
    let mut verdict_events: Vec<VerdictEvent> = Vec::new();
    let mut last_heavy_checkpoint: Option<
        mo_agent_runtime::pipeline::step_protocol::StepCheckpoint,
    > = None;
    let mut tool_call_records: Vec<mo_agent_services::session_journal::ToolCallRecord> = Vec::new();
    // Capture first turn's TTFT for observability
    let mut first_ttft_ms: Option<u64> = None;
    // Cross-turn dedup: IdempotencyCache with content-hash keys (Step Protocol)
    let mut idempotency_cache = InMemoryIdempotencyCache::new();
    // Semantic near-duplicate tracker (Tier 2: param-aware, Tier 3: output similarity)
    let mut semantic_dedup = mo_agent_runtime::semantic_dedup::SemanticDedup::new(
        mo_agent_runtime::semantic_dedup::DEFAULT_SIMILARITY_THRESHOLD,
    );
    // Unified non-happy-path guard: stall + divergence + tool health + error recovery + escalation
    let mut turn_guard = if tool_health_entries.is_empty() {
        mo_agent_runtime::turn::turn_guard::TurnGuard::new()
    } else {
        let health = mo_agent_runtime::turn::tool_health::ToolHealthTracker::from_entries(
            tool_health_entries,
        );
        mo_agent_runtime::turn::turn_guard::TurnGuard::with_health(health)
    };
    // Stall enforcement: tools restricted from schema after nudge-ignore
    let mut restricted_tools: HashSet<String> = HashSet::new();
    // Dynamic turn budget: each stall/divergence costs turns to prevent runaway sessions
    let max_turns = RuntimeLimits::global().max_turns;
    let mut remaining_turns: usize = max_turns;
    // Intent drift tracker: per-turn tool names + args for drift detection
    let mut intent_tool_turns: Vec<(Vec<String>, String)> = Vec::new();
    // Step Protocol recorder: maps implicit chat_stream phases to explicit Step events
    let mut step_recorder =
        mo_agent_runtime::pipeline::step_recorder::StepRecorder::with_persistence(
            current_session_id.as_deref().unwrap_or("ephemeral"),
            &format!("chat-{}", start.elapsed().as_millis()),
        );

    // Track first turn's context assembly time for observability
    let mut first_context_assembly_ms: Option<u64> = None;
    let mut first_memoria_ms: Option<u64> = None;
    let mut first_selector_ms: Option<u64> = None;
    let mut first_selector_strategy: Option<String> = None;
    let mut selector_tokens_in: u64 = 0;
    let mut selector_tokens_out: u64 = 0;
    let mut all_selected_skills: Vec<String> = Vec::new();

    for _turn in 0..max_turns {
        if remaining_turns == 0 {
            return Err("Turn budget exhausted due to repeated stalls. Aborting.".to_string());
        }
        remaining_turns = remaining_turns.saturating_sub(1);
        step_recorder.begin_turn(_turn as u32);

        // Track context assembly time
        let assembly_start = Instant::now();

        // Build request payload
        let git_branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let memoria_url = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| mo_agent_core::config::DEFAULT_MEMORIA_URL.to_string());
        let memoria_key = std::env::var("MEMORIA_API_KEY")
            .ok()
            .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
            .unwrap_or_default();
        let mut payload = serde_json::json!({
            "messages": messages,
            "session_id": current_session_id,
            "model": model,
            "explain": match explain { ExplainMode::Off => serde_json::json!(false), ExplainMode::On => serde_json::json!(true), ExplainMode::Verbose => serde_json::json!("verbose") },
            "edge_executor_id": edge_executor_instance_id(),
            "capabilities": mo_thin_client::builtin_capability_preset(),
            "edge_profile": {
                "cwd": project_root.to_string_lossy(),
                "git_branch": git_branch,
                "memoria_url": memoria_url,
                "memoria_key": memoria_key,
                "workspace": detect_workspace_context(&project_root),
            },
        });
        // Detect active system skills from skill instruction block in the message
        // and pass them as edge_profile hints so the server system prompt can reference them.
        {
            let skill_names: Vec<&str> = ["markdown", "concise"]
                .iter()
                .copied()
                .filter(|name| {
                    message.contains(&format!("Output Format: {}", capitalize(name)))
                        || message.contains(&format!("Output Constraint: {}", capitalize(name)))
                })
                .collect();
            if !skill_names.is_empty() {
                payload["edge_profile"]["active_skills"] = serde_json::json!(skill_names);
            }
        }
        // NOTE: Skill instructions are now injected after tool selection (see below)
        // when LLM-based selection chooses a skill.
        
        // Tool selection via pluggable ToolSelector strategy.
        // First turn: selector decides which tools. Follow-up turns: also pin
        // tools the LLM already invoked so they remain available.

        // ── Budget pressure: pre-estimate token usage to reduce tool count ──
        // When context is filling up, select fewer dynamic tools to save tokens.
        // Uses precise estimation with actual schema token costs when available.
        let budget_pressure = {
            let schema_tokens = selector.registry().total_pinned_token_cost();
            let estimated = mo_agent_runtime::prompts::estimate_tokens_precise(
                &messages,
                schema_tokens as usize,
                0, // use default system prompt estimate
            );
            let budget = mo_agent_runtime::prompts::budget_for_model(model);
            let tier = budget.compaction_tier(estimated);
            match tier {
                mo_agent_runtime::prompts::CompactionTier::Normal => 0.0,
                mo_agent_runtime::prompts::CompactionTier::TrimSchemas => 0.3,
                mo_agent_runtime::prompts::CompactionTier::CompactHistory => 0.6,
                mo_agent_runtime::prompts::CompactionTier::AggressivePrune => 0.9,
            }
        };

        // Phase 7.5: Memory-augmented boost terms.
        // Step 1: Extract domain keywords from session history (sync, always works).
        let mut boost_terms =
            mo_agent_runtime::turn::retrieval::extract_boost_terms_from_pairs(history, message);
        // Step 2: Augment with memory service (async, best-effort, 2s timeout).
        // On cold-start (no relevant history), memory may still have stored
        // domain hints (e.g., "matrixorigin is a GitHub org") that improve
        // tool selection. This closes the cold-start gap in entity-rich queries.
        //
        // Memory results are re-ranked by TF-IDF cosine similarity to filter
        // irrelevant memories before boost term extraction (Phase A.2).
        {
            let mem_start = Instant::now();
            let memory_contents = executor.memory_boost_search(message, 5).await;
            let mem_elapsed = mem_start.elapsed().as_millis() as u64;
            if first_memoria_ms.is_none() {
                first_memoria_ms = Some(mem_elapsed);
            }
            if !memory_contents.is_empty() {
                // Bridge memory→preferred_repos: extract owner/repo references
                // from memory content so tool executor can resolve bare repo names.
                for content in &memory_contents {
                    for repo in extract_repos_from_memory(content) {
                        executor.add_preferred_repo(&repo);
                    }
                }

                // Re-rank by TF-IDF similarity; filter below threshold.
                let ranked = mo_agent_runtime::turn::retrieval::rank_memory_results(
                    message,
                    &memory_contents,
                );
                if !ranked.is_empty() {
                    // Use only relevant memories for boost term extraction.
                    let virtual_history: Vec<(String, String)> = ranked
                        .into_iter()
                        .map(|(content, _score)| ("memory".to_string(), content))
                        .collect();
                    let memory_terms =
                        mo_agent_runtime::turn::retrieval::extract_boost_terms_from_pairs(
                            &virtual_history,
                            message,
                        );
                    let existing: std::collections::HashSet<String> =
                        boost_terms.iter().cloned().collect();
                    for term in memory_terms {
                        if !existing.contains(&term) {
                            boost_terms.push(term);
                        }
                    }
                }
            }
        }

        // ── Extract memory domain hints from boost terms ──
        // Map detected boost term keywords → DomainHint for gate softening.
        // General: boost_terms containing domain-related keywords map to hints.
        let memory_domain_hints = {
            use mo_agent_runtime::pipeline::routing::DomainHint;
            let mut hints = Vec::new();
            let terms_lower: Vec<String> = boost_terms.iter().map(|t| t.to_lowercase()).collect();
            let has = |kw: &str| terms_lower.iter().any(|t| t.contains(kw));
            if has("github") || has("repo") || has("pr") || has("issue") || has("pull") {
                hints.push(DomainHint::GitHub);
            }
            if has("git") || has("commit") || has("branch") || has("diff") || has("log") {
                hints.push(DomainHint::Git);
            }
            if has("code") || has("file") || has("edit") || has("read") || has("write") {
                hints.push(DomainHint::Code);
            }
            if has("memory") || has("store") || has("remember") || has("preference") {
                hints.push(DomainHint::Memory);
            }
            hints
        };

        // Proactively seed restricted_tools with deprioritized tools from health tracker.
        // This ensures cross-session deprioritized tools are excluded BEFORE scoring.
        for tool in turn_guard.health.deprioritized_tools() {
            restricted_tools.insert(tool.to_string());
        }
        let restricted_vec: Vec<String> = restricted_tools.iter().cloned().collect();

        // Record PERCEIVE phase: user query + memory context + domain hints
        step_recorder.record_perceive(
            message,
            &[], // memory IDs not yet tracked individually
            &memory_domain_hints
                .iter()
                .map(|h| format!("{:?}", h))
                .collect::<Vec<_>>(),
            &boost_terms,
        );

        let learned_context = selector.learned_context(message, recent_tools);
        let learned_context_hint = learned_context.prompt_fragment();
        let learned_task_type = learned_context
            .task_archetype
            .map(|task_type| format!("{task_type:?}").to_lowercase());

        // Variables to capture selection results including skills
        let mut selected_skills: Vec<String> = Vec::new();
        
        let (turn_schemas, selection_report, selection_confidence) = if tool_results.is_empty() {
            let sel_start = Instant::now();
            let turn_count = history.len() as u32 + 1;
            let sel_ctx = tool_selector::SelectionContext {
                query: message,
                turn_count,
                recent_tools,
                budget_tokens: registry.default_budget(),
                boost_terms: boost_terms.clone(),
                budget_pressure,
                memory_domain_hints: memory_domain_hints.clone(),
                restricted_tools: restricted_vec.clone(),
                file_context: file_context.clone(),
            };
            let sel_result = selector
                .select_with_learned_context(&sel_ctx, &learned_context)
                .await;
            if first_selector_ms.is_none() {
                first_selector_ms = Some(sel_start.elapsed().as_millis() as u64);
                first_selector_strategy = Some(format!(
                    "{} (conf={:.2})",
                    sel_result.strategy, sel_result.confidence
                ));
            }
            selector_tokens_in += sel_result.selector_tokens_in;
            selector_tokens_out += sel_result.selector_tokens_out;
            
            // Capture selected skills from LLM selection
            selected_skills = sel_result.selected_skills.clone();
            
            let conf = sel_result.confidence;
            let (schemas, report) = tool_selector::resolve_schemas_with_pressure(
                &registry,
                &sel_result.tool_names,
                budget_pressure,
            );
            (schemas, report, conf)
        } else {
            // Follow-up turn: use 2x budget, then pin tools already invoked.
            let turn_count = history.len() as u32 + 1;
            let sel_ctx = tool_selector::SelectionContext {
                query: message,
                turn_count,
                recent_tools,
                budget_tokens: registry.default_budget() * 2,
                boost_terms,
                budget_pressure,
                memory_domain_hints,
                restricted_tools: restricted_vec,
                file_context: file_context.clone(),
            };
            let sel_result = selector
                .select_with_learned_context(&sel_ctx, &learned_context)
                .await;
            
            // Capture selected skills (may be new skills in follow-up)
            if !sel_result.selected_skills.is_empty() {
                selected_skills = sel_result.selected_skills.clone();
            }
            
            let conf = sel_result.confidence;
            let (mut selected, mut report) = tool_selector::resolve_schemas_with_pressure(
                &registry,
                &sel_result.tool_names,
                budget_pressure,
            );
            // Add any tools the LLM invoked that aren't already selected
            let selected_names: std::collections::HashSet<String> = selected
                .iter()
                .filter_map(|s| {
                    s.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .map(String::from)
                })
                .collect();
            for tr in &tool_results {
                if let Some(name) = tr.get("name").and_then(|n| n.as_str())
                    && !selected_names.contains(name)
                    && let Some(schema) = all_schemas.iter().find(|s| {
                        s.get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            == Some(name)
                    })
                {
                    selected.push(schema.clone());
                    report.tools_selected.push(name.to_string());
                    report.selected_count += 1;
                }
            }
            (selected, report, conf)
        };
        
        // Load skill instructions if LLM selected any skills
        let skill_instructions: Option<String> = if !selected_skills.is_empty() {
            let mut instructions = Vec::new();
            let mut activated_skills = Vec::new();
            if let Ok(mut reg) = skill_registry.try_write() {
                for skill_name in &selected_skills {
                    // Load instructions if not already loaded
                    if let Err(e) = reg.load_instructions(skill_name) {
                        eprintln!("  {} Failed to load skill {}: {}", "⚠".yellow(), skill_name, e);
                        continue;
                    }
                    // Get the instruction text
                    if let Some(skill) = reg.get(skill_name)
                        && let Some(text) = skill.instruction_text()
                    {
                        activated_skills.push(skill_name.clone());
                        instructions.push(format!("## Skill: {}\n\n{}", skill_name, text));
                    }
                }
            }
            if instructions.is_empty() {
                None
            } else {
                if !quiet {
                    eprintln!(
                        "  {} Using skill: {}",
                        "◆".cyan(),
                        activated_skills.join(", ").cyan()
                    );
                }
                Some(instructions.join("\n\n---\n\n"))
            }
        } else {
            None
        };
        for skill_name in &selected_skills {
            if !all_selected_skills.contains(skill_name) {
                all_selected_skills.push(skill_name.clone());
            }
        }
        
        // Inject skill instructions into payload if LLM selected any skills
        if let Some(ref instructions) = skill_instructions {
            payload["edge_profile"]["skill_instructions"] = serde_json::json!(instructions);
        }
        
        if first_selection_report.is_none() {
            first_selection_report = Some(selection_report);
            first_budget_pressure = budget_pressure;
        }
        // Propagate budget pressure to tool executor for output scaling.
        // Updated each iteration so tools always use the latest pressure.
        executor.set_budget_pressure(budget_pressure);

        // ── Tool guidance hint: when the selector is confident, tell the server
        // which dynamic tools scored highest. The server can inject this as a
        // system prompt hint, biasing the LLM toward the right tools.
        // Only emitted when confidence >= 0.4 and there are dynamic tools.
        {
            use mo_agent_runtime::tool_registry::TOOL_CATALOG;
            let dynamic_tools: Vec<&str> = first_selection_report
                .as_ref()
                .map(|r| {
                    r.tools_selected
                        .iter()
                        .filter(|n| {
                            !TOOL_CATALOG
                                .iter()
                                .any(|t| t.pinned && t.name == n.as_str())
                        })
                        .map(|s| s.as_str())
                        .take(3) // Top 3 dynamic tools (already in score order)
                        .collect()
                })
                .unwrap_or_default();
            if selection_confidence >= 0.4 && !dynamic_tools.is_empty() {
                payload["edge_profile"]["recommended_tools"] = serde_json::json!(dynamic_tools);
                payload["edge_profile"]["selection_confidence"] =
                    serde_json::json!(selection_confidence);
            }
            if !learned_context_hint.is_empty() {
                payload["edge_profile"]["learned_context_hint"] =
                    serde_json::json!(learned_context_hint);
            }
            if let Some(task_type) = learned_task_type.as_ref() {
                payload["edge_profile"]["selection_task_type"] = serde_json::json!(task_type);
            }
        }
        // Dynamic schema restriction: remove tools that were stall-restricted
        let final_schemas = if restricted_tools.is_empty() {
            turn_schemas
        } else {
            turn_schemas
                .into_iter()
                .filter(|s| {
                    let name = s
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    !restricted_tools.contains(name)
                })
                .collect()
        };
        payload["edge_tools"] = serde_json::Value::Array(final_schemas);
        if explain != ExplainMode::Off && !restricted_tools.is_empty() {
            eprintln!(
                "{}",
                format!(
                    "  ├─ restricted: {} tool(s) filtered [{}]",
                    restricted_tools.len(),
                    restricted_tools
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                )
                .dim()
            );
        }
        if explain != ExplainMode::Off
            && let Some(recommended) = payload["edge_profile"]["recommended_tools"].as_array()
        {
            let names: Vec<&str> = recommended.iter().filter_map(|v| v.as_str()).collect();
            if !names.is_empty() {
                eprintln!(
                    "{}",
                    format!(
                        "  ├─ guidance: {} (confidence: {:.2})",
                        names.join(", "),
                        selection_confidence
                    )
                    .dim()
                );
            }
        }
        if !tool_results.is_empty() {
            payload["tool_results"] = serde_json::Value::Array(tool_results.clone());
        }

        // Step recorder: mark plan phase (tool selection done, LLM call about to start)
        {
            let selected_tool_names: Vec<String> = first_selection_report
                .as_ref()
                .map(|r| r.tools_selected.clone())
                .unwrap_or_default();
            let bp = first_budget_pressure;
            let bt = first_selection_report
                .as_ref()
                .map(|r| r.budget_used as u64)
                .unwrap_or(0);
            step_recorder.record_plan(&selected_tool_names, selection_confidence, bp, bt);
        }

        // Capture context assembly time (first turn only)
        if first_context_assembly_ms.is_none() {
            first_context_assembly_ms = Some(assembly_start.elapsed().as_millis() as u64);
        }

        let resp = api
            .post_chat_turn_retry_429(token, &payload, 3, quiet)
            .await
            .map_err(|e| e.to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("API Error ({}): {}", status, compact_or_raw(&body)));
        }

        let edge_ctx = crate::stream_render::EdgeSseContext {
            api,
            token,
            executor_id: edge_executor_instance_id(),
            executor: &executor,
            quiet,
            perm_manager: Some(std::ptr::NonNull::from(&mut *perm_manager)),
            _pm: std::marker::PhantomData,
        };
        let turn_result = consume_turn_sse(
            resp,
            render_md,
            term_width,
            quiet,
            Some(edge_ctx),
        )
        .await;

        // Capture TTFT from first turn for observability
        if first_ttft_ms.is_none() {
            first_ttft_ms = turn_result.ttft_ms;
        }

        if let Some(sid) = &turn_result.session_id {
            current_session_id = Some(sid.clone());
        }
        if turn_result.run_id.is_some() {
            current_run_id = turn_result.run_id.clone();
        }
        if !turn_result.full_text.is_empty() {
            final_text = turn_result.full_text.clone();

            // Response guard: detect prompt leakage in LLM output
            if mo_agent_runtime::turn::response_guard::is_prompt_leaked(&final_text, &[]) {
                agent_warn!(
                    "response_guard",
                    "Prompt leak detected in LLM output, sanitizing"
                );
                final_text = "I apologize, but I encountered an issue generating that response. Let me try again.".to_string();
                break;
            }

            // Response guard: detect repetition loops (LLM stuck repeating same word)
            if mo_agent_runtime::turn::response_guard::is_repetition_loop(&final_text) {
                agent_warn!(
                    "response_guard",
                    "Repetition loop detected in LLM output, breaking"
                );
                final_text = "I noticed I was repeating myself. Let me approach this differently."
                    .to_string();
                break;
            }

            // Response guard: quality check for fabrication and echo
            let quality = mo_agent_runtime::turn::response_guard::check_response_quality(
                &final_text,
                &turn_result.tool_calls,
                &[], // tool name validation handled at execution time (line 1262+)
                message,
            );
            if quality.has_fabrication_markers {
                agent_warn!(
                    "response_guard",
                    "Fabrication markers detected: placeholder paths in response"
                );
            }
            if quality.is_echo {
                agent_warn!(
                    "response_guard",
                    "Echo detected: LLM repeated user query instead of answering"
                );
            }
        }
        total_prompt += turn_result.prompt_tokens;
        total_completion += turn_result.completion_tokens;
        total_tool_calls += if !turn_result.tool_calls.is_empty() {
            turn_result.tool_calls.len()
        } else {
            turn_result.edge_tool_round.len()
        } as u32;

        // Record LLM token usage in step recorder
        step_recorder.record_tokens(turn_result.prompt_tokens, turn_result.completion_tokens);
        // Track all unique tool names that the LLM actually invoked
        for tc in &turn_result.tool_calls {
            if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                all_tools_used.insert(name.to_string());
            }
        }
        for e in &turn_result.edge_tool_round {
            all_tools_used.insert(e.tool.clone());
        }
        has_any_usage = has_any_usage || turn_result.has_usage;
        explain_turns.extend(turn_result.explain_turns);

        if let Some(ref err) = turn_result.error_message {
            return Err(err.clone());
        }

        let round_has_edge_work =
            !turn_result.tool_calls.is_empty() || !turn_result.edge_tool_round.is_empty();
        if !round_has_edge_work {
            if should_force_factual_tool_retry(
                message,
                recent_tools,
                total_tool_calls,
                forced_factual_retry,
            ) {
                forced_factual_retry = true;
                if !quiet {
                    eprintln!(
                        "{}",
                        "  ↻ No tool call on a live-data query; forcing one corrective retry…"
                            .yellow()
                    );
                }
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": factual_tool_retry_message(message),
                }));
                final_text.clear();
                continue;
            }
            break;
        }

        let tool_calls_for_guard: Vec<serde_json::Value> = if !turn_result.tool_calls.is_empty() {
            turn_result.tool_calls.clone()
        } else {
            turn_result
                .edge_tool_round
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    serde_json::json!({
                        "id": format!("edge-{i}"),
                        "name": e.tool,
                        "arguments": e.args.clone(),
                    })
                })
                .collect()
        };

        // Stall & divergence detection via unified TurnGuard
        {
            use std::collections::BTreeSet;

            let sig_set: BTreeSet<String> = tool_calls_for_guard
                .iter()
                .map(|tc| {
                    let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = tc.get("arguments").cloned().unwrap_or_default();
                    format!(
                        "{}:{}",
                        name,
                        serde_json::to_string(&args).unwrap_or_default()
                    )
                })
                .collect();
            let name_set: HashSet<String> = tool_calls_for_guard
                .iter()
                .map(|tc| {
                    tc.get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            turn_sigs.push(sig_set);
            turn_tool_names.push(name_set.clone());

            // Feed tool call signatures into TurnGuard
            turn_guard.record_tool_calls(&tool_calls_for_guard);

            // Name-based stall detection (complementary to TurnGuard's signature stall)
            let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
                && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
                    .windows(2)
                    .all(|w| w[0] == w[1]);

            if name_stall {
                stall_events.push(("name_stall".to_string(), _turn as u32));
            }
        }

        // Assemble tool results from SSE `tool_request` only — legacy inline execution removed.
        tool_results = Vec::new();

        let mut assistant_tc_msg = if !turn_result.tool_calls.is_empty() {
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": turn_result.tool_calls.iter().map(|tc| {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = tc.get("arguments").cloned().unwrap_or(serde_json::json!({}));
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(&args)
                                .unwrap_or_else(|_| r#"{"error":"argument serialization failed"}"#.to_string()),
                        }
                    })
                }).collect::<Vec<_>>(),
            })
        } else {
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "tool_calls": turn_result.edge_tool_round.iter().enumerate().map(|(i, e)| {
                    let id = if e.request_id.is_empty() {
                        format!("edge-{i}")
                    } else {
                        e.request_id.clone()
                    };
                    serde_json::json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": e.tool,
                            "arguments": serde_json::to_string(&e.args)
                                .unwrap_or_else(|_| "{}".to_string()),
                        }
                    })
                }).collect::<Vec<_>>(),
            })
        };
        if !turn_result.reasoning_content.is_empty() {
            assistant_tc_msg["reasoning_content"] =
                serde_json::Value::String(turn_result.reasoning_content.clone());
        }
        messages.push(assistant_tc_msg);

        enum RoundToolItem {
            ServerTc(usize),
            Synthetic(usize),
        }
        let indices: Vec<RoundToolItem> = if !turn_result.tool_calls.is_empty() {
            (0..turn_result.tool_calls.len())
                .map(RoundToolItem::ServerTc)
                .collect()
        } else {
            (0..turn_result.edge_tool_round.len())
                .map(RoundToolItem::Synthetic)
                .collect()
        };

        let tool_count = indices.len().max(1);
        let mut seen_calls: HashSet<String> = HashSet::new();
        step_recorder.begin_act(tool_count);
        let step_start_time = std::time::Instant::now();
        let step_timeout_ms = step_recorder.scheduling().timeout_ms;
        let mut consumed_edge = vec![false; turn_result.edge_tool_round.len()];
        let by_sig: &HashMap<String, String> = &turn_result.edge_callback_outputs;

        for item in &indices {
            let step_elapsed_ms = step_start_time.elapsed().as_millis() as u64;
            if step_elapsed_ms > step_timeout_ms {
                let aborted_count = indices.len() - tool_results.len();
                let aborted_tools: Vec<String> = indices[tool_results.len()..]
                    .iter()
                    .map(|it| match it {
                        RoundToolItem::ServerTc(i) => turn_result.tool_calls[*i]
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        RoundToolItem::Synthetic(i) => turn_result.edge_tool_round[*i].tool.clone(),
                    })
                    .collect();
                agent_warn!(
                    "step",
                    "Step timeout exceeded: {}ms > {}ms, aborting {} tools: {:?}",
                    step_elapsed_ms,
                    step_timeout_ms,
                    aborted_count,
                    aborted_tools
                );
                turn_guard.record_step_abort(&aborted_tools);
                break;
            }

            let (id, name, args, from_synthetic) = match item {
                RoundToolItem::ServerTc(i) => {
                    let tc_event = &turn_result.tool_calls[*i];
                    let id = tc_event
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = tc_event
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args_raw = tc_event
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::Value::Object(Default::default()));
                    let args = match args_raw {
                        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
                            .unwrap_or_else(|_| serde_json::Value::Object(Default::default())),
                        other => other,
                    };
                    (id, name, args, false)
                }
                RoundToolItem::Synthetic(i) => {
                    let e = &turn_result.edge_tool_round[*i];
                    (
                        format!("edge-{i}"),
                        e.tool.clone(),
                        e.args.clone(),
                        true,
                    )
                }
            };

            let call_sig = tool_dedup_signature(&name, &args);
            if !seen_calls.insert(call_sig.clone()) {
                let cached_tr = serde_json::json!({
                    "tool_call_id": id,
                    "name": name,
                    "result": "(duplicate call — result same as previous identical call this turn)",
                });
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": "(duplicate call — result same as previous identical call this turn)",
                }));
                tool_results.push(cached_tr);
                tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                    name: name.clone(),
                    ok: true,
                    ms: 0,
                    error: Some("duplicate_within_turn".to_string()),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: make_args_preview(&name, &args),
                });
                continue;
            }

            let idem_key = IdempotencyKey::semantic(&name, &args);
            if CACHEABLE_TOOLS.contains(&name.as_str())
                && let Some(cached) = idempotency_cache.check(&idem_key)
            {
                let cached_note = format!(
                    "(cached from earlier turn — identical call)\n{}",
                    cached.output
                );
                if !quiet {
                    eprintln!("{}", format!("  ↻ {name} (cached)").dim());
                }
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": cached_note,
                }));
                tool_results.push(serde_json::json!({
                    "tool_call_id": id,
                    "name": name,
                    "result": cached_note,
                }));
                let cache_key = idem_key.cache_key();
                step_recorder.begin_tool_with_key(&name, &id, Some(&cache_key));
                step_recorder.record_cache_hit(&name, cached.clone());
                turn_guard.record_cache_hit(&name);
                tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                    name: name.clone(),
                    ok: true,
                    ms: 0,
                    error: Some("cached_cross_turn".to_string()),
                    input_bytes: None,
                    output_bytes: Some(cached.output.len() as u32),
                    args_preview: make_args_preview(&name, &args),
                });
                continue;
            }

            let mut result_str = if from_synthetic {
                match item {
                    RoundToolItem::Synthetic(i) => turn_result.edge_tool_round[*i].output.clone(),
                    _ => unreachable!(),
                }
            } else {
                take_edge_output_for_tool_call(
                    &name,
                    &args,
                    &turn_result.edge_tool_round,
                    &mut consumed_edge,
                    by_sig,
                )
            };

            if !valid_tool_names.contains(&name) {
                let err_msg = format!(
                    "Unknown tool '{}'. Available: {}",
                    name,
                    valid_tool_names
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                if !quiet {
                    eprintln!("{}", format!("  ✗ {name}").red());
                }
                if !quiet {
                    eprintln!("  {}", format!("└ {err_msg}").dim());
                }
                let err_tr = serde_json::json!({
                    "tool_call_id": id,
                    "name": name,
                    "result": err_msg,
                });
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": err_msg,
                }));
                tool_results.push(err_tr);
                tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                    name: name.clone(),
                    ok: false,
                    ms: 0,
                    error: Some(format!("unknown_tool: {name}")),
                    input_bytes: None,
                    output_bytes: None,
                    args_preview: None,
                });
                continue;
            }

            result_str = hydrate_reflect_placeholder_if_needed(
                api,
                token,
                current_session_id.as_ref(),
                &name,
                &args,
                result_str,
            )
            .await;

            let tool_start = Instant::now();
            let tool_idem_key = if CACHEABLE_TOOLS.contains(&name.as_str()) {
                Some(idem_key.cache_key())
            } else {
                None
            };
            step_recorder.begin_tool_with_key(&name, &id, tool_idem_key.as_deref());

            let mut is_err = is_tool_error(&result_str);
            let tool_already_restricted = restricted_tools.contains(&name);
            let mut resource_limit_recorded = false;

            if is_err && !tool_already_restricted {
                use mo_agent_runtime::turn::error_recovery::{
                    build_recovery_message, classify_error,
                };
                let category = classify_error(&result_str);

                if matches!(
                    category,
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::ResourceLimit
                ) {
                    turn_guard.health.record_resource_limit_failure(&name);
                    turn_guard.errors.record_error(category);
                    restricted_tools.insert(name.clone());
                    resource_limit_recorded = true;
                    if !quiet {
                        eprintln!(
                            "{}",
                            format!("  ⚠ {name} blocked: system resource limit reached").yellow()
                        );
                    }
                }

                if matches!(
                    category,
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::Transient
                ) {
                    turn_guard.errors.record_retry(false);
                }

                let deprioritized = turn_guard.health.deprioritized_tools();
                let recovery_msg =
                    build_recovery_message(&name, &result_str, category, &deprioritized);
                result_str.push_str(&format!("\n{recovery_msg}"));
            }

            if !is_err && !tool_already_restricted && is_resource_limit_output(&result_str) {
                turn_guard.health.record_resource_limit_failure(&name);
                turn_guard.errors.record_error(
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::ResourceLimit,
                );
                restricted_tools.insert(name.clone());
                is_err = true;
                resource_limit_recorded = true;
                if !quiet {
                    eprintln!(
                        "{}",
                        format!("  ⚠ {name}: resource limit detected in output — tool blocked").dim()
                    );
                }
            }

            let result_quality = if resource_limit_recorded {
                mo_agent_runtime::turn::result_quality::ResultQuality::Error
            } else {
                turn_guard.record_tool_result(&name, &result_str)
            };

            if let Some(feedback) = turn_guard.result_feedback(&name, result_quality) {
                result_str.push_str(&format!("\n{feedback}"));
            }

            let args_size = serde_json::to_string(&args)
                .map(|s| s.len() as u32)
                .unwrap_or(0);
            let result_size = result_str.len() as u32;
            let args_preview = make_args_preview(&name, &args);
            let tool_elapsed = tool_start.elapsed();
            tool_call_records.push(mo_agent_services::session_journal::ToolCallRecord {
                name: name.clone(),
                ok: !is_err,
                ms: tool_elapsed.as_millis() as u64,
                error: if is_err {
                    result_str
                        .lines()
                        .next()
                        .map(|l| l.chars().take(200).collect())
                } else {
                    None
                },
                input_bytes: Some(args_size),
                output_bytes: Some(result_size),
                args_preview,
            });
            step_recorder.complete_tool_with_result(
                &name,
                is_err,
                tool_elapsed.as_millis() as u64,
                false,
                &result_str,
            );

            if let Some(ref sid) = current_session_id
                && let Some(light) = step_recorder.build_light_checkpoint()
            {
                let cp = mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Light(light);
                let _ = mo_agent_runtime::pipeline::step_checkpoint::write_step_checkpoint(
                    sid,
                    step_recorder.summary().checkpoints,
                    &cp,
                );
            }

            if !is_err && CACHEABLE_TOOLS.contains(&name.as_str()) {
                let cached_result = CachedToolResult {
                    tool_name: name.clone(),
                    output: result_str.clone(),
                    is_error: false,
                    cached_at: mo_agent_runtime::pipeline::step_protocol::epoch_ms(),
                };
                step_recorder.attach_cached_result(cached_result.clone());
                idempotency_cache.record(&idem_key, cached_result);
                if let Some((prev_turn, reason)) =
                    semantic_dedup.check_and_record(&name, &args, &result_str, _turn)
                {
                    let hint = format!(
                        "\n⚠ Note: this result is similar to a previous {} call (turn {}, {}). \
                         Avoid re-fetching the same information.",
                        name,
                        prev_turn + 1,
                        reason
                    );
                    result_str.push_str(&hint);
                }
            }

            if !quiet {
                let duration_str = if tool_elapsed.as_secs_f64() >= 1.0 {
                    format!("{:.1}s", tool_elapsed.as_secs_f64())
                } else {
                    format!("{}ms", tool_elapsed.as_millis())
                };
                let detail = tool_call_detail(&name, &args);
                let summary = if !is_err {
                    tool_result_summary(&name, &result_str)
                } else {
                    None
                };
                if is_err {
                    eprintln!("{}", format!("  ✗ {name} ({duration_str})").red());
                    if let Some(first_line) = result_str.lines().next() {
                        let preview = if first_line.len() > 100 {
                            format!("{}…", &first_line[..100])
                        } else {
                            first_line.to_string()
                        };
                        eprintln!("  {}", format!("└ Error: {preview}").dim());
                    }
                } else {
                    eprintln!("{}", format!("  ✓ {name} ({duration_str})").green());
                    match (&detail, &summary) {
                        (Some(d), Some(s)) => {
                            eprintln!("  {}", format!("└ {d}  →  {s}").dim());
                        }
                        (Some(d), None) => {
                            eprintln!("  {}", format!("└ {d}").dim());
                        }
                        (None, Some(s)) => {
                            eprintln!("  {}", format!("└ {s}").dim());
                        }
                        (None, None) => {}
                    }
                }
            }

            let tr = serde_json::json!({
                "tool_call_id": id,
                "name": name,
                "result": result_str,
            });
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_str,
            }));
            tool_results.push(tr);
        }
        // ── Intent drift detection ──
        // Track per-turn tool names + args, detect when agent drifts from user's query
        {
            let turn_names: Vec<String> = tool_calls_for_guard
                .iter()
                .filter_map(|tc| tc.get("name").and_then(|v| v.as_str()).map(String::from))
                .collect();
            let turn_args_text: String = tool_calls_for_guard
                .iter()
                .filter_map(|tc| {
                    tc.get("arguments")
                        .map(|v| serde_json::to_string(v).unwrap_or_default())
                })
                .collect::<Vec<_>>()
                .join(" ");
            intent_tool_turns.push((turn_names, turn_args_text));

            if let mo_agent_runtime::turn::stall::IntentDrift::Drifting { correction, .. } =
                mo_agent_runtime::turn::stall::detect_intent_drift(message, &intent_tool_turns)
            {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": correction
                }));
                stall_events.push(("intent_drift".to_string(), _turn as u32));
            }
        }

        // ── TurnGuard: unified non-happy-path evaluation ──
        // Evaluate AFTER all tool results recorded, BEFORE next LLM call.
        {
            use mo_agent_runtime::turn::turn_guard::VerdictSeverity;

            let verdict = turn_guard.evaluate();

            // ── Audit: collect non-Healthy verdict events ──
            if verdict.severity > VerdictSeverity::Healthy {
                let severity_str = match verdict.severity {
                    VerdictSeverity::Critical => "critical",
                    VerdictSeverity::Warning => "warning",
                    VerdictSeverity::Info => "info",
                    VerdictSeverity::Healthy => unreachable!(),
                };
                let health_summary = turn_guard.health.summary();
                verdict_events.push(VerdictEvent {
                    turn: _turn as u32,
                    severity: severity_str.to_string(),
                    injections: verdict.injections.clone(),
                    avoid_tools: verdict.avoid_tools.clone(),
                    force_stop: verdict.force_stop,
                    nudge_count: turn_guard.nudge_count,
                    total_errors: turn_guard.errors.total_errors,
                    deprioritized_count: health_summary.deprioritized_count,
                    total_timeouts: health_summary.total_timeouts,
                    total_cache_hits: health_summary.total_cache_hits,
                    flaky_count: health_summary.flaky_count,
                });
            }

            // Inject all verdict messages (stall nudge, divergence correction,
            // tool health warnings, escalation messages, nudge-ignore warnings)
            for injection in &verdict.injections {
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": injection
                }));
            }

            // Restrict tools that TurnGuard says to avoid
            for tool in &verdict.avoid_tools {
                restricted_tools.insert(tool.clone());
            }

            // Apply turn budget penalties based on severity.
            match verdict.severity {
                VerdictSeverity::Critical => {
                    remaining_turns = remaining_turns.saturating_sub(5);
                }
                VerdictSeverity::Warning => {
                    remaining_turns = remaining_turns.saturating_sub(2);
                }
                _ => {}
            }

            // Step recorder: record verdict outcome
            let severity_label = match verdict.severity {
                VerdictSeverity::Critical => "critical",
                VerdictSeverity::Warning => "warning",
                VerdictSeverity::Info => "info",
                VerdictSeverity::Healthy => "healthy",
            };
            step_recorder.record_verdict(
                severity_label,
                verdict.stall_detected,
                verdict.is_diverging,
                verdict.force_stop,
                verdict.injections.len(),
            );

            // Heavy checkpoint after verdict (captures full conversation state)
            if let Some(ref sid) = current_session_id
                && let Some(heavy) = step_recorder.build_heavy_checkpoint(
                    &messages,
                    0, // budget tokens filled by caller if available
                    max_turns.saturating_sub(_turn) as u32,
                    &turn_guard
                        .health
                        .deprioritized_tools()
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>(),
                    recent_tools,
                )
            {
                let cp = mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Heavy(
                    Box::new(heavy),
                );
                let _ = mo_agent_runtime::pipeline::step_checkpoint::write_step_checkpoint(
                    sid,
                    step_recorder.summary().checkpoints,
                    &cp,
                );
                last_heavy_checkpoint = Some(cp);
            }

            // Force stop on critical verdict
            if verdict.force_stop {
                step_recorder.end_turn(true);
                return Err(
                    "Agent escalated to critical — too many errors and stalls. Aborting."
                        .to_string(),
                );
            }

            // If verdict injected stall messages, skip to next LLM call (don't re-process results)
            if !verdict.injections.is_empty() && verdict.severity >= VerdictSeverity::Warning {
                step_recorder.end_turn(false);
                tool_results = Vec::new();
                continue;
            }
        }
        step_recorder.end_turn(false);
    }

    if explain != ExplainMode::Off && !explain_turns.is_empty() && !quiet {
        print_explain_report(&explain_turns, explain == ExplainMode::Verbose);
    }
    if explain != ExplainMode::Off && !verdict_events.is_empty() && !quiet {
        print_verdict_report(&verdict_events, explain == ExplainMode::Verbose);
    }

    let elapsed = start.elapsed().as_secs_f64();
    let format_footer_tokens = |tokens: u64| -> String {
        if tokens < 1000 {
            format!("{}tok", tokens)
        } else {
            format!("{:.1}k", tokens as f64 / 1000.0)
        }
    };
    let model_tag = model.unwrap_or("auto");
    let session_tag = current_session_id
        .as_deref()
        .map(|s| if s.len() > 8 { &s[..8] } else { s })
        .unwrap_or("?");
    if verbose_mode && !quiet {
        eprintln!(
            "{}",
            format!(
                "  ⏱ {:.1}s  ↓ {}  ↑ {}  model: {}  session: {}",
                elapsed,
                if has_any_usage {
                    format_footer_tokens(total_completion)
                } else {
                    "?".to_string()
                },
                if has_any_usage {
                    format_footer_tokens(total_prompt)
                } else {
                    "?".to_string()
                },
                model_tag,
                session_tag,
            )
            .dim()
        );
    }

    let report = first_selection_report.unwrap_or_else(|| tool_registry::SelectionReport {
        tools_selected: Vec::new(),
        selected_count: 0,
        budget_used: 0,
        budget_total: 0,
    });

    // Deduplicate stall events by type (keep only one of each type per user turn).
    // The internal _turn numbers were used for in-loop deduplication; for journal
    // output, we normalize all turn numbers to 0 (repl_turn.rs will use state.turn).
    let deduped_stall_events: Vec<(String, u32)> = {
        let mut seen = std::collections::HashSet::new();
        stall_events
            .into_iter()
            .filter(|(stall_type, _)| seen.insert(stall_type.clone()))
            .map(|(stall_type, _)| (stall_type, 0)) // turn will be filled by repl_turn
            .collect()
    };

    // Deduplicate verdict events by severity (keep only the first of each severity).
    // Same rationale: internal turn numbers are loop-internal, not user turns.
    let deduped_verdict_events: Vec<VerdictEvent> = {
        let mut seen = std::collections::HashSet::new();
        verdict_events
            .into_iter()
            .filter(|ve| seen.insert(ve.severity.clone()))
            .map(|mut ve| {
                ve.turn = 0; // turn will be filled by repl_turn
                ve
            })
            .collect()
    };

    Ok(StreamResult {
        session_id: current_session_id,
        run_id: current_run_id,
        full_text: final_text,
        prompt_tokens: total_prompt,
        completion_tokens: total_completion,
        tool_calls_count: total_tool_calls,
        tools_selected: report.tools_selected,
        selected_skills: all_selected_skills,
        tools_used: all_tools_used.into_iter().collect(),
        tool_call_records,
        budget_used: report.budget_used,
        budget_pressure: first_budget_pressure,
        stall_events: deduped_stall_events,
        verdict_events: deduped_verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        // Export tool health with merged historical entries to preserve unused tools
        tool_health_export: turn_guard.health.export_merged(tool_health_entries),
        last_heavy_checkpoint,
        ttft_ms: first_ttft_ms,
        context_ms: first_context_assembly_ms,
        selector_strategy: first_selector_strategy,
        selector_ms: first_selector_ms,
        selector_tokens_in,
        selector_tokens_out,
        memoria_ms: first_memoria_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Cross-turn dedup: CACHEABLE_TOOLS constant ──

    #[test]
    fn cacheable_tools_are_all_read_only() {
        // Ensure no side-effectful tools are in the cacheable list
        const SIDE_EFFECTFUL: &[&str] = &[
            "bash",
            "write_file",
            "str_replace",
            "delete_file",
            "multi_edit",
            "git_commit",
            "git_stash",
            "git_checkout_file",
            "github_create_issue",
            "mo_query",
            "mo_snapshot",
            "mo_branch",
            "memory_store",
            "memory_purge",
            "memory_correct",
        ];
        for tool in CACHEABLE_TOOLS {
            assert!(
                !SIDE_EFFECTFUL.contains(tool),
                "CACHEABLE_TOOLS must not contain side-effectful tool: {tool}"
            );
        }
    }

    #[test]
    fn cacheable_tools_covers_git_and_github_reads() {
        // All read-only git/github tools should be cacheable
        for expected in &[
            "git_status",
            "git_diff",
            "git_log",
            "git_blame",
            "read_file",
            "grep",
            "glob",
            "list_dir",
            "github_list_prs",
            "github_get_pr",
        ] {
            assert!(
                CACHEABLE_TOOLS.contains(expected),
                "missing cacheable tool: {expected}"
            );
        }
    }

    // ── ToolCallRecord ingestion completeness ──

    /// Verify ToolCallRecord can represent all early-exit paths
    /// (duplicate, cached, unknown tool, permission denied) so that
    /// DB ingestion captures 100% of tool_calls.
    #[test]
    fn tool_call_record_covers_early_exit_paths() {
        use mo_agent_services::session_journal::ToolCallRecord;

        // Duplicate within turn
        let dup = ToolCallRecord {
            name: "read_file".to_string(),
            ok: true,
            ms: 0,
            error: Some("duplicate_within_turn".to_string()),
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("src/main.rs".to_string()),
        };
        assert!(dup.ok);
        assert_eq!(dup.ms, 0);

        // Cross-turn cache hit
        let cached = ToolCallRecord {
            name: "grep".to_string(),
            ok: true,
            ms: 0,
            error: Some("cached_cross_turn".to_string()),
            input_bytes: None,
            output_bytes: Some(500),
            args_preview: Some("/TODO/ in src/".to_string()),
        };
        assert!(cached.ok);

        // Unknown tool
        let unknown = ToolCallRecord {
            name: "nonexistent_tool".to_string(),
            ok: false,
            ms: 0,
            error: Some("unknown_tool: nonexistent_tool".to_string()),
            input_bytes: None,
            output_bytes: None,
            args_preview: None,
        };
        assert!(!unknown.ok);
        assert!(unknown.error.as_ref().unwrap().starts_with("unknown_tool:"));

        // Permission denied
        let denied = ToolCallRecord {
            name: "bash".to_string(),
            ok: false,
            ms: 0,
            error: Some("permission_denied".to_string()),
            input_bytes: None,
            output_bytes: None,
            args_preview: Some("rm -rf /".to_string()),
        };
        assert!(!denied.ok);

        // All records serialize cleanly (required for DB ingestion)
        let records = vec![dup, cached, unknown, denied];
        let json = serde_json::to_string(&records).unwrap();
        assert!(json.contains("duplicate_within_turn"));
        assert!(json.contains("cached_cross_turn"));
        assert!(json.contains("unknown_tool"));
        assert!(json.contains("permission_denied"));
    }

    /// ToolCallRecord round-trips through JSON correctly.
    #[test]
    fn tool_call_record_json_roundtrip() {
        use mo_agent_services::session_journal::ToolCallRecord;

        let original = ToolCallRecord {
            name: "web_fetch".to_string(),
            ok: true,
            ms: 42,
            error: None,
            input_bytes: Some(100),
            output_bytes: Some(5000),
            args_preview: Some("https://example.com".to_string()),
        };
        let json = serde_json::to_string(&original).unwrap();
        let restored: ToolCallRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "web_fetch");
        assert_eq!(restored.ms, 42);
        assert!(restored.ok);
        assert!(restored.error.is_none());
        // error field should be absent when None (skip_serializing_if)
        assert!(!json.contains("error"));
    }
}
