use astra_services::session_journal::{JournalEvent, ToolCallRecord};
use astra_text_utils::str_preview::truncate_str;
use astra_turn_core::context_assembly_trace::{ContextAssemblyTrace, DecisionType};

#[derive(Debug, Clone)]
pub(crate) struct ExplainTurnMeta<'a> {
    pub(crate) turn_label: Option<String>,
    pub(crate) duration_ms: Option<u64>,
    pub(crate) ttft_ms: Option<u64>,
    pub(crate) context_ms: Option<u64>,
    pub(crate) memoria_ms: Option<u64>,
    pub(crate) total_llm_ms: Option<u64>,
    pub(crate) total_tool_ms: Option<u64>,
    pub(crate) prompt_tokens: Option<u64>,
    pub(crate) completion_tokens: Option<u64>,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_creation_tokens: Option<u64>,
    pub(crate) tool_count: Option<u32>,
    pub(crate) llm_rounds: Option<u32>,
    pub(crate) routing_domain_hint: Option<String>,
    pub(crate) assistant_output: Option<&'a str>,
    pub(crate) tool_call_records: &'a [ToolCallRecord],
    pub(crate) selection_strategy: Option<String>,
    pub(crate) selection_confidence: Option<f64>,
    pub(crate) selected_tools: Vec<String>,
}

impl<'a> ExplainTurnMeta<'a> {
    pub(crate) fn from_journal_event(event: &'a JournalEvent) -> Self {
        let selection = event.selection_trace.as_ref();
        Self {
            turn_label: event.turn.map(|turn| format!("turn-{turn}")),
            duration_ms: event.duration_ms,
            ttft_ms: event.ttft_ms,
            context_ms: event.context_ms,
            memoria_ms: event.memoria_ms,
            total_llm_ms: event.total_llm_ms,
            total_tool_ms: event.total_tool_ms,
            prompt_tokens: event.tokens_in,
            completion_tokens: event.tokens_out,
            cache_read_tokens: event.cache_read_tokens,
            cache_creation_tokens: event.cache_creation_tokens,
            tool_count: event.tool_count,
            llm_rounds: event.llm_rounds,
            routing_domain_hint: event.routing_domain_hint.clone(),
            assistant_output: event.assistant_output.as_deref(),
            tool_call_records: event.tool_calls.as_deref().unwrap_or(&[]),
            selection_strategy: selection.map(|trace| trace.strategy.clone()),
            selection_confidence: selection.map(|trace| trace.confidence),
            selected_tools: selection
                .map(|trace| trace.final_tools.clone())
                .unwrap_or_default(),
        }
    }
}

pub(crate) fn context_trace_from_json(
    trace_json: &serde_json::Value,
) -> Option<ContextAssemblyTrace> {
    serde_json::from_value(trace_json.clone()).ok()
}

fn push_tree_sections(out: &mut Vec<String>, prefix: &str, sections: Vec<Vec<String>>) {
    let section_count = sections.len();
    for (idx, section) in sections.into_iter().enumerate() {
        if section.is_empty() {
            continue;
        }
        let is_last = idx + 1 == section_count;
        let head = if is_last { "└─" } else { "├─" };
        let cont = if is_last { "   " } else { "│  " };
        out.push(format!("{prefix}{head} {}", section[0]));
        for line in section.into_iter().skip(1) {
            out.push(format!("{prefix}{cont}{line}"));
        }
    }
}

fn format_ms(value: Option<u64>) -> String {
    value
        .map(|ms| {
            if ms >= 1000 {
                format!("{:.1}s", ms as f64 / 1000.0)
            } else {
                format!("{ms}ms")
            }
        })
        .unwrap_or_else(|| "?".to_string())
}

fn shorten_joined(names: &[String], max_items: usize) -> String {
    if names.is_empty() {
        return "-".to_string();
    }
    let mut parts: Vec<String> = names.iter().take(max_items).cloned().collect();
    if names.len() > max_items {
        parts.push(format!("+{}", names.len() - max_items));
    }
    parts.join(", ")
}

fn tool_call_status(record: &ToolCallRecord) -> &'static str {
    if record.was_blocked_by_policy() {
        "blocked"
    } else if record.ok {
        "ok"
    } else {
        "error"
    }
}

