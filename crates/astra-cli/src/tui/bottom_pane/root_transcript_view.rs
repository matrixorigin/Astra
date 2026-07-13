//! Durable root-conversation browser.
//!
//! The root and delegated runs intentionally share the same item projection
//! and `TranscriptView` interaction model. This wrapper only owns the root
//! read scope and its pagination; it does not manufacture a second renderer.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{buffer::Buffer, layout::Rect, text::Line};

use super::agent_transcript_view::durable_transcript_items;
use super::transcript_view::{
    TranscriptItem, TranscriptItemId, TranscriptSnapshot, TranscriptView,
};
use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ConversationTabId,
    ViewActionDisposition, ViewActionRequest, ViewCompletion,
};

#[derive(Debug, Clone)]
pub(crate) enum RootTranscriptUpdate {
    Loaded {
        session_id: String,
        page: astra_thin_client::SessionTranscriptPage,
        replace: bool,
        source: RootTranscriptSource,
    },
    Failed {
        session_id: String,
        message: String,
    },
}

/// The authority that supplied the visible root-conversation page.
///
/// A locally durable page is not silently merged with a server page. It is
/// selected only for an initial read when it has broader conversational
/// coverage than the server's current page, and the view keeps that
/// provenance visible to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootTranscriptSource {
    DurableServer,
    LocalDurableOnly,
    LocalDurableWhileServerCatchesUp,
    LocalDurableWithBroaderHistory,
    LocalDurableWhileServerUnavailable,
}

/// Typed read location for a root transcript page.
///
/// The local canonical journal and the server projection have different cursor
/// domains. A view may initially use the local journal while server ingestion
/// catches up, but pagination stays on the selected source. A refresh starts
/// a new server read at its own initial cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootTranscriptTarget {
    DurableServer,
    LocalDurable,
}

pub(crate) struct RootTranscriptView {
    session_id: String,
    items: Vec<astra_thin_client::SessionTranscriptItem>,
    next_before_seq: Option<i64>,
    has_more: bool,
    loading: bool,
    error: Option<String>,
    source: Option<RootTranscriptSource>,
    /// The current root cell is visible before the canonical transcript page
    /// acknowledges it. This is intentionally a separate suffix: it carries
    /// local provenance rather than pretending to be a durable item or trying
    /// to reconcile equivalent text across process boundaries.
    local_live: Option<LocalLiveItem>,
    /// Pending runtime facts such as tool approvals. They are typed local
    /// evidence, not canonical history, and are replaced from authoritative
    /// in-memory state on every refresh.
    local_context: Vec<TranscriptItem>,
    /// A root turn's terminal edge follows its transcript persistence attempt.
    /// Refresh exactly once when the active suffix settles, so Ctrl+O shows
    /// the newly canonical page without guessing whether equal text means the
    /// same conversation item.
    terminal_refresh_requested: bool,
    /// A transcript sidecar landed while a previous page was loading. Keep a
    /// typed follow-up refresh pending instead of losing the durable update.
    durable_refresh_pending: bool,
    viewport_width: u16,
    transcript: TranscriptView,
    completed: bool,
    pending_action: Option<ViewActionRequest>,
    export_pending: bool,
    export_seen_cursors: std::collections::HashSet<i64>,
}

#[derive(Debug, Clone)]
struct LocalLiveItem {
    item: TranscriptItem,
    settled: bool,
}

impl RootTranscriptView {
    pub(crate) fn loading(session_id: String, viewport_width: u16, terminal_height: u16) -> Self {
        let session_label = short_session_label(&session_id).to_string();
        let mut view = Self {
            session_id,
            items: Vec::new(),
            next_before_seq: None,
            has_more: false,
            loading: true,
            error: None,
            source: None,
            local_live: None,
            local_context: Vec::new(),
            terminal_refresh_requested: false,
            durable_refresh_pending: false,
            viewport_width,
            transcript: TranscriptView::from_snapshot(
                TranscriptSnapshot::default(),
                terminal_height,
                viewport_width,
            )
            .with_title(format!("Main conversation · {session_label} · Transcript")),
            completed: false,
            pending_action: None,
            export_pending: false,
            export_seen_cursors: std::collections::HashSet::new(),
        };
        view.rebuild_transcript();
        view
    }

