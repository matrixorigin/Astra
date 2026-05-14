//! Inline approval prompt rendered above the composer.
//!
//! Visual language mirrors Cursor/Copilot:
//!
//! ```text
//! ⏸ bash wants to run
//!   rm -rf /tmp/scratch
//!   destructive path outside cwd
//!
//! ▸ Accept      Reject    Always   Skip
//!   ← → navigate · Enter confirm · Esc reject
//! ```
//!
//! The focused button uses a reversed pill (accent bg, contrasting fg);
//! other buttons render plain/dim. Footer advertises the four key
//! bindings so no training is required.
//!
//! Lives in `history_cell` but is NOT committed to scrollback — the
//! bottom pane owns its lifetime (one live approval cell at a time,
//! destroyed when the user resolves). The trait membership just keeps
//! the cell API uniform; `to_persist` returns `None` so the cell is
//! never written to the transcript.

use std::any::Any;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::HistoryCell;
use crate::tui::approval::ButtonRow;
use crate::tui::turn_event::TurnEvent;

/// Issue #326 P3 / R1 Major 7: pick the most-alarming colour
/// from the risk tag list. Order is fixed (worst-first) so the
/// badge is stable across renders.
fn highest_risk_color(labels: &[String]) -> Color {
    let critical = ["WritesSensitiveFile", "GitDestructive", "WritesOutsideWorkspace", "CredentialAccess"];
    let high = ["NetworkExfiltration", "SqlDestructive", "MCPUnknownCapability", "WorkspaceUntrusted"];
    let medium = ["WritesOutsidePackage", "SandboxExpansion"];
    if labels.iter().any(|l| critical.contains(&l.as_str())) {
        return Color::Red;
    }
    if labels.iter().any(|l| high.contains(&l.as_str())) {
        return Color::LightRed;
    }
    if labels.iter().any(|l| medium.contains(&l.as_str())) {
        return Color::Yellow;
    }
    // BashExecute and other "vanilla" tags fall through to a
    // softer colour so the screen isn't shouting on every prompt.
    Color::Cyan
}

#[derive(Debug, Clone)]
pub(crate) struct ApprovalCell {
    pub id: u64,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub focused: bool,
    pub buttons: ButtonRow,
    /// Issue #326 P3 / R1 Major 7: risk classification for the
    /// badge line. Each tag becomes a coloured chip in the
    /// header. Empty = no badge displayed.
    pub risk_tag_labels: Vec<String>,
    /// Issue #326 P3: precomputed "Will save" preview. Renders
    /// as a separate line `Will save: <rule>` so the user sees
    /// exactly what permissions.json would gain before pressing
    /// Always.
    pub will_save_preview: Option<String>,
    /// Issue #326 P3 / R1 Major 11 / scenarios #21-#25: agent
    /// that issued the request. Renders as `[agent: <id>]` chip
    /// next to the header.
    pub source_agent: Option<String>,
    /// Issue #326 P3 / scenario #39: remote host label rendered
    /// as `host:` prefix on the detail block.
    pub host: Option<String>,
    /// Issue #326 P5d / R2 Minor 1: post-save confirmation
    /// message. Distinct from `will_save_preview` (the *future*
    /// rule shown before the user clicks Always) — this is the
    /// *past* outcome shown after the save attempt:
    ///
    ///   None        -> not yet attempted (or no Always pressed)
    ///   Some("…")   -> save outcome, e.g.
    ///                  "Saved to .kiro/permissions.json" or
    ///                  "Failed to save rule: <reason>"
    pub save_outcome: Option<String>,
}

impl ApprovalCell {
    pub fn new(
        id: u64,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        focused: bool,
    ) -> Self {
        Self {
            id,
            tool,
            header,
            detail,
            reason,
            focused,
            buttons: ButtonRow::primary(),
            risk_tag_labels: Vec::new(),
            will_save_preview: None,
            source_agent: None,
            host: None,
            save_outcome: None,
        }
    }

