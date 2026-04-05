//! Skill lifecycle hooks — pre/post invocation actions.
//!
//! Inspired by Claude Code's hooks system where skills can declare shell commands,
//! environment variable changes, or custom actions that run before/after execution.

use serde::{Deserialize, Serialize};

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
}
