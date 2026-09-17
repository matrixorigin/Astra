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
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
///
/// The preview is the model's first bounded view of an artifact.  Keeping it
/// at roughly one token-cache block avoids forcing a second/third introspect
/// round for ordinary source/config windows, while the complete result remains
/// durable and pageable through the artifact handle.
const PREVIEW_CHARS: usize = 4_000;

/// XML-style tag that wraps the persisted-output reference.
const PERSISTED_TAG_OPEN: &str = "<persisted-output>";
const PERSISTED_TAG_CLOSE: &str = "</persisted-output>";

/// Subdirectory under the session folder for tool result files.
const TOOL_RESULTS_SUBDIR: &str = "tool-results";

/// Immutable, journal-addressed results live below a separate namespace from
/// the call-id projection used by the active model-facing artifact handle.
/// A provider call id is unique only within a run, so the physical authority
/// must include both identities.
const RUN_SCOPED_RESULTS_SUBDIR: &str = "runs";

static NEXT_IMMUTABLE_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Logical URI prefix for a persisted tool result scoped to the current
/// session.
pub const SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX: &str = "artifact://session/tool-result/";

/// Internal canonical-history field carrying the immutable run owner of a
/// tool-result message.  It is retained in runtime history so later
/// compaction can address the same run-bound artifact; provider projection
/// removes it before serialization.
pub const TOOL_RESULT_RUN_ID_FIELD: &str = "_astra_tool_result_run_id";

/// Internal canonical-history field carrying the immutable descriptor for a
/// result that has already been moved out of the prompt.  Compression layers
/// use this typed marker instead of classifying the rendered body.
pub const TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD: &str = "_astra_tool_result_artifact";

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

/// Persisted model projection plus the journal-only authority for its full
/// bytes. The replacement is intentionally unchanged from the ordinary
/// provider-facing representation; only the typed journal record carries the
/// descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistedToolResult {
    pub replacement: String,
    pub descriptor: astra_services::session_journal::ToolResultArtifactDescriptor,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolResultRunIdentityError {
    #[error("tool-result run identity must be non-empty")]
    Empty,
    #[error("tool-result run identity is not a valid durable identifier")]
    Invalid,
    #[error("tool-result message already contains invalid run identity metadata")]
    ExistingInvalid,
    #[error(
        "tool-result run identity cannot be replaced once assigned (existing '{existing}', requested '{requested}')"
    )]
    Conflict { existing: String, requested: String },
    #[error("tool-result message must be an object")]
    NotAnObject,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ToolResultArtifactMetadataError {
    #[error("tool-result message must be an object")]
    NotAnObject,
    #[error("tool-result artifact descriptor is invalid")]
    InvalidDescriptor,
    #[error("tool-result artifact descriptor cannot be replaced once assigned")]
    Conflict,
    #[error("tool-result artifact descriptor identity does not match its message")]
    IdentityMismatch,
}

/// Return the run identity attached to a canonical tool-result message.
///
/// A current loop run is not a sufficient substitute: history may contain
/// results from multiple runs, and assigning the current id would make a
/// replayed call appear to own another run's evidence.  Callers therefore
/// must use this per-message identity and fail closed when it is absent.
#[must_use]
pub fn tool_result_run_id(message: &Value) -> Option<&str> {
    message
        .get(TOOL_RESULT_RUN_ID_FIELD)
        .and_then(Value::as_str)
        .filter(|run_id| is_valid_tool_result_run_id(run_id))
}

/// The run identity is durable protocol metadata, not arbitrary model text.
/// Keep its validation aligned with the service identity boundary so invalid
/// values cannot become either artifact owners or sanitizer exemptions.
#[must_use]
pub fn is_valid_tool_result_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 64
        && run_id.trim() == run_id
        && !run_id.chars().any(char::is_control)
}

/// Attach the immutable run owner to a canonical tool-result message.
/// Empty identities are deliberately ignored instead of creating a handle
/// that cannot be verified during resume or introspection.
pub fn mark_tool_result_run_id(
    message: &mut Value,
    run_id: Option<&str>,
) -> Result<(), ToolResultRunIdentityError> {
    let Some(run_id) = run_id else {
        return Ok(());
    };
    if run_id.trim().is_empty() {
        return Err(ToolResultRunIdentityError::Empty);
    }
    if !is_valid_tool_result_run_id(run_id) {
        return Err(ToolResultRunIdentityError::Invalid);
    }
    let Some(object) = message.as_object_mut() else {
        return Err(ToolResultRunIdentityError::NotAnObject);
    };
    if let Some(existing) = object.get(TOOL_RESULT_RUN_ID_FIELD) {
        let Some(existing) = existing.as_str() else {
            return Err(ToolResultRunIdentityError::ExistingInvalid);
        };
        if !is_valid_tool_result_run_id(existing) {
            return Err(ToolResultRunIdentityError::ExistingInvalid);
        }
        if existing == run_id {
            return Ok(());
        }
        return Err(ToolResultRunIdentityError::Conflict {
            existing: existing.to_string(),
            requested: run_id.to_string(),
        });
    }
    object.insert(
        TOOL_RESULT_RUN_ID_FIELD.to_string(),
        Value::String(run_id.to_string()),
    );
    Ok(())
}

/// Read the typed descriptor carried by a canonical tool-result message.
#[must_use]
pub fn tool_result_artifact_descriptor(
    message: &Value,
) -> Option<astra_services::session_journal::ToolResultArtifactDescriptor> {
    let descriptor = message
        .get(TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD)
        .and_then(parse_tool_result_artifact_descriptor)?;
    let call_id = message.get("tool_call_id").and_then(Value::as_str);
    let run_id = message
        .get(TOOL_RESULT_RUN_ID_FIELD)
        .and_then(Value::as_str);
    artifact_descriptor_matches_identity(&descriptor, call_id, run_id).then_some(descriptor)
}

/// Check that an artifact descriptor is bound to the message identity that
/// carries it. Descriptor shape alone is not authority: a valid handle from
/// another call or run must never make a tool result look recoverable.
#[must_use]
pub fn artifact_descriptor_matches_identity(
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    call_id: Option<&str>,
    run_id: Option<&str>,
) -> bool {
    descriptor.document_kind.is_result()
        && call_id == Some(descriptor.call_id.as_str())
        && run_id == Some(descriptor.run_id.as_str())
}

/// Parse and validate one descriptor value carried in canonical message
/// metadata.  Keeping this parser separate lets typed compression layers
/// inspect `Message::extra` without reserializing the complete message.
#[must_use]
pub fn parse_tool_result_artifact_descriptor(
    value: &Value,
) -> Option<astra_services::session_journal::ToolResultArtifactDescriptor> {
    let descriptor: astra_services::session_journal::ToolResultArtifactDescriptor =
        serde_json::from_value(value.clone()).ok()?;
    (descriptor.document_kind.is_result() && valid_artifact_descriptor(&descriptor))
        .then_some(descriptor)
}

