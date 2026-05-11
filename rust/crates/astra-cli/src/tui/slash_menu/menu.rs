//! Pure-logic inline slash-command menu.
//!
//! Supports a two-level completion model:
//!   1. `/` → filter top-level commands.
//!   2. `/cmd <space>` → filter that command's subcommands (from
//!      [`SlashItem::subcommands`]).
//!
//! The mode transition is driven entirely by the buffer text, so
//! callers only need to feed the composer content back via
//! [`SlashMenu::set_filter`].

#![allow(dead_code)]

use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::Atom};

/// A single slash command exposed to the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashItem {
    /// Full command token including leading `/`, e.g. `/help`.
    pub name: &'static str,
    /// One-line description shown beside the name.
    pub description: &'static str,
    /// Optional sub-tokens surfaced once the user types a trailing
    /// space. Matches the `(token, description)` shape in the
    /// command registry so the two stay trivially in sync.
    pub subcommands: &'static [(&'static str, &'static str)],
}

impl SlashItem {
    /// Tests and narrow callsites that don't need subcommands can
    /// skip the field rather than threading an empty slice through
    /// every construction site.
    pub const fn simple(name: &'static str, description: &'static str) -> Self {
        Self {
            name,
            description,
            subcommands: &[],
        }
    }
}

/// Should the menu be open for the given composer buffer?
///
/// Rule: open iff the *first line* starts with `/` (no leading whitespace).
pub(crate) fn is_open_for(buffer: &str) -> bool {
    buffer
        .lines()
        .next()
        .map(|first| first.starts_with('/'))
        .unwrap_or(false)
}

/// Which completion axis the menu is filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Mode {
    /// Filtering top-level commands.  The stored token is the
    /// text after the leading `/`, without its trailing space (if
    /// any).
    Command { token: String },
    /// Filtering subcommands of a known parent command. `parent`
    /// is the full `/cmd` string (with leading `/`); `token` is
    /// the partial subcommand the user has typed so far.
    Subcommand { parent: String, token: String },
}

/// Parse a buffer into the current filter mode. The mode is
/// determined by whether the first line contains whitespace after
/// the command token AND that token resolves to a known command in
/// `items` with a non-empty subcommand list.
fn derive_mode(buffer: &str, items: &[SlashItem]) -> Mode {
    let first = buffer.lines().next().unwrap_or("");
    let body = first.strip_prefix('/').unwrap_or("");
    match body.find(char::is_whitespace) {
        None => Mode::Command {
            token: body.to_ascii_lowercase(),
        },
        Some(sp) => {
            let cmd_tok = &body[..sp];
            let after = body[sp..].trim_start();
            // Parent must exist AND carry subcommands. Otherwise
            // fall back to command-mode filtering so users don't
            // get an empty menu on something like `/unknown foo`.
            let parent = format!("/{cmd_tok}");
            let has_subs = items
                .iter()
                .any(|it| it.name == parent && !it.subcommands.is_empty());
            if has_subs {
                Mode::Subcommand {
                    parent,
                    token: after.split_whitespace().next().unwrap_or("").to_ascii_lowercase(),
                }
            } else {
                // No subcommands → menu collapses to "no matches"
                // for the user's typed token; keep Command-mode
                // so the UX is consistent with free-text args.
                Mode::Command {
                    token: cmd_tok.to_ascii_lowercase(),
                }
            }
        }
    }
}

/// Pure-logic slash command menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashMenu {
    items: Vec<SlashItem>,
    /// Indices of `items` that currently match, ordered by match score
    /// (best first). When the filter is empty this is `0..items.len()`.
    /// Used only in [`Mode::Command`].
    filtered: Vec<usize>,
    /// In [`Mode::Subcommand`], the filtered subcommand list derived
    /// from the parent item's `subcommands` slice.  Each entry holds
    /// `(token, description, full_name)` — full_name is `"/cmd sub"`
    /// so the popup renderer + accept-handler can use it uniformly.
    sub_filtered: Vec<SlashItem>,
    mode: Mode,
    selected: usize,
}