#[derive(Debug)]
struct ExplainBatch<'a> {
    label: String,
    parallel: bool,
    records: Vec<&'a ToolCallRecord>,
}

#[derive(Debug)]
struct ExplainRound<'a> {
    round: u32,
    batches: Vec<ExplainBatch<'a>>,
}

fn explain_rounds(tool_call_records: &[ToolCallRecord]) -> Vec<ExplainRound<'_>> {
    let mut rounds: Vec<ExplainRound<'_>> = Vec::new();
    for (idx, record) in tool_call_records
        .iter()
        .filter(|record| !record.is_synthetic_placeholder())
        .enumerate()
    {
        let round_index = record.round.unwrap_or(0);
        let round_slot =
            if let Some(pos) = rounds.iter().position(|round| round.round == round_index) {
                pos
            } else {
                rounds.push(ExplainRound {
                    round: round_index,
                    batches: Vec::new(),
                });
                rounds.len() - 1
            };
        let batch_key = record
            .batch_id
            .clone()
            .unwrap_or_else(|| format!("serial-{}", idx + 1));
        if let Some(batch) = rounds[round_slot]
            .batches
            .iter_mut()
            .find(|batch| batch.label == batch_key)
        {
            batch.parallel |= record.parallel.unwrap_or(false);
            batch.records.push(record);
        } else {
            rounds[round_slot].batches.push(ExplainBatch {
                label: batch_key,
                parallel: record.parallel.unwrap_or(false),
                records: vec![record],
            });
        }
    }
    rounds.sort_by_key(|round| round.round);
    rounds
}

fn explain_value_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_i64().and_then(|n| u64::try_from(n).ok()))
    })
}

fn decision_label(decision_type: &DecisionType) -> String {
    match decision_type {
        DecisionType::ToolSelection { tools } => format!("tool selection ({} tools)", tools.len()),
        DecisionType::HistoryCompression { turns_affected } => {
            format!("history compression ({} turns)", turns_affected.len())
        }
        DecisionType::MemoryRetrieval { memories } => {
            format!("memory retrieval ({} memories)", memories.len())
        }
        DecisionType::StrategyChoice { strategy } => format!("strategy: {strategy}"),
    }
}

