//! Shared credential redaction and safe edit-reference support.
//!
//! Tool output is redacted before it reaches the model.  An exact-text editor
//! still needs a way to address a redacted span without learning the secret,
//! so markers carry only a process-keyed opaque reference.  The editor resolves
//! that reference against the raw file at execution time and fails closed
//! unless exactly one span matches.

use regex::{Regex, bytes::Regex as BytesRegex};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

struct CredentialPattern {
    regex: &'static Regex,
    label: &'static str,
    secret_capture: Option<&'static str>,
}

fn credential_patterns() -> &'static [CredentialPattern] {
    static PATTERNS: OnceLock<Vec<CredentialPattern>> = OnceLock::new();

    macro_rules! pat {
        ($re:expr, $label:expr) => {{
            static RE: OnceLock<Regex> = OnceLock::new();
            CredentialPattern {
                regex: RE.get_or_init(|| Regex::new($re).expect("credential pattern regex")),
                label: $label,
                secret_capture: None,
            }
        }};
        ($re:expr, $label:expr, $secret_capture:expr) => {{
            static RE: OnceLock<Regex> = OnceLock::new();
            CredentialPattern {
                regex: RE.get_or_init(|| Regex::new($re).expect("credential pattern regex")),
                label: $label,
                secret_capture: Some($secret_capture),
            }
        }};
    }

    PATTERNS.get_or_init(|| {
        vec![
            pat!(r"AKIA[0-9A-Z]{16}", "AWS_ACCESS_KEY"),
            pat!(
                r#"(?i)(?:aws_secret_access_key|aws_secret_key|\[\s*['"]?(?:aws_secret_access_key|aws_secret_key)['"]?\s*\]|['"](?:aws_secret_access_key|aws_secret_key)['"])\s*[=:]\s*['"]?[A-Za-z0-9/+=._-]{30,}"#,
                "AWS_SECRET_KEY"
            ),
            pat!(
                r#"(?i)(?:--?(?:token|api[-_]?key|access[-_]?token|auth[-_]?token))(?:\s+|=)\s*['"]?(?P<secret>[A-Za-z0-9._\-/+=]{20,})"#,
                "TOKEN_ARGUMENT",
                "secret"
            ),
            pat!(r"gh[pousr]_[A-Za-z0-9_]{36,255}", "GITHUB_TOKEN"),
            // Hugging Face user-access tokens are opaque bearer credentials.
            // The public `hf_` prefix is fixed while the body is
            // case-sensitive alphanumeric data; accept the documented
            // minimum body length so a direct tool output cannot expose one
            // merely because it lacks an assignment/CLI flag wrapper.
            pat!(r"hf_[A-Za-z0-9]{34,255}", "HUGGINGFACE_TOKEN"),
            pat!(
                r"(?i)Bearer\s+[A-Za-z0-9._\-/+=]{40,}",
                "BEARER_TOKEN"
            ),
            pat!(r"://[^:@\s/]+:[^:@\s/]+@", "CONNECTION_CREDENTIAL"),
            pat!(
                r#"(?i)(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key|\[\s*['"]?(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)['"]?\s*\]|['"](?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)['"])\s*[=:]\s*['"]?[^\s'"]{12,}"#,
                "SECRET_ASSIGNMENT"
            ),
        ]
    })
}

fn credential_secret_spans<'a>(
    text: &'a str,
    pattern: &CredentialPattern,
) -> Vec<regex::Match<'a>> {
    if let Some(secret_capture) = pattern.secret_capture {
        pattern
            .regex
            .captures_iter(text)
            .filter(|captures| {
                captures.get(0).is_some_and(|whole| {
                    !span_is_generated_marker(text, whole.start(), whole.end())
                })
            })
            .filter_map(|captures| captures.name(secret_capture))
            .filter(|matched| !credential_match_is_explicit_placeholder(matched.as_str()))
            .collect()
    } else {
        pattern
            .regex
            .find_iter(text)
            .filter(|matched| !span_is_generated_marker(text, matched.start(), matched.end()))
            .filter(|matched| !credential_match_is_explicit_placeholder(matched.as_str()))
            .collect()
    }
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
        let value = authority
            .rsplit_once(':')
            .map_or(authority, |(_, password)| password);
        return is_explicit_placeholder_value(value);
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
            if span_is_generated_marker(text, matched.start(), matched.end()) {
                return None;
            }
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
            if span_is_generated_marker(text, matched.start(), matched.end()) {
                return None;
            }
            pem_marker_kind(matched.as_str(), "-----END ")
                .map(|kind| (matched.start(), matched.end(), kind.to_string()))
        })
        .collect()
}

