//! Shared credential redaction and safe edit-reference support for text boundaries.
//!
//! Tool output is redacted before it reaches the model.  An exact-text editor
//! still needs a way to address a redacted span without learning the secret,
//! so markers carry only a process-keyed opaque reference.  The editor resolves
//! that reference against the raw file at execution time and fails closed
//! unless exactly one span matches.

use regex::{Regex, bytes::Regex as BytesRegex};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, OnceLock},
};
use uuid::Uuid;

struct CredentialPattern {
    regex: &'static Regex,
    label: &'static str,
    secret_capture: Option<&'static str>,
}

fn credential_patterns() -> &'static [CredentialPattern] {
    credential_patterns_for_stream(false)
}

pub(crate) fn live_credential_patterns() -> Vec<&'static str> {
    credential_patterns_for_stream(true)
        .iter()
        .map(|pattern| pattern.regex.as_str())
        .chain(std::iter::once(pem_header_regex().as_str()))
        .collect()
}

fn credential_patterns_for_stream(streaming: bool) -> &'static [CredentialPattern] {
    static PATTERNS: OnceLock<Vec<CredentialPattern>> = OnceLock::new();
    static STREAMING_PATTERNS: OnceLock<Vec<CredentialPattern>> = OnceLock::new();

    macro_rules! pat_min {
        ($re:expr, $minimum:expr, $label:expr, $capture:expr) => {{
            static REGULAR: OnceLock<Regex> = OnceLock::new();
            static STREAMING: OnceLock<Regex> = OnceLock::new();
            let (slot, minimum) = if streaming {
                (&STREAMING, 1)
            } else {
                (&REGULAR, $minimum)
            };
            CredentialPattern {
                regex: slot.get_or_init(|| {
                    Regex::new(&format!($re, minimum)).expect("credential pattern regex")
                }),
                label: $label,
                secret_capture: $capture,
            }
        }};
    }

    macro_rules! pat_stream {
        ($regular:expr, $live:expr, $label:expr) => {{
            static REGULAR: OnceLock<Regex> = OnceLock::new();
            static STREAMING: OnceLock<Regex> = OnceLock::new();
            let (slot, pattern) = if streaming {
                (&STREAMING, $live)
            } else {
                (&REGULAR, $regular)
            };
            CredentialPattern {
                regex: slot.get_or_init(|| Regex::new(pattern).expect("credential pattern regex")),
                label: $label,
                secret_capture: None,
            }
        }};
    }

    let patterns = if streaming {
        &STREAMING_PATTERNS
    } else {
        &PATTERNS
    };
    patterns.get_or_init(|| {
        vec![
            pat_stream!(
                r"AKIA[0-9A-Z]{16}",
                r"AKIA[0-9A-Z]+",
                "AWS_ACCESS_KEY"
            ),
            pat_min!(
                r#"(?i)(?:aws_secret_access_key|aws_secret_key|\[\s*['"]?(?:aws_secret_access_key|aws_secret_key)['"]?\s*\]|['"](?:aws_secret_access_key|aws_secret_key)['"])\s*[=:]\s*['"]?(?P<secret>[A-Za-z0-9/+=._-]{{{},}})"#,
                30,
                "AWS_SECRET_KEY",
                Some("secret")
            ),
            pat_min!(
                r#"(?i)(?:--?(?:token|api[-_]?key|access[-_]?token|auth[-_]?token))(?:\s+|=)\s*(?:\\*['"])?(?P<secret>[A-Za-z0-9._\-/+=]{{{},}})"#,
                20,
                "TOKEN_ARGUMENT",
                Some("secret")
            ),
            pat_stream!(
                r"gh[pousr]_[A-Za-z0-9_]{36,}",
                r"gh[pousr]_[A-Za-z0-9_]+",
                "GITHUB_TOKEN"
            ),
            // Hugging Face user-access tokens are opaque bearer credentials.
            // The public `hf_` prefix is fixed while the body is
            // case-sensitive alphanumeric data; accept the documented
            // minimum body length so a direct tool output cannot expose one
            // merely because it lacks an assignment/CLI flag wrapper.
            pat_stream!(
                r"hf_[A-Za-z0-9]{34,}",
                r"hf_[A-Za-z0-9]+",
                "HUGGINGFACE_TOKEN"
            ),
            pat_min!(
                r"(?i)Bearer\s+[A-Za-z0-9._\-/+=]{{{},}}",
                40,
                "BEARER_TOKEN",
                None
            ),
            CredentialPattern {
                // URI userinfo can be empty, contain multiple colons, or be
                // a token without a password. A later @ can extend userinfo,
                // so consume through the last @ before the authority ends.
                regex: {
                    static RE: OnceLock<Regex> = OnceLock::new();
                    RE.get_or_init(|| Regex::new(r"://[^/?#\s]*@").expect("URL authority regex"))
                },
                label: "CONNECTION_CREDENTIAL",
                secret_capture: None,
            },
            pat_min!(
                r#"(?i)(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key|\[\s*['"]?(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)['"]?\s*\]|['"](?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)['"])\s*[=:]\s*['"]?(?P<secret>[^\s'"]{{{},}})"#,
                12,
                "SECRET_ASSIGNMENT",
                Some("secret")
            ),
        ]
    })
}

fn credential_secret_matches<'text: 'pattern, 'pattern>(
    text: &'text str,
    pattern: &'pattern CredentialPattern,
    envelopes: &'pattern RedactionEnvelopeIndex,
    overlapping: bool,
) -> impl Iterator<Item = regex::Match<'text>> + 'pattern {
    let matches: Box<dyn Iterator<Item = regex::Match<'text>> + 'pattern> = if overlapping {
        // A live match can consume the beginning of another context,
        // e.g. `Bearer xBearer `, whose value arrives after the first
        // match. Revisit possible starts near the end of each match.
        // Context keywords are short, so this lookback remains bounded
        // for long values. Bare token patterns consume their entire
        // character run and do not need overlapping matching.
        const LOOKBACK: usize = 320;
        let mut next = 0;
        Box::new(std::iter::from_fn(move || {
            if next > text.len() {
                return None;
            }
            let captures = pattern.regex.captures_at(text, next)?;
            let whole = captures.get(0)?;
            let matched = pattern
                .secret_capture
                .and_then(|name| captures.name(name))
                .unwrap_or(whole);
            next = whole.end().saturating_sub(LOOKBACK).max(whole.start() + 1);
            while next < text.len() && !text.is_char_boundary(next) {
                next += 1;
            }
            Some(matched)
        }))
    } else if let Some(secret_capture) = pattern.secret_capture {
        Box::new(
            pattern
                .regex
                .captures_iter(text)
                .filter_map(move |captures| captures.name(secret_capture)),
        )
    } else {
        Box::new(pattern.regex.find_iter(text))
    };
    matches.filter(|matched| {
        !envelopes.contains(matched.start(), matched.end())
            && !credential_match_is_explicit_placeholder(matched.as_str())
    })
}

/// Deliberate documentation/configuration placeholders are public contract
/// text, not credentials. Redacting them makes a secret-removal workflow
/// impossible to verify and can drive an agent into repeatedly rewriting an
/// already-safe value. Keep this grammar intentionally narrow: only a whole
/// lower-case `<your-...>` value is exempt, never a prefix/suffix containing
/// additional bytes.
fn is_explicit_placeholder_value(value: &str) -> bool {
    let value = value.trim().trim_matches(['\'', '"']);
    let Some(body) = value
        .strip_prefix("<your-")
        .and_then(|value| value.strip_suffix('>'))
    else {
        return false;
    };
    (3..=128).contains(&body.len())
        && body.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-._".contains(&byte)
        })
}

fn credential_match_is_explicit_placeholder(matched: &str) -> bool {
    if let Some(authority) = matched
        .strip_prefix("://")
        .and_then(|value| value.strip_suffix('@'))
    {
        if authority.contains('@') {
            return false;
        }
        return match authority.split_once(':') {
            Some((username, password)) => {
                (username.is_empty() || is_explicit_placeholder_value(username))
                    && is_explicit_placeholder_value(password)
            }
            None => is_explicit_placeholder_value(authority),
        };
    }
    matched
        .rfind(['=', ':'])
        .is_some_and(|separator| is_explicit_placeholder_value(&matched[separator + 1..]))
        || is_explicit_placeholder_value(matched)
}

const PEM_MAX_BYTES: usize = 64 * 1024;

fn pem_header_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"-----BEGIN [A-Z ]{0,64}PRIVATE KEY-----").expect("PEM header regex")
    })
}

fn pem_end_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"-----END [A-Z ]{0,64}PRIVATE KEY-----").expect("PEM end regex"))
}

fn pem_header_bytes_regex() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| {
        BytesRegex::new(r"-----BEGIN [A-Z ]{0,64}PRIVATE KEY-----").expect("PEM header bytes regex")
    })
}

fn pem_end_bytes_regex() -> &'static BytesRegex {
    static RE: OnceLock<BytesRegex> = OnceLock::new();
    RE.get_or_init(|| {
        BytesRegex::new(r"-----END [A-Z ]{0,64}PRIVATE KEY-----").expect("PEM end bytes regex")
    })
}

/// Return private-key header spans in a bounded presentation fragment.
///
/// This is intentionally metadata-only: callers use the spans to decide
/// whether a head/tail window may have omitted the beginning of a PEM block;
/// no credential bytes are retained by this helper.
pub fn private_key_header_ranges(text: &str) -> Vec<(usize, usize)> {
    private_key_header_markers(text)
        .into_iter()
        .map(|(start, end, _)| (start, end))
        .collect()
}

/// Return private-key header spans together with their normalized marker kind.
/// The kind is metadata only and lets a bounded stream scanner pair a header
/// with the correct END marker without retaining key material.
pub fn private_key_header_markers(text: &str) -> Vec<(usize, usize, String)> {
    pem_header_regex()
        .find_iter(text)
        .filter_map(|matched| {
            pem_marker_kind(matched.as_str(), "-----BEGIN ")
                .map(|kind| (matched.start(), matched.end(), kind.to_string()))
        })
        .collect()
}

/// Return private-key END marker spans together with their normalized kind.
pub fn private_key_end_markers(text: &str) -> Vec<(usize, usize, String)> {
    pem_end_regex()
        .find_iter(text)
        .filter_map(|matched| {
            pem_marker_kind(matched.as_str(), "-----END ")
                .map(|kind| (matched.start(), matched.end(), kind.to_string()))
        })
        .collect()
}

/// Byte-oriented counterpart used by streaming output owners.  PEM markers
/// are ASCII, so their offsets remain raw-byte offsets even when arbitrary
/// binary output surrounds them.
pub fn private_key_header_markers_bytes(text: &[u8]) -> Vec<(usize, usize, String)> {
    overlapping_pem_markers(text, pem_header_bytes_regex(), "-----BEGIN ")
}

/// Byte-oriented counterpart to [`private_key_end_markers`].
pub fn private_key_end_markers_bytes(text: &[u8]) -> Vec<(usize, usize, String)> {
    overlapping_pem_markers(text, pem_end_bytes_regex(), "-----END ")
}

fn overlapping_pem_markers(
    text: &[u8],
    regex: &BytesRegex,
    prefix: &str,
) -> Vec<(usize, usize, String)> {
    let mut markers = Vec::new();
    let mut next = 0;
    while let Some(matched) = regex.find_at(text, next) {
        if let Ok(marker) = std::str::from_utf8(matched.as_bytes())
            && let Some(kind) = pem_marker_kind(marker, prefix)
        {
            markers.push((matched.start(), matched.end(), kind.to_string()));
        }
        next = matched.end().saturating_sub(5).max(matched.start() + 1);
    }
    markers
}

/// Return the PEM type carried by a complete BEGIN/END marker, for example
/// `RSA PRIVATE KEY`.  Matching the type is part of the safety boundary: an
/// unrelated END marker must never terminate a private-key span early.
fn pem_marker_kind<'a>(marker: &'a str, prefix: &str) -> Option<&'a str> {
    marker
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix("-----"))
        .filter(|value| {
            !value.is_empty()
                && value
                    .chars()
                    .all(|character| character.is_ascii_uppercase() || character == ' ')
                && value.ends_with("PRIVATE KEY")
        })
}

const MARKER_PREFIX: &str = "[REDACTED:";
const REDACTION_REFERENCE_ERROR: &str = "Error: redaction reference is invalid, stale, forged, or ambiguous. Re-read the target and preserve one complete marker with its surrounding non-secret context.";

fn reference_key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        // Process-local keying makes markers opaque bearer capabilities. A
        // marker copied into another process or after restart cannot be
        // resolved; within this process it remains intentionally short-lived
        // and is still gated by exact single-match source resolution.
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let mut key = [0_u8; 32];
        key[..16].copy_from_slice(first.as_bytes());
        key[16..].copy_from_slice(second.as_bytes());
        key
    })
}

