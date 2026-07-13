//! Pure-logic inline `@`-mention menu.

#![allow(dead_code)]

use std::sync::Arc;

use nucleo_matcher::{Config, Matcher, Utf32Str, pattern::Atom};

use super::provider::{FileEntry, FileKind, FileProvider};

/// The text span the menu is currently bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MentionToken {
    /// Byte offset of the `@` character in the buffer.
    pub at_byte: usize,
    /// Byte offset just past the end of the token (cursor or whitespace).
    pub end_byte: usize,
    /// The text between `@` and `end_byte` (never includes the leading
    /// `@` itself). e.g. `src/main` from `@src/main`.
    pub partial: String,
}

/// Parse a mention token out of `buffer` around `cursor` (byte offset).
///
/// Returns `None` unless:
/// - there's an `@` at or before `cursor`,
/// - the `@` sits at the start of buffer or follows whitespace, and
/// - there's no whitespace between `@` and `cursor` (the token is
///   terminated by whitespace regardless of where the cursor is).
pub(crate) fn extract_mention_at(buffer: &str, cursor: usize) -> Option<MentionToken> {
    // Clamp cursor into bounds.
    let cursor = cursor.min(buffer.len());
    let before = &buffer[..cursor];

    // Find the last '@' before the cursor.
    let at_byte = before.rfind('@')?;

    // The char right before '@' must be whitespace or start-of-buffer.
    if at_byte > 0 {
        let prev = buffer[..at_byte].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }

    // Scan from `@+1` forward — abort if whitespace is crossed before
    // reaching the cursor (means the cursor has moved past the token
    // and we shouldn't show the menu anymore).
    let mut end_byte = at_byte + 1;
    for (i, c) in buffer[at_byte + 1..].char_indices() {
        let at_offset = at_byte + 1 + i;
        if c.is_whitespace() {
            // If the cursor is past this whitespace, the mention is
            // over — no active menu.
            if cursor > at_offset {
                return None;
            }
            break;
        }
        let next = at_offset + c.len_utf8();
        if next > cursor {
            break;
        }
        end_byte = next;
    }

    let partial = buffer[at_byte + 1..end_byte].to_string();

    Some(MentionToken {
        at_byte,
        end_byte,
        partial,
    })
}

/// Pure-logic mention menu.
pub(crate) struct MentionMenu {
    provider: Arc<dyn FileProvider>,
    current_entries: Vec<FileEntry>,
    /// Indices into `current_entries`, ranked by fuzzy score.
    filtered: Vec<usize>,
    selected: usize,
    current_token: Option<MentionToken>,
    current_dir: String,
    provider_revision: u64,
}

impl std::fmt::Debug for MentionMenu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MentionMenu")
            .field("current_entries", &self.current_entries)
            .field("filtered", &self.filtered)
            .field("selected", &self.selected)
            .finish()
    }
}

impl Clone for MentionMenu {
    fn clone(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            current_entries: self.current_entries.clone(),
            filtered: self.filtered.clone(),
            selected: self.selected,
            current_token: self.current_token.clone(),
            current_dir: self.current_dir.clone(),
            provider_revision: self.provider_revision,
        }
    }
}

impl PartialEq for MentionMenu {
    fn eq(&self, other: &Self) -> bool {
        // Pointer equality on the provider (two menus with different
        // providers are considered distinct); structural on the rest.
        Arc::ptr_eq(&self.provider, &other.provider)
            && self.current_entries == other.current_entries
            && self.filtered == other.filtered
            && self.selected == other.selected
            && self.current_token == other.current_token
            && self.current_dir == other.current_dir
            && self.provider_revision == other.provider_revision
    }
}
impl Eq for MentionMenu {}

