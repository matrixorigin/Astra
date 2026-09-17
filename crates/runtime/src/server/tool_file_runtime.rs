use std::path::{Path, PathBuf};
use std::sync::Mutex;

use astra_turn_core::file_edit_journal::{EditType, FileEditJournal, UndoResult};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use astra_services::{SessionArtifactJsonRecord, SessionArtifactJsonStore, StoredSessionArtifact};

use crate::server::tool_workspace_path_guard::unique_path_variants;
use astra_sandbox::normalize_path;

pub(crate) const MAX_PUBLISH_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) async fn execute_server_run_script<E>(
    args: &Value,
    executor: &E,
    workspace_root: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> astra_tools::ToolResult
where
    E: astra_tools::ToolExecutor,
{
    #[cfg(unix)]
    {
        use std::collections::HashSet;

        let mut config = astra_tools::run_script::RunScriptConfig::default();
        config.mode = astra_tools::run_script::ExecutionMode::Project;
        config.session_cwd = Some(workspace_root.to_path_buf());
        config.allowed_tools = astra_tools::schemas::SERVER_RUN_SCRIPT_RPC_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<HashSet<_>>();
        astra_tools::run_script::handle_run_script_with_cancel(args, executor, config, cancel_token)
            .await
    }
    #[cfg(not(unix))]
    {
        let _ = args;
        let _ = executor;
        let _ = workspace_root;
        astra_tools::ToolResult::error(
            "run_script is not available on this platform (requires Unix domain sockets)"
                .to_string(),
        )
    }
}

pub(crate) async fn execute_publish_artifact(
    args: &Value,
    store: Option<&dyn SessionArtifactJsonStore>,
    workspace_root: &Path,
    session_id: &str,
    user_id: &str,
    turn_index: u32,
) -> astra_tools::ToolResult {
    let Some(store) = store else {
        return astra_tools::ToolResult::error(
            "Error: publish_artifact requires a configured MatrixOne artifact store for this session"
                .to_string(),
        );
    };

    let Some(raw_path) = string_arg(args, "path") else {
        return astra_tools::ToolResult::error(
            "Error: publish_artifact requires a non-empty path".to_string(),
        );
    };
    let (path, bytes) = match resolve_publish_artifact_path(workspace_root, raw_path) {
        Ok(value) => value,
        Err(error) => return astra_tools::ToolResult::error(error),
    };
    let prepared = match prepare_publish_artifact_record(
        args,
        &path,
        &bytes,
        workspace_root,
        session_id,
        user_id,
        turn_index,
    ) {
        Ok(prepared) => prepared,
        Err(error) => return astra_tools::ToolResult::error(error),
    };

    let artifact = match store.persist_json_artifact(prepared.record.clone()).await {
        Ok(artifact) => artifact,
        Err(error) => {
            return astra_tools::ToolResult::error(format!(
                "Error: failed to persist published artifact: {error}"
            ));
        }
    };
    published_artifact_tool_result(session_id, artifact, &prepared)
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub(crate) fn validate_short_token(value: &str, field: &str, max_len: usize) -> Result<(), String> {
    if value.len() > max_len {
        return Err(format!("Error: {field} must be at most {max_len} bytes"));
    }
    if value.chars().any(|ch| ch.is_control()) {
        return Err(format!(
            "Error: {field} must not contain control characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_artifact_kind(value: &str) -> Result<String, String> {
    validate_short_token(value, "artifact_kind", 64)?;
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return Err(
            "Error: artifact_kind may only contain ASCII letters, digits, '_', '-', or '.'"
                .to_string(),
        );
    }
    Ok(value.to_ascii_lowercase())
}

pub(crate) fn validate_content_type(value: &str) -> Result<String, String> {
    validate_short_token(value, "content_type", 128)?;
    if !value.contains('/') || value.contains(';') {
        return Err("Error: content_type must be a simple MIME type such as image/png".to_string());
    }
    Ok(value.to_ascii_lowercase())
}

pub(crate) fn infer_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("pdf") => "application/pdf",
        Some("html") | Some("htm") => "text/html",
        Some("md") | Some("markdown") => "text/markdown",
        Some("txt") | Some("log") => "text/plain",
        Some("json") => "application/json",
        Some("jsonl") => "application/x-ndjson",
        Some("csv") => "text/csv",
        Some("tsv") => "text/tab-separated-values",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") | Some("tgz") => "application/gzip",
        Some("parquet") => "application/vnd.apache.parquet",
        _ => "application/octet-stream",
    }
}

pub(crate) fn infer_artifact_kind(path: &Path, content_type: &str) -> &'static str {
    if content_type.starts_with("image/") {
        return "image";
    }
    match content_type {
        "application/pdf" => "pdf",
        "text/html" => "html",
        "text/markdown" => "markdown",
        "application/json"
        | "application/x-ndjson"
        | "text/csv"
        | "text/tab-separated-values"
        | "application/yaml"
        | "application/toml" => "data",
        "text/plain" => "text",
        "application/zip" | "application/x-tar" | "application/gzip" => "archive",
        _ => match path.extension().and_then(|ext| ext.to_str()) {
            Some("rs" | "go" | "py" | "ts" | "tsx" | "js" | "jsx" | "sql" | "sh") => "code",
            _ => "file",
        },
    }
}

pub(crate) fn should_store_artifact_as_text(content_type: &str, path: &Path) -> bool {
    content_type.starts_with("text/")
        || matches!(
            content_type,
            "image/svg+xml"
                | "application/json"
                | "application/x-ndjson"
                | "application/yaml"
                | "application/toml"
                | "application/xml"
        )
        || matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("rs" | "go" | "py" | "ts" | "tsx" | "js" | "jsx" | "sql" | "sh")
        )
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedPublishArtifact {
    pub(crate) record: SessionArtifactJsonRecord,
    title: String,
    filename: String,
    content_type: String,
    byte_size: usize,
}

pub(crate) fn prepare_publish_artifact_record(
    args: &Value,
    path: &Path,
    bytes: &[u8],
    workspace_root: &Path,
    session_id: &str,
    user_id: &str,
    turn: u32,
) -> Result<PreparedPublishArtifact, String> {
    if bytes.len() as u64 > MAX_PUBLISH_ARTIFACT_BYTES {
        return Err(format!(
            "Error: publish_artifact currently supports files up to {} MiB; {} is {} bytes",
            MAX_PUBLISH_ARTIFACT_BYTES / 1024 / 1024,
            path.display(),
            bytes.len()
        ));
    }

    let content_type = match string_arg(args, "content_type") {
        Some(value) => validate_content_type(value)?,
        None => infer_content_type(path).to_string(),
    };
    let artifact_kind = match string_arg(args, "artifact_kind") {
        Some(value) => validate_artifact_kind(value)?,
        None => infer_artifact_kind(path, &content_type).to_string(),
    };
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("artifact")
        .to_string();
    let title = string_arg(args, "title")
        .map(ToString::to_string)
        .unwrap_or_else(|| filename.clone());
    validate_short_token(&title, "title", 160)?;
    let description = string_arg(args, "description").map(ToString::to_string);
    if let Some(description) = &description {
        validate_short_token(description, "description", 1000)?;
    }

    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let sha256 = format!("{:x}", hasher.finalize());
    let (encoding, data) = if should_store_artifact_as_text(&content_type, path) {
        match std::str::from_utf8(bytes) {
            Ok(text) => ("utf-8", text.to_string()),
            Err(_) => ("base64", BASE64_STANDARD.encode(bytes)),
        }
    } else {
        ("base64", BASE64_STANDARD.encode(bytes))
    };

    let source_path = relative_to_workspace_root(path, workspace_root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let artifact_id = uuid::Uuid::new_v4().to_string();
    let record = SessionArtifactJsonRecord {
        artifact_id,
        session_id: session_id.to_string(),
        user_id: user_id.to_string(),
        artifact_kind: artifact_kind.clone(),
        source: Some("publish_artifact".to_string()),
        turn: Some(turn),
        round: None,
        content: json!({
            "kind": artifact_kind,
            "title": title.clone(),
            "filename": filename.clone(),
            "content_type": content_type.clone(),
            "encoding": encoding,
            "data": data,
            "description": description,
            "byte_size": bytes.len(),
            "sha256": sha256.clone(),
        }),
        metadata: Some(json!({
            "download_filename": filename.clone(),
            "content_type": content_type.clone(),
            "byte_size": bytes.len(),
            "sha256": sha256,
            "source_path": source_path,
            "normalize_version": "artifact_file_v1",
        })),
        references: Vec::new(),
    };

    Ok(PreparedPublishArtifact {
        record,
        title,
        filename,
        content_type,
        byte_size: bytes.len(),
    })
}

pub(crate) fn published_artifact_tool_result(
    session_id: &str,
    artifact: StoredSessionArtifact,
    prepared: &PreparedPublishArtifact,
) -> astra_tools::ToolResult {
    let artifact_ref = format!("artifact://session/{session_id}/{}", artifact.artifact_id);
    let output = format!(
        "Published artifact '{title}'.\n\
         artifact_ref: {artifact_ref}\n\
         artifact_id: {artifact_id}\n\
         artifact_kind: {artifact_kind}\n\
         content_type: {content_type}\n\
         download_filename: {filename}\n\
         byte_size: {byte_size}\n\
         The web UI can preview supported file types and download the stored artifact.",
        title = prepared.title,
        artifact_id = artifact.artifact_id,
        artifact_kind = artifact.artifact_kind,
        content_type = prepared.content_type,
        filename = prepared.filename,
        byte_size = prepared.byte_size,
    );
    let mut result_metadata = Map::new();
    result_metadata.insert("artifact_id".to_string(), json!(artifact.artifact_id));
    result_metadata.insert("artifact_kind".to_string(), json!(artifact.artifact_kind));
    result_metadata.insert("artifact_ref".to_string(), json!(artifact_ref));
    result_metadata.insert("download_filename".to_string(), json!(prepared.filename));
    result_metadata.insert("content_type".to_string(), json!(prepared.content_type));
    result_metadata.insert("byte_size".to_string(), json!(prepared.byte_size));
    astra_tools::ToolResult {
        output,
        metadata: Some(result_metadata),
        is_error: false,
        exit_semantics: None,
    }
}

pub(crate) fn resolve_publish_artifact_path(
    workspace_root: &Path,
    raw_path: &str,
) -> Result<(PathBuf, Vec<u8>), String> {
    let candidate = workspace_root.join(raw_path);
    let canonical = candidate.canonicalize().map_err(|error| {
        format!(
            "Error: publish_artifact path does not resolve to an existing file: {} ({error})",
            candidate.display()
        )
    })?;

    let mut allowed = false;
    if let Ok(canonical_root) = workspace_root.canonicalize()
        && canonical.starts_with(&canonical_root)
    {
        allowed = true;
    }
    if !allowed
        && let Ok(temp_root) = std::env::temp_dir().canonicalize()
        && canonical.starts_with(&temp_root)
    {
        allowed = true;
    }
    if !allowed {
        return Err(format!(
            "Error: publish_artifact can only publish files under the session workspace or /tmp: {}",
            canonical.display()
        ));
    }

    let bytes = std::fs::read(&canonical).map_err(|error| {
        format!(
            "Error: publish_artifact failed to read resolved file: {} ({error})",
            canonical.display()
        )
    })?;

    Ok((canonical, bytes))
}

pub(crate) fn undo_file_with_candidates(
    journal: &FileEditJournal,
    candidates: &[PathBuf],
) -> std::io::Result<Option<(PathBuf, EditType)>> {
    for candidate in candidates {
        match journal.undo_file(candidate)? {
            Some(edit_type) => return Ok(Some((candidate.clone(), edit_type))),
            None => continue,
        }
    }
    Ok(None)
}

pub(crate) fn relative_to_workspace_root(path: &Path, workspace_root: &Path) -> Option<PathBuf> {
    let path_variants = unique_path_variants(path);
    let root_variants = unique_path_variants(workspace_root);

    path_variants.iter().find_map(|candidate| {
        root_variants.iter().find_map(|root| {
            candidate
                .strip_prefix(root)
                .ok()
                .map(std::path::Path::to_path_buf)
        })
    })
}

pub(crate) fn display_path(path: &Path, workspace_root: &Path) -> String {
    relative_to_workspace_root(path, workspace_root)
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

pub(crate) fn rollback_path_candidates(
    raw_path: &str,
    resolved: &Path,
    workspace_root: &Path,
) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    let mut push_unique = |candidate: PathBuf| {
        if !candidates.iter().any(|existing| existing == &candidate) {
            candidates.push(candidate);
        }
    };

    for variant in unique_path_variants(resolved) {
        push_unique(variant);
    }

    let relative = if Path::new(raw_path).is_absolute() {
        relative_to_workspace_root(Path::new(raw_path), workspace_root)
    } else {
        Some(normalize_path(Path::new(raw_path)))
    };

    if let Some(relative) = relative {
        push_unique(workspace_root.join(&relative));
        if let Ok(canonical_root) = workspace_root.canonicalize() {
            push_unique(canonical_root.join(relative));
        }
    }

    candidates
}

pub(crate) fn edit_type_label(edit_type: EditType) -> &'static str {
    match edit_type {
        EditType::Create => "create",
        EditType::Overwrite => "overwrite",
        EditType::Patch => "patch",
        EditType::Delete => "delete",
    }
}

pub(crate) fn rollback_file_edits_list_result(
    summary: Vec<(PathBuf, u32, EditType)>,
    workspace_root: &Path,
) -> String {
    let entries: Vec<Value> = summary
        .into_iter()
        .map(|(path, turn_index, edit_type)| {
            json!({
                "path": display_path(&path, workspace_root),
                "turn_index": turn_index,
                "edit_type": edit_type_label(edit_type),
            })
        })
        .collect();
    json!({
        "success": true,
        "scope": "list",
        "total_entries": entries.len(),
        "entries": entries,
    })
    .to_string()
}

pub(crate) fn rollback_file_edits_missing_path_result() -> String {
    json!({
        "success": false,
        "error": "missing 'path' for scope=file",
    })
    .to_string()
}

pub(crate) fn rollback_file_edits_file_result(
    path: &Path,
    undo_result: std::io::Result<Option<(PathBuf, EditType)>>,
    workspace_root: &Path,
) -> String {
    match undo_result {
        Ok(Some((rolled_back_path, edit_type))) => json!({
            "success": true,
            "scope": "file",
            "path": display_path(&rolled_back_path, workspace_root),
            "edit_type": edit_type_label(edit_type),
            "summary": format!(
                "Rolled back the latest recorded edit for {}",
                display_path(&rolled_back_path, workspace_root)
            ),
        })
        .to_string(),
        Ok(None) => json!({
            "success": false,
            "scope": "file",
            "path": display_path(path, workspace_root),
            "error": "no recorded file edit found for that path",
        })
        .to_string(),
        Err(error) => json!({
            "success": false,
            "scope": "file",
            "path": display_path(path, workspace_root),
            "error": error.to_string(),
        })
        .to_string(),
    }
}

pub(crate) fn rollback_file_edits_missing_turn_result() -> String {
    json!({
        "success": false,
        "error": "missing 'turn_index' for scope=turn",
    })
    .to_string()
}

pub(crate) fn rollback_file_edits_turn_result(
    scope: &str,
    turn_index: u32,
    result: UndoResult,
    workspace_root: &Path,
) -> String {
    let reverted: Vec<String> = result
        .reverted
        .iter()
        .map(|path| display_path(path, workspace_root))
        .collect();
    let failed: Vec<Value> = result
        .failed
        .iter()
        .map(|(path, error)| {
            json!({
                "path": display_path(path, workspace_root),
                "error": error,
            })
        })
        .collect();
    let success = !reverted.is_empty() && failed.is_empty();
    let summary = if reverted.is_empty() {
        format!("No recorded file edits found for turn {turn_index}")
    } else if failed.is_empty() {
        format!(
            "Rolled back {} file edit{} from turn {turn_index}",
            reverted.len(),
            if reverted.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "Rolled back {} file edit{} from turn {turn_index} with {} failure{}",
            reverted.len(),
            if reverted.len() == 1 { "" } else { "s" },
            failed.len(),
            if failed.len() == 1 { "" } else { "s" }
        )
    };
    json!({
        "success": success,
        "scope": scope,
        "turn_index": turn_index,
        "reverted": reverted,
        "failed": failed,
        "summary": summary,
    })
    .to_string()
}

pub(crate) fn rollback_file_edits_invalid_scope_result(scope: &str) -> String {
    json!({
        "success": false,
        "error": format!(
            "invalid 'scope': {scope} (expected one of current_turn, turn, file, list)"
        ),
    })
    .to_string()
}

fn with_file_journal_mut<T>(
    file_journal: &Mutex<FileEditJournal>,
    operation: &'static str,
    f: impl FnOnce(&mut FileEditJournal) -> T,
) -> T {
    match file_journal.lock() {
        Ok(mut journal) => f(&mut journal),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "file_journal mutex poisoned; recovering inner journal"
            );
            let mut journal = poisoned.into_inner();
            f(&mut journal)
        }
    }
}