/// Attach a validated immutable artifact descriptor to a canonical message.
/// Replaying the same message is idempotent; a different descriptor cannot
/// replace the existing owner/evidence binding.
pub fn mark_tool_result_artifact_descriptor(
    message: &mut Value,
    descriptor: Option<&astra_services::session_journal::ToolResultArtifactDescriptor>,
) -> Result<(), ToolResultArtifactMetadataError> {
    let Some(descriptor) = descriptor else {
        return Ok(());
    };
    if !valid_artifact_descriptor(descriptor) {
        return Err(ToolResultArtifactMetadataError::InvalidDescriptor);
    }
    let Some(object) = message.as_object_mut() else {
        return Err(ToolResultArtifactMetadataError::NotAnObject);
    };
    let call_id = object.get("tool_call_id").and_then(Value::as_str);
    let run_id = object.get(TOOL_RESULT_RUN_ID_FIELD).and_then(Value::as_str);
    if !artifact_descriptor_matches_identity(descriptor, call_id, run_id) {
        return Err(ToolResultArtifactMetadataError::IdentityMismatch);
    }
    if let Some(existing) = object.get(TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD) {
        let existing = serde_json::from_value::<
            astra_services::session_journal::ToolResultArtifactDescriptor,
        >(existing.clone())
        .ok();
        if existing.as_ref() == Some(descriptor) {
            return Ok(());
        }
        return Err(ToolResultArtifactMetadataError::Conflict);
    }
    object.insert(
        TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD.to_string(),
        serde_json::to_value(descriptor)
            .map_err(|_| ToolResultArtifactMetadataError::InvalidDescriptor)?,
    );
    Ok(())
}

/// Failure to establish the durable identity of a tool-result artifact.
///
/// An identity conflict is a protocol violation: replaying the same run/call
/// may observe the same bytes, but it may never replace them with different
/// bytes. Ordinary filesystem failures remain separately observable so the
/// caller does not have to infer either condition from display text.
#[derive(Debug, thiserror::Error)]
pub enum ToolResultPersistenceError {
    #[error(
        "tool-document {document_kind:?} identity conflict for run '{run_id}' and call '{call_id}'"
    )]
    IdentityConflict {
        run_id: String,
        call_id: String,
        document_kind: astra_services::session_journal::ToolResultDocumentKind,
    },
    #[error("tool-result persistence failed: {0}")]
    Io(#[source] io::Error),
}

struct PersistedWrite {
    replacement: String,
    byte_len: u64,
    content_sha256: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunBoundArtifactHandle {
    #[serde(
        default,
        skip_serializing_if = "astra_services::session_journal::ToolResultDocumentKind::is_result"
    )]
    kind: astra_services::session_journal::ToolResultDocumentKind,
    v: u32,
    run: String,
    call: String,
    len: u64,
    sha256: String,
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

    persist_tool_result_with_replacement(session_dir, tool_call_id, tool_name, content)
}

/// Persist a tool result and return the standard bounded model-facing
/// replacement, regardless of the result's size.
///
/// Callers use this when an earlier presentation boundary has already made
/// the inline result lossy. In that situation the persistence threshold is
/// irrelevant: the omitted evidence must remain recoverable even when the
/// original result happens to be smaller than [`PERSIST_THRESHOLD_CHARS`].
pub fn persist_tool_result_with_replacement(
    session_dir: &Path,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Option<String> {
    persist_tool_result(session_dir, None, tool_call_id, tool_name, content)
        .ok()
        .map(|persisted| persisted.replacement)
}

/// Persist a result and return its typed, run-bound journal authority.
///
/// This metadata is internal C2 evidence. It is not embedded into the
/// model-facing display envelope, so provider prompt/schema/cache structure
/// remains unchanged.
pub fn persist_tool_result_with_descriptor(
    session_dir: &Path,
    run_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Result<PersistedToolResult, ToolResultPersistenceError> {
    persist_tool_document_with_descriptor(
        session_dir,
        run_id,
        tool_call_id,
        tool_name,
        content,
        astra_services::session_journal::ToolResultDocumentKind::Result,
    )
}

/// Persist a separately identified document of the same invocation. It uses
/// the existing immutable artifact store and recovery protocol.
pub fn persist_tool_document_with_descriptor(
    session_dir: &Path,
    run_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
    document_kind: astra_services::session_journal::ToolResultDocumentKind,
) -> Result<PersistedToolResult, ToolResultPersistenceError> {
    if !is_valid_tool_result_run_id(run_id) || tool_call_id.trim().is_empty() {
        return Err(ToolResultPersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run_id and tool_call_id must be valid non-empty identifiers",
        )));
    }
    let persisted = persist_tool_document(
        session_dir,
        Some(run_id),
        tool_call_id,
        tool_name,
        content,
        document_kind,
    )?;
    Ok(PersistedToolResult {
        replacement: persisted.replacement,
        descriptor: astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind,
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: tool_call_id.to_string(),
            run_id: run_id.to_string(),
            byte_len: persisted.byte_len,
            content_sha256: persisted.content_sha256,
        },
    })
}

/// Persist a result for history compaction and return a deliberately small
/// recovery projection.
///
/// The ordinary model projection includes a preview and several lines of
/// navigation guidance because it is the first presentation of a large tool
/// result.  A microcompacted result has already been shown to the model, so
/// replaying that envelope would spend more prompt/cache bytes than the
/// bounded result itself.  The compact projection keeps only the typed,
/// run-bound handle and the canonical introspection operation.
pub fn persist_tool_result_for_compaction(
    session_dir: &Path,
    run_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Result<PersistedToolResult, ToolResultPersistenceError> {
    let prepared = prepare_tool_result_for_compaction(run_id, tool_call_id, tool_name, content)?;
    let persisted =
        persist_tool_result(session_dir, Some(run_id), tool_call_id, tool_name, content)?;
    Ok(PersistedToolResult {
        replacement: prepared.replacement,
        descriptor: astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind: Default::default(),
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: tool_call_id.to_string(),
            run_id: run_id.to_string(),
            byte_len: persisted.byte_len,
            content_sha256: persisted.content_sha256,
        },
    })
}

/// Prepare the exact compact recovery projection without touching disk.
/// Callers can reject a candidate that would not reduce bytes/tokens before
/// establishing an immutable artifact, avoiding orphan files for tiny tool
/// results.
pub fn prepare_tool_result_for_compaction(
    run_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Result<PersistedToolResult, ToolResultPersistenceError> {
    if !is_valid_tool_result_run_id(run_id) || tool_call_id.trim().is_empty() {
        return Err(ToolResultPersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run_id and tool_call_id must be valid non-empty identifiers",
        )));
    }
    let persisted = persistable_content(content);
    let persisted_bytes = persisted.text.as_bytes();
    let byte_len = u64::try_from(persisted_bytes.len()).map_err(|_| {
        ToolResultPersistenceError::Io(io::Error::other(
            "persisted tool result exceeds supported byte length",
        ))
    })?;
    let descriptor = astra_services::session_journal::ToolResultArtifactDescriptor {
        document_kind: Default::default(),
        version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
        call_id: tool_call_id.to_string(),
        run_id: run_id.to_string(),
        byte_len,
        content_sha256: format!("{:x}", Sha256::digest(persisted_bytes)),
    };
    let artifact_uri = session_tool_result_artifact_uri_for_descriptor(&descriptor);
    Ok(PersistedToolResult {
        replacement: build_compact_replacement(tool_call_id, tool_name, &artifact_uri),
        descriptor,
    })
}

