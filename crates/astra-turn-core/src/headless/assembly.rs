//! Match cloud assistant `tool_calls` to edge-executed `tool_request` rows (§5.5 headless path).
//!
//! Shared between the CLI SSE loop and any future server-side handler that consumes the same shape.

use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

use crate::tool::args::shape::canonicalize_tool_call_for_execution;
use crate::tool::categories::is_file_mutation_tool;
use crate::tool::result::semantics::tool_dedup_signature;

/// One tool slot to execute in a headless round: either a server `tool_calls[i]` or synthetic edge row `i`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadlessRoundToolIdx {
    ServerToolCall(usize),
    SyntheticEdge(usize),
}

/// Resolved id/name/args for one headless round slot (server `tool_calls` row or synthetic edge row).
#[derive(Debug, Clone, PartialEq)]
pub struct HeadlessResolvedToolSlot {
    pub id: String,
    pub name: String,
    pub args: Value,
    pub synthetic_edge_index: Option<usize>,
}

/// Map one [`HeadlessRoundToolIdx`] to flat call fields; `edge_lookup` is used only for [`HeadlessRoundToolIdx::SyntheticEdge`].
#[must_use]
pub fn resolve_headless_tool_slot(
    item: HeadlessRoundToolIdx,
    server_tool_calls: &[Value],
    mut edge_lookup: impl FnMut(usize) -> (String, String, Value),
) -> HeadlessResolvedToolSlot {
    match item {
        HeadlessRoundToolIdx::ServerToolCall(i) => {
            let (id, name, args) = server_tool_calls
                .get(i)
                .map(parse_flat_tool_call_event)
                .unwrap_or_else(|| (String::new(), String::new(), json!({})));
            HeadlessResolvedToolSlot {
                id,
                name,
                args,
                synthetic_edge_index: None,
            }
        }
        HeadlessRoundToolIdx::SyntheticEdge(i) => {
            let (id, name, args) = edge_lookup(i);
            HeadlessResolvedToolSlot {
                id,
                name,
                args,
                synthetic_edge_index: Some(i),
            }
        }
    }
}

/// Prefer iterating server `tool_calls` when present; otherwise one synthetic slot per edge row (§5.5).
pub fn headless_round_tool_indices(
    server_tool_calls_len: usize,
    edge_round_len: usize,
) -> Vec<HeadlessRoundToolIdx> {
    if server_tool_calls_len > 0 {
        (0..server_tool_calls_len)
            .map(HeadlessRoundToolIdx::ServerToolCall)
            .collect()
    } else {
        (0..edge_round_len)
            .map(HeadlessRoundToolIdx::SyntheticEdge)
            .collect()
    }
}

