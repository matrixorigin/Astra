//! Session-scoped Explain Analyze artifacts.
//!
//! The event stream remains the source of truth. This module stores a bounded
//! redacted snapshot of the same typed facts so a later agent turn can refer to
//! the completed report without receiving the whole graph in its prompt.

use std::{
    collections::HashMap,
    fs::File,
    io::{IsTerminal, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use astra_config::runtime_config::ExplainReportFormat;
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
    artifact_kind: String,
    artifact_type: String,
    content_type: String,
    storage: String,
    representation: String,
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
    pub(crate) format: ExplainReportFormat,
    pub(crate) rendered_path: Option<PathBuf>,
    pub(crate) render_error: Option<String>,
}

impl PublishedArtifact {
    pub(crate) fn user_notice(&self) -> String {
        let format = self.format.display_name();
        match (&self.rendered_path, self.render_error.as_deref()) {
            (Some(_), None) => format!("Explain Analyze {format} report ready"),
            (Some(_), Some(error)) => {
                format!("Explain Analyze {format} report ready · rendering warning: {error}")
            }
            (None, Some(error)) => {
                format!("Explain Analyze data saved · {format} report unavailable: {error}")
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
            artifact_kind: "explain_analyze".into(),
            artifact_type: ARTIFACT_TYPE.into(),
            content_type: ARTIFACT_CONTENT_TYPE.into(),
            storage: ARTIFACT_STORAGE.into(),
            representation: "canonical".into(),
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

/// Publish the canonical JSON snapshot and a bounded human-readable rendering
/// in the selected format for the local user interface. The canonical handle remains
/// readable even if the derived report cannot be written, so a rendering
/// failure is returned as metadata instead of hiding the usable artifact.
pub(crate) fn persist_rendered_report(
    session_id: &str,
    events: &[ExplainAnalyzeEventV1],
    delivery_degraded: bool,
    format: ExplainReportFormat,
    verbose: bool,
) -> Result<Option<PublishedArtifact>, String> {
    let Some(handle) = persist(session_id, events, delivery_degraded)? else {
        return Ok(None);
    };
    let token = token_from_handle(&handle)
        .map(str::to_owned)
        .ok_or_else(|| "invalid Explain artifact handle after publication".to_string())?;
    let directory = artifact_directory(session_id)?;
    let (extension, rendered) = match format {
        ExplainReportFormat::Html => (
            "html",
            crate::explain_analyze_html::render(events, verbose, delivery_degraded),
        ),
        ExplainReportFormat::Markdown => {
            let report = crate::explain_analyze_report::render(events, verbose, delivery_degraded);
            ("md", markdown_report(&report))
        }
        ExplainReportFormat::Text => (
            "txt",
            crate::explain_analyze_report::render(events, verbose, delivery_degraded),
        ),
    };
    let path = directory.join(format!("{token}.{extension}"));
    if rendered.len() > RENDERED_REPORT_MAX_BYTES {
        return Ok(Some(PublishedArtifact {
            handle,
            format,
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
        // Keep the temporary file owned by `NamedTempFile` until the atomic
        // publish succeeds. A failed write or rename therefore removes its
        // partial bytes immediately instead of accumulating one bounded file
        // per failed turn.
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)
            .map_err(|error| format!("create Explain Analyze rendered report: {error}"))?;
        temporary
            .write_all(rendered.as_bytes())
            .map_err(|error| format!("write Explain Analyze rendered report: {error}"))?;
        temporary
            .persist(&path)
            .map_err(|error| format!("publish Explain Analyze rendered report: {}", error.error))?;
        Ok::<(), String>(())
    })();
    match render_result {
        Ok(()) => Ok(Some(PublishedArtifact {
            handle,
            format,
            rendered_path: Some(path),
            render_error: None,
        })),
        Err(error) => Ok(Some(PublishedArtifact {
            handle,
            format,
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
    let bytes = read_bounded_bytes(&path, 16 * 1024)?;
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

fn read_bounded_bytes(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| "local Explain capture unavailable".to_string())?;
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "local Explain capture unreadable".to_string())?;
    if bytes.len() > limit {
        return Err("local Explain capture exceeds read bound".into());
    }
    Ok(bytes)
}

pub(crate) struct LocalCapturedJudgmentUsage {
    pub(crate) scope: astra_services::reflect::JudgmentUsageScope,
    pub(crate) facts: astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1,
}

impl LocalCapturedJudgmentUsage {
    pub(crate) fn summary(&self) -> astra_services::reflect::JudgmentUsageSummary {
        let mut summary =
            astra_services::reflect::JudgmentUsageSummary::from_physical_attempts(&self.facts);
        summary.scope = self.scope.clone();
        summary.capture_incomplete = true;
        for group in &mut summary.groups {
            group.input_incomplete = true;
            group.output_incomplete = true;
        }
        summary.omitted_groups += summary.groups.len().saturating_sub(32);
        summary.groups.truncate(32);
        summary
    }
}

/// One owner-local typed reader shared by Reflect and introspect. Never falls
/// back to an older handle and never interprets journal LLM-round counters.
pub(crate) fn local_judgment_usage(session_id: &str) -> LocalCapturedJudgmentUsage {
    read_local_judgment_usage(session_id).unwrap_or_else(|_| LocalCapturedJudgmentUsage {
        scope: astra_services::reflect::JudgmentUsageScope::LocalCaptureUnavailable,
        facts: astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1 {
            available: false,
            truncated: false,
            attempts: vec![],
        },
    })
}

#[cfg(test)]
pub(crate) fn persist_test_judgment_usage(session_id: &str) {
    let mut events = tests::complete_events();
    events[1].auxiliary_usage = Some(Box::new(astra_turn_types::ExplainAnalyzeAuxiliaryUsageV1 {
        available: true,
        truncated: false,
        attempts: vec![astra_turn_types::ExplainAnalyzeAuxiliaryAttemptV1 {
            attempt_id: "physical-1".into(),
            provider: "typesafe".into(),
            offering_id: "jev-test".into(),
            model_name: "jev".into(),
            purpose: "verification_judge".into(),
            operation_id: "request_judgment".into(),
            usage_status: astra_turn_types::ExplainAnalyzeAuxiliaryUsageStatusV1::ProviderExact,
            usage: Some(astra_turn_types::ExplainAnalyzeTokenUsageV1 {
                basis: astra_turn_types::ExplainAnalyzeUsageBasisV1::ProviderExact,
                fresh_input_tokens: Some(123),
                output_tokens: Some(7),
                cache_read_tokens: None,
                cache_creation_tokens: None,
            }),
        }],
    }));
    events.push(events[1].clone());
    persist(session_id, &events, false).unwrap();
}

fn read_local_judgment_usage(session_id: &str) -> Result<LocalCapturedJudgmentUsage, String> {
    let session_dir = astra_services::local_session_artifact_store().session_dir(session_id)?;
    if publication_failure(&session_dir.join(ARTIFACT_DIR)).is_some() {
        return Err("local Explain publication unavailable".into());
    }
    let capture = latest_capture(&session_dir)?.ok_or("no local Explain capture")?;
    let handle = capture
        .handle
        .as_deref()
        .ok_or("local Explain capture not readable")?;
    let run_id = capture.run_id.as_deref().ok_or("missing capture run")?;
    let turn_id = capture.turn_id.as_deref().ok_or("missing capture turn")?;
    if [run_id, turn_id]
        .iter()
        .any(|id| id.is_empty() || id.len() > 512 || id.chars().any(char::is_control))
    {
        return Err("invalid capture identity".into());
    }
    if handle != artifact_handle(run_id, turn_id) {
        return Err("capture handle mismatch".into());
    }
    let token = token_from_handle(handle).ok_or("invalid capture handle")?;
    let bytes = read_bounded_bytes(&artifact_path(&session_dir, token), ARTIFACT_MAX_BYTES)?;
    if capture.size_bytes != Some(bytes.len())
        || capture.checksum_sha256.as_deref()
            != Some(format!("{:x}", Sha256::digest(&bytes)).as_str())
    {
        return Err("capture integrity mismatch".into());
    }
    let artifact: ExplainAnalyzeArtifactV1 =
        serde_json::from_slice(&bytes).map_err(|_| "invalid typed Explain capture".to_string())?;
    if artifact.artifact_schema_version != ARTIFACT_SCHEMA_VERSION
        || artifact.schema_version != EXPLAIN_ANALYZE_SCHEMA_VERSION
        || artifact.artifact_kind != "explain_analyze"
        || artifact.artifact_type != ARTIFACT_TYPE
        || artifact.content_type != ARTIFACT_CONTENT_TYPE
        || artifact.storage != ARTIFACT_STORAGE
        || artifact.representation != "canonical"
        || artifact.session_id != session_id
        || artifact.run_id != run_id
        || artifact.turn_id != turn_id
        || artifact.capture_status != capture.status
        || artifact
            .events
            .iter()
            .any(|e| e.run_id != run_id || e.turn_id != turn_id)
    {
        return Err("capture scope or contract mismatch".into());
    }
    let mut graph = astra_turn_types::ExplainAnalyzeGraphV1::default();
    for event in artifact.events {
        graph.apply(event);
    }
    let facts = astra_services::reflect::JudgmentUsageSummary::supported_attempts(
        &graph.auxiliary_usage_snapshot(),
    );
    // Graph::apply validates every event and auxiliary identity before retaining
    // it. Historical coverage is separate from the producer's truncation flag.
    Ok(LocalCapturedJudgmentUsage {
        scope: astra_services::reflect::JudgmentUsageScope::LocalCapturedRunTurn {
            run_id: artifact.run_id,
            turn_id: artifact.turn_id,
        },
        facts,
    })
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

    pub(super) fn complete_events() -> Vec<ExplainAnalyzeEventV1> {
        let common = ExplainAnalyzeEventV1 {
            auxiliary_usage: None,
            auxiliary_details: None,
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
    fn local_usage_conflicting_node_cannot_be_revived_by_independent_replay() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session = "local-usage-conflict";
        persist_test_judgment_usage(session);
        let directory = astra_services::local_session_artifact_store()
            .session_dir(session)
            .unwrap();
        let index = latest_capture(&directory).unwrap().unwrap();
        let path = artifact_path(
            &directory,
            token_from_handle(index.handle.as_deref().unwrap()).unwrap(),
        );
        let artifact: ExplainAnalyzeArtifactV1 =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let first = artifact.events[1].clone();
        let mut conflicting = first.clone();
        conflicting.auxiliary_usage.as_mut().unwrap().attempts[0]
            .usage
            .as_mut()
            .unwrap()
            .fresh_input_tokens = Some(947);
        let mut independent = conflicting.clone();
        independent.event_id = "independent-terminal".into();
        independent.node_id = "independent-segment".into();
        let records = [first, conflicting, independent];
        for order in [[0, 1, 2], [2, 1, 0], [0, 2, 1]] {
            let mut events = vec![artifact.events[0].clone()];
            events.extend(order.map(|index| records[index].clone()));
            assert!(events.iter().all(ExplainAnalyzeEventV1::is_valid));
            persist(session, &events, false).unwrap();
            let captured = local_judgment_usage(session);
            assert!(matches!(
                captured.scope,
                astra_services::reflect::JudgmentUsageScope::LocalCapturedRunTurn { .. }
            ));
            assert!(!captured.facts.available);
            assert!(!captured.facts.truncated);
            assert!(captured.facts.attempts.is_empty());
            let summary = captured.summary();
            assert_eq!(summary.coverage, "unavailable");
            assert!(summary.groups.is_empty());
            assert!(!summary.render().contains("947"));
            assert!(!summary.render().contains("123"));
        }
    }

    #[test]
    fn local_usage_validates_scope_checksum_bounds_and_missing_coverage() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session = "local-usage-test";
        assert!(!local_judgment_usage(session).facts.available);
        persist(session, &complete_events(), false).unwrap();
        assert!(
            !local_judgment_usage(session).facts.available,
            "no snapshot is not zero usage"
        );
        persist_test_judgment_usage(session);
        let captured = local_judgment_usage(session);
        assert_eq!(captured.facts.attempts.len(), 1);
        assert!(!captured.facts.truncated);
        let summary = captured.summary();
        assert!(summary.capture_incomplete);
        assert_eq!(summary.coverage, "available");
        assert!(!summary.render().contains("capture truncated"));
        assert_eq!(summary.groups[0].known_input_tokens, 123);
        assert!(summary.groups[0].input_incomplete);
        assert!(matches!(
            summary.scope,
            astra_services::reflect::JudgmentUsageScope::LocalCapturedRunTurn {
                ref run_id,
                ref turn_id,
            } if run_id == "run-1" && turn_id == "turn-2"
        ));
        let dir = astra_services::local_session_artifact_store()
            .session_dir(session)
            .unwrap();
        let index = latest_capture(&dir).unwrap().unwrap();
        let path = artifact_path(
            &dir,
            token_from_handle(index.handle.as_deref().unwrap()).unwrap(),
        );
        let original = std::fs::read(&path).unwrap();
        for field in [
            "attempt_id",
            "provider",
            "offering_id",
            "operation_id",
            "purpose",
            "model_name",
        ] {
            let mut payload: Value = serde_json::from_slice(&original).unwrap();
            for event in payload["events"].as_array_mut().unwrap() {
                if let Some(attempts) = event
                    .get_mut("auxiliary_usage")
                    .and_then(|usage| usage.get_mut("attempts"))
                    .and_then(Value::as_array_mut)
                {
                    attempts[0][field] = Value::String("x".repeat(1024));
                }
            }
            let bytes = serde_json::to_vec(&payload).unwrap();
            assert!(
                serde_json::from_slice::<ExplainAnalyzeArtifactV1>(&bytes).is_ok(),
                "{field}: fixture must reach canonical identity validation"
            );
            std::fs::write(&path, &bytes).unwrap();
            let mut changed = index.clone();
            changed.size_bytes = Some(bytes.len());
            changed.checksum_sha256 = Some(format!("{:x}", Sha256::digest(&bytes)));
            write_latest(&dir, changed);
            let invalid = local_judgment_usage(session);
            assert!(!invalid.facts.available, "{field}");
            assert!(matches!(
                invalid.scope,
                astra_services::reflect::JudgmentUsageScope::LocalCapturedRunTurn { .. }
            ));
            assert!(!invalid.summary().render().contains(&"x".repeat(1024)));
        }
        std::fs::write(&path, b"{}").unwrap();
        assert!(!local_judgment_usage(session).facts.available);
        for field in ["session_id", "run_id", "turn_id"] {
            let mut payload: Value = serde_json::from_slice(&original).unwrap();
            payload[field] = Value::String("foreign".into());
            let bytes = serde_json::to_vec(&payload).unwrap();
            std::fs::write(&path, &bytes).unwrap();
            let mut changed = index.clone();
            changed.size_bytes = Some(bytes.len());
            changed.checksum_sha256 = Some(format!("{:x}", Sha256::digest(&bytes)));
            write_latest(&dir, changed);
            assert!(!local_judgment_usage(session).facts.available, "{field}");
        }
        std::fs::write(&path, vec![b'x'; ARTIFACT_MAX_BYTES + 1]).unwrap();
        assert!(!local_judgment_usage(session).facts.available);
        persist_test_judgment_usage(session);
        mark_unavailable(session, None, None, "not captured").unwrap();
        assert!(
            !local_judgment_usage(session).facts.available,
            "must not reuse older success"
        );
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
            format: ExplainReportFormat::Html,
            rendered_path: Some(PathBuf::from("/tmp/report with spaces.html")),
            render_error: None,
        };
        let notice = local.user_notice();
        assert_eq!(notice, "Explain Analyze HTML report ready");
        let link = local
            .user_link()
            .expect("local report should be actionable");
        assert_eq!(link.label, "Open report");
        assert!(
            link.uri
                .starts_with("file:///tmp/report%20with%20spaces.html")
        );
        assert_eq!(link.fallback, "/tmp/report with spaces.html");
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
        assert!(notice.contains("HTML report unavailable"));
        assert!(notice.contains("rendered report exceeds the bound"));
        assert_eq!(
            notice.matches("rendered report exceeds the bound").count(),
            1
        );

        let rendered_with_warning = PublishedArtifact {
            rendered_path: Some(PathBuf::from("/tmp/report with spaces.html")),
            ..failed_render.clone()
        };
        let notice = rendered_with_warning.user_notice();
        assert!(notice.starts_with("Explain Analyze HTML report ready · rendering warning:"));
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
            format: ExplainReportFormat::Html,
            rendered_path: Some(PathBuf::from("/tmp/report.md")),
            render_error: None,
        };

        let notice = publication.terminal_notice_with_links(false);
        assert_eq!(notice, "Explain Analyze HTML report ready · /tmp/report.md");
        assert!(!notice.contains('\x1b'));
    }

    #[test]
    fn persist_rendered_report_keeps_canonical_handle_and_local_markdown_in_sync() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "9a5c2f6e-0f88-44db-a7a4-5e89c1d2f304";
        let events = complete_events();
        let publication = persist_rendered_report(
            session_id,
            &events,
            false,
            ExplainReportFormat::Markdown,
            false,
        )
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
    fn persist_rendered_report_writes_selected_html_companion_by_default() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "9a5c2f6e-0f88-44db-a7a4-5e89c1d2f305";
        let publication = persist_rendered_report(
            session_id,
            &complete_events(),
            false,
            ExplainReportFormat::Html,
            false,
        )
        .expect("publication should succeed")
        .expect("non-empty capture should publish");
        assert_eq!(publication.format, ExplainReportFormat::Html);
        let path = publication
            .rendered_path
            .as_ref()
            .expect("HTML companion should be written");
        assert_eq!(
            path.extension().and_then(|value| value.to_str()),
            Some("html")
        );
        let html = std::fs::read_to_string(path).expect("HTML report");
        assert!(html.starts_with("<!doctype html>"), "{html}");
        assert!(html.contains("Execution workspace"), "{html}");
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
        std::fs::create_dir_all(session_dir.join(ARTIFACT_DIR).join(format!("{token}.html")))
            .expect("block HTML temporary path");

        let publication =
            persist_rendered_report(session_id, &events, false, ExplainReportFormat::Html, false)
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
    fn repeated_render_failures_clean_up_temporary_reports() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "a5c2f6e9-0f88-44db-a7a4-5e89c1d2f306";
        let events = complete_events();
        let handle = artifact_handle("run-1", "turn-2");
        let token = token_from_handle(&handle).expect("encoded handle token");
        let session_dir = astra_services::local_session_artifact_store()
            .session_dir(session_id)
            .expect("session directory");
        let artifact_dir = session_dir.join(ARTIFACT_DIR);
        std::fs::create_dir_all(artifact_dir.join(format!("{token}.html")))
            .expect("block HTML publication target");

        for _ in 0..3 {
            let publication = persist_rendered_report(
                session_id,
                &events,
                false,
                ExplainReportFormat::Html,
                false,
            )
            .expect("canonical publication should succeed")
            .expect("non-empty capture should publish");
            assert!(publication.rendered_path.is_none());
            assert!(publication.render_error.is_some());
        }

        let mut entries = std::fs::read_dir(&artifact_dir)
            .expect("artifact directory")
            .map(|entry| entry.expect("directory entry").file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                std::ffi::OsString::from(format!("{token}.html")),
                std::ffi::OsString::from(format!("{token}.json")),
                std::ffi::OsString::from("latest.json"),
            ],
            "failed derived publications must not leave temporary files"
        );
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
