//! Built-in agent type definitions.

use std::collections::HashSet;
use std::str::FromStr;

/// A built-in agent type identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinAgentType {
    Explore,
    CodeReview,
    Task,
    GeneralPurpose,
}

impl FromStr for BuiltinAgentType {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "explore" => Ok(Self::Explore),
            "code-review" | "code_review" | "codereview" => Ok(Self::CodeReview),
            "task" => Ok(Self::Task),
            "general-purpose" | "general_purpose" | "generalpurpose" | "general" => {
                Ok(Self::GeneralPurpose)
            }
            _ => Err(()),
        }
    }
}

impl BuiltinAgentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Explore => "explore",
            Self::CodeReview => "code-review",
            Self::Task => "task",
            Self::GeneralPurpose => "general-purpose",
        }
    }
}

impl std::fmt::Display for BuiltinAgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Definition of an agent type including its capabilities and constraints.
#[derive(Debug, Clone)]
pub struct AgentTypeDefinition {
    pub agent_type: String,
    pub description: String,
    pub system_prompt_addendum: String,
    pub default_model: String,
    pub max_turns: u32,
    pub allowed_tools: HashSet<String>,
    pub read_only: bool,
}

impl AgentTypeDefinition {
    pub fn is_tool_allowed(&self, tool_name: &str) -> bool {
        self.allowed_tools.contains("*") || self.allowed_tools.contains(tool_name)
    }
}

/// Get all built-in agent type definitions.
pub fn get_builtin_agent_types() -> Vec<AgentTypeDefinition> {
    vec![
        AgentTypeDefinition {
            agent_type: "explore".to_string(),
            description: "Fast codebase exploration and research.".to_string(),
            system_prompt_addendum: EXPLORE_PROMPT.to_string(),
            default_model: "claude-haiku".to_string(),
            max_turns: 20,
            allowed_tools: ["view", "glob", "grep", "bash"]
                .into_iter()
                .map(String::from)
                .collect(),
            read_only: true,
        },
        AgentTypeDefinition {
            agent_type: "code-review".to_string(),
            description: "Review code changes with high signal-to-noise ratio.".to_string(),
            system_prompt_addendum: CODE_REVIEW_PROMPT.to_string(),
            default_model: "claude-sonnet".to_string(),
            max_turns: 15,
            allowed_tools: ["view", "glob", "grep", "bash"]
                .into_iter()
                .map(String::from)
                .collect(),
            read_only: true,
        },
        AgentTypeDefinition {
            agent_type: "task".to_string(),
            description: "Execute commands with verbose output tracking.".to_string(),
            system_prompt_addendum: TASK_PROMPT.to_string(),
            default_model: "claude-haiku".to_string(),
            max_turns: 30,
            allowed_tools: ["bash", "view", "glob", "grep", "edit", "create"]
                .into_iter()
                .map(String::from)
                .collect(),
            read_only: false,
        },
        AgentTypeDefinition {
            agent_type: "general-purpose".to_string(),
            description: "Full-capability agent for complex multi-step tasks.".to_string(),
            system_prompt_addendum: String::new(),
            default_model: "claude-sonnet".to_string(),
            max_turns: 60,
            allowed_tools: ["*"].into_iter().map(String::from).collect(),
            read_only: false,
        },
    ]
}

/// Get the definition for a specific agent type.
pub fn get_agent_type_definition(agent_type: &str) -> Option<AgentTypeDefinition> {
    get_builtin_agent_types()
        .into_iter()
        .find(|def| def.agent_type == agent_type)
}

const EXPLORE_PROMPT: &str = r#"
You are an exploration agent focused on understanding codebases quickly.
- Search and navigate code to answer questions
- Report findings with file paths and line numbers
- You are READ-ONLY: do not modify any files
"#;

const CODE_REVIEW_PROMPT: &str = r#"
You are a code review agent with high signal-to-noise ratio.
- Only flag issues that genuinely matter: bugs, security, logic errors
- NEVER comment on style or formatting
- You are READ-ONLY: do not modify any files
"#;

const TASK_PROMPT: &str = r#"
You are a task execution agent focused on running commands reliably.
- Execute the given task using bash commands
- On success: brief summary
- On failure: include relevant error output
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_agent_type_from_str() {
        assert_eq!(
            "explore".parse::<BuiltinAgentType>(),
            Ok(BuiltinAgentType::Explore)
        );
        assert_eq!(
            "code-review".parse::<BuiltinAgentType>(),
            Ok(BuiltinAgentType::CodeReview)
        );
        assert!("unknown".parse::<BuiltinAgentType>().is_err());
    }

    #[test]
    fn test_get_builtin_agent_types() {
        let types = get_builtin_agent_types();
        assert_eq!(types.len(), 4);
    }
}
