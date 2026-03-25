use super::*;

const MAX_TURNS: usize = 25;

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
    } = p;
    let start = Instant::now();
    let term_width = terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
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
    let mut all_tools_used: std::collections::HashSet<String> = std::collections::HashSet::new();

    let mut turn_sigs: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut turn_tool_names: Vec<std::collections::HashSet<String>> = Vec::new();
    let mut nudge_count: usize = 0;
    const STALL_WINDOW: usize = 3;
    const TOOL_NAME_STALL_WINDOW: usize = 4;
    const MAX_NUDGES: usize = 2;
    let mut current_run_id: Option<String> = None;

    for _turn in 0..MAX_TURNS {
        // Build request payload
        let git_branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let memoria_url = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:8100".to_string());
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
        let (turn_schemas, selection_report) = if tool_results.is_empty() {
            let turn_count = history.len() as u32 + 1;
            let sel_ctx = tool_selector::SelectionContext {
                query: message,
                turn_count,
                recent_tools,
                budget_tokens: registry.default_budget(),
            };
            let sel_result = selector.select(&sel_ctx).await;
            let (schemas, report) =
                tool_selector::resolve_schemas(&registry, &sel_result.tool_names);
            (schemas, report)
        } else {
            // Follow-up turn: use 2x budget, then pin tools already invoked.
            let turn_count = history.len() as u32 + 1;
            let sel_ctx = tool_selector::SelectionContext {
                query: message,
                turn_count,
                recent_tools,
                budget_tokens: registry.default_budget() * 2,
            };
            let sel_result = selector.select(&sel_ctx).await;
            let (mut selected, mut report) =
                tool_selector::resolve_schemas(&registry, &sel_result.tool_names);
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
            (selected, report)
        };
        if first_selection_report.is_none() {
            first_selection_report = Some(selection_report);
        }
        payload["edge_tools"] = serde_json::Value::Array(turn_schemas);
        if !tool_results.is_empty() {
            payload["tool_results"] = serde_json::Value::Array(tool_results.clone());
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
        }
        total_prompt += turn_result.prompt_tokens;
        total_completion += turn_result.completion_tokens;
        total_tool_calls += turn_result.tool_calls.len() as u32;
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
            break;
        }

        // Stall detection
        {
            let sig_set: HashSet<String> = turn_result
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
            turn_tool_names.push(name_set);

            let sig_stall = turn_sigs.len() >= STALL_WINDOW
                && turn_sigs[turn_sigs.len() - STALL_WINDOW..]
                    .windows(2)
                    .all(|w| w[0] == w[1]);
            let name_stall = turn_tool_names.len() >= TOOL_NAME_STALL_WINDOW
                && turn_tool_names[turn_tool_names.len() - TOOL_NAME_STALL_WINDOW..]
                    .windows(2)
                    .all(|w| w[0] == w[1]);

            if sig_stall || name_stall {
                if nudge_count >= MAX_NUDGES {
                    return Err(
                        "Agent stuck in loop — same tools called repeatedly. Aborting.".to_string(),
                    );
                }
                nudge_count += 1;
                messages.push(serde_json::json!({
                    "role": "user",
                    "content": prompts::STALL_NUDGE
                }));
                tool_results = Vec::new();
                continue;
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
                        "arguments": serde_json::to_string(&args).unwrap_or_default(),
                    }
                })
            }).collect::<Vec<_>>(),
        });
        if !turn_result.reasoning_content.is_empty() {
            assistant_tc_msg["reasoning_content"] =
                serde_json::Value::String(turn_result.reasoning_content.clone());
        }
        messages.push(assistant_tc_msg);

        // Deduplicate tool calls within this turn — skip exact (name, args) repeats
        let mut seen_calls: HashSet<String> = HashSet::new();

        for tc_event in &turn_result.tool_calls {
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
            let call_sig = format!(
                "{}:{}",
                name,
                serde_json::to_string(&args).unwrap_or_default()
            );
            if !seen_calls.insert(call_sig) {
                // Already ran this exact call; feed back a cached-result marker
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

            let mut result_str = executor.execute(&name, &args).await;

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
            let is_err = result_str.to_lowercase().starts_with("error");

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
    }

    if explain != ExplainMode::Off && !explain_turns.is_empty() && !quiet {
        print_explain_report(&explain_turns, explain == ExplainMode::Verbose);
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
        budget_used: report.budget_used,
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
        "commit",
        "ci ",
        "workflow",
        "pipeline",
        "merge",
        "branch",
        "release",
        "tag",
    ];
    let has_github = github_keywords.iter().any(|kw| q.contains(kw));
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
    has_github || has_code || has_web
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
    fn factual_query_rejects_general_questions() {
        assert!(!looks_like_factual_query("what is Rust?"));
        assert!(!looks_like_factual_query("explain monads"));
        assert!(!looks_like_factual_query("write a function"));
        assert!(!looks_like_factual_query("hello"));
    }

    // ── is_session_not_found_error ────────────────────────────────────────────

    #[test]
    fn session_not_found_detection() {
        assert!(is_session_not_found_error("Session not found"));
        assert!(is_session_not_found_error("error: SESSION NOT FOUND"));
        assert!(!is_session_not_found_error("authentication failed"));
        assert!(!is_session_not_found_error(""));
    }
}
