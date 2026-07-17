//! TUI outer event loop.
//!
//! Owns [`run_tui_session`] — the entry point that ratatui mode
//! runs under for the lifetime of the interactive session. The loop:
//!
//! 1. Completes business bootstrap (auth, state, task stores,
//!    startup trace) BEFORE entering TUI so startup errors still
//!    land in normal stderr.
//! 2. Installs [`stream_bridge`]'s channels so SSE events from the
//!    chat host flow into the TUI as [`TuiAppEvent`]s.
//! 3. Seeds a [`ChatWidget`], [`BottomPane`], [`StatusIndicator`],
//!    and [`TaskBoardObserver`].
//! 4. Runs a `tokio::select!` over: keyboard events, draw ticks,
//!    approval requests, and mid-turn app events.
//!
//! The draw pipeline lives in `super::draw`; priority is
//! `Active > TaskBoard > Status > NextHint > Empty`.

use std::{collections::VecDeque, sync::Arc, time::Duration};

use crate::lock_recovery::LockRecovery;
#[cfg(test)]
use astra_services::session_journal::ToolCallRecord;
use astra_services::session_journal::{JournalEvent, JournalEventType};
use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::explain_dag::{ExplainTurnMeta, render_explain_dag};

use super::app_event::TuiAppEvent;
use super::bottom_pane::view::BottomPaneViewAction;
use super::bottom_pane::{BottomPane, BottomPaneAction};
use super::chat_widget::UserEvent;
use super::draw::{active_viewport, do_draw};
use super::event::{TuiEvent, TuiEventStream};
use super::frame_requester::FrameRequester;
use super::history_cell::HistoryCell;
use super::keymap::{AppAction, AppKeymap};
use super::render::line_utils::sanitize_lines_for_terminal;
use super::task_status::TaskStatus;
use super::terminal::TerminalGuard;

use super::agent_view::*;
use super::bg_task_proxy::*;
use super::bg_task_rendering::*;
use super::plan_mode::*;
use super::{
    bottom_pane, chat_widget, history_cell, mention_menu, resume_summary, slash_dispatch,
    slash_menu, status_indicator, status_line, stream_bridge, task_board_observer, ui_adapter,
};

const BASH_DETACH_HANDOFF_WAIT: Duration = Duration::from_millis(500);
const BACKGROUND_REGISTRY_SHUTDOWN_WAIT: Duration = Duration::from_secs(2);
const LOCAL_AGENT_RECONCILE_INTERVAL: Duration = Duration::from_millis(500);
const LOCAL_AGENT_SESSION_REBIND_DRAIN: Duration = Duration::from_millis(250);
const RUNTIME_NOTIFICATION_TURN_SENTINEL: &str = "\u{2063}astra-runtime-notification-turn\u{2063}";
const LOCAL_AGENT_SESSION_REBIND_REASON: &str =
    "session changed; local agent belonged to the previous session";

/// A one-shot startup observation. These are deliberately presentation-only:
/// the TUI can accept input before they arrive, and no effect changes the
/// session's authoritative runtime state.
enum StartupUiEffect {
    GitBranch(Option<String>),
    McpCompletions(Vec<(String, String)>),
    ResumeSummary(String),
}

/// Completion of the one-shot `/model` catalog action. The catalog stays
/// structured through the handoff so picking a model does not trigger a
/// second remote fetch just to recover provider/thinking metadata.
enum ModelCatalogEffect {
    Ready(Result<Vec<serde_json::Value>, String>),
}

/// A completed read-only slash action. The payload stays structured until it
/// reaches the workbench, so the UI decides how to present an empty result,
/// a load failure, or an interactive view without parsing display strings.
enum SlashBackgroundReadEffect {
    Clipboard {
        success_message: String,
        result: Result<(), String>,
    },
    Worktrees(Vec<crate::tui::worktrees::model::WorktreeEntry>),
    Timeline {
        session_id: String,
        timeline: crate::tui::timeline::Timeline,
    },
    ResumePicker(crate::tui::session_picker::SessionDiscovery),
    ForkPicker(crate::tui::session_picker::SessionDiscovery),
    SessionHub {
        snapshot: Box<slash_dispatch::SessionHubSnapshot>,
        workspace:
            Box<Result<Option<astra_services::session_workspace::WorkspaceMetadata>, String>>,
    },
    SessionAnalysis {
        session_id: String,
        result: Result<Vec<astra_services::session_journal::JournalEvent>, String>,
    },
    Reflection(Result<crate::cli::slash::slash_state::ReflectSurface, String>),
    Memory(MemoryReadEffect),
    Mcp(String),
    Context(Box<crate::tui::context_panel::ContextBreakdown>),
    Failed {
        action: &'static str,
        error: String,
    },
}

/// Structured completion for a `/memory` read. The event loop receives facts,
/// not preformatted terminal text, so empty states and interactive lists keep
/// the same semantics as a foreground action without blocking input.
enum MemoryReadEffect {
    Health(Result<String, String>),
    Session {
        session_id: String,
        result: Box<Result<MemorySessionSurface, String>>,
    },
    Search {
        query: String,
        stats_view: bool,
        result: Result<Vec<serde_json::Value>, String>,
    },
}

struct MemorySessionSurface {
    record: Option<crate::cli::slash::slash_memory::SessionMemoryRecord>,
    status_hint: Option<crate::cli::slash::slash_memory::SessionMemoryStatusHint>,
    status: crate::cli::slash::slash_memory::SessionMemorySurfaceStatus,
}

struct SlashBackgroundReadCompletion {
    generation: u64,
    effect: SlashBackgroundReadEffect,
}

/// Runs derived turn persistence in order. A turn's canonical journal event is
/// already durable before it reaches this worker; a restart therefore loses no
/// conversation truth and future turns can continue using a fresh queue.
fn spawn_turn_post_commit_worker(
    mut jobs: tokio::sync::mpsc::Receiver<crate::cli::turn::turn_post_commit::TurnPostCommitJob>,
    completions: tokio::sync::mpsc::Sender<
        crate::cli::turn::turn_post_commit::TurnPostCommitCompletion,
    >,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(job) = jobs.recv().await {
            let completion =
                crate::cli::turn::turn_post_commit::execute_turn_post_commit_job(job).await;
            if completions.send(completion).await.is_err() {
                break;
            }
        }
    })
}

/// Applies a presentation-only startup observation after the first frame is
/// already possible. Returns whether the transcript needs flushing.
fn apply_startup_ui_effect(
    effect: StartupUiEffect,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
) -> bool {
    match effect {
        StartupUiEffect::GitBranch(Some(branch)) => {
            bottom_pane.footer.git_branch = Some(branch);
            false
        }
        StartupUiEffect::GitBranch(None) => false,
        StartupUiEffect::McpCompletions(completions) => {
            bottom_pane.update_mcp_completions(completions);
            false
        }
        StartupUiEffect::ResumeSummary(summary) => {
            chat_widget.commit_resume_summary(summary);
            true
        }
    }
}

/// Projects a completed model-catalog request into the workbench. Returns
/// `true` when a picker is now open and its paired response should remain
/// deferred until the picker is resolved.
fn apply_model_catalog_effect(
    effect: ModelCatalogEffect,
    state: &crate::cli::session::session_state::SessionState,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
    cached_catalog: &mut Option<Vec<serde_json::Value>>,
) -> bool {
    match effect {
        ModelCatalogEffect::Ready(Ok(catalog)) => {
            let names = catalog
                .iter()
                .filter_map(crate::cli::slash::slash_router::entry_model_name)
                .map(ToOwned::to_owned)
                .collect();
            *cached_catalog = Some(catalog);
            slash_dispatch::push_model_picker(state, bottom_pane, chat_widget, names)
        }
        ModelCatalogEffect::Ready(Err(error)) => {
            chat_widget.commit_system(history_cell::system::SystemCell::error(error));
            false
        }
    }
}

fn load_worktree_entries() -> Vec<crate::tui::worktrees::model::WorktreeEntry> {
    use crate::tui::worktrees::parse;

    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output();
    let porcelain = match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
    };
    let mut entries = parse(&porcelain);
    for entry in &mut entries {
        let sessions =
            astra_services::session_workspace::list_sessions_by_git_root(&entry.path, None, 50);
        entry.session_count = sessions.len();
        entry.last_session_at = sessions.first().map(|session| session.updated_at.clone());
    }
    entries
}

fn load_session_picker() -> crate::tui::session_picker::SessionDiscovery {
    crate::tui::session_picker::SessionDiscovery::new(
        crate::tui::session_picker::FsSessionSource::new(),
        50,
    )
}

async fn load_memory_session_surface(
    session_id: String,
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
) -> Result<MemorySessionSurface, String> {
    let local_session_id = session_id.clone();
    let local_record = tokio::task::spawn_blocking(move || {
        crate::cli::slash::slash_memory::load_local_session_memory(&local_session_id)
    })
    .await
    .map_err(|error| format!("local session memory read failed: {error}"))?;

    let record = match local_record {
        Some(record) => Some(record),
        None => {
            let token =
                crate::cli::session::session_runtime::fresh_access_token(&api, profile.as_deref())
                    .await
                    .ok_or_else(|| "Not logged in. Use /login.".to_string())?;
            crate::cli::slash::slash_memory::load_remote_session_memory(&api, &token, &session_id)
                .await?
        }
    };

    let status_session_id = session_id.clone();
    tokio::task::spawn_blocking(move || {
        let body_is_empty = record
            .as_ref()
            .is_none_or(|memory| memory.body.trim().is_empty());
        let status_hint = body_is_empty
            .then(|| {
                crate::cli::slash::slash_memory::latest_session_memory_status_hint(
                    &status_session_id,
                )
            })
            .flatten();
        let status = crate::cli::slash::slash_memory::session_memory_surface_status(
            &status_session_id,
            record.as_ref(),
        );
        MemorySessionSurface {
            record,
            status_hint,
            status,
        }
    })
    .await
    .map_err(|error| format!("session memory status read failed: {error}"))
}

async fn load_memory_search(
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
    query: String,
    top_k: usize,
) -> Result<Vec<serde_json::Value>, String> {
    let token = crate::cli::session::session_runtime::fresh_access_token(&api, profile.as_deref())
        .await
        .ok_or_else(|| "Not logged in. Use /login.".to_string())?;
    let payload = serde_json::json!({
        "query": query,
        "top_k": top_k,
    });
    let response = api
        .post_memory_search_json(&token, &payload)
        .await
        .map_err(|error| format!("Memory unreachable: {error}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("Memory search failed (HTTP {status})"));
    }
    let body = response
        .text()
        .await
        .map_err(|error| format!("Memory search response failed: {error}"))?;
    serde_json::from_str(&body).map_err(|_| "Failed to parse memory results.".to_string())
}

fn dispatch_slash_background_read(
    action: slash_dispatch::SlashBackgroundRead,
    generation: u64,
    effect_tx: tokio::sync::mpsc::Sender<SlashBackgroundReadCompletion>,
    tasks: &mut tokio::task::JoinSet<()>,
) {
    tasks.spawn(async move {
        let effect = match action {
            slash_dispatch::SlashBackgroundRead::Clipboard {
                text,
                success_message,
            } => SlashBackgroundReadEffect::Clipboard {
                success_message,
                result: crate::cli::slash::slash_info::copy_to_clipboard_async(text).await,
            },
            slash_dispatch::SlashBackgroundRead::Worktrees => {
                match tokio::task::spawn_blocking(load_worktree_entries).await {
                    Ok(entries) => SlashBackgroundReadEffect::Worktrees(entries),
                    Err(error) => SlashBackgroundReadEffect::Failed {
                        action: "worktree discovery",
                        error: error.to_string(),
                    },
                }
            }
            slash_dispatch::SlashBackgroundRead::Timeline { session_id } => {
                let session_for_worker = session_id.clone();
                match tokio::task::spawn_blocking(move || {
                    crate::tui::timeline::Timeline::new(
                        crate::tui::timeline::JournalTurnSource::new(),
                        &session_for_worker,
                    )
                })
                .await
                {
                    Ok(timeline) => SlashBackgroundReadEffect::Timeline {
                        session_id,
                        timeline,
                    },
                    Err(error) => SlashBackgroundReadEffect::Failed {
                        action: "timeline load",
                        error: error.to_string(),
                    },
                }
            }
            slash_dispatch::SlashBackgroundRead::ResumePicker => {
                match tokio::task::spawn_blocking(load_session_picker).await {
                    Ok(discovery) => SlashBackgroundReadEffect::ResumePicker(discovery),
                    Err(error) => SlashBackgroundReadEffect::Failed {
                        action: "session discovery",
                        error: error.to_string(),
                    },
                }
            }
            slash_dispatch::SlashBackgroundRead::ForkPicker => {
                match tokio::task::spawn_blocking(load_session_picker).await {
                    Ok(discovery) => SlashBackgroundReadEffect::ForkPicker(discovery),
                    Err(error) => SlashBackgroundReadEffect::Failed {
                        action: "session discovery",
                        error: error.to_string(),
                    },
                }
            }
            slash_dispatch::SlashBackgroundRead::SessionHub { snapshot } => {
                let workspace = if snapshot.session_id.is_empty() {
                    Ok(None)
                } else {
                    let session_for_worker = snapshot.session_id.clone();
                    match tokio::task::spawn_blocking(move || {
                        astra_services::session_workspace::read_workspace_optional(
                            &session_for_worker,
                        )
                    })
                    .await
                    {
                        Ok(Ok(workspace)) => Ok(workspace),
                        Ok(Err(error)) => Err(error.to_string()),
                        Err(error) => Err(format!("workspace read task failed: {error}")),
                    }
                };
                SlashBackgroundReadEffect::SessionHub {
                    snapshot,
                    workspace: Box::new(workspace),
                }
            }
            slash_dispatch::SlashBackgroundRead::SessionAnalysis { session_id } => {
                let session_for_worker = session_id.clone();
                let result = match tokio::task::spawn_blocking(move || {
                    astra_services::session_journal::read_journal(&session_for_worker)
                })
                .await
                {
                    Ok(Ok(events)) => Ok(events),
                    Ok(Err(error)) => Err(format!("Failed to read journal: {error}")),
                    Err(error) => Err(format!("Journal read task failed: {error}")),
                };
                SlashBackgroundReadEffect::SessionAnalysis { session_id, result }
            }
            slash_dispatch::SlashBackgroundRead::Reflection {
                session_id,
                api,
                profile,
                token,
                args,
            } => SlashBackgroundReadEffect::Reflection(
                crate::cli::slash::slash_state::load_reflect_report_for_session(
                    &session_id,
                    &api,
                    profile.as_deref(),
                    token.as_deref(),
                    &args,
                )
                .await,
            ),
            slash_dispatch::SlashBackgroundRead::Memory(request) => match request {
                slash_dispatch::MemoryReadRequest::Health => SlashBackgroundReadEffect::Memory(
                    MemoryReadEffect::Health(crate::edge_tools::memoria::memoria_health().await),
                ),
                slash_dispatch::MemoryReadRequest::Session {
                    session_id,
                    api,
                    profile,
                } => {
                    let result =
                        load_memory_session_surface(session_id.clone(), api, profile).await;
                    SlashBackgroundReadEffect::Memory(MemoryReadEffect::Session {
                        session_id,
                        result: Box::new(result),
                    })
                }
                slash_dispatch::MemoryReadRequest::Search {
                    api,
                    profile,
                    query,
                    top_k,
                    stats_view,
                } => {
                    let result = load_memory_search(api, profile, query.clone(), top_k).await;
                    SlashBackgroundReadEffect::Memory(MemoryReadEffect::Search {
                        query,
                        stats_view,
                        result,
                    })
                }
            },
            slash_dispatch::SlashBackgroundRead::Mcp { manager, action } => {
                SlashBackgroundReadEffect::Mcp(
                    slash_dispatch::execute_mcp_read(manager, action).await,
                )
            }
            slash_dispatch::SlashBackgroundRead::Context {
                mut breakdown,
                session_id,
                journal_dir_override,
            } => {
                let read_activity = match session_id {
                    Some(session_id) => {
                        let result = tokio::task::spawn_blocking(move || {
                            let _scope = journal_dir_override
                                .as_deref()
                                .map(astra_services::session_journal::JournalDirGuard::new);
                            astra_services::session_journal::read_journal_append_order(&session_id)
                        })
                        .await;
                        match result {
                            Ok(Ok(events)) => crate::tui::context_panel::model::ReadActivity::Available(
                                crate::tui::context_panel::model::summarize_session_read_activity(
                                    &events,
                                ),
                            ),
                            Ok(Err(error)) => crate::tui::context_panel::model::ReadActivity::Unavailable(
                                format!("local journal could not be read: {error}"),
                            ),
                            Err(error) => crate::tui::context_panel::model::ReadActivity::Unavailable(
                                format!("journal read task failed: {error}"),
                            ),
                        }
                    }
                    None => crate::tui::context_panel::model::ReadActivity::Unavailable(
                        "no durable local session".to_string(),
                    ),
                };
                breakdown.set_read_activity(read_activity);
                SlashBackgroundReadEffect::Context(breakdown)
            }
        };
        let _ = effect_tx
            .send(SlashBackgroundReadCompletion { generation, effect })
            .await;
    });
}

fn apply_memory_read_effect(
    effect: MemoryReadEffect,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
) {
    match effect {
        MemoryReadEffect::Health(Ok(body)) => {
            use crate::tui::bottom_pane::info_view::InfoView;

            let lines = crate::cli::slash::slash_memory::memory_health_lines(&body);
            chat_widget.commit_system(history_cell::system::SystemCell::response(
                "Opened memory health",
            ));
            bottom_pane.push_view(Box::new(
                InfoView::from_plain("Memory Health", lines).with_reopen("/memory health"),
            ));
        }
        MemoryReadEffect::Health(Err(error)) => {
            chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                "Memory health failed: {error}"
            )));
        }
        MemoryReadEffect::Session { session_id, result } => match *result {
            Ok(surface) => {
                let body = surface
                    .record
                    .as_ref()
                    .map(|memory| memory.body.as_str())
                    .unwrap_or_default();
                let summary = surface
                    .record
                    .as_ref()
                    .and_then(|memory| memory.summary.as_deref());
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    crate::cli::slash::slash_memory::format_session_memory_response(
                        summary,
                        body,
                        Some(&session_id),
                        surface
                            .status_hint
                            .as_ref()
                            .map(|hint| hint.summary.as_str()),
                        Some(&surface.status),
                    ),
                ));
            }
            Err(error) => {
                chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                    "Session memory failed: {error}"
                )));
            }
        },
        MemoryReadEffect::Search {
            query,
            stats_view,
            result,
        } => match result {
            Err(error) => {
                chat_widget.commit_system(history_cell::system::SystemCell::error(error));
            }
            Ok(memories) if memories.is_empty() => {
                chat_widget
                    .commit_system(history_cell::system::SystemCell::info("No memories found."));
            }
            Ok(memories) if stats_view => {
                use crate::tui::bottom_pane::info_view::InfoView;

                let lines = crate::cli::slash::slash_memory::memory_stats_lines(&memories);
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    "Opened memory stats",
                ));
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain("Memory Stats", lines).with_reopen("/memory stats"),
                ));
            }
            Ok(memories) => {
                let mut hidden_session_entries = 0usize;
                let entries: Vec<_> = memories
                    .iter()
                    .filter_map(|memory| {
                        let content = memory
                            .get("content")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("?");
                        if crate::cli::slash::slash_memory::is_session_proto(content) {
                            hidden_session_entries += 1;
                            return None;
                        }
                        let id = memory
                            .get("memory_id")
                            .or_else(|| memory.get("id"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        let short_id = &id[..id.len().min(8)];
                        (!id.is_empty()).then(|| {
                            (
                                crate::tui::bottom_pane::list_selection_view::SelectionItem {
                                    name: crate::cli::slash::slash_memory::format_memory_entry_line(
                                        memory,
                                    ),
                                    description: Some(format!("id:{short_id}")),
                                    is_current: false,
                                },
                                crate::tui::bottom_pane::view::ViewResult::Memory(
                                    crate::tui::bottom_pane::view::MemorySelection {
                                        memory_id: id.to_string(),
                                        content: content.to_string(),
                                    },
                                ),
                            )
                        })
                    })
                    .collect();
                let (items, results): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
                if items.is_empty() {
                    let mut message = "No non-session memories found.".to_string();
                    if hidden_session_entries > 0 {
                        message.push_str(" Use /memory session to view session state.");
                    }
                    chat_widget.commit_system(history_cell::system::SystemCell::info(message));
                    return;
                }
                let header = format!(
                    "Memory — {} result{} for: {}{}",
                    items.len(),
                    if items.len() == 1 { "" } else { "s" },
                    query,
                    if hidden_session_entries > 0 {
                        format!(
                            " ({hidden_session_entries} session entr{} hidden)",
                            if hidden_session_entries == 1 {
                                "y"
                            } else {
                                "ies"
                            }
                        )
                    } else {
                        String::new()
                    }
                );
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    "Opened memory browser",
                ));
                bottom_pane.push_view(Box::new(
                    crate::tui::bottom_pane::list_selection_view::ListSelectionView::new(
                        items,
                        Some(header),
                    )
                    .with_results(results)
                    .with_footer_hint("↑↓ navigate · q / Esc close"),
                ));
            }
        },
    }
}

fn apply_slash_background_read_effect(
    effect: SlashBackgroundReadEffect,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
) {
    match effect {
        SlashBackgroundReadEffect::Clipboard {
            success_message,
            result,
        } => match result {
            Ok(()) => chat_widget
                .commit_system(history_cell::system::SystemCell::response(success_message)),
            Err(error) => chat_widget.commit_system(history_cell::system::SystemCell::error(
                format!("Copy failed: {error}"),
            )),
        },
        SlashBackgroundReadEffect::Worktrees(entries) if entries.is_empty() => {
            chat_widget.commit_system(history_cell::system::SystemCell::info(
                "No worktrees found (or `git worktree list` failed).",
            ));
        }
        SlashBackgroundReadEffect::Worktrees(entries) => {
            use crate::tui::bottom_pane::worktrees_view::WorktreesView;
            chat_widget.commit_system(history_cell::system::SystemCell::response(
                "Opened worktrees",
            ));
            bottom_pane.push_view(Box::new(WorktreesView::new(
                crate::tui::worktrees::WorktreeList::new(entries),
            )));
        }
        SlashBackgroundReadEffect::Timeline {
            session_id,
            timeline,
        } => {
            if let Some(error) = timeline.load_error() {
                chat_widget.commit_system(history_cell::system::SystemCell::info(error));
            } else if timeline.is_empty() {
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "No turns recorded yet for session {session_id}."
                )));
            } else {
                use crate::tui::bottom_pane::timeline_view::TimelineView;
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    "Opened timeline",
                ));
                bottom_pane.push_view(Box::new(TimelineView::new(timeline)));
            }
        }
        SlashBackgroundReadEffect::ResumePicker(discovery) => {
            if discovery.total() == 0 {
                chat_widget.commit_system(history_cell::system::SystemCell::info(
                    "No previous sessions found.",
                ));
            } else {
                use crate::tui::bottom_pane::session_picker_view::SessionPickerView;
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    "Opened session picker",
                ));
                bottom_pane.push_view(Box::new(SessionPickerView::new(discovery)));
            }
        }
        SlashBackgroundReadEffect::ForkPicker(discovery) => {
            if discovery.total() == 0 {
                chat_widget.commit_system(history_cell::system::SystemCell::info(
                    "No previous sessions to fork from.",
                ));
            } else {
                use crate::tui::bottom_pane::session_picker_view::SessionPickerView;
                use crate::tui::bottom_pane::view::SessionSelectionIntent;
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    "Opened session fork picker",
                ));
                bottom_pane.push_view(Box::new(
                    SessionPickerView::new(discovery).with_intent(SessionSelectionIntent::Fork),
                ));
            }
        }
        SlashBackgroundReadEffect::SessionHub {
            snapshot,
            workspace,
        } => {
            chat_widget.commit_system(history_cell::system::SystemCell::response(
                "Opened session overview",
            ));
            bottom_pane.push_view(Box::new(slash_dispatch::session_hub_view(
                *snapshot, *workspace,
            )));
        }
        SlashBackgroundReadEffect::SessionAnalysis { session_id, result } => match result {
            Ok(events) if events.is_empty() => {
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "Session {session_id} has no journal events."
                )));
            }
            Ok(events) => {
                let session_short = &session_id[..session_id.len().min(8)];
                chat_widget.commit_system(history_cell::system::SystemCell::response(format!(
                    "Opened session analysis · {session_short}"
                )));
                bottom_pane.push_view(Box::new(slash_dispatch::session_analysis_view(
                    &session_id,
                    &events,
                )));
            }
            Err(error) => {
                chat_widget.commit_system(history_cell::system::SystemCell::error(error));
            }
        },
        SlashBackgroundReadEffect::Reflection(result) => match result {
            Ok(crate::cli::slash::slash_state::ReflectSurface::Diff { body }) => {
                use crate::tui::bottom_pane::info_view::InfoView;
                chat_widget.commit_system(history_cell::system::SystemCell::response(
                    "Opened session reflection delta",
                ));
                bottom_pane.push_view(Box::new(
                    InfoView::from_plain(
                        "Reflection · Session Delta",
                        body.lines().map(str::to_owned).collect(),
                    )
                    .with_primary_workspace(),
                ));
            }
            Ok(crate::cli::slash::slash_state::ReflectSurface::Report { source, body, .. }) => {
                match crate::cli::slash::slash_state::parse_reflection_report(&body) {
                    Ok(report) => {
                        use crate::tui::bottom_pane::info_view::InfoView;
                        let provenance = match source {
                        crate::cli::slash::slash_state::ReflectEvidenceSource::LocalArtifacts => {
                            "locally available canonical artifacts"
                        }
                        crate::cli::slash::slash_state::ReflectEvidenceSource::Server => {
                            "durable server observation"
                        }
                    };
                        chat_widget.commit_system(history_cell::system::SystemCell::response(
                            "Opened session reflection",
                        ));
                        bottom_pane.push_view(Box::new(
                            InfoView::from_reflection("Reflection", provenance, report)
                                .with_primary_workspace(),
                        ));
                    }
                    Err(error) => {
                        chat_widget.commit_system(history_cell::system::SystemCell::error(error));
                    }
                }
            }
            Err(error) => {
                chat_widget.commit_system(history_cell::system::SystemCell::error(error));
            }
        },
        SlashBackgroundReadEffect::Memory(effect) => {
            apply_memory_read_effect(effect, bottom_pane, chat_widget);
        }
        SlashBackgroundReadEffect::Mcp(body) => {
            chat_widget.commit_system(history_cell::system::SystemCell::response(body));
        }
        SlashBackgroundReadEffect::Context(breakdown) => {
            use crate::tui::bottom_pane::context_panel_view::ContextPanelView;

            chat_widget.commit_system(history_cell::system::SystemCell::response(
                "Opened context panel",
            ));
            bottom_pane.push_view(Box::new(ContextPanelView::new(*breakdown)));
        }
        SlashBackgroundReadEffect::Failed { action, error } => {
            chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                "{action} failed: {error}"
            )));
        }
    }
}

fn should_reset_agent_scope(previous: Option<&str>, next: Option<&str>) -> bool {
    previous
        .filter(|session_id| !session_id.is_empty())
        .is_some_and(|previous_session_id| {
            next.filter(|session_id| !session_id.is_empty()) != Some(previous_session_id)
        })
}

fn is_initial_session_binding(previous: Option<&str>, next: Option<&str>) -> bool {
    previous
        .filter(|session_id| !session_id.is_empty())
        .is_none()
        && next.is_some_and(|session_id| !session_id.is_empty())
}

async fn rebuild_local_agent_runtime_after_session_rebind(
    state: &mut crate::cli::session::session_state::SessionState,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Option<super::local_agent_snapshot::LocalAgentSnapshot> {
    state.prepare_for_session_rebind().await;
    let previous_spawner = state.agent_spawner.take();
    state.delegation_engine = None;

    let retired_snapshot = if let Some(spawner) = previous_spawner {
        retire_local_agent_spawner(spawner.clone()).await;
        Some(super::local_agent_snapshot::LocalAgentSnapshot::capture(Some(&spawner)).await)
    } else {
        None
    };

    let Some(token) = crate::cli::session::session_runtime::current_access_token(profile) else {
        tracing::warn!(
            session_id = state.session_id.as_deref().unwrap_or(""),
            "local agent runtime is unavailable after session rebind because no access token is available"
        );
        return retired_snapshot;
    };
    crate::cli::agent_runtime::initialize_multi_agent_runtime(state, api, token, profile).await;
    retired_snapshot
}

async fn retire_local_agent_spawner(
    spawner: Arc<astra_runtime::orchestration::DynamicAgentSpawner>,
) -> usize {
    let active_agent_ids: Vec<_> = spawner
        .get_agent_history(None)
        .await
        .into_iter()
        .filter(|agent| !agent.status.is_terminal())
        .map(|agent| agent.agent_id)
        .collect();
    let mut cancelled = 0;
    for agent_id in active_agent_ids {
        cancelled += usize::from(
            spawner
                .cancel_agent(&agent_id, LOCAL_AGENT_SESSION_REBIND_REASON)
                .await,
        );
    }
    spawner
        .shutdown_and_wait(LOCAL_AGENT_SESSION_REBIND_DRAIN)
        .await;
    cancelled
}

fn ctrl_b_promoted_agent_message(
    agents: &[astra_turn_core::orchestration_types::SpawnedAgentInfo],
) -> String {
    let Some(first) = agents.first() else {
        return "Opened background tasks.".to_string();
    };
    if agents.len() > 1 {
        let group_id = first
            .fanout_slot
            .as_ref()
            .map(|slot| slot.group_id.as_str())
            .unwrap_or("agent group");
        return format!(
            "Backgrounded {group_id} ({} agents). Opened background tasks.",
            agents.len()
        );
    }
    let description = first.description.trim();
    if description.is_empty() {
        format!(
            "Backgrounded agent {}. Opened background tasks.",
            first.agent_id
        )
    } else {
        format!(
            "Backgrounded agent {} ({description}). Opened background tasks.",
            first.agent_id
        )
    }
}

fn should_show_ctrl_b_background_hint(detach_ready: bool) -> bool {
    detach_ready
}

fn set_bash_background_hint_enabled(
    chat_widget: &mut chat_widget::ChatWidget,
    status_indicator: &mut status_indicator::StatusIndicator,
    enabled: bool,
) {
    chat_widget.set_bash_background_hint_enabled(enabled);
    status_indicator.set_bash_background_hint_enabled(enabled);
}

async fn sync_default_model_after_auth(
    api: &astra_thin_client::ThinClient,
    token: &str,
    state: &mut crate::cli::session::session_state::SessionState,
    bottom_pane: &mut BottomPane,
) -> Option<String> {
    let model =
        crate::cli::session::session_runtime::ensure_state_default_model(api, token, state).await?;
    crate::cli::slash::slash_config::set_active_model_for_display(Some(model.clone()));
    bottom_pane.footer.model = Some(model.clone());
    Some(model)
}

async fn install_bash_detach_listener(
    slot: &astra_tools::detach::DetachShellSlot,
    chat_widget: &mut chat_widget::ChatWidget,
    status_indicator: &mut status_indicator::StatusIndicator,
) -> astra_tools::detach::DetachShellListener {
    let (handle, listener) = astra_tools::detach::new_detach_pair();
    *slot.lock().await = Some(handle);
    set_bash_background_hint_enabled(chat_widget, status_indicator, false);
    listener
}

fn bash_detach_hint_enabled(listener: Option<&astra_tools::detach::DetachShellListener>) -> bool {
    let Some(listener) = listener else {
        return false;
    };
    should_show_ctrl_b_background_hint(listener.is_active())
}

type BashDetachHandoffResult = Result<astra_tools::detach::DetachedShellPayload, String>;

async fn await_bash_detach_handoff_with_timeout(
    listener: astra_tools::detach::DetachShellListener,
    timeout: Duration,
) -> BashDetachHandoffResult {
    match tokio::time::timeout(timeout, listener.payload_rx).await {
        Ok(Ok(payload)) => Ok(payload),
        Ok(Err(_)) => Err("bash runner ended before handing off the process.".to_string()),
        Err(_) => Err("bash runner did not hand off the process before timeout.".to_string()),
    }
}

async fn await_bash_detach_handoff(
    listener: astra_tools::detach::DetachShellListener,
) -> BashDetachHandoffResult {
    await_bash_detach_handoff_with_timeout(listener, BASH_DETACH_HANDOFF_WAIT).await
}

fn is_background_task_manage_key(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Down
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::SHIFT)
}

fn is_ctrl_b_background_key(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Char('b')
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReopenTarget {
    Agents,
}

impl ReopenTarget {
    const AGENTS: &'static str = "agents";

    fn as_str(self) -> &'static str {
        match self {
            Self::Agents => Self::AGENTS,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            Self::AGENTS => Some(Self::Agents),
            _ => None,
        }
    }
}

/// Blit any images the `display_sixel` tool queued during the turn.
///
/// Under the inline-viewport TUI the tool cannot write to the terminal itself —
/// the render loop would paint over it (the "white box" symptom). Instead it
/// queues raw sixel bytes and we show each here on a clean, paused screen via
/// `with_restored`, which drops raw mode, hands the whole screen to the image,
/// and forces a full repaint once the user presses Enter to dismiss it.
async fn render_pending_sixel_images(guard: &mut TerminalGuard) {
    for bytes in astra_tools::display_sixel::take_pending_sixel() {
        let display_result = guard
            .with_restored(|| async move {
                tokio::task::spawn_blocking(move || {
                    use std::io::Write as _;
                    let mut out = std::io::stdout();
                    // Home + clear-to-end, blit the image at the top (full height),
                    // then a prompt line.
                    let _ = out.write_all(b"\x1b[H\x1b[J");
                    let _ = out.write_all(&bytes);
                    let _ = out.write_all(b"\r\n\x1b[7m Press Enter to continue \x1b[0m");
                    let _ = out.flush();
                    // Raw mode is off inside `with_restored`, so this is a cooked,
                    // line-buffered read that returns when the user hits Enter.
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                })
                .await
            })
            .await;
        match display_result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(error = %err, "sixel display task failed; skipping rest");
                break;
            }
            Err(err) => {
                // Restoring raw mode / bracketed paste / repaint failed. The TUI may
                // be left in a degraded state; surface it in traces rather than
                // swallowing it, and stop blitting the remaining queued images.
                tracing::warn!(error = %err, "failed to display queued sixel image; skipping rest");
                break;
            }
        }
    }
}

async fn edit_composer_in_external_editor(
    guard: &mut TerminalGuard,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
    initial: String,
) {
    let edit_result = guard
        .with_restored(|| crate::tui::external_editor::edit_in_external_editor_async(initial))
        .await;
    guard.terminal.invalidate_viewport();
    match edit_result {
        Ok(Ok(edited)) => {
            bottom_pane.replace_composer_text(&edited);
        }
        Ok(Err(error)) => {
            commit_external_editor_warning(chat_widget, format!("External editor failed: {error}"));
        }
        Err(error) => {
            commit_external_editor_warning(
                chat_widget,
                format!("Failed to restore TUI after external editor: {error}"),
            );
        }
    }
}

fn commit_external_editor_warning(chat_widget: &mut chat_widget::ChatWidget, message: String) {
    chat_widget.commit_ephemeral_warning(message);
}

fn surface_external_editor_unavailable(chat_widget: &mut chat_widget::ChatWidget) {
    chat_widget.commit_ephemeral_warning(
        "External editor waits until the current turn is idle.\nKeep typing · Enter queues this draft · Ctrl+C stops the run.",
    );
}

/// Drain newly-committed cells from the widget and render each
/// to the terminal scrollback. Single choke point for all
/// "a cell just landed in history" writes — callers don't touch
/// `guard.queue_history_lines` directly for chat content anymore.
/// A trailing blank row separates cells visually.
fn flush_chat_widget(
    guard: &mut TerminalGuard,
    chat_widget: &mut chat_widget::ChatWidget,
    width: u16,
) {
    let new_cells = chat_widget.drain_new_committed();
    if new_cells.is_empty() {
        return;
    }
    guard.queue_history_cells(new_cells, width);
}

#[cfg(test)]
fn render_history_batch_lines(
    cells: &[Arc<dyn history_cell::HistoryCell>],
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    let mut batch = Vec::new();
    for (idx, cell) in cells.iter().enumerate() {
        batch.extend(cell.display_lines(width));
        let next = cells.get(idx + 1).map(|next| next.as_ref());
        batch.extend(std::iter::repeat_n(
            ratatui::text::Line::default(),
            history_cell::separator_rows_after(cell.as_ref(), next),
        ));
    }
    batch
}

fn user_intent_preview(text: &str) -> String {
    let single_line = text.trim().replace('\n', " ↩ ");
    let mut preview: String = single_line.chars().take(120).collect();
    if single_line.chars().count() > 120 {
        preview.push_str("...");
    }
    preview
}

/// One cancellation choke point for keyboard and composer-driven stop
/// requests. Local cancellation is visible to the run before any durable
/// child-task RPCs are attempted, so a service round trip cannot allow another
/// model round to start before cancellation is visible locally.
async fn request_active_run_cancel(
    chat_widget: &mut chat_widget::ChatWidget,
    task_service: Option<&Arc<dyn astra_services::TaskService>>,
    run_control: &crate::cli::turn::local_run_control::LocalRunControl,
    tui_cancel_token: &tokio_util::sync::CancellationToken,
) {
    run_control.request_cancel();
    tui_cancel_token.cancel();

    let ids = chat_widget.in_flight_task_ids().to_vec();
    chat_widget.mark_control_tasks_cancelling(&ids);
    let cancelled_count = ids.len();
    chat_widget.commit_cancel_banner(cancelled_count);

    if !ids.is_empty()
        && let Some(service) = task_service
    {
        let service = service.clone();
        let user_id = Arc::new(crate::cli::cli_config::cli_utils::cli_user_id());
        let errors = super::cancel_fanout::fanout(&ids, move |id| {
            let service = service.clone();
            let user_id = user_id.clone();
            async move {
                service
                    .update_status(user_id.as_str(), &id, astra_services::TaskStatus::Cancelled)
                    .await
            }
        })
        .await;
        for (id, error) in errors {
            tracing::warn!(
                target: "astra_cli::tui",
                task_id = %id,
                error = %error,
                "active-run cancel fan-out: cancel rpc failed"
            );
        }
    }
}

/// Resolve submissions that were accepted while a turn still owned the
/// composer. A normal turn gives them a deterministic next-turn route; an
/// interrupted or failed turn returns every queued byte to the caller for
/// restoration instead of starting work from an uncertain state.
fn settle_followup_submissions(
    queued_followups: &mut VecDeque<String>,
    unapplied_current_turn: impl IntoIterator<Item = String>,
    post_output_current_turn: &mut VecDeque<String>,
    should_start_followups: bool,
) -> Option<String> {
    if should_start_followups {
        queued_followups.extend(unapplied_current_turn);
        queued_followups.append(post_output_current_turn);
        return None;
    }

    let mut restored = std::mem::take(queued_followups);
    restored.extend(unapplied_current_turn);
    restored.append(post_output_current_turn);
    (!restored.is_empty()).then(|| restored.into_iter().collect::<Vec<_>>().join("\n\n"))
}

async fn submit_active_run_guidance(
    run_control: &std::sync::Arc<
        std::sync::Mutex<
            Option<std::sync::Arc<crate::cli::turn::local_run_control::LocalRunControl>>,
        >,
    >,
    text: &str,
) -> Result<crate::cli::turn::local_run_control::LocalUserIntentReceipt, String> {
    let provider = astra_core::sync_poison::recover_mutex_lock(run_control)
        .clone()
        .ok_or_else(|| "Run is changing state. Try again, or press Ctrl+C to stop.".to_string())?;
    provider.accept_guidance(text)
}

async fn submit_active_runtime_notification(
    run_control: &std::sync::Arc<
        std::sync::Mutex<
            Option<std::sync::Arc<crate::cli::turn::local_run_control::LocalRunControl>>,
        >,
    >,
    content: &str,
) -> Result<(), String> {
    let provider = astra_core::sync_poison::recover_mutex_lock(run_control)
        .clone()
        .ok_or_else(|| "active run control is settling".to_string())?;
    provider.accept_runtime_notification(content)
}

fn background_task_event_requires_model_attention(
    event: &super::background_tasks::BgTaskEvent,
) -> bool {
    matches!(
        event,
        super::background_tasks::BgTaskEvent::Completed { .. }
            | super::background_tasks::BgTaskEvent::Failed { .. }
            | super::background_tasks::BgTaskEvent::Killed { .. }
    )
}

/// Service tool-to-workbench background commands in every event-loop phase.
/// The foreground turn owns `&mut SessionState`, so the shared queue and
/// backends are snapshotted before that borrow and drained from its 80ms tick.
/// Without this, `task_output` waits for the event loop that is itself waiting
/// for the active agent, producing a deterministic registry timeout.
async fn drain_background_task_commands(
    commands: &Arc<std::sync::Mutex<Vec<crate::edge_tools::BgTaskCommand>>>,
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    agent_spawner: Option<&Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    list_cache: &Arc<tokio::sync::RwLock<String>>,
) {
    let commands: Vec<_> = commands.lock_recover().drain(..).collect();
    for command in commands {
        match command {
            crate::edge_tools::BgTaskCommand::Kill { task_id, reply } => {
                let _ = reply.send(
                    stop_background_task_with_agents(
                        background_registry,
                        agent_spawner,
                        restored_local_agents,
                        &task_id,
                    )
                    .await
                    .map(|_| ()),
                );
            }
            crate::edge_tools::BgTaskCommand::GetOutputSince {
                task_id,
                offset,
                max_bytes,
                reply,
            } => {
                let _ = reply.send(
                    background_task_output_snapshot_with_agents(
                        background_registry,
                        agent_spawner,
                        restored_local_agents,
                        &task_id,
                        offset,
                        max_bytes,
                    )
                    .await,
                );
            }
            crate::edge_tools::BgTaskCommand::SearchOutput {
                task_id,
                pattern,
                context_lines,
                max_bytes,
                reply,
            } => {
                let _ = reply.send(
                    background_task_output_search_snapshot(
                        background_registry,
                        &task_id,
                        &pattern,
                        context_lines,
                        max_bytes,
                    )
                    .await,
                );
            }
            crate::edge_tools::BgTaskCommand::List { reply } => {
                let rendered = render_background_task_list_xml_with_agents(
                    background_registry,
                    agent_spawner,
                    restored_local_agents,
                )
                .await;
                *list_cache.write().await = rendered.clone();
                let _ = reply.send(rendered);
            }
        }
    }
}

fn transcript_snapshot(
    chat_widget: &chat_widget::ChatWidget,
    width: u16,
    runtime_context: Vec<bottom_pane::transcript_view::TranscriptItem>,
) -> bottom_pane::transcript_view::TranscriptSnapshot {
    use bottom_pane::transcript_view::{TranscriptItem, TranscriptItemId, TranscriptSnapshot};

    let history = chat_widget.history();
    let mut items =
        Vec::with_capacity(history.len() + usize::from(chat_widget.active_cell().is_some()));
    for (index, cell) in history.iter().enumerate() {
        let next = history
            .get(index + 1)
            .map(|next| next.as_ref())
            .or_else(|| chat_widget.active_cell());
        let separator_rows = history_cell::separator_rows_after(cell.as_ref(), next);
        items.push(TranscriptItem::committed(
            TranscriptItemId::from_widget_id(chat_widget.history_cell_id(index)),
            Arc::clone(cell),
            separator_rows,
        ));
    }
    if let Some(active) = chat_widget.active_cell() {
        items.push(transcript_item_for_cell(
            TranscriptItemId::from_widget_id(
                chat_widget
                    .active_cell_id()
                    .expect("active transcript cell must have an identity"),
            ),
            active,
            history_cell::trailing_blank_rows(active),
            width,
        ));
    }
    items.extend(runtime_context);
    TranscriptSnapshot::new(items)
}

/// Project only the mutable root cell as a local suffix for the durable
/// transcript workspace. Committed history is intentionally not copied here:
/// it must arrive through the canonical paged lane with its own stable event
/// identity, never by equal-text matching.
fn active_root_transcript_item(
    chat_widget: &chat_widget::ChatWidget,
    width: u16,
) -> Option<bottom_pane::transcript_view::TranscriptItem> {
    let active = chat_widget.active_cell()?;
    let id = bottom_pane::transcript_view::TranscriptItemId::from_widget_id(
        chat_widget
            .active_cell_id()
            .expect("active transcript cell must have an identity"),
    );
    Some(transcript_item_for_cell(
        id,
        active,
        history_cell::trailing_blank_rows(active),
        width,
    ))
}

fn pending_root_transcript_context(
    bottom_pane: &BottomPane,
) -> Vec<bottom_pane::transcript_view::TranscriptItem> {
    use bottom_pane::transcript_view::{TranscriptItem, TranscriptItemId, TranscriptItemKind};
    use ratatui::text::Line;

    bottom_pane
        .approval_views()
        .into_iter()
        .map(|approval| {
            let mut lines = vec![
                Line::from(format!("Approval pending · {}", approval.header)),
                Line::from(format!("tool · {}", approval.tool)),
            ];
            if let Some(detail) = approval.detail.filter(|detail| !detail.trim().is_empty()) {
                lines.extend(
                    detail
                        .lines()
                        .take(3)
                        .map(|line| Line::from(line.to_string())),
                );
            }
            if !approval.reason.trim().is_empty() {
                lines.push(Line::from(format!("why · {}", approval.reason)));
            }
            TranscriptItem::rendered_kind(
                TranscriptItemId::from_canonical(
                    format!("local-approval:{}", approval.id),
                    "pending",
                ),
                TranscriptItemKind::Tool,
                lines,
                1,
            )
        })
        .collect()
}

fn transcript_item_for_cell(
    id: bottom_pane::transcript_view::TranscriptItemId,
    cell: &dyn history_cell::HistoryCell,
    separator_rows: usize,
    width: u16,
) -> bottom_pane::transcript_view::TranscriptItem {
    use bottom_pane::transcript_view::TranscriptItem;

    // A transcript opened during a long stream is still a live surface. Do
    // not reintroduce per-token full-Markdown layout through this alternate
    // path: the complete formatted reply is available once it finalizes, but
    // the mutable suffix only needs the finite viewport tail.
    if let Some(assistant) = cell
        .as_any_ref()
        .downcast_ref::<history_cell::assistant::AssistantCell>()
        && cell.is_live()
    {
        return TranscriptItem::rendered_cell(
            id,
            cell,
            sanitize_lines_for_terminal(assistant.live_viewport_lines(width, 48)),
            separator_rows,
        );
    }

    if let Some(reasoning) = cell
        .as_any_ref()
        .downcast_ref::<history_cell::reasoning::ReasoningCell>()
    {
        return TranscriptItem::reasoning(id, reasoning.clone(), separator_rows);
    }
    if let Some(tool) = cell
        .as_any_ref()
        .downcast_ref::<history_cell::tool::ToolCell>()
    {
        return TranscriptItem::tool(id, tool.clone(), separator_rows);
    }
    TranscriptItem::rendered_cell(
        id,
        cell,
        sanitize_lines_for_terminal(cell.display_lines(width)),
        separator_rows,
    )
}

/// Make the root conversation visible without applying toggle semantics.
///
/// This is used by the run navigator: selecting the root must never close an
/// already-open transcript that was merely covered by the navigator.
fn open_transcript_view(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    width: u16,
    terminal_height: u16,
) {
    let runtime_context = pending_root_transcript_context(bottom_pane);
    if bottom_pane.transcript_view_is_open() {
        if bottom_pane.uses_local_root_transcript_snapshot() {
            bottom_pane.refresh_transcript_snapshot(
                transcript_snapshot(chat_widget, width, runtime_context),
                width,
            );
        }
        return;
    }

    // A permission/plan modal may have arrived above an already-open root
    // transcript. Reactivate that exact tab rather than rebuilding it: cursor,
    // expanded cells, search state, and its live suffix are part of the
    // conversation, not disposable overlay chrome.
    if bottom_pane.activate_root_transcript() {
        if bottom_pane.uses_local_root_transcript_snapshot() {
            bottom_pane.refresh_transcript_snapshot(
                transcript_snapshot(chat_widget, width, runtime_context),
                width,
            );
        }
        return;
    }
    bottom_pane.push_view(Box::new(
        bottom_pane::transcript_view::TranscriptView::from_snapshot(
            transcript_snapshot(chat_widget, width, runtime_context),
            terminal_height,
            width,
        )
        .with_title("Main conversation · Transcript"),
    ));
}

#[cfg(test)]
fn toggle_local_root_transcript_fallback(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    width: u16,
    terminal_height: u16,
) {
    if bottom_pane.transcript_view_is_open() {
        bottom_pane.close_active_view();
    } else {
        open_transcript_view(chat_widget, bottom_pane, width, terminal_height);
    }
}

/// Open the canonical root-conversation workspace.
///
/// A bound session always reads its durable typed transcript, regardless of
/// whether the user arrived through Ctrl+O or the agent navigator. Before the
/// session exists, the in-memory conversation is the only truthful source and
/// remains an explicitly local fallback.
fn open_root_transcript_workspace(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    width: u16,
    terminal_height: u16,
    backends: ViewActionBackends,
    frame_requester: &FrameRequester,
) {
    if let Some(session_id) = backends
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.trim().is_empty())
        .map(ToString::to_string)
    {
        if bottom_pane.ensure_durable_root_transcript(session_id.clone(), width, terminal_height) {
            dispatch_root_transcript_load(
                session_id,
                bottom_pane::root_transcript_view::RootTranscriptTarget::DurableServer,
                None,
                backends,
            );
        }
    } else {
        open_transcript_view(chat_widget, bottom_pane, width, terminal_height);
    }
    let runtime_context = pending_root_transcript_context(bottom_pane);
    bottom_pane.refresh_root_transcript_live(active_root_transcript_item(chat_widget, width));
    bottom_pane.refresh_root_transcript_context(runtime_context);
    bottom_pane.sync_popups();
    frame_requester.schedule_frame();
}

enum GlobalKeyHandling {
    Handled,
    OpenRootTranscript,
}

fn handle_global_key_action(
    key: crossterm::event::KeyEvent,
    guard: &mut TerminalGuard,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> Option<GlobalKeyHandling> {
    let action = AppKeymap::resolve(key)?;
    match action {
        AppAction::ToggleTranscript => {
            if bottom_pane.transcript_view_is_open() {
                bottom_pane.close_active_view();
                bottom_pane.sync_popups();
                frame_requester.schedule_frame();
                Some(GlobalKeyHandling::Handled)
            } else {
                Some(GlobalKeyHandling::OpenRootTranscript)
            }
        }
        AppAction::ForceRedraw => {
            let _ = guard.terminal.clear();
            guard.terminal.invalidate_viewport();
            frame_requester.schedule_frame();
            Some(GlobalKeyHandling::Handled)
        }
    }
}

/// Drain a plan executor through the same typed event model used by a
/// foreground turn. The executor remains background work; this function only
/// owns its local UI projection and state reconciliation.
///
/// Keeping this in the event loop is intentional: only the workbench owns
/// focus, approvals, scrollback, and frame scheduling. The plan runtime owns
/// execution and emits `PlanUpdate` facts without knowing about terminal UI.
fn drain_plan_updates_into_workbench(
    state: &mut crate::cli::session::session_state::SessionState,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    status_indicator: &mut status_indicator::StatusIndicator,
    frame_requester: &FrameRequester,
) -> bool {
    use crate::cli::plan::plan_executor::PlanUpdate;

    let mut changed = false;
    let mut terminal_update = false;

    loop {
        let update = state
            .plan_handle
            .as_mut()
            .and_then(|handle| handle.try_recv());
        let Some(update) = update else {
            break;
        };
        changed = true;

        match update {
            PlanUpdate::SubtaskStarted {
                id,
                title,
                index,
                total,
            } => {
                state.current_plan_subtask_id = Some(id.clone());
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "Plan · {index}/{total} · {title} ({id})"
                )));
                bottom_pane.set_task_status(TaskStatus::WaitingModel);
                status_indicator.set_state(status_indicator::IndicatorState::WaitingModel {
                    started_at: std::time::Instant::now(),
                });
            }
            PlanUpdate::SubtaskCompleted {
                id,
                title,
                pct,
                elapsed,
                verification_passed,
            } => {
                let elapsed = elapsed
                    .map(crate::cli::plan::plan_monitor::format_duration_short)
                    .map(|value| format!(" · {value}"))
                    .unwrap_or_default();
                let verdict = if verification_passed {
                    "verified"
                } else {
                    "needs review"
                };
                chat_widget.commit_system(history_cell::system::SystemCell::response(format!(
                    "Plan · {pct}% · {title} ({id}) · {verdict}{elapsed}"
                )));
            }
            PlanUpdate::SubtaskRetry {
                id,
                attempt,
                max_retries,
                retries_exhausted,
                failure_hint,
                ..
            } => {
                let retry = if max_retries == 0 {
                    "retrying".to_string()
                } else {
                    format!("attempt {attempt}/{max_retries}")
                };
                let result = if retries_exhausted {
                    "verification exhausted"
                } else {
                    "verification failed; retrying"
                };
                let hint = failure_hint
                    .map(|value| format!(" · {value}"))
                    .unwrap_or_default();
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "Plan · {id} · {result} · {retry}{hint}"
                )));
            }
            PlanUpdate::PlanProgress { .. } | PlanUpdate::ParallelGroupInfo { .. } => {}
            PlanUpdate::SubtaskTurnResult {
                subtask_id,
                prompt_tokens,
                completion_tokens,
                session_id,
                ..
            } => {
                state.total_prompt_tokens = state.total_prompt_tokens.saturating_add(prompt_tokens);
                state.total_completion_tokens = state
                    .total_completion_tokens
                    .saturating_add(completion_tokens);
                state.turn = state.turn.saturating_add(1);
                state.current_plan_subtask_id = Some(subtask_id);
                if state.session_id.is_none() {
                    if let Some(session_id) = session_id {
                        state.set_session_id(session_id);
                    }
                }
                if let Some(event) = chat_widget::translate(
                    TuiAppEvent::TurnComplete,
                    chat_widget::TurnContext::default(),
                ) {
                    chat_widget.handle_event(event);
                }
            }
            PlanUpdate::SubtaskStatusSync { id, status } => {
                crate::cli::plan::plan_monitor::sync_subtask_status(state, &id, status);
            }
            PlanUpdate::DurableStateReturn(durable) => {
                state.durable_task_state = Some(*durable);
            }
            PlanUpdate::Advisory { title, detail } => {
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "{title} · {detail}"
                )));
            }
            PlanUpdate::JournalEvent(event) => {
                if let Some(journal) = state.journal.as_ref() {
                    crate::cli::cli_config::cli_utils::append_journal_event_or_warn(
                        journal,
                        state.session_id.as_deref(),
                        &event,
                        "tui_plan_projection:journal_event",
                    );
                }
            }
            PlanUpdate::HistoryEntry {
                user_msg,
                assistant_msg,
            } => state.history.push((user_msg, assistant_msg)),
            PlanUpdate::DeliveryReport(report) => state.last_delivery_report = Some(report),
            PlanUpdate::VerificationReport(report) => {
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "Plan verification recorded for {}",
                    report.subtask_id
                )));
            }
            PlanUpdate::StreamingEvent { event, .. } => {
                if let Some(event) = stream_bridge::map_stream_event(event) {
                    if let Some(chat_event) =
                        chat_widget::translate(event.clone(), chat_widget::TurnContext::default())
                    {
                        chat_widget.handle_event(chat_event);
                        refresh_open_agent_views_for_event(&event, chat_widget, bottom_pane);
                    }
                    apply_tui_control_event(&event, bottom_pane, chat_widget);
                    handle_app_event(&event, bottom_pane, status_indicator, frame_requester);
                }
            }
            PlanUpdate::ApprovalNeeded {
                tool,
                header,
                detail,
                reason,
                response_tx,
            } => {
                bottom_pane.enqueue_approval(
                    tool.clone(),
                    header,
                    detail,
                    reason,
                    serde_json::Value::Null,
                    response_tx,
                );
                bottom_pane.set_task_status(TaskStatus::WaitingApproval { tool });
                status_indicator.set_state(status_indicator::IndicatorState::AwaitingApproval {
                    started_at: std::time::Instant::now(),
                });
            }
            PlanUpdate::PlanPaused {
                pct,
                remaining,
                elapsed,
                blocked_ids,
            } => {
                state.plan_execution_paused = true;
                let blocked = if blocked_ids.is_empty() {
                    String::new()
                } else {
                    format!(" · blocked by {}", blocked_ids.join(", "))
                };
                chat_widget.commit_system(history_cell::system::SystemCell::response(format!(
                    "Plan paused · {pct}% · {remaining} remaining · {}{blocked}",
                    crate::cli::plan::plan_monitor::format_duration_short(elapsed),
                )));
                let mut extra = serde_json::Map::new();
                extra.insert("pct".to_string(), serde_json::json!(pct));
                extra.insert("remaining".to_string(), serde_json::json!(remaining));
                extra.insert("blocked_ids".to_string(), serde_json::json!(blocked_ids));
                crate::cli::plan::plan_monitor::emit_plan_lifecycle_event(
                    state.journal.as_ref(),
                    state.session_id.as_deref(),
                    state.executing_plan.as_ref(),
                    "Plan execution paused",
                    "paused",
                    extra,
                );
                bottom_pane.set_task_status(TaskStatus::Idle);
                status_indicator.set_state(status_indicator::IndicatorState::Idle);
            }
            PlanUpdate::PlanFinished {
                pct,
                elapsed,
                outcome,
            } => {
                terminal_update = true;
                state.plan_execution_paused = false;
                let stage = outcome.as_str();
                let description = format!("Plan execution {stage}");
                let mut extra = serde_json::Map::new();
                extra.insert("pct".to_string(), serde_json::json!(pct));
                extra.insert(
                    "elapsed_ms".to_string(),
                    serde_json::json!(elapsed.as_millis() as u64),
                );
                extra.insert("outcome".to_string(), serde_json::json!(stage));
                crate::cli::plan::plan_monitor::emit_plan_lifecycle_event(
                    state.journal.as_ref(),
                    state.session_id.as_deref(),
                    state.executing_plan.as_ref(),
                    &description,
                    stage,
                    extra,
                );
                state.plan_execution_last_error = None;
                state.executing_plan = None;
                state.current_plan_subtask_id = None;
                chat_widget.commit_system(history_cell::system::SystemCell::response(format!(
                    "Plan {stage} · {pct}% · {}",
                    crate::cli::plan::plan_monitor::format_duration_short(elapsed),
                )));
                bottom_pane.set_task_status(TaskStatus::Idle);
                status_indicator.set_state(status_indicator::IndicatorState::Idle);
            }
            PlanUpdate::PlanError { error } => {
                terminal_update = true;
                state.plan_execution_paused = false;
                state.plan_execution_last_error = Some(error.clone());
                let mut extra = serde_json::Map::new();
                extra.insert("error".to_string(), serde_json::json!(error));
                crate::cli::plan::plan_monitor::emit_plan_lifecycle_event(
                    state.journal.as_ref(),
                    state.session_id.as_deref(),
                    state.executing_plan.as_ref(),
                    "Plan execution failed",
                    "error",
                    extra,
                );
                state.executing_plan = None;
                state.current_plan_subtask_id = None;
                chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                    "Plan failed: {error}"
                )));
                bottom_pane.set_task_status(TaskStatus::Idle);
                status_indicator.set_state(status_indicator::IndicatorState::Idle);
            }
        }
    }

    let executor_closed = state
        .plan_handle
        .as_ref()
        .is_some_and(|handle| handle.is_finished());
    if terminal_update || executor_closed {
        if let Some(mut handle) = state.plan_handle.take() {
            while let Some(update) = handle.try_recv() {
                crate::cli::plan::plan_monitor::apply_trailing_update(update, state);
            }
        }
    }

    if executor_closed && !terminal_update && !state.plan_execution_paused {
        changed = true;
        let error = "Plan executor stopped without a terminal update.".to_string();
        state.plan_execution_last_error = Some(error.clone());
        state.plan_execution_paused = false;
        let mut extra = serde_json::Map::new();
        extra.insert("error".to_string(), serde_json::json!(error));
        crate::cli::plan::plan_monitor::emit_plan_lifecycle_event(
            state.journal.as_ref(),
            state.session_id.as_deref(),
            state.executing_plan.as_ref(),
            "Plan executor stopped unexpectedly",
            "error",
            extra,
        );
        state.executing_plan = None;
        state.current_plan_subtask_id = None;
        chat_widget.commit_system(history_cell::system::SystemCell::error(
            "Plan stopped unexpectedly. No further plan work will run; inspect the transcript before retrying.",
        ));
        bottom_pane.set_task_status(TaskStatus::Idle);
        status_indicator.set_state(status_indicator::IndicatorState::Idle);
    }

    if changed {
        frame_requester.schedule_frame();
    }
    changed
}

fn handle_task_surface_shortcut(
    key: &crossterm::event::KeyEvent,
    task_board: &task_board_observer::TaskBoardObserver,
    board_expanded: &mut bool,
    board_user_pin: &mut Option<bool>,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    if key.code != crossterm::event::KeyCode::Char('t')
        || !key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        || (bottom_pane.has_active_view() && !bottom_pane.primary_workspace_is_open())
    {
        return false;
    }

    // A focused transcript owns the primary canvas, so the compact board
    // beneath the composer is deliberately not painted. Ctrl+T therefore
    // opens the same canonical projection as a workspace, preserving the
    // conversation tab below it instead of silently toggling an invisible
    // strip. Ctrl+B remains the explicit background-task surface.
    if bottom_pane.conversation_tab_is_open() {
        if key
            .modifiers
            .contains(crossterm::event::KeyModifiers::SHIFT)
        {
            task_board.toggle_view_mode();
        }
        task_board.reveal_completed_for_review();
        bottom_pane.push_view(Box::new(bottom_pane::task_board_view::TaskBoardView::new(
            task_board.active_projection(),
        )));
        frame_requester.schedule_frame();
        return true;
    }

    if key
        .modifiers
        .contains(crossterm::event::KeyModifiers::SHIFT)
    {
        task_board.toggle_view_mode();
        // Ctrl+Shift+T is an explicit request to inspect the cross-session
        // board. Keep it open even if its first read fails; otherwise the
        // user sees no acknowledgement and cannot distinguish an empty board
        // from an unavailable checklist service.
        *board_expanded = true;
        *board_user_pin = Some(true);
        frame_requester.schedule_frame();
        return true;
    }

    let new_pin = !*board_expanded;
    *board_user_pin = Some(new_pin);
    *board_expanded = new_pin;
    if new_pin {
        task_board.reveal_completed_for_review();
    } else {
        task_board.hide_completed_after_review();
    }
    frame_requester.schedule_frame();
    true
}

fn handle_agent_monitor_shortcut(
    key: &crossterm::event::KeyEvent,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    if key.code != crossterm::event::KeyCode::Char('g')
        || !key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL)
        // Ctrl+G is global workbench navigation. It can replace another
        // full-canvas workspace (task board, context, a transcript), but a
        // bounded picker/form/approval is actively collecting a decision and
        // must retain focus.
        || (bottom_pane.has_active_view()
            && !bottom_pane.conversation_tab_is_open()
            && !bottom_pane.primary_workspace_is_open())
    {
        return false;
    }
    if !reopen_agents_view(chat_widget, bottom_pane, frame_requester) {
        chat_widget.commit_system(history_cell::system::SystemCell::info(
            "No agent runs yet. Active and recent delegated work will appear here.".to_string(),
        ));
    }
    frame_requester.schedule_frame();
    true
}

/// Switch retained root/agent conversations without first reopening the run
/// tree. Left/Right remain transcript-local hierarchy controls (collapse /
/// expand); Shift+Left/Right switches between peer conversation workspaces.
/// This deliberately avoids Ctrl+Tab and Alt+arrows: terminal emulators,
/// multiplexers, browsers, and operating systems commonly reserve them.
fn handle_conversation_tab_shortcut(
    key: &crossterm::event::KeyEvent,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    if !bottom_pane.conversation_tab_is_open() {
        return false;
    }

    let shift = key
        .modifiers
        .contains(crossterm::event::KeyModifiers::SHIFT);
    let reverse = match key.code {
        crossterm::event::KeyCode::Right if shift => false,
        crossterm::event::KeyCode::Left if shift => true,
        _ => return false,
    };

    if bottom_pane.cycle_conversation_tab(reverse) {
        frame_requester.schedule_frame();
    }
    // A recognized workspace navigation command must not fall through to a
    // transcript item or composer when only one tab is currently open.
    true
}

fn refresh_open_transcript_view(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    width: u16,
) -> bool {
    if !bottom_pane.has_root_transcript_tab() {
        return false;
    }
    let pending_context = pending_root_transcript_context(bottom_pane);
    let durable_live =
        bottom_pane.refresh_root_transcript_live(active_root_transcript_item(chat_widget, width));
    let runtime_context = bottom_pane.refresh_root_transcript_context(pending_context.clone());
    let local_snapshot = if bottom_pane.uses_local_root_transcript_snapshot() {
        bottom_pane.refresh_transcript_snapshot(
            transcript_snapshot(chat_widget, width, pending_context),
            width,
        )
    } else {
        false
    };
    durable_live || runtime_context || local_snapshot
}

fn refresh_skill_popup(
    registry: &astra_runtime::skills::UnifiedSkillRegistry,
    bottom_pane: &mut BottomPane,
) {
    let skill_items = registry
        .all_manifests()
        .into_iter()
        .filter(|manifest| manifest.user_invocable)
        .map(|manifest| bottom_pane::skill_popup::SkillItem {
            name: manifest.name,
            description: manifest.description,
            source: manifest.source.as_str().to_string(),
        })
        .collect();
    bottom_pane.set_skill_items(skill_items);
}

fn apply_external_capability_discovery(
    completion: Result<
        crate::cli::session::session_runtime::ExternalPipelineDiscoveryReport,
        tokio::task::JoinError,
    >,
    registry: &astra_runtime::skills::UnifiedSkillRegistry,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
) {
    refresh_skill_popup(registry, bottom_pane);
    match completion {
        Ok(report) => {
            let mut sources = std::collections::BTreeSet::new();
            let ready = match report.skills {
                Ok(skills) => {
                    for failure in skills.failures {
                        sources.insert(failure.source.as_str().to_string());
                        tracing::warn!(
                            source = failure.source.as_str(),
                            error = %failure.message,
                            "external skill provider unavailable"
                        );
                    }
                    skills.registered.len()
                }
                Err(error) => {
                    sources.insert("catalog".to_string());
                    tracing::warn!(%error, "external skill discovery could not update");
                    registry.len()
                }
            };
            if !report.mcp_failures.is_empty() {
                sources.insert("mcp".to_string());
                for failure in report.mcp_failures {
                    tracing::warn!(
                        server = %failure.name,
                        error = %failure.error,
                        "MCP server unavailable during background discovery"
                    );
                }
            }
            if sources.is_empty() {
                return;
            }
            let sources = sources.into_iter().collect::<Vec<_>>().join(", ");
            chat_widget.commit_ephemeral_warning(format!(
                "Skill sources unavailable: {sources} · {ready} skills ready · Retry: /skill refresh",
            ));
        }
        Err(error) => chat_widget.commit_ephemeral_warning(format!(
            "Skill discovery task stopped unexpectedly · existing local skills remain ready · {error}"
        )),
    }
}

/// Apply structured app events that update both the bottom-pane control
/// surface and transcript projection.
fn apply_tui_control_event(
    event: &TuiAppEvent,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
) {
    match event {
        TuiAppEvent::PermissionAutoApproved { tool, reason } => {
            chat_widget.commit_system(history_cell::system::SystemCell::info(
                astra_turn_core::permission::notice::format_auto_approved_permission(tool, reason)
                    .trim()
                    .to_string(),
            ));
        }
        TuiAppEvent::UserIntentApplied {
            intent_id,
            delivery,
            status,
            event_index,
            content,
        } => {
            if let Some(intent) =
                bottom_pane.apply_user_intent(intent_id, *delivery, *status, content)
            {
                chat_widget.commit_applied_user_intent(
                    intent.intent_id,
                    intent.delivery,
                    intent.status,
                    intent.text,
                );
            } else {
                tracing::debug!(
                    target: "astra_cli::tui",
                    intent_id,
                    event_index,
                    "ignored duplicate or incomplete user-intent applied event"
                );
            }
        }
        TuiAppEvent::AgentCommunication(event)
            if event.direction == astra_turn_types::AgentCommunicationDirection::Received =>
        {
            if let Some(intent) = bottom_pane.remove_agent_guide(&event.message_id) {
                let agent_name = match intent.target {
                    bottom_pane::PendingUserIntentTarget::AgentRun { agent_name, .. } => agent_name,
                    bottom_pane::PendingUserIntentTarget::ActiveRun => return,
                };
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "Guidance received by {agent_name}: {}",
                    intent.text
                )));
            }
        }
        _ => {}
    }
}

fn context_trace_count(state: &crate::cli::session::session_state::SessionState) -> usize {
    state
        .observability_session
        .as_ref()
        .map(|session| {
            let guard = astra_core::sync_poison::recover_rwlock_read(&session);
            guard.context_traces.len()
        })
        .unwrap_or(0)
}

fn total_input_tokens(
    fresh_prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> u64 {
    fresh_prompt_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_creation_tokens)
}

fn context_window_from_trace(
    trace: &ContextAssemblyTrace,
) -> Option<astra_turn_types::ContextWindowUsage> {
    let limit = u64::from(trace.token_budget.max_tokens);
    (limit > 0).then_some(astra_turn_types::ContextWindowUsage {
        used_tokens: u64::from(trace.token_budget.total_used),
        limit_tokens: limit,
        source: trace.token_budget.usage_source,
    })
}

fn latest_context_trace(
    state: &crate::cli::session::session_state::SessionState,
) -> Option<ContextAssemblyTrace> {
    state.latest_context_assembly_trace.clone().or_else(|| {
        let session = state.observability_session.as_ref()?;
        let guard = astra_core::sync_poison::recover_rwlock_read(session);
        guard.context_traces.last().cloned()
    })
}

fn latest_context_trace_since(
    state: &crate::cli::session::session_state::SessionState,
    baseline_cached_turn_id: Option<&str>,
    baseline_count: usize,
) -> Option<ContextAssemblyTrace> {
    if let Some(trace) = state.latest_context_assembly_trace.as_ref()
        && baseline_cached_turn_id != Some(trace.turn_id.as_str())
    {
        return Some(trace.clone());
    }
    let session = state.observability_session.as_ref()?;
    let guard = astra_core::sync_poison::recover_rwlock_read(&session);
    (guard.context_traces.len() > baseline_count)
        .then(|| guard.context_traces.last().cloned())
        .flatten()
}

fn current_turn_event(
    state: &crate::cli::session::session_state::SessionState,
) -> Option<&JournalEvent> {
    state.last_turn_event.as_ref().filter(|event| {
        event.event_type == JournalEventType::Turn && event.turn == Some(state.turn)
    })
}

fn commit_explain_dag(
    state: &crate::cli::session::session_state::SessionState,
    explain_items: &[serde_json::Value],
    baseline_cached_turn_id: Option<&str>,
    baseline_context_traces: usize,
    chat_widget: &mut chat_widget::ChatWidget,
) -> bool {
    if state.explain == crate::cli::session::session_state::ExplainMode::Off {
        return false;
    }
    let trace = latest_context_trace_since(state, baseline_cached_turn_id, baseline_context_traces);
    let turn_event = current_turn_event(state);
    let meta = turn_event.map(ExplainTurnMeta::from_journal_event);
    let Some(text) = render_explain_dag(
        trace.as_ref(),
        meta.as_ref(),
        explain_items,
        state.explain == crate::cli::session::session_state::ExplainMode::Verbose,
    ) else {
        return false;
    };
    chat_widget.commit_system(history_cell::system::SystemCell::info(text));
    true
}

#[derive(Debug)]
enum AgentWorkbenchOutcome {
    Clipboard {
        success_message: String,
        result: Result<(), String>,
    },
    LocalJournalRuns {
        session_id: String,
        runs: Vec<crate::tui::local_agent_journal::LocalJournalAgentRun>,
    },
    ControlAccepted {
        agent_id: String,
        action: astra_thin_client::SessionRunAction,
    },
    ControlContinuationRequired {
        agent_id: String,
        session_id: String,
        source_run_id: String,
    },
    ControlRejected {
        agent_id: String,
        action: astra_thin_client::SessionRunAction,
        reason: String,
    },
    GuideAccepted {
        intent_id: String,
    },
    GuideApplied {
        intent_id: String,
        agent_name: String,
        content: String,
    },
    GuideRejected {
        intent_id: String,
        agent_id: String,
        agent_name: String,
        run_id: String,
        target: crate::tui::agent_run_projection::AgentControlTarget,
        content: String,
        reason: String,
    },
    GuideApplicationUnconfirmed {
        intent_id: String,
        agent_name: String,
        reason: String,
    },
    TranscriptUpdate(bottom_pane::agent_transcript_view::AgentTranscriptUpdate),
    RootTranscriptUpdate(bottom_pane::root_transcript_view::RootTranscriptUpdate),
}

/// Load the durable local-agent index without delaying input or rendering.
/// This is one bounded read per session binding; its typed result is ignored
/// if the user moves to another session before it completes.
fn dispatch_local_agent_journal_load(
    session_id: String,
    outcome_tx: tokio::sync::mpsc::Sender<AgentWorkbenchOutcome>,
) {
    tokio::spawn(async move {
        let session_id_for_read = session_id.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::tui::local_agent_journal::load_terminal_runs(&session_id_for_read)
        })
        .await;
        let runs = match result {
            Ok(Ok(runs)) => runs,
            Ok(Err(error)) => {
                tracing::warn!(session_id, %error, "could not rebuild local agent workbench index");
                return;
            }
            Err(error) => {
                tracing::warn!(session_id, %error, "local agent workbench index task failed");
                return;
            }
        };
        let _ = outcome_tx
            .send(AgentWorkbenchOutcome::LocalJournalRuns { session_id, runs })
            .await;
    });
}

const AGENT_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

enum AgentControlExecution {
    Applied,
    SessionContinuationRequired {
        session_id: String,
        source_run_id: String,
    },
}

fn project_agent_control_execution(
    value: &serde_json::Value,
) -> Result<AgentControlExecution, String> {
    match value.get("disposition").and_then(serde_json::Value::as_str) {
        Some("session_continuation_required") => {
            let continuation = value
                .get("continuation")
                .ok_or_else(|| "server omitted the continuation directive".to_string())?;
            let session_id = continuation
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "server omitted continuation session_id".to_string())?;
            let source_run_id = continuation
                .get("source_run_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "server omitted continuation source_run_id".to_string())?;
            Ok(AgentControlExecution::SessionContinuationRequired {
                session_id: session_id.to_string(),
                source_run_id: source_run_id.to_string(),
            })
        }
        Some("applied") => Ok(AgentControlExecution::Applied),
        Some(other) => Err(format!(
            "server returned unknown control disposition '{other}'"
        )),
        None => Err("server omitted the control disposition".to_string()),
    }
}

/// Dispatch an explicit control request to exactly the backend named by the
/// row's typed control target. The UI receives a typed outcome asynchronously;
/// no task/run identity guessing or dual-write fallback is allowed here.
fn dispatch_agent_control(
    agent_id: &str,
    target: crate::tui::agent_run_projection::AgentControlTarget,
    action: astra_thin_client::SessionRunAction,
    backends: ViewActionBackends,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) {
    let ViewActionBackends {
        agent_spawner: spawner,
        delegation_engine,
        api,
        profile,
        agent_workbench_tx: outcome_tx,
        ..
    } = backends;
    let agent_id_owned = agent_id.to_string();
    if !chat_widget.mark_agent_control_pending(&agent_id_owned, action) {
        chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
            "{} is no longer available for {agent_id}.",
            agent_control_action_label(action)
        )));
        bottom_pane.sync_popups();
        frame_requester.schedule_frame();
        return;
    }
    tokio::spawn(async move {
        let result = tokio::time::timeout(
            AGENT_CONTROL_TIMEOUT,
            execute_agent_control(
                target,
                action,
                spawner,
                delegation_engine,
                &api,
                profile.as_deref(),
            ),
        )
        .await;
        let outcome = match result {
            Ok(Ok(AgentControlExecution::Applied)) => AgentWorkbenchOutcome::ControlAccepted {
                agent_id: agent_id_owned,
                action,
            },
            Ok(Ok(AgentControlExecution::SessionContinuationRequired {
                session_id,
                source_run_id,
            })) => AgentWorkbenchOutcome::ControlContinuationRequired {
                agent_id: agent_id_owned,
                session_id,
                source_run_id,
            },
            Ok(Err(reason)) => AgentWorkbenchOutcome::ControlRejected {
                agent_id: agent_id_owned,
                action,
                reason,
            },
            Err(_) => AgentWorkbenchOutcome::ControlRejected {
                agent_id: agent_id_owned,
                action,
                reason: format!(
                    "the control backend did not acknowledge {} in time",
                    agent_control_action_label(action).to_lowercase()
                ),
            },
        };
        let _ = outcome_tx.send(outcome).await;
    });
    bottom_pane.sync_popups();
    frame_requester.schedule_frame();
}

async fn execute_agent_control(
    target: crate::tui::agent_run_projection::AgentControlTarget,
    action: astra_thin_client::SessionRunAction,
    spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    delegation_engine: Option<Arc<astra_runtime::server::delegation::engine::DelegationEngine>>,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
) -> Result<AgentControlExecution, String> {
    match target {
        crate::tui::agent_run_projection::AgentControlTarget::LocalAgent { agent_id } => {
            if action != astra_thin_client::SessionRunAction::Cancel {
                return Err("the local runtime does not provide this control action".into());
            }
            let Some(spawner) = spawner else {
                return Err("the local runtime that owns this agent is unavailable".into());
            };
            if spawner
                .cancel_agent(&agent_id, "user-requested via Agent monitor")
                .await
            {
                Ok(AgentControlExecution::Applied)
            } else {
                Err("the local runtime no longer owns an active agent with this identity".into())
            }
        }
        crate::tui::agent_run_projection::AgentControlTarget::LocalDelegatedRun { run_id } => {
            if action != astra_thin_client::SessionRunAction::Cancel {
                return Err("this local delegated run can only be cancelled".into());
            }
            let Some(engine) = delegation_engine else {
                return Err("the local delegation runtime is unavailable".into());
            };
            if engine.cancel_sub_run(&run_id).await {
                Ok(AgentControlExecution::Applied)
            } else {
                Err("the local delegated run is no longer active or controllable".into())
            }
        }
        crate::tui::agent_run_projection::AgentControlTarget::DurableRun { run_id } => {
            let token = crate::cli::session::session_runtime::fresh_access_token(api, profile)
                .await
                .ok_or_else(|| "authentication is unavailable".to_string())?;
            let result = match action {
                astra_thin_client::SessionRunAction::Pause => {
                    api.pause_run(Some(&token), &run_id).await
                }
                astra_thin_client::SessionRunAction::Resume => {
                    api.resume_run(Some(&token), &run_id).await
                }
                astra_thin_client::SessionRunAction::ContinueSession => {
                    api.resume_run(Some(&token), &run_id).await
                }
                astra_thin_client::SessionRunAction::Cancel => {
                    api.cancel_run(Some(&token), &run_id).await
                }
            };
            let value = result.map_err(|error| error.to_string())?;
            if action == astra_thin_client::SessionRunAction::Cancel {
                Ok(AgentControlExecution::Applied)
            } else {
                project_agent_control_execution(&value)
            }
        }
    }
}

fn agent_control_action_label(action: astra_thin_client::SessionRunAction) -> &'static str {
    match action {
        astra_thin_client::SessionRunAction::Pause => "Pause",
        astra_thin_client::SessionRunAction::Resume => "Resume",
        astra_thin_client::SessionRunAction::ContinueSession => "Continue",
        astra_thin_client::SessionRunAction::Cancel => "Cancel",
    }
}

fn drain_agent_workbench_outcomes(
    outcome_rx: &mut tokio::sync::mpsc::Receiver<AgentWorkbenchOutcome>,
    active_session_id: Option<&str>,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) {
    while let Ok(outcome) = outcome_rx.try_recv() {
        match outcome {
            AgentWorkbenchOutcome::Clipboard {
                success_message,
                result,
            } => match result {
                Ok(()) => chat_widget
                    .commit_system(history_cell::system::SystemCell::response(success_message)),
                Err(error) => chat_widget.commit_system(history_cell::system::SystemCell::error(
                    format!("Copy failed: {error}"),
                )),
            },
            AgentWorkbenchOutcome::LocalJournalRuns { session_id, runs } => {
                if active_session_id == Some(session_id.as_str()) {
                    chat_widget.reconcile_local_agent_journal_runs(&runs);
                }
            }
            AgentWorkbenchOutcome::ControlAccepted { agent_id, action } => {
                tracing::debug!(agent_id, ?action, "agent control accepted");
            }
            AgentWorkbenchOutcome::ControlContinuationRequired {
                agent_id,
                session_id,
                source_run_id,
            } => {
                chat_widget.reject_agent_control(&agent_id);
                tracing::info!(
                    %agent_id,
                    %session_id,
                    %source_run_id,
                    "agent requires session continuation"
                );
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "{agent_id}'s previous executor is no longer running. Continue in the main conversation; Astra will restore its durable history and checkpoint evidence without pretending the old process resumed."
                )));
            }
            AgentWorkbenchOutcome::ControlRejected {
                agent_id,
                action,
                reason,
            } => {
                if chat_widget.reject_agent_control(&agent_id) {
                    chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                        "Could not {} {agent_id}: {reason}. Its last confirmed state remains visible.",
                        agent_control_action_label(action).to_lowercase()
                    )));
                } else {
                    tracing::debug!(
                        agent_id,
                        reason,
                        "agent control rejection arrived after authoritative state changed"
                    );
                }
            }
            AgentWorkbenchOutcome::GuideAccepted { intent_id } => {
                bottom_pane.promote_agent_guide_accepted(&intent_id);
            }
            AgentWorkbenchOutcome::GuideApplied {
                intent_id,
                agent_name,
                content,
            } => {
                if bottom_pane.remove_agent_guide(&intent_id).is_some() {
                    chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                        "Guidance applied to {agent_name}: {content}"
                    )));
                }
            }
            AgentWorkbenchOutcome::GuideRejected {
                intent_id,
                agent_id,
                agent_name,
                run_id,
                target,
                content,
                reason,
            } => {
                bottom_pane.remove_agent_guide(&intent_id);
                bottom_pane.push_view(Box::new(
                    bottom_pane::agent_guide_view::AgentGuideView::with_draft(
                        agent_id,
                        agent_name.clone(),
                        run_id,
                        target,
                        content,
                        format!("Not sent: {reason}"),
                    ),
                ));
                chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                    "Could not send guidance to {agent_name}: {reason}. Your draft is preserved."
                )));
            }
            AgentWorkbenchOutcome::GuideApplicationUnconfirmed {
                intent_id,
                agent_name,
                reason,
            } => {
                if bottom_pane.remove_agent_guide(&intent_id).is_some() {
                    chat_widget.commit_system(history_cell::system::SystemCell::warning(format!(
                        "{agent_name} accepted the guidance, but application could not be confirmed: {reason}. It was not resent."
                    )));
                }
            }
            AgentWorkbenchOutcome::TranscriptUpdate(update) => {
                if !bottom_pane.refresh_agent_transcript(update) {
                    tracing::debug!("agent transcript update arrived after its view closed");
                }
            }
            AgentWorkbenchOutcome::RootTranscriptUpdate(update) => {
                if !bottom_pane.refresh_root_transcript(update) {
                    tracing::debug!("root transcript update arrived after its view closed");
                }
            }
        }
        refresh_open_agent_views(chat_widget, bottom_pane);
        frame_requester.schedule_frame();
    }
}

fn reconcile_server_agent_observer(
    observer: &crate::tui::server_agent_observer::ServerAgentObserver,
    applied_sequence: &mut Option<u64>,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) {
    observer.maybe_refresh();
    let projection = observer.projection();
    if *applied_sequence == Some(projection.sequence) {
        return;
    }
    *applied_sequence = Some(projection.sequence);
    if chat_widget.reconcile_server_agent_projection(&projection) {
        refresh_open_agent_views(chat_widget, bottom_pane);
        frame_requester.schedule_frame();
    }
}

/// Stage an explicitly selected permission policy while a turn owns
/// `SessionState`. The running turn retains the policy/tool surface it was
/// assembled with; the event loop applies this UI intent only after the turn
/// settles. Keeping it out of the active manager prevents a picker from
/// pretending that an in-flight tool changed policy retroactively.
fn stage_permission_mode_for_next_turn(
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
    mode: crate::cli::permission_manager::PermissionMode,
) {
    bottom_pane.stage_permission_mode_for_next_turn(mode);
    chat_widget.commit_system(history_cell::system::SystemCell::response(format!(
        "{} · applies after the current turn",
        slash_dispatch::permission_mode_feedback(mode)
    )));
}

#[derive(Clone)]
struct ViewActionBackends {
    agent_spawner: Option<Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    delegation_engine: Option<Arc<astra_runtime::server::delegation::engine::DelegationEngine>>,
    api: astra_thin_client::ThinClient,
    profile: Option<String>,
    session_id: Option<String>,
    file_writer: Option<super::file_writer::TuiFileWriter>,
    agent_workbench_tx: tokio::sync::mpsc::Sender<AgentWorkbenchOutcome>,
}

/// Dispatch actions emitted by a live projection refresh. This keeps the
/// event loop as the sole owner of effects while allowing an already-open
/// transcript to upgrade itself when later typed metadata makes a durable
/// read possible.
// Central event-loop dispatch deliberately exposes every mutable subsystem it
// may advance, avoiding an ambient bag that could be retained across awaits.
#[allow(clippy::too_many_arguments)]
async fn dispatch_projection_actions(
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    server_agent_observer: &crate::tui::server_agent_observer::ServerAgentObserver,
    server_agent_projection_sequence: &mut Option<u64>,
    task_board: &task_board_observer::TaskBoardObserver,
    plan_task_observer: &crate::tui::plan_task_observer::PlanTaskObserver,
    backends: ViewActionBackends,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
    viewport_width: u16,
    terminal_height: u16,
) {
    while let Some(action) = bottom_pane.take_projection_action() {
        dispatch_bottom_pane_view_action(
            action,
            background_registry,
            server_agent_observer,
            server_agent_projection_sequence,
            task_board,
            plan_task_observer,
            backends.clone(),
            restored_local_agents,
            chat_widget,
            bottom_pane,
            frame_requester,
            viewport_width,
            terminal_height,
        )
        .await;
    }
}

const AGENT_GUIDE_APPLICATION_TIMEOUT: Duration = Duration::from_secs(60);

fn local_transcript_content(message: &serde_json::Value) -> String {
    if let Some(content) = message.get("content").and_then(serde_json::Value::as_str)
        && !content.is_empty()
    {
        return content.to_string();
    }
    if let Some(content) = message.get("content").filter(|value| !value.is_null()) {
        return serde_json::to_string(content).unwrap_or_else(|_| "[structured content]".into());
    }
    let calls = message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let function = call.get("function")?;
            let name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("tool");
            let arguments = function
                .get("arguments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("{}");
            Some(format!("Tool call · {name}\nArguments · {arguments}"))
        })
        .collect::<Vec<_>>();
    if calls.is_empty() {
        "[empty message]".into()
    } else {
        // Tool identity/arguments travel in the structured `tool_calls`
        // projection. Duplicating them into assistant markdown would render
        // one provider message twice.
        String::new()
    }
}

fn local_transcript_tool_calls(
    message: &serde_json::Value,
) -> Vec<astra_thin_client::SessionTranscriptToolCall> {
    message
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|call| {
            let function = call.get("function")?;
            let name = function.get("name")?.as_str()?.to_string();
            let arguments = function
                .get("arguments")
                .map(|value| {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| value.to_string())
                })
                .unwrap_or_else(|| "{}".into());
            Some(astra_thin_client::SessionTranscriptToolCall {
                tool_use_id: call
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name,
                arguments,
            })
        })
        .collect()
}

fn local_transcript_tool_result(
    message: &serde_json::Value,
) -> Option<astra_thin_client::SessionTranscriptToolResult> {
    (message.get("role").and_then(serde_json::Value::as_str) == Some("tool")).then(|| {
        astra_thin_client::SessionTranscriptToolResult {
            tool_use_id: message
                .get("tool_call_id")
                .or_else(|| message.get("call_id"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: message
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            status: message
                .get("status")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            duration_ms: message
                .get("duration_ms")
                .and_then(serde_json::Value::as_u64),
        }
    })
}

fn local_transcript_item(
    session_id: &str,
    payload: astra_services::session_journal::JournalTranscriptItem,
    item_seq: i64,
    created_at: String,
) -> Option<astra_thin_client::SessionTranscriptItem> {
    let role = payload
        .message
        .get("role")
        .and_then(serde_json::Value::as_str)?
        .to_string();
    let reasoning = payload
        .message
        .get("reasoning_content")
        .or_else(|| payload.message.get("reasoning"))
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let evidence = payload
        .message
        .get("evidence")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let source_event_id = (!payload.source_event_id.trim().is_empty())
        .then(|| format!("journal:{}", payload.source_event_id));
    Some(astra_thin_client::SessionTranscriptItem {
        session_id: session_id.to_string(),
        item_seq,
        run_id: Some(payload.run_id),
        role,
        content: local_transcript_content(&payload.message),
        reasoning_status: reasoning.as_ref().map(|_| "done".to_string()),
        reasoning,
        tool_calls: local_transcript_tool_calls(&payload.message),
        tool_result: local_transcript_tool_result(&payload.message),
        evidence,
        source_event_id,
        created_at,
    })
}

fn project_local_agent_transcript_page(
    session_id: &str,
    run_id: &str,
    events: Vec<astra_services::session_journal::JournalEvent>,
    before_seq: Option<i64>,
    limit: usize,
) -> astra_thin_client::SessionTranscriptPage {
    let mut items = events
        .into_iter()
        .filter_map(|event| {
            let payload = event.transcript_item?;
            if payload.run_id != run_id {
                return None;
            }
            let item_seq = i64::try_from(payload.item_seq).ok()?;
            if before_seq.is_some_and(|before| item_seq >= before) {
                return None;
            }
            local_transcript_item(session_id, payload, item_seq, event.ts)
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|item| item.item_seq);
    // A journal append may be retried after an ambiguous local durability
    // result. The run-local sequence is the canonical identity, so retain the
    // first durable item for a sequence without comparing message text.
    items.dedup_by_key(|item| item.item_seq);
    let has_more = items.len() > limit;
    if has_more {
        items.drain(..items.len().saturating_sub(limit));
    }
    let next_before_seq = has_more.then(|| items[0].item_seq);
    astra_thin_client::SessionTranscriptPage {
        session_id: session_id.to_string(),
        items,
        page_refs: Vec::new(),
        next_before_seq,
        has_more,
    }
}

/// Project the root's append-ordered canonical journal lane into the same
/// page contract returned by the server. `item_seq` is a root-conversation
/// cursor, deliberately separate from each run's local item sequence. It is
/// assigned from first-seen durable source identities, never message text.
fn project_local_root_transcript_page(
    session_id: &str,
    events: Vec<astra_services::session_journal::JournalEvent>,
    before_seq: Option<i64>,
    limit: usize,
) -> astra_thin_client::SessionTranscriptPage {
    let mut source_ids = std::collections::HashSet::new();
    let mut root_seq = 0i64;
    let mut items = Vec::new();
    for event in events {
        let Some(payload) = event.transcript_item else {
            continue;
        };
        if payload.agent_id != "root" {
            continue;
        }
        let identity = if payload.source_event_id.trim().is_empty() {
            format!("{}:{}", payload.run_id, payload.item_seq)
        } else {
            payload.source_event_id.clone()
        };
        if !source_ids.insert(identity) {
            continue;
        }
        root_seq = root_seq.saturating_add(1);
        if before_seq.is_some_and(|before| root_seq >= before) {
            continue;
        }
        if let Some(item) = local_transcript_item(session_id, payload, root_seq, event.ts) {
            items.push(item);
        }
    }
    let has_more = items.len() > limit;
    if has_more {
        items.drain(..items.len().saturating_sub(limit));
    }
    let next_before_seq = has_more.then(|| items[0].item_seq);
    astra_thin_client::SessionTranscriptPage {
        session_id: session_id.to_string(),
        items,
        page_refs: Vec::new(),
        next_before_seq,
        has_more,
    }
}

fn load_local_root_transcript_page(
    session_id: &str,
    before_seq: Option<i64>,
    limit: usize,
) -> Result<astra_thin_client::SessionTranscriptPage, String> {
    let events = astra_services::session_journal::read_journal_append_order(session_id)
        .map_err(|error| format!("Could not load local canonical conversation: {error}"))?;
    Ok(project_local_root_transcript_page(
        session_id, events, before_seq, limit,
    ))
}

/// Select a complete initial root-conversation source without mixing cursor
/// domains. A non-empty server response is not necessarily a complete
/// conversation: during replication it can contain only the newest delta.
/// When locally durable history contains more real user/assistant turns, it
/// is the more useful truthful surface until an explicit refresh finds a
/// broader server page. Pagination never switches source mid-navigation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TranscriptCoverage {
    conversation_items: usize,
    detail_items: usize,
}

fn transcript_coverage(page: &astra_thin_client::SessionTranscriptPage) -> TranscriptCoverage {
    let conversation_items = page
        .items
        .iter()
        .filter(|item| {
            item.role == "user" || (item.role == "assistant" && !item.content.trim().is_empty())
        })
        .count();
    let detail_items = page
        .items
        .iter()
        .map(|item| {
            usize::from(
                item.reasoning
                    .as_deref()
                    .is_some_and(|value| !value.is_empty()),
            ) + item.tool_calls.len()
                + usize::from(item.tool_result.is_some())
                + usize::from(item.evidence.is_some())
        })
        .sum();
    TranscriptCoverage {
        conversation_items,
        detail_items,
    }
}

fn select_root_transcript_page(
    before_seq: Option<i64>,
    durable_page: astra_thin_client::SessionTranscriptPage,
    local_page: Option<astra_thin_client::SessionTranscriptPage>,
) -> (
    astra_thin_client::SessionTranscriptPage,
    bottom_pane::root_transcript_view::RootTranscriptSource,
) {
    let Some(local_page) = local_page.filter(|page| !page.items.is_empty()) else {
        return (
            durable_page,
            bottom_pane::root_transcript_view::RootTranscriptSource::DurableServer,
        );
    };

    if before_seq.is_none() && durable_page.items.is_empty() {
        return (
            local_page,
            bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableWhileServerCatchesUp,
        );
    }

    if before_seq.is_none() && transcript_coverage(&local_page) > transcript_coverage(&durable_page)
    {
        return (
            local_page,
            bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableWithBroaderHistory,
        );
    }

    (
        durable_page,
        bottom_pane::root_transcript_view::RootTranscriptSource::DurableServer,
    )
}

/// Build the initial root-transcript update from the two independently
/// durable projections. A server page containing *some* events is not proof
/// that it contains the whole conversation: replication commonly exposes the
/// newest suffix first. Keep source selection at this boundary so Ctrl+O and
/// the conversation navigator cannot accidentally bypass the broader local
/// history policy.
fn initial_root_transcript_update(
    session_id: String,
    durable_page: astra_thin_client::SessionTranscriptPage,
    local_page: Option<astra_thin_client::SessionTranscriptPage>,
) -> bottom_pane::root_transcript_view::RootTranscriptUpdate {
    let (page, source) = select_root_transcript_page(None, durable_page, local_page);
    bottom_pane::root_transcript_view::RootTranscriptUpdate::Loaded {
        session_id,
        page,
        replace: true,
        source,
    }
}

async fn load_local_root_transcript_page_async(
    session_id: String,
    before_seq: Option<i64>,
) -> Result<astra_thin_client::SessionTranscriptPage, String> {
    tokio::task::spawn_blocking(move || {
        load_local_root_transcript_page(&session_id, before_seq, 200)
    })
    .await
    .map_err(|error| format!("local transcript read task failed: {error}"))?
}

async fn try_load_local_root_initial_page(
    session_id: String,
) -> Option<astra_thin_client::SessionTranscriptPage> {
    load_local_root_transcript_page_async(session_id, None)
        .await
        .ok()
        .filter(|page| !page.items.is_empty())
}

fn load_local_agent_transcript_page(
    session_id: &str,
    run_id: &str,
    before_seq: Option<i64>,
    limit: usize,
) -> Result<astra_thin_client::SessionTranscriptPage, String> {
    let events = astra_services::session_journal::read_journal(session_id)
        .map_err(|error| format!("Could not load local conversation: {error}"))?;
    Ok(project_local_agent_transcript_page(
        session_id, run_id, events, before_seq, limit,
    ))
}

async fn try_load_local_agent_initial_page(
    session_id: String,
    run_id: String,
) -> Option<astra_thin_client::SessionTranscriptPage> {
    tokio::task::spawn_blocking(move || {
        load_local_agent_transcript_page(&session_id, &run_id, None, 200)
    })
    .await
    .ok()
    .and_then(Result::ok)
    .filter(|page| !page.items.is_empty())
}

/// A local agent journal is an exact run-scoped history source for edge and
/// CLI runtimes. A non-empty server suffix is not necessarily a complete run
/// transcript, so initial reads select the broader source; the two pagination
/// cursor domains must never be interleaved.
fn select_agent_transcript_page(
    before_seq: Option<i64>,
    durable_page: astra_thin_client::SessionTranscriptPage,
    local_page: Option<astra_thin_client::SessionTranscriptPage>,
) -> (
    astra_thin_client::SessionTranscriptPage,
    bottom_pane::agent_transcript_view::AgentTranscriptSource,
) {
    let Some(local_page) = local_page.filter(|page| !page.items.is_empty()) else {
        return (
            durable_page,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::DurableServer,
        );
    };

    if before_seq.is_none() && durable_page.items.is_empty() {
        return (
            local_page,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWhileServerCatchesUp,
        );
    }

    if before_seq.is_none() && transcript_coverage(&local_page) > transcript_coverage(&durable_page)
    {
        return (
            local_page,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWithBroaderHistory,
        );
    }

    (
        durable_page,
        bottom_pane::agent_transcript_view::AgentTranscriptSource::DurableServer,
    )
}

fn initial_agent_transcript_update(
    agent_id: String,
    run_id: String,
    durable_page: astra_thin_client::SessionTranscriptPage,
    local_page: Option<astra_thin_client::SessionTranscriptPage>,
) -> bottom_pane::agent_transcript_view::AgentTranscriptUpdate {
    let (page, source) = select_agent_transcript_page(None, durable_page, local_page);
    bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Loaded {
        agent_id,
        run_id,
        page,
        replace: true,
        source,
    }
}

fn dispatch_agent_transcript_load(
    agent_id: String,
    session_id: String,
    run_id: String,
    transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget,
    before_seq: Option<i64>,
    backends: ViewActionBackends,
) {
    let ViewActionBackends {
        api,
        profile,
        agent_workbench_tx: outcome_tx,
        ..
    } = backends;
    tokio::spawn(async move {
        let update_run_id = run_id.clone();
        let update = if matches!(
            transcript_target,
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal
        ) {
            let session_id_for_read = session_id.clone();
            let run_id_for_read = run_id.clone();
            match tokio::task::spawn_blocking(move || {
                load_local_agent_transcript_page(
                    &session_id_for_read,
                    &run_id_for_read,
                    before_seq,
                    200,
                )
            })
            .await
            {
                Ok(Ok(page)) => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Loaded {
                    agent_id,
                    run_id: update_run_id.clone(),
                    page,
                    replace: before_seq.is_none(),
                    source:
                        bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalOnly,
                },
                Ok(Err(message)) => {
                    bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                        agent_id,
                        run_id: update_run_id.clone(),
                        message,
                    }
                }
                Err(error) => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                    agent_id,
                    run_id: update_run_id.clone(),
                    message: format!("Could not load local conversation: {error}"),
                },
            }
        } else {
            let local_initial_page = if before_seq.is_none() {
                try_load_local_agent_initial_page(session_id.clone(), run_id.clone()).await
            } else {
                None
            };
            match crate::cli::session::session_runtime::fresh_access_token(&api, profile.as_deref())
                .await
            {
                Some(token) => match api
                    .get_session_transcript(
                        Some(&token),
                        &session_id,
                        astra_thin_client::SessionTranscriptReadScope::Run(&run_id),
                        before_seq,
                        200,
                    )
                    .await
                {
                    Ok(page)
                        if page.session_id == session_id
                            && page.items.iter().all(|item| {
                                item.session_id == session_id
                                    && item.run_id.as_deref() == Some(run_id.as_str())
                            }) =>
                {
                    if before_seq.is_none() {
                        initial_agent_transcript_update(
                            agent_id,
                            update_run_id.clone(),
                            page,
                            local_initial_page,
                        )
                    } else {
                        bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Loaded {
                            agent_id,
                            run_id: update_run_id.clone(),
                            page,
                            replace: false,
                            source: bottom_pane::agent_transcript_view::AgentTranscriptSource::DurableServer,
                        }
                    }
                    }
                    Ok(_) => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                        agent_id,
                        run_id: update_run_id.clone(),
                        message: "Server returned conversation data for a different run.".into(),
                    },
                    Err(error) if before_seq.is_none() => match local_initial_page {
                        Some(page) => {
                            bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Loaded {
                                agent_id,
                                run_id: update_run_id.clone(),
                                page,
                                replace: true,
                                source: bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWhileServerUnavailable,
                            }
                        }
                        _ => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                            agent_id,
                            run_id: update_run_id.clone(),
                            message: format!("Could not load durable conversation: {error}"),
                        },
                    },
                    Err(error) => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                        agent_id,
                        run_id: update_run_id.clone(),
                        message: format!("Could not load durable conversation: {error}"),
                    },
                },
                None if before_seq.is_none() => match local_initial_page {
                    Some(page) => {
                        bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Loaded {
                            agent_id,
                            run_id: update_run_id,
                            page,
                            replace: true,
                            source: bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWhileServerUnavailable,
                        }
                    }
                    _ => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                        agent_id,
                        run_id: update_run_id,
                        message: "Authentication is unavailable; durable conversation was not loaded."
                            .into(),
                    },
                },
                None => bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Failed {
                    agent_id,
                    run_id: update_run_id,
                    message: "Authentication is unavailable; durable conversation was not loaded."
                        .into(),
                },
            }
        };
        let _ = outcome_tx
            .send(AgentWorkbenchOutcome::TranscriptUpdate(update))
            .await;
    });
}

/// Read the root conversation through its typed durable lane. Authenticated
/// deployments prefer the server's canonical projection, but retain an
/// explicitly labelled local journal page when the first server projection
/// has not caught up yet. Standalone CLI reads that journal directly.
fn dispatch_root_transcript_load(
    session_id: String,
    transcript_target: bottom_pane::root_transcript_view::RootTranscriptTarget,
    before_seq: Option<i64>,
    backends: ViewActionBackends,
) {
    let ViewActionBackends {
        api,
        profile,
        file_writer,
        agent_workbench_tx: outcome_tx,
        ..
    } = backends;
    tokio::spawn(async move {
        if let Some(file_writer) = file_writer
            && let Err(error) = file_writer.flush().await
        {
            tracing::warn!(session_id, %error, "could not flush local transcript before root transcript read");
        }
        // Read the local page before selecting an initial server projection.
        // It is needed for every initial server page, not only an empty one:
        // a replicated server may legitimately have a recent suffix while the
        // older canonical history is still available locally.
        let local_initial_page = if before_seq.is_none() {
            try_load_local_root_initial_page(session_id.clone()).await
        } else {
            None
        };
        let update = match transcript_target {
            bottom_pane::root_transcript_view::RootTranscriptTarget::LocalDurable => {
                match load_local_root_transcript_page_async(session_id.clone(), before_seq).await {
                    Ok(page) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Loaded {
                        session_id,
                        page,
                        replace: before_seq.is_none(),
                        source: bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableOnly,
                    },
                    Err(message) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Failed {
                        session_id,
                        message: format!("Could not load local conversation: {message}"),
                    },
                }
            }
            bottom_pane::root_transcript_view::RootTranscriptTarget::DurableServer => {
                match crate::cli::session::session_runtime::fresh_access_token(
                    &api,
                    profile.as_deref(),
                )
                .await
                {
                    Some(token) => match api
                        .get_session_transcript(
                            Some(&token),
                            &session_id,
                            astra_thin_client::SessionTranscriptReadScope::RootConversation,
                            before_seq,
                            200,
                        )
                        .await
                    {
                        Ok(page)
                            if page.session_id == session_id
                                && page.items.iter().all(|item| item.session_id == session_id) =>
                        {
                            if before_seq.is_none() {
                                initial_root_transcript_update(
                                    session_id,
                                    page,
                                    local_initial_page,
                                )
                            } else {
                                bottom_pane::root_transcript_view::RootTranscriptUpdate::Loaded {
                                    session_id,
                                    page,
                                    replace: false,
                                    source: bottom_pane::root_transcript_view::RootTranscriptSource::DurableServer,
                                }
                            }
                        }
                        Ok(_) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Failed {
                            session_id,
                            message: "Server returned conversation data for a different session.".into(),
                        },
                        Err(error) if before_seq.is_none() => {
                            match local_initial_page {
                                Some(page) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Loaded {
                                    session_id,
                                    page,
                                    replace: true,
                                    source: bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableWhileServerUnavailable,
                                },
                                None => bottom_pane::root_transcript_view::RootTranscriptUpdate::Failed {
                                    session_id,
                                    message: format!("Could not load durable conversation: {error}"),
                                },
                            }
                        }
                        Err(error) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Failed {
                            session_id,
                            message: format!("Could not load durable conversation: {error}"),
                        },
                    },
                    // No server credential makes local durable history the only
                    // available root source.  This remains distinct from a
                    // transport failure while authenticated.
                    None => match load_local_root_transcript_page_async(session_id.clone(), before_seq).await {
                        Ok(page) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Loaded {
                            session_id,
                            page,
                            replace: before_seq.is_none(),
                            source: bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableOnly,
                        },
                        Err(message) => bottom_pane::root_transcript_view::RootTranscriptUpdate::Failed {
                            session_id,
                            message: format!("Could not load local conversation: {message}"),
                        },
                    },
                }
            }
        };
        let _ = outcome_tx
            .send(AgentWorkbenchOutcome::RootTranscriptUpdate(update))
            .await;
    });
}

// This is a one-shot UI-to-runtime command boundary; identity, routing, and
// repaint capabilities remain explicit and independently testable.
#[allow(clippy::too_many_arguments)]
fn dispatch_agent_guide(
    agent_id: String,
    agent_name: String,
    run_id: String,
    target: crate::tui::agent_run_projection::AgentControlTarget,
    content: String,
    backends: ViewActionBackends,
    bottom_pane: &mut BottomPane,
    chat_widget: &mut chat_widget::ChatWidget,
    frame_requester: &FrameRequester,
) {
    let intent_id = uuid::Uuid::new_v4().to_string();
    if !bottom_pane.accept_agent_guide(
        intent_id.clone(),
        run_id.clone(),
        agent_name.clone(),
        content.clone(),
    ) {
        chat_widget.commit_system(history_cell::system::SystemCell::error(
            "Could not stage agent guidance. The draft was not sent.",
        ));
        bottom_pane.push_view(Box::new(
            bottom_pane::agent_guide_view::AgentGuideView::with_draft(
                agent_id,
                agent_name,
                run_id,
                target,
                content,
                "Not sent: local validation failed",
            ),
        ));
        frame_requester.schedule_frame();
        return;
    }

    let ViewActionBackends {
        agent_spawner: spawner,
        api,
        profile,
        agent_workbench_tx: outcome_tx,
        ..
    } = backends;
    tokio::spawn(async move {
        if let crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
            agent_id: local_agent_id,
        } = &target
        {
            let result = match spawner {
                Some(spawner) => {
                    spawner
                        .guide_agent(local_agent_id, &intent_id, &content)
                        .await
                }
                None => Err("the local runtime that owns this agent is unavailable".into()),
            };
            if let Err(reason) = result {
                let _ = outcome_tx
                    .send(AgentWorkbenchOutcome::GuideRejected {
                        intent_id,
                        agent_id,
                        agent_name,
                        run_id,
                        target,
                        content,
                        reason,
                    })
                    .await;
                return;
            }
            let _ = outcome_tx
                .send(AgentWorkbenchOutcome::GuideAccepted {
                    intent_id: intent_id.clone(),
                })
                .await;
            tokio::time::sleep(AGENT_GUIDE_APPLICATION_TIMEOUT).await;
            let _ = outcome_tx
                .send(AgentWorkbenchOutcome::GuideApplicationUnconfirmed {
                    intent_id,
                    agent_name,
                    reason: "no matching mailbox-received event arrived within 60 seconds".into(),
                })
                .await;
            return;
        }

        let crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
            run_id: target_run_id,
        } = &target
        else {
            unreachable!();
        };
        if target_run_id != &run_id {
            let _ = outcome_tx
                .send(AgentWorkbenchOutcome::GuideRejected {
                    intent_id,
                    agent_id,
                    agent_name,
                    run_id,
                    target,
                    content,
                    reason: "the selected run identity changed before guidance dispatch".into(),
                })
                .await;
            return;
        }
        let token = match crate::cli::session::session_runtime::fresh_access_token(
            &api,
            profile.as_deref(),
        )
        .await
        {
            Some(token) => token,
            None => {
                let _ = outcome_tx
                    .send(AgentWorkbenchOutcome::GuideRejected {
                        intent_id,
                        agent_id,
                        agent_name,
                        run_id,
                        target,
                        content,
                        reason: "authentication is unavailable".into(),
                    })
                    .await;
                return;
            }
        };
        let request = astra_thin_client::RunUserIntentRequest {
            intent_id: intent_id.clone(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            input: serde_json::json!({ "content": content }),
        };
        let response = match api
            .submit_run_user_intent(Some(&token), &run_id, &request)
            .await
        {
            Ok(response)
                if response.run_id == run_id
                    && response.intent_id == intent_id
                    && response.status == astra_turn_types::UserIntentStatus::AcceptedRemote =>
            {
                response
            }
            Ok(_) => {
                let _ = outcome_tx
                    .send(AgentWorkbenchOutcome::GuideRejected {
                        intent_id,
                        agent_id,
                        agent_name,
                        run_id,
                        target,
                        content,
                        reason: "the server returned an inconsistent acknowledgement".into(),
                    })
                    .await;
                return;
            }
            Err(error) => {
                let _ = outcome_tx
                    .send(AgentWorkbenchOutcome::GuideRejected {
                        intent_id,
                        agent_id,
                        agent_name,
                        run_id,
                        target,
                        content,
                        reason: error.to_string(),
                    })
                    .await;
                return;
            }
        };
        tracing::debug!(
            duplicate = response.duplicate,
            "agent guidance accepted remotely"
        );
        let _ = outcome_tx
            .send(AgentWorkbenchOutcome::GuideAccepted {
                intent_id: intent_id.clone(),
            })
            .await;

        let observed = tokio::time::timeout(AGENT_GUIDE_APPLICATION_TIMEOUT, async {
            let mut stream = api.stream_run(&run_id, 0, Some(&token));
            while let Some(event) = stream.next().await {
                match event {
                    Ok(astra_thin_client::StreamEvent::RunUserIntentApplied {
                        run_id: event_run_id,
                        intent_id: event_intent_id,
                        content: applied_content,
                        ..
                    }) if event_run_id == run_id && event_intent_id == intent_id => {
                        return Ok(applied_content);
                    }
                    Ok(astra_thin_client::StreamEvent::RunFinished { .. })
                    | Ok(astra_thin_client::StreamEvent::RunCancelled { .. }) => {
                        return Err("the agent run ended before a matching applied event".into());
                    }
                    Ok(astra_thin_client::StreamEvent::RunError { message, .. }) => {
                        return Err(format!("the agent run reported an error: {message}"));
                    }
                    Err(error) => return Err(error.to_string()),
                    _ => {}
                }
            }
            Err("the agent event stream ended before application was observed".into())
        })
        .await;
        let outcome = match observed {
            Ok(Ok(applied_content)) => AgentWorkbenchOutcome::GuideApplied {
                intent_id,
                agent_name,
                content: applied_content,
            },
            Ok(Err(reason)) => AgentWorkbenchOutcome::GuideApplicationUnconfirmed {
                intent_id,
                agent_name,
                reason,
            },
            Err(_) => AgentWorkbenchOutcome::GuideApplicationUnconfirmed {
                intent_id,
                agent_name,
                reason: "no matching applied event arrived within 60 seconds".into(),
            },
        };
        let _ = outcome_tx.send(outcome).await;
    });
    frame_requester.schedule_frame();
}

fn projected_truth_for_plan_task(
    truth_state: crate::tui::plan_task_observer::PlanTaskTruthState,
) -> task_board_observer::ProjectedTaskTruthState {
    match truth_state {
        crate::tui::plan_task_observer::PlanTaskTruthState::Unbound => {
            task_board_observer::ProjectedTaskTruthState::NotConfigured
        }
        crate::tui::plan_task_observer::PlanTaskTruthState::Loading => {
            task_board_observer::ProjectedTaskTruthState::Loading
        }
        crate::tui::plan_task_observer::PlanTaskTruthState::Confirmed => {
            task_board_observer::ProjectedTaskTruthState::Confirmed
        }
        crate::tui::plan_task_observer::PlanTaskTruthState::Stale => {
            task_board_observer::ProjectedTaskTruthState::Stale
        }
        crate::tui::plan_task_observer::PlanTaskTruthState::Unavailable => {
            task_board_observer::ProjectedTaskTruthState::Unavailable
        }
    }
}

/// Session identity is the ownership boundary for every workbench projection.
/// Rebind all observer lanes at the same point so local CLI, Edge+Server and
/// Server-only session changes cannot briefly project one session's agents or
/// plans beside another session's task board.
fn rebind_workbench_observers(
    session_id: Option<&str>,
    task_board: &task_board_observer::TaskBoardObserver,
    server_agent_observer: &crate::tui::server_agent_observer::ServerAgentObserver,
    plan_task_observer: &crate::tui::plan_task_observer::PlanTaskObserver,
    board_user_pin: &mut Option<bool>,
) {
    let session_id = session_id.filter(|session_id| !session_id.trim().is_empty());
    task_board.rebind_session(session_id.unwrap_or_default().to_owned());
    server_agent_observer.rebind_session(session_id);
    plan_task_observer.rebind_session(session_id);
    *board_user_pin = None;
}

// Central event-loop dispatch deliberately exposes every mutable subsystem it
// may advance, avoiding an ambient bag that could be retained across awaits.
#[allow(clippy::too_many_arguments)]
async fn dispatch_bottom_pane_view_action(
    action: BottomPaneViewAction,
    background_registry: &mut super::background_tasks::BackgroundTaskRegistry,
    server_agent_observer: &crate::tui::server_agent_observer::ServerAgentObserver,
    server_agent_projection_sequence: &mut Option<u64>,
    task_board: &task_board_observer::TaskBoardObserver,
    plan_task_observer: &crate::tui::plan_task_observer::PlanTaskObserver,
    backends: ViewActionBackends,
    restored_local_agents: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
    viewport_width: u16,
    terminal_height: u16,
) {
    match action {
        BottomPaneViewAction::OpenRootTranscript => {
            open_root_transcript_workspace(
                chat_widget,
                bottom_pane,
                viewport_width,
                terminal_height,
                backends,
                frame_requester,
            );
        }
        BottomPaneViewAction::ReturnToConversationNavigator => {
            // A transcript tab remains alive when the user goes back to the
            // run tree, preserving its live suffix, scroll, search and
            // expansion state. Standalone Ctrl+O has no tree parent, so its
            // existing close behavior is retained without fabricating one.
            if !bottom_pane.activate_agent_monitor() {
                bottom_pane.close_active_view();
            }
            bottom_pane.sync_popups();
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::InspectAgent {
            agent_id,
            agent_name,
            run_id,
            transcript_target,
        } => {
            if let (Some(transcript_target), Some(session_id), Some(run_id)) = (
                transcript_target,
                backends.session_id.clone(),
                run_id.clone(),
            ) {
                let reused_open_tab = bottom_pane.activate_agent_transcript(&agent_id, &run_id);
                if !reused_open_tab {
                    let view = bottom_pane::agent_transcript_view::AgentTranscriptView::loading(
                        agent_id.clone(),
                        agent_name,
                        session_id.clone(),
                        run_id.clone(),
                        transcript_target,
                        ReopenTarget::Agents.as_str(),
                        viewport_width,
                        terminal_height,
                    );
                    bottom_pane.push_view(Box::new(view));
                    replay_agent_live_transcript(chat_widget, bottom_pane, &agent_id, &run_id);
                }
                dispatch_agent_transcript_load(
                    agent_id,
                    session_id,
                    run_id,
                    transcript_target,
                    None,
                    backends,
                );
            } else if let Some(run_id) = run_id {
                // A child can publish an immutable run id before either its
                // parent session binding or its durable transcript location
                // arrives. The typed live stream is still an honest
                // inspectable conversation suffix, so open it now rather
                // than making the user wait for a receipt we do not have.
                if !bottom_pane.activate_agent_transcript(&agent_id, &run_id) {
                    bottom_pane.push_view(Box::new(
                        bottom_pane::agent_transcript_view::AgentTranscriptView::live_unbound(
                            agent_id.clone(),
                            agent_name,
                            run_id.clone(),
                            transcript_target,
                            ReopenTarget::Agents.as_str(),
                            viewport_width,
                            terminal_height,
                        ),
                    ));
                    replay_agent_live_transcript(chat_widget, bottom_pane, &agent_id, &run_id);
                }
                // The session can already be known even when the launch
                // receipt has not yet reported its transcript location.
                // Preserve that real binding now; once the location arrives,
                // the open live conversation can request canonical history
                // without inventing either boundary.
                if let Some(session_id) = backends.session_id.as_deref() {
                    let _ = bottom_pane.bind_open_agent_transcript_session(session_id);
                }
            } else {
                // An agent row is already a real observable object even if
                // its immutable run receipt is still in flight. Open the
                // live conversation now with an explicit pending identity;
                // the view binds only when a typed monitor/live event names
                // the run, never by guessing from display text.
                if !bottom_pane.activate_agent_transcript(&agent_id, "") {
                    bottom_pane.push_view(Box::new(
                        bottom_pane::agent_transcript_view::AgentTranscriptView::live_unbound(
                            agent_id,
                            agent_name,
                            String::new(),
                            transcript_target,
                            ReopenTarget::Agents.as_str(),
                            viewport_width,
                            terminal_height,
                        ),
                    ));
                }
                if let Some(session_id) = backends.session_id.as_deref() {
                    let _ = bottom_pane.bind_open_agent_transcript_session(session_id);
                }
            }
            bottom_pane.sync_popups();
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::LoadRootTranscript {
            session_id,
            transcript_target,
            before_seq,
        } => {
            dispatch_root_transcript_load(session_id, transcript_target, before_seq, backends);
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::RefreshAgentMonitor => {
            if server_agent_observer.request_refresh() {
                server_agent_observer.maybe_refresh();
                reconcile_server_agent_observer(
                    server_agent_observer,
                    server_agent_projection_sequence,
                    chat_widget,
                    bottom_pane,
                    frame_requester,
                );
            }
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::RefreshTaskBoard => {
            // A task board is a projection over two canonical sources. A
            // manual refresh asks both to reconcile; neither source is
            // mutated and an existing request always wins over a duplicate.
            let refresh_started = task_board.request_refresh();
            let plan_refresh_started = plan_task_observer.request_refresh();
            if refresh_started || plan_refresh_started {
                plan_task_observer.maybe_refresh();
                let plan_projection = plan_task_observer.projection();
                task_board.set_projected_task_projection(
                    plan_projection.tasks,
                    projected_truth_for_plan_task(plan_projection.truth_state),
                );
                task_board.set_projected_multi_summaries(plan_projection.multi_session);
                task_board.maybe_refresh();
                bottom_pane.refresh_task_board(&task_board.active_projection());
            }
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::ControlAgent {
            agent_id,
            target,
            action,
        } => {
            dispatch_agent_control(
                &agent_id,
                target,
                action,
                backends,
                chat_widget,
                bottom_pane,
                frame_requester,
            );
            refresh_open_agent_monitor(chat_widget, bottom_pane);
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::BeginAgentGuide {
            agent_id,
            agent_name,
            run_id,
            target,
        } => {
            bottom_pane.push_view(Box::new(
                bottom_pane::agent_guide_view::AgentGuideView::new(
                    agent_id, agent_name, run_id, target,
                ),
            ));
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::SubmitAgentGuide {
            agent_id,
            agent_name,
            run_id,
            target,
            content,
        } => {
            // The guide input was popped by its typed Close disposition. Close
            // the monitor beneath it as well so delivery status is immediately
            // visible in the normal pending-intent band.
            bottom_pane.dismiss_active_agent_monitor();
            dispatch_agent_guide(
                agent_id,
                agent_name,
                run_id,
                target,
                content,
                backends,
                bottom_pane,
                chat_widget,
                frame_requester,
            );
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::LoadAgentTranscript {
            agent_id,
            session_id,
            run_id,
            transcript_target,
            before_seq,
        } => {
            dispatch_agent_transcript_load(
                agent_id,
                session_id,
                run_id,
                transcript_target,
                before_seq,
                backends,
            );
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::ExportTranscript { path, lines } => {
            if let Some(writer) = backends.file_writer {
                writer.rewrite_lines("Transcript export", path.clone(), lines);
                chat_widget.commit_system(history_cell::system::SystemCell::info(format!(
                    "Transcript export queued → {}",
                    path.display()
                )));
            } else {
                chat_widget.commit_system(history_cell::system::SystemCell::warning(
                    "Transcript export is unavailable because the asynchronous file writer is not running."
                        .to_string(),
                ));
            }
            bottom_pane.sync_popups();
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::CopyToClipboard {
            text,
            success_message,
        } => {
            let outcome_tx = backends.agent_workbench_tx.clone();
            tokio::spawn(async move {
                let result = crate::cli::slash::slash_info::copy_to_clipboard_async(text).await;
                let _ = outcome_tx
                    .send(AgentWorkbenchOutcome::Clipboard {
                        success_message,
                        result,
                    })
                    .await;
            });
            frame_requester.schedule_frame();
        }
        BottomPaneViewAction::StopBackgroundTask { task_id } => {
            dispatch_background_task_stop(
                &task_id,
                background_registry,
                backends.agent_spawner,
                restored_local_agents,
                chat_widget,
                bottom_pane,
                frame_requester,
            )
            .await;
        }
    }
}

fn replay_agent_live_transcript(
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    agent_id: &str,
    run_id: &str,
) {
    let (events, dropped_event_count) = chat_widget.agent_live_transcript_replay(agent_id, run_id);
    if dropped_event_count > 0 {
        bottom_pane.refresh_agent_live_gap(&astra_turn_core::agent_live_event::AgentLiveGap {
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            dropped_event_count,
        });
    }
    for event in &events {
        bottom_pane.refresh_agent_live_event(event);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubmissionProjection<'a> {
    Conversation(&'a str),
    LocalAction(&'a str),
}

fn submission_projection(text: &str) -> SubmissionProjection<'_> {
    if let Some(goal) = slash_plan_goal(text) {
        SubmissionProjection::Conversation(goal)
    } else if text.trim_start().starts_with('/') {
        SubmissionProjection::LocalAction(text)
    } else {
        SubmissionProjection::Conversation(text)
    }
}

fn commit_submission_projection(chat_widget: &mut chat_widget::ChatWidget, text: &str) {
    match submission_projection(text) {
        SubmissionProjection::Conversation(text) => chat_widget.handle_event(
            chat_widget::AppEvent::User(UserEvent::Submit(text.to_string())),
        ),
        SubmissionProjection::LocalAction(action) => {
            chat_widget.commit_system(history_cell::system::SystemCell::action(action));
        }
    }
}

/// Conversation submits hit scrollback immediately. Local actions wait for
/// their paired result/view so the action and outcome render as one unit.
fn should_flush_submission_immediately(text: &str) -> bool {
    matches!(
        submission_projection(text),
        SubmissionProjection::Conversation(_)
    )
}

fn begin_submission_dispatch_feedback(
    bottom_pane: &mut BottomPane,
    status_indicator: &mut status_indicator::StatusIndicator,
    at: std::time::Instant,
) {
    bottom_pane.set_task_status(TaskStatus::Dispatching);
    status_indicator.begin_dispatch(at);
}

fn finish_submission_feedback(
    bottom_pane: &mut BottomPane,
    status_indicator: &mut status_indicator::StatusIndicator,
) {
    bottom_pane.set_task_status(TaskStatus::Idle);
    status_indicator.set_state(status_indicator::IndicatorState::Idle);
}

/// Freeze the reply as soon as its visible stream has ended. Durable journal
/// and derived projections may still be settling, but they are not model work
/// and must not keep the composer in an active-looking state.
///
/// The returned flag is the one-shot transition. The timestamp remains the
/// authoritative control fact for input received before the turn future hands
/// ownership back to the outer loop.
fn settle_visible_reply(
    output_settled_at: &mut Option<std::time::Instant>,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    status_indicator: &mut status_indicator::StatusIndicator,
    now: std::time::Instant,
) -> bool {
    if output_settled_at.is_some() {
        return false;
    }
    *output_settled_at = Some(now);
    chat_widget.finish_stream_projection();
    bottom_pane.set_task_status(TaskStatus::Idle);
    status_indicator.set_state(status_indicator::IndicatorState::Idle);
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalShellSubmission {
    NotShell,
    Empty,
    Background(String),
    Interactive(String),
}

fn classify_local_shell_submission(text: &str) -> LocalShellSubmission {
    let Some(command) = text.trim_start().strip_prefix('!') else {
        return LocalShellSubmission::NotShell;
    };
    let command = command.trim();
    if command.is_empty() {
        return LocalShellSubmission::Empty;
    }
    if shell_command_needs_tty(command) {
        LocalShellSubmission::Interactive(command.to_string())
    } else {
        LocalShellSubmission::Background(command.to_string())
    }
}

fn start_local_background_shell(
    registry: &mut super::background_tasks::BackgroundTaskRegistry,
    command: &str,
) -> Result<String, String> {
    registry.try_spawn_shell(command, command)
}

fn local_background_shell_started_message(task_id: &str, command: &str) -> String {
    format!(
        "Shell {task_id} started: {command}\n{} to inspect output or stop it.",
        super::background_shortcut::ctrl_b_background_shortcut()
    )
}

async fn run_interactive_shell_command(command: &str) -> std::io::Result<std::process::ExitStatus> {
    let mut child = tokio::process::Command::new("sh");
    child.arg("-c").arg(command).kill_on_drop(true);
    child.status().await
}

/// Whether a `!cmd` shell command needs a real TTY (inherited stdio).
/// Non-interactive commands use the background-task runner so their lifecycle,
/// output cap, cancellation and task UI stay consistent with tool-started work.
/// Interactive commands temporarily hand the terminal to the child.
///
/// We look at the basename of the first whitespace-delimited token. This
/// misses sudo-wrapped commands (`sudo vim`) and env-prefixed forms
/// (`EDITOR=vim git commit`), which intentionally run in the background; if
/// those become a problem, extend the check, don't try to parse the shell.
fn shell_command_needs_tty(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let basename = first.rsplit('/').next().unwrap_or("");
    matches!(
        basename,
        "vim"
            | "vi"
            | "nvim"
            | "nano"
            | "emacs"
            | "ed"
            | "less"
            | "more"
            | "most"
            | "man"
            | "htop"
            | "top"
            | "btop"
            | "btm"
            | "tmux"
            | "screen"
            | "ssh"
            | "mosh"
            | "telnet"
    )
}

fn should_flush_after_slash_dispatch(result: &slash_dispatch::SlashResult) -> bool {
    !matches!(
        result,
        slash_dispatch::SlashResult::Deferred
            | slash_dispatch::SlashResult::OpenRootTranscript { .. }
    )
}

fn transcript_session_id(requested: Option<String>, current: Option<String>) -> Option<String> {
    requested.or(current)
}

fn next_pending_deferred_slash_flush(result: &slash_dispatch::SlashResult) -> bool {
    matches!(result, slash_dispatch::SlashResult::Deferred)
}

fn should_flush_ambient_commits(pending_deferred_slash_flush: bool) -> bool {
    !pending_deferred_slash_flush
}
fn refresh_footer_from_state(
    bottom_pane: &mut BottomPane,
    state: &crate::cli::session::session_state::SessionState,
) {
    bottom_pane.footer.model = state.model.clone();
    bottom_pane.footer.session_id = state
        .session_id
        .as_ref()
        .map(|sid| sid[..8.min(sid.len())].to_string());
    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
    if let Some(trace) = latest_context_trace(state)
        && let Some(usage) = context_window_from_trace(&trace)
    {
        bottom_pane.footer.restore_context_window(usage);
    }
}

fn surface_tui_file_write_errors(
    errors: &mut tokio::sync::mpsc::UnboundedReceiver<super::file_writer::TuiFileWriteError>,
    reported: &mut std::collections::HashSet<super::file_writer::TuiFileWriteError>,
    state: &mut crate::cli::session::session_state::SessionState,
    chat_widget: &mut chat_widget::ChatWidget,
    frame_requester: &FrameRequester,
) {
    while let Ok(error) = errors.try_recv() {
        let detail = error.user_message();
        state.session_persistence_error = Some(detail.clone());
        if reported.insert(error) {
            chat_widget.commit_ephemeral_warning(format!(
                "Local persistence degraded · {detail}. Conversation continues; resume/history may be stale."
            ));
            frame_requester.schedule_frame();
        }
    }
}

/// Replay a session's canonical root journal transcript into a fresh
/// `ChatWidget` and paint the restored cells into terminal scrollback.
///
/// A one-line banner is prepended so the user can tell the
/// scrollback they're seeing is restored context, not live.
/// Empty transcripts short-circuit to an empty widget with no
/// banner — there's nothing to tell the user about.
async fn replay_session_into_widget(
    guard: &mut TerminalGuard,
    session_id: &str,
    width: u16,
) -> chat_widget::ChatWidget {
    let mut widget = chat_widget::load_resume(session_id).await;
    let restored = widget.history().len();
    if restored == 0 {
        return widget;
    }
    // Banner first so it lands above the restored cells.
    let banner = history_cell::system::SystemCell::info(format!(
        "Resumed session {} — {} cells restored",
        &session_id[..8.min(session_id.len())],
        restored
    ));
    guard.queue_history_lines(banner.display_lines(width));
    guard.queue_history_lines(vec![ratatui::text::Line::default()]);
    // Paint the restored cells exactly once via the same rendering
    // path that streaming flushes use, so the visual match is
    // lossless.
    flush_chat_widget(guard, &mut widget, width);
    widget.mark_all_flushed();
    widget
}

/// One-shot lookup of the current git branch name via `gix`. Returns
/// `None` when the cwd isn't a git repo, detached HEAD, or errors.
///
/// Cached process-wide; see `crate::git_branch_cache`.
fn detect_git_branch() -> Option<String> {
    crate::git_branch_cache::detect_git_branch_cached()
}

/// Check if the terminal supports TUI mode.
pub(crate) fn can_run_tui() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

pub(crate) async fn run_tui_session(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    resume_session_id: Option<&str>,
    no_instructions: bool,
    max_budget: f64,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<(), String> {
    use crate::cli::session::session_runtime::{
        initialize_session_state, install_task_service, install_task_store, resolve_task_service,
        resolve_task_store,
    };
    use crate::cli::session::session_startup::complete_session_startup;
    use crate::cli::startup_trace::StartupTracer;

    // ── Ensure terminal is in sane state before startup output ────────
    // Previous astra crashes may leave terminal in raw mode, causing
    // startup eprintln output to lose carriage returns.
    let _ = crossterm::terminal::disable_raw_mode();

    // ── Initialize the gradient gutter time origin (PR #335) ─────────
    // Without this, the first cell to finalize before any
    // `elapsed_since_start()` call would saturate to 0 and the gutter
    // would jump on freeze. Eager init guarantees `>= PROCESS_START`.
    super::shimmer::init_time_origin();

    // ── Business initialization BEFORE entering TUI ─────────────────────
    let mut tracer = StartupTracer::new();
    // Cached credentials are sufficient to render and compose immediately.
    // Turn dispatch already obtains a fresh token at the network boundary, so
    // a synchronous `/auth/me` probe here only turns a degraded server into a
    // frozen startup screen.
    tracer.phase("cached_auth");
    let mut state = initialize_session_state(profile, initial_model, cli_context);
    let task_service = resolve_task_service(profile).await;
    install_task_service(&mut state, task_service);
    let (task_store, task_notify_tx) = resolve_task_store(profile, Some(&api.api_origin())).await;
    install_task_store(&mut state, task_store);
    state.task_notify_tx = task_notify_tx.clone();
    if max_budget > 0.0 {
        state.max_budget_limit = max_budget;
    }
    tracer.phase("state_init");
    let _startup = complete_session_startup(
        &mut state,
        &mut tracer,
        api,
        profile,
        resume_session_id,
        no_instructions,
        cli_context,
    )
    .await?;
    tracer.finish(state.session_id.as_deref());

    // Local and bundled skills are already available. External providers
    // converge in a supervised task after startup so DB/MCP latency cannot
    // delay the first interactive frame.
    let skill_registry = Arc::clone(&state.unified_skill_registry);
    let discovery_registry = Arc::clone(&skill_registry);
    let mcp_manager = Arc::clone(&state.mcp_manager);
    let discovery_mcp_manager = Arc::clone(&mcp_manager);
    let mut external_skill_discovery = tokio::spawn(async move {
        crate::cli::session::session_runtime::discover_external_pipeline_capabilities(
            discovery_registry,
            discovery_mcp_manager,
        )
        .await
    });
    let mut external_skill_discovery_pending = true;

    // ── TUI mode overrides ──────────────────────────────────────────────
    let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
    state.tui_render_policy = Some(crate::cli::stream::stream_render::RenderPolicy::Silent);
    let mut tui_cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    state.tui_cancel_token = Some(tui_cancel_token.clone());

    // Approval channel: tool approval requests from SSE host → TUI overlay
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::channel::<crate::cli::chat_stream::ApprovalRequest>(
            crate::cli::chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
        );
    state.tui_approval_request_tx = Some(approval_tx);
    let (ask_user_tx, mut ask_user_rx) =
        tokio::sync::mpsc::channel::<crate::cli::chat_stream::AskUserRequest>(
            crate::cli::chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
        );
    state.tui_ask_user_request_tx = Some(ask_user_tx);
    let (plan_review_tx, mut plan_review_rx) =
        tokio::sync::mpsc::channel::<crate::cli::chat_stream::PlanReviewRequest>(
            crate::cli::chat_stream::INTERACTIVE_REQUEST_CHANNEL_CAPACITY,
        );
    state.tui_plan_review_request_tx = Some(plan_review_tx);

    // Create local presentation infrastructure before taking over the
    // terminal. Startup observations themselves are deferred below so neither
    // git discovery nor a remote task summary can delay the first frame.
    let (file_writer, file_writer_runtime, mut file_write_errors) = super::file_writer::spawn();
    let mut reported_file_write_errors = std::collections::HashSet::new();
    let mut bottom_pane = BottomPane::new();
    bottom_pane.set_file_writer(file_writer.clone());
    // ── Enter TUI ───────────────────────────────────────────────────────
    let mut guard = TerminalGuard::init().map_err(|e| format!("TUI init failed: {e}"))?;
    let (draw_tx, draw_rx) = broadcast::channel(16);
    let frame_requester = FrameRequester::new(draw_tx);
    guard.set_history_drain_requester(frame_requester.clone());
    let mut event_stream = TuiEventStream::new(draw_rx);
    let (startup_effect_tx, mut startup_effect_rx) = tokio::sync::mpsc::channel(4);
    let mut startup_observation_tasks = Vec::with_capacity(3);
    {
        let startup_effect_tx = startup_effect_tx.clone();
        startup_observation_tasks.push(tokio::spawn(async move {
            let branch = tokio::task::spawn_blocking(detect_git_branch)
                .await
                .unwrap_or(None);
            let _ = startup_effect_tx
                .send(StartupUiEffect::GitBranch(branch))
                .await;
        }));
    }
    {
        let startup_effect_tx = startup_effect_tx.clone();
        let mcp_manager = Arc::clone(&state.mcp_manager);
        startup_observation_tasks.push(tokio::spawn(async move {
            let completions = {
                let manager = mcp_manager.read().await;
                crate::cli::slash::slash_mcp::build_mcp_extra_subcommands(&manager)
            };
            let _ = startup_effect_tx
                .send(StartupUiEffect::McpCompletions(completions))
                .await;
        }));
    }
    let (model_catalog_tx, mut model_catalog_rx) = tokio::sync::mpsc::channel(2);
    let mut model_catalog_tasks = tokio::task::JoinSet::new();
    let mut model_catalog_loading = false;
    let mut model_catalog_cache = None;
    let (slash_background_read_tx, mut slash_background_read_rx) =
        tokio::sync::mpsc::channel::<SlashBackgroundReadCompletion>(8);
    let mut slash_background_read_tasks = tokio::task::JoinSet::new();
    let mut slash_background_read_count = 0usize;
    let mut slash_background_read_generation = 0u64;
    // A single ordered worker owns all derived persistence for completed
    // turns. The canonical journal fsync remains in the foreground turn;
    // workspace/checkpoint/CSL/telemetry projections never do.
    let (mut turn_post_commit_tx, turn_post_commit_rx) = tokio::sync::mpsc::channel(16);
    let (turn_post_commit_completion_tx, mut turn_post_commit_completion_rx) =
        tokio::sync::mpsc::channel(16);
    let mut turn_post_commit_worker =
        spawn_turn_post_commit_worker(turn_post_commit_rx, turn_post_commit_completion_tx.clone());

    if let Some(ref model) = state.model {
        bottom_pane.footer.model = Some(model.clone());
    }
    if let Some(ref sid) = state.session_id {
        bottom_pane.footer.session_id = Some(sid[..8.min(sid.len())].to_string());
    }
    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
    // Lock-free observer of `perm_manager.mode()` so the inner-tick
    // path can refresh the status-line chip while the agentic loop
    // holds `&mut state`.
    // Explicit picker selections stay outside this mirror until turn end;
    // it always reports the policy governing the current turn.
    let perm_mode_mirror = state.perm_manager.mode_mirror_handle();
    // Wire the same mirror into the footer so every frame render
    // reads the live mode from the atomic mirror rather than the
    // cached `permission_mode` field. This eliminates the ~50 ms
    // staleness window that the tick-based self-healing had.
    bottom_pane.footer.set_mode_mirror(perm_mode_mirror.clone());

    // Seed the popup from the process-local baseline. External results update
    // this same projection through the event loop when they converge.
    refresh_skill_popup(&skill_registry, &mut bottom_pane);

    // Load slash-command catalog for the inline `/` menu.
    {
        let slash_items: Vec<slash_menu::SlashItem> = crate::cli::command_registry::tui_commands()
            .filter(|m| !m.name.contains(' '))
            .map(|m| slash_menu::SlashItem {
                name: m.name.into(),
                description: m.description.into(),
                subcommands: m.visible_tui_subcommands(),
                group: Some(m.group),
                primary: m.is_primary(),
                usage_examples: m.usage_examples,
                ..Default::default()
            })
            .collect();
        bottom_pane.set_slash_items(slash_items);

        // Dynamic MCP completions arrive through `StartupUiEffect` once the
        // manager lock is available. The static command catalog is enough to
        // compose `/mcp` immediately.
    }

    // Install a filesystem-backed file provider for the `@`-mention menu,
    // rooted at the current working directory.
    if let Ok(cwd) = std::env::current_dir() {
        bottom_pane.set_file_provider(std::sync::Arc::new(
            mention_menu::provider::FsFileProvider::new(cwd),
        ));
    }

    // ChatWidget owns the scrollback + active cell. If the user
    // entered via `astra -c` / `astra --resume <id>`, replay the
    // prior session's canonical root transcript into the widget and
    // paint it to terminal scrollback exactly once. A brand-new
    // session falls through to an empty local projection.
    let mut chat_widget = match state.session_id.as_deref() {
        Some(sid) if !sid.is_empty() => {
            let w0 = guard.terminal.size().map(|s| s.width).unwrap_or(80);
            replay_session_into_widget(&mut guard, sid, w0).await
        }
        _ => chat_widget::ChatWidget::new(String::new()),
    };

    if let Some(prompt) = state.perm_manager.workspace_trust_startup_prompt() {
        use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};

        let items = vec![
            SelectionItem {
                name: "Trust Workspace".into(),
                description: Some("Enable saved workspace rules for this path".into()),
                is_current: false,
            },
            SelectionItem {
                name: "Continue This Session".into(),
                description: Some(
                    "Keep saved workspace rules off for now; ask again next time".into(),
                ),
                is_current: false,
            },
            SelectionItem {
                name: "Mark Untrusted".into(),
                description: Some(
                    "Keep saved workspace rules off and stop asking on startup".into(),
                ),
                is_current: false,
            },
        ];
        bottom_pane.push_view(Box::new(
            ListSelectionView::new(items, Some(prompt.header)).with_results(vec![
                bottom_pane::view::ViewResult::WorkspaceTrust(
                    bottom_pane::view::WorkspaceTrustChoice::Trust,
                ),
                bottom_pane::view::ViewResult::WorkspaceTrust(
                    bottom_pane::view::WorkspaceTrustChoice::ContinueUntrusted,
                ),
                bottom_pane::view::ViewResult::WorkspaceTrust(
                    bottom_pane::view::WorkspaceTrustChoice::MarkUntrusted,
                ),
            ]),
        ));
    } else if let Some(notice) = state.perm_manager.workspace_trust_notice() {
        chat_widget.commit_system(history_cell::system::SystemCell::info(notice));
    }

    // Resume-time summary is useful but not required to start composing.
    // Fetch it after terminal ownership is established; a remote task service
    // can otherwise hold the first interactive frame for its whole transport
    // timeout.
    if let (Some(svc), Some(sid)) = (state.task_service.clone(), state.session_id.clone())
        && !sid.is_empty()
    {
        let user_id = state
            .ingestion_user_id
            .clone()
            .unwrap_or_else(|| "local".into());
        let cutoff = resume_summary::last_seen_cutoff(state.last_turn_event.as_ref())
            .unwrap_or("")
            .to_string();
        let startup_effect_tx = startup_effect_tx.clone();
        startup_observation_tasks.push(tokio::spawn(async move {
            match svc
                .list_recent_tasks_for_session(&user_id, &sid, None)
                .await
            {
                Ok(items) => {
                    let summary = resume_summary::summarize(&items, &sid, &cutoff);
                    if !summary.is_empty() {
                        let _ = startup_effect_tx
                            .send(StartupUiEffect::ResumeSummary(summary.render()))
                            .await;
                    }
                }
                Err(error) => tracing::debug!(
                    target: "astra_cli::tui",
                    error = %error,
                    "resume summary: list_recent_tasks failed; skipping banner"
                ),
            }
        }));
    }
    drop(startup_effect_tx);
    let mut status_indicator = status_indicator::StatusIndicator::new();
    let mut pending_deferred_slash_flush = false;
    // Set on Ctrl+C/Esc while a turn is winding down. We do not drain accepted
    // input in the cancel handler: applied events may still arrive during the
    // cancel→stop window. The single turn-end settlement point then restores
    // anything unresolved instead of guessing whether a cancelled turn should
    // launch more work.
    let mut interrupt_pending = false;
    // Messages submitted while a response is visibly winding down, plus
    // active-run guidance that did not reach a model boundary before a
    // normally completed turn. They are real user submissions, so preserve
    // FIFO order and re-enter the ordinary submit path rather than presenting
    // an "accepted" acknowledgement that never produces a response.
    let mut queued_followup_submissions = VecDeque::<String>::new();
    let mut runtime_notification_turn_pending = false;
    let mut runtime_notification_wake_at: Option<std::time::Instant> = None;
    let mut last_plan_interrupt: Option<std::time::Instant> = None;

    // Task board observer + toggle state. Observer is tick-driven
    // (see task_board_observer.rs rationale); no background loop
    // holding locks across `.await`. Ctrl+T flips the toggle; when
    // the board transitions from empty to non-empty we auto-open it.
    let task_board = task_board_observer::TaskBoardObserver::new(
        state.task_manager.store(),
        state.session_id.clone().unwrap_or_default(),
    );
    // Durable plans are a separate canonical owner from `session_todos`.
    // The observer only contributes read-only display rows to the shared
    // Taskboard projection; it never mirrors or writes plan steps as todos.
    let plan_task_observer = crate::tui::plan_task_observer::PlanTaskObserver::new(
        api.clone(),
        profile,
        state.session_id.as_deref(),
    );
    let mut board_expanded = false;
    let server_agent_observer = crate::tui::server_agent_observer::ServerAgentObserver::new(
        api.clone(),
        profile,
        state.session_id.as_deref(),
    );
    let mut server_agent_projection_sequence = None;
    let (agent_workbench_tx, mut agent_workbench_rx) =
        tokio::sync::mpsc::channel::<AgentWorkbenchOutcome>(64);

    // Background task registry — owns spawned shell/agent processes.
    let mut background_registry = super::background_tasks::BackgroundTaskRegistry::new(
        background_task_output_dir(state.session_id.as_deref()),
    );
    let mut restored_local_agent_task_projections =
        restore_background_task_projections(&mut background_registry, state.session_id.as_deref())
            .await;
    if let Some(spawner) = state.agent_spawner.as_ref() {
        spawner
            .restore_workspace_agent_projections(&restored_local_agent_task_projections)
            .await;
    }
    let mut background_task_projection_cache = background_registry.export_shell_task_projections();
    let mut background_local_agent_projection_cache = restored_local_agent_task_projections.clone();
    let mut background_registry_session_id = state.session_id.clone();
    let mut local_agent_snapshot =
        super::local_agent_snapshot::LocalAgentSnapshot::capture(state.agent_spawner.as_ref())
            .await;
    chat_widget.reconcile_local_agent_snapshot(
        &local_agent_snapshot,
        &restored_local_agent_task_projections,
    );
    if let Some(session_id) = state
        .session_id
        .as_deref()
        .filter(|session_id| !session_id.trim().is_empty())
    {
        dispatch_local_agent_journal_load(session_id.to_string(), agent_workbench_tx.clone());
    }
    let mut next_local_agent_reconcile = std::time::Instant::now() + LOCAL_AGENT_RECONCILE_INTERVAL;
    // User's explicit Ctrl+T choice. `None` = compact baseline;
    // `Some(true|false)` = honour the user's pin until the task list empties.
    let mut board_user_pin: Option<bool> = None;

    frame_requester.schedule_frame();

    let result: Result<(), String> = 'main: loop {
        guard
            .ensure_tui_modes()
            .map_err(|e| format!("failed to restore terminal input mode: {e}"))?;
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(tick);

        tokio::select! {
            Some(completion) = turn_post_commit_completion_rx.recv() => {
                let completed_session_id = completion.session_id.clone();
                let errors = crate::cli::turn::turn_post_commit::apply_turn_post_commit_completion(
                    completion,
                    &mut state,
                );
                if let Some(session_id) = completed_session_id
                    .filter(|session_id| state.session_id.as_deref() == Some(session_id.as_str()))
                {
                    bottom_pane.refresh_root_transcript_committed(&session_id);
                }
                if !errors.is_empty() {
                    chat_widget.commit_system(history_cell::system::SystemCell::warning(format!(
                        "Turn is saved. A local continuation projection failed: {}",
                        errors.join("; "),
                    )));
                    let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                    flush_chat_widget(&mut guard, &mut chat_widget, width);
                }
                frame_requester.schedule_frame();
            }
            worker = &mut turn_post_commit_worker => {
                let detail = match worker {
                    Ok(()) => "worker exited before TUI shutdown".to_string(),
                    Err(error) => format!("worker failed: {error}"),
                };
                chat_widget.commit_system(history_cell::system::SystemCell::warning(format!(
                    "Turn post-commit worker restarted; canonical journal turns remain recoverable. ({detail})"
                )));
                let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                flush_chat_widget(&mut guard, &mut chat_widget, width);
                let (replacement_tx, replacement_rx) = tokio::sync::mpsc::channel(16);
                turn_post_commit_tx = replacement_tx;
                turn_post_commit_worker = spawn_turn_post_commit_worker(
                    replacement_rx,
                    turn_post_commit_completion_tx.clone(),
                );
                frame_requester.schedule_frame();
            }
            Some(completion) = slash_background_read_rx.recv() => {
                if completion.generation != slash_background_read_generation {
                    continue;
                }
                slash_background_read_count = slash_background_read_count.saturating_sub(1);
                apply_slash_background_read_effect(
                    completion.effect,
                    &mut bottom_pane,
                    &mut chat_widget,
                );
                let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                flush_chat_widget(&mut guard, &mut chat_widget, width);
                bottom_pane.sync_popups();
                frame_requester.schedule_frame();
            }
            Some(effect) = model_catalog_rx.recv() => {
                model_catalog_loading = false;
                pending_deferred_slash_flush = apply_model_catalog_effect(
                    effect,
                    &state,
                    &mut bottom_pane,
                    &mut chat_widget,
                    &mut model_catalog_cache,
                );
                if !pending_deferred_slash_flush {
                    let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                    flush_chat_widget(&mut guard, &mut chat_widget, width);
                }
                bottom_pane.sync_popups();
                frame_requester.schedule_frame();
            }
            Some(Err(error)) = model_catalog_tasks.join_next(), if !model_catalog_tasks.is_empty() => {
                model_catalog_loading = false;
                chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
                    "Model catalog request stopped before completion: {error}"
                )));
                let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                flush_chat_widget(&mut guard, &mut chat_widget, width);
                frame_requester.schedule_frame();
            }
            Some(effect) = startup_effect_rx.recv() => {
                if apply_startup_ui_effect(effect, &mut bottom_pane, &mut chat_widget) {
                    let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                    flush_chat_widget(&mut guard, &mut chat_widget, width);
                }
                frame_requester.schedule_frame();
            }
            Some(ev) = event_stream.next() => {
                match ev {
                    TuiEvent::Key(key) => {
                        match handle_global_key_action(
                            key,
                            &mut guard,
                            &mut bottom_pane,
                            &frame_requester,
                        ) {
                            Some(GlobalKeyHandling::Handled) => continue,
                            Some(GlobalKeyHandling::OpenRootTranscript) => {
                                let terminal_size = guard.terminal.size().ok();
                                open_root_transcript_workspace(
                                    &chat_widget,
                                    &mut bottom_pane,
                                    terminal_size.map(|size| size.width).unwrap_or(80),
                                    terminal_size.map(|size| size.height).unwrap_or(0),
                                    ViewActionBackends {
                                        agent_spawner: state.agent_spawner.clone(),
                                        delegation_engine: state.delegation_engine.clone(),
                                        api: api.clone(),
                                        profile: profile.map(str::to_string),
                                        session_id: state.session_id.clone(),
                                        file_writer: Some(file_writer.clone()),
                                        agent_workbench_tx: agent_workbench_tx.clone(),
                                    },
                                    &frame_requester,
                                );
                                continue;
                            }
                            None => {}
                        }
                        if key.code == crossterm::event::KeyCode::Char('c')
                            && key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL)
                            && bottom_pane.composer.is_empty()
                            && !bottom_pane.has_active_view()
                            && let Some(handle) = state.plan_handle.as_ref()
                        {
                            let now = std::time::Instant::now();
                            let cancel = last_plan_interrupt.is_some_and(|at| {
                                now.duration_since(at) < Duration::from_secs(2)
                            });
                            last_plan_interrupt = Some(now);
                            let command = if cancel {
                                crate::cli::plan::plan_executor::PlanCommand::Cancel
                            } else {
                                crate::cli::plan::plan_executor::PlanCommand::Pause
                            };
                            match handle.send_command(command) {
                                Ok(()) if cancel => chat_widget.commit_system(
                                    history_cell::system::SystemCell::response(
                                        "Cancelling plan…".to_string(),
                                    ),
                                ),
                                Ok(()) => chat_widget.commit_system(
                                    history_cell::system::SystemCell::response(
                                        "Pausing plan… press Ctrl+C again within 2s to cancel."
                                            .to_string(),
                                    ),
                                ),
                                Err(error) => chat_widget.commit_system(
                                    history_cell::system::SystemCell::error(format!(
                                        "Could not control plan: {error}"
                                    )),
                                ),
                            }
                            frame_requester.schedule_frame();
                            continue;
                        }
                        if key.code == crossterm::event::KeyCode::Char('c')
                            && key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL)
                            && bottom_pane.composer.is_empty()
                            && !bottom_pane.has_active_view()
                            && slash_background_read_count > 0
                        {
                            let dismissed = slash_background_read_count;
                            slash_background_read_tasks.abort_all();
                            slash_background_read_count = 0;
                            slash_background_read_generation =
                                slash_background_read_generation.wrapping_add(1);
                            chat_widget.commit_system(history_cell::system::SystemCell::info(
                                format!(
                                    "Stopped waiting for {dismissed} background action(s). Work already launched locally may finish separately, but its result will not interrupt this session."
                                ),
                            ));
                            let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                            flush_chat_widget(&mut guard, &mut chat_widget, width);
                            frame_requester.schedule_frame();
                            continue;
                        }
                        if is_background_task_manage_key(&key) || is_ctrl_b_background_key(&key) {
                            let _ = force_open_background_task_view(
                                &mut background_registry,
                                state.agent_spawner.as_ref(),
                                &restored_local_agent_task_projections,
                                &mut bottom_pane,
                                &frame_requester,
                            )
                            .await;
                            frame_requester.schedule_frame();
                            continue;
                        }
                        if handle_conversation_tab_shortcut(
                            &key,
                            &mut bottom_pane,
                            &frame_requester,
                        ) {
                            continue;
                        }
                        if handle_task_surface_shortcut(
                            &key,
                            &task_board,
                            &mut board_expanded,
                            &mut board_user_pin,
                            &mut bottom_pane,
                            &frame_requester,
                        )
                        {
                            continue;
                        }
                        // Ctrl+R: edit last — pull the most recent user
                        // message back into the composer so the user can
                        // re-word and resubmit without retyping. Works only
                        // when idle (no overlay, composer empty) so it
                        // doesn't clobber in-flight drafts. The prior
                        // scrollback stays visible: the retry runs as a
                        // fresh turn below, and the model sees the earlier
                        // attempt + its reply as context (which is the point
                        // — "try again, differently").
                        if key.code == crossterm::event::KeyCode::Char('r')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                            && !bottom_pane.has_active_view()
                            && bottom_pane.composer.is_empty()
                            && let Some(prev) = chat_widget.last_user_text()
                        {
                            bottom_pane.composer.set_text(&prev);
                            frame_requester.schedule_frame();
                            continue;
                        }
                        if handle_agent_monitor_shortcut(
                            &key,
                            &mut chat_widget,
                            &mut bottom_pane,
                            &frame_requester,
                        ) {
                            continue;
                        }
                        match bottom_pane.handle_key(key) {
                            BottomPaneAction::OpenPermissionModePicker => {
                                bottom_pane.push_view(Box::new(
                                    slash_dispatch::build_permission_mode_picker(
                                        state.perm_manager.mode(),
                                    ),
                                ));
                                frame_requester.schedule_frame();
                            }
                            BottomPaneAction::SubmitInput(text) => {
                                let runtime_notification_submission =
                                    runtime_notification_turn_pending
                                        && text == RUNTIME_NOTIFICATION_TURN_SENTINEL;
                                if runtime_notification_turn_pending {
                                    // The scheduled wake is consumed either by its
                                    // sentinel or by a real user submission that won
                                    // the race. In the latter case ordinary input
                                    // finalization normally carries the pending runtime
                                    // facts. Re-arm as a fallback because the real input
                                    // may instead be a local slash/shell action that does
                                    // not enter a model boundary.
                                    runtime_notification_turn_pending = false;
                                    if !runtime_notification_submission {
                                        runtime_notification_wake_at.get_or_insert_with(|| {
                                            std::time::Instant::now()
                                                + Duration::from_millis(200)
                                        });
                                    }
                                }
                                let text = if runtime_notification_submission {
                                    String::new()
                                } else {
                                    text
                                };
                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);

                                match classify_local_shell_submission(&text) {
                                    LocalShellSubmission::NotShell => {}
                                    LocalShellSubmission::Empty => {
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::error(
                                                "Shell command is empty. Enter `!<command>`.",
                                            ),
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    LocalShellSubmission::Background(command) => {
                                        match start_local_background_shell(
                                            &mut background_registry,
                                            &command,
                                        ) {
                                            Ok(task_id) => chat_widget.commit_system(
                                                history_cell::system::SystemCell::info(
                                                    local_background_shell_started_message(
                                                        &task_id, &command,
                                                    ),
                                                ),
                                            ),
                                            Err(error) => chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(format!(
                                                    "Could not start shell command: {error}"
                                                )),
                                            ),
                                        }
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        refresh_footer_from_state(&mut bottom_pane, &state);
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    LocalShellSubmission::Interactive(command) => {
                                        let command_for_child = command.clone();
                                        let status = guard
                                            .with_restored(|| async move {
                                                println!("! {command_for_child}");
                                                run_interactive_shell_command(&command_for_child)
                                                    .await
                                            })
                                            .await;
                                        guard.terminal.invalidate_viewport();
                                        match status {
                                            Ok(Ok(status)) if status.success() => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::response(
                                                        format!("! {command}"),
                                                    ),
                                                );
                                            }
                                            Ok(Ok(status)) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(format!(
                                                        "! {command}: exit {}",
                                                        status.code().unwrap_or(-1)
                                                    )),
                                                );
                                            }
                                            Ok(Err(error)) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(format!(
                                                        "! {command}: {error}"
                                                    )),
                                                );
                                            }
                                            Err(error) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(format!(
                                                        "! {command}: failed to restore terminal: {error}"
                                                    )),
                                                );
                                            }
                                        }
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        refresh_footer_from_state(&mut bottom_pane, &state);
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                }

                                let flush_submission_immediately =
                                    should_flush_submission_immediately(&text);
                                // Persist the semantic submission, not merely the
                                // composer bytes: local slash actions belong to the
                                // durable workbench transcript, while only actual
                                // conversational input becomes a User turn.
                                if !runtime_notification_submission {
                                    commit_submission_projection(&mut chat_widget, &text);
                                }
                                begin_submission_dispatch_feedback(
                                    &mut bottom_pane,
                                    &mut status_indicator,
                                    std::time::Instant::now(),
                                );
                                if flush_submission_immediately {
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                }
                                {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board)?;
                                }

                                // Browsing the model catalog is remote I/O, but the command
                                // itself is an ordinary local intent. Acknowledge it now and
                                // let the main loop keep accepting keys while the catalog is
                                // fetched. The structured result below retains thinking and
                                // provider metadata for the eventual picker.
                                if slash_dispatch::is_model_picker_request(&text) {
                                    if model_catalog_loading {
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::info(
                                                "Model catalog is already loading.",
                                            ),
                                        );
                                    } else {
                                        model_catalog_loading = true;
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::response(
                                                "Loading model catalog…",
                                            ),
                                        );
                                        let api = api.clone();
                                        let profile = profile.map(str::to_string);
                                        let model_catalog_tx = model_catalog_tx.clone();
                                        model_catalog_tasks.spawn(async move {
                                            let result = slash_dispatch::load_model_catalog(api, profile).await;
                                            let _ = model_catalog_tx
                                                .send(ModelCatalogEffect::Ready(result))
                                                .await;
                                        });
                                    }
                                    pending_deferred_slash_flush = false;
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    finish_submission_feedback(
                                        &mut bottom_pane,
                                        &mut status_indicator,
                                    );
                                    frame_requester.schedule_frame();
                                    continue;
                                }

                                let mut inline_chat_submit = None;
                                if let Some(plan_goal) = slash_plan_goal(&text) {
                                    let before = capture_plan_mode_ui_snapshot(&state);
                                    let Some(token) =
                                        crate::cli::plan::plan_lifecycle::fresh_token_for_plan(api, profile).await
                                    else {
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::error(
                                                "Not logged in. Use /login.",
                                            ),
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        finish_submission_feedback(
                                            &mut bottom_pane,
                                            &mut status_indicator,
                                        );
                                        frame_requester.schedule_frame();
                                        continue;
                                    };
                                    match crate::cli::plan::plan_lifecycle::enter_remote_plan_mode(
                                        api,
                                        profile,
                                        &token,
                                        &mut state,
                                        plan_goal,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            inline_chat_submit = Some(plan_goal.to_string());
                                            commit_plan_transition_notice(
                                                &mut chat_widget,
                                                &before,
                                                &state,
                                                true,
                                            );
                                            if let Some(ref sid) = state.session_id
                                                && chat_widget.session_id() != sid
                                            {
                                                chat_widget.set_session_id(sid.clone());
                                                rebind_workbench_observers(
                                                    Some(sid),
                                                    &task_board,
                                                    &server_agent_observer,
                                                    &plan_task_observer,
                                                    &mut board_user_pin,
                                                );
                                            }
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        }
                                        Err(error) => {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(error),
                                            );
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            finish_submission_feedback(
                                                &mut bottom_pane,
                                                &mut status_indicator,
                                            );
                                            frame_requester.schedule_frame();
                                            continue;
                                        }
                                    }
                                }

                                if text.trim_start().starts_with('/')
                                    && inline_chat_submit.is_none()
                                {
                                    // Snapshot the session identity before a
                                    // native slash action so the existing
                                    // replay path can observe a rebind.
                                    let pre_sid = state.session_id.clone();
                                    let pre_plan_snapshot = (text.trim() == "/plan")
                                        .then(|| capture_plan_mode_ui_snapshot(&state));
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane,
                                        chat_widget: &mut chat_widget, width: w,
                                    };
                                    let result = slash_dispatch::dispatch(&text, &mut dctx).await;
                                    let flush_slash_response =
                                        should_flush_after_slash_dispatch(&result);
                                    pending_deferred_slash_flush =
                                        next_pending_deferred_slash_flush(&result);
                                    match result {
                                        slash_dispatch::SlashResult::Handled => {}
                                        slash_dispatch::SlashResult::Deferred => {}
                                        slash_dispatch::SlashResult::OpenRootTranscript {
                                            session_id,
                                        } => {
                                            let terminal_size = guard.terminal.size().ok();
                                            open_root_transcript_workspace(
                                                &chat_widget,
                                                &mut bottom_pane,
                                                terminal_size
                                                    .map(|size| size.width)
                                                    .unwrap_or(80),
                                                terminal_size
                                                    .map(|size| size.height)
                                                    .unwrap_or(0),
                                                ViewActionBackends {
                                                    agent_spawner: state.agent_spawner.clone(),
                                                    delegation_engine: state
                                                        .delegation_engine
                                                        .clone(),
                                                    api: api.clone(),
                                                    profile: profile.map(str::to_string),
                                                    session_id: transcript_session_id(
                                                        session_id,
                                                        state.session_id.clone(),
                                                    ),
                                                    file_writer: Some(file_writer.clone()),
                                                    agent_workbench_tx: agent_workbench_tx.clone(),
                                                },
                                                &frame_requester,
                                            );
                                        }
                                        slash_dispatch::SlashResult::OpenBackgroundTasks => {
                                            let _ = force_open_background_task_view(
                                                &mut background_registry,
                                                state.agent_spawner.as_ref(),
                                                &restored_local_agent_task_projections,
                                                &mut bottom_pane,
                                                &frame_requester,
                                            )
                                            .await;
                                        }
                                        slash_dispatch::SlashResult::BackgroundRead(action) => {
                                            slash_background_read_count += 1;
                                            dispatch_slash_background_read(
                                                *action,
                                                slash_background_read_generation,
                                                slash_background_read_tx.clone(),
                                                &mut slash_background_read_tasks,
                                            );
                                        }
                                        slash_dispatch::SlashResult::Exit => { break 'main Ok(()); }
                                    }
                                    // Flush the slash-command response
                                    // cells (`⎿ Set model to …`, etc.)
                                    // into scrollback immediately so
                                    // the reply appears under `› /cmd`
                                    // without the ~50ms tick delay.
                                    if flush_slash_response {
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }
                                    // If the slash command rebound state.session_id
                                    // (resume/new-session paths), swap the
                                    // ChatWidget so its scrollback + persistence
                                    // attach to the restored session.
                                    if state.session_id != pre_sid
                                        && let Some(ref new_sid) = state.session_id
                                        && !new_sid.is_empty()
                                    {
                                        chat_widget = replay_session_into_widget(
                                            &mut guard,
                                            new_sid,
                                            w,
                                        )
                                        .await;
                                        rebind_workbench_observers(
                                            Some(new_sid),
                                            &task_board,
                                            &server_agent_observer,
                                            &plan_task_observer,
                                            &mut board_user_pin,
                                        );
                                    }
                                    refresh_footer_from_state(&mut bottom_pane, &state);
                                    // After any /mcp command refresh the dynamic
                                    // server/tool completions so that a freshly
                                    // added or removed server is immediately
                                    // visible in the tab-completion menu.
                                    if text.starts_with("/mcp") {
                                        let mcp_extras = {
                                            let mgr = state.mcp_manager.read().await;
                                            crate::cli::slash::slash_mcp::build_mcp_extra_subcommands(&mgr)
                                        };
                                        bottom_pane.update_mcp_completions(mcp_extras);
                                    }
                                    if let Some(before) = pre_plan_snapshot.as_ref() {
                                        commit_plan_transition_notice(
                                            &mut chat_widget,
                                            before,
                                            &state,
                                            true,
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }
                                    finish_submission_feedback(
                                        &mut bottom_pane,
                                        &mut status_indicator,
                                    );
                                    frame_requester.schedule_frame();
                                } else {
                                    let submit_was_inline_plan_goal = inline_chat_submit.is_some();
                                    let submit_text = inline_chat_submit.unwrap_or(text);
                                    if !runtime_notification_submission
                                        && crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(
                                            &state,
                                        )
                                    {
                                        let Some(token) =
                                            crate::cli::plan::plan_lifecycle::fresh_token_for_plan(api, profile)
                                                .await
                                        else {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(
                                                    "Not logged in. Use /login.",
                                                ),
                                            );
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            finish_submission_feedback(
                                                &mut bottom_pane,
                                                &mut status_indicator,
                                            );
                                            frame_requester.schedule_frame();
                                            continue;
                                        };
                                        match crate::cli::plan::plan_lifecycle::enter_remote_plan_mode(
                                            api,
                                            profile,
                                            &token,
                                            &mut state,
                                            &submit_text,
                                        )
                                        .await
                                        {
                                            Ok(_) => {
                                                // After bare `/plan`, the first plain message is
                                                // the user's real planning goal. Don't insert a
                                                // synthetic `Plan goal set ...` system line above
                                                // the actual planning/model output.
                                                if let Some(ref sid) = state.session_id
                                                    && chat_widget.session_id() != sid
                                                {
                                                    chat_widget.set_session_id(sid.clone());
                                                    rebind_workbench_observers(
                                                        Some(sid),
                                                        &task_board,
                                                        &server_agent_observer,
                                                        &plan_task_observer,
                                                        &mut board_user_pin,
                                                    );
                                                }
                                                refresh_footer_from_state(&mut bottom_pane, &state);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            }
                                            Err(error) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(error),
                                                );
                                                refresh_footer_from_state(&mut bottom_pane, &state);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                finish_submission_feedback(
                                                    &mut bottom_pane,
                                                    &mut status_indicator,
                                                );
                                                frame_requester.schedule_frame();
                                                continue;
                                            }
                                        }
                                    } else {
                                        if !submit_was_inline_plan_goal {
                                            if let Some(plan_command) =
                                                crate::cli::plan::plan_commands::parse_plan_command(&submit_text)
                                                    .filter(|command| {
                                                        crate::cli::plan::plan_commands::is_plan_command_available(
                                                            &state, command,
                                                        )
                                                    })
                                            {
                                                match plan_command {
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::Go => {
                                                        let go_result = async {
                                                            let Some(token) =
                                                                crate::cli::plan::plan_lifecycle::fresh_token_for_plan(
                                                                    api, profile,
                                                                )
                                                                .await
                                                            else {
                                                                return Err(
                                                                    "Not logged in. Use /login."
                                                                        .to_string(),
                                                                );
                                                            };
                                                            crate::cli::plan::plan_commands::prepare_plan_execution(
                                                                &mut state, api, &token,
                                                            )
                                                            .await?;
                                                            crate::cli::plan::plan_runtime::start_plan_executor(
                                                                &mut state,
                                                                Some(&token),
                                                                api,
                                                                profile,
                                                            )
                                                            .await
                                                        }
                                                        .await;
                                                        let plan_started = go_result.is_ok();
                                                        match go_result {
                                                            Ok(()) => {
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::response(
                                                                        "Plan started in the workbench · Ctrl+C pauses · Ctrl+O opens its live transcript.".to_string(),
                                                                    ),
                                                                );
                                                                let now = std::time::Instant::now();
                                                                bottom_pane.set_task_status(TaskStatus::WaitingModel);
                                                                status_indicator.begin_turn(now);
                                                                status_indicator.set_state(
                                                                    status_indicator::IndicatorState::WaitingModel {
                                                                        started_at: now,
                                                                    },
                                                                );
                                                            }
                                                            Err(error) => {
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::error(
                                                                        error,
                                                                    ),
                                                                );
                                                            }
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        if !plan_started {
                                                            finish_submission_feedback(
                                                                &mut bottom_pane,
                                                                &mut status_indicator,
                                                            );
                                                        }
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::Show => {
                                                        match crate::cli::plan::plan_commands::render_plan_snapshot(
                                                            &state,
                                                        ) {
                                                            Ok(message) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    message,
                                                                ),
                                                            ),
                                                            Err(error) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    error,
                                                                ),
                                                            ),
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        finish_submission_feedback(
                                                            &mut bottom_pane,
                                                            &mut status_indicator,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::Rewind {
                                                        anchor,
                                                    } => {
                                                        let token =
                                                            crate::cli::plan::plan_lifecycle::fresh_token_for_plan(
                                                                api, profile,
                                                            )
                                                            .await;
                                                        match crate::cli::plan::plan_commands::rewind_plan(
                                                            &mut state,
                                                            api,
                                                            token.as_deref(),
                                                            &anchor,
                                                        )
                                                        .await
                                                        {
                                                            Ok(message) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    message,
                                                                ),
                                                            ),
                                                            Err(error) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    error,
                                                                ),
                                                            ),
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        finish_submission_feedback(
                                                            &mut bottom_pane,
                                                            &mut status_indicator,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::AddCorrection { .. }
                                                    | crate::cli::plan::plan_commands::ParsedPlanCommand::ClearCorrections => {
                                                        match crate::cli::plan::plan_commands::apply_plan_correction(
                                                            &mut state,
                                                            &plan_command,
                                                        ) {
                                                            Ok(message) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    message,
                                                                ),
                                                            ),
                                                            Err(error) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    error,
                                                                ),
                                                            ),
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        finish_submission_feedback(
                                                            &mut bottom_pane,
                                                            &mut status_indicator,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                }
                                            }
                                            if state.executing_plan.is_some()
                                                && !state.plan_mode_active()
                                                && crate::cli::plan::plan_commands::abandon_plan_execution(
                                                    &mut state,
                                                )
                                            {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::info(
                                                        "Paused plan abandoned — continuing with normal chat.".to_string(),
                                                    ),
                                                );
                                                refresh_footer_from_state(
                                                    &mut bottom_pane,
                                                    &state,
                                                );
                                                flush_chat_widget(
                                                    &mut guard,
                                                    &mut chat_widget,
                                                    w,
                                                );
                                            }
                                        }
                                    }

                                    let turn_start = std::time::Instant::now();
                                    bottom_pane.footer.clear_context_window_for_new_request();
                                    let pre_prompt_tokens = state.total_prompt_tokens;
                                    let pre_completion_tokens = state.total_completion_tokens;
                                    let _pre_cost = state.total_session_cost;
                                    let pre_cache_read = state.total_cache_read_tokens;
                                    let pre_cache_creation = state.total_cache_creation_tokens;
                                    let pre_cached_context_trace_turn_id = state
                                        .latest_context_assembly_trace
                                        .as_ref()
                                        .map(|trace| trace.turn_id.clone());
                                    let pre_context_trace_count = context_trace_count(&state);
                                    let mut turn_tool_count: u32 = 0;
                                    let mut turn_ttft: Option<std::time::Instant> = None;
                                    let mut output_settled_at: Option<std::time::Instant> = None;
                                    let mut explain_items: Vec<serde_json::Value> = Vec::new();
                                    // Phase 3b.3c: prime the bash detach slot for this
                                    // turn. The bash runner takes the handle on entry;
                                    // we keep the listener so a Ctrl+B keypress can
                                    // fire the signal and await the live child + streams
                                    // payload back. Replaces any stale handle from a
                                    // prior turn that was never consumed (e.g. the model
                                    // didn't run bash last turn).
                                    let mut bash_detach_listener = Some(
                                        install_bash_detach_listener(
                                            &state.bash_detach_slot,
                                            &mut chat_widget,
                                            &mut status_indicator,
                                        )
                                        .await,
                                    );
                                    let (
                                        bash_detach_handoff_tx,
                                        mut bash_detach_handoff_rx,
                                    ) = tokio::sync::mpsc::unbounded_channel::<
                                        BashDetachHandoffResult,
                                    >();
                                    let mut bash_detach_request_pending = false;
                                    let mut active_bash_tool_use_id: Option<String> = None;
                                    let mut active_bash_description: Option<String> = None;
                                    let mut deferred_active_bg_notifications = Vec::new();

                                    let turn_tx = stream_bridge::create_per_turn_bridge(tui_tx.clone());
                                    let live_sink = stream_bridge::create_agent_live_sink(tui_tx.clone());
                                    state.tui_stream_event_tx = Some(turn_tx);
                                    state.tui_agent_live_event_sink = Some(live_sink);

                                    let turn_result = {
                                        // Snapshot the Arc<dyn TaskService> before the
                                        // turn borrows `state` mutably — Ctrl+C still
                                        // needs to issue cancel RPCs for in-flight
                                        // sub-agents, and the mutable borrow prevents
                                        // us from reaching through `state` inside the
                                        // inner select.
                                        let task_service_for_cancel = state.task_service.clone();
                                        let agent_spawner_for_cancel = state.agent_spawner.clone();
                                        let delegation_engine_for_control =
                                            state.delegation_engine.clone();
                                        let active_turn_local_run_control =
                                            state.active_turn_local_run_control.clone();
                                        let preinstalled_run_control =
                                            crate::cli::turn::local_run_control::LocalRunControl::shared();
                                        *astra_core::sync_poison::recover_mutex_lock(
                                            &active_turn_local_run_control,
                                        ) = Some(preinstalled_run_control.clone());
                                        let bash_detach_slot_for_ctrl_b =
                                            state.bash_detach_slot.clone();
                                        let background_registry_turn_session_id =
                                            state.session_id.clone();
                                        let background_registry_turn_model = state.model.clone();
                                        let bg_task_commands_for_turn =
                                            state.bg_task_commands.clone();
                                        let bg_task_list_cache_for_turn =
                                            state.bg_task_list_cache.clone();
                                        let ctx = crate::cli::turn::turn_entry::TurnContext {
                                            api,
                                            profile,
                                            post_commit_tx: Some(turn_post_commit_tx.clone()),
                                        };
                                        let mut tui_ui = ui_adapter::TuiUiAdapter::new(tui_tx.clone());
                                        // Authentication is part of the polled turn future, not
                                        // an await in the UI event handler. Slow refreshes therefore
                                        // leave transcript, composer, resize, and interrupt input
                                        // responsive while the visible state remains `Sending`.
                                        let fut = async {
                                            let token = crate::cli::session::session_runtime::fresh_access_token(api, profile).await;
                                            if runtime_notification_submission {
                                                crate::cli::turn::turn_entry::handle_runtime_notifications_with_ui(
                                                    token.as_deref(),
                                                    &mut state,
                                                    ctx,
                                                    &mut tui_ui,
                                                )
                                                .await
                                            } else {
                                                crate::cli::turn::turn_entry::handle_chat_input_with_ui(
                                                    submit_text,
                                                    token.as_deref(),
                                                    &mut state,
                                                    ctx,
                                                    &mut tui_ui,
                                                )
                                                .await
                                            }
                                        };
                                        tokio::pin!(fut);

                                        let mut turn_result_ready: Option<Result<(), String>> = None;
                                        let r: Result<(), String> = loop {
                                            if let Err(e) = guard.ensure_tui_modes() {
                                                break Err(format!(
                                                    "failed to restore terminal input mode: {e}"
                                                ));
                                            }
                                            let itick = tokio::time::sleep(Duration::from_millis(80));
                                            tokio::pin!(itick);
                                            tokio::select! {
                                                // Fair selection keeps key storms from starving a
                                                // ready Bash handoff or runtime event.
                                                result = &mut fut, if turn_result_ready.is_none() => {
                                                    if bash_detach_request_pending {
                                                        turn_result_ready = Some(result);
                                                        continue;
                                                    }
                                                    break result;
                                                }
                                                // Keep the UI live while a completed turn is still
                                                // waiting for Bash background-handoff adoption. The
                                                // event branch only touches UI-owned projections and
                                                // preinstalled run control; it does not borrow the
                                                // completed turn's SessionState. Turn-end remains the
                                                // single owner that resolves any queued input.
                                                Some(tev) = event_stream.next() => {
                                                    match tev {
                                                        TuiEvent::Key(k) => {
                                                            match handle_global_key_action(
                                                                k,
                                                                &mut guard,
                                                                &mut bottom_pane,
                                                                &frame_requester,
                                                            ) {
                                                                Some(GlobalKeyHandling::Handled) => continue,
                                                                Some(GlobalKeyHandling::OpenRootTranscript) => {
                                                                    let terminal_size = guard.terminal.size().ok();
                                                                    open_root_transcript_workspace(
                                                                        &chat_widget,
                                                                        &mut bottom_pane,
                                                                        terminal_size.map(|size| size.width).unwrap_or(80),
                                                                        terminal_size.map(|size| size.height).unwrap_or(0),
                                                                        ViewActionBackends {
                                                                            agent_spawner: agent_spawner_for_cancel.clone(),
                                                                            delegation_engine: delegation_engine_for_control.clone(),
                                                                            api: api.clone(),
                                                                            profile: profile.map(str::to_string),
                                                                            session_id: background_registry_turn_session_id.clone(),
                                                                            file_writer: Some(file_writer.clone()),
                                                                            agent_workbench_tx: agent_workbench_tx.clone(),
                                                                        },
                                                                        &frame_requester,
                                                                    );
                                                                    continue;
                                                                }
                                                                None => {}
                                                            }
                                                            // Shift+Tab opens the same explicit picker as
                                                            // idle mode. Permission policies are not a
                                                            // cycling dial, and a selected mode only applies
                                                            // at the next safe turn boundary.
                                                            if k.code == crossterm::event::KeyCode::BackTab {
                                                                bottom_pane.push_view(Box::new(
                                                                    slash_dispatch::build_permission_mode_picker(
                                                                        perm_mode_mirror.current(),
                                                                    ),
                                                                ));
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            if is_background_task_manage_key(&k) {
                                                                let _ = force_open_background_task_view(
                                                                    &mut background_registry,
                                                                    agent_spawner_for_cancel.as_ref(),
                                                                    &restored_local_agent_task_projections,
                                                                    &mut bottom_pane,
                                                                    &frame_requester,
                                                                )
                                                                .await;
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            // Ctrl+B backgrounds foreground work without replacing
                                                            // the user's composer with a management view. The task
                                                            // panel remains an explicit navigation action when there
                                                            // is no foreground work to promote.
                                                            if is_ctrl_b_background_key(&k) {
                                                                let detach_fired = if !bash_detach_request_pending
                                                                    && background_registry.can_spawn_shell_task()
                                                                    && let Some(listener) = bash_detach_listener.take()
                                                                {
                                                                    if listener.is_active()
                                                                        && listener.signal_tx.send(true).is_ok()
                                                                    {
                                                                        listener.retire();
                                                                        bash_detach_request_pending = true;
                                                                        set_bash_background_hint_enabled(
                                                                            &mut chat_widget,
                                                                            &mut status_indicator,
                                                                            false,
                                                                        );
                                                                        let handoff_tx =
                                                                            bash_detach_handoff_tx.clone();
                                                                        tokio::spawn(async move {
                                                                            let result = await_bash_detach_handoff(listener).await;
                                                                            let _ = handoff_tx.send(result);
                                                                        });
                                                                        true
                                                                    } else {
                                                                        bash_detach_listener = Some(listener);
                                                                        false
                                                                    }
                                                                } else {
                                                                    false
                                                                };

                                                                let mut promoted_agent_id: Option<String> = None;
                                                                if !detach_fired
                                                                    && let Some(spawner) =
                                                                        agent_spawner_for_cancel.as_ref()
                                                                    && let promoted = spawner
                                                                        .promote_foreground_work_to_background(None)
                                                                        .await
                                                                    && !promoted.is_empty()
                                                                {
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::background_task(
                                                                            ctrl_b_promoted_agent_message(&promoted),
                                                                        ),
                                                                    );
                                                                    promoted_agent_id = promoted
                                                                        .first()
                                                                        .map(|agent| agent.agent_id.clone());
                                                                    tui_cancel_token.cancel();
                                                                }

                                                                if detach_fired {
                                                                    let pending_title = active_bash_description
                                                                        .as_deref()
                                                                        .unwrap_or("Bash");
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::background_task(
                                                                            format!("Moving {pending_title} to the background…"),
                                                                        ),
                                                                    );
                                                                } else if promoted_agent_id.is_none() {
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::info(
                                                                            "Ctrl+B: Nothing to background. Opening task panel.",
                                                                        ),
                                                                    );
                                                                    let _ = force_open_background_task_view(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                    )
                                                                    .await;
                                                                }
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            if handle_agent_monitor_shortcut(
                                                                &k,
                                                                &mut chat_widget,
                                                                &mut bottom_pane,
                                                                &frame_requester,
                                                            ) {
                                                                continue;
                                                            }
                                                            if handle_task_surface_shortcut(
                                                                &k,
                                                                &task_board,
                                                                &mut board_expanded,
                                                                &mut board_user_pin,
                                                                &mut bottom_pane,
                                                                &frame_requester,
                                                            )
                                                            {
                                                                continue;
                                                            }
                                                            // During turn: composer stays usable.
                                                            // Enter queues a user intent against the active run.
                                                            // Ctrl+C interrupts.
                                                            bottom_pane.pre_draw_tick(std::time::Instant::now());
                                                            match bottom_pane.handle_key(k) {
                                                                    BottomPaneAction::SubmitInput(queued_text) => {
                                                                        match slash_dispatch::immediate_control(&queued_text) {
                                                                            Some(slash_dispatch::ImmediateControl::Exit) => {
                                                                                // `/exit` is a workbench lifecycle command in every
                                                                                // turn phase. Cancel the in-flight local boundary and
                                                                                // leave through the regular shutdown path; never queue
                                                                                // it as model guidance or a follow-up user message.
                                                                                tui_cancel_token.cancel();
                                                                                break 'main Ok(());
                                                                            }
                                                                            Some(slash_dispatch::ImmediateControl::StopCurrentRun) => {
                                                                                if output_settled_at.is_some() {
                                                                                    chat_widget.commit_system(
                                                                                        history_cell::system::SystemCell::info(
                                                                                            "The current run has already finished its response and is settling.",
                                                                                        ),
                                                                                    );
                                                                                } else {
                                                                                    chat_widget.commit_system(
                                                                                        history_cell::system::SystemCell::info(
                                                                                            "Stopping the current run…",
                                                                                        ),
                                                                                    );
                                                                                    request_active_run_cancel(
                                                                                        &mut chat_widget,
                                                                                        task_service_for_cancel.as_ref(),
                                                                                        &preinstalled_run_control,
                                                                                        &tui_cancel_token,
                                                                                    )
                                                                                    .await;
                                                                                    interrupt_pending = true;
                                                                                    bottom_pane.interrupt_pending = true;
                                                                                }
                                                                                flush_chat_widget(
                                                                                    &mut guard,
                                                                                    &mut chat_widget,
                                                                                    w,
                                                                                );
                                                                                frame_requester.schedule_frame();
                                                                                continue;
                                                                            }
                                                                            None => {}
                                                                        }
                                                                        match classify_local_shell_submission(&queued_text) {
                                                                            LocalShellSubmission::NotShell => {}
                                                                            LocalShellSubmission::Empty => {
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::error(
                                                                                        "Shell command is empty. Enter `!<command>`.",
                                                                                    ),
                                                                                );
                                                                                flush_chat_widget(
                                                                                    &mut guard,
                                                                                    &mut chat_widget,
                                                                                    w,
                                                                                );
                                                                                frame_requester.schedule_frame();
                                                                                continue;
                                                                            }
                                                                            LocalShellSubmission::Background(command) => {
                                                                                match start_local_background_shell(
                                                                                    &mut background_registry,
                                                                                    &command,
                                                                                ) {
                                                                                    Ok(task_id) => chat_widget.commit_system(
                                                                                        history_cell::system::SystemCell::info(
                                                                                            local_background_shell_started_message(
                                                                                                &task_id,
                                                                                                &command,
                                                                                            ),
                                                                                        ),
                                                                                    ),
                                                                                    Err(error) => chat_widget.commit_system(
                                                                                        history_cell::system::SystemCell::error(format!(
                                                                                            "Could not start shell command: {error}"
                                                                                        )),
                                                                                    ),
                                                                                }
                                                                                flush_chat_widget(
                                                                                    &mut guard,
                                                                                    &mut chat_widget,
                                                                                    w,
                                                                                );
                                                                                frame_requester.schedule_frame();
                                                                                continue;
                                                                            }
                                                                            LocalShellSubmission::Interactive(command) => {
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::error(format!(
                                                                                        "Interactive shell `{command}` needs the terminal. Wait for the current turn to finish or interrupt it first."
                                                                                    )),
                                                                                );
                                                                                flush_chat_widget(
                                                                                    &mut guard,
                                                                                    &mut chat_widget,
                                                                                    w,
                                                                                );
                                                                                frame_requester.schedule_frame();
                                                                                continue;
                                                                            }
                                                                        }
                                                                        if output_settled_at.is_some() {
                                                                            // The response stream has ended, so this
                                                                            // is a real next-turn message rather than
                                                                            // guidance for a run that can no longer
                                                                            // consume it. Re-dispatch it as soon as
                                                                            // the durable current-turn settlement hands
                                                                            // ownership back to the outer loop.
                                                                            bottom_pane.queue_next_turn_submission(queued_text);
                                                                            frame_requester.schedule_frame();
                                                                            continue;
                                                                        }
                                                                        match submit_active_run_guidance(
                                                                            &active_turn_local_run_control,
                                                                            &queued_text,
                                                                        )
                                                                        .await
                                                                        {
                                                                                Ok(receipt) => {
                                                                                    bottom_pane.accept_user_intent(
                                                                                        receipt.intent_id,
                                                                                        receipt.delivery,
                                                                                        receipt.status,
                                                                                        queued_text,
                                                                                    );
                                                                                }
                                                                            Err(error) => {
                                                                                bottom_pane.composer.set_text(&queued_text);
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::error(error),
                                                                                );
                                                                            }
                                                                        }
                                                                    }
                                                                    BottomPaneAction::ExternalEditorUnavailable => {
                                                                        surface_external_editor_unavailable(
                                                                            &mut chat_widget,
                                                                        );
                                                                        flush_chat_widget(
                                                                            &mut guard,
                                                                            &mut chat_widget,
                                                                            w,
                                                                        );
                                                                    }
                                                                    BottomPaneAction::ViewAction(action) => {
                                                                        let terminal_height = guard
                                                                            .terminal
                                                                            .size()
                                                                            .map(|size| size.height)
                                                                            .unwrap_or(0);
                                                                        dispatch_bottom_pane_view_action(
                                                                            action,
                                                                            &mut background_registry,
                                                                            &server_agent_observer,
                                                                            &mut server_agent_projection_sequence,
                                                                            &task_board,
                                                                            &plan_task_observer,
                                                                            ViewActionBackends {
                                                                                agent_spawner: agent_spawner_for_cancel.clone(),
                                                                                delegation_engine: delegation_engine_for_control.clone(),
                                                                                api: api.clone(),
                                                                                profile: profile.map(str::to_string),
                                                                                session_id: background_registry_turn_session_id.clone(),
                                                                                file_writer: Some(file_writer.clone()),
                                                                                agent_workbench_tx: agent_workbench_tx.clone(),
                                                                            },
                                                                            &restored_local_agent_task_projections,
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                            w,
                                                                            terminal_height,
                                                                        )
                                                                        .await;
                                                                    }
                                                                    BottomPaneAction::ViewCompleted {
                                                                        result: Some(bottom_pane::view::ViewResult::Permission(mode)),
                                                                        ..
                                                                    } if slash_dispatch::permission_mode_requires_confirmation(mode) => {
                                                                        bottom_pane.push_view(Box::new(
                                                                            slash_dispatch::build_permission_mode_confirmation(mode),
                                                                        ));
                                                                        frame_requester.schedule_frame();
                                                                    }
                                                                    BottomPaneAction::ViewCompleted {
                                                                        result: Some(bottom_pane::view::ViewResult::Permission(mode)),
                                                                        ..
                                                                    } => {
                                                                        stage_permission_mode_for_next_turn(
                                                                            &mut bottom_pane,
                                                                            &mut chat_widget,
                                                                            mode,
                                                                        );
                                                                        frame_requester.schedule_frame();
                                                                    }
                                                                    BottomPaneAction::ViewCompleted {
                                                                        result: Some(bottom_pane::view::ViewResult::PermissionConfirmation {
                                                                            mode,
                                                                            confirmed: true,
                                                                        }),
                                                                        ..
                                                                    } => {
                                                                        stage_permission_mode_for_next_turn(
                                                                            &mut bottom_pane,
                                                                            &mut chat_widget,
                                                                            mode,
                                                                        );
                                                                        frame_requester.schedule_frame();
                                                                    }
                                                                    BottomPaneAction::ViewCompleted {
                                                                        result: Some(bottom_pane::view::ViewResult::PermissionConfirmation {
                                                                            confirmed: false,
                                                                            ..
                                                                        }),
                                                                        ..
                                                                    } => {}
                                                                    BottomPaneAction::ViewCompleted { result: Some(_), reopen: _ } => {}
                                                                    BottomPaneAction::ViewCompleted {
                                                                        result: None,
                                                                        reopen: Some(cmd),
                                                                    } if ReopenTarget::parse(&cmd)
                                                                        == Some(ReopenTarget::Agents)
                                                                        && reopen_agents_view(
                                                                            &chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        ) =>
                                                                    {
                                                                        continue;
                                                                    }
                                                                    BottomPaneAction::Interrupt
                                                                    | BottomPaneAction::Quit
                                                                        if output_settled_at.is_some() => {
                                                                        // The agentic loop has already settled its
                                                                        // last output. There is no model work left
                                                                        // for Ctrl+C to stop; leaving the turn to
                                                                        // settle preserves its durable result.
                                                                        continue;
                                                                    }
                                                                    BottomPaneAction::Interrupt | BottomPaneAction::Quit => {
                                                                        request_active_run_cancel(
                                                                            &mut chat_widget,
                                                                            task_service_for_cancel.as_ref(),
                                                                            &preinstalled_run_control,
                                                                            &tui_cancel_token,
                                                                        )
                                                                        .await;
                                                                        // Don't drain the queue here. The run is
                                                                        // being cancelled but may still emit
                                                                        // typed run-input applied events
                                                                        // before it fully stops — those must
                                                                        // keep popping the head and committing
                                                                        // to chat history. We record intent and
                                                                        // resolve leftover items once at turn
                                                                        // end (single decision point), so the
                                                                        // unhappy path "cancel failed / slow to
                                                                        // stop" can never drop user input.
                                                                        interrupt_pending = true;
                                                                        bottom_pane.interrupt_pending = true;
                                                                    }
                                                                    _ => {}
                                                                }
                                                            frame_requester.schedule_frame();
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                        }
                                                        event @ (TuiEvent::Resize | TuiEvent::Draw) => {
                                                            let w = guard
                                                                .terminal
                                                                .size()
                                                                .map(|s| s.width)
                                                                .unwrap_or(80);
                                                            if matches!(event, TuiEvent::Resize) {
                                                                refresh_open_transcript_view(
                                                                    &chat_widget,
                                                                    &mut bottom_pane,
                                                                    w,
                                                                );
                                                            }
                                                            {
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                        }
                                                        TuiEvent::Paste(text) => {
                                                            bottom_pane.handle_paste(&text);
                                                            frame_requester.schedule_frame();
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                        }
                                                    }
                                                }
                                                handoff = bash_detach_handoff_rx.recv() => {
                                                    bash_detach_request_pending = false;
                                                    let handoff = match handoff {
                                                        Some(handoff) => handoff,
                                                        None => {
                                                            tracing::warn!(
                                                                "bash detach handoff channel closed without payload"
                                                            );
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    "⏎ Backgrounding failed: runner disconnected",
                                                                ),
                                                            );
                                                            set_bash_background_hint_enabled(
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                                false,
                                                            );
                                                            frame_requester.schedule_frame();
                                                            if let Some(result) = turn_result_ready.take() {
                                                                break result;
                                                            }
                                                            continue;
                                                        }
                                                    };

                                                    match handoff {
                                                        Ok(payload) => {
                                                            if !background_registry.can_spawn_shell_task() {
                                                                let error = format!(
                                                                    "background shell task limit reached ({} running)",
                                                                    background_registry.running_count()
                                                                );
                                                                let astra_tools::detach::DetachedShellPayload {
                                                                    mut child,
                                                                    adoption_tx,
                                                                    ..
                                                                } = payload;
                                                                let _ = adoption_tx.send(Err(error));
                                                                let _ = child.kill().await;
                                                                let _ = child.wait().await;
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::error(
                                                                        "⏎ Backgrounding failed: task limit reached",
                                                                    ),
                                                                );
                                                                set_bash_background_hint_enabled(
                                                                    &mut chat_widget,
                                                                    &mut status_indicator,
                                                                    false,
                                                                );
                                                                frame_requester.schedule_frame();
                                                                if let Some(result) = turn_result_ready.take() {
                                                                    break result;
                                                                }
                                                                continue;
                                                            }

                                                            let astra_tools::detach::DetachedShellPayload {
                                                                child,
                                                                stdout,
                                                                stderr,
                                                                command,
                                                                partial_stdout,
                                                                partial_stderr,
                                                                adoption_tx,
                                                            } = payload;
                                                            let id = match background_registry.adopt_detached_shell(
                                                                child,
                                                                stdout,
                                                                stderr,
                                                                &command,
                                                                partial_stdout,
                                                                partial_stderr,
                                                            ) {
                                                                Ok(id) => {
                                                                    let _ = adoption_tx.send(Ok(id.clone()));
                                                                    id
                                                                }
                                                                Err(error) => {
                                                                    let _ = adoption_tx.send(Err(error.clone()));
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::error(
                                                                            format!("⏎ Backgrounding failed: {error}")
                                                                        ),
                                                                    );
                                                                    set_bash_background_hint_enabled(
                                                                        &mut chat_widget,
                                                                        &mut status_indicator,
                                                                        false,
                                                                    );
                                                                    let _ = reveal_background_task_view_with_extra_rows(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                        Vec::new(),
                                                                        None,
                                                                    )
                                                                    .await;
                                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                                    frame_requester.schedule_frame();
                                                                    if let Some(result) = turn_result_ready.take() {
                                                                        break result;
                                                                    }
                                                                    continue;
                                                                }
                                                            };

                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::background_task(
                                                                    format!("Running in background · {id} · open the task panel to inspect")
                                                                ),
                                                            );
                                                            set_bash_background_hint_enabled(
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                                false,
                                                            );
                                                            persist_background_task_projections_if_changed(
                                                                &mut background_registry,
                                                                background_registry_turn_session_id.as_deref(),
                                                                background_registry_turn_model.as_deref(),
                                                                &mut background_task_projection_cache,
                                                            ).await;
                                                            frame_requester.schedule_frame();
                                                        }
                                                        Err(error) => {
                                                            *bash_detach_slot_for_ctrl_b.lock().await = None;
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    format!("⏎ Backgrounding failed: {error}")
                                                                ),
                                                            );
                                                            set_bash_background_hint_enabled(
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                                false,
                                                            );
                                                            let _ = reveal_background_task_view_with_extra_rows(
                                                                &mut background_registry,
                                                                agent_spawner_for_cancel.as_ref(),
                                                                &restored_local_agent_task_projections,
                                                                &mut bottom_pane,
                                                                &frame_requester,
                                                                Vec::new(),
                                                                None,
                                                            )
                                                            .await;
                                                        }
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                    frame_requester.schedule_frame();
                                                    if let Some(result) = turn_result_ready.take() {
                                                        break result;
                                                    }
                                                    continue;
                                                }
                                                Some(ae) = tui_rx.recv() => {
                                                    if matches!(
                                                        &ae,
                                                        TuiAppEvent::AssistantOutputSettled
                                                            | TuiAppEvent::TurnStreamClosed
                                                    ) {
                                                        let now = std::time::Instant::now();
                                                        if !settle_visible_reply(
                                                            &mut output_settled_at,
                                                            &mut chat_widget,
                                                            &mut bottom_pane,
                                                            &mut status_indicator,
                                                            now,
                                                        ) {
                                                            continue;
                                                        }
                                                        let w = guard
                                                            .terminal
                                                            .size()
                                                            .map(|size| size.width)
                                                            .unwrap_or(80);
                                                        refresh_open_transcript_view(
                                                            &chat_widget,
                                                            &mut bottom_pane,
                                                            w,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    // Track per-turn metrics
                                                    match &ae {
                                                        TuiAppEvent::Token(_)
                                                            if turn_ttft.is_none() => {
                                                                turn_ttft = Some(std::time::Instant::now());
                                                            }
                                                        TuiAppEvent::ToolStarted { .. } => {
                                                            turn_tool_count += 1;
                                                        }
                                                        TuiAppEvent::ExplainReport(items) if !items.is_empty() => {
                                                            explain_items.extend(items.clone());
                                                            continue;
                                                        }
                                                        _ => {}
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    // Shadow mirror into ChatWidget.
                                                    // Clone the event because handle_app_event
                                                    // consumes it by value on the app-event path.
                                                    if let Some(new_ev) = chat_widget::translate(
                                                        ae.clone(),
                                                        chat_widget::TurnContext::default(),
                                                    ) {
                                                        chat_widget.handle_event(new_ev);
                                                        refresh_open_agent_views_for_event(&ae, &chat_widget, &mut bottom_pane);
                                                    }
                                                    if matches!(&ae, TuiAppEvent::AgentLiveGap(_))
                                                        && server_agent_observer.request_refresh()
                                                    {
                                                        reconcile_server_agent_observer(
                                                            &server_agent_observer,
                                                            &mut server_agent_projection_sequence,
                                                            &mut chat_widget,
                                                            &mut bottom_pane,
                                                            &frame_requester,
                                                        );
                                                    }
                                                    apply_tui_control_event(
                                                        &ae,
                                                        &mut bottom_pane,
                                                        &mut chat_widget,
                                                    );
                                                    refresh_open_transcript_view(
                                                        &chat_widget,
                                                        &mut bottom_pane,
                                                        w,
                                                    );
                                                    handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                                                    let should_rearm_bash_detach =
                                                        !bash_detach_request_pending
                                                            && bash_detach_listener.is_none()
                                                            && match &ae {
                                                                TuiAppEvent::ToolStarted {
                                                                    name,
                                                                    description,
                                                                    tool_use_id,
                                                                    parent_tool_use_id,
                                                                    ..
                                                                } => {
                                                                    if name == "bash"
                                                                        && parent_tool_use_id.is_none()
                                                                    {
                                                                        active_bash_tool_use_id =
                                                                            Some(tool_use_id.clone());
                                                                        active_bash_description =
                                                                            Some(description.clone());
                                                                    }
                                                                    name == "bash"
                                                                }
                                                                TuiAppEvent::ToolCompleted {
                                                                    tool_use_id,
                                                                    ..
                                                                } => {
                                                                    if active_bash_tool_use_id.as_deref()
                                                                        == Some(tool_use_id.as_str())
                                                                    {
                                                                        active_bash_tool_use_id = None;
                                                                        active_bash_description = None;
                                                                    }
                                                                    true
                                                                }
                                                                _ => false,
                                                            };
                                                    if should_rearm_bash_detach {
                                                        bash_detach_listener = Some(
                                                            install_bash_detach_listener(
                                                                &bash_detach_slot_for_ctrl_b,
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                            )
                                                            .await,
                                                        );
                                                    }
                                                    set_bash_background_hint_enabled(
                                                        &mut chat_widget,
                                                        &mut status_indicator,
                                                        bash_detach_hint_enabled(
                                                            bash_detach_listener.as_ref(),
                                                        ),
                                                    );
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                }
                                                Some(req) = approval_rx.recv() => {
                                                    // Non-blocking: enqueue only. The live, interactive
                                                    // approval card is rendered by BottomPane above the
                                                    // composer so arrow-key focus is visible. Resolve
                                                    // events flush a compact audit line to scrollback.
                                                    let _id = if let Some(metadata) = req.metadata {
                                                        bottom_pane.enqueue_approval_with_metadata(
                                                            req.tool,
                                                            req.header,
                                                            req.detail,
                                                            req.reason,
                                                            req.args,
                                                            req.response_tx,
                                                            *metadata,
                                                        )
                                                    } else {
                                                        bottom_pane.enqueue_approval(
                                                            req.tool,
                                                            req.header,
                                                            req.detail,
                                                            req.reason,
                                                            req.args,
                                                            req.response_tx,
                                                        )
                                                    };
                                                    let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    refresh_open_transcript_view(
                                                        &chat_widget,
                                                        &mut bottom_pane,
                                                        width,
                                                    );
                                                    frame_requester.schedule_frame();
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                 }
                                                Some(req) = ask_user_rx.recv() => {
                                                    // Draft transition: show a brief
                                                    // indicator before the ask-user form
                                                    // opens so the user isn't surprised by
                                                    // a sudden modal.
                                                    chat_widget.commit_system(
                                                        crate::tui::history_cell::system::SystemCell::response(
                                                            "🤔 The agent needs your input — opening question…",
                                                        ),
                                                    );
                                                    bottom_pane.enqueue_ask_user(req.prompt, req.response_tx);
                                                    frame_requester.schedule_frame();
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                }
                                                Some(req) = plan_review_rx.recv() => {
                                                    bottom_pane.enqueue_plan_review(req.plan_markdown, req.response_tx);
                                                    frame_requester.schedule_frame();
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    let frame = active_viewport(
                                                        &chat_widget,
                                                        &status_indicator,
                                                        Some(&*task_board),
                                                        board_expanded,
                                                        board_user_pin,
                                                        w,
                                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                                    );
                                                    board_expanded = frame.resolved_board_expanded;
                                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                                }
                                                discovery = &mut external_skill_discovery, if external_skill_discovery_pending => {
                                                    external_skill_discovery_pending = false;
                                                    apply_external_capability_discovery(
                                                        discovery,
                                                        &skill_registry,
                                                        &mut bottom_pane,
                                                        &mut chat_widget,
                                                    );
                                                    let mcp_extras = {
                                                        let manager = mcp_manager.read().await;
                                                        crate::cli::slash::slash_mcp::build_mcp_extra_subcommands(&manager)
                                                    };
                                                    bottom_pane.update_mcp_completions(mcp_extras);
                                                    let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                                                    refresh_open_transcript_view(
                                                        &chat_widget,
                                                        &mut bottom_pane,
                                                        width,
                                                    );
                                                    flush_chat_widget(&mut guard, &mut chat_widget, width);
                                                    frame_requester.schedule_frame();
                                                }
                                                _ = &mut itick => {
                                                    drain_background_task_commands(
                                                        &bg_task_commands_for_turn,
                                                        &mut background_registry,
                                                        agent_spawner_for_cancel.as_ref(),
                                                        &restored_local_agent_task_projections,
                                                        &bg_task_list_cache_for_turn,
                                                    )
                                                    .await;
                                                    drain_agent_workbench_outcomes(
                                                        &mut agent_workbench_rx,
                                                        background_registry_session_id.as_deref(),
                                                        &mut chat_widget,
                                                        &mut bottom_pane,
                                                        &frame_requester,
                                                    );
                                                    reconcile_server_agent_observer(
                                                        &server_agent_observer,
                                                        &mut server_agent_projection_sequence,
                                                        &mut chat_widget,
                                                        &mut bottom_pane,
                                                        &frame_requester,
                                                    );
                                                    let terminal_size = guard.terminal.size().ok();
                                                    dispatch_projection_actions(
                                                        &mut background_registry,
                                                        &server_agent_observer,
                                                        &mut server_agent_projection_sequence,
                                                        &task_board,
                                                        &plan_task_observer,
                                                        ViewActionBackends {
                                                            agent_spawner: agent_spawner_for_cancel.clone(),
                                                            delegation_engine: delegation_engine_for_control.clone(),
                                                            api: api.clone(),
                                                            profile: profile.map(str::to_string),
                                                            session_id: background_registry_turn_session_id.clone(),
                                                            file_writer: Some(file_writer.clone()),
                                                            agent_workbench_tx: agent_workbench_tx.clone(),
                                                        },
                                                        &restored_local_agent_task_projections,
                                                        &mut chat_widget,
                                                        &mut bottom_pane,
                                                        &frame_requester,
                                                        terminal_size.map(|size| size.width).unwrap_or(80),
                                                        terminal_size.map(|size| size.height).unwrap_or(0),
                                                    )
                                                    .await;

                                                    if std::time::Instant::now()
                                                        >= next_local_agent_reconcile
                                                    {
                                                        let next_snapshot =
                                                            super::local_agent_snapshot::LocalAgentSnapshot::capture(
                                                                agent_spawner_for_cancel.as_ref(),
                                                            )
                                                            .await;
                                                        let notifications = next_snapshot
                                                            .attention_notifications_since(
                                                                &local_agent_snapshot,
                                                            );
                                                        for notification in notifications {
                                                            if submit_active_runtime_notification(
                                                                &active_turn_local_run_control,
                                                                &notification,
                                                            )
                                                            .await
                                                            .is_err()
                                                            {
                                                                deferred_active_bg_notifications
                                                                    .push(notification);
                                                            }
                                                        }
                                                        local_agent_snapshot = next_snapshot;
                                                        let changed = chat_widget
                                                            .reconcile_local_agent_snapshot(
                                                                &local_agent_snapshot,
                                                                &restored_local_agent_task_projections,
                                                            );
                                                        next_local_agent_reconcile =
                                                            std::time::Instant::now()
                                                                + LOCAL_AGENT_RECONCILE_INTERVAL;
                                                        if changed {
                                                            refresh_open_agent_views(
                                                                &chat_widget,
                                                                &mut bottom_pane,
                                                            );
                                                        }
                                                    }

                                                    // Background terminal facts are consumed even
                                                    // while the foreground agent owns `&mut state`.
                                                    // Route them through local run control so the
                                                    // next model boundary sees them without polling;
                                                    // if output has already settled, preserve them for
                                                    // the next turn instead of fabricating user input.
                                                    let bg_events = background_registry.poll_completions();
                                                    for event in &bg_events {
                                                        if !background_task_event_requires_model_attention(event) {
                                                            continue;
                                                        }
                                                        let notification =
                                                            super::background_tasks::format_notification_xml(event);
                                                        if notification.is_empty() {
                                                            continue;
                                                        }
                                                        let delivered = output_settled_at.is_none()
                                                            && submit_active_runtime_notification(
                                                                &active_turn_local_run_control,
                                                                &notification,
                                                            )
                                                            .await
                                                            .is_ok();
                                                        if !delivered {
                                                            deferred_active_bg_notifications
                                                                .push(notification);
                                                        }
                                                    }
                                                    for message in
                                                        background_task_event_system_messages(&bg_events)
                                                    {
                                                        chat_widget.commit_system(
                                                            history_cell::system::SystemCell::info(&message),
                                                        );
                                                    }
                                                    if !bg_events.is_empty() {
                                                        persist_background_task_projections_if_changed(
                                                            &mut background_registry,
                                                            background_registry_turn_session_id.as_deref(),
                                                            background_registry_turn_model.as_deref(),
                                                            &mut background_task_projection_cache,
                                                        )
                                                        .await;
                                                    }
                                                    // The tool executor reads this cache directly.
                                                    // Refresh it during active turns too, including
                                                    // immediately after a shell handoff or local `&`
                                                    // launch, rather than exposing the last idle tick.
                                                    let rows = background_task_rows_with_agent_snapshot(
                                                        &mut background_registry,
                                                        &local_agent_snapshot,
                                                        &restored_local_agent_task_projections,
                                                    );
                                                    *bg_task_list_cache_for_turn.write().await =
                                                        render_background_task_rows_xml(&rows);
                                                    sync_background_task_footer_from_rows(
                                                        &mut bottom_pane,
                                                        &rows,
                                                    );
                                                    if bottom_pane.accepts_background_task_rows() {
                                                        bottom_pane.refresh_background_task_rows(rows);
                                                    }
                                                    // Refresh the permission-mode chip via the
                                                    // lock-free mirror — the agentic loop holds
                                                    // `&mut state` so reading `state.perm_manager`
                                                    // here would clash. Catches turn-boundary
                                                    // pivots (e.g. exit_plan_mode → Auto) within
                                                    // one inner tick.
                                                    let live_mode = perm_mode_mirror.current();
                                                    if bottom_pane.footer.permission_mode
                                                        != Some(live_mode)
                                                    {
                                                        bottom_pane.footer.permission_mode = Some(live_mode);
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                                    board_expanded = frame.resolved_board_expanded;
                                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                                }
                                            }
                                        };
                                        let mut pending_runtime_notifications =
                                            preinstalled_run_control
                                                .take_pending_runtime_notifications();
                                        if !pending_runtime_notifications.is_empty() {
                                            let settled_agent_snapshot =
                                                super::local_agent_snapshot::LocalAgentSnapshot::capture(
                                                    agent_spawner_for_cancel.as_ref(),
                                                )
                                                .await;
                                            pending_runtime_notifications.retain(|notification| {
                                                settled_agent_snapshot
                                                    .notification_still_requires_reconciliation(
                                                        notification,
                                                    )
                                            });
                                        }
                                        deferred_active_bg_notifications
                                            .extend(pending_runtime_notifications);
                                        // Turn fully settled: blit any images display_sixel
                                        // queued this turn on a paused screen (see fn docs).
                                        render_pending_sixel_images(&mut guard).await;
                                        r
                                    };

                                    if !deferred_active_bg_notifications.is_empty() {
                                        state
                                            .pending_bg_notifications
                                            .extend(deferred_active_bg_notifications);
                                        runtime_notification_wake_at.get_or_insert_with(|| {
                                            std::time::Instant::now()
                                                + Duration::from_millis(200)
                                        });
                                    }

                                    if let Some(mode) = bottom_pane.take_staged_permission_mode() {
                                        slash_dispatch::apply_permission_mode_selection(
                                            &mut state,
                                            &mut bottom_pane,
                                            &mut chat_widget,
                                            mode,
                                        );
                                    }

                                    *astra_core::sync_poison::recover_mutex_lock(
                                        &state.active_turn_local_run_control,
                                    ) = None;
                                    state.tui_stream_event_tx = None;
                                    state.tui_agent_live_event_sink = None;

                                    // The stream bridge owns only the transport boundary. It
                                    // freezes the reply as soon as its sender closes; do not
                                    // await a terminal event here. Waiting on bookkeeping after
                                    // the user-visible answer has landed is exactly the
                                    // completion-phase input stall this loop must avoid.
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);

                                    if let Some(output_settled_at) = output_settled_at {
                                        tracing::debug!(
                                            target: "astra_cli::turn_settlement",
                                            output_to_turn_settled_ms = output_settled_at.elapsed().as_millis() as u64,
                                            turn_ok = turn_result.is_ok(),
                                            "foreground turn settled after model-visible output"
                                        );
                                    }

                                    // Resolve the active-run intent ledger once the run ends.
                                    // Applied events have already removed their identities. A
                                    // normal completion must not turn an accepted submission
                                    // into an inert draft merely because the model had already
                                    // produced its visible answer before the next safe boundary.
                                    let unapplied_user_intents =
                                        bottom_pane.take_unapplied_user_intents();
                                    let should_start_followups =
                                        turn_result.is_ok() && !state.last_turn_interrupted;
                                    if interrupt_pending {
                                        interrupt_pending = false;
                                        bottom_pane.interrupt_pending = false;
                                    }
                                    let mut post_output_submissions =
                                        bottom_pane.take_queued_next_turn_submissions();
                                    if let Some(restored) = settle_followup_submissions(
                                        &mut queued_followup_submissions,
                                        unapplied_user_intents
                                            .iter()
                                            .map(|intent| intent.text.clone()),
                                        &mut post_output_submissions,
                                        should_start_followups,
                                    ) {
                                        let preview = user_intent_preview(&restored);
                                        bottom_pane.restore_into_composer(&restored);
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::info(format!(
                                                "Queued input was not started because the run did not settle normally; draft restored: {preview}",
                                            )),
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }

                                    if turn_result.is_ok()
                                        && commit_explain_dag(
                                            &state,
                                            &explain_items,
                                            pre_cached_context_trace_turn_id.as_deref(),
                                            pre_context_trace_count,
                                            &mut chat_widget,
                                        )
                                    {
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }

                                    // Turn end — ChatWidget handles any
                                    // remaining live cell on TurnComplete.
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    set_bash_background_hint_enabled(
                                        &mut chat_widget,
                                        &mut status_indicator,
                                        false,
                                    );

                                    bottom_pane.set_task_status(TaskStatus::Idle);
                                    status_indicator.set_state(
                                        status_indicator::IndicatorState::Idle,
                                    );
                                    // Session id may have been assigned by
                                    // the server during the turn. Re-seat
                                    // so subsequent turns persist under the
                                    // correct id.
                                    if let Some(ref sid) = state.session_id
                                        && chat_widget.session_id() != sid
                                    {
                                        chat_widget.set_session_id(sid.clone());
                                        rebind_workbench_observers(
                                            Some(sid),
                                            &task_board,
                                            &server_agent_observer,
                                            &plan_task_observer,
                                            &mut board_user_pin,
                                        );
                                    }
                                    if let Err(ref e) = turn_result {
                                        if let Some(ev) = chat_widget::translate(
                                            TuiAppEvent::TurnError(e.clone()),
                                            chat_widget::TurnContext::default(),
                                        ) {
                                            chat_widget.handle_event(ev);
                                        }
                                    }

                                    // Update footer
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    if let Some(ref s) = state.session_id { bottom_pane.footer.session_id = Some(s[..8.min(s.len())].to_string()); }
                                    bottom_pane.footer.token_usage = Some(format!("{}↑ {}↓", state.total_prompt_tokens, state.total_completion_tokens));
                                    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
                                    // The live stream has already updated the footer from the
                                    // current request's assembly and provider usage. A trace is
                                    // only a recovery fallback for paths that did not expose the
                                    // live context signals.
                                    let turn_prompt = state.total_prompt_tokens - pre_prompt_tokens;
                                    let turn_completion = state.total_completion_tokens - pre_completion_tokens;
                                    let turn_cache_read = state.total_cache_read_tokens - pre_cache_read;
                                    let turn_cache_creation = state.total_cache_creation_tokens - pre_cache_creation;
                                    let turn_input = total_input_tokens(
                                        turn_prompt,
                                        turn_cache_read,
                                        turn_cache_creation,
                                    );
                                    let footer_context_trace = latest_context_trace_since(
                                        &state,
                                        pre_cached_context_trace_turn_id.as_deref(),
                                        pre_context_trace_count,
                                    )
                                    .or_else(|| state.latest_context_assembly_trace.clone());
                                    if bottom_pane.footer.context_window.is_none()
                                        && let Some(usage) = footer_context_trace
                                            .as_ref()
                                            .and_then(context_window_from_trace)
                                    {
                                        bottom_pane.footer.restore_context_window(usage);
                                    }

                                    // Turn summary: dispatch to ChatWidget,
                                    // which builds the TurnSummaryCell and
                                    // persists it. `flush_chat_widget` below
                                    // paints it into scrollback.
                                    {
                                        let elapsed = turn_start.elapsed();
                                        let ttft_ms = turn_ttft.map(|t| {
                                            t.duration_since(turn_start).as_millis() as u64
                                        });
                                        let ctx = chat_widget::TurnContext {
                                            elapsed_ms: Some(elapsed.as_millis() as u64),
                                            ttft_ms,
                                            tokens_in: Some(turn_input),
                                            tokens_out: Some(turn_completion),
                                            // Drive the `💾 N%` segment:
                                            // hit rate = cache_read / total_input.
                                            // Only plumbed when the provider
                                            // reported a cache_read value this
                                            // turn — `None` keeps the segment
                                            // off entirely (first turn, non-
                                            // caching provider, etc.).
                                            cache_read_tokens: (turn_cache_read > 0)
                                                .then_some(turn_cache_read),
                                            tools: turn_tool_count,
                                            cumulative_tokens: Some(
                                                state.total_prompt_tokens
                                                    + state.total_completion_tokens
                                                    + state.total_cache_read_tokens
                                                    + state.total_cache_creation_tokens,
                                            ),
                                            cumulative_cost_usd: Some(state.total_session_cost),
                                        };
                                        if let Some(ev) = chat_widget::translate(
                                            TuiAppEvent::TurnComplete,
                                            ctx,
                                        ) {
                                            chat_widget.handle_event(ev);
                                        }
                                    }
                                    // Flush everything new from the widget
                                    // (assistant cell + tool cells +
                                    // possibly TurnSummary + SystemError) to
                                    // scrollback in one shot.
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);

                                    let new_tok = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                                    tui_cancel_token = new_tok.clone();
                                    state.tui_cancel_token = Some(new_tok);

                                    // A user may keep typing while a queued follow-up is waiting.
                                    // Preserve that draft instead of replacing it; the queued text
                                    // is restored beside it so neither intent is silently lost.
                                    if !queued_followup_submissions.is_empty()
                                        && !bottom_pane.composer.is_empty()
                                    {
                                        let restored = std::mem::take(
                                            &mut queued_followup_submissions,
                                        )
                                        .into_iter()
                                        .collect::<Vec<_>>()
                                        .join("\n\n");
                                        let preview = user_intent_preview(&restored);
                                        bottom_pane.restore_into_composer(&restored);
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::info(format!(
                                                "Queued follow-up kept with your current draft: {preview}",
                                            )),
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }

                                    if let Some(next_turn_submission) = queued_followup_submissions.pop_front() {
                                        bottom_pane.composer.set_text(&next_turn_submission);
                                        event_stream.push_front(TuiEvent::Key(
                                            crossterm::event::KeyEvent::new(
                                                crossterm::event::KeyCode::Enter,
                                                crossterm::event::KeyModifiers::NONE,
                                            ),
                                        ));
                                    }

                                }
                            }
                            BottomPaneAction::OpenExternalEditor(initial) => {
                                edit_composer_in_external_editor(
                                    &mut guard,
                                    &mut bottom_pane,
                                    &mut chat_widget,
                                    initial,
                                )
                                .await;
                            }
                            BottomPaneAction::ExternalEditorUnavailable => {
                                surface_external_editor_unavailable(&mut chat_widget);
                                let width =
                                    guard.terminal.size().map(|size| size.width).unwrap_or(80);
                                flush_chat_widget(&mut guard, &mut chat_widget, width);
                            }
                            BottomPaneAction::ViewAction(action) => {
                                let terminal_size = guard.terminal.size().ok();
                                dispatch_bottom_pane_view_action(
                                    action,
                                    &mut background_registry,
                                    &server_agent_observer,
                                    &mut server_agent_projection_sequence,
                                    &task_board,
                                    &plan_task_observer,
                                    ViewActionBackends {
                                        agent_spawner: state.agent_spawner.clone(),
                                        delegation_engine: state.delegation_engine.clone(),
                                        api: api.clone(),
                                        profile: profile.map(str::to_string),
                                        session_id: state.session_id.clone(),
                                        file_writer: Some(file_writer.clone()),
                                        agent_workbench_tx: agent_workbench_tx.clone(),
                                    },
                                    &restored_local_agent_task_projections,
                                    &mut chat_widget,
                                    &mut bottom_pane,
                                    &frame_requester,
                                    terminal_size.map(|size| size.width).unwrap_or(80),
                                    terminal_size.map(|size| size.height).unwrap_or(0),
                                )
                                .await;
                            }
                            BottomPaneAction::ViewCompleted { result, reopen } => {
                                if let Some(result) = result {
                                    if let bottom_pane::view::ViewResult::WorkspaceTrust(choice) = &result {
                                        match choice {
                                            bottom_pane::view::WorkspaceTrustChoice::Trust => {
                                                match state.perm_manager.trust_workspace() {
                                                    Ok(message) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::response(
                                                            message,
                                                        ),
                                                    ),
                                                    Err(err) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!(
                                                                "Failed to trust workspace: {err}"
                                                            ),
                                                        ),
                                                    ),
                                                }
                                            }
                                            bottom_pane::view::WorkspaceTrustChoice::ContinueUntrusted => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::info(
                                                        "Continuing without trusting this workspace. Saved workspace rules stay off for this session."
                                                            .to_string(),
                                                    ),
                                                );
                                            }
                                            bottom_pane::view::WorkspaceTrustChoice::MarkUntrusted => {
                                                match state.perm_manager.untrust_workspace() {
                                                    Ok(message) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::response(
                                                            message,
                                                        ),
                                                    ),
                                                    Err(err) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!(
                                                                "Failed to mark workspace untrusted: {err}"
                                                            ),
                                                        ),
                                                    ),
                                                }
                                            }
                                        }
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    if let bottom_pane::view::ViewResult::Login { username, password } = &result {
                                        match crate::cli::auth_flow::do_login(api, profile, username, password).await {
                                            Ok(token) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::response(format!("Logged in as {username}")));
                                                let sync_report = crate::post_auth_cloud_resync(profile, &mut state).await;
                                                if let Some(notice) = sync_report.user_notice() {
                                                    chat_widget.commit_system(
                                                        history_cell::system::SystemCell::warning(notice),
                                                    );
                                                }
                                                if let Some(model) = sync_default_model_after_auth(
                                                    api,
                                                    &token,
                                                    &mut state,
                                                    &mut bottom_pane,
                                                )
                                                .await
                                                {
                                                    chat_widget.commit_system(
                                                        history_cell::system::SystemCell::response(
                                                            format!("Default model: {model}"),
                                                        ),
                                                    );
                                                }
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Login failed: {e}")));
                                            }
                                        }
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    if let bottom_pane::view::ViewResult::Register { username, email, password } = &result {
                                        match crate::cli::auth_flow::do_register(api, profile, username, email, password).await {
                                            Ok(_) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::response("Registered — logging in…"));
                                                match crate::cli::auth_flow::do_login(api, profile, username, password).await {
                                                    Ok(token) => {
                                                        chat_widget.commit_system(history_cell::system::SystemCell::response(format!("Logged in as {username}")));
                                                        let sync_report = crate::post_auth_cloud_resync(profile, &mut state).await;
                                                        if let Some(notice) = sync_report.user_notice() {
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::warning(notice),
                                                            );
                                                        }
                                                        if let Some(model) = sync_default_model_after_auth(
                                                            api,
                                                            &token,
                                                            &mut state,
                                                            &mut bottom_pane,
                                                        )
                                                        .await
                                                        {
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    format!("Default model: {model}"),
                                                                ),
                                                            );
                                                        }
                                                    }
                                                    Err(e) => {
                                                        chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Auto-login failed: {e}")));
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Register failed: {e}")));
                                            }
                                        }
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    if let bottom_pane::view::ViewResult::ConfigEdit {
                                        disposition,
                                        toml_body,
                                    } = &result {
                                        let result =
                                            crate::tui::config_edit_router::finalize_async(
                                                *disposition,
                                                toml_body.clone(),
                                            )
                                            .await;
                                        let msg = match result {
                                            Ok(outcome) => {
                                                if let Some(save) = outcome.save.as_ref() {
                                                    let prev = state.config_version_id.clone();
                                                    if let (Some(ref j), Some(ref sid)) = (
                                                        state.journal.as_ref(),
                                                        state.session_id.as_ref(),
                                                    ) {
                                                        let ev = astra_services::session_journal::JournalEvent::config_version_change(
                                                            Some(sid.as_str()),
                                                            state.turn,
                                                            prev.as_deref(),
                                                            &save.new_version_id,
                                                            save.source,
                                                        );
                                                        let _ = j.append(&ev);
                                                    }
                                                    state.config_version_id =
                                                        Some(save.new_version_id.clone());
                                                }
                                                history_cell::system::SystemCell::response(outcome.message)
                                            }
                                            Err(e) => history_cell::system::SystemCell::error(e),
                                        };
                                        chat_widget.commit_system(msg);
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // `/model` picker → check thinking capability.
                                    if let bottom_pane::view::ViewResult::Model { name: base_model } = &result {
                                        let base_model = base_model.clone();
                                        let raw = model_catalog_cache.clone().unwrap_or_default();
                                        let entry = crate::cli::slash::slash_router::find_model_entry_by_name(
                                            &raw,
                                            &base_model,
                                        );
                                        let thinking_cap = entry
                                            .and_then(crate::cli::slash::slash_router::entry_thinking_capability);
                                        let provider =
                                            entry.and_then(crate::cli::slash::slash_router::entry_provider);
                                        let model_id = entry
                                            .and_then(crate::cli::slash::slash_router::entry_model_id)
                                            .map(ToOwned::to_owned);
                                        let opts = astra_turn_core::thinking_config::thinking_options_with_capability(
                                            &base_model,
                                            provider,
                                            thinking_cap,
                                        );
                                        if opts.is_empty() {
                                            state.model = Some(base_model.clone());
                                            crate::cli::slash::slash_config::set_active_model_for_display(
                                                Some(base_model.clone()),
                                            );
                                            crate::cli::slash::slash_config::set_active_model_id_for_request(
                                                model_id,
                                            );
                                            bottom_pane.footer.model = Some(base_model.clone());
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::response(
                                                    format!("Set model to {base_model}"),
                                                ),
                                            );
                                            pending_deferred_slash_flush = false;
                                            let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        } else {
                                            use crate::tui::bottom_pane::list_selection_view::{
                                                ListSelectionView, SelectionItem,
                                            };
                                            let items: Vec<SelectionItem> = opts
                                                .iter()
                                                .map(|o| SelectionItem {
                                                    name: o.label.to_string(),
                                                    description: None,
                                                    is_current: o.is_default,
                                                })
                                                .collect();
                                            let view = ListSelectionView::new(
                                                items,
                                                Some(format!("Select thinking mode for {base_model}:")),
                                            )
                                            .with_footer_hint(
                                                slash_dispatch::MODEL_THINKING_PICKER_FOOTER_HINT,
                                            )
                                            .with_results(
                                                opts.into_iter()
                                                    .map(|option| bottom_pane::view::ViewResult::ModelThinking {
                                                        base_model: base_model.clone(),
                                                        config: option.config,
                                                    })
                                                    .collect(),
                                            );
                                            bottom_pane.push_view(Box::new(view));
                                        }
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // `/model` thinking-mode picker.
                                    if let bottom_pane::view::ViewResult::ModelThinking {
                                        base_model,
                                        config,
                                    } = &result {
                                        let raw = model_catalog_cache.clone().unwrap_or_default();
                                        let entry = crate::cli::slash::slash_router::find_model_entry_by_name(
                                            &raw,
                                            &base_model,
                                        );
                                        let model_id = entry
                                            .and_then(crate::cli::slash::slash_router::entry_model_id)
                                            .map(ToOwned::to_owned);
                                        let suffix = astra_turn_core::thinking_config::thinking_suffix_for(config);
                                        let composed = format!("{base_model}{suffix}");
                                        state.model = Some(composed.clone());
                                        crate::cli::slash::slash_config::set_active_model_for_display(
                                            Some(composed.clone()),
                                        );
                                        crate::cli::slash::slash_config::set_active_model_id_for_request(
                                            model_id,
                                        );
                                        bottom_pane.footer.model = Some(composed.clone());
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::response(format!(
                                                "Set model to {composed}"
                                            )),
                                        );
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // Session picker result → restore the selected session
                                    // in-place. Its semantic intent is explicit, so a session
                                    // id can never be misread as an unrelated picker label.
                                    if let bottom_pane::view::ViewResult::Session {
                                        session_id: name,
                                        intent: bottom_pane::view::SessionSelectionIntent::Resume,
                                    } = &result {
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        let pre_sid = state.session_id.clone();
                                        if let Err(error) = crate::cli::slash::slash_session::restore_session_into_state(
                                            &name,
                                            profile,
                                            api,
                                            &mut state,
                                        )
                                        .await
                                        {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(format!(
                                                    "Could not resume session: {error}"
                                                )),
                                            );
                                        }
                                        // If the resume attached a new session
                                        // id, swap the ChatWidget to replay
                                        // that session's transcript. The
                                        // `replay_session_into_widget` helper
                                        // emits its own "resumed N cells"
                                        // banner — so no extra info line here.
                                        if state.session_id != pre_sid
                                            && let Some(ref new_sid) = state.session_id
                                            && !new_sid.is_empty()
                                        {
                                            chat_widget = replay_session_into_widget(
                                                &mut guard,
                                                new_sid,
                                                w,
                                            )
                                            .await;
                                            rebind_workbench_observers(
                                                Some(new_sid),
                                                &task_board,
                                                &server_agent_observer,
                                                &plan_task_observer,
                                                &mut board_user_pin,
                                            );
                                        }
                                        bottom_pane.footer.session_id = state
                                            .session_id
                                            .as_ref()
                                            .map(|s| s[..8.min(s.len())].to_string());
                                    } else if let bottom_pane::view::ViewResult::Session {
                                        session_id: parent_id,
                                        intent: bottom_pane::view::SessionSelectionIntent::Fork,
                                    } = &result {
                                        match crate::cli::slash::slash_session::fork_session_into_state(
                                            parent_id,
                                            None,
                                            api,
                                            profile,
                                            &mut state,
                                        )
                                        .await
                                        {
                                            Ok(outcome) => {
                                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                chat_widget = replay_session_into_widget(
                                                    &mut guard,
                                                    &outcome.new_session_id,
                                                    w,
                                                )
                                                .await;
                                                rebind_workbench_observers(
                                                    Some(&outcome.new_session_id),
                                                    &task_board,
                                                    &server_agent_observer,
                                                    &plan_task_observer,
                                                    &mut board_user_pin,
                                                );
                                                let task_board_note = if outcome.preserved_existing_child_task_board {
                                                    " · preserved the child task board"
                                                } else {
                                                    " · copied the parent task board"
                                                };
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::response(format!(
                                                        "Forked session {} from {} · copied {} journal events{}",
                                                        outcome.new_session_id,
                                                        parent_id,
                                                        outcome.events_copied,
                                                        task_board_note,
                                                    )),
                                                );
                                                bottom_pane.footer.session_id = Some(
                                                    outcome.new_session_id[..8.min(outcome.new_session_id.len())]
                                                        .to_string(),
                                                );
                                            }
                                            Err(error) => chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(format!(
                                                    "Could not fork session: {error}"
                                                )),
                                            ),
                                        }
                                    } else {
                                        slash_dispatch::handle_view_result(
                                            result,
                                            &mut state,
                                            &mut bottom_pane,
                                            &mut chat_widget,
                                        );
                                    }
                                    // Flush view-driven system cells
                                    // (login success, permission change,
                                    // etc.) into scrollback without waiting
                                    // for the 50ms tick.
                                    let _w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    flush_chat_widget(&mut guard, &mut chat_widget, _w);
                                    bottom_pane.sync_popups();
                                    // Update footer after view actions (model/permission may change)
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
                                    // Clear the deferred-flush flag for every
                                    // semantic view completion that reaches
                                    // this point. Without this,
                                    // ambient TUI flushes stay suppressed
                                    // for the rest of the session.
                                    pending_deferred_slash_flush = false;
                                } else if pending_deferred_slash_flush {
                                    // The deferred view returned with no
                                    // result name (typically an Esc-cancel
                                    // or any slash that consumed the action
                                    // entirely). Cells committed during the
                                    // deferred window — e.g. background
                                    // permission auto-approval banners
                                    // surfaced by `apply_tui_control_event`
                                    // — must still land in scrollback. The
                                    // original code skipped flush and only
                                    // advanced the watermark, dropping
                                    // those cells silently. `flush_chat_widget`
                                    // both renders pending cells AND
                                    // advances the watermark via
                                    // `drain_new_committed`, so it
                                    // subsumes the bare `mark_all_flushed`.
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    pending_deferred_slash_flush = false;
                                } else if reopen
                                    .as_deref()
                                    .and_then(ReopenTarget::parse)
                                    == Some(ReopenTarget::Agents)
                                {
                                    let _ = reopen_agents_view(
                                        &chat_widget,
                                        &mut bottom_pane,
                                        &frame_requester,
                                    );
                                    // Reopen-Agents path: a deferred slash
                                    // could have set this flag and only
                                    // requested an Agents view reopen on
                                    // close. Clear so ambient flushes
                                    // resume.
                                    pending_deferred_slash_flush = false;
                                } else if let Some(cmd) = reopen {
                                    // Reopen parent menu (e.g., Esc from stats detail → back to /stats menu)
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane,
                                        chat_widget: &mut chat_widget, width: w,
                                    };
                                    let _ = slash_dispatch::dispatch(&cmd, &mut dctx).await;
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    // Generic reopen path: same rationale
                                    // as the Agents branch above.
                                    pending_deferred_slash_flush = false;
                                }
                            }
                            BottomPaneAction::Interrupt | BottomPaneAction::Quit => { break 'main Ok(()); }
                            BottomPaneAction::Consumed => {}
                            BottomPaneAction::Escalate(_) => {}
                            BottomPaneAction::ApprovalResolved { .. } => {
                                // BottomPane already sent the response via its
                                // oneshot. Refresh retained transcript tabs so
                                // the resolved runtime condition disappears
                                // immediately instead of lingering until the
                                // next model event.
                                let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                refresh_open_transcript_view(
                                    &chat_widget,
                                    &mut bottom_pane,
                                    width,
                                );
                            }
                        }
                        frame_requester.schedule_frame();
                    }
                    TuiEvent::Resize => {
                        guard.terminal.invalidate_viewport();
                        {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    refresh_open_transcript_view(
                                        &chat_widget,
                                        &mut bottom_pane,
                                        w,
                                    );
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board)?;
                                }
                    }
                    TuiEvent::Draw => {
                        {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board)?;
                                }
                    }
                    TuiEvent::Paste(text) => {
                        // BottomPane routes short pastes to the textarea
                        // verbatim and folds multi-line pastes behind a
                        // `[Pasted #N · M lines]` placeholder. The
                        // placeholder expands back to the original text
                        // on submit.
                        bottom_pane.handle_paste(&text);
                        frame_requester.schedule_frame();
                    }
                }
            }
            Some(ae) = tui_rx.recv() => {
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                if let Some(new_ev) = chat_widget::translate(
                    ae.clone(),
                    chat_widget::TurnContext::default(),
                ) {
                    chat_widget.handle_event(new_ev);
                    refresh_open_agent_views_for_event(&ae, &chat_widget, &mut bottom_pane);
                }
                if matches!(&ae, TuiAppEvent::AgentLiveGap(_))
                    && server_agent_observer.request_refresh()
                {
                    reconcile_server_agent_observer(
                        &server_agent_observer,
                        &mut server_agent_projection_sequence,
                        &mut chat_widget,
                        &mut bottom_pane,
                        &frame_requester,
                    );
                }
                apply_tui_control_event(&ae, &mut bottom_pane, &mut chat_widget);
                refresh_open_transcript_view(&chat_widget, &mut bottom_pane, w);
                handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                if should_flush_ambient_commits(pending_deferred_slash_flush) {
                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                }
            }
            discovery = &mut external_skill_discovery, if external_skill_discovery_pending => {
                external_skill_discovery_pending = false;
                apply_external_capability_discovery(
                    discovery,
                    &skill_registry,
                    &mut bottom_pane,
                    &mut chat_widget,
                );
                let mcp_extras = {
                    let manager = mcp_manager.read().await;
                    crate::cli::slash::slash_mcp::build_mcp_extra_subcommands(&manager)
                };
                bottom_pane.update_mcp_completions(mcp_extras);
                let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                refresh_open_transcript_view(&chat_widget, &mut bottom_pane, width);
                flush_chat_widget(&mut guard, &mut chat_widget, width);
                frame_requester.schedule_frame();
            }
            _ = &mut tick => {
                if drain_plan_updates_into_workbench(
                    &mut state,
                    &mut chat_widget,
                    &mut bottom_pane,
                    &mut status_indicator,
                    &frame_requester,
                ) {
                    let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
                    refresh_open_transcript_view(&chat_widget, &mut bottom_pane, width);
                }
                drain_agent_workbench_outcomes(
                    &mut agent_workbench_rx,
                    background_registry_session_id.as_deref(),
                    &mut chat_widget,
                    &mut bottom_pane,
                    &frame_requester,
                );
                reconcile_server_agent_observer(
                    &server_agent_observer,
                    &mut server_agent_projection_sequence,
                    &mut chat_widget,
                    &mut bottom_pane,
                    &frame_requester,
                );
                let terminal_size = guard.terminal.size().ok();
                dispatch_projection_actions(
                    &mut background_registry,
                    &server_agent_observer,
                    &mut server_agent_projection_sequence,
                    &task_board,
                    &plan_task_observer,
                    ViewActionBackends {
                        agent_spawner: state.agent_spawner.clone(),
                        delegation_engine: state.delegation_engine.clone(),
                        api: api.clone(),
                        profile: profile.map(str::to_string),
                        session_id: state.session_id.clone(),
                        file_writer: Some(file_writer.clone()),
                        agent_workbench_tx: agent_workbench_tx.clone(),
                    },
                    &restored_local_agent_task_projections,
                    &mut chat_widget,
                    &mut bottom_pane,
                    &frame_requester,
                    terminal_size.map(|size| size.width).unwrap_or(80),
                    terminal_size.map(|size| size.height).unwrap_or(0),
                )
                .await;
                if bottom_pane.pre_draw_tick(std::time::Instant::now()) {
                    frame_requester.schedule_frame();
                }
                // Re-derive permission-mode chip from live state so
                // active mode transitions driven by the agentic loop (e.g.
                // the `exit_plan_mode` overlay handing the next turn back
                // to Auto) reach the status line within one tick
                // instead of waiting for the next turn boundary that
                // happens to call `refresh_footer_from_state`. Cheap:
                // a string format and an Option<u64> compare per 50ms.
                let live_mode_enum = state.perm_manager.mode();
                if bottom_pane.footer.permission_mode != Some(live_mode_enum) {
                    bottom_pane.footer.permission_mode = Some(live_mode_enum);
                    // The manager's active mode just shifted (for example,
                    // after a plan-review transition or a staged picker
                    // selection was activated at turn end). Re-evaluate the
                    // approval queue against that active policy so the chip
                    // and pending count stay truthful.
                    let released = bottom_pane.reevaluate_approvals_for_mode(live_mode_enum);
                    if released > 0 {
                        chat_widget.commit_system(
                            crate::tui::history_cell::system::SystemCell::response(
                                format!(
                                    "  ✓ {released} pending approval(s) resolved by the new mode",
                                ),
                            ),
                        );
                    }
                    frame_requester.schedule_frame();
                }
                // Pulse the chat-widget scrollback so if any async
                // event was handled since the last draw the new
                // cells land promptly instead of waiting for the
                // next event edge.
                surface_tui_file_write_errors(
                    &mut file_write_errors,
                    &mut reported_file_write_errors,
                    &mut state,
                    &mut chat_widget,
                    &frame_requester,
                );
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                if should_flush_ambient_commits(pending_deferred_slash_flush) {
                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                }
                // Poll the task-board observer. No-op most ticks
                // (gated by POLL_INTERVAL); spawns a one-shot fetch
                // when due. Visibility now flows through the pure
                // `board_pin::resolve_board_visibility` state
                // machine so auto-open/hide never fights with the
                // user's explicit Ctrl+T pin.
                if state.session_id != background_registry_session_id {
                    let first_session_binding = is_initial_session_binding(
                        background_registry_session_id.as_deref(),
                        state.session_id.as_deref(),
                    );
                    rebind_workbench_observers(
                        state.session_id.as_deref(),
                        &task_board,
                        &server_agent_observer,
                        &plan_task_observer,
                        &mut board_user_pin,
                    );
                    let reset_agent_scope = should_reset_agent_scope(
                        background_registry_session_id.as_deref(),
                        state.session_id.as_deref(),
                    );
                    if first_session_binding {
                        // The server assigning this conversation's durable id
                        // is not a session switch. Keep live shell handles and
                        // agents intact, scope only future output to the real
                        // id, and force the current projections into the new
                        // workspace. Resetting the registry here used to kill
                        // a just-detached command and make it disappear before
                        // the user's next "status?" turn.
                        background_registry.rebind_output_dir_for_new_tasks(
                            background_task_output_dir(state.session_id.as_deref()),
                        );
                        background_registry_session_id = state.session_id.clone();
                        background_task_projection_cache.clear();
                        background_local_agent_projection_cache.clear();
                        persist_background_task_projections_if_changed(
                            &mut background_registry,
                            background_registry_session_id.as_deref(),
                            state.model.as_deref(),
                            &mut background_task_projection_cache,
                        )
                        .await;
                        restored_local_agent_task_projections =
                            persist_background_local_agent_task_projections_from_snapshot_if_changed(
                                &local_agent_snapshot,
                                &restored_local_agent_task_projections,
                                background_registry_session_id.as_deref(),
                                state.model.as_deref(),
                                &mut background_local_agent_projection_cache,
                            )
                            .await;
                    } else {
                        persist_background_task_projections_if_changed(
                            &mut background_registry,
                            background_registry_session_id.as_deref(),
                            state.model.as_deref(),
                            &mut background_task_projection_cache,
                        )
                        .await;
                        let _ = persist_background_local_agent_task_projections_from_snapshot_if_changed(
                            &local_agent_snapshot,
                            &restored_local_agent_task_projections,
                            background_registry_session_id.as_deref(),
                            state.model.as_deref(),
                            &mut background_local_agent_projection_cache,
                        )
                        .await;
                        if reset_agent_scope
                            && let Some(retired_snapshot) =
                                rebuild_local_agent_runtime_after_session_rebind(
                                    &mut state,
                                    api,
                                    profile,
                                )
                                .await
                        {
                            let _ = persist_background_local_agent_task_projections_from_snapshot_if_changed(
                                &retired_snapshot,
                                &restored_local_agent_task_projections,
                                background_registry_session_id.as_deref(),
                                state.model.as_deref(),
                                &mut background_local_agent_projection_cache,
                            )
                            .await;
                        }
                        background_registry
                            .kill_all_and_wait(BACKGROUND_REGISTRY_SHUTDOWN_WAIT)
                            .await;
                        // Persist the reconciled terminal state to the old
                        // session before replacing the registry. Otherwise a
                        // stopped process is restored forever as `running`.
                        persist_background_task_projections_if_changed(
                            &mut background_registry,
                            background_registry_session_id.as_deref(),
                            state.model.as_deref(),
                            &mut background_task_projection_cache,
                        )
                        .await;
                        background_registry = super::background_tasks::BackgroundTaskRegistry::new(
                            background_task_output_dir(state.session_id.as_deref()),
                        );
                        restored_local_agent_task_projections = restore_background_task_projections(
                            &mut background_registry,
                            state.session_id.as_deref(),
                        )
                        .await;
                        background_task_projection_cache =
                            background_registry.export_shell_task_projections();
                        background_local_agent_projection_cache =
                            restored_local_agent_task_projections.clone();
                        background_registry_session_id = state.session_id.clone();
                    }
                    if first_session_binding
                        && let Some(session_id) = state.session_id.as_deref()
                    {
                        let terminal_size = guard.terminal.size().ok();
                        let width = terminal_size.map(|size| size.width).unwrap_or(80);
                        let terminal_height = terminal_size.map(|size| size.height).unwrap_or(0);
                        if bottom_pane.promote_open_root_transcript_to_durable(
                            session_id.to_string(),
                            width,
                            terminal_height,
                        ) {
                            dispatch_root_transcript_load(
                                session_id.to_string(),
                                bottom_pane::root_transcript_view::RootTranscriptTarget::DurableServer,
                                None,
                                ViewActionBackends {
                                    agent_spawner: state.agent_spawner.clone(),
                                    delegation_engine: state.delegation_engine.clone(),
                                    api: api.clone(),
                                    profile: profile.map(str::to_string),
                                    session_id: state.session_id.clone(),
                                    file_writer: Some(file_writer.clone()),
                                    agent_workbench_tx: agent_workbench_tx.clone(),
                                },
                            );
                        }
                        if let Some(action) =
                            bottom_pane.bind_open_agent_transcript_session(session_id)
                        {
                            dispatch_bottom_pane_view_action(
                                action,
                                &mut background_registry,
                                &server_agent_observer,
                                &mut server_agent_projection_sequence,
                                &task_board,
                                &plan_task_observer,
                                ViewActionBackends {
                                    agent_spawner: state.agent_spawner.clone(),
                                    delegation_engine: state.delegation_engine.clone(),
                                    api: api.clone(),
                                    profile: profile.map(str::to_string),
                                    session_id: state.session_id.clone(),
                                    file_writer: Some(file_writer.clone()),
                                    agent_workbench_tx: agent_workbench_tx.clone(),
                                },
                                &restored_local_agent_task_projections,
                                &mut chat_widget,
                                &mut bottom_pane,
                                &frame_requester,
                                width,
                                terminal_height,
                            )
                            .await;
                        }
                    }
                    if reset_agent_scope {
                        chat_widget.reset_agent_scope();
                    }
                    if let Some(session_id) = state
                        .session_id
                        .as_deref()
                        .filter(|session_id| !session_id.trim().is_empty())
                    {
                        dispatch_local_agent_journal_load(
                            session_id.to_string(),
                            agent_workbench_tx.clone(),
                        );
                    }
                    local_agent_snapshot =
                        super::local_agent_snapshot::LocalAgentSnapshot::capture(
                            state.agent_spawner.as_ref(),
                        )
                        .await;
                    let projection_changed = chat_widget.reconcile_local_agent_snapshot(
                        &local_agent_snapshot,
                        &restored_local_agent_task_projections,
                    );
                    next_local_agent_reconcile =
                        std::time::Instant::now() + LOCAL_AGENT_RECONCILE_INTERVAL;
                    if projection_changed {
                        refresh_open_agent_views(&chat_widget, &mut bottom_pane);
                        frame_requester.schedule_frame();
                    }
                }
                if std::time::Instant::now() >= next_local_agent_reconcile {
                    let next_local_agent_snapshot =
                        super::local_agent_snapshot::LocalAgentSnapshot::capture(
                            state.agent_spawner.as_ref(),
                        )
                        .await;
                    let agent_notifications = next_local_agent_snapshot
                        .attention_notifications_since(&local_agent_snapshot);
                    if !agent_notifications.is_empty() {
                        state.pending_bg_notifications.extend(agent_notifications);
                        runtime_notification_wake_at.get_or_insert_with(|| {
                            std::time::Instant::now() + Duration::from_millis(200)
                        });
                    }
                    local_agent_snapshot = next_local_agent_snapshot;
                    let projection_changed = chat_widget.reconcile_local_agent_snapshot(
                        &local_agent_snapshot,
                        &restored_local_agent_task_projections,
                    );
                    next_local_agent_reconcile =
                        std::time::Instant::now() + LOCAL_AGENT_RECONCILE_INTERVAL;
                    if projection_changed {
                        refresh_open_agent_views(&chat_widget, &mut bottom_pane);
                        frame_requester.schedule_frame();
                    }
                }
                drain_background_task_commands(
                    &state.bg_task_commands,
                    &mut background_registry,
                    state.agent_spawner.as_ref(),
                    &restored_local_agent_task_projections,
                    &state.bg_task_list_cache,
                )
                .await;

                // Poll background shell completions.
                let bg_events = background_registry.poll_completions();
                for ev in &bg_events {
                    if !background_task_event_requires_model_attention(ev) {
                        continue;
                    }
                    let notification = super::background_tasks::format_notification_xml(ev);
                    if !notification.is_empty() {
                        state.pending_bg_notifications.push(notification);
                        runtime_notification_wake_at.get_or_insert_with(|| {
                            std::time::Instant::now() + Duration::from_millis(200)
                        });
                    }
                }
                for msg in background_task_event_system_messages(&bg_events) {
                    chat_widget.commit_system(
                        history_cell::system::SystemCell::info(&msg),
                    );
                    frame_requester.schedule_frame();
                }
                if state.pending_bg_notifications.is_empty() {
                    runtime_notification_wake_at = None;
                }
                if runtime_notification_wake_at
                    .is_some_and(|deadline| std::time::Instant::now() >= deadline)
                    && !state.pending_bg_notifications.is_empty()
                    && bottom_pane.composer.is_empty()
                    && !bottom_pane.has_active_view()
                    && !runtime_notification_turn_pending
                {
                    runtime_notification_wake_at = None;
                    runtime_notification_turn_pending = true;
                    bottom_pane
                        .composer
                        .set_text(RUNTIME_NOTIFICATION_TURN_SENTINEL);
                    event_stream.push_front(TuiEvent::Key(
                        crossterm::event::KeyEvent::new(
                            crossterm::event::KeyCode::Enter,
                            crossterm::event::KeyModifiers::NONE,
                        ),
                    ));
                }
                persist_background_task_projections_if_changed(
                    &mut background_registry,
                    state.session_id.as_deref(),
                    state.model.as_deref(),
                    &mut background_task_projection_cache,
                )
                .await;
                restored_local_agent_task_projections =
                    persist_background_local_agent_task_projections_from_snapshot_if_changed(
                        &local_agent_snapshot,
                        &restored_local_agent_task_projections,
                        state.session_id.as_deref(),
                        state.model.as_deref(),
                        &mut background_local_agent_projection_cache,
                    )
                    .await;
                let rows_for_footer =
                    background_task_rows_with_agent_snapshot(
                        &mut background_registry,
                        &local_agent_snapshot,
                        &restored_local_agent_task_projections,
                    );
                // Refresh shared cache every tick so task_list_bg
                // always sees the latest state without queue latency.
                {
                    let xml = render_background_task_rows_xml(&rows_for_footer);
                    *state.bg_task_list_cache.write().await = xml;
                }
                if bottom_pane.accepts_background_task_rows() {
                    bottom_pane.refresh_background_task_rows(rows_for_footer.clone());
                    frame_requester.schedule_frame();
                }

                // Surface bg task counts on the status line from the
                // same typed rows used by the switcher. That keeps
                // footer and list projection in lock-step as more
                // task kinds land.
                let bg_counts = status_line::BackgroundTaskCounts::from_rows(&rows_for_footer);
                bottom_pane.footer.bg_task_counts = if bg_counts.is_empty() {
                    None
                } else {
                    Some(bg_counts)
                };
                bottom_pane.footer.bg_fanout_summaries =
                    status_line::BackgroundTaskFanoutSummary::from_rows(&rows_for_footer);

                plan_task_observer.set_view_mode(task_board.view_mode());
                plan_task_observer.maybe_refresh();
                let plan_task_projection = plan_task_observer.projection();
                let plan_task_truth = projected_truth_for_plan_task(plan_task_projection.truth_state);
                task_board
                    .set_projected_task_projection(plan_task_projection.tasks, plan_task_truth);
                task_board
                    .set_projected_multi_summaries(plan_task_projection.multi_session);
                task_board.maybe_refresh();
                let projection = task_board.active_projection();
                if bottom_pane.refresh_task_board(&projection) {
                    frame_requester.schedule_frame();
                }
                let (next_expanded, reset_pin) = super::board_pin::resolve_board_visibility(
                    board_expanded,
                    board_user_pin,
                    projection.has_tasks(),
                );
                if reset_pin {
                    board_user_pin = None;
                }
                if next_expanded != board_expanded {
                    board_expanded = next_expanded;
                    frame_requester.schedule_frame();
                }
            }
        }
    };
    if external_skill_discovery_pending {
        external_skill_discovery.abort();
        let _ = external_skill_discovery.await;
    }
    slash_background_read_tasks.abort_all();
    while slash_background_read_tasks.join_next().await.is_some() {}
    // Post-commit projections are recoverable from the canonical journal.
    // Give the ordered worker a short graceful drain on exit, then cancel it
    // rather than trapping terminal teardown behind a slow filesystem/server.
    drop(turn_post_commit_tx);
    if tokio::time::timeout(Duration::from_millis(500), &mut turn_post_commit_worker)
        .await
        .is_err()
    {
        turn_post_commit_worker.abort();
        let _ = turn_post_commit_worker.await;
        tracing::debug!("aborted deferred turn-post-commit worker during TUI shutdown");
    }
    model_catalog_tasks.abort_all();
    while model_catalog_tasks.join_next().await.is_some() {}
    for task in startup_observation_tasks {
        task.abort();
        let _ = task.await;
    }
    // Clean up background shells on exit and persist their reconciled terminal
    // state. A fire-and-forget cancellation request followed by registry drop
    // leaves durable projections falsely marked as running.
    background_registry
        .kill_all_and_wait(BACKGROUND_REGISTRY_SHUTDOWN_WAIT)
        .await;
    persist_background_task_projections_if_changed(
        &mut background_registry,
        state.session_id.as_deref(),
        state.model.as_deref(),
        &mut background_task_projection_cache,
    )
    .await;
    if let Err(error) = file_writer_runtime.shutdown().await {
        tracing::warn!(%error, "TUI file writer did not shut down cleanly");
    }
    surface_tui_file_write_errors(
        &mut file_write_errors,
        &mut reported_file_write_errors,
        &mut state,
        &mut chat_widget,
        &frame_requester,
    );
    let width = guard.terminal.size().map(|size| size.width).unwrap_or(80);
    flush_chat_widget(&mut guard, &mut chat_widget, width);
    drop(guard);
    result
}

/// Handle a TUI app event for BOTTOM-PANE state only.
/// Scrollback mutations are handled independently by
/// `chat_widget::handle_event` via the bridge translator; this
/// function updates the task-status pill, the orbiter-equivalent
/// `StatusIndicator`, and nothing else.
fn handle_app_event(
    ev: &TuiAppEvent,
    bottom_pane: &mut BottomPane,
    status_indicator: &mut status_indicator::StatusIndicator,
    fr: &FrameRequester,
) {
    let now = std::time::Instant::now();
    if matches!(
        ev,
        TuiAppEvent::Token(_)
            | TuiAppEvent::ThinkingStarted
            | TuiAppEvent::ThinkingChunk(_)
            | TuiAppEvent::ThinkingStopped
            | TuiAppEvent::WaitingForModel
            | TuiAppEvent::ModelResponding
            | TuiAppEvent::ToolStarted { .. }
            | TuiAppEvent::ToolCompleted { .. }
            | TuiAppEvent::AgentControlStarted { .. }
            | TuiAppEvent::AgentControlCompleted { .. }
    ) {
        status_indicator.mark_dispatched();
    }
    match ev {
        TuiAppEvent::ContextWindowEstimated(usage) => {
            bottom_pane.footer.begin_context_window_estimate(*usage);
        }
        TuiAppEvent::ContextSystemPromptTokens(tokens) => {
            bottom_pane.footer.set_context_system_prompt_tokens(*tokens);
        }
        TuiAppEvent::ContextWindowMeasured(tokens) => {
            bottom_pane.footer.set_context_window_measured(*tokens);
        }
        TuiAppEvent::Token(text) => {
            // Bump the per-turn token approximation so the
            // StatusIndicator shows `↓ N tokens` climbing.
            status_indicator.bump_stream_chars(text.chars().count());
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: now });
            // A provider may stream its first token without a distinct
            // ModelResponding event. Promote Dispatching directly so the
            // activity never regresses to `Working · Sending`.
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ThinkingStarted => {
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ThinkingChunk(_) => {
            // ChatWidget handles the cell update; nothing to do
            // in the bottom pane. The indicator stays `Thinking`.
        }
        TuiAppEvent::ThinkingStopped => {
            // Keep the indicator active — the model may still be
            // generating the answer body. It flips to `Idle` on
            // TurnComplete / TurnError.
        }
        TuiAppEvent::WaitingForModel => {
            bottom_pane.set_task_status(TaskStatus::WaitingModel);
            status_indicator
                .set_state(status_indicator::IndicatorState::WaitingModel { started_at: now });
        }
        TuiAppEvent::ModelResponding => {
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: now });
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ToolStarted { name, .. } => {
            bottom_pane.set_task_status(TaskStatus::ToolExecuting {
                name: name.clone(),
                started_at: now,
            });
            status_indicator.set_state(status_indicator::IndicatorState::Tool {
                name: name.clone(),
                started_at: now,
            });
        }
        TuiAppEvent::AgentControlStarted { label, .. } => {
            bottom_pane.set_task_status(TaskStatus::ToolExecuting {
                name: label.clone(),
                started_at: now,
            });
            status_indicator.set_state(status_indicator::IndicatorState::Tool {
                name: label.clone(),
                started_at: now,
            });
        }
        TuiAppEvent::ToolCompleted { .. } => {
            // Flip back to thinking; the ChatWidget committed the
            // tool cell in its own event handler.
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::AgentControlCompleted { .. } => {
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ToolOutput { .. } => {
            // Progress ticks handled by ChatWidget via the bridge.
        }
        TuiAppEvent::AssistantOutputSettled
        | TuiAppEvent::TurnStreamClosed
        | TuiAppEvent::AgentLive(_)
        | TuiAppEvent::AgentLiveBatch(_)
        | TuiAppEvent::AgentLiveGap(_)
        | TuiAppEvent::AgentCommunication(_)
        | TuiAppEvent::StatusLine(_)
        | TuiAppEvent::UserIntentApplied { .. }
        | TuiAppEvent::Compaction(_)
        | TuiAppEvent::ExplainReport(_)
        | TuiAppEvent::VerdictReport(_)
        | TuiAppEvent::SystemWarning(_)
        | TuiAppEvent::SystemInfo(_)
        | TuiAppEvent::PermissionAutoApproved { .. } => {}
        TuiAppEvent::TurnComplete | TuiAppEvent::TurnError(_) => {
            bottom_pane.set_task_status(TaskStatus::Idle);
            status_indicator.set_state(status_indicator::IndicatorState::Idle);
        }
    }
    fr.schedule_frame();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::background_task_error::BackgroundTaskError;
    use crate::cli::turn::local_run_control::LocalRunControl;
    use crate::tui::background_tasks::BgTaskEvent;
    use crate::tui::status_line::{StatusContext, StatusLine};
    use astra_runtime::turn::run_control::{
        RunControlStatus, RunStatusProvider, UserIntentProvider,
    };
    use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
    use astra_turn_core::orchestration_types::{
        AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics,
    };
    use std::path::PathBuf;

    fn render_bottom_pane_text(bottom_pane: &BottomPane, width: u16, height: u16) -> String {
        let area = ratatui::layout::Rect::new(0, 0, width, height);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        bottom_pane.render(area, &mut buffer);
        crate::tui::testing::render::buffer_to_string(&buffer)
    }

    #[test]
    fn agent_control_projects_typed_session_continuation() {
        let result = project_agent_control_execution(&serde_json::json!({
            "disposition": "session_continuation_required",
            "continuation": {
                "strategy": "session_continuation",
                "session_id": "session-1",
                "source_run_id": "child-run"
            }
        }))
        .expect("typed continuation");
        assert!(matches!(
            result,
            AgentControlExecution::SessionContinuationRequired {
                session_id,
                source_run_id
            } if session_id == "session-1" && source_run_id == "child-run"
        ));
    }

    #[test]
    fn agent_control_rejects_incomplete_continuation_contract() {
        let error = project_agent_control_execution(&serde_json::json!({
            "disposition": "session_continuation_required",
            "continuation": {"session_id": "session-1"}
        }))
        .err()
        .expect("missing source run must be explicit");
        assert_eq!(error, "server omitted continuation source_run_id");
    }

    #[test]
    fn settled_visible_reply_releases_active_chrome_once() {
        let now = std::time::Instant::now();
        let mut output_settled_at = None;
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let mut indicator = status_indicator::StatusIndicator::new();
        indicator.begin_turn(now);
        indicator.set_state(status_indicator::IndicatorState::Thinking { started_at: now });

        assert!(settle_visible_reply(
            &mut output_settled_at,
            &mut chat_widget,
            &mut bottom_pane,
            &mut indicator,
            now,
        ));
        assert_eq!(output_settled_at, Some(now));
        assert!(
            indicator.render_at(now).is_none(),
            "a completed reply must not retain a working spinner while local settlement continues"
        );
        assert!(
            !settle_visible_reply(
                &mut output_settled_at,
                &mut chat_widget,
                &mut bottom_pane,
                &mut indicator,
                now + std::time::Duration::from_secs(1),
            ),
            "duplicate stream-close notifications must not reset the completion boundary"
        );
        assert_eq!(output_settled_at, Some(now));
    }

    #[test]
    fn normal_settlement_replays_unapplied_and_post_output_input_in_fifo_order() {
        let mut queued = VecDeque::from(["already queued".to_string()]);
        let mut post_output = VecDeque::from(["submitted after output settled".to_string()]);

        let restored = settle_followup_submissions(
            &mut queued,
            ["missed the last model boundary".to_string()],
            &mut post_output,
            true,
        );

        assert!(restored.is_none());
        assert_eq!(
            queued.into_iter().collect::<Vec<_>>(),
            vec![
                "already queued",
                "missed the last model boundary",
                "submitted after output settled",
            ],
        );
        assert!(post_output.is_empty());
    }

    #[test]
    fn failed_settlement_restores_every_queued_submission_without_replaying() {
        let mut queued = VecDeque::from(["earlier follow-up".to_string()]);
        let mut post_output = VecDeque::from(["post-output input".to_string()]);

        let restored = settle_followup_submissions(
            &mut queued,
            ["unapplied guidance".to_string()],
            &mut post_output,
            false,
        );

        assert_eq!(
            restored.as_deref(),
            Some("earlier follow-up\n\nunapplied guidance\n\npost-output input"),
        );
        assert!(queued.is_empty());
        assert!(post_output.is_empty());
    }

    #[test]
    fn active_turn_permission_selection_is_staged_without_rewriting_current_policy() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state
            .perm_manager
            .set_mode(crate::cli::permission_manager::PermissionMode::Prompt);
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = chat_widget::ChatWidget::new("");

        stage_permission_mode_for_next_turn(
            &mut bottom_pane,
            &mut chat_widget,
            crate::cli::permission_manager::PermissionMode::Auto,
        );

        assert_eq!(
            state.perm_manager.mode(),
            crate::cli::permission_manager::PermissionMode::Prompt,
            "an active turn must retain the policy it was assembled with"
        );
        assert_eq!(
            bottom_pane.take_staged_permission_mode(),
            Some(crate::cli::permission_manager::PermissionMode::Auto)
        );
    }

    #[test]
    fn staged_permission_selection_becomes_active_only_after_turn_settles() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = chat_widget::ChatWidget::new("");
        bottom_pane.stage_permission_mode_for_next_turn(
            crate::cli::permission_manager::PermissionMode::Auto,
        );

        let mode = bottom_pane
            .take_staged_permission_mode()
            .expect("a chosen policy must survive until turn end");
        slash_dispatch::apply_permission_mode_selection(
            &mut state,
            &mut bottom_pane,
            &mut chat_widget,
            mode,
        );

        assert_eq!(
            state.perm_manager.mode(),
            crate::cli::permission_manager::PermissionMode::Auto
        );
        assert!(bottom_pane.take_staged_permission_mode().is_none());
    }

    fn root_transcript_item(
        item_seq: i64,
        role: &str,
        content: &str,
    ) -> astra_thin_client::SessionTranscriptItem {
        astra_thin_client::SessionTranscriptItem {
            session_id: "session-1".into(),
            item_seq,
            run_id: Some("root".into()),
            role: role.into(),
            content: content.into(),
            reasoning_status: None,
            reasoning: None,
            tool_calls: Vec::new(),
            tool_result: None,
            evidence: None,
            source_event_id: Some(format!("test:{item_seq}")),
            created_at: "2026-07-12T00:00:00Z".into(),
        }
    }

    fn agent_transcript_item(
        item_seq: i64,
        role: &str,
        content: &str,
    ) -> astra_thin_client::SessionTranscriptItem {
        let mut item = root_transcript_item(item_seq, role, content);
        item.run_id = Some("run-review".into());
        item
    }

    #[test]
    fn plan_updates_stay_in_the_workbench_and_reach_a_terminal_state() {
        use crate::cli::plan::plan_executor::{self, PlanUpdate};

        let mut state = crate::cli::session::session_state::SessionState::default();
        let (handle, updates, _commands) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);
        updates
            .send(PlanUpdate::SubtaskStarted {
                id: "step-1".to_string(),
                title: "Inspect the worktree".to_string(),
                index: 1,
                total: 1,
            })
            .expect("plan update channel should be open");
        updates
            .send(PlanUpdate::PlanFinished {
                pct: 100,
                elapsed: std::time::Duration::from_secs(1),
                outcome: astra_services::task_orchestrator::TaskOutcome::Success,
            })
            .expect("plan update channel should be open");
        drop(updates);

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let mut status_indicator = status_indicator::StatusIndicator::new();

        assert!(drain_plan_updates_into_workbench(
            &mut state,
            &mut chat_widget,
            &mut bottom_pane,
            &mut status_indicator,
            &FrameRequester::test_dummy(),
        ));
        assert!(state.plan_handle.is_none());
        assert!(state.executing_plan.is_none());
        assert!(!bottom_pane.footer.is_turn_active);
        assert_eq!(chat_widget.history().len(), 2);
    }

    #[test]
    fn closed_plan_executor_without_terminal_update_is_not_left_running() {
        use crate::cli::plan::plan_executor;

        let mut state = crate::cli::session::session_state::SessionState::default();
        state.executing_plan = Some(astra_services::task_orchestrator::TaskPlan::default());
        let (handle, updates, _commands) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);
        drop(updates);

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let mut status_indicator = status_indicator::StatusIndicator::new();

        assert!(drain_plan_updates_into_workbench(
            &mut state,
            &mut chat_widget,
            &mut bottom_pane,
            &mut status_indicator,
            &FrameRequester::test_dummy(),
        ));
        assert!(state.plan_handle.is_none());
        assert!(state.executing_plan.is_none());
        assert!(!state.plan_execution_paused);
        assert!(state.plan_execution_last_error.is_some());
        assert!(!bottom_pane.footer.is_turn_active);
    }

    #[test]
    fn paused_plan_executor_remains_resumable_after_its_channel_closes() {
        use crate::cli::plan::plan_executor::{self, PlanUpdate};

        let mut state = crate::cli::session::session_state::SessionState::default();
        state.executing_plan = Some(astra_services::task_orchestrator::TaskPlan::default());
        let (handle, updates, _commands) = plan_executor::create_plan_channels();
        state.plan_handle = Some(handle);
        updates
            .send(PlanUpdate::PlanPaused {
                pct: 40,
                remaining: 2,
                elapsed: std::time::Duration::from_secs(3),
                blocked_ids: Vec::new(),
            })
            .expect("plan update channel should be open");
        drop(updates);

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let mut status_indicator = status_indicator::StatusIndicator::new();

        assert!(drain_plan_updates_into_workbench(
            &mut state,
            &mut chat_widget,
            &mut bottom_pane,
            &mut status_indicator,
            &FrameRequester::test_dummy(),
        ));
        assert!(state.plan_handle.is_none());
        assert!(state.executing_plan.is_some());
        assert!(state.plan_execution_paused);
        assert!(state.plan_execution_last_error.is_none());
    }

    fn rendered_transcript_overlay(chat_widget: &chat_widget::ChatWidget, width: u16) -> String {
        let mut bottom_pane = BottomPane::new();
        let terminal_height = 120;
        toggle_local_root_transcript_fallback(
            chat_widget,
            &mut bottom_pane,
            width,
            terminal_height,
        );
        render_bottom_pane_text(&bottom_pane, width, terminal_height)
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn local_agent_transcript_projection_is_run_scoped_and_paginates_canonical_messages() {
        astra_services::session_journal::set_journal_content_redact_override(Some(false));
        let event = |run_id: &str, seq: u64, message: serde_json::Value| {
            astra_services::session_journal::JournalEvent::transcript_item(
                "parent-session",
                run_id,
                "reviewer",
                seq,
                &message,
            )
            .unwrap()
        };
        let events = vec![
            event(
                "run-review",
                1,
                serde_json::json!({"role": "user", "content": "Review the scheduler"}),
            ),
            event(
                "other-run",
                1,
                serde_json::json!({"role": "assistant", "content": "unrelated"}),
            ),
            event(
                "run-review",
                2,
                serde_json::json!({
                    "role": "assistant",
                    "content": null,
                    "reasoning_content": "inspect ownership",
                    "tool_calls": [{
                        "function": {"name": "read", "arguments": "{\"path\":\"src/lib.rs\"}"}
                    }]
                }),
            ),
            event(
                "run-review",
                3,
                serde_json::json!({"role": "tool", "content": "scheduler source"}),
            ),
        ];

        let recent = project_local_agent_transcript_page(
            "parent-session",
            "run-review",
            events.clone(),
            None,
            2,
        );
        assert_eq!(
            recent
                .items
                .iter()
                .map(|item| item.item_seq)
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
        assert!(recent.has_more);
        assert_eq!(recent.next_before_seq, Some(2));
        assert!(recent.items[0].content.is_empty());
        assert_eq!(recent.items[0].tool_calls.len(), 1);
        assert_eq!(recent.items[0].tool_calls[0].name, "read");
        assert_eq!(recent.items[0].tool_calls[0].tool_use_id, "");
        assert_eq!(
            recent.items[0].tool_calls[0].arguments,
            "{\"path\":\"src/lib.rs\"}"
        );
        assert_eq!(
            recent.items[0].reasoning.as_deref(),
            Some("inspect ownership")
        );

        let older = project_local_agent_transcript_page(
            "parent-session",
            "run-review",
            events,
            recent.next_before_seq,
            2,
        );
        assert_eq!(older.items.len(), 1);
        assert_eq!(older.items[0].content, "Review the scheduler");
        assert!(!older.has_more);
        astra_services::session_journal::set_journal_content_redact_override(None);
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn local_agent_transcript_projection_deduplicates_by_run_sequence() {
        astra_services::session_journal::set_journal_content_redact_override(Some(false));
        let first = astra_services::session_journal::JournalEvent::transcript_item(
            "parent-session",
            "run-review",
            "reviewer",
            7,
            &serde_json::json!({"role": "assistant", "content": "first durable answer"}),
        )
        .expect("valid transcript message");
        let retry_duplicate = astra_services::session_journal::JournalEvent::transcript_item(
            "parent-session",
            "run-review",
            "reviewer",
            7,
            &serde_json::json!({"role": "assistant", "content": "must not replace first item"}),
        )
        .expect("valid transcript message");

        let page = project_local_agent_transcript_page(
            "parent-session",
            "run-review",
            vec![first, retry_duplicate],
            None,
            20,
        );
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].item_seq, 7);
        assert_eq!(page.items[0].content, "first durable answer");
        astra_services::session_journal::set_journal_content_redact_override(None);
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn local_journal_root_transcript_is_typed_and_paginates_by_durable_identity() {
        astra_services::session_journal::set_journal_content_redact_override(Some(false));
        let event = |item_seq, message| {
            astra_services::session_journal::JournalEvent::transcript_item(
                "session-1",
                "root-run-1",
                "root",
                item_seq,
                &message,
            )
            .expect("valid canonical root transcript item")
        };
        let first = event(
            1,
            serde_json::json!({"role": "user", "content": "review this branch"}),
        );
        let second = event(
            2,
            serde_json::json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": "inspect the execution boundary",
            }),
        );
        let third = event(
            3,
            serde_json::json!({
                "role": "tool",
                "name": "git",
                "tool_call_id": "call-1",
                "status": "uncertain",
                "duration_ms": 48,
                "content": "+21 -18 in 1 file(s)",
            }),
        );
        let fourth = event(
            4,
            serde_json::json!({
                "role": "assistant",
                "content": "The branch needs one follow-up.",
            }),
        );
        let retry = event(
            4,
            serde_json::json!({
                "role": "assistant",
                "content": "must not replace the first durable item",
            }),
        );
        let child = astra_services::session_journal::JournalEvent::transcript_item(
            "session-1",
            "child-run-1",
            "reviewer",
            1,
            &serde_json::json!({"role": "assistant", "content": "child-only"}),
        )
        .expect("valid child transcript item");
        let events = vec![first, second, third, fourth, retry, child];

        let recent = project_local_root_transcript_page("session-1", events.clone(), None, 2);
        assert_eq!(
            recent
                .items
                .iter()
                .map(|item| item.item_seq)
                .collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(recent.has_more);
        assert_eq!(recent.next_before_seq, Some(3));
        let tool = &recent.items[0];
        assert_eq!(tool.role, "tool");
        assert_eq!(tool.content, "+21 -18 in 1 file(s)");
        assert_eq!(
            tool.tool_result
                .as_ref()
                .and_then(|result| result.status.as_deref()),
            Some("uncertain")
        );
        assert_eq!(recent.items[1].content, "The branch needs one follow-up.");

        let older =
            project_local_root_transcript_page("session-1", events, recent.next_before_seq, 2);
        assert_eq!(
            older
                .items
                .iter()
                .map(|item| item.item_seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(older.items[0].role, "user");
        assert_eq!(older.items[0].content, "review this branch");
        assert_eq!(
            older.items[1].reasoning.as_deref(),
            Some("inspect the execution boundary")
        );
        assert!(!older.has_more);
        astra_services::session_journal::set_journal_content_redact_override(None);
    }

    #[test]
    fn empty_initial_server_root_page_yields_visible_local_history_without_changing_pagination() {
        let durable_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: Vec::new(),
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };
        let local_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![astra_thin_client::SessionTranscriptItem {
                session_id: "session-1".into(),
                item_seq: 7,
                run_id: Some("root-run".into()),
                role: "assistant".into(),
                content: "locally durable answer".into(),
                reasoning_status: None,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_result: None,
                evidence: None,
                source_event_id: Some("tui:7".into()),
                created_at: "2026-07-12T00:00:00Z".into(),
            }],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };

        let (initial, source) =
            select_root_transcript_page(None, durable_page.clone(), Some(local_page.clone()));
        assert_eq!(
            source,
            bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableWhileServerCatchesUp
        );
        assert_eq!(initial.items, local_page.items);

        let (older, source) = select_root_transcript_page(Some(7), durable_page, Some(local_page));
        assert_eq!(
            source,
            bottom_pane::root_transcript_view::RootTranscriptSource::DurableServer
        );
        assert!(older.items.is_empty());
    }

    #[test]
    fn partial_server_root_page_yields_broader_local_conversation_history() {
        let durable_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![root_transcript_item(
                99,
                "assistant",
                "newest replicated answer",
            )],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };
        let local_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![
                root_transcript_item(1, "user", "first request"),
                root_transcript_item(2, "assistant", "first answer"),
                root_transcript_item(3, "user", "follow-up"),
                root_transcript_item(4, "assistant", "follow-up answer"),
            ],
            page_refs: Vec::new(),
            next_before_seq: Some(1),
            has_more: true,
        };

        let update = initial_root_transcript_update(
            "session-1".into(),
            durable_page,
            Some(local_page.clone()),
        );
        let bottom_pane::root_transcript_view::RootTranscriptUpdate::Loaded {
            session_id,
            page: selected,
            replace,
            source,
        } = update
        else {
            panic!("a valid initial server page must yield a loaded root transcript");
        };

        assert_eq!(session_id, "session-1");
        assert!(replace);
        assert_eq!(
            source,
            bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableWithBroaderHistory
        );
        assert_eq!(selected.items, local_page.items);
        assert_eq!(selected.next_before_seq, Some(1));
    }

    #[test]
    fn server_root_page_wins_when_it_covers_at_least_as_much_conversation() {
        let durable_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![
                root_transcript_item(40, "user", "server request"),
                root_transcript_item(41, "assistant", "server answer"),
            ],
            page_refs: Vec::new(),
            next_before_seq: Some(40),
            has_more: true,
        };
        let local_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![root_transcript_item(1, "user", "local request")],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };

        let (selected, source) =
            select_root_transcript_page(None, durable_page.clone(), Some(local_page));

        assert_eq!(
            source,
            bottom_pane::root_transcript_view::RootTranscriptSource::DurableServer
        );
        assert_eq!(selected.items, durable_page.items);
    }

    #[test]
    fn richer_local_root_page_wins_when_server_drops_expandable_details() {
        let thin = root_transcript_item(40, "assistant", "same answer");
        let mut rich = root_transcript_item(1, "assistant", "same answer");
        rich.reasoning = Some("inspect ownership".into());
        rich.reasoning_status = Some("done".into());
        rich.tool_calls = vec![astra_thin_client::SessionTranscriptToolCall {
            tool_use_id: "call-1".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"src/lib.rs\"}".into(),
        }];
        let durable = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![thin],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };
        let local = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![rich],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };

        let (selected, source) = select_root_transcript_page(None, durable, Some(local.clone()));
        assert_eq!(
            source,
            bottom_pane::root_transcript_view::RootTranscriptSource::LocalDurableWithBroaderHistory
        );
        assert_eq!(selected.items, local.items);
    }

    #[test]
    fn empty_initial_server_agent_page_yields_exact_local_run_history_without_cross_pagination() {
        let durable_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: Vec::new(),
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };
        let local_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![astra_thin_client::SessionTranscriptItem {
                session_id: "session-1".into(),
                item_seq: 7,
                run_id: Some("run-review".into()),
                role: "assistant".into(),
                content: "local edge finding".into(),
                reasoning_status: None,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_result: None,
                evidence: None,
                source_event_id: Some("journal:run-review:7".into()),
                created_at: "2026-07-12T00:00:00Z".into(),
            }],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };

        let (initial, source) =
            select_agent_transcript_page(None, durable_page.clone(), Some(local_page.clone()));
        assert_eq!(
            source,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWhileServerCatchesUp
        );
        assert_eq!(initial.items, local_page.items);

        let (older, source) = select_agent_transcript_page(Some(7), durable_page, Some(local_page));
        assert_eq!(
            source,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::DurableServer
        );
        assert!(older.items.is_empty());
    }

    #[test]
    fn partial_server_agent_page_yields_broader_local_run_history() {
        let durable_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![agent_transcript_item(
                99,
                "assistant",
                "latest server suffix",
            )],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };
        let local_page = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![
                agent_transcript_item(1, "assistant", "initial finding"),
                agent_transcript_item(2, "tool", "inspection output"),
                agent_transcript_item(3, "assistant", "review conclusion"),
            ],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };

        let update = initial_agent_transcript_update(
            "reviewer".into(),
            "run-review".into(),
            durable_page,
            Some(local_page.clone()),
        );
        let bottom_pane::agent_transcript_view::AgentTranscriptUpdate::Loaded {
            agent_id,
            run_id,
            page,
            replace,
            source,
        } = update
        else {
            panic!("a valid initial agent page must yield a loaded transcript");
        };

        assert_eq!(agent_id, "reviewer");
        assert_eq!(run_id, "run-review");
        assert!(replace);
        assert_eq!(
            source,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWithBroaderHistory
        );
        assert_eq!(page.items, local_page.items);
    }

    #[test]
    fn richer_local_agent_prefix_wins_over_equally_long_thin_server_page() {
        let thin = agent_transcript_item(99, "assistant", "same finding");
        let mut rich = agent_transcript_item(1, "assistant", "same finding");
        rich.reasoning = Some("earlier live reasoning".into());
        rich.reasoning_status = Some("done".into());
        let durable = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![thin],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };
        let local = astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![rich],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        };

        let (selected, source) = select_agent_transcript_page(None, durable, Some(local.clone()));
        assert_eq!(
            source,
            bottom_pane::agent_transcript_view::AgentTranscriptSource::LocalJournalWithBroaderHistory
        );
        assert_eq!(selected.items, local.items);
    }

    #[test]
    #[serial_test::serial(astra_journal_content_redact_env)]
    fn local_agent_transcript_projects_typed_communication_evidence() {
        astra_services::session_journal::set_journal_content_redact_override(Some(false));
        let communication = astra_turn_types::AgentCommunicationEvent {
            schema_version: astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION.into(),
            observed_by: astra_turn_types::AgentCommunicationParty {
                run_id: "run-review".into(),
                agent_id: "reviewer".into(),
            },
            direction: astra_turn_types::AgentCommunicationDirection::Received,
            message_id: "message-1".into(),
            from: astra_turn_types::AgentCommunicationParty {
                run_id: "run-code".into(),
                agent_id: "coder".into(),
            },
            to: astra_turn_types::AgentCommunicationTarget::Direct {
                address: astra_turn_types::AgentCommunicationParty {
                    run_id: "run-review".into(),
                    agent_id: "reviewer".into(),
                },
            },
            payload_kind: "review".into(),
            summary: Some("lock ownership is unsafe".into()),
            response_accepted: None,
            related_message_id: None,
            timestamp_ms: 42,
            correlation_id: None,
            requires_ack: false,
        };
        let event = astra_services::session_journal::JournalEvent::transcript_evidence(
            "parent-session",
            "run-review",
            "reviewer",
            3,
            &astra_turn_types::AgentTranscriptEvidence::AgentCommunication {
                event: communication,
            },
        )
        .expect("typed evidence is a valid transcript item");

        let page = project_local_agent_transcript_page(
            "parent-session",
            "run-review",
            vec![event],
            None,
            20,
        );
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].role, "event");
        assert!(matches!(
            page.items[0].evidence.as_ref(),
            Some(astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event })
                if event.message_id == "message-1" && event.summary.as_deref() == Some("lock ownership is unsafe")
        ));
        astra_services::session_journal::set_journal_content_redact_override(None);
    }

    #[test]
    fn agent_scope_resets_only_after_a_real_session_rebind() {
        assert!(!should_reset_agent_scope(None, Some("session-a")));
        assert!(!should_reset_agent_scope(Some(""), Some("session-a")));
        assert!(!should_reset_agent_scope(
            Some("session-a"),
            Some("session-a")
        ));
        assert!(should_reset_agent_scope(
            Some("session-a"),
            Some("session-b")
        ));
        assert!(should_reset_agent_scope(Some("session-a"), None));
    }

    #[test]
    fn initial_session_binding_is_identity_discovery_not_a_session_switch() {
        assert!(is_initial_session_binding(None, Some("session-a")));
        assert!(is_initial_session_binding(Some(""), Some("session-a")));
        assert!(!is_initial_session_binding(None, None));
        assert!(!is_initial_session_binding(
            Some("session-a"),
            Some("session-a")
        ));
        assert!(!is_initial_session_binding(
            Some("session-a"),
            Some("session-b")
        ));
    }

    #[tokio::test]
    async fn session_rebind_retires_active_local_agents_with_explicit_terminal_state() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let output = spawner
            .spawn(
                SpawnAgentInput {
                    description: "long review".into(),
                    prompt: "long review".into(),
                    agent_type: "task".into(),
                    run_in_background: true,
                    ..Default::default()
                },
                &test_spawn_context(),
            )
            .await
            .expect("background agent launch");
        let agent_id = match output {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched agent, got {other:?}"),
        };

        assert_eq!(retire_local_agent_spawner(spawner.clone()).await, 1);

        let state = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("retired agent remains inspectable");
        assert!(matches!(
            state.status,
            AgentStatus::Cancelled {
                by_user: true,
                ref reason,
            } if reason == LOCAL_AGENT_SESSION_REBIND_REASON
        ));
        assert!(state.ended_at.is_some());
        assert_eq!(spawner.background_task_count(), 0);
    }

    #[test]
    fn runtime_reconciliation_refreshes_an_open_agent_detail_view() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let agent_id = "agent-open-detail";
        let mut widget = chat_widget::ChatWidget::new("");
        widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: agent_id.into(),
                kind: AgentLiveEventKind::OutputDelta("initial output".into()),
            }),
        ));
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(
            bottom_pane::task_detail_view::TaskDetailView::from_task_cell(
                widget.agent_run_cell(agent_id).expect("live agent detail"),
            )
            .with_live_task_id(agent_id),
        ));

        let snapshot = crate::tui::local_agent_snapshot::LocalAgentSnapshot {
            available: true,
            agents: vec![agent_info(
                agent_id,
                AgentStatus::Completed {
                    result: "authoritative final result".into(),
                    finish_reason: Some("normal".into()),
                },
                false,
            )],
            fanout_groups: Vec::new(),
        };
        assert!(widget.reconcile_local_agent_snapshot(&snapshot, &[]));
        assert!(refresh_open_agent_views(&widget, &mut bottom_pane));

        let rendered = render_bottom_pane_text(&bottom_pane, 100, 12);
        assert!(
            rendered.contains("authoritative final result"),
            "{rendered}"
        );
    }

    #[test]
    fn agent_action_opens_monitor_from_live_projection() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut widget = chat_widget::ChatWidget::new("");
        widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-visible".into(),
                kind: AgentLiveEventKind::OutputDelta("reviewing changes".into()),
            }),
        ));
        let mut bottom_pane = BottomPane::new();

        assert!(open_agents_view(&widget, &mut bottom_pane));
        assert!(bottom_pane.agent_monitor_is_open());
    }

    #[test]
    fn agent_monitor_shortcut_reports_confirmed_empty_without_stealing_modal_focus() {
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('g'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let frame_requester = FrameRequester::test_dummy();
        let mut widget = chat_widget::ChatWidget::new("");
        let mut bottom_pane = BottomPane::new();

        assert!(handle_agent_monitor_shortcut(
            &key,
            &mut widget,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert!(!bottom_pane.has_active_view());
        let rendered = rendered_transcript_overlay(&widget, 100);
        assert!(rendered.contains("No agent runs yet"), "{rendered}");

        bottom_pane.push_view(Box::new(
            crate::tui::bottom_pane::info_view::InfoView::from_plain(
                "Permission review",
                vec!["Keep focus here".to_string()],
            ),
        ));
        let history_len = widget.history().len();
        assert!(!handle_agent_monitor_shortcut(
            &key,
            &mut widget,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert_eq!(widget.history().len(), history_len);
    }

    #[test]
    fn conversation_tab_shortcut_switches_full_transcript_workspaces() {
        use crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptView;
        use crate::tui::bottom_pane::transcript_view::{TranscriptSnapshot, TranscriptView};
        use crate::tui::bottom_pane::view::ConversationTabId;

        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(TranscriptView::from_snapshot(
            TranscriptSnapshot::default(),
            24,
            80,
        )));
        bottom_pane.push_view(Box::new(AgentTranscriptView::live_unbound(
            "agent-reviewer".into(),
            "Reviewer".into(),
            "run-reviewer".into(),
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
            "agents",
            80,
            24,
        )));

        let next = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert!(handle_conversation_tab_shortcut(
            &next,
            &mut bottom_pane,
            &FrameRequester::test_dummy(),
        ));
        assert!(matches!(
            bottom_pane.active_conversation_tab_id(),
            Some(ConversationTabId::Root)
        ));

        let previous = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::SHIFT,
        );
        assert!(handle_conversation_tab_shortcut(
            &previous,
            &mut bottom_pane,
            &FrameRequester::test_dummy(),
        ));
        assert!(matches!(
            bottom_pane.active_conversation_tab_id(),
            Some(ConversationTabId::Run { agent_id, run_id })
                if agent_id == "agent-reviewer" && run_id == "run-reviewer"
        ));

        let reserved_by_terminal = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(
            !handle_conversation_tab_shortcut(
                &reserved_by_terminal,
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            ),
            "Ctrl+Tab must remain available to the terminal or operating system"
        );
    }

    #[test]
    fn agent_monitor_shortcut_opens_over_a_conversation_tab_and_returns_to_it() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('g'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let frame_requester = FrameRequester::test_dummy();
        let mut widget = chat_widget::ChatWidget::new("");
        widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "run-reviewer".into(),
                agent_id: "agent-reviewer".into(),
                kind: AgentLiveEventKind::OutputDelta("reviewing".into()),
            }),
        ));
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(
            bottom_pane::transcript_view::TranscriptView::from_snapshot(
                bottom_pane::transcript_view::TranscriptSnapshot::default(),
                24,
                80,
            ),
        ));

        assert!(handle_agent_monitor_shortcut(
            &key,
            &mut widget,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert!(bottom_pane.agent_monitor_is_open());
        assert!(bottom_pane.close_active_view());
        assert!(bottom_pane.conversation_tab_is_open());
    }

    async fn wait_for_background_shell_terminal(
        registry: &mut crate::tui::background_tasks::BackgroundTaskRegistry,
        id: &str,
    ) {
        crate::tests::wait_until(
            std::time::Duration::from_secs(3),
            std::time::Duration::from_millis(25),
            || {
                registry.drain_join_set();
                registry
                    .get(id)
                    .map(|handle| {
                        matches!(handle.projected_status(), "completed" | "failed" | "killed")
                    })
                    .unwrap_or(false)
            },
        )
        .await
        .unwrap_or_else(|()| {
            let status = registry
                .get(id)
                .map(|handle| handle.projected_status())
                .unwrap_or("missing");
            panic!("background shell {id} did not terminate; current status: {status}");
        });
    }

    async fn wait_for_background_shell_preview(
        registry: &mut crate::tui::background_tasks::BackgroundTaskRegistry,
        id: &str,
    ) {
        crate::tests::wait_until(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(10),
            || {
                let _ = registry.poll_completions();
                background_task_rows(registry)
                    .into_iter()
                    .find(|row| row.id == id)
                    .is_some_and(|row| row.output_tail.is_some())
            },
        )
        .await
        .expect("background shell preview should converge asynchronously");
    }

    #[test]
    fn context_window_trace_preserves_request_provenance() {
        let mut trace = ContextAssemblyTrace::default();
        trace.token_budget.total_used = 20_687;
        trace.token_budget.max_tokens = 800_000;
        trace.token_budget.usage_source =
            astra_turn_types::ContextWindowUsageSource::ProviderReported;

        let usage = context_window_from_trace(&trace);
        assert_eq!(
            usage,
            Some(astra_turn_types::ContextWindowUsage::provider_reported(
                20_687, 800_000
            ))
        );

        let plain = StatusLine::from_context(&StatusContext {
            context_window: usage,
            ..StatusContext::default()
        })
        .plain();
        assert!(
            plain.contains("Ctx 3%") && plain.contains("21k/800k"),
            "footer should present one request's context occupancy: {plain:?}"
        );
    }

    #[test]
    fn context_window_trace_is_absent_when_context_limit_is_unknown() {
        let mut trace = ContextAssemblyTrace::default();
        trace.token_budget.total_used = 20_687;
        trace.token_budget.max_tokens = 0;

        assert_eq!(context_window_from_trace(&trace), None);
    }

    fn agent_info(
        agent_id: &str,
        status: AgentStatus,
        run_in_background: bool,
    ) -> SpawnedAgentInfo {
        let ended_at = status.is_terminal().then(std::time::SystemTime::now);
        SpawnedAgentInfo {
            agent_id: agent_id.to_string(),
            run_id: format!("run-{agent_id}"),
            parent_run_id: "root".to_string(),
            agent_type: "task".to_string(),
            description: "review auth flow".to_string(),
            status,
            started_at: std::time::SystemTime::now(),
            ended_at,
            metrics: SpawnedAgentMetrics::default(),
            has_permission_issues: false,
            run_in_background,
            spawn_tool_call_id: None,
            fanout_slot: None,
        }
    }

    fn test_spawn_context() -> astra_runtime::orchestration::SpawnContext {
        astra_runtime::orchestration::SpawnContext {
            parent_run_id: "root".to_string(),
            parent_agent_id: "root".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: astra_runtime::orchestration::InheritedPermissions::auto_approve(
            ),
            inherited_skills: Vec::new(),
            live_event_sink: None,
            client_tool_delivery_tx: None,
            trace_context: None,
            spawn_tool_call_id: None,
            execution_metadata: None,
            delegation_chain: Vec::new(),
        }
    }

    fn test_agent_spawner(
        executor: Arc<dyn astra_runtime::orchestration::SpawnAgentExecutor>,
    ) -> Arc<astra_runtime::orchestration::DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(router).with_executor(executor),
        )
    }

    struct PendingAgentExecutor;

    #[async_trait::async_trait]
    impl astra_runtime::orchestration::SpawnAgentExecutor for PendingAgentExecutor {
        async fn execute(
            &self,
            _config: astra_runtime::orchestration::SpawnRunConfig,
        ) -> Result<astra_runtime::orchestration::SpawnRunResult, String> {
            std::future::pending::<Result<astra_runtime::orchestration::SpawnRunResult, String>>()
                .await
        }
    }

    /// REGRESSION (reviewer L3 — Architecture): the
    /// `ReopenTarget::as_str() ↔ ReopenTarget::parse()` round-trip
    /// MUST be lossless for every variant. The dispatcher channel
    /// (`BottomPaneAction::ViewCompleted { reopen: Option<String> }`)
    /// carries the string-form across the view boundary, so a typo
    /// in the constant would silently break re-open semantics
    /// without compile-checking. Pin every variant — when a future
    /// `ReopenTarget::Foo` lands, add it to the array below and the
    /// round-trip check covers it.
    #[test]
    fn reopen_target_round_trips_through_string() {
        let variants: &[ReopenTarget] = &[ReopenTarget::Agents];
        for &target in variants {
            let encoded = target.as_str();
            let decoded = ReopenTarget::parse(encoded).expect("known variant must round-trip");
            assert_eq!(decoded, target, "variant {encoded} did not round-trip");
        }
    }

    #[test]
    fn ctrl_b_promoted_agent_message_is_user_facing() {
        let agent = agent_info(
            "reviewer@run-1",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
            true,
        );
        let message = ctrl_b_promoted_agent_message(&[agent]);

        assert!(message.contains("Backgrounded agent reviewer@run-1"));
        assert!(message.contains("review auth flow"));
        assert!(message.contains("Opened background tasks"));
        assert!(!message.contains("agent(action="), "{message}");
        assert!(!message.contains("task_output"), "{message}");
        assert!(!message.contains("job("), "{message}");
    }

    #[test]
    fn ctrl_b_fanout_message_names_the_atomic_group() {
        let agents = (0..2)
            .map(|slot_index| {
                let mut agent = agent_info(
                    &format!("reviewer-{slot_index}"),
                    AgentStatus::Running {
                        activity: "reviewing".into(),
                    },
                    true,
                );
                agent.fanout_slot = Some(
                    astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity::new(
                        "review-group",
                        2,
                        slot_index,
                        None,
                    )
                    .unwrap(),
                );
                agent
            })
            .collect::<Vec<_>>();

        let message = ctrl_b_promoted_agent_message(&agents);
        assert!(message.contains("Backgrounded review-group (2 agents)"));
        assert!(message.contains("Opened background tasks"));
    }

    #[test]
    fn background_task_row_for_local_agent_only_projects_background_agents() {
        let foreground = agent_info(
            "agent-foreground",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
            false,
        );
        assert!(
            background_task_row_for_local_agent(&foreground).is_none(),
            "foreground sync agents must not appear in the background task footer"
        );

        let background = agent_info(
            "agent-background",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
            true,
        );
        let row = background_task_row_for_local_agent(&background)
            .expect("background agent should project to a task row");
        assert_eq!(
            row.kind,
            bottom_pane::background_task_view::BackgroundTaskKind::LocalAgent
        );
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Running
        );
        assert_eq!(row.title, "review auth flow");

        let counts = status_line::BackgroundTaskCounts::from_rows(&[row]);
        assert_eq!(counts.local_agents, 1);
        assert_eq!(counts.running, 0);
    }

    #[test]
    fn failed_background_agent_projects_as_failed_local_agent_attention() {
        let failed = agent_info(
            "agent-failed",
            AgentStatus::Failed {
                error: "review failed".into(),
                finish_reason: Some("failed".into()),
            },
            true,
        );
        let row = background_task_row_for_local_agent(&failed)
            .expect("failed background agent should remain reachable");

        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Failed
        );
        assert_eq!(row.output_tail.as_deref(), Some("review failed"));
        assert_eq!(row.terminal_reason.as_deref(), Some("failed"));

        let counts = status_line::BackgroundTaskCounts::from_rows(&[row]);
        assert_eq!(counts.failed_local_agents, 1);
    }

    #[test]
    fn background_task_rows_xml_projects_local_agent_rows() {
        let agent = agent_info(
            "agent-1",
            AgentStatus::Running {
                activity: "reviewing auth middleware".into(),
            },
            true,
        );
        let row = background_task_row_for_local_agent(&agent)
            .expect("background agent should project to a task row");

        let xml = render_background_task_rows_xml(&[row]);

        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("id=\"agent-1\""), "{xml}");
        assert!(xml.contains("kind=\"local agent\""), "{xml}");
        assert!(xml.contains("status=\"running\""), "{xml}");
        assert!(xml.contains("description=\"review auth flow\""), "{xml}");
        assert!(
            xml.contains("preview=\"reviewing auth middleware\""),
            "{xml}"
        );
        assert!(!xml.contains("Job"), "{xml}");
    }

    #[test]
    fn background_local_agent_row_preserves_fanout_membership_for_footer_and_switcher() {
        let mut agent = agent_info(
            "agent-auth",
            AgentStatus::Running {
                activity: "reviewing auth middleware".into(),
            },
            true,
        );
        agent.fanout_slot = Some(
            astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity::new(
                "review-1", 3, 0, None,
            )
            .unwrap(),
        );

        let row =
            background_task_row_for_local_agent_with_fanout_title(&agent, Some("review fanout"))
                .expect("background fanout agent should project to a task row");
        let fanout = row.fanout.as_ref().expect("fanout metadata");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 0);

        let summaries =
            status_line::BackgroundTaskFanoutSummary::from_rows(std::slice::from_ref(&row));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].target_count, 3);
        assert_eq!(summaries[0].running, 1);

        let xml = render_background_task_rows_xml(&[row]);
        assert!(xml.contains("fanout_group_id=\"review-1\""), "{xml}");
        assert!(
            xml.contains("fanout_group_title=\"review fanout\""),
            "{xml}"
        );
        assert!(xml.contains("fanout_target_count=\"3\""), "{xml}");
        assert!(xml.contains("fanout_slot_index=\"0\""), "{xml}");
    }

    #[test]
    fn background_task_row_projects_rejected_fanout_slot_without_agent_history() {
        let mut group =
            astra_turn_core::orchestration_fanout_group::AgentFanoutGroupProjection::new(
                "review-1",
                "review fanout",
                3,
            );
        group
            .set_slot_request(
                1,
                Some("api-reviewer".to_string()),
                "api reviewer",
                "review API surface",
            )
            .unwrap();
        group
            .record_spawn_rejected(1, "concurrency cap reached")
            .unwrap();

        let row = background_task_row_for_rejected_fanout_slot(&group, &group.slots[1])
            .expect("rejected fanout slot should project to a task row");

        assert_eq!(row.id, "fanout:review-1:slot:1:spawn_rejected");
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Failed
        );
        assert_eq!(row.title, "review API surface");
        assert_eq!(row.output_tail.as_deref(), Some("concurrency cap reached"));
        assert_eq!(
            row.terminal_reason.as_deref(),
            Some("concurrency cap reached")
        );
        assert_eq!(
            row.live_control,
            bottom_pane::background_task_view::LiveControlState::UnsupportedInMode
        );

        let fanout = row.fanout.as_ref().expect("fanout metadata");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 1);
        assert_eq!(fanout.slot_label, "review API surface");

        let summaries =
            status_line::BackgroundTaskFanoutSummary::from_rows(std::slice::from_ref(&row));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].failed, 1);

        let xml = render_background_task_rows_xml(std::slice::from_ref(&row));
        assert!(
            xml.contains("id=\"fanout:review-1:slot:1:spawn_rejected\""),
            "{xml}"
        );
        assert!(xml.contains("status=\"failed\""), "{xml}");
        assert!(xml.contains("preview=\"concurrency cap reached\""), "{xml}");

        let snapshot =
            background_task_output_snapshot_for_rejected_fanout_slot(&group, &group.slots[1], 0, 9)
                .expect("snapshot");
        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.status, "failed");
        assert!(snapshot.terminal);
        assert_eq!(snapshot.output, "concurren");
        assert_eq!(snapshot.total_bytes, "concurrency cap reached".len() as u64);
    }

    #[test]
    fn background_task_output_snapshot_projects_local_agent_state() {
        let agent = agent_info(
            "agent-1",
            AgentStatus::Running {
                activity: "reviewing auth middleware".into(),
            },
            true,
        );

        let snapshot = background_task_output_snapshot_for_local_agent(&agent, 0, 8192);

        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.title.as_deref(), Some("review auth flow"));
        assert_eq!(snapshot.status, "running");
        assert_eq!(snapshot.output, "reviewing auth middleware");
        assert_eq!(snapshot.output_ref, "agent_state: agent-1");
        assert!(!snapshot.terminal);
    }

    fn restored_local_agent_projection(
        status: &str,
    ) -> astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: "agent-restored".into(),
            run_id: "run-restored".into(),
            parent_run_id: "root".into(),
            status: status.into(),
            title: "review auth flow".into(),
            started_at_ms: 1,
            ended_at_ms: None,
            output_tail: Some("reviewing auth middleware".into()),
            terminal_reason: None,
            fanout: None,
        }
    }

    fn restored_fanout_local_agent_projection(
        status: &str,
    ) -> astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: "agent-restored-fanout".into(),
            run_id: "run-restored-fanout".into(),
            parent_run_id: "root".into(),
            status: status.into(),
            title: "review auth flow".into(),
            started_at_ms: 1,
            ended_at_ms: None,
            output_tail: Some("reviewing auth middleware".into()),
            terminal_reason: None,
            fanout: Some(
                astra_services::session_workspace::BackgroundLocalAgentFanoutProjection {
                    group_id: "review-1".into(),
                    group_title: "review fanout".into(),
                    target_count: 3,
                    slot_index: 1,
                    slot_label: "auth review".into(),
                },
            ),
        }
    }

    #[tokio::test]
    async fn restored_local_agent_preserves_last_observed_state_and_marks_stale() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let rows = background_task_rows_with_agents(&mut registry, None, &restored).await;

        let row = rows
            .iter()
            .find(|row| row.id == "agent-restored")
            .expect("restored local agent row");
        assert_eq!(
            row.kind,
            bottom_pane::background_task_view::BackgroundTaskKind::LocalAgent
        );
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Running
        );
        assert_eq!(
            row.live_control,
            bottom_pane::background_task_view::LiveControlState::StaleHandle
        );
        assert_eq!(
            row.output_tail.as_deref(),
            Some("reviewing auth middleware")
        );

        let counts = status_line::BackgroundTaskCounts::from_rows(&rows);
        assert_eq!(counts.local_agents, 1);
        assert_eq!(counts.stale_snapshots, 1);
        assert_eq!(counts.unavailable_local_agents, 0);
        assert!(!counts.is_empty());
    }

    #[tokio::test]
    async fn background_task_list_xml_includes_restored_local_agent_without_spawner() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let xml = render_background_task_list_xml_with_agents(&mut registry, None, &restored).await;

        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("id=\"agent-restored\""), "{xml}");
        assert!(xml.contains("kind=\"local agent\""), "{xml}");
        assert!(xml.contains("status=\"running\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
        assert!(
            xml.contains("preview=\"reviewing auth middleware\""),
            "{xml}"
        );
        assert!(!xml.contains("Job"), "{xml}");
    }

    #[tokio::test]
    async fn foreground_tick_services_background_output_commands() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let task_id = registry.spawn_shell("printf 'progress\\n'", "long test");
        for _ in 0..100 {
            registry.poll_completions();
            if registry
                .get(&task_id)
                .is_some_and(|task| task.status().as_str() != "running")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let commands = Arc::new(std::sync::Mutex::new(vec![
            crate::edge_tools::BgTaskCommand::GetOutputSince {
                task_id: task_id.clone(),
                offset: 0,
                max_bytes: 8192,
                reply: reply_tx,
            },
        ]));
        let list_cache = Arc::new(tokio::sync::RwLock::new(String::new()));

        drain_background_task_commands(&commands, &mut registry, None, &[], &list_cache).await;

        let snapshot = reply_rx
            .await
            .expect("foreground tick must answer command")
            .expect("background output must be readable");
        assert_eq!(snapshot.output, "progress\n");
        assert!(snapshot.terminal);
        assert!(commands.lock_recover().is_empty());
    }

    #[tokio::test]
    async fn foreground_tick_services_background_output_search_commands() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let task_id = registry.spawn_shell(
            "printf 'before\\nfailing_test_name\\npanic detail\\n'",
            "failing test",
        );
        for _ in 0..100 {
            registry.poll_completions();
            if registry
                .get(&task_id)
                .is_some_and(|task| task.status().as_str() != "running")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let commands = Arc::new(std::sync::Mutex::new(vec![
            crate::edge_tools::BgTaskCommand::SearchOutput {
                task_id: task_id.clone(),
                pattern: "failing_test_name".to_string(),
                context_lines: 1,
                max_bytes: 8192,
                reply: reply_tx,
            },
        ]));
        let list_cache = Arc::new(tokio::sync::RwLock::new(String::new()));

        drain_background_task_commands(&commands, &mut registry, None, &[], &list_cache).await;

        let snapshot = reply_rx
            .await
            .expect("foreground tick must answer search command")
            .expect("background output must be searchable");
        assert_eq!(snapshot.matching_lines, 1);
        assert!(
            snapshot.output.contains("panic detail"),
            "{}",
            snapshot.output
        );
        assert!(snapshot.terminal);
        assert!(commands.lock_recover().is_empty());
    }

    #[tokio::test]
    async fn restored_local_agent_keeps_fanout_group_metadata_for_resume_footer() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_fanout_local_agent_projection("running")];

        let rows = background_task_rows_with_agents(&mut registry, None, &restored).await;
        let row = rows
            .iter()
            .find(|row| row.id == "agent-restored-fanout")
            .expect("restored fanout row");
        let fanout = row.fanout.as_ref().expect("fanout metadata");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 1);
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Running
        );

        let summaries = status_line::BackgroundTaskFanoutSummary::from_rows(&rows);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].target_count, 3);
        assert_eq!(summaries[0].running, 1);
        assert_eq!(summaries[0].unavailable, 0);

        let xml = render_background_task_rows_xml(&rows);
        assert!(xml.contains("fanout_group_id=\"review-1\""), "{xml}");
        assert!(xml.contains("fanout_target_count=\"3\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
    }

    #[tokio::test]
    async fn task_output_command_reads_restored_local_agent_projection() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let snapshot = background_task_output_snapshot_with_agents(
            &mut registry,
            None,
            &restored,
            "agent-restored",
            0,
            8192,
        )
        .await
        .expect("restored projection should be readable");

        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.status, "running");
        assert!(!snapshot.terminal);
        assert_eq!(snapshot.output, "reviewing auth middleware");
        assert_eq!(snapshot.output_ref, "workspace_projection: agent-restored");
    }

    #[tokio::test]
    async fn task_stop_command_reports_stale_handle_for_restored_local_agent() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let error =
            stop_background_task_with_agents(&mut registry, None, &restored, "agent-restored")
                .await
                .expect_err("restored local agent has no live handle");

        assert_eq!(
            error,
            BackgroundTaskError::StaleHandle {
                task_id: "agent-restored".into(),
            }
        );
    }

    #[tokio::test]
    async fn background_task_list_xml_includes_local_agent_without_shells() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        assert!(matches!(spawned, SpawnAgentOutput::Launched { .. }));

        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));

        let xml =
            render_background_task_list_xml_with_agents(&mut registry, Some(&spawner), &[]).await;

        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("kind=\"local agent\""), "{xml}");
        assert!(xml.contains("description=\"review auth flow\""), "{xml}");
        assert!(!xml.contains("kind=\"shell\""), "{xml}");
        assert!(!xml.contains("Job"), "{xml}");

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[tokio::test]
    async fn task_stop_command_can_cancel_local_agent() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));

        let target =
            stop_background_task_with_agents(&mut registry, Some(&spawner), &[], &agent_id)
                .await
                .expect("task_stop should cancel a background local agent");
        assert_eq!(target, BackgroundTaskStopTarget::LocalAgent);

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled agent should remain in history");
        assert!(matches!(
            archived.status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
    }

    #[tokio::test]
    async fn task_output_command_projects_local_agent_without_shell_output() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));

        let snapshot = background_task_output_snapshot_with_agents(
            &mut registry,
            Some(&spawner),
            &[],
            &agent_id,
            0,
            8192,
        )
        .await
        .expect("task_output should project a background local agent");

        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.title.as_deref(), Some("review auth flow"));
        assert_ne!(snapshot.output_ref, "");

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[tokio::test]
    async fn background_task_stop_action_can_cancel_local_agent() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let mut chat_widget = chat_widget::ChatWidget::new("");
        let mut bottom_pane = BottomPane::new();
        dispatch_background_task_stop(
            &agent_id,
            &mut registry,
            Some(spawner.clone()),
            &[],
            &mut chat_widget,
            &mut bottom_pane,
            &FrameRequester::test_dummy(),
        )
        .await;

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled agent should remain in history");
        assert!(matches!(
            archived.status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
    }

    #[tokio::test]
    async fn background_task_switcher_opens_for_local_agent_without_shells() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        assert!(matches!(spawned, SpawnAgentOutput::Launched { .. }));

        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            open_background_task_view(
                &mut registry,
                Some(&spawner),
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "local agent rows must open the background task switcher even when no shell tasks exist"
        );
        assert!(bottom_pane.has_active_view());

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[test]
    fn ctrl_b_background_hint_requires_detach() {
        assert!(should_show_ctrl_b_background_hint(true));
        assert!(!should_show_ctrl_b_background_hint(false));
    }

    #[test]
    fn shift_down_is_background_task_manage_key() {
        let shift_down = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::SHIFT,
        );
        let plain_down = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        let ctrl_b = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        );

        assert!(is_background_task_manage_key(&shift_down));
        assert!(!is_background_task_manage_key(&plain_down));
        assert!(!is_background_task_manage_key(&ctrl_b));
    }

    #[test]
    fn ctrl_b_is_background_key() {
        let ctrl_b = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let plain_b = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::NONE,
        );
        let ctrl_c = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('c'),
            crossterm::event::KeyModifiers::CONTROL,
        );

        assert!(is_ctrl_b_background_key(&ctrl_b));
        assert!(!is_ctrl_b_background_key(&plain_b));
        assert!(!is_ctrl_b_background_key(&ctrl_c));
    }

    #[tokio::test]
    async fn ctrl_t_task_surface_is_consistent_and_does_not_mutate_behind_a_modal() {
        let temp = crate::tests::test_temp_dir();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("task-shortcut"),
        );
        let store = Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new().with_validation());
        let task_board = task_board_observer::TaskBoardObserver::new(store, "session-a");
        let frame_requester = FrameRequester::test_dummy();
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('t'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let mut expanded = false;
        let mut pin = None;
        let mut bottom_pane = BottomPane::new();
        let background_task_id = registry.spawn_shell("sleep 60", "unrelated background work");

        assert!(handle_task_surface_shortcut(
            &key,
            &task_board,
            &mut expanded,
            &mut pin,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert!(expanded);
        assert_eq!(pin, Some(true));
        assert!(
            !bottom_pane.has_active_view(),
            "Ctrl+T must not open the background-task panel even when background work exists"
        );
        registry.kill(&background_task_id).unwrap();

        bottom_pane.push_view(Box::new(
            crate::tui::bottom_pane::info_view::InfoView::from_plain(
                "Permission review",
                vec!["Keep focus here".to_string()],
            ),
        ));
        assert!(!handle_task_surface_shortcut(
            &key,
            &task_board,
            &mut expanded,
            &mut pin,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert!(expanded);
        assert_eq!(pin, Some(true));
    }

    #[test]
    fn ctrl_t_opens_canonical_task_board_over_a_conversation_tab() {
        let store = Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new().with_validation());
        let task_board = task_board_observer::TaskBoardObserver::new(store, "session-a");
        let frame_requester = FrameRequester::test_dummy();
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('t'),
            crossterm::event::KeyModifiers::CONTROL,
        );
        let mut expanded = false;
        let mut pin = None;
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(
            crate::tui::bottom_pane::transcript_view::TranscriptView::from_snapshot(
                crate::tui::bottom_pane::transcript_view::TranscriptSnapshot::default(),
                24,
                80,
            ),
        ));

        assert!(handle_task_surface_shortcut(
            &key,
            &task_board,
            &mut expanded,
            &mut pin,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert!(bottom_pane.primary_workspace_is_open());
        assert!(render_bottom_pane_text(&bottom_pane, 80, 16).contains("Task board"));

        assert!(matches!(
            bottom_pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Esc,
                crossterm::event::KeyModifiers::NONE,
            )),
            crate::tui::bottom_pane::BottomPaneAction::ViewCompleted { .. }
        ));
        assert!(bottom_pane.conversation_tab_is_open());
    }

    #[test]
    fn ctrl_g_switches_from_task_board_to_the_agent_run_tree() {
        let store = Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new().with_validation());
        let task_board = task_board_observer::TaskBoardObserver::new(store, "session-a");
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(
            crate::tui::bottom_pane::task_board_view::TaskBoardView::new(
                task_board.active_projection(),
            ),
        ));
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            crate::tui::chat_widget::WireEvent::AgentLive(
                astra_turn_core::agent_live_event::AgentLiveEvent {
                    run_id: "run-review".into(),
                    agent_id: "reviewer@run-review".into(),
                    kind: astra_turn_core::agent_live_event::AgentLiveEventKind::OutputDelta(
                        "reviewing".into(),
                    ),
                },
            ),
        ));

        assert!(handle_agent_monitor_shortcut(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('g'),
                crossterm::event::KeyModifiers::CONTROL,
            ),
            &mut chat_widget,
            &mut bottom_pane,
            &FrameRequester::test_dummy(),
        ));
        assert!(bottom_pane.agent_monitor_is_open());
    }

    #[test]
    fn session_rebind_keeps_every_workbench_projection_in_one_scope() {
        let task_board = task_board_observer::TaskBoardObserver::new(
            Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new()),
            "session-a",
        );
        let api = astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
            .expect("test API client");
        let server_agent_observer = crate::tui::server_agent_observer::ServerAgentObserver::new(
            api.clone(),
            None,
            Some("session-a"),
        );
        let plan_task_observer =
            crate::tui::plan_task_observer::PlanTaskObserver::new(api, None, Some("session-a"));
        let mut board_user_pin = Some(true);

        rebind_workbench_observers(
            Some("session-b"),
            &task_board,
            &server_agent_observer,
            &plan_task_observer,
            &mut board_user_pin,
        );

        assert_eq!(
            task_board.truth_state(),
            task_board_observer::TaskBoardTruthState::Loading
        );
        assert_eq!(
            server_agent_observer.projection().truth_state,
            crate::tui::server_agent_observer::ServerAgentTruthState::Loading
        );
        assert_eq!(
            plan_task_observer.projection().truth_state,
            crate::tui::plan_task_observer::PlanTaskTruthState::Loading
        );
        assert_eq!(board_user_pin, None);

        rebind_workbench_observers(
            Some("  "),
            &task_board,
            &server_agent_observer,
            &plan_task_observer,
            &mut board_user_pin,
        );
        assert_eq!(
            task_board.truth_state(),
            task_board_observer::TaskBoardTruthState::Unbound
        );
        assert_eq!(
            server_agent_observer.projection().truth_state,
            crate::tui::server_agent_observer::ServerAgentTruthState::Unbound
        );
        assert_eq!(
            plan_task_observer.projection().truth_state,
            crate::tui::plan_task_observer::PlanTaskTruthState::Unbound
        );
    }

    #[test]
    fn ctrl_shift_t_opens_the_cross_session_board_even_before_it_has_rows() {
        let store = Arc::new(astra_tools::task_mgmt::InMemoryTaskStore::new().with_validation());
        let task_board = task_board_observer::TaskBoardObserver::new(store, "session-a");
        let frame_requester = FrameRequester::test_dummy();
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('t'),
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::SHIFT,
        );
        let mut expanded = false;
        let mut pin = None;
        let mut bottom_pane = BottomPane::new();

        assert!(handle_task_surface_shortcut(
            &key,
            &task_board,
            &mut expanded,
            &mut pin,
            &mut bottom_pane,
            &frame_requester,
        ));
        assert_eq!(
            task_board.view_mode(),
            task_board_observer::ViewMode::AllSessions
        );
        assert!(expanded);
        assert_eq!(pin, Some(true));
    }

    #[tokio::test]
    async fn bash_detach_handoff_timeout_returns_error() {
        let (handle, listener) = astra_tools::detach::new_detach_pair();
        handle.mark_active(true);

        let result =
            await_bash_detach_handoff_with_timeout(listener, Duration::from_millis(1)).await;

        let error = match result {
            Ok(_) => panic!("missing payload should time out"),
            Err(error) => error,
        };
        assert!(error.contains("before timeout"));
        assert!(!handle.is_blocked());
    }

    #[tokio::test]
    async fn force_open_background_task_view_opens_panel_on_empty_registry() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("empty"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            force_open_background_task_view(
                &mut registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "Ctrl+B should open the background panel even on an empty registry"
        );
        assert!(
            bottom_pane.has_active_view(),
            "background task shortcut must open a panel for an empty registry"
        );
    }

    #[tokio::test]
    async fn background_task_switcher_opens_for_pending_bash_handoff() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("pending"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            reveal_background_task_view_with_extra_rows(
                &mut registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
                vec![pending_bash_handoff_row("$ make build", 0)],
                Some(PENDING_BASH_HANDOFF_TASK_ID),
            )
            .await,
            "Ctrl+B should open background tasks immediately while bash handoff is pending"
        );

        assert!(bottom_pane.has_active_view());
        let counts = bottom_pane
            .footer
            .bg_task_counts
            .expect("pending handoff row should surface footer counts");
        assert_eq!(counts.running, 1);
    }

    struct NonBackgroundPane;

    impl bottom_pane::view::BottomPaneView for NonBackgroundPane {
        fn render(&self, _area: ratatui::layout::Rect, _buf: &mut ratatui::buffer::Buffer) {}

        fn desired_height(&self, _width: u16) -> u16 {
            1
        }

        fn handle_key(&mut self, _key: crossterm::event::KeyEvent) {}

        fn cursor_pos(&self, _area: ratatui::layout::Rect) -> Option<(u16, u16)> {
            None
        }
    }

    #[tokio::test]
    async fn force_open_background_task_view_preempts_existing_bottom_pane() {
        let temp = crate::tests::test_temp_dir();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("overlay"));
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(NonBackgroundPane));

        assert!(
            !bottom_pane.accepts_background_task_rows(),
            "test pane must model a non-background active pane"
        );
        assert!(
            force_open_background_task_view(
                &mut registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "background manager should open even when another bottom pane is active"
        );
        assert!(
            bottom_pane.accepts_background_task_rows(),
            "background manager must become the active pane"
        );
        assert!(
            bottom_pane.close_active_view(),
            "background manager should close normally"
        );
        assert!(
            bottom_pane.has_active_view(),
            "the previous pane should still be underneath the background manager"
        );
    }

    #[tokio::test]
    async fn background_task_rows_include_typed_status_and_combined_tail() {
        let temp = crate::tests::test_temp_dir();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-row-projection"),
        );
        let id = registry.spawn_shell("printf 'stdout-line'; printf 'stderr-line' >&2", "demo");

        wait_for_background_shell_terminal(&mut registry, &id).await;
        wait_for_background_shell_preview(&mut registry, &id).await;

        let rows = background_task_rows(&mut registry);
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .expect("spawned task should project into switcher rows");
        assert_eq!(row.kind.as_str(), "shell");
        assert_eq!(row.status.as_str(), "completed");
        let output_ref = row.output_ref.as_deref().unwrap_or_default();
        assert!(output_ref.contains("stdout:"), "{output_ref:?}");
        assert!(output_ref.contains("stderr:"), "{output_ref:?}");
        let tail = row.output_tail.as_deref().unwrap_or_default();
        assert!(tail.contains("stdout-line"), "{tail:?}");
        assert!(tail.contains("stderr-line"), "{tail:?}");
        assert!(
            row.total_bytes.unwrap_or_default() >= "stdout-linestderr-line".len() as u64,
            "total bytes should include captured stdout and stderr"
        );
    }

    #[tokio::test]
    async fn background_task_rows_surface_missing_output_artifact() {
        let temp = crate::tests::test_temp_dir();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-missing-output"),
        );
        let id = registry.spawn_shell("printf 'done'", "missing output artifact");

        wait_for_background_shell_terminal(&mut registry, &id).await;
        let stdout_path = registry.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).expect("remove captured stdout artifact");
        wait_for_background_shell_preview(&mut registry, &id).await;

        let rows = background_task_rows(&mut registry);
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .expect("spawned task should project into switcher rows");
        let tail = row.output_tail.as_deref().unwrap_or_default();

        assert!(tail.contains("Output artifact missing"), "{tail}");
        assert!(tail.contains(&stdout_path.display().to_string()), "{tail}");
        assert!(row.total_bytes.is_none(), "{row:?}");
        assert!(row.output_offset.is_none(), "{row:?}");
    }

    #[tokio::test]
    async fn background_task_rows_separate_restored_freshness_from_lifecycle() {
        let temp = crate::tests::test_temp_dir();
        let stdout = temp.path().join("restored.stdout");
        let stderr = temp.path().join("restored.stderr");
        std::fs::write(&stdout, "line from previous session\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-restored-row"),
        );
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    id: "bg-shell-restored".into(),
                    status: "running".into(),
                    title: "cargo build".into(),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: None,
                    terminal_reason: None,
                },
            )
            .unwrap();

        wait_for_background_shell_preview(&mut registry, "bg-shell-restored").await;
        let rows = background_task_rows(&mut registry);
        let row = rows
            .iter()
            .find(|row| row.id == "bg-shell-restored")
            .expect("restored row");

        assert_eq!(row.status.as_str(), "running");
        assert_eq!(row.started_at_ms, Some(1));
        assert_eq!(row.ended_at_ms, None);
        assert_eq!(
            row.live_control,
            bottom_pane::background_task_view::LiveControlState::StaleHandle
        );
        assert_eq!(
            row.output_tail.as_deref(),
            Some("line from previous session")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn background_task_projection_persistence_round_trips_workspace() {
        let temp = crate::tests::test_temp_dir();
        let _guard = astra_services::session_journal::ProcessJournalDirGuard::new(temp.path());
        let session_id = "bg-projection-session";
        let mut workspace = astra_services::session_workspace::WorkspaceMetadata::with_context(
            session_id,
            "gpt-5",
            "/tmp",
            Some("main"),
        );
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let stdout = temp.path().join("restored.stdout");
        let stderr = temp.path().join("restored.stderr");
        std::fs::write(&stdout, "persisted\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-persist"),
        );
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    id: "bg-shell-persist".into(),
                    status: "completed".into(),
                    title: "cargo test".into(),
                    started_at_ms: 42,
                    ended_at_ms: Some(84),
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: Some(0),
                    terminal_reason: Some("exit code 0".into()),
                },
            )
            .unwrap();

        let mut cache = Vec::new();
        persist_background_task_projections_if_changed(
            &mut registry,
            Some(session_id),
            Some("gpt-5"),
            &mut cache,
        )
        .await;
        workspace = astra_services::session_workspace::read_workspace(session_id).unwrap();

        assert_eq!(workspace.background_shell_tasks.len(), 1);
        assert_eq!(workspace.background_shell_tasks[0].id, "bg-shell-persist");
        assert_eq!(workspace.background_shell_tasks[0].status, "completed");
        assert_eq!(workspace.background_shell_tasks[0].ended_at_ms, Some(84));
        assert_eq!(workspace.background_shell_tasks[0].exit_code, Some(0));
        assert_eq!(cache, workspace.background_shell_tasks);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn background_local_agent_projection_persistence_round_trips_workspace() {
        let temp = crate::tests::test_temp_dir();
        let _guard = astra_services::session_journal::ProcessJournalDirGuard::new(temp.path());
        let session_id = "bg-local-agent-projection-session";
        let workspace = astra_services::session_workspace::WorkspaceMetadata::with_context(
            session_id,
            "gpt-5",
            "/tmp",
            Some("main"),
        );
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let mut cache = Vec::new();
        let projections = persist_background_local_agent_task_projections_if_changed(
            Some(&spawner),
            &[],
            Some(session_id),
            Some("gpt-5"),
            &mut cache,
        )
        .await;
        let workspace = astra_services::session_workspace::read_workspace(session_id).unwrap();

        assert_eq!(workspace.background_local_agent_tasks.len(), 1);
        assert_eq!(workspace.background_local_agent_tasks[0].id, agent_id);
        assert_eq!(
            workspace.background_local_agent_tasks[0].title,
            "review auth flow"
        );
        assert_eq!(cache, workspace.background_local_agent_tasks);
        assert_eq!(projections, workspace.background_local_agent_tasks);

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[test]
    fn shared_local_agent_snapshot_keeps_task_rows_and_persistence_in_lockstep() {
        let temp = crate::tests::test_temp_dir();
        let agent = agent_info(
            "agent-shared-snapshot",
            AgentStatus::Running {
                activity: "checking session ownership".into(),
            },
            true,
        );
        let snapshot = crate::tui::local_agent_snapshot::LocalAgentSnapshot {
            available: true,
            agents: vec![agent],
            fanout_groups: Vec::new(),
        };
        let projections =
            export_background_local_agent_task_projections_from_snapshot(&snapshot, &[]);
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("shared-snapshot"),
        );
        let rows = background_task_rows_with_agent_snapshot(&mut registry, &snapshot, &[]);

        assert_eq!(projections.len(), 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(projections[0].id, rows[0].id);
        assert_eq!(projections[0].title, rows[0].title);
        assert_eq!(projections[0].status, "running");
        assert_eq!(
            rows[0].status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Running
        );
        assert_eq!(projections[0].output_tail, rows[0].output_tail);
    }

    #[test]
    fn terminal_local_agent_uses_runtime_end_time_in_every_projection() {
        let temp = crate::tests::test_temp_dir();
        let mut agent = agent_info(
            "agent-terminal-time",
            AgentStatus::Completed {
                result: "done".into(),
                finish_reason: Some("normal".into()),
            },
            true,
        );
        agent.started_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(100);
        agent.ended_at = Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(103));
        let snapshot = crate::tui::local_agent_snapshot::LocalAgentSnapshot {
            available: true,
            agents: vec![agent],
            fanout_groups: Vec::new(),
        };
        let projections =
            export_background_local_agent_task_projections_from_snapshot(&snapshot, &[]);
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("terminal-time"),
        );
        let rows = background_task_rows_with_agent_snapshot(&mut registry, &snapshot, &[]);

        assert_eq!(projections[0].started_at_ms, 100_000);
        assert_eq!(projections[0].ended_at_ms, Some(103_000));
        assert_eq!(rows[0].elapsed_ms, 3_000);
        assert_eq!(rows[0].started_at_ms, Some(100_000));
        assert_eq!(rows[0].ended_at_ms, Some(103_000));
    }

    #[tokio::test]
    async fn background_task_output_snapshot_drains_completion_before_status() {
        let temp = crate::tests::test_temp_dir();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-output-snapshot"),
        );
        let id = registry.spawn_shell("printf 'done\\n'", "quick output");

        wait_for_background_shell_terminal(&mut registry, &id).await;
        let snapshot = background_task_output_snapshot(&mut registry, &id, 0, 1024)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.status, "completed");
        assert!(snapshot.terminal, "{snapshot:?}");
        assert!(snapshot.output.contains("done"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stdout:"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stderr:"), "{snapshot:?}");
    }

    #[tokio::test]
    async fn background_task_output_snapshot_includes_stderr_only_shell_output() {
        let temp = crate::tests::test_temp_dir();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-output-stderr"),
        );
        let id = registry.spawn_shell("printf 'stderr-line\\n' >&2; exit 2", "stderr output");

        wait_for_background_shell_terminal(&mut registry, &id).await;
        let snapshot = background_task_output_snapshot(&mut registry, &id, 0, 1024)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.status, "failed");
        assert!(snapshot.terminal, "{snapshot:?}");
        assert!(snapshot.output.contains("<stderr>"), "{snapshot:?}");
        assert!(snapshot.output.contains("stderr-line"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stdout:"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stderr:"), "{snapshot:?}");
    }

    #[tokio::test]
    async fn background_task_output_snapshot_preserves_restored_last_observed_state() {
        let temp = crate::tests::test_temp_dir();
        let stdout = temp.path().join("restored.stdout");
        let stderr = temp.path().join("restored.stderr");
        std::fs::write(&stdout, "old output\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-output-restored"),
        );
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    id: "bg-shell-restored".into(),
                    status: "running".into(),
                    title: "cargo build".into(),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: None,
                    terminal_reason: None,
                },
            )
            .unwrap();

        let snapshot = background_task_output_snapshot(&mut registry, "bg-shell-restored", 0, 1024)
            .await
            .expect("snapshot");

        assert_eq!(snapshot.status, "running");
        assert!(!snapshot.terminal, "{snapshot:?}");
        assert_eq!(snapshot.output, "old output\n");
    }

    #[tokio::test]
    async fn background_task_switcher_opens_for_failed_but_not_completed_only() {
        let temp = crate::tests::test_temp_dir();
        let mut completed_registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("completed-only"),
        );
        let completed_id = completed_registry.spawn_shell("true", "completed");
        for _ in 0..50 {
            completed_registry.poll_completions();
            if completed_registry
                .get(&completed_id)
                .is_some_and(|h| h.status().as_str() == "completed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut bottom_pane = BottomPane::new();
        assert!(
            !open_background_task_view(
                &mut completed_registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "completed-only background tasks should not trigger an attention-only background view"
        );
        assert!(!bottom_pane.has_active_view());

        let mut failed_registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("failed"));
        let failed_id = failed_registry.spawn_shell("/definitely_missing_astra_binary", "failed");
        for _ in 0..50 {
            failed_registry.poll_completions();
            if failed_registry
                .get(&failed_id)
                .is_some_and(|h| h.status().as_str() == "failed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            open_background_task_view(
                &mut failed_registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "failed background tasks must remain reachable from the background-task surface"
        );
        assert!(bottom_pane.has_active_view());
    }

    #[tokio::test]
    async fn submit_active_run_guidance_enqueues_against_active_local_run_control() {
        let run_control = Arc::new(std::sync::Mutex::new(Some(LocalRunControl::shared())));

        let receipt = submit_active_run_guidance(&run_control, "先停下来吧")
            .await
            .expect("user intent should be accepted");

        let provider = astra_core::sync_poison::recover_mutex_lock(&run_control)
            .clone()
            .expect("run control should stay installed");
        let polled = provider
            .poll_user_intents("local-user", "run-local", 0)
            .await;
        assert_eq!(polled.inputs.len(), 1, "one user intent should be queued");
        assert_eq!(polled.inputs[0].intent_id, receipt.intent_id);
        assert_eq!(polled.inputs[0].input["content"], "先停下来吧");
    }

    #[tokio::test]
    async fn active_run_cancel_updates_control_plane_before_returning() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let run_control = LocalRunControl::default();
        let cancel_token = tokio_util::sync::CancellationToken::new();

        request_active_run_cancel(&mut chat_widget, None, &run_control, &cancel_token).await;

        assert!(cancel_token.is_cancelled());
        assert_eq!(
            run_control
                .control_status("local-user", "run-local")
                .await
                .expect("local control status"),
            Some(RunControlStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn submit_active_run_guidance_rejects_missing_local_run_control() {
        let run_control = Arc::new(std::sync::Mutex::new(None));
        let error = submit_active_run_guidance(&run_control, "先停下来吧")
            .await
            .expect_err("missing run control must be rejected locally");
        assert!(
            error.contains("changing state"),
            "missing run control should surface a user-facing transient-state error"
        );
    }

    #[test]
    fn typed_user_intent_applied_resolves_once_by_intent_id() {
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        bottom_pane.accept_user_intent(
            "input-7",
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::AcceptedLocal,
            "local full input",
        );
        let event = TuiAppEvent::UserIntentApplied {
            intent_id: "input-7".into(),
            delivery: astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            status: astra_turn_types::UserIntentStatus::Applied,
            event_index: 7,
            content: "runtime transport content".into(),
        };

        apply_tui_control_event(&event, &mut bottom_pane, &mut chat_widget);
        apply_tui_control_event(&event, &mut bottom_pane, &mut chat_widget);

        let rendered = rendered_transcript_overlay(&chat_widget, 80);
        assert!(
            rendered.contains("runtime transport content"),
            "{rendered:?}"
        );
        assert_eq!(
            rendered.matches("runtime transport content").count(),
            1,
            "replayed applied event must not duplicate user history"
        );
        assert!(bottom_pane.take_unapplied_user_intents().is_empty());
    }

    #[test]
    fn local_agent_mailbox_received_event_closes_guidance_delivery_once() {
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        assert!(bottom_pane.accept_agent_guide(
            "guide-7".into(),
            "run-reviewer".into(),
            "Reviewer".into(),
            "inspect the storage race".into(),
        ));
        assert!(bottom_pane.promote_agent_guide_accepted("guide-7"));
        let event = TuiAppEvent::AgentCommunication(astra_turn_types::AgentCommunicationEvent {
            schema_version: astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION.into(),
            observed_by: astra_turn_types::AgentCommunicationParty {
                run_id: "run-reviewer".into(),
                agent_id: "reviewer".into(),
            },
            direction: astra_turn_types::AgentCommunicationDirection::Received,
            message_id: "guide-7".into(),
            from: astra_turn_types::AgentCommunicationParty {
                run_id: "run-root".into(),
                agent_id: "main".into(),
            },
            to: astra_turn_types::AgentCommunicationTarget::Direct {
                address: astra_turn_types::AgentCommunicationParty {
                    run_id: "run-reviewer".into(),
                    agent_id: "reviewer".into(),
                },
            },
            payload_kind: "text".into(),
            summary: Some("User guidance".into()),
            response_accepted: None,
            related_message_id: None,
            timestamp_ms: 42,
            correlation_id: None,
            requires_ack: true,
        });

        apply_tui_control_event(&event, &mut bottom_pane, &mut chat_widget);
        apply_tui_control_event(&event, &mut bottom_pane, &mut chat_widget);

        assert!(bottom_pane.remove_agent_guide("guide-7").is_none());
        let rendered = rendered_transcript_overlay(&chat_widget, 100);
        assert_eq!(rendered.matches("Guidance received by Reviewer").count(), 1);
        assert!(
            rendered.contains("inspect the storage race"),
            "{rendered:?}"
        );
    }

    #[test]
    fn transcript_toggle_opens_empty_and_refreshes_live_content() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());
        assert!(bottom_pane.uses_local_root_transcript_snapshot());

        let area = ratatui::layout::Rect::new(0, 0, 80, 20);
        let mut empty = ratatui::buffer::Buffer::empty(area);
        bottom_pane.render(area, &mut empty);
        let empty_text = crate::tui::testing::render::buffer_to_string(&empty);
        assert!(empty_text.contains("No conversation yet."));

        chat_widget.handle_event(chat_widget::AppEvent::User(UserEvent::Submit(
            "live transcript message".to_string(),
        )));
        assert!(refresh_open_transcript_view(
            &chat_widget,
            &mut bottom_pane,
            80,
        ));

        let mut refreshed = ratatui::buffer::Buffer::empty(area);
        bottom_pane.render(area, &mut refreshed);
        let refreshed_text = crate::tui::testing::render::buffer_to_string(&refreshed);
        assert!(refreshed_text.contains("live transcript message"));

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(!bottom_pane.has_active_view());
    }

    #[tokio::test]
    async fn bound_root_transcript_workspace_starts_from_the_durable_lane() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let (agent_workbench_tx, _agent_workbench_rx) = tokio::sync::mpsc::channel(1);

        open_root_transcript_workspace(
            &chat_widget,
            &mut bottom_pane,
            80,
            24,
            ViewActionBackends {
                agent_spawner: None,
                delegation_engine: None,
                api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                    .expect("test thin client"),
                profile: None,
                session_id: Some("durable-root-session".into()),
                file_writer: None,
                agent_workbench_tx,
            },
            &FrameRequester::test_dummy(),
        );

        assert_eq!(
            bottom_pane.active_conversation_tab_id(),
            Some(bottom_pane::view::ConversationTabId::Root),
        );
        assert!(
            !bottom_pane.uses_local_root_transcript_snapshot(),
            "the durable root view must not rebuild a local full-history snapshot on stream updates"
        );
        let rendered = render_bottom_pane_text(&bottom_pane, 80, 24);
        assert!(
            rendered.contains("Loading durable conversation…"),
            "a bound session must not silently fall back to in-memory history: {rendered:?}"
        );

        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AnswerDelta("live durable-root output".into()),
        ));
        assert!(refresh_open_transcript_view(
            &chat_widget,
            &mut bottom_pane,
            80,
        ));
        let refreshed = render_bottom_pane_text(&bottom_pane, 80, 24);
        assert!(
            refreshed.contains("live durable-root output"),
            "a durable root workspace must project the current root cell before its page catches up: {refreshed:?}"
        );
        assert!(
            refreshed.contains("Live local projection"),
            "live root output must carry local provenance until durable reconciliation: {refreshed:?}"
        );
    }

    #[tokio::test]
    async fn binding_a_session_upgrades_an_open_local_root_transcript_to_durable_history() {
        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let (agent_workbench_tx, _agent_workbench_rx) = tokio::sync::mpsc::channel(1);

        // Ctrl+O is available before the first turn creates a session. That
        // local view must not capture the Root tab forever once the canonical
        // session identity arrives.
        open_transcript_view(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(render_bottom_pane_text(&bottom_pane, 80, 24).contains("No conversation yet."));

        open_root_transcript_workspace(
            &chat_widget,
            &mut bottom_pane,
            80,
            24,
            ViewActionBackends {
                agent_spawner: None,
                delegation_engine: None,
                api: astra_thin_client::ThinClient::new("http://127.0.0.1:1", None)
                    .expect("test thin client"),
                profile: None,
                session_id: Some("durable-root-session".into()),
                file_writer: None,
                agent_workbench_tx,
            },
            &FrameRequester::test_dummy(),
        );

        let rendered = render_bottom_pane_text(&bottom_pane, 80, 24);
        assert!(
            rendered.contains("Loading durable conversation…"),
            "a session-bound Ctrl+O must replace the local view and request canonical history: {rendered:?}"
        );
        assert_eq!(
            bottom_pane.conversation_tabs().len(),
            1,
            "source promotion must retain one root conversation tab"
        );
    }

    #[test]
    fn opening_root_transcript_never_closes_an_existing_transcript() {
        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());

        // This is the path after the run navigator has popped itself. It must
        // keep the original conversation visible instead of treating Enter as
        // a second Ctrl+O toggle.
        open_transcript_view(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());
        assert!(bottom_pane.close_active_view());
        assert!(!bottom_pane.has_active_view());
    }

    #[test]
    fn transcript_toggle_switches_from_agent_conversation_to_root_and_returns() {
        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(
            bottom_pane::agent_transcript_view::AgentTranscriptView::live_unbound(
                "agent-reviewer".into(),
                "Reviewer".into(),
                "run-reviewer".into(),
                Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
                "agents",
                80,
                24,
            ),
        ));

        assert!(bottom_pane.has_active_view());
        assert!(
            !bottom_pane.transcript_view_is_open(),
            "an agent transcript is not the root Ctrl+O scope"
        );

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(
            !bottom_pane.transcript_view_is_open() && bottom_pane.has_active_view(),
            "closing the root transcript restores the agent conversation"
        );
    }

    #[test]
    fn root_conversation_tab_reactivation_preserves_search_over_an_agent_tab() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::User(UserEvent::Submit(
            "root conversation".to_string(),
        )));
        let mut bottom_pane = BottomPane::new();

        open_transcript_view(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(matches!(
            bottom_pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('/'),
                crossterm::event::KeyModifiers::NONE,
            )),
            BottomPaneAction::Consumed
        ));
        bottom_pane.push_view(Box::new(
            bottom_pane::agent_transcript_view::AgentTranscriptView::live_unbound(
                "agent-reviewer".into(),
                "Reviewer".into(),
                "run-reviewer".into(),
                Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
                "agents",
                80,
                24,
            ),
        ));

        open_transcript_view(&chat_widget, &mut bottom_pane, 80, 24);
        let rendered = render_bottom_pane_text(&bottom_pane, 80, 24);
        assert!(
            rendered.contains("Search: /"),
            "reactivating a root tab must preserve its view state: {rendered:?}"
        );

        assert!(bottom_pane.close_active_view());
        assert!(
            bottom_pane.activate_agent_transcript("agent-reviewer", "run-reviewer"),
            "closing the root tab restores the existing delegated conversation"
        );
    }

    #[test]
    fn hidden_root_conversation_receives_live_updates_while_an_agent_tab_is_active() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::User(UserEvent::Submit(
            "first root message".to_string(),
        )));
        let mut bottom_pane = BottomPane::new();
        open_transcript_view(&chat_widget, &mut bottom_pane, 80, 24);
        bottom_pane.push_view(Box::new(
            bottom_pane::agent_transcript_view::AgentTranscriptView::live_unbound(
                "agent-reviewer".into(),
                "Reviewer".into(),
                "run-reviewer".into(),
                Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
                "agents",
                80,
                24,
            ),
        ));

        chat_widget.handle_event(chat_widget::AppEvent::User(UserEvent::Submit(
            "root update while reviewer is open".to_string(),
        )));
        assert!(refresh_open_transcript_view(
            &chat_widget,
            &mut bottom_pane,
            80,
        ));

        assert!(bottom_pane.close_active_view());
        assert!(bottom_pane.transcript_view_is_open());
        assert!(
            render_bottom_pane_text(&bottom_pane, 80, 24)
                .contains("root update while reviewer is open"),
            "the root tab must stay live even while another conversation has focus"
        );
    }

    #[test]
    fn external_skill_discovery_failure_keeps_local_ui_usable() {
        let registry = astra_runtime::skills::UnifiedSkillRegistry::new();
        let mut bottom_pane = BottomPane::new();
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        let report = crate::cli::session::session_runtime::ExternalPipelineDiscoveryReport {
            skills: Ok(astra_runtime::skills::SkillDiscoveryReport {
                registered: vec!["local-skill".into(), "bundled-skill".into()],
                failures: vec![astra_runtime::skills::SkillDiscoveryFailure {
                    source: astra_runtime::skills::SkillSourceKind::Database,
                    message: "service unavailable".into(),
                }],
            }),
            mcp_failures: Vec::new(),
        };

        apply_external_capability_discovery(
            Ok(report),
            &registry,
            &mut bottom_pane,
            &mut chat_widget,
        );

        let rendered = rendered_transcript_overlay(&chat_widget, 100);
        assert!(rendered.contains("Skill sources unavailable: database"));
        assert!(rendered.contains("2 skills ready"));
        assert!(rendered.contains("/skill refresh"));
    }

    #[test]
    fn active_external_editor_unavailable_feedback_is_actionable_and_ephemeral() {
        let mut chat_widget = chat_widget::ChatWidget::new("session");

        surface_external_editor_unavailable(&mut chat_widget);

        assert_eq!(chat_widget.history().len(), 1);
        assert!(chat_widget.history()[0].to_persist().is_none());
        let rendered = rendered_transcript_overlay(&chat_widget, 100);
        assert!(rendered.contains("current turn is idle"), "{rendered:?}");
        assert!(rendered.contains("Enter queues"), "{rendered:?}");
        assert!(rendered.contains("Ctrl+C stops"), "{rendered:?}");
    }

    #[test]
    fn transcript_toggle_preserves_pending_plan_review_and_restores_it() {
        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
        bottom_pane.enqueue_plan_review("1. Keep the plan alive".to_string(), response_tx);

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());
        assert!(matches!(
            response_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(!bottom_pane.transcript_view_is_open());
        assert!(bottom_pane.has_active_view());
        let restored = render_bottom_pane_text(&bottom_pane, 80, 24);
        assert!(restored.contains("Keep the plan alive"), "{restored:?}");

        assert!(matches!(
            bottom_pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            BottomPaneAction::ViewCompleted { .. }
        ));
        assert!(matches!(
            response_rx.try_recv(),
            Ok(crate::cli::chat_stream::PlanReviewDecision::Approve {
                mode: crate::cli::permission_manager::PermissionMode::Auto
            })
        ));
    }

    #[test]
    fn transcript_toggle_replaces_a_hidden_transcript_instead_of_stacking_it() {
        use crate::tui::bottom_pane::info_view::InfoView;

        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        bottom_pane.push_view(Box::new(InfoView::from_plain(
            "Permission impact",
            vec!["Review pending".to_string()],
        )));

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(!bottom_pane.transcript_view_is_open());
        assert!(bottom_pane.has_active_view());
        assert!(bottom_pane.close_active_view());
        assert!(
            !bottom_pane.has_active_view(),
            "the first transcript must not remain hidden below the modal"
        );
    }

    #[test]
    fn transcript_toggle_preserves_pending_ask_user_and_restores_it() {
        use crate::cli::chat_stream::{
            AskUserChoice, AskUserPrompt, AskUserQuestion, AskUserResponse,
        };

        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        let (response_tx, mut response_rx) = tokio::sync::oneshot::channel();
        bottom_pane.enqueue_ask_user(
            AskUserPrompt {
                context: None,
                questions: vec![AskUserQuestion {
                    header: "Choice".to_string(),
                    question: "Which path should survive?".to_string(),
                    options: vec![AskUserChoice {
                        label: "Keep modal".to_string(),
                        description: None,
                        preview: None,
                    }],
                    multi_select: false,
                    allow_freeform: false,
                }],
                timeout_ms: None,
            },
            response_tx,
        );

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(bottom_pane.transcript_view_is_open());
        assert!(matches!(
            response_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));

        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 24);
        assert!(!bottom_pane.transcript_view_is_open());
        assert!(bottom_pane.has_active_view());
        let restored = render_bottom_pane_text(&bottom_pane, 80, 24);
        assert!(
            restored.contains("Which path should survive?"),
            "{restored:?}"
        );

        assert!(matches!(
            bottom_pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('1'),
                crossterm::event::KeyModifiers::NONE,
            )),
            BottomPaneAction::ViewCompleted { .. }
        ));
        assert!(matches!(
            response_rx.try_recv(),
            Ok(AskUserResponse::Submitted(_))
        ));
    }

    #[test]
    fn transcript_expansion_survives_reasoning_live_to_committed_transition() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::ReasoningDelta(
                "one\ntwo\nthree\nfour\nfive\nsix\nseven".to_string(),
            ),
        ));
        let mut bottom_pane = BottomPane::new();
        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 30);

        assert!(matches!(
            bottom_pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char('e'),
                crossterm::event::KeyModifiers::CONTROL,
            )),
            BottomPaneAction::Consumed
        ));
        assert!(render_bottom_pane_text(&bottom_pane, 80, 30).contains("one"));

        chat_widget.commit_applied_user_intent(
            "intent-1",
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::Applied,
            "steer the active run",
        );
        assert!(refresh_open_transcript_view(
            &chat_widget,
            &mut bottom_pane,
            80,
        ));
        assert!(
            render_bottom_pane_text(&bottom_pane, 80, 30).contains("one"),
            "mid-turn user insertion must not change the live reasoning identity"
        );

        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::ReasoningDone,
        ));
        assert!(refresh_open_transcript_view(
            &chat_widget,
            &mut bottom_pane,
            80,
        ));

        let settled = render_bottom_pane_text(&bottom_pane, 80, 30);
        assert!(
            settled.contains("one"),
            "expanded body was lost: {settled:?}"
        );
        assert!(settled.contains("▼ Thought"), "{settled:?}");
    }

    #[test]
    fn resumed_reasoning_is_collapsed_then_expands_through_real_overlay() {
        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.replay(vec![crate::tui::turn_event::TurnEvent::Thinking {
            ts: None,
            text: "restored reasoning detail".to_string(),
            duration_ms: Some(900),
        }]);
        let mut bottom_pane = BottomPane::new();
        toggle_local_root_transcript_fallback(&chat_widget, &mut bottom_pane, 80, 30);

        let collapsed = render_bottom_pane_text(&bottom_pane, 80, 30);
        assert!(!collapsed.contains("restored reasoning detail"));
        assert!(collapsed.contains("▶ Thought"), "{collapsed:?}");

        assert!(matches!(
            bottom_pane.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Enter,
                crossterm::event::KeyModifiers::NONE,
            )),
            BottomPaneAction::Consumed
        ));
        let expanded = render_bottom_pane_text(&bottom_pane, 80, 30);
        assert!(expanded.contains("restored reasoning detail"));
        assert!(expanded.contains("▼ Thought"), "{expanded:?}");
    }

    #[test]
    fn local_submit_feedback_is_visible_before_runtime_events_arrive() {
        let mut bottom_pane = BottomPane::new();
        let mut indicator = status_indicator::StatusIndicator::new();
        let at = std::time::Instant::now();

        begin_submission_dispatch_feedback(&mut bottom_pane, &mut indicator, at);

        assert!(matches!(
            indicator.state(),
            status_indicator::IndicatorState::Dispatching { started_at }
                if *started_at == at
        ));
        let indicator_text = indicator
            .render_at(at)
            .expect("waiting feedback should render")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(indicator_text.contains("Sending"), "{indicator_text:?}");

        let area = ratatui::layout::Rect::new(0, 0, 80, 5);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        bottom_pane.render(area, &mut buf);
        let pane_text = crate::tui::testing::render::buffer_to_string(&buf);
        assert!(
            pane_text.contains("Enter queues") && pane_text.contains("Ctrl+C stops"),
            "active-turn composer feedback missing: {pane_text:?}"
        );

        finish_submission_feedback(&mut bottom_pane, &mut indicator);
        assert!(matches!(
            indicator.state(),
            status_indicator::IndicatorState::Idle
        ));
        assert!(indicator.render_at(at).is_none());

        let mut settled = ratatui::buffer::Buffer::empty(area);
        bottom_pane.render(area, &mut settled);
        let settled_text = crate::tui::testing::render::buffer_to_string(&settled);
        assert!(!settled_text.contains("Enter queues follow-up"));
    }

    #[test]
    fn first_runtime_ack_promotes_sending_without_resetting_turn_feedback() {
        let mut bottom_pane = BottomPane::new();
        let mut indicator = status_indicator::StatusIndicator::new();
        let at = std::time::Instant::now();
        begin_submission_dispatch_feedback(&mut bottom_pane, &mut indicator, at);

        handle_app_event(
            &TuiAppEvent::WaitingForModel,
            &mut bottom_pane,
            &mut indicator,
            &FrameRequester::test_dummy(),
        );

        assert!(matches!(
            indicator.state(),
            status_indicator::IndicatorState::WaitingModel { .. }
        ));
        let text = indicator
            .render_at(at)
            .expect("runtime acknowledgement should remain visible")
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("Working"), "{text:?}");
        assert!(text.contains("Starting"), "{text:?}");
        assert!(!text.contains("Sending"), "{text:?}");
    }

    #[test]
    fn file_write_failure_becomes_deduplicated_ephemeral_session_health() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let error = crate::tui::file_writer::TuiFileWriteError {
            target: "session transcript",
            path: std::path::PathBuf::from("/tmp/transcript.jsonl"),
            message: "disk full".into(),
        };
        tx.send(error.clone()).unwrap();
        tx.send(error).unwrap();
        let mut reported = std::collections::HashSet::new();
        let mut state = crate::cli::session::session_state::SessionState::default();
        let mut widget = chat_widget::ChatWidget::new("session-with-id");

        surface_tui_file_write_errors(
            &mut rx,
            &mut reported,
            &mut state,
            &mut widget,
            &FrameRequester::test_dummy(),
        );

        assert_eq!(reported.len(), 1);
        assert_eq!(widget.history().len(), 1, "duplicate failures collapse");
        assert!(
            state
                .session_persistence_error
                .as_deref()
                .is_some_and(|message| message.contains("disk full"))
        );
        let transcript = rendered_transcript_overlay(&widget, 100);
        assert!(
            transcript.contains("Local persistence degraded"),
            "{transcript:?}"
        );
    }

    #[test]
    fn local_shell_submission_has_one_idle_and_active_turn_classification() {
        assert_eq!(
            classify_local_shell_submission("explain !important"),
            LocalShellSubmission::NotShell
        );
        assert_eq!(
            classify_local_shell_submission("  !   "),
            LocalShellSubmission::Empty
        );
        assert_eq!(
            classify_local_shell_submission(" !  printf ready  "),
            LocalShellSubmission::Background("printf ready".to_string())
        );
        assert_eq!(
            classify_local_shell_submission("!nvim notes.md"),
            LocalShellSubmission::Interactive("nvim notes.md".to_string())
        );
    }

    #[tokio::test]
    async fn local_shell_submission_returns_started_before_process_completion_and_captures_output()
    {
        let tmp = crate::tests::test_temp_dir();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            tmp.path().join("local-shell"),
        );

        let id = start_local_background_shell(
            &mut registry,
            "sleep 0.15; printf 'local shell ready\\n'",
        )
        .expect("start local shell");

        let initial_events = registry.poll_completions();
        assert!(initial_events.iter().any(|event| matches!(
            event,
            BgTaskEvent::Started { id: started_id, description }
                if started_id == &id && description.contains("local shell ready")
        )));
        assert!(
            registry
                .get(&id)
                .is_some_and(|handle| handle.status().as_str() == "running"),
            "the submit path must return while the shell is still running"
        );

        wait_for_background_shell_terminal(&mut registry, &id).await;
        let (output, _, _) = registry
            .get_combined_output_stats(&id, 4096)
            .expect("captured local shell output");
        assert!(output.contains("local shell ready"), "{output:?}");
        assert!(
            local_background_shell_started_message(&id, "printf ready").contains("Ctrl+B"),
            "the immediate receipt must expose the observation/control path"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interactive_shell_wait_yields_to_runtime_timers() {
        let shell = run_interactive_shell_command("sleep 0.15");
        tokio::pin!(shell);

        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut shell)
                .await
                .is_err(),
            "the child should still be running while the runtime timer fires"
        );
        assert!(shell.await.expect("wait for shell").success());
    }

    #[test]
    fn reopen_target_parse_rejects_unknown() {
        assert_eq!(ReopenTarget::parse(""), None);
        assert_eq!(ReopenTarget::parse("not-a-target"), None);
        assert_eq!(ReopenTarget::parse("Agents"), None, "case-sensitive");
    }

    #[test]
    fn plan_transition_notice_covers_enter_goal_and_exit() {
        let inactive = PlanModeUiSnapshot::default();
        let entered_empty = PlanModeUiSnapshot {
            active: true,
            goal: String::new(),
            executing: false,
        };
        let entered_goal = PlanModeUiSnapshot {
            active: true,
            goal: "Implement auth middleware".into(),
            executing: false,
        };
        let exited_running = PlanModeUiSnapshot {
            active: false,
            goal: String::new(),
            executing: true,
        };

        let enter_msg = plan_transition_notice(&inactive, &entered_empty, false)
            .expect("entering plan mode should announce itself");
        assert!(enter_msg.contains("Plan mode active"));
        assert!(enter_msg.contains("describe your goal"));

        let goal_msg = plan_transition_notice(&entered_empty, &entered_goal, false)
            .expect("setting the first goal should be surfaced");
        assert!(goal_msg.contains("Plan goal set"));
        assert!(goal_msg.contains("Implement auth middleware"));

        let exit_msg = plan_transition_notice(&entered_goal, &exited_running, false)
            .expect("background execution exit should be surfaced");
        assert!(exit_msg.contains("running in the background"));

        assert!(
            plan_transition_notice(&inactive, &inactive, true).is_none(),
            "a failed/no-op plan request must not be reported as delivered"
        );
    }

    #[test]
    fn detail_refresh_is_scoped_to_incoming_agent_id() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-a".into(),
                kind: AgentLiveEventKind::OutputDelta("a".into()),
            }),
        ));
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-b".into(),
                kind: AgentLiveEventKind::OutputDelta("b".into()),
            }),
        ));

        let mut bottom_pane = BottomPane::new();
        let cell = chat_widget.task_cell_anywhere("agent-a").unwrap();
        bottom_pane.push_view(Box::new(
            bottom_pane::task_detail_view::TaskDetailView::from_task_cell(cell)
                .with_live_task_id("agent-a"),
        ));

        let unrelated = TuiAppEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-b".into(),
            kind: AgentLiveEventKind::OutputDelta("more b".into()),
        });
        assert!(
            !refresh_open_agent_detail_for_event(&unrelated, &chat_widget, &mut bottom_pane),
            "non-open agent events must not rebuild the open detail view"
        );

        let related = TuiAppEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::OutputDelta("more a".into()),
        });
        assert!(
            refresh_open_agent_detail_for_event(&related, &chat_widget, &mut bottom_pane),
            "open agent events should refresh the detail view"
        );
    }

    #[test]
    fn detail_refresh_skips_work_when_no_agent_detail_is_open() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-a".into(),
                kind: AgentLiveEventKind::OutputDelta("a".into()),
            }),
        ));

        let event = TuiAppEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::OutputDelta("more a".into()),
        });
        let mut bottom_pane = BottomPane::new();
        assert!(
            !refresh_open_agent_detail_for_event(&event, &chat_widget, &mut bottom_pane),
            "agent live events should not rebuild detail rows unless a matching detail view is open"
        );
    }

    #[test]
    fn agent_monitor_refresh_ignores_token_only_events() {
        use crate::tui::agent_run_projection::{AgentRunState, AgentRunStatus};
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use bottom_pane::in_flight_agents_view::{AgentRow, InFlightAgentsView};

        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
            agent_id: "agent-a".into(),
            name: "agent-a".into(),
            spawn_tool_call_id: None,
            activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
            run_id: Some("run-agent-a".into()),
            parent_run_id: Some("root-run".into()),
            depth: 1,
            provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
            elapsed_ms: 0,
            state: AgentRunState::observed(AgentRunStatus::Running),
            attention_summary: None,
            fanout: None,
            control_target: None,
            transcript_target: Some(
                crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
            ),
            available_actions: Vec::new(),
            runtime: Default::default(),
        }])));

        let token = TuiAppEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::OutputDelta("token".into()),
        });
        assert!(
            !refresh_open_agent_monitor_for_event(&token, &chat_widget, &mut bottom_pane),
            "token-only events must not rebuild the agent monitor rows"
        );
    }

    #[test]
    fn agent_monitor_refreshes_for_row_affecting_events_only_when_open() {
        use crate::tui::agent_run_projection::{AgentRunState, AgentRunStatus};
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use bottom_pane::in_flight_agents_view::{AgentRow, InFlightAgentsView};

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "agent-a".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "bash".into(),
                    description: "work".into(),
                    tool_use_id: "tool-1".into(),
                },
            }),
        ));
        let event = TuiAppEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "more work".into(),
                tool_use_id: "tool-2".into(),
            },
        });

        let mut closed = BottomPane::new();
        assert!(
            !refresh_open_agent_monitor_for_event(&event, &chat_widget, &mut closed),
            "row-affecting events should not build rows when monitor is closed"
        );

        let mut open = BottomPane::new();
        open.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
            agent_id: "agent-a".into(),
            name: "agent-a".into(),
            spawn_tool_call_id: None,
            activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
            run_id: Some("run-agent-a".into()),
            parent_run_id: Some("root-run".into()),
            depth: 1,
            provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
            elapsed_ms: 0,
            state: AgentRunState::observed(AgentRunStatus::Running),
            attention_summary: None,
            fanout: None,
            control_target: None,
            transcript_target: Some(
                crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
            ),
            available_actions: Vec::new(),
            runtime: Default::default(),
        }])));
        assert!(refresh_open_agent_monitor_for_event(
            &event,
            &chat_widget,
            &mut open
        ));
    }

    #[test]
    fn local_actions_wait_for_their_result_while_conversation_is_immediate() {
        assert!(!should_flush_submission_immediately("/model"));
        assert!(!should_flush_submission_immediately("   /help"));
        assert!(should_flush_submission_immediately("hi"));
        assert!(
            should_flush_submission_immediately("/plan ship the workbench"),
            "an inline plan goal is real conversational input"
        );
    }

    #[test]
    fn startup_observations_update_presentation_without_creating_user_input() {
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-1");

        assert!(!apply_startup_ui_effect(
            StartupUiEffect::GitBranch(None),
            &mut bottom_pane,
            &mut widget,
        ));
        assert!(!apply_startup_ui_effect(
            StartupUiEffect::GitBranch(Some("feature/fast-start".into())),
            &mut bottom_pane,
            &mut widget,
        ));
        assert_eq!(
            bottom_pane.footer.git_branch.as_deref(),
            Some("feature/fast-start")
        );
        assert!(widget.history().is_empty());

        assert!(apply_startup_ui_effect(
            StartupUiEffect::ResumeSummary(
                "While you were away: 1 background shell finished (1 ok).".into()
            ),
            &mut bottom_pane,
            &mut widget,
        ));
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Info,
                text,
                ..
            }) if text == "While you were away: 1 background shell finished (1 ok)."
        ));
    }

    #[test]
    fn model_catalog_completion_opens_picker_and_retains_structured_metadata() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.model = Some("gpt-5".into());
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-1");
        let mut cached_catalog = None;
        let catalog = vec![serde_json::json!({
            "name": "gpt-5",
            "model_id": "provider-gpt-5",
            "provider": "openai",
            "thinking_capability": "high",
            "is_active": true,
        })];

        assert!(apply_model_catalog_effect(
            ModelCatalogEffect::Ready(Ok(catalog)),
            &state,
            &mut bottom_pane,
            &mut widget,
            &mut cached_catalog,
        ));
        assert!(bottom_pane.has_active_view());
        assert_eq!(
            cached_catalog
                .as_ref()
                .and_then(|models| models[0].get("model_id"))
                .and_then(serde_json::Value::as_str),
            Some("provider-gpt-5")
        );
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == "Opened model picker"
        ));
    }

    #[test]
    fn model_catalog_failure_is_visible_and_does_not_open_a_stale_picker() {
        let state = crate::cli::session::session_state::SessionState::default();
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-1");
        let mut cached_catalog = Some(vec![serde_json::json!({ "name": "old" })]);

        assert!(!apply_model_catalog_effect(
            ModelCatalogEffect::Ready(Err("Cannot reach server — check connection".into())),
            &state,
            &mut bottom_pane,
            &mut widget,
            &mut cached_catalog,
        ));
        assert!(!bottom_pane.has_active_view());
        assert_eq!(cached_catalog.as_ref().map(Vec::len), Some(1));
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Error,
                text,
                ..
            }) if text == "Cannot reach server — check connection"
        ));
    }

    #[test]
    fn background_worktree_read_opens_an_interactive_view_from_typed_rows() {
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-1");
        let rows = vec![crate::tui::worktrees::model::WorktreeEntry {
            path: "/repo/feature".into(),
            branch: Some("feature/fast-ui".into()),
            head: Some("abcdef0".into()),
            is_bare: false,
            is_detached: false,
            session_count: 2,
            last_session_at: Some("2026-07-13T00:00:00Z".into()),
        }];

        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::Worktrees(rows),
            &mut bottom_pane,
            &mut widget,
        );

        assert!(bottom_pane.has_active_view());
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == "Opened worktrees"
        ));
    }

    #[test]
    fn background_memory_search_projects_a_typed_payload_into_the_browser() {
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-memory");
        let payload = vec![serde_json::json!({
            "memory_id": "mem-123456789",
            "content": "The user prefers concise review summaries.",
            "memory_type": "preference",
            "score": 0.94,
        })];

        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::Memory(MemoryReadEffect::Search {
                query: "review preferences".into(),
                stats_view: false,
                result: Ok(payload),
            }),
            &mut bottom_pane,
            &mut widget,
        );

        assert!(bottom_pane.has_active_view());
        let rendered = render_bottom_pane_text(&bottom_pane, 100, 12);
        assert!(rendered.contains("review preferences"), "{rendered}");
        assert!(rendered.contains("mem-123"), "{rendered}");
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == "Opened memory browser"
        ));
    }

    #[test]
    fn background_session_memory_keeps_snapshot_and_status_as_separate_facts() {
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-memory");
        let surface = MemorySessionSurface {
            record: Some(crate::cli::slash::slash_memory::SessionMemoryRecord {
                memory_id: "local-session-memory".into(),
                summary: Some("Review branch state".into()),
                body: "## Active Goals\n- Finish the review".into(),
            }),
            status_hint: None,
            status: crate::cli::slash::slash_memory::SessionMemorySurfaceStatus {
                snapshot: "local current-session artifact".into(),
                snapshot_provenance: Some("canonical local artifact".into()),
                extraction: Some("fresh".into()),
                prompt_injection: Some("present on turn 4; 85 tokens".into()),
                repository_prompt_memories: None,
                user_preferences: None,
                remote_sync: Some("not required".into()),
                last_local_refresh_at: Some("2026-07-13T00:00:00Z".into()),
                stable_memory_epoch: Some(4),
            },
        };

        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::Memory(MemoryReadEffect::Session {
                session_id: "session-memory".into(),
                result: Box::new(Ok(surface)),
            }),
            &mut bottom_pane,
            &mut widget,
        );

        assert!(!bottom_pane.has_active_view());
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text.contains("Review branch state")
                && text.contains("Finish the review")
                && text.contains("Current Session Snapshot: local current-session artifact")
        ));
    }

    #[tokio::test]
    async fn background_mcp_read_projects_a_worker_result_without_a_modal_wait() {
        let manager = std::sync::Arc::new(tokio::sync::RwLock::new(
            crate::mcp_client::McpClientManager::new(),
        ));
        let body =
            slash_dispatch::execute_mcp_read(manager, slash_dispatch::McpReadAction::Overview)
                .await;
        assert!(body.contains("No MCP servers connected"), "{body}");

        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-mcp");
        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::Mcp(body.clone()),
            &mut bottom_pane,
            &mut widget,
        );

        assert!(!bottom_pane.has_active_view());
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == body
        ));
    }

    #[test]
    fn background_session_journal_reads_open_analysis_view() {
        let event = astra_services::session_journal::JournalEvent::turn(
            Some("session-123456"),
            1,
            Some("gpt-5"),
            "review the branch",
            "The branch is ready.",
            2,
            120,
            30,
            90,
        );

        let mut analysis_pane = BottomPane::new();
        let mut analysis_widget = chat_widget::ChatWidget::new("session-123456");
        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::SessionAnalysis {
                session_id: "session-123456".into(),
                result: Ok(vec![event]),
            },
            &mut analysis_pane,
            &mut analysis_widget,
        );
        assert!(analysis_pane.has_active_view());
        assert!(matches!(
            analysis_widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == "Opened session analysis · session-"
        ));
    }

    #[test]
    fn background_session_hub_is_bound_to_the_submission_snapshot() {
        let mut state = crate::cli::session::session_state::SessionState {
            session_id: Some("session-before".into()),
            model: Some("model-before".into()),
            turn: 7,
            total_prompt_tokens: 101,
            total_completion_tokens: 22,
            ..Default::default()
        };
        let snapshot = slash_dispatch::session_hub_snapshot(&state);
        state.session_id = Some("session-after".into());
        state.model = Some("model-after".into());

        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-after");
        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::SessionHub {
                snapshot: Box::new(snapshot),
                workspace: Box::new(Ok(None)),
            },
            &mut bottom_pane,
            &mut widget,
        );

        let rendered = render_bottom_pane_text(&bottom_pane, 120, 24);
        assert!(rendered.contains("session-before"), "{rendered}");
        assert!(rendered.contains("model-before"), "{rendered}");
        assert!(!rendered.contains("session-after"), "{rendered}");
        assert!(!rendered.contains("model-after"), "{rendered}");
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == "Opened session overview"
        ));
    }

    #[test]
    fn background_reflection_keeps_structured_server_evidence_until_view_projection() {
        let body = serde_json::json!({
            "schema_version": 1,
            "tool": "reflect",
            "session_id": "session-reflect",
            "analysis_view": "runtime_errors",
            "topic": "runtime",
            "facet": "errors",
            "depth": "diagnostic",
            "horizon": "session",
            "source_policy": "auto",
            "include_context": false,
            "data_coverage": {
                "overall": "fresh",
                "source": "server_db",
                "events": 3,
                "decisions": 1
            },
            "summary": "The server observed a recoverable timeout.",
            "observations": [],
            "evidence": [],
            "action_hints": [],
            "failure_clusters": []
        })
        .to_string();
        let mut bottom_pane = BottomPane::new();
        let mut widget = chat_widget::ChatWidget::new("session-reflect");

        apply_slash_background_read_effect(
            SlashBackgroundReadEffect::Reflection(Ok(
                crate::cli::slash::slash_state::ReflectSurface::Report {
                    session_id: "session-reflect".into(),
                    source: crate::cli::slash::slash_state::ReflectEvidenceSource::Server,
                    body,
                },
            )),
            &mut bottom_pane,
            &mut widget,
        );

        assert!(bottom_pane.has_active_view());
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Response,
                text,
                ..
            }) if text == "Opened session reflection"
        ));
    }

    #[test]
    fn submission_projection_separates_local_actions_from_user_turns() {
        let mut widget = chat_widget::ChatWidget::new(String::new());

        commit_submission_projection(&mut widget, "/allow auto");
        commit_submission_projection(&mut widget, "/plan ship the workbench");
        commit_submission_projection(&mut widget, "explain the result");

        let events = widget
            .history()
            .iter()
            .filter_map(|cell| cell.to_persist())
            .collect::<Vec<_>>();
        assert!(matches!(
            &events[0],
            crate::tui::turn_event::TurnEvent::System {
                level: crate::tui::turn_event::SystemLevel::Action,
                text,
                ..
            } if text == "/allow auto"
        ));
        assert!(matches!(
            &events[1],
            crate::tui::turn_event::TurnEvent::User { text, .. }
                if text == "ship the workbench"
        ));
        assert!(matches!(
            &events[2],
            crate::tui::turn_event::TurnEvent::User { text, .. }
                if text == "explain the result"
        ));
    }

    #[test]
    fn deferred_slash_dispatch_keeps_user_cell_pending() {
        assert!(!should_flush_after_slash_dispatch(
            &slash_dispatch::SlashResult::Deferred
        ));
        assert!(should_flush_after_slash_dispatch(
            &slash_dispatch::SlashResult::Handled
        ));
        assert!(
            !should_flush_after_slash_dispatch(&slash_dispatch::SlashResult::OpenRootTranscript {
                session_id: None
            }),
            "opening a workspace must not mutate native scrollback before the next frame"
        );
    }

    #[test]
    fn explicit_session_history_target_wins_over_the_active_session() {
        assert_eq!(
            transcript_session_id(
                Some("session-requested".into()),
                Some("session-current".into())
            )
            .as_deref(),
            Some("session-requested")
        );
        assert_eq!(
            transcript_session_id(None, Some("session-current".into())).as_deref(),
            Some("session-current")
        );
    }

    #[test]
    fn ambient_flush_waits_while_deferred_slash_pair_is_pending() {
        assert!(!should_flush_ambient_commits(true));
        assert!(should_flush_ambient_commits(false));
    }

    #[test]
    fn total_input_tokens_includes_cache_buckets_exactly_once() {
        assert_eq!(total_input_tokens(1200, 800, 100), 2100);
        assert_eq!(total_input_tokens(0, 5000, 0), 5000);
    }

    #[test]
    fn non_deferred_slash_dispatch_clears_pending_pair_state() {
        assert!(!next_pending_deferred_slash_flush(
            &slash_dispatch::SlashResult::Handled
        ));
        assert!(next_pending_deferred_slash_flush(
            &slash_dispatch::SlashResult::Deferred
        ));
    }

    #[test]
    fn render_history_batch_lines_keeps_user_card_owned_spacing() {
        let user = history_cell::user::UserCell::new("hi");
        let user_rows = user.display_lines(80).len();
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> = vec![Arc::new(user)];
        let lines = render_history_batch_lines(&cells, 80);

        let bottom_breathing_row = lines.last().expect("user card owns a bottom breathing row");
        assert_eq!(
            bottom_breathing_row.style.bg,
            crate::tui::style::user_message_style().bg,
            "the user card's bottom breathing row is part of the card, not a detached separator"
        );
        assert_eq!(
            bottom_breathing_row.width(),
            0,
            "card ownership must be semantic metadata, not viewport-width space padding"
        );
        assert_eq!(
            lines.len(),
            user_rows,
            "batch rendering must not insert a second blank row after a user card"
        );
    }

    #[test]
    fn render_history_batch_lines_gives_tool_blocks_more_air() {
        let mut tool = history_cell::tool::ToolCell::new_running("bash", "ls /tmp");
        tool.complete(
            "completed",
            42,
            String::new(),
            Some("3 entries".into()),
            None,
        );
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> = vec![Arc::new(tool)];
        let lines = render_history_batch_lines(&cells, 80);

        let blank_count = lines
            .iter()
            .rev()
            .take_while(|line| line.spans.is_empty())
            .count();
        assert_eq!(blank_count, 1, "tool blocks should end with one blank row");
    }

    #[test]
    fn render_history_batch_lines_keeps_local_action_readable() {
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> =
            vec![Arc::new(history_cell::system::SystemCell::action("/allow"))];
        let lines = render_history_batch_lines(&cells, 80);

        assert_eq!(
            lines
                .iter()
                .rev()
                .take_while(|line| line.spans.iter().all(|span| span.content.is_empty()))
                .count(),
            1,
            "slash command should keep one trailing blank row"
        );
    }

    #[test]
    fn render_history_batch_lines_gives_action_result_pair_one_breath() {
        let slash = history_cell::system::SystemCell::action("/allow");
        let slash_rows = slash.display_lines(80).len();
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> = vec![
            Arc::new(slash),
            Arc::new(history_cell::system::SystemCell::response("Mode → Auto")),
        ];
        let lines = render_history_batch_lines(&cells, 80);

        assert_eq!(
            lines
                .iter()
                .rev()
                .take_while(|line| line.spans.iter().all(|span| span.content.is_empty()))
                .count(),
            1,
            "slash command and response should end with one blank row"
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(
            rendered.iter().any(|line| line.contains("/allow")),
            "slash command row present"
        );
        let response_idx = rendered
            .iter()
            .position(|line| line.contains("Mode → Auto"))
            .expect("response row present");
        assert_eq!(
            response_idx, slash_rows,
            "slash response should start right after the slash card's own breathing room"
        );
    }

    #[test]
    fn transcript_view_lines_include_active_cell_output() {
        let mut w = chat_widget::ChatWidget::new("");
        w.handle_event(chat_widget::AppEvent::User(chat_widget::UserEvent::Submit(
            "review".into(),
        )));
        w.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AnswerDelta("still working".into()),
        ));

        let rendered = rendered_transcript_overlay(&w, 80);

        assert!(
            rendered.contains("still working"),
            "active assistant output should be visible in transcript overlay: {rendered:?}"
        );
    }

    #[test]
    fn open_transcript_keeps_a_long_live_reply_to_its_visible_tail() {
        let mut w = chat_widget::ChatWidget::new("");
        w.handle_event(chat_widget::AppEvent::wire(
            chat_widget::WireEvent::AnswerDelta(
                (0..2_000)
                    .map(|index| format!("live-line-{index}\n"))
                    .collect(),
            ),
        ));

        let rendered = rendered_transcript_overlay(&w, 80);

        assert!(rendered.contains("live-line-1999"), "{rendered:?}");
        assert!(
            !rendered.contains("live-line-0\n"),
            "the transcript live suffix must not materialize the full reply: {rendered:?}"
        );
    }

    #[test]
    fn render_explain_dag_formats_rounds_cache_and_batches() {
        let mut trace = ContextAssemblyTrace {
            turn_id: "turn-2".into(),
            session_id: "sess-1".into(),
            ..Default::default()
        };
        trace.system_prompt.total_tokens = 3943;
        trace.token_budget.total_used = 7658;
        trace.token_budget.max_tokens = 160_000;
        trace.token_budget.history_tokens = 7;
        trace.token_budget.tool_schema_tokens = 3708;
        trace
            .history
            .turns_retained
            .push(astra_turn_core::context_assembly_trace::TurnRetention {
                turn_index: 0,
                role: "assistant".into(),
                tokens: 7,
                has_tool_calls: false,
                content_preview: String::new(),
            });
        trace.memory.candidates_considered = 5;
        trace.memory.retrieval_latency_ms = 51;
        trace.tools.tools_available = 27;
        trace
            .tools
            .visible_tools
            .push(astra_turn_core::context_assembly_trace::VisibleTool {
                tool_name: "bash".into(),
                tokens: 243,
            });
        trace
            .tools
            .visible_tools
            .push(astra_turn_core::context_assembly_trace::VisibleTool {
                tool_name: "read_file".into(),
                tokens: 128,
            });
        let mut turn_event = astra_services::session_journal::JournalEvent::turn(
            Some("sess-1"),
            2,
            Some("gpt-5"),
            "hi",
            "done",
            2,
            10_023,
            32,
            2_930,
        )
        .with_cache_tokens(900, 200)
        .with_tool_calls(vec![
            ToolCallRecord {
                tool_call_id: Some("call-1".into()),
                name: "bash".into(),
                ok: true,
                ms: 3000,
                batch_id: Some("parallel-1".into()),
                parallel: Some(true),
                round: Some(0),
                start_offset_ms: Some(40),
                args_preview: Some("{\"command\":\"git status\"}".into()),
                ..Default::default()
            },
            ToolCallRecord {
                tool_call_id: Some("call-2".into()),
                name: "read_file".into(),
                ok: true,
                ms: 48,
                batch_id: Some("parallel-1".into()),
                parallel: Some(true),
                round: Some(0),
                file_path: Some("README.md".into()),
                ..Default::default()
            },
        ]);
        turn_event.ttft_ms = Some(1900);
        turn_event.context_ms = Some(88);
        turn_event.memoria_ms = Some(51);
        turn_event.total_llm_ms = Some(2930);
        turn_event.total_tool_ms = Some(3048);
        turn_event.llm_rounds = Some(1);
        let explain_items = vec![serde_json::json!({
            "total_ms": 2930,
            "prompt_tokens": 10023,
            "completion_tokens": 32,
            "steps": [{
                "step": "llm",
                "duration_ms": 2930,
                "in": 10023,
                "cached_in": 900,
                "cache_write": 200,
                "out": 32,
                "tool_calls": 2
            }],
            "routing": {
                "intent": "default",
                "confidence": 0.0,
                "tier": 0,
                "skipped": false,
                "reason": ""
            }
        })];

        let meta = ExplainTurnMeta::from_journal_event(&turn_event);
        let text =
            render_explain_dag(Some(&trace), Some(&meta), &explain_items, false).expect("text");
        assert!(text.contains("Explain Analyze DAG — turn-2"));
        assert!(text.contains("context_assembly ms=88ms budget=7658/160000 (4.8%)"));
        assert!(text.contains(
            "llm ms=2.9s fresh_in=10023 cache_read=900 cache_write=200 out=32 tool_calls=2"
        ));
        assert!(text.contains("batch[parallel-1] parallel tools=2"));
        assert!(text.contains("bash ok ms=3.0s offset=40ms id=call-1"));
        assert!(text.contains("read_file ok ms=48ms id=call-2 path=README.md"));
    }

    #[test]
    fn commit_explain_dag_commits_trace_to_history() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.explain = crate::cli::session::session_state::ExplainMode::On;
        state.turn = 9;
        state.latest_context_assembly_trace = Some(ContextAssemblyTrace {
            turn_id: "turn-9".into(),
            session_id: "sid-trace".into(),
            token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                total_used: 1024,
                max_tokens: 4096,
                ..Default::default()
            },
            ..Default::default()
        });
        state.last_turn_event = Some(astra_services::session_journal::JournalEvent::turn(
            Some("sid-trace"),
            9,
            Some("gpt-5"),
            "hi",
            "hello",
            0,
            12,
            8,
            1200,
        ));
        let mut widget = chat_widget::ChatWidget::new("");

        assert!(commit_explain_dag(&state, &[], None, 0, &mut widget));

        let sys = widget
            .history()
            .last()
            .and_then(|cell| {
                cell.as_any_ref()
                    .downcast_ref::<history_cell::system::SystemCell>()
            })
            .expect("expected a committed system cell");
        assert!(sys.message().contains("Explain Analyze DAG — turn-9"));
    }

    #[test]
    fn commit_explain_dag_skips_unchanged_cached_trace() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.explain = crate::cli::session::session_state::ExplainMode::On;
        state.latest_context_assembly_trace = Some(ContextAssemblyTrace {
            turn_id: "turn-9".into(),
            session_id: "sid-trace".into(),
            ..Default::default()
        });
        let mut widget = chat_widget::ChatWidget::new("");

        assert!(!commit_explain_dag(
            &state,
            &[],
            Some("turn-9"),
            0,
            &mut widget,
        ));
        assert!(widget.history().is_empty());
    }

    #[test]
    fn commit_explain_dag_preserves_unknown_cache_write_marker() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.explain = crate::cli::session::session_state::ExplainMode::On;
        state.turn = 4;
        state.last_turn_event = Some(astra_services::session_journal::JournalEvent::turn(
            Some("sid-trace"),
            4,
            Some("gpt-5"),
            "hi",
            "hello",
            0,
            12,
            8,
            1200,
        ));
        state
            .last_turn_event
            .as_mut()
            .expect("turn event")
            .cache_read_tokens = Some(144);
        let mut widget = chat_widget::ChatWidget::new("");

        assert!(commit_explain_dag(&state, &[], None, 0, &mut widget));

        let sys = widget
            .history()
            .last()
            .and_then(|cell| {
                cell.as_any_ref()
                    .downcast_ref::<history_cell::system::SystemCell>()
            })
            .expect("expected a committed system cell");
        assert!(sys.message().contains("cache_write=?"));
    }

    #[test]
    fn background_task_event_system_message_uses_typed_shell_vocabulary() {
        let completed = background_task_event_system_message(&BgTaskEvent::Completed {
            id: "bg-shell-1".to_string(),
            title: "cargo test -p astra-cli".to_string(),
            exit_code: Some(0),
            summary: "ok".to_string(),
        })
        .expect("completed should notify");
        assert!(
            completed.contains("Background shell \"cargo test -p astra-cli\" completed (exit 0)"),
            "{completed}"
        );
        assert!(!completed.contains("Background command"));

        let stalled = background_task_event_system_message(&BgTaskEvent::NoRecentOutput {
            id: "bg-shell-2".to_string(),
            title: "python script.py".to_string(),
            inactive_ms: 47_000,
            last_output_tail: "still processing".to_string(),
        });
        assert!(
            stalled.is_none(),
            "quiet-work advisory must not pollute chat history"
        );

        let killed = background_task_event_system_message(&BgTaskEvent::Killed {
            id: "bg-shell-3".to_string(),
            title: "deploy.sh".to_string(),
        })
        .expect("killed should notify");
        assert!(killed.contains("stopped"), "{killed}");
        assert!(killed.contains("\"deploy.sh\""), "{killed}");
        assert!(!killed.contains("killed"), "{killed}");
    }

    #[test]
    fn background_task_output_system_message_includes_title_offsets_and_lines() {
        let message = format_background_task_output_system_message(
            "bg-shell-1",
            "npm run dev",
            "running",
            8192,
            13_244,
            312,
            "Listening on http://localhost:5173/\n",
        );

        assert!(
            message.contains("Read shell output bg-shell-1"),
            "{message}"
        );
        assert!(message.contains("\"npm run dev\""), "{message}");
        assert!(message.contains("1 new line"), "{message}");
        assert!(message.contains("offset 8192 -> 13244"), "{message}");
        assert!(message.contains("total 13244 bytes"), "{message}");
        assert!(message.contains("312 total lines"), "{message}");
        assert!(message.contains("still running"), "{message}");
        assert!(message.contains("Output chunk:"), "{message}");
        assert!(
            message.contains("Listening on http://localhost:5173/"),
            "{message}"
        );
        assert!(
            !message.contains("Background shell bg-shell-1 output"),
            "{message}"
        );
    }

    #[test]
    fn background_task_output_system_message_names_terminal_empty_output() {
        let message = format_background_task_output_system_message(
            "bg-shell-2",
            "cargo test -p astra-cli",
            "completed",
            0,
            0,
            0,
            "",
        );

        assert!(
            message.contains("Read shell output bg-shell-2"),
            "{message}"
        );
        assert!(message.contains("Completed with no output"), "{message}");
        assert!(message.contains("offset 0 -> 0"), "{message}");
        assert!(!message.contains("No output captured yet"), "{message}");
    }

    #[test]
    fn background_task_stop_terminal_race_is_not_reported_as_failure() {
        let error = BackgroundTaskError::AlreadyTerminated {
            task_id: "bg-shell-1".into(),
        };
        let message = format_background_task_stop_error_system_message(&error);

        assert_eq!(message, "Background task bg-shell-1 already finished.");
    }

    #[test]
    fn background_task_stop_stale_handle_is_not_reported_as_generic_failure() {
        let message =
            format_background_task_stop_error_system_message(&BackgroundTaskError::StaleHandle {
                task_id: "bg-shell-1".into(),
            });

        assert_eq!(
            message,
            "Background task bg-shell-1 cannot be stopped because it was restored from a previous session and no live process handle is available."
        );
    }

    #[test]
    fn background_task_output_read_unknown_id_uses_typed_not_found() {
        let message = format_background_task_output_read_error(&BackgroundTaskError::NotFound {
            task_id: "bg-shell-missing".into(),
        });

        assert_eq!(message, "Background task not found: bg-shell-missing");
    }

    #[test]
    fn background_task_stop_unknown_id_uses_typed_not_found() {
        let message =
            format_background_task_stop_error_system_message(&BackgroundTaskError::NotFound {
                task_id: "bg-shell-missing".into(),
            });

        assert_eq!(message, "Background task not found: bg-shell-missing");
    }

    #[test]
    fn background_task_event_system_messages_collapses_many_successes() {
        let messages = background_task_event_system_messages(&[
            BgTaskEvent::Completed {
                id: "bg-shell-1".to_string(),
                title: "cmd one".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
            BgTaskEvent::Completed {
                id: "bg-shell-2".to_string(),
                title: "cmd two".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
        ]);

        assert_eq!(messages, vec!["2 background shells completed".to_string()]);
    }

    #[test]
    fn background_task_event_system_messages_keeps_attention_events_explicit() {
        let messages = background_task_event_system_messages(&[
            BgTaskEvent::Completed {
                id: "bg-shell-1".to_string(),
                title: "cmd one".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
            BgTaskEvent::Failed {
                id: "bg-shell-2".to_string(),
                title: "npm test".to_string(),
                error: "exit 2".to_string(),
            },
            BgTaskEvent::Completed {
                id: "bg-shell-3".to_string(),
                title: "cmd three".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
            BgTaskEvent::Killed {
                id: "bg-shell-4".to_string(),
                title: "deploy.sh".to_string(),
            },
            BgTaskEvent::NoRecentOutput {
                id: "bg-shell-5".to_string(),
                title: "python script.py".to_string(),
                inactive_ms: 45_000,
                last_output_tail: "still processing".to_string(),
            },
        ]);

        assert_eq!(messages[0], "2 background shells completed");
        assert!(messages[1].contains("\"npm test\" failed"), "{messages:?}");
        assert!(
            messages[2].contains("\"deploy.sh\" was stopped"),
            "{messages:?}"
        );
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn background_task_event_system_messages_does_not_collapse_unknown_or_nonzero_exit() {
        let messages = background_task_event_system_messages(&[
            BgTaskEvent::Completed {
                id: "bg-shell-1".to_string(),
                title: "false".to_string(),
                exit_code: Some(1),
                summary: "exit 1".to_string(),
            },
            BgTaskEvent::Completed {
                id: "bg-shell-2".to_string(),
                title: "".to_string(),
                exit_code: None,
                summary: "unknown exit".to_string(),
            },
        ]);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"false\" completed"), "{messages:?}");
        assert!(messages[1].contains("bg-shell-2 completed"), "{messages:?}");
        assert!(
            messages
                .iter()
                .all(|msg| !msg.contains("background shells completed")),
            "{messages:?}"
        );
    }
}
