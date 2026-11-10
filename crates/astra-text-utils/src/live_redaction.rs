//! Bounded incremental presentation of a growing, potentially sensitive stream.
//!
//! The scanner uses the same credential expressions as the full projection.
//! Bytes that might still become part of a match stay pending until the match
//! is resolved; unusually long ambiguous input is withheld instead of stored.

use std::collections::VecDeque;
use std::sync::OnceLock;

use regex_automata::{
    Anchored, Input, MatchKind,
    dfa::{Automaton, StartKind, dense},
    util::primitives::StateID,
};

use crate::credential_redaction::{
    live_credential_patterns, pending_display_start, private_key_end_markers_bytes,
    private_key_header_markers_bytes, redact_credentials_for_display_streaming,
};

const TAIL_BYTES: usize = 64 * 1024;
const PENDING_BYTES: usize = 64 * 1024;
const CHUNK_BYTES: usize = 8 * 1024;
const PEM_OVERLAP: usize = 128;
const MAX_STATES: usize = 512;
const MAX_MATCH_INTERVALS: usize = 1024;
const WITHHELD: &str =
    "[Live output withheld: incomplete sensitive content exceeded the preview limit.]";

struct Matcher {
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
}

fn matcher() -> Option<&'static Matcher> {
    static MATCHER: OnceLock<Option<Matcher>> = OnceLock::new();
    MATCHER
        .get_or_init(|| {
            let dfa = dense::Builder::new()
                .configure(
                    dense::Config::new()
                        .start_kind(StartKind::Anchored)
                        .match_kind(MatchKind::All)
                        .dfa_size_limit(Some(64 * 1024 * 1024)),
                )
                .build_many(&live_credential_patterns())
                .ok()?;
            let start = dfa
                .start_state_forward(&Input::new(b"").anchored(Anchored::Yes))
                .ok()?;
            Some(Matcher { dfa, start })
        })
        .as_ref()
}

struct OpenPem {
    start: u64,
    kind: String,
}

/// The visible, bounded tail and its current safe-stream counts.
pub struct LiveRedactedSnapshot {
    pub text: String,
    pub total_bytes: u64,
    pub total_lines: u64,
}

/// Bounded command output. A long stream keeps a safely redacted tail when
/// there is no other way to retrieve it; short streams need no live scanner.
pub struct OutputPreview {
    limit: usize,
    retain_tail: bool,
    prefix: Vec<u8>,
    tail: Option<LiveRedactor>,
    truncated: bool,
}

impl OutputPreview {
    pub fn new(limit: usize, retain_tail: bool) -> Self {
        Self {
            limit,
            retain_tail,
            prefix: Vec::new(),
            tail: None,
            truncated: false,
        }
    }

    pub fn append(&mut self, bytes: &[u8]) {
        if let Some(tail) = &mut self.tail {
            tail.append(bytes);
            return;
        }
        let keep = bytes
            .len()
            .min(self.limit.saturating_sub(self.prefix.len()));
        self.prefix.extend_from_slice(&bytes[..keep]);
        if keep < bytes.len() {
            self.truncated = true;
            if self.retain_tail {
                let mut tail = LiveRedactor::default();
                tail.append(&self.prefix);
                tail.append(&bytes[keep..]);
                self.prefix.clear();
                self.tail = Some(tail);
            }
        }
    }

    /// Incomplete input has no trustworthy EOF boundary. Project it with the
    /// streaming rules so useful partial lines survive without exposing a
    /// credential that may continue beyond the captured prefix.
    pub fn finish(mut self, incomplete: bool) -> (Vec<u8>, bool, bool) {
        if let Some(mut tail) = self.tail {
            // Natural EOF resolves incomplete URL/token candidates. Interrupted
            // streams have no such boundary and must stay conservative.
            if !incomplete {
                tail.append(b"\n");
            }
            let mut text = tail.snapshot().text;
            if !incomplete && text.ends_with('\n') {
                text.pop();
            }
            return (text.into_bytes(), self.truncated, false);
        }
        let non_utf8 = std::str::from_utf8(&self.prefix).is_err();
        if self.truncated || incomplete {
            let text = String::from_utf8_lossy(&self.prefix);
            if let Ok((safe, _)) = redact_credentials_for_display_streaming(&text, usize::MAX) {
                return (safe.into_bytes(), self.truncated, non_utf8);
            }
            // On projection failure, retain only complete lines.
            let end = self
                .prefix
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |i| i + 1);
            self.prefix.truncate(end);
        }
        (self.prefix, self.truncated, non_utf8)
    }
}