fn tool_call_ids_are_unique(tool_calls: &[Value]) -> bool {
    let mut ids = HashSet::with_capacity(tool_calls.len());
    tool_calls.iter().all(|tool_call| {
        tool_call
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| ids.insert(id))
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ProviderToolBatchError {
    #[error("provider tool call at index {index} is invalid: {detail}")]
    InvalidCall { index: usize, detail: &'static str },
    #[error("provider tool call at index {index} has a malformed identity")]
    InvalidIdentity { index: usize },
    #[error("provider tool-call identity `{id}` is duplicated")]
    DuplicateIdentity { id: String },
}

/// Canonicalize one complete provider-owned tool batch without inventing
/// identities. Any malformed entry or repeated identity invalidates the whole
/// batch: partially retaining it would silently lose model intent and could
/// execute two effects under one durable key.
pub fn canonicalize_provider_tool_batch(
    tool_calls: &[Value],
) -> Result<std::borrow::Cow<'_, [Value]>, ProviderToolBatchError> {
    let all_exact = tool_calls.iter().all(|tool_call| {
        canonicalize_tool_call_for_execution(tool_call)
            .is_ok_and(|canonical| canonical == *tool_call)
    });
    let canonical = tool_calls
        .iter()
        .enumerate()
        .map(|(index, tool_call)| {
            canonicalize_tool_call_for_execution(tool_call)
                .map_err(|detail| ProviderToolBatchError::InvalidCall { index, detail })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut identities = HashSet::with_capacity(canonical.len());
    for (index, tool_call) in canonical.iter().enumerate() {
        let id = tool_call["id"]
            .as_str()
            .filter(|id| id.len() <= 512 && !id.chars().any(char::is_control))
            .ok_or(ProviderToolBatchError::InvalidIdentity { index })?;
        if !identities.insert(id) {
            return Err(ProviderToolBatchError::DuplicateIdentity { id: id.to_string() });
        }
    }
    if all_exact && tool_call_ids_are_unique(tool_calls) {
        Ok(std::borrow::Cow::Borrowed(tool_calls))
    } else {
        Ok(std::borrow::Cow::Owned(canonical))
    }
}

/// Parse one exact canonical tool call. Invalid or id-less input returns an
/// empty sentinel and must not be routed to execution.
pub fn parse_flat_tool_call_event(tc: &Value) -> (String, String, Value) {
    let Ok(canonical) = canonicalize_tool_call_for_execution(tc) else {
        return (String::new(), String::new(), Value::Null);
    };
    if canonical != *tc {
        return (String::new(), String::new(), Value::Null);
    }
    let id = canonical["id"].as_str().unwrap_or_default().to_string();
    let name = canonical["function"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let args = canonical["function"]["arguments"]
        .as_str()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or(Value::Null);
    (id, name, args)
}

/// Tool names still pending when step scheduling times out (abort tail of `indices`).
pub fn headless_timeout_aborted_tool_names(
    indices: &[HeadlessRoundToolIdx],
    completed_tool_results_len: usize,
    server_tool_calls: &[Value],
    mut synthetic_tool_name: impl FnMut(usize) -> String,
) -> Vec<String> {
    indices
        .iter()
        .skip(completed_tool_results_len)
        .map(|idx| match *idx {
            HeadlessRoundToolIdx::ServerToolCall(i) => server_tool_calls
                .get(i)
                .map(|tc| parse_flat_tool_call_event(tc).1)
                .unwrap_or_default(),
            HeadlessRoundToolIdx::SyntheticEdge(i) => synthetic_tool_name(i),
        })
        .collect()
}

/// Sentinel prefix for cache-hit / duplicate-call stubs. All messages
/// produced by [`idempotency_cache_hit_message`] start with this exact
/// string, and downstream detectors (see list on that function's doc
/// comment) use `starts_with(CACHED_SENTINEL)` to recognise replayed
/// tool output across compaction boundaries. Changing this value is a
/// breaking change across crates — audit before touching.
pub const CACHED_SENTINEL: &str = "(cached";

/// User-facing error when the LLM names a tool not in the local registry.
pub fn unknown_local_tool_error_message(name: &str, valid_tool_names: &HashSet<String>) -> String {
    let mut names: Vec<_> = valid_tool_names.iter().cloned().collect();
    names.sort();
    format!("Unknown tool '{}'. Available: {}", name, names.join(", "))
}

/// User-visible `tool` message body when replaying an idempotent cache hit.
///
/// Session 0e37eb46 regression: the previous implementation returned a
/// bare "(cached — identical call already executed …)" stub that the
/// LLM routinely misread as "empty result, try something else". The
/// model would call `read_file` → cache-hit → stub → assume nothing
/// there → call a variant (different offset, bash cat, etc.) burning
/// rounds re-reading content it already had.
///
/// New contract: include enough of the cached content that the LLM
/// can tell the result is MEANINGFUL and matches what it saw earlier.
///   * Short output (≤ [`IDEMPOTENCY_INLINE_MAX_BYTES`]): return it
///     inline, tagged as cached. Token cost is negligible and the LLM
///     gets the full signal.
///   * Larger output: return a preview header (`N chars`) +
///     first ~500 chars + explicit pointer to the earlier tool_result
///     for the rest. The preview makes the cached status obvious; the
///     byte count + head prove the content is real.
///
/// Both forms start with [`CACHED_SENTINEL`] so downstream detectors
/// (memory writability gates, adaptive runtime signals, compaction
/// replay guards in `context_compression::SYNTHETIC_USER_SENTINELS`,
/// regression test `compaction_survival`) that look for that sentinel
/// continue to work. Keep this single source of truth — if you change
/// the sentinel, audit every `starts_with("(cached")` / fixed-string
/// match in `astra-text-utils`, `astra-turn-core`, and `runtime`.
#[must_use]
pub fn idempotency_cache_hit_message(cached_output: &str) -> String {
    let trimmed = cached_output.trim_end();
    if trimmed.is_empty() {
        return "(cached — identical call already executed; original output was empty)".to_string();
    }
    if trimmed == "{}" {
        return format!(
            "(cached — identical call already executed; degraded historical tool output was an empty JSON object placeholder) [tag={}]",
            crate::history::DEGRADED_EMPTY_OBJECT_TAG
        );
    }
    if trimmed.len() <= IDEMPOTENCY_INLINE_MAX_BYTES {
        // If the cached output itself already starts with the sentinel
        // (e.g. a replay across a compaction boundary where the stored
        // value is an earlier cache-hit stub), don't double-wrap — the
        // original stub already carries the sentinel that downstream
        // detectors look for.
        if trimmed.starts_with(CACHED_SENTINEL) {
            return trimmed.to_string();
        }
        return format!("(cached — identical call already executed)\n{trimmed}");
    }
    // Large: preview + pointer. Char boundary safe for UTF-8.
    let preview_end = trimmed
        .char_indices()
        .take_while(|(i, _)| *i < IDEMPOTENCY_PREVIEW_BYTES)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let preview = &trimmed[..preview_end];
    format!(
        "(cached — identical call already executed; {} bytes total, preview:)\n{preview}\n\
         […truncated. The full content is in the earlier tool_result with the \
         same call signature — scroll back if you need the rest.]",
        trimmed.len()
    )
}

/// Outputs at or below this size are returned inline on cache-hit.
/// Chosen so the common case (a single file read under ~2k) stays
/// fully visible; larger outputs fall through to the preview path.
pub const IDEMPOTENCY_INLINE_MAX_BYTES: usize = 2000;

/// Preview size for the large-output cache-hit path. Enough for the
/// LLM to recognize the content; short enough that token cost stays
/// bounded even if the cache-hit fires repeatedly.
pub const IDEMPOTENCY_PREVIEW_BYTES: usize = 500;

/// `tool` / `tool_results` body when the same signature was already executed this headless round.
pub const HEADLESS_DUPLICATE_WITHIN_TURN_BODY: &str =
    "(duplicate call — result same as previous identical call this turn)";

#[must_use]
pub fn headless_openai_duplicate_within_turn_pair(
    tool_call_id: &str,
    tool_name: &str,
) -> (Value, Value) {
    openai_tool_roundtrip_values(tool_call_id, tool_name, HEADLESS_DUPLICATE_WITHIN_TURN_BODY)
}

#[must_use]
pub fn headless_idempotency_hit_openai_pair(
    tool_call_id: &str,
    tool_name: &str,
    cached_output: &str,
) -> (Value, Value) {
    let body = idempotency_cache_hit_message(cached_output);
    openai_tool_roundtrip_values(tool_call_id, tool_name, body.as_str())
}

#[must_use]
pub fn headless_unknown_local_tool_openai_pair(
    tool_call_id: &str,
    tool_name: &str,
    valid_tool_names: &HashSet<String>,
) -> (Value, Value) {
    let err = unknown_local_tool_error_message(tool_name, valid_tool_names);
    openai_tool_roundtrip_values(tool_call_id, tool_name, err.as_str())
}

/// Read-only tools for headless (edge) execution — safe to execute
/// concurrently and cache across turns. Derived from the central
/// [`crate::tool_categories`] registry (excludes web/memory/aliases).
///
/// This is a lazily-initialized static so callers can use
/// `READ_ONLY_TOOLS.contains(&name)` with zero allocation per call.
pub static READ_ONLY_TOOLS: std::sync::LazyLock<Vec<&'static str>> =
    std::sync::LazyLock::new(|| crate::tool::categories::registry().headless_read_only_names());

/// One edge-executed tool row in the current LLM round (ordering preserved vs `tool_calls`).
pub trait EdgeToolRoundRow {
    fn tool_name(&self) -> &str;
    fn tool_args(&self) -> &Value;
    fn tool_output(&self) -> &str;
    /// Machine-owned terminal status supplied by the edge executor.
    ///
    /// This is deliberately distinct from a tool's human/JSON output.  The
    /// execution pipeline uses it to preserve a typed transport failure even
    /// when the diagnostic body is prose.  Legacy rows that did not carry a
    /// terminal status remain `None` and use their established compatibility
    /// path.
    fn tool_execution_status(&self) -> Option<&str> {
        None
    }
    fn tool_result_fields(&self) -> Option<&serde_json::Map<String, Value>> {
        None
    }
    fn tool_duration_ms(&self) -> u64 {
        0
    }

    /// OpenAI `tool_calls[].id` when synthesizing from an edge-only round (§5.5).
    /// Default `edge-{index}`; rows with a server `request_id` should override.
    fn assistant_tool_call_id(&self, index: usize) -> String {
        format!("edge-{index}")
    }

    /// True when [`Self::assistant_tool_call_id`] came from a server tool-call
    /// id or edge executor request id, rather than the synthetic `edge-{index}`
    /// fallback used for edge-only rounds.
    fn has_explicit_assistant_tool_call_id(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchedEdgeToolOutput {
    pub output: String,
    pub duration_ms: u64,
    pub execution_status: Option<String>,
    pub tool_result_fields: Option<serde_json::Map<String, Value>>,
}

fn matched_edge_tool_output<T: EdgeToolRoundRow>(row: &T) -> MatchedEdgeToolOutput {
    MatchedEdgeToolOutput {
        output: row.tool_output().to_string(),
        duration_ms: row.tool_duration_ms(),
        execution_status: row.tool_execution_status().map(ToString::to_string),
        tool_result_fields: row.tool_result_fields().cloned(),
    }
}

pub fn take_edge_output_for_tool_call_id_or_signature_with_duration<T: EdgeToolRoundRow>(
    tool_call_id: &str,
    name: &str,
    args: &Value,
    round: &[T],
    consumed: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> MatchedEdgeToolOutput {
    if !tool_call_id.is_empty() {
        for (i, e) in round.iter().enumerate() {
            if consumed.get(i).copied().unwrap_or(true) {
                continue;
            }
            if e.has_explicit_assistant_tool_call_id()
                && e.assistant_tool_call_id(i) == tool_call_id
                && e.tool_name() == name
            {
                consumed[i] = true;
                return matched_edge_tool_output(e);
            }
        }
    }

    take_edge_output_for_tool_call_with_duration(name, args, round, consumed, by_sig)
}

pub fn take_edge_output_for_tool_call_with_duration<T: EdgeToolRoundRow>(
    name: &str,
    args: &Value,
    round: &[T],
    consumed: &mut [bool],
    by_sig: &HashMap<String, String>,
) -> MatchedEdgeToolOutput {
    let sig = tool_dedup_signature(name, args);
    for (i, e) in round.iter().enumerate() {
        if consumed.get(i).copied().unwrap_or(true) {
            continue;
        }
        if tool_dedup_signature(e.tool_name(), e.tool_args()) == sig {
            consumed[i] = true;
            return matched_edge_tool_output(e);
        }
    }
    MatchedEdgeToolOutput {
        output: by_sig.get(&sig).cloned().unwrap_or_else(|| {
            // IMPORTANT: the prefix "Error: headless edge protocol" is
            // load-bearing — `execute.rs::execute_tool_pure` keys on it
            // to trigger server-side re-execution. If this tool has a
            // ServerToolExecutor available, the error below is replaced
            // with the real result. Only tools that truly have NO
            // server-side executor will surface this message to the LLM.
            no_matching_edge_execution_message(name)
        }),
        duration_ms: 0,
        execution_status: None,
        tool_result_fields: None,
    }
}

fn no_matching_edge_execution_message(name: &str) -> String {
    if matches!(name, "enter_plan_mode" | "exit_plan_mode") {
        return format!(
            "Error: headless edge protocol — tool `{name}` has no matching \
             edge execution in this turn.\n\
             This plan lifecycle tool requires a trusted plan executor/review \
             overlay, but no matching executor was bound for this turn. \
             Continue normal execution if the user already asked to implement, \
             or ask the user to switch to an interactive plan-capable surface."
        );
    }

    if astra_tools::agent_tool_contract::is_agent_runtime_tool(name) {
        return format!(
            "Error: headless edge protocol — tool `{name}` has no matching \
             edge execution in this turn.\n\n\
             This tool requires the multi-agent runtime, but no agent executor \
             path was bound for this turn. This call cannot run in the current \
             turn regardless of retries. Continue only with tools that are \
             visible and executable in this turn."
        );
    }

    if is_file_mutation_tool(name) {
        return format!(
            "Error: headless edge protocol — tool `{name}` has no matching \
             edge execution in this turn.\n\n\
             This dedicated file mutation tool requires a server-side execution \
             path that is not available in this turn. Use a visible dedicated \
             file-edit tool only when that tool is actually executable in this \
             turn."
        );
    }

    format!(
        "Error: headless edge protocol — tool `{name}` has no matching \
         edge execution in this turn. \
         This means `{name}` requires a server-side execution path that is \
         not bound for this turn. \
         Use only tools that have a bound executor this turn. If the user \
         actually needs `{name}`, tell them which executor capability is \
         missing in the current turn."
    )
}

/// Normalize server `tool_calls` or synthetic edge-round rows for stall / TurnGuard signature tracking.
pub fn tool_calls_for_stall_guard<T: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[T],
) -> Vec<Value> {
    if !server_tool_calls.is_empty() {
        server_tool_calls.to_vec()
    } else {
        // Match historical CLI behavior: stall/TurnGuard sees synthetic ids `edge-{i}` only
        // (OpenAI-shaped assistant `tool_calls` may still use `request_id` elsewhere).
        edge_round
            .iter()
            .enumerate()
            .map(|(i, e)| {
                json!({
                    "id": format!("edge-{i}"),
                    "name": e.tool_name(),
                    "arguments": e.tool_args().clone(),
                })
            })
            .collect()
    }
}

fn openai_tool_call_entries_from_server(tool_calls: &[Value]) -> Vec<Value> {
    tool_calls
        .iter()
        .filter_map(|tc| {
            let (id, name, args) = parse_flat_tool_call_event(tc);
            if id.is_empty() || name.is_empty() || !args.is_object() {
                return None;
            }
            Some(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(&args)
                        .unwrap_or_else(|_| r#"{"error":"argument serialization failed"}"#.to_string()),
                }
            }))
        })
        .collect()
}

/// Assistant message with `content: null` and OpenAI-shaped `tool_calls` (server list or edge round).
pub fn openai_assistant_with_tool_calls_message<T: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[T],
    reasoning_content: &str,
) -> Value {
    openai_assistant_with_tool_calls_message_ext(
        server_tool_calls,
        edge_round,
        reasoning_content,
        "",
        false,
    )
}

/// Extended variant that accepts `force_reasoning_field`.
///
/// When `force_reasoning_field` is true the `reasoning_content` key is always
/// present (empty string when `reasoning_content` is blank).  Thinking-enabled
/// models require this on every assistant message.
pub fn openai_assistant_with_tool_calls_message_ext<T: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[T],
    reasoning_content: &str,
    reasoning_signature: &str,
    force_reasoning_field: bool,
) -> Value {
    let tool_calls = if !server_tool_calls.is_empty() {
        openai_tool_call_entries_from_server(server_tool_calls)
    } else {
        edge_round
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let id = e.assistant_tool_call_id(i);
                json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": e.tool_name(),
                        "arguments": serde_json::to_string(e.tool_args())
                            .unwrap_or_else(|_| "{}".to_string()),
                    }
                })
            })
            .collect()
    };
    let mut msg = json!({
        "role": "assistant",
        "content": Value::Null,
    });
    if !tool_calls.is_empty() {
        msg["tool_calls"] = Value::Array(tool_calls);
    }
    if !reasoning_content.is_empty() {
        if let Some(obj) = msg.as_object_mut() {
            obj.insert(
                "reasoning_content".to_string(),
                Value::String(reasoning_content.to_string()),
            );
            if !reasoning_signature.is_empty() {
                obj.insert(
                    "reasoning_signature".to_string(),
                    Value::String(reasoning_signature.to_string()),
                );
            }
        }
    } else if force_reasoning_field && let Some(obj) = msg.as_object_mut() {
        obj.insert(
            "reasoning_content".to_string(),
            Value::String(String::new()),
        );
    }
    msg
}

