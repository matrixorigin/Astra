use ratatui::text::{Line, Span};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Structured links need to survive ratatui's `Buffer` projection. A Buffer
/// cell has no metadata slot, so SystemCell attaches this process-local token
/// as zero-width variation selectors to each grapheme in the label. The
/// renderer resolves the token through the registry and emits OSC 8 only at
/// the terminal boundary. Tokens are never persisted or accepted from model
/// text as links unless they were registered by a structured SystemLink.
const LINK_MARKER_PREFIX: char = '\u{034f}';
const LINK_MARKER_DIGITS: usize = 16;
const LINK_MARKER_VARIATION_BASE: u32 = 0xfe00;
const MAX_REGISTERED_LINKS: usize = 4096;
const MAX_REGISTERED_LINK_URI_BYTES: usize = 1024 * 1024;

pub(crate) fn contains_link_marker(text: &str) -> bool {
    text.contains(LINK_MARKER_PREFIX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LinkMarker {
    pub(crate) token: u64,
    pub(crate) open: bool,
    pub(crate) close: bool,
}

static NEXT_LINK_TOKEN: AtomicU64 = AtomicU64::new(1);
struct LinkRegistry {
    entries: HashMap<u64, String>,
    uri_bytes: usize,
}

static LINK_REGISTRY: OnceLock<Mutex<LinkRegistry>> = OnceLock::new();

#[derive(Debug)]
struct LinkLeaseInner {
    token: u64,
}

impl Drop for LinkLeaseInner {
    fn drop(&mut self) {
        if let Ok(mut registry) = link_registry().lock() {
            if let Some(uri) = registry.entries.remove(&self.token) {
                registry.uri_bytes = registry.uri_bytes.saturating_sub(uri.len());
            }
        }
    }
}

/// A process-local capability held by the owning structured cell. Clones keep
/// the destination alive for Buffer and pending scrollback projections; the
/// registry entry is released after the last clone is dropped.
#[derive(Debug, Clone)]
pub(crate) struct LinkLease(Arc<LinkLeaseInner>);

impl LinkLease {
    pub(crate) fn token(&self) -> u64 {
        self.0.token
    }
}

fn link_registry() -> &'static Mutex<LinkRegistry> {
    LINK_REGISTRY.get_or_init(|| {
        Mutex::new(LinkRegistry {
            entries: HashMap::new(),
            uri_bytes: 0,
        })
    })
}

/// Register the sanitized destination of a structured UI link and return its
/// process-local token. The registry is intentionally shared by the TUI
/// Buffer and scrollback writers, while the durable event stores only the
/// presentation-neutral URI/label/fallback fields.
pub(crate) fn register_link(uri: &str) -> Option<LinkLease> {
    let uri = sanitize_osc8_component(uri);
    if !is_safe_osc8_uri(&uri) {
        return None;
    }
    let mut registry = link_registry().lock().ok()?;
    if registry.entries.len() >= MAX_REGISTERED_LINKS
        || registry.uri_bytes.saturating_add(uri.len()) > MAX_REGISTERED_LINK_URI_BYTES
    {
        return None;
    }
    let token = NEXT_LINK_TOKEN.fetch_add(1, Ordering::Relaxed);
    registry.uri_bytes = registry.uri_bytes.saturating_add(uri.len());
    registry.entries.insert(token, uri);
    Some(LinkLease(Arc::new(LinkLeaseInner { token })))
}

pub(crate) fn link_uri(token: u64) -> Option<String> {
    link_registry()
        .lock()
        .ok()
        .and_then(|registry| registry.entries.get(&token).cloned())
}

pub(crate) fn osc8_open(uri: &str) -> String {
    let uri = sanitize_osc8_component(uri);
    if !is_safe_osc8_uri(&uri) {
        return String::new();
    }
    format!("\x1b]8;;{uri}\x1b\\")
}

pub(crate) fn osc8_close() -> &'static str {
    "\x1b]8;;\x1b\\"
}

fn variation_digit(value: u8) -> char {
    char::from_u32(LINK_MARKER_VARIATION_BASE + u32::from(value))
        .expect("variation selector digit is valid Unicode")
}

fn variation_value(ch: char) -> Option<u8> {
    let value = u32::from(ch).checked_sub(LINK_MARKER_VARIATION_BASE)?;
    (value < 16).then_some(value as u8)
}

pub(crate) fn link_marker(token: u64, open: bool, close: bool) -> String {
    let mut marker = String::with_capacity(2 + LINK_MARKER_DIGITS);
    marker.push(LINK_MARKER_PREFIX);
    marker.push(variation_digit((open as u8) | ((close as u8) << 1)));
    for shift in (0..LINK_MARKER_DIGITS).rev() {
        marker.push(variation_digit(((token >> (shift * 4)) & 0xf) as u8));
    }
    marker
}

