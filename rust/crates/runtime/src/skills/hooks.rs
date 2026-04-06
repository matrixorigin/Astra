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

// ── Config loading ──────────────────────────────────────────────────────────

/// File names to search for in `.astra/` directory.
const HOOK_CONFIG_CANDIDATES: &[&str] = &["hooks.json", "hooks.yaml", "hooks.yml"];

/// Load tool event hooks from the project's `.astra/` directory.
///
/// Searches for `hooks.json`, `hooks.yaml`, or `hooks.yml` under
/// `<project_root>/.astra/`. Returns an empty registry if no file is found
/// or if parsing fails (with a warning log).
pub fn load_tool_event_hooks(project_root: &std::path::Path) -> ToolEventHookRegistry {
    let astra_dir = project_root.join(".astra");
    if !astra_dir.is_dir() {
        return ToolEventHookRegistry::default();
    }

    for candidate in HOOK_CONFIG_CANDIDATES {
        let path = astra_dir.join(candidate);
        if path.is_file() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    let hooks = if candidate.ends_with(".json") {
                        parse_hooks_json(&content, &path)
                    } else {
                        parse_hooks_yaml(&content, &path)
                    };
                    if !hooks.is_empty() {
                        astra_core::agent_warn!(
                            "hook",
                            "Loaded {} tool event hooks from {}",
                            hooks.len(),
                            path.display()
                        );
                    }
                    return ToolEventHookRegistry::new(hooks);
                }
                Err(e) => {
                    astra_core::agent_warn!(
                        "hook",
                        "Failed to read {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    ToolEventHookRegistry::default()
}

/// JSON format: top-level array of ToolEventHook objects, or an object with a "hooks" key.
fn parse_hooks_json(content: &str, path: &std::path::Path) -> Vec<ToolEventHook> {
    // Try direct array first
    if let Ok(hooks) = serde_json::from_str::<Vec<ToolEventHook>>(content) {
        return hooks;
    }

    // Try { "hooks": [...] } wrapper
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        hooks: Vec<ToolEventHook>,
    }
    if let Ok(w) = serde_json::from_str::<Wrapper>(content) {
        return w.hooks;
    }

    astra_core::agent_warn!(
        "hook",
        "Failed to parse {}: expected JSON array or {{\"hooks\": [...]}}",
        path.display()
    );
    Vec::new()
}

/// YAML format: same as JSON — top-level list or `hooks:` key.
fn parse_hooks_yaml(content: &str, path: &std::path::Path) -> Vec<ToolEventHook> {
    // Try direct array
    if let Ok(hooks) = serde_yaml::from_str::<Vec<ToolEventHook>>(content) {
        return hooks;
    }

    // Try wrapper
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default)]
        hooks: Vec<ToolEventHook>,
    }
    if let Ok(w) = serde_yaml::from_str::<Wrapper>(content) {
        return w.hooks;
    }

    astra_core::agent_warn!(
        "hook",
        "Failed to parse {}: expected YAML list or `hooks:` mapping",
        path.display()
    );
    Vec::new()
}

// ── Hook execution ──────────────────────────────────────────────────────────

/// Execute all PreToolUse hooks matching a tool name.
///
/// Returns the aggregate decision:
/// - If any hook returns Block, the tool is blocked.
/// - If any hook returns AllowWithContext, the context is appended.
/// - Otherwise, Allow.
///
/// Shell hooks receive tool info via stdin JSON and produce a JSON decision:
/// ```json
/// {"decision": "allow"}
/// {"decision": "block", "reason": "dangerous command"}
/// {"decision": "allow", "context": "extra info to append"}
/// ```
pub async fn evaluate_pre_tool_hooks(
    registry: &ToolEventHookRegistry,
    tool_name: &str,
    tool_args: &serde_json::Value,
) -> PreToolDecision {
    let hooks = registry.matching(ToolEventKind::PreToolUse, tool_name);
    if hooks.is_empty() {
        return PreToolDecision::Allow;
    }

    let mut accumulated_context: Vec<String> = Vec::new();

    for hook in hooks {
        match &hook.action {
            HookAction::Shell { command } => {
                let decision = run_shell_pre_hook(
                    command,
                    tool_name,
                    tool_args,
                    hook.timeout_secs,
                )
                .await;
                match decision {
                    PreToolDecision::Block(reason) => return PreToolDecision::Block(reason),
                    PreToolDecision::AllowWithContext(ctx) => accumulated_context.push(ctx),
                    PreToolDecision::Allow => {}
                }
            }
            HookAction::SetEnv { key, value } => {
                // Safety: only used in single-threaded test/CLI contexts
                unsafe { std::env::set_var(key, value) };
            }
            HookAction::Custom { id, .. } => {
                astra_core::agent_warn!("hook", "Custom hook '{}' matched tool '{}' — not yet implemented", id, tool_name);
            }
        }
    }

    if accumulated_context.is_empty() {
        PreToolDecision::Allow
    } else {
        PreToolDecision::AllowWithContext(accumulated_context.join("\n"))
    }
}

