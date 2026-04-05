//! Skill lifecycle hooks and tool event hooks.
//!
//! Two hook systems coexist:
//!
//! 1. **Skill lifecycle hooks** (`SkillHooks`) — pre/post invocation of a skill itself.
//! 2. **Tool event hooks** (`ToolEventHook`) — fire on any tool call matching a pattern,
//!    inspired by Claude Code's PreToolUse / PostToolUse system.

use serde::{Deserialize, Serialize};

// ── Skill lifecycle hooks (existing) ─────────────────────────────────────

/// An action to execute as part of a hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookAction {
    /// Run a shell command.
    Shell { command: String },
    /// Set an environment variable.
    SetEnv { key: String, value: String },
    /// Custom hook identifier (for extensibility).
    Custom {
        id: String,
        config: Option<serde_json::Value>,
    },
}

/// Lifecycle hooks for a skill.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillHooks {
    /// Actions to run before skill invocation.
    #[serde(default)]
    pub pre_invoke: Vec<HookAction>,
    /// Actions to run after successful skill completion.
    #[serde(default)]
    pub post_invoke: Vec<HookAction>,
    /// Actions to run when skill execution fails.
    #[serde(default)]
    pub on_error: Vec<HookAction>,
}

impl SkillHooks {
    pub fn is_empty(&self) -> bool {
        self.pre_invoke.is_empty() && self.post_invoke.is_empty() && self.on_error.is_empty()
    }
}

// ── Tool event hooks (CC-inspired) ──────────────────────────────────────

/// When in the tool lifecycle the hook fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolEventKind {
    /// Before the tool executes. Can block or inject context.
    PreToolUse,
    /// After the tool completes successfully. Can append context.
    PostToolUse,
}

/// Outcome of a pre-tool-use hook evaluation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreToolDecision {
    /// Allow the tool call to proceed.
    Allow,
    /// Allow, but inject additional context into the tool output.
    AllowWithContext(String),
    /// Block the tool call with a reason.
    Block(String),
}

/// A single tool event hook configuration.
///
/// Configured in project settings (`.astra/hooks.json`) or skill frontmatter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEventHook {
    /// Which event triggers this hook.
    pub event: ToolEventKind,
    /// Tool name matcher — glob-style pattern (e.g. `"bash"`, `"read_*"`, `"*"`).
    /// If empty or `"*"`, matches all tools.
    #[serde(default)]
    pub matcher: String,
    /// The action to execute when the hook fires.
    pub action: HookAction,
    /// Optional timeout in seconds for shell actions (default: 10).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u32,
}

fn default_hook_timeout() -> u32 {
    10
}

impl ToolEventHook {
    /// Check if this hook's matcher matches the given tool name.
    pub fn matches_tool(&self, tool_name: &str) -> bool {
        let pattern = self.matcher.trim();
        if pattern.is_empty() || pattern == "*" {
            return true;
        }
        glob_match(pattern, tool_name)
    }
}