/// Attach a registered token to every visible grapheme in a short action
/// label. Keeping the marker on the grapheme it describes means a diff that
/// redraws only the middle of a label can still reopen the correct hyperlink.
pub(crate) fn mark_link_label(token: u64, label: &str) -> String {
    let label = sanitize_osc8_component(label);
    let graphemes = unicode_segmentation::UnicodeSegmentation::graphemes(label.as_str(), true)
        .collect::<Vec<_>>();
    if graphemes.is_empty() {
        return String::new();
    }
    let mut marked =
        String::with_capacity(label.len() + graphemes.len() * (2 + LINK_MARKER_DIGITS));
    for (index, grapheme) in graphemes.iter().enumerate() {
        marked.push_str(grapheme);
        marked.push_str(&link_marker(
            token,
            index == 0,
            index + 1 == graphemes.len(),
        ));
    }
    marked
}

/// Parse one registered marker embedded in a Buffer cell or text span.
/// Returns the marker and the byte range occupied by its zero-width payload.
pub(crate) fn parse_link_marker(text: &str) -> Option<(std::ops::Range<usize>, LinkMarker)> {
    let (range, marker) = parse_link_marker_payload(text)?;
    link_uri(marker.token)?;
    Some((range, marker))
}

fn parse_link_marker_payload(text: &str) -> Option<(std::ops::Range<usize>, LinkMarker)> {
    let start = text.find(LINK_MARKER_PREFIX)?;
    let payload_start = start + LINK_MARKER_PREFIX.len_utf8();
    let mut chars = text[payload_start..].char_indices();
    let (_, flags) = chars.next()?;
    let flags = variation_value(flags)?;
    let mut token = 0u64;
    let mut payload_end = 0usize;
    for _ in 0..LINK_MARKER_DIGITS {
        let (offset, digit) = chars.next()?;
        token = (token << 4) | u64::from(variation_value(digit)?);
        payload_end = offset + digit.len_utf8();
    }
    let end = payload_start + payload_end;
    Some((
        start..end,
        LinkMarker {
            token,
            open: flags & 1 != 0,
            close: flags & 2 != 0,
        },
    ))
}

/// Remove the private marker syntax from untrusted text regardless of whether
/// its token is currently registered. This is the provenance boundary: a
/// model/tool string cannot smuggle a marker into a later render pass and
/// borrow a structured link that another cell registered.
pub(crate) fn strip_untrusted_link_markers(text: &str) -> Cow<'_, str> {
    strip_marker_payloads(text, false)
}

/// Remove marker payloads that no longer have a live structured-link lease,
/// while preserving payloads whose destination is still registered. This is
/// used by trusted rendering paths because queued scrollback lines can outlive
/// the owning [`SystemCell`]. A stale marker must degrade to its visible text,
/// never leak the private marker syntax to a terminal.
pub(crate) fn strip_unregistered_link_markers(text: &str) -> Cow<'_, str> {
    strip_marker_payloads(text, true)
}

pub(crate) fn strip_link_markers(text: &str) -> Cow<'_, str> {
    strip_marker_payloads(text, false)
}

/// Strip valid private marker payloads while scanning past malformed prefixes.
/// When `preserve_registered` is true, live structured-link payloads remain so
/// the terminal boundary can resolve them; malformed and stale payloads are
/// always removed. Advancing by one prefix character on parse failure prevents
/// an attacker-controlled malformed marker from hiding a later valid marker.
fn strip_marker_payloads(text: &str, preserve_registered: bool) -> Cow<'_, str> {
    if !text.contains(LINK_MARKER_PREFIX) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    while cursor < text.len() {
        let tail = &text[cursor..];
        let Some(prefix_offset) = tail.find(LINK_MARKER_PREFIX) else {
            out.push_str(tail);
            break;
        };
        out.push_str(&tail[..prefix_offset]);
        let marker_tail = &tail[prefix_offset..];
        let Some((range, marker)) = parse_link_marker_payload(marker_tail) else {
            // The prefix is private syntax even when the payload is malformed.
            // Drop just that prefix and keep scanning for later valid markers.
            cursor += prefix_offset + LINK_MARKER_PREFIX.len_utf8();
            continue;
        };
        if preserve_registered && link_uri(marker.token).is_some() {
            out.push_str(&marker_tail[..range.end]);
        }
        cursor += prefix_offset + range.end;
    }
    Cow::Owned(out)
}