/// OpenAI `role: "tool"` message plus matching `/chat` `tool_results` row (`content` / `result` identical).
#[must_use]
pub fn openai_tool_roundtrip_values(
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> (Value, Value) {
    openai_tool_roundtrip_values_with_result_fields(tool_call_id, tool_name, content, None)
}

#[must_use]
pub fn openai_tool_roundtrip_values_with_result_fields(
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
    tool_result_fields: Option<&serde_json::Map<String, Value>>,
) -> (Value, Value) {
    let msg = json!({
        "role": "tool",
        "tool_call_id": tool_call_id,
        "content": content,
    });
    let mut tr = serde_json::Map::from_iter([
        (
            "tool_call_id".to_string(),
            Value::String(tool_call_id.to_string()),
        ),
        ("name".to_string(), Value::String(tool_name.to_string())),
        ("result".to_string(), Value::String(content.to_string())),
    ]);
    if let Some(extra_fields) = tool_result_fields {
        tr.extend(extra_fields.clone());
    }
    let tr = Value::Object(tr);
    (msg, tr)
}

/// Assistant `tool_calls` message + iteration indices for the headless round loop (§5.5).
#[derive(Debug, Clone)]
pub struct HeadlessRoundOpening {
    pub assistant_message: Value,
    pub indices: Vec<HeadlessRoundToolIdx>,
    pub tool_count: usize,
}

/// Build the assistant message and per-tool indices.
#[must_use]
pub fn begin_headless_tool_round_opening<Edge: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[Edge],
    reasoning_content: &str,
) -> HeadlessRoundOpening {
    begin_headless_tool_round_opening_ext(
        server_tool_calls,
        edge_round,
        reasoning_content,
        "",
        false,
    )
}

