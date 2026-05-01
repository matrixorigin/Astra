//! TUI-native slash command dispatch.
//!
//! Each command is handled inline without leaving the TUI. Commands that
//! need complex interactive UI push a BottomPaneView. Commands that only
//! produce output render to scrollback. Unrecognized or complex commands
//! fall back to `with_restored()` which temporarily exits the TUI.

use crate::command_registry;
use crate::repl_state::ReplState;
use crate::tui::bottom_pane::BottomPane;
use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};
use crate::tui::chat_cell::system_cell::SystemChatCell;
use crate::tui::chat_cell::ChatCell;
use crate::tui::terminal::TerminalGuard;

pub(crate) enum SlashResult {
    Handled,
    Exit,
    Fallback,
}

/// Context needed by slash dispatch — avoids passing 8+ loose arguments.
pub(crate) struct DispatchContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub profile: Option<&'a str>,
    pub state: &'a mut ReplState,
    pub guard: &'a mut TerminalGuard,
    pub bottom_pane: &'a mut BottomPane,
    pub width: u16,
}

impl<'a> DispatchContext<'a> {
    fn show_info(&mut self, msg: String) {
        let cell = SystemChatCell::info(msg);
        self.guard.queue_history_lines(cell.display_lines(self.width));
    }

    fn show_error(&mut self, msg: String) {
        let cell = SystemChatCell::error(msg);
        self.guard.queue_history_lines(cell.display_lines(self.width));
    }
}

/// Parse and dispatch a slash command. Returns how the caller should proceed.
pub(crate) async fn dispatch(text: &str, ctx: &mut DispatchContext<'_>) -> SlashResult {
    let (cmd, args) = parse_slash(text);

    // Resolve command name via registry (handles prefix matching)
    let resolved = match command_registry::resolve_command(cmd) {
        Ok(name) => name,
        Err(candidates) => {
            if candidates.is_empty() {
                ctx.show_error(format!("Unknown command: {cmd}"));
            } else {
                ctx.show_error(format!(
                    "Ambiguous command: {cmd} (did you mean: {}?)",
                    candidates.join(", ")
                ));
            }
            return SlashResult::Handled;
        }
    };

    match resolved {
        // ── Exit ────────────────────────────────────────────────────
        "/exit" | "/quit" => SlashResult::Exit,

        // ── Help ────────────────────────────────────────────────────
        "/help" | "/commands" => {
            let lines = crate::repl_ui::format_help_lines();
            ctx.guard.queue_history_lines(
                lines.into_iter().map(|s| {
                    ratatui::text::Line::from(ratatui::text::Span::styled(
                        s,
                        ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
                    ))
                }).collect(),
            );
            SlashResult::Handled
        }

        // ── Model selector ──────────────────────────────────────────
        "/model" => {
            if !args.is_empty() {
                // /model <name> — set directly
                ctx.state.model = Some(args.to_string());
                ctx.bottom_pane.footer.model = Some(args.to_string());
                ctx.show_info(format!("Model set to: {args}"));
                return SlashResult::Handled;
            }
            let token = crate::repl_runtime::current_access_token(ctx.profile);
            match crate::slash_router::fetch_model_list(ctx.api, token.as_deref()).await {
                Ok(models) => {
                    let current = ctx.state.model.clone().unwrap_or_default();
                    let items: Vec<SelectionItem> = models.into_iter().map(|m| {
                        let is_current = m == current;
                        SelectionItem { name: m, description: None, is_current }
                    }).collect();
                    if items.is_empty() {
                        ctx.show_info("No models available".into());
                    } else {
                        let view = ListSelectionView::new(items, Some("Select model:".into()));
                        ctx.bottom_pane.push_view(Box::new(view));
                    }
                }
                Err(e) => ctx.show_error(format!("Failed to fetch models: {e}")),
            }
            SlashResult::Handled
        }

        // ── Stats ───────────────────────────────────────────────────
        "/stats" | "/info" => {
            let info = format!(
                "Session: {} | Model: {} | Tokens: {}↑ {}↓ | Cost: ${:.4}",
                ctx.state.session_id.as_deref().unwrap_or("<none>"),
                ctx.state.model.as_deref().unwrap_or("<unset>"),
                ctx.state.total_prompt_tokens,
                ctx.state.total_completion_tokens,
                ctx.state.total_session_cost,
            );
            ctx.show_info(info);
            SlashResult::Handled
        }

        // ── Skills ──────────────────────────────────────────────────
        "/skill" | "/skills" => {
            let skill_count = ctx.state.unified_skill_registry.all_manifests()
                .iter().filter(|m| m.user_invocable).count();
            let items = vec![
                SelectionItem {
                    name: "List skills".into(),
                    description: Some(format!("Tip: press $ to open this list directly. ({skill_count} skills)")),
                    is_current: false,
                },
                SelectionItem {
                    name: "Skill info".into(),
                    description: Some("Show details of a specific skill".into()),
                    is_current: false,
                },
            ];
            let view = ListSelectionView::new(items, Some("Skills — choose an action:".into()));
            ctx.bottom_pane.push_view(Box::new(view));
            SlashResult::Handled
        }

        // ── Allow/Deny (permission mode) ────────────────────────────
        "/allow" => {
            if !args.is_empty() {
                return SlashResult::Fallback; // /allow auto|prompt|deny needs slash_router
            }
            let mode = ctx.state.perm_manager.mode();
            ctx.show_info(format!("Permission mode: {mode}"));
            SlashResult::Handled
        }

        // ── Everything else → with_restored fallback ────────────────
        _ => SlashResult::Fallback,
    }
}

/// Handle a ViewCompleted result from a BottomPaneView.
pub(crate) fn handle_view_result(
    name: &str,
    state: &mut ReplState,
    guard: &mut TerminalGuard,
    bottom_pane: &mut BottomPane,
) {
    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);

    // Skill menu actions
    if name == "List skills" {
        bottom_pane.composer.set_text("$");
        return;
    }
    if name == "Skill info" {
        let msg = SystemChatCell::info("Use /skill info <name> for details".into());
        guard.queue_history_lines(msg.display_lines(w));
        return;
    }

    // Skill name → insert $name into composer
    if state.unified_skill_registry.get_manifest(name).is_some() {
        bottom_pane.composer.set_text(&format!("${name} "));
        return;
    }

    // Model name → apply
    state.model = Some(name.to_string());
    bottom_pane.footer.model = Some(name.to_string());
    let msg = SystemChatCell::info(format!("Model set to: {name}"));
    guard.queue_history_lines(msg.display_lines(w));
}

fn parse_slash(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(' ') {
        Some(pos) => (&text[..pos], text[pos..].trim()),
        None => (text, ""),
    }
}