/// Split one Buffer cell into its visible grapheme and an optional trusted
/// marker. A cell normally contains one marker after its visible grapheme;
/// the loop is defensive so malformed or repeated marker text cannot bypass
/// the registry check in [`parse_link_marker`].
pub(crate) fn split_link_marker(text: &str) -> (Cow<'_, str>, Option<LinkMarker>) {
    if !text.contains(LINK_MARKER_PREFIX) {
        return (Cow::Borrowed(text), None);
    }
    let cleaned = strip_unregistered_link_markers(text);
    let marker = parse_link_marker(cleaned.as_ref()).map(|(_, marker)| marker);
    let visible = strip_link_markers(cleaned.as_ref());
    (Cow::Owned(visible.into_owned()), marker)
}

/// Convert registered link markers to OSC 8 for a direct terminal writer.
/// Unregistered marker-looking text is left alone and therefore cannot turn
/// arbitrary model/tool output into a trusted hyperlink.
pub(crate) fn render_link_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut active = None;
    while cursor < text.len() {
        let tail = &text[cursor..];
        let Some((range, marker)) = parse_link_marker(tail) else {
            out.push_str(tail);
            break;
        };
        let before = &tail[..range.start];
        // Markers are attached after the grapheme they describe so ratatui
        // keeps the metadata in the same Buffer cell. Split that final
        // grapheme back out here; opening the link at `before` would make the
        // whole notice (including its status text) clickable.
        let Some((grapheme_start, _)) =
            unicode_segmentation::UnicodeSegmentation::grapheme_indices(before, true).next_back()
        else {
            out.push_str(tail);
            break;
        };
        out.push_str(&before[..grapheme_start]);
        let grapheme = &before[grapheme_start..];
        if active != Some(marker.token) {
            if active.is_some() {
                out.push_str(osc8_close());
            }
            if let Some(uri) = link_uri(marker.token) {
                out.push_str(&osc8_open(&uri));
                active = Some(marker.token);
            }
        }
        out.push_str(grapheme);
        cursor += range.end;
        if marker.close {
            out.push_str(osc8_close());
            active = None;
        }
    }
    if active.is_some() {
        out.push_str(osc8_close());
    }
    out
}

pub(crate) fn terminal_hyperlinks_enabled() -> bool {
    std::env::var("ASTRA_OSC8")
        .map(|v| !matches!(v.as_str(), "0" | "false" | "off" | "no"))
        .unwrap_or(true)
}

pub(crate) fn sanitize_osc8_component(value: &str) -> String {
    value
        .chars()
        .filter(|c| !matches!(*c, '\x1b' | '\x07') && !c.is_control())
        .collect()
}

pub(crate) fn osc8_link(uri: &str, label: &str) -> String {
    let uri = sanitize_osc8_component(uri);
    let label = sanitize_osc8_component(label);
    if !is_safe_osc8_uri(&uri) {
        return label;
    }
    format!("\x1b]8;;{uri}\x1b\\{label}\x1b]8;;\x1b\\")
}

fn is_safe_osc8_uri(uri: &str) -> bool {
    (uri.starts_with("file://") || uri.starts_with("http://") || uri.starts_with("https://"))
        && !uri
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\x1b' | '\x07'))
}

pub(crate) fn hyperlink_line_file_paths(line: &Line<'static>, cwd: Option<&Path>) -> Line<'static> {
    if !terminal_hyperlinks_enabled() {
        return line.clone();
    }

    let mut changed = false;
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|span| {
            let content = hyperlink_text_file_paths(span.content.as_ref(), cwd);
            if content != span.content.as_ref() {
                changed = true;
            }
            Span::styled(content, span.style)
        })
        .collect();

    if !changed {
        return line.clone();
    }

    let mut out = Line::from(spans);
    out.style = line.style;
    out.alignment = line.alignment;
    out
}

fn hyperlink_text_file_paths(text: &str, cwd: Option<&Path>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut cursor = 0usize;

    while cursor < text.len() {
        let Some(ch) = text[cursor..].chars().next() else {
            break;
        };
        if ch.is_whitespace() {
            out.push(ch);
            cursor += ch.len_utf8();
            continue;
        }

        let start = cursor;
        cursor += ch.len_utf8();
        while cursor < text.len() {
            let Some(next) = text[cursor..].chars().next() else {
                break;
            };
            if next.is_whitespace() {
                break;
            }
            cursor += next.len_utf8();
        }

        let token = &text[start..cursor];
        if let Some(linked) = hyperlink_file_path_token(token, cwd) {
            out.push_str(&linked);
            changed = true;
        } else {
            out.push_str(token);
        }
    }

    if changed { out } else { text.to_string() }
}

fn hyperlink_file_path_token(raw_token: &str, cwd: Option<&Path>) -> Option<String> {
    if raw_token.contains("\x1b]8;;") {
        return None;
    }

    let (prefix, core, suffix) = split_surrounding_punctuation(raw_token);
    if core.is_empty() || looks_like_url(core) {
        return None;
    }

    let (path, _) = split_optional_line_suffix(core);
    if !is_file_path_like(path) {
        return None;
    }

    let uri = file_uri_for_path(path, cwd)?;
    Some(format!("{prefix}{}{suffix}", osc8_link(&uri, core)))
}

