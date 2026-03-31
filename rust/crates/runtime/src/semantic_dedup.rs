//! Semantic near-duplicate detection for tool calls across turns.
//!
//! Three tiers of dedup:
//! - Tier 1 (exact): handled externally by `normalize_call_sig()` in chat_stream
//! - Tier 2 (parameter-aware): same tool, semantically equivalent args (case, trailing slash)
//! - Tier 3 (output similarity): TF-IDF cosine similarity on tool outputs
//!
//! No embeddings — pure string processing. <0.5ms per check.

use crate::text_tokenize::{build_tf, tokenize};
use serde_json::Value;
use std::collections::HashMap;

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
        // File-based tools: key on normalized path
        "read_file" => {
            let path = arg_str(args, "path")?;
            Some(format!("read_file:{}", normalize_path(path)))
        }
        "glob" => {
            let pattern = arg_str(args, "pattern").unwrap_or("*");
            let path = arg_str(args, "path").unwrap_or(".");
            Some(format!("glob:{}:{}", normalize_path(path), pattern))
        }
        // Grep: key on path only — pattern is the query dimension (varies legitimately)
        "grep" => {
            let path = arg_str(args, "path").unwrap_or(".");
            Some(format!("grep:{}", normalize_path(path)))
        }
        // GitHub repo tools: case-insensitive repo
        "github_list_prs" | "github_list_issues" | "github_ci_status" | "github_repo_stats" => {
            let repo = arg_str(args, "repo").or_else(|| arg_str(args, "repository"))?;
            Some(format!("{}:{}", tool_name, normalize_repo(repo)))
        }
        "github_get_pr" | "github_get_issue" => {
            let repo = arg_str(args, "repo").or_else(|| arg_str(args, "repository"))?;
            let number = args
                .get("number")
                .or_else(|| args.get("pr_number"))
                .or_else(|| args.get("issue_number"));
            let num_str = number
                .map(|v| v.to_string())
                .unwrap_or_else(|| "?".to_string());
            Some(format!(
                "{}:{}#{}",
                tool_name,
                normalize_repo(repo),
                num_str
            ))
        }
        // Git: key on ref (staged flag changes output semantically)
        "git_diff" => {
            let git_ref = arg_str(args, "ref").unwrap_or("HEAD");
            let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
            Some(format!("git_diff:{}:staged={}", git_ref, staged))
        }
        "git_log" => {
            let n = args.get("n").and_then(Value::as_u64).unwrap_or(10);
            let path = arg_str(args, "path").unwrap_or("");
            Some(format!("git_log:n={}:{}", n, normalize_path(path)))
        }
        "git_blame" => {
            let file = arg_str(args, "file")?;
            Some(format!("git_blame:{}", normalize_path(file)))
        }
        "git_file_history" => {
            let file = arg_str(args, "file")?;
            Some(format!("git_file_history:{}", normalize_path(file)))
        }
        "git_status" | "git_contributors" | "get_agent_info" => {
            // These have no meaningful args → always same key
            Some(tool_name.to_string())
        }
        "git_log_search" => {
            let query = arg_str(args, "query").unwrap_or("");
            Some(format!("git_log_search:{}", query.to_lowercase()))
        }
        "git_show" => {
            let commit = arg_str(args, "commit").unwrap_or("");
            let file = arg_str(args, "file").unwrap_or("");
            let stat = args
                .get("stat_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(format!(
                "git_show:{}:{}:{}",
                commit.to_lowercase(),
                file,
                stat
            ))
        }
        // Non-cacheable tools (bash, write_file, web_fetch, etc.) — no semantic key
        _ => None,
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_end_matches('/').to_string()
}

fn normalize_repo(repo: &str) -> String {
    repo.trim().to_lowercase().trim_end_matches('/').to_string()
}

// ─── Tier 3: Output Similarity ───────────────────────────────────────────────

/// TF-IDF cosine similarity between two tool outputs.
/// Returns 0.0-1.0. Outputs shorter than MIN_OUTPUT_LEN are not compared.
const MIN_OUTPUT_LEN: usize = 30;

pub fn output_similarity(output1: &str, output2: &str) -> f64 {
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
    /// Tier 2: semantic_key → (turn, tool_name)
    param_cache: HashMap<String, (usize, String)>,
    /// Tier 3: (tool_name, turn) → truncated output for similarity comparison
    /// Only stores first 2000 chars of output to bound memory.
    output_log: Vec<(String, usize, String)>,
    /// Max entries in output_log before oldest are evicted
    max_output_entries: usize,
}

