//! Skill tool — allows the LLM to invoke registered skills as tool calls.
//!
//! # Architecture
//!
//! The skill system follows the same interception pattern as delegation:
//!
//! 1. **Schema injection**: If a [`SkillResolver`] is wired on [`AgenticLoopState`],
//!    the loop injects a `skill` tool schema listing available skills.
//!
//! 2. **Call interception**: When the LLM emits a `skill` tool call,
//!    [`partition_and_execute_skills`] intercepts it before the headless tool round.
//!
//! 3. **Resolution**: The [`SkillResolver`] loads the skill instructions and returns
//!    them as the tool result, so the LLM follows those instructions in the
//!    current conversation.
//!
//! # Host Implementations
//!
//! | Host | Crate | SkillResolver |
//! |------|-------|---------------|
//! | CLI  | astra-cli | Wraps `SkillRegistry` from `skill_instructions.rs` |
//! | Server | runtime/server | (Future) wraps cloud skill catalog |
//!
//! # Future: Sub-agent execution
//!
//! Skills with `isolated: true` (not yet supported) will get a full sub-loop
//! via [`SubRunExecutor`](super::super::server::delegation_engine::SubRunExecutor).

use serde_json::Value;

// ─── Skill resolution trait ──────────────────────────────────────────────────

/// Lightweight description of a skill for tool schema generation.
#[derive(Clone, Debug)]
pub struct SkillToolInfo {
    pub name: String,
    pub description: String,
    /// Natural-language hint for when the model should pick this skill.
    pub when_to_use: Option<String>,
}

/// A fully resolved skill ready for execution.
#[derive(Clone, Debug)]
pub struct ResolvedSkill {
    pub name: String,
    pub instructions: String,
    /// Model override (e.g. `"claude-sonnet-4-20250514"`).
    pub model: Option<String>,
    /// Token budget (0 or None = system default).
    pub max_tokens: Option<u32>,
    /// Tool allowlist (empty = all tools).
    pub allowed_tools: Vec<String>,
}

/// Trait for resolving skill names to instructions.
///
/// Implementations live in host crates (astra-cli, server) since the runtime
/// crate cannot depend on them.
pub trait SkillResolver: Send + Sync {
    /// Resolve a skill by name, loading instructions if needed.
    fn resolve(&self, name: &str) -> Result<ResolvedSkill, String>;

    /// List available skills for schema generation.
    fn available_skills(&self) -> Vec<SkillToolInfo>;
}

// ─── Tool schema ─────────────────────────────────────────────────────────────

const SKILL_TOOL_NAME: &str = "skill";

/// Generate the OpenAI-compatible tool schema for the `skill` tool.
///
/// The schema includes an enum of available skill names so the LLM can only
/// call skills that actually exist.
pub fn skill_tool_schema(skills: &[SkillToolInfo]) -> Value {
    let skill_entries: Vec<String> = skills
        .iter()
        .map(|s| {
            let mut entry = format!("- **{}**: {}", s.name, s.description);
            if let Some(when) = &s.when_to_use {
                entry.push_str(&format!(" (use when: {})", when));
            }
            entry
        })
        .collect();

    let skill_names: Vec<Value> = skills
        .iter()
        .map(|s| Value::String(s.name.clone()))
        .collect();

    let description = format!(
        "Execute a specialized skill. Each skill provides domain-specific \
         instructions that guide your behavior for the task.\n\n\
         Available skills:\n{}",
        skill_entries.join("\n")
    );

    serde_json::json!({
        "type": "function",
        "function": {
            "name": SKILL_TOOL_NAME,
            "description": description,
            "parameters": {
                "type": "object",
                "required": ["skill_name"],
                "properties": {
                    "skill_name": {
                        "type": "string",
                        "enum": skill_names,
                        "description": "The name of the skill to execute."
                    },
                    "task": {
                        "type": "string",
                        "description": "Optional task description or additional context for the skill. If omitted, the skill uses the current conversation context."
                    }
                }
            }
        }
    })
}

/// Check if a tool call is a skill invocation.
pub fn is_skill_call(tool_call: &Value) -> bool {
    tool_call
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
        == Some(SKILL_TOOL_NAME)
}

// ─── Skill execution ─────────────────────────────────────────────────────────

/// Activation effects from a skill invocation.
///
/// Returned alongside tool results so the agentic loop can apply
/// model overrides and tool restrictions to subsequent turns.
#[derive(Clone, Debug, Default)]
pub struct SkillActivation {
    /// Model override for subsequent turns (e.g. `"claude-sonnet-4-20250514"`).
    pub model_override: Option<String>,
    /// Tool allow-list — only these tools should be available.
    /// Empty means no restriction (all tools allowed).
    pub allowed_tools: Vec<String>,
}

