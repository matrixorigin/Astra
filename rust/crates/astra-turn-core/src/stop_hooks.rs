//! Stop hooks: verification phase when the agent thinks it's done.
//!
//! Instead of executing shell commands directly (which bypasses the tool
//! permission/audit system), stop hooks inject a user message that instructs
//! the LLM to run verification commands via the normal `bash` tool.
//! This ensures all execution goes through PermissionManager, tool event
//! auditing, and TurnGuard error tracking.
//!
//! Enhanced features (D-3):
//! - `depends_on`: declare dependencies between hooks for ordered execution
//! - `timeout`: per-hook timeout override
//! - `cache_key`: skip hooks whose inputs haven't changed since last pass
//! - Topological layering: hooks at the same depth can run in parallel

use std::collections::{HashMap, VecDeque};

/// A verification command to run before the loop is allowed to complete.
#[derive(Debug, Clone)]
pub struct StopHook {
    /// Human-readable label (e.g. "type-check", "lint").
    pub label: String,
    /// Shell command to execute (e.g. "cargo check").
    pub command: String,
    /// Working directory (informational, included in the prompt).
    pub working_dir: Option<String>,
    /// Labels of hooks that must complete before this one.
    pub depends_on: Vec<String>,
    /// Per-hook timeout hint (seconds). Included in the prompt for the LLM.
    pub timeout_secs: Option<u32>,
    /// Cache key for skipping re-runs. If present and the cache contains a
    /// passing result for this key, the hook is omitted from the prompt.
    pub cache_key: Option<String>,
}

/// Cached result from a previous stop-hook execution.
#[derive(Debug, Clone)]
pub struct CachedHookResult {
    pub passed: bool,
    pub cache_key: String,
}

/// A cache for stop hook results, enabling skip-on-pass behaviour.
#[derive(Debug, Default)]
pub struct StopHookCache {
    entries: HashMap<String, CachedHookResult>,
}

impl StopHookCache {
    pub fn record(&mut self, key: &str, passed: bool) {
        self.entries.insert(
            key.to_string(),
            CachedHookResult {
                passed,
                cache_key: key.to_string(),
            },
        );
    }

    pub fn should_skip(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|r| r.passed)
    }
}

/// Topological sort of hooks into layers. Hooks in the same layer can execute
/// in parallel; layers themselves execute sequentially.
///
/// Returns `None` if there is a dependency cycle.
pub fn build_execution_layers(hooks: &[StopHook]) -> Option<Vec<Vec<usize>>> {
    let n = hooks.len();
    let label_to_idx: HashMap<&str, usize> = hooks
        .iter()
        .enumerate()
        .map(|(i, h)| (h.label.as_str(), i))
        .collect();

    // Build adjacency + in-degree
    let mut in_degree = vec![0u32; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];

    for (i, hook) in hooks.iter().enumerate() {
        for dep_label in &hook.depends_on {
            if let Some(&dep_idx) = label_to_idx.get(dep_label.as_str()) {
                dependents[dep_idx].push(i);
                in_degree[i] += 1;
            }
            // Unknown deps are silently ignored (may be from a different config)
        }
    }

    // Kahn's algorithm
    let mut queue: VecDeque<usize> = VecDeque::new();
    for (i, &deg) in in_degree.iter().enumerate() {
        if deg == 0 {
            queue.push_back(i);
        }
    }

    let mut layers: Vec<Vec<usize>> = Vec::new();
    let mut processed = 0usize;

    while !queue.is_empty() {
        let layer: Vec<usize> = queue.drain(..).collect();
        for &idx in &layer {
            processed += 1;
            for &dep in &dependents[idx] {
                in_degree[dep] -= 1;
                if in_degree[dep] == 0 {
                    queue.push_back(dep);
                }
            }
        }
        layers.push(layer);
    }

    if processed == n {
        Some(layers)
    } else {
        None // cycle detected
    }
}

/// Filter out hooks that the cache says can be skipped.
pub fn filter_cached(hooks: &[StopHook], cache: &StopHookCache) -> Vec<StopHook> {
    hooks
        .iter()
        .filter(|h| h.cache_key.as_ref().is_none_or(|k| !cache.should_skip(k)))
        .cloned()
        .collect()
}