    /// Construct with the extended Accept-all / Reject-all buttons
    /// appended. Call when more than one approval is pending.
    #[allow(dead_code)]
    pub fn with_batch(
        id: u64,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        focused: bool,
    ) -> Self {
        Self {
            id,
            tool,
            header,
            detail,
            reason,
            focused,
            buttons: ButtonRow::primary_with_batch(),
            risk_tag_labels: Vec::new(),
            will_save_preview: None,
            source_agent: None,
            host: None,
            save_outcome: None,
        }
    }

    /// Issue #326 P3: builder for the approval card's display
    /// metadata. Threading these through positionally would
    /// churn every call site; a builder keeps the API local to
    /// the few places that compute risk/will_save/agent.
    #[must_use]
    pub fn with_risk_tag_labels(mut self, labels: Vec<String>) -> Self {
        self.risk_tag_labels = labels;
        self
    }

    #[must_use]
    pub fn with_will_save_preview(mut self, preview: impl Into<String>) -> Self {
        self.will_save_preview = Some(preview.into());
        self
    }

    #[must_use]
    pub fn with_source_agent(mut self, agent: impl Into<String>) -> Self {
        self.source_agent = Some(agent.into());
        self
    }

    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    /// Issue #326 P5d / R2 Minor 1: post-save confirmation
    /// line. Use `Saved to …` for success, `Failed to save
    /// rule: …` for failure.
    #[must_use]
    pub fn with_save_outcome(mut self, outcome: impl Into<String>) -> Self {
        self.save_outcome = Some(outcome.into());
        self
    }

    /// Move button focus — only honoured when this cell itself is focused.
    #[allow(dead_code)]
    pub fn move_button_left(&mut self) {
        if self.focused {
            self.buttons.move_left();
        }
    }

    #[allow(dead_code)]
    pub fn move_button_right(&mut self) {
        if self.focused {
            self.buttons.move_right();
        }
    }

    /// Issue #326 P3: returns the human-readable reason the
    /// Always button cannot persist a Project/User rule for
    /// this approval, or None when persistence is allowed.
    /// Mirrors the policy in
    /// [`astra_turn_core::permission_scope::permitted_scopes`]
    /// using only the labels we render in the cell — keeps the
    /// view layer free of the engine's `RiskTag` enum.
    pub fn always_disabled_reason(&self) -> Option<&'static str> {
        // Sub-agent requests can never persist on the parent's
        // permissions.json.
        if self.source_agent.is_some() {
            return Some("sub-agent");
        }
        for label in &self.risk_tag_labels {
            match label.as_str() {
                "WritesSensitiveFile" => return Some("sensitive path"),
                "GitDestructive" => return Some("git destructive"),
                "WritesOutsideWorkspace" => return Some("outside workspace"),
                "CredentialAccess" => return Some("credential access"),
                "MCPUnknownCapability" => return Some("MCP unknown"),
                _ => {}
            }
        }
        None
    }

    fn button_line(&self) -> Line<'static> {
        // Button row lives INSIDE the card, so the caller pads the
        // leading bar. We only emit spans for the buttons + their
        // inter-button spacing here.
        let mut spans: Vec<Span<'static>> = Vec::new();
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(Color::DarkGray);
        let body = if self.focused {
            Style::default().fg(Color::Gray)
        } else {
            dim
        };

        for (i, btn) in self.buttons.buttons().iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" ".to_string(), dim));
            }

            let is_focused = self.focused && i == self.buttons.focus();
            if is_focused {
                // Cursor-style reversed pill with bracket markers so
                // the focused button reads as a target even when the
                // terminal strips background color (screen readers,
                // monochrome, copy-paste).
                let sel_style = Style::default()
                    .bg(theme.accent)
                    .fg(theme.selected_fg)
                    .add_modifier(Modifier::BOLD);
                spans.push(Span::styled(format!(" {} ", btn.label), sel_style));
            } else {
                // Subtle bracket outline so all buttons have the same
                // shape as the focused one — just without the fill.
                spans.push(Span::styled(format!(" {} ", btn.label), body));
            }
        }
        Line::from(spans)
    }
}

