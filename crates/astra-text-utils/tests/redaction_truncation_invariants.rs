//! Budget-sweep invariants for the redaction-aware truncation and paging
//! helpers: no budget may emit a partial redaction marker or re-expose a
//! redacted value, and redaction itself must be idempotent.

use astra_text_utils::credential_redaction::{
    SafeOutputProjection, redact_credentials_for_display, redact_credentials_in_text,
    redact_line_window, redaction_marker_status, truncate_redacted_head_tail,
    truncate_redacted_head_tail_chars, truncate_redacted_output,
};
use astra_text_utils::str_preview::{shorten_path, truncate_line};

const SECRET: &str = "abcdefghijklmnop";
const MARKER_PREFIX: &str = "[REDACTED:";
const TRUNCATION_NOTICE: &str = "\n[truncated]";

fn complete_marker_len(text: &str, at: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    if !bytes.get(at..)?.starts_with(MARKER_PREFIX.as_bytes()) {
        return None;
    }
    let mut i = at + MARKER_PREFIX.len();
    if !bytes.get(i).copied().map_or(false, |b| b.is_ascii_uppercase()) {
        return None;
    }
    i += 1;
    while bytes
        .get(i)
        .copied()
        .map_or(false, |b| is_marker_label_byte(b))
    {
        i += 1;
    }
    if bytes.get(i) == Some(&b']') {
        return Some(i + 1 - at);
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    i = hex32_field(bytes, i)?;
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    i = hex32_field(bytes, i)?;
    if bytes.get(i) == Some(&b':') {
        i += 1;
        i = hex32_field(bytes, i)?;
    }
    if bytes.get(i) == Some(&b']') {
        Some(i + 1 - at)
    } else {
        None
    }
}

fn every_marker_is_complete(text: &str) -> bool {
    for (at, _) in text.match_indices(MARKER_PREFIX) {
        if complete_marker_len(text, at).is_none() {
            return false;
        }
    }
    // In-prefix fragments: a clipped `[REDACTED:` prefix anywhere in the
    // window, not only at the tail. Fixture text carries `[` only inside
    // markers, and truncation notices never start with `[R`.
    let bytes = text.as_bytes();
    for at in 0..text.len() {
        if bytes[at] != b'[' {
            continue;
        }
        if complete_marker_len(text, at).is_some() {
            continue;
        }
        for len in 2..=MARKER_PREFIX.len() {
            if at + len <= text.len() && text[at..].starts_with(&MARKER_PREFIX[..len]) {
                return false;
            }
        }
    }
    !text.ends_with('[')
}

fn hex32_field(bytes: &[u8], i: usize) -> Option<usize> {
    let Some(limit) = i.checked_add(32) else {
        return None;
    };
    let Some(field) = bytes.get(i..limit) else {
        return None;
    };
    for &b in field {
        if !is_lower_hex(b) {
            return None;
        }
    }
    Some(limit)
}

fn is_lower_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b >= b'a' && b <= b'f')
}

fn is_marker_label_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_uppercase() || b.is_ascii_digit()
}

fn marker_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut at = 0;
    while at < text.len() {
        if let Some(len) = complete_marker_len(text, at) {
            spans.push((at, at + len));
            at += len;
        } else {
            at += 1;
        }
    }
    spans
}

fn redacted_fixture() -> String {
    let raw = format!("数据 password={SECRET} 行\n").repeat(40);
    let (redacted, count) = redact_credentials_in_text(&raw);
    assert_eq!(count, 40);
    redacted
}

#[test]
fn truncation_helpers_never_split_a_marker_for_any_budget() {
    let redacted = redacted_fixture();
    for budget in (0..=redacted.len() + 1).step_by(7) {
        for output in [
            truncate_redacted_head_tail(&redacted, budget),
            truncate_redacted_head_tail_chars(&redacted, budget),
            truncate_redacted_output(redacted.clone(), budget),
        ] {
            assert!(every_marker_is_complete(&output), "budget={budget}");
            assert!(!output.contains(SECRET), "budget={budget}");
        }
    }

    // Exact expectations at the first marker's boundary budgets instead of
    // re-deriving them from the oracle under test.
    let spans = marker_spans(&redacted);
    assert_eq!(spans.len(), 40, "every fixture marker must parse");
    let (mstart, mend) = spans[0];
    assert!(redacted.is_char_boundary(mstart + 1));
    for budget in [mstart, mstart + 1, mend - 1] {
        assert_eq!(
            truncate_redacted_output(redacted.clone(), budget),
            format!("{}{TRUNCATION_NOTICE}", &redacted[..mstart]),
            "budget={budget} must fall back to the marker start",
        );
    }
    assert_eq!(
        truncate_redacted_output(redacted.clone(), mend),
        format!("{}{TRUNCATION_NOTICE}", &redacted[..mend]),
        "budget == marker end keeps the whole marker plus notice",
    );
}

#[test]
fn truncation_helpers_are_identity_within_budget() {
    let redacted = redacted_fixture();
    let chars = redacted.chars().count();
    assert_eq!(
        truncate_redacted_head_tail(&redacted, redacted.len()),
        redacted
    );
    assert_eq!(
        truncate_redacted_head_tail_chars(&redacted, chars),
        redacted
    );
    assert_eq!(
        truncate_redacted_output(redacted.clone(), redacted.len()),
        redacted
    );
}