/// Build a user message that instructs the LLM to run verification commands.
///
/// Enhanced: orders hooks by dependency layers and marks parallel groups.
/// Returns `None` if there are no hooks (caller should complete normally).
pub fn build_stop_hook_prompt(hooks: &[StopHook]) -> Option<serde_json::Value> {
    if hooks.is_empty() {
        return None;
    }

    let layers = build_execution_layers(hooks);
    let content = match layers {
        Some(layers) if layers.len() > 1 => {
            // Multi-layer: show execution order
            let mut parts = Vec::new();
            for (li, layer) in layers.iter().enumerate() {
                let parallel_note = if layer.len() > 1 {
                    " (these can run in parallel)"
                } else {
                    ""
                };
                parts.push(format!("Phase {}{}:", li + 1, parallel_note));
                for &idx in layer {
                    let h = &hooks[idx];
                    let timeout_hint = h
                        .timeout_secs
                        .map(|t| format!(" [timeout: {t}s]"))
                        .unwrap_or_default();
                    if let Some(dir) = &h.working_dir {
                        parts.push(format!(
                            "  - `{}` (in `{dir}`) — {}{}",
                            h.command, h.label, timeout_hint
                        ));
                    } else {
                        parts.push(format!("  - `{}` — {}{}", h.command, h.label, timeout_hint));
                    }
                }
            }
            format!(
                "⚠️ VERIFICATION REQUIRED: Before you finish, run these checks using the bash tool:\n\
                 {}\n\n\
                 If any check fails, fix the issues and re-run the failing check. \
                 Repeat until all checks pass. Only then may you complete.",
                parts.join("\n")
            )
        }
        _ => {
            // Single layer or no deps: flat list
            let commands: Vec<String> = hooks
                .iter()
                .map(|h| {
                    let timeout_hint = h
                        .timeout_secs
                        .map(|t| format!(" [timeout: {t}s]"))
                        .unwrap_or_default();
                    if let Some(dir) = &h.working_dir {
                        format!(
                            "- `{}` (in `{dir}`) — {}{}",
                            h.command, h.label, timeout_hint
                        )
                    } else {
                        format!("- `{}` — {}{}", h.command, h.label, timeout_hint)
                    }
                })
                .collect();
            format!(
                "⚠️ VERIFICATION REQUIRED: Before you finish, run these checks using the bash tool:\n\
                 {}\n\n\
                 If any check fails, fix the issues and re-run the failing check. \
                 Repeat until all checks pass. Only then may you complete.",
                commands.join("\n")
            )
        }
    };

    Some(serde_json::json!({
        "role": "user",
        "content": content
    }))
}

