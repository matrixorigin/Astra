//! Delegation tree visualization — renders agent hierarchy as ASCII tree.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

#[cfg(test)]
use crate::orchestration_types::SpawnedAgentMetrics;
use crate::orchestration_types::{AgentStatus, SpawnedAgentInfo};

/// Node in the agent delegation tree.
#[derive(Debug, Clone)]
pub struct AgentTreeNode {
    pub agent_id: String,
    pub run_id: String,
    pub agent_type: String,
    pub description: String,
    pub status: AgentStatus,
    pub elapsed: Duration,
    pub metrics: TreeNodeMetrics,
    pub children: Vec<AgentTreeNode>,
}

/// Lightweight metrics for tree display.
#[derive(Debug, Clone, Default)]
pub struct TreeNodeMetrics {
    pub current_turn: u32,
    pub max_turns: u32,
    pub tool_calls: u32,
    pub has_permission_issues: bool,
}

impl AgentTreeNode {
    /// Build a tree from a flat list of agents.
    /// Returns the root nodes (agents with no parent or parent_run_id == "root").
    pub fn build_forest(agents: &[SpawnedAgentInfo]) -> Vec<AgentTreeNode> {
        let now = SystemTime::now();

        // Build lookup maps
        let mut nodes: HashMap<String, AgentTreeNode> = HashMap::new();
        let mut children_map: HashMap<String, Vec<String>> = HashMap::new();

        for agent in agents {
            let elapsed = now
                .duration_since(agent.started_at)
                .unwrap_or(Duration::ZERO);

            let (current_turn, max_turns) = match &agent.status {
                AgentStatus::Running { activity: _ } => (0, 30), // Default
                _ => (0, 0),
            };

            let node = AgentTreeNode {
                agent_id: agent.agent_id.clone(),
                run_id: agent.run_id.clone(),
                agent_type: agent.agent_type.clone(),
                description: agent.description.clone(),
                status: agent.status.clone(),
                elapsed,
                metrics: TreeNodeMetrics {
                    current_turn,
                    max_turns,
                    tool_calls: agent.metrics.tool_calls,
                    has_permission_issues: agent.has_permission_issues,
                },
                children: vec![],
            };

            nodes.insert(agent.run_id.clone(), node);

            // Track parent-child relationships
            children_map
                .entry(agent.parent_run_id.clone())
                .or_default()
                .push(agent.run_id.clone());
        }

        // Recursively attach children
        fn attach_children(
            run_id: &str,
            nodes: &mut HashMap<String, AgentTreeNode>,
            children_map: &HashMap<String, Vec<String>>,
        ) -> Option<AgentTreeNode> {
            let mut node = nodes.remove(run_id)?;

            if let Some(child_ids) = children_map.get(run_id) {
                for child_id in child_ids {
                    if let Some(child) = attach_children(child_id, nodes, children_map) {
                        node.children.push(child);
                    }
                }
            }

            Some(node)
        }

        // Find root nodes (parent_run_id == "root" or parent not in our list)
        let mut roots = vec![];
        let run_ids: Vec<String> = agents.iter().map(|a| a.run_id.clone()).collect();

        for agent in agents {
            if agent.parent_run_id == "root" || !run_ids.contains(&agent.parent_run_id) {
                if let Some(root) = attach_children(&agent.run_id, &mut nodes, &children_map) {
                    roots.push(root);
                }
            }
        }

        roots
    }

    /// Render tree to string with ASCII box-drawing characters.
    pub fn render(&self) -> String {
        let mut output = String::new();
        self.render_node(&mut output, "", true);
        output
    }

