//! Pure-logic inline slash-command menu.
//!
//! Two-level completion:
//!   1. `/` → filter top-level commands (fuzzy, multi-token).
//!   2. `/cmd <space>` → filter that command's subcommands.
//!
//! Scoring features:
//! * Fuzzy scoring via `nucleo-matcher::Atom`.
//! * Per-item `usage_boost` nudges frequently-used commands to the top.
//! * Returns match positions for highlight rendering.

#![allow(dead_code)]

use std::borrow::Cow;

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{Atom, AtomKind, CaseMatching, Normalization},
};

/// A single slash command exposed to the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashItem {
    /// Full command token including leading `/`, e.g. `/help`.
    pub name: Cow<'static, str>,
    /// One-line description shown beside the name.
    pub description: Cow<'static, str>,
    /// Static sub-tokens surfaced once the user types a trailing space.
    /// Matches the `(token, description)` shape in the command registry.
    pub subcommands: &'static [(&'static str, &'static str)],
    /// Dynamic sub-tokens injected at runtime (e.g. MCP server/tool names).
    /// Merged with `subcommands` when the menu is constructed.
    pub extra_subcommands: Vec<(String, String)>,
    /// Frequency hint — higher means "show sooner on ties".
    pub usage_boost: u32,
    /// Included in the curated list shown for a bare `/`.
    pub primary: bool,
    /// Command group for categorized rendering in the popup.
    pub group: Option<crate::cli::command_registry::CommandGroup>,
    /// Example usages shown in help output.
    pub usage_examples: &'static [&'static str],
}

impl Default for SlashItem {
    fn default() -> Self {
        Self {
            name: Cow::Borrowed(""),
            description: Cow::Borrowed(""),
            subcommands: &[],
            extra_subcommands: Vec::new(),
            usage_boost: 0,
            primary: true,
            group: None,
            usage_examples: &[],
        }
    }
}

impl SlashItem {
    /// Minimal constructor for the common case.
    pub const fn simple(name: &'static str, description: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            description: Cow::Borrowed(description),
            subcommands: &[],
            extra_subcommands: Vec::new(),
            usage_boost: 0,
            primary: true,
            group: None,
            usage_examples: &[],
        }
    }
}

/// Should the menu be open for the given composer buffer?
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
    Command { token: String },
    Subcommand { parent: String, token: String },
}

/// Parse a buffer into the current filter mode.
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
            let parent = format!("/{cmd_tok}");
            let has_subs = items.iter().any(|it| {
                it.name == parent
                    && (!it.subcommands.is_empty() || !it.extra_subcommands.is_empty())
            });
            if has_subs {
                Mode::Subcommand {
                    parent,
                    // Keep the full suffix so multi-word subcommand names
                    // like "inspect github:list_prs" can be filtered by
                    // typing "/mcp inspect git".
                    token: after.to_ascii_lowercase(),
                }
            } else {
                Mode::Command {
                    token: cmd_tok.to_ascii_lowercase(),
                }
            }
        }
    }
}

/// Result of scoring one candidate against the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ScoredItem {
    item_idx: usize,
    score: u32,
    name_hit: bool,
    indices: Vec<u32>,
}