/// Threshold-aware variant of [`persist_tool_result_with_descriptor`].
pub fn maybe_persist_tool_result_with_descriptor(
    session_dir: &Path,
    run_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Result<Option<PersistedToolResult>, ToolResultPersistenceError> {
    if content.chars().count() <= PERSIST_THRESHOLD_CHARS {
        return Ok(None);
    }
    persist_tool_result_with_descriptor(session_dir, run_id, tool_call_id, tool_name, content)
        .map(Some)
}

fn persist_tool_result(
    session_dir: &Path,
    run_id: Option<&str>,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Result<PersistedWrite, ToolResultPersistenceError> {
    persist_tool_document(
        session_dir,
        run_id,
        tool_call_id,
        tool_name,
        content,
        astra_services::session_journal::ToolResultDocumentKind::Result,
    )
}

fn persist_tool_document(
    session_dir: &Path,
    run_id: Option<&str>,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
    document_kind: astra_services::session_journal::ToolResultDocumentKind,
) -> Result<PersistedWrite, ToolResultPersistenceError> {
    if let Some(run_id) = run_id
        && !is_valid_tool_result_run_id(run_id)
    {
        return Err(ToolResultPersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run_id is not a valid durable identifier",
        )));
    }
    let dir = session_dir.join(TOOL_RESULTS_SUBDIR);
    std::fs::create_dir_all(&dir).map_err(ToolResultPersistenceError::Io)?;

    // Sanitize tool_call_id for filesystem safety (with hash suffix to avoid collisions)
    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = dir.join(format!("{safe_id}.txt"));

    let persisted = persistable_content(content);
    if let Some(run_id) = run_id {
        let authoritative_path =
            run_scoped_document_path(session_dir, run_id, tool_call_id, document_kind);
        match write_immutable_result(&authoritative_path, persisted.text.as_bytes()) {
            Ok(()) => {}
            Err(ImmutableWriteError::IdentityConflict) => {
                return Err(ToolResultPersistenceError::IdentityConflict {
                    run_id: run_id.to_string(),
                    call_id: tool_call_id.to_string(),
                    document_kind,
                });
            }
            Err(ImmutableWriteError::Io(error)) => {
                return Err(ToolResultPersistenceError::Io(error));
            }
        }
    }

    // Keep the call-id projection for local readers that still inspect a
    // session directory.  It is neither journal nor model-handle authority:
    // a later run may reuse the provider call id without changing either
    // run-scoped file.  Once a run-bound file exists, failure of this mutable
    // convenience projection must not invalidate the immutable artifact.
    if document_kind.is_result()
        && let Err(error) = std::fs::write(&file_path, persisted.text.as_str())
    {
        if run_id.is_none() {
            return Err(ToolResultPersistenceError::Io(error));
        }
        tracing::debug!(
            path = %file_path.display(),
            error = %error,
            "tool-result mutable call-id projection unavailable; immutable run artifact remains authoritative"
        );
    }

    let persisted_bytes = persisted.text.as_bytes();
    let byte_len = u64::try_from(persisted_bytes.len()).map_err(|_| {
        ToolResultPersistenceError::Io(io::Error::other(
            "persisted tool result exceeds supported byte length",
        ))
    })?;
    let content_sha256 = format!("{:x}", Sha256::digest(persisted_bytes));
    let artifact_uri = run_id.map_or_else(
        || session_tool_result_artifact_uri(tool_call_id),
        |run_id| {
            session_tool_result_artifact_uri_for_descriptor(
                &astra_services::session_journal::ToolResultArtifactDescriptor {
                    document_kind,
                    version:
                        astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
                    call_id: tool_call_id.to_string(),
                    run_id: run_id.to_string(),
                    byte_len,
                    content_sha256: content_sha256.clone(),
                },
            )
        },
    );
    Ok(PersistedWrite {
        replacement: build_replacement(tool_call_id, tool_name, content, &persisted, &artifact_uri),
        byte_len,
        content_sha256,
    })
}

fn run_scoped_result_path(session_dir: &Path, run_id: &str, tool_call_id: &str) -> PathBuf {
    run_scoped_document_path(
        session_dir,
        run_id,
        tool_call_id,
        astra_services::session_journal::ToolResultDocumentKind::Result,
    )
}

fn run_scoped_document_path(
    session_dir: &Path,
    run_id: &str,
    tool_call_id: &str,
    kind: astra_services::session_journal::ToolResultDocumentKind,
) -> PathBuf {
    let run_dir = session_dir
        .join(TOOL_RESULTS_SUBDIR)
        .join(RUN_SCOPED_RESULTS_SUBDIR)
        .join(safe_filename_stem(run_id));
    let dir = match kind {
        astra_services::session_journal::ToolResultDocumentKind::Result => run_dir,
        astra_services::session_journal::ToolResultDocumentKind::RuntimeGuidance => {
            run_dir.join("runtime-guidance")
        }
    };
    dir.join(format!("{}.txt", safe_filename_stem(tool_call_id)))
}

/// Establish one immutable physical identity. A same-run/same-call replay may
/// observe the exact same bytes; different bytes are an identity conflict and
/// fail closed rather than overwriting already-journaled evidence.
#[derive(Debug)]
enum ImmutableWriteError {
    IdentityConflict,
    Io(io::Error),
}

fn write_immutable_result(path: &Path, content: &[u8]) -> Result<(), ImmutableWriteError> {
    write_immutable_result_with(path, content, |file, bytes| {
        std::io::Write::write_all(file, bytes)
    })
}

