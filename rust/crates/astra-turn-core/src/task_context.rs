//! Compact task lifecycle context for model prompts.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskContextItem {
    pub id: String,
    pub title: String,
    pub status: String,
    pub progress_pct: Option<u8>,
    pub active_form: Option<String>,
    pub blocks: Vec<String>,
    pub blocked_by: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskContextSnapshot {
    pub active_tasks: Vec<TaskContextItem>,
}

impl TaskContextSnapshot {
    #[must_use]
    pub fn render_for_prompt(&self) -> Option<String> {
        if self.active_tasks.is_empty() {
            return None;
        }
        let mut out = String::from("Active task lifecycle:\n");
        for task in &self.active_tasks {
            out.push_str("- ");
            out.push_str(&task.id);
            out.push_str(": ");
            out.push_str(&task.title);
            out.push_str(" [");
            out.push_str(&task.status);
            out.push(']');
            if let Some(progress) = task.progress_pct {
                out.push_str(&format!(" {progress}%"));
            }
            if let Some(active_form) = &task.active_form {
                out.push_str(" - ");
                out.push_str(active_form);
            }
            if !task.blocks.is_empty() {
                out.push_str(" blocks=");
                out.push_str(&task.blocks.join(","));
            }
            if !task.blocked_by.is_empty() {
                out.push_str(" blocked_by=");
                out.push_str(&task.blocked_by.join(","));
            }
            out.push('\n');
        }
        Some(out)
    }

    pub fn validate_dependencies(&self) -> Result<(), TaskContextError> {
        for task in &self.active_tasks {
            if task.blocked_by.iter().any(|dep| dep == &task.id) {
                return Err(TaskContextError::SelfDependency {
                    id: task.id.clone(),
                });
            }
            if task.blocks.iter().any(|blocked| blocked == &task.id) {
                return Err(TaskContextError::SelfDependency {
                    id: task.id.clone(),
                });
            }
            for dep in task.blocked_by.iter().chain(task.blocks.iter()) {
                self.require_known_task(&task.id, dep)?;
            }
        }
        Ok(())
    }

    fn require_known_task(&self, id: &str, referenced: &str) -> Result<(), TaskContextError> {
        if self
            .active_tasks
            .iter()
            .any(|candidate| candidate.id == referenced)
        {
            Ok(())
        } else {
            Err(TaskContextError::MissingDependency {
                id: id.to_string(),
                dependency: referenced.to_string(),
            })
        }
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum TaskContextError {
    #[error("task '{id}' cannot block on itself")]
    SelfDependency { id: String },
    #[error("task '{id}' references missing dependency '{dependency}'")]
    MissingDependency { id: String, dependency: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_progress_renders_into_model_context() {
        let snapshot = TaskContextSnapshot {
            active_tasks: vec![
                TaskContextItem {
                    id: "p0-6".into(),
                    title: "Task lifecycle".into(),
                    status: "in_progress".into(),
                    progress_pct: Some(60),
                    active_form: Some("wiring task context".into()),
                    blocks: vec!["p0-7".into()],
                    blocked_by: vec![],
                },
                TaskContextItem {
                    id: "p0-7".into(),
                    title: "Cost ledger".into(),
                    status: "pending".into(),
                    progress_pct: None,
                    active_form: None,
                    blocks: vec![],
                    blocked_by: vec!["p0-6".into()],
                },
            ],
        };
        snapshot.validate_dependencies().unwrap();
        let rendered = snapshot.render_for_prompt().unwrap();
        assert!(rendered.contains("p0-6"));
        assert!(rendered.contains("60%"));
        assert!(rendered.contains("wiring task context"));
        assert!(rendered.contains("blocks=p0-7"));
        assert!(rendered.contains("blocked_by=p0-6"));
    }

    #[test]
    fn dependency_validation_rejects_self_and_missing_deps() {
        let self_dep = TaskContextSnapshot {
            active_tasks: vec![TaskContextItem {
                id: "a".into(),
                title: "A".into(),
                status: "blocked".into(),
                progress_pct: None,
                active_form: None,
                blocks: vec![],
                blocked_by: vec!["a".into()],
            }],
        };
        assert!(matches!(
            self_dep.validate_dependencies(),
            Err(TaskContextError::SelfDependency { .. })
        ));

        let missing = TaskContextSnapshot {
            active_tasks: vec![TaskContextItem {
                id: "a".into(),
                title: "A".into(),
                status: "blocked".into(),
                progress_pct: None,
                active_form: None,
                blocks: vec![],
                blocked_by: vec!["b".into()],
            }],
        };
        assert!(matches!(
            missing.validate_dependencies(),
            Err(TaskContextError::MissingDependency { .. })
        ));
    }
}
