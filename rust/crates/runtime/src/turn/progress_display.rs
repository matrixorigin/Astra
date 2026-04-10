//! CLI progress display adapter.
//!
//! Connects the `ProgressBroadcaster` event stream to the terminal UI
//! via `MultiAgentProgress` rendering.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

use crate::orchestration::{AgentProgressEvent, ProgressBroadcaster, ProgressEventType};
use crate::turn::agent_progress_ui::{AgentColor, AgentProgress, AgentStatus, MultiAgentProgress};

/// Terminal progress display mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressDisplayMode {
    /// No progress display (silent).
    Silent,
    /// Single-line status updates (good for non-interactive).
    SingleLine,
    /// Multi-line live display with box rendering (interactive TTY).
    MultiLine,
}

/// Manages terminal progress display for spawned agents.
///
/// # Usage
///
/// ```rust,ignore
/// let broadcaster = Arc::new(ProgressBroadcaster::default());
/// let display = ProgressDisplay::new(ProgressDisplayMode::MultiLine);
///
/// // Start display in background task
/// let display_handle = display.start(broadcaster.clone());
///
/// // ... spawn agents, run loop ...
///
/// // Stop display
/// display_handle.stop();
/// ```
pub struct ProgressDisplay {
    mode: ProgressDisplayMode,
    agents: HashMap<String, AgentProgress>,
    start_time: Option<Instant>,
    last_render_lines: usize,
}

impl ProgressDisplay {
    /// Create a new progress display with the given mode.
    pub fn new(mode: ProgressDisplayMode) -> Self {
        Self {
            mode,
            agents: HashMap::new(),
            start_time: None,
            last_render_lines: 0,
        }
    }

    /// Update state from a progress event.
    pub fn handle_event(&mut self, event: AgentProgressEvent) {
        if self.start_time.is_none() {
            self.start_time = Some(Instant::now());
        }

        let agent_id = &event.agent_id;

        match &event.event_type {
            ProgressEventType::Started { description } => {
                let color = AgentColor::from_index(self.agents.len());
                let mut progress = AgentProgress::new(agent_id, description, color);
                progress.started_at = Some(Instant::now());
                progress.status = AgentStatus::Running {
                    current_turn: 0,
                    max_turns: 30, // default
                    last_tool: None,
                };
                self.agents.insert(agent_id.clone(), progress);
            }

            ProgressEventType::TurnCompleted {
                turn,
                tool_calls_this_turn: _,
                activity,
            } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.status = AgentStatus::Running {
                        current_turn: *turn,
                        max_turns: 30,
                        last_tool: Some(activity.clone()),
                    };
                }
            }