fn render_context_section(
    trace: &ContextAssemblyTrace,
    meta: Option<&ExplainTurnMeta<'_>>,
    verbose: bool,
) -> Vec<String> {
    let total_tokens = trace.token_budget.total_used;
    let limit = trace.token_budget.max_tokens;
    let pressure = if limit > 0 {
        (total_tokens as f64 / limit as f64) * 100.0
    } else {
        0.0
    };
    let mut lines = vec![format!(
        "context_assembly ms={} budget={}/{} ({pressure:.1}%)",
        format_ms(meta.and_then(|meta| meta.context_ms)),
        total_tokens,
        limit
    )];
    let mut children = Vec::new();
    children.push(vec![format!(
        "prompt system={} history={} memory={} tool_schemas={} user={}",
        trace.token_budget.system_prompt_tokens,
        trace.token_budget.history_tokens,
        trace.token_budget.memory_tokens,
        trace.token_budget.tool_schema_tokens,
        trace.token_budget.user_message_tokens
    )]);
    let selected_tools: Vec<String> = trace
        .tools
        .tools_selected
        .iter()
        .map(|tool| tool.tool_name.clone())
        .collect();
    if !selected_tools.is_empty() || !trace.tools.selection_strategy.is_empty() {
        children.push(vec![format!(
            "tool_selection strategy={} conf={:.2} selected={}/{} [{}]",
            if trace.tools.selection_strategy.is_empty() {
                "-".to_string()
            } else {
                trace.tools.selection_strategy.clone()
            },
            trace.tools.selection_confidence,
            selected_tools.len(),
            trace.tools.tools_available,
            shorten_joined(&selected_tools, if verbose { 10 } else { 6 })
        )]);
    } else if let Some(meta) = meta
        && (!meta.selected_tools.is_empty() || meta.selection_strategy.is_some())
    {
        children.push(vec![format!(
            "tool_selection strategy={} conf={:.2} selected={} [{}]",
            meta.selection_strategy.as_deref().unwrap_or("-"),
            meta.selection_confidence.unwrap_or(0.0),
            meta.selected_tools.len(),
            shorten_joined(&meta.selected_tools, if verbose { 10 } else { 6 })
        )]);
    }
    children.push(vec![format!(
        "memory query={:?} considered={} selected={} tokens={} ms={}",
        trace.memory.query,
        trace.memory.candidates_considered,
        trace.memory.memories_selected.len(),
        trace.memory.total_tokens,
        format_ms(Some(trace.memory.retrieval_latency_ms))
    )]);
    if verbose && !trace.explanations.is_empty() {
        let mut decision_section = vec![format!("decisions {}", trace.explanations.len())];
        let mut decision_children = Vec::new();
        for explanation in &trace.explanations {
            let mut entry = vec![format!(
                "{} conf={:.2}",
                decision_label(&explanation.decision_type),
                explanation.confidence
            )];
            if !explanation.reasoning.trim().is_empty() {
                entry.push(format!(
                    "└─ why {}",
                    truncate_str(explanation.reasoning.trim(), 180)
                ));
            }
            if !explanation.alternatives_considered.is_empty() {
                let alternatives = explanation
                    .alternatives_considered
                    .iter()
                    .take(3)
                    .map(|alternative| {
                        format!(
                            "{}@{:.2} {}",
                            alternative.description,
                            alternative.score,
                            truncate_str(&alternative.why_not_chosen, 80)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" | ");
                entry.push(format!("└─ alts {alternatives}"));
            }
            decision_children.push(entry);
        }
        push_tree_sections(&mut decision_section, "", decision_children);
        children.push(decision_section);
    }
    push_tree_sections(&mut lines, "", children);
    lines
}

fn render_llm_section(
    round_index: u32,
    explain_item: Option<&serde_json::Value>,
    meta: Option<&ExplainTurnMeta<'_>>,
    round_batches: &[ExplainBatch<'_>],
    verbose: bool,
) -> Vec<String> {
    let llm_step = explain_item
        .and_then(|item| item.get("steps"))
        .and_then(serde_json::Value::as_array)
        .and_then(|steps| {
            steps
                .iter()
                .find(|step| step.get("step").and_then(|value| value.as_str()) == Some("llm"))
        });
    let fresh_in = llm_step
        .and_then(|step| explain_value_u64(step, "in"))
        .or_else(|| explain_item.and_then(|item| explain_value_u64(item, "prompt_tokens")))
        .or_else(|| meta.and_then(|meta| meta.prompt_tokens));
    let cache_read = llm_step
        .and_then(|step| explain_value_u64(step, "cached_in"))
        .or_else(|| meta.and_then(|meta| meta.cache_read_tokens));
    let cache_write = llm_step
        .and_then(|step| explain_value_u64(step, "cache_write"))
        .or_else(|| meta.and_then(|meta| meta.cache_creation_tokens));
    let out = llm_step
        .and_then(|step| explain_value_u64(step, "out"))
        .or_else(|| explain_item.and_then(|item| explain_value_u64(item, "completion_tokens")))
        .or_else(|| meta.and_then(|meta| meta.completion_tokens));
    let tool_calls = llm_step
        .and_then(|step| explain_value_u64(step, "tool_calls"))
        .or_else(|| meta.and_then(|meta| meta.tool_count.map(u64::from)))
        .unwrap_or_else(|| {
            round_batches
                .iter()
                .map(|batch| batch.records.len() as u64)
                .sum()
        });
    let duration_ms = llm_step
        .and_then(|step| explain_value_u64(step, "duration_ms"))
        .or_else(|| explain_item.and_then(|item| explain_value_u64(item, "total_ms")))
        .or_else(|| meta.and_then(|meta| meta.total_llm_ms));
    let mut lines = vec![format!(
        "llm ms={} fresh_in={} cache_read={} cache_write={} out={} tool_calls={}",
        format_ms(duration_ms),
        fresh_in
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string()),
        cache_read
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string()),
        cache_write
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string()),
        out.map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string()),
        tool_calls
    )];
    let mut children = Vec::new();
    if let Some(routing) = explain_item
        .and_then(|item| item.get("routing"))
        .and_then(serde_json::Value::as_object)
    {
        let skipped = routing
            .get("skipped")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let reason = routing
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        let intent = routing
            .get("intent")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("default");
        let tier = routing
            .get("tier")
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let confidence = routing
            .get("confidence")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        children.push(vec![format!(
            "routing intent={intent} conf={confidence:.2} tier={tier} skipped={skipped} reason={reason}"
        )]);
    } else if let Some(domain) = meta.and_then(|meta| meta.routing_domain_hint.as_deref()) {
        children.push(vec![format!("routing domain_hint={domain}")]);
    }
    if let Some(steps) = explain_item
        .and_then(|item| item.get("steps"))
        .and_then(serde_json::Value::as_array)
    {
        for step in steps {
            let label = step
                .get("step")
                .and_then(|value| value.as_str())
                .unwrap_or("?");
            if label == "llm" {
                continue;
            }
            children.push(vec![format!(
                "step[{label}] ms={}",
                format_ms(explain_value_u64(step, "duration_ms"))
            )]);
        }
    }
    if verbose
        && let Some(aux) = explain_item
            .and_then(|item| item.get("auxiliary_llm_calls"))
            .and_then(serde_json::Value::as_array)
    {
        for call in aux {
            let purpose = call
                .get("purpose")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("aux");
            children.push(vec![format!(
                "aux_llm[{purpose}] ms={} in={} out={}",
                format_ms(explain_value_u64(call, "ms")),
                explain_value_u64(call, "tokens_in")
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "?".to_string()),
                explain_value_u64(call, "tokens_out")
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "?".to_string()),
            )]);
        }
    }
    if verbose && round_index == 0 {
        children.push(vec![format!(
            "timing ttft={} context={} memoria={}",
            format_ms(meta.and_then(|meta| meta.ttft_ms)),
            format_ms(meta.and_then(|meta| meta.context_ms)),
            format_ms(meta.and_then(|meta| meta.memoria_ms))
        )]);
    }
    push_tree_sections(&mut lines, "", children);
    lines
}