/// Byte-oriented counterpart used by streaming output owners.  PEM markers
/// are ASCII, so their offsets remain raw-byte offsets even when arbitrary
/// binary output surrounds them.
pub fn private_key_header_markers_bytes(text: &[u8]) -> Vec<(usize, usize, String)> {
    pem_header_bytes_regex()
        .find_iter(text)
        .filter_map(|matched| {
            let marker = std::str::from_utf8(matched.as_bytes()).ok()?;
            pem_marker_kind(marker, "-----BEGIN ")
                .map(|kind| (matched.start(), matched.end(), kind.to_string()))
        })
        .collect()
}

/// Byte-oriented counterpart to [`private_key_end_markers`].
pub fn private_key_end_markers_bytes(text: &[u8]) -> Vec<(usize, usize, String)> {
    pem_end_bytes_regex()
        .find_iter(text)
        .filter_map(|matched| {
            let marker = std::str::from_utf8(matched.as_bytes()).ok()?;
            pem_marker_kind(marker, "-----END ")
                .map(|kind| (matched.start(), matched.end(), kind.to_string()))
        })
        .collect()
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
    let patterns = credential_patterns();
    let (mut result, mut total) = redact_pem_blocks(text, true, false);

    for pat in patterns {
        let mut new_result = String::new();
        let mut last_end = 0;
        let mut found = false;

        // A generated marker may itself contain words such as `SECRET_KEY:`.
        // Never redact inside an existing marker or the second pass would
        // inflate counts and make the reference stale. Structured patterns
        // may select only their secret-value capture so public syntax remains
        // visible and exact edits cannot accidentally delete it.
        for matched in credential_secret_spans(&result, pat) {
            found = true;
            total += 1;
            new_result.push_str(&result[last_end..matched.start()]);
            new_result.push_str(&marker(pat.label, matched.as_str()));
            last_end = matched.end();
        }

        if found {
            new_result.push_str(&result[last_end..]);
            result = new_result;
        }
    }

    (result, total)
}

/// Redact text at a non-owning presentation/persistence boundary.
///
/// Unlike [`redact_credentials_in_text`], this deliberately emits a
/// display-only marker. A runtime/server that does not own the source file
/// must never mint an edit capability that a different Edge process cannot
/// verify. Executor-owned output should use the edit-capable function above.
pub fn redact_credentials_for_display(text: &str) -> (String, usize) {
    redact_credentials_for_display_with_boundary(text, false)
}

/// Redact a bounded tail/head capture whose first byte may be in the middle
/// of a PEM block. An orphan END marker is treated as key material only for
/// this explicitly partial-input path; complete source/RPC views use the
/// ordinary display function and preserve unrelated documentation footers.
pub fn redact_credentials_for_display_partial(text: &str) -> (String, usize) {
    redact_credentials_for_display_with_boundary(text, true)
}

fn redact_credentials_for_display_with_boundary(
    text: &str,
    may_start_inside_pem: bool,
) -> (String, usize) {
    let patterns = credential_patterns();
    let (mut result, mut total) = redact_pem_blocks(text, false, may_start_inside_pem);

    for pat in patterns {
        let mut new_result = String::new();
        let mut last_end = 0;
        let mut found = false;
        for matched in credential_secret_spans(&result, pat) {
            found = true;
            total += 1;
            new_result.push_str(&result[last_end..matched.start()]);
            new_result.push_str(&format!("{MARKER_PREFIX}{}]", pat.label));
            last_end = matched.end();
        }
        if found {
            new_result.push_str(&result[last_end..]);
            result = new_result;
        }
    }
    (result, total)
}

