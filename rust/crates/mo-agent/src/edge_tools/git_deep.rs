//! Advanced git tools: blame, file history, contributors, semantic commit search.
//!
//! These tools provide deep git repository intelligence beyond basic status/diff/log.
//! The semantic search reuses the shared CJK-aware tokenizer from `text_tokenize`.

use std::collections::HashMap;

use super::*;

// ─── Blame parsing ──────────────────────────────────────────────────────────

/// A single blame entry from `git blame --porcelain`.
struct BlameEntry {
    commit: String,
    author: String,
    date: String,
    line_no: u32,
    content: String,
}

/// Parse `git blame --porcelain` output into structured text.
fn parse_blame_porcelain(raw: &str) -> String {
    let mut entries: Vec<BlameEntry> = Vec::new();
    let mut current_commit = String::new();
    let mut current_author = String::new();
    let mut current_date = String::new();
    let mut current_line_no: u32 = 0;

    for line in raw.lines() {
        if line.len() >= 40 && line.chars().take(40).all(|c| c.is_ascii_hexdigit()) {
            // Commit header line: <sha> <orig_line> <final_line> [<num_lines>]
            let parts: Vec<&str> = line.split_whitespace().collect();
            current_commit = parts
                .first()
                .map(|s| s[..8.min(s.len())].to_string())
                .unwrap_or_default();
            current_line_no = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("author ") {
            current_author = rest.to_string();
        } else if let Some(rest) = line.strip_prefix("author-time ") {
            if let Ok(ts) = rest.parse::<i64>() {
                let dt = chrono::DateTime::from_timestamp(ts, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| rest.to_string());
                current_date = dt;
            }
        } else if let Some(content) = line.strip_prefix('\t') {
            entries.push(BlameEntry {
                commit: current_commit.clone(),
                author: current_author.clone(),
                date: current_date.clone(),
                line_no: current_line_no,
                content: content.to_string(),
            });
        }
    }

    if entries.is_empty() {
        return raw.to_string(); // Fallback: return raw output
    }

    // Format as structured output
    let mut result = String::new();
    for e in &entries {
        result.push_str(&format!(
            "L{:<4} {} {} [{}] {}\n",
            e.line_no, e.commit, e.date, e.author, e.content
        ));
    }

    // Add summary
    let unique_authors: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.author.as_str()).collect();
    let unique_commits: std::collections::HashSet<&str> =
        entries.iter().map(|e| e.commit.as_str()).collect();
    result.push_str(&format!(
        "\n--- {} lines, {} authors, {} commits ---",
        entries.len(),
        unique_authors.len(),
        unique_commits.len()
    ));

    result
}

// ─── TF-IDF commit search ──────────────────────────────────────────────────

/// A parsed commit with pre-computed tokens.
struct CommitDoc {
    hash: String,
    author: String,
    date: String,
    message: String,
    tokens: Vec<String>,
}

/// Score commit messages against a query using TF-IDF cosine similarity.
/// Returns (commit_index, score) pairs sorted by descending score.
fn score_commits(query: &str, commits: &[CommitDoc]) -> Vec<(usize, f64)> {
    let query_tokens = mo_agent_runtime::text_tokenize::tokenize(query);
    if query_tokens.is_empty() || commits.is_empty() {
        return Vec::new();
    }

    let n = commits.len() as f64;

    // Build IDF from the commit corpus
    let mut doc_freq: HashMap<String, usize> = HashMap::new();
    for doc in commits {
        let unique: std::collections::HashSet<&String> = doc.tokens.iter().collect();
        for t in unique {
            *doc_freq.entry(t.clone()).or_default() += 1;
        }
    }
    let idf: HashMap<String, f64> = doc_freq
        .into_iter()
        .map(|(term, df)| (term, (n / df as f64).ln().max(0.1)))
        .collect();

    // Score each commit
    let mut scores: Vec<(usize, f64)> = commits
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            // Build doc TF
            let total = doc.tokens.len().max(1) as f64;
            let mut doc_tf: HashMap<&str, f64> = HashMap::new();
            for t in &doc.tokens {
                *doc_tf.entry(t.as_str()).or_default() += 1.0;
            }
            for v in doc_tf.values_mut() {
                *v /= total;
            }

            // Cosine similarity
            let mut dot = 0.0;
            let mut q_norm_sq = 0.0;
            let mut d_norm_sq = 0.0;

            for qt in &query_tokens {
                let idf_val = idf.get(qt.as_str()).copied().unwrap_or(0.0);
                let q_w = idf_val; // query uses binary TF
                q_norm_sq += q_w * q_w;
                if let Some(&tf) = doc_tf.get(qt.as_str()) {
                    let d_w = tf * idf_val;
                    dot += q_w * d_w;
                }
            }
            for (term, &tf) in &doc_tf {
                let idf_val = idf.get(*term).copied().unwrap_or(0.0);
                let d_w = tf * idf_val;
                d_norm_sq += d_w * d_w;
            }

            let denom = q_norm_sq.sqrt() * d_norm_sq.sqrt();
            let score = if denom > 0.0 { dot / denom } else { 0.0 };
            (i, score)
        })
        .filter(|(_, s)| *s > 0.01) // Filter noise
        .collect();

    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores
}

