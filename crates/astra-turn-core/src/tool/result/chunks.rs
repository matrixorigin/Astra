//! Deterministic, source-bound chunks for persisted tool results.
//!
//! Chunks are navigation over one immutable, owner-bound artifact. They are
//! not summaries and never replace the durable source. Every byte range is a
//! UTF-8-safe slice of the exact bytes named by the artifact descriptor.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const TOOL_RESULT_CHUNKER_VERSION: u32 = 1;
pub const DEFAULT_TOOL_RESULT_CHUNK_BYTES: usize = 2 * 1024;
pub const DEFAULT_TOOL_RESULT_SCAN_BYTES: usize = 64 * 1024;
pub const MAX_TOOL_RESULT_CHUNK_BYTES: usize = 8 * 1024;
pub const MAX_TOOL_RESULT_SCAN_BYTES: usize = 64 * 1024;
pub const MAX_TOOL_RESULT_CHUNKS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultChunk {
    pub id: String,
    pub start_byte: u64,
    pub end_byte: u64,
    /// One-based source line containing `start_byte`.
    pub start_line: u32,
    /// One-based source line containing the last byte in this chunk.
    pub end_line: u32,
    /// False when a long logical line had to be split at a UTF-8 boundary.
    /// This says nothing about JSON or other higher-level syntax.
    pub line_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolResultChunkProjection {
    pub chunker_version: u32,
    pub source_bytes: u64,
    pub scanned_bytes: u64,
    pub scan_complete: bool,
    pub chunks: Vec<ToolResultChunk>,
}

/// Build a bounded index over a verified prefix of the content named by
/// `descriptor`.
///
/// Newline-terminated records stay whole when they fit the soft target.
/// A single UTF-8 scalar may exceed that target so every non-empty verified
/// window still makes progress. Oversized lines are split at UTF-8 boundaries
/// and explicitly marked incomplete. The
/// projection may cover only a source prefix; callers must preserve
/// `scan_complete=false` rather than implying that unscanned bytes were judged.
pub(crate) fn project_tool_result_chunks(
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    window: &crate::tool_result_storage::PersistedToolResultWindow,
    target_chunk_bytes: usize,
    max_chunks: usize,
) -> Result<ToolResultChunkProjection, &'static str> {
    if target_chunk_bytes == 0
        || target_chunk_bytes > MAX_TOOL_RESULT_CHUNK_BYTES
        || max_chunks == 0
        || max_chunks > MAX_TOOL_RESULT_CHUNKS
    {
        return Err("tool-result chunk budgets are outside supported bounds");
    }
    if !descriptor.document_kind.is_result()
        || descriptor.version
            != astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION
        || descriptor.call_id.trim().is_empty()
        || !crate::tool_result_storage::is_valid_tool_result_run_id(&descriptor.run_id)
        || descriptor.content_sha256.len() != 64
        || !descriptor
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("invalid tool-result artifact descriptor");
    }
    if window.offset != 0
        || window.total_bytes as u64 != descriptor.byte_len
        || window.next_offset != window.content.len()
        || window.next_offset > window.total_bytes
        || (window.content.is_empty() && window.total_bytes != 0)
        || window.content.len() > MAX_TOOL_RESULT_SCAN_BYTES.saturating_add(3)
    {
        return Err("tool-result chunk source is not a verified prefix window");
    }
    let source = &window.content;
    let scan_end = source.len();
    let mut chunks = Vec::new();
    let mut cursor = 0usize;
    let mut line = 1u32;

    while cursor < scan_end && chunks.len() < max_chunks {
        let mut hard_end = cursor.saturating_add(target_chunk_bytes).min(scan_end);
        while hard_end > cursor && !source.is_char_boundary(hard_end) {
            hard_end -= 1;
        }
        if hard_end == cursor {
            hard_end = source[cursor..scan_end]
                .char_indices()
                .nth(1)
                .map_or(scan_end, |(offset, _)| cursor + offset);
        }

        let bounded = &source[cursor..hard_end];
        let end = bounded
            .rfind('\n')
            .map(|offset| cursor + offset + 1)
            .filter(|end| *end > cursor)
            .unwrap_or(hard_end);
        let text = &source[cursor..end];
        let newline_count = text.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let end_line = if text.ends_with('\n') {
            line.saturating_add(newline_count.saturating_sub(1))
        } else {
            line.saturating_add(newline_count)
        };
        let line_complete = (cursor == 0 || source.as_bytes()[cursor - 1] == b'\n')
            && (end == window.total_bytes || source.as_bytes()[end - 1] == b'\n');
        chunks.push(ToolResultChunk {
            id: chunk_id(descriptor, cursor, end),
            start_byte: cursor as u64,
            end_byte: end as u64,
            start_line: line,
            end_line,
            line_complete,
        });
        line = line.saturating_add(newline_count);
        cursor = end;
    }

    Ok(ToolResultChunkProjection {
        chunker_version: TOOL_RESULT_CHUNKER_VERSION,
        source_bytes: window.total_bytes as u64,
        scanned_bytes: cursor as u64,
        scan_complete: cursor == window.total_bytes,
        chunks,
    })
}