impl MentionMenu {
    pub fn new<P: FileProvider + 'static>(provider: P) -> Self {
        Self::from_arc(Arc::new(provider))
    }

    pub fn from_arc(provider: Arc<dyn FileProvider>) -> Self {
        Self {
            provider,
            current_entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            current_token: None,
            current_dir: String::new(),
            provider_revision: 0,
        }
    }

    /// Refresh entries given a parsed token. Splits `partial` into
    /// `(dir_prefix, file_fragment)`:
    /// - `src/main` → dir=`src`, fragment=`main`
    /// - `src/`     → dir=`src`, fragment=``
    /// - `main`     → dir=``,    fragment=`main`
    pub fn set_token(&mut self, token: &MentionToken) {
        let (dir, fragment) = split_dir_fragment(&token.partial);

        self.current_token = Some(token.clone());
        self.current_dir = dir.to_string();
        self.provider_revision = self.provider.revision();
        self.current_entries = self.provider.list(dir);

        if fragment.is_empty() {
            // No fuzzy — keep provider's stable ordering (dirs first).
            self.filtered = (0..self.current_entries.len()).collect();
            self.clamp_selected();
            return;
        }

        let mut matcher = Matcher::new(Config::DEFAULT);
        let atom = Atom::new(
            fragment,
            nucleo_matcher::pattern::CaseMatching::Ignore,
            nucleo_matcher::pattern::Normalization::Smart,
            nucleo_matcher::pattern::AtomKind::Fuzzy,
            false,
        );

        let mut scored: Vec<(u16, usize)> = self
            .current_entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| {
                // Match against the final path segment; users type file names.
                let hay = leaf_name(&e.path);
                let mut buf = Vec::new();
                let hay_utf32 = Utf32Str::new(hay, &mut buf);
                atom.score(hay_utf32, &mut matcher).map(|s| (s, i))
            })
            .collect();

        // Higher score first; directories then break ties; then original order.
        scored.sort_by(|a, b| {
            let rank = b.0.cmp(&a.0);
            if rank != std::cmp::Ordering::Equal {
                return rank;
            }
            let ad = matches!(self.current_entries[a.1].kind, FileKind::Dir);
            let bd = matches!(self.current_entries[b.1].kind, FileKind::Dir);
            match (ad, bd) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.1.cmp(&b.1),
            }
        });

        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        self.clamp_selected();
    }

    pub(crate) fn refresh_if_provider_changed(&mut self) -> bool {
        self.provider.poll_refresh(&self.current_dir);
        let revision = self.provider.revision();
        if revision == self.provider_revision {
            return false;
        }
        let Some(token) = self.current_token.clone() else {
            self.provider_revision = revision;
            return false;
        };
        self.set_token(&token);
        true
    }

    pub(crate) fn is_loading(&self) -> bool {
        self.provider.is_loading(&self.current_dir)
    }

    pub(crate) fn load_error(&self) -> Option<String> {
        self.provider.load_error(&self.current_dir)
    }

    pub fn matches(&self) -> Vec<&FileEntry> {
        self.filtered
            .iter()
            .map(|&i| &self.current_entries[i])
            .collect()
    }

    pub fn len(&self) -> usize {
        self.filtered.len()
    }

    pub fn is_empty(&self) -> bool {
        self.filtered.is_empty()
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

    pub fn selected(&self) -> Option<usize> {
        if self.filtered.is_empty() {
            None
        } else {
            Some(self.selected)
        }
    }

    pub fn selected_item(&self) -> Option<&FileEntry> {
        self.filtered
            .get(self.selected)
            .map(|&i| &self.current_entries[i])
    }

    fn clamp_selected(&mut self) {
        if self.filtered.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
    }
}

/// Split `partial` into `(dir, leaf_fragment)` on the rightmost `/`.
fn split_dir_fragment(partial: &str) -> (&str, &str) {
    match partial.rfind('/') {
        Some(i) => (&partial[..i], &partial[i + 1..]),
        None => ("", partial),
    }
}

fn leaf_name(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[i + 1..],
        None => path,
    }
}