/// Execute all PostToolUse hooks matching a tool name.
///
/// Returns modified output if any hook changed it, otherwise None.
pub async fn evaluate_post_tool_hooks(
    registry: &ToolEventHookRegistry,
    tool_name: &str,
    tool_args: &serde_json::Value,
    tool_output: &str,
) -> Option<String> {
    let hooks = registry.matching(ToolEventKind::PostToolUse, tool_name);
    if hooks.is_empty() {
        return None;
    }

    let mut current_output = tool_output.to_string();

    for hook in hooks {
        match &hook.action {
            HookAction::Shell { command } => {
                if let Some(modified) = run_shell_post_hook(
                    command,
                    tool_name,
                    tool_args,
                    &current_output,
                    hook.timeout_secs,
                )
                .await
                {
                    current_output = modified;
                }
            }
            HookAction::Custom { id, .. } => {
                astra_core::agent_warn!("hook", "PostToolUse custom hook '{}' for '{}' — not yet implemented", id, tool_name);
            }
            _ => {}
        }
    }

    if current_output != tool_output {
        Some(current_output)
    } else {
        None
    }
}

/// Run a shell command for a PreToolUse hook, with timeout.
async fn run_shell_pre_hook(
    command: &str,
    tool_name: &str,
    tool_args: &serde_json::Value,
    timeout_secs: u32,
) -> PreToolDecision {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::Command;

    let input = serde_json::json!({
        "hook_event": "pre_tool_use",
        "tool_name": tool_name,
        "tool_input": tool_args,
    });

    let mut child = match Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            astra_core::agent_warn!("hook", "Failed to spawn hook '{}': {}", command, e);
            return PreToolDecision::Allow;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
        drop(stdin);
    }

    let mut stdout_handle = child.stdout.take();
    let timeout = std::time::Duration::from_secs(timeout_secs as u64);

    let wait_result = tokio::time::timeout(timeout, child.wait()).await;
    match wait_result {
        Ok(Ok(status)) => {
            if !status.success() {
                return PreToolDecision::Block(format!(
                    "Hook '{}' exited with status {}",
                    command,
                    status.code().unwrap_or(-1)
                ));
            }
            let mut buf = Vec::new();
            if let Some(ref mut stdout) = stdout_handle {
                let _ = stdout.read_to_end(&mut buf).await;
            }
            parse_pre_hook_output(&buf)
        }
        Ok(Err(e)) => {
            astra_core::agent_warn!("hook", "Hook I/O error for '{}': {}", command, e);
            PreToolDecision::Allow
        }
        Err(_) => {
            let _ = child.kill().await;
            PreToolDecision::Block(format!(
                "Hook '{}' timed out after {}s",
                command, timeout_secs
            ))
        }
    }
}

