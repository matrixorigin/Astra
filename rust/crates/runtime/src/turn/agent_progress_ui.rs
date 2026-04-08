//! Terminal Progress Visualization for Sub-Agent Execution (D-11)
//!
//! Provides a structured progress display for multi-agent runs in CLI mode.
//! Inspired by Claude Code's Tmux/iTerm2 split-pane visualization where
//! each agent has a visible progress indicator.
//!
//! This module provides the data model and text-rendering layer. It does NOT
//! depend on ratatui or ncurses — it uses simple ANSI escape sequences for
//! inline terminal progress that works in any terminal.

use std::fmt;
use std::time::{Duration, Instant};

// ───────────────────────────── Agent Status ─────────────────────────────

/// Current status of a sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Queued but not yet started.
    Pending,
    /// Currently executing.
    Running {
        current_turn: u32,
        max_turns: u32,
        last_tool: Option<String>,
    },
    /// Completed successfully.
    Completed { turns_used: u32, tool_calls: u32 },
    /// Failed with an error.
    Failed { reason: String },
    /// Paused (e.g., waiting for gate verification).
    Paused { reason: String },
}

impl AgentStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentStatus::Completed { .. } | AgentStatus::Failed { .. })
    }
}

/// Visual color for an agent (ANSI 256-color codes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentColor {
    Blue,
    Green,
    Yellow,
    Red,
    Cyan,
    Magenta,
}

impl AgentColor {
    /// ANSI escape code for foreground color.
    pub fn ansi_fg(&self) -> &'static str {
        match self {
            AgentColor::Blue => "\x1b[34m",
            AgentColor::Green => "\x1b[32m",
            AgentColor::Yellow => "\x1b[33m",
            AgentColor::Red => "\x1b[31m",
            AgentColor::Cyan => "\x1b[36m",
            AgentColor::Magenta => "\x1b[35m",
        }
    }

    /// Auto-assign color by index.
    pub fn from_index(i: usize) -> Self {
        match i % 6 {
            0 => AgentColor::Blue,
            1 => AgentColor::Green,
            2 => AgentColor::Yellow,
            3 => AgentColor::Cyan,
            4 => AgentColor::Magenta,
            _ => AgentColor::Red,
        }
    }
}

const RESET: &str = "\x1b[0m";
const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";

// ───────────────────────────── Progress Tracker ─────────────────────────

/// Tracks progress for one agent.
#[derive(Debug, Clone)]
pub struct AgentProgress {
    pub agent_id: String,
    pub display_name: String,
    pub color: AgentColor,
    pub status: AgentStatus,
    pub started_at: Option<Instant>,
    pub ended_at: Option<Instant>,
}

impl AgentProgress {
    pub fn new(agent_id: &str, display_name: &str, color: AgentColor) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            display_name: display_name.to_string(),
            color,
            status: AgentStatus::Pending,
            started_at: None,
            ended_at: None,
        }
    }

    pub fn elapsed(&self) -> Option<Duration> {
        self.started_at.map(|s| {
            self.ended_at
                .unwrap_or_else(Instant::now)
                .duration_since(s)
        })
    }

    /// Render a single-line status for this agent.
    pub fn render_line(&self) -> String {
        let color = self.color.ansi_fg();
        let elapsed = self
            .elapsed()
            .map(|d| format!(" ({}s)", d.as_secs()))
            .unwrap_or_default();

        match &self.status {
            AgentStatus::Pending => {
                format!(
                    "{color}⏳ {}{RESET}{DIM} pending{RESET}",
                    self.display_name
                )
            }
            AgentStatus::Running {
                current_turn,
                max_turns,
                last_tool,
            } => {
                let bar = progress_bar(*current_turn, *max_turns, 15);
                let tool_info = last_tool
                    .as_deref()
                    .map(|t| format!(" → {t}"))
                    .unwrap_or_default();
                format!(
                    "{color}▶ {}{RESET} {bar} {current_turn}/{max_turns}{tool_info}{DIM}{elapsed}{RESET}",
                    self.display_name
                )
            }
            AgentStatus::Completed {
                turns_used,
                tool_calls,
            } => {
                format!(
                    "{color}✓ {}{RESET}{DIM} done ({turns_used} turns, {tool_calls} tools){elapsed}{RESET}",
                    self.display_name
                )
            }
            AgentStatus::Failed { reason } => {
                format!(
                    "\x1b[31m✗ {}{RESET}{DIM} failed: {reason}{elapsed}{RESET}",
                    self.display_name
                )
            }
            AgentStatus::Paused { reason } => {
                format!(
                    "{color}⏸ {}{RESET}{DIM} paused: {reason}{RESET}",
                    self.display_name
                )
            }
        }
    }
}

/// Simple text progress bar.
fn progress_bar(current: u32, max: u32, width: usize) -> String {
    if max == 0 {
        return format!("[{}]", " ".repeat(width));
    }
    let filled = ((current as f64 / max as f64) * width as f64).min(width as f64) as usize;
    let empty = width - filled;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(empty))
}

// ───────────────────────────── Multi-Agent Display ──────────────────────

/// Manages progress display for a multi-agent execution round.
pub struct MultiAgentProgress {
    agents: Vec<AgentProgress>,
    title: String,
    coordination: String,
}

impl MultiAgentProgress {
    pub fn new(title: &str, coordination: &str) -> Self {
        Self {
            agents: Vec::new(),
            title: title.to_string(),
            coordination: coordination.to_string(),
        }
    }