/// Same as [`build_stop_hook_prompt`], but framed for post-delegation / teammate rounds.
pub fn build_teammate_idle_hook_prompt(hooks: &[StopHook]) -> Option<serde_json::Value> {
    if hooks.is_empty() {
        return None;
    }
    let commands: Vec<String> = hooks
        .iter()
        .map(|h| {
            if let Some(dir) = &h.working_dir {
                format!("- `{}` (in `{dir}`) — {}", h.command, h.label)
            } else {
                format!("- `{}` — {}", h.command, h.label)
            }
        })
        .collect();
    Some(serde_json::json!({
        "role": "user",
        "content": format!(
            "⚠️ TEAMMATE ROUND COMPLETE: Delegated agents have returned. Before continuing, run these checks using the bash tool:\n\
             {}\n\n\
             If any check fails, fix the issues and re-run the failing check. \
             Then proceed with your plan.",
            commands.join("\n")
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_hook(label: &str, command: &str) -> StopHook {
        StopHook {
            label: label.into(),
            command: command.into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,
            cache_key: None,
        }
    }

    #[test]
    fn empty_hooks_returns_none() {
        assert!(build_stop_hook_prompt(&[]).is_none());
    }

    #[test]
    fn single_hook_generates_prompt() {
        let hooks = vec![StopHook {
            label: "type-check".into(),
            command: "cargo check".into(),
            working_dir: Some("/project".into()),
            depends_on: Vec::new(),
            timeout_secs: None,
            cache_key: None,
        }];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("cargo check"));
        assert!(content.contains("/project"));
        assert!(content.contains("type-check"));
        assert!(content.contains("bash tool"));
    }

    #[test]
    fn multiple_hooks_listed() {
        let hooks = vec![
            simple_hook("check", "cargo check"),
            simple_hook("lint", "cargo clippy"),
        ];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("cargo check"));
        assert!(content.contains("cargo clippy"));
    }

    #[test]
    fn teammate_idle_prompt_differs() {
        let hooks = vec![simple_hook("sync-check", "make verify")];
        let msg = build_teammate_idle_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("TEAMMATE ROUND"));
        assert!(content.contains("make verify"));
    }

    #[test]
    fn dependency_layers_no_deps() {
        let hooks = vec![
            simple_hook("a", "cmd_a"),
            simple_hook("b", "cmd_b"),
            simple_hook("c", "cmd_c"),
        ];
        let layers = build_execution_layers(&hooks).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 3);
    }

    #[test]
    fn dependency_layers_chain() {
        let hooks = vec![
            simple_hook("build", "make build"),
            StopHook {
                label: "test".into(),
                command: "make test".into(),
                working_dir: None,
                depends_on: vec!["build".into()],
                timeout_secs: None,
                cache_key: None,
            },
            StopHook {
                label: "lint".into(),
                command: "make lint".into(),
                working_dir: None,
                depends_on: vec!["test".into()],
                timeout_secs: None,
                cache_key: None,
            },
        ];
        let layers = build_execution_layers(&hooks).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![0]); // build
        assert_eq!(layers[1], vec![1]); // test
        assert_eq!(layers[2], vec![2]); // lint
    }

    #[test]
    fn dependency_layers_diamond() {
        // build → test, build → lint, test+lint → deploy
        let hooks = vec![
            simple_hook("build", "make build"),
            StopHook {
                label: "test".into(),
                command: "make test".into(),
                working_dir: None,
                depends_on: vec!["build".into()],
                timeout_secs: None,
                cache_key: None,
            },
            StopHook {
                label: "lint".into(),
                command: "make lint".into(),
                working_dir: None,
                depends_on: vec!["build".into()],
                timeout_secs: None,
                cache_key: None,
            },
            StopHook {
                label: "deploy".into(),
                command: "make deploy".into(),
                working_dir: None,
                depends_on: vec!["test".into(), "lint".into()],
                timeout_secs: None,
                cache_key: None,
            },
        ];
        let layers = build_execution_layers(&hooks).unwrap();
        assert_eq!(layers.len(), 3);
        assert_eq!(layers[0], vec![0]); // build
        assert!(layers[1].contains(&1) && layers[1].contains(&2)); // test + lint parallel
        assert_eq!(layers[2], vec![3]); // deploy
    }

    #[test]
    fn dependency_cycle_returns_none() {
        let hooks = vec![
            StopHook {
                label: "a".into(),
                command: "cmd_a".into(),
                working_dir: None,
                depends_on: vec!["b".into()],
                timeout_secs: None,
                cache_key: None,
            },
            StopHook {
                label: "b".into(),
                command: "cmd_b".into(),
                working_dir: None,
                depends_on: vec!["a".into()],
                timeout_secs: None,
                cache_key: None,
            },
        ];
        assert!(build_execution_layers(&hooks).is_none());
    }

    #[test]
    fn cache_skip_passing() {
        let mut cache = StopHookCache::default();
        cache.record("check-key", true);
        assert!(cache.should_skip("check-key"));
        assert!(!cache.should_skip("unknown"));
    }

    #[test]
    fn cache_no_skip_failing() {
        let mut cache = StopHookCache::default();
        cache.record("check-key", false);
        assert!(!cache.should_skip("check-key"));
    }

    #[test]
    fn filter_cached_removes_passing() {
        let mut cache = StopHookCache::default();
        cache.record("lint-key", true);
        let hooks = vec![
            StopHook {
                label: "lint".into(),
                command: "make lint".into(),
                working_dir: None,
                depends_on: Vec::new(),
                timeout_secs: None,
                cache_key: Some("lint-key".into()),
            },
            simple_hook("test", "make test"),
        ];
        let filtered = filter_cached(&hooks, &cache);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].label, "test");
    }

    #[test]
    fn multi_layer_prompt_shows_phases() {
        let hooks = vec![
            simple_hook("build", "make build"),
            StopHook {
                label: "test".into(),
                command: "make test".into(),
                working_dir: None,
                depends_on: vec!["build".into()],
                timeout_secs: Some(60),
                cache_key: None,
            },
        ];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("Phase 1"));
        assert!(content.contains("Phase 2"));
        assert!(content.contains("[timeout: 60s]"));
    }
}
