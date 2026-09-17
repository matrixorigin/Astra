//! Session-scoped Explain Analyze artifacts.
//!
//! The event stream remains the source of truth. This module stores a bounded
//! redacted snapshot of the same typed facts so a later agent turn can refer to
//! the completed report without receiving the whole graph in its prompt.

use std::{
    collections::HashMap,
    fs::File,
    io::{IsTerminal, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use astra_services::SessionArtifactStore;
use astra_turn_types::{EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeEventV1};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const ARTIFACT_URI_PREFIX: &str = "artifact://session/explain-analyze/";
const ARTIFACT_DIR: &str = "explain_analyze";
const ARTIFACT_MAX_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_WINDOW_BYTES: usize = 8 * 1024;
const MAX_WINDOW_BYTES: usize = 64 * 1024;
const ARTIFACT_SCHEMA_VERSION: u16 = 1;
const ARTIFACT_TYPE: &str = "explain_analyze_snapshot";
const ARTIFACT_CONTENT_TYPE: &str = "application/json";
const ARTIFACT_STORAGE: &str = "local_session";
const RENDERED_REPORT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone)]
struct PublicationFailure {
    run_id: Option<String>,
    turn_id: Option<String>,
    reason: String,
}

static PUBLICATION_FAILURES: OnceLock<Mutex<HashMap<PathBuf, PublicationFailure>>> =
    OnceLock::new();

fn publication_failures() -> &'static Mutex<HashMap<PathBuf, PublicationFailure>> {
    PUBLICATION_FAILURES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn remember_publication_failure(
    directory: &Path,
    run_id: Option<String>,
    turn_id: Option<String>,
    reason: String,
) {
    let mut failures = publication_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    failures.insert(
        directory.to_path_buf(),
        PublicationFailure {
            run_id,
            turn_id,
            reason,
        },
    );
}

fn clear_publication_failure(directory: &Path) {
    let mut failures = publication_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    failures.remove(directory);
}