    fn request_load(&mut self, before_seq: Option<i64>) {
        self.request_load_with_live_suffix(before_seq, before_seq.is_some());
    }

    /// Reload after the local live suffix settled. Unlike an explicit `R`,
    /// this preserves that suffix until a future shared item identity can
    /// reconcile it without treating equal text as proof of equality.
    fn request_terminal_refresh(&mut self) {
        self.request_load_with_live_suffix(None, true);
    }

    fn request_load_with_live_suffix(&mut self, before_seq: Option<i64>, preserve_live: bool) {
        if self.loading {
            return;
        }
        // Explicit refresh means the user has asked to inspect the durable
        // projection again. Drop the local-only suffix instead of silently
        // merging it with a new page without a shared canonical identity.
        if before_seq.is_none() && !preserve_live {
            self.local_live = None;
        }
        let target = if before_seq.is_some()
            && matches!(
                self.source,
                Some(
                    RootTranscriptSource::LocalDurableOnly
                        | RootTranscriptSource::LocalDurableWhileServerCatchesUp
                        | RootTranscriptSource::LocalDurableWithBroaderHistory
                        | RootTranscriptSource::LocalDurableWhileServerUnavailable
                )
            ) {
            RootTranscriptTarget::LocalDurable
        } else {
            RootTranscriptTarget::DurableServer
        };
        self.loading = true;
        self.error = None;
        self.transcript.set_activity_status(Some(
            match (before_seq, target) {
                (Some(_), RootTranscriptTarget::LocalDurable) => {
                    "Loading older local conversation…"
                }
                (Some(_), RootTranscriptTarget::DurableServer) => {
                    "Loading older durable conversation…"
                }
                (None, _) => "Syncing durable conversation…",
            }
            .to_string(),
        ));
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::LoadRootTranscript {
                session_id: self.session_id.clone(),
                transcript_target: target,
                before_seq,
            },
            disposition: ViewActionDisposition::KeepOpen,
        });
        self.rebuild_transcript();
    }

    fn apply_page(
        &mut self,
        page: astra_thin_client::SessionTranscriptPage,
        replace: bool,
        source: RootTranscriptSource,
    ) {
        if replace {
            self.items = page.items;
            self.source = Some(source);
        } else {
            let known = self
                .items
                .iter()
                .map(root_transcript_item_identity)
                .collect::<std::collections::HashSet<_>>();
            let mut older: Vec<_> = page
                .items
                .into_iter()
                .filter(|item| !known.contains(&root_transcript_item_identity(item)))
                .collect();
            older.append(&mut self.items);
            self.items = older;
        }
        self.next_before_seq = page.next_before_seq;
        self.has_more = page.has_more;
        self.loading = false;
        self.error = None;
        self.transcript
            .set_activity_status(match (self.source, self.items.is_empty()) {
                (_, true) => None,
                (Some(RootTranscriptSource::DurableServer) | None, false) => None,
                (Some(RootTranscriptSource::LocalDurableOnly), false) => {
                    Some("Local durable history".into())
                }
                (Some(RootTranscriptSource::LocalDurableWhileServerCatchesUp), false) => Some(
                    "Local durable history · server transcript is still syncing · R refresh".into(),
                ),
                (Some(RootTranscriptSource::LocalDurableWithBroaderHistory), false) => {
                    Some("Local durable history · server page is incomplete · R refresh".into())
                }
                (Some(RootTranscriptSource::LocalDurableWhileServerUnavailable), false) => {
                    Some("Local durable history · server transcript unavailable · R refresh".into())
                }
            });
        self.rebuild_transcript();
        if self.durable_refresh_pending {
            self.durable_refresh_pending = false;
            self.request_terminal_refresh();
        } else if self.export_pending {
            self.continue_export();
        }
    }

    fn request_export(&mut self) {
        self.export_pending = true;
        self.export_seen_cursors.clear();
        if self.loading {
            self.transcript
                .set_activity_status(Some("Preparing complete transcript export…".into()));
            return;
        }
        self.continue_export();
    }

    fn continue_export(&mut self) {
        if self.has_more {
            let Some(cursor) = self.next_before_seq else {
                self.fail_export("Transcript source reported older history without a cursor");
                return;
            };
            if !self.export_seen_cursors.insert(cursor) {
                self.fail_export("Transcript pagination stopped at a repeated cursor");
                return;
            }
            self.request_load(Some(cursor));
            self.transcript
                .set_activity_status(Some("Loading complete conversation for export…".into()));
            return;
        }

        self.export_pending = false;
        let path = transcript_export_path("main", &self.session_id);
        let mut lines = vec![
            "# Astra conversation transcript".to_string(),
            String::new(),
            format!("- Session: {}", self.session_id),
            "- Scope: main conversation".to_string(),
            String::new(),
        ];
        lines.extend(self.transcript.export_plain_lines());
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::ExportTranscript {
                path: path.clone(),
                lines,
            },
            disposition: ViewActionDisposition::KeepOpen,
        });
        self.transcript.set_activity_status(Some(format!(
            "Transcript export queued → {}",
            path.display()
        )));
    }

    fn fail_export(&mut self, message: &str) {
        self.export_pending = false;
        self.export_seen_cursors.clear();
        self.transcript
            .set_activity_status(Some(format!("Export stopped · {message} · R retry")));
    }

    fn rebuild_transcript(&mut self) {
        let mut items = if self.items.is_empty() {
            let message = if self.loading {
                "Loading durable conversation…"
            } else if let Some(error) = self.error.as_deref() {
                error
            } else {
                match self.source {
                    Some(RootTranscriptSource::LocalDurableOnly) => {
                        "No local conversation history exists for this session yet."
                    }
                    Some(RootTranscriptSource::LocalDurableWhileServerCatchesUp)
                    | Some(RootTranscriptSource::LocalDurableWithBroaderHistory)
                    | Some(RootTranscriptSource::LocalDurableWhileServerUnavailable)
                    | Some(RootTranscriptSource::DurableServer)
                    | None => "No canonical conversation items have synced for this session yet.",
                }
            };
            vec![TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(0),
                vec![Line::from(message.to_string())],
                0,
            )]
        } else {
            durable_transcript_items(&self.items)
        };
        if let Some(local_live) = &self.local_live {
            let state = if local_live.settled {
                "Local turn result · awaiting durable reconciliation"
            } else {
                "Live local projection · awaiting durable reconciliation"
            };
            items.push(TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(u64::MAX - 1),
                vec![Line::from(state)],
                0,
            ));
            items.push(local_live.item.clone());
        }
        if !self.local_context.is_empty() {
            items.push(TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(u64::MAX - 2),
                vec![Line::from(
                    "Live runtime context · awaiting user action".to_string(),
                )],
                0,
            ));
            items.extend(self.local_context.iter().cloned());
        }
        self.transcript
            .replace_with(TranscriptSnapshot::new(items), self.viewport_width);
    }

    fn refresh_local_live(&mut self, item: Option<TranscriptItem>) {
        let settled_now = match (item, self.local_live.as_mut()) {
            (Some(item), _) => {
                // A resumed root run may reuse its session identity. Its next
                // completion must be allowed to refresh canonical history.
                self.terminal_refresh_requested = false;
                self.local_live = Some(LocalLiveItem {
                    item,
                    settled: false,
                });
                false
            }
            (None, Some(local_live)) if !local_live.settled => {
                local_live.settled = true;
                true
            }
            (None, Some(_)) | (None, None) => return,
        };
        // Server and local runners attempt canonical transcript persistence
        // before their root turn settles. This typed view action fetches that
        // page asynchronously; if it fails, the visibly labelled local suffix
        // remains intact.
        if settled_now && !self.terminal_refresh_requested && !self.loading {
            self.terminal_refresh_requested = true;
            self.request_terminal_refresh();
        }
        self.rebuild_transcript();
    }

    fn refresh_durable_commit(&mut self, session_id: &str) -> bool {
        if self.session_id != session_id {
            return false;
        }
        if self.loading {
            self.durable_refresh_pending = true;
        } else {
            self.request_terminal_refresh();
        }
        self.rebuild_transcript();
        true
    }
}

