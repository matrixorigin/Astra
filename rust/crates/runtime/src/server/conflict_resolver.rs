//! LLM-assisted git merge conflict resolution.
//!
//! When [`WorktreeManager::merge_worktrees`] detects a conflict, the resolver
//! extracts base/ours/theirs versions via `git show :N:path`, builds a
//! structured prompt, and asks an LLM to produce the resolved content.
//!
//! # Flow
//!
//! ```text
//! conflict detected
//!       ↓
//! extract_file_conflicts()     — git show :1/:2/:3:path
//!       ↓
//! ConflictResolver::resolve()  — LLM call per file (or batched)
//!       ↓
//! apply_resolutions()          — write files + git add + git commit
//! ```

use std::path::Path;
use tokio::process::Command;

// ─── Types ──────────────────────────────────────────────────────────────────

/// A single file conflict with base, ours, and theirs versions.
#[derive(Debug, Clone)]
pub struct FileConflict {
    /// Relative path within the repository.
    pub path: String,
    /// Common ancestor content (merge base).
    pub base: String,
    /// Current branch content (HEAD / ours).
    pub ours: String,
    /// Incoming branch content (agent branch / theirs).
    pub theirs: String,
}

/// Result of resolving a single file.
#[derive(Debug, Clone)]
pub struct ResolvedFile {
    pub path: String,
    pub content: String,
    /// Brief LLM explanation of how the conflict was resolved.
    pub explanation: String,
}

/// Outcome of attempting to resolve all conflicts for one agent merge.
#[derive(Debug)]
pub struct ConflictResolution {
    pub agent_id: String,
    pub resolved: Vec<ResolvedFile>,
    /// Files that the resolver could not handle.
    pub failed: Vec<String>,
}

/// Trait for resolving git merge conflicts.
///
/// Implementations can use an LLM, a rule-based engine, or a human-in-the-loop.
#[async_trait::async_trait]
pub trait ConflictResolver: Send + Sync {
    /// Attempt to resolve the given file conflicts.
    ///
    /// `task_context` provides the high-level team task description so the LLM
    /// can make semantically-aware merge decisions (e.g. "refactor auth module"
    /// tells it that auth-related changes should likely be kept).
    async fn resolve_conflicts(
        &self,
        agent_id: &str,
        task_context: &str,
        conflicts: &[FileConflict],
    ) -> ConflictResolution;
}

// ─── LLM Implementation ────────────────────────────────────────────────────

/// Resolves merge conflicts by sending base/ours/theirs to an LLM.
pub struct LlmConflictResolver {
    model: String,
    api_key: String,
    base_url: String,
    provider: String,
    /// Maximum file size (bytes) to send to the LLM. Larger files are skipped.
    max_file_size: usize,
}

impl LlmConflictResolver {
    pub fn new(model: String, api_key: String, base_url: String, provider: String) -> Self {
        Self {
            model,
            api_key,
            base_url,
            provider,
            max_file_size: 100_000, // ~100KB default
        }
    }

    pub fn with_max_file_size(mut self, size: usize) -> Self {
        self.max_file_size = size;
        self
    }

    /// Build the prompt for a single file conflict.
    fn build_prompt(
        task_context: &str,
        agent_id: &str,
        conflict: &FileConflict,
    ) -> Vec<serde_json::Value> {
        let system = format!(
            "You are a precise code merge conflict resolver. A multi-agent team is working on: {task_context}\n\n\
             Agent '{agent_id}' made changes that conflict with the main branch.\n\
             You will receive the BASE (common ancestor), OURS (main branch), and THEIRS (agent branch) versions of a file.\n\n\
             Rules:\n\
             1. Produce the COMPLETE resolved file content — not a diff, not a partial snippet.\n\
             2. Preserve the intent of BOTH sides when possible.\n\
             3. If changes are in different regions, include both.\n\
             4. If changes conflict in the same region, use your judgment based on the task context.\n\
             5. Never include conflict markers (<<<<<<< / ======= / >>>>>>>).\n\
             6. Output ONLY the resolved file content inside a single fenced code block.\n\
             7. After the code block, add a one-line explanation starting with 'Explanation:'."
        );

        let user = format!(
            "Resolve this merge conflict in `{path}`:\n\n\
             ## BASE (common ancestor)\n```\n{base}\n```\n\n\
             ## OURS (main branch / HEAD)\n```\n{ours}\n```\n\n\
             ## THEIRS (agent '{agent_id}' branch)\n```\n{theirs}\n```",
            path = conflict.path,
            base = conflict.base,
            ours = conflict.ours,
            theirs = conflict.theirs,
        );

        vec![
            serde_json::json!({ "role": "system", "content": system }),
            serde_json::json!({ "role": "user", "content": user }),
        ]
    }

