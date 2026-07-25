//! Disk persistence for large tool results.
//!
//! When a tool result exceeds [`PERSIST_THRESHOLD_CHARS`], the full output is
//! written to `~/.astra/sessions/<session_id>/tool-results/<tool_call_id>.txt`
//! and the in-memory content is replaced with a compact preview + file
//! reference.  This prevents oversized tool outputs from bloating the LLM
//! context window while still preserving the full output for later retrieval.
//!
//! The model-facing reference is a logical session artifact handle, not a
//! physical path. Paths contain runtime-specific user scopes and are easy for
//! the model to copy incorrectly; callers that need the full body must resolve
//! the handle through runtime-owned artifact APIs.

use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;

/// Maximum length of the human-readable portion of a sanitized filename.
/// Full filename has an 8-char hex hash suffix to prevent collisions when
/// different `tool_call_id`s sanitize to the same string.
const SAFE_ID_MAX_READABLE: usize = 64;

/// Sanitize a tool_call_id into a filesystem-safe filename stem.
///
/// Replaces every non-`[A-Za-z0-9_-]` character with `_`, truncates the
/// readable portion, and appends an 8-char hex hash of the original id to
/// prevent collisions (e.g. `a/b` and `a_b` would otherwise both map to `a_b`).
fn safe_filename_stem(tool_call_id: &str) -> String {
    let mut readable: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if readable.chars().count() > SAFE_ID_MAX_READABLE {
        readable = readable.chars().take(SAFE_ID_MAX_READABLE).collect();
    }
    // FNV-1a 64-bit: stable and deterministic across processes/Rust versions,
    // unlike std::collections::hash_map::DefaultHasher which has no stability guarantees.
    let suffix = fnv1a_64(tool_call_id.as_bytes());
    format!("{readable}-{suffix:016x}")
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Tool results larger than this (in chars) are persisted to disk.
/// Default 30 000 chars ≈ ~7 500 tokens.
pub const PERSIST_THRESHOLD_CHARS: usize = 30_000;

/// Number of chars to include as a preview in the replacement message.
const PREVIEW_CHARS: usize = 2_000;

/// XML-style tag that wraps the persisted-output reference.
const PERSISTED_TAG_OPEN: &str = "<persisted-output>";
const PERSISTED_TAG_CLOSE: &str = "</persisted-output>";

/// Subdirectory under the session folder for tool result files.
const TOOL_RESULTS_SUBDIR: &str = "tool-results";

/// Logical URI prefix for a persisted tool result scoped to the current
/// session.
pub const SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX: &str = "artifact://session/tool-result/";

/// Default payload size for one model-requested artifact window.
pub const DEFAULT_TOOL_RESULT_WINDOW_BYTES: usize = 8 * 1024;

/// Largest payload size accepted for one model-requested artifact window.
///
/// This is a transport-window bound, not a cap on the result itself: callers
/// can continue from `next_offset` until the complete durable result is read.
pub const MAX_TOOL_RESULT_WINDOW_BYTES: usize = 64 * 1024;

/// A UTF-8-safe byte window over one persisted tool result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedToolResultWindow {
    pub content: String,
    pub offset: usize,
    pub next_offset: usize,
    pub total_bytes: usize,
}