// ─── Tool implementations ───────────────────────────────────────────────────

impl ToolExecutor {
    /// `git_blame`: structured blame output with author, date, commit per line.
    pub(crate) fn git_blame(&self, args: &Value) -> String {
        let file = match args.get("file").and_then(Value::as_str) {
            Some(f) => f,
            None => return "Error: missing 'file' parameter".to_string(),
        };

        let mut cmd_args = vec!["blame", "--porcelain"];

        let line_range;
        let start = args.get("line_start").and_then(Value::as_u64);
        let end = args.get("line_end").and_then(Value::as_u64);
        if let Some(s) = start {
            let e = end.unwrap_or(s);
            line_range = format!("-L{},{}", s, e);
            cmd_args.push(&line_range);
        }

        cmd_args.push(file);

        let output = self.git_run(&cmd_args);
        if output.starts_with("Error") || output.contains("fatal:") {
            return output;
        }

        let parsed = parse_blame_porcelain(&output);
        truncate_output(parsed, tool_output_limit())
    }

    /// `git_file_history`: change history for a specific file with follow support.
    pub(crate) fn git_file_history(&self, args: &Value) -> String {
        let file = match args.get("file").and_then(Value::as_str) {
            Some(f) => f,
            None => return "Error: missing 'file' parameter".to_string(),
        };

        let n = args.get("n").and_then(Value::as_u64).unwrap_or(10);
        let n_str = format!("-{}", n);

        let output = self.git_run(&[
            "log",
            "--follow",
            "--format=%H|%an|%ad|%s",
            "--date=short",
            &n_str,
            "--",
            file,
        ]);

        if output.trim().is_empty() {
            return format!("No history found for '{file}'");
        }

        let mut lines = Vec::new();
        for line in output.lines() {
            let parts: Vec<&str> = line.splitn(4, '|').collect();
            if parts.len() >= 4 {
                lines.push(format!(
                    "{} {} [{}] {}",
                    &parts[0][..8.min(parts[0].len())],
                    parts[2],
                    parts[1],
                    parts[3],
                ));
            }
        }

        if lines.is_empty() {
            return format!("No history found for '{file}'");
        }

        truncate_output(
            format!(
                "File: {}\nCommits: {}\n\n{}",
                file,
                lines.len(),
                lines.join("\n")
            ),
            tool_output_limit(),
        )
    }