impl SlashMenu {
    pub fn new(items: Vec<SlashItem>) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        Self {
            items,
            filtered,
            sub_filtered: Vec::new(),
            mode: Mode::Command {
                token: String::new(),
            },
            selected: 0,
        }
    }

    /// Update the filter from a composer buffer. Re-scores and re-orders
    /// `filtered`. Clamps `selected` into bounds.  Also detects the
    /// Command → Subcommand transition based on whitespace in the
    /// buffer (see [`derive_mode`]).
    pub fn set_filter(&mut self, buffer: &str) {
        let new_mode = derive_mode(buffer, &self.items);
        // Mode transition resets selection so the user doesn't land
        // on a stale index from the other axis.
        if std::mem::discriminant(&new_mode) != std::mem::discriminant(&self.mode) {
            self.selected = 0;
        }
        self.mode = new_mode;

        match &self.mode {
            Mode::Command { token } => {
                self.sub_filtered.clear();
                if token.is_empty() {
                    self.filtered = (0..self.items.len()).collect();
                    self.clamp_selected();
                    return;
                }
                let scored = score_tokens(
                    token,
                    self.items.iter().map(|it| it.name.trim_start_matches('/')),
                );
                self.filtered = scored;
                self.clamp_selected();
            }
            Mode::Subcommand { parent, token } => {
                self.filtered.clear();
                // Find parent and enumerate its subcommands.
                let subs = self
                    .items
                    .iter()
                    .find(|it| it.name == parent.as_str())
                    .map(|it| it.subcommands)
                    .unwrap_or(&[]);
                let filtered_sub_idxs: Vec<usize> = if token.is_empty() {
                    (0..subs.len()).collect()
                } else {
                    score_tokens(token, subs.iter().map(|(name, _)| *name))
                };
                // Build the synthetic SlashItems the popup renders.
                // Use a leaked static description — the popup only
                // reads `name` + `description` through &str borrows
                // of `'static` elsewhere, but we need owned storage
                // here. Workaround: keep SlashItem `'static`-ish by
                // using Box::leak is overkill; instead we carry
                // a parallel owned-string Vec and expose it via a
                // separate accessor so the popup uses the same
                // name/description columns.
                self.sub_filtered = filtered_sub_idxs
                    .into_iter()
                    .map(|i| {
                        let (name, desc) = subs[i];
                        // Leak the composed full name once — a
                        // subcommand list is tiny (≤ ~20 entries)
                        // and leaking N strings per process is
                        // negligible.  Gives us a `'static str`
                        // name that matches SlashItem's field.
                        let full: &'static str =
                            Box::leak(format!("{parent} {name}").into_boxed_str());
                        SlashItem {
                            name: full,
                            description: desc,
                            subcommands: &[],
                        }
                    })
                    .collect();
                self.clamp_selected();
            }
        }
    }

    /// Ordered, filtered view into items. Empty when no matches.
    /// In subcommand mode this returns the synthetic
    /// `"/cmd sub"` items generated from the parent's subcommand
    /// list so every caller treats them uniformly.
    pub fn matches(&self) -> Vec<&SlashItem> {
        match &self.mode {
            Mode::Command { .. } => self.filtered.iter().map(|&i| &self.items[i]).collect(),
            Mode::Subcommand { .. } => self.sub_filtered.iter().collect(),
        }
    }

    /// `true` when the menu is currently filtering subcommands.
    pub fn is_subcommand_mode(&self) -> bool {
        matches!(self.mode, Mode::Subcommand { .. })
    }

    pub fn move_up(&mut self) {
        let n = self.match_count();
        if n == 0 {
            return;
        }
        self.selected = if self.selected == 0 { n - 1 } else { self.selected - 1 };
    }

    pub fn move_down(&mut self) {
        let n = self.match_count();
        if n == 0 {
            return;
        }
        self.selected = (self.selected + 1) % n;
    }

    /// Index of current selection within `matches()`. `None` when empty.
    pub fn selected(&self) -> Option<usize> {
        if self.match_count() == 0 {
            None
        } else {
            Some(self.selected)
        }
    }

    /// The currently selected item, if any. In subcommand mode this
    /// returns the synthetic `"/cmd sub"` item.
    pub fn selected_item(&self) -> Option<&SlashItem> {
        match &self.mode {
            Mode::Command { .. } => self.filtered.get(self.selected).map(|&i| &self.items[i]),
            Mode::Subcommand { .. } => self.sub_filtered.get(self.selected),
        }
    }

    pub fn len(&self) -> usize {
        self.match_count()
    }

    pub fn is_empty(&self) -> bool {
        self.match_count() == 0
    }

    fn match_count(&self) -> usize {
        match &self.mode {
            Mode::Command { .. } => self.filtered.len(),
            Mode::Subcommand { .. } => self.sub_filtered.len(),
        }
    }

    fn clamp_selected(&mut self) {
        let n = self.match_count();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }
}

fn score_tokens<'a, I>(token: &str, haystacks: I) -> Vec<usize>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = Atom::new(
        token,
        nucleo_matcher::pattern::CaseMatching::Ignore,
        nucleo_matcher::pattern::Normalization::Smart,
        nucleo_matcher::pattern::AtomKind::Fuzzy,
        false,
    );
    let mut scored: Vec<(u16, usize)> = haystacks
        .into_iter()
        .enumerate()
        .filter_map(|(i, hay)| {
            let mut buf = Vec::new();
            let hay_utf32 = Utf32Str::new(hay, &mut buf);
            atom.score(hay_utf32, &mut matcher).map(|s| (s, i))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, i)| i).collect()
}