impl PersistedToolResultWindow {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.next_offset >= self.total_bytes
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PersistedFormat {
    PlainText,
    PrettyJson,
}

struct PersistedContent {
    text: String,
    format: PersistedFormat,
}

/// FNV-1a 64-bit hash — stable and deterministic across processes and Rust versions.
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// If `content` exceeds the persistence threshold, write it to disk and return
/// a compact replacement string with a preview and stable session artifact id.
///
/// Returns `None` if the content is small enough to keep inline, or if disk
/// persistence fails (in which case the caller should use the original content).
///
/// `session_dir` is `~/.astra/sessions/<session_id>/`.
pub fn maybe_persist_tool_result(
    session_dir: &Path,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Option<String> {
    if content.chars().count() <= PERSIST_THRESHOLD_CHARS {
        return None;
    }

    let dir = session_dir.join(TOOL_RESULTS_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "tool_result_storage: failed to create dir"
        );
        return None;
    }

    // Sanitize tool_call_id for filesystem safety (with hash suffix to avoid collisions)
    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = dir.join(format!("{safe_id}.txt"));

    let persisted = persistable_content(content);
    if let Err(e) = std::fs::write(&file_path, persisted.text.as_str()) {
        tracing::warn!(
            path = %file_path.display(),
            error = %e,
            "tool_result_storage: failed to write"
        );
        return None;
    }

    Some(build_replacement(
        tool_call_id,
        tool_name,
        content,
        &persisted,
    ))
}

/// Persist a tool result to disk unconditionally (no size threshold).
///
/// Used by compaction to save full content before clearing. Unlike
/// `maybe_persist_tool_result`, this always writes regardless of content size.
/// Returns `true` on success.
pub fn maybe_persist_tool_result_unconditional(
    session_dir: &Path,
    tool_call_id: &str,
    // tool_name is reserved for future metadata embedding in the persisted file header.
    // Currently unused because the file is identified solely by tool_call_id.
    _tool_name: &str,
    content: &str,
) -> bool {
    let dir = session_dir.join(TOOL_RESULTS_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "tool_result_storage: failed to create dir"
        );
        return false;
    }

    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = dir.join(format!("{safe_id}.txt"));

    if let Err(e) = std::fs::write(&file_path, content) {
        tracing::warn!(
            path = %file_path.display(),
            error = %e,
            "tool_result_storage: failed to write"
        );
        return false;
    }
    true
}

/// Read a previously-persisted tool result back from disk.
///
/// Returns `None` if the file doesn't exist or can't be read.
pub fn read_persisted_result(session_dir: &Path, tool_call_id: &str) -> Option<String> {
    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = session_dir
        .join(TOOL_RESULTS_SUBDIR)
        .join(format!("{safe_id}.txt"));
    std::fs::read_to_string(file_path).ok()
}

/// Parse a session-local logical tool-result handle without exposing a
/// filesystem path.
///
/// Handles use URL-safe Base64 rather than embedding provider-supplied call
/// ids directly. Provider identifiers are opaque strings, not a protocol we
/// control; encoding them preserves every valid id while keeping a handle to
/// one non-traversable URI segment.
#[must_use]
pub fn parse_session_tool_result_artifact_uri(value: &str) -> Option<String> {
    let encoded = value.strip_prefix(SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX)?;
    if encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let tool_call_id = String::from_utf8(decoded).ok()?;
    (!tool_call_id.is_empty()).then_some(tool_call_id)
}