            ProgressEventType::Idle => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.status = AgentStatus::Paused {
                        reason: "idle".to_string(),
                    };
                }
            }

            ProgressEventType::Busy { activity } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.status = AgentStatus::Running {
                        current_turn: match &agent.status {
                            AgentStatus::Running { current_turn, .. } => *current_turn,
                            _ => 0,
                        },
                        max_turns: 30,
                        last_tool: Some(activity.clone()),
                    };
                }
            }

            ProgressEventType::Completed {
                result_summary: _,
                total_tool_calls,
                total_tokens: _,
                duration_ms: _,
            } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    let turns_used = match &agent.status {
                        AgentStatus::Running { current_turn, .. } => *current_turn,
                        _ => 0,
                    };
                    agent.status = AgentStatus::Completed {
                        turns_used,
                        tool_calls: *total_tool_calls,
                    };
                    agent.ended_at = Some(Instant::now());
                }
            }

            ProgressEventType::Failed { error } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.status = AgentStatus::Failed {
                        reason: error.clone(),
                    };
                    agent.ended_at = Some(Instant::now());
                }
            }

            ProgressEventType::Cancelled { reason } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.status = AgentStatus::Failed {
                        reason: format!("Cancelled: {reason}"),
                    };
                    agent.ended_at = Some(Instant::now());
                }
            }

            ProgressEventType::PermissionDenied { tool_name, .. } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.display_name = format!("{} (🔒 denied: {tool_name})", agent.display_name);
                }
            }

            ProgressEventType::ToolExecuting {
                tool_name, turn, ..
            } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    let current_turn = match &agent.status {
                        AgentStatus::Running { current_turn, .. } => *current_turn,
                        _ => *turn,
                    };
                    agent.status = AgentStatus::Running {
                        current_turn,
                        max_turns: 30,
                        last_tool: Some(tool_name.clone()),
                    };
                }
            }

            ProgressEventType::LlmCallStarted { turn, .. } => {
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    agent.status = AgentStatus::Running {
                        current_turn: *turn,
                        max_turns: 30,
                        last_tool: Some("thinking…".into()),
                    };
                }
            }

            ProgressEventType::LlmCallCompleted { .. } => {
                // Status stays Running; next event (ToolExecuting or TurnCompleted) will update.
            }

            ProgressEventType::MetricsUpdate {
                turn, max_turns, ..
            } => {
                // Update progress bar based on turn/max_turns ratio
                if let Some(agent) = self.agents.get_mut(agent_id) {
                    if let AgentStatus::Running {
                        current_turn: _,
                        max_turns: _,
                        last_tool,
                    } = &agent.status
                    {
                        agent.status = AgentStatus::Running {
                            current_turn: *turn,
                            max_turns: *max_turns,
                            last_tool: last_tool.clone(),
                        };
                    }
                }
            }
        }
    }

    /// Render and display the current progress state.
    pub fn render(&mut self) -> String {
        match self.mode {
            ProgressDisplayMode::Silent => String::new(),
            ProgressDisplayMode::SingleLine => self.render_single_line(),
            ProgressDisplayMode::MultiLine => self.render_multi_line(),
        }
    }

    /// Display progress (writes to stderr).
    pub fn display(&mut self) {
        if self.mode == ProgressDisplayMode::Silent {
            return;
        }

        let output = self.render();
        if output.is_empty() {
            return;
        }

        let mut stderr = std::io::stderr();
        match self.mode {
            ProgressDisplayMode::SingleLine => {
                // Overwrite single line with carriage return
                let _ = write!(stderr, "\r{}\x1b[K", output);
                let _ = stderr.flush();
            }
            ProgressDisplayMode::MultiLine => {
                // Clear previous lines and render new
                if self.last_render_lines > 0 {
                    // Move cursor up and clear lines
                    for _ in 0..self.last_render_lines {
                        let _ = write!(stderr, "\x1b[1A\x1b[2K");
                    }
                }
                let lines: Vec<&str> = output.lines().collect();
                self.last_render_lines = lines.len();
                for line in &lines {
                    let _ = writeln!(stderr, "{}", line);
                }
                let _ = stderr.flush();
            }
            ProgressDisplayMode::Silent => {}
        }
    }

    /// Clear the display (for multi-line mode).
    pub fn clear(&mut self) {
        if self.mode != ProgressDisplayMode::MultiLine || self.last_render_lines == 0 {
            return;
        }

        let mut stderr = std::io::stderr();
        for _ in 0..self.last_render_lines {
            let _ = write!(stderr, "\x1b[1A\x1b[2K");
        }
        let _ = stderr.flush();
        self.last_render_lines = 0;
    }

    fn render_single_line(&self) -> String {
        let total = self.agents.len();
        if total == 0 {
            return String::new();
        }

        let running = self
            .agents
            .values()
            .filter(|a| matches!(a.status, AgentStatus::Running { .. }))
            .count();
        let done = self
            .agents
            .values()
            .filter(|a| matches!(a.status, AgentStatus::Completed { .. }))
            .count();
        let failed = self
            .agents
            .values()
            .filter(|a| matches!(a.status, AgentStatus::Failed { .. }))
            .count();

        let elapsed = self.start_time.map(|t| t.elapsed().as_secs()).unwrap_or(0);

        // Get activity from most recent running agent
        let activity = self
            .agents
            .values()
            .filter_map(|a| match &a.status {
                AgentStatus::Running { last_tool, .. } => last_tool.clone(),
                _ => None,
            })
            .last()
            .unwrap_or_else(|| "...".to_string());

        format!(
            "🤖 Agents: {done}/{total} done, {running} running, {failed} failed [{elapsed}s] → {activity}"
        )
    }

    fn render_multi_line(&self) -> String {
        if self.agents.is_empty() {
            return String::new();
        }

        let mut progress = MultiAgentProgress::new("Agent Execution", "Dynamic");
        for (agent_id, agent) in &self.agents {
            progress.add_agent(agent_id, &agent.display_name);
            progress.update_status(agent_id, agent.status.clone());
        }
        progress.render()
    }

    /// Check if all tracked agents have completed.
    pub fn all_done(&self) -> bool {
        !self.agents.is_empty() && self.agents.values().all(|a| a.status.is_terminal())
    }
}