/// Task-local redaction state; it owns neither the process nor its lifecycle.
#[derive(Default)]
pub struct LiveRedactor {
    tail: VecDeque<u8>,
    utf8_pending: Vec<u8>,
    pending: Vec<u8>,
    pending_start: u64,
    position: u64,
    states: Vec<(StateID, u64)>,
    matches: Vec<(u64, u64)>,
    pem: Option<OpenPem>,
    pem_overlap: Vec<u8>,
    poisoned: bool,
    suppressing: bool,
    total_bytes: u64,
    newline_count: u64,
    last_byte: Option<u8>,
}

impl LiveRedactor {
    /// A failed scanner must never resume from partially updated state.
    pub fn withhold(&mut self) {
        if !self.suppressing {
            self.push_safe(WITHHELD);
        }
        self.poisoned = true;
        self.suppressing = true;
        self.pending.clear();
        self.utf8_pending.clear();
        self.states.clear();
        self.matches.clear();
        self.pem = None;
    }

    /// Advance in the order bytes are observed from this stream.
    pub fn append(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            // The canonical projection matches lossy UTF-8, so the DFA must
            // see those same bytes. Keep a split code point until its next
            // write rather than treating its partial bytes as delimiters.
            if self.utf8_pending.is_empty() {
                let end = complete_utf8_prefix(chunk);
                self.append_chunk(String::from_utf8_lossy(&chunk[..end]).as_bytes());
                self.utf8_pending.extend_from_slice(&chunk[end..]);
            } else {
                let mut input = std::mem::take(&mut self.utf8_pending);
                input.extend_from_slice(chunk);
                let end = complete_utf8_prefix(&input);
                self.append_chunk(String::from_utf8_lossy(&input[..end]).as_bytes());
                self.utf8_pending.extend_from_slice(&input[end..]);
            }
        }
    }

    fn append_chunk(&mut self, bytes: &[u8]) {
        let Some(matcher) = matcher() else {
            self.poisoned = true;
            if !self.suppressing {
                self.push_safe(WITHHELD);
                self.suppressing = true;
            }
            return;
        };
        let before = self.position;
        let markers = self.pem_markers(bytes, before);
        let mut markers = markers.into_iter().peekable();
        let mut collect_from = (!self.suppressing).then_some(0);
        for (index, &byte) in bytes.iter().enumerate() {
            let end = self.position.saturating_add(1);
            while markers.peek().is_some_and(|marker| marker.1 == end) {
                let (start, end, kind, header) = markers.next().unwrap();
                self.apply_pem_marker(start, end, kind, header);
            }
            for (state, _) in &mut self.states {
                *state = matcher.dfa.next_state(*state, byte);
            }
            self.states
                .push((matcher.dfa.next_state(matcher.start, byte), self.position));
            self.states
                .retain(|(state, _)| !matcher.dfa.is_dead_state(*state));
            if self.states.len() > 1 {
                self.states.sort_unstable();
                self.states.dedup_by_key(|(state, _)| *state);
            }
            let match_start = self
                .states
                .iter()
                .filter_map(|(state, start)| matcher.dfa.is_match_state(*state).then_some(*start))
                .min();
            if !self.suppressing
                && let Some(start) = match_start
            {
                self.record_match(start, end);
            }
            self.position = end;
            if self.states.len() > MAX_STATES {
                self.poisoned = true;
                self.states.clear();
            }
            if self.poisoned {
                break;
            }
            if !self.suppressing
                && self.pem.as_ref().is_some_and(|pem| {
                    self.position.saturating_sub(pem.start) > PENDING_BYTES as u64
                })
            {
                self.pending.clear();
                self.matches.clear();
                self.pending_start = self.position;
                self.push_safe(WITHHELD);
                self.suppressing = true;
                collect_from = None;
            }
            if self.suppressing {
                self.matches.clear();
                self.pending_start = self.position;
                if self.states.is_empty() && self.pem.is_none() {
                    self.suppressing = false;
                    // DFA matches are delayed by one byte. The byte that
                    // takes every prior candidate dead is only lookahead;
                    // keep it so recovery does not eat the next prompt.
                    self.pending_start = self.position.saturating_sub(1);
                    collect_from = Some(index);
                }
            }
        }
        if self.position < before.saturating_add(bytes.len() as u64) {
            self.position = before.saturating_add(bytes.len() as u64);
        }
        if let Some(start) = collect_from {
            self.pending.extend_from_slice(&bytes[start..]);
        }
        if self.poisoned && !self.suppressing {
            self.push_safe(WITHHELD);
            self.suppressing = true;
        }
        if self.suppressing {
            self.pending.clear();
            self.matches.clear();
            self.pending_start = self.position;
            if !self.poisoned && self.states.is_empty() && self.pem.is_none() {
                self.suppressing = false;
            }
            return;
        }

        self.matches.sort_unstable();
        let mut cutoff = self
            .states
            .iter()
            .map(|(_, start)| *start)
            .chain(self.pem.iter().map(|pem| pem.start))
            .min()
            .unwrap_or(self.position);
        // A completed match can overlap another still-open candidate. Keep
        // the whole match until it can be projected with its original context.
        for &(start, end) in self.matches.iter().rev() {
            if start < cutoff && end > cutoff {
                cutoff = start;
            }
        }
        let available = cutoff.saturating_sub(self.pending_start) as usize;
        let mut end = complete_utf8_prefix(&self.pending[..available.min(self.pending.len())]);
        while end > 0 {
            let previous = end;
            let boundary = self.pending_start + end as u64;
            for &(start, match_end) in self.matches.iter().rev() {
                if start < boundary && match_end > boundary {
                    end = end.min(start.saturating_sub(self.pending_start) as usize);
                }
            }
            end = complete_utf8_prefix(&self.pending[..end]);
            let raw = String::from_utf8_lossy(&self.pending[..end]);
            if let Some(start) = pending_display_start(&raw) {
                let next = lossy_offset_to_raw(&self.pending[..end], start);
                end = complete_utf8_prefix(&self.pending[..next.min(end)]);
            }
            if end == previous {
                break;
            }
        }
        if end > 0 {
            let raw = String::from_utf8_lossy(&self.pending[..end]).into_owned();
            if self.matches.is_empty() {
                // The shared matcher found no credential and the incomplete
                // boundary checks above have passed. Avoid running every
                // full-text regex over ordinary output a second time.
                self.push_safe(&raw);
            } else {
                match redact_credentials_for_display_streaming(&raw, PENDING_BYTES * 4) {
                    Ok((safe, _)) => self.push_safe(&safe),
                    Err(_) => self.push_safe(WITHHELD),
                }
            }
            self.pending.drain(..end);
            self.pending_start += end as u64;
            self.matches.retain(|(_, end)| *end > self.pending_start);
        }
        if self.pending.len() > PENDING_BYTES {
            self.pending.clear();
            self.matches.clear();
            self.pending_start = self.position;
            self.push_safe(WITHHELD);
            self.suppressing = true;
        }
    }

    fn pem_markers(&mut self, bytes: &[u8], before: u64) -> Vec<(u64, u64, String, bool)> {
        let mut scan = self.pem_overlap.clone();
        scan.extend_from_slice(bytes);
        let base = before.saturating_sub(self.pem_overlap.len() as u64);
        let mut markers: Vec<_> =
            private_key_header_markers_bytes(&scan)
                .into_iter()
                .map(|(start, end, kind)| (base + start as u64, base + end as u64, kind, true))
                .chain(private_key_end_markers_bytes(&scan).into_iter().map(
                    |(start, end, kind)| (base + start as u64, base + end as u64, kind, false),
                ))
                .filter(|(_, end, _, _)| *end > before)
                .collect();
        markers.sort_unstable_by_key(|(start, end, _, _)| (*end, *start));
        self.pem_overlap = scan[scan.len().saturating_sub(PEM_OVERLAP)..].to_vec();
        markers
    }

    fn apply_pem_marker(&mut self, start: u64, end: u64, kind: String, header: bool) {
        if header {
            if self.pem.is_some() {
                self.poisoned = true;
            } else {
                self.pem = Some(OpenPem { start, kind });
            }
        } else if let Some(pem) = self.pem.take() {
            if pem.kind != kind {
                self.poisoned = true;
            } else if !self.suppressing {
                self.record_match(pem.start, end);
            }
        }
    }

    fn record_match(&mut self, start: u64, end: u64) {
        if let Some(last) = self.matches.last_mut()
            && start <= last.1
        {
            last.0 = last.0.min(start);
            last.1 = last.1.max(end);
        } else {
            self.matches.push((start, end));
        }
        if self.matches.len() > MAX_MATCH_INTERVALS {
            // This is conservative: an interval spanning several matches can
            // delay unrelated text, but cannot expose an overlapping secret.
            let (start, end) = self
                .matches
                .iter()
                .fold((u64::MAX, 0), |(lo, hi), (start, end)| {
                    (lo.min(*start), hi.max(*end))
                });
            self.matches.clear();
            self.matches.push((start, end));
        }
    }

    fn push_safe(&mut self, safe: &str) {
        self.total_bytes = self.total_bytes.saturating_add(safe.len() as u64);
        self.newline_count = self
            .newline_count
            .saturating_add(safe.bytes().filter(|byte| *byte == b'\n').count() as u64);
        if let Some(last) = safe.as_bytes().last() {
            self.last_byte = Some(*last);
        }
        let old_len = self.tail.len();
        let total = old_len.saturating_add(safe.len());
        let mut cut = total.saturating_sub(TAIL_BYTES);
        let byte = |index: usize| {
            if index < old_len {
                self.tail.get(index).copied()
            } else {
                safe.as_bytes().get(index - old_len).copied()
            }
        };
        while byte(cut).is_some_and(|value| value & 0xc0 == 0x80) {
            cut += 1;
        }
        // Do not split a canonical redaction marker at the ring boundary.
        // Only the nearby marker can matter; never rescan the whole tail.
        const MARKER: &[u8] = b"[REDACTED:";
        if let Some(start) = (cut.saturating_sub(256)..cut).rev().find(|start| {
            MARKER
                .iter()
                .enumerate()
                .all(|(offset, expected)| byte(start + offset) == Some(*expected))
        }) && let Some(end) =
            (start + MARKER.len()..total.min(start + 256)).find(|index| byte(*index) == Some(b']'))
            && end >= cut
        {
            cut = end + 1;
        }
        if cut <= old_len {
            self.tail.drain(..cut);
            self.tail.extend(safe.bytes());
        } else {
            self.tail.clear();
            self.tail
                .extend(safe.as_bytes()[cut - old_len..].iter().copied());
        }
    }

    /// Return a current safe snapshot without scanning already committed data.
    pub fn snapshot(&self) -> LiveRedactedSnapshot {
        let mut text =
            String::from_utf8_lossy(&self.tail.iter().copied().collect::<Vec<_>>()).into_owned();
        let pending = if self.suppressing {
            String::new()
        } else {
            let end = complete_utf8_prefix(&self.pending);
            let raw = String::from_utf8_lossy(&self.pending[..end]);
            redact_credentials_for_display_streaming(&raw, PENDING_BYTES * 4)
                .map(|(safe, _)| safe)
                .unwrap_or_else(|_| WITHHELD.into())
        };
        let total_bytes = self.total_bytes.saturating_add(pending.len() as u64);
        let newline_count = self
            .newline_count
            .saturating_add(pending.bytes().filter(|byte| *byte == b'\n').count() as u64);
        let last = pending.as_bytes().last().copied().or(self.last_byte);
        let total_lines = newline_count + u64::from(last.is_some_and(|byte| byte != b'\n'));
        text.push_str(&pending);
        if text.len() > TAIL_BYTES {
            let cut = safe_tail_start(&text, TAIL_BYTES);
            text.drain(..cut);
        }
        LiveRedactedSnapshot {
            text,
            total_bytes,
            total_lines,
        }
    }
}