    fn render_node(&self, output: &mut String, prefix: &str, is_last: bool) {
        // Connector characters
        let connector = if is_last { "└── " } else { "├── " };
        let child_prefix = if is_last { "    " } else { "│   " };

        // Status icon
        let (icon, status_text) = match &self.status {
            AgentStatus::Initializing => ("⏳", "initializing".to_string()),
            AgentStatus::Running { activity } => {
                let activity_short = if activity.chars().count() > 20 {
                    format!("{}…", activity.chars().take(19).collect::<String>())
                } else {
                    activity.clone()
                };
                ("▶", activity_short)
            }
            AgentStatus::Idle => ("◯", "idle".to_string()),
            AgentStatus::Completed { result: _ } => ("✓", "done".to_string()),
            AgentStatus::Failed { error: _ } => ("✗", "failed".to_string()),
            AgentStatus::Cancelled => ("⊘", "cancelled".to_string()),
        };

        // Format elapsed time
        let elapsed_secs = self.elapsed.as_secs();
        let elapsed_str = if elapsed_secs >= 60 {
            format!("{}m{}s", elapsed_secs / 60, elapsed_secs % 60)
        } else {
            format!("{}s", elapsed_secs)
        };

        // Metrics string
        let metrics_str = if self.metrics.tool_calls > 0 {
            format!(" ({} tools)", self.metrics.tool_calls)
        } else {
            String::new()
        };

        // Permission warning
        let perm_warning = if self.metrics.has_permission_issues {
            " 🔒"
        } else {
            ""
        };

        // Render this node
        output.push_str(&format!(
            "{}{}{} {} [{}] ({}) {}{}{}",
            prefix,
            connector,
            icon,
            self.agent_id,
            self.agent_type,
            elapsed_str,
            status_text,
            metrics_str,
            perm_warning,
        ));
        output.push('\n');

        // Render children
        let new_prefix = format!("{}{}", prefix, child_prefix);
        for (i, child) in self.children.iter().enumerate() {
            let is_last_child = i == self.children.len() - 1;
            child.render_node(output, &new_prefix, is_last_child);
        }
    }
}

/// Render a forest (multiple roots) as a complete tree display.
pub fn render_agent_forest(roots: &[AgentTreeNode]) -> String {
    if roots.is_empty() {
        return "No agents running.\n".to_string();
    }

    let mut output = String::new();

    for (i, root) in roots.iter().enumerate() {
        let is_last = i == roots.len() - 1;
        root.render_node(&mut output, "", is_last);
    }

    // Summary line
    let (running, completed, failed) = count_statuses(roots);
    output.push_str(&format!(
        "\nSummary: {} running, {} completed, {} failed\n",
        running, completed, failed,
    ));

    output
}

fn count_statuses(nodes: &[AgentTreeNode]) -> (usize, usize, usize) {
    let mut running = 0;
    let mut completed = 0;
    let mut failed = 0;

    for node in nodes {
        match &node.status {
            AgentStatus::Running { .. } | AgentStatus::Initializing => running += 1,
            AgentStatus::Completed { .. } => completed += 1,
            AgentStatus::Failed { .. } => failed += 1,
            _ => {}
        }

        let (r, c, f) = count_statuses(&node.children);
        running += r;
        completed += c;
        failed += f;
    }

    (running, completed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_agent(
        agent_id: &str,
        run_id: &str,
        parent_run_id: &str,
        status: AgentStatus,
    ) -> SpawnedAgentInfo {
        SpawnedAgentInfo {
            agent_id: agent_id.to_string(),
            run_id: run_id.to_string(),
            parent_run_id: parent_run_id.to_string(),
            agent_type: "worker".to_string(),
            description: "test agent".to_string(),
            status,
            started_at: SystemTime::now(),
            metrics: SpawnedAgentMetrics::default(),
            has_permission_issues: false,
        }
    }

    #[test]
    fn test_build_simple_tree() {
        let agents = vec![
            make_agent(
                "main",
                "run-1",
                "root",
                AgentStatus::Running {
                    activity: "planning".into(),
                },
            ),
            make_agent(
                "child-1",
                "run-2",
                "run-1",
                AgentStatus::Completed {
                    result: "ok".into(),
                },
            ),
            make_agent(
                "child-2",
                "run-3",
                "run-1",
                AgentStatus::Running {
                    activity: "coding".into(),
                },
            ),
        ];

        let forest = AgentTreeNode::build_forest(&agents);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].agent_id, "main");
        assert_eq!(forest[0].children.len(), 2);
    }

    #[test]
    fn test_render_tree() {
        let agents = vec![
            make_agent(
                "main",
                "run-1",
                "root",
                AgentStatus::Running {
                    activity: "planning".into(),
                },
            ),
            make_agent(
                "researcher",
                "run-2",
                "run-1",
                AgentStatus::Completed {
                    result: "ok".into(),
                },
            ),
            make_agent(
                "coder",
                "run-3",
                "run-1",
                AgentStatus::Running {
                    activity: "writing".into(),
                },
            ),
            make_agent("tester", "run-4", "run-3", AgentStatus::Idle),
        ];

        let forest = AgentTreeNode::build_forest(&agents);
        let rendered = render_agent_forest(&forest);

        assert!(rendered.contains("main"));
        assert!(rendered.contains("researcher"));
        assert!(rendered.contains("coder"));
        assert!(rendered.contains("tester"));
        assert!(rendered.contains("Summary"));
    }

    #[test]
    fn test_empty_forest() {
        let forest: Vec<AgentTreeNode> = vec![];
        let rendered = render_agent_forest(&forest);
        assert!(rendered.contains("No agents"));
    }
}
