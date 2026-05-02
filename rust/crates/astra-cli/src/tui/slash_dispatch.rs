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
            use crate::tui::bottom_pane::help_view::HelpView;
            ctx.bottom_pane.push_view(Box::new(HelpView::new()));
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
        "/stats" => {
            if !args.is_empty() {
                // Direct subcommand: /stats history, /stats tools, etc.
                show_stats_view(args, ctx.state, ctx.guard, ctx.bottom_pane);
                return SlashResult::Handled;
            }
            let items = vec![
                SelectionItem {
                    name: "Session overview".into(),
                    description: Some("Current session: turns, tokens, cost, duration".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "History".into(),
                    description: Some("Stats across recent sessions".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Tools".into(),
                    description: Some("Tool call performance dashboard".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Cost".into(),
                    description: Some("Token cost breakdown".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Health".into(),
                    description: Some("Tool health status".into()),
                    is_current: false,
                },
                SelectionItem {
                    name: "Learn".into(),
                    description: Some("Learning insights: entities, patterns, drift".into()),
                    is_current: false,
                },
            ];
            let view = ListSelectionView::new(items, Some("Stats — choose a view:".into()));
            ctx.bottom_pane.push_view(Box::new(view));
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

    // Stats menu → show inline view
    let stats_sub = match name {
        "Session overview" => Some(""),
        "History" => Some("history"),
        "Tools" => Some("tools"),
        "Cost" => Some("cost"),
        "Health" => Some("health"),
        "Learn" => Some("learn"),
        _ => None,
    };
    if let Some(sub) = stats_sub {
        show_stats_view(sub, state, guard, bottom_pane);
        return;
    }

    // Slash command selected from help → insert into composer
    if name.starts_with('/') {
        bottom_pane.composer.set_text(&format!("{name} "));
        return;
    }

    // Model name → apply
    state.model = Some(name.to_string());
    bottom_pane.footer.model = Some(name.to_string());
    let msg = SystemChatCell::info(format!("Model set to: {name}"));
    guard.queue_history_lines(msg.display_lines(w));
}

fn show_stats_view(
    sub: &str,
    state: &ReplState,
    _guard: &mut TerminalGuard,
    bottom_pane: &mut BottomPane,
) {
    use crate::tui::bottom_pane::info_view::InfoView;
    use astra_services::{session_analytics, session_journal};

    match sub {
        "" | "overview" => {
            let sid = state.session_id.clone().unwrap_or_default();
            let short_sid = sid.get(..8.min(sid.len())).unwrap_or(&sid);
            let mut pairs: Vec<(&str, String)> = vec![
                ("session", short_sid.to_string()),
                ("model", state.model.clone().unwrap_or_else(|| "<unset>".into())),
                ("turns", state.turn.to_string()),
                ("tokens", format!("{}↑ {}↓", state.total_prompt_tokens, state.total_completion_tokens)),
                ("cost", format!("${:.4}", state.total_session_cost)),
            ];
            if !sid.is_empty() {
                if let Ok(events) = session_journal::read_journal(&sid) {
                    let stats = session_analytics::compute_session_stats(&sid, &events);
                    pairs.push(("duration", format!("{:.1}s ({:.0}ms/turn)", stats.total_duration_ms as f64 / 1000.0, stats.avg_duration_ms as f64)));
                    pairs.push(("tool calls", format!("{} ({} failed, {:.0}% err)", stats.total_tool_calls, stats.failed_tool_calls, stats.tool_error_rate * 100.0)));
                    if !stats.unique_tools.is_empty() {
                        pairs.push(("tools used", stats.unique_tools.join(", ")));
                    }
                    if stats.error_count > 0 || stats.stall_count > 0 {
                        pairs.push(("issues", format!("{} errors, {} stalls", stats.error_count, stats.stall_count)));
                    }
                    if stats.checkpoint_count > 0 {
                        pairs.push(("checkpoints", stats.checkpoint_count.to_string()));
                    }
                }
            }
            bottom_pane.push_view(Box::new(InfoView::from_key_value("Session Stats", pairs).with_reopen("/stats")));
        }

        "history" => {
            let sessions = session_journal::list_sessions().unwrap_or_default();
            if sessions.is_empty() {
                bottom_pane.push_view(Box::new(InfoView::from_plain("Session History", vec!["  No sessions found.".into()]).with_reopen("/stats")));
                return;
            }
            let mut lines = Vec::new();
            let recent: Vec<_> = sessions.into_iter().take(10).collect();
            for sid in &recent {
                if let Ok(events) = session_journal::read_journal(sid) {
                    let s = session_analytics::compute_session_stats(sid, &events);
                    let short = &sid[..8.min(sid.len())];
                    let model = s.model.as_deref().unwrap_or("?");
                    lines.push(format!(
                        "  {short}  {:>3} turns  {:>6}+{:<6} tok  {:>3} tools  {model}",
                        s.turn_count, s.total_tokens_in, s.total_tokens_out, s.total_tool_calls,
                    ));
                }
            }
            let agg = {
                let mut all = Vec::new();
                for sid in &recent {
                    if let Ok(events) = session_journal::read_journal(sid) {
                        all.push(session_analytics::compute_session_stats(sid, &events));
                    }
                }
                session_analytics::aggregate_stats(&all)
            };
            lines.push(String::new());
            lines.push(format!(
                "  Summary: {} sessions, {} turns, {}+{} tokens",
                agg.session_count, agg.total_turns, agg.total_tokens_in, agg.total_tokens_out,
            ));
            bottom_pane.push_view(Box::new(InfoView::from_plain("Recent Sessions", lines).with_reopen("/stats")));
        }

        "tools" => {
            let sid = state.session_id.clone().unwrap_or_default();
            if sid.is_empty() {
                bottom_pane.push_view(Box::new(InfoView::from_plain("Tool Performance", vec!["  No active session.".into()]).with_reopen("/stats")));
                return;
            }
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let profiles = session_analytics::compute_tool_profiles(&events);
            if profiles.is_empty() {
                bottom_pane.push_view(Box::new(InfoView::from_plain("Tool Performance", vec!["  No tool calls recorded.".into()]).with_reopen("/stats")));
                return;
            }
            let mut lines = Vec::new();
            lines.push(format!("  {:<20} {:>5} {:>5} {:>7} {:>7} {:>6}", "tool", "calls", "fail", "avg ms", "max ms", "err%"));
            for p in &profiles {
                lines.push(format!(
                    "  {:<20} {:>5} {:>5} {:>7} {:>7} {:>5.0}%",
                    p.name, p.call_count, p.fail_count, p.avg_ms, p.max_ms, p.error_rate * 100.0,
                ));
            }
            let total_calls: u32 = profiles.iter().map(|p| p.call_count).sum();
            let total_ms: u64 = profiles.iter().map(|p| p.total_ms).sum();
            lines.push(String::new());
            lines.push(format!("  {} calls, {:.1}s total tool time", total_calls, total_ms as f64 / 1000.0));
            bottom_pane.push_view(Box::new(InfoView::from_plain("Tool Performance", lines).with_reopen("/stats")));
        }

        "cost" => {
            let pricing = &state.cached_pricing;
            let cost = crate::slash_stats::cost_for_tokens(
                state.total_prompt_tokens, state.total_completion_tokens,
                state.total_cache_read_tokens, state.total_cache_creation_tokens, pricing,
            );
            let mut pairs: Vec<(&str, String)> = vec![
                ("model", state.model.clone().unwrap_or_else(|| "<unset>".into())),
                ("rates", format!("${:.4}/1k prompt, ${:.4}/1k completion", pricing.prompt, pricing.completion)),
                ("prompt", format!("{} ({})", state.total_prompt_tokens, crate::slash_stats::format_cost(state.total_prompt_tokens as f64 * pricing.prompt / 1000.0))),
                ("completion", format!("{} ({})", state.total_completion_tokens, crate::slash_stats::format_cost(state.total_completion_tokens as f64 * pricing.completion / 1000.0))),
            ];
            if state.total_cache_read_tokens > 0 {
                let rate = pricing.cache_read.unwrap_or(pricing.prompt * 0.1);
                pairs.push(("cache read", format!("{} ({})", state.total_cache_read_tokens, crate::slash_stats::format_cost(state.total_cache_read_tokens as f64 * rate / 1000.0))));
            }
            pairs.push(("total", crate::slash_stats::format_cost(cost)));
            if state.turn > 0 {
                pairs.push(("avg/turn", crate::slash_stats::format_cost(cost / state.turn as f64)));
            }
            bottom_pane.push_view(Box::new(InfoView::from_key_value("Session Cost", pairs).with_reopen("/stats")));
        }

        "learn" => {
            let mut pairs: Vec<(&str, String)> = Vec::new();
            if let Some(ref eg) = state.entity_graph {
                if let Ok(g) = eg.lock() {
                    pairs.push(("entities", g.len().to_string()));
                }
            }
            if let Some(ref pl) = state.pattern_library {
                if let Ok(p) = pl.lock() {
                    pairs.push(("patterns", p.len().to_string()));
                }
            }
            pairs.push(("skills tracked", state.skill_quality_tracker.all_entries().len().to_string()));
            if !state.drift_user_corrections.is_empty() {
                pairs.push(("corrections", state.drift_user_corrections.len().to_string()));
            }
            if !state.drift_compressed_turns.is_empty() {
                pairs.push(("compactions", state.drift_compressed_turns.len().to_string()));
            }
            if let Some(ref q) = state.drift_original_query {
                let short: String = q.chars().take(50).collect();
                pairs.push(("original query", short));
            }
            pairs.push(("discovered skills", state.discovered_skills.len().to_string()));
            if pairs.is_empty() {
                pairs.push(("status", "No learning data yet.".into()));
            }
            bottom_pane.push_view(Box::new(InfoView::from_key_value("Learning Insights", pairs).with_reopen("/stats")));
        }

        "health" => {
            let sid = state.session_id.clone().unwrap_or_default();
            if sid.is_empty() {
                bottom_pane.push_view(Box::new(InfoView::from_plain("Tool Health", vec!["  No active session.".into()]).with_reopen("/stats")));
                return;
            }
            let events = session_journal::read_journal(&sid).unwrap_or_default();
            let profiles = session_analytics::compute_tool_profiles(&events);
            let mut lines = Vec::new();
            for p in &profiles {
                let status = if p.fail_count == 0 { "✓" } else { "✗" };
                lines.push(format!(
                    "  {status} {:<20} {}/{} ok  {:.0}% err  avg {}ms",
                    p.name, p.success_count, p.call_count, p.error_rate * 100.0, p.avg_ms,
                ));
                if let Some(ref err) = p.last_error {
                    let short: String = err.chars().take(60).collect();
                    lines.push(format!("    └ {short}"));
                }
            }
            if lines.is_empty() {
                lines.push("  No tool calls recorded.".into());
            }
            bottom_pane.push_view(Box::new(InfoView::from_plain("Tool Health", lines).with_reopen("/stats")));
        }

        _ => {}
    }
}

fn parse_slash(text: &str) -> (&str, &str) {
    let text = text.trim();
    match text.find(' ') {
        Some(pos) => (&text[..pos], text[pos..].trim()),
        None => (text, ""),
    }
}