    /// Parse LLM response: extract code block content and explanation.
    fn parse_response(text: &str) -> Option<(String, String)> {
        // Find the first fenced code block
        let code_start = text.find("```")?;
        let after_fence = &text[code_start + 3..];
        // Skip optional language identifier on the opening fence line
        let content_start = after_fence.find('\n')? + 1;
        let content_area = &after_fence[content_start..];
        let code_end = content_area.find("```")?;
        let content = content_area[..code_end].trim_end().to_string();

        // Extract explanation if present
        let remainder = &content_area[code_end + 3..];
        let explanation = remainder
            .lines()
            .find(|l| l.trim().starts_with("Explanation:"))
            .map(|l| {
                l.trim()
                    .trim_start_matches("Explanation:")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();

        // Reject if content still has conflict markers
        if content.contains("<<<<<<<") || content.contains(">>>>>>>") {
            return None;
        }

        Some((content, explanation))
    }

    /// Call the LLM for a single file conflict.
    async fn resolve_one(
        &self,
        agent_id: &str,
        task_context: &str,
        conflict: &FileConflict,
    ) -> Result<ResolvedFile, String> {
        let messages = Self::build_prompt(task_context, agent_id, conflict);

        let result = crate::turn::llm_client::call_llm_and_collect(
            &messages,
            &[], // no tools
            &self.model,
            &self.api_key,
            &self.base_url,
            &self.provider,
            Some(8192),
            false,
            crate::turn::llm_client::LlmCancel::None,
        )
        .await
        .map_err(|e| format!("LLM call failed for {}: {e}", conflict.path))?;

        let (content, explanation) = Self::parse_response(&result.full_text)
            .ok_or_else(|| format!("failed to parse LLM response for {}", conflict.path))?;

        Ok(ResolvedFile {
            path: conflict.path.clone(),
            content,
            explanation,
        })
    }
}

#[async_trait::async_trait]
impl ConflictResolver for LlmConflictResolver {
    async fn resolve_conflicts(
        &self,
        agent_id: &str,
        task_context: &str,
        conflicts: &[FileConflict],
    ) -> ConflictResolution {
        let mut resolved = Vec::new();
        let mut failed = Vec::new();

        for conflict in conflicts {
            // Skip files that are too large for the LLM context
            let total_size = conflict.base.len() + conflict.ours.len() + conflict.theirs.len();
            if total_size > self.max_file_size {
                eprintln!(
                    "[conflict-resolver] skipping {} ({total_size} bytes > {} limit)",
                    conflict.path, self.max_file_size
                );
                failed.push(conflict.path.clone());
                continue;
            }

            match self.resolve_one(agent_id, task_context, conflict).await {
                Ok(rf) => {
                    eprintln!(
                        "[conflict-resolver] resolved {}: {}",
                        rf.path,
                        if rf.explanation.is_empty() {
                            "(no explanation)"
                        } else {
                            &rf.explanation
                        }
                    );
                    resolved.push(rf);
                }
                Err(e) => {
                    eprintln!("[conflict-resolver] failed {}: {e}", conflict.path);
                    failed.push(conflict.path.clone());
                }
            }
        }

        ConflictResolution {
            agent_id: agent_id.to_string(),
            resolved,
            failed,
        }
    }
}

// ─── Git helpers ────────────────────────────────────────────────────────────

/// Extract base/ours/theirs versions of conflicted files from a merge in progress.
///
/// Must be called while the merge conflict is still active (before `git merge --abort`).
/// Uses `git show :N:path` where N is 1=base, 2=ours, 3=theirs.
pub async fn extract_file_conflicts(
    repo_root: &Path,
    conflict_files: &[String],
) -> Vec<FileConflict> {
    let mut conflicts = Vec::with_capacity(conflict_files.len());

    for path in conflict_files {
        let base = git_show_stage(repo_root, 1, path).await;
        let ours = git_show_stage(repo_root, 2, path).await;
        let theirs = git_show_stage(repo_root, 3, path).await;

        conflicts.push(FileConflict {
            path: path.clone(),
            base: base.unwrap_or_default(),
            ours: ours.unwrap_or_default(),
            theirs: theirs.unwrap_or_default(),
        });
    }

    conflicts
}

/// Read a file at a specific merge stage from git's index.
///
/// Stage 1 = base, stage 2 = ours, stage 3 = theirs.
async fn git_show_stage(repo_root: &Path, stage: u8, path: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["show", &format!(":{stage}:{path}")])
        .current_dir(repo_root)
        .output()
        .await
        .ok()?;

    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        None
    }
}