fn publication_failure(directory: &Path) -> Option<PublicationFailure> {
    let failures = publication_failures()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    failures.get(directory).cloned()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ExplainAnalyzeCaptureStatus {
    InProgress,
    Complete,
    Partial,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LatestExplainAnalyzeCapture {
    artifact_schema_version: u16,
    artifact_type: String,
    content_type: String,
    storage: String,
    representation: String,
    status: ExplainAnalyzeCaptureStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    checksum_sha256: Option<String>,
}

#[derive(Debug)]
struct LatestRecordUpdate {
    status: ExplainAnalyzeCaptureStatus,
    handle: Option<String>,
    run_id: Option<String>,
    turn_id: Option<String>,
    reason: Option<String>,
    size_bytes: Option<usize>,
    checksum_sha256: Option<String>,
}

impl Default for LatestRecordUpdate {
    fn default() -> Self {
        Self {
            status: ExplainAnalyzeCaptureStatus::InProgress,
            handle: None,
            run_id: None,
            turn_id: None,
            reason: None,
            size_bytes: None,
            checksum_sha256: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExplainAnalyzeArtifactV1 {
    artifact_schema_version: u16,
    artifact_kind: &'static str,
    artifact_type: &'static str,
    content_type: &'static str,
    storage: &'static str,
    representation: &'static str,
    schema_version: u16,
    session_id: String,
    run_id: String,
    turn_id: String,
    capture_status: ExplainAnalyzeCaptureStatus,
    delivery_degraded: bool,
    events: Vec<ExplainAnalyzeEventV1>,
}

/// Result of publishing the canonical artifact and its local, human-readable
/// companion. The opaque handle is the only identifier intended for model
/// context; the rendered path is a local UI affordance for the user.
#[derive(Debug, Clone)]
pub(crate) struct PublishedArtifact {
    pub(crate) handle: String,
    pub(crate) rendered_path: Option<PathBuf>,
    pub(crate) render_error: Option<String>,
}

impl PublishedArtifact {
    pub(crate) fn user_notice(&self) -> String {
        match (&self.rendered_path, self.render_error.as_deref()) {
            (Some(_), None) => "Explain Analyze report ready".to_string(),
            (Some(_), Some(error)) => {
                format!("Explain Analyze report ready · rendering warning: {error}")
            }
            (None, Some(error)) => {
                format!("Explain Analyze data saved · Markdown unavailable: {error}")
            }
            (None, None) => "Explain Analyze data saved · no local report is available".to_string(),
        }
    }

    /// Return the one actionable local link for a human-facing notice. The
    /// opaque session handle remains an internal/model-facing identity and is
    /// deliberately absent from this presentation path.
    pub(crate) fn user_link(&self) -> Option<crate::tui::turn_event::SystemLink> {
        let path = self.rendered_path.as_deref()?;
        let uri =
            crate::cli::terminal_hyperlinks::file_uri_for_path(&path.display().to_string(), None)?;
        Some(crate::tui::turn_event::SystemLink {
            uri,
            label: "Open report".to_string(),
            fallback: compact_local_path(path),
        })
    }

    /// Compact one-line output for non-TUI callers such as the streaming CLI.
    pub(crate) fn terminal_notice(&self) -> String {
        self.terminal_notice_with_links(std::io::stderr().is_terminal())
    }

    fn terminal_notice_with_links(&self, allow_osc8: bool) -> String {
        let notice = self.user_notice();
        let Some(link) = self.user_link() else {
            return notice;
        };
        let target = if allow_osc8 && crate::cli::terminal_hyperlinks::terminal_hyperlinks_enabled()
        {
            crate::cli::terminal_hyperlinks::osc8_link(&link.uri, &link.label)
        } else {
            link.fallback
        };
        format!("{notice} · {target}")
    }
}

fn compact_local_path(path: &Path) -> String {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return path.display().to_string();
    };
    path.strip_prefix(&home)
        .map(|relative| format!("~/{}", relative.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

pub(crate) fn artifact_handle(run_id: &str, turn_id: &str) -> String {
    let token = URL_SAFE_NO_PAD.encode(format!("{run_id}\n{turn_id}"));
    format!("{ARTIFACT_URI_PREFIX}{token}")
}

fn token_from_handle(handle: &str) -> Option<&str> {
    let token = handle.strip_prefix(ARTIFACT_URI_PREFIX)?;
    (!token.is_empty()
        && token.len() <= 1024
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(token)
}

fn artifact_path(session_dir: &Path, token: &str) -> PathBuf {
    session_dir.join(ARTIFACT_DIR).join(format!("{token}.json"))
}

fn artifact_directory(session_id: &str) -> Result<PathBuf, String> {
    astra_services::local_session_artifact_store()
        .session_path(session_id, Path::new(ARTIFACT_DIR))
        .map_err(|error| format!("resolve Explain Analyze artifact directory: {error}"))
}

fn capture_status(
    events: &[ExplainAnalyzeEventV1],
    delivery_degraded: bool,
) -> ExplainAnalyzeCaptureStatus {
    if delivery_degraded {
        return ExplainAnalyzeCaptureStatus::Partial;
    }
    let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
    for event in events {
        graph.apply(event.clone());
    }
    graph.finish_ingest();
    if !graph.nodes().is_empty()
        && graph.nodes().iter().all(|node| node.terminal_observed)
        && graph.diagnostics().is_empty()
    {
        ExplainAnalyzeCaptureStatus::Complete
    } else {
        ExplainAnalyzeCaptureStatus::Partial
    }
}

fn latest_record(session_id: &str, update: LatestRecordUpdate) -> Result<(), String> {
    let directory = artifact_directory(session_id)?;
    let record = LatestExplainAnalyzeCapture {
        artifact_schema_version: ARTIFACT_SCHEMA_VERSION,
        artifact_type: ARTIFACT_TYPE.to_string(),
        content_type: ARTIFACT_CONTENT_TYPE.to_string(),
        storage: ARTIFACT_STORAGE.to_string(),
        representation: "canonical".to_string(),
        status: update.status,
        handle: update.handle,
        run_id: update.run_id,
        turn_id: update.turn_id,
        reason: update.reason,
        size_bytes: update.size_bytes,
        checksum_sha256: update.checksum_sha256,
    };
    match publish_latest_record(&directory, &record) {
        Ok(()) => {
            clear_publication_failure(&directory);
            Ok(())
        }
        Err(error) => {
            let failure_run_id = record.run_id.clone();
            let failure_turn_id = record.turn_id.clone();
            let message = error;
            remember_publication_failure(
                &directory,
                failure_run_id,
                failure_turn_id,
                message.clone(),
            );
            Err(message)
        }
    }
}

fn publish_latest_record(
    directory: &Path,
    record: &LatestExplainAnalyzeCapture,
) -> Result<(), String> {
    let latest = directory.join("latest.json");
    let latest_temporary = directory.join("latest.json.tmp");
    if let Err(error) = std::fs::create_dir_all(directory) {
        let cleanup = remove_latest_files(&latest, &latest_temporary);
        return Err(format!(
            "create Explain Analyze artifact directory: {error}{}",
            cleanup
                .err()
                .map(|error| format!("; clear stale index: {error}"))
                .unwrap_or_default()
        ));
    }
    let bytes = serde_json::to_vec(record)
        .map_err(|error| format!("encode Explain Analyze artifact index: {error}"))?;
    let result = (|| {
        std::fs::write(&latest_temporary, bytes)
            .map_err(|error| format!("write Explain Analyze artifact index: {error}"))?;
        std::fs::rename(&latest_temporary, &latest)
            .map_err(|error| format!("publish Explain Analyze artifact index: {error}"))
    })();
    if let Err(error) = result {
        let cleanup = remove_latest_files(&latest, &latest_temporary);
        return Err(format!(
            "{error}{}",
            cleanup
                .err()
                .map(|error| format!("; clear stale index: {error}"))
                .unwrap_or_default()
        ));
    }
    Ok(())
}

fn remove_latest_files(latest: &Path, temporary: &Path) -> Result<(), String> {
    let mut errors = Vec::new();
    for path in [latest, temporary] {
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            errors.push(format!("unable to remove stale artifact index: {error}"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join(", "))
    }
}

pub(crate) fn mark_unavailable(
    session_id: &str,
    run_id: Option<&str>,
    turn_id: Option<&str>,
    reason: &str,
) -> Result<(), String> {
    if session_id.trim().is_empty() {
        return Ok(());
    }
    latest_record(
        session_id,
        LatestRecordUpdate {
            status: ExplainAnalyzeCaptureStatus::Unavailable,
            run_id: run_id.map(str::to_owned),
            turn_id: turn_id.map(str::to_owned),
            reason: Some(reason.to_string()),
            ..LatestRecordUpdate::default()
        },
    )
}

pub(crate) fn persist(
    session_id: &str,
    events: &[ExplainAnalyzeEventV1],
    delivery_degraded: bool,
) -> Result<Option<String>, String> {
    if session_id.trim().is_empty() {
        return Ok(None);
    }
    let Some(first) = events.first() else {
        mark_unavailable(
            session_id,
            None,
            None,
            "no Explain Analyze runtime facts were captured",
        )?;
        return Ok(None);
    };
    let handle = artifact_handle(&first.run_id, &first.turn_id);
    let token = token_from_handle(&handle)
        .map(str::to_owned)
        .ok_or_else(|| "invalid Explain artifact handle".to_string())?;
    let status = capture_status(events, delivery_degraded);
    latest_record(
        session_id,
        LatestRecordUpdate {
            status: ExplainAnalyzeCaptureStatus::InProgress,
            run_id: Some(first.run_id.clone()),
            turn_id: Some(first.turn_id.clone()),
            ..LatestRecordUpdate::default()
        },
    )?;
    let result = (|| {
        let payload = ExplainAnalyzeArtifactV1 {
            artifact_schema_version: ARTIFACT_SCHEMA_VERSION,
            artifact_kind: "explain_analyze",
            artifact_type: ARTIFACT_TYPE,
            content_type: ARTIFACT_CONTENT_TYPE,
            storage: ARTIFACT_STORAGE,
            representation: "canonical",
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            session_id: session_id.to_string(),
            run_id: first.run_id.clone(),
            turn_id: first.turn_id.clone(),
            capture_status: status,
            delivery_degraded,
            events: events.to_vec(),
        };
        let bytes = serde_json::to_vec_pretty(&payload).map_err(|error| error.to_string())?;
        if bytes.len() > ARTIFACT_MAX_BYTES {
            return Err(format!(
                "Explain Analyze artifact exceeds the {} byte bound",
                ARTIFACT_MAX_BYTES
            ));
        }
        let checksum_sha256 = format!("{:x}", Sha256::digest(&bytes));

        let store = astra_services::local_session_artifact_store();
        let relative_path = Path::new(ARTIFACT_DIR).join(format!("{token}.json"));
        let path = store
            .session_path(session_id, relative_path)
            .map_err(|error| format!("resolve Explain Analyze artifact path: {error}"))?;
        let parent = path
            .parent()
            .ok_or_else(|| "Explain Analyze artifact path has no parent".to_string())?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create Explain Analyze artifact directory: {error}"))?;
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, &bytes)
            .map_err(|error| format!("write Explain Analyze artifact: {error}"))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("publish Explain Analyze artifact: {error}"))?;

        latest_record(
            session_id,
            LatestRecordUpdate {
                status,
                handle: Some(handle.clone()),
                run_id: Some(first.run_id.clone()),
                turn_id: Some(first.turn_id.clone()),
                size_bytes: Some(bytes.len()),
                checksum_sha256: Some(checksum_sha256),
                ..LatestRecordUpdate::default()
            },
        )?;
        Ok(Some(handle))
    })();
    match result {
        Ok(handle) => Ok(handle),
        Err(error) => {
            let failure = mark_unavailable(
                session_id,
                Some(&first.run_id),
                Some(&first.turn_id),
                &error,
            );
            if let Err(index_error) = failure {
                return Err(format!("{error}; record unavailable status: {index_error}"));
            }
            Err(error)
        }
    }
}

/// Publish the canonical JSON snapshot and a bounded Markdown/plain-text
/// rendering for the local user interface. The canonical handle remains
/// readable even if the derived report cannot be written, so a rendering
/// failure is returned as metadata instead of hiding the usable artifact.
pub(crate) fn persist_rendered_report(
    session_id: &str,
    events: &[ExplainAnalyzeEventV1],
    delivery_degraded: bool,
    verbose: bool,
) -> Result<Option<PublishedArtifact>, String> {
    let Some(handle) = persist(session_id, events, delivery_degraded)? else {
        return Ok(None);
    };
    let token = token_from_handle(&handle)
        .map(str::to_owned)
        .ok_or_else(|| "invalid Explain artifact handle after publication".to_string())?;
    let report = crate::explain_analyze_report::render(events, verbose, delivery_degraded);
    let directory = artifact_directory(session_id)?;
    let path = directory.join(format!("{token}.md"));
    // Keep tree prefixes and aligned timing columns intact in Markdown
    // previews. The report itself is deliberately plain text; a fenced block
    // prevents proportional-font rendering from destroying its graph shape.
    let rendered = markdown_report(&report);
    if rendered.len() > RENDERED_REPORT_MAX_BYTES {
        return Ok(Some(PublishedArtifact {
            handle,
            rendered_path: None,
            render_error: Some(format!(
                "rendered report exceeds the {} byte bound",
                RENDERED_REPORT_MAX_BYTES
            )),
        }));
    }
    let render_result = (|| {
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("create Explain Analyze report directory: {error}"))?;
        let temporary = path.with_extension("md.tmp");
        std::fs::write(&temporary, rendered.as_bytes())
            .map_err(|error| format!("write Explain Analyze rendered report: {error}"))?;
        std::fs::rename(&temporary, &path)
            .map_err(|error| format!("publish Explain Analyze rendered report: {error}"))?;
        Ok::<(), String>(())
    })();
    match render_result {
        Ok(()) => Ok(Some(PublishedArtifact {
            handle,
            rendered_path: Some(path),
            render_error: None,
        })),
        Err(error) => Ok(Some(PublishedArtifact {
            handle,
            rendered_path: None,
            render_error: Some(error),
        })),
    }
}

fn markdown_report(report: &str) -> String {
    let mut fence = "```".to_string();
    while report.contains(&fence) {
        fence.push('`');
    }
    format!("# Explain Analyze\n\n{fence}text\n{report}\n{fence}\n")
}

fn latest_capture(session_dir: &Path) -> Result<Option<LatestExplainAnalyzeCapture>, String> {
    let path = session_dir.join(ARTIFACT_DIR).join("latest.json");
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read Explain Analyze artifact index: {error}")),
    };
    if metadata.len() > 16 * 1024 {
        return Err("Explain Analyze artifact index exceeds the read bound".to_string());
    }
    let bytes = std::fs::read(path)
        .map_err(|error| format!("read Explain Analyze artifact index: {error}"))?;
    let capture: LatestExplainAnalyzeCapture = serde_json::from_slice(&bytes)
        .map_err(|error| format!("decode Explain Analyze artifact index: {error}"))?;
    if capture.artifact_schema_version != ARTIFACT_SCHEMA_VERSION
        || capture.artifact_type != ARTIFACT_TYPE
        || capture.content_type != ARTIFACT_CONTENT_TYPE
        || capture.storage != ARTIFACT_STORAGE
        || capture.representation != "canonical"
    {
        return Err(
            "Explain Analyze artifact index has an unsupported type or storage".to_string(),
        );
    }
    if let Some(handle) = &capture.handle
        && token_from_handle(handle).is_none()
    {
        return Err("Explain Analyze artifact index contains an invalid handle".to_string());
    }
    match capture.status {
        ExplainAnalyzeCaptureStatus::Complete | ExplainAnalyzeCaptureStatus::Partial => {
            if capture.handle.is_none()
                || capture
                    .size_bytes
                    .is_none_or(|size| size > ARTIFACT_MAX_BYTES)
                || capture.checksum_sha256.as_deref().is_none_or(|value| {
                    value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
            {
                return Err(
                    "Explain Analyze artifact index is missing completed artifact metadata"
                        .to_string(),
                );
            }
        }
        ExplainAnalyzeCaptureStatus::InProgress | ExplainAnalyzeCaptureStatus::Unavailable => {
            if capture.handle.is_some() {
                return Err(
                    "Explain Analyze artifact index has a handle for a non-readable status"
                        .to_string(),
                );
            }
        }
    }
    Ok(Some(capture))
}

pub(crate) fn latest_notice(session_dir: &Path) -> Result<Option<String>, String> {
    let artifact_directory = session_dir.join(ARTIFACT_DIR);
    if let Some(failure) = publication_failure(&artifact_directory) {
        return Ok(Some(publication_failure_notice(&failure)));
    }
    let Some(capture) = latest_capture(session_dir)? else {
        return Ok(None);
    };
    Ok(latest_notice_for_capture(&capture))
}

fn publication_failure_notice(failure: &PublicationFailure) -> String {
    let capture = match (failure.run_id.as_deref(), failure.turn_id.as_deref()) {
        (Some(run_id), Some(turn_id)) => format!(" for run {run_id}, turn {turn_id}"),
        _ => String::new(),
    };
    format!(
        "Explain Analyze artifact unavailable{capture}: publication failed: {}",
        failure.reason
    )
}

fn latest_notice_for_capture(capture: &LatestExplainAnalyzeCapture) -> Option<String> {
    match capture.status {
        ExplainAnalyzeCaptureStatus::Complete | ExplainAnalyzeCaptureStatus::Partial => {
            let status = match capture.status {
                ExplainAnalyzeCaptureStatus::Complete => "complete",
                ExplainAnalyzeCaptureStatus::Partial => "partial",
                ExplainAnalyzeCaptureStatus::InProgress
                | ExplainAnalyzeCaptureStatus::Unavailable => unreachable!(),
            };
            capture.handle.as_deref().map(|handle| {
                format!(
                    "Explain Analyze artifact · type={} · content={} · storage={} · status={} · size={} bytes\nHandle: {handle}\nRead it with introspect(artifact=\"{handle}\", offset=0).",
                    capture.artifact_type,
                    capture.content_type,
                    capture.storage,
                    status,
                    capture.size_bytes.unwrap_or_default(),
                )
            })
        }
        ExplainAnalyzeCaptureStatus::InProgress => Some(
            "Explain Analyze artifact (latest capture is still being written; retry after the turn completes)."
                .to_string(),
        ),
        ExplainAnalyzeCaptureStatus::Unavailable => Some(format!(
            "Explain Analyze artifact unavailable for this capture: {}",
            capture
                .reason
                .as_deref()
                .unwrap_or("the runtime did not publish a report")
        )),
    }
}

fn context_notice_for_capture(capture: &LatestExplainAnalyzeCapture) -> String {
    let notice = latest_notice_for_capture(capture).unwrap_or_else(|| {
        "Explain Analyze artifact is not currently readable; report that limitation instead of inferring runtime facts."
            .to_string()
    });
    let read_instruction = capture
        .handle
        .as_deref()
        .map(|handle| {
            format!(
                "If the user asks about the previous/latest Explain Analyze run, call introspect(artifact=\"{handle}\", offset=0, max_bytes=65536) before drawing conclusions. The handle is readable only through the host that owns this session artifact store; a remote Server may report it unavailable rather than reading a client path. If introspect is unavailable in the visible tool set, report that artifact recovery is unavailable instead of guessing."
            )
        })
        .unwrap_or_else(|| {
            "The latest Explain Analyze capture is not currently readable; report that limitation instead of inferring runtime facts."
                .to_string()
        });
    format!(
        "[Explain Analyze artifact discovery]\n{notice}\n{read_instruction}\nThe CLI may also show a local rendered report path to the user; that path is a presentation affordance, while the opaque artifact handle is the model-facing source. Treat the artifact as the runtime source of truth; do not infer timing from renderer text or source code."
    )
}

/// Compact prompt-facing discovery notice for the next model turn. It names
/// the typed artifact capability and the bounded reader without copying the
/// report into the prompt or exposing a host filesystem path.
pub(crate) fn latest_context_notice(session_dir: &Path) -> Result<Option<String>, String> {
    let artifact_directory = session_dir.join(ARTIFACT_DIR);
    if let Some(failure) = publication_failure(&artifact_directory) {
        return Ok(Some(format!(
            "[Explain Analyze artifact discovery]\n{}\nThe CLI may show a rendered report path to the user when available, but the local path is not a model authority. The latest Explain Analyze capture is not currently readable; report that limitation instead of inferring runtime facts.",
            publication_failure_notice(&failure)
        )));
    }
    let Some(capture) = latest_capture(session_dir)? else {
        return Ok(None);
    };
    Ok(Some(context_notice_for_capture(&capture)))
}

fn read_artifact_window(
    path: &Path,
    offset: usize,
    max_bytes: usize,
) -> Result<(String, usize, usize), String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("read Explain Analyze artifact: {error}"))?;
    let total_bytes = usize::try_from(metadata.len())
        .map_err(|_| "Explain Analyze artifact is too large to address".to_string())?;
    if total_bytes > ARTIFACT_MAX_BYTES {
        return Err("Explain Analyze artifact exceeds the read bound".to_string());
    }
    if offset > total_bytes {
        return Err("offset is past the end of the Explain Analyze artifact".to_string());
    }

    let mut file =
        File::open(path).map_err(|error| format!("read Explain Analyze artifact: {error}"))?;
    if offset < total_bytes {
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|error| format!("seek Explain Analyze artifact: {error}"))?;
        let mut first = [0_u8; 1];
        file.read_exact(&mut first)
            .map_err(|error| format!("read Explain Analyze artifact: {error}"))?;
        if (first[0] & 0b1100_0000) == 0b1000_0000 {
            return Err("offset must be a UTF-8 boundary".to_string());
        }
        file.seek(SeekFrom::Start(offset as u64))
            .map_err(|error| format!("seek Explain Analyze artifact: {error}"))?;
    }

    let available = total_bytes.saturating_sub(offset);
    let read_len = available.min(max_bytes.saturating_add(3));
    let mut bytes = Vec::with_capacity(read_len);
    file.take(read_len as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read Explain Analyze artifact: {error}"))?;
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) if error.utf8_error().error_len().is_none() => {
            let valid_up_to = error.utf8_error().valid_up_to();
            let mut bytes = error.into_bytes();
            bytes.truncate(valid_up_to);
            String::from_utf8(bytes)
                .map_err(|error| format!("Explain Analyze artifact is not valid UTF-8: {error}"))?
        }
        Err(error) => {
            return Err(format!(
                "Explain Analyze artifact is not valid UTF-8: {error}"
            ));
        }
    };
    let mut end = content.len().min(max_bytes);
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 && available > 0 {
        return Err(
            "max_bytes is too small to advance one UTF-8 character; increase max_bytes".to_string(),
        );
    }
    Ok((content[..end].to_string(), total_bytes, offset + end))
}

pub(crate) fn resolve_request(session_dir: &Path, args: &Value) -> Option<Result<String, String>> {
    let handle = args.get("artifact")?.as_str().map(str::to_owned);
    let Some(handle) = handle else {
        return Some(Err("artifact must be a string handle".to_string()));
    };
    let token = token_from_handle(&handle)?;
    let offset = match args.get("offset") {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| "offset must be a non-negative integer".to_string()),
        None => Ok(0),
    };
    let max_bytes = match args.get("max_bytes") {
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| (1..=MAX_WINDOW_BYTES).contains(value))
            .ok_or_else(|| format!("max_bytes must be an integer from 1 to {MAX_WINDOW_BYTES}")),
        None => Ok(DEFAULT_WINDOW_BYTES),
    };
    Some((|| {
        let offset = offset?;
        let max_bytes = max_bytes?;
        let (window, total_bytes, next_offset) =
            read_artifact_window(&artifact_path(session_dir, token), offset, max_bytes)?;
        let complete = next_offset >= total_bytes;
        let continuation = if complete {
            "Complete.".to_string()
        } else {
            format!(
                "Continue with introspect(artifact=\"{handle}\", offset={next_offset}, max_bytes={max_bytes})."
            )
        };
        Ok(format!(
            "<explain-analyze-artifact>\nArtifact handle: {handle}\nBytes: [{offset}..{end}) of {}\n\n{}\n\n{continuation}\n</explain-analyze-artifact>",
            total_bytes,
            window,
            end = next_offset,
        ))
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_services::SessionArtifactStore;
    use astra_turn_types::{
        EXPLAIN_ANALYZE_SCHEMA_VERSION, ExplainAnalyzeNodeKindV1, ExplainAnalyzeOutcomeV1,
        ExplainAnalyzeTransitionV1,
    };

    fn write_latest(temp: &Path, record: LatestExplainAnalyzeCapture) {
        let directory = temp.join(ARTIFACT_DIR);
        std::fs::create_dir_all(&directory).expect("artifact directory");
        std::fs::write(
            directory.join("latest.json"),
            serde_json::to_vec(&record).expect("latest record"),
        )
        .expect("latest pointer");
    }

    fn complete_latest(handle: String, size_bytes: usize) -> LatestExplainAnalyzeCapture {
        LatestExplainAnalyzeCapture {
            artifact_schema_version: ARTIFACT_SCHEMA_VERSION,
            artifact_type: ARTIFACT_TYPE.to_string(),
            content_type: ARTIFACT_CONTENT_TYPE.to_string(),
            storage: ARTIFACT_STORAGE.to_string(),
            representation: "canonical".to_string(),
            status: ExplainAnalyzeCaptureStatus::Complete,
            handle: Some(handle),
            run_id: Some("run-1".to_string()),
            turn_id: Some("turn-2".to_string()),
            reason: None,
            size_bytes: Some(size_bytes),
            checksum_sha256: Some("0".repeat(64)),
        }
    }

    fn complete_events() -> Vec<ExplainAnalyzeEventV1> {
        let common = ExplainAnalyzeEventV1 {
            schema_version: EXPLAIN_ANALYZE_SCHEMA_VERSION,
            event_id: "clock-1:0".to_string(),
            run_id: "run-1".to_string(),
            turn_id: "turn-2".to_string(),
            node_id: "turn-2".to_string(),
            parent_node_id: None,
            dependency_node_ids: Vec::new(),
            producer_id: "test".to_string(),
            clock_domain_id: "clock-1".to_string(),
            kind: ExplainAnalyzeNodeKindV1::Turn,
            round_index: None,
            attempt_index: None,
            label: "User turn".to_string(),
            transition: ExplainAnalyzeTransitionV1::Started,
            elapsed_ms: 0,
            start_elapsed_ms: None,
            duration_ms: None,
            outcome: None,
            usage: None,
            context: None,
            coverage_gaps: Vec::new(),
        };
        let mut finished = common.clone();
        finished.event_id = "clock-1:1".to_string();
        finished.transition = ExplainAnalyzeTransitionV1::Finished;
        finished.elapsed_ms = 5;
        finished.start_elapsed_ms = Some(0);
        finished.duration_ms = Some(5);
        finished.outcome = Some(ExplainAnalyzeOutcomeV1::Completed);
        vec![common, finished]
    }

    #[test]
    fn handle_round_trip_rejects_unscoped_paths() {
        let handle = artifact_handle("run-1", "turn-2");
        assert!(token_from_handle(&handle).is_some());
        assert!(token_from_handle("/tmp/explain.json").is_none());
        assert!(token_from_handle("artifact://session/explain-analyze/").is_none());
    }

    #[test]
    fn markdown_report_preserves_tree_whitespace_and_escapes_fences() {
        let report = "Explain Analyze\n  └─ provider · 2ms\n```\nuser label";
        let markdown = markdown_report(report);
        assert!(markdown.contains("```text"), "{markdown}");
        assert!(markdown.contains("  └─ provider · 2ms"), "{markdown}");
        assert!(markdown.contains("````text"), "{markdown}");
    }

    #[test]
    fn published_artifact_notice_keeps_human_copy_compact_and_separates_the_link() {
        let local = PublishedArtifact {
            handle: artifact_handle("run-1", "turn-1"),
            rendered_path: Some(PathBuf::from("/tmp/report with spaces.md")),
            render_error: None,
        };
        let notice = local.user_notice();
        assert_eq!(notice, "Explain Analyze report ready");
        let link = local
            .user_link()
            .expect("local report should be actionable");
        assert_eq!(link.label, "Open report");
        assert!(
            link.uri
                .starts_with("file:///tmp/report%20with%20spaces.md")
        );
        assert_eq!(link.fallback, "/tmp/report with spaces.md");
        assert!(!notice.contains("artifact://"));

        let server = PublishedArtifact {
            rendered_path: None,
            ..local
        };
        let notice = server.user_notice();
        assert_eq!(
            notice,
            "Explain Analyze data saved · no local report is available"
        );
        assert!(server.user_link().is_none());

        let failed_render = PublishedArtifact {
            render_error: Some("rendered report exceeds the bound".into()),
            ..server
        };
        let notice = failed_render.user_notice();
        assert!(notice.contains("Markdown unavailable"));
        assert!(notice.contains("rendered report exceeds the bound"));
        assert_eq!(
            notice.matches("rendered report exceeds the bound").count(),
            1
        );

        let rendered_with_warning = PublishedArtifact {
            rendered_path: Some(PathBuf::from("/tmp/report with spaces.md")),
            ..failed_render.clone()
        };
        let notice = rendered_with_warning.user_notice();
        assert!(notice.starts_with("Explain Analyze report ready · rendering warning:"));
        assert!(rendered_with_warning.user_link().is_some());
        assert_eq!(
            notice.matches("rendered report exceeds the bound").count(),
            1
        );

        let no_local_copy = PublishedArtifact {
            rendered_path: None,
            render_error: None,
            ..failed_render
        };
        let notice = no_local_copy.user_notice();
        assert_eq!(
            notice,
            "Explain Analyze data saved · no local report is available"
        );
        assert!(no_local_copy.user_link().is_none());
    }

    #[test]
    fn redirected_artifact_notice_uses_plain_path_without_osc8() {
        let publication = PublishedArtifact {
            handle: artifact_handle("run-redirected", "turn-1"),
            rendered_path: Some(PathBuf::from("/tmp/report.md")),
            render_error: None,
        };

        let notice = publication.terminal_notice_with_links(false);
        assert_eq!(notice, "Explain Analyze report ready · /tmp/report.md");
        assert!(!notice.contains('\x1b'));
    }

    #[test]
    fn persist_rendered_report_keeps_canonical_handle_and_local_markdown_in_sync() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "9a5c2f6e-0f88-44db-a7a4-5e89c1d2f304";
        let events = complete_events();
        let publication = persist_rendered_report(session_id, &events, false, false)
            .expect("publication should succeed")
            .expect("non-empty capture should publish");
        let rendered_path = publication
            .rendered_path
            .as_ref()
            .expect("Markdown companion should be written");
        let rendered = std::fs::read_to_string(rendered_path).expect("rendered report");
        assert!(rendered.contains("# Explain Analyze"), "{rendered}");
        assert!(rendered.contains("```text"), "{rendered}");
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir(session_id)
            .expect("session directory");
        let resolved = resolve_request(
            &session_dir,
            &serde_json::json!({"artifact": publication.handle, "offset": 0}),
        )
        .expect("canonical handle should be recognized")
        .expect("canonical artifact should be readable");
        assert!(resolved.contains("Artifact handle:"), "{resolved}");
    }

    #[test]
    fn rendered_report_failure_does_not_hide_the_canonical_artifact() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "a5c2f6e9-0f88-44db-a7a4-5e89c1d2f305";
        let events = complete_events();
        let handle = artifact_handle("run-1", "turn-2");
        let token = token_from_handle(&handle).expect("encoded handle token");
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir(session_id)
            .expect("session directory");
        std::fs::create_dir_all(
            session_dir
                .join(ARTIFACT_DIR)
                .join(format!("{token}.md.tmp")),
        )
        .expect("block Markdown temporary path");

        let publication = persist_rendered_report(session_id, &events, false, false)
            .expect("canonical publication should succeed")
            .expect("non-empty capture should publish");
        assert!(publication.rendered_path.is_none());
        assert!(publication.render_error.is_some());
        let resolved = resolve_request(
            &session_dir,
            &serde_json::json!({"artifact": publication.handle, "offset": 0}),
        )
        .expect("canonical handle should be recognized")
        .expect("canonical artifact must remain readable when rendering fails");
        assert!(resolved.contains("Artifact handle:"), "{resolved}");
    }

    #[test]
    fn resolve_request_returns_bounded_windows_and_continuation() {
        let temp = tempfile::tempdir().expect("temporary artifact directory");
        let handle = artifact_handle("run-1", "turn-2");
        let token = token_from_handle(&handle).expect("encoded handle token");
        let artifact_dir = temp.path().join(ARTIFACT_DIR);
        std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
        let content = r#"{"artifact_kind":"explain_analyze","events":[{"node":"provider"}]}"#;
        std::fs::write(artifact_path(temp.path(), token), content).expect("artifact payload");

        let first = resolve_request(
            temp.path(),
            &serde_json::json!({"artifact": handle, "offset": 0, "max_bytes": 18}),
        )
        .expect("Explain artifact handle recognized")
        .expect("first page");
        assert!(first.contains("<explain-analyze-artifact>"), "{first}");
        assert!(first.contains("Continue with introspect"), "{first}");

        write_latest(temp.path(), complete_latest(handle.clone(), content.len()));
        let notice = latest_notice(temp.path())
            .expect("latest notice")
            .expect("completed notice");
        assert!(notice.contains(&handle), "{notice}");
        let context = latest_context_notice(temp.path())
            .expect("latest context notice")
            .expect("completed context notice");
        assert!(context.contains("introspect(artifact="), "{context}");
        assert!(context.contains("runtime source of truth"), "{context}");
    }

    #[test]
    fn resolve_request_rejects_a_window_that_cannot_advance_utf8() {
        let temp = tempfile::tempdir().expect("temporary artifact directory");
        let handle = artifact_handle("run-1", "turn-2");
        let token = token_from_handle(&handle).expect("encoded handle token");
        let artifact_dir = temp.path().join(ARTIFACT_DIR);
        std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
        std::fs::write(artifact_path(temp.path(), token), "中文").expect("artifact payload");

        let error = resolve_request(
            temp.path(),
            &serde_json::json!({"artifact": handle, "offset": 0, "max_bytes": 1}),
        )
        .expect("Explain artifact handle recognized")
        .expect_err("one byte cannot advance a Chinese UTF-8 character");
        assert!(error.contains("too small"), "{error}");
    }

    #[test]
    fn resolve_request_rejects_an_oversized_artifact_before_reading_it() {
        let temp = tempfile::tempdir().expect("temporary artifact directory");
        let handle = artifact_handle("run-1", "turn-2");
        let token = token_from_handle(&handle).expect("encoded handle token");
        let artifact_dir = temp.path().join(ARTIFACT_DIR);
        std::fs::create_dir_all(&artifact_dir).expect("artifact directory");
        std::fs::write(
            artifact_path(temp.path(), token),
            vec![b'x'; ARTIFACT_MAX_BYTES + 1],
        )
        .expect("artifact payload");

        let error = resolve_request(temp.path(), &serde_json::json!({"artifact": handle}))
            .expect("Explain artifact handle recognized")
            .expect_err("oversized artifacts are rejected");
        assert!(error.contains("exceeds the read bound"), "{error}");
    }

    #[test]
    fn unavailable_latest_capture_does_not_fall_back_to_an_older_handle() {
        let temp = tempfile::tempdir().expect("temporary artifact directory");
        let handle = artifact_handle("run-1", "turn-2");
        write_latest(
            temp.path(),
            LatestExplainAnalyzeCapture {
                artifact_schema_version: ARTIFACT_SCHEMA_VERSION,
                artifact_type: ARTIFACT_TYPE.to_string(),
                content_type: ARTIFACT_CONTENT_TYPE.to_string(),
                storage: ARTIFACT_STORAGE.to_string(),
                representation: "canonical".to_string(),
                status: ExplainAnalyzeCaptureStatus::Unavailable,
                handle: None,
                run_id: Some("run-2".to_string()),
                turn_id: Some("turn-3".to_string()),
                reason: Some("write failed".to_string()),
                size_bytes: None,
                checksum_sha256: None,
            },
        );
        let notice = latest_notice(temp.path())
            .expect("latest notice")
            .expect("unavailable notice");
        assert!(notice.contains("unavailable"), "{notice}");
        assert!(!notice.contains(&handle), "{notice}");
    }

    #[test]
    fn failed_latest_publication_suppresses_an_older_pointer_in_process() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "9a5c2f6e-0f88-44db-a7a4-5e89c1d2f304";
        let store = astra_services::local_session_artifact_store();
        let session_dir = store.session_dir(session_id).expect("session directory");
        let handle = artifact_handle("run-old", "turn-old");
        write_latest(&session_dir, complete_latest(handle.clone(), 12));
        std::fs::create_dir_all(session_dir.join(ARTIFACT_DIR).join("latest.json.tmp"))
            .expect("block the temporary index path");

        let error = latest_record(
            session_id,
            LatestRecordUpdate {
                status: ExplainAnalyzeCaptureStatus::Complete,
                handle: Some(artifact_handle("run-new", "turn-new")),
                run_id: Some("run-new".to_string()),
                turn_id: Some("turn-new".to_string()),
                size_bytes: Some(10),
                checksum_sha256: Some("1".repeat(64)),
                ..LatestRecordUpdate::default()
            },
        )
        .expect_err("the temporary index directory must make publication fail");
        assert!(
            error.contains("write Explain Analyze artifact index"),
            "{error}"
        );

        assert!(publication_failure(&session_dir.join(ARTIFACT_DIR)).is_some());
        let notice = latest_notice(&session_dir)
            .expect("latest notice")
            .unwrap_or_else(|| {
                panic!(
                    "publication failure notice missing: {error}; dir={}",
                    session_dir.display()
                )
            });
        assert!(notice.contains("publication failed"), "{notice}");
        assert!(!notice.contains(&handle), "{notice}");
    }

    #[test]
    fn resolve_request_does_not_capture_tool_result_handles() {
        let result = resolve_request(
            Path::new("/tmp"),
            &serde_json::json!({"artifact": "artifact://session/tool-result/abc"}),
        );
        assert!(result.is_none());
    }
}