fn marker(label: &str, value: &str) -> String {
    let label = marker_label_code(label).expect("credential pattern has marker code");
    let nonce = Uuid::new_v4().simple().to_string();
    let value_mac = marker_value_mac(label, &nonce, value);
    // The value MAC is intentionally not independently verifiable: it binds
    // the opaque reference to the raw source bytes at edit time. The issuer
    // tag proves provenance during later display passes without revealing or
    // storing the raw value in a global registry.
    let issuer_mac = marker_issuer_mac(label, &nonce);
    format!("{MARKER_PREFIX}{label}:{nonce}:{value_mac}:{issuer_mac}]")
}

/// Redact high-confidence credential patterns before model context.
///
/// The marker contains an opaque, process-keyed reference so an exact-text
/// editor can later resolve a redacted anchor against the raw file without
/// exposing the value or providing an offline guess oracle.
pub fn redact_credentials_in_text(text: &str) -> (String, usize) {
    render_redaction(text, true, false, false, usize::MAX).expect("unbounded edit projection")
}

/// Redact text at a presentation boundary without minting edit authority.
pub fn redact_credentials_for_display(text: &str) -> (String, usize) {
    render_redaction(text, false, false, false, usize::MAX).expect("unbounded display projection")
}

/// Bound intermediate display projections as well as their final size.
pub fn redact_credentials_for_display_bounded(
    text: &str,
    max_bytes: usize,
) -> Result<(String, usize), &'static str> {
    render_redaction(text, false, false, false, max_bytes)
}

/// Redact a growing preview conservatively before slicing it for display.
pub fn redact_credentials_for_display_streaming(
    text: &str,
    max_bytes: usize,
) -> Result<(String, usize), &'static str> {
    render_redaction(text, false, true, false, max_bytes)
}

fn render_redaction(
    text: &str,
    edit_capable: bool,
    streaming: bool,
    partial: bool,
    max_bytes: usize,
) -> Result<(String, usize), &'static str> {
    if text.len() > max_bytes {
        return Err("Safe output projection exceeds the bounded display limit.");
    }
    let mut result = String::new();
    let mut cursor = 0;
    let mut total = 0;
    for_each_redaction_span(text, streaming, partial, |span| {
        append_safe_projection(&mut result, &text[cursor..span.start], max_bytes)?;
        if edit_capable && span.editable {
            append_safe_projection(
                &mut result,
                &marker(span.label, &text[span.start..span.end]),
                max_bytes,
            )?;
        } else {
            append_safe_projection(&mut result, MARKER_PREFIX, max_bytes)?;
            append_safe_projection(&mut result, span.label, max_bytes)?;
            append_safe_projection(&mut result, "]", max_bytes)?;
        }
        cursor = span.end;
        total += 1;
        Ok(())
    })?;
    append_safe_projection(&mut result, &text[cursor..], max_bytes)?;
    Ok((result, total))
}

fn append_safe_projection(
    output: &mut String,
    text: &str,
    limit: usize,
) -> Result<(), &'static str> {
    let needed = output
        .len()
        .checked_add(text.len())
        .filter(|size| *size <= limit)
        .ok_or("Safe output projection exceeds the bounded display limit.")?;
    if needed > output.capacity() {
        let capacity = output.capacity().saturating_mul(2).max(needed).min(limit);
        output.reserve_exact(capacity - output.len());
    }
    output.push_str(text);
    Ok(())
}

/// A complete BEGIN sentinel with an unfinished label is not yet classifiable.
/// Once the full header is present, `pem_spans` owns the entire key body.
fn pending_pem_header(text: &str) -> Option<usize> {
    const BEGIN: &str = "-----BEGIN ";
    if let Some(start) = text.rfind(BEGIN) {
        let rest = &text[start + BEGIN.len()..];
        let complete = pem_header_regex()
            .find_at(text, start)
            .is_some_and(|matched| matched.start() == start);
        if !complete {
            let letters = rest
                .bytes()
                .take_while(|b| b.is_ascii_uppercase() || *b == b' ')
                .count();
            let suffix = &rest[letters..];
            if letters <= 75
                && (suffix.is_empty()
                    || (rest[..letters].ends_with("PRIVATE KEY")
                        && suffix.len() < 5
                        && suffix.bytes().all(|b| b == b'-')))
            {
                return Some(start);
            }
        }
    }
    None
}

fn pending_url_authority(text: &str) -> Option<usize> {
    let start = text.rfind("://")?;
    let authority = &text[start + 3..];
    if authority
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '/' | '?' | '#'))
    {
        return None;
    }
    // Even a previous @ can still be part of userinfo if more bytes arrive.
    Some(start)
}

pub(crate) fn pending_display_start(text: &str) -> Option<usize> {
    pending_pem_header(text)
        .into_iter()
        .chain(pending_url_authority(text))
        .min()
}

#[derive(Clone, Copy)]
struct RedactionSpan {
    start: usize,
    end: usize,
    label: &'static str,
    editable: bool,
}

/// Merge overlapping rules on the original text before replacing any bytes.
/// The same groups are used by presentation, line windows and edit references.
fn for_each_redaction_span(
    text: &str,
    streaming: bool,
    partial: bool,
    mut emit: impl FnMut(RedactionSpan) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    let patterns = credential_patterns_for_stream(streaming);
    let envelopes = redaction_envelope_index(text);
    let mut streams: Vec<_> = patterns
        .iter()
        .map(|pattern| {
            let overlapping = pattern.secret_capture.is_some()
                || pattern.label == "BEARER_TOKEN"
                || (!streaming && pattern.label == "AWS_ACCESS_KEY");
            credential_secret_matches(text, pattern, &envelopes, overlapping).peekable()
        })
        .collect();
    let mut pem = pem_spans(text).peekable();
    let mut extra = Vec::new();
    if partial {
        let first_header = pem_header_regex().find(text).map(|m| m.start());
        let mut search_from = 0;
        let mut last_end = None;
        while let Some(end) = pem_end_regex().find_at(text, search_from) {
            if first_header.is_some_and(|header| header <= end.start()) {
                break;
            }
            last_end = Some(first_header.map_or(end.end(), |header| end.end().min(header)));
            search_from = end.end() - 5;
        }
        if let Some(end) = last_end {
            extra.push(RedactionSpan {
                start: 0,
                end,
                label: "PRIVATE_KEY",
                editable: false,
            });
        }
    }
    if streaming {
        if let Some(start) = pending_pem_header(text) {
            extra.push(RedactionSpan {
                start,
                end: text.len(),
                label: "PRIVATE_KEY",
                editable: false,
            });
        }
        if let Some(start) = pending_url_authority(text) {
            extra.push(RedactionSpan {
                start,
                end: text.len(),
                label: "URL_AUTHORITY_PENDING",
                editable: false,
            });
        }
    }
    extra.sort_unstable_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut extra = extra.into_iter().peekable();
    let mut active: Option<RedactionSpan> = None;
    loop {
        let mut selected = streams
            .iter_mut()
            .enumerate()
            .filter_map(|(i, stream)| {
                stream
                    .peek()
                    .map(|m| (m.start(), std::cmp::Reverse(m.end()), i))
            })
            .min();
        if let Some(span) = pem.peek() {
            let key = (span.start, std::cmp::Reverse(span.end), patterns.len());
            if selected.is_none_or(|current| key < current) {
                selected = Some(key);
            }
        }
        if let Some(span) = extra.peek() {
            let key = (span.start, std::cmp::Reverse(span.end), patterns.len() + 1);
            if selected.is_none_or(|current| key < current) {
                selected = Some(key);
            }
        }
        let Some((_, _, source)) = selected else {
            break;
        };
        let span = if source < patterns.len() {
            let matched = streams[source].next().expect("peeked credential");
            RedactionSpan {
                start: matched.start(),
                end: matched.end(),
                label: patterns[source].label,
                editable: true,
            }
        } else if source == patterns.len() {
            let span = pem.next().expect("peeked PEM");
            RedactionSpan {
                start: span.start,
                end: span.end,
                label: "PRIVATE_KEY",
                editable: span.complete,
            }
        } else {
            extra.next().expect("peeked partial span")
        };
        if let Some(current) = active.as_mut()
            && span.start < current.end
        {
            current.end = current.end.max(span.end);
            current.editable &= span.editable;
            if span.label == "PRIVATE_KEY" {
                current.label = "PRIVATE_KEY";
            }
        } else {
            if let Some(mut previous) = active.replace(span) {
                if previous.label == "PRIVATE_KEY" && previous.end - previous.start > PEM_MAX_BYTES
                {
                    previous.editable = false;
                }
                emit(previous)?;
            }
        }
    }
    if let Some(mut last) = active {
        if last.label == "PRIVATE_KEY" && last.end - last.start > PEM_MAX_BYTES {
            last.editable = false;
        }
        emit(last)?;
    }
    Ok(())
}

/// Redact a bounded tail/head capture whose first byte may be in the middle
/// of a PEM block. An orphan END marker is treated as key material only for
/// this explicitly partial-input path; complete source/RPC views use the
/// ordinary display function and preserve unrelated documentation footers.
pub fn redact_credentials_for_display_partial(text: &str) -> (String, usize) {
    render_redaction(text, false, false, true, usize::MAX).expect("unbounded display projection")
}

struct PemSpan {
    start: usize,
    end: usize,
    complete: bool,
}

/// One source of truth for full reads, line windows and edit references.
fn pem_spans(text: &str) -> impl Iterator<Item = PemSpan> + '_ {
    let mut cursor = 0;
    std::iter::from_fn(move || {
        let header = pem_header_regex().find_at(text, cursor)?;
        let (mut end, mut complete) = pem_block_end(text, header);
        // END and a following BEGIN can share five dashes. Treat all
        // overlapping blocks as one source span and one edit reference.
        while complete && end < text.len() {
            let Some(next) = pem_header_regex().find_at(text, end.saturating_sub(5)) else {
                break;
            };
            if next.start() >= end {
                break;
            }
            (end, complete) = pem_block_end(text, next);
        }
        cursor = end;
        Some(PemSpan {
            start: header.start(),
            end,
            complete,
        })
    })
}

fn pem_block_end(text: &str, header: regex::Match<'_>) -> (usize, bool) {
    let max_end = header.start().saturating_add(PEM_MAX_BYTES);
    if let Some(end) = pem_end_regex().find_at(text, header.end())
        && end.end() <= max_end
        && pem_marker_kind(end.as_str(), "-----END ")
            == pem_marker_kind(header.as_str(), "-----BEGIN ")
        && pem_header_regex()
            .find_at(text, header.start() + 1)
            .is_none_or(|nested| nested.start() >= end.start())
    {
        return (end.end(), true);
    }
    // Missing, mismatched, nested or oversized blocks cannot delimit key data.
    (text.len(), false)
}

const DISPLAY_SECRET_FIELD_MARKER: &str = "[REDACTED:SECRET_FIELD]";

/// Apply the non-owning display boundary to every string nested in a JSON
/// value. Callback metadata is extensible, so sanitizing only the top-level
/// output would leave status/fields usable as a raw secret lane. Object keys
/// provide an additional provider-neutral signal for structured credentials;
/// low-confidence keys such as pagination/revision tokens are deliberately
/// excluded to avoid turning ordinary cursors into secrets.
pub fn redact_credentials_in_json(value: &mut Value) -> usize {
    redact_json_value(value, None)
}

/// Redact one structured field while retaining its key-aware fallback. This
/// is used by protocol boundaries that walk a message graph themselves (for
/// example, to skip an already-normalized nested tool-argument document).
pub fn redact_credentials_in_json_field(value: &mut Value, key: &str) -> usize {
    redact_json_value(value, Some(key))
}