/// Redact a complete PEM private-key block before the ordinary single-line
/// patterns run. If a header is present without a bounded matching END line,
/// conceal the remainder of the logical output rather than leaking the body.
/// This is deliberately conservative: a malformed/oversized key is not a
/// reason to expose bytes that are indistinguishable from key material.
fn redact_pem_blocks(
    text: &str,
    edit_capable: bool,
    may_start_inside_pem: bool,
) -> (String, usize) {
    // A bounded/tail view can begin inside a PEM block and therefore contain
    // an END marker without its BEGIN.  The preceding bytes are then
    // indistinguishable from key material; conceal through that END rather
    // than returning an orphaned base64 body.
    let first_header = pem_header_regex().find(text).map(|matched| matched.start());
    if may_start_inside_pem
        && let Some(end_marker) = pem_end_regex().find(text)
        && first_header.is_none_or(|header_start| end_marker.start() < header_start)
    {
        let (suffix, suffix_count) = redact_pem_blocks(
            &text[end_marker.end()..],
            edit_capable,
            may_start_inside_pem,
        );
        let mut result = String::from("[REDACTED:PRIVATE_KEY]");
        result.push_str(&suffix);
        return (result, suffix_count + 1);
    }

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut total = 0usize;
    let mut search_from = 0usize;

    while let Some(header) = pem_header_regex().find_at(text, search_from) {
        if span_is_generated_marker(text, header.start(), header.end()) {
            search_from = header.end();
            continue;
        }
        let max_end = header.start().saturating_add(PEM_MAX_BYTES);
        let header_kind = pem_marker_kind(header.as_str(), "-----BEGIN ");
        let mut end = None;
        let mut mismatched_end = false;
        if let Some(candidate) = pem_end_regex().find_iter(&text[header.end()..]).next() {
            let absolute_end = header.end().saturating_add(candidate.end());
            if absolute_end <= max_end {
                let candidate_kind = pem_marker_kind(candidate.as_str(), "-----END ");
                if candidate_kind == header_kind {
                    end = Some(absolute_end);
                } else {
                    // A mismatched terminator makes the rest of the source
                    // indistinguishable from key material.  Conceal to EOF
                    // rather than trying to guess which block the model meant.
                    mismatched_end = true;
                }
            }
        }
        let end = if mismatched_end {
            text.len()
        } else {
            end.unwrap_or(text.len())
        };
        result.push_str(&text[cursor..header.start()]);
        let value = &text[header.start()..end];
        if edit_capable {
            result.push_str(&marker("PRIVATE_KEY", value));
        } else {
            result.push_str("[REDACTED:PRIVATE_KEY]");
        }
        total += 1;
        cursor = end;
        search_from = end;
        if end == text.len() {
            break;
        }
    }

    if total == 0 {
        return (text.to_string(), 0);
    }
    result.push_str(&text[cursor..]);
    (result, total)
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
            let exact_trusted_marker = is_exact_trusted_marker(&original);
            if count == 0
                && !exact_trusted_marker
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

/// Redact a line window while retaining the caller's original line
/// coordinates.  A range read cannot safely run the PEM matcher on the slice
/// alone: the requested window may start in the middle of a private-key
/// block.  We therefore scan the complete source for PEM line spans and use a
/// display-only marker for every intersecting line.  Ordinary single-line
/// credentials remain editable when their line is present.
pub fn redact_line_window(raw: &str, start_line: usize, end_line: usize) -> String {
    let lines = raw.lines().collect::<Vec<_>>();
    let start = start_line.saturating_sub(1).min(lines.len());
    let end = end_line.min(lines.len()).max(start);
    let pem_lines = pem_line_ranges(&lines);
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| {
            let line_number = start + offset + 1;
            if pem_lines.iter().any(|(span_start, span_end)| {
                *span_start <= line_number && line_number <= *span_end
            }) {
                "[REDACTED:PRIVATE_KEY]".to_string()
            } else {
                redact_credentials_in_text(line).0
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pem_line_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut active_start: Option<(usize, Option<String>)> = None;
    let mut bytes = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let line_number = index + 1;
        if active_start.is_none()
            && let Some(header) = pem_header_regex().find(line)
        {
            active_start = Some((
                line_number,
                pem_marker_kind(header.as_str(), "-----BEGIN ").map(str::to_string),
            ));
            bytes = 0;
        }
        if active_start.is_some() {
            bytes = bytes.saturating_add(line.len().saturating_add(1));
            let (start, kind) = active_start.as_ref().expect("active PEM span");
            let mut ended = false;
            let mut mismatched = false;
            if let Some(end_marker) = pem_end_regex().find_iter(line).next() {
                if pem_marker_kind(end_marker.as_str(), "-----END ") == kind.as_deref() {
                    ended = true;
                } else {
                    mismatched = true;
                }
            }
            // An unbounded/malformed block is concealed through EOF.  This
            // keeps the range path fail-closed without retaining its bytes.
            if mismatched {
                ranges.push((*start, lines.len()));
                active_start = None;
                break;
            } else if ended {
                ranges.push((*start, line_number));
                active_start = None;
                bytes = 0;
            } else if bytes > PEM_MAX_BYTES {
                ranges.push((*start, lines.len()));
                active_start = None;
                break;
            }
        }
    }
    if let Some((start, _)) = active_start {
        ranges.push((start, lines.len()));
    }
    ranges
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
        let marker_ranges = Arc::new(redaction_marker_ranges(&text));
        let total_lines = text.lines().count();
        Self {
            text: Arc::<str>::from(text),
            marker_ranges,
            total_lines,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn total_bytes(&self) -> usize {
        self.text.len()
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
    let mut ranges = marker_regex()
        .find_iter(text)
        .map(|m| (m.start(), m.end()))
        .collect::<Vec<_>>();
    ranges.extend(
        display_marker_regex()
            .find_iter(text)
            .map(|m| (m.start(), m.end())),
    );
    ranges.sort_unstable();
    ranges.dedup();
    let mut merged = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some((_, previous_end)) = merged.last_mut()
            && start <= *previous_end
        {
            *previous_end = (*previous_end).max(end);
        } else {
            merged.push((start, end));
        }
    }
    merged
}

fn span_is_generated_marker(text: &str, start: usize, end: usize) -> bool {
    marker_regex().find_iter(text).any(|marker| {
        if !(marker.start() <= start && end <= marker.end()) {
            return false;
        }
        trusted_marker(marker.as_str())
    })
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
    // A syntactically valid marker from an untrusted callback is still
    // ordinary text. Only an executor-issued marker with a valid
    // process-bound MAC may suppress a second redaction pass.
    marker_issuer_matches(label.as_str(), nonce.as_str(), issuer_mac.as_str())
}

fn is_exact_trusted_marker(text: &str) -> bool {
    let trimmed = text.trim();
    let Some(matched) = marker_regex().find(trimmed) else {
        return false;
    };
    matched.start() == 0 && matched.end() == trimmed.len() && trusted_marker(matched.as_str())
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

fn pattern_for_label(label: &str) -> Option<&'static str> {
    match label {
        "C1" => Some(r"AKIA[0-9A-Z]{16}"),
        "C2" => Some(
            r#"(?i)(?:aws_secret_access_key|aws_secret_key|\[\s*['"]?(?:aws_secret_access_key|aws_secret_key)['"]?\s*\]|['"](?:aws_secret_access_key|aws_secret_key)['"])\s*[=:]\s*['"]?[A-Za-z0-9/+=._-]{30,}"#,
        ),
        "C7" => Some(r#"[A-Za-z0-9._\-/+=]{20,}"#),
        "C3" => Some(r"gh[pousr]_[A-Za-z0-9_]{36,255}"),
        "C8" => Some(r"hf_[A-Za-z0-9]{34,255}"),
        "C4" => Some(r"(?i)Bearer\s+[A-Za-z0-9._\-/+=]{40,}"),
        "C5" => Some(r"://[^:@\s/]+:[^:@\s/]+@"),
        "C6" => Some(
            r#"(?i)(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key|\[\s*['"]?(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)['"]?\s*\]|['"](?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)['"])\s*[=:]\s*['"]?[^\s'"]{12,}"#,
        ),
        // Bound the captured span after matching in `resolve_redacted_anchor`.
        // A counted `{0,65536}` here exceeds regex's compiled-size limit and
        // makes valid PEM references silently unresolvable.
        "K1" => Some(r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----"),
        "PRIVATE_KEY" => Some(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"),
        "AWS_ACCESS_KEY" => Some(r"AKIA[0-9A-Z]{16}"),
        "AWS_SECRET_KEY" => Some(
            r#"(?i)(?:aws_secret_access_key|aws_secret_key)\s*[=:]\s*['"]?[A-Za-z0-9/+=]{30,}"#,
        ),
        "GITHUB_TOKEN" => Some(r"gh[pousr]_[A-Za-z0-9_]{36,255}"),
        "HUGGINGFACE_TOKEN" => Some(r"hf_[A-Za-z0-9]{34,255}"),
        "BEARER_TOKEN" => Some(r"(?i)Bearer\s+[A-Za-z0-9._\-/+=]{40,}"),
        "CONNECTION_CREDENTIAL" => Some(r"://[^:@\s/]+:[^:@\s/]+@"),
        "SECRET_ASSIGNMENT" => Some(
            r#"(?i)(?:password|passwd|secret_key|api_key|apikey|access_token|auth_token|secret_access_key)\s*[=:]\s*['"]?[^\s'"]{12,}"#,
        ),
        _ => None,
    }
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

    let mut pattern = String::new();
    let mut cursor = 0usize;
    let mut saw_marker = false;
    let mut marker_specs = Vec::new();
    for (capture_group, matched) in (1usize..).zip(marker_regex().captures_iter(anchor)) {
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
        let issuer_mac = matched.get(4).map(|value| value.as_str().to_string());
        let Some(issuer_mac) = issuer_mac.as_deref() else {
            // Three-field markers predate the issuer MAC and cannot prove
            // provenance at an edit/display boundary.  Keeping their legacy
            // value-MAC resolver alive would turn persisted or forged model
            // text into a bearer edit capability across rolling upgrades.
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        };
        if !marker_issuer_matches(label, nonce, issuer_mac) {
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        }
        let Some(label_pattern) = pattern_for_label(label) else {
            return Err(REDACTION_REFERENCE_ERROR.to_string());
        };
        pattern.push_str(&regex::escape(&anchor[cursor..whole.start()]));
        pattern.push('(');
        pattern.push_str(label_pattern);
        pattern.push(')');
        marker_specs.push((
            capture_group,
            label.to_string(),
            nonce.to_string(),
            mac.to_string(),
            whole.as_str().to_string(),
        ));
        cursor = whole.end();
        saw_marker = true;
    }

    if !saw_marker {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    }
    pattern.push_str(&regex::escape(&anchor[cursor..]));
    let matcher = Regex::new(&pattern).map_err(|_| REDACTION_REFERENCE_ERROR.to_string())?;

    let candidates = matcher
        .captures_iter(content)
        .filter_map(|captures| {
            let whole = captures.get(0)?;
            let candidate = whole.as_str();
            for (group, label, nonce, mac, _) in &marker_specs {
                let value = captures.get(*group)?.as_str();
                // The owner-side PEM scanner uses the same bound.  Keep this
                // check outside the regex: embedding a large counted
                // repetition exceeds regex's compiled-size limit.
                if label == "K1" && value.len() > PEM_MAX_BYTES {
                    return None;
                }
                let valid = marker_mac_matches(label, nonce, value, mac);
                if !valid {
                    return None;
                }
            }
            let mut redacted = String::with_capacity(candidate.len());
            let mut candidate_cursor = 0usize;
            for (group, _, _, _, marker) in &marker_specs {
                let span = captures.get(*group)?;
                let relative_start = span.start() - whole.start();
                let relative_end = span.end() - whole.start();
                redacted.push_str(&candidate[candidate_cursor..relative_start]);
                redacted.push_str(marker);
                candidate_cursor = relative_end;
            }
            redacted.push_str(&candidate[candidate_cursor..]);
            (redacted == anchor).then_some(candidate.to_string())
        })
        .collect::<Vec<_>>();

    if candidates.is_empty() {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    }
    if candidates.len() > 1 {
        return Err(REDACTION_REFERENCE_ERROR.to_string());
    }
    Ok(candidates.into_iter().next())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_pem_scanner_handles_chunk_boundary_and_bounded_labels() {
        let kind = format!("{}PRIVATE KEY", "A".repeat(64));
        let header = format!("-----BEGIN {kind}-----");
        let mut bytes = vec![b'x'; 4096 - 3];
        bytes.extend_from_slice(header.as_bytes());
        let markers = private_key_header_markers_bytes(&bytes);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].0, 4096 - 3);

        let overlong = format!(
            "-----BEGIN {}-----",
            format!("{}PRIVATE KEY", "A".repeat(65))
        );
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
        let raw =
            r#"os.environ["AWS_SECRET_ACCESS_KEY"] = "D4w8z9wKN1aVeT3BpQj6kIuN7wH8X0M9KfV5OqzF""#;
        let (redacted, count) = redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("D4w8z9wKN1aVeT3BpQj6kIuN7wH8X0M9KfV5OqzF"));
        let resolved = resolve_redacted_anchor(raw, &redacted, false)
            .expect("indexed assignment reference should parse")
            .expect("indexed assignment marker should resolve");
        assert_eq!(resolved, raw);
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
        assert_eq!(count, 0);
        assert!(display.contains(&legacy));
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