fn root_transcript_item_identity(item: &astra_thin_client::SessionTranscriptItem) -> String {
    item.source_event_id
        .as_deref()
        .map(|event_id| format!("event:{event_id}"))
        .unwrap_or_else(|| format!("item:{}", item.item_seq))
}

impl BottomPaneView for RootTranscriptView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.transcript.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.transcript.desired_height(width)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left if self.transcript.is_search_active() => self.transcript.handle_key(key),
            KeyCode::Left if !self.transcript.collapse_current_item() => {
                self.pending_action = Some(ViewActionRequest {
                    action: BottomPaneViewAction::ReturnToConversationNavigator,
                    disposition: ViewActionDisposition::KeepOpen,
                });
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pending_action = Some(ViewActionRequest {
                    action: BottomPaneViewAction::ReturnToConversationNavigator,
                    disposition: ViewActionDisposition::KeepOpen,
                });
            }
            KeyCode::Right => {
                self.transcript.expand_current_item();
            }
            KeyCode::Char('r' | 'R') => self.request_load(None),
            KeyCode::Char('o' | 'O') if self.has_more => self.request_load(self.next_before_seq),
            KeyCode::Char('s' | 'S') => self.request_export(),
            _ => self.transcript.handle_key(key),
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.transcript.cursor_pos(area)
    }

    fn is_complete(&self) -> bool {
        self.completed || self.transcript.is_complete()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.is_complete().then_some(ViewCompletion {
            result: None,
            reopen: None,
        })
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action.take()
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        self.transcript.handle_paste(text)
    }

    fn refresh_root_transcript(&mut self, update: RootTranscriptUpdate) -> bool {
        match update {
            RootTranscriptUpdate::Loaded {
                session_id,
                page,
                replace,
                source,
            } if session_id == self.session_id => self.apply_page(page, replace, source),
            RootTranscriptUpdate::Failed {
                session_id,
                message,
            } if session_id == self.session_id => {
                self.loading = false;
                self.export_pending = false;
                self.error = Some(message);
                let status = match self.source {
                    Some(RootTranscriptSource::LocalDurableWhileServerCatchesUp) => {
                        "Could not sync durable conversation · showing local history · R retry"
                    }
                    Some(RootTranscriptSource::LocalDurableWithBroaderHistory) => {
                        "Could not refresh server transcript · showing broader local history · R retry"
                    }
                    Some(RootTranscriptSource::LocalDurableWhileServerUnavailable) => {
                        "Server transcript unavailable · showing local history · R retry"
                    }
                    Some(RootTranscriptSource::LocalDurableOnly) => {
                        "Could not load local conversation · R retry"
                    }
                    Some(RootTranscriptSource::DurableServer) | None => {
                        "Could not sync durable conversation · R retry"
                    }
                };
                self.transcript
                    .set_activity_status(Some(status.to_string()));
                self.rebuild_transcript();
            }
            _ => return false,
        }
        true
    }

    fn refresh_root_transcript_live(&mut self, item: Option<TranscriptItem>) -> bool {
        self.refresh_local_live(item);
        true
    }

    fn refresh_root_transcript_context(&mut self, items: Vec<TranscriptItem>) -> bool {
        self.local_context = items;
        self.rebuild_transcript();
        true
    }

    fn refresh_root_transcript_committed(&mut self, session_id: &str) -> bool {
        self.refresh_durable_commit(session_id)
    }

    fn is_transcript_view(&self) -> bool {
        true
    }

    fn is_root_transcript_view(&self) -> bool {
        true
    }

    fn durable_root_transcript_session(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    fn conversation_tab_id(&self) -> Option<ConversationTabId> {
        Some(ConversationTabId::Root)
    }

    fn conversation_tab_label(&self) -> Option<String> {
        Some(format!("Main · {}", short_session_label(&self.session_id)))
    }

    fn fit_conversation_workspace(&mut self, terminal_height: u16, width: u16) {
        self.viewport_width = width;
        self.transcript.fit_workspace(terminal_height, width);
    }

    fn hint_keys(&self) -> Option<String> {
        let mut hints = vec!["R refresh"];
        if self.has_more {
            hints.push("O older");
        }
        hints.push("Ctrl+E toggle");
        hints.push("S export");
        hints.push("Ctrl+G conversations");
        hints.push("Shift+←/→ switch");
        hints.push("←/Esc return");
        Some(hints.join(" · "))
    }
}