/// Read one UTF-8-safe byte window from a persisted result.
///
/// The caller owns the session directory, so a logical handle can never cross
/// session ownership boundaries. `next_offset` is always a valid UTF-8
/// boundary and is the only continuation cursor a model needs to retain.
pub fn read_persisted_result_window(
    session_dir: &Path,
    tool_call_id: &str,
    offset: usize,
    max_bytes: usize,
) -> Result<Option<PersistedToolResultWindow>, String> {
    if max_bytes == 0 || max_bytes > MAX_TOOL_RESULT_WINDOW_BYTES {
        return Err(format!(
            "max_bytes must be between 1 and {MAX_TOOL_RESULT_WINDOW_BYTES}"
        ));
    }

    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = session_dir
        .join(TOOL_RESULTS_SUBDIR)
        .join(format!("{safe_id}.txt"));
    let total_bytes = match std::fs::metadata(&file_path) {
        Ok(metadata) => usize::try_from(metadata.len())
            .map_err(|_| "persisted result is too large for this runtime".to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to inspect persisted result: {error}")),
    };
    if offset > total_bytes {
        return Err(format!(
            "offset {offset} is past the end of this {total_bytes}-byte tool result"
        ));
    }
    if offset == total_bytes {
        return Ok(Some(PersistedToolResultWindow {
            content: String::new(),
            offset,
            next_offset: offset,
            total_bytes,
        }));
    }

    let mut file = std::fs::File::open(&file_path)
        .map_err(|error| format!("failed to open persisted result: {error}"))?;
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|error| format!("failed to seek persisted result: {error}"))?;

    // Read a few extra bytes so a UTF-8 scalar straddling `max_bytes` can
    // still make forward progress without returning malformed text.
    let available = total_bytes.saturating_sub(offset);
    let read_len = available.min(max_bytes.saturating_add(3));
    let mut bytes = vec![0_u8; read_len];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("failed to read persisted result: {error}"))?;

    if offset > 0 && bytes[0] & 0b1100_0000 == 0b1000_0000 {
        return Err(format!(
            "offset {offset} is not a UTF-8 boundary; continue with the next_offset returned by the prior window"
        ));
    }

    let budget = available.min(max_bytes);
    let mut consumed = match std::str::from_utf8(&bytes[..budget]) {
        Ok(_) => budget,
        Err(error) if error.error_len().is_some() => {
            return Err(format!("persisted result is not valid UTF-8: {error}"));
        }
        Err(error) => error.valid_up_to(),
    };
    if consumed == 0 {
        // `bytes` includes up to three extra bytes, enough to complete one
        // valid UTF-8 scalar. Returning that scalar is preferable to an empty
        // non-terminal window, which would make recovery unable to progress.
        let scalar_len = utf8_scalar_len(bytes[0])
            .ok_or_else(|| "persisted result is not valid UTF-8".to_string())?;
        let scalar = bytes
            .get(..scalar_len)
            .ok_or_else(|| "persisted result ended inside a UTF-8 scalar".to_string())?;
        std::str::from_utf8(scalar)
            .map_err(|error| format!("persisted result is not valid UTF-8: {error}"))?;
        consumed = scalar_len;
    }
    let content = std::str::from_utf8(&bytes[..consumed])
        .map_err(|error| format!("persisted result is not valid UTF-8: {error}"))?
        .to_string();

    Ok(Some(PersistedToolResultWindow {
        content,
        offset,
        next_offset: offset.saturating_add(consumed),
        total_bytes,
    }))
}

/// Resolve the model-facing artifact fields accepted by `introspect`.
///
/// Returning `None` means this is an ordinary introspection request. A present
/// `artifact` field is always handled here, including malformed requests, so
/// callers never silently fall back to an unrelated runtime snapshot.
pub fn resolve_session_tool_result_artifact_request(
    session_dir: &Path,
    args: &Value,
) -> Option<Result<String, String>> {
    let artifact = args.get("artifact")?;
    let artifact = match artifact.as_str() {
        Some(value) => value,
        None => {
            return Some(Err(
                "artifact must be a session tool-result handle string".to_string()
            ));
        }
    };
    let tool_call_id = match parse_session_tool_result_artifact_uri(artifact) {
        Some(tool_call_id) => tool_call_id,
        None => {
            return Some(Err(
                "artifact must be a valid artifact://session/tool-result/<opaque_token> handle"
                    .to_string(),
            ));
        }
    };
    let offset = match args.get("offset") {
        Some(value) => match value.as_u64().and_then(|value| usize::try_from(value).ok()) {
            Some(offset) => offset,
            None => return Some(Err("offset must be a non-negative integer".to_string())),
        },
        None => 0,
    };
    let max_bytes = match args.get("max_bytes") {
        Some(value) => match value.as_u64().and_then(|value| usize::try_from(value).ok()) {
            Some(max_bytes) => max_bytes,
            None => return Some(Err("max_bytes must be a positive integer".to_string())),
        },
        None => DEFAULT_TOOL_RESULT_WINDOW_BYTES,
    };

    Some(read_persisted_result_window(session_dir, &tool_call_id, offset, max_bytes).and_then(
        |window| {
            let Some(window) = window else {
                return Err("tool-result artifact was not found in the active session".to_string());
            };
            let handle = session_tool_result_artifact_uri(&tool_call_id);
            let continuation = if window.is_complete() {
                "Complete.".to_string()
            } else {
                format!(
                    "Continue with introspect(artifact=\"{handle}\", offset={}, max_bytes={max_bytes}).",
                    window.next_offset
                )
            };
            Ok(format!(
                "<tool-result-window>\n\
                 Artifact handle: {handle}\n\
                 Bytes: [{}..{}) of {}\n\n\
                 {}\n\n\
                 {continuation}\n\
                 </tool-result-window>",
                window.offset, window.next_offset, window.total_bytes, window.content,
            ))
        },
    ))
}