fn redact_json_value(value: &mut Value, object_key: Option<&str>) -> usize {
    match value {
        Value::String(text) => {
            let original = text.clone();
            let (redacted, mut count) = redact_credentials_for_display(&original);
            *text = redacted;
            // A generic JSON boundary must not parse protocol-specific
            // `function.arguments`, but it still has to redact credential
            // syntax contained in that string.  The assistant-message
            // boundary parses valid tool arguments separately; this fallback
            // only performs display redaction and never rewrites JSON inside a
            // string.
            let exact_opaque_reference = is_exact_opaque_reference(&original);
            if count == 0
                && !exact_opaque_reference
                && !is_explicit_placeholder_value(&original)
                && is_high_confidence_secret_key(object_key, &original)
                && !original.trim().is_empty()
            {
                *text = DISPLAY_SECRET_FIELD_MARKER.to_string();
                count = 1;
            }
            count
        }
        Value::Array(values) => values
            .iter_mut()
            .map(|value| redact_json_value(value, None))
            .sum(),
        Value::Object(values) => {
            let mut count = 0;
            for (key, child) in values.iter_mut() {
                count += redact_json_value(child, Some(key));
            }
            count
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => 0,
    }
}

fn is_high_confidence_secret_key(key: Option<&str>, value: &str) -> bool {
    let Some(key) = key else {
        return false;
    };
    let normalized = normalize_json_key(key);
    // Internal artifact ownership fields are reserved protocol metadata. They
    // are exempt only when a canonical tool-message boundary has validated
    // them; any nested/invalid occurrence must be treated as sensitive data,
    // rather than becoming a bypass merely because the key is implementation
    // specific.
    if matches!(
        normalized.as_str(),
        "_astra_tool_result_run_id" | "_astra_tool_result_artifact"
    ) {
        return true;
    }
    if normalized.is_empty() || is_metadata_key(&normalized) {
        return false;
    }
    if normalized == "token" {
        return credential_value_shape(value);
    }
    if matches!(
        normalized.as_str(),
        "api_key"
            | "apikey"
            | "access_key"
            | "access_token"
            | "auth_token"
            | "authorization"
            | "password"
            | "passwd"
            | "secret"
            | "secret_key"
            | "secret_access_key"
            | "client_secret"
            | "private_key"
            | "refresh_token"
            | "id_token"
            | "bearer_token"
            | "credential"
    ) {
        return true;
    }

    // `credentials` is also a typed policy-state field in runtime
    // environment advertisements.  Only the finite, serde-stable enum
    // values are exempt.  Shape heuristics are intentionally not used here:
    // short or punctuation-bearing secrets are still secrets, and a generic
    // JSON boundary must fail closed for every value outside the protocol
    // enum.
    if normalized == "credentials" {
        return !matches!(value, "disabled" | "user_approved" | "scoped_injection");
    }

    // Prefixes such as `x-` and `provider-` are common in transport
    // metadata.  Treat only the explicit API-key family as authoritative;
    // arbitrary `*_key` names are ordinary domain data surprisingly often.
    if normalized.ends_with("_api_key") || normalized.ends_with("_access_key") {
        return true;
    }

    // Keep the heuristic bounded.  A name containing these words is not
    // enough by itself (`secret_name`, `credential_id`); require a plausible
    // opaque value after excluding metadata qualifiers.  Likewise, generic
    // `*_token` fields include design/pagination tokens and need shape
    // evidence plus a credential-oriented prefix.
    if normalized.contains("secret") || normalized.contains("credential") {
        return credential_value_shape(value);
    }
    if normalized.ends_with("_token") {
        let prefix = normalized.trim_end_matches("_token");
        let credential_prefix = [
            "access", "auth", "bearer", "client", "csrf", "id", "oauth", "refresh", "service",
            "session", "api", "hf",
        ];
        return credential_prefix
            .iter()
            .any(|candidate| prefix == *candidate || prefix.ends_with(&format!("_{candidate}")))
            && credential_value_shape(value);
    }
    false
}

fn is_metadata_key(normalized: &str) -> bool {
    [
        "_id",
        "_name",
        "_version",
        "_path",
        "_type",
        "_reference",
        "_ref",
        "_label",
        "_description",
        "_url",
    ]
    .iter()
    .any(|suffix| normalized.ends_with(suffix))
        || normalized.starts_with("page_")
        || normalized.starts_with("cursor_")
        || normalized.starts_with("revision_")
        || normalized.starts_with("continuation_")
        || normalized.starts_with("offset_")
        || matches!(
            normalized,
            "page" | "cursor" | "revision" | "continuation" | "offset"
        )
}

fn normalize_json_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous_was_lower_or_digit = false;
    for ch in key.trim().chars() {
        if ch.is_ascii_uppercase() && previous_was_lower_or_digit {
            normalized.push('_');
        }
        if ch == '-' || ch == ' ' {
            normalized.push('_');
            previous_was_lower_or_digit = false;
        } else {
            normalized.push(ch.to_ascii_lowercase());
            previous_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    normalized
}

fn credential_value_shape(value: &str) -> bool {
    let value = value.trim();
    value.len() >= 20
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ".:/_+=-".contains(ch))
}

/// Redact a line window using the same source groups as a complete read.
/// Complete single-line credentials retain edit references; a group crossing
/// a line boundary or containing a PEM is represented by a display marker.
pub fn redact_line_window(raw: &str, start_line: usize, end_line: usize) -> String {
    let start_line = start_line.max(1);
    if end_line < start_line {
        return String::new();
    }
    let mut selected_start = None;
    let mut selected_end = 0;
    let mut offset = 0;
    for (index, segment) in raw.split_inclusive('\n').enumerate() {
        let line = index + 1;
        if line == start_line {
            selected_start = Some(offset);
        }
        offset += segment.len();
        if line >= start_line {
            selected_end = offset;
        }
        if line >= end_line {
            break;
        }
    }
    let Some(selected_start) = selected_start else {
        return String::new();
    };
    let mut groups = Vec::new();
    for_each_redaction_span(raw, false, false, |span| {
        if span.end > selected_start && span.start < selected_end {
            groups.push(span);
        }
        Ok(())
    })
    .expect("unbounded redaction scan");

    let mut output = Vec::new();
    let mut group_index = 0;
    let mut line_start = selected_start;
    for segment in raw[selected_start..selected_end].split_inclusive('\n') {
        let line = segment
            .strip_suffix('\n')
            .map_or(segment, |line| line.strip_suffix('\r').unwrap_or(line));
        let line_end = line_start + line.len();
        let segment_end = line_start + segment.len();
        while group_index < groups.len() && groups[group_index].end <= line_start {
            group_index += 1;
        }
        let mut last = group_index;
        while last < groups.len() && groups[last].start < segment_end {
            last += 1;
        }
        let relevant = &groups[group_index..last];
        if let Some(span) = relevant.iter().find(|span| {
            span.label == "PRIVATE_KEY"
                || !span.editable
                || span.start < line_start
                || span.end > line_end
        }) {
            output.push(format!("{MARKER_PREFIX}{}]", span.label));
        } else {
            let mut rendered = String::new();
            let mut cursor = line_start;
            for span in relevant {
                rendered.push_str(&raw[cursor..span.start]);
                rendered.push_str(&marker(span.label, &raw[span.start..span.end]));
                cursor = span.end;
            }
            rendered.push_str(&raw[cursor..line_end]);
            output.push(rendered);
        }
        line_start = segment_end;
    }
    output.join("\n")
}

/// Truncate already-redacted text without cutting through an executor marker.
/// Raw credentials must be removed before this function is called; its second
/// invariant is preserving a complete edit anchor for the bytes that remain.
pub fn truncate_redacted_output(mut output: String, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output;
    }
    let end = output.floor_char_boundary(max_bytes);
    let mut cut = output[..end]
        .rfind('\n')
        .filter(|&pos| pos > end / 2)
        .map(|pos| pos + 1)
        .unwrap_or(end);
    for (start, end) in redaction_marker_ranges(&output) {
        if start < cut && cut < end {
            cut = start;
        }
    }
    output.truncate(cut);
    output.push_str("\n[truncated]");
    output
}

/// Truncate a presentation string using head+tail semantics while treating
/// both edit-capable and display-only redaction markers as atomic spans.
/// This is used by bounded transports (RPC/run_script): a marker split at a
/// window boundary is not a useful reference and can also re-expose a secret
/// fragment on the next page.
pub fn truncate_redacted_head_tail(output: &str, max_bytes: usize) -> String {
    if output.len() <= max_bytes {
        return output.to_string();
    }
    if max_bytes == 0 {
        return format!(
            "... [OUTPUT TRUNCATED — {} bytes omitted out of {} total] ...",
            output.len(),
            output.len()
        );
    }

    let head_target = (max_bytes as f64 * 0.4) as usize;
    let tail_target = max_bytes.saturating_sub(head_target);
    let mut head_end = output.floor_char_boundary(head_target);
    let mut tail_start = output.ceil_char_boundary(output.len().saturating_sub(tail_target));
    let ranges = redaction_marker_ranges(output);

    // Expand a boundary to include a marker when that does not overlap the
    // opposite side; otherwise drop the partial marker rather than emitting a
    // fragment.  The result may use slightly fewer than `max_bytes`, which is
    // preferable to breaking the transport/edit contract.
    for (start, end) in &ranges {
        if *start < head_end && head_end < *end {
            if *end <= tail_start {
                head_end = *end;
            } else {
                head_end = *start;
            }
        }
        if *start < tail_start && tail_start < *end {
            if head_end <= *start {
                tail_start = *start;
            } else {
                tail_start = *end;
            }
        }
    }
    if head_end > tail_start {
        tail_start = head_end;
    }

    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let omitted = output.len().saturating_sub(head.len() + tail.len());
    format!(
        "{head}\n\n... [OUTPUT TRUNCATED — {omitted} bytes omitted out of {} total] ...\n\n{tail}",
        output.len()
    )
}

/// Character-budget counterpart used by the model-message boundary.  Rust's
/// string slices are byte-indexed, while the model contract is expressed in
/// characters; keep the same atomic-marker rules without turning a CJK/emoji
/// boundary into a byte overrun.
pub fn truncate_redacted_head_tail_chars(output: &str, max_chars: usize) -> String {
    let total_chars = output.chars().count();
    if total_chars <= max_chars {
        return output.to_string();
    }
    if max_chars == 0 {
        return "[… truncated output …]".to_string();
    }
    let head_chars = max_chars * 2 / 5;
    let tail_chars = max_chars.saturating_sub(head_chars);
    let mut head_end = output
        .char_indices()
        .nth(head_chars)
        .map(|(index, _)| index)
        .unwrap_or(output.len());
    let tail_start_char = total_chars.saturating_sub(tail_chars);
    let mut tail_start = output
        .char_indices()
        .nth(tail_start_char)
        .map(|(index, _)| index)
        .unwrap_or(output.len());
    for (start, end) in redaction_marker_ranges(output) {
        if start < head_end && head_end < end {
            if end <= tail_start {
                head_end = end;
            } else {
                head_end = start;
            }
        }
        if start < tail_start && tail_start < end {
            if head_end <= start {
                tail_start = start;
            } else {
                tail_start = end;
            }
        }
    }
    if head_end > tail_start {
        tail_start = head_end;
    }
    let head = &output[..head_end];
    let tail = &output[tail_start..];
    let omitted = total_chars.saturating_sub(head.chars().count() + tail.chars().count());
    format!("{head}\n\n[… truncated {omitted} characters …]\n\n{tail}")
}

/// Return a bounded window from a non-owning presentation stream.
///
/// The input is redacted before offsets are interpreted.  This is important
/// for restored/background/RPC output: an offset copied from a raw stream can
/// otherwise land in the middle of a credential and make the remaining bytes
/// unrecognisable to the matcher.  A window never contains a partial redaction
/// marker.  Offsets and totals in the return value are in the safe view.
pub fn redacted_output_window(
    raw: &str,
    offset: usize,
    max_bytes: usize,
) -> (String, usize, usize, usize) {
    let (safe, _) = redact_credentials_for_display(raw);
    SafeOutputProjection::new(safe).window(offset, max_bytes)
}

/// A reusable, display-safe presentation view.
///
/// The projection owns only redacted text and precomputes its marker spans and
/// line count. Consumers that page a large artifact can therefore reuse one
/// immutable view instead of rescanning and copying the complete string for
/// every window. Raw credential bytes never enter this type.
#[derive(Clone, Debug)]
pub struct SafeOutputProjection {
    text: Arc<str>,
    marker_ranges: Arc<Vec<(usize, usize)>>,
    total_lines: usize,
}

impl SafeOutputProjection {
    pub fn new(text: String) -> Self {
        Self::try_new_bounded(text, usize::MAX).expect("unbounded projection")
    }

