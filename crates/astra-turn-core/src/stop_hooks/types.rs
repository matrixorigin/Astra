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
//! - `timeout`: per-hook timeout hint
//! - Topological layering: hooks at the same depth can run in parallel

pub use astra_turn_types::StopHook;

pub use astra_turn_types::{
    MAX_COMPLETION_CHECKS, MAX_COMPLETION_DECLARATION_BYTES, build_execution_layers,
    validate_completion_check_declarations,
};

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
                "⚠️ VERIFICATION REQUIRED: Before you finish, run any missing checks using the bash tool:\n\
                 {}\n\n\
                 If you already ran the exact relevant check(s) after the latest file edit and they passed, \
                 do not rerun them; summarize that evidence and finish. \
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
                "⚠️ VERIFICATION REQUIRED: Before you finish, run any missing checks using the bash tool:\n\
                 {}\n\n\
                 If you already ran the exact relevant check(s) after the latest file edit and they passed, \
                 do not rerun them; summarize that evidence and finish. \
                 If any check fails, fix the issues and re-run the failing check. \
                 Repeat until all checks pass. Only then may you complete.",
                commands.join("\n")
            )
        }
    };

    Some(astra_turn_types::runtime_owned_message(
        "user",
        content,
        astra_turn_types::RuntimeMessageDelivery::RequiredContext,
    ))
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

            authoritative: false,
        }
    }

    #[test]
    fn completion_declarations_reject_ambiguous_dependencies_and_oversized_input() {
        use astra_turn_types::CompletionCheckDeclarations;
        let checks = CompletionCheckDeclarations {
            stop: vec![
                simple_hook("build", "make build"),
                simple_hook("test", "make test"),
            ],
            task_completed: vec![simple_hook("build", "make child")],
        };
        assert!(validate_completion_check_declarations(&checks).is_ok());
        let mut dependent = checks.clone();
        dependent.stop[1].depends_on = vec!["build".into()];
        assert!(validate_completion_check_declarations(&dependent).is_ok());
        let mut invalid = Vec::new();
        let mut duplicate = checks.clone();
        duplicate.stop[1].label = "build".into();
        invalid.push(duplicate);
        let mut unknown = checks.clone();
        unknown.stop[0].depends_on = vec!["missing".into()];
        invalid.push(unknown);
        let mut cross_phase = checks.clone();
        cross_phase.task_completed[0].depends_on = vec!["test".into()];
        invalid.push(cross_phase);
        let mut advisory_dependency = checks.clone();
        advisory_dependency.stop[1].authoritative = true;
        advisory_dependency.stop[1].depends_on = vec!["build".into()];
        invalid.push(advisory_dependency);
        dependent.stop[0].depends_on = vec!["test".into()];
        invalid.push(dependent);
        let mut count = checks.clone();
        count.stop = (0..64)
            .map(|i| simple_hook(&format!("check-{i}"), "true"))
            .collect();
        invalid.push(count);
        let mut oversized_directory = checks.clone();
        oversized_directory.stop[0].working_dir = Some("d".repeat(4097));
        invalid.push(oversized_directory);
        let mut total = checks.clone();
        for check in total.stop.iter_mut().chain(&mut total.task_completed) {
            check.command = "x".repeat(astra_core::MAX_SHELL_COMMAND_BYTES);
        }
        invalid.push(total);
        let mut escaped = checks.clone();
        escaped.task_completed.clear();
        for check in &mut escaped.stop {
            check.command = "\\a".repeat(48_000);
        }
        invalid.push(escaped);
        for declarations in invalid {
            assert!(validate_completion_check_declarations(&declarations).is_err());
        }

        let mut boundary = CompletionCheckDeclarations {
            stop: vec![simple_hook("verify", "true")],
            task_completed: Vec::new(),
        };
        boundary.stop[0].working_dir = Some("/workspace".into());
        boundary.stop[0].command =
            "x".repeat(astra_core::MAX_SHELL_COMMAND_BYTES - "/workspace".len() - 7);
        assert!(validate_completion_check_declarations(&boundary).is_ok());
        boundary.stop[0].command.push('x');
        assert!(validate_completion_check_declarations(&boundary).is_err());
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
            authoritative: false,
        }];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("cargo check"));
        assert!(content.contains("/project"));
        assert!(content.contains("type-check"));
        assert!(content.contains("bash tool"));
        assert!(content.contains("already ran the exact relevant check"));
        assert!(content.contains("after the latest file edit"));
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
                authoritative: false,
            },
            StopHook {
                label: "lint".into(),
                command: "make lint".into(),
                working_dir: None,
                depends_on: vec!["test".into()],
                timeout_secs: None,
                authoritative: false,
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
                authoritative: false,
            },
            StopHook {
                label: "lint".into(),
                command: "make lint".into(),
                working_dir: None,
                depends_on: vec!["build".into()],
                timeout_secs: None,
                authoritative: false,
            },
            StopHook {
                label: "deploy".into(),
                command: "make deploy".into(),
                working_dir: None,
                depends_on: vec!["test".into(), "lint".into()],
                timeout_secs: None,
                authoritative: false,
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
                authoritative: false,
            },
            StopHook {
                label: "b".into(),
                command: "cmd_b".into(),
                working_dir: None,
                depends_on: vec!["a".into()],
                timeout_secs: None,
                authoritative: false,
            },
        ];
        assert!(build_execution_layers(&hooks).is_none());
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
                authoritative: false,
            },
        ];
        let msg = build_stop_hook_prompt(&hooks).unwrap();
        let content = msg["content"].as_str().unwrap();
        assert!(content.contains("Phase 1"));
        assert!(content.contains("Phase 2"));
        assert!(content.contains("[timeout: 60s]"));
        assert!(content.contains("do not rerun them"));
    }
}