/// Public fuzzy-score helper: rank a single `haystack` against a user
/// `needle`. Returns `None` if there is no match, or a score where
/// larger = better.
pub fn score_token(needle: &str, haystack: &str) -> Option<u32> {
    let n = needle.to_ascii_lowercase();
    let h = haystack.to_ascii_lowercase();

    if n.is_empty() {
        return Some(0);
    }

    let short_bonus = 100u32.saturating_sub(h.len().min(100) as u32);

    if h == n {
        return Some(1200 + short_bonus);
    }
    if h.starts_with(&n) {
        return Some(500 + short_bonus);
    }
    if h.contains(&n) {
        return Some(250 + short_bonus);
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let atom = Atom::new(
        &n,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );
    let mut buf = Vec::new();
    let hay = Utf32Str::new(&h, &mut buf);
    atom.score(hay, &mut matcher)
        .map(|s| s as u32 + short_bonus)
}

/// Score a single candidate against the token.
fn score_one(
    matcher: &mut Matcher,
    token: &str,
    item_idx: usize,
    item: &SlashItem,
) -> Option<ScoredItem> {
    let atom = Atom::new(
        token,
        CaseMatching::Ignore,
        Normalization::Smart,
        AtomKind::Fuzzy,
        false,
    );

    let name_lower = item.name.trim_start_matches('/').to_ascii_lowercase();

    // Primary: command name (without '/').
    let mut name_buf = Vec::new();
    let name_haystack = Utf32Str::new(name_lower.as_str(), &mut name_buf);
    let mut name_indices: Vec<u32> = Vec::new();
    let name_score = atom.indices(name_haystack, matcher, &mut name_indices);

    // Secondary: description.
    let desc_lower = item.description.to_ascii_lowercase();
    let mut db = Vec::new();
    let desc_haystack = Utf32Str::new(desc_lower.as_str(), &mut db);
    let desc_score = atom.score(desc_haystack, matcher);

    let name_hit = name_score.is_some();
    let base = match (name_score, desc_score) {
        (Some(ns), _) => ns as u32,
        (None, Some(ds)) => ds as u32 / 2,
        (None, None) => return None,
    };

    let mut bonus = 0u32;
    if name_lower.starts_with(token) {
        bonus += 500;
    }
    if name_lower == token {
        bonus += 200;
    }
    bonus += item.usage_boost.min(150);

    let indices = if name_score.is_some() {
        name_indices.iter().map(|i| i + 1).collect()
    } else {
        Vec::new()
    };

    Some(ScoredItem {
        item_idx,
        score: base + bonus,
        name_hit,
        indices,
    })
}

/// Pure-logic slash command menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashMenu {
    items: Vec<SlashItem>,
    /// Pre-built synthetic `SlashItem`s for each parent's subcommands.
    /// `prebuilt_subs[i]` corresponds to `items[i].subcommands`.
    /// Built once at construction — avoids per-keystroke allocation.
    prebuilt_subs: Vec<Vec<SlashItem>>,
    filtered: Vec<usize>,
    /// Indices into the prebuilt subcommand list for the active parent.
    sub_filtered: Vec<usize>,
    /// Index of the parent item whose subcommands are currently shown.
    sub_parent_idx: Option<usize>,
    highlights: Vec<Vec<u32>>,
    mode: Mode,
    selected: usize,
}

impl SlashMenu {
    pub fn new(items: Vec<SlashItem>) -> Self {
        let prebuilt_subs: Vec<Vec<SlashItem>> = items
            .iter()
            .map(|item| {
                // Static subcommands from the compile-time registry.
                let static_items = item
                    .subcommands
                    .iter()
                    .map(|(sub_name, sub_desc)| SlashItem {
                        name: Cow::Owned(format!("{} {sub_name}", item.name)),
                        description: Cow::Borrowed(sub_desc),
                        ..Default::default()
                    });
                // Dynamic subcommands injected at runtime (e.g. connected MCP
                // server names and tool identifiers).
                let dynamic_items =
                    item.extra_subcommands
                        .iter()
                        .map(|(sub_name, sub_desc)| SlashItem {
                            name: Cow::Owned(format!("{} {sub_name}", item.name)),
                            description: Cow::Owned(sub_desc.clone()),
                            ..Default::default()
                        });
                static_items.chain(dynamic_items).collect()
            })
            .collect();
        let mut s = Self {
            items,
            prebuilt_subs,
            filtered: Vec::new(),
            sub_filtered: Vec::new(),
            sub_parent_idx: None,
            highlights: Vec::new(),
            mode: Mode::Command {
                token: String::new(),
            },
            selected: 0,
        };
        s.reset_to_all();
        s
    }