fn short_session_label(session_id: &str) -> &str {
    &session_id[..session_id.len().min(12)]
}

pub(crate) fn transcript_export_path(scope: &str, identity: &str) -> std::path::PathBuf {
    let safe_identity = identity
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let root = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".astra")
        .join("exports");
    root.join(format!("{scope}-{safe_identity}.md"))
}

#[cfg(test)]
mod tests {
    use super::{
        RootTranscriptSource, RootTranscriptTarget, RootTranscriptUpdate, RootTranscriptView,
        TranscriptItem, TranscriptItemId,
    };
    use crate::tui::bottom_pane::view::BottomPaneView;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::text::Line;

    fn page() -> astra_thin_client::SessionTranscriptPage {
        astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![astra_thin_client::SessionTranscriptItem {
                session_id: "session-1".into(),
                item_seq: 10,
                run_id: Some("root-run-1".into()),
                role: "assistant".into(),
                content: "durable root answer".into(),
                reasoning_status: None,
                reasoning: None,
                tool_calls: Vec::new(),
                tool_result: None,
                evidence: None,
                source_event_id: Some("root-item-10".into()),
                created_at: "2026-07-12T00:00:00Z".into(),
            }],
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        }
    }

    #[test]
    fn root_view_accepts_only_its_session_page() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        assert!(view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        }));
        assert!(!view.refresh_root_transcript(RootTranscriptUpdate::Failed {
            session_id: "other-session".into(),
            message: "wrong session".into(),
        }));

        let mut buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 20));
        view.render(ratatui::layout::Rect::new(0, 0, 80, 20), &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("durable root answer"), "{text}");
        assert!(
            text.contains("Main conversation · session-1 · Transcript"),
            "the transcript must identify the session being inspected: {text}"
        );
        assert_eq!(
            view.conversation_tab_label().as_deref(),
            Some("Main · session-1")
        );
    }

    #[test]
    fn root_refresh_keeps_confirmed_history_and_reports_sync_progress_or_failure() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });
        let area = ratatui::layout::Rect::new(0, 0, 80, 20);

        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let mut syncing = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut syncing);
        let syncing_text = crate::tui::testing::render::buffer_to_string(&syncing);
        assert!(
            syncing_text.contains("durable root answer"),
            "{syncing_text}"
        );
        assert!(
            syncing_text.contains("Syncing durable conversation…"),
            "{syncing_text}"
        );

        view.refresh_root_transcript(RootTranscriptUpdate::Failed {
            session_id: "session-1".into(),
            message: "unreachable internal endpoint with credentials".into(),
        });
        let mut failed = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut failed);
        let failed_text = crate::tui::testing::render::buffer_to_string(&failed);
        assert!(failed_text.contains("durable root answer"), "{failed_text}");
        assert!(
            failed_text.contains("Could not sync durable conversation · R retry"),
            "{failed_text}"
        );
        assert!(!failed_text.contains("credentials"), "{failed_text}");
    }

    #[test]
    fn settled_root_live_suffix_refreshes_canonical_history_once_without_text_reconciliation() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });
        view.refresh_root_transcript_live(Some(TranscriptItem::rendered(
            TranscriptItemId::from_widget_id(44),
            vec![Line::from("live model output")],
            1,
        )));

        let area = ratatui::layout::Rect::new(0, 0, 80, 20);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(
            text.contains("Live local projection · awaiting durable reconciliation"),
            "{text}"
        );
        assert!(text.contains("live model output"), "{text}");

        view.refresh_root_transcript_live(None);
        let mut settled = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut settled);
        let settled_text = crate::tui::testing::render::buffer_to_string(&settled);
        assert!(
            settled_text.contains("Local turn result · awaiting durable reconciliation"),
            "{settled_text}"
        );
        assert!(settled_text.contains("live model output"), "{settled_text}");

        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::LoadRootTranscript {
                    session_id,
                    transcript_target: RootTranscriptTarget::DurableServer,
                    before_seq: None,
                },
                ..
            }) if session_id == "session-1"
        ));

        // An automatic refresh is complete, but repeated no-active-cell
        // updates do not start a polling loop. The unproven local suffix is
        // intentionally still visible rather than removed by text matching.
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });
        view.refresh_root_transcript_live(None);
        assert!(view.take_action_request().is_none());
        let mut after_refresh = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut after_refresh);
        assert!(
            crate::tui::testing::render::buffer_to_string(&after_refresh)
                .contains("live model output")
        );

        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let mut reconciled = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut reconciled);
        let reconciled_text = crate::tui::testing::render::buffer_to_string(&reconciled);
        assert!(
            !reconciled_text.contains("live model output"),
            "explicit reconciliation must not merge a local suffix without canonical identity: {reconciled_text}"
        );
    }

    #[test]
    fn durable_commit_queues_one_follow_up_reload_after_an_in_flight_page_load() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });
        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let _initial_refresh = view.take_action_request().expect("initial refresh request");
        assert!(view.loading);

        assert!(view.refresh_root_transcript_committed("session-1"));
        assert!(view.durable_refresh_pending);
        assert!(!view.refresh_root_transcript_committed("other-session"));

        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::LocalDurableWhileServerCatchesUp,
        });
        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::LoadRootTranscript {
                    session_id,
                    transcript_target: RootTranscriptTarget::DurableServer,
                    before_seq: None,
                },
                ..
            }) if session_id == "session-1"
        ));
    }

    #[test]
    fn left_returns_root_view_to_the_conversation_navigator() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!view.is_complete());
        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::ReturnToConversationNavigator,
                ..
            })
        ));
    }

    #[test]
    fn root_transcript_advertises_detail_and_conversation_navigation() {
        let view = RootTranscriptView::loading("session-1".into(), 80, 24);
        let hints = view.hint_keys().expect("root transcript hints");
        assert!(hints.contains("Ctrl+E toggle"), "{hints}");
        assert!(hints.contains("Ctrl+G conversations"), "{hints}");
    }

    #[test]
    fn refresh_is_a_typed_root_conversation_request() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });

        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));

        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::LoadRootTranscript {
                    session_id,
                    transcript_target: RootTranscriptTarget::DurableServer,
                    before_seq: None,
                },
                ..
            }) if session_id == "session-1"
        ));
    }

    #[test]
    fn older_page_after_local_server_fallback_keeps_the_local_cursor_domain() {
        let mut view = RootTranscriptView::loading("session-1".into(), 80, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::LocalDurableWhileServerCatchesUp,
        });
        view.has_more = true;
        view.next_before_seq = Some(10);

        view.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::LoadRootTranscript {
                    transcript_target: RootTranscriptTarget::LocalDurable,
                    before_seq: Some(10),
                    ..
                },
                ..
            })
        ));

        // Refresh does not reuse a local cursor: it explicitly starts a
        // fresh server reconciliation.
        view.loading = false;
        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::LoadRootTranscript {
                    transcript_target: RootTranscriptTarget::DurableServer,
                    before_seq: None,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn local_root_history_remains_visible_when_the_server_is_unavailable() {
        let mut view = RootTranscriptView::loading("session-1".into(), 100, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::LocalDurableWhileServerUnavailable,
        });

        let area = ratatui::layout::Rect::new(0, 0, 100, 20);
        let mut initial = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut initial);
        let initial = crate::tui::testing::render::buffer_to_string(&initial);
        assert!(initial.contains("durable root answer"), "{initial}");
        assert!(
            initial.contains("Local durable history · server transcript unavailable · R refresh"),
            "{initial}"
        );

        view.refresh_root_transcript(RootTranscriptUpdate::Failed {
            session_id: "session-1".into(),
            message: "network failed".into(),
        });
        let mut failed = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut failed);
        let failed = crate::tui::testing::render::buffer_to_string(&failed);
        assert!(failed.contains("durable root answer"), "{failed}");
        assert!(
            failed.contains("Server transcript unavailable · showing local history · R retry"),
            "{failed}"
        );
    }

    #[test]
    fn local_journal_history_remains_visible_while_server_projection_catches_up() {
        let mut view = RootTranscriptView::loading("session-1".into(), 100, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: page(),
            replace: true,
            source: RootTranscriptSource::LocalDurableWhileServerCatchesUp,
        });

        let area = ratatui::layout::Rect::new(0, 0, 100, 20);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("durable root answer"), "{text}");
        assert!(
            text.contains("Local durable history · server transcript is still syncing · R refresh"),
            "{text}"
        );
    }

    #[test]
    fn export_loads_every_canonical_page_before_emitting_one_file_effect() {
        let mut newest = page();
        newest.has_more = true;
        newest.next_before_seq = Some(10);
        let mut view = RootTranscriptView::loading("session-1".into(), 100, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: newest,
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });

        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: super::BottomPaneViewAction::LoadRootTranscript {
                    before_seq: Some(10),
                    ..
                },
                ..
            })
        ));

        let mut older = page();
        older.items[0].item_seq = 2;
        older.items[0].source_event_id = Some("root-item-2".into());
        older.items[0].content = "old durable question".into();
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: older,
            replace: false,
            source: RootTranscriptSource::DurableServer,
        });

        let Some(super::ViewActionRequest {
            action: super::BottomPaneViewAction::ExportTranscript { path, lines },
            ..
        }) = view.take_action_request()
        else {
            panic!("complete pagination must emit one transcript export");
        };
        let body = lines.join("\n");
        assert!(body.contains("old durable question"), "{body}");
        assert!(body.contains("durable root answer"), "{body}");
        assert!(path.ends_with("main-session-1.md"), "{}", path.display());
    }

    #[test]
    fn export_stops_on_a_repeated_pagination_cursor() {
        let mut newest = page();
        newest.has_more = true;
        newest.next_before_seq = Some(10);
        let mut view = RootTranscriptView::loading("session-1".into(), 100, 24);
        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: newest.clone(),
            replace: true,
            source: RootTranscriptSource::DurableServer,
        });
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        let _ = view.take_action_request().expect("first page request");

        view.refresh_root_transcript(RootTranscriptUpdate::Loaded {
            session_id: "session-1".into(),
            page: newest,
            replace: false,
            source: RootTranscriptSource::DurableServer,
        });

        assert!(view.take_action_request().is_none());
        assert!(!view.export_pending);
        let area = ratatui::layout::Rect::new(0, 0, 100, 20);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(
            text.contains(
                "Export stopped · Transcript pagination stopped at a repeated cursor · R retry"
            ),
            "{text}"
        );
    }
}
