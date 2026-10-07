//! Pure completion-check obligations shared by execution and checkpoints.

use crate::completion_settlement::deserialize_required_option;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// A verification command surfaced at the completion boundary.
/// Only authoritative hooks form a terminal contract; discovery is advisory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopHook {
    pub label: String,
    pub command: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    pub working_dir: Option<String>,
    pub depends_on: Vec<String>,
    /// Per-hook timeout hint, not authority to extend the run's deadline.
    #[serde(deserialize_with = "deserialize_required_option")]
    pub timeout_secs: Option<u32>,
    pub authoritative: bool,
}

/// Declared checks for both completion boundaries of one workspace.
/// These are verification obligations, never permission to execute a tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionCheckDeclarations {
    pub stop: Vec<StopHook>,
    pub task_completed: Vec<StopHook>,
}

/// Completion boundary frozen with the execution contract.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionCheckPhase {
    #[default]
    Stop,
    TaskCompleted,
}

impl CompletionCheckDeclarations {
    pub fn into_selected(
        self,
        is_plan_subtask: bool,
        verification_required: bool,
    ) -> Vec<StopHook> {
        let mut checks = if is_plan_subtask {
            self.task_completed
        } else {
            self.stop
        };
        checks.retain(|check| check.authoritative || verification_required);
        checks
    }
}

/// Frozen declarations and selected completion boundary at one execution frontier.
/// This contains neither request headers nor model credentials. Missing fields
/// must never become an empty obligation set during recovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopHookObligations {
    pub declarations: CompletionCheckDeclarations,
    pub phase: CompletionCheckPhase,
}

pub const MAX_COMPLETION_CHECKS: usize = 64;
pub const MAX_COMPLETION_DECLARATION_BYTES: usize = 256 * 1024;

/// Bound input and reject ambiguous verification dependencies before admission.
/// This validates declarations; it does not authorize any tool invocation.
pub fn validate_completion_check_declarations(
    declarations: &CompletionCheckDeclarations,
) -> Result<(), String> {
    const MAX_CHECKS: usize = MAX_COMPLETION_CHECKS;
    const MAX_LABEL_BYTES: usize = 256;
    const MAX_DIRECTORY_BYTES: usize = 4096;
    const MAX_DECLARATION_BYTES: usize = MAX_COMPLETION_DECLARATION_BYTES;
    if declarations
        .stop
        .len()
        .saturating_add(declarations.task_completed.len())
        > MAX_CHECKS
    {
        return Err(format!(
            "completion checks exceed {MAX_CHECKS} declarations"
        ));
    }
    let mut string_bytes = 0usize;
    for checks in [&declarations.stop, &declarations.task_completed] {
        let mut labels = HashMap::new();
        for check in checks {
            if check.label.trim().is_empty()
                || check.label.trim() != check.label
                || check.label.len() > MAX_LABEL_BYTES
                || labels
                    .insert(check.label.as_str(), check.authoritative)
                    .is_some()
            {
                return Err("completion check labels must be unique, nonblank and bounded".into());
            }
            let directory = check.working_dir.as_deref().unwrap_or_default();
            let invocation_bytes = check.command.len().saturating_add(if directory.is_empty() {
                0
            } else {
                directory.len().saturating_add(7)
            });
            if check.command.trim().is_empty()
                || invocation_bytes > astra_core::MAX_SHELL_COMMAND_BYTES
                || directory.len() > MAX_DIRECTORY_BYTES
                || check.depends_on.len() > MAX_CHECKS
                || check
                    .depends_on
                    .iter()
                    .any(|label| label.len() > MAX_LABEL_BYTES)
                || check.timeout_secs == Some(0)
            {
                return Err(format!(
                    "completion check '{}' has invalid or oversized fields",
                    check.label
                ));
            }
            string_bytes = string_bytes.saturating_add(
                check.label.len()
                    + check.command.len()
                    + directory.len()
                    + check.depends_on.iter().map(String::len).sum::<usize>(),
            );
        }
        if checks.iter().any(|check| {
            check.depends_on.iter().any(|label| {
                labels
                    .get(label.as_str())
                    .is_none_or(|authoritative| check.authoritative && !authoritative)
            })
        }) || build_execution_layers(checks).is_none()
        {
            return Err(
                "completion check dependencies must exist in the same phase, be acyclic, and keep authoritative checks independent of advisory checks".into(),
            );
        }
    }
    if string_bytes > MAX_DECLARATION_BYTES
        || serde_json::to_vec(declarations)
            .map_err(|error| error.to_string())?
            .len()
            > MAX_DECLARATION_BYTES
    {
        return Err(format!(
            "completion check declarations exceed {MAX_DECLARATION_BYTES} bytes"
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_obligations_require_every_fact_including_nullable_fields() {
        let hook = StopHook {
            label: "verify".into(),
            command: "make check".into(),
            working_dir: None,
            depends_on: Vec::new(),
            timeout_secs: None,

            authoritative: true,
        };
        let snapshot = StopHookObligations {
            declarations: CompletionCheckDeclarations {
                stop: vec![hook.clone()],
                task_completed: Vec::new(),
            },
            phase: CompletionCheckPhase::Stop,
        };
        let wire = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_value::<StopHookObligations>(wire.clone()).unwrap(),
            snapshot
        );
        for field in wire.as_object().unwrap().keys() {
            let mut incomplete = wire.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(
                serde_json::from_value::<StopHookObligations>(incomplete).is_err(),
                "missing {field}"
            );
        }
        for field in wire["declarations"]["stop"][0].as_object().unwrap().keys() {
            let mut incomplete = wire.clone();
            incomplete["declarations"]["stop"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                serde_json::from_value::<StopHookObligations>(incomplete).is_err(),
                "missing hook {field}"
            );
        }
        for phase in [serde_json::json!("unknown"), serde_json::Value::Null] {
            let mut invalid = wire.clone();
            invalid["phase"] = phase;
            assert!(serde_json::from_value::<StopHookObligations>(invalid).is_err());
        }
        assert!(
            serde_json::from_value::<StopHookObligations>(serde_json::json!({
                "stop_hooks": [], "stop_hook_runs": 0,
            }))
            .is_err(),
            "retired fields do not grant an empty contract"
        );
        let mut extra = wire;
        extra["forward_headers"] = serde_json::json!({});
        assert!(serde_json::from_value::<StopHookObligations>(extra).is_err());
    }
}
