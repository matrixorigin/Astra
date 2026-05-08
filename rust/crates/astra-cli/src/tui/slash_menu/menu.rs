//! Pure-logic inline slash-command menu.

#![allow(dead_code)]

use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::Atom};

/// A single slash command exposed to the menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashItem {
    /// Full command token including leading `/`, e.g. `/help`.
    pub name: &'static str,
    /// One-line description shown beside the name.
    pub description: &'static str,
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

/// Extract the command-token portion used for filtering from a composer
/// buffer. The first whitespace-delimited word of the first line, with the
/// leading `/` stripped. Returns empty string when nothing to filter by.
fn filter_token(buffer: &str) -> String {
    let first = buffer.lines().next().unwrap_or("");
    let tok = first
        .strip_prefix('/')
        .unwrap_or("")
        .split_whitespace()
        .next()
        .unwrap_or("");
    tok.to_ascii_lowercase()
}

/// Pure-logic slash command menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlashMenu {
    items: Vec<SlashItem>,
    /// Indices of `items` that currently match, ordered by match score
    /// (best first). When the filter is empty this is `0..items.len()`.
    filtered: Vec<usize>,
    selected: usize,
}

impl SlashMenu {
    pub fn new(items: Vec<SlashItem>) -> Self {
        let filtered: Vec<usize> = (0..items.len()).collect();
        Self {
            items,
            filtered,
            selected: 0,
        }
    }

    /// Update the filter from a composer buffer. Re-scores and re-orders
    /// `filtered`. Clamps `selected` into bounds.
    pub fn set_filter(&mut self, buffer: &str) {
        let token = filter_token(buffer);

        if token.is_empty() {
            self.filtered = (0..self.items.len()).collect();
            self.clamp_selected();
            return;
        }

        // Score every item. `nucleo-matcher::Atom` handles casefold +
        // subsequence matching with reasonable scoring.
        let mut matcher = Matcher::new(Config::DEFAULT);
        let atom = Atom::new(
            &token,
            nucleo_matcher::pattern::CaseMatching::Ignore,
            nucleo_matcher::pattern::Normalization::Smart,
            nucleo_matcher::pattern::AtomKind::Fuzzy,
            false,
        );

        let mut scored: Vec<(u16, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let hay = item.name.trim_start_matches('/');
                let mut buf = Vec::new();
                let hay_utf32 = Utf32Str::new(hay, &mut buf);
                atom.score(hay_utf32, &mut matcher).map(|s| (s, i))
            })
            .collect();

        // Higher score first; tie-break by original item order (stable).
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));

        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        self.clamp_selected();
    }

    /// Ordered, filtered view into items. Empty when no matches.
    pub fn matches(&self) -> Vec<&SlashItem> {
        self.filtered.iter().map(|&i| &self.items[i]).collect()
    }

    pub fn move_up(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.filtered.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn move_down(&mut self) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.filtered.len();
    }

    /// Index of current selection within `matches()`. `None` when empty.
    pub fn selected(&self) -> Option<usize> {
        if self.filtered.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    /// The currently selected item, if any.
    pub fn selected_item(&self) -> Option<&SlashItem> {
        self.filtered
            .get(self.selected)
            .map(|&i| &self.items[i])
    }

    pub fn len(&self) -> usize {
        self.filtered.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }
}
