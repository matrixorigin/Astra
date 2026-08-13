use std::path::{Path, PathBuf};
use std::sync::Mutex;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use astra_services::{SessionArtifactJsonRecord, SessionArtifactJsonStore, StoredSessionArtifact};
#[cfg(test)]
use astra_tools::artifact_metadata::{
    infer_artifact_kind, infer_content_type, validate_artifact_kind, validate_content_type,
};
use astra_tools::artifact_metadata::{
    normalize_artifact_file_metadata, should_store_artifact_as_text, validate_short_token,
};
use astra_turn_core::file_edit_journal::{EditType, FileEditJournal, UndoResult};

use crate::server::tool_workspace_path_guard::unique_path_variants;
use astra_sandbox::normalize_path;

pub(crate) const MAX_PUBLISH_ARTIFACT_BYTES: u64 = 16 * 1024 * 1024;

pub(crate) async fn execute_server_run_script<E>(
    args: &Value,
    executor: &E,
    workspace_root: &Path,
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
        astra_tools::run_script::handle_run_script(args, executor, config).await
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
        Ok(v) => v,
        Err(e) => return astra_tools::ToolResult::error(e),
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

    let normalized_metadata = normalize_artifact_file_metadata(
        path,
        string_arg(args, "content_type"),
        string_arg(args, "artifact_kind"),
    )?;
    let content_type = normalized_metadata.content_type;
    let artifact_kind = normalized_metadata.artifact_kind;
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
) -> String {
    let prepared = match astra_tools::fs_ops::prepare_write_file(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error.output,
    };

    with_file_journal_mut(file_journal, "server_write_file:record_before", |journal| {
        journal.record_before(prepared.path(), "server-write", turn_index);
    });

    let result = prepared.apply();
    if !result.is_error {
        with_file_journal_mut(file_journal, "server_write_file:record_after", |journal| {
            journal.record_after(prepared.path(), "server-write", prepared.content_bytes());
        });
    }
    result.output
}