    /// Add an agent to track.
    pub fn add_agent(&mut self, agent_id: &str, display_name: &str) {
        let color = AgentColor::from_index(self.agents.len());
        self.agents.push(AgentProgress::new(agent_id, display_name, color));
    }

    /// Update an agent's status.
    pub fn update_status(&mut self, agent_id: &str, status: AgentStatus) {
        if let Some(agent) = self.agents.iter_mut().find(|a| a.agent_id == agent_id) {
            match &status {
                AgentStatus::Running { .. } if agent.started_at.is_none() => {
                    agent.started_at = Some(Instant::now());
                }
                s if s.is_terminal() && agent.ended_at.is_none() => {
                    agent.ended_at = Some(Instant::now());
                }
                _ => {}
            }
            agent.status = status;
        }
    }

    /// Render the full progress display as a string.
    pub fn render(&self) -> String {
        let mut lines = Vec::new();

        // Header
        lines.push(format!(
            "{BOLD}╭─ {} ({}){}",
            self.title, self.coordination, RESET
        ));

        // Agent lines
        for agent in &self.agents {
            lines.push(format!("│ {}", agent.render_line()));
        }

        // Summary footer
        let total = self.agents.len();
        let done = self
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Completed { .. }))
            .count();
        let failed = self
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Failed { .. }))
            .count();
        let running = self
            .agents
            .iter()
            .filter(|a| matches!(a.status, AgentStatus::Running { .. }))
            .count();

        lines.push(format!(
            "{BOLD}╰─ {done}/{total} done, {running} running, {failed} failed{RESET}"
        ));

        lines.join("\n")
    }

    /// Check if all agents have reached a terminal state.
    pub fn all_done(&self) -> bool {
        self.agents.iter().all(|a| a.status.is_terminal())
    }
}

impl fmt::Display for MultiAgentProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_bar_rendering() {
        assert_eq!(progress_bar(0, 10, 10), "[░░░░░░░░░░]");
        assert_eq!(progress_bar(5, 10, 10), "[█████░░░░░]");
        assert_eq!(progress_bar(10, 10, 10), "[██████████]");
        assert_eq!(progress_bar(0, 0, 10), "[          ]");
    }

    #[test]
    fn agent_color_cycle() {
        assert_eq!(AgentColor::from_index(0), AgentColor::Blue);
        assert_eq!(AgentColor::from_index(1), AgentColor::Green);
        assert_eq!(AgentColor::from_index(6), AgentColor::Blue); // wraps
    }

    #[test]
    fn agent_status_terminal() {
        assert!(!AgentStatus::Pending.is_terminal());
        assert!(!AgentStatus::Running {
            current_turn: 1,
            max_turns: 10,
            last_tool: None,
        }
        .is_terminal());
        assert!(AgentStatus::Completed {
            turns_used: 5,
            tool_calls: 10,
        }
        .is_terminal());
        assert!(AgentStatus::Failed {
            reason: "err".into(),
        }
        .is_terminal());
    }

    #[test]
    fn multi_agent_progress_lifecycle() {
        let mut progress = MultiAgentProgress::new("Code Review", "FanOut");
        progress.add_agent("coder-1", "Coder");
        progress.add_agent("reviewer-1", "Reviewer");

        assert!(!progress.all_done());

        progress.update_status(
            "coder-1",
            AgentStatus::Running {
                current_turn: 1,
                max_turns: 10,
                last_tool: Some("read_file".into()),
            },
        );

        let output = progress.render();
        assert!(output.contains("Code Review"));
        assert!(output.contains("FanOut"));
        assert!(output.contains("Coder"));
        assert!(output.contains("Reviewer"));
        assert!(output.contains("read_file"));

        progress.update_status(
            "coder-1",
            AgentStatus::Completed {
                turns_used: 5,
                tool_calls: 12,
            },
        );
        progress.update_status(
            "reviewer-1",
            AgentStatus::Completed {
                turns_used: 3,
                tool_calls: 6,
            },
        );

        assert!(progress.all_done());
        let output = progress.render();
        assert!(output.contains("2/2 done"));
    }

    #[test]
    fn render_includes_failed() {
        let mut progress = MultiAgentProgress::new("Test", "Sequential");
        progress.add_agent("a1", "Agent 1");
        progress.update_status(
            "a1",
            AgentStatus::Failed {
                reason: "timeout".into(),
            },
        );

        let output = progress.render();
        assert!(output.contains("failed"));
        assert!(output.contains("timeout"));
        assert!(output.contains("1 failed"));
    }

    #[test]
    fn render_line_all_statuses() {
        let mut agent = AgentProgress::new("test", "Test Agent", AgentColor::Blue);

        // Pending
        let line = agent.render_line();
        assert!(line.contains("pending"));

        // Running
        agent.status = AgentStatus::Running {
            current_turn: 3,
            max_turns: 10,
            last_tool: Some("grep".into()),
        };
        agent.started_at = Some(Instant::now());
        let line = agent.render_line();
        assert!(line.contains("3/10"));
        assert!(line.contains("grep"));

        // Completed
        agent.status = AgentStatus::Completed {
            turns_used: 7,
            tool_calls: 20,
        };
        agent.ended_at = Some(Instant::now());
        let line = agent.render_line();
        assert!(line.contains("done"));
        assert!(line.contains("7 turns"));

        // Failed
        agent.status = AgentStatus::Failed {
            reason: "rate limit".into(),
        };
        let line = agent.render_line();
        assert!(line.contains("rate limit"));

        // Paused
        agent.status = AgentStatus::Paused {
            reason: "gate check".into(),
        };
        let line = agent.render_line();
        assert!(line.contains("gate check"));
    }
}
