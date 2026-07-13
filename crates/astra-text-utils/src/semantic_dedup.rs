//! Semantic near-duplicate detection for tool calls across turns.
//!
//! Three tiers of dedup:
//! - Tier 1 (exact): handled externally by `normalize_call_sig()` in chat_stream
//! - Tier 2 (parameter-aware): same tool, semantically equivalent args (case, trailing slash)
//! - Tier 3 (output similarity hint): token cosine similarity on tool outputs
//!
//! No embeddings, no corpus IDF, and no model calls — pure string processing.

use crate::text_tokenize::{build_tf, tokenize};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

/// Structured record of a detected near-duplicate tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupAuditRecord {
    pub tool_name: String,
    pub duplicate_count: u32,
    pub original_signature: String,
}

/// Default similarity threshold for output-based dedup.
/// 0.75 = conservative (avoids false positives on different data).
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.75;

// ─── Tier 2: Parameter-Aware Semantic Key ────────────────────────────────────

/// Compute a semantic key for a tool call that normalizes parameter variations.
///
/// Returns `Some(key)` for tools where parameter equivalence can be detected,
/// `None` for tools where args are too context-dependent (e.g., bash).
///
/// Normalization:
/// - Paths: strip trailing slashes
/// - Repos: case-insensitive
/// - Git refs: default to "HEAD" when omitted
pub fn semantic_call_key(tool_name: &str, args: &Value) -> Option<String> {
    match tool_name {
        // File-based tools: key on normalized path + output-critical params
        "read_file" => read_file_semantic_key(args),
        "glob" => {
            let pattern = arg_str(args, "pattern").unwrap_or("*");
            let path = arg_str(args, "path").unwrap_or(".");
            let offset = arg_u64(args, "offset")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let head_limit = arg_u64(args, "head_limit")
                .map(|v| v.to_string())
                .unwrap_or_default();
            Some(format!(
                "glob:{}:{}:offset={}:limit={}",
                normalize_path(path),
                pattern,
                offset,
                head_limit,
            ))
        }
        // Grep: key on path + pattern + output_mode + include — all affect output
        "grep" => {
            let path = arg_str(args, "path").unwrap_or(".");
            let pattern = arg_str(args, "pattern").unwrap_or(".*");
            let output_mode = arg_str(args, "output_mode").unwrap_or("content");
            let include = arg_str(args, "include").unwrap_or("");
            Some(format!(
                "grep:{}:{}:mode={}:include={}",
                normalize_path(path),
                pattern,
                output_mode,
                include,
            ))
        }
        "github" => semantic_github_key(args),
        "git" => semantic_git_key(args),
        "get_agent_info" => Some(tool_name.to_string()),
        "list_dir" => {
            let path = arg_str(args, "path").unwrap_or(".");
            let depth = arg_u64(args, "depth")
                .map(|v| v.to_string())
                .unwrap_or_default();
            Some(format!("list_dir:{}:depth={}", normalize_path(path), depth))
        }
        "bash" => semantic_bash_git_key(args),
        // Non-cacheable tools (write_file, web_fetch, most bash commands, etc.) — no semantic key
        // Analysis tools: key on target symbol/file + output-shaping params
        "symbols" => {
            let path = arg_str(args, "path")?;
            let pattern = arg_str(args, "pattern").unwrap_or("");
            let kinds = args.get("kinds").map(|v| v.to_string()).unwrap_or_default();
            let calls = arg_bool(args, "calls").unwrap_or(false);
            Some(format!(
                "symbols:{}:pattern={}:kinds={}:calls={}",
                normalize_path(path),
                pattern,
                kinds,
                calls,
            ))
        }
        "find_definition" | "find_references" => {
            let path = arg_str(args, "file").or_else(|| arg_str(args, "path"))?;
            let symbol = arg_str(args, "symbol").unwrap_or("");
            Some(format!("{}:{}:{}", tool_name, normalize_path(path), symbol))
        }
        "symbol_search" => {
            let query = arg_str(args, "query").unwrap_or("");
            Some(format!("symbol_search:{}", query.to_lowercase()))
        }
        "hover_info" => {
            let file = arg_str(args, "file")?;
            let line = args.get("line").and_then(Value::as_u64).unwrap_or(0);
            let col = args.get("column").and_then(Value::as_u64).unwrap_or(0);
            Some(format!(
                "hover_info:{}:{}:{}",
                normalize_path(file),
                line,
                col
            ))
        }
        "call_graph" => {
            let symbol = arg_str(args, "symbol")?;
            let file = arg_str(args, "file").unwrap_or("");
            let callers = args
                .get("callers")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(format!(
                "call_graph:{}:{}:callers={}",
                symbol,
                normalize_path(file),
                callers
            ))
        }
        "type_hierarchy" | "dead_code" | "extract_members" => {
            let file = arg_str(args, "file")
                .or_else(|| arg_str(args, "path"))
                .unwrap_or(".");
            Some(format!("{}:{}", tool_name, normalize_path(file)))
        }
        // Memory tool is action-aware; dedup keys depend on the action.
        // Only pure-read / idempotent actions should dedupe — write verbs
        // (remember, forget, update, focus, reflect, feedback) must not be
        // merged across duplicate calls.
        "memory" => {
            let action = arg_str(args, "action").unwrap_or("");
            match action {
                "recall" => {
                    let query = arg_str(args, "query").unwrap_or("");
                    Some(format!("memory_recall:{}", query.to_lowercase()))
                }
                "profile" => Some("memory_profile".to_string()),
                "expand" => arg_str(args, "memory_id").map(|id| format!("memory_expand:{id}")),
                _ => None,
            }
        }
        _ => None,
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn arg_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn arg_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_end_matches('/').to_string()
}

fn read_file_semantic_key(args: &Value) -> Option<String> {
    let path = arg_str(args, "path")?;
    let (start, end) = read_file_effective_line_bounds(args);
    let outline = arg_bool(args, "outline").unwrap_or(false);
    Some(format!(
        "read_file:{}:start={}:end={}:outline={}",
        normalize_path(path),
        start.map(|v| v.to_string()).unwrap_or_default(),
        end.map(|v| v.to_string()).unwrap_or_default(),
        outline,
    ))
}

fn read_file_effective_line_bounds(args: &Value) -> (Option<u64>, Option<u64>) {
    (arg_u64(args, "start_line"), arg_u64(args, "end_line"))
}

fn read_file_path_from_semantic_key(key: &str) -> Option<&str> {
    key.strip_prefix("read_file:")
        .and_then(|rest| rest.split_once(":start=").map(|(path, _)| path))
}

fn normalize_repo(repo: &str) -> String {
    repo.trim().to_lowercase().trim_end_matches('/').to_string()
}

fn semantic_github_key(args: &Value) -> Option<String> {
    let action = arg_str(args, "action")?;
    match action {
        "list_prs" | "list_issues" | "ci_status" | "repo_stats" => {
            let repo = arg_str(args, "repo")?;
            Some(format!("github:{}:{}", action, normalize_repo(repo)))
        }
        "get_pr" => {
            let repo = arg_str(args, "repo")?;
            let number = args.get("pr_number").or_else(|| args.get("number"))?;
            Some(format!("github:get_pr:{}#{}", normalize_repo(repo), number))
        }
        "get_issue" => {
            let repo = arg_str(args, "repo")?;
            let number = args.get("issue_number").or_else(|| args.get("number"))?;
            Some(format!(
                "github:get_issue:{}#{}",
                normalize_repo(repo),
                number
            ))
        }
        _ => None,
    }
}

fn semantic_git_key(args: &Value) -> Option<String> {
    let action = arg_str(args, "action")?;
    match action {
        "status" => Some("git:status".to_string()),
        "diff" => {
            let git_ref = arg_str(args, "ref").unwrap_or("HEAD");
            let base_ref = arg_str(args, "base_ref").unwrap_or("");
            let staged = arg_bool(args, "staged").unwrap_or(false);
            let stat_only = arg_bool(args, "stat_only").unwrap_or(false);
            let path = arg_str(args, "path").unwrap_or("");
            Some(format!(
                "git:diff:{}..{}:staged={}:stat_only={}:path={}",
                base_ref,
                git_ref,
                staged,
                stat_only,
                normalize_path(path)
            ))
        }
        "log" => {
            let git_ref = arg_str(args, "ref").unwrap_or("HEAD");
            let n = arg_u64(args, "n").unwrap_or(10);
            let path = arg_str(args, "path").unwrap_or("");
            Some(format!(
                "git:log:{}:n={}:path={}",
                git_ref,
                n,
                normalize_path(path)
            ))
        }
        "show" => {
            let revision = arg_str(args, "revision").unwrap_or("HEAD");
            let path = arg_str(args, "path").unwrap_or("");
            let stat = arg_bool(args, "stat_only").unwrap_or(false);
            Some(format!(
                "git:show:{}:{}:{}",
                revision.to_lowercase(),
                normalize_path(path),
                stat
            ))
        }
        "blame" => {
            let file = arg_str(args, "path").or_else(|| arg_str(args, "file"))?;
            let line_start = arg_u64(args, "line_start")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let line_end = arg_u64(args, "line_end")
                .map(|v| v.to_string())
                .unwrap_or_default();
            Some(format!(
                "git:blame:{}:line_start={}:line_end={}",
                normalize_path(file),
                line_start,
                line_end,
            ))
        }
        "file_history" => {
            let file = arg_str(args, "file")?;
            let n = arg_u64(args, "n").unwrap_or(10);
            Some(format!("git:file_history:{}:n={}", normalize_path(file), n))
        }
        "log_search" => {
            let query = arg_str(args, "query").unwrap_or("");
            Some(format!("git:log_search:{}", query.to_lowercase()))
        }
        "contributors" => {
            let path = arg_str(args, "path").unwrap_or("");
            let since = arg_str(args, "since").unwrap_or("");
            Some(format!(
                "git:contributors:path={}:since={}",
                normalize_path(path),
                since
            ))
        }
        _ => None,
    }
}

fn semantic_bash_git_key(args: &Value) -> Option<String> {
    let command = arg_str(args, "command")?.trim();
    if command
        .chars()
        .any(|ch| matches!(ch, '|' | ';' | '&' | '>' | '<' | '$' | '`' | '\n' | '\r'))
    {
        return None;
    }
    let mut parts: Vec<&str> = command.split_whitespace().collect();
    if parts.first() == Some(&"git") && parts.get(1) == Some(&"--no-pager") {
        parts.remove(1);
    }
    match parts.as_slice() {
        ["git", "status"] | ["git", "status", "--short"] | ["git", "status", "--porcelain"] => {
            Some("git:status".to_string())
        }
        ["git", "diff"] | ["git", "diff", "HEAD"] => {
            Some("git:diff:..HEAD:staged=false:stat_only=false:path=".to_string())
        }
        ["git", "diff", "--stat"] => {
            Some("git:diff:..HEAD:staged=false:stat_only=true:path=".to_string())
        }
        ["git", "diff", "--cached"] | ["git", "diff", "--staged"] => {
            Some("git:diff:..HEAD:staged=true:stat_only=false:path=".to_string())
        }
        ["git", "diff", "--", path] => Some(format!(
            "git:diff:..HEAD:staged=false:stat_only=false:path={}",
            normalize_path(path)
        )),
        _ => None,
    }
}

// ─── Tier 3: Output Similarity Hint ──────────────────────────────────────────

/// Token-frequency cosine similarity between two tool outputs.
/// Returns 0.0-1.0. Outputs shorter than MIN_OUTPUT_LEN are not compared.
const MIN_OUTPUT_LEN: usize = 30;
const DEFAULT_PARAM_CACHE_ENTRIES: usize = 512;
const DEFAULT_AUDIT_ENTRIES: usize = 256;

/// Conservative read-only predicate used to decide whether a short cached
/// output can be safely re-executed. Action-shaped tools must inspect args so
/// mutating actions are never replayed blindly.
fn is_read_only_tool(tool_name: &str, args: &Value) -> bool {
    if tool_name == "git" {
        return matches!(
            arg_str(args, "action"),
            Some(
                "status"
                    | "diff"
                    | "log"
                    | "show"
                    | "blame"
                    | "file_history"
                    | "log_search"
                    | "contributors"
            )
        );
    }
    if tool_name == "github" {
        return matches!(
            arg_str(args, "action"),
            Some("list_prs" | "get_pr" | "ci_status" | "repo_stats" | "list_issues" | "get_issue")
        );
    }
    matches!(
        tool_name,
        "read_file"
            | "list_dir"
            | "grep"
            | "glob"
            | "symbols"
            | "find_definition"
            | "find_references"
            | "lsp"
    )
}

pub fn token_cosine_similarity(output1: &str, output2: &str) -> f64 {
    // Too-short outputs aren't meaningful for similarity comparison
    if output1.len() < MIN_OUTPUT_LEN || output2.len() < MIN_OUTPUT_LEN {
        return 0.0;
    }
    if output1 == output2 {
        return 1.0;
    }

    let terms1 = tokenize(output1);
    let terms2 = tokenize(output2);
    if terms1.is_empty() || terms2.is_empty() {
        return 0.0;
    }

    let tf1 = build_tf(&terms1);
    let tf2 = build_tf(&terms2);

    // Cosine similarity on TF vectors
    let mut dot = 0.0_f64;
    let mut norm1 = 0.0_f64;
    let mut norm2 = 0.0_f64;

    for (term, &c1) in &tf1 {
        norm1 += c1 * c1;
        if let Some(&c2) = tf2.get(term) {
            dot += c1 * c2;
        }
    }
    for &c2 in tf2.values() {
        norm2 += c2 * c2;
    }

    let denom = norm1.sqrt() * norm2.sqrt();
    if denom < 1e-9 {
        0.0
    } else {
        (dot / denom).min(1.0)
    }
}

// ─── SemanticDedup Tracker ───────────────────────────────────────────────────

/// Tracks tool call history for semantic near-duplicate detection.
pub struct SemanticDedup {
    threshold: f64,
    /// Tier 2: semantic_key → (turn, tool_name, context_generation)
    param_cache: HashMap<String, (usize, String, u64)>,
    /// FIFO order for bounding `param_cache`.
    param_order: VecDeque<String>,
    /// Tier 3: truncated output for same semantic key similarity comparison.
    /// Only stores first 2000 chars of output to bound memory.
    output_log: Vec<OutputLogEntry>,
    /// Max entries in output_log before oldest are evicted
    max_output_entries: usize,
    /// Max semantic-key entries before oldest are evicted.
    max_param_entries: usize,
    /// Structured audit trail of detected duplicates.
    dedup_audit: Vec<DedupAuditRecord>,
    /// Max audit records before oldest are evicted.
    max_audit_entries: usize,
}

#[derive(Debug, Clone)]
struct OutputLogEntry {
    tool_name: String,
    semantic_key: Option<String>,
    turn: usize,
    context_generation: u64,
    output: String,
}

impl SemanticDedup {
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold,
            param_cache: HashMap::new(),
            param_order: VecDeque::new(),
            output_log: Vec::new(),
            max_output_entries: 50,
            max_param_entries: DEFAULT_PARAM_CACHE_ENTRIES,
            dedup_audit: Vec::new(),
            max_audit_entries: DEFAULT_AUDIT_ENTRIES,
        }
    }

    /// Clear cached observations whose freshness depends on external state.
    /// Audit records are retained for diagnostics and remain bounded.
    pub fn clear_observation_cache(&mut self) {
        self.param_cache.clear();
        self.param_order.clear();
        self.output_log.clear();
    }

    fn record_param_key(
        &mut self,
        key: String,
        current_turn: usize,
        tool_name: &str,
        context_generation: u64,
    ) {
        if self.param_cache.contains_key(&key)
            && let Some(pos) = self
                .param_order
                .iter()
                .position(|existing| existing == &key)
        {
            self.param_order.remove(pos);
        }
        self.param_order.push_back(key.clone());
        self.param_cache.insert(
            key,
            (current_turn, tool_name.to_string(), context_generation),
        );
        while self.param_cache.len() > self.max_param_entries {
            let Some(oldest) = self.param_order.pop_front() else {
                break;
            };
            self.param_cache.remove(&oldest);
        }
    }

    fn record_audit_duplicate(&mut self, tool_name: &str, signature: String) {
        let existing = self
            .dedup_audit
            .iter_mut()
            .find(|r| r.tool_name == tool_name && r.original_signature == signature);
        if let Some(rec) = existing {
            rec.duplicate_count += 1;
            return;
        }
        self.dedup_audit.push(DedupAuditRecord {
            tool_name: tool_name.to_string(),
            duplicate_count: 1,
            original_signature: signature,
        });
        if self.dedup_audit.len() > self.max_audit_entries {
            let overflow = self.dedup_audit.len() - self.max_audit_entries;
            self.dedup_audit.drain(0..overflow);
        }
    }

    /// Pre-execution hard-block check for high-confidence semantic duplicates.
    ///
    /// Returns `Some((prev_turn, cached_output))` when a Tier 2 parameter-aware
    /// match exists AND a prior output is available in the output log.
    /// The caller should skip execution and return the cached output directly.
    ///
    /// Unlike `check_and_record`, this does NOT update internal state — call
    /// `check_and_record` after execution (or after returning cached output)
    /// to keep the param cache and output log in sync.
    pub fn pre_check_block(
        &self,
        tool_name: &str,
        args: &Value,
        current_turn: usize,
    ) -> Option<(usize, String)> {
        self.pre_check_block_with_generation(tool_name, args, current_turn, 0)
    }

    /// Like [`Self::pre_check_block`], but scopes duplicate detection to an
    /// external observation generation. Callers should advance the generation
    /// whenever the world being observed may have changed, e.g. after a
    /// workspace mutation.
    pub fn pre_check_block_with_generation(
        &self,
        tool_name: &str,
        args: &Value,
        current_turn: usize,
        context_generation: u64,
    ) -> Option<(usize, String)> {
        let sem_key = semantic_call_key(tool_name, args)?;
        let (latest_turn, _prev_tool, latest_generation) = self.param_cache.get(&sem_key)?;
        if *latest_generation != context_generation {
            return None;
        }
        if current_turn <= *latest_turn {
            return None;
        }
        // Find the most recent output from the same tool and semantic key.
        // Skip outputs that have been microcompact-cleared or are stubs —
        // returning "[Cleared]" or a short placeholder instead of real content
        // would leave the caller with nothing useful, forcing them to re-fetch
        // anyway. In that case, allow the tool to re-execute.
        //
        // The short-output bypass is gated to read-only tools: a write/mutation
        // tool that happens to return a short "OK" / "Done" body must NOT be
        // re-executed — the side-effect already happened. For write tools we
        // return the cached short output as-is so the caller short-circuits.
        let tool_is_read_only = is_read_only_tool(tool_name, args);
        for entry in self.output_log.iter().rev() {
            if entry.tool_name == tool_name
                && entry.semantic_key.as_deref() == Some(&sem_key)
                && entry.context_generation == context_generation
            {
                if entry.output.starts_with("[Cleared") || entry.output.starts_with("(cached") {
                    return None; // Force re-execution — cached content is gone
                }
                if tool_is_read_only && entry.output.len() < 20 {
                    return None; // Read-only + trivial output → cheap to re-run
                }
                return Some((entry.turn, entry.output.clone()));
            }
        }
        None
    }

    /// Check if a tool call is a semantic near-duplicate of a previous call.
    ///
    /// Returns `Some((prev_turn, reason))` if a near-duplicate is found.
    /// The caller decides whether to use cached results or inject a hint.
    ///
    /// This should be called AFTER the tool executes (needs output for Tier 3).
    pub fn check_and_record(
        &mut self,
        tool_name: &str,
        args: &Value,
        output: &str,
        current_turn: usize,
    ) -> Option<(usize, String)> {
        self.check_and_record_with_generation(tool_name, args, output, current_turn, 0)
    }

    /// Like [`Self::check_and_record`], but scopes duplicate matching and
    /// output history to a caller-supplied observation generation.
    pub fn check_and_record_with_generation(
        &mut self,
        tool_name: &str,
        args: &Value,
        output: &str,
        current_turn: usize,
        context_generation: u64,
    ) -> Option<(usize, String)> {
        let mut result = None;
        let sem_key = semantic_call_key(tool_name, args);

        // Tier 2: Parameter-aware match
        if let Some(key) = sem_key.as_ref() {
            if let Some((prev_turn, _prev_tool, prev_generation)) = self.param_cache.get(key)
                && *prev_generation == context_generation
                && current_turn > *prev_turn
            {
                result = Some((*prev_turn, "param_match".to_string()));
            }
            self.record_param_key(key.clone(), current_turn, tool_name, context_generation);
        }

        // Tier 3: Output similarity, scoped to the same semantic key.
        // Without key scoping, unrelated read_file ranges or grep patterns can
        // look highly similar and produce false "do not read again" guidance.
        if let Some(key) = sem_key.as_deref()
            && result.is_none()
            && output.len() >= MIN_OUTPUT_LEN
        {
            for entry in self.output_log.iter().rev() {
                if entry.tool_name == tool_name
                    && entry.semantic_key.as_deref() == Some(key)
                    && entry.context_generation == context_generation
                    && current_turn > entry.turn
                {
                    let sim = token_cosine_similarity(output, &entry.output);
                    if sim >= self.threshold {
                        result = Some((entry.turn, format!("token_cosine={:.2}", sim)));
                        break;
                    }
                }
            }
        }

        if let Some((_, ref reason)) = result {
            let sig = sem_key
                .clone()
                .unwrap_or_else(|| format!("{}:<no-key>", tool_name));
            self.record_audit_duplicate(tool_name, sig);
            let _ = reason; // used above via ref
        }

        // Record this output (truncated at character boundary)
        let truncated = if output.len() > 2000 {
            // Find a valid UTF-8 boundary before 2000
            let mut end = 2000;
            while end > 0 && !output.is_char_boundary(end) {
                end -= 1;
            }
            &output[..end]
        } else {
            output
        };
        self.output_log.push(OutputLogEntry {
            tool_name: tool_name.to_string(),
            semantic_key: sem_key,
            turn: current_turn,
            context_generation,
            output: truncated.to_string(),
        });
        if self.output_log.len() > self.max_output_entries {
            self.output_log.remove(0);
        }

        result
    }

    /// Run [`Self::check_and_record`] and append a user-visible hint when output matches a prior call.
    pub fn append_near_duplicate_hint_if_any(
        &mut self,
        result_str: &mut String,
        tool_name: &str,
        args: &Value,
        turn_index: usize,
    ) {
        self.append_near_duplicate_hint_if_any_with_generation(
            result_str, tool_name, args, turn_index, 0,
        );
    }

    /// Like [`Self::append_near_duplicate_hint_if_any`], scoped to an external
    /// observation generation.
    pub fn append_near_duplicate_hint_if_any_with_generation(
        &mut self,
        result_str: &mut String,
        tool_name: &str,
        args: &Value,
        turn_index: usize,
        context_generation: u64,
    ) {
        if let Some((prev_turn, reason)) = self.check_and_record_with_generation(
            tool_name,
            args,
            result_str.as_str(),
            turn_index,
            context_generation,
        ) {
            result_str.push_str(&format!(
                "\n\n⚠️ DUPLICATE HINT: This {tool_name} output matches turn {} ({reason}). \
                 If this is the same data, use the earlier result. \
                 If you intentionally changed arguments or need fresher data, this hint is informational.",
                prev_turn + 1,
            ));
        }
    }
    pub fn output_log_size(&self) -> usize {
        self.output_log.len()
    }

    /// Drain and return all accumulated audit records.
    pub fn take_audit_records(&mut self) -> Vec<DedupAuditRecord> {
        std::mem::take(&mut self.dedup_audit)
    }

    /// Generate a concise inventory of context already fetched this session.
    ///
    /// Returns a human-readable summary suitable for injection into conversation
    /// to help the LLM avoid re-fetching and plan efficiently.
    ///
    /// Example output:
    /// ```text
    /// Files read: src/main.rs, src/lib.rs (2 files)
    /// Searches: grep "error" in src/, glob "*.rs" (2 searches)
    /// Git: status, diff HEAD~3, log (3 ops)
    /// GitHub: matrixorigin/mo PRs, CI status (2 ops)
    /// ```
    pub fn context_inventory(&self) -> String {
        if self.param_cache.is_empty() {
            return String::new();
        }

        let mut files = std::collections::BTreeSet::new();
        let mut searches: Vec<String> = Vec::new();
        let mut git_ops: Vec<&str> = Vec::new();
        let mut github_ops: Vec<String> = Vec::new();
        let mut memory_ops: Vec<&str> = Vec::new();
        let mut other: Vec<String> = Vec::new();

        for (key, (_turn, tool, _generation)) in &self.param_cache {
            match tool.as_str() {
                "read_file" => {
                    if let Some(path) = read_file_path_from_semantic_key(key) {
                        files.insert(path.to_string());
                    }
                }
                "grep" => {
                    if let Some(path) = key.strip_prefix("grep:") {
                        searches.push(format!("grep in {}", path));
                    }
                }
                "glob" => {
                    if let Some(rest) = key.strip_prefix("glob:") {
                        searches.push(format!("glob {}", rest));
                    }
                }
                "git" => {
                    if let Some(action) = key.split(':').nth(1) {
                        git_ops.push(action);
                    }
                }
                "github" => {
                    // Extract repo from key if present
                    let mut parts = key.split(':');
                    let _tool = parts.next();
                    if let Some(action) = parts.next() {
                        if let Some(repo) = parts.next() {
                            github_ops.push(format!("{action} {repo}"));
                        } else {
                            github_ops.push(action.to_string());
                        }
                    } else {
                        github_ops.push("github".to_string());
                    }
                }
                t if t.starts_with("memory_") => {
                    memory_ops.push(t.strip_prefix("memory_").unwrap_or(t));
                }
                _ => {
                    other.push(key.clone());
                }
            }
        }

        let mut parts = Vec::new();

        if !files.is_empty() {
            let count = files.len();
            let display: Vec<_> = files.iter().take(5).map(String::as_str).collect();
            let suffix = if count > 5 {
                format!(" (+{} more)", count - 5)
            } else {
                String::new()
            };
            parts.push(format!("Files: {}{}", display.join(", "), suffix));
        }

        if !searches.is_empty() {
            parts.push(format!("Searches: {}", searches.join(", ")));
        }

        if !git_ops.is_empty() {
            let unique: std::collections::HashSet<_> = git_ops.iter().collect();
            let ops: Vec<_> = unique.iter().map(|s| **s).collect();
            parts.push(format!("Git: {}", ops.join(", ")));
        }

        if !github_ops.is_empty() {
            parts.push(format!("GitHub: {}", github_ops.join(", ")));
        }

        if !memory_ops.is_empty() {
            let unique: std::collections::HashSet<_> = memory_ops.iter().collect();
            let ops: Vec<_> = unique.iter().map(|s| **s).collect();
            parts.push(format!("Memory: {}", ops.join(", ")));
        }

        if parts.is_empty() {
            return String::new();
        }

        format!("Context already fetched:\n{}", parts.join("\n"))
    }

    /// Check if we've already read a specific file (for pre-call planning).
    /// Uses prefix match so line-range-specific keys still register the file as "read".
    pub fn has_file(&self, path: &str) -> bool {
        let prefix = format!("read_file:{}:", normalize_path(path));
        self.param_cache.keys().any(|k| k.starts_with(&prefix))
    }

    /// Check if we've already done a grep scoped to this path (any pattern).
    ///
    /// Keys are `grep:{normalized_path}:{pattern}`; any cache entry whose path prefix matches
    /// counts (e.g. `src/` matches `grep:src:foo` after normalizing `src/` → `src`).
    pub fn has_grep_in(&self, path: &str) -> bool {
        let needle = normalize_path(path);
        let prefix = format!("grep:{needle}:");
        self.param_cache.keys().any(|k| k.starts_with(&prefix))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Tier 2: semantic_call_key ──

    #[test]
    fn github_repo_case_insensitive() {
        let k1 = semantic_call_key(
            "github",
            &json!({"action": "list_prs", "repo": "matrixorigin/mo"}),
        );
        let k2 = semantic_call_key(
            "github",
            &json!({"action": "list_prs", "repo": "MatrixOrigin/MO"}),
        );
        assert_eq!(k1, k2);
    }

    #[test]
    fn read_file_trailing_slash() {
        let k1 = semantic_call_key("read_file", &json!({"path": "src/main.rs"}));
        let k2 = semantic_call_key("read_file", &json!({"path": "src/main.rs/"}));
        assert_eq!(k1, k2);
    }

    #[test]
    fn read_file_different_line_ranges_differ() {
        let k1 = semantic_call_key(
            "read_file",
            &json!({"path": "foo.rs", "start_line": 1, "end_line": 120}),
        );
        let k2 = semantic_call_key(
            "read_file",
            &json!({"path": "foo.rs", "start_line": 200, "end_line": 350}),
        );
        assert_ne!(
            k1, k2,
            "different line ranges must produce distinct keys — otherwise agent can't read different regions of the same file"
        );
    }

    #[test]
    fn read_file_omitted_end_is_distinct_from_bounded_range() {
        let unbounded =
            semantic_call_key("read_file", &json!({"path": "foo.rs", "start_line": 20}));
        let bounded = semantic_call_key(
            "read_file",
            &json!({"path": "foo.rs", "start_line": 20, "end_line": 49}),
        );
        assert_ne!(unbounded, bounded);
    }

    #[test]
    fn read_file_outline_vs_full_differ() {
        let k1 = semantic_call_key("read_file", &json!({"path": "foo.rs", "outline": true}));
        let k2 = semantic_call_key("read_file", &json!({"path": "foo.rs"}));
        assert_ne!(
            k1, k2,
            "outline vs full read must differ — outline only returns signatures"
        );
    }

    #[test]
    fn grep_same_file_different_pattern_differs() {
        let k1 = semantic_call_key("grep", &json!({"pattern": "foo", "path": "src/main.rs"}));
        let k2 = semantic_call_key("grep", &json!({"pattern": "bar", "path": "src/main.rs"}));
        assert_ne!(
            k1, k2,
            "grep dedup keys include pattern — different searches are distinct"
        );
    }

    #[test]
    fn grep_different_output_mode_differs() {
        let k1 = semantic_call_key(
            "grep",
            &json!({"pattern": "foo", "path": "src/a.rs", "output_mode": "content"}),
        );
        let k2 = semantic_call_key(
            "grep",
            &json!({"pattern": "foo", "path": "src/a.rs", "output_mode": "files_with_matches"}),
        );
        assert_ne!(
            k1, k2,
            "different output_mode must not share same key — results differ"
        );
    }

    #[test]
    fn grep_include_filter_differs() {
        let k1 = semantic_call_key(
            "grep",
            &json!({"pattern": "foo", "path": "src", "include": "*.rs"}),
        );
        let k2 = semantic_call_key(
            "grep",
            &json!({"pattern": "foo", "path": "src", "include": "*.toml"}),
        );
        assert_ne!(k1, k2, "different include file filters must differ");
    }

    #[test]
    fn glob_offset_differs() {
        let k1 = semantic_call_key("glob", &json!({"pattern": "**/*.rs", "offset": 0}));
        let k2 = semantic_call_key("glob", &json!({"pattern": "**/*.rs", "offset": 50}));
        assert_ne!(k1, k2, "different pagination offsets must differ");
    }

    #[test]
    fn glob_head_limit_differs() {
        let k1 = semantic_call_key("glob", &json!({"pattern": "**/*.rs", "head_limit": 10}));
        let k2 = semantic_call_key("glob", &json!({"pattern": "**/*.rs", "head_limit": 100}));
        assert_ne!(
            k1, k2,
            "different head_limit values must differ — output size changes"
        );
    }

    #[test]
    fn git_action_diff_default_vs_explicit_head() {
        let k1 = semantic_call_key("git", &json!({"action": "diff"}));
        let k2 = semantic_call_key("git", &json!({"action": "diff", "ref": "HEAD"}));
        assert_eq!(k1, k2, "default should match explicit HEAD");
    }

    #[test]
    fn git_action_diff_different_refs() {
        let k1 = semantic_call_key("git", &json!({"action": "diff", "ref": "HEAD"}));
        let k2 = semantic_call_key("git", &json!({"action": "diff", "ref": "main"}));
        assert_ne!(k1, k2, "different refs should differ");
    }

    #[test]
    fn git_action_diff_staged_differs_from_unstaged() {
        let k1 = semantic_call_key("git", &json!({"action": "diff"}));
        let k2 = semantic_call_key("git", &json!({"action": "diff", "staged": true}));
        assert_ne!(k1, k2, "staged vs unstaged should differ");
    }

    #[test]
    fn git_action_diff_stat_only_differs_from_full_patch() {
        let k1 = semantic_call_key("git", &json!({"action": "diff", "stat_only": true}));
        let k2 = semantic_call_key("git", &json!({"action": "diff"}));
        assert_ne!(
            k1, k2,
            "stat-only diff must not share a semantic cache key with full patch output"
        );
    }

    #[test]
    fn git_action_diff_path_filter_differs_from_repo_wide_diff() {
        let k1 = semantic_call_key("git", &json!({"action": "diff", "path": "src/a.rs"}));
        let k2 = semantic_call_key("git", &json!({"action": "diff"}));
        assert_ne!(
            k1, k2,
            "path-scoped diff must not share a semantic cache key with repo-wide diff"
        );
    }

    #[test]
    fn git_action_diff_different_paths_do_not_collide() {
        let k1 = semantic_call_key("git", &json!({"action": "diff", "path": "src/a.rs"}));
        let k2 = semantic_call_key("git", &json!({"action": "diff", "path": "src/b.rs"}));
        assert_ne!(
            k1, k2,
            "different git(action=diff) path filters must stay distinct for cache safety"
        );
    }

    #[test]
    fn git_action_status_always_same_key() {
        let k1 = semantic_call_key("git", &json!({"action": "status"}));
        let k2 = semantic_call_key("git", &json!({"action": "status", "extra": "ignored"}));
        assert_eq!(k1, k2, "git(action=status) should always be same key");
    }

    #[test]
    fn bash_non_git_returns_none() {
        assert!(semantic_call_key("bash", &json!({"command": "ls"})).is_none());
    }

    #[test]
    fn bash_git_diff_command_shares_git_action_diff_semantic_key() {
        let bash = semantic_call_key("bash", &json!({"command": "git --no-pager diff"}));
        let structured = semantic_call_key("git", &json!({"action": "diff"}));
        assert_eq!(bash, structured);

        let bash_head = semantic_call_key("bash", &json!({"command": "git diff HEAD"}));
        assert_eq!(bash_head, structured);

        let bash_path = semantic_call_key("bash", &json!({"command": "git diff -- src/"}));
        let structured_path = semantic_call_key(
            "git",
            &json!({"action": "diff", "path": "src", "ref": "HEAD"}),
        );
        assert_eq!(bash_path, structured_path);
    }

    #[test]
    fn bash_compound_git_diff_command_is_not_canonicalized() {
        assert!(semantic_call_key("bash", &json!({"command": "git diff | head"})).is_none());
    }

    #[test]
    fn write_file_returns_none() {
        assert!(semantic_call_key("write_file", &json!({"path": "a.rs"})).is_none());
    }

    #[test]
    fn github_action_get_pr_includes_number() {
        let k1 = semantic_call_key(
            "github",
            &json!({"action": "get_pr", "repo": "org/repo", "pr_number": 42}),
        );
        let k2 = semantic_call_key(
            "github",
            &json!({"action": "get_pr", "repo": "org/repo", "pr_number": 43}),
        );
        assert_ne!(k1, k2, "different PR numbers should differ");
    }

    #[test]
    fn github_action_get_pr_same_pr_case_insensitive() {
        let k1 = semantic_call_key(
            "github",
            &json!({"action": "get_pr", "repo": "Org/Repo", "pr_number": 42}),
        );
        let k2 = semantic_call_key(
            "github",
            &json!({"action": "get_pr", "repo": "org/repo", "pr_number": 42}),
        );
        assert_eq!(k1, k2, "same PR on same repo should match");
    }

    #[test]
    fn memory_read_actions_are_action_aware() {
        assert_eq!(
            semantic_call_key(
                "memory",
                &json!({"action": "recall", "query": "Rust Memory"})
            ),
            Some("memory_recall:rust memory".to_string())
        );
        assert_eq!(
            semantic_call_key("memory", &json!({"action": "profile"})),
            Some("memory_profile".to_string())
        );
        assert_eq!(
            semantic_call_key("memory", &json!({"action": "expand", "memory_id": "m1"})),
            Some("memory_expand:m1".to_string())
        );
    }

    #[test]
    fn memory_write_actions_do_not_dedupe() {
        for action in [
            "remember", "forget", "update", "focus", "reflect", "feedback",
        ] {
            assert!(
                semantic_call_key(
                    "memory",
                    &json!({"action": action, "query": "x", "content": "x", "memory_id": "m"})
                )
                .is_none(),
                "{action} must not be semantically deduped"
            );
        }
    }

    #[test]
    fn git_action_log_search_case_insensitive() {
        let k1 = semantic_call_key("git", &json!({"action": "log_search", "query": "Fix Bug"}));
        let k2 = semantic_call_key("git", &json!({"action": "log_search", "query": "fix bug"}));
        assert_eq!(k1, k2, "search query should be case insensitive");
    }

    #[test]
    fn symbols_param_differs_kinds_and_calls() {
        let k1 = semantic_call_key("symbols", &json!({"path": "foo.rs"}));
        let k2 = semantic_call_key(
            "symbols",
            &json!({"path": "foo.rs", "kinds": ["fn"], "calls": true}),
        );
        assert_ne!(
            k1, k2,
            "symbols with kinds+calls must differ from bare path"
        );
    }

    #[test]
    fn symbols_pattern_differs() {
        let k1 = semantic_call_key("symbols", &json!({"path": "foo.rs", "pattern": "test_"}));
        let k2 = semantic_call_key("symbols", &json!({"path": "foo.rs", "pattern": "parse_"}));
        assert_ne!(k1, k2, "different pattern filters must differ");
    }

    #[test]
    fn git_action_blame_line_range_differs() {
        let k1 = semantic_call_key(
            "git",
            &json!({"action": "blame", "path": "foo.rs", "line_start": 1, "line_end": 50}),
        );
        let k2 = semantic_call_key(
            "git",
            &json!({"action": "blame", "path": "foo.rs", "line_start": 100, "line_end": 150}),
        );
        assert_ne!(k1, k2, "different blame line ranges must differ");
    }

    #[test]
    fn git_action_blame_no_line_range_zero_defaults() {
        let k1 = semantic_call_key("git", &json!({"action": "blame", "path": "foo.rs"}));
        let k2 = semantic_call_key(
            "git",
            &json!({"action": "blame", "path": "foo.rs", "line_start": 0, "line_end": 0}),
        );
        // 0 serializes as "0" but arg_u64 returns None for missing,
        // Some(0) for explicit zero → default "" for missing, "0" for explicit.
        // These are intentionally different: explicit 0 vs no range are distinct requests.
        assert_ne!(
            k1, k2,
            "no line range vs explicit line_start=0 should differ (different gix-blame behaviour)"
        );
    }

    #[test]
    fn git_action_file_history_n_differs() {
        let k1 = semantic_call_key(
            "git",
            &json!({"action": "file_history", "file": "foo.rs", "n": 10}),
        );
        let k2 = semantic_call_key(
            "git",
            &json!({"action": "file_history", "file": "foo.rs", "n": 50}),
        );
        assert_ne!(
            k1, k2,
            "different n values must differ for git(action=file_history)"
        );
    }

    #[test]
    fn git_action_contributors_path_differs() {
        let k1 = semantic_call_key("git", &json!({"action": "contributors", "path": "src"}));
        let k2 = semantic_call_key("git", &json!({"action": "contributors", "path": "tests"}));
        assert_ne!(k1, k2, "different path filters must differ");
    }

    #[test]
    fn git_action_contributors_since_differs() {
        let k1 = semantic_call_key(
            "git",
            &json!({"action": "contributors", "since": "2.weeks.ago"}),
        );
        let k2 = semantic_call_key(
            "git",
            &json!({"action": "contributors", "since": "1.year.ago"}),
        );
        assert_ne!(k1, k2, "different since values must differ");
    }

    #[test]
    fn git_action_contributors_no_args_same_key() {
        let k1 = semantic_call_key("git", &json!({"action": "contributors"}));
        let k2 = semantic_call_key(
            "git",
            &json!({"action": "contributors", "extra": "ignored"}),
        );
        assert_eq!(
            k1, k2,
            "bare git(action=contributors) calls should share same key"
        );
    }

    #[test]
    fn list_dir_depth_differs() {
        let k1 = semantic_call_key("list_dir", &json!({"path": "."}));
        let k2 = semantic_call_key("list_dir", &json!({"path": ".", "depth": 3}));
        assert_ne!(k1, k2, "different depth values must differ for list_dir");
    }

    #[test]
    fn list_dir_path_differs() {
        let k1 = semantic_call_key("list_dir", &json!({"path": "src"}));
        let k2 = semantic_call_key("list_dir", &json!({"path": "tests"}));
        assert_ne!(k1, k2, "different paths must differ for list_dir");
    }

    // ── Tier 3: token_cosine_similarity ──

    #[test]
    fn identical_outputs() {
        let out = "PR #123: fix bug in parser\nPR #124: add tests";
        assert!((token_cosine_similarity(out, out) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn high_overlap_outputs() {
        let out1 = "PR #1: fix authentication bug\nPR #2: add new feature\nPR #3: docs update";
        let out2 = "PR #1: fix authentication bug\nPR #2: add new feature\nPR #4: test coverage";
        let sim = token_cosine_similarity(out1, out2);
        assert!(sim >= 0.5, "2/3 overlap should be high: {sim}");
    }

    #[test]
    fn completely_different_outputs() {
        let out1 = "src/main.rs: fn main() { println!(\"hello world\"); }";
        let out2 =
            "CREATE TABLE users (id INT, name VARCHAR(255), email VARCHAR(255) PRIMARY KEY);";
        let sim = token_cosine_similarity(out1, out2);
        assert!(sim < 0.3, "different domains should be low: {sim}");
    }

    #[test]
    fn short_outputs_return_zero() {
        assert_eq!(token_cosine_similarity("ok", "ok"), 0.0);
        assert_eq!(token_cosine_similarity("", "something"), 0.0);
    }

    // ── SemanticDedup tracker ──

    #[test]
    fn tracker_detects_param_match() {
        let mut tracker = SemanticDedup::new(0.75);
        let result1 = tracker.check_and_record(
            "read_file",
            &json!({"path": "src/main.rs"}),
            "fn main() {}",
            1,
        );
        assert!(result1.is_none(), "first call should not match");

        let result2 = tracker.check_and_record(
            "read_file",
            &json!({"path": "src/main.rs/"}),
            "fn main() {}",
            2,
        );
        assert!(result2.is_some(), "trailing-slash variant should match");
        let (prev_turn, reason) = result2.unwrap();
        assert_eq!(prev_turn, 1);
        assert_eq!(reason, "param_match");
    }

    #[test]
    fn append_near_duplicate_hint_inserts_on_second_read_file() {
        let mut tracker = SemanticDedup::new(0.75);
        let args = json!({"path": "src/main.rs"});
        let mut out1 = "fn main() {}".to_string();
        tracker.append_near_duplicate_hint_if_any(&mut out1, "read_file", &args, 1);
        assert!(
            !out1.contains("DUPLICATE HINT"),
            "first recording should not append hint"
        );

        let mut out2 = "fn main() {}".to_string();
        tracker.append_near_duplicate_hint_if_any(&mut out2, "read_file", &args, 2);
        assert!(out2.contains("DUPLICATE HINT"));
        assert!(!out2.contains("Do NOT call this tool again"));
        assert!(out2.contains("read_file"));
    }

    #[test]
    fn tracker_does_not_use_output_similarity_across_different_semantic_keys() {
        let mut tracker = SemanticDedup::new(0.7);
        let output1 = "PR #1: fix\nPR #2: feature\nPR #3: docs\nPR #4: perf improvement";
        tracker.check_and_record(
            "github",
            &json!({"action": "list_prs", "repo": "a/b"}),
            output1,
            1,
        );

        // Same tool, different args, but very similar output
        let output2 = "PR #1: fix\nPR #2: feature\nPR #3: docs\nPR #5: refactor code";
        assert!(
            token_cosine_similarity(output1, output2) >= 0.7,
            "test must exercise an output pair that old tool-name-only Tier 3 would flag"
        );
        let result = tracker.check_and_record(
            "github",
            &json!({"action": "list_prs", "repo": "a/c"}),
            output2,
            2,
        );
        assert!(
            result.is_none(),
            "different semantic keys must not be deduped by output similarity"
        );
    }

    #[test]
    fn tracker_detects_token_cosine_only_with_same_semantic_key() {
        let mut tracker = SemanticDedup::new(0.7);
        let args = json!({"path": "src/main.rs", "start_line": 1, "end_line": 80});
        let sem_key = semantic_call_key("read_file", &args).expect("read_file key");
        let output1 =
            "fn parse_user() {}\nfn parse_team() {}\nfn parse_token() {}\nfn parse_config() {}";
        let output2 =
            "fn parse_user() {}\nfn parse_team() {}\nfn parse_token() {}\nfn parse_runtime() {}";
        assert!(
            token_cosine_similarity(output1, output2) >= 0.7,
            "test must exercise a high-similarity output pair"
        );
        tracker.output_log.push(OutputLogEntry {
            tool_name: "read_file".to_string(),
            semantic_key: Some(sem_key),
            turn: 1,
            context_generation: 0,
            output: output1.to_string(),
        });

        let result = tracker.check_and_record("read_file", &args, output2, 2);
        let (prev_turn, reason) = result.expect("same-key output similarity should be detected");
        assert_eq!(prev_turn, 1);
        assert!(reason.starts_with("token_cosine"));
    }

    #[test]
    fn read_file_different_ranges_do_not_trigger_output_similarity_hint() {
        let mut tracker = SemanticDedup::new(0.7);
        let range1 = json!({"path": "src/lib.rs", "start_line": 1, "end_line": 500});
        let range2 = json!({"path": "src/lib.rs", "start_line": 501, "end_line": 1000});
        let output1 = "pub fn handler_a() {}\npub fn handler_b() {}\npub fn handler_c() {}\npub fn handler_d() {}\n";
        let output2 = "pub fn handler_e() {}\npub fn handler_f() {}\npub fn handler_g() {}\npub fn handler_h() {}\n";
        assert!(
            token_cosine_similarity(output1, output2) >= 0.7,
            "test must reproduce the high-similarity same-file range case"
        );

        let mut first = output1.to_string();
        tracker.append_near_duplicate_hint_if_any(&mut first, "read_file", &range1, 1);
        let mut second = output2.to_string();
        tracker.append_near_duplicate_hint_if_any(&mut second, "read_file", &range2, 2);
        assert!(
            !second.contains("DUPLICATE HINT") && !second.contains("DUPLICATE DETECTED"),
            "different read_file ranges must not receive duplicate guidance: {second}"
        );
    }

    #[test]
    fn pre_check_block_does_not_reuse_cached_output_for_different_read_file_range() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record(
            "read_file",
            &json!({"path": "src/lib.rs", "start_line": 1, "end_line": 500}),
            "first range content that is long enough to be a useful cached output",
            1,
        );

        let block = tracker.pre_check_block(
            "read_file",
            &json!({"path": "src/lib.rs", "start_line": 501, "end_line": 1000}),
            2,
        );
        assert!(
            block.is_none(),
            "different read_file ranges must not reuse cached output from another range"
        );
    }

    #[test]
    fn tracker_no_false_positive_different_tools() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record("read_file", &json!({"path": "a.rs"}), "content a", 1);
        let result = tracker.check_and_record(
            "git",
            &json!({"action": "blame", "path": "a.rs"}),
            "different content",
            2,
        );
        // read_file and git(action=blame) have different semantic keys
        // Output similarity won't match (different tool names in Tier 3)
        assert!(result.is_none(), "different tools should not match");
    }

    #[test]
    fn tracker_same_turn_no_match() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record("read_file", &json!({"path": "a.rs"}), "content", 1);
        let result = tracker.check_and_record("read_file", &json!({"path": "a.rs/"}), "content", 1);
        // Same turn → should not trigger (dedup is cross-turn)
        assert!(result.is_none(), "same turn should not trigger dedup");
    }

    #[test]
    fn tracker_output_log_bounded() {
        let mut tracker = SemanticDedup::new(0.75);
        for i in 0..60 {
            tracker.check_and_record(
                "read_file",
                &json!({"path": format!("file_{i}.rs")}),
                &format!("content of file {i} with enough length to pass threshold check"),
                i,
            );
        }
        assert!(
            tracker.output_log_size() <= 50,
            "output log should be bounded at 50"
        );
    }

    // ── Context Inventory ──

    #[test]
    fn context_inventory_empty_when_no_calls() {
        let tracker = SemanticDedup::new(0.75);
        assert!(tracker.context_inventory().is_empty());
    }

    #[test]
    fn context_inventory_shows_files() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record("read_file", &json!({"path": "src/main.rs"}), "content", 0);
        tracker.check_and_record("read_file", &json!({"path": "src/lib.rs"}), "content", 1);

        let inv = tracker.context_inventory();
        assert!(inv.contains("Files:"), "should have Files section");
        assert!(inv.contains("src/main.rs"));
        assert!(inv.contains("src/lib.rs"));
    }

    #[test]
    fn context_inventory_counts_read_ranges_as_one_file() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record(
            "read_file",
            &json!({"path": "src/lib.rs", "start_line": 1, "end_line": 40}),
            "content",
            0,
        );
        tracker.check_and_record(
            "read_file",
            &json!({"path": "src/lib.rs", "start_line": 80, "end_line": 120}),
            "content",
            1,
        );

        let inv = tracker.context_inventory();
        assert_eq!(inv.matches("src/lib.rs").count(), 1, "{inv}");
        assert!(!inv.contains("more"), "{inv}");
    }

    #[test]
    fn context_inventory_shows_searches() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record(
            "grep",
            &json!({"pattern": "TODO", "path": "src/"}),
            "match",
            0,
        );
        tracker.check_and_record("glob", &json!({"pattern": "*.rs", "path": "."}), "files", 1);

        let inv = tracker.context_inventory();
        assert!(inv.contains("Searches:"), "should have Searches section");
        assert!(inv.contains("grep"));
        assert!(inv.contains("glob"));
    }

    #[test]
    fn context_inventory_shows_git_ops() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record("git", &json!({"action": "status"}), "clean", 0);
        tracker.check_and_record(
            "git",
            &json!({"action": "diff", "ref": "HEAD~3"}),
            "diff",
            1,
        );

        let inv = tracker.context_inventory();
        assert!(inv.contains("Git:"), "should have Git section");
    }

    #[test]
    fn context_inventory_truncates_long_file_lists() {
        let mut tracker = SemanticDedup::new(0.75);
        for i in 0..10 {
            tracker.check_and_record(
                "read_file",
                &json!({"path": format!("src/file_{i}.rs")}),
                "content",
                i,
            );
        }

        let inv = tracker.context_inventory();
        assert!(
            inv.contains("+5 more"),
            "should truncate to 5 files with count"
        );
    }

    #[test]
    fn has_file_checks_cache() {
        let mut tracker = SemanticDedup::new(0.75);
        assert!(!tracker.has_file("src/main.rs"));

        tracker.check_and_record("read_file", &json!({"path": "src/main.rs"}), "content", 0);
        assert!(tracker.has_file("src/main.rs"));
        assert!(tracker.has_file("src/main.rs/")); // normalized
        assert!(!tracker.has_file("src/other.rs"));
    }

    #[test]
    fn has_grep_in_checks_cache() {
        let mut tracker = SemanticDedup::new(0.75);
        assert!(!tracker.has_grep_in("src/"));

        tracker.check_and_record(
            "grep",
            &json!({"pattern": "foo", "path": "src/"}),
            "match",
            0,
        );
        assert!(tracker.has_grep_in("src/"));
        assert!(!tracker.has_grep_in("tests/"));
    }

    #[test]
    fn utf8_boundary_truncation_no_panic() {
        // Create output with multi-byte UTF-8 characters (Chinese)
        // Each Chinese character is 3 bytes. Build a string that's > 2000 bytes
        // so truncation kicks in, with the 2000th byte in the middle of a char.
        let chinese_chars = "你好世界"; // 4 chars × 3 bytes = 12 bytes
        let output: String = chinese_chars.repeat(200); // 2400 bytes
        assert!(output.len() > 2000);

        // This should NOT panic even though byte 2000 lands mid-character
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record("shell_exec", &json!({"command": "echo"}), &output, 1);

        // Second call with similar output - triggers similarity check
        let output2: String = chinese_chars.repeat(198);
        let result =
            tracker.check_and_record("shell_exec", &json!({"command": "echo2"}), &output2, 2);

        // Just verify no panic occurred - similarity result depends on exact truncation
        drop(result);
    }

    // ── DedupAuditRecord tests ──

    #[test]
    fn audit_record_created_on_param_duplicate() {
        let mut dedup = SemanticDedup::new(DEFAULT_SIMILARITY_THRESHOLD);
        // First call — no duplicate
        let r1 =
            dedup.check_and_record("read_file", &json!({"path": "src/main.rs"}), "content1", 0);
        assert!(r1.is_none());
        assert!(dedup.take_audit_records().is_empty());

        // Same tool, same path, different turn → duplicate
        let r2 =
            dedup.check_and_record("read_file", &json!({"path": "src/main.rs"}), "content1", 1);
        assert!(r2.is_some(), "should detect param duplicate");

        let records = dedup.take_audit_records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tool_name, "read_file");
        assert_eq!(records[0].duplicate_count, 1);
    }

    #[test]
    fn audit_record_increments_on_repeated_duplicates() {
        let mut dedup = SemanticDedup::new(DEFAULT_SIMILARITY_THRESHOLD);
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 0);
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 1); // dup 1
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 2); // dup 2

        let records = dedup.take_audit_records();
        assert_eq!(records.len(), 1, "same signature should be one record");
        assert_eq!(records[0].duplicate_count, 2);
    }

    #[test]
    fn take_audit_records_drains() {
        let mut dedup = SemanticDedup::new(DEFAULT_SIMILARITY_THRESHOLD);
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 0);
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 1);

        let r1 = dedup.take_audit_records();
        assert_eq!(r1.len(), 1);
        let r2 = dedup.take_audit_records();
        assert!(r2.is_empty(), "take should drain");
    }

    #[test]
    fn different_tools_get_separate_audit_records() {
        let mut dedup = SemanticDedup::new(DEFAULT_SIMILARITY_THRESHOLD);
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 0);
        dedup.check_and_record("read_file", &json!({"path": "a.rs"}), "x", 1);
        dedup.check_and_record("glob", &json!({"pattern": "*.rs", "path": "src"}), "y", 0);
        dedup.check_and_record("glob", &json!({"pattern": "*.rs", "path": "src"}), "y", 1);

        let records = dedup.take_audit_records();
        assert_eq!(records.len(), 2);
        let names: Vec<_> = records.iter().map(|r| r.tool_name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"glob"));
    }

    // ── token_cosine_similarity (already covered above by Tier 3 tests) ─

    #[test]
    fn git_action_diff_key_includes_base_ref() {
        let k1 = semantic_call_key(
            "git",
            &json!({"action": "diff", "base_ref": "HEAD~5", "ref": "HEAD"}),
        );
        let k2 = semantic_call_key("git", &json!({"action": "diff", "ref": "HEAD"}));
        assert_ne!(
            k1, k2,
            "range diff should have different key from single-ref diff"
        );
    }

    #[test]
    fn git_action_diff_same_range_same_key() {
        let k1 = semantic_call_key(
            "git",
            &json!({"action": "diff", "base_ref": "HEAD~5", "ref": "HEAD"}),
        );
        let k2 = semantic_call_key(
            "git",
            &json!({"action": "diff", "base_ref": "HEAD~5", "ref": "HEAD"}),
        );
        assert_eq!(k1, k2);
    }

    #[test]
    fn pre_check_block_returns_none_on_first_call() {
        let dedup = SemanticDedup::new(0.75);
        let result = dedup.pre_check_block("read_file", &json!({"path": "src/main.rs"}), 0);
        assert!(result.is_none(), "first call should not block");
    }

    #[test]
    fn pre_check_block_returns_cached_output_on_repeat() {
        let mut dedup = SemanticDedup::new(0.75);
        // First call — records param cache + output
        let output = "fn main() { println!(\"hello\"); }";
        let res = dedup.check_and_record("read_file", &json!({"path": "src/main.rs"}), output, 0);
        assert!(res.is_none());

        // Second call with same semantic key — should block
        let block = dedup.pre_check_block("read_file", &json!({"path": "src/main.rs"}), 1);
        assert!(block.is_some(), "should block semantic duplicate");
        let (prev_turn, cached) = block.unwrap();
        assert_eq!(prev_turn, 0);
        assert!(cached.contains("main"), "should contain prior output");
    }

    #[test]
    fn pre_check_block_ignores_different_tool() {
        let mut dedup = SemanticDedup::new(0.75);
        dedup.check_and_record("read_file", &json!({"path": "src/main.rs"}), "content", 0);
        // Different tool, different semantic namespace
        let block = dedup.pre_check_block("grep", &json!({"path": "src/main.rs"}), 1);
        assert!(block.is_none(), "different tool should not block");
    }

    #[test]
    fn pre_check_block_normalizes_trailing_slash() {
        let mut dedup = SemanticDedup::new(0.75);
        dedup.check_and_record(
            "read_file",
            &json!({"path": "src/main.rs"}),
            "file content here for normalization test",
            0,
        );
        let block = dedup.pre_check_block("read_file", &json!({"path": "src/main.rs/"}), 1);
        assert!(block.is_some(), "normalized path should match");
    }

    #[test]
    fn pre_check_block_is_scoped_by_context_generation() {
        let mut dedup = SemanticDedup::new(0.75);
        let args = json!({"path": "src/main.rs"});
        dedup.check_and_record_with_generation(
            "read_file",
            &args,
            "fn before() { println!(\"fresh enough content\"); }",
            0,
            7,
        );

        assert!(
            dedup
                .pre_check_block_with_generation("read_file", &args, 1, 7)
                .is_some(),
            "same generation should still allow a semantic cache hit"
        );
        assert!(
            dedup
                .pre_check_block_with_generation("read_file", &args, 1, 8)
                .is_none(),
            "new generation must force a fresh observation"
        );
    }

    #[test]
    fn clear_observation_cache_drops_stale_semantic_state() {
        let mut dedup = SemanticDedup::new(0.75);
        let args = json!({"path": "src/main.rs"});
        dedup.check_and_record("read_file", &args, "fn before() {}", 0);
        assert!(dedup.has_file("src/main.rs"));

        dedup.clear_observation_cache();

        assert!(!dedup.has_file("src/main.rs"));
        assert!(dedup.pre_check_block("read_file", &args, 1).is_none());
        assert_eq!(dedup.output_log_size(), 0);
    }

    /// When microcompact has cleared the cached output to `[Cleared]`,
    /// pre_check_block must allow re-execution rather than returning a
    /// useless stub. Without this, long sessions hit a deadlock:
    /// old content cleared for pressure → agent tries to re-read →
    /// dedup blocks because "same file seen before" → agent stuck.
    #[test]
    fn pre_check_block_allows_reexecution_when_output_cleared() {
        let mut dedup = SemanticDedup::new(0.75);
        // First call — record a real output
        dedup.check_and_record(
            "read_file",
            &json!({"path": "src/lib.rs"}),
            "pub fn important() { /* ... */ }",
            0,
        );
        // Simulate microcompact clearing the output_log entry
        for entry in dedup.output_log.iter_mut() {
            if entry.tool_name == "read_file" {
                entry.output = "[Cleared]".to_string();
            }
        }
        // Re-read same file — must NOT be blocked (content is gone)
        let block = dedup.pre_check_block("read_file", &json!({"path": "src/lib.rs"}), 2);
        assert!(
            block.is_none(),
            "must allow re-execution when cached output is [Cleared]"
        );
    }

    /// Same as above but for the "(cached — identical call)" stub that an
    /// earlier dedup pass may have stored into output_log.
    #[test]
    fn pre_check_block_allows_reexecution_when_output_is_dedup_stub() {
        let mut dedup = SemanticDedup::new(0.75);
        dedup.check_and_record(
            "read_file",
            &json!({"path": "Cargo.toml"}),
            "(cached — identical call already executed in this conversation. Re-read the file only if you need the content again.)",
            0,
        );
        let block = dedup.pre_check_block("read_file", &json!({"path": "Cargo.toml"}), 1);
        assert!(
            block.is_none(),
            "must allow re-execution when prior output was itself a dedup stub"
        );
    }

    /// Very short outputs (< 20 chars) are likely placeholders or errors,
    /// not meaningful cached content. Allow re-execution.
    #[test]
    fn pre_check_block_allows_reexecution_for_trivially_short_output() {
        let mut dedup = SemanticDedup::new(0.75);
        dedup.check_and_record("read_file", &json!({"path": "x.rs"}), "err", 0);
        let block = dedup.pre_check_block("read_file", &json!({"path": "x.rs"}), 1);
        assert!(
            block.is_none(),
            "trivially short output should not block re-execution"
        );
    }

    #[test]
    fn param_cache_is_bounded() {
        let mut dedup = SemanticDedup::new(0.75);
        for i in 0..(DEFAULT_PARAM_CACHE_ENTRIES + 10) {
            dedup.check_and_record(
                "read_file",
                &json!({"path": format!("src/{i}.rs")}),
                "long enough content for cache bookkeeping",
                i,
            );
        }

        assert_eq!(dedup.param_cache.len(), DEFAULT_PARAM_CACHE_ENTRIES);
        assert!(
            !dedup.has_file("src/0.rs"),
            "oldest semantic keys should be evicted"
        );
        assert!(dedup.has_file(&format!("src/{}.rs", DEFAULT_PARAM_CACHE_ENTRIES + 9)));
    }

    #[test]
    fn dedup_audit_is_bounded() {
        let mut dedup = SemanticDedup::new(0.75);
        for i in 0..(DEFAULT_AUDIT_ENTRIES + 10) {
            let args = json!({"path": format!("src/{i}.rs")});
            dedup.check_and_record("read_file", &args, "first long enough output", i * 2);
            dedup.check_and_record("read_file", &args, "second long enough output", i * 2 + 1);
        }

        assert_eq!(dedup.dedup_audit.len(), DEFAULT_AUDIT_ENTRIES);
    }

    // ── P1-H: Semantic dedup behavioral tests ───────────────────────

    /// Scenario: Agent reads the same file twice. The second call must be
    /// blocked by pre_check_block and return the cached output.
    #[test]
    fn duplicate_read_file_returns_cached_output() {
        let mut dedup = SemanticDedup::new(0.75);
        let original_content = "fn main() {\n    println!(\"hello\");\n}";

        // First read — records the output
        let dup = dedup.check_and_record(
            "read_file",
            &json!({"path": "src/main.rs"}),
            original_content,
            0,
        );
        assert!(dup.is_none(), "first read should not be a duplicate");

        // Second read — pre_check_block should return cached output
        let block = dedup.pre_check_block("read_file", &json!({"path": "src/main.rs"}), 1);
        let (prev_turn, cached) = block.expect("second read must be blocked as duplicate");
        assert_eq!(prev_turn, 0, "must reference the original turn");
        assert_eq!(cached, original_content, "must return exact cached output");
    }

    /// Scenario: Agent reads two DIFFERENT files. No dedup should trigger.
    #[test]
    fn different_files_not_deduplicated() {
        let mut dedup = SemanticDedup::new(0.75);

        dedup.check_and_record(
            "read_file",
            &json!({"path": "src/main.rs"}),
            "fn main() {}",
            0,
        );

        let block = dedup.pre_check_block("read_file", &json!({"path": "src/lib.rs"}), 1);
        assert!(block.is_none(), "different files must not be deduplicated");
    }

    /// Scenario: Agent greps the same pattern in the same directory twice.
    /// Second call must be detected as duplicate.
    #[test]
    fn duplicate_grep_detected() {
        let mut dedup = SemanticDedup::new(0.75);

        dedup.check_and_record(
            "grep",
            &json!({"path": "src/", "pattern": "TODO"}),
            "src/main.rs:10: // TODO: fix this",
            0,
        );

        let block = dedup.pre_check_block("grep", &json!({"path": "src/", "pattern": "TODO"}), 1);
        assert!(block.is_some(), "identical grep must be deduplicated");
    }

    /// Scenario: an unkeyed tool may produce identical output for different
    /// commands. Without a semantic key, output similarity is not enough to
    /// infer duplicate intent.
    #[test]
    fn unkeyed_tools_do_not_dedupe_on_output_similarity() {
        let mut dedup = SemanticDedup::new(0.75);
        let output = "error: cannot find module `auth`\n  --> src/main.rs:5:1\n  |\n5 | mod auth;\n  | ^^^^^^^^^ file not found";

        // First call
        dedup.check_and_record("bash", &json!({"command": "cargo check 2>&1"}), output, 0);

        // Second call with slightly different command but same output
        let dup = dedup.check_and_record(
            "bash",
            &json!({"command": "cargo check --all 2>&1"}),
            output,
            1,
        );
        assert!(
            dup.is_none(),
            "unkeyed tools must not dedupe solely because outputs match"
        );
    }
}