/// Partition tool calls into skill calls and regular calls, executing skills
/// via the resolver.
///
/// Returns `(skill_results, remaining_tool_calls, activation)` where:
/// - `skill_results`: `(tool_call_id, output_text)` pairs
/// - `remaining_tool_calls`: non-skill tool calls passed through
/// - `activation`: optional model/tool overrides from the last skill invoked
pub async fn partition_and_execute_skills(
    tool_calls: &[Value],
    resolver: &dyn SkillResolver,
) -> (Vec<(String, String)>, Vec<Value>, Option<SkillActivation>) {
    let mut skill_results = Vec::new();
    let mut remaining = Vec::new();
    let mut activation: Option<SkillActivation> = None;

    for tc in tool_calls {
        if !is_skill_call(tc) {
            remaining.push(tc.clone());
            continue;
        }

        let call_id = tc
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let args_str = tc
            .get("function")
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .unwrap_or("{}");

        let result = match serde_json::from_str::<Value>(args_str) {
            Ok(args) => {
                let skill_name = args.get("skill_name").and_then(Value::as_str).unwrap_or("");

                let task_hint = args.get("task").and_then(Value::as_str).unwrap_or("");

                let (text, act) = execute_skill(resolver, skill_name, task_hint);
                if let Some(a) = act {
                    activation = Some(a);
                }
                text
            }
            Err(e) => format!("Invalid skill arguments: {e}"),
        };

        skill_results.push((call_id, result));
    }

    (skill_results, remaining, activation)
}

