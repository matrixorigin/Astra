pub use astra_server_types::conflict_resolver::*;

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
        let response = "```\n<<<<<<< HEAD\nfoo\n=======\nbar\n>>>>>>> branch\n```";
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