impl HistoryCell for ApprovalCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        // Colors come from the active theme so the card remains
        // legible on both dark and light terminals. `accent` is the
        // card's identity color (border + focused pill); `muted` is
        // for detail/reason rows and the unfocused hint.
        let theme = crate::tui::theme::current();
        let accent_style = Style::default().fg(theme.accent);
        let muted = Style::default().fg(Color::DarkGray);
        let body_style = if self.focused {
            Style::default().fg(Color::Gray)
        } else {
            muted
        };

        let mut lines = Vec::new();

        // ── Top border with embedded title ────────────────────────
        //     ╭─ ⏸ bash wants to run ─[agent:foo]─[ssh:host]──
        let mut top_spans = vec![
            Span::styled("╭─ ".to_string(), accent_style),
            Span::styled("⏸ ".to_string(), accent_style),
            Span::styled(
                self.header.clone(),
                accent_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".to_string(), accent_style),
        ];
        if let Some(agent) = &self.source_agent {
            top_spans.push(Span::styled(
                format!("[agent: {agent}] "),
                muted.add_modifier(Modifier::ITALIC),
            ));
        }
        if let Some(host) = &self.host {
            top_spans.push(Span::styled(
                format!("[{host}] "),
                muted.add_modifier(Modifier::ITALIC),
            ));
        }
        lines.push(Line::from(top_spans));

        // Body rows use a vertical accent bar on the left so the
        // card reads as one visual block, not a heap of bullet
        // points. Mirrors Cursor's inline tool cards.
        let bar = Span::styled("│ ".to_string(), accent_style);

        // Risk badge row (issue #326 P3 / R1 Major 7).
        if !self.risk_tag_labels.is_empty() {
            let risk_color = highest_risk_color(&self.risk_tag_labels);
            let risk_style = Style::default()
                .fg(risk_color)
                .add_modifier(Modifier::BOLD);
            let badges = self
                .risk_tag_labels
                .iter()
                .map(|t| format!("⚑ {t}"))
                .collect::<Vec<_>>()
                .join("  ");
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(badges, risk_style),
            ]));
        }

        // Optional detail (first 3 lines — bumped from 2 so a
        // multi-line bash command isn't mystery-truncated).
        if let Some(ref detail) = self.detail {
            for dl in detail.lines().take(3) {
                lines.push(Line::from(vec![
                    bar.clone(),
                    Span::styled(dl.to_string(), body_style),
                ]));
            }
        }

        // Reason — prefixed with a subtle marker so it's clearly
        // not user-command text.
        if !self.reason.is_empty() {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled("⚠ ".to_string(), muted),
                Span::styled(self.reason.clone(), muted),
            ]));
        }

        // Will-save preview (issue #326 P3): users see exactly
        // what permissions.json would gain before pressing
        // Always.
        if let Some(preview) = &self.will_save_preview {
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled("Will save: ".to_string(), muted),
                Span::styled(
                    preview.clone(),
                    body_style.add_modifier(Modifier::BOLD),
                ),
            ]));
        }

        // Save outcome (issue #326 P5d / R2 Minor 1): the
        // post-action confirmation. We use a green-leaning
        // body style for "Saved" and yellow for "Failed".
        if let Some(outcome) = &self.save_outcome {
            let outcome_style = if outcome.starts_with("Failed") {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Green)
            };
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(outcome.clone(), outcome_style),
            ]));
        }

        // Issue #326 P3 / scenarios #6/#9/#15: when the request
        // carries destructive risk tags, advertise that the
        // Always button can't be used to persist a project /
        // user rule. The button row itself stays clickable —
        // the Always-resolve handler upstream still records
        // the override in session-only memory — but the user
        // sees up-front why no on-disk rule landed.
        if self.always_disabled_reason().is_some() {
            let reason_text = self.always_disabled_reason().unwrap_or_default();
            lines.push(Line::from(vec![
                bar.clone(),
                Span::styled(
                    "Always: ".to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    "session-only ".to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("({reason_text})"),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
        }

        // Breathing space inside the card.
        lines.push(Line::from(bar.clone()));

        // Button row (prefixed with bar so alignment is preserved).
        let mut button_row = vec![bar.clone()];
        button_row.extend(self.button_line().spans);
        lines.push(Line::from(button_row));

        // ── Bottom border: hint on the border itself ──────────────
        // Focused: advertise every binding on the border, Cursor-
        // style. Unfocused: plain border close — the user isn't
        // looking at this card for actions yet.
        let bottom = if self.focused {
            Line::from(vec![
                Span::styled("╰─ ".to_string(), accent_style),
                Span::styled(
                    "↑↓←→ select · Enter confirm · Esc reject · Ctrl+Enter quick accept"
                        .to_string(),
                    muted,
                ),
                Span::styled(" ".to_string(), accent_style),
            ])
        } else {
            Line::from(vec![Span::styled("╰─".to_string(), accent_style)])
        };
        lines.push(bottom);

        lines
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    /// Approval cells are ephemeral — they disappear when the user
    /// resolves. Never persisted.
    fn to_persist(&self) -> Option<TurnEvent> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(cell: &ApprovalCell) -> String {
        cell.display_lines(80)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn focused_cell_renders_rounded_box_and_hint() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "bash wants to run".into(),
            Some("rm -rf /tmp/x".into()),
            "destructive path".into(),
            true,
        );
        let rendered = render(&cell);
        assert!(rendered.contains("⏸"), "header glyph missing");
        assert!(rendered.contains("rm -rf /tmp/x"), "detail missing");
        assert!(rendered.contains("destructive path"), "reason missing");
        // Cursor-style rounded box edges.
        assert!(rendered.contains("╭─"), "top border missing");
        assert!(rendered.contains("╰─"), "bottom border missing");
        assert!(
            rendered.contains("│"),
            "left-edge accent bar missing inside the card"
        );
        // Hint is advertised on the bottom border for focused cells,
        // including every key binding the user can reach.
        assert!(
            rendered.contains("↑↓←→ select"),
            "arrow-key hint missing on focused cell"
        );
        assert!(
            rendered.contains("Ctrl+Enter"),
            "Ctrl+Enter shortcut hint missing on focused cell"
        );
    }

    #[test]
    fn unfocused_cell_keeps_border_but_omits_hint() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "bash wants to run".into(),
            None,
            "reason".into(),
            false,
        );
        let rendered = render(&cell);
        // Box edges still render so the pending queue reads as a
        // stack of cards.
        assert!(rendered.contains("╭─"));
        assert!(rendered.contains("╰─"));
        // But the action hint is reserved for the focused cell.
        assert!(
            !rendered.contains("↑↓←→ select"),
            "unfocused cell should not advertise actions"
        );
    }

    #[test]
    fn never_persists() {
        let cell = ApprovalCell::new(1, "t".into(), "h".into(), None, "r".into(), true);
        assert!(cell.to_persist().is_none());
    }

    // ── Issue #326 P3 enrichment: risk badge / will-save / agent / host ──

    #[test]
    fn renders_risk_tag_badge_when_present() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "rm -rf /tmp".into(),
            Some("rm -rf /tmp/scratch".into()),
            "execute".into(),
            true,
        )
        .with_risk_tag_labels(vec!["BashExecute".into(), "WritesOutsidePackage".into()]);
        let rendered = render(&cell);
        assert!(
            rendered.contains("⚑ BashExecute"),
            "risk tag chip should appear, got:\n{rendered}"
        );
        assert!(rendered.contains("⚑ WritesOutsidePackage"));
    }

    #[test]
    fn renders_will_save_preview_when_present() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            true,
        )
        .with_will_save_preview("Bash(npm test:*)");
        let rendered = render(&cell);
        assert!(
            rendered.contains("Will save: Bash(npm test:*)"),
            "expected Will save preview line, got:\n{rendered}"
        );
    }

    #[test]
    fn renders_save_outcome_when_present() {
        // Issue #326 P5d / R2 Minor 1: post-save confirmation
        // line appears below Will save and tells the user the
        // rule actually persisted.
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            true,
        )
        .with_save_outcome("Saved to .kiro/permissions.json");
        let rendered = render(&cell);
        assert!(
            rendered.contains("Saved to .kiro/permissions.json"),
            "save outcome should render verbatim, got:\n{rendered}"
        );
    }

    #[test]
    fn renders_failed_save_outcome_when_present() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            true,
        )
        .with_save_outcome("Failed to save rule: read-only filesystem");
        let rendered = render(&cell);
        assert!(rendered.contains("Failed to save rule"));
    }

    #[test]
    fn renders_source_agent_chip_in_header() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "npm test".into(),
            None,
            "".into(),
            true,
        )
        .with_source_agent("review-subagent");
        let rendered = render(&cell);
        assert!(
            rendered.contains("[agent: review-subagent]"),
            "expected [agent: …] chip, got:\n{rendered}"
        );
    }

    #[test]
    fn renders_remote_host_chip_in_header() {
        let cell = ApprovalCell::new(
            1,
            "edit_file".into(),
            "edit /etc/hosts".into(),
            None,
            "".into(),
            true,
        )
        .with_host("ssh:bastion-prod");
        let rendered = render(&cell);
        assert!(
            rendered.contains("[ssh:bastion-prod]"),
            "expected [host:] chip, got:\n{rendered}"
        );
    }

    #[test]
    fn omits_optional_lines_when_unset() {
        // Default cell has no risk tags / will-save / agent /
        // host, and should not render those lines.
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "h".into(),
            None,
            "r".into(),
            true,
        );
        let rendered = render(&cell);
        assert!(!rendered.contains("⚑ "), "no risk badge expected");
        assert!(!rendered.contains("Will save:"), "no will-save row expected");
        assert!(!rendered.contains("[agent:"), "no agent chip expected");
    }

    #[test]
    fn highest_risk_color_picks_red_for_critical() {
        // Pure unit test on the colour-picker so a future CSS
        // refactor doesn't downgrade catastrophic tags into the
        // "vanilla" shade.
        assert_eq!(
            highest_risk_color(&[
                "BashExecute".into(),
                "WritesSensitiveFile".into(),
            ]),
            Color::Red
        );
        assert_eq!(
            highest_risk_color(&["GitDestructive".into()]),
            Color::Red
        );
        assert_eq!(
            highest_risk_color(&["NetworkExfiltration".into()]),
            Color::LightRed
        );
        assert_eq!(highest_risk_color(&["BashExecute".into()]), Color::Cyan);
    }

    // ── Issue #326 P3 / R2 Major 1: scope-picker policy ──────

    #[test]
    fn always_disabled_for_destructive_risk() {
        let cell = ApprovalCell::new(
            1,
            "edit_file".into(),
            "edit /etc/hosts".into(),
            None,
            "writes outside workspace".into(),
            true,
        )
        .with_risk_tag_labels(vec!["WritesOutsideWorkspace".into()]);
        assert_eq!(cell.always_disabled_reason(), Some("outside workspace"));
    }

    #[test]
    fn always_disabled_for_sensitive_path() {
        let cell = ApprovalCell::new(
            1,
            "write_file".into(),
            "write .env".into(),
            None,
            "sensitive path".into(),
            true,
        )
        .with_risk_tag_labels(vec!["WritesSensitiveFile".into()]);
        assert_eq!(cell.always_disabled_reason(), Some("sensitive path"));
    }

    #[test]
    fn always_disabled_for_sub_agent_request() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            true,
        )
        .with_source_agent("review-subagent");
        assert_eq!(cell.always_disabled_reason(), Some("sub-agent"));
    }

    #[test]
    fn always_enabled_for_benign_request() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            true,
        )
        .with_risk_tag_labels(vec!["BashExecute".into()]);
        assert!(cell.always_disabled_reason().is_none());
    }

    #[test]
    fn always_disabled_renders_in_card() {
        let cell = ApprovalCell::new(
            1,
            "bash".into(),
            "rm -rf /".into(),
            None,
            "execute".into(),
            true,
        )
        .with_risk_tag_labels(vec!["GitDestructive".into()]);
        let rendered = render(&cell);
        assert!(
            rendered.contains("Always: session-only"),
            "destructive cell must advertise the disabled-Always state, got:\n{rendered}"
        );
        assert!(rendered.contains("git destructive"));
    }

    // ── Issue #326 P3 / R1 Major 11: snapshot tests at 3 widths ──

    fn render_at(cell: &ApprovalCell, width: u16) -> String {
        cell.display_lines(width)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fixture_full() -> ApprovalCell {
        ApprovalCell::new(
            42,
            "bash".into(),
            "npm test --filter auth".into(),
            Some("npm test --filter auth -- --verbose".into()),
            "execute (Prompt mode, no allow rule matches)".into(),
            true,
        )
        .with_risk_tag_labels(vec!["BashExecute".into()])
        .with_will_save_preview("Bash(npm test:*)")
    }

    /// Smoke-snapshot the rendered card at three terminal widths
    /// to lock the visual budget plan v3 §P3 promised: usable on
    /// 80x24, comfortable on 100x30, breathing-room on 160x40.
    /// The snapshot is just the textual content (ANSI styling
    /// stripped), so they're cheap and human-readable.
    #[test]
    fn snapshot_full_card_80() {
        insta::assert_snapshot!("approval_card_80", render_at(&fixture_full(), 80));
    }

    #[test]
    fn snapshot_full_card_100() {
        insta::assert_snapshot!("approval_card_100", render_at(&fixture_full(), 100));
    }

    #[test]
    fn snapshot_full_card_160() {
        insta::assert_snapshot!("approval_card_160", render_at(&fixture_full(), 160));
    }

    #[test]
    fn snapshot_card_with_agent_and_host_chips() {
        // Scenarios #21 (sub-agent) + #39 (remote host)
        // co-occurrence: header should fit both chips at 80 cols.
        let cell = fixture_full()
            .with_source_agent("review-subagent")
            .with_host("ssh:bastion-prod");
        insta::assert_snapshot!(
            "approval_card_agent_and_host_80",
            render_at(&cell, 80)
        );
    }

    #[test]
    fn snapshot_destructive_card_disables_persistent_scopes_visually() {
        // Destructive risk + sensitive-path file: the card must
        // make this read as RED-coded.
        let cell = ApprovalCell::new(
            7,
            "edit_file".into(),
            "edit /etc/hosts".into(),
            Some("/etc/hosts".into()),
            "writes outside workspace".into(),
            true,
        )
        .with_risk_tag_labels(vec![
            "WritesOutsideWorkspace".into(),
            "WritesSensitiveFile".into(),
        ]);
        insta::assert_snapshot!("approval_card_destructive_80", render_at(&cell, 80));
    }

    #[test]
    fn budget_card_height_under_8_lines_at_80() {
        // The plan promised the card stays under ~8 lines on the
        // baseline 80x24 terminal so the scrollback isn't
        // squeezed. Lock that as a budget assertion.
        let rendered = render_at(&fixture_full(), 80);
        let lines = rendered.lines().count();
        assert!(
            lines <= 9,
            "approval card grew past budget: {lines} lines for width=80, content:\n{rendered}"
        );
    }
}