pub(crate) fn execute_server_str_replace(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> String {
    let prepared = match astra_tools::fs_ops::prepare_str_replace(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error.output,
    };

    if prepared.is_dry_run() {
        return prepared.apply().output;
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
    result.output
}

pub(crate) fn execute_server_multi_edit(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> String {
    let prepared = match astra_tools::fs_ops::prepare_multi_edit(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error.output,
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
    result.output
}

pub(crate) fn execute_server_delete_file(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> String {
    let prepared = match astra_tools::fs_ops::prepare_delete_file(workspace_root, args) {
        Ok(prepared) => prepared,
        Err(error) => return error.output,
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
    result.output
}

pub(crate) fn execute_rollback_file_edits(
    workspace_root: &Path,
    args: &Value,
    turn_index: u32,
    file_journal: &Mutex<FileEditJournal>,
) -> String {
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
                .or_else(|| args.get("after_sequence"))
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
    fn artifact_kind_validation_normalizes_and_rejects_unsafe_values() {
        assert_eq!(
            validate_artifact_kind("Report.JSON").unwrap(),
            "report.json"
        );
        assert!(validate_artifact_kind("bad/kind").is_err());
        assert!(validate_artifact_kind("bad;kind").is_err());
        assert!(validate_artifact_kind(&"a".repeat(65)).is_err());
    }

    #[test]
    fn content_type_validation_normalizes_and_rejects_parameters() {
        assert_eq!(validate_content_type("Image/PNG").unwrap(), "image/png");
        assert!(validate_content_type("not-a-mime").is_err());
        assert!(validate_content_type("text/html; charset=utf-8").is_err());
    }

    #[test]
    fn content_type_and_artifact_kind_inference_cover_common_files() {
        assert_eq!(
            infer_content_type(Path::new("report.pdf")),
            "application/pdf"
        );
        assert_eq!(
            infer_content_type(Path::new("data.jsonl")),
            "application/x-ndjson"
        );
        assert_eq!(
            infer_content_type(Path::new("main.rs")),
            "application/octet-stream"
        );
        assert_eq!(
            infer_artifact_kind(Path::new("plot.png"), "image/png"),
            "image"
        );
        assert_eq!(
            infer_artifact_kind(Path::new("data.json"), "application/json"),
            "data"
        );
        assert_eq!(
            infer_artifact_kind(Path::new("main.rs"), "application/octet-stream"),
            "code"
        );
    }

    #[test]
    fn text_storage_policy_prefers_text_for_structured_and_code_files() {
        assert!(should_store_artifact_as_text(
            "text/plain",
            Path::new("notes.txt")
        ));
        assert!(should_store_artifact_as_text(
            "application/json",
            Path::new("data.bin")
        ));
        assert!(should_store_artifact_as_text(
            "application/octet-stream",
            Path::new("main.ts")
        ));
        assert!(!should_store_artifact_as_text(
            "application/octet-stream",
            Path::new("image.raw")
        ));
    }

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
    fn prepare_publish_artifact_record_builds_text_record_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("report.md");
        let prepared = prepare_publish_artifact_record(
            &json!({"title": "Run report"}),
            &path,
            b"# report",
            dir.path(),
            "session-1",
            "user-1",
            7,
        )
        .expect("prepare artifact");

        assert_eq!(prepared.title, "Run report");
        assert_eq!(prepared.filename, "report.md");
        assert_eq!(prepared.content_type, "text/markdown");
        assert_eq!(prepared.byte_size, 8);
        assert_eq!(prepared.record.session_id, "session-1");
        assert_eq!(prepared.record.user_id, "user-1");
        assert_eq!(prepared.record.turn, Some(7));
        assert_eq!(prepared.record.content["encoding"], "utf-8");
        assert_eq!(
            prepared.record.metadata.as_ref().unwrap()["source_path"],
            "report.md"
        );
    }

    #[test]
    fn prepare_publish_artifact_record_rejects_oversized_or_invalid_metadata() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("report.md");
        let too_large = vec![0u8; MAX_PUBLISH_ARTIFACT_BYTES as usize + 1];

        assert!(
            prepare_publish_artifact_record(
                &json!({}),
                &path,
                &too_large,
                dir.path(),
                "session-1",
                "user-1",
                7,
            )
            .expect_err("oversized artifact must fail")
            .contains("supports files up to")
        );
        assert!(
            prepare_publish_artifact_record(
                &json!({"content_type": "text/html; charset=utf-8"}),
                &path,
                b"report",
                dir.path(),
                "session-1",
                "user-1",
                7,
            )
            .expect_err("invalid content type must fail")
            .contains("simple MIME type")
        );
    }

    #[test]
    fn resolve_publish_artifact_path_reads_workspace_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("report.md");
        std::fs::write(&path, "report").expect("write report");

        let (resolved, bytes) =
            resolve_publish_artifact_path(dir.path(), "report.md").expect("resolve artifact path");

        assert_eq!(resolved, path.canonicalize().expect("canonical path"));
        assert_eq!(bytes, b"report");
    }

    #[test]
    fn resolve_publish_artifact_path_rejects_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");

        let error = resolve_publish_artifact_path(dir.path(), "missing.md")
            .expect_err("missing file should fail");

        assert!(error.contains("does not resolve to an existing file"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_publish_artifact_path_rejects_existing_file_outside_allowed_roots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let temp_root = std::env::temp_dir()
            .canonicalize()
            .expect("canonical temp root");
        let outside = ["/etc/passwd", "/bin/sh", "/usr/bin/env"]
            .into_iter()
            .map(Path::new)
            .find_map(|path| {
                let canonical = path.canonicalize().ok()?;
                (!canonical.starts_with(&temp_root)).then_some(canonical)
            });
        let Some(outside) = outside else {
            return;
        };

        let error = resolve_publish_artifact_path(dir.path(), outside.to_str().unwrap())
            .expect_err("outside file should fail");

        assert!(error.contains("can only publish files under the session workspace or /tmp"));
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

        assert!(output.contains("Successfully wrote"));
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

        assert!(output.contains("Dry run"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("note.txt")).expect("read note"),
            "alpha\n"
        );
        assert!(journal.lock().expect("journal").summary().is_empty());
    }

    #[test]
    fn execute_server_delete_file_records_delete_entry_only_on_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("note.txt"), "hello").expect("write note");
        let journal = Mutex::new(FileEditJournal::new(10));

        let output =
            execute_server_delete_file(dir.path(), &json!({"path": "note.txt"}), 5, &journal);

        assert!(output.contains("Successfully deleted"));
        assert!(!dir.path().join("note.txt").exists());
        let summary = journal.lock().expect("journal").summary();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].1, 5);
        assert_eq!(summary[0].2, EditType::Delete);

        let failed =
            execute_server_delete_file(dir.path(), &json!({"path": "missing.txt"}), 6, &journal);
        assert!(failed.contains("PATH_RESOLUTION_FAILED"));
        assert_eq!(journal.lock().expect("journal").summary().len(), 1);
    }

    #[test]
    fn published_artifact_tool_result_uses_stored_artifact_identity() {
        let record = SessionArtifactJsonRecord {
            artifact_id: "prepared-id".to_string(),
            session_id: "session-1".to_string(),
            user_id: "user-1".to_string(),
            artifact_kind: "markdown".to_string(),
            source: Some("publish_artifact".to_string()),
            turn: Some(1),
            round: None,
            content: json!({}),
            metadata: None,
            references: Vec::new(),
        };
        let prepared = PreparedPublishArtifact {
            record,
            title: "Run report".to_string(),
            filename: "report.md".to_string(),
            content_type: "text/markdown".to_string(),
            byte_size: 8,
        };
        let artifact = StoredSessionArtifact {
            artifact_id: "stored-id".to_string(),
            session_id: "session-1".to_string(),
            user_id: "user-1".to_string(),
            artifact_kind: "markdown".to_string(),
            source: Some("publish_artifact".to_string()),
            turn: Some(1),
            round: None,
            content: json!({}),
            metadata: None,
            retention_policy: None,
            retention_until: None,
            status: None,
            referenced_by_manifest_count: 0,
            referenced_by_state_items_count: 0,
            referenced_by_citation_count: 0,
            referenced_by_durable_count: 0,
            created_at: Some("2026-06-14T00:00:00Z".to_string()),
        };

        let result = published_artifact_tool_result("session-1", artifact, &prepared);

        assert!(!result.is_error);
        assert!(result.output.contains("artifact_id: stored-id"));
        let metadata = result.metadata.expect("metadata");
        assert_eq!(
            metadata["artifact_ref"],
            "artifact://session/session-1/stored-id"
        );
        assert_eq!(metadata["download_filename"], "report.md");
    }
}