/// Return the storage directory for tool results under a session.
pub fn tool_results_dir(session_dir: &Path) -> PathBuf {
    session_dir.join(TOOL_RESULTS_SUBDIR)
}

/// Return the model-facing logical artifact URI for a persisted tool result.
#[must_use]
pub fn session_tool_result_artifact_uri(tool_call_id: &str) -> String {
    format!(
        "{SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(tool_call_id)
    )
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn persistable_content(content: &str) -> PersistedContent {
    if content.lines().count() <= 1
        && let Ok(value) = serde_json::from_str::<Value>(content)
        && let Ok(pretty) = serde_json::to_string_pretty(&value)
    {
        return PersistedContent {
            text: pretty,
            format: PersistedFormat::PrettyJson,
        };
    }
    PersistedContent {
        text: content.to_string(),
        format: PersistedFormat::PlainText,
    }
}

/// Return the encoded length of a valid UTF-8 scalar from its first byte.
/// Continuation bytes and invalid leading bytes deliberately return `None`.
fn utf8_scalar_len(first: u8) -> Option<usize> {
    match first {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        _ => None,
    }
}

fn build_replacement(
    tool_call_id: &str,
    tool_name: &str,
    original_content: &str,
    persisted: &PersistedContent,
) -> String {
    let total_chars = original_content.chars().count();
    let stored_chars = persisted.text.chars().count();
    let preview: String = persisted.text.chars().take(PREVIEW_CHARS).collect();

    // Try to cut at a newline for cleaner preview
    let preview = if let Some(nl_pos) = preview.rfind('\n') {
        if nl_pos > PREVIEW_CHARS / 2 {
            &preview[..nl_pos]
        } else {
            &preview
        }
    } else {
        &preview
    };
    let format_note = match persisted.format {
        PersistedFormat::PlainText => String::new(),
        PersistedFormat::PrettyJson => format!(
            "Stored as pretty JSON for readable line ranges ({stored_chars} chars on disk; semantic JSON unchanged).\n         "
        ),
    };

    format!(
        "{PERSISTED_TAG_OPEN}\n\
         Tool `{tool_name}` produced {total_chars} chars of output.\n\
         Tool result id: {tool_call_id}\n\
         Artifact handle: {artifact_uri}\n\
         Storage: session tool-result artifact.\n\
         Read bounded windows with introspect(artifact=\"{artifact_uri}\", offset=0, max_bytes={DEFAULT_TOOL_RESULT_WINDOW_BYTES}); \
         continue with the returned next_offset. Do not search, copy, or read physical local session paths.\n\
         {format_note}\
         \n\
         Preview (first ~{prev_len} chars):\n\
         {preview}\n\
         ...[truncated — full output is available through the session tool-result artifact, not workspace filesystem tools]\n\
         {PERSISTED_TAG_CLOSE}",
        prev_len = preview.len(),
        artifact_uri = session_tool_result_artifact_uri(tool_call_id),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_result_returns_none() {
        let dir = std::env::temp_dir().join("trs_small");
        let _ = std::fs::create_dir_all(&dir);
        let content = "hello world";
        assert!(maybe_persist_tool_result(&dir, "call-1", "bash", content).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_result_persisted_and_replaced() {
        let dir = std::env::temp_dir().join("trs_large");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let content = "x".repeat(PERSIST_THRESHOLD_CHARS + 100);
        let replacement = maybe_persist_tool_result(&dir, "call-42", "bash", &content).unwrap();

        // Replacement contains the tag
        assert!(replacement.contains(PERSISTED_TAG_OPEN));
        assert!(replacement.contains(PERSISTED_TAG_CLOSE));
        assert!(replacement.contains("bash"));
        assert!(replacement.contains("Tool result id: call-42"));
        assert!(replacement.contains(&format!(
            "Artifact handle: {}",
            session_tool_result_artifact_uri("call-42")
        )));
        assert!(replacement.contains("session tool-result artifact"));
        assert!(replacement.contains("introspect(artifact="));
        assert!(!replacement.contains("read_file"));
        assert!(!replacement.contains("File:"));
        assert!(!replacement.contains("tool-results"));
        assert!(!replacement.contains("~/.astra"));

        // File was written (name is `<safe_id>-<hash>.txt` to avoid collisions)
        let results_dir = dir.join(TOOL_RESULTS_SUBDIR);
        let entries: Vec<_> = std::fs::read_dir(&results_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let file_path = entries[0].path();
        let fname = file_path.file_name().unwrap().to_string_lossy();
        assert!(fname.starts_with("call-42-"));
        assert!(fname.ends_with(".txt"));
        let stored = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(stored.len(), content.len());

        // Roundtrip read via public API
        let recovered = read_persisted_result(&dir, "call-42").unwrap();
        assert_eq!(recovered, content);

        // Replacement is much smaller than original
        assert!(replacement.len() < content.len() / 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_persisted_result_roundtrip() {
        let dir = std::env::temp_dir().join("trs_read");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let content = "y".repeat(PERSIST_THRESHOLD_CHARS + 50);
        let _ = maybe_persist_tool_result(&dir, "call-99", "grep", &content);

        let recovered = read_persisted_result(&dir, "call-99").unwrap();
        assert_eq!(recovered, content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_single_line_json_is_persisted_as_pretty_json() {
        let dir = std::env::temp_dir().join("trs_pretty_json");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let rows: Vec<_> = (0..1500)
            .map(|idx| serde_json::json!({"slot_index": idx, "summary": format!("review {idx}")}))
            .collect();
        let content = serde_json::json!({
            "status": "completed",
            "results": rows
        })
        .to_string();
        assert!(
            content.chars().count() > PERSIST_THRESHOLD_CHARS,
            "test setup must cross persistence threshold"
        );

        let replacement =
            maybe_persist_tool_result(&dir, "call-json", "agent_fanout", &content).unwrap();
        let recovered = read_persisted_result(&dir, "call-json").unwrap();

        assert!(
            replacement.contains("Stored as pretty JSON"),
            "{replacement}"
        );
        assert!(replacement.contains("\"results\""), "{replacement}");
        assert!(
            recovered.lines().count() > 100,
            "persisted JSON must be readable by line range, got {} lines",
            recovered.lines().count()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&recovered).unwrap(),
            serde_json::from_str::<Value>(&content).unwrap()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_persisted_result_missing_returns_none() {
        let dir = std::env::temp_dir().join("trs_missing");
        let _ = std::fs::create_dir_all(&dir);
        assert!(read_persisted_result(&dir, "nonexistent").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_artifact_uri_is_stable_and_path_free() {
        let uri = session_tool_result_artifact_uri("call_abc123");

        assert_eq!(uri, "artifact://session/tool-result/Y2FsbF9hYmMxMjM");
        assert!(!uri.contains(".astra"));
        assert!(!uri.contains("tool-results/"));
    }

    #[test]
    fn artifact_handles_are_opaque_and_round_trip_every_provider_call_id() {
        let provider_id = "call/with spaces: provider-owned 😀";
        let uri = session_tool_result_artifact_uri(provider_id);
        assert_eq!(
            parse_session_tool_result_artifact_uri(&uri),
            Some(provider_id.to_string())
        );
        assert!(!uri.contains(provider_id));
        for invalid in [
            "artifact://session/tool-result/",
            "artifact://session/tool-result/../other-session",
            "artifact://session/tool-result/call/child",
            "artifact://session/tool-result/call+child",
            "artifact://session/tool-result/call_abc-123",
            "file:///tmp/result.txt",
        ] {
            assert_eq!(
                parse_session_tool_result_artifact_uri(invalid),
                None,
                "{invalid}"
            );
        }
    }

    #[test]
    fn artifact_windows_round_trip_unicode_without_exposing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let content = "前缀😀\n".repeat(10_000);
        assert!(
            maybe_persist_tool_result(dir.path(), "call-unicode", "bash", &content).is_some(),
            "setup must create a session artifact"
        );

        let mut offset = 0;
        let mut recovered = String::new();
        while offset < content.len() {
            let window = read_persisted_result_window(dir.path(), "call-unicode", offset, 7)
                .unwrap()
                .expect("persisted result exists");
            assert!(window.next_offset > offset, "window must make progress");
            assert!(content.is_char_boundary(window.offset));
            assert!(content.is_char_boundary(window.next_offset));
            recovered.push_str(&window.content);
            offset = window.next_offset;
        }
        assert_eq!(recovered, content);

        let rendered = resolve_session_tool_result_artifact_request(
            dir.path(),
            &serde_json::json!({
                "artifact": session_tool_result_artifact_uri("call-unicode"),
                "max_bytes": 7,
            }),
        )
        .expect("artifact request is recognized")
        .expect("artifact request succeeds");
        assert!(rendered.contains(&format!(
            "Artifact handle: {}",
            session_tool_result_artifact_uri("call-unicode")
        )));
        assert!(rendered.contains("Continue with introspect("));
        assert!(!rendered.contains(dir.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn artifact_windows_reject_non_boundary_and_cross_session_lookup() {
        let owner = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let content = "évidence\n".repeat(6_000);
        assert!(
            maybe_persist_tool_result(owner.path(), "call-boundary", "grep", &content).is_some()
        );

        let boundary_error = read_persisted_result_window(owner.path(), "call-boundary", 1, 32)
            .expect_err("middle of a UTF-8 scalar must not be accepted");
        assert!(boundary_error.contains("not a UTF-8 boundary"));
        assert!(
            read_persisted_result_window(other.path(), "call-boundary", 0, 32)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn artifact_windows_fail_closed_for_corrupt_content() {
        let dir = tempfile::tempdir().unwrap();
        let results = tool_results_dir(dir.path());
        std::fs::create_dir_all(&results).unwrap();
        let path = results.join(format!("{}.txt", safe_filename_stem("call-corrupt")));
        std::fs::write(path, b"valid prefix\xff").unwrap();

        let error = read_persisted_result_window(dir.path(), "call-corrupt", 0, 64)
            .expect_err("a corrupt artifact must not yield a partial evidence window");
        assert!(error.contains("not valid UTF-8"), "{error}");
    }

    #[test]
    fn sanitizes_tool_call_id_for_filesystem() {
        let dir = std::env::temp_dir().join("trs_sanitize");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let content = "z".repeat(PERSIST_THRESHOLD_CHARS + 10);
        let replacement =
            maybe_persist_tool_result(&dir, "call/../../etc/passwd", "bash", &content);
        assert!(replacement.is_some());

        // Verify the file was created (with sanitized name)
        let results_dir = dir.join(TOOL_RESULTS_SUBDIR);
        assert!(results_dir.exists());
        let entries: Vec<_> = std::fs::read_dir(&results_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        // The filename should not contain path separators
        let filename = entries[0].file_name().to_string_lossy().to_string();
        assert!(!filename.contains('/'));
        assert!(!filename.contains(".."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_cuts_at_newline() {
        let dir = std::env::temp_dir().join("trs_preview_nl");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        // Build content with newlines
        let mut content = String::new();
        for i in 0..5000 {
            content.push_str(&format!("line {i}\n"));
        }

        let replacement = maybe_persist_tool_result(&dir, "call-nl", "bash", &content).unwrap();
        // Preview should end at a clean newline
        assert!(replacement.contains("Preview"));
        assert!(replacement.contains("line "));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fnv1a_64_known_vectors() {
        // empty string → offset basis
        assert_eq!(super::fnv1a_64(b""), 0xcbf29ce484222325);
        // "a" → well-known FNV-1a-64 value
        assert_eq!(super::fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn threshold_boundary_exact() {
        let dir = std::env::temp_dir().join("trs_boundary");
        let _ = std::fs::create_dir_all(&dir);

        // Exactly at threshold → not persisted
        let at_limit = "a".repeat(PERSIST_THRESHOLD_CHARS);
        assert!(maybe_persist_tool_result(&dir, "c1", "bash", &at_limit).is_none());

        // One over → persisted
        let over_limit = "a".repeat(PERSIST_THRESHOLD_CHARS + 1);
        assert!(maybe_persist_tool_result(&dir, "c2", "bash", &over_limit).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