    /// `git_contributors`: repository contributor analytics with hot files.
    pub(crate) fn git_contributors(&self, args: &Value) -> String {
        let path = args.get("path").and_then(Value::as_str);
        let since = args.get("since").and_then(Value::as_str);

        let mut parts = Vec::new();

        // 1. Top contributors
        let mut shortlog_args = vec!["shortlog", "-sn", "--all", "--no-merges"];
        let since_flag;
        if let Some(s) = since {
            since_flag = format!("--since={}", s);
            shortlog_args.push(&since_flag);
        }
        if let Some(p) = path {
            shortlog_args.push("--");
            shortlog_args.push(p);
        }
        let contributors = self.git_run(&shortlog_args);
        if !contributors.trim().is_empty() {
            let top: Vec<&str> = contributors.lines().take(10).collect();
            parts.push(format!("## Top Contributors\n{}", top.join("\n")));
        }

        // 2. Hot files (most frequently changed)
        let n_str = "-200".to_string();
        let mut log_args = vec!["log", "--format=format:", "--name-only", &n_str];
        let since_flag2;
        if let Some(s) = since {
            since_flag2 = format!("--since={}", s);
            log_args.push(&since_flag2);
        }
        if let Some(p) = path {
            log_args.push("--");
            log_args.push(p);
        }
        let file_output = self.git_run(&log_args);
        if !file_output.trim().is_empty() {
            let mut freq: HashMap<&str, usize> = HashMap::new();
            for line in file_output.lines() {
                let l = line.trim();
                if !l.is_empty() {
                    *freq.entry(l).or_default() += 1;
                }
            }
            let mut sorted: Vec<_> = freq.into_iter().collect();
            sorted.sort_by(|a, b| b.1.cmp(&a.1));
            let top_files: Vec<String> = sorted
                .iter()
                .take(10)
                .map(|(f, c)| format!("  {:>3}× {}", c, f))
                .collect();
            parts.push(format!(
                "## Hot Files (most changed)\n{}",
                top_files.join("\n")
            ));
        }

        // 3. Recent activity
        let recent = self.git_run(&["log", "--oneline", "-5"]);
        if !recent.trim().is_empty() {
            parts.push(format!("## Recent Activity\n{}", recent.trim()));
        }

        if parts.is_empty() {
            "No git history found".to_string()
        } else {
            truncate_output(parts.join("\n\n"), tool_output_limit())
        }
    }