fn chunk_id(
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    start: usize,
    end: usize,
) -> String {
    let mut hash = Sha256::new();
    hash.update(b"astra.tool-result.chunk.v1\0");
    hash.update(descriptor.version.to_be_bytes());
    hash.update([descriptor.document_kind as u8]);
    hash.update(descriptor.run_id.as_bytes());
    hash.update([0]);
    hash.update(descriptor.call_id.as_bytes());
    hash.update([0]);
    hash.update(descriptor.content_sha256.as_bytes());
    hash.update(TOOL_RESULT_CHUNKER_VERSION.to_be_bytes());
    hash.update((start as u64).to_be_bytes());
    hash.update((end as u64).to_be_bytes());
    format!("chunk-{:x}", hash.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(source: &str) -> astra_services::session_journal::ToolResultArtifactDescriptor {
        astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind: Default::default(),
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: "call-1".into(),
            run_id: "run-1".into(),
            byte_len: source.len() as u64,
            content_sha256: format!("{:x}", Sha256::digest(source.as_bytes())),
        }
    }

    fn window(source: &str, end: usize) -> crate::tool_result_storage::PersistedToolResultWindow {
        crate::tool_result_storage::PersistedToolResultWindow {
            content: source[..end].to_string(),
            offset: 0,
            next_offset: end,
            total_bytes: source.len(),
        }
    }

    #[test]
    fn chunks_are_stable_exact_source_ranges() {
        let source = "alpha\nbeta\ngamma\n";
        let descriptor = descriptor(source);
        let window = window(source, source.len());
        let first = project_tool_result_chunks(&descriptor, &window, 11, 10).unwrap();
        let second = project_tool_result_chunks(&descriptor, &window, 11, 10).unwrap();
        assert_eq!(first, second);
        assert!(first.scan_complete);
        let rebuilt = first
            .chunks
            .iter()
            .map(|chunk| &source[chunk.start_byte as usize..chunk.end_byte as usize])
            .collect::<String>();
        assert_eq!(rebuilt, source);
        assert!(first.chunks.iter().all(|chunk| chunk.line_complete));
    }

    #[test]
    fn oversized_unicode_line_is_bounded_and_marked_incomplete() {
        let source = format!("{}\ntail\n", "证据😀".repeat(100));
        let descriptor = descriptor(&source);
        let window = window(&source, source.len());
        let projection =
            project_tool_result_chunks(&descriptor, &window, 31, 32).expect("valid source");
        assert!(projection.chunks.iter().any(|chunk| !chunk.line_complete));
        for chunk in &projection.chunks {
            assert!(source.is_char_boundary(chunk.start_byte as usize));
            assert!(source.is_char_boundary(chunk.end_byte as usize));
        }
    }

    #[test]
    fn bounded_projection_reports_unscanned_source_and_rejects_wrong_bytes() {
        let source = "line\n".repeat(100);
        let descriptor = descriptor(&source);
        let window = window(&source, 40);
        let projection = project_tool_result_chunks(&descriptor, &window, 16, 2).unwrap();
        assert!(!projection.scan_complete);
        assert!(projection.scanned_bytes < projection.source_bytes);
        assert_eq!(projection.chunks.len(), 2);
        let malformed = crate::tool_result_storage::PersistedToolResultWindow {
            content: "different".into(),
            offset: 0,
            next_offset: 8,
            total_bytes: source.len(),
        };
        assert!(project_tool_result_chunks(&descriptor, &malformed, 16, 2).is_err());
        assert!(project_tool_result_chunks(&descriptor, &window, 16, 33).is_err());
    }

    #[test]
    fn chunk_identity_is_bound_to_artifact_owner_not_just_equal_content() {
        let source = "same bytes\n";
        let first = descriptor(source);
        let mut second = descriptor(source);
        second.run_id = "run-2".into();
        let window = window(source, source.len());
        let first = project_tool_result_chunks(&first, &window, 100, 1).unwrap();
        let second = project_tool_result_chunks(&second, &window, 100, 1).unwrap();
        assert_ne!(first.chunks[0].id, second.chunks[0].id);
    }

    #[test]
    fn empty_complete_source_is_valid_but_empty_incomplete_window_is_not() {
        let empty_descriptor = descriptor("");
        let empty = window("", 0);
        let projection = project_tool_result_chunks(&empty_descriptor, &empty, 16, 2).unwrap();
        assert!(projection.scan_complete);
        assert!(projection.chunks.is_empty());

        let descriptor = descriptor("😀");
        let empty_prefix = crate::tool_result_storage::PersistedToolResultWindow {
            content: String::new(),
            offset: 0,
            next_offset: 0,
            total_bytes: 4,
        };
        assert!(project_tool_result_chunks(&descriptor, &empty_prefix, 16, 2).is_err());
    }

    #[test]
    fn runtime_guidance_is_not_eligible_for_result_selection() {
        let source = "guidance\n";
        let mut descriptor = descriptor(source);
        descriptor.document_kind =
            astra_services::session_journal::ToolResultDocumentKind::RuntimeGuidance;
        assert!(
            project_tool_result_chunks(&descriptor, &window(source, source.len()), 16, 2).is_err()
        );
    }

    #[test]
    fn crlf_records_remain_exact_complete_source_ranges() {
        let source = "first\r\nsecond\r\n";
        let descriptor = descriptor(source);
        let projection =
            project_tool_result_chunks(&descriptor, &window(source, source.len()), 9, 4).unwrap();
        let slices = projection
            .chunks
            .iter()
            .map(|chunk| &source[chunk.start_byte as usize..chunk.end_byte as usize])
            .collect::<Vec<_>>();
        assert_eq!(slices, ["first\r\n", "second\r\n"]);
        assert!(projection.chunks.iter().all(|chunk| chunk.line_complete));
    }
}