fn safe_tail_start(text: &str, max_bytes: usize) -> usize {
    let mut cut = text.ceil_char_boundary(text.len().saturating_sub(max_bytes));
    let lookback = text.floor_char_boundary(cut.saturating_sub(256));
    if let Some(relative) = text[lookback..cut].rfind("[REDACTED:") {
        let start = lookback + relative;
        if let Some(end) = text[start..].find(']') {
            let end = start + end + 1;
            if end > cut {
                cut = end;
            }
        }
    }
    cut
}

fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    let Some(mut start) = bytes.len().checked_sub(1) else {
        return 0;
    };
    while start > 0 && bytes[start] & 0xc0 == 0x80 && bytes.len() - start < 4 {
        start -= 1;
    }
    match std::str::from_utf8(&bytes[start..]) {
        Err(error) if error.error_len().is_none() => start + error.valid_up_to(),
        _ => bytes.len(),
    }
}

fn lossy_offset_to_raw(bytes: &[u8], offset: usize) -> usize {
    let mut raw = 0;
    let mut rendered = 0;
    for chunk in bytes.utf8_chunks() {
        let valid = chunk.valid().len();
        if offset <= rendered + valid {
            return raw + offset.saturating_sub(rendered);
        }
        raw += valid;
        rendered += valid;
        if !chunk.invalid().is_empty() {
            if offset < rendered + 3 {
                return raw;
            }
            raw += chunk.invalid().len();
            rendered += 3;
        }
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_retains_safe_tail_and_resolves_natural_eof() {
        for incomplete in [false, true] {
            let mut preview = OutputPreview::new(16, true);
            preview.append(b"header\npassword=");
            preview.append(b"example-secret\nOpen http://127.0.0.1:3000");
            let (output, truncated, _) = preview.finish(incomplete);
            let output = String::from_utf8(output).unwrap();
            assert!(truncated, "{output}");
            assert!(!output.contains("example-secret"), "{output}");
            assert_eq!(output.contains("127.0.0.1:3000"), !incomplete, "{output}");
        }
    }

    #[test]
    fn incomplete_inline_preview_preserves_safe_text_and_redacts_partial_secrets() {
        for (raw, visible, secret) in [
            (
                "diagnostic without newline",
                "diagnostic without newline",
                None,
            ),
            (
                "error password=topsecret",
                "error password=",
                Some("topsecret"),
            ),
            ("Open http://user:topsecret", "Open http", Some("topsecret")),
            ("错误详情", "错误详情", None),
        ] {
            let mut preview = OutputPreview::new(1024, true);
            preview.append(raw.as_bytes());
            let (bytes, truncated, _) = preview.finish(true);
            let output = String::from_utf8(bytes).unwrap();
            assert!(!truncated);
            assert!(output.contains(visible), "{output:?}");
            assert!(
                secret.is_none_or(|secret| !output.contains(secret)),
                "{output:?}"
            );
        }
    }

    #[test]
    fn incomplete_preview_keeps_lossy_diagnostics_without_leaking_secrets() {
        for bytes in [
            b"diagnostic\xff password=topsecret".as_slice(),
            b"diagnostic password=topsecret \xe4\xb8".as_slice(),
        ] {
            let mut preview = OutputPreview::new(1024, true);
            for chunk in bytes.chunks(3) {
                preview.append(chunk);
            }
            let (output, truncated, non_utf8) = preview.finish(true);
            let output = String::from_utf8(output).unwrap();
            assert!(non_utf8 && !truncated);
            assert!(output.contains("diagnostic"), "{output:?}");
            assert!(!output.contains("topsecret"), "{output:?}");
        }
    }

    #[test]
    fn every_byte_boundary_matches_the_canonical_live_projection() {
        assert!(matcher().is_some());
        for raw in [
            "normal output\npassword=ZZZZZZZZZZZZ\nnext\n",
            "Bearer ZZZZZZZZZZZZ\nnext\n",
            "--token ZZZZZZZZZZZZ\nnext\n",
            "prefixghp_ZZZZZZZZZZZZ\nnext\n",
            "Open https://user:ZZZZZZZZZZZZ@example.test/path\n",
            "password=fooBearer ZZZZZZZZZZZZ\nnext\n",
            "{\"password\":\"ZZZZZZZZZZZZ\"}\nnext\n",
            "escaped \\\"--token \\\"ZZZZZZZZZZZZ\\\"\nnext\n",
            "-----BEGIN PRIVATE KEY-----\nZZZZZZZZZZZZ\n-----END PRIVATE KEY-----\nnext\n",
        ] {
            let mut live = LiveRedactor::default();
            for end in 1..=raw.len() {
                live.append(&raw.as_bytes()[end - 1..end]);
                let actual = live.snapshot().text;
                let expected = redact_credentials_for_display_streaming(&raw[..end], usize::MAX)
                    .unwrap()
                    .0;
                assert_eq!(actual, expected, "prefix {end} of {raw:?}");
                let secret_start = raw.find('Z').unwrap();
                if end > secret_start {
                    assert!(!actual.contains('Z'), "secret leaked at {end}: {actual:?}");
                }
            }
        }
    }

    #[test]
    fn bounded_ambiguity_is_withheld_and_later_output_can_resume() {
        let mut live = LiveRedactor::default();
        live.append(b"password=");
        for _ in 0..20 {
            live.append(&vec![b'Z'; CHUNK_BYTES]);
        }
        let snapshot = live.snapshot();
        assert!(!snapshot.text.contains('Z'));
        assert!(snapshot.text.contains("withheld"));
        assert!(live.pending.len() <= PENDING_BYTES);
        live.append(b"\n");
        live.append(b"READY\n");
        let recovered = live.snapshot().text;
        assert!(recovered.contains("READY"));
        assert!(!recovered.contains('Z'));
    }

    #[test]
    fn long_ambiguous_output_keeps_match_bookkeeping_bounded() {
        let mut live = LiveRedactor::default();
        live.append(b"https://");
        for _ in 0..2000 {
            live.append(b"password=Z'");
        }
        assert!(live.matches.len() <= MAX_MATCH_INTERVALS);
        assert!(live.pending.len() <= PENDING_BYTES);
        assert!(!live.snapshot().text.contains('Z'));
    }

    #[test]
    fn chunk_boundaries_preserve_overlapping_context_and_pem() {
        let raw = "prefix password=fooBearer ZZZZZZZZZZZZ\n-----BEGIN PRIVATE KEY-----\nZZZZZZZZZZZZ\n-----END PRIVATE KEY-----\nafter\nhttps://user:ZZZZZZZZZZZZ@example.test/path\n";
        for size in [1, 2, 3, 7, 64, 8192] {
            let mut live = LiveRedactor::default();
            let mut end = 0;
            for chunk in raw.as_bytes().chunks(size) {
                live.append(chunk);
                end += chunk.len();
                let expected = redact_credentials_for_display_streaming(&raw[..end], usize::MAX)
                    .unwrap()
                    .0;
                assert_eq!(
                    live.snapshot().text,
                    expected,
                    "chunk size {size}, prefix {end}"
                );
            }
        }
        let mut incomplete = LiveRedactor::default();
        incomplete
            .append(b"-----BEGIN PRIVATE KEY-----\nZZZZ\n-----END RSA PRIVATE KEY-----\nZZZZ\n");
        assert!(!incomplete.snapshot().text.contains('Z'));
        let mut nested = LiveRedactor::default();
        for chunk in b"-----BEGIN PRIVATE KEY-----BEGIN PRIVATE KEY-----\nZZZZ\n-----END PRIVATE KEY-----\nZZZZ".chunks(7) {
            nested.append(chunk);
            assert!(!nested.snapshot().text.contains('Z'));
        }
    }

    #[test]
    fn split_utf8_and_large_ordinary_output_keep_a_bounded_tail() {
        let mut live = LiveRedactor::default();
        for _ in 0..200 {
            live.append(&vec![b'x'; 4096]);
            live.append(b"\n");
        }
        for byte in "你好\n".as_bytes() {
            live.append(&[*byte]);
        }
        let snapshot = live.snapshot();
        assert!(snapshot.text.ends_with("你好\n"));
        assert!(snapshot.text.len() <= TAIL_BYTES);
        assert_eq!(snapshot.total_bytes, 200 * 4097 + "你好\n".len() as u64);
    }

    #[test]
    fn mixed_contexts_agree_with_full_projection_across_chunk_sizes() {
        let pieces = [
            "password",
            "api_key",
            "Bearer",
            "--token",
            "AKIA",
            "ghp_",
            "hf_",
            "://",
            "@",
            "/",
            "=",
            " ",
            "\n",
            "'",
            "\"",
            "[",
            "]",
            "Z",
            "xyz",
            "_",
            ":",
            "-",
            "a",
            "<your-token>",
        ];
        let mut seed = 0x3f29_1d4a_u64;
        for _ in 0..60 {
            let mut raw = String::new();
            for _ in 0..50 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                raw.push_str(pieces[((seed >> 32) as usize) % pieces.len()]);
            }
            for size in [1, 7, 31] {
                let mut live = LiveRedactor::default();
                let mut end = 0;
                for chunk in raw.as_bytes().chunks(size) {
                    live.append(chunk);
                    end += chunk.len();
                    let expected =
                        redact_credentials_for_display_streaming(&raw[..end], usize::MAX)
                            .unwrap()
                            .0;
                    assert_eq!(
                        live.snapshot().text,
                        expected,
                        "size {size}, prefix {end}, input {raw:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn invalid_bytes_and_incomplete_unicode_preserve_safe_boundaries() {
        let raw = b"\xff hello \xe4\xbd\xa0 http://user:ZZZZZZ@host/ \xfe password=ZZZZ\n";
        let mut live = LiveRedactor::default();
        for end in 1..=raw.len() {
            live.append(&raw[end - 1..end]);
            let complete = complete_utf8_prefix(&raw[..end]);
            let expected = redact_credentials_for_display_streaming(
                &String::from_utf8_lossy(&raw[..complete]),
                usize::MAX,
            )
            .unwrap()
            .0;
            assert_eq!(live.snapshot().text, expected, "byte boundary {end}");
        }
    }

    #[test]
    fn invalid_utf8_inside_a_secret_never_exposes_its_suffix() {
        for raw in [
            &b"password=abc\xffZ\nready\n"[..],
            &b"password=abc\xe4\xbdZ\nready\n"[..],
            &b"https://user:abc\xffZ@example.test/\nready\n"[..],
        ] {
            let mut live = LiveRedactor::default();
            for end in 1..=raw.len() {
                live.append(&raw[end - 1..end]);
                let complete = complete_utf8_prefix(&raw[..end]);
                let expected = redact_credentials_for_display_streaming(
                    &String::from_utf8_lossy(&raw[..complete]),
                    usize::MAX,
                )
                .unwrap()
                .0;
                assert_eq!(live.snapshot().text, expected, "byte boundary {end}");
            }
        }
    }

    #[test]
    fn mixed_invalid_utf8_and_secret_contexts_match_the_canonical_projection() {
        let pieces: &[&[u8]] = &[
            b"password=",
            b"Bearer ",
            b"https://",
            b"user:",
            b"@host/",
            b"--token ",
            b"ghp_",
            b"abc",
            b"Z",
            b"\xff",
            b"\xe4\xbd",
            b"\xa0",
            b"\n",
            b" ",
            b"\"",
        ];
        let mut seed = 0xa893_2c61_u64;
        for _ in 0..120 {
            let mut raw = Vec::new();
            for _ in 0..35 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                raw.extend_from_slice(pieces[(seed >> 32) as usize % pieces.len()]);
            }
            for size in [1, 3, 17] {
                let mut live = LiveRedactor::default();
                let mut end = 0;
                for chunk in raw.chunks(size) {
                    live.append(chunk);
                    end += chunk.len();
                    let complete = complete_utf8_prefix(&raw[..end]);
                    let expected = redact_credentials_for_display_streaming(
                        &String::from_utf8_lossy(&raw[..complete]),
                        usize::MAX,
                    )
                    .unwrap()
                    .0;
                    assert_eq!(live.snapshot().text, expected, "chunk {size}, prefix {end}");
                }
            }
        }
    }

    #[test]
    fn oversized_pem_resumes_after_its_real_end_marker() {
        let mut live = LiveRedactor::default();
        live.append(b"-----BEGIN PRIVATE KEY-----\n");
        for _ in 0..20 {
            live.append(&vec![b'Z'; CHUNK_BYTES]);
        }
        assert!(!live.snapshot().text.contains('Z'));
        live.append(b"\n-----END PRIVATE KEY-----\nREADY\n");
        let output = live.snapshot().text;
        assert!(!output.contains('Z'));
        assert!(output.contains("READY"), "{output:?}");
    }

    #[test]
    fn safe_ring_preserves_utf8_and_redaction_boundaries() {
        let mut live = LiveRedactor::default();
        let mut full = "a".repeat(TAIL_BYTES - 8);
        live.push_safe(&full);
        for piece in ["[REDACTED:SECRET_ASSIGNMENT]", "你", "好", "\n", "x"] {
            live.push_safe(piece);
            full.push_str(piece);
            let cut = safe_tail_start(&full, TAIL_BYTES);
            assert_eq!(live.snapshot().text, full[cut..]);
        }
    }
}