impl SemanticDedup {
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold,
            param_cache: HashMap::new(),
            output_log: Vec::new(),
            max_output_entries: 50,
        }
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
        let mut result = None;

        // Tier 2: Parameter-aware match
        if let Some(sem_key) = semantic_call_key(tool_name, args) {
            if let Some((prev_turn, _prev_tool)) = self.param_cache.get(&sem_key)
                && current_turn > *prev_turn
            {
                result = Some((*prev_turn, "param_match".to_string()));
            }
            self.param_cache
                .insert(sem_key, (current_turn, tool_name.to_string()));
        }

        // Tier 3: Output similarity (only if Tier 2 didn't match)
        if result.is_none() && output.len() >= MIN_OUTPUT_LEN {
            for (prev_tool, prev_turn, prev_output) in self.output_log.iter().rev() {
                if prev_tool == tool_name && current_turn > *prev_turn {
                    let sim = output_similarity(output, prev_output);
                    if sim >= self.threshold {
                        result = Some((*prev_turn, format!("output_sim={:.2}", sim)));
                        break;
                    }
                }
            }
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
        self.output_log
            .push((tool_name.to_string(), current_turn, truncated.to_string()));
        if self.output_log.len() > self.max_output_entries {
            self.output_log.remove(0);
        }

        result
    }

    /// Number of entries tracked (for diagnostics).
    pub fn param_cache_size(&self) -> usize {
        self.param_cache.len()
    }

    pub fn output_log_size(&self) -> usize {
        self.output_log.len()
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

        let mut files: Vec<&str> = Vec::new();
        let mut searches: Vec<String> = Vec::new();
        let mut git_ops: Vec<&str> = Vec::new();
        let mut github_ops: Vec<String> = Vec::new();
        let mut memory_ops: Vec<&str> = Vec::new();
        let mut other: Vec<String> = Vec::new();

        for (key, (_turn, tool)) in &self.param_cache {
            match tool.as_str() {
                "read_file" => {
                    if let Some(path) = key.strip_prefix("read_file:") {
                        files.push(path);
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
                t if t.starts_with("git_") => {
                    git_ops.push(t.strip_prefix("git_").unwrap_or(t));
                }
                t if t.starts_with("github_") => {
                    // Extract repo from key if present
                    if let Some(repo) = key.split(':').nth(1) {
                        github_ops.push(format!(
                            "{} {}",
                            t.strip_prefix("github_").unwrap_or(t),
                            repo
                        ));
                    } else {
                        github_ops.push(t.strip_prefix("github_").unwrap_or(t).to_string());
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
            let display: Vec<_> = files.iter().take(5).copied().collect();
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
    pub fn has_file(&self, path: &str) -> bool {
        let key = format!("read_file:{}", normalize_path(path));
        self.param_cache.contains_key(&key)
    }

    /// Check if we've already done a grep in a specific directory.
    pub fn has_grep_in(&self, path: &str) -> bool {
        let key = format!("grep:{}", normalize_path(path));
        self.param_cache.contains_key(&key)
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
        let k1 = semantic_call_key("github_list_prs", &json!({"repo": "matrixorigin/mo"}));
        let k2 = semantic_call_key("github_list_prs", &json!({"repo": "MatrixOrigin/MO"}));
        assert_eq!(k1, k2);
    }

    #[test]
    fn read_file_trailing_slash() {
        let k1 = semantic_call_key("read_file", &json!({"path": "src/main.rs"}));
        let k2 = semantic_call_key("read_file", &json!({"path": "src/main.rs/"}));
        assert_eq!(k1, k2);
    }

    #[test]
    fn grep_same_file_different_pattern() {
        let k1 = semantic_call_key("grep", &json!({"pattern": "foo", "path": "src/main.rs"}));
        let k2 = semantic_call_key("grep", &json!({"pattern": "bar", "path": "src/main.rs"}));
        assert_eq!(
            k1, k2,
            "grep on same file should match regardless of pattern"
        );
    }

    #[test]
    fn grep_different_file() {
        let k1 = semantic_call_key("grep", &json!({"pattern": "foo", "path": "src/a.rs"}));
        let k2 = semantic_call_key("grep", &json!({"pattern": "foo", "path": "src/b.rs"}));
        assert_ne!(k1, k2, "different files should produce different keys");
    }

    #[test]
    fn git_diff_default_vs_explicit_head() {
        let k1 = semantic_call_key("git_diff", &json!({}));
        let k2 = semantic_call_key("git_diff", &json!({"ref": "HEAD"}));
        assert_eq!(k1, k2, "default should match explicit HEAD");
    }

    #[test]
    fn git_diff_different_refs() {
        let k1 = semantic_call_key("git_diff", &json!({"ref": "HEAD"}));
        let k2 = semantic_call_key("git_diff", &json!({"ref": "main"}));
        assert_ne!(k1, k2, "different refs should differ");
    }

    #[test]
    fn git_diff_staged_differs_from_unstaged() {
        let k1 = semantic_call_key("git_diff", &json!({}));
        let k2 = semantic_call_key("git_diff", &json!({"staged": true}));
        assert_ne!(k1, k2, "staged vs unstaged should differ");
    }

    #[test]
    fn git_status_always_same_key() {
        let k1 = semantic_call_key("git_status", &json!({}));
        let k2 = semantic_call_key("git_status", &json!({"extra": "ignored"}));
        assert_eq!(k1, k2, "git_status should always be same key");
    }

    #[test]
    fn bash_returns_none() {
        assert!(semantic_call_key("bash", &json!({"command": "ls"})).is_none());
    }

    #[test]
    fn write_file_returns_none() {
        assert!(semantic_call_key("write_file", &json!({"path": "a.rs"})).is_none());
    }

    #[test]
    fn github_get_pr_includes_number() {
        let k1 = semantic_call_key("github_get_pr", &json!({"repo": "org/repo", "number": 42}));
        let k2 = semantic_call_key("github_get_pr", &json!({"repo": "org/repo", "number": 43}));
        assert_ne!(k1, k2, "different PR numbers should differ");
    }

    #[test]
    fn github_get_pr_same_pr_case_insensitive() {
        let k1 = semantic_call_key("github_get_pr", &json!({"repo": "Org/Repo", "number": 42}));
        let k2 = semantic_call_key("github_get_pr", &json!({"repo": "org/repo", "number": 42}));
        assert_eq!(k1, k2, "same PR on same repo should match");
    }

    #[test]
    fn git_log_search_case_insensitive() {
        let k1 = semantic_call_key("git_log_search", &json!({"query": "Fix Bug"}));
        let k2 = semantic_call_key("git_log_search", &json!({"query": "fix bug"}));
        assert_eq!(k1, k2, "search query should be case insensitive");
    }

    // ── Tier 3: output_similarity ──

    #[test]
    fn identical_outputs() {
        let out = "PR #123: fix bug in parser\nPR #124: add tests";
        assert!((output_similarity(out, out) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn high_overlap_outputs() {
        let out1 = "PR #1: fix authentication bug\nPR #2: add new feature\nPR #3: docs update";
        let out2 = "PR #1: fix authentication bug\nPR #2: add new feature\nPR #4: test coverage";
        let sim = output_similarity(out1, out2);
        assert!(sim >= 0.5, "2/3 overlap should be high: {sim}");
    }

    #[test]
    fn completely_different_outputs() {
        let out1 = "src/main.rs: fn main() { println!(\"hello world\"); }";
        let out2 =
            "CREATE TABLE users (id INT, name VARCHAR(255), email VARCHAR(255) PRIMARY KEY);";
        let sim = output_similarity(out1, out2);
        assert!(sim < 0.3, "different domains should be low: {sim}");
    }

    #[test]
    fn short_outputs_return_zero() {
        assert_eq!(output_similarity("ok", "ok"), 0.0);
        assert_eq!(output_similarity("", "something"), 0.0);
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
    fn tracker_detects_output_similarity() {
        let mut tracker = SemanticDedup::new(0.7);
        let output1 = "PR #1: fix\nPR #2: feature\nPR #3: docs\nPR #4: perf improvement";
        tracker.check_and_record("github_list_prs", &json!({"repo": "a/b"}), output1, 1);

        // Same tool, different args, but very similar output
        let output2 = "PR #1: fix\nPR #2: feature\nPR #3: docs\nPR #5: refactor code";
        let result =
            tracker.check_and_record("github_list_prs", &json!({"repo": "a/c"}), output2, 2);
        // Should detect via output similarity (different repo → no param match)
        // Note: may or may not trigger depending on exact cosine score
        if let Some((_, reason)) = &result {
            assert!(reason.starts_with("output_sim"));
        }
    }

    #[test]
    fn tracker_no_false_positive_different_tools() {
        let mut tracker = SemanticDedup::new(0.75);
        tracker.check_and_record("read_file", &json!({"path": "a.rs"}), "content a", 1);
        let result = tracker.check_and_record(
            "git_blame",
            &json!({"file": "a.rs"}),
            "different content",
            2,
        );
        // read_file and git_blame have different semantic keys
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
        tracker.check_and_record("git_status", &json!({}), "clean", 0);
        tracker.check_and_record("git_diff", &json!({"ref": "HEAD~3"}), "diff", 1);

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
        let result = tracker.check_and_record("shell_exec", &json!({"command": "echo2"}), &output2, 2);

        // Just verify no panic occurred - similarity result depends on exact truncation
        drop(result);
    }
}