#[test]
fn display_and_edit_redaction_are_idempotent() {
    let raw = format!(
        "token ghp_abcdefghijklmnopqrstuvwxyz0123456789AB\npassword={SECRET}\n\
         -----BEGIN PRIVATE KEY-----\nMIIabc\n-----END PRIVATE KEY-----\n"
    );
    let (display, display_count) = redact_credentials_for_display(&raw);
    assert_eq!(display_count, 3);
    assert_eq!(
        redact_credentials_for_display(&display),
        (display.clone(), 0)
    );

    let (edit, edit_count) = redact_credentials_in_text(&raw);
    assert_eq!(edit_count, 3);
    assert_eq!(redact_credentials_in_text(&edit), (edit.clone(), 0));
    assert_eq!(redaction_marker_status(&edit), (true, true));
    assert_eq!(redaction_marker_status(&display), (false, true));
    assert_eq!(redaction_marker_status("plain"), (false, false));
}

#[test]
fn paging_a_projection_reaches_the_end_without_partial_markers() {
    let (safe, _) = redact_credentials_for_display(&format!("x password={SECRET} y\n").repeat(20));
    for page in [1usize, 5, 17, 23, 64, 4096] {
        let projection = SafeOutputProjection::new(safe.clone());
        let mut offset = 0;
        let mut pages = 0;
        loop {
            let (window, next, total, _) = projection.window(offset, page);
            assert!(
                every_marker_is_complete(&window),
                "page={page} offset={offset}"
            );
            if next >= total {
                break;
            }
            assert!(next > offset, "paging stalled: page={page} offset={offset}");
            offset = next;
            pages += 1;
            assert!(pages <= safe.len(), "paging did not terminate: page={page}");
        }
    }
}

#[test]
fn line_window_clamps_out_of_range_requests() {
    let raw = format!("a\npassword={SECRET}\nb");
    assert!(!redact_line_window(&raw, 2, 2).contains(SECRET));
    assert_eq!(redact_line_window(&raw, 1, 1), "a");
    assert_eq!(redact_line_window(&raw, 0, 0), "");
    assert_eq!(redact_line_window(&raw, 5, 9), "");
    assert_eq!(redact_line_window(&raw, 3, 1), "");
    assert_eq!(redact_line_window(&raw, 3, 99), "b");
}

#[test]
fn truncate_line_uses_first_line_and_counts_scalars() {
    assert_eq!(truncate_line("first\nsecond", 20), "first");
    assert_eq!(truncate_line("hello world", 5), "hell…");
    assert_eq!(truncate_line("数据数据数据", 4), "数据数…");
    assert_eq!(truncate_line("", 3), "");
}

#[test]
fn shorten_path_output_respects_budget() {
    for (path, budget) in [
        ("/a/b/c/d/e/file.txt", 15),
        ("/very/long/directory/name/file.rs", 12),
        ("/x/数据目录/文件名.rs", 9),
        ("/a/b/c.txt", 3),
    ] {
        let short = shorten_path(path, budget);
        assert!(
            short.chars().count() <= budget,
            "{path} -> {short} > {budget}"
        );
    }
}

#[test]
fn marker_oracle_rejects_clipped_marker_fragments() {
    // Intact marker forms and marker-free notices must pass the scan.
    for (case, text) in [
        ("display marker", "[REDACTED:SECRET_ASSIGNMENT]"),
        ("executor marker", "[REDACTED:C6:8124567890abcdef8124567890abcdef:8124567890abcdef8124567890abcdef]"),
        ("marker amid text", "x [REDACTED:SECRET_ASSIGNMENT] y"),
        ("notice only", "\n[truncated]"),
        ("empty window", ""),
    ] {
        assert!(every_marker_is_complete(text), "must accept intact: {case}");
    }
    // Clipped markers: each was accepted by the previous
    // split("[REDACTED:").all(contains(']')) oracle because a later `]`
    // in the truncation notice closed the fragment.
    for (case, text) in [
        ("cut mid nonce", "password=[REDACTED:C6:8124\n[truncated]"),
        ("cut mid hex field", "password=[REDACTED:C6:8124567890abcdef8124567890abcdef\n[truncated]"),
        ("executor missing close", "[REDACTED:C6:8124567890abcdef8124567890abcdef:8124567890abcdef8124567890abcdef\n[truncated]"),
        ("display label cut deep", "[REDACTED:SECRET_ASSIG\n[truncated]"),
        ("bare prefix at tail", "password=[REDACTED:"),
        ("prefix fragment mid text", "password=[R\n[truncated]"),
        ("prefix fragment deeper", "x=[RED\n[truncated]"),
        ("second marker clipped", "a [REDACTED:SECRET_ASSIGNMENT] b [REDACTED:PRIVATE_KE\n[truncated]"),
    ] {
        assert!(!every_marker_is_complete(text), "must reject clipped: {case}");
    }
    // Pin the replaced oracle's blind spot: it accepted the clipped nonce.
    fn weak_every_marker_is_complete(text: &str) -> bool {
        text.split("[REDACTED:").skip(1).all(|rest| rest.contains(']'))
    }
    assert!(weak_every_marker_is_complete("password=[REDACTED:C6:8124\n[truncated]"));
}