/// Run a shell command for a PostToolUse hook, with timeout.
async fn run_shell_post_hook(
    command: &str,
    tool_name: &str,
    tool_args: &serde_json::Value,
    tool_output: &str,
    timeout_secs: u32,
) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::process::Command;

    let input = serde_json::json!({
        "hook_event": "post_tool_use",
        "tool_name": tool_name,
        "tool_input": tool_args,
        "tool_output": tool_output,
    });

    let mut child = match Command::new("sh")
        .args(["-c", command])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            astra_core::agent_warn!("hook", "Failed to spawn post-hook '{}': {}", command, e);
            return None;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.to_string().as_bytes()).await;
        drop(stdin);
    }

    let mut stdout_handle = child.stdout.take();
    let timeout = std::time::Duration::from_secs(timeout_secs as u64);

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) if status.success() => {
            let mut buf = Vec::new();
            if let Some(ref mut stdout) = stdout_handle {
                let _ = stdout.read_to_end(&mut buf).await;
            }
            parse_post_hook_output(&buf)
        }
        _ => {
            let _ = child.kill().await;
            None
        }
    }
}

/// Parse stdout from a PreToolUse shell hook into a decision.
fn parse_pre_hook_output(stdout: &[u8]) -> PreToolDecision {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return PreToolDecision::Allow;
    }

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        match v.get("decision").and_then(|d| d.as_str()) {
            Some("block") => {
                let reason = v
                    .get("reason")
                    .and_then(|r| r.as_str())
                    .unwrap_or("blocked by hook")
                    .to_string();
                PreToolDecision::Block(reason)
            }
            Some("allow") => {
                if let Some(ctx) = v.get("context").and_then(|c| c.as_str()) {
                    PreToolDecision::AllowWithContext(ctx.to_string())
                } else {
                    PreToolDecision::Allow
                }
            }
            _ => PreToolDecision::Allow,
        }
    } else {
        PreToolDecision::Allow
    }
}

/// Parse stdout from a PostToolUse shell hook for output modification.
fn parse_post_hook_output(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        v.get("output").and_then(|o| o.as_str()).map(String::from)
    } else {
        None
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

    // ── E2E: async hook execution with real shell commands ──────────

    #[tokio::test]
    async fn e2e_pre_hook_shell_allow() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "allow"}'"#.into(),
            },
            timeout_secs: 5,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_shell_block() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "block", "reason": "rm -rf detected"}'"#.into(),
            },
            timeout_secs: 5,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Block("rm -rf detected".into()));
    }

    #[tokio::test]
    async fn e2e_pre_hook_shell_allow_with_context() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "allow", "context": "hook injected info"}'"#
                    .into(),
            },
            timeout_secs: 5,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "read_file", &serde_json::json!({})).await;
        assert_eq!(
            decision,
            PreToolDecision::AllowWithContext("hook injected info".into())
        );
    }

    #[tokio::test]
    async fn e2e_pre_hook_shell_exit_nonzero_blocks() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: "exit 1".into(),
            },
            timeout_secs: 5,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        match decision {
            PreToolDecision::Block(reason) => assert!(reason.contains("exited with status")),
            _ => panic!("expected Block, got {:?}", decision),
        }
    }

    #[tokio::test]
    async fn e2e_pre_hook_no_match_allows() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"decision": "block", "reason": "nope"}'"#.into(),
            },
            timeout_secs: 5,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "read_file", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_reads_tool_input() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: r#"INPUT=$(cat); TOOL=$(echo "$INPUT" | grep -o '"tool_name":"[^"]*"' | head -1); if echo "$TOOL" | grep -q 'write_file'; then echo '{"decision":"block","reason":"writes blocked"}'; else echo '{"decision":"allow"}'; fi"#.into(),
            },
            timeout_secs: 5,
        }]);

        let allow =
            evaluate_pre_tool_hooks(&registry, "read_file", &serde_json::json!({})).await;
        assert_eq!(allow, PreToolDecision::Allow);

        let block =
            evaluate_pre_tool_hooks(&registry, "write_file", &serde_json::json!({})).await;
        assert_eq!(block, PreToolDecision::Block("writes blocked".into()));
    }

    #[tokio::test]
    async fn e2e_post_hook_shell_modifies_output() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{"output": "modified output"}'"#.into(),
            },
            timeout_secs: 5,
        }]);

        let result = evaluate_post_tool_hooks(
            &registry,
            "bash",
            &serde_json::json!({}),
            "original output",
        )
        .await;
        assert_eq!(result, Some("modified output".into()));
    }

    #[tokio::test]
    async fn e2e_post_hook_shell_no_modification() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PostToolUse,
            matcher: "bash".into(),
            action: HookAction::Shell {
                command: r#"echo '{}'"#.into(),
            },
            timeout_secs: 5,
        }]);

        let result = evaluate_post_tool_hooks(
            &registry,
            "bash",
            &serde_json::json!({}),
            "original output",
        )
        .await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn e2e_pre_hook_empty_registry_allows() {
        let registry = ToolEventHookRegistry::default();
        let decision =
            evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_empty_output_allows() {
        let registry = ToolEventHookRegistry::new(vec![ToolEventHook {
            event: ToolEventKind::PreToolUse,
            matcher: "*".into(),
            action: HookAction::Shell {
                command: "true".into(),
            },
            timeout_secs: 5,
        }]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        assert_eq!(decision, PreToolDecision::Allow);
    }

    #[tokio::test]
    async fn e2e_pre_hook_multiple_context_accumulates() {
        let registry = ToolEventHookRegistry::new(vec![
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "*".into(),
                action: HookAction::Shell {
                    command: r#"echo '{"decision":"allow","context":"hook1 info"}'"#.into(),
                },
                timeout_secs: 5,
            },
            ToolEventHook {
                event: ToolEventKind::PreToolUse,
                matcher: "*".into(),
                action: HookAction::Shell {
                    command: r#"echo '{"decision":"allow","context":"hook2 info"}'"#.into(),
                },
                timeout_secs: 5,
            },
        ]);

        let decision =
            evaluate_pre_tool_hooks(&registry, "bash", &serde_json::json!({})).await;
        match decision {
            PreToolDecision::AllowWithContext(ctx) => {
                assert!(ctx.contains("hook1 info"));
                assert!(ctx.contains("hook2 info"));
            }
            _ => panic!("expected AllowWithContext, got {:?}", decision),
        }
    }

    // ── Config loading tests ────────────────────────────────────────

    #[test]
    fn load_hooks_json_array() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"[
            {
                "event": "pre_tool_use",
                "matcher": "bash",
                "action": {"type": "shell", "command": "check-bash.sh"},
                "timeout_secs": 5
            }
        ]"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
        let hooks = registry.matching(ToolEventKind::PreToolUse, "bash");
        assert_eq!(hooks.len(), 1);
    }

    #[test]
    fn load_hooks_json_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let json = r#"{
            "hooks": [
                {
                    "event": "post_tool_use",
                    "matcher": "write_*",
                    "action": {"type": "shell", "command": "lint.sh"}
                }
            ]
        }"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_hooks_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let yaml = r#"