fn split_surrounding_punctuation(token: &str) -> (&str, &str, &str) {
    let start = token
        .char_indices()
        .find(|(_, c)| !is_leading_trimmed_token_punctuation(*c))
        .map(|(idx, _)| idx)
        .unwrap_or(token.len());
    let end = token
        .char_indices()
        .rev()
        .find(|(_, c)| !is_trailing_trimmed_token_punctuation(*c))
        .map(|(idx, c)| idx + c.len_utf8())
        .unwrap_or(start);
    if start >= end {
        return (token, "", "");
    }
    (&token[..start], &token[start..end], &token[end..])
}

fn is_leading_trimmed_token_punctuation(c: char) -> bool {
    matches!(c, '(' | '[' | '{' | '<' | '\'' | '"')
}

fn is_trailing_trimmed_token_punctuation(c: char) -> bool {
    matches!(
        c,
        ')' | ']' | '}' | '>' | ',' | '.' | ';' | ':' | '!' | '\'' | '"'
    )
}

fn looks_like_url(token: &str) -> bool {
    token.contains("://")
        || token.starts_with("www.")
        || token.starts_with("localhost:")
        || token.starts_with("localhost/")
}

fn split_optional_line_suffix(token: &str) -> (&str, Option<&str>) {
    let Some((path, suffix)) = token.rsplit_once(':') else {
        return (token, None);
    };
    if suffix.chars().all(|c| c.is_ascii_digit()) && path.contains('/') {
        (path, Some(suffix))
    } else {
        (token, None)
    }
}

fn is_file_path_like(path: &str) -> bool {
    is_absolute_path_like(path) || is_relative_path_like(path)
}

fn is_absolute_path_like(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && path
            .chars()
            .all(|c| !c.is_control() && !matches!(c, '\x1b' | '\x07'))
}

fn is_relative_path_like(path: &str) -> bool {
    if !(path.starts_with("./") || path.starts_with("../") || path.contains('/')) {
        return false;
    }
    if path.ends_with('/') || path.contains("://") {
        return false;
    }
    let Some(last) = path.rsplit('/').next() else {
        return false;
    };
    last.contains('.')
        && !last.starts_with('.')
        && path
            .chars()
            .all(|c| !c.is_control() && !matches!(c, '\x1b' | '\x07' | '*' | '?'))
}

pub(crate) fn file_uri_for_path(path: &str, cwd: Option<&Path>) -> Option<String> {
    let clean = sanitize_osc8_component(path);
    if clean.is_empty() {
        return None;
    }

    let candidate = if clean.starts_with('/') {
        PathBuf::from(&clean)
    } else if let Some(cwd) = cwd {
        cwd.join(&clean)
    } else {
        return Some(format!("file://./{clean}"));
    };

    url::Url::from_file_path(candidate)
        .ok()
        .map(|url| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        hyperlink_text_file_paths, link_uri, mark_link_label, register_link, render_link_markers,
        split_surrounding_punctuation,
    };

    #[test]
    fn split_surrounding_punctuation_handles_empty_core() {
        let (prefix, core, suffix) = split_surrounding_punctuation(r#"<<")"#);
        assert_eq!(prefix, r#"<<")"#);
        assert_eq!(core, "");
        assert_eq!(suffix, "");
    }

    #[test]
    fn hyperlink_text_file_paths_ignores_punctuation_only_tokens() {
        let text = r#"prefix <<") suffix"#;
        assert_eq!(hyperlink_text_file_paths(text, None), text);
    }

    #[test]
    fn structured_marker_links_only_the_action_label() {
        let lease = register_link("file:///tmp/report.md").expect("test link should fit registry");
        let token = lease.token();
        let text = format!(
            "Explain Analyze report ready · {}",
            mark_link_label(token, "Open report")
        );
        let rendered = render_link_markers(&text);
        assert_eq!(
            rendered,
            "Explain Analyze report ready · \x1b]8;;file:///tmp/report.md\x1b\\Open report\x1b]8;;\x1b\\"
        );
    }

    #[test]
    fn unregistered_marker_text_is_not_promoted_to_a_link() {
        let marker = super::link_marker(u64::MAX, true, true);
        let text = format!("status {marker}Open");
        assert_eq!(render_link_markers(&text), text);
    }

    #[test]
    fn link_lease_releases_registry_entry_after_all_clones_drop() {
        let lease = register_link("file:///tmp/report.md").expect("test link should fit registry");
        let token = lease.token();
        assert!(link_uri(token).is_some());
        let clone = lease.clone();
        drop(lease);
        assert!(link_uri(token).is_some());
        drop(clone);
        assert!(link_uri(token).is_none());
    }
}