    /// `git_log_search`: semantic search on commit messages using TF-IDF.
    /// Reuses the shared CJK-aware tokenizer for cross-language support.
    pub(crate) fn git_log_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q,
            _ => return "Error: missing or empty 'query' parameter".to_string(),
        };

        let n = args.get("n").and_then(Value::as_u64).unwrap_or(200);
        let n_str = format!("-{}", n);

        let output = self.git_run(&["log", "--format=%H|%an|%ad|%s", "--date=short", &n_str]);

        if output.trim().is_empty() {
            return "No commits found".to_string();
        }

        // Parse commits and tokenize messages
        let commits: Vec<CommitDoc> = output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.splitn(4, '|').collect();
                if parts.len() >= 4 {
                    let msg = parts[3].to_string();
                    let tokens = mo_agent_runtime::text_tokenize::tokenize(&msg);
                    Some(CommitDoc {
                        hash: parts[0].to_string(),
                        author: parts[1].to_string(),
                        date: parts[2].to_string(),
                        message: msg,
                        tokens,
                    })
                } else {
                    None
                }
            })
            .collect();

        if commits.is_empty() {
            return "No commits found".to_string();
        }

        // Score and rank
        let ranked = score_commits(query, &commits);
        if ranked.is_empty() {
            return format!(
                "No commits matching '{}' found in last {} commits",
                query,
                commits.len()
            );
        }

        // Return top 10
        let top_k = 10.min(ranked.len());
        let mut result = format!(
            "Search: '{}' ({} commits searched, {} matches)\n\n",
            query,
            commits.len(),
            ranked.len()
        );
        for (i, &(idx, score)) in ranked.iter().take(top_k).enumerate() {
            let c = &commits[idx];
            result.push_str(&format!(
                "{}. [score:{:.2}] {} {} [{}] {}\n",
                i + 1,
                score,
                &c.hash[..8.min(c.hash.len())],
                c.date,
                c.author,
                c.message,
            ));
        }

        truncate_output(result, tool_output_limit())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Blame parsing ──

    #[test]
    fn parse_blame_empty_returns_raw() {
        let result = parse_blame_porcelain("");
        assert_eq!(result, "");
    }

    #[test]
    fn parse_blame_porcelain_format() {
        let raw = "\
abc1234567890abc1234567890abc1234567890ab 1 1 1
author Alice
author-mail <alice@example.com>
author-time 1700000000
author-tz +0000
committer Alice
committer-mail <alice@example.com>
committer-time 1700000000
committer-tz +0000
summary Initial commit
filename test.rs
\tfn main() {}
def5678901234567890123456789012345678901 2 2 1
author Bob
author-mail <bob@example.com>
author-time 1700100000
author-tz +0000
committer Bob
committer-mail <bob@example.com>
committer-time 1700100000
committer-tz +0000
summary Add feature
filename test.rs
\t    println!(\"hello\");
";
        let result = parse_blame_porcelain(raw);
        assert!(
            result.contains("Alice"),
            "should contain author Alice: {result}"
        );
        assert!(
            result.contains("Bob"),
            "should contain author Bob: {result}"
        );
        assert!(result.contains("fn main"), "should contain code: {result}");
        assert!(
            result.contains("2 authors"),
            "should contain author count: {result}"
        );
        assert!(
            result.contains("2 commits"),
            "should contain commit count: {result}"
        );
    }

    #[test]
    fn parse_blame_single_line() {
        let raw = "\
abc1234567890abc1234567890abc1234567890ab 5 5 1
author Alice
author-time 1700000000
filename test.rs
\tlet x = 42;
";
        let result = parse_blame_porcelain(raw);
        assert!(result.contains("L5"), "should have line number: {result}");
        assert!(
            result.contains("let x = 42"),
            "should have content: {result}"
        );
        assert!(result.contains("1 lines"), "should show 1 line: {result}");
    }

    // ── TF-IDF commit search ──

    fn make_commit(hash: &str, author: &str, date: &str, msg: &str) -> CommitDoc {
        CommitDoc {
            hash: hash.to_string(),
            author: author.to_string(),
            date: date.to_string(),
            message: msg.to_string(),
            tokens: mo_agent_runtime::text_tokenize::tokenize(msg),
        }
    }

    #[test]
    fn score_commits_empty_query() {
        let commits = vec![make_commit("abc", "alice", "2024-01-01", "fix bug")];
        let results = score_commits("", &commits);
        assert!(results.is_empty(), "empty query should return no results");
    }

    #[test]
    fn score_commits_empty_corpus() {
        let results = score_commits("fix bug", &[]);
        assert!(results.is_empty(), "empty corpus should return no results");
    }

    #[test]
    fn score_commits_exact_match_ranks_first() {
        let commits = vec![
            make_commit(
                "aaa",
                "alice",
                "2024-01-01",
                "refactor authentication module",
            ),
            make_commit("bbb", "bob", "2024-01-02", "fix memory leak in cache"),
            make_commit("ccc", "carol", "2024-01-03", "update documentation for API"),
        ];
        let results = score_commits("authentication refactor", &commits);
        assert!(!results.is_empty(), "should find matches");
        assert_eq!(results[0].0, 0, "auth refactor commit should rank first");
    }

    #[test]
    fn score_commits_partial_match() {
        let commits = vec![
            make_commit("aaa", "alice", "2024-01-01", "add unit tests for parser"),
            make_commit(
                "bbb",
                "bob",
                "2024-01-02",
                "fix parser edge case with unicode",
            ),
            make_commit("ccc", "carol", "2024-01-03", "update CI configuration"),
        ];
        let results = score_commits("parser", &commits);
        assert!(results.len() >= 2, "should match commits mentioning parser");
        // Both commits mentioning parser should score higher than CI one
        let matched_indices: Vec<usize> = results.iter().map(|r| r.0).collect();
        assert!(matched_indices.contains(&0), "should match test commit");
        assert!(matched_indices.contains(&1), "should match fix commit");
    }

    #[test]
    fn score_commits_cjk_query() {
        let commits = vec![
            make_commit("aaa", "alice", "2024-01-01", "修复认证模块的内存泄漏"),
            make_commit("bbb", "bob", "2024-01-02", "add feature flag for beta"),
            make_commit("ccc", "carol", "2024-01-03", "优化数据库查询性能"),
        ];
        let results = score_commits("认证", &commits);
        assert!(!results.is_empty(), "should find CJK matches");
        assert_eq!(results[0].0, 0, "认证 should match the auth commit");
    }

    #[test]
    fn score_commits_no_match() {
        let commits = vec![
            make_commit("aaa", "alice", "2024-01-01", "fix bug in parser"),
            make_commit("bbb", "bob", "2024-01-02", "update documentation"),
        ];
        let results = score_commits("kubernetes deployment helm", &commits);
        assert!(results.is_empty(), "unrelated query should have no matches");
    }

    #[test]
    fn score_commits_stemming_helps() {
        let commits = vec![
            make_commit(
                "aaa",
                "alice",
                "2024-01-01",
                "deploying new service to production",
            ),
            make_commit("bbb", "bob", "2024-01-02", "fix test flakiness"),
        ];
        // "deployment" stems to "deploy", "deploying" stems to "deploy" → should match
        let results = score_commits("deployment", &commits);
        assert!(
            !results.is_empty(),
            "stemming should match deploying ↔ deployment"
        );
        assert_eq!(results[0].0, 0, "deploy commit should rank first");
    }

    // ── Tool implementations (integration, requires git repo) ──

    #[test]
    fn git_blame_missing_file_param() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.git_blame(&serde_json::json!({}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn git_file_history_missing_file_param() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.git_file_history(&serde_json::json!({}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn git_log_search_missing_query() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.git_log_search(&serde_json::json!({}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn git_log_search_empty_query() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.git_log_search(&serde_json::json!({"query": ""}));
        assert!(result.contains("Error"), "should error on empty: {result}");
    }

    // Integration tests that run in the actual repo
    #[test]
    fn git_blame_on_real_file() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);
        // Try to blame Cargo.toml (exists in most Rust projects)
        let result = executor.git_blame(&serde_json::json!({"file": "Cargo.toml"}));
        // May fail if not in a git repo — just verify no panic
        assert!(!result.is_empty(), "should return something");
    }

    #[test]
    fn git_file_history_on_real_file() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);
        let result = executor.git_file_history(&serde_json::json!({"file": "Cargo.toml", "n": 3}));
        assert!(!result.is_empty(), "should return something");
    }

    #[test]
    fn git_contributors_real_repo() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);
        let result = executor.git_contributors(&serde_json::json!({}));
        assert!(!result.is_empty(), "should return something");
    }

    #[test]
    fn git_log_search_real_repo() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);
        let result = executor.git_log_search(&serde_json::json!({"query": "fix", "n": 50}));
        assert!(!result.is_empty(), "should return something");
    }

    // ── Additional integration tests for stronger verification ──

    #[test]
    fn git_tools_all_have_required_params() {
        let executor = ToolExecutor::new(std::env::temp_dir());

        // git_blame requires file
        let blame = executor.git_blame(&serde_json::json!({}));
        assert!(blame.contains("missing"), "blame should require file param");

        // git_file_history requires file
        let history = executor.git_file_history(&serde_json::json!({}));
        assert!(
            history.contains("missing"),
            "history should require file param"
        );

        // git_log_search requires query
        let search = executor.git_log_search(&serde_json::json!({}));
        assert!(
            search.contains("missing"),
            "search should require query param"
        );

        // git_contributors is optional (no required params)
        let contrib = executor.git_contributors(&serde_json::json!({}));
        assert!(
            !contrib.contains("missing"),
            "contributors should be optional"
        );
    }

    #[test]
    fn git_blame_with_line_range_respects_bounds() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);

        // Blame lines 1-3 of Cargo.toml
        let result = executor.git_blame(&serde_json::json!({
            "file": "Cargo.toml",
            "line_start": 1,
            "line_end": 3
        }));

        // Parse line numbers from output: format is "L1   hash date [author] content"
        let line_numbers: Vec<u32> = result
            .lines()
            .filter_map(|l| {
                let trimmed = l.trim_start_matches('L');
                trimmed.split_whitespace().next()?.parse::<u32>().ok()
            })
            .collect();

        // All parsed line numbers must be within 1-3
        for n in &line_numbers {
            assert!(
                *n >= 1 && *n <= 3,
                "line number {} is outside range 1-3 in output: {}",
                n,
                result
            );
        }

        // Must have found at least one line
        assert!(
            !line_numbers.is_empty(),
            "should have parsed at least one line number: {}",
            result
        );
    }

    #[test]
    fn git_log_search_respects_limit() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);

        // Search with limit of 2
        let result = executor.git_log_search(&serde_json::json!({
            "query": "test",
            "n": 2
        }));

        // Output format is "hash|author|date|message" per line
        let commit_lines: Vec<_> = result
            .lines()
            .filter(|l| l.contains('|') && !l.starts_with('#'))
            .collect();

        assert!(
            commit_lines.len() <= 2,
            "should return at most 2 results, got {}",
            commit_lines.len()
        );
    }

    #[test]
    fn git_file_history_returns_structured_output() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);

        let result = executor.git_file_history(&serde_json::json!({
            "file": "Cargo.toml",
            "n": 3
        }));

        // Should NOT be the "no history" message
        assert!(
            !result.contains("No history found"),
            "should find history for Cargo.toml: {}",
            result
        );

        // Output format: "File: <name>\nCommits: <N>\n\n<hash> <date> [<author>] <msg>"
        assert!(
            result.contains("File: Cargo.toml"),
            "should contain file header"
        );
        assert!(result.contains("Commits:"), "should contain commit count");

        // Extract commit count and verify it's a positive number
        let commit_count: usize = result
            .lines()
            .find(|l| l.starts_with("Commits:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        assert!(commit_count > 0, "should have at least 1 commit");
        assert!(
            commit_count <= 3,
            "should respect n=3 limit, got {}",
            commit_count
        );
    }

    #[test]
    fn git_blame_porcelain_parses_author_and_date() {
        // Test the parsing function directly with known input
        let raw = "abc1234567890abc1234567890abc1234567890ab 1 1 1\nauthor Alice\nauthor-mail <alice@example.com>\nauthor-time 1700000000\nauthor-tz +0000\nsummary Initial commit\nfilename test.rs\n\tlet x = 42;\n";
        let result = parse_blame_porcelain(raw);

        // Verify structured output
        assert!(result.contains("Alice"), "should contain author name");
        assert!(result.contains("2023"), "should contain parsed date");
        assert!(result.contains("L1"), "should contain line number");
        assert!(result.contains("let x = 42"), "should contain code content");
        assert!(result.contains("1 lines"), "should contain line count");
        assert!(result.contains("1 authors"), "should contain author count");
        assert!(result.contains("1 commits"), "should contain commit count");
    }

    #[test]
    fn git_blame_porcelain_multi_author_summary() {
        let raw = "aaaa1234567890aaaa1234567890aaaa12345678 1 1 1\nauthor Alice\nauthor-time 1700000000\nfilename a.rs\n\tline one\nbbbb1234567890bbbb1234567890bbbb12345678 2 2 1\nauthor Bob\nauthor-time 1700100000\nfilename a.rs\n\tline two\n";
        let result = parse_blame_porcelain(raw);

        assert!(result.contains("2 lines"), "should count 2 lines");
        assert!(result.contains("2 authors"), "should count 2 authors");
        assert!(result.contains("2 commits"), "should count 2 commits");
    }

    #[test]
    fn git_contributors_returns_top_list() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);

        let result = executor.git_contributors(&serde_json::json!({}));

        // Should contain Top Contributors section
        assert!(
            result.contains("Top Contributors"),
            "should have Top Contributors section: {}",
            result
        );

        // Should contain numbers (commit counts)
        assert!(
            result.contains('\t'),
            "should contain tab-separated counts: {}",
            result
        );
    }

    #[test]
    fn git_contributors_with_path_filter() {
        let root = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
        let executor = ToolExecutor::new(&root);

        // Filter to a specific path
        let result = executor.git_contributors(&serde_json::json!({
            "path": "Cargo.toml"
        }));

        // Should still return valid output (even if only 1 contributor)
        assert!(!result.is_empty());
        assert!(result.contains("Top Contributors") || result.contains("Hot Files"));
    }
}