/// Write resolved files back to the working tree and stage them.
///
/// After all files are staged, commits the merge with a descriptive message.
/// Returns `Ok(())` on success, `Err` if any write/stage/commit fails.
pub async fn apply_resolutions(
    repo_root: &Path,
    agent_id: &str,
    delegation_id: &str,
    resolved: &[ResolvedFile],
) -> Result<(), String> {
    for rf in resolved {
        let file_path = repo_root.join(&rf.path);

        // Write resolved content
        tokio::fs::write(&file_path, &rf.content)
            .await
            .map_err(|e| format!("failed to write {}: {e}", rf.path))?;

        // git add <file>
        let output = Command::new("git")
            .args(["add", &rf.path])
            .current_dir(repo_root)
            .output()
            .await
            .map_err(|e| format!("failed to run git add {}: {e}", rf.path))?;

        if !output.status.success() {
            return Err(format!(
                "git add {} failed: {}",
                rf.path,
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }

    // Check if there are still unresolved conflicts
    let unmerged = Command::new("git")
        .args(["diff", "--name-only", "--diff-filter=U"])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("failed to check unmerged files: {e}"))?;

    if !unmerged.stdout.is_empty() {
        return Err(format!(
            "unresolved conflicts remain: {}",
            String::from_utf8_lossy(&unmerged.stdout).trim()
        ));
    }

    // Commit the resolved merge
    let files_list: Vec<&str> = resolved.iter().map(|rf| rf.path.as_str()).collect();
    let commit_msg = format!(
        "merge agent {} (LLM-resolved conflicts in {})\n\nDelegation: {}\nResolved files: {}",
        agent_id,
        &delegation_id[..delegation_id.len().min(8)],
        delegation_id,
        files_list.join(", ")
    );

    let output = Command::new("git")
        .args(["commit", "--no-edit", "-m", &commit_msg])
        .current_dir(repo_root)
        .output()
        .await
        .map_err(|e| format!("git commit failed: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    Ok(())
}

// ─── Stub for testing ───────────────────────────────────────────────────────

/// Always-succeed resolver for testing.
///
/// Resolves by taking the "theirs" version (agent's changes win).
pub struct TheirsWinsResolver;

#[async_trait::async_trait]
impl ConflictResolver for TheirsWinsResolver {
    async fn resolve_conflicts(
        &self,
        agent_id: &str,
        _task_context: &str,
        conflicts: &[FileConflict],
    ) -> ConflictResolution {
        let resolved = conflicts
            .iter()
            .map(|c| ResolvedFile {
                path: c.path.clone(),
                content: c.theirs.clone(),
                explanation: "theirs-wins strategy".to_string(),
            })
            .collect();
        ConflictResolution {
            agent_id: agent_id.to_string(),
            resolved,
            failed: vec![],
        }
    }
}

/// Always-fail resolver for testing.
pub struct FailingResolver;

#[async_trait::async_trait]
impl ConflictResolver for FailingResolver {
    async fn resolve_conflicts(
        &self,
        agent_id: &str,
        _task_context: &str,
        conflicts: &[FileConflict],
    ) -> ConflictResolution {
        ConflictResolution {
            agent_id: agent_id.to_string(),
            resolved: vec![],
            failed: conflicts.iter().map(|c| c.path.clone()).collect(),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_response_extracts_code_block() {
        let response = r#"Here is the resolved file:

```rust
fn main() {
    println!("hello");
}
```

Explanation: Combined both changes, keeping the println from theirs."#;

        let (content, explanation) = LlmConflictResolver::parse_response(response).unwrap();
        assert_eq!(content, "fn main() {\n    println!(\"hello\");\n}");
        assert!(explanation.contains("Combined both changes"));
    }

    #[test]
    fn parse_response_rejects_conflict_markers() {
        let response = r#"```
<<<<<<< HEAD
foo
=======
bar
>>>>>>> branch
```"#;
        assert!(LlmConflictResolver::parse_response(response).is_none());
    }

    #[test]
    fn parse_response_handles_no_explanation() {
        let response = "```\nresolved content\n```";
        let (content, explanation) = LlmConflictResolver::parse_response(response).unwrap();
        assert_eq!(content, "resolved content");
        assert!(explanation.is_empty());
    }

    #[test]
    fn parse_response_handles_plain_code_fence() {
        let response = "```\nline1\nline2\n```\nExplanation: simple merge";
        let (content, explanation) = LlmConflictResolver::parse_response(response).unwrap();
        assert_eq!(content, "line1\nline2");
        assert_eq!(explanation, "simple merge");
    }

    #[tokio::test]
    async fn theirs_wins_resolver_takes_theirs() {
        let resolver = TheirsWinsResolver;
        let conflicts = vec![FileConflict {
            path: "src/main.rs".to_string(),
            base: "base".to_string(),
            ours: "ours".to_string(),
            theirs: "theirs content".to_string(),
        }];

        let result = resolver
            .resolve_conflicts("agent-1", "test task", &conflicts)
            .await;
        assert_eq!(result.resolved.len(), 1);
        assert_eq!(result.failed.len(), 0);
        assert_eq!(result.resolved[0].content, "theirs content");
    }

    #[tokio::test]
    async fn failing_resolver_fails_all() {
        let resolver = FailingResolver;
        let conflicts = vec![
            FileConflict {
                path: "a.rs".to_string(),
                base: "".to_string(),
                ours: "".to_string(),
                theirs: "".to_string(),
            },
            FileConflict {
                path: "b.rs".to_string(),
                base: "".to_string(),
                ours: "".to_string(),
                theirs: "".to_string(),
            },
        ];

        let result = resolver
            .resolve_conflicts("agent-1", "test task", &conflicts)
            .await;
        assert_eq!(result.resolved.len(), 0);
        assert_eq!(result.failed.len(), 2);
    }

    #[tokio::test]
    async fn extract_file_conflicts_on_non_git_dir() {
        // When called outside a git repo, all stages return None → empty strings
        let conflicts =
            extract_file_conflicts(Path::new("/tmp"), &["nonexistent.txt".to_string()]).await;
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].base.is_empty());
        assert!(conflicts[0].ours.is_empty());
        assert!(conflicts[0].theirs.is_empty());
    }

    #[test]
    fn build_prompt_contains_key_elements() {
        let conflict = FileConflict {
            path: "src/auth.rs".to_string(),
            base: "fn auth() {}".to_string(),
            ours: "fn auth() { check() }".to_string(),
            theirs: "fn auth() { verify() }".to_string(),
        };

        let messages = LlmConflictResolver::build_prompt("refactor auth", "coder", &conflict);
        assert_eq!(messages.len(), 2);

        let system = messages[0]["content"].as_str().unwrap();
        assert!(system.contains("merge conflict resolver"));
        assert!(system.contains("refactor auth"));
        assert!(system.contains("coder"));

        let user = messages[1]["content"].as_str().unwrap();
        assert!(user.contains("src/auth.rs"));
        assert!(user.contains("fn auth() { check() }"));
        assert!(user.contains("fn auth() { verify() }"));
    }
}