/// Extended variant that accepts `force_reasoning_field` for thinking-model sessions.
#[must_use]
pub fn begin_headless_tool_round_opening_ext<Edge: EdgeToolRoundRow>(
    server_tool_calls: &[Value],
    edge_round: &[Edge],
    reasoning_content: &str,
    reasoning_signature: &str,
    force_reasoning_field: bool,
) -> HeadlessRoundOpening {
    let assistant_message = openai_assistant_with_tool_calls_message_ext(
        server_tool_calls,
        edge_round,
        reasoning_content,
        reasoning_signature,
        force_reasoning_field,
    );
    let indices = headless_round_tool_indices(server_tool_calls.len(), edge_round.len());
    let tool_count = indices.len();
    HeadlessRoundOpening {
        assistant_message,
        indices,
        tool_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Row {
        tool: String,
        args: Value,
        output: String,
        duration_ms: u64,
    }

    impl EdgeToolRoundRow for Row {
        fn tool_name(&self) -> &str {
            &self.tool
        }
        fn tool_args(&self) -> &Value {
            &self.args
        }
        fn tool_output(&self) -> &str {
            &self.output
        }
        fn tool_duration_ms(&self) -> u64 {
            self.duration_ms
        }
    }

    #[derive(Debug)]
    struct RowWithResultFields {
        tool: String,
        args: Value,
        output: String,
        tool_result_fields: serde_json::Map<String, Value>,
    }

    impl EdgeToolRoundRow for RowWithResultFields {
        fn tool_name(&self) -> &str {
            &self.tool
        }
        fn tool_args(&self) -> &Value {
            &self.args
        }
        fn tool_output(&self) -> &str {
            &self.output
        }
        fn tool_result_fields(&self) -> Option<&serde_json::Map<String, Value>> {
            Some(&self.tool_result_fields)
        }
    }

    #[test]
    fn take_edge_output_matches_first_unconsumed_row() {
        let rows = vec![
            Row {
                tool: "read_file".into(),
                args: json!({"path": "x.rs"}),
                output: "one".into(),
                duration_ms: 7,
            },
            Row {
                tool: "read_file".into(),
                args: json!({"path": "y.rs"}),
                output: "two".into(),
                duration_ms: 13,
            },
        ];
        let mut consumed = vec![false; 2];
        let by_sig: HashMap<String, String> = HashMap::new();
        let out = take_edge_output_for_tool_call_with_duration(
            "read_file",
            &json!({"path": "y.rs"}),
            &rows,
            &mut consumed,
            &by_sig,
        );
        assert_eq!(out.output, "two");
        assert_eq!(out.duration_ms, 13);
        assert!(!consumed[0]);
        assert!(consumed[1]);
    }

    #[test]
    fn take_edge_output_falls_back_to_callback_map() {
        let rows: Vec<Row> = vec![];
        let mut consumed = vec![];
        let mut by_sig = HashMap::new();
        let sig = tool_dedup_signature("grep", &json!({"pattern": "foo"}));
        by_sig.insert(sig, "from-map".into());
        let out = take_edge_output_for_tool_call_with_duration(
            "grep",
            &json!({"pattern": "foo"}),
            &rows,
            &mut consumed,
            &by_sig,
        );
        assert_eq!(out.output, "from-map");
        assert_eq!(out.duration_ms, 0);
    }

    #[test]
    fn no_edge_execution_for_file_mutation_does_not_suggest_shell_fallback() {
        let rows: Vec<Row> = vec![];
        let mut consumed = vec![];
        let by_sig = HashMap::new();
        let out = take_edge_output_for_tool_call_with_duration(
            "write_file",
            &json!({"path": "index.html", "content": "<main></main>"}),
            &rows,
            &mut consumed,
            &by_sig,
        );

        let lower = out.output.to_ascii_lowercase();
        assert!(
            lower.contains("dedicated file mutation tool"),
            "{}",
            out.output
        );
        assert!(
            lower.contains("server-side execution path"),
            "{}",
            out.output
        );
        assert!(!lower.contains("workaround: use `bash`"), "{}", out.output);
        assert!(
            !out.output.contains("Workaround: use `bash`"),
            "{}",
            out.output
        );
    }

    #[test]
    fn take_edge_output_preserves_tool_result_fields() {
        let rows = vec![RowWithResultFields {
            tool: "mo_query".into(),
            args: json!({"sql": "UPDATE t SET v = 1"}),
            output: "OK (no results)".into(),
            tool_result_fields: serde_json::Map::from_iter([(
                "pre_state_snapshot_id".to_string(),
                Value::String("moq_snap_1".into()),
            )]),
        }];
        let mut consumed = vec![false];
        let by_sig: HashMap<String, String> = HashMap::new();
        let out = take_edge_output_for_tool_call_with_duration(
            "mo_query",
            &json!({"sql": "UPDATE t SET v = 1"}),
            &rows,
            &mut consumed,
            &by_sig,
        );

        assert_eq!(out.output, "OK (no results)");
        assert_eq!(
            out.tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("pre_state_snapshot_id"))
                .and_then(Value::as_str),
            Some("moq_snap_1")
        );
        assert!(consumed[0]);
    }

    #[test]
    fn headless_round_indices_server_first() {
        let v = headless_round_tool_indices(2, 5);
        assert_eq!(
            v,
            vec![
                HeadlessRoundToolIdx::ServerToolCall(0),
                HeadlessRoundToolIdx::ServerToolCall(1),
            ]
        );
    }

    #[test]
    fn headless_round_indices_edge_when_no_server_calls() {
        let v = headless_round_tool_indices(0, 2);
        assert_eq!(
            v,
            vec![
                HeadlessRoundToolIdx::SyntheticEdge(0),
                HeadlessRoundToolIdx::SyntheticEdge(1),
            ]
        );
    }

    #[test]
    fn resolve_headless_slot_server_and_synthetic() {
        let server = vec![
            json!({"id":"a","type":"function","function":{"name":"read_file","arguments":"{}"}}),
        ];
        let s0 =
            resolve_headless_tool_slot(HeadlessRoundToolIdx::ServerToolCall(0), &server, |_| {
                panic!("edge lookup not used")
            });
        assert_eq!(s0.id, "a");
        assert_eq!(s0.name, "read_file");
        assert_eq!(s0.args, json!({}));
        assert!(s0.synthetic_edge_index.is_none());

        let s1 = resolve_headless_tool_slot(HeadlessRoundToolIdx::SyntheticEdge(2), &[], |i| {
            assert_eq!(i, 2);
            (
                "edge-request-2".into(),
                "bash".into(),
                json!({"command":"ls"}),
            )
        });
        assert_eq!(s1.id, "edge-request-2");
        assert_eq!(s1.name, "bash");
        assert_eq!(s1.args, json!({"command":"ls"}));
        assert_eq!(s1.synthetic_edge_index, Some(2));
    }

    #[test]
    fn parse_canonical_tool_call_string_arguments() {
        let tc = json!({
            "id": "c1",
            "type": "function",
            "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}
        });
        let (id, name, args) = parse_flat_tool_call_event(&tc);
        assert_eq!(id, "c1");
        assert_eq!(name, "bash");
        assert_eq!(args, json!({"command":"ls"}));
    }

    #[test]
    fn parse_canonical_tool_call_rejects_non_exact_object_arguments() {
        let tc = json!({
            "id": "c2",
            "type": "function",
            "function": {"name": "grep", "arguments": {"pattern": "x"}}
        });
        let (id, name, args) = parse_flat_tool_call_event(&tc);
        assert!(id.is_empty());
        assert!(name.is_empty());
        assert!(args.is_null());
    }

    #[test]
    fn parse_canonical_tool_call_requires_exact_name() {
        let tool_call = json!({
            "id": "c2",
            "type": "function",
            "function": {
                "name": "grep",
                "arguments": "{}"
            }
        });
        let (_, name, _) = parse_flat_tool_call_event(&tool_call);
        assert_eq!(name, "grep");
    }

    #[test]
    fn headless_timeout_aborted_names_tail() {
        let idx = vec![
            HeadlessRoundToolIdx::ServerToolCall(0),
            HeadlessRoundToolIdx::SyntheticEdge(0),
        ];
        let server = vec![json!({"name":"read_file","arguments":{}})];
        let names = headless_timeout_aborted_tool_names(&idx, 1, &server, |_| "bash".to_string());
        assert_eq!(names, vec!["bash".to_string()]);
    }

    #[test]
    fn unknown_local_tool_lists_sorted() {
        let mut s = HashSet::new();
        s.insert("zebra".into());
        s.insert("alpha".into());
        let m = unknown_local_tool_error_message("foo", &s);
        assert_eq!(m, "Unknown tool 'foo'. Available: alpha, zebra");
    }

    // ── Session 0e37eb46 regression: cache-hit must NOT look empty ──
    //
    // The previous stub "(cached — identical call already executed…)"
    // was routinely misread by LLMs as "nothing here" → model called
    // a variant of the same read to re-fetch. Burned 3 rounds in
    // t5 r6/r7/r12 of session 0e37eb46 alone.
    //
    // New contract (see `idempotency_cache_hit_message` doc):
    //   * Always starts with "(cached" so sentinel detectors work.
    //   * Short outputs (≤ 2000 bytes) come back INLINE — the model
    //     sees real content and knows it's cached, no ambiguity.
    //   * Large outputs come back with a preview + byte count +
    //     explicit scroll-back pointer. The preview and size prove
    //     there's real content and that it's NOT the "empty result"
    //     the model previously assumed.

    #[test]
    fn idempotency_cache_hit_short_output_inlined() {
        let out = "file content line 1\nfile content line 2\n";
        let m = idempotency_cache_hit_message(out);
        assert!(m.starts_with("(cached"), "must start with (cached sentinel");
        assert!(
            m.contains("file content line 1"),
            "short output must be inlined so the LLM can see real content \
             (session 0e37eb46 regression)"
        );
    }

    #[test]
    fn idempotency_cache_hit_large_output_preview_has_content_and_size() {
        // Build an output larger than IDEMPOTENCY_INLINE_MAX_BYTES.
        let big = format!(
            "HEADER_SIGNAL\n{}",
            "x".repeat(IDEMPOTENCY_INLINE_MAX_BYTES + 500)
        );
        let m = idempotency_cache_hit_message(&big);
        assert!(m.starts_with("(cached"), "sentinel preserved");
        // Byte count must be visible so the model KNOWS there's real
        // content, not empty.
        assert!(
            m.contains(&format!("{}", big.len())),
            "large-output path must include byte count so the model knows \
             the cache-hit has real content: {m}"
        );
        // Preview must include the actual beginning of content, not
        // just a generic stub.
        assert!(
            m.contains("HEADER_SIGNAL"),
            "preview must include head of actual content (session 0e37eb46 \
             r6/r7/r12 pattern: LLM reads bare stub as 'empty' and retries): {m}"
        );
        // Explicit hint telling the model where to find the full
        // content — not a vague "re-read if needed".
        assert!(
            m.to_lowercase().contains("scroll back")
                || m.to_lowercase().contains("earlier tool_result"),
            "large-output path must point at the earlier tool_result: {m}"
        );
    }

    #[test]
    fn idempotency_cache_hit_empty_output_is_explicit() {
        // Empty-in, empty-meta-out: still NOT a bare stub that looks
        // like "nothing here" — state that the original was empty.
        let m = idempotency_cache_hit_message("");
        assert!(m.starts_with("(cached"));
        assert!(
            m.to_lowercase().contains("empty"),
            "empty-output cache-hit must say so explicitly: {m}"
        );
    }

    #[test]
    fn idempotency_cache_hit_empty_object_placeholder_is_degraded_not_replayed() {
        let m = idempotency_cache_hit_message("{}");
        assert!(m.starts_with("(cached"));
        assert!(
            m.to_lowercase().contains("degraded") || m.to_lowercase().contains("placeholder"),
            "empty-object cache hits should be marked as suspect historical data: {m}"
        );
        assert!(
            !m.lines().any(|line| line.trim() == "{}"),
            "cache-hit wrapper must not replay a bare '{{}}' line to the LLM: {m}"
        );
    }

    #[test]
    fn idempotency_cache_hit_never_returns_bare_stub_placeholder() {
        // The failure mode we're protecting against is the model
        // reading the cache-hit as "no content, try something else".
        // Whatever the output shape, the message must NOT be just the
        // old placeholder with no signal about the actual content.
        let too_vague = "(cached — identical call already executed in this conversation. \
                         Re-read the file only if you need the content again.)";
        let m = idempotency_cache_hit_message("real output here");
        assert_ne!(
            m, too_vague,
            "cache-hit must not regress to the content-free stub"
        );
    }

    #[test]
    fn headless_duplicate_pair_matches_constant_body() {
        let (msg, tr) = headless_openai_duplicate_within_turn_pair("c1", "bash");
        assert_eq!(
            msg["content"].as_str(),
            Some(HEADLESS_DUPLICATE_WITHIN_TURN_BODY)
        );
        assert_eq!(
            tr["result"].as_str(),
            Some(HEADLESS_DUPLICATE_WITHIN_TURN_BODY)
        );
    }

    #[test]
    fn begin_headless_opening_counts_server_calls() {
        let server = vec![json!({"id":"1","name":"bash","arguments":{}})];
        let edge: Vec<Row> = vec![];
        let o = begin_headless_tool_round_opening(&server, &edge, "");
        assert_eq!(o.indices.len(), 1);
        assert_eq!(o.tool_count, 1);
    }

    #[test]
    fn begin_headless_opening_does_not_invent_an_action_for_an_empty_round() {
        let edge: Vec<Row> = vec![];
        let opening = begin_headless_tool_round_opening(&[], &edge, "");

        assert!(opening.indices.is_empty());
        assert_eq!(opening.tool_count, 0);
        assert!(opening.assistant_message.get("tool_calls").is_none());
    }

    #[test]
    fn tool_calls_for_stall_guard_prefers_server_list() {
        let server = vec![json!({"id":"1","name":"bash","arguments":{}})];
        let edge = vec![Row {
            tool: "read_file".into(),
            args: json!({}),
            output: "".into(),
            duration_ms: 0,
        }];
        let g = tool_calls_for_stall_guard(&server, &edge);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0]["name"], "bash");
    }

    #[test]
    fn tool_calls_for_stall_guard_synthetic_ids() {
        let edge = vec![
            Row {
                tool: "a".into(),
                args: json!({}),
                output: "".into(),
                duration_ms: 0,
            },
            Row {
                tool: "b".into(),
                args: json!({"x":1}),
                output: "".into(),
                duration_ms: 0,
            },
        ];
        let g = tool_calls_for_stall_guard(&[], &edge);
        assert_eq!(g[0]["id"], "edge-0");
        assert_eq!(g[1]["id"], "edge-1");
        assert_eq!(g[1]["name"], "b");
    }

    #[test]
    fn read_only_tools_are_all_read_only() {
        const SIDE_EFFECTFUL: &[&str] = &[
            "bash",
            "write_file",
            "str_replace",
            "delete_file",
            "multi_edit",
            "git",
            "github",
            "mo_query",
            "memory", // action-aware; conservatively classified as Mutating
        ];
        for &tool in READ_ONLY_TOOLS.iter() {
            assert!(
                !SIDE_EFFECTFUL.contains(&tool),
                "READ_ONLY_TOOLS must not contain side-effectful tool: {tool}"
            );
        }
    }

    #[test]
    fn read_only_tools_covers_git_and_github_reads() {
        for expected in &["read_file", "grep", "glob", "list_dir"] {
            assert!(
                READ_ONLY_TOOLS.contains(expected),
                "missing cacheable tool: {expected}"
            );
        }
    }

    #[test]
    fn openai_assistant_message_from_server_tool_calls() {
        let server = vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
        })];
        let msg = openai_assistant_with_tool_calls_message(&server, &[] as &[Row], "");
        assert_eq!(msg["role"], "assistant");
        assert!(msg["content"].is_null());
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc.len(), 1);
        assert_eq!(tc[0]["id"], "call_1");
        assert_eq!(tc[0]["type"], "function");
        assert_eq!(tc[0]["function"]["name"], "read_file");
        let args: Value =
            serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "a.rs");
    }

    #[test]
    fn openai_assistant_message_from_edge_round_default_ids() {
        let edge = vec![Row {
            tool: "grep".into(),
            args: json!({"pattern": "x"}),
            output: "".into(),
            duration_ms: 0,
        }];
        let msg = openai_assistant_with_tool_calls_message(&[], &edge, "");
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc[0]["id"], "edge-0");
        assert_eq!(tc[0]["function"]["name"], "grep");
    }

    #[derive(Debug)]
    struct RowWithRequestId {
        tool: String,
        args: Value,
        output: String,
        request_id: String,
    }

    impl EdgeToolRoundRow for RowWithRequestId {
        fn tool_name(&self) -> &str {
            &self.tool
        }
        fn tool_args(&self) -> &Value {
            &self.args
        }
        fn tool_output(&self) -> &str {
            &self.output
        }
        fn assistant_tool_call_id(&self, index: usize) -> String {
            if self.request_id.is_empty() {
                format!("edge-{index}")
            } else {
                self.request_id.clone()
            }
        }
        fn has_explicit_assistant_tool_call_id(&self) -> bool {
            !self.request_id.is_empty()
        }
    }

    #[test]
    fn openai_assistant_message_edge_uses_request_id_when_set() {
        let edge = vec![RowWithRequestId {
            tool: "bash".into(),
            args: json!({"command": "true"}),
            output: "ok".into(),
            request_id: "req-abc".into(),
        }];
        let msg = openai_assistant_with_tool_calls_message(&[], &edge, "");
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc[0]["id"], "req-abc");
    }

    #[test]
    fn synthetic_edge_slot_id_matches_assistant_tool_call_id() {
        let edge = vec![RowWithRequestId {
            tool: "agent_fanout".into(),
            args: json!({"action": "start"}),
            output: r#"{"status":"started","group_id":"fanout-1"}"#.into(),
            request_id: "req-fanout-1".into(),
        }];
        let opening = begin_headless_tool_round_opening(&[], &edge, "");
        let assistant_id = opening.assistant_message["tool_calls"][0]["id"]
            .as_str()
            .expect("assistant tool call id");
        let slot = resolve_headless_tool_slot(opening.indices[0], &[], |i| {
            let edge = &edge[i];
            (
                edge.assistant_tool_call_id(i),
                edge.tool_name().to_string(),
                edge.tool_args().clone(),
            )
        });

        assert_eq!(assistant_id, "req-fanout-1");
        assert_eq!(slot.id, assistant_id);
    }

    #[test]
    fn take_edge_output_prefers_request_id_when_arguments_differ() {
        let edge_args = json!({
            "action": "start",
            "target_count": 3,
            "slots": [{"id": "review", "prompt": "review"}],
            "title": "Review"
        });
        let server_args = json!({
            "action": "start",
            "target_count": 3,
            "slots": [{"id": "review", "prompt": "review"}]
        });
        let rows = vec![RowWithRequestId {
            tool: "agent_fanout".into(),
            args: edge_args,
            output: r#"{"completed":3}"#.into(),
            request_id: "call-fanout-1".into(),
        }];
        let mut consumed = vec![false];

        let out = take_edge_output_for_tool_call_id_or_signature_with_duration(
            "call-fanout-1",
            "agent_fanout",
            &server_args,
            &rows,
            &mut consumed,
            &HashMap::new(),
        );

        assert_eq!(out.output, r#"{"completed":3}"#);
        assert!(consumed[0]);
    }

    #[test]
    fn take_edge_output_falls_back_to_signature_when_request_id_differs() {
        let args = json!({"pattern": "needle"});
        let rows = vec![RowWithRequestId {
            tool: "grep".into(),
            args: args.clone(),
            output: "matched by args".into(),
            request_id: "other-call".into(),
        }];
        let mut consumed = vec![false];

        let out = take_edge_output_for_tool_call_id_or_signature_with_duration(
            "call-grep-1",
            "grep",
            &args,
            &rows,
            &mut consumed,
            &HashMap::new(),
        );

        assert_eq!(out.output, "matched by args");
        assert!(consumed[0]);
    }

    #[test]
    fn openai_assistant_message_omits_empty_tool_calls() {
        let msg = openai_assistant_with_tool_calls_message(&[] as &[Value], &[] as &[Row], "");
        assert!(msg.get("tool_calls").is_none(), "{msg:?}");
    }

    #[test]
    fn openai_assistant_message_includes_reasoning_content_when_non_empty() {
        let msg = openai_assistant_with_tool_calls_message(
            &[],
            &[Row {
                tool: "t".into(),
                args: json!({}),
                output: "".into(),
                duration_ms: 0,
            }],
            "think",
        );
        assert_eq!(msg["reasoning_content"], "think");
    }

    #[test]
    fn openai_tool_roundtrip_values_matches_headless_shape() {
        let (m, tr) = openai_tool_roundtrip_values("call-1", "read_file", "ok");
        assert_eq!(m["role"], "tool");
        assert_eq!(m["tool_call_id"], "call-1");
        assert_eq!(m["content"], "ok");
        assert_eq!(tr["tool_call_id"], "call-1");
        assert_eq!(tr["name"], "read_file");
        assert_eq!(tr["result"], "ok");
    }

    #[test]
    fn openai_tool_roundtrip_values_with_result_fields_merges_metadata() {
        let extra_fields = serde_json::Map::from_iter([(
            "pre_state_snapshot_id".to_string(),
            Value::String("moq_snap_2".into()),
        )]);
        let (_, tr) = openai_tool_roundtrip_values_with_result_fields(
            "call-2",
            "mo_query",
            "OK (no results)",
            Some(&extra_fields),
        );
        assert_eq!(tr["tool_call_id"], "call-2");
        assert_eq!(tr["name"], "mo_query");
        assert_eq!(tr["result"], "OK (no results)");
        assert_eq!(tr["pre_state_snapshot_id"], "moq_snap_2");
    }

    #[test]
    fn parse_canonical_tool_call_rejects_missing_or_empty_id() {
        let tc = json!({"type":"function","function":{"name":"bash","arguments":"{}"}});
        let (id, name, _) = parse_flat_tool_call_event(&tc);
        assert!(id.is_empty());
        assert!(name.is_empty());

        let tc2 = json!({"id":"","type":"function","function":{"name":"bash","arguments":"{}"}});
        let (id2, _, _) = parse_flat_tool_call_event(&tc2);
        assert!(id2.is_empty());
    }

    // ── canonicalize_provider_tool_batch regression tests ───────────────

    #[test]
    fn canonical_batch_borrows_when_all_calls_are_exact() {
        let tcs = vec![
            json!({"id":"a","type":"function","function":{"name":"bash","arguments":"{}"}}),
            json!({"id":"b","type":"function","function":{"name":"grep","arguments":"{}"}}),
        ];
        let result = canonicalize_provider_tool_batch(&tcs).unwrap();
        assert!(matches!(result, std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn canonical_batch_rejects_entire_batch_when_one_identity_is_empty() {
        let tcs = vec![
            json!({"id":"","type":"function","function":{"name":"bash","arguments":"{}"}}),
            json!({"id":"ok","type":"function","function":{"name":"grep","arguments":"{}"}}),
        ];
        let error = canonicalize_provider_tool_batch(&tcs).unwrap_err();
        assert!(matches!(
            error,
            ProviderToolBatchError::InvalidCall { index: 0, .. }
        ));
    }

    #[test]
    fn canonical_batch_rejects_missing_id() {
        let tcs = vec![json!({"type":"function","function":{"name":"bash","arguments":"{}"}})];
        assert!(matches!(
            canonicalize_provider_tool_batch(&tcs),
            Err(ProviderToolBatchError::InvalidCall { index: 0, .. })
        ));
    }

    #[test]
    fn canonical_batch_never_mints_identity() {
        let tcs = vec![
            json!({"id":"","type":"function","function":{"name":"a","arguments":"{}"}}),
            json!({"type":"function","function":{"name":"b","arguments":"{}"}}),
        ];
        assert!(canonicalize_provider_tool_batch(&tcs).is_err());
    }

    #[test]
    fn canonical_batch_rejects_idless_call_before_assistant_pairing() {
        let tcs =
            vec![json!({"id":"","type":"function","function":{"name":"bash","arguments":"{}"}})];
        assert!(canonicalize_provider_tool_batch(&tcs).is_err());
    }

    #[test]
    fn canonical_batch_rejects_entire_batch_for_duplicate_exact_identity() {
        let tcs = vec![
            json!({"id":"same","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}),
            json!({"id":"same","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}),
        ];

        assert!(matches!(
            canonicalize_provider_tool_batch(&tcs),
            Err(ProviderToolBatchError::DuplicateIdentity { .. })
        ));
    }

    #[test]
    fn canonical_batch_rejects_duplicate_identity_with_conflicting_payloads() {
        let tcs = vec![
            json!({"id":"same","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"a\"}"}}),
            json!({"id":"same","type":"function","function":{"name":"read_file","arguments":"{\"path\":\"b\"}"}}),
        ];

        assert!(matches!(
            canonicalize_provider_tool_batch(&tcs),
            Err(ProviderToolBatchError::DuplicateIdentity { .. })
        ));
    }

    #[test]
    fn canonical_batch_normalizes_semantically_equivalent_argument_json() {
        let tcs = vec![json!({
            "id":"call-a",
            "function":{"name":"read_file","arguments":"{\n  \"path\": \"a\", \"line_start\": 1\n}"}
        })];

        let canonical = canonicalize_provider_tool_batch(&tcs).unwrap();

        assert_eq!(canonical.len(), 1);
        assert_eq!(canonical[0]["type"], "function");
        assert_eq!(
            canonical[0]["function"]["arguments"],
            serde_json::to_string(&json!({"path":"a", "line_start":1})).unwrap()
        );
    }

    /// parse_flat_tool_call_event must handle OpenAI format (function.name/arguments)
    /// — this is the format produced by normalize_tool_call_for_accum and stored
    /// in accum.tool_calls. Regression test for the qwen-plus infinite loop.
    #[test]
    fn parse_flat_tool_call_openai_format() {
        let tc = json!({
            "id": "call_abc",
            "type": "function",
            "function": {
                "name": "git",
                "arguments": "{\"action\":\"log\",\"n\":5}"
            }
        });
        let (id, name, args) = parse_flat_tool_call_event(&tc);
        assert_eq!(id, "call_abc");
        assert_eq!(name, "git");
        assert_eq!(args, json!({"action": "log", "n": 5}));
    }

    /// OpenAI format with empty function.name should still return empty
    /// (not panic or return a different field).
    #[test]
    fn parse_flat_tool_call_openai_format_empty_name() {
        let tc = json!({
            "id": "call_xyz",
            "type": "function",
            "function": {
                "name": "",
                "arguments": "{}"
            }
        });
        let (_, name, _) = parse_flat_tool_call_event(&tc);
        assert_eq!(name, "");
    }

    /// resolve_headless_tool_slot must extract name from OpenAI-format tool calls
    /// in accum.tool_calls. This is the end-to-end path that was broken.
    #[test]
    fn resolve_headless_slot_openai_format_extracts_name() {
        let server = vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {
                "name": "git",
                "arguments": "{\"action\":\"show\",\"revision\":\"abc\"}"
            }
        })];
        let slot =
            resolve_headless_tool_slot(HeadlessRoundToolIdx::ServerToolCall(0), &server, |_| {
                panic!("edge lookup not used")
            });
        assert_eq!(slot.name, "git");
        assert_eq!(slot.args, json!({"action": "show", "revision": "abc"}));
    }

    /// Regression: openai_tool_call_entries_from_server must handle OpenAI-format
    /// tool_calls (function.name / function.arguments) — the format produced by
    /// normalize_tool_call_for_accum and stored in accum.tool_calls.
    /// Bug: old code read tc.get("name") (flat) instead of tc["function"]["name"],
    /// producing empty names and empty arguments in assistant message history.
    #[test]
    fn openai_assistant_message_from_server_openai_format_tool_calls() {
        let server = vec![json!({
            "id": "call_abc123",
            "type": "function",
            "function": {
                "name": "skill",
                "arguments": "{\"skill_name\":\"review-changes\"}"
            }
        })];
        let msg = openai_assistant_with_tool_calls_message(&server, &[] as &[Row], "");
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(
            tc[0]["function"]["name"], "skill",
            "must extract name from OpenAI-format function.name"
        );
        let args: Value =
            serde_json::from_str(tc[0]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(
            args["skill_name"], "review-changes",
            "must preserve arguments from OpenAI-format function.arguments"
        );
    }

    /// Multiple canonical tool calls preserve order and payload.
    #[test]
    fn openai_assistant_message_multiple_canonical_tool_calls() {
        let server = vec![
            json!({"id": "c1", "type": "function", "function": {"name": "git", "arguments": "{\"action\":\"status\"}"}}),
            json!({"id": "c2", "type": "function", "function": {"name": "git", "arguments": "{\"action\":\"diff\",\"ref\":\"HEAD\"}"}}),
        ];
        let msg = openai_assistant_with_tool_calls_message(&server, &[] as &[Row], "");
        let tc = msg["tool_calls"].as_array().unwrap();
        assert_eq!(tc[0]["function"]["name"], "git");
        assert_eq!(tc[1]["function"]["name"], "git");
        let args: Value =
            serde_json::from_str(tc[1]["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["action"], "diff");
        assert_eq!(args["ref"], "HEAD");
    }

    #[test]
    fn plan_lifecycle_no_matching_edge_error_does_not_suggest_bash() {
        let message = no_matching_edge_execution_message("exit_plan_mode");

        assert!(message.contains("trusted plan executor"));
        assert!(
            !message.contains("Workaround: use `bash`"),
            "plan approval must not be replaced with bash: {message}"
        );
    }

    #[test]
    fn agent_fanout_no_matching_edge_error_does_not_suggest_bash_or_serial_agent() {
        let message = no_matching_edge_execution_message("agent_fanout");

        assert!(message.contains("multi-agent runtime"), "{message}");
        // The message must NOT offer a degraded serial/bash fallback — those
        // silently reduce coverage and are never equivalent to fan-out.
        assert!(
            !message.contains("explicitly accepts degraded"),
            "fanout message must not smuggle in a degraded-coverage escape hatch: {message}"
        );
        assert!(
            !message.contains("Workaround: use `bash`"),
            "multi-agent execution must not be replaced with bash: {message}"
        );
        assert!(
            message.contains("visible and executable in this turn"),
            "fanout binding message must explain the executable-tool boundary: {message}"
        );
    }

    #[test]
    fn non_plan_no_matching_edge_error_forbids_substitution() {
        let message = no_matching_edge_execution_message("github");

        // The generic binding message used to suggest `bash` as a workaround,
        // which let the model route around the binding gate. It must now tell
        // the model NOT to substitute and to ask the user for a bound mode.
        assert!(
            !message.contains("Workaround: use `bash`"),
            "generic binding message must not suggest bash substitution: {message}"
        );
        assert!(
            message.contains("Use only tools that have a bound executor"),
            "generic binding message must state the bound-executor constraint: {message}"
        );
    }
}