/// Handle for controlling a background progress display task.
pub struct ProgressDisplayHandle {
    stop_tx: tokio::sync::oneshot::Sender<()>,
}

impl ProgressDisplayHandle {
    /// Stop the background display task.
    pub fn stop(self) {
        let _ = self.stop_tx.send(());
    }
}

/// Start a background task that subscribes to progress events and displays them.
///
/// Returns a handle that can be used to stop the display.
pub fn start_progress_display(
    broadcaster: Arc<ProgressBroadcaster>,
    mode: ProgressDisplayMode,
) -> ProgressDisplayHandle {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut display = ProgressDisplay::new(mode);
        let mut rx = broadcaster.subscribe();

        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    display.clear();
                    break;
                }
                event = rx.recv() => {
                    match event {
                        Ok(e) => {
                            display.handle_event(e);
                            display.display();
                            if display.all_done() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                            // Dropped events - continue
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    });

    ProgressDisplayHandle { stop_tx }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_rendering() {
        let mut display = ProgressDisplay::new(ProgressDisplayMode::SingleLine);

        display.handle_event(AgentProgressEvent {
            agent_id: "agent-1".to_string(),
            event_type: ProgressEventType::Started {
                description: "Exploring code".to_string(),
            },
            timestamp_epoch_ms: 0,
        });

        let output = display.render();
        assert!(output.contains("1 running"));
        assert!(output.contains("0/1 done"));

        display.handle_event(AgentProgressEvent {
            agent_id: "agent-1".to_string(),
            event_type: ProgressEventType::TurnCompleted {
                turn: 1,
                tool_calls_this_turn: 2,
                activity: "reading files".to_string(),
            },
            timestamp_epoch_ms: 0,
        });

        let output = display.render();
        assert!(output.contains("reading files"));
    }

    #[test]
    fn multi_line_rendering() {
        let mut display = ProgressDisplay::new(ProgressDisplayMode::MultiLine);

        display.handle_event(AgentProgressEvent {
            agent_id: "explore-1".to_string(),
            event_type: ProgressEventType::Started {
                description: "Analyzing codebase".to_string(),
            },
            timestamp_epoch_ms: 0,
        });

        display.handle_event(AgentProgressEvent {
            agent_id: "review-1".to_string(),
            event_type: ProgressEventType::Started {
                description: "Code review".to_string(),
            },
            timestamp_epoch_ms: 0,
        });

        let output = display.render();
        assert!(output.contains("Agent Execution"));
        assert!(output.contains("Dynamic"));
        assert!(output.contains("2 running"));
    }

    #[test]
    fn completion_tracking() {
        let mut display = ProgressDisplay::new(ProgressDisplayMode::SingleLine);

        display.handle_event(AgentProgressEvent {
            agent_id: "agent-1".to_string(),
            event_type: ProgressEventType::Started {
                description: "Task".to_string(),
            },
            timestamp_epoch_ms: 0,
        });

        assert!(!display.all_done());

        display.handle_event(AgentProgressEvent {
            agent_id: "agent-1".to_string(),
            event_type: ProgressEventType::Completed {
                result_summary: "Done".to_string(),
                total_tool_calls: 5,
                total_tokens: (1000, 500),
                duration_ms: 3000,
            },
            timestamp_epoch_ms: 0,
        });

        assert!(display.all_done());
    }
}
