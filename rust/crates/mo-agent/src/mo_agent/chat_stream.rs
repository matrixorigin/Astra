use super::*;

use mo_agent_core::{RuntimeLimits, agent_warn};
use mo_agent_runtime::pipeline::step_protocol::{
    CachedToolResult, IdempotencyKey, InMemoryIdempotencyCache,
};

/// Build a compact workspace context string for the LLM system prompt.
/// Detects project type, key files, and top-level directory structure.
/// Capped at ~500 chars to stay token-efficient.
fn detect_workspace_context(project_root: &std::path::Path) -> serde_json::Value {
    let mut project_type = Vec::new();
    let mut key_files = Vec::new();

    // Detect project type from marker files
    let markers = [
        ("Cargo.toml", "rust"),
        ("package.json", "node/javascript"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("requirements.txt", "python"),
        ("pom.xml", "java/maven"),
        ("build.gradle", "java/gradle"),
        ("Makefile", "make"),
        ("Dockerfile", "docker"),
        ("docker-compose.yml", "docker-compose"),
        ("docker-compose.yaml", "docker-compose"),
    ];
    for (file, ptype) in markers {
        if project_root.join(file).exists() {
            if !project_type.contains(&ptype) {
                project_type.push(ptype);
            }
            key_files.push(file.to_string());
        }
    }

    // Get top-level directories (max 15, skip hidden/noise)
    let mut top_dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(project_root) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.')
                || name_str == "target"
                || name_str == "node_modules"
                || name_str == "__pycache__"
                || name_str == "dist"
                || name_str == "build"
                || name_str == "htmlcov"
            {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                top_dirs.push(format!("{}/", name_str));
            }
            if top_dirs.len() >= 15 {
                break;
            }
        }
    }

    serde_json::json!({
        "project_types": project_type,
        "key_files": key_files,
        "top_directories": top_dirs,
    })
}

