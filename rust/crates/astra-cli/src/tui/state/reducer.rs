//! Pure reducer: `(State, Action) -> (State, Vec<Effect>)`.

#![allow(dead_code)]

use super::{Action, CellSnapshot, ScrollPosition, Severity, State, ToolStatus, TurnStatus};
use crate::tui::slash_menu::{SlashMenu, is_open_for};

/// Side-effects produced by [`reduce`]. The reducer itself stays pure;
/// the event loop interprets these afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    /// Send a new user prompt to the backend.
    SendPrompt(String),
    /// Interrupt the in-flight turn.
    Interrupt,
    /// Terminal bell / system notification.
    Beep,
    /// Persist current turn to journal.
    PersistJournal,
}

/// Pure state transition. Must not perform I/O.
pub(crate) fn reduce(mut state: State, action: Action) -> (State, Vec<Effect>) {
    let mut effects = Vec::new();

    match action {
        // ── User intent ──────────────────────────────────────────
        Action::SubmitPrompt(raw) => {
            let text = raw.trim();
            if text.is_empty() {
                return (state, effects);
            }
            state.messages.push(CellSnapshot::User {
                text: text.to_string(),
            });
            state.input_draft.clear();
            state.turn_status = TurnStatus::WaitingModel;
            effects.push(Effect::SendPrompt(text.to_string()));
        }

        Action::UpdateDraft(draft) => {
            state.input_draft = draft;
            sync_slash_menu(&mut state);
        }

        Action::CancelTurn => {
            if state.turn_status != TurnStatus::Idle {
                state.turn_status = TurnStatus::Idle;
                effects.push(Effect::Interrupt);
            }
        }

        Action::CyclePermissionMode => {
            state.permission_mode = state.permission_mode.next();
        }

        Action::ScrollUp(n) => {
            let current = match state.viewport_scroll {
                ScrollPosition::Bottom => 0,
                ScrollPosition::Offset(o) => o,
            };
            let next = current.saturating_add(n as usize);
            state.viewport_scroll = ScrollPosition::Offset(next);
        }

        Action::ScrollDown(n) => {
            let current = match state.viewport_scroll {
                ScrollPosition::Bottom => {
                    // Already at bottom — stay.
                    return (state, effects);
                }
                ScrollPosition::Offset(o) => o,
            };
            let next = current.saturating_sub(n as usize);
            state.viewport_scroll = if next == 0 {
                ScrollPosition::Bottom
            } else {
                ScrollPosition::Offset(next)
            };
        }

        Action::ScrollToBottom => {
            state.viewport_scroll = ScrollPosition::Bottom;
        }

        // ── Stream events ────────────────────────────────────────
        Action::Token(chunk) => {
            if state.turn_status == TurnStatus::Idle {
                // Stray token outside a turn — ignore.
                return (state, effects);
            }
            match state.messages.last_mut() {
                Some(CellSnapshot::Assistant { markdown }) => {
                    markdown.push_str(&chunk);
                }
                _ => {
                    state
                        .messages
                        .push(CellSnapshot::Assistant { markdown: chunk });
                }
            }
        }

        Action::ThinkingStarted => {
            state.messages.push(CellSnapshot::Thinking {
                text: String::new(),
                finalized: false,
            });
        }

        Action::ThinkingChunk(chunk) => match state.messages.last_mut() {
            Some(CellSnapshot::Thinking {
                text,
                finalized: false,
            }) => {
                text.push_str(&chunk);
            }
            _ => {
                state.messages.push(CellSnapshot::Thinking {
                    text: chunk,
                    finalized: false,
                });
            }
        },

        Action::ThinkingStopped => {
            if let Some(CellSnapshot::Thinking {
                finalized: flag @ false,
                ..
            }) = state.messages.last_mut()
            {
                *flag = true;
            }
        }

        Action::ToolStarted { name, description } => {
            state.messages.push(CellSnapshot::Tool {
                name: name.clone(),
                description,
                status: ToolStatus::Running,
                duration_ms: None,
                output_summary: None,
                output: None,
            });
            state.turn_status = TurnStatus::ToolRunning { name };
        }

        Action::ToolCompleted {
            name,
            status,
            duration_ms,
            output_summary,
            output,
        } => {
            // Update the most recent matching running Tool cell, if any.
            let updated = state
                .messages
                .iter_mut()
                .rev()
                .find_map(|cell| match cell {
                    CellSnapshot::Tool {
                        name: n,
                        status: s @ ToolStatus::Running,
                        duration_ms: d,
                        output_summary: os,
                        output: o,
                        ..
                    } if *n == name => {
                        *s = status;
                        *d = Some(duration_ms);
                        *os = output_summary.clone();
                        *o = output.clone();
                        Some(())
                    }
                    _ => None,
                });
            if updated.is_none() {
                // Completion for an unknown tool — record a closed cell so
                // nothing is silently dropped.
                state.messages.push(CellSnapshot::Tool {
                    name,
                    description: String::new(),
                    status,
                    duration_ms: Some(duration_ms),
                    output_summary,
                    output,
                });
            }
            state.turn_status = TurnStatus::Streaming;
        }

        Action::WaitingForModel => {
            state.turn_status = TurnStatus::WaitingModel;
        }

        Action::ModelResponding => {
            state.turn_status = TurnStatus::Streaming;
        }

        Action::TurnComplete => {
            state.turn_status = TurnStatus::Idle;
            effects.push(Effect::PersistJournal);
        }

        Action::TurnError(msg) => {
            state.messages.push(CellSnapshot::System {
                severity: Severity::Error,
                text: msg.clone(),
            });
            state.turn_status = TurnStatus::Error(msg);
        }

        // ── Session / system ─────────────────────────────────────
        Action::SessionLoaded(id) => {
            state.session_id = Some(id);
        }

        Action::TokenBudgetUpdated(budget) => {
            state.token_budget = Some(budget);
        }

        // ── Slash menu ───────────────────────────────────────────
        Action::SlashMenuMoveUp => {
            if let Some(menu) = state.slash_menu.as_mut() {
                menu.move_up();
            }
        }

        Action::SlashMenuMoveDown => {
            if let Some(menu) = state.slash_menu.as_mut() {
                menu.move_down();
            }
        }

        Action::SlashMenuAccept => {
            if let Some(menu) = state.slash_menu.as_ref() {
                if let Some(picked) = menu.selected_item() {
                    state.input_draft = format!("{} ", picked.name);
                    state.slash_menu = None;
                }
                // Empty matches: leave draft and menu untouched.
            }
        }
    }

    (state, effects)
}

/// Reconcile `state.slash_menu` with `state.input_draft`.
///
/// - If the draft triggers the menu (leading '/'), open/refresh it using
///   the injected `slash_items`.
/// - Otherwise drop any existing menu.
fn sync_slash_menu(state: &mut State) {
    if is_open_for(&state.input_draft) {
        match state.slash_menu.as_mut() {
            Some(menu) => menu.set_filter(&state.input_draft),
            None => {
                let mut menu = SlashMenu::new(state.slash_items.clone());
                menu.set_filter(&state.input_draft);
                state.slash_menu = Some(menu);
            }
        }
    } else {
        state.slash_menu = None;
    }
}