    /// Bound both the safe text and its marker index before caching or paging.
    pub fn try_new_bounded(text: String, max_heap_bytes: usize) -> Result<Self, &'static str> {
        let max_markers = max_heap_bytes
            .checked_sub(text.len())
            .ok_or("Safe output projection exceeds the bounded display limit.")?
            / std::mem::size_of::<(usize, usize)>();
        let ranges = redaction_marker_ranges_bounded(&text, max_markers)?;
        let total_lines = text.lines().count();
        Ok(Self {
            text: Arc::<str>::from(text),
            marker_ranges: Arc::new(ranges),
            total_lines,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn total_bytes(&self) -> usize {
        self.text.len()
    }

    pub fn heap_bytes(&self) -> usize {
        self.text.len().saturating_add(
            self.marker_ranges
                .capacity()
                .saturating_mul(std::mem::size_of::<(usize, usize)>()),
        )
    }

    pub fn total_lines(&self) -> usize {
        self.total_lines
    }

    pub fn window(&self, offset: usize, max_bytes: usize) -> (String, usize, usize, usize) {
        let safe = self.text();
        let total_bytes = safe.len();
        if max_bytes == 0 || safe.is_empty() {
            let start = offset.min(total_bytes);
            return (String::new(), start, total_bytes, self.total_lines);
        }

        // A tiny page can skip one marker and land in the next adjacent
        // marker; normalize repeatedly so no page ever returns a partial
        // marker.
        let mut start = safe.ceil_char_boundary(offset.min(total_bytes));
        loop {
            if let Some((_, marker_end)) = self
                .marker_ranges
                .iter()
                .find(|(marker_start, marker_end)| *marker_start < start && start < *marker_end)
            {
                start = *marker_end;
                continue;
            }
            if start >= total_bytes {
                return (String::new(), total_bytes, total_bytes, self.total_lines);
            }

            let mut end =
                safe.floor_char_boundary(start.saturating_add(max_bytes).min(total_bytes));
            if let Some((marker_start, marker_end)) = self
                .marker_ranges
                .iter()
                .find(|(marker_start, marker_end)| *marker_start < end && end < *marker_end)
            {
                if *marker_start <= start {
                    start = *marker_end;
                    continue;
                }
                end = *marker_start;
            }
            if end < start {
                end = start;
            }
            return (
                safe[start..end].to_string(),
                end,
                total_bytes,
                self.total_lines,
            );
        }
    }
}

/// Window an already display-safe string.  Callers that own a cached safe
/// projection can avoid rescanning the same artifact on every page while
/// preserving the exact marker/offset contract of [`redacted_output_window`].
pub fn safe_output_window(
    safe: &str,
    offset: usize,
    max_bytes: usize,
) -> (String, usize, usize, usize) {
    SafeOutputProjection::new(safe.to_string()).window(offset, max_bytes)
}

/// Return a display-safe tail without cutting through a redaction marker.
/// This is used for compact local-agent summaries where a head+tail window
/// would change the established tail-only UX.
pub fn redacted_output_tail_chars(raw: &str, max_chars: usize) -> String {
    let (safe, _) = redact_credentials_for_display(raw);
    let total_chars = safe.chars().count();
    if total_chars <= max_chars {
        return safe;
    }
    let mut start = safe
        .char_indices()
        .nth(total_chars.saturating_sub(max_chars))
        .map(|(index, _)| index)
        .unwrap_or(0);
    for (marker_start, marker_end) in redaction_marker_ranges(&safe) {
        if marker_start < start && start < marker_end {
            start = marker_end;
            break;
        }
    }
    safe[start..].to_string()
}

fn redaction_marker_ranges(text: &str) -> Vec<(usize, usize)> {
    redaction_marker_ranges_bounded(text, usize::MAX).expect("unbounded marker index")
}

fn redaction_marker_ranges_bounded(
    text: &str,
    max_markers: usize,
) -> Result<Vec<(usize, usize)>, &'static str> {
    let mut ranges = Vec::<(usize, usize)>::new();
    let mut opaque = marker_regex().find_iter(text).peekable();
    let mut display = display_marker_regex().find_iter(text).peekable();
    loop {
        let marker = match (opaque.peek(), display.peek()) {
            (Some(a), Some(b)) if a.start() <= b.start() => opaque.next(),
            (Some(_), Some(_)) => display.next(),
            (Some(_), None) => opaque.next(),
            (None, Some(_)) => display.next(),
            (None, None) => break,
        }
        .expect("peeked marker");
        if let Some((_, end)) = ranges.last_mut()
            && marker.start() <= *end
        {
            *end = (*end).max(marker.end());
            continue;
        }
        if ranges.len() == max_markers {
            return Err("Safe output projection exceeds the bounded display limit.");
        }
        if ranges.len() == ranges.capacity() {
            let desired = ranges.capacity().saturating_mul(2).max(1).min(max_markers);
            ranges.reserve_exact(desired - ranges.len());
            if ranges.capacity() > max_markers {
                return Err("Safe output projection exceeds the bounded display limit.");
            }
        }
        ranges.push((marker.start(), marker.end()));
    }
    Ok(ranges)
}

struct RedactionEnvelopeIndex(Vec<(usize, usize)>);

impl RedactionEnvelopeIndex {
    fn contains(&self, start: usize, end: usize) -> bool {
        let index = self
            .0
            .partition_point(|(marker_start, _)| *marker_start <= start);
        index > 0 && self.0[index - 1].1 >= end
    }
}

fn redaction_envelope_index(text: &str) -> RedactionEnvelopeIndex {
    if !text.contains(MARKER_PREFIX) {
        return RedactionEnvelopeIndex(Vec::new());
    }
    let mut ranges: Vec<_> = marker_regex()
        .find_iter(text)
        .filter(|marker| opaque_reference(marker.as_str()))
        .chain(display_marker_regex().find_iter(text).filter(|marker| {
            marker
                .as_str()
                .strip_prefix(MARKER_PREFIX)
                .and_then(|value| value.strip_suffix(']'))
                .is_some_and(|label| {
                    label == "PRIVATE_KEY"
                        || credential_patterns()
                            .iter()
                            .any(|pattern| pattern.label == label)
                })
        }))
        .map(|marker| (marker.start(), marker.end()))
        .collect();
    ranges.sort_unstable_by_key(|&(start, _)| start);
    // Keep the largest end of a single preceding envelope; merging adjacent
    // ranges would incorrectly exempt a secret spanning two separate markers.
    let mut max_end = 0;
    for (_, end) in &mut ranges {
        max_end = max_end.max(*end);
        *end = max_end;
    }
    RedactionEnvelopeIndex(ranges)
}

// Transport preserves a constrained, non-secret envelope without asserting
// authenticity. Only the original executor may authenticate and resolve it.
fn opaque_reference(marker: &str) -> bool {
    let Some(captures) = marker_regex().captures(marker) else {
        return false;
    };
    let Some(label) = captures.get(1) else {
        return false;
    };
    captures.get(4).is_some()
        && (label.as_str() == "K1"
            || credential_patterns()
                .iter()
                .any(|pattern| marker_label_code(pattern.label) == Some(label.as_str())))
}

fn trusted_marker(marker: &str) -> bool {
    let Some(captures) = marker_regex().captures(marker) else {
        return false;
    };
    let (Some(label), Some(nonce), Some(issuer_mac)) =
        (captures.get(1), captures.get(2), captures.get(4))
    else {
        // Legacy markers bind their MAC to the raw value and therefore cannot
        // authenticate themselves at a non-owning display boundary. They are
        // still resolved below when the owner supplies the raw source.
        return false;
    };
    // Authentication is process-local. Presentation separately preserves
    // constrained opaque envelopes, without granting edit authority.
    marker_issuer_matches(label.as_str(), nonce.as_str(), issuer_mac.as_str())
}

fn is_exact_opaque_reference(text: &str) -> bool {
    let trimmed = text.trim();
    let Some(matched) = marker_regex().find(trimmed) else {
        return false;
    };
    matched.start() == 0 && matched.end() == trimmed.len() && opaque_reference(matched.as_str())
}

/// Return `(trusted_executor_marker, any_marker)` for presentation notes.
/// A complete marker from another executor remains opaque transport text but
/// is intentionally not treated as locally editable.
pub fn redaction_marker_status(text: &str) -> (bool, bool) {
    let trusted = marker_regex()
        .find_iter(text)
        .any(|marker| trusted_marker(marker.as_str()));
    let any = marker_regex().is_match(text) || display_marker_regex().is_match(text);
    (trusted, any)
}

fn marker_label_code(label: &str) -> Option<&'static str> {
    match label {
        "PRIVATE_KEY" => Some("K1"),
        "AWS_ACCESS_KEY" => Some("C1"),
        "AWS_SECRET_KEY" => Some("C2"),
        "GITHUB_TOKEN" => Some("C3"),
        "HUGGINGFACE_TOKEN" => Some("C8"),
        "BEARER_TOKEN" => Some("C4"),
        "CONNECTION_CREDENTIAL" => Some("C5"),
        "SECRET_ASSIGNMENT" => Some("C6"),
        "TOKEN_ARGUMENT" => Some("C7"),
        _ => None,
    }
}

fn marker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Parse the short-lived three-field shape only so it can be
        // recognized and rejected safely during a rolling upgrade. New
        // markers always include the fourth issuer MAC; callers must not
        // treat the legacy shape as an authenticated capability.
        Regex::new(
            r"\[REDACTED:([A-Z][A-Z0-9_]*):([0-9a-f]{32}):([0-9a-f]{32})(?::([0-9a-f]{32}))?\]",
        )
        .expect("redaction marker regex")
    })
}

fn display_marker_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\[REDACTED:[A-Z][A-Z0-9_]*\]").expect("display redaction marker regex")
    })
}

pub fn reject_redaction_markers_in_replacement(replacement: &str) -> Result<(), String> {
    if replacement.contains(MARKER_PREFIX)
        || marker_regex().is_match(replacement)
        || display_marker_regex().is_match(replacement)
    {
        return Err(
            "Error: replacement text must not contain a redaction reference marker. Use ordinary non-secret text instead."
                .to_string(),
        );
    }
    Ok(())
}