/// Tools that are idempotent reads — safe to cache across turns.
/// Side-effectful tools (bash, write_file, mo_query, etc.) must NOT be in this list.
const CACHEABLE_TOOLS: &[&str] = &[
    "read_file",
    "list_dir",
    "grep",
    "glob",
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

/// Determine whether a tool result string indicates an error.
///
/// For structured JSON results (our tools), checks `"ok": false` or a non-null
/// `"error"` field. For plain-text results, falls back to `starts_with("error")`.
/// This avoids false positives from `"error": null` in success responses.
fn is_tool_error(result_str: &str) -> bool {
    // Try JSON-aware detection first
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(result_str) {
        // Structured response with boolean "ok" field (our GitHub/MO tools)
        if let Some(ok_val) = v.get("ok").and_then(|o| o.as_bool()) {
            return !ok_val;
        }
        // JSON with explicit non-null "error" field
        if let Some(err) = v.get("error") {
            return !err.is_null() && err.as_str() != Some("");
        }
    }
    // Plain-text fallback: "Error: ..." or "error ..."
    result_str.to_lowercase().starts_with("error")
}

/// Normalize a tool call signature for cache key matching.
/// - Strips trailing slashes from path-like string args
/// - Sorts object keys for deterministic hashing
/// - Normalizes whitespace in string args
fn normalize_call_sig(name: &str, args: &serde_json::Value) -> String {
    let normalized = normalize_args(args);
    format!(
        "{}:{}",
        name,
        serde_json::to_string(&normalized).unwrap_or_default()
    )
}

fn normalize_args(val: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match val {
        Value::String(s) => {
            // Normalize paths: strip trailing slashes, collapse double slashes
            let trimmed = s.trim();
            let normalized = trimmed.trim_end_matches('/');
            Value::String(normalized.to_string())
        }
        Value::Object(map) => {
            // Sort keys for deterministic serialization
            let mut sorted: serde_json::Map<String, Value> = serde_json::Map::new();
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for k in keys {
                sorted.insert(k.clone(), normalize_args(&map[k]));
            }
            Value::Object(sorted)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(normalize_args).collect()),
        other => other.clone(),
    }
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
    pub(super) client: &'a reqwest::Client,
    pub(super) base: &'a str,
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
}

/// Full edge-cloud agentic loop: sends message, executes tools, loops until done.
pub(super) async fn stream_chat_sse(p: ChatTurnParams<'_>) -> Result<StreamResult, String> {
    // Destructure for readability within the function body
    let ChatTurnParams {
        client,
        base,
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
    } = p;
    let start = Instant::now();
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let file_context = detect_project_languages(&project_root);
    let executor = edge_tools::ToolExecutor::new(&project_root).with_cloud(base, token);
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
    // Step Protocol recorder: maps implicit chat_stream phases to explicit Step events
    let mut step_recorder =
        mo_agent_runtime::pipeline::step_recorder::StepRecorder::with_persistence(
            current_session_id.as_deref().unwrap_or("ephemeral"),
            &format!("chat-{}", start.elapsed().as_millis()),
        );

    for _turn in 0..max_turns {
        if remaining_turns == 0 {
            return Err("Turn budget exhausted due to repeated stalls. Aborting.".to_string());
        }
        remaining_turns = remaining_turns.saturating_sub(1);
        step_recorder.begin_turn(_turn as u32);
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
            let memory_contents = executor.memory_boost_search(message, 5).await;
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

        let (turn_schemas, selection_report, selection_confidence) = if tool_results.is_empty() {
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
            let sel_result = selector.select(&sel_ctx).await;
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
            let sel_result = selector.select(&sel_ctx).await;
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
        if first_selection_report.is_none() {
            first_selection_report = Some(selection_report);
            first_budget_pressure = budget_pressure;
        }

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

        // HTTP call with retry on 429 (rate limit) — exponential backoff up to 3 attempts.
        let mut resp_result = None;
        for attempt in 0..3u32 {
            let resp = client
                .post(format!("{base}/chat/turn"))
                .headers(auth_headers(token)?)
                .header("Accept", "text/event-stream")
                .json(&payload)
                .send()
                .await
                .map_err(|e| e.to_string())?;

            if resp.status().as_u16() == 429 && attempt < 2 {
                let delay_secs = 2u64 << attempt; // 2s, 4s
                if !quiet {
                    eprintln!("  ⏳ Rate limited (429), retrying in {}s…", delay_secs);
                }
                tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                continue;
            }
            resp_result = Some(resp);
            break;
        }
        let resp = resp_result.ok_or_else(|| "retry exhausted".to_string())?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.map_err(|e| e.to_string())?;
            return Err(format!("API Error ({}): {}", status, compact_or_raw(&body)));
        }

        let turn_result = consume_turn_sse(resp, render_md, term_width, quiet).await;

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
        }
        total_prompt += turn_result.prompt_tokens;
        total_completion += turn_result.completion_tokens;
        total_tool_calls += turn_result.tool_calls.len() as u32;

        // Record LLM token usage in step recorder
        step_recorder.record_tokens(turn_result.prompt_tokens, turn_result.completion_tokens);
        // Track all unique tool names that the LLM actually invoked
        for tc in &turn_result.tool_calls {
            if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                all_tools_used.insert(name.to_string());
            }
        }
        has_any_usage = has_any_usage || turn_result.has_usage;
        explain_turns.extend(turn_result.explain_turns);

        if let Some(ref err) = turn_result.error_message {
            return Err(err.clone());
        }

        if !turn_result.has_tool_calls {
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

        // Stall & divergence detection via unified TurnGuard
        {
            use std::collections::BTreeSet;

            let sig_set: BTreeSet<String> = turn_result
                .tool_calls
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
            let name_set: HashSet<String> = turn_result
                .tool_calls
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
            turn_guard.record_tool_calls(&turn_result.tool_calls);

            // Name-based stall detection (complementary to TurnGuard's signature stall)
            let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
                && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
                    .windows(2)
                    .all(|w| w[0] == w[1]);

            if name_stall {
                stall_events.push(("name_stall".to_string(), _turn as u32));
            }
        }

        // Execute tool calls locally
        tool_results = Vec::new();
        // Don't clear messages — keep full history. Append assistant tool_calls message.
        // Include reasoning_content when present: thinking models (Kimi-k2.5, DeepSeek-R1)
        // require it on subsequent turns or they return HTTP 400.
        let mut assistant_tc_msg = serde_json::json!({
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
        });
        if !turn_result.reasoning_content.is_empty() {
            assistant_tc_msg["reasoning_content"] =
                serde_json::Value::String(turn_result.reasoning_content.clone());
        }
        messages.push(assistant_tc_msg);

        // Deduplicate tool calls — skip exact (name, args) repeats within AND across turns
        // Only cache idempotent read-only tools; side-effectful tools always re-execute
        let mut seen_calls: HashSet<String> = HashSet::new();
        let tool_count = turn_result.tool_calls.len();
        step_recorder.begin_act(tool_count);
        let step_start_time = std::time::Instant::now();
        let step_timeout_ms = step_recorder.scheduling().timeout_ms;

        for tc_event in &turn_result.tool_calls {
            // Step-level timeout: abort remaining tools if step time exceeded
            let step_elapsed_ms = step_start_time.elapsed().as_millis() as u64;
            if step_elapsed_ms > step_timeout_ms {
                // Collect names of aborted tools for health tracking
                let aborted_count = turn_result.tool_calls.len() - tool_results.len();
                let aborted_tools: Vec<String> = turn_result.tool_calls[tool_results.len()..]
                    .iter()
                    .filter_map(|tc| tc.get("name").and_then(|v| v.as_str()).map(String::from))
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
            let args = tc_event
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::Value::Object(Default::default()));

            // Skip exact duplicate (same tool + same args) within this turn
            let call_sig = normalize_call_sig(&name, &args);
            if !seen_calls.insert(call_sig.clone()) {
                // Already ran this exact call this turn
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
                continue;
            }

            // Cross-turn dedup: for idempotent tools, check IdempotencyCache
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
                // Record cache hit with full slot tracking
                let cache_key = idem_key.cache_key();
                step_recorder.begin_tool_with_key(&name, &id, Some(&cache_key));
                step_recorder.record_cache_hit(&name, cached.clone());
                turn_guard.record_cache_hit(&name);
                continue;
            }

            // Validate tool name against known schemas
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
                continue;
            }

            if !perm_manager.check(&name, &args) {
                let denied_tr = serde_json::json!({
                    "tool_call_id": id,
                    "name": name,
                    "result": "Permission denied",
                });
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": "Permission denied",
                }));
                tool_results.push(denied_tr);
                continue;
            }

            // Start spinner
            let spinner = if !quiet {
                Some(Spinner::start(format!("  ● {name}")))
            } else {
                None
            };
            let tool_start = Instant::now();
            let tool_idem_key = if CACHEABLE_TOOLS.contains(&name.as_str()) {
                Some(idem_key.cache_key())
            } else {
                None
            };
            step_recorder.begin_tool_with_key(&name, &id, tool_idem_key.as_deref());

            // Enforce per-tool timeout from scheduling contract,
            // reconciled with RuntimeLimits for the more restrictive policy.
            let contract = step_recorder.scheduling();
            let limits = RuntimeLimits::global();
            let tool_timeout_ms = contract.effective_tool_timeout_ms(tool_count);
            let effective_retries = (contract.max_retries as usize).min(limits.max_tool_retries);
            let mut tool_timed_out = false;
            let mut result_str = match tokio::time::timeout(
                std::time::Duration::from_millis(tool_timeout_ms),
                executor.execute(&name, &args),
            )
            .await
            {
                Ok(r) => r,
                Err(_) => {
                    tool_timed_out = true;
                    turn_guard.record_tool_timeout(&name);
                    format!(
                        "Tool '{}' took too long (>{}s). Consider retrying.",
                        name,
                        tool_timeout_ms / 1000
                    )
                }
            };

            // If the `reflect` tool returned a placeholder, call the server.
            if name == "reflect"
                && result_str.contains("reflect_requires_session")
                && let Some(ref sid) = current_session_id
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
                let reflect_url = format!("{base}/chat/session/{sid}/reflect?{}", qp.join("&"));
                match auth_headers(token) {
                    Ok(hdrs) => match client.get(&reflect_url).headers(hdrs).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            result_str = resp.text().await.unwrap_or(result_str);
                        }
                        Ok(resp) => {
                            result_str =
                                format!("{{\"error\": \"reflect HTTP {}\"}}", resp.status());
                        }
                        Err(e) => {
                            result_str = format!("{{\"error\": \"reflect failed: {e}\"}}");
                        }
                    },
                    Err(e) => {
                        result_str = format!("{{\"error\": \"reflect auth: {e}\"}}");
                    }
                }
            }
            let tool_elapsed = tool_start.elapsed();
            let is_err = is_tool_error(&result_str);

            // Error recovery: classify, retry transient errors, track via TurnGuard
            if is_err {
                use mo_agent_runtime::turn::error_recovery::{
                    build_recovery_message, classify_error,
                };
                let category = classify_error(&result_str);
                turn_guard.errors.record_error(category);

                // Automatic retry for transient errors using scheduling contract
                let mut retried_ok = false;
                if matches!(
                    category,
                    mo_agent_runtime::turn::error_recovery::ErrorCategory::Transient
                ) {
                    for attempt in 0..effective_retries {
                        let backoff_ms = contract.backoff_ms(attempt as u32);
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                        // Retry with same timeout as original attempt
                        let retry_result = match tokio::time::timeout(
                            std::time::Duration::from_millis(tool_timeout_ms),
                            executor.execute(&name, &args),
                        )
                        .await
                        {
                            Ok(r) => r,
                            Err(_) => format!(
                                "Tool '{}' retry #{} took too long (>{}s)",
                                name,
                                attempt + 1,
                                tool_timeout_ms / 1000
                            ),
                        };
                        if !is_tool_error(&retry_result) {
                            result_str = retry_result;
                            retried_ok = true;
                            turn_guard.errors.record_retry(true);
                            if !quiet {
                                eprintln!(
                                    "{}",
                                    format!("  ↻ {name} retry #{} succeeded", attempt + 1).green()
                                );
                            }
                            break;
                        }
                        turn_guard.errors.record_retry(false);
                    }
                }

                if !retried_ok {
                    // Inject structured recovery message with alternatives
                    let deprioritized = turn_guard.health.deprioritized_tools();
                    let recovery_msg =
                        build_recovery_message(&name, &result_str, category, &deprioritized);
                    result_str.push_str(&format!("\n{recovery_msg}"));
                }
            }

            // Record result in TurnGuard (handles health tracking + quality classification).
            // Skip if already recorded as timeout — avoid double-counting.
            let result_quality = if tool_timed_out {
                mo_agent_runtime::turn::result_quality::ResultQuality::Error
            } else {
                turn_guard.record_tool_result(&name, &result_str)
            };

            // Inject immediate feedback for empty/truncated results
            if let Some(feedback) = turn_guard.result_feedback(&name, result_quality) {
                result_str.push_str(&format!("\n{feedback}"));
            }

            // Record per-tool-call audit entry
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
            });
            step_recorder.complete_tool_with_result(
                &name,
                is_err,
                tool_elapsed.as_millis() as u64,
                false,
                &result_str,
            );

            // Light checkpoint after each tool completion (best-effort, non-blocking)
            if let Some(ref sid) = current_session_id {
                if let Some(light) = step_recorder.build_light_checkpoint() {
                    let cp =
                        mo_agent_runtime::pipeline::step_protocol::StepCheckpoint::Light(light);
                    let _ = mo_agent_runtime::pipeline::step_checkpoint::write_step_checkpoint(
                        sid,
                        step_recorder.summary().checkpoints,
                        &cp,
                    );
                }
            }

            // Stop spinner, print final status with duration
            if let Some(spinner) = spinner {
                spinner.stop_clear();
            }
            let duration_str = if tool_elapsed.as_secs_f64() >= 1.0 {
                format!("{:.1}s", tool_elapsed.as_secs_f64())
            } else {
                format!("{}ms", tool_elapsed.as_millis())
            };

            // Build a brief detail from tool args for the └ line
            let detail = tool_call_detail(&name, &args);

            if is_err {
                if !quiet {
                    eprintln!("{}", format!("  ✗ {name} ({duration_str})").red());
                }
                // Show first line of error on └ line
                if !quiet && let Some(first_line) = result_str.lines().next() {
                    let preview = if first_line.len() > 100 {
                        format!("{}…", &first_line[..100])
                    } else {
                        first_line.to_string()
                    };
                    eprintln!("  {}", format!("└ {preview}").dim());
                }
            } else {
                if !quiet {
                    eprintln!("{}", format!("  ✓ {name} ({duration_str})").green());
                }
                if !quiet && let Some(d) = &detail {
                    eprintln!("  {}", format!("└ {d}").dim());
                }
            }

            // Cache successful idempotent tool results via IdempotencyCache
            if !is_err && CACHEABLE_TOOLS.contains(&name.as_str()) {
                let cached_result = CachedToolResult {
                    tool_name: name.clone(),
                    output: result_str.clone(),
                    is_error: false,
                    cached_at: mo_agent_runtime::pipeline::step_protocol::epoch_ms(),
                };
                step_recorder.attach_cached_result(cached_result.clone());
                idempotency_cache.record(&idem_key, cached_result);
                // Record in semantic tracker for near-duplicate detection in future turns
                if let Some((prev_turn, reason)) =
                    semantic_dedup.check_and_record(&name, &args, &result_str, _turn)
                {
                    // Inject a hint so the LLM knows it's re-fetching similar data
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
            let tr = serde_json::json!({
                "tool_call_id": id,
                "name": name,
                "result": result_str,
            });
            // Append tool result as a "tool" role message in history
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_str,
            }));
            tool_results.push(tr);
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

            // Apply turn budget penalties based on severity
            match verdict.severity {
                VerdictSeverity::Critical => {
                    remaining_turns = remaining_turns.saturating_sub(5);
                    stall_events.push(("critical_escalation".to_string(), _turn as u32));
                }
                VerdictSeverity::Warning => {
                    remaining_turns = remaining_turns.saturating_sub(2);
                    stall_events.push(("warning".to_string(), _turn as u32));
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
            if let Some(ref sid) = current_session_id {
                if let Some(heavy) = step_recorder.build_heavy_checkpoint(
                    &messages,
                    0, // budget tokens filled by caller if available
                    max_turns.saturating_sub(_turn) as u32,
                    &turn_guard
                        .health
                        .deprioritized_tools()
                        .iter()
                        .map(|s| s.to_string())
                        .collect::<Vec<_>>(),
                    &recent_tools,
                ) {
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

    Ok(StreamResult {
        session_id: current_session_id,
        run_id: current_run_id,
        full_text: final_text,
        prompt_tokens: total_prompt,
        completion_tokens: total_completion,
        tool_calls_count: total_tool_calls,
        tools_selected: report.tools_selected,
        tools_used: all_tools_used.into_iter().collect(),
        tool_call_records,
        budget_used: report.budget_used,
        budget_pressure: first_budget_pressure,
        stall_events,
        verdict_events,
        step_recorder_summary: Some(step_recorder.summary()),
        tool_health_export: turn_guard.health.export(),
        last_heavy_checkpoint,
    })
}

pub(super) fn is_session_not_found_error(error: &str) -> bool {
    error.to_lowercase().contains("session not found")
}

/// Detect queries that almost certainly need tool calls to answer correctly.
/// Used for the hallucination guard: if LLM answers these with 0 tool calls,
/// the response is likely fabricated.
pub(super) fn looks_like_factual_query(input: &str) -> bool {
    let q = input.to_lowercase();
    // GitHub data queries
    let github_keywords = [
        "pr",
        "pull request",
        "issue",
        "拉取请求",
        "问题",
        "commit",
        "提交",
        "ci ",
        " ci?",
        "ci状态",
        "最新的一个ci",
        "workflow",
        "工作流",
        "pipeline",
        "merge",
        "branch",
        "分支",
        "release",
        "tag",
        "star",
        "stars",
        "多少star",
    ];
    let has_github = github_keywords.iter().any(|kw| q.contains(kw));
    let memory_keywords = ["记忆", "memory", "memories", "存了什么", "记住了什么"];
    let has_memory = memory_keywords.iter().any(|kw| q.contains(kw));
    let git_live_keywords = [
        "git status",
        "git diff",
        "改了什么",
        "有哪些修改",
        "当前有哪些修改",
    ];
    let has_git_live = git_live_keywords.iter().any(|kw| q.contains(kw));
    // File/code queries
    let code_keywords = [
        "read file",
        "cat ",
        "show me the code",
        "what's in",
        "file content",
    ];
    let has_code = code_keywords.iter().any(|kw| q.contains(kw));
    // Web/API queries
    let web_keywords = ["http", "url", "api ", "endpoint", "fetch", "download"];
    let has_web = web_keywords.iter().any(|kw| q.contains(kw));
    has_github || has_memory || has_git_live || has_code || has_web
}

fn recent_tools_imply_live_domain(recent_tools: &[String]) -> bool {
    recent_tools.iter().any(|tool| {
        tool.starts_with("github_")
            || tool.starts_with("memory_")
            || matches!(tool.as_str(), "git_status" | "git_diff")
    })
}

pub(super) fn looks_like_live_query_with_context(input: &str, recent_tools: &[String]) -> bool {
    if looks_like_factual_query(input) {
        return true;
    }

    if !recent_tools_imply_live_domain(recent_tools) {
        return false;
    }

    let q = input.trim().to_lowercase();
    let is_short_followup = q.chars().count() <= 12;
    if !is_short_followup {
        return false;
    }

    [
        "最新",
        "latest",
        "那",
        "呢",
        "还有",
        "然后",
        "继续",
        "what about",
        "how about",
    ]
    .iter()
    .any(|kw| q.contains(kw))
}

fn should_force_factual_tool_retry(
    input: &str,
    recent_tools: &[String],
    total_tool_calls: u32,
    already_retried: bool,
) -> bool {
    !already_retried
        && total_tool_calls == 0
        && looks_like_live_query_with_context(input, recent_tools)
}

fn factual_tool_retry_message(original_query: &str) -> String {
    format!(
        "Runtime correction: your previous draft answered a live/factual query without using tools. Retry this turn from scratch and call at least one tool before answering.\n\
\n\
- For GitHub live data prefer github_ci_status / github_list_prs / github_list_issues / github_repo_stats.\n\
- For memory contents use memory_search or memory_profile.\n\
- For workspace change status use git_status or git_diff.\n\
- Do NOT fall back to bash when a dedicated GitHub or memory tool exists.\n\
- If repo was omitted before, infer it from the user's text or recent conversation. Bare names like 'memoria' and 'matrixone' are allowed.\n\
\n\
Original user query: {original_query}"
    )
}

/// Extract `owner/repo` patterns from memory text.
///
/// Matches GitHub-style references like "matrixorigin/Memoria", "user/project-name".
/// Also recognizes org names from patterns like "follows {org}" or "tracks {org}/{repo}".
/// Returns deduplicated Vec of "owner/repo" strings.
fn extract_repos_from_memory(text: &str) -> Vec<String> {
    use std::sync::LazyLock;

    static GITHUB_URL_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)github\.com/([a-zA-Z0-9][\w-]{0,38})/([a-zA-Z0-9][\w.-]{0,99})")
            .expect("github url regex")
    });

    static BARE_REPO_RE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)\b([a-zA-Z0-9][\w-]{0,38})/([a-zA-Z0-9][\w.-]{0,99})\b")
            .expect("repo regex")
    });

    let mut repos = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut add = |owner: &str, repo: &str| {
        let full = format!("{owner}/{repo}");
        let key = full.to_lowercase();
        if seen.insert(key) {
            repos.push(full);
        }
    };

    // 1. Extract from GitHub URLs: github.com/{owner}/{repo}
    for cap in GITHUB_URL_RE.captures_iter(text) {
        add(&cap[1], &cap[2]);
    }

    // 2. Match bare owner/repo patterns (e.g., "matrixorigin/Memoria")
    for cap in BARE_REPO_RE.captures_iter(text) {
        let owner = &cap[1];
        let repo = &cap[2];
        // Skip protocols, paths, domains
        if [
            "http", "https", "ftp", "ssh", "git", "usr", "etc", "var", "tmp", "home",
        ]
        .contains(&owner.to_lowercase().as_str())
        {
            continue;
        }
        if owner.contains('.') {
            continue;
        }
        // Skip tag-like patterns (e.g., "@pref/active")
        // cap.get(0) always succeeds — group 0 is the full match from captures_iter
        let match_start = cap.get(0).expect("group 0 always exists").start();
        if text[..match_start].ends_with('@') {
            continue;
        }
        add(owner, repo);
    }

    repos
}