- event: pre_tool_use
  matcher: "*"
  action:
    type: shell
    command: audit-log.sh
  timeout_secs: 2
"#;
        std::fs::write(astra.join("hooks.yaml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_hooks_yaml_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        let yaml = r#"
hooks:
  - event: pre_tool_use
    matcher: bash
    action:
      type: shell
      command: validate.sh
"#;
        std::fs::write(astra.join("hooks.yml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn load_hooks_no_astra_dir() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load_tool_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_hooks_no_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".astra")).unwrap();
        let registry = load_tool_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_hooks_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();
        std::fs::write(astra.join("hooks.json"), "not valid json").unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert!(registry.is_empty());
    }

    #[test]
    fn load_hooks_json_preferred_over_yaml() {
        let dir = tempfile::tempdir().unwrap();
        let astra = dir.path().join(".astra");
        std::fs::create_dir_all(&astra).unwrap();

        let json = r#"[
            {"event": "pre_tool_use", "matcher": "a", "action": {"type": "shell", "command": "a"}},
            {"event": "pre_tool_use", "matcher": "b", "action": {"type": "shell", "command": "b"}}
        ]"#;
        std::fs::write(astra.join("hooks.json"), json).unwrap();

        let yaml = "- event: pre_tool_use\n  matcher: c\n  action:\n    type: shell\n    command: c\n";
        std::fs::write(astra.join("hooks.yaml"), yaml).unwrap();

        let registry = load_tool_event_hooks(dir.path());
        assert_eq!(registry.len(), 2); // JSON takes precedence
    }
}