fn write_immutable_result_with(
    path: &Path,
    content: &[u8],
    write_content: impl FnOnce(&mut std::fs::File, &[u8]) -> io::Result<()>,
) -> Result<(), ImmutableWriteError> {
    let parent = path.parent().ok_or_else(|| {
        ImmutableWriteError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run-scoped result path has no parent",
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(ImmutableWriteError::Io)?;
    let file_name = path.file_name().ok_or_else(|| {
        ImmutableWriteError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "run-scoped result path has no filename",
        ))
    })?;
    let temp_id = NEXT_IMMUTABLE_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        temp_id
    ));
    let mut temp_file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .map_err(ImmutableWriteError::Io)?;
    if let Err(error) = write_content(&mut temp_file, content).and_then(|()| temp_file.sync_all()) {
        drop(temp_file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(ImmutableWriteError::Io(error));
    }
    drop(temp_file);

    let published = match std::fs::hard_link(&temp_path, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => match std::fs::read(path) {
            Ok(existing) if existing == content => Ok(()),
            Ok(_) => Err(ImmutableWriteError::IdentityConflict),
            Err(error) => Err(ImmutableWriteError::Io(error)),
        },
        Err(error) => Err(ImmutableWriteError::Io(error)),
    };
    if let Err(error) = published {
        // A failed publish must not leave unbounded orphaned temporary files
        // under a long-lived session.  Best-effort cleanup is deliberately
        // separate from the authoritative publish error.
        let _ = std::fs::remove_file(&temp_path);
        let _ = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
        return Err(error);
    }
    let cleanup = std::fs::remove_file(&temp_path);
    let sync = std::fs::File::open(parent).and_then(|directory| directory.sync_all());
    cleanup.map_err(ImmutableWriteError::Io)?;
    sync.map_err(ImmutableWriteError::Io)
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

/// Read and verify the complete bytes named by a typed journal descriptor.
///
/// `session_dir` must be the artifact root derived from the journal currently
/// being loaded. The descriptor never supplies a path, so it cannot escape or
/// redirect the owner/session boundary. Length is checked before allocation
/// and SHA-256 is checked before UTF-8 decoding.
pub fn read_verified_persisted_result(
    session_dir: &Path,
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    max_bytes: u64,
) -> Result<String, String> {
    if !descriptor.document_kind.is_result() {
        return Err("runtime guidance cannot supply tool-result authority".to_string());
    }
    if descriptor.version
        != astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION
    {
        return Err("unsupported persisted tool-result descriptor version".to_string());
    }
    if descriptor.call_id.trim().is_empty() || !is_valid_tool_result_run_id(&descriptor.run_id) {
        return Err("persisted tool-result descriptor has an empty identity".to_string());
    }
    if descriptor.byte_len > max_bytes {
        return Err(format!(
            "persisted tool result exceeds the {max_bytes}-byte verification limit"
        ));
    }
    if descriptor.content_sha256.len() != 64
        || !descriptor
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err("persisted tool-result descriptor has an invalid SHA-256 digest".to_string());
    }

    let file_path = run_scoped_result_path(session_dir, &descriptor.run_id, &descriptor.call_id);
    let path_metadata = std::fs::symlink_metadata(&file_path)
        .map_err(|error| format!("failed to inspect persisted result: {error}"))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        return Err("persisted tool result is not a regular owner-scoped file".to_string());
    }
    if path_metadata.len() != descriptor.byte_len {
        return Err("persisted tool-result byte length does not match its descriptor".to_string());
    }

    let mut file = std::fs::File::open(&file_path)
        .map_err(|error| format!("failed to open persisted result: {error}"))?;
    let mut bytes = Vec::with_capacity(
        usize::try_from(descriptor.byte_len)
            .map_err(|_| "persisted result is too large for this runtime".to_string())?,
    );
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read persisted result: {error}"))?;
    if u64::try_from(bytes.len()).ok() != Some(descriptor.byte_len) {
        return Err("persisted tool-result bytes changed while being read".to_string());
    }
    let actual_digest = format!("{:x}", Sha256::digest(&bytes));
    if actual_digest != descriptor.content_sha256 {
        return Err("persisted tool-result digest does not match its descriptor".to_string());
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("persisted tool result is not valid UTF-8: {error}"))
}