/// Simple glob matching: `*` matches any sequence, `?` matches one char.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star_pi, mut star_ti) = (usize::MAX, 0);

    while ti < txt.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == txt[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star_pi = pi;
            star_ti = ti;
            pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

/// A collection of tool event hooks with efficient lookup.
#[derive(Clone, Debug, Default)]
pub struct ToolEventHookRegistry {
    hooks: Vec<ToolEventHook>,
}

impl ToolEventHookRegistry {
    pub fn new(hooks: Vec<ToolEventHook>) -> Self {
        Self { hooks }
    }

    /// Return all hooks that match the given event kind and tool name.
    pub fn matching(&self, event: ToolEventKind, tool_name: &str) -> Vec<&ToolEventHook> {
        self.hooks
            .iter()
            .filter(|h| h.event == event && h.matches_tool(tool_name))
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.hooks.len()
    }
}

/// Events emitted during the skill lifecycle.
///
/// Can be consumed by telemetry, logging, or debugging systems.
#[derive(Clone, Debug)]
pub enum SkillLifecycleEvent {
    /// A skill was discovered from a source.
    Discovered {
        name: String,
        source: super::manifest::SkillSourceKind,
    },
    /// A skill's instructions were fully loaded.
    Loaded { name: String },
    /// A conditional skill was activated by a path match.
    Activated { name: String, trigger: String },
    /// A skill invocation started.
    Invoked {
        name: String,
        context: super::manifest::ExecutionContext,
    },
    /// A skill invocation completed successfully.
    Completed {
        name: String,
        tokens_used: u32,
        turns: u32,
    },
    /// A skill invocation failed.
    Failed { name: String, error: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Skill lifecycle hook tests ──────────────────────────────────────

    #[test]
    fn hooks_empty_check() {
        let h = SkillHooks::default();
        assert!(h.is_empty());

        let h = SkillHooks {
            pre_invoke: vec![HookAction::Shell {
                command: "echo test".into(),
            }],
            ..Default::default()
        };
        assert!(!h.is_empty());
    }

    #[test]
    fn hook_action_serde_roundtrip() {
        let actions = vec![
            HookAction::Shell {
                command: "echo hello".into(),
            },
            HookAction::SetEnv {
                key: "FOO".into(),
                value: "bar".into(),
            },
            HookAction::Custom {
                id: "my-hook".into(),
                config: Some(serde_json::json!({"key": "value"})),
            },
        ];

        let json = serde_json::to_string(&actions).unwrap();
        let parsed: Vec<HookAction> = serde_json::from_str(&json).unwrap();
        assert_eq!(actions, parsed);
    }

    #[test]
    fn hooks_all_phases_non_empty() {
        let h = SkillHooks {
            pre_invoke: vec![HookAction::Shell {
                command: "before".into(),
            }],
            post_invoke: vec![HookAction::SetEnv {
                key: "K".into(),
                value: "V".into(),
            }],
            on_error: vec![HookAction::Custom {
                id: "cleanup".into(),
                config: None,
            }],
        };
        assert!(!h.is_empty());
    }

    #[test]
    fn hooks_only_on_error_non_empty() {
        let h = SkillHooks {
            on_error: vec![HookAction::Shell {
                command: "notify".into(),
            }],
            ..Default::default()
        };
        assert!(!h.is_empty());
    }

    #[test]
    fn hook_action_shell_deserialize_from_json() {
        let json = r#"{"type": "shell", "command": "echo test"}"#;
        let action: HookAction = serde_json::from_str(json).unwrap();
        assert_eq!(
            action,
            HookAction::Shell {
                command: "echo test".into()
            }
        );
    }

    #[test]
    fn hook_action_set_env_deserialize_from_json() {
        let json = r#"{"type": "set_env", "key": "PATH", "value": "/usr/bin"}"#;
        let action: HookAction = serde_json::from_str(json).unwrap();
        assert_eq!(
            action,
            HookAction::SetEnv {
                key: "PATH".into(),
                value: "/usr/bin".into()
            }
        );
    }

    #[test]
    fn hook_action_custom_no_config() {
        let json = r#"{"type": "custom", "id": "webhook"}"#;
        let action: HookAction = serde_json::from_str(json).unwrap();
        assert_eq!(
            action,
            HookAction::Custom {
                id: "webhook".into(),
                config: None
            }
        );
    }

    #[test]
    fn skill_hooks_yaml_roundtrip() {
        let hooks = SkillHooks {
            pre_invoke: vec![
                HookAction::Shell {
                    command: "make lint".into(),
                },
                HookAction::SetEnv {
                    key: "SKILL_ACTIVE".into(),
                    value: "1".into(),
                },
            ],
            post_invoke: vec![HookAction::Shell {
                command: "echo done".into(),
            }],
            on_error: vec![],
        };

        let yaml = serde_yaml::to_string(&hooks).unwrap();
        let parsed: SkillHooks = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(hooks, parsed);
    }

    #[test]
    fn lifecycle_event_variants_constructible() {
        let events = vec![
            SkillLifecycleEvent::Discovered {
                name: "test".into(),
                source: super::super::manifest::SkillSourceKind::Local,
            },
            SkillLifecycleEvent::Loaded {
                name: "test".into(),
            },
            SkillLifecycleEvent::Activated {
                name: "test".into(),
                trigger: "src/main.rs".into(),
            },
            SkillLifecycleEvent::Invoked {
                name: "test".into(),
                context: super::super::manifest::ExecutionContext::Inline,
            },
            SkillLifecycleEvent::Completed {
                name: "test".into(),
                tokens_used: 1000,
                turns: 3,
            },
            SkillLifecycleEvent::Failed {
                name: "test".into(),
                error: "timeout".into(),
            },
        ];
        assert_eq!(events.len(), 6);
    }

    // ── Glob matching tests ─────────────────────────────────────────────

    #[test]
    fn glob_exact_match() {
        assert!(glob_match("bash", "bash"));
        assert!(!glob_match("bash", "read_file"));
    }

    #[test]
    fn glob_wildcard_star() {
        assert!(glob_match("read_*", "read_file"));
        assert!(glob_match("read_*", "read_dir"));
        assert!(!glob_match("read_*", "write_file"));
    }

    #[test]
    fn glob_wildcard_question() {
        assert!(glob_match("git_?", "git_a"));
        assert!(!glob_match("git_?", "git_ab"));
    }

    #[test]
    fn glob_star_matches_all() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn glob_complex_pattern() {
        assert!(glob_match("*file*", "read_file_contents"));
        assert!(glob_match("git_*_*", "git_log_search"));
        assert!(!glob_match("git_*_*", "git_status"));
    }

    // ── Tool event hook tests ───────────────────────────────────────────

    #[test]
    fn tool_event_hook_matches_exact() {
        let hook = ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: "check".into(),
            },
            timeout_secs: 10,
        };
        assert!(hook.matches_tool("bash"));
        assert!(!hook.matches_tool("read_file"));
    }

    #[test]
    fn tool_event_hook_matches_glob() {
        let hook = ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "write_*".into(),
            action: HookAction::Shell {
                command: "lint".into(),
            },
            timeout_secs: 10,
        };
        assert!(hook.matches_tool("write_file"));
        assert!(hook.matches_tool("write_new_file"));
        assert!(!hook.matches_tool("read_file"));
    }

    #[test]
    fn tool_event_hook_empty_matcher_matches_all() {
        let hook = ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: String::new(),
            action: HookAction::Shell {
                command: "log".into(),
            },
            timeout_secs: 10,
        };
        assert!(hook.matches_tool("bash"));
        assert!(hook.matches_tool("read_file"));
    }

    #[test]
    fn tool_event_hook_serde_roundtrip() {
        let hook = ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: "echo pre".into(),
            },
            timeout_secs: 5,
        };
        let json = serde_json::to_string(&hook).unwrap();
        let parsed: ToolEventHook = serde_json::from_str(&json).unwrap();
        assert_eq!(hook, parsed);
    }

    #[test]
    fn registry_matching_filters_by_event_and_tool() {
        let registry = ToolEventHookRegistry::new(vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "pre-bash".into(),
                },
                timeout_secs: 10,
            },
            ToolEventHook {
                event: ToolEventKind::PostToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "post-bash".into(),
                },
                timeout_secs: 10,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "read_*".into(),
                action: HookAction::Shell {
                    command: "pre-read".into(),
                },
                timeout_secs: 10,
            },
        ]);
        assert_eq!(registry.len(), 3);

        let pre_bash = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(pre_bash.len(), 1);
        assert_eq!(
            pre_bash[0].action,
            HookAction::Shell {
                command: "pre-bash".into()
            }
        );

        let post_bash = registry.matching(ToolEventKind::PostToolUse, "bash");
        assert_eq!(post_bash.len(), 1);

        let pre_read = registry.matching(ToolEventKind::PreToolUse, "read_file");
        assert_eq!(pre_read.len(), 1);

        let pre_write = registry.matching(ToolEventKind::PreToolUse, "write_file");
        assert!(pre_write.is_empty());
    }

    #[test]
    fn registry_empty_returns_no_matches() {
        let registry = ToolEventHookRegistry::default();
        assert!(registry.is_empty());
        assert!(registry
            .matching(ToolEventKind::PreToolUse, "bash")
            .is_empty());
    }

    #[test]
    fn pre_tool_decision_variants() {
        let allow = PreToolDecision::Allow;
        let ctx = PreToolDecision::AllowWithContext("extra info".into());
        let block = PreToolDecision::Block("denied".into());

        assert_eq!(allow, PreToolDecision::Allow);
        assert_eq!(
            ctx,
            PreToolDecision::AllowWithContext("extra info".into())
        );
        assert_eq!(block, PreToolDecision::Block("denied".into()));
    }

    // ── E2E: full hook pipeline ─────────────────────────────────────────

    #[test]
    fn e2e_hooks_json_config_to_registry_to_decisions() {
        // Simulates the full hook lifecycle:
        // 1. Load hooks from JSON config
        // 2. Build registry
        // 3. Match against tool calls
        // 4. Produce pre-tool decisions

        // Step 1: Parse hook config from JSON (as would come from .astra/hooks.json)
        let config_json = r#"[
            {
                "event": "pre_tool_use",
                "matcher": "bash",
                "action": {"type": "shell", "command": "echo 'checking bash command'"},
                "timeout_secs": 5
            },
            {
                "event": "post_tool_use",
                "matcher": "write_*",
                "action": {"type": "shell", "command": "make lint"},
                "timeout_secs": 30
            },
            {
                "event": "pre_tool_use",
                "matcher": "*",
                "action": {"type": "custom", "id": "audit_log"},
                "timeout_secs": 2
            }
        ]"#;
        let hooks: Vec<ToolEventHook> = serde_json::from_str(config_json).unwrap();
        assert_eq!(hooks.len(), 3);

        // Step 2: Build registry
        let registry = ToolEventHookRegistry::new(hooks);
        assert_eq!(registry.len(), 3);

        // Step 3: Match against various tool calls
        // bash: should match both the "bash" hook and the "*" hook
        let pre_bash = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(pre_bash.len(), 2);
        assert_eq!(pre_bash[0].matcher, "bash");
        assert_eq!(pre_bash[1].matcher, "*");

        // write_file: should match post_tool_use "write_*" and pre_tool_use "*"
        let post_write = registry.matching(ToolEventKind::PostToolUse, "write_file");
        assert_eq!(post_write.len(), 1);
        assert_eq!(post_write[0].matcher, "write_*");

        let pre_write = registry.matching(ToolEventKind::PreToolUse, "write_file");
        assert_eq!(pre_write.len(), 1); // only the "*" catch-all
        assert_eq!(pre_write[0].matcher, "*");

        // read_file: only the catch-all "*" for pre_tool_use
        let pre_read = registry.matching(ToolEventKind::PreToolUse, "read_file");
        assert_eq!(pre_read.len(), 1);

        // no post_tool_use hooks for read_file
        let post_read = registry.matching(ToolEventKind::PostToolUse, "read_file");
        assert!(post_read.is_empty());

        // Step 4: Verify decision flow
        // Simulate: a pre-tool hook returns "block" for dangerous bash command
        let bash_hooks = registry.matching(ToolEventKind::PreToolUse, "bash");
        let first_hook = bash_hooks[0];
        match &first_hook.action {
            HookAction::Shell { command } => {
                assert!(command.contains("checking bash"));
                // In a real system: run the shell command, parse its JSON output
                // to get allow/block decision. Here we verify the config is correct.
            }
            _ => panic!("expected shell action"),
        }

        // The catch-all audit hook should be a Custom action
        let audit_hook = bash_hooks[1];
        match &audit_hook.action {
            HookAction::Custom { id, .. } => assert_eq!(id, "audit_log"),
            _ => panic!("expected custom action"),
        }
    }

    #[test]
    fn e2e_hooks_yaml_config_roundtrip() {
        let hooks = vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "validate-command.sh".into(),
                },
                timeout_secs: 10,
            },
            ToolEventHook {
                event: ToolEventKind::PostToolUse,
                matcher: "write_*".into(),
                action: HookAction::Shell {
                    command: "run-linter.sh".into(),
                },
                timeout_secs: 30,
            },
        ];

        // Roundtrip through YAML (skill frontmatter format)
        let yaml = serde_yaml::to_string(&hooks).unwrap();
        let parsed: Vec<ToolEventHook> = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(hooks, parsed);

        // Roundtrip through JSON (project config format)
        let json = serde_json::to_string_pretty(&hooks).unwrap();
        let parsed: Vec<ToolEventHook> = serde_json::from_str(&json).unwrap();
        assert_eq!(hooks, parsed);
    }

    #[test]
    fn e2e_multiple_matchers_priority_order() {
        // Hooks are evaluated in config order — most specific first, catch-all last
        let registry = ToolEventHookRegistry::new(vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bash".into(),
                action: HookAction::Shell {
                    command: "specific-bash-check".into(),
                },
                timeout_secs: 5,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "bas*".into(),
                action: HookAction::Shell {
                    command: "glob-bash-check".into(),
                },
                timeout_secs: 5,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "*".into(),
                action: HookAction::Shell {
                    command: "catch-all".into(),
                },
                timeout_secs: 5,
            },
        ]);

        let matches = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(matches.len(), 3);
        // Order preserved from config
        match &matches[0].action {
            HookAction::Shell { command } => assert_eq!(command, "specific-bash-check"),
            _ => panic!(),
        }
        match &matches[1].action {
            HookAction::Shell { command } => assert_eq!(command, "glob-bash-check"),
            _ => panic!(),
        }
        match &matches[2].action {
            HookAction::Shell { command } => assert_eq!(command, "catch-all"),
            _ => panic!(),
        }
    }
}