fn render_batch_section(batch: &ExplainBatch<'_>, verbose: bool) -> Vec<String> {
    let max_ms = batch
        .records
        .iter()
        .map(|record| record.ms)
        .max()
        .unwrap_or(0);
    let total_ms: u64 = batch.records.iter().map(|record| record.ms).sum();
    let mut lines = vec![format!(
        "batch[{}] {} tools={} max={} total={}",
        batch.label,
        if batch.parallel { "parallel" } else { "serial" },
        batch.records.len(),
        format_ms(Some(max_ms)),
        format_ms(Some(total_ms))
    )];
    let mut children = Vec::new();
    for record in &batch.records {
        let mut tool_line = format!(
            "{} {} ms={}{}{}",
            record.name,
            tool_call_status(record),
            format_ms(Some(record.ms)),
            record
                .start_offset_ms
                .map(|offset| format!(" offset={offset}ms"))
                .unwrap_or_default(),
            record
                .tool_call_id
                .as_deref()
                .map(|id| format!(" id={id}"))
                .unwrap_or_default()
        );
        if let Some(path) = record.file_path.as_deref() {
            tool_line.push_str(&format!(" path={path}"));
        }
        let mut entry = vec![tool_line];
        if verbose {
            if let Some(args) = record
                .args_preview
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                entry.push(format!("└─ args {}", truncate_str(args, 160)));
            }
            if let Some(output) = record
                .result_preview
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                entry.push(format!("└─ out {}", truncate_str(output, 160)));
            }
            if let Some(error) = record
                .error
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                entry.push(format!("└─ err {}", truncate_str(error, 160)));
            }
        }
        children.push(entry);
    }
    push_tree_sections(&mut lines, "", children);
    lines
}