fn valid_artifact_descriptor(
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
) -> bool {
    descriptor.version == astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION
        && !descriptor.call_id.trim().is_empty()
        && is_valid_tool_result_run_id(&descriptor.run_id)
        && descriptor.content_sha256.len() == 64
        && descriptor
            .content_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Parse a session-local logical tool-result handle without exposing a
/// filesystem path.
///
/// Handles use URL-safe Base64 rather than embedding provider-supplied
/// identities directly. The opaque payload carries the complete immutable
/// descriptor; a call id alone is not a physical artifact identity.
#[must_use]
pub fn parse_session_tool_result_artifact_uri(
    value: &str,
) -> Option<astra_services::session_journal::ToolResultArtifactDescriptor> {
    let encoded = value.strip_prefix(SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX)?;
    if encoded.is_empty()
        || !encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD.decode(encoded).ok()?;
    let handle: RunBoundArtifactHandle = serde_json::from_slice(&decoded).ok()?;
    let descriptor = astra_services::session_journal::ToolResultArtifactDescriptor {
        document_kind: handle.kind,
        version: handle.v,
        call_id: handle.call,
        run_id: handle.run,
        byte_len: handle.len,
        content_sha256: handle.sha256,
    };
    valid_artifact_descriptor(&descriptor).then_some(descriptor)
}

/// Parse the descriptor from a runtime-generated model projection.  This is a
/// strict codec for the two persisted-output grammars emitted by this module;
/// it is not a classifier over arbitrary model text.  Typed message metadata
/// remains the preferred path, while this parser protects older in-memory
/// projections that have not yet been round-tripped through `Message`.
#[must_use]
pub fn parse_tool_result_artifact_projection(
    content: &str,
) -> Option<astra_services::session_journal::ToolResultArtifactDescriptor> {
    let body = content
        .trim()
        .strip_prefix(PERSISTED_TAG_OPEN)?
        .strip_suffix(PERSISTED_TAG_CLOSE)?
        .trim();
    for line in body.lines().map(str::trim) {
        let uri = if let Some(uri) = line.strip_prefix("Artifact handle: ") {
            uri.trim_end_matches('.')
        } else if let Some(rest) = line.strip_prefix("Recover with introspect(artifact=\"") {
            rest.split_once('\"')?.0
        } else {
            continue;
        };
        if let Some(descriptor) = parse_session_tool_result_artifact_uri(uri)
            && descriptor.document_kind.is_result()
        {
            return Some(descriptor);
        }
    }
    None
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
    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = session_dir
        .join(TOOL_RESULTS_SUBDIR)
        .join(format!("{safe_id}.txt"));
    read_persisted_result_window_at_path(&file_path, offset, max_bytes)
}

fn read_persisted_result_window_at_path(
    file_path: &Path,
    offset: usize,
    max_bytes: usize,
) -> Result<Option<PersistedToolResultWindow>, String> {
    if max_bytes == 0 || max_bytes > MAX_TOOL_RESULT_WINDOW_BYTES {
        return Err(format!(
            "max_bytes must be between 1 and {MAX_TOOL_RESULT_WINDOW_BYTES}"
        ));
    }
    let total_bytes = match std::fs::metadata(file_path) {
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

    let mut file = std::fs::File::open(file_path)
        .map_err(|error| format!("failed to open persisted result: {error}"))?;

    // Models occasionally retain an offset copied from a bounded preview
    // instead of the previous window's `next_offset`.  Treat that as a
    // recoverable cursor defect, not as a tool failure: seek back to the
    // beginning of the scalar containing the requested byte.  Flooring is
    // deliberate — it may repeat a few bytes, but it can never silently lose
    // evidence.  The returned `next_offset` is a canonical cursor for the
    // next request.
    let offset = floor_persisted_utf8_boundary(&mut file, offset)?;
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|error| format!("failed to seek persisted result: {error}"))?;

    // Read a few extra bytes so a UTF-8 scalar straddling `max_bytes` can
    // still make forward progress without returning malformed text.
    let available = total_bytes.saturating_sub(offset);
    let read_len = available.min(max_bytes.saturating_add(3));
    let mut bytes = vec![0_u8; read_len];
    file.read_exact(&mut bytes)
        .map_err(|error| format!("failed to read persisted result: {error}"))?;

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

fn read_verified_persisted_result_window(
    session_dir: &Path,
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
    offset: usize,
    max_bytes: usize,
) -> Result<Option<PersistedToolResultWindow>, String> {
    if !valid_artifact_descriptor(descriptor) {
        return Err("tool-result artifact handle has an invalid immutable descriptor".to_string());
    }
    let file_path = run_scoped_document_path(
        session_dir,
        &descriptor.run_id,
        &descriptor.call_id,
        descriptor.document_kind,
    );
    let metadata = match std::fs::symlink_metadata(&file_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to inspect persisted result: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("persisted tool result is not a regular owner-scoped file".to_string());
    }
    if metadata.len() != descriptor.byte_len {
        return Err("persisted tool-result byte length does not match its handle".to_string());
    }

    let mut file = std::fs::File::open(&file_path)
        .map_err(|error| format!("failed to open persisted result: {error}"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to verify persisted result: {error}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if format!("{:x}", digest.finalize()) != descriptor.content_sha256 {
        return Err("persisted tool-result digest does not match its handle".to_string());
    }
    read_persisted_result_window_at_path(&file_path, offset, max_bytes)
}

/// Return the greatest UTF-8 boundary at or before `offset` without loading
/// the whole artifact.  A UTF-8 scalar is at most four bytes, so at most three
/// one-byte probes are required.  The caller has already checked that
/// `offset <= total_bytes`.
fn floor_persisted_utf8_boundary(file: &mut std::fs::File, offset: usize) -> Result<usize, String> {
    if offset == 0 {
        return Ok(0);
    }

    let mut candidate = offset;
    for _ in 0..3 {
        file.seek(SeekFrom::Start(candidate as u64))
            .map_err(|error| format!("failed to inspect UTF-8 boundary: {error}"))?;
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte)
            .map_err(|error| format!("failed to inspect UTF-8 boundary: {error}"))?;
        if byte[0] & 0b1100_0000 != 0b1000_0000 {
            return Ok(candidate);
        }
        candidate = candidate.saturating_sub(1);
        if candidate == 0 {
            return Ok(0);
        }
    }

    // A valid UTF-8 scalar cannot have more than three continuation bytes.
    // Returning the conservative floor lets the normal UTF-8 validation below
    // produce the authoritative corruption error if the artifact is invalid.
    Ok(candidate)
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
    let descriptor = match parse_session_tool_result_artifact_uri(artifact) {
        Some(descriptor) => descriptor,
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

    Some(read_verified_persisted_result_window(session_dir, &descriptor, offset, max_bytes).and_then(
        |window| {
            let Some(window) = window else {
                return Err("tool-result artifact was not found in the active session".to_string());
            };
            let handle = session_tool_result_artifact_uri_for_descriptor(&descriptor);
            let continuation = if window.is_complete() {
                "Complete.".to_string()
            } else {
                format!(
                    "Continue with introspect(artifact=\"{handle}\", offset={}, max_bytes={max_bytes}).",
                    window.next_offset
                )
            };
            let document_label = if descriptor.document_kind.is_result() {
                ""
            } else {
                "Document: runtime guidance\n"
            };
            Ok(format!(
                "<tool-result-window>\n\
                 {document_label}\
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

/// Return the canonical model-facing handle for one immutable artifact.
#[must_use]
pub fn session_tool_result_artifact_uri_for_descriptor(
    descriptor: &astra_services::session_journal::ToolResultArtifactDescriptor,
) -> String {
    let payload = RunBoundArtifactHandle {
        kind: descriptor.document_kind,
        v: descriptor.version,
        run: descriptor.run_id.clone(),
        call: descriptor.call_id.clone(),
        len: descriptor.byte_len,
        sha256: descriptor.content_sha256.clone(),
    };
    let encoded = serde_json::to_vec(&payload)
        .expect("tool-result artifact handle serialization must be infallible");
    format!(
        "{SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX}{}",
        URL_SAFE_NO_PAD.encode(encoded)
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
    artifact_uri: &str,
) -> String {
    let total_chars = original_content.chars().count();
    let stored_chars = persisted.text.chars().count();
    let preview = model_preview(&persisted.text, PREVIEW_CHARS);

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
         Reading retained output requires an authorized artifact reader; the handle does not grant access. Do not search, copy, or read physical local session paths.\n\
         {format_note}\
         \n\
         Preview (~{prev_len} chars; structured head/tail when available):\n\
         {preview}\n\
         ...[truncated — full output is available through the session tool-result artifact, not workspace filesystem tools]\n\
         {PERSISTED_TAG_CLOSE}",
        prev_len = preview.len(),
    )
}

fn build_compact_replacement(tool_call_id: &str, tool_name: &str, artifact_uri: &str) -> String {
    format!(
        "{PERSISTED_TAG_OPEN}\n\
         Tool `{tool_name}` result `{tool_call_id}` was compacted.\n\
         Artifact handle: {artifact_uri}\n\
         Reading retained output requires an authorized artifact reader; the handle does not grant access.\n\
         {PERSISTED_TAG_CLOSE}"
    )
}

/// Produce a useful first view of a persisted result without loading the
/// complete artifact into the next model request. Structured content results
/// put their most useful navigation links after a large page-content field;
/// taking the first bytes alone therefore turns a recoverable result into an
/// avoidable extra `introspect` round. Keep this projection structural and
/// bounded: it does not inspect user prose or infer a task-specific answer.
fn model_preview(persisted: &str, budget: usize) -> String {
    if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(persisted)
        && object.get("content").and_then(Value::as_str).is_some()
        && object.get("links").and_then(Value::as_array).is_some()
    {
        let mut projection = serde_json::Map::new();
        for key in [
            "url",
            "final_url",
            "status",
            "content_type",
            "metadata",
            "content_length",
            "truncated",
            "cached",
            "elapsed_ms",
        ] {
            if let Some(value) = object.get(key) {
                projection.insert(key.to_string(), value.clone());
            }
        }
        if let Some(links) = object.get("links").and_then(Value::as_array) {
            let link_budget = 6usize;
            let mut selected = Vec::with_capacity(link_budget.min(links.len()));
            selected.extend(links.iter().take(link_budget / 2).cloned());
            if links.len() > link_budget / 2 {
                selected.extend(links.iter().skip(links.len() - link_budget / 2).cloned());
            }
            projection.insert("links".to_string(), Value::Array(selected));
            if links.len() > link_budget {
                projection.insert(
                    "links_omitted".to_string(),
                    Value::from(links.len() - link_budget),
                );
            }
        }
        if let Some(content) = object.get("content").and_then(Value::as_str) {
            projection.insert(
                "content".to_string(),
                Value::String(head_tail_preview(content, budget / 3, budget / 3)),
            );
        }
        if let Ok(pretty) = serde_json::to_string_pretty(&Value::Object(projection)) {
            return truncate_preview(&pretty, budget);
        }
    }

    truncate_preview(persisted, budget)
}

fn truncate_preview(text: &str, budget: usize) -> String {
    let preview: String = text.chars().take(budget).collect();
    if preview.len() < text.len() {
        preview
    } else {
        text.to_string()
    }
}

fn head_tail_preview(text: &str, head_budget: usize, tail_budget: usize) -> String {
    if text.chars().count() <= head_budget.saturating_add(tail_budget) {
        return text.to_string();
    }
    let head: String = text.chars().take(head_budget).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(tail_budget)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}\n[… preview middle omitted …]\n{tail}")
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
        assert!(replacement.contains("requires an authorized artifact reader"));
        assert!(!replacement.contains("introspect(artifact="));
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
    fn typed_descriptor_verifies_exact_persisted_bytes_with_run_bound_handle() {
        let dir = tempfile::tempdir().unwrap();
        let content = "first line\nsecond line\n";
        let ordinary =
            persist_tool_result_with_replacement(dir.path(), "call-authority", "agent", content)
                .unwrap();
        let typed = persist_tool_result_with_descriptor(
            dir.path(),
            "run-authority",
            "call-authority",
            "agent",
            content,
        )
        .unwrap();

        assert_ne!(typed.replacement, ordinary);
        assert_eq!(typed.descriptor.version, 1);
        assert_eq!(typed.descriptor.call_id, "call-authority");
        assert_eq!(typed.descriptor.run_id, "run-authority");
        assert_eq!(typed.descriptor.byte_len, content.len() as u64);
        assert_eq!(typed.descriptor.content_sha256.len(), 64);
        assert!(!typed.replacement.contains(&typed.descriptor.content_sha256));
        let handle = session_tool_result_artifact_uri_for_descriptor(&typed.descriptor);
        assert!(typed.replacement.contains(&handle));
        assert_eq!(
            parse_session_tool_result_artifact_uri(&handle),
            Some(typed.descriptor.clone())
        );
        assert_eq!(
            read_verified_persisted_result(dir.path(), &typed.descriptor, 1024).unwrap(),
            content
        );
    }

    #[test]
    fn result_and_guidance_share_invocation_but_not_artifact_authority() {
        use astra_services::session_journal::ToolResultDocumentKind;
        let dir = tempfile::tempdir().unwrap();
        let other_owner = tempfile::tempdir().unwrap();
        let result = persist_tool_result_with_descriptor(
            dir.path(),
            "run-1",
            "call-1",
            "probe",
            "RESULT-OK",
        )
        .unwrap();
        let full_guidance = "指令🦀: preserve complete recovery evidence.\n".repeat(500);
        let guidance = persist_tool_document_with_descriptor(
            dir.path(),
            "run-1",
            "call-1",
            "probe",
            &full_guidance,
            ToolResultDocumentKind::RuntimeGuidance,
        )
        .unwrap();
        assert_eq!(
            read_verified_persisted_result(dir.path(), &result.descriptor, 1024).unwrap(),
            "RESULT-OK"
        );
        assert!(read_verified_persisted_result(dir.path(), &guidance.descriptor, 100_000).is_err());
        assert!(!artifact_descriptor_matches_identity(
            &guidance.descriptor,
            Some("call-1"),
            Some("run-1")
        ));
        assert!(parse_tool_result_artifact_projection(&guidance.replacement).is_none());
        let mut message = serde_json::json!({"role":"tool", "tool_call_id":"call-1", TOOL_RESULT_RUN_ID_FIELD:"run-1"});
        assert!(
            mark_tool_result_artifact_descriptor(&mut message, Some(&guidance.descriptor)).is_err()
        );
        assert!(
            parse_tool_result_artifact_descriptor(
                &serde_json::to_value(&guidance.descriptor).unwrap()
            )
            .is_none()
        );
        let handle = session_tool_result_artifact_uri_for_descriptor(&guidance.descriptor);
        assert_eq!(
            parse_session_tool_result_artifact_uri(&handle),
            Some(guidance.descriptor.clone())
        );
        let first = resolve_session_tool_result_artifact_request(
            dir.path(),
            &serde_json::json!({"artifact":handle,"offset":0,"max_bytes":257}),
        )
        .unwrap()
        .unwrap();
        assert!(first.contains("Document: runtime guidance"));
        let mut recovered = String::new();
        let mut offset = 0;
        loop {
            let window = read_verified_persisted_result_window(
                dir.path(),
                &guidance.descriptor,
                offset,
                257,
            )
            .unwrap()
            .unwrap();
            assert!(window.next_offset > offset);
            recovered.push_str(&window.content);
            if window.is_complete() {
                break;
            }
            offset = window.next_offset;
        }
        assert_eq!(recovered, full_guidance);
        assert!(
            read_verified_persisted_result_window(other_owner.path(), &guidance.descriptor, 0, 257)
                .unwrap()
                .is_none()
        );
        let mut wrong_run = guidance.descriptor.clone();
        wrong_run.run_id = "another-run".into();
        assert!(
            read_verified_persisted_result_window(dir.path(), &wrong_run, 0, 257)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            persist_tool_document_with_descriptor(
                dir.path(),
                "run-1",
                "call-1",
                "probe",
                "different guidance",
                ToolResultDocumentKind::RuntimeGuidance
            ),
            Err(ToolResultPersistenceError::IdentityConflict { .. })
        ));
        let replay = persist_tool_document_with_descriptor(
            dir.path(),
            "run-1",
            "call-1",
            "probe",
            &full_guidance,
            ToolResultDocumentKind::RuntimeGuidance,
        )
        .unwrap();
        assert_eq!(replay.descriptor, guidance.descriptor);
        // Ordinary result handles retain their original wire shape and paths.
        assert!(
            serde_json::to_value(&result.descriptor)
                .unwrap()
                .get("document_kind")
                .is_none()
        );
        let result_handle = session_tool_result_artifact_uri_for_descriptor(&result.descriptor);
        let encoded = result_handle
            .strip_prefix(SESSION_TOOL_RESULT_ARTIFACT_URI_PREFIX)
            .unwrap();
        let payload: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        assert!(payload.get("kind").is_none());
    }

    #[test]
    fn descriptor_must_match_the_carrying_tool_message_identity() {
        let descriptor = astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind: Default::default(),
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: "call-bound".to_string(),
            run_id: "run-bound".to_string(),
            byte_len: 4,
            content_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_string(),
        };
        let mut message = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-bound",
            TOOL_RESULT_RUN_ID_FIELD: "run-bound",
        });
        assert_eq!(
            mark_tool_result_artifact_descriptor(&mut message, Some(&descriptor)),
            Ok(())
        );
        assert_eq!(
            tool_result_artifact_descriptor(&message),
            Some(descriptor.clone())
        );

        let mut mismatched = serde_json::json!({
            "role": "tool",
            "tool_call_id": "call-other",
            TOOL_RESULT_RUN_ID_FIELD: "run-other",
        });
        assert_eq!(
            mark_tool_result_artifact_descriptor(&mut mismatched, Some(&descriptor)),
            Err(ToolResultArtifactMetadataError::IdentityMismatch)
        );
        mismatched[TOOL_RESULT_ARTIFACT_DESCRIPTOR_FIELD] =
            serde_json::to_value(&descriptor).expect("descriptor serializes");
        assert!(
            tool_result_artifact_descriptor(&mismatched).is_none(),
            "a valid descriptor from another call/run is not recovery authority"
        );
    }

    #[test]
    fn typed_descriptor_rejects_content_tampering_and_oversize_before_materialization() {
        let dir = tempfile::tempdir().unwrap();
        let content = "trusted bytes\n";
        let typed = persist_tool_result_with_descriptor(
            dir.path(),
            "run-authority",
            "call-authority",
            "agent",
            content,
        )
        .unwrap();
        assert!(
            read_verified_persisted_result(
                dir.path(),
                &typed.descriptor,
                typed.descriptor.byte_len - 1,
            )
            .unwrap_err()
            .contains("verification limit")
        );

        let artifact = run_scoped_result_path(
            dir.path(),
            &typed.descriptor.run_id,
            &typed.descriptor.call_id,
        );
        std::fs::write(artifact, "forgedd bytes\n").unwrap();
        assert_eq!(content.len(), "forgedd bytes\n".len());
        assert!(
            read_verified_persisted_result(dir.path(), &typed.descriptor, 1024)
                .unwrap_err()
                .contains("digest")
        );
    }

    #[test]
    fn same_call_id_in_different_runs_keeps_both_descriptor_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = persist_tool_result_with_descriptor(
            dir.path(),
            "run-first",
            "call-reused",
            "agent",
            "first run bytes",
        )
        .unwrap();
        let second = persist_tool_result_with_descriptor(
            dir.path(),
            "run-second",
            "call-reused",
            "agent",
            "second run bytes",
        )
        .unwrap();

        assert_eq!(
            read_verified_persisted_result(dir.path(), &first.descriptor, 1024).unwrap(),
            "first run bytes"
        );
        assert_eq!(
            read_verified_persisted_result(dir.path(), &second.descriptor, 1024).unwrap(),
            "second run bytes"
        );
        assert_ne!(
            run_scoped_result_path(dir.path(), "run-first", "call-reused"),
            run_scoped_result_path(dir.path(), "run-second", "call-reused")
        );

        let first_handle = session_tool_result_artifact_uri_for_descriptor(&first.descriptor);
        let second_handle = session_tool_result_artifact_uri_for_descriptor(&second.descriptor);
        assert_ne!(first_handle, second_handle);
        let first_window = resolve_session_tool_result_artifact_request(
            dir.path(),
            &serde_json::json!({"artifact": first_handle}),
        )
        .unwrap()
        .unwrap();
        let second_window = resolve_session_tool_result_artifact_request(
            dir.path(),
            &serde_json::json!({"artifact": second_handle}),
        )
        .unwrap()
        .unwrap();
        assert!(first_window.contains("first run bytes"));
        assert!(!first_window.contains("second run bytes"));
        assert!(second_window.contains("second run bytes"));
        assert!(!second_window.contains("first run bytes"));
    }

    #[test]
    fn run_bound_handle_fails_closed_when_immutable_bytes_are_tampered() {
        let dir = tempfile::tempdir().unwrap();
        let persisted = persist_tool_result_with_descriptor(
            dir.path(),
            "run-tamper",
            "call-tamper",
            "agent",
            "original bytes",
        )
        .unwrap();
        let handle = session_tool_result_artifact_uri_for_descriptor(&persisted.descriptor);
        let artifact = run_scoped_result_path(dir.path(), "run-tamper", "call-tamper");
        std::fs::write(artifact, "tampered bytes").unwrap();
        assert_eq!("original bytes".len(), "tampered bytes".len());

        let error = resolve_session_tool_result_artifact_request(
            dir.path(),
            &serde_json::json!({"artifact": handle}),
        )
        .unwrap()
        .unwrap_err();
        assert!(error.contains("digest"), "{error}");
    }

    #[test]
    fn same_run_and_call_with_different_bytes_is_an_identity_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let first = persist_tool_result_with_descriptor(
            dir.path(),
            "run-stable",
            "call-stable",
            "agent",
            "first immutable bytes",
        )
        .unwrap();

        let error = persist_tool_result_with_descriptor(
            dir.path(),
            "run-stable",
            "call-stable",
            "agent",
            "conflicting bytes",
        )
        .unwrap_err();
        assert!(matches!(
            error,
            ToolResultPersistenceError::IdentityConflict { ref run_id, ref call_id, .. }
                if run_id == "run-stable" && call_id == "call-stable"
        ));
        assert_eq!(
            read_verified_persisted_result(dir.path(), &first.descriptor, 1024).unwrap(),
            "first immutable bytes"
        );
        assert_eq!(
            read_persisted_result(dir.path(), "call-stable").as_deref(),
            Some("first immutable bytes"),
            "a rejected identity conflict must not overwrite the mutable model projection"
        );
    }

    #[test]
    fn exact_replay_repairs_projection_after_artifact_before_descriptor_crash_window() {
        let dir = tempfile::tempdir().unwrap();
        let first = persist_tool_result_with_descriptor(
            dir.path(),
            "run-replay",
            "call-replay",
            "agent",
            "acknowledged immutable bytes",
        )
        .unwrap();
        let projection = dir
            .path()
            .join(TOOL_RESULTS_SUBDIR)
            .join(format!("{}.txt", safe_filename_stem("call-replay")));
        std::fs::remove_file(&projection).unwrap();

        let replay = persist_tool_result_with_descriptor(
            dir.path(),
            "run-replay",
            "call-replay",
            "agent",
            "acknowledged immutable bytes",
        )
        .unwrap();

        assert_eq!(replay.descriptor, first.descriptor);
        assert_eq!(
            read_verified_persisted_result(dir.path(), &replay.descriptor, 1024).unwrap(),
            "acknowledged immutable bytes"
        );
        assert_eq!(
            std::fs::read_to_string(projection).unwrap(),
            "acknowledged immutable bytes",
            "same-byte replay must repair the non-authoritative projection"
        );
    }

    #[test]
    fn partial_temp_write_failure_never_publishes_an_immutable_identity() {
        let dir = tempfile::tempdir().unwrap();
        let artifact = run_scoped_result_path(dir.path(), "run-partial", "call-partial");
        let error =
            write_immutable_result_with(&artifact, b"complete bytes", |file, _complete_bytes| {
                std::io::Write::write_all(file, b"partial")?;
                Err(io::Error::other("injected write failure"))
            })
            .unwrap_err();

        assert!(matches!(error, ImmutableWriteError::Io(_)));
        assert!(
            !artifact.exists(),
            "a partially written temp file must never establish final identity"
        );
        write_immutable_result(&artifact, b"complete bytes").unwrap();
        assert_eq!(std::fs::read(artifact).unwrap(), b"complete bytes");
    }

    #[test]
    fn descriptor_persistence_reports_filesystem_failures_as_io() {
        let dir = tempfile::tempdir().unwrap();
        let blocked_session_dir = dir.path().join("not-a-directory");
        std::fs::write(&blocked_session_dir, "file").unwrap();

        let error = persist_tool_result_with_descriptor(
            &blocked_session_dir,
            "run-io",
            "call-io",
            "agent",
            "bytes",
        )
        .unwrap_err();
        assert!(matches!(error, ToolResultPersistenceError::Io(_)));
    }

    #[test]
    fn persisted_result_storage_does_not_assume_reader_authority() {
        let dir = tempfile::tempdir().expect("tempdir");
        let content = "bounded evidence\n".repeat(PERSIST_THRESHOLD_CHARS);
        let stored = persist_tool_result_with_descriptor(
            dir.path(),
            "run-reader-authority",
            "call-reader-authority",
            "reader",
            &content,
        )
        .expect("persist result");
        let compact = prepare_tool_result_for_compaction(
            "run-reader-authority",
            "call-reader-authority",
            "reader",
            &content,
        )
        .expect("prepare compact result");

        for projection in [&stored.replacement, &compact.replacement] {
            assert_eq!(
                parse_tool_result_artifact_projection(projection),
                Some(stored.descriptor.clone()),
                "neutral projection must retain the exact artifact identity"
            );
            assert!(
                !projection.contains("introspect("),
                "storage has no current reader authority; request assembly owns recovery instructions"
            );
        }
    }

    #[test]
    fn web_fetch_preview_preserves_structured_links_and_content_tail() {
        let dir = tempfile::tempdir().expect("tempdir");

        let content = format!(
            "page head\n{}\npage tail marker",
            "body ".repeat(PERSIST_THRESHOLD_CHARS)
        );
        let value = serde_json::json!({
            "url": "https://example.test/news",
            "content": content,
            "links": [
                {"href": "https://example.test/article", "text": "Article"},
                {"href": "https://example.test/other", "text": "Other"}
            ],
            "status": 200,
            "truncated": true
        });
        let raw = serde_json::to_string(&value).unwrap();
        let replacement = maybe_persist_tool_result(dir.path(), "structured-call", "reader", &raw)
            .expect("large web result should be persisted");

        assert!(replacement.contains("https://example.test/article"));
        assert!(replacement.contains("page tail marker"));
        assert!(replacement.contains("structured head/tail"));
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
        assert!(
            parse_session_tool_result_artifact_uri(&uri).is_none(),
            "a legacy call-only token is not immutable artifact authority"
        );
    }

    #[test]
    fn artifact_handles_are_opaque_and_round_trip_every_provider_call_id() {
        let provider_id = "call/with spaces: provider-owned 😀";
        let descriptor = astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind: Default::default(),
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: provider_id.to_string(),
            run_id: "run/with spaces: runtime-owned 😀".to_string(),
            byte_len: 42,
            content_sha256: "a".repeat(64),
        };
        let uri = session_tool_result_artifact_uri_for_descriptor(&descriptor);
        assert_eq!(
            parse_session_tool_result_artifact_uri(&uri),
            Some(descriptor.clone())
        );
        assert!(!uri.contains(provider_id));
        assert!(!uri.contains(&descriptor.run_id));
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
        let persisted = persist_tool_result_with_descriptor(
            dir.path(),
            "run-unicode",
            "call-unicode",
            "bash",
            &content,
        )
        .expect("setup must create a session artifact");

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
                "artifact": session_tool_result_artifact_uri_for_descriptor(&persisted.descriptor),
                "max_bytes": 7,
            }),
        )
        .expect("artifact request is recognized")
        .expect("artifact request succeeds");
        assert!(rendered.contains(&format!(
            "Artifact handle: {}",
            session_tool_result_artifact_uri_for_descriptor(&persisted.descriptor)
        )));
        assert!(rendered.contains("Continue with introspect("));
        assert!(!rendered.contains(dir.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn artifact_windows_normalize_non_boundary_and_preserve_cross_session_lookup() {
        let owner = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let content = "évidence\n".repeat(6_000);
        assert!(
            maybe_persist_tool_result(owner.path(), "call-boundary", "grep", &content).is_some()
        );

        let window = read_persisted_result_window(owner.path(), "call-boundary", 1, 32)
            .expect("a stale cursor should be normalized to a safe boundary")
            .expect("persisted result exists");
        assert_eq!(
            window.offset, 0,
            "the cursor must floor to the scalar start"
        );
        assert!(window.content.starts_with('é'));
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

    #[test]
    fn run_identity_is_idempotent_and_cannot_be_reassigned() {
        let mut message = serde_json::json!({"role": "tool", "content": "evidence"});
        mark_tool_result_run_id(&mut message, Some("run-a")).unwrap();
        mark_tool_result_run_id(&mut message, Some("run-a")).unwrap();
        let error = mark_tool_result_run_id(&mut message, Some("run-b")).unwrap_err();
        assert!(matches!(
            error,
            ToolResultRunIdentityError::Conflict { existing, requested }
                if existing == "run-a" && requested == "run-b"
        ));
        assert_eq!(tool_result_run_id(&message), Some("run-a"));
    }

    #[test]
    fn invalid_existing_run_identity_cannot_be_overwritten() {
        let mut message = serde_json::json!({
            "role": "tool",
            "content": "evidence",
            TOOL_RESULT_RUN_ID_FIELD: {"unexpected": "shape"},
        });
        assert_eq!(
            mark_tool_result_run_id(&mut message, Some("run-a")).unwrap_err(),
            ToolResultRunIdentityError::ExistingInvalid
        );
        assert_eq!(
            message[TOOL_RESULT_RUN_ID_FIELD],
            serde_json::json!({"unexpected": "shape"})
        );

        let mut invalid_string = serde_json::json!({
            "role": "tool",
            "content": "evidence",
            TOOL_RESULT_RUN_ID_FIELD: "run\nid",
        });
        assert_eq!(
            mark_tool_result_run_id(&mut invalid_string, Some("run-a")).unwrap_err(),
            ToolResultRunIdentityError::ExistingInvalid
        );
        assert_eq!(
            invalid_string[TOOL_RESULT_RUN_ID_FIELD],
            serde_json::json!("run\nid")
        );
    }

    #[test]
    fn verified_read_rejects_noncanonical_run_identity_before_path_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let descriptor = astra_services::session_journal::ToolResultArtifactDescriptor {
            document_kind: Default::default(),
            version: astra_services::session_journal::TOOL_RESULT_ARTIFACT_DESCRIPTOR_VERSION,
            call_id: "call-a".to_string(),
            run_id: "run\nid".to_string(),
            byte_len: 1,
            content_sha256: format!("{:064x}", 0),
        };
        let error = read_verified_persisted_result(dir.path(), &descriptor, 64).unwrap_err();
        assert_eq!(
            error,
            "persisted tool-result descriptor has an empty identity"
        );
    }

    #[test]
    fn compact_projection_contains_one_verified_recovery_handle() {
        let dir = tempfile::tempdir().unwrap();
        let persisted = persist_tool_result_for_compaction(
            dir.path(),
            "run-compact",
            "call-compact",
            "read_file",
            &"evidence\n".repeat(500),
        )
        .unwrap();
        assert_eq!(
            persisted
                .replacement
                .matches("artifact://session/tool-result/")
                .count(),
            1
        );
        let descriptor = parse_tool_result_artifact_projection(&persisted.replacement)
            .expect("compact projection must use the canonical handle grammar");
        assert_eq!(descriptor, persisted.descriptor);
        assert_eq!(
            read_verified_persisted_result(dir.path(), &descriptor, 64 * 1024).unwrap(),
            "evidence\n".repeat(500)
        );
    }
}