    fn reset_to_all(&mut self) {
        let mut idx: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| item.primary.then_some(index))
            .collect();
        idx.sort_by(|&a, &b| {
            self.items[b]
                .usage_boost
                .cmp(&self.items[a].usage_boost)
                .then(a.cmp(&b))
        });
        self.filtered = idx;
        self.highlights = vec![Vec::new(); self.filtered.len()];
        self.sub_filtered.clear();
        self.clamp_selected();
    }

    /// Update the filter from a composer buffer.
    pub fn set_filter(&mut self, buffer: &str) {
        let new_mode = derive_mode(buffer, &self.items);
        if std::mem::discriminant(&new_mode) != std::mem::discriminant(&self.mode) {
            self.selected = 0;
        }
        self.mode = new_mode;

        match &self.mode {
            Mode::Command { token } => {
                self.sub_filtered.clear();
                if token.is_empty() {
                    self.reset_to_all();
                    return;
                }

                let mut matcher = Matcher::new(Config::DEFAULT);
                let mut scored: Vec<ScoredItem> = self
                    .items
                    .iter()
                    .enumerate()
                    .filter_map(|(i, item)| score_one(&mut matcher, token, i, item))
                    .collect();

                scored.sort_by(|a, b| {
                    b.name_hit
                        .cmp(&a.name_hit)
                        .then(b.score.cmp(&a.score))
                        .then(
                            self.items[b.item_idx]
                                .usage_boost
                                .cmp(&self.items[a.item_idx].usage_boost),
                        )
                        .then(a.item_idx.cmp(&b.item_idx))
                });

                self.highlights = scored.iter().map(|s| s.indices.clone()).collect();
                self.filtered = scored.into_iter().map(|s| s.item_idx).collect();
                self.clamp_selected();
            }
            Mode::Subcommand { parent, token } => {
                self.filtered.clear();
                let parent_idx = self.items.iter().position(|it| it.name == parent.as_str());
                self.sub_parent_idx = parent_idx;

                let prebuilt = parent_idx
                    .map(|i| self.prebuilt_subs[i].as_slice())
                    .unwrap_or(&[]);

                self.sub_filtered = if token.is_empty() {
                    (0..prebuilt.len()).collect()
                } else {
                    let parent_prefix = format!("{parent} ");
                    let mut scored: Vec<(usize, u32)> = prebuilt
                        .iter()
                        .enumerate()
                        .filter_map(|(i, item)| {
                            // Match the full suffix (e.g. "tools github") so that
                            // multi-word dynamic entries like "inspect server:tool"
                            // are reachable by typing the full prefix.
                            let sub_suffix = item
                                .name
                                .strip_prefix(parent_prefix.as_str())
                                .unwrap_or(item.name.as_ref());
                            score_token(token, sub_suffix).map(|s| (i, s))
                        })
                        .collect();
                    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                    scored.into_iter().map(|(i, _)| i).collect()
                };
                self.highlights = vec![Vec::new(); self.sub_filtered.len()];
                self.clamp_selected();
            }
        }
    }

    /// Ordered, filtered view into items.
    pub fn matches(&self) -> Vec<&SlashItem> {
        match &self.mode {
            Mode::Command { .. } => self.filtered.iter().map(|&i| &self.items[i]).collect(),
            Mode::Subcommand { .. } => {
                let prebuilt = self
                    .sub_parent_idx
                    .map(|i| self.prebuilt_subs[i].as_slice())
                    .unwrap_or(&[]);
                self.sub_filtered.iter().map(|&i| &prebuilt[i]).collect()
            }
        }
    }

    /// `true` when the menu is currently filtering subcommands.
    pub fn is_subcommand_mode(&self) -> bool {
        matches!(self.mode, Mode::Subcommand { .. })
    }

    /// Per-match highlight indices.
    pub fn match_indices(&self) -> &[Vec<u32>] {
        &self.highlights
    }

    pub fn move_up(&mut self) {
        let n = self.match_count();
        if n == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            n - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        let n = self.match_count();
        if n == 0 {
            return;
        }
        self.selected = (self.selected + 1) % n;
    }

    pub fn page_down(&mut self, n: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + n).min(self.match_count().saturating_sub(1));
    }

    pub fn page_up(&mut self, n: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = self.selected.saturating_sub(n);
    }

    pub fn go_first(&mut self) {
        if self.match_count() > 0 {
            self.selected = 0;
        }
    }

    pub fn go_last(&mut self) {
        let n = self.match_count();
        if n > 0 {
            self.selected = n - 1;
        }
    }

    pub fn selected(&self) -> Option<usize> {
        if self.match_count() == 0 {
            None
        } else {
            Some(self.selected)
        }
    }

    pub fn select(&mut self, index: usize) -> bool {
        if index < self.match_count() {
            self.selected = index;
            true
        } else {
            false
        }
    }

    pub fn selected_item(&self) -> Option<&SlashItem> {
        match &self.mode {
            Mode::Command { .. } => self.filtered.get(self.selected).map(|&i| &self.items[i]),
            Mode::Subcommand { .. } => {
                let prebuilt = self
                    .sub_parent_idx
                    .map(|i| self.prebuilt_subs[i].as_slice())
                    .unwrap_or(&[]);
                self.sub_filtered.get(self.selected).map(|&i| &prebuilt[i])
            }
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