fn digest_hex(tag: &[u8], label: &str, nonce: &str, value: Option<&str>) -> String {
    let mut mac = Sha256::new();
    mac.update(tag);
    mac.update(reference_key());
    mac.update(label.as_bytes());
    mac.update(nonce.as_bytes());
    if let Some(value) = value {
        mac.update(value.as_bytes());
    }
    mac.finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn marker_value_mac(label: &str, nonce: &str, value: &str) -> String {
    digest_hex(
        b"astra-redaction-reference-v1-value",
        label,
        nonce,
        Some(value),
    )
}

fn marker_issuer_mac(label: &str, nonce: &str) -> String {
    digest_hex(b"astra-redaction-reference-v1-issuer", label, nonce, None)
}

fn marker_mac_matches(label: &str, nonce: &str, value: &str, expected: &str) -> bool {
    marker_value_mac(label, nonce, value) == expected
}

fn marker_issuer_matches(label: &str, nonce: &str, expected: &str) -> bool {
    marker_issuer_mac(label, nonce) == expected
}

/// Resolve an old-text anchor containing redaction markers against raw content.
///
/// `Ok(None)` means the anchor contains no supported marker and should be
/// handled by ordinary exact/fuzzy matching.  A marker-bearing anchor only
/// succeeds when its complete non-secret context and every opaque reference match
/// exactly one raw span.  Ambiguous, malformed, or forged references fail
/// closed with a safe error and never return credential bytes in the message.
pub fn resolve_redacted_anchor(
    content: &str,
    anchor: &str,
    replace_all: bool,
) -> Result<Option<String>, String> {
    if !anchor.contains(MARKER_PREFIX) {
        return Ok(None);
    }
    if replace_all {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    }

    struct ReferenceCandidates<'a> {
        spans: Vec<(usize, usize)>,
        values: Vec<&'a str>,
    }

    let mut resolved = String::new();
    let mut cursor = 0usize;
    let mut marker_ranges = Vec::new();
    let mut source_matches: HashMap<&str, ReferenceCandidates<'_>> = HashMap::new();
    let mut canonical_spans = None;
    for matched in marker_regex().captures_iter(anchor) {
        let whole = matched
            .get(0)
            .expect("marker capture must contain whole match");
        let label = matched
            .get(1)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let nonce = matched
            .get(2)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let mac = matched
            .get(3)
            .map(|value| value.as_str())
            .unwrap_or_default();
        let Some(issuer_mac) = matched.get(4).map(|value| value.as_str()) else {
            // Three-field markers predate the issuer MAC and cannot prove
            // provenance at an edit/display boundary.  Keeping their legacy
            // value-MAC resolver alive would turn persisted or forged model
            // text into a bearer edit capability across rolling upgrades.
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        };
        if !marker_issuer_matches(label, nonce, issuer_mac) {
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        }
        let groups = canonical_spans.get_or_insert_with(|| {
            let mut groups = Vec::new();
            for_each_redaction_span(content, false, false, |span| {
                groups.push(span);
                Ok(())
            })
            .expect("unbounded redaction scan");
            groups
        });
        let candidates = source_matches.entry(label).or_insert_with(|| {
            let spans: Vec<_> = groups
                .iter()
                .filter(|span| span.editable && marker_label_code(span.label) == Some(label))
                .map(|span| (span.start, span.end))
                .collect();
            let mut seen = HashSet::new();
            let values = spans
                .iter()
                .map(|&(start, end)| &content[start..end])
                .filter(|value| seen.insert(*value))
                .collect();
            ReferenceCandidates { spans, values }
        });
        let Some(value) = candidates
            .values
            .iter()
            .copied()
            .find(|value| marker_mac_matches(label, nonce, value, mac))
        else {
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        };
        append_safe_projection(&mut resolved, &anchor[cursor..whole.start()], content.len())
            .map_err(|_| REDACTION_REFERENCE_ERROR.to_string())?;
        let start = resolved.len();
        append_safe_projection(&mut resolved, value, content.len())
            .map_err(|_| REDACTION_REFERENCE_ERROR.to_string())?;
        marker_ranges.push((label, start, resolved.len()));
        cursor = whole.end();
    }

    if marker_ranges.is_empty() {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    }
    append_safe_projection(&mut resolved, &anchor[cursor..], content.len())
        .map_err(|_| REDACTION_REFERENCE_ERROR.to_string())?;
    let Some(start) = content.find(resolved.as_str()) else {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    };
    // The editor consumes the returned raw anchor using ordinary substring
    // replacement. Reject any other raw occurrence, even a noncanonical or
    // overlapping one that did not qualify as a redacted candidate.
    if crate::first_two_match_positions(content, &resolved).is_some() {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    }
    for (label, relative_start, relative_end) in marker_ranges {
        let range = (start + relative_start, start + relative_end);
        if !source_matches
            .get(label)
            .is_some_and(|candidates| candidates.spans.binary_search(&range).is_ok())
        {
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        }
    }
    Ok(Some(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_authorities_remain_private_until_complete_and_public_hosts_stay_visible() {
        for authority in [
            "alice:secretvalue@host",
            ":secretvalue@host",
            "alice:part:secretvalue@host",
            "secretvalue@host",
            "alice@secretvalue@host",
            "alice:secret%40value@host",
        ] {
            let prefix = "https://";
            let raw = format!("{prefix}{authority}/path\n");
            for end in prefix.len() + 1..=raw.len() {
                let safe = redact_credentials_for_display_streaming(&raw[..end], usize::MAX)
                    .unwrap()
                    .0;
                assert!(
                    safe.starts_with("https[REDACTED:"),
                    "authority leaked at {end}: {safe:?}"
                );
            }
            let settled = redact_credentials_for_display(&raw).0;
            assert!(!settled.contains("secret"), "{settled}");
            let (editable, _) = redact_credentials_in_text(&raw);
            assert!(!editable.contains("secret"), "{editable}");
            assert_eq!(
                resolve_redacted_anchor(&raw, &editable, false).unwrap(),
                Some(raw)
            );
        }

        for url in [
            "http://127.0.0.1:3000\n",
            "https://example.test:8443/path@elsewhere",
            "https://example.test:8443?email=a@b",
            "https://example.test:8443#contact@host",
            "http://[::1]:3000 ",
        ] {
            assert_eq!(
                redact_credentials_for_display_streaming(url, usize::MAX)
                    .unwrap()
                    .0,
                url
            );
            assert_eq!(redact_credentials_for_display(url).0, url);
        }
        let pending = redact_credentials_for_display_streaming("http://localhost:3000", usize::MAX)
            .unwrap()
            .0;
        assert!(pending.contains("URL_AUTHORITY_PENDING"), "{pending}");
    }

    #[test]
    fn url_placeholder_exemptions_do_not_hide_other_userinfo() {
        for authority in [
            "user:secretvalue:<your-token>@host",
            "user@secretvalue:<your-token>@host",
            "secretvalue:<your-token>@host",
        ] {
            let raw = format!("https://{authority}/");
            let safe = redact_credentials_for_display(&raw).0;
            assert!(!safe.contains("secretvalue"), "{safe}");
            let safe_live = redact_credentials_for_display_streaming(&raw, usize::MAX)
                .unwrap()
                .0;
            let safe_editable = redact_credentials_in_text(&raw).0;
            assert!(!safe_live.contains("secretvalue"), "{safe_live}");
            assert!(!safe_editable.contains("secretvalue"), "{safe_editable}");
        }
        for url in [
            "https://<your-token>@host/",
            "https://<your-user>:<your-token>@host/",
            "https://:<your-token>@host/",
        ] {
            assert_eq!(redact_credentials_for_display(url).0, url);
        }
    }

    #[test]
    fn streaming_display_conservatively_redacts_short_contextual_values() {
        for raw in [
            "password=short ",
            "AWS_SECRET_KEY=short ",
            "--token short ",
            "Bearer short ",
        ] {
            let (live, count) = redact_credentials_for_display_streaming(raw, usize::MAX).unwrap();
            assert!(count > 0 && !live.contains("short"), "{raw:?}: {live}");
        }
        let raw = "password=short ";
        assert_eq!(redact_credentials_for_display(raw).0, raw);
        for prompt in [
            "Confirm? ",
            "确认？ ",
            "Password: ",
            "Password:",
            "继续？",
            "running",
        ] {
            assert_eq!(
                redact_credentials_for_display_streaming(prompt, usize::MAX)
                    .unwrap()
                    .0,
                prompt
            );
        }
    }

    #[test]
    fn streaming_keeps_public_prefixes_visible_until_secret_bytes_arrive() {
        for prefix in ["-", "A", "g", "h", "AKIA", "ghp_", "hf_", "-----BEGIN"] {
            assert_eq!(
                redact_credentials_for_display_streaming(prefix, usize::MAX)
                    .unwrap()
                    .0,
                prefix
            );
        }
        for prefix in ["AKIA", "ghp_", "hf_"] {
            for end in 1..=prefix.len() {
                assert_eq!(
                    redact_credentials_for_display_streaming(&prefix[..end], usize::MAX)
                        .unwrap()
                        .0,
                    &prefix[..end]
                );
            }
            let secret = format!("{prefix}Z");
            assert!(
                !redact_credentials_for_display_streaming(&secret, usize::MAX)
                    .unwrap()
                    .0
                    .contains(&secret)
            );
        }
        let pem = "-----BEGIN PRIVATE KEY-----\nbody";
        for end in 1..="-----BEGIN".len() {
            assert_eq!(
                redact_credentials_for_display_streaming(&pem[..end], usize::MAX)
                    .unwrap()
                    .0,
                &pem[..end]
            );
        }
        assert!(
            !redact_credentials_for_display_streaming(pem, usize::MAX)
                .unwrap()
                .0
                .contains("body")
        );
    }

    #[test]
    fn streaming_display_never_exposes_known_secret_prefixes() {
        for (prefix, secret, suffix) in [
            ("password=", "zzzzzzzzzzzz", "\n"),
            ("Bearer ", "zzzzzzzzzzzz", "\n"),
            ("AKIA", "ZZZZZZZZZZZZZZZZ", "\n"),
            ("ghp_", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", "\n"),
            ("hf_", "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz", "\n"),
            ("https://user:", "zzzzzzzz", "@example.test\n"),
            (
                "-----BEGIN PRIVATE KEY-----\n",
                "zzzzzzzz",
                "\n-----END PRIVATE KEY-----",
            ),
        ] {
            let text = format!("{prefix}{secret}{suffix}");
            for end in prefix.len() + 1..=text.len() {
                let preview = redact_credentials_for_display_streaming(&text[..end], usize::MAX)
                    .unwrap()
                    .0;
                assert!(
                    !preview.contains(&secret[..1]),
                    "secret prefix leaked at byte {end}: {preview:?}"
                );
            }
        }

        let overlapping = format!("password=fooBearer {}", "z".repeat(40));
        let preview = redact_credentials_for_display_streaming(&overlapping, usize::MAX)
            .unwrap()
            .0;
        assert!(
            !preview.contains("foo") && !preview.contains('z'),
            "{preview}"
        );
        let complete_token = format!("ghp_{}h", "a".repeat(35));
        let preview = redact_credentials_for_display_streaming(&complete_token, usize::MAX)
            .unwrap()
            .0;
        assert!(!preview.contains(&complete_token), "{preview}");
        for raw in ["prefixghp_z", "prefixAKIAZ", "prefixhf_z"] {
            let preview = redact_credentials_for_display_streaming(raw, usize::MAX)
                .unwrap()
                .0;
            assert!(
                !preview.ends_with('z') && !preview.ends_with('Z'),
                "{raw:?}: {preview:?}"
            );
        }
    }

    #[test]
    fn envelope_index_preserves_single_valid_marker_boundaries() {
        let text = format!(
            "[REDACTED:PRIVATE_KEY][REDACTED:GITHUB_TOKEN][REDACTED:UNKNOWN][REDACTED:C3:{0}:{0}:{0}][REDACTED:C3:{0}:{0}]",
            "a".repeat(32)
        );
        let index = redaction_envelope_index(&text);
        let markers: Vec<_> = display_marker_regex()
            .find_iter(&text)
            .chain(marker_regex().find_iter(&text))
            .collect();
        for marker in &markers {
            let valid = marker.as_str().contains("PRIVATE_KEY")
                || marker.as_str().contains("GITHUB_TOKEN")
                || (marker.as_str().starts_with("[REDACTED:C3:")
                    && marker.as_str().matches(':').count() == 4);
            assert_eq!(
                index.contains(marker.start(), marker.end()),
                valid,
                "{marker:?}"
            );
        }
        let first = &markers[0];
        let second = &markers[1];
        assert!(
            !index.contains(first.start(), second.end()),
            "adjacent markers are not one envelope"
        );
    }

    #[test]
    fn streaming_projection_rejects_expansion_over_budget() {
        let input = "password=z ".repeat(20);
        assert!(redact_credentials_for_display_streaming(&input, input.len()).is_err());
        let safe = "[REDACTED:GITHUB_TOKEN]".to_string();
        assert!(SafeOutputProjection::try_new_bounded(safe.clone(), safe.len()).is_err());
        let projection =
            SafeOutputProjection::try_new_bounded(safe.clone(), safe.len() + 64).unwrap();
        assert!(projection.heap_bytes() <= safe.len() + 64);
        assert_eq!(projection.text(), safe);
    }

    #[test]
    fn streaming_redacts_nested_contexts_at_every_chunk_boundary() {
        for prefix in [
            "Bearer xBearer ",
            "--token --token ",
            "password=password = ",
            "aws_secret_key=aws_secret_key = ",
        ] {
            let text = format!("{prefix}{} ", "z".repeat(48));
            for end in prefix.len() + 1..=text.len() {
                let preview = redact_credentials_for_display_streaming(&text[..end], usize::MAX)
                    .unwrap()
                    .0;
                assert!(!preview.contains('z'), "{prefix:?} byte {end}: {preview:?}");
            }
        }
        let text = "password=foo-----BEGIN PRIVATE";
        let preview = redact_credentials_for_display_streaming(text, usize::MAX)
            .unwrap()
            .0;
        assert!(!preview.contains("foo"), "{preview:?}");
    }

    #[test]
    fn streaming_nested_contexts_cover_long_values_and_whitespace() {
        for size in [319, 320, 321, 4096] {
            let value = "a".repeat(size);
            let whitespace = " ".repeat(size);
            for text in [
                format!("Bearer {value}Bearer{whitespace}zzzz"),
                format!("--token {value}--token{whitespace}zzzz"),
                format!("password={value}password{whitespace}={whitespace}zzzz"),
            ] {
                let preview = redact_credentials_for_display_streaming(&text, usize::MAX)
                    .unwrap()
                    .0;
                assert!(!preview.contains("zzzz"), "{size}: {preview:?}");
            }
        }
    }

    #[test]
    fn streaming_withholds_an_unfinished_url_authority() {
        let text = "https://alice:zzzz@example.test/path";
        for end in "https://a".len()..=text.len() {
            let preview = redact_credentials_for_display_streaming(&text[..end], usize::MAX)
                .unwrap()
                .0;
            assert!(!preview.contains("alice"), "byte {end}: {preview:?}");
            assert!(!preview.contains('z'), "byte {end}: {preview:?}");
        }
        let ordinary = "https://alice/path ";
        assert_eq!(
            redact_credentials_for_display_streaming(ordinary, usize::MAX)
                .unwrap()
                .0,
            ordinary
        );
    }

    #[test]
    fn bounded_settled_redaction_stops_before_projection_expansion() {
        let input = "password=abcdefghijkl ".repeat(100);
        assert!(redact_credentials_for_display_bounded(&input, input.len()).is_err());
        assert_eq!(
            redact_credentials_for_display_bounded(&input, input.len() * 3).unwrap(),
            redact_credentials_for_display(&input)
        );
        let adjacent = "[REDACTED:PRIVATE_KEY][REDACTED:GITHUB_TOKEN]";
        assert_eq!(
            redaction_marker_ranges_bounded(adjacent, 1).unwrap().len(),
            1
        );
    }

    #[test]
    fn partial_pem_redaction_handles_many_orphan_end_markers() {
        let prefix = "partial-key-----END PRIVATE KEY-----".repeat(10_000);
        let text = format!(
            "{prefix}visible\n-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----\nafter"
        );
        let (safe, count) = redact_credentials_for_display_partial(&text);
        assert_eq!(count, 2);
        assert_eq!(safe.matches("[REDACTED:PRIVATE_KEY]").count(), 2);
        assert!(safe.contains("visible\n[REDACTED:PRIVATE_KEY]\nafter"));
        assert!(!safe.contains("partial-key") && !safe.contains("secret"));
    }

    #[test]
    fn partial_pem_redaction_preserves_an_overlapping_begin_marker() {
        for suffix in ["", "\n-----END PRIVATE KEY-----\nafter"] {
            let raw = format!(
                "prefix-----END PRIVATE KEY-----BEGIN PRIVATE KEY-----\nprivate-body{suffix}"
            );
            let (safe, count) = redact_credentials_for_display_partial(&raw);
            assert!(!safe.contains("private-body"), "{safe:?}");
            assert_eq!(count, 2, "{safe:?}");
            if !suffix.is_empty() {
                assert!(safe.ends_with("\nafter"), "{safe:?}");
            }
        }
        let adjacent = "prefix-----END PRIVATE KEY-----END PRIVATE KEY-----after";
        let (safe, count) = redact_credentials_for_display_partial(adjacent);
        assert_eq!(count, 1);
        assert_eq!(safe, "[REDACTED:PRIVATE_KEY]after");
    }

    #[test]
    fn overlapping_pem_blocks_share_one_safe_edit_reference() {
        let raw = concat!(
            "before\n-----BEGIN PRIVATE KEY-----\nFIRST_BODY\n",
            "-----END PRIVATE KEY-----BEGIN PRIVATE KEY-----\nSECOND_BODY\n",
            "-----END PRIVATE KEY-----\nafter"
        );
        let (editable, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        for safe in [
            editable.clone(),
            redact_credentials_for_display(raw).0,
            redact_credentials_for_display_streaming(raw, usize::MAX)
                .unwrap()
                .0,
            redact_line_window(raw, 4, 4),
        ] {
            assert!(
                !safe.contains("FIRST_BODY") && !safe.contains("SECOND_BODY"),
                "{safe:?}"
            );
        }
        assert_eq!(
            resolve_redacted_anchor(raw, &editable, false).unwrap(),
            Some(raw.into())
        );
        let marker = marker_regex().find(&editable).unwrap().as_str();
        assert!(
            resolve_redacted_anchor(raw, marker, false)
                .unwrap()
                .is_some()
        );

        let incomplete = raw.replace("-----END PRIVATE KEY-----\nafter", "unterminated");
        let editable = redact_credentials_in_text(&incomplete).0;
        assert!(!editable.contains("SECOND_BODY"));
        assert!(resolve_redacted_anchor(&incomplete, &editable, false).is_err());
    }

    #[test]
    fn pem_line_window_uses_all_blocks_on_the_same_line() {
        let raw = concat!(
            "before\n-----BEGIN PRIVATE KEY-----\nFIRST_BODY\n",
            "-----END PRIVATE KEY----- -----BEGIN PRIVATE KEY-----\n",
            "SECOND_BODY\n-----END PRIVATE KEY-----\nafter\n"
        );
        assert_eq!(redact_line_window(raw, 5, 5), "[REDACTED:PRIVATE_KEY]");
        assert_eq!(redact_line_window(raw, 7, 7), "after");
    }

    #[test]
    fn overlapping_pem_edit_reference_respects_size_limit() {
        for (body_len, resolvable) in [(32_000, true), (33_000, false)] {
            let body = "A".repeat(body_len);
            let raw = format!(
                "-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----BEGIN PRIVATE KEY-----\n{body}\n-----END PRIVATE KEY-----"
            );
            let editable = redact_credentials_in_text(&raw).0;
            let resolved = resolve_redacted_anchor(&raw, &editable, false);
            assert_eq!(resolved.is_ok(), resolvable, "body length {body_len}");
            if resolvable {
                assert_eq!(resolved.unwrap(), Some(raw));
            }
        }
    }

    #[test]
    fn pem_edit_anchors_resolve_chains_and_many_separate_blocks() {
        let chain = concat!(
            "-----BEGIN RSA PRIVATE KEY-----\nfirst\n-----END RSA PRIVATE KEY-----",
            "BEGIN EC PRIVATE KEY-----\nsecond\n-----END EC PRIVATE KEY-----",
            "BEGIN PRIVATE KEY-----\nthird\n-----END PRIVATE KEY-----"
        );
        let (safe, count) = redact_credentials_in_text(chain);
        assert_eq!(count, 1);
        assert_eq!(
            resolve_redacted_anchor(chain, &safe, false).unwrap(),
            Some(chain.into())
        );

        let many = (0..120)
            .map(|i| format!("-----BEGIN PRIVATE KEY-----\nkey{i}\n-----END PRIVATE KEY-----"))
            .collect::<Vec<_>>()
            .join("\n");
        let (safe, count) = redact_credentials_in_text(&many);
        assert_eq!(count, 120);
        assert_eq!(
            resolve_redacted_anchor(&many, &safe, false).unwrap(),
            Some(many)
        );

        let mixed = concat!(
            "-----BEGIN PRIVATE KEY-----\nfirst\n-----END PRIVATE KEY-----\n",
            "password=abcdefghijklmnop\n",
            "-----BEGIN PRIVATE KEY-----\nsecond\n-----END PRIVATE KEY-----"
        );
        let (safe, count) = redact_credentials_in_text(mixed);
        assert_eq!(count, 3);
        assert_eq!(
            resolve_redacted_anchor(mixed, &safe, false).unwrap(),
            Some(mixed.into())
        );
    }

    #[test]
    fn pem_anchor_search_checks_overlapping_candidate_locations() {
        for duplicate in [false, true] {
            let blocks = (0..3)
                .map(|i| {
                    format!(
                        "-----BEGIN PRIVATE KEY-----\nkey{}\n-----END PRIVATE KEY-----",
                        if duplicate { 0 } else { i }
                    )
                })
                .collect::<Vec<_>>();
            let raw = blocks.join("\n");
            let safe = redact_credentials_in_text(&raw).0;
            let markers = safe.lines().collect::<Vec<_>>();
            let anchor = markers[1..].join("\n");
            let resolved = resolve_redacted_anchor(&raw, &anchor, false);
            if duplicate {
                assert!(
                    resolved.is_err(),
                    "overlapping candidates must be ambiguous"
                );
            } else {
                assert_eq!(resolved.unwrap(), Some(blocks[1..].join("\n")));
            }
        }
    }

    #[test]
    fn pem_anchor_rejects_an_overlapping_noncanonical_raw_occurrence() {
        let first = "-----BEGIN PRIVATE KEY-----\nfirst\n-----END PRIVATE KEY-----";
        let second = "-----BEGIN PRIVATE KEY-----\nsecond\n-----END PRIVATE KEY-----";
        let raw = format!("{first}{}{second}-", &second[5..]);
        let (safe, count) = redact_credentials_in_text(&raw);
        assert_eq!(count, 2);
        let anchor = &safe[safe.rfind(MARKER_PREFIX).unwrap()..];
        assert!(resolve_redacted_anchor(&raw, anchor, false).is_err());
    }

    #[test]
    fn redaction_anchor_cannot_expand_past_the_source_size() {
        let raw = "-----BEGIN PRIVATE KEY-----\nkey\n-----END PRIVATE KEY-----";
        let marker = redact_credentials_in_text(raw).0;
        assert!(resolve_redacted_anchor(raw, &marker.repeat(10_000), false).is_err());
    }

    #[test]
    fn redaction_anchor_resolves_values_issued_by_overlapping_rules() {
        let aws_key = ["AK", "IA", "ABCDEFGHIJKLMNOP"].concat();
        let raw = format!("--token abcdefghijklmnopqrst{aws_key}\n--token abcdefghijklmnopqrst\n");
        let (safe, count) = redact_credentials_in_text(&raw);
        assert_eq!(count, 2);
        assert_eq!(
            resolve_redacted_anchor(&raw, &safe, false).unwrap(),
            Some(raw)
        );
    }

    #[test]
    fn overlapping_rules_redact_and_resolve_the_original_secret_group() {
        let aws_key = ["AK", "IA", "ABCDEFGHIJKLMNOP"].concat();
        for raw in [
            format!("--token {aws_key}suffixsuffixsuffix"),
            format!("password=prefix{aws_key}"),
            format!("--token abcdefghijklmnopqrst{aws_key}"),
            "password=fooBearer zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz".to_owned(),
        ] {
            let (editable, count) = redact_credentials_in_text(&raw);
            let display = redact_credentials_for_display(&raw).0;
            assert_eq!(count, 1, "{editable}");
            assert!(!editable.contains(&aws_key) && !editable.contains("zzzz"));
            assert!(!display.contains("suffixsuffixsuffix") && !display.contains("zzzz"));
            assert_eq!(
                resolve_redacted_anchor(&raw, &editable, false).unwrap(),
                Some(raw)
            );
        }
    }

    #[test]
    fn line_window_uses_context_from_the_complete_source() {
        let raw = format!("Bearer\n{}\nafter", "x".repeat(40));
        let line = redact_line_window(&raw, 2, 2);
        assert!(!line.contains(&"x".repeat(40)), "{line}");
        assert_eq!(line, "[REDACTED:BEARER_TOKEN]");

        let raw = format!("AWS_SECRET_KEY=\n{}\nafter", "x".repeat(30));
        let line = redact_line_window(&raw, 2, 2);
        assert!(!line.contains(&"x".repeat(30)), "{line}");
        assert_eq!(
            resolve_redacted_anchor(&raw, &line, false).unwrap(),
            Some("x".repeat(30))
        );

        let crlf = format!(
            "before\r\nAWS_SECRET_KEY=\r\n{}\r\nafter\r\n",
            "x".repeat(30)
        );
        let line = redact_line_window(&crlf, 3, 3);
        assert_eq!(
            resolve_redacted_anchor(&crlf, &line, false).unwrap(),
            Some("x".repeat(30))
        );
        assert_eq!(redact_line_window(&crlf, 0, 1), "before");
        assert_eq!(redact_line_window(&crlf, 5, 9), "");
        assert_eq!(redact_line_window(&crlf, 3, 2), "");
    }

    #[test]
    fn nested_pem_headers_conceal_the_unbounded_remainder() {
        for between in ["\nOUTER_BODY\n-----", ""] {
            let raw = format!(
                "-----BEGIN PRIVATE KEY-----{between}BEGIN PRIVATE KEY-----\nINNER_BODY\n-----END PRIVATE KEY-----\nOUTER_SECRET\n-----END PRIVATE KEY-----\nafter"
            );
            let editable = redact_credentials_in_text(&raw).0;
            for safe in [
                editable.clone(),
                redact_credentials_for_display(&raw).0,
                redact_credentials_for_display_streaming(&raw, usize::MAX)
                    .unwrap()
                    .0,
                redact_line_window(&raw, 1, raw.lines().count()),
            ] {
                assert!(
                    !safe.contains("INNER_BODY") && !safe.contains("OUTER_SECRET"),
                    "{safe:?}"
                );
            }
            assert!(resolve_redacted_anchor(&raw, &editable, false).is_err());
        }
    }

    #[test]
    fn byte_pem_scanner_handles_chunk_boundary_and_bounded_labels() {
        let kind = format!("{}PRIVATE KEY", "A".repeat(64));
        let header = format!("-----BEGIN {kind}-----");
        let mut bytes = vec![b'x'; 4096 - 3];
        bytes.extend_from_slice(header.as_bytes());
        let markers = private_key_header_markers_bytes(&bytes);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].0, 4096 - 3);

        let overlong = format!("-----BEGIN {}PRIVATE KEY-----", "A".repeat(65));
        assert!(private_key_header_markers_bytes(overlong.as_bytes()).is_empty());
    }

    #[test]
    fn marker_is_non_secret_and_resolves_exactly() {
        let raw = "clone https://ghp_abcdefghijklmnopqrstuvwxyz0123456789@github.com/a/b.git";
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("ghp_"));
        let resolved = resolve_redacted_anchor(raw, &redacted, false)
            .expect("reference should parse")
            .expect("marker should resolve");
        assert_eq!(resolved, raw);
    }

    #[test]
    fn direct_huggingface_token_is_redacted_and_resolves_exactly() {
        let token = "hf_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let raw = format!("credential probe returned '{token}'");

        let (redacted, count) = redact_credentials_in_text(&raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains(token));
        assert!(redacted.contains("[REDACTED:C8:"));
        assert_eq!(
            resolve_redacted_anchor(&raw, &redacted, false)
                .expect("reference should parse")
                .expect("marker should resolve"),
            raw
        );

        let (display, count) = redact_credentials_for_display(&raw);
        assert_eq!(count, 1);
        assert!(!display.contains(token));
        assert!(display.contains("[REDACTED:HUGGINGFACE_TOKEN]"));
        assert!(resolve_redacted_anchor(&raw, &display, false).is_err());
    }

    #[test]
    fn indexed_secret_assignments_are_redacted_and_resolve_exactly() {
        let secret = ["D4w8z9wKN1aVeT3BpQj6", "kIuN7wH8X0M9KfV5OqzF"].concat();
        let raw = format!(r#"os.environ["AWS_SECRET_ACCESS_KEY"] = "{secret}""#);
        let (redacted, count) = redact_credentials_in_text(&raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains(&secret));
        let resolved = resolve_redacted_anchor(&raw, &redacted, false)
            .expect("indexed assignment reference should parse")
            .expect("indexed assignment marker should resolve");
        assert_eq!(resolved, raw);
    }

    #[test]
    fn assignment_references_preserve_public_syntax_and_bind_only_the_value() {
        let secret = "synthetic-sensitive-value-0123456789";
        for (prefix, suffix) in [
            ("export AWS_SECRET_ACCESS_KEY='", "'\nnext=42\n"),
            ("AWS_SECRET_KEY=", "\nnext=42\n"),
            (
                "os.environ[\"AWS_SECRET_ACCESS_KEY\"] = \"",
                "\"\nnext=42\n",
            ),
            ("password = \"", "\";\nnext=42\n"),
            ("auth_token=", "\nnext=42\n"),
            ("config['api_key'] = '", "'\nnext=42\n"),
        ] {
            let raw = format!("{prefix}{secret}{suffix}");
            let (view, count) = redact_credentials_in_text(&raw);
            assert_eq!(count, 1);
            assert!(!view.contains(secret));
            assert!(
                view.starts_with(prefix),
                "public assignment prefix was hidden"
            );
            assert!(view.ends_with(suffix), "public trailing syntax was hidden");
            let marker = &view[prefix.len()..view.len() - suffix.len()];
            assert_eq!(
                resolve_redacted_anchor(&raw, marker, false)
                    .unwrap()
                    .unwrap(),
                secret
            );
            assert_eq!(
                resolve_redacted_anchor(&raw, &view, false)
                    .unwrap()
                    .unwrap(),
                raw
            );
            assert_eq!(redact_credentials_in_text(&view), (view.clone(), 0));
            let (display, count) = redact_credentials_for_display(&raw);
            assert_eq!(count, 1, "overlapping rules must not count one value twice");
            assert_eq!(
                redact_credentials_for_display(&display),
                (display.clone(), 0)
            );
            assert!(resolve_redacted_anchor(&raw, &display, false).is_err());
            assert!(display.starts_with(prefix) && display.ends_with(suffix));
            assert!(!display.contains(secret));
        }
    }

    #[test]
    fn explicit_your_placeholders_remain_visible_while_real_credentials_are_redacted() {
        let safe = concat!(
            "AWS_SECRET_ACCESS_KEY=<your-aws-secret-access-key>\n",
            "os.environ[\"SECRET_ACCESS_KEY\"] = \"<your-secret-access-key>\"\n",
            "git clone https://<your-github-token>@github.com/example/repo.git\n",
            "tool --token <your-service-token>\n",
        );
        let real = concat!(
            "AWS_SECRET_ACCESS_KEY=abcdefghijklmnopqrstuvwxyz0123456789\n",
            "git clone https://user:abcdefghijklmnopqrstuvwxyz123456@github.com/example/repo.git\n",
        );

        let (redacted, count) = redact_credentials_in_text(&format!("{safe}{real}"));
        assert_eq!(count, 2);
        assert!(redacted.starts_with(safe));
        assert!(!redacted.contains("abcdefghijklmnopqrstuvwxyz0123456789"));
        assert!(!redacted.contains("abcdefghijklmnopqrstuvwxyz123456"));
    }

    #[test]
    fn structured_secret_field_keeps_only_the_narrow_explicit_placeholder() {
        let mut safe = serde_json::json!({
            "api_key": "<your-service-api-key>",
            "password": "<your-database-password>",
        });
        assert_eq!(redact_credentials_in_json(&mut safe), 0);
        assert_eq!(safe["api_key"], "<your-service-api-key>");
        assert_eq!(safe["password"], "<your-database-password>");

        let mut unsafe_value = serde_json::json!({"api_key": "not-a-placeholder-secret"});
        assert_eq!(redact_credentials_in_json(&mut unsafe_value), 1);
        assert_eq!(unsafe_value["api_key"], DISPLAY_SECRET_FIELD_MARKER);
    }

    #[test]
    fn explicit_placeholder_near_misses_remain_secret() {
        for candidate in [
            "<your-service-token>-live",
            "prefix-<your-service-token>",
            "<YOUR-service-token>",
            "<your-service token>",
            "<your->",
        ] {
            let mut value = serde_json::json!({"api_key": candidate});
            assert_eq!(
                redact_credentials_in_json(&mut value),
                1,
                "near-miss placeholder was incorrectly trusted: {candidate}"
            );
            assert_eq!(value["api_key"], DISPLAY_SECRET_FIELD_MARKER);
        }

        let raw = "AWS_SECRET_ACCESS_KEY=<your-service-token>-live";
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("<your-service-token>-live"));
    }

    #[test]
    fn token_argument_redaction_is_provider_neutral_and_resolves() {
        let raw = concat!(
            "tool --token hf_abcdefghijklmnopqrstuvwxyz123456 ",
            "--auth-token=tok_abcdefghijklmnopqrstuvwxyz123456 ",
            "--api-key \"key_abcdefghijklmnopqrstuvwxyz123456\"",
        );
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 3);
        for secret in [
            "hf_abcdefghijklmnopqrstuvwxyz123456",
            "tok_abcdefghijklmnopqrstuvwxyz123456",
            "key_abcdefghijklmnopqrstuvwxyz123456",
        ] {
            assert!(!redacted.contains(secret));
        }
        assert!(redacted.contains("tool --token [REDACTED:C7:"));
        assert!(redacted.contains(" --auth-token=[REDACTED:C7:"));
        assert!(redacted.contains(" --api-key \"[REDACTED:C7:"));
        let resolved = resolve_redacted_anchor(raw, &redacted, false)
            .expect("token argument reference should parse")
            .expect("token argument marker should resolve");
        assert_eq!(resolved, raw);
    }

    #[test]
    fn token_argument_display_redaction_preserves_non_secret_cli_syntax() {
        let raw = concat!(
            "tool --token hf_abcdefghijklmnopqrstuvwxyz123456 ",
            "--auth-token=tok_abcdefghijklmnopqrstuvwxyz123456 ",
            "--api-key \"key_abcdefghijklmnopqrstuvwxyz123456\"",
        );
        let (redacted, count) = redact_credentials_for_display(raw);
        assert_eq!(count, 3);
        assert_eq!(
            redacted,
            concat!(
                "tool --token [REDACTED:TOKEN_ARGUMENT] ",
                "--auth-token=[REDACTED:TOKEN_ARGUMENT] ",
                "--api-key \"[REDACTED:TOKEN_ARGUMENT]\"",
            )
        );
    }

    #[test]
    fn token_arguments_in_json_strings_are_redacted_before_escaping() {
        let raw = r#"{"cmd":"--api-key \"quoted-test-secret-987654321\""}"#;
        let secret = "quoted-test-secret-987654321";
        for input in [raw.to_owned(), serde_json::to_string(raw).unwrap()] {
            let (safe, count) = redact_credentials_for_display(&input);
            assert_eq!(count, 1, "{safe}");
            assert!(!safe.contains("quoted-test-secret-987654321"));
            assert!(serde_json::from_str::<Value>(&safe).is_ok(), "{safe}");
            let editable = redact_credentials_in_text(&input).0;
            assert_eq!(
                resolve_redacted_anchor(&input, &editable, false).unwrap(),
                Some(input.clone())
            );
            let start = input.find(secret).unwrap();
            for length in 1..=secret.len() {
                let live =
                    redact_credentials_for_display_streaming(&input[..start + length], usize::MAX)
                        .unwrap()
                        .0;
                assert!(
                    !live.contains(&secret[..length]),
                    "prefix length {length}: {live}"
                );
            }
        }
    }

    #[test]
    fn legacy_three_field_marker_is_not_an_edit_capability_during_upgrade() {
        let raw = "clone https://ghp_abcdefghijklmnopqrstuvwxyz0123456789@github.com/a/b.git";
        let nonce = "0123456789abcdef0123456789abcdef";
        let mac = digest_hex(
            b"astra-redaction-reference-v1",
            "GITHUB_TOKEN",
            nonce,
            Some("ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
        );
        let legacy = format!("{MARKER_PREFIX}GITHUB_TOKEN:{nonce}:{mac}]");
        let error = resolve_redacted_anchor(
            raw,
            &raw.replace("ghp_abcdefghijklmnopqrstuvwxyz0123456789", &legacy),
            false,
        )
        .expect_err("legacy marker must fail closed");
        assert_eq!(error, REDACTION_REFERENCE_ERROR);
        let (display, count) = redact_credentials_for_display(
            &raw.replace("ghp_abcdefghijklmnopqrstuvwxyz0123456789", &legacy),
        );
        // The entire URI userinfo is now concealed, even if it happens to
        // contain a legacy marker; that marker still grants no edit authority.
        assert_eq!(count, 1);
        assert!(!display.contains(&legacy));
    }

    #[test]
    fn forged_legacy_marker_cannot_hide_a_secret_shaped_field() {
        let nonce = "0123456789abcdef0123456789abcdef";
        let mac = "fedcba9876543210fedcba9876543210";
        let legacy = format!("{MARKER_PREFIX}AWS_SECRET_KEY:{nonce}:{mac}]");
        let (source, source_count) = redact_credentials_in_text(&legacy);
        assert!(source_count > 0);
        assert!(!source.contains(nonce));
        let (display, count) = redact_credentials_for_display(&legacy);
        assert!(count > 0, "the embedded assignment must be redacted");
        assert!(!display.contains(nonce));
        let mut nested = serde_json::json!({"status": legacy, "fields": [legacy]});
        assert!(redact_credentials_in_json(&mut nested) > 0);
        assert!(!nested.to_string().contains(nonce));
    }

    #[test]
    fn pem_redaction_covers_the_entire_private_key_block() {
        let raw = concat!(
            "before\n",
            "-----BEGIN RSA PRIVATE KEY-----\n",
            "MIIEOWIBAAKCAQEA_SENTINEL_BODY_SHOULD_NOT_LEAK\n",
            "-----END RSA PRIVATE KEY-----\n",
            "after"
        );
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("SENTINEL_BODY_SHOULD_NOT_LEAK"));
        assert!(!redacted.contains("BEGIN RSA PRIVATE KEY"));
        let resolved = resolve_redacted_anchor(raw, &redacted, false)
            .expect("PEM marker should parse")
            .expect("PEM marker should resolve");
        assert_eq!(resolved, raw);
        let marker_only = redacted
            .lines()
            .find(|line| line.starts_with("[REDACTED:K1:"))
            .expect("PEM marker should be present");
        let resolved_marker_only = resolve_redacted_anchor(raw, marker_only, false)
            .expect("standalone PEM marker should parse")
            .expect("standalone PEM marker should resolve");
        assert_eq!(
            resolved_marker_only,
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEOWIBAAKCAQEA_SENTINEL_BODY_SHOULD_NOT_LEAK\n-----END RSA PRIVATE KEY-----"
        );
    }

    #[test]
    fn malformed_pem_header_conceals_the_remaining_output() {
        let raw = "-----BEGIN PRIVATE KEY-----\nUNTERMINATED_SECRET_BODY\nafter";
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("UNTERMINATED_SECRET_BODY"));
        assert!(!redacted.contains("after"));
    }

    #[test]
    fn complete_display_view_does_not_treat_an_orphan_pem_footer_as_a_key() {
        let raw = "documentation\n-----END RSA PRIVATE KEY-----\nordinary suffix";
        let (redacted, count) = redact_credentials_for_display(raw);
        assert_eq!(count, 0);
        assert_eq!(redacted, raw);

        let (partial, partial_count) = redact_credentials_for_display_partial(raw);
        assert_eq!(partial_count, 1);
        assert!(partial.contains("ordinary suffix"));
        assert!(!partial.contains("documentation"));
    }

    #[test]
    fn mismatched_pem_end_is_fail_closed_for_full_and_range_views() {
        let raw = concat!(
            "-----BEGIN RSA PRIVATE KEY-----\n",
            "BODY_BEFORE_MISMATCH\n",
            "-----END EC PRIVATE KEY-----\n",
            "BODY_AFTER_MISMATCH\n",
            "ordinary suffix"
        );
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("BODY_BEFORE_MISMATCH"));
        assert!(!redacted.contains("BODY_AFTER_MISMATCH"));
        assert!(!redacted.contains("ordinary suffix"));

        let range = redact_line_window(raw, 4, 5);
        assert!(!range.contains("BODY_AFTER_MISMATCH"));
        assert!(!range.contains("ordinary suffix"));
    }

    #[test]
    fn line_window_conceals_pem_when_range_starts_inside_block() {
        let raw = concat!(
            "before\n",
            "-----BEGIN RSA PRIVATE KEY-----\n",
            "PRIVATE_BODY_SENTINEL\n",
            "-----END RSA PRIVATE KEY-----\n",
            "after\n"
        );
        let body_only = redact_line_window(raw, 3, 3);
        assert_eq!(body_only, "[REDACTED:PRIVATE_KEY]");
        assert!(!body_only.contains("PRIVATE_BODY_SENTINEL"));
        let end_only = redact_line_window(raw, 4, 5);
        assert!(!end_only.contains("PRIVATE_BODY_SENTINEL"));
    }

    #[test]
    fn safe_window_uses_redacted_offsets_and_never_splits_marker() {
        let raw = "prefix AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE suffix";
        let (safe, _) = redact_credentials_for_display(raw);
        let marker_start = safe.find("[REDACTED:").expect("display marker");
        let inside = marker_start + 3;

        let (first, first_end, total, _) = redacted_output_window(raw, 0, marker_start + 2);
        assert!(!first.contains("[REDACTED:"));
        assert_eq!(first_end, marker_start);
        assert_eq!(total, safe.len());

        let (second, second_end, _, _) = redacted_output_window(raw, inside, 64);
        assert!(!second.contains("[REDACTED:"));
        assert!(second_end > first_end);
        assert!(!second.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn safe_window_skips_adjacent_markers_when_page_is_smaller_than_one() {
        let raw = "AKIAIOSFODNN7EXAMPLEAKIAIOSFODNN7EXAMPLE";
        let (page, end, total, _) = redacted_output_window(raw, 0, 1);
        assert!(
            page.is_empty(),
            "a one-byte page must not expose marker fragments"
        );
        assert_eq!(end, total);
        assert!(!page.contains("[REDACTED:"));
    }

    #[test]
    fn safe_tail_does_not_emit_a_partial_marker() {
        let raw = format!("prefix {} suffix", "AKIAIOSFODNN7EXAMPLE");
        let tail = redacted_output_tail_chars(&raw, 12);
        assert!(!tail.contains("[REDACTED:"));
        assert!(!tail.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(tail.ends_with("suffix"));
    }

    #[test]
    fn executor_marker_survives_a_non_owning_display_pass() {
        let raw = "AWS_SECRET_KEY=abcdefghijklmnopqrstuvwxyz0123456789";
        let (issued, _) = redact_credentials_in_text(raw);
        let (display, count) = redact_credentials_for_display(&issued);
        assert_eq!(count, 0);
        assert_eq!(display, issued);
    }

    #[test]
    fn reference_round_trip_crosses_processes_without_transferring_edit_authority() {
        use std::io::{BufRead, Write};
        const CHILD: &str = "ASTRA_TEST_REFERENCE_SOURCE_PROCESS";
        let raw = "password=\"synthetic-sensitive-body-long-enough\";\n";
        if std::env::var_os(CHILD).is_some() {
            let issued = redact_credentials_in_text(raw).0;
            println!(
                "ASTRA_REFERENCE {}",
                serde_json::to_string(&issued).unwrap()
            );
            std::io::stdout().flush().unwrap();
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            let returned: String = serde_json::from_str(&line).unwrap();
            assert_eq!(returned, issued);
            assert_eq!(
                resolve_redacted_anchor(raw, &returned, false)
                    .unwrap()
                    .as_deref(),
                Some(raw)
            );
            return;
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "credential_redaction::tests::reference_round_trip_crosses_processes_without_transferring_edit_authority", "--nocapture"])
            .env(CHILD, "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn().unwrap();
        let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
        let reference = loop {
            let mut line = String::new();
            assert!(
                output.read_line(&mut line).unwrap() > 0,
                "source failed before issuing a reference"
            );
            if let Some(json) = line.strip_prefix("ASTRA_REFERENCE ") {
                break serde_json::from_str::<String>(json).unwrap();
            }
        };
        let safe = redact_credentials_for_display(&reference).0;
        assert_eq!(safe, reference);
        assert_eq!(redaction_marker_status(&safe), (false, true));
        assert!(resolve_redacted_anchor(raw, &safe, false).is_err());
        let mut nested = serde_json::json!({"output": safe});
        redact_credentials_in_json(&mut nested);
        let mut input = child.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            serde_json::to_string(nested["output"].as_str().unwrap()).unwrap()
        )
        .unwrap();
        drop(input);
        for line in output.lines() {
            line.unwrap();
        }
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn transport_opacity_never_grants_edit_authenticity_or_hides_adjacent_secrets() {
        let foreign = "[REDACTED:C6:00000000000000000000000000000000:00000000000000000000000000000000:00000000000000000000000000000000]";
        let raw = "password=\"synthetic-sensitive-body-long-enough\"";
        assert_eq!(redact_credentials_for_display(foreign).0, foreign);
        assert_eq!(redaction_marker_status(foreign), (false, true));
        assert!(resolve_redacted_anchor(raw, foreign, false).is_err());
        let mut nested = serde_json::json!({"nested": {"password": foreign}});
        assert_eq!(redact_credentials_in_json(&mut nested), 0);
        assert_eq!(nested["nested"]["password"], foreign);
        for suffix in ["adjacent-sensitive-body", " adjacent-sensitive-body"] {
            let mut value = serde_json::json!({"password": format!("{foreign}{suffix}")});
            assert!(redact_credentials_in_json(&mut value) > 0);
            assert!(!value.to_string().contains("adjacent-sensitive-body"));
        }
        for malformed in [
            foreign.replace(":C6:", ":UNKNOWN:"),
            foreign.replace(":000", ":GGG"),
        ] {
            let mut value = serde_json::json!({"password": malformed});
            assert!(redact_credentials_in_json(&mut value) > 0);
            assert_eq!(value["password"], DISPLAY_SECRET_FIELD_MARKER);
        }
        for marker in [
            "[REDACTED:AWS_SECRET_KEY]",
            "[REDACTED:UNKNOWN]",
            "[REDACTED:AWS_SECRET_KEY",
        ] {
            let wrapped = format!("password=\"{marker}adjacent-sensitive-body\"");
            let (safe, count) = redact_credentials_for_display(&wrapped);
            assert_eq!(count, 1);
            assert!(!safe.contains("adjacent-sensitive-body"));
        }
    }

    #[test]
    fn forged_or_ambiguous_reference_fails_closed() {
        let raw = "a AKIAIOSFODNN7EXAMPLE\nb AKIAIOSFODNN7EXAMPLE";
        let (redacted, _) = redact_credentials_in_text(raw);
        let marker = redacted
            .split_whitespace()
            .nth(1)
            .expect("marker should be present");
        assert!(resolve_redacted_anchor(raw, marker, false).is_err());
        assert!(resolve_redacted_anchor(raw, marker, true).is_err());
        assert!(
            resolve_redacted_anchor(
                "AKIAIOSFODNN7EXAMPLE",
                "[REDACTED:AWS_ACCESS_KEY:0000000000000000]",
                false
            )
            .is_err()
        );
    }

    #[test]
    fn ordinary_anchor_is_not_interpreted_as_reference() {
        assert_eq!(
            resolve_redacted_anchor("plain", "plain", false).unwrap(),
            None
        );
    }

    #[test]
    fn replacement_cannot_smuggle_a_reference_marker() {
        assert!(reject_redaction_markers_in_replacement(
            "[REDACTED:AWS_ACCESS_KEY:00000000000000000000000000000000:00000000000000000000000000000000:00000000000000000000000000000000]"
        )
        .is_err());
        assert!(reject_redaction_markers_in_replacement("[REDACTED:AWS_ACCESS_KEY]").is_err());
        assert!(reject_redaction_markers_in_replacement("[REDACTED:unknown:marker]").is_err());
        assert!(reject_redaction_markers_in_replacement("ordinary text").is_ok());
    }

    #[test]
    fn display_redaction_is_not_an_edit_capability() {
        let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE";
        let (display, count) = redact_credentials_for_display(raw);
        assert_eq!(count, 1);
        assert_eq!(display, "AWS_ACCESS_KEY_ID=[REDACTED:AWS_ACCESS_KEY]");
        assert!(resolve_redacted_anchor(raw, &display, false).is_err());
    }

    #[test]
    fn nested_json_redaction_covers_status_and_metadata() {
        let mut value = serde_json::json!({
            "status": "AWS_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE",
            "fields": ["password=super-secret-value-1234"],
        });
        assert_eq!(redact_credentials_in_json(&mut value), 2);
        let encoded = value.to_string();
        assert!(!encoded.contains("AKIAIOSFODNN7EXAMPLE"));
        assert!(!encoded.contains("super-secret-value-1234"));
    }

    #[test]
    fn structured_secret_keys_redact_values_without_losing_json_shape() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let mut value = serde_json::json!({
            "api_key": secret,
            "accessToken": "camel-case-secret-value-123456",
            "credentials": "opaque-credential-value-123456",
            "auth_token": "token-value-that-is-long-enough-123456",
            "path": "src/main.rs",
            "revision_token": "ordinary-pagination-revision",
            "output_key": "ordinary-output-key",
            "idempotency_key": "ordinary-idempotency-key",
        });

        let count = redact_credentials_in_json(&mut value);
        assert_eq!(count, 4);
        assert!(!value.to_string().contains(secret));
        assert_eq!(value["path"], "src/main.rs");
        assert_eq!(value["revision_token"], "ordinary-pagination-revision");
        assert_eq!(value["output_key"], "ordinary-output-key");
        assert_eq!(value["idempotency_key"], "ordinary-idempotency-key");
        assert_eq!(value["api_key"], DISPLAY_SECRET_FIELD_MARKER);
        assert!(
            value["auth_token"]
                .as_str()
                .unwrap()
                .starts_with("[REDACTED:")
        );
    }

    #[test]
    fn typed_runtime_policy_credentials_enum_is_not_redacted_as_a_secret() {
        for policy in ["disabled", "user_approved", "scoped_injection"] {
            let mut value = serde_json::json!({
                "runtime_environment_advertisement": {
                    "binding": {"policy": {"credentials": policy}}
                }
            });

            assert_eq!(redact_credentials_in_json(&mut value), 0, "{policy}");
            assert_eq!(
                value["runtime_environment_advertisement"]["binding"]["policy"]["credentials"],
                policy
            );
        }
    }

    #[test]
    fn credentials_key_redacts_values_outside_typed_policy_enum() {
        for secret in [
            "s3cr3t!",
            "short",
            "secret value with spaces",
            "opaque-credential-value-123456",
        ] {
            let mut value = serde_json::json!({"credentials": secret});
            assert_eq!(redact_credentials_in_json(&mut value), 1, "{secret}");
            assert_eq!(value["credentials"], DISPLAY_SECRET_FIELD_MARKER);
        }
    }

    #[test]
    fn generic_json_sanitizer_does_not_parse_function_arguments() {
        let mut message = serde_json::json!({
            "function": {"arguments": "ordinary expression"}
        });

        redact_credentials_in_json(&mut message);
        assert_eq!(message["function"]["arguments"], "ordinary expression");
    }

    #[test]
    fn generic_json_sanitizer_redacts_credentials_inside_function_arguments() {
        let secret = "hf_abcdefghijklmnopqrstuvwxyz123456";
        let mut message = serde_json::json!({
            "function": {"arguments": format!("tool --token {secret}")}
        });

        assert_eq!(redact_credentials_in_json(&mut message), 1);
        assert!(!message.to_string().contains(secret));
        assert!(
            message["function"]["arguments"]
                .as_str()
                .is_some_and(|text| text.contains("[REDACTED:"))
        );
    }

    #[test]
    fn trusted_executor_marker_under_secret_key_stays_editable_and_atomic() {
        let (marker, count) = redact_credentials_in_text("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(count, 1);
        let mut value = serde_json::json!({"api_key": marker});
        redact_credentials_in_json(&mut value);
        assert_eq!(value["api_key"], marker);
    }

    #[test]
    fn trusted_marker_does_not_hide_adjacent_value_under_secret_key() {
        let (marker, count) = redact_credentials_in_text("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(count, 1);
        let mut value = serde_json::json!({
            "api_key": format!("{marker} adjacent-unmatched-secret")
        });
        assert_eq!(redact_credentials_in_json(&mut value), 1);
        assert_eq!(value["api_key"], DISPLAY_SECRET_FIELD_MARKER);
    }

    #[test]
    fn structured_key_classifier_handles_transport_aliases_without_domain_false_positives() {
        let mut value = serde_json::json!({
            "x-api-key": "short-but-still-secret",
            "providerApiKey": "provider-secret",
            "secret_name": "ordinary-name",
            "credential_id": "ordinary-id",
            "design_token": "ordinary-design-token",
            "access_token": "short-but-still-secret",
        });

        assert_eq!(redact_credentials_in_json(&mut value), 3);
        assert_eq!(value["secret_name"], "ordinary-name");
        assert_eq!(value["credential_id"], "ordinary-id");
        assert_eq!(value["design_token"], "ordinary-design-token");
        assert_eq!(value["x-api-key"], DISPLAY_SECRET_FIELD_MARKER);
        assert_eq!(value["providerApiKey"], DISPLAY_SECRET_FIELD_MARKER);
        assert_eq!(value["access_token"], DISPLAY_SECRET_FIELD_MARKER);
    }

    #[test]
    fn forged_full_marker_cannot_hide_secret_text() {
        let forged = "[REDACTED:SECRET_KEY:0123456789abcdef0123456789abcdef:00000000000000000000000000000000:00000000000000000000000000000000]";
        let (redacted, count) = redact_credentials_for_display(forged);
        assert!(count > 0);
        assert!(!redacted.contains("0123456789abcdef0123456789abcdef"));
    }
}