fn with_file_journal<T>(
    file_journal: &Mutex<FileEditJournal>,
    operation: &'static str,
    f: impl FnOnce(&FileEditJournal) -> T,
) -> T {
    match file_journal.lock() {
        Ok(journal) => f(&journal),
        Err(poisoned) => {
            tracing::warn!(
                operation,
                "file_journal mutex poisoned; recovering inner journal"
            );
            let journal = poisoned.into_inner();
            f(&journal)
        }
    }
}

pub(crate) fn file_journal_checkpoint(file_journal: &Mutex<FileEditJournal>) -> u64 {
    with_file_journal(file_journal, "file_journal_checkpoint", |journal| {
        journal.checkpoint()
    })
}

pub(crate) fn execute_server_write_file(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> astra_tools::ToolResult {
    let prepared = match astra_tools::fs_ops::prepare_write_file(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };

    let already_desired = prepared.is_already_desired();
    if !already_desired {
        with_file_journal_mut(file_journal, "server_write_file:record_before", |journal| {
            journal.record_before(prepared.path(), "server-write", turn_index);
        });
    }

    let result = prepared.apply();
    if !result.is_error && !already_desired {
        with_file_journal_mut(file_journal, "server_write_file:record_after", |journal| {
            journal.record_after(prepared.path(), "server-write", prepared.content_bytes());
        });
    }
    result
}

pub(crate) fn execute_server_str_replace(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> astra_tools::ToolResult {
    let prepared = match astra_tools::fs_ops::prepare_str_replace(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };

    if prepared.is_dry_run() {
        return prepared.apply();
    }

    let path = prepared.path().to_owned();
    let new_content_bytes = prepared.new_content_bytes().to_vec();
    with_file_journal_mut(
        file_journal,
        "server_str_replace:record_before",
        |journal| {
            journal.record_before_patch(&path, "server-str-replace", turn_index);
        },
    );

    let result = prepared.apply();
    if !result.is_error {
        with_file_journal_mut(file_journal, "server_str_replace:record_after", |journal| {
            journal.record_after(&path, "server-str-replace", &new_content_bytes);
        });
    }
    result
}

pub(crate) fn execute_server_multi_edit(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> astra_tools::ToolResult {
    // The public str_replace contract permits a top-level path to be omitted
    // when every edit carries its own path.  Keep this route on the same
    // owner/journal path as single-file multi_edit instead of forcing a
    // server-only top-level-path requirement.
    let has_per_edit_path = args
        .get("edits")
        .and_then(Value::as_array)
        .is_some_and(|edits| edits.iter().any(|edit| edit.get("path").is_some()));
    if args.get("edits").and_then(Value::as_array).is_some()
        && (args.get("path").and_then(Value::as_str).is_none() || has_per_edit_path)
    {
        let prepared = match astra_tools::fs_ops::prepare_multi_path_edit(workspace_root, args) {
            Ok(prepared) => prepared,
            Err(error) => return error,
        };
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !dry_run {
            for edit in prepared.prepared_edits() {
                with_file_journal_mut(file_journal, "server_multi_path:record_before", |journal| {
                    journal.record_before_patch(edit.path(), "server-multi-path", turn_index);
                });
            }
        }
        let result = prepared.apply();
        if !result.is_error && !dry_run {
            for edit in prepared.prepared_edits() {
                with_file_journal_mut(file_journal, "server_multi_path:record_after", |journal| {
                    journal.record_after(
                        edit.path(),
                        "server-multi-path",
                        edit.new_content_bytes(),
                    );
                });
            }
        } else if result.is_error && !dry_run {
            record_partial_mutation_journal(&result, prepared.prepared_edits(), file_journal);
        }
        return result;
    }
    let prepared = match astra_tools::fs_ops::prepare_multi_edit(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };
    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if !dry_run {
        with_file_journal_mut(file_journal, "server_multi_edit:record_before", |journal| {
            journal.record_before_patch(prepared.path(), "server-multi-edit", turn_index);
        });
    }

    let result = prepared.apply();
    if !result.is_error && !dry_run {
        with_file_journal_mut(file_journal, "server_multi_edit:record_after", |journal| {
            journal.record_after(
                prepared.path(),
                "server-multi-edit",
                prepared.new_content_bytes(),
            );
        });
    }
    result
}

fn record_partial_mutation_journal(
    result: &astra_tools::ToolResult,
    prepared: &[astra_tools::fs_ops::PreparedMultiEdit],
    file_journal: &Mutex<FileEditJournal>,
) {
    if !result.is_error {
        return;
    }
    for edit in prepared {
        let path = edit.path();
        let Ok(content) = std::fs::read(path) else {
            continue;
        };
        with_file_journal_mut(
            file_journal,
            "server_multi_path:record_partial_after",
            |journal| {
                // Match the before-entry key exactly.  Record every prepared
                // target, including the untouched suffix, so rollback restores
                // both the committed prefix and the uncommitted original bytes.
                journal.record_after(path, "server-multi-path", &content);
            },
        );
    }
}

pub(crate) fn execute_server_delete_file(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> astra_tools::ToolResult {
    let prepared = match astra_tools::fs_ops::prepare_delete_file(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error,
    };
    let path = prepared.path().to_path_buf();

    let result = prepared.apply();
    if !result.is_error {
        with_file_journal_mut(
            file_journal,
            "server_delete_file:record_delete",
            |journal| {
                journal.record_delete(
                    &path,
                    "server-delete",
                    turn_index,
                    prepared.into_before_content(),
                );
            },
        );
    }
    result
}

pub(crate) fn execute_rollback_file_edits(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> String {
    if args.get("after_sequence").is_some() {
        return serde_json::json!({
            "success": false,
            "error": "unknown field 'after_sequence'; use 'file_after_sequence'",
        })
        .to_string();
    }
    let scope = args
        .get("scope")
        .and_then(Value::as_str)
        .or_else(|| {
            if args.get("path").is_some() {
                Some("file")
            } else {
                None
            }
        })
        .unwrap_or("current_turn");

    match scope {
        "list" => {
            let summary = with_file_journal(file_journal, "rollback_file_edits:list", |journal| {
                journal.summary()
            });
            rollback_file_edits_list_result(summary, workspace_root)
        }
        "file" => {
            let raw_path = match args.get("path").and_then(Value::as_str) {
                Some(path) => path,
                None => return rollback_file_edits_missing_path_result(),
            };
            let path = match astra_tools::fs_ops::resolve_path(workspace_root, raw_path) {
                Ok(path) => path,
                Err(error) => return error,
            };
            let rollback_candidates = rollback_path_candidates(raw_path, &path, workspace_root);
            let undo_result =
                with_file_journal(file_journal, "rollback_file_edits:file", |journal| {
                    undo_file_with_candidates(journal, &rollback_candidates)
                });
            rollback_file_edits_file_result(&path, undo_result, workspace_root)
        }
        "turn" | "current_turn" => {
            let selected_turn_index = if scope == "turn" {
                match args.get("turn_index").and_then(Value::as_u64) {
                    Some(turn_index) => turn_index as u32,
                    None => return rollback_file_edits_missing_turn_result(),
                }
            } else {
                turn_index
            };
            let checkpoint = args
                .get("file_after_sequence")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let result = with_file_journal(file_journal, "rollback_file_edits:turn", |journal| {
                journal.undo_turn_since(selected_turn_index, checkpoint)
            });
            rollback_file_edits_turn_result(scope, selected_turn_index, result, workspace_root)
        }
        other => rollback_file_edits_invalid_scope_result(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execute_rollback_file_edits_requires_turn_index_for_turn_scope() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output =
            execute_rollback_file_edits(dir.path(), &json!({"scope": "turn"}), 3, &journal);
        let value: Value = serde_json::from_str(&output).expect("rollback json");

        assert_eq!(value["success"].as_bool(), Some(false));
        assert_eq!(
            value["error"].as_str(),
            Some("missing 'turn_index' for scope=turn")
        );
    }

    #[test]
    fn undo_file_with_candidates_tries_aliases_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("created.txt");
        let missing_alias = dir.path().join("missing.txt");
        let mut journal = FileEditJournal::new(10);
        journal.record_before(&path, "test", 1);
        std::fs::write(&path, "created").expect("write created file");
        journal.record_after(&path, "test", b"created");

        let result = undo_file_with_candidates(&journal, &[missing_alias, path.clone()])
            .expect("undo should not fail")
            .expect("second candidate should match");

        assert_eq!(result, (path.clone(), EditType::Create));
        assert!(!path.exists(), "created file should be deleted");
    }

    #[test]
    fn relative_and_display_path_use_workspace_when_possible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/report.md");
        std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        std::fs::write(&path, "report").expect("write");

        assert_eq!(
            relative_to_workspace_root(&path, dir.path()).unwrap(),
            PathBuf::from("nested/report.md")
        );
        assert_eq!(display_path(&path, dir.path()), "nested/report.md");
    }

    #[test]
    fn rollback_path_candidates_include_resolved_and_workspace_relative_aliases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = dir.path().join("src/main.rs");

        let candidates = rollback_path_candidates("src/main.rs", &resolved, dir.path());

        assert!(candidates.iter().any(|candidate| candidate == &resolved));
        assert!(
            candidates
                .iter()
                .any(|candidate| candidate.ends_with("src/main.rs"))
        );
    }

    #[test]
    fn edit_type_labels_are_stable_wire_values() {
        assert_eq!(edit_type_label(EditType::Create), "create");
        assert_eq!(edit_type_label(EditType::Overwrite), "overwrite");
        assert_eq!(edit_type_label(EditType::Patch), "patch");
        assert_eq!(edit_type_label(EditType::Delete), "delete");
    }

    #[test]
    fn rollback_file_edits_list_result_projects_workspace_paths() {
        let dir = tempfile::tempdir().expect("tempdir");
        let value: Value = serde_json::from_str(&rollback_file_edits_list_result(
            vec![(dir.path().join("src/main.rs"), 4, EditType::Patch)],
            dir.path(),
        ))
        .expect("json");

        assert_eq!(value["success"], true);
        assert_eq!(value["scope"], "list");
        assert_eq!(value["entries"][0]["path"], "src/main.rs");
        assert_eq!(value["entries"][0]["turn_index"], 4);
        assert_eq!(value["entries"][0]["edit_type"], "patch");
    }

    #[test]
    fn rollback_file_edits_file_result_covers_success_missing_and_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("src/main.rs");

        let success: Value = serde_json::from_str(&rollback_file_edits_file_result(
            &path,
            Ok(Some((path.clone(), EditType::Overwrite))),
            dir.path(),
        ))
        .expect("json");
        assert_eq!(success["success"], true);
        assert_eq!(success["path"], "src/main.rs");
        assert_eq!(success["edit_type"], "overwrite");

        let missing: Value = serde_json::from_str(&rollback_file_edits_file_result(
            &path,
            Ok(None),
            dir.path(),
        ))
        .expect("json");
        assert_eq!(missing["success"], false);
        assert_eq!(
            missing["error"],
            "no recorded file edit found for that path"
        );

        let failed: Value = serde_json::from_str(&rollback_file_edits_file_result(
            &path,
            Err(std::io::Error::other("permission denied")),
            dir.path(),
        ))
        .expect("json");
        assert_eq!(failed["success"], false);
        assert_eq!(failed["error"], "permission denied");
    }

    #[test]
    fn rollback_file_edits_turn_result_reports_partial_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = UndoResult {
            reverted: vec![dir.path().join("ok.txt")],
            failed: vec![(dir.path().join("bad.txt"), "locked".to_string())],
        };

        let value: Value = serde_json::from_str(&rollback_file_edits_turn_result(
            "turn",
            8,
            result,
            dir.path(),
        ))
        .expect("json");

        assert_eq!(value["success"], false);
        assert_eq!(value["scope"], "turn");
        assert_eq!(value["reverted"][0], "ok.txt");
        assert_eq!(value["failed"][0]["path"], "bad.txt");
        assert_eq!(value["failed"][0]["error"], "locked");
        assert!(
            value["summary"]
                .as_str()
                .unwrap()
                .contains("with 1 failure")
        );
    }

    #[test]
    fn rollback_file_edits_error_results_are_actionable() {
        let missing_path: Value =
            serde_json::from_str(&rollback_file_edits_missing_path_result()).expect("json");
        assert_eq!(missing_path["success"], false);
        assert_eq!(missing_path["error"], "missing 'path' for scope=file");

        let missing_turn: Value =
            serde_json::from_str(&rollback_file_edits_missing_turn_result()).expect("json");
        assert_eq!(missing_turn["success"], false);
        assert_eq!(missing_turn["error"], "missing 'turn_index' for scope=turn");

        let invalid_scope: Value =
            serde_json::from_str(&rollback_file_edits_invalid_scope_result("workspace"))
                .expect("json");
        assert_eq!(invalid_scope["success"], false);
        assert!(
            invalid_scope["error"]
                .as_str()
                .unwrap()
                .contains("expected one of current_turn")
        );
    }

    #[test]
    fn execute_server_write_file_records_create_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output = execute_server_write_file(
            dir.path(),
            &json!({"path": "note.txt", "content": "hello"}),
            3,
            &journal,
        );

        assert!(output.output.contains("Successfully wrote"));
        assert_eq!(
            output
                .metadata
                .as_ref()
                .and_then(|m| m.get("workspace_mutation_applied"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("read note"),
            "hello\n"
        );
        let summary = journal.lock().expect("journal").summary();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].1, 3);
        assert_eq!(summary[0].2, EditType::Create);
    }

    #[test]
    fn execute_server_write_file_exact_noop_preserves_journal_checkpoint() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "hello\n").expect("seed note");
        let journal = Mutex::new(FileEditJournal::new(10));
        let checkpoint = file_journal_checkpoint(&journal);

        let output = execute_server_write_file(
            dir.path(),
            &json!({"path": "note.txt", "content": "hello"}),
            3,
            &journal,
        );

        assert!(!output.is_error, "{output:?}");
        assert_eq!(file_journal_checkpoint(&journal), checkpoint);
        assert!(journal.lock().expect("journal").summary().is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("read note"),
            "hello\n"
        );
    }

    #[test]
    fn execute_server_multi_edit_dry_run_does_not_mutate_or_journal() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "alpha\n").expect("write note");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output = execute_server_multi_edit(
            dir.path(),
            &json!({
                "path": "note.txt",
                "dry_run": true,
                "edits": [{"old_str": "alpha", "new_str": "beta"}],
            }),
            4,
            &journal,
        );

        assert!(output.output.contains("Dry run"));
        assert!(
            output
                .metadata
                .as_ref()
                .and_then(|m| m.get("workspace_mutation_applied"))
                .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("read note"),
            "alpha\n"
        );
        assert!(journal.lock().expect("journal").summary().is_empty());
    }

    #[test]
    fn execute_server_str_replace_accepts_per_entry_paths_and_journals_each_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "alpha\n").expect("write a");
        std::fs::write(dir.path().join("b.txt"), "bravo\n").expect("write b");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output = execute_server_multi_edit(
            dir.path(),
            &json!({
                "edits": [
                    {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                    {"path": "b.txt", "old_str": "bravo", "new_str": "BRAVO"}
                ]
            }),
            9,
            &journal,
        );

        assert!(!output.is_error, "got: {}", output.output);
        assert_eq!(
            output
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "BRAVO\n"
        );
        assert_eq!(journal.lock().expect("journal").summary().len(), 2);
    }

    #[test]
    fn execute_server_str_replace_honors_per_entry_override_over_top_level_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "alpha\n").expect("write a");
        std::fs::write(dir.path().join("b.txt"), "bravo\n").expect("write b");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output = execute_server_multi_edit(
            dir.path(),
            &json!({
                "path": "a.txt",
                "edits": [
                    {"old_str": "alpha", "new_str": "ALPHA"},
                    {"path": "b.txt", "old_str": "bravo", "new_str": "BRAVO"}
                ]
            }),
            10,
            &journal,
        );

        assert!(!output.is_error, "got: {}", output.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "ALPHA\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("b.txt")).unwrap(),
            "BRAVO\n"
        );
        assert_eq!(journal.lock().expect("journal").summary().len(), 2);
    }

    #[test]
    fn execute_server_delete_file_records_delete_entry_only_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "hello").expect("write note");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output =
            execute_server_delete_file(dir.path(), &json!({"path": "note.txt"}), 5, &journal);

        assert!(output.output.contains("Successfully deleted"));
        assert_eq!(
            output
                .metadata
                .as_ref()
                .and_then(|m| m.get("workspace_mutation_applied"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(!dir.path().join("note.txt").exists());
        let summary = journal.lock().expect("journal").summary();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].1, 5);
        assert_eq!(summary[0].2, EditType::Delete);

        let failed =
            execute_server_delete_file(dir.path(), &json!({"path": "missing.txt"}), 6, &journal);
        assert!(failed.output.contains("PATH_RESOLUTION_FAILED"));
        assert_eq!(journal.lock().expect("journal").summary().len(), 1);
    }
}