fn render_round_section(
    round_index: u32,
    explain_item: Option<&serde_json::Value>,
    meta: Option<&ExplainTurnMeta<'_>>,
    round_batches: &[ExplainBatch<'_>],
    verbose: bool,
) -> Vec<String> {
    let mut lines = vec![format!("round[{}]", round_index + 1)];
    let mut children = vec![render_llm_section(
        round_index,
        explain_item,
        meta,
        round_batches,
        verbose,
    )];
    for batch in round_batches {
        children.push(render_batch_section(batch, verbose));
    }
    push_tree_sections(&mut lines, "", children);
    lines
}

fn render_assistant_section(meta: &ExplainTurnMeta<'_>, verbose: bool) -> Vec<String> {
    let text = meta.assistant_output.unwrap_or("");
    let mut lines = vec![format!(
        "assistant out_tokens={} chars={}",
        meta.completion_tokens
            .map(|value| value.to_string())
            .unwrap_or_else(|| "?".to_string()),
        text.chars().count()
    )];
    if !text.trim().is_empty() {
        let preview_len = if verbose { 220 } else { 120 };
        lines.push(format!(
            "└─ preview {}",
            truncate_str(text.trim(), preview_len)
        ));
    }
    lines
}

pub(crate) fn render_explain_dag(
    trace: Option<&ContextAssemblyTrace>,
    meta: Option<&ExplainTurnMeta<'_>>,
    explain_items: &[serde_json::Value],
    verbose: bool,
) -> Option<String> {
    if trace.is_none() && meta.is_none() && explain_items.is_empty() {
        return None;
    }
    let turn_label = trace
        .map(|trace| trace.turn_id.clone())
        .or_else(|| meta.and_then(|meta| meta.turn_label.clone()))
        .unwrap_or_else(|| "turn-?".to_string());
    let total_ms = meta.and_then(|meta| meta.duration_ms).or_else(|| {
        let total: u64 = explain_items
            .iter()
            .filter_map(|item| explain_value_u64(item, "total_ms"))
            .sum();
        (total > 0).then_some(total)
    });
    let total_tools = meta.and_then(|meta| meta.tool_count).unwrap_or_else(|| {
        meta.map(|meta| meta.tool_call_records.len() as u32)
            .unwrap_or(0)
    });
    let total_rounds = meta
        .and_then(|meta| meta.llm_rounds)
        .unwrap_or_else(|| explain_items.len().max(1) as u32);
    let mut lines = vec![
        format!("Explain Analyze DAG — {turn_label}"),
        format!(
            "● turn total={} ttft={} context={} llm={} tools={} rounds={} tool_calls={}",
            format_ms(total_ms),
            format_ms(meta.and_then(|meta| meta.ttft_ms)),
            format_ms(meta.and_then(|meta| meta.context_ms)),
            format_ms(meta.and_then(|meta| meta.total_llm_ms)),
            format_ms(meta.and_then(|meta| meta.total_tool_ms)),
            total_rounds,
            total_tools
        ),
        format!(
            "  tokens fresh_in={} cache_read={} cache_write={} out={}",
            meta.and_then(|meta| meta.prompt_tokens)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string()),
            meta.and_then(|meta| meta.cache_read_tokens)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string()),
            meta.and_then(|meta| meta.cache_creation_tokens)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string()),
            meta.and_then(|meta| meta.completion_tokens)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "?".to_string())
        ),
    ];
    let round_batches = meta
        .map(|meta| explain_rounds(meta.tool_call_records))
        .unwrap_or_default();
    let total_round_count = explain_items.len().max(round_batches.len());
    let mut sections = Vec::new();
    if let Some(trace) = trace {
        sections.push(render_context_section(trace, meta, verbose));
    }
    for idx in 0..total_round_count.max(meta.is_some() as usize) {
        let explain_item = explain_items.get(idx);
        let round_index = idx as u32;
        let batches = round_batches
            .iter()
            .find(|round| round.round == round_index)
            .map(|round| round.batches.as_slice())
            .unwrap_or(&[]);
        if explain_item.is_none() && batches.is_empty() && idx > 0 {
            continue;
        }
        sections.push(render_round_section(
            round_index,
            explain_item,
            meta,
            batches,
            verbose,
        ));
    }
    if let Some(meta) = meta {
        sections.push(render_assistant_section(meta, verbose));
    }
    push_tree_sections(&mut lines, "", sections);
    Some(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{ExplainTurnMeta, context_trace_from_json, render_explain_dag};
    use astra_services::session_journal::ToolCallRecord;
    use astra_services::session_journal::{JournalEvent, SelectionTrace};
    use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;

    #[test]
    fn render_explain_dag_includes_cache_and_parallel_batches() {
        let mut trace = ContextAssemblyTrace {
            turn_id: "turn-2".into(),
            session_id: "sess-1".into(),
            ..Default::default()
        };
        trace.system_prompt.total_tokens = 3943;
        trace.token_budget.total_used = 7658;
        trace.token_budget.max_tokens = 160_000;
        trace.token_budget.history_tokens = 7;
        trace.token_budget.tool_schema_tokens = 3708;
        trace.tools.tools_available = 27;
        trace.tools.selection_strategy = "registry".into();
        trace
            .tools
            .tools_selected
            .push(astra_turn_core::context_assembly_trace::ToolSelected {
                tool_name: "bash".into(),
                score: 1.0,
                tokens: 243,
                selection_factors: Vec::new(),
            });

        let tool_calls = vec![
            ToolCallRecord {
                tool_call_id: Some("call-1".into()),
                name: "bash".into(),
                ok: true,
                ms: 3000,
                batch_id: Some("parallel-1".into()),
                parallel: Some(true),
                round: Some(0),
                start_offset_ms: Some(40),
                ..Default::default()
            },
            ToolCallRecord {
                tool_call_id: Some("call-2".into()),
                name: "read_file".into(),
                ok: true,
                ms: 48,
                batch_id: Some("parallel-1".into()),
                parallel: Some(true),
                round: Some(0),
                file_path: Some("README.md".into()),
                ..Default::default()
            },
        ];
        let meta = ExplainTurnMeta {
            turn_label: Some("turn-2".into()),
            duration_ms: Some(2930),
            ttft_ms: Some(1900),
            context_ms: Some(88),
            memoria_ms: Some(51),
            total_llm_ms: Some(2930),
            total_tool_ms: Some(3048),
            prompt_tokens: Some(10023),
            completion_tokens: Some(32),
            cache_read_tokens: Some(900),
            cache_creation_tokens: Some(200),
            tool_count: Some(2),
            llm_rounds: Some(1),
            routing_domain_hint: None,
            assistant_output: Some("done"),
            tool_call_records: &tool_calls,
            selection_strategy: None,
            selection_confidence: None,
            selected_tools: Vec::new(),
        };
        let explain_items = vec![serde_json::json!({
            "total_ms": 2930,
            "steps": [{
                "step": "llm",
                "duration_ms": 2930,
                "in": 10023,
                "cached_in": 900,
                "cache_write": 200,
                "out": 32,
                "tool_calls": 2
            }]
        })];

        let text =
            render_explain_dag(Some(&trace), Some(&meta), &explain_items, false).expect("text");
        assert!(text.contains("Explain Analyze DAG — turn-2"));
        assert!(text.contains(
            "llm ms=2.9s fresh_in=10023 cache_read=900 cache_write=200 out=32 tool_calls=2"
        ));
        assert!(text.contains("batch[parallel-1] parallel tools=2"));
        assert!(text.contains("read_file ok ms=48ms id=call-2 path=README.md"));
    }

    #[test]
    fn context_trace_from_json_parses_full_trace() {
        let trace = ContextAssemblyTrace {
            turn_id: "turn-7".into(),
            session_id: "sid".into(),
            ..Default::default()
        };
        let parsed = context_trace_from_json(&trace.to_json_value()).expect("parsed trace");
        assert_eq!(parsed.turn_id, "turn-7");
    }

    #[test]
    fn explain_turn_meta_from_journal_event_preserves_unknown_cache_write() {
        let mut event =
            JournalEvent::turn(Some("sid"), 3, Some("gpt-5"), "hi", "hello", 1, 21, 8, 1200)
                .with_tool_calls(vec![ToolCallRecord {
                    tool_call_id: Some("call-1".into()),
                    name: "bash".into(),
                    ok: true,
                    ms: 30,
                    ..Default::default()
                }]);
        event.cache_read_tokens = Some(144);
        event.cache_creation_tokens = None;
        event.selection_trace = Some(SelectionTrace {
            candidate_scores: None,
            boost_terms: None,
            learned_context_summary: None,
            final_tools: vec!["bash".into()],
            confidence: 0.91,
            strategy: "registry".into(),
        });

        let meta = ExplainTurnMeta::from_journal_event(&event);
        let text = render_explain_dag(None, Some(&meta), &[], false).expect("text");

        assert_eq!(meta.turn_label.as_deref(), Some("turn-3"));
        assert_eq!(meta.selection_strategy.as_deref(), Some("registry"));
        assert_eq!(meta.selected_tools, vec!["bash".to_string()]);
        assert!(text.contains("tokens fresh_in=21 cache_read=144 cache_write=? out=8"));
        assert!(
            text.contains("llm ms=? fresh_in=21 cache_read=144 cache_write=? out=8 tool_calls=1")
        );
    }
}