/// Execute a single skill call and return the output text + activation metadata.
fn execute_skill(
    resolver: &dyn SkillResolver,
    skill_name: &str,
    task_hint: &str,
) -> (String, Option<SkillActivation>) {
    match resolver.resolve(skill_name) {
        Ok(skill) => {
            let mut output = format!(
                "# Skill: {}\n\n\
                 You are now executing the **{}** skill. \
                 Follow the instructions below carefully.\n\n\
                 ---\n\n\
                 {}",
                skill.name, skill.name, skill.instructions
            );

            if !task_hint.is_empty() {
                output.push_str(&format!("\n\n---\n\n**Task context:** {}", task_hint));
            }

            if !skill.allowed_tools.is_empty() {
                output.push_str(&format!(
                    "\n\n**Allowed tools for this skill:** {}",
                    skill.allowed_tools.join(", ")
                ));
            }

            let activation = SkillActivation {
                model_override: skill.model,
                allowed_tools: skill.allowed_tools,
            };

            // Only return activation if it has any effects
            let activation =
                if activation.model_override.is_some() || !activation.allowed_tools.is_empty() {
                    Some(activation)
                } else {
                    None
                };

            (output, activation)
        }
        Err(e) => (
            format!("Failed to load skill '{}': {}", skill_name, e),
            None,
        ),
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Stub resolver for tests.
    struct StubResolver {
        skills: Vec<(String, String, String)>, // (name, description, instructions)
    }

    impl SkillResolver for StubResolver {
        fn resolve(&self, name: &str) -> Result<ResolvedSkill, String> {
            self.skills
                .iter()
                .find(|(n, _, _)| n == name)
                .map(|(n, _, inst)| ResolvedSkill {
                    name: n.clone(),
                    instructions: inst.clone(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec![],
                })
                .ok_or_else(|| format!("Unknown skill: {name}"))
        }

        fn available_skills(&self) -> Vec<SkillToolInfo> {
            self.skills
                .iter()
                .map(|(n, d, _)| SkillToolInfo {
                    name: n.clone(),
                    description: d.clone(),
                    when_to_use: None,
                })
                .collect()
        }
    }

    fn stub_resolver() -> StubResolver {
        StubResolver {
            skills: vec![
                (
                    "code-review".into(),
                    "Review code for bugs and best practices".into(),
                    "Check for bugs, security issues, and style.".into(),
                ),
                (
                    "test-writer".into(),
                    "Generate unit tests".into(),
                    "Write comprehensive unit tests with edge cases.".into(),
                ),
            ],
        }
    }

    #[test]
    fn schema_has_correct_structure() {
        let resolver = stub_resolver();
        let skills = resolver.available_skills();
        let schema = skill_tool_schema(&skills);

        assert_eq!(schema["function"]["name"], SKILL_TOOL_NAME);
        let params = &schema["function"]["parameters"];
        assert_eq!(params["type"], "object");

        let skill_enum = &params["properties"]["skill_name"]["enum"];
        assert!(skill_enum.is_array());
        let names: Vec<&str> = skill_enum
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["code-review", "test-writer"]);
    }

    #[test]
    fn schema_empty_when_no_skills() {
        let schema = skill_tool_schema(&[]);
        let skill_enum = &schema["function"]["parameters"]["properties"]["skill_name"]["enum"];
        assert_eq!(skill_enum.as_array().unwrap().len(), 0);
    }

    #[test]
    fn is_skill_call_detects_skill_tool() {
        let skill = serde_json::json!({
            "id": "call_1",
            "function": {
                "name": "skill",
                "arguments": "{\"skill_name\": \"code-review\"}"
            }
        });
        let non_skill = serde_json::json!({
            "id": "call_2",
            "function": {
                "name": "bash",
                "arguments": "{\"command\": \"ls\"}"
            }
        });
        assert!(is_skill_call(&skill));
        assert!(!is_skill_call(&non_skill));
    }

    #[test]
    fn is_skill_call_rejects_missing_function() {
        let malformed = serde_json::json!({"id": "x"});
        assert!(!is_skill_call(&malformed));
    }

    #[test]
    fn execute_skill_returns_instructions() {
        let resolver = stub_resolver();
        let (output, activation) = execute_skill(&resolver, "code-review", "");
        assert!(output.contains("# Skill: code-review"));
        assert!(output.contains("Check for bugs, security issues, and style."));
        // No model/tools override in stub → no activation
        assert!(activation.is_none());
    }

    #[test]
    fn execute_skill_includes_task_hint() {
        let resolver = stub_resolver();
        let (output, _) = execute_skill(&resolver, "code-review", "Review auth module");
        assert!(output.contains("**Task context:** Review auth module"));
    }

    #[test]
    fn execute_skill_unknown_name() {
        let resolver = stub_resolver();
        let (output, activation) = execute_skill(&resolver, "nonexistent", "");
        assert!(output.contains("Failed to load skill 'nonexistent'"));
        assert!(activation.is_none());
    }

    #[tokio::test]
    async fn partition_separates_skill_and_regular_calls() {
        let resolver = stub_resolver();
        let tool_calls = vec![
            serde_json::json!({
                "id": "call_1",
                "function": {
                    "name": "skill",
                    "arguments": "{\"skill_name\": \"code-review\"}"
                }
            }),
            serde_json::json!({
                "id": "call_2",
                "function": {
                    "name": "bash",
                    "arguments": "{\"command\": \"ls\"}"
                }
            }),
            serde_json::json!({
                "id": "call_3",
                "function": {
                    "name": "skill",
                    "arguments": "{\"skill_name\": \"test-writer\"}"
                }
            }),
        ];

        let (skill_results, remaining, _activation) =
            partition_and_execute_skills(&tool_calls, &resolver).await;

        assert_eq!(skill_results.len(), 2);
        assert_eq!(remaining.len(), 1);

        assert_eq!(skill_results[0].0, "call_1");
        assert!(skill_results[0].1.contains("code-review"));

        assert_eq!(skill_results[1].0, "call_3");
        assert!(skill_results[1].1.contains("test-writer"));

        assert_eq!(remaining[0]["function"]["name"], "bash");
    }

    #[tokio::test]
    async fn partition_handles_invalid_arguments() {
        let resolver = stub_resolver();
        let tool_calls = vec![serde_json::json!({
            "id": "call_bad",
            "function": {
                "name": "skill",
                "arguments": "not valid json"
            }
        })];

        let (results, remaining, _) = partition_and_execute_skills(&tool_calls, &resolver).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].1.contains("Invalid skill arguments"));
        assert_eq!(remaining.len(), 0);
    }

    #[test]
    fn schema_includes_when_to_use() {
        let skills = vec![SkillToolInfo {
            name: "deployer".into(),
            description: "Deploy services".into(),
            when_to_use: Some("when user asks to deploy".into()),
        }];
        let schema = skill_tool_schema(&skills);
        let desc = schema["function"]["description"].as_str().unwrap();
        assert!(desc.contains("when user asks to deploy"));
    }

    #[test]
    fn execute_skill_shows_allowed_tools() {
        struct ToolRestrictedResolver;
        impl SkillResolver for ToolRestrictedResolver {
            fn resolve(&self, _name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: "restricted".into(),
                    instructions: "Do the thing.".into(),
                    model: None,
                    max_tokens: None,
                    allowed_tools: vec!["bash".into(), "read_file".into()],
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let (output, activation) = execute_skill(&ToolRestrictedResolver, "restricted", "");
        assert!(output.contains("**Allowed tools for this skill:** bash, read_file"));
        // allowed_tools set → activation returned
        let act = activation.unwrap();
        assert_eq!(act.allowed_tools, vec!["bash", "read_file"]);
        assert!(act.model_override.is_none());
    }

    #[test]
    fn execute_skill_returns_activation_with_model() {
        struct ModelOverrideResolver;
        impl SkillResolver for ModelOverrideResolver {
            fn resolve(&self, _name: &str) -> Result<ResolvedSkill, String> {
                Ok(ResolvedSkill {
                    name: "fancy".into(),
                    instructions: "Be fancy.".into(),
                    model: Some("gpt-4o".into()),
                    max_tokens: Some(4096),
                    allowed_tools: vec!["bash".into()],
                })
            }
            fn available_skills(&self) -> Vec<SkillToolInfo> {
                vec![]
            }
        }

        let (_, activation) = execute_skill(&ModelOverrideResolver, "fancy", "");
        let act = activation.unwrap();
        assert_eq!(act.model_override.as_deref(), Some("gpt-4o"));
        assert_eq!(act.allowed_tools, vec!["bash"]);
    }

    #[test]
    fn execute_skill_no_activation_when_no_overrides() {
        let resolver = stub_resolver();
        let (_, activation) = execute_skill(&resolver, "code-review", "");
        assert!(activation.is_none());
    }
}