/// Detect project languages/frameworks from workspace marker files.
/// Returns tags like "rust", "typescript", "python", "go", "java", etc.
fn detect_project_languages(root: &std::path::Path) -> Vec<String> {
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("package.json", "javascript"),
        ("tsconfig.json", "typescript"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("go.mod", "go"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "kotlin"),
        ("Gemfile", "ruby"),
        ("mix.exs", "elixir"),
        ("CMakeLists.txt", "cpp"),
        ("Makefile", "make"),
        (".csproj", "csharp"),
        ("composer.json", "php"),
        ("Dockerfile", "docker"),
    ];
    let mut langs = Vec::new();
    for &(file, lang) in markers {
        if root.join(file).exists() {
            langs.push(lang.to_string());
        }
    }
    // Check for *.csproj in root (glob-style, since filename varies)
    if langs.iter().all(|l| l != "csharp") {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if name.ends_with(".csproj") || name.ends_with(".sln") {
                        langs.push("csharp".to_string());
                        break;
                    }
                }
            }
        }
    }
    langs.dedup();
    langs
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── looks_like_factual_query ──────────────────────────────────────────────

    #[test]
    fn factual_query_detects_github_keywords() {
        assert!(looks_like_factual_query("show me the latest PR"));
        assert!(looks_like_factual_query("list open issues"));
        assert!(looks_like_factual_query("check CI status"));
        assert!(looks_like_factual_query("what's in the commit?"));
        assert!(looks_like_factual_query("workflow status"));
        assert!(looks_like_factual_query("最新的一个ci?"));
        assert!(looks_like_factual_query("多少star了？"));
        assert!(looks_like_factual_query("pr呢？"));
    }

    #[test]
    fn factual_query_detects_file_keywords() {
        assert!(looks_like_factual_query("read file src/main.rs"));
        assert!(looks_like_factual_query("cat the config"));
        assert!(looks_like_factual_query("show me the code in lib.rs"));
    }

    #[test]
    fn factual_query_detects_web_keywords() {
        assert!(looks_like_factual_query("fetch the API endpoint"));
        assert!(looks_like_factual_query("check http://example.com"));
    }

    #[test]
    fn factual_query_detects_memory_and_git_live_queries() {
        assert!(looks_like_factual_query("我有哪些记忆？"));
        assert!(looks_like_factual_query("当前有哪些修改？"));
        assert!(looks_like_factual_query("改了什么，看一眼"));
    }

    #[test]
    fn factual_query_rejects_general_questions() {
        assert!(!looks_like_factual_query("what is Rust?"));
        assert!(!looks_like_factual_query("explain monads"));
        assert!(!looks_like_factual_query("write a function"));
        assert!(!looks_like_factual_query("hello"));
    }

    #[test]
    fn force_retry_only_for_first_zero_tool_factual_answer() {
        let none: Vec<String> = vec![];
        assert!(should_force_factual_tool_retry(
            "最新的一个ci?",
            &none,
            0,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            "最新的一个ci?",
            &none,
            1,
            false
        ));
        assert!(!should_force_factual_tool_retry(
            "最新的一个ci?",
            &none,
            0,
            true
        ));
        assert!(!should_force_factual_tool_retry("hello", &none, 0, false));
    }

    #[test]
    fn contextual_live_query_detects_short_followup() {
        let recent = vec!["github_ci_status".to_string()];
        assert!(looks_like_live_query_with_context("最新的", &recent));
        assert!(looks_like_live_query_with_context("pr呢？", &recent));
        assert!(!looks_like_live_query_with_context("hello", &recent));
    }

    #[test]
    fn factual_retry_message_guides_toward_dedicated_tools() {
        let msg = factual_tool_retry_message("memoria 最新的一个ci?");
        assert!(msg.contains("github_ci_status"));
        assert!(msg.contains("github_repo_stats"));
        assert!(msg.contains("memoria"));
        assert!(msg.contains("Do NOT fall back to bash"));
    }

    // ── is_session_not_found_error ────────────────────────────────────────────

    #[test]
    fn session_not_found_detection() {
        assert!(is_session_not_found_error("Session not found"));
        assert!(is_session_not_found_error("error: SESSION NOT FOUND"));
        assert!(!is_session_not_found_error("authentication failed"));
        assert!(!is_session_not_found_error(""));
    }

    // ── is_tool_error ──────────────────────────────────────────────────────────

    #[test]
    fn tool_error_success_with_null_error_is_not_error() {
        // GitHub tool success response: "error": null should NOT be an error
        let result = r#"{"ok":true,"tool":"github_list_prs","error":null,"count":6}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_ok_false_is_error() {
        let result = r#"{"ok":false,"tool":"github_ci_status","error":"missing repo"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_non_null_error_field_is_error() {
        // JSON without "ok" field but with non-null error
        let result = r#"{"error":"connection refused"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_plain_text_error() {
        assert!(is_tool_error("Error: command not found"));
        assert!(is_tool_error("error reading file"));
    }

    #[test]
    fn tool_error_plain_text_success() {
        assert!(!is_tool_error("file contents here"));
        assert!(!is_tool_error("{}"));
        assert!(!is_tool_error("[]"));
    }

    #[test]
    fn tool_error_empty_error_string_is_not_error() {
        let result = r#"{"error":""}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_ok_true_with_error_string_trusts_ok_field() {
        // If "ok" is present, it takes precedence
        let result = r#"{"ok":true,"error":"leftover field"}"#;
        assert!(!is_tool_error(result));
    }

    // ── is_tool_error edge cases ──

    #[test]
    fn tool_error_nested_error_key_is_not_error() {
        // Only top-level "error" matters — nested doesn't trigger
        let result = r#"{"ok":true,"data":{"error":"some inner issue"}}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_array_response_is_not_error() {
        let result = r#"[{"name":"pr1"},{"name":"pr2"}]"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_error_count_field_is_not_error() {
        // Field named "error_count" shouldn't trigger; only "error" exactly
        let result = r#"{"error_count":0,"status":"ok"}"#;
        assert!(!is_tool_error(result));
    }

    #[test]
    fn tool_error_html_error_page() {
        // HTTP 5xx returning HTML
        let result = "<html><body>error 502 Bad Gateway</body></html>";
        assert!(!is_tool_error(result)); // doesn't start with "error"
    }

    #[test]
    fn tool_error_unicode_error_message() {
        let result = r#"{"ok":false,"error":"连接被拒绝"}"#;
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_ok_as_string_not_boolean() {
        // "ok": "false" (string, not boolean) — should still check as false
        let result = r#"{"ok":"false","error":"something"}"#;
        // ok field is not a boolean — should fall through to error field check
        assert!(is_tool_error(result));
    }

    #[test]
    fn tool_error_empty_string_is_not_error() {
        assert!(!is_tool_error(""));
    }

    #[test]
    fn tool_error_whitespace_is_not_error() {
        assert!(!is_tool_error("   \n\t  "));
    }

    // ── Cross-turn dedup: CACHEABLE_TOOLS constant ──

    #[test]
    fn cacheable_tools_are_all_read_only() {
        // Ensure no side-effectful tools are in the cacheable list
        const SIDE_EFFECTFUL: &[&str] = &[
            "bash",
            "write_file",
            "str_replace",
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

    // ── extract_repos_from_memory ───────────────────────────────────────────

    #[test]
    fn extract_repos_explicit_owner_repo() {
        let text = "user follows matrixorigin/Memoria and wants to track their projects";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos, vec!["matrixorigin/Memoria"]);
    }

    #[test]
    fn extract_repos_multiple() {
        let text = "tracks matrixorigin/Memoria and also watches rust-lang/rust";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains(&"matrixorigin/Memoria".to_string()));
        assert!(repos.contains(&"rust-lang/rust".to_string()));
    }

    #[test]
    fn extract_repos_dedup() {
        let text = "matrixorigin/Memoria and MATRIXORIGIN/memoria again";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos.len(), 1, "should deduplicate case-insensitively");
    }

    #[test]
    fn extract_repos_skips_tag_namespaces() {
        let text = "[@pref/active] user follows matrixorigin/Memoria";
        let repos = extract_repos_from_memory(text);
        assert_eq!(repos, vec!["matrixorigin/Memoria"]);
        assert!(
            !repos.iter().any(|r| r.contains("pref")),
            "should not extract @pref/active as a repo"
        );
    }

    #[test]
    fn extract_repos_skips_protocols() {
        let text = "see https://github.com/matrixorigin/Memoria for details";
        let repos = extract_repos_from_memory(text);
        // Should extract matrixorigin/Memoria but NOT https/github.com
        assert!(repos.iter().any(|r| r == "matrixorigin/Memoria"));
        assert!(!repos.iter().any(|r| r.to_lowercase().contains("http")));
    }

    #[test]
    fn extract_repos_empty_for_no_repos() {
        let text = "user prefers concise responses and dark mode";
        let repos = extract_repos_from_memory(text);
        assert!(repos.is_empty());
    }

    #[test]
    fn extract_repos_handles_hyphen() {
        let text = "watching my-org/my-project and also some-user/cool-lib";
        let repos = extract_repos_from_memory(text);
        assert!(repos.iter().any(|r| r == "my-org/my-project"));
        assert!(repos.iter().any(|r| r == "some-user/cool-lib"));
    }

    // ── Argument normalization tests ──

    #[test]
    fn normalize_call_sig_strips_trailing_slash() {
        let args = serde_json::json!({"path": "src/", "pattern": "*.rs"});
        let sig1 = normalize_call_sig("glob", &args);
        let args2 = serde_json::json!({"path": "src", "pattern": "*.rs"});
        let sig2 = normalize_call_sig("glob", &args2);
        assert_eq!(sig1, sig2, "trailing slash should be normalized");
    }

    #[test]
    fn normalize_call_sig_sorts_keys() {
        let args1 = serde_json::json!({"b": "2", "a": "1"});
        let args2 = serde_json::json!({"a": "1", "b": "2"});
        let sig1 = normalize_call_sig("test", &args1);
        let sig2 = normalize_call_sig("test", &args2);
        assert_eq!(sig1, sig2, "key order should not affect signature");
    }

    #[test]
    fn normalize_call_sig_preserves_distinct_args() {
        let args1 = serde_json::json!({"file": "a.rs"});
        let args2 = serde_json::json!({"file": "b.rs"});
        let sig1 = normalize_call_sig("read_file", &args1);
        let sig2 = normalize_call_sig("read_file", &args2);
        assert_ne!(sig1, sig2, "different args should produce different sigs");
    }

    #[test]
    fn normalize_call_sig_trims_whitespace() {
        let args1 = serde_json::json!({"query": " hello world "});
        let args2 = serde_json::json!({"query": "hello world"});
        let sig1 = normalize_call_sig("search", &args1);
        let sig2 = normalize_call_sig("search", &args2);
        assert_eq!(sig1, sig2, "whitespace should be normalized");
    }

    #[test]
    fn normalize_args_handles_nested_objects() {
        let args = serde_json::json!({"outer": {"z": 1, "a": 2}});
        let norm = normalize_args(&args);
        // Keys should be sorted even in nested objects
        let keys: Vec<&String> = norm["outer"].as_object().unwrap().keys().collect();
        assert_eq!(keys, vec!["a", "z"]);
    }

    #[test]
    fn normalize_args_preserves_numbers_and_bools() {
        let args = serde_json::json!({"count": 5, "verbose": true});
        let norm = normalize_args(&args);
        assert_eq!(norm["count"], 5);
        assert_eq!(norm["verbose"], true);
    }

    // ── detect_project_languages ─────────────────────────────────────────────

    #[test]
    fn detect_project_languages_finds_cargo_toml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.contains(&"rust".to_string()));
    }

    #[test]
    fn detect_project_languages_finds_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM rust").unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.contains(&"javascript".to_string()));
        assert!(langs.contains(&"docker".to_string()));
    }

    #[test]
    fn detect_project_languages_empty_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.is_empty());
    }

    #[test]
    fn detect_project_languages_typescript_from_tsconfig() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("tsconfig.json"), "{}").unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.contains(&"typescript".to_string()));
    }

    // ── workspace context detection ──────────────────────────────────────────

    #[test]
    fn workspace_context_detects_rust_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::create_dir(tmp.path().join("tests")).unwrap();

        let ctx = detect_workspace_context(tmp.path());
        let types = ctx["project_types"].as_array().unwrap();
        assert!(
            types.iter().any(|v| v.as_str() == Some("rust")),
            "should detect rust, got: {ctx}"
        );
        assert!(
            ctx["key_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("Cargo.toml")),
            "should list Cargo.toml, got: {ctx}"
        );
        let dirs = ctx["top_directories"].as_array().unwrap();
        assert!(
            dirs.iter().any(|v| v.as_str() == Some("src/")),
            "should list src/, got: {ctx}"
        );
    }

    #[test]
    fn workspace_context_detects_multiple_project_types() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("Makefile"), "").unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "").unwrap();

        let ctx = detect_workspace_context(tmp.path());
        let types = ctx["project_types"].as_array().unwrap();
        assert!(types.len() >= 3, "should detect 3+ types, got: {ctx}");
    }

    #[test]
    fn workspace_context_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = detect_workspace_context(tmp.path());
        let types = ctx["project_types"].as_array().unwrap();
        assert!(types.is_empty(), "empty dir should have no project types");
        let dirs = ctx["top_directories"].as_array().unwrap();
        assert!(dirs.is_empty(), "empty dir should have no dirs");
    }

    #[test]
    fn workspace_context_skips_hidden_and_noise() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::create_dir(tmp.path().join("target")).unwrap();
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();

        let ctx = detect_workspace_context(tmp.path());
        let dirs = ctx["top_directories"].as_array().unwrap();
        let dir_strs: Vec<&str> = dirs.iter().filter_map(|v| v.as_str()).collect();
        assert!(!dir_strs.contains(&".git/"), "should skip .git");
        assert!(!dir_strs.contains(&"target/"), "should skip target");
        assert!(!dir_strs.contains(&"node_modules/"), "should skip node_modules");
        assert!(dir_strs.contains(&"src/"), "should include src/");
    }
}
