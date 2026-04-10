//! LSP tool: unified language server interface for code intelligence.
//! Also includes find_definition_at_position for LSP-based goto-definition.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};
use url::Url;

use super::lsp_stdio_session::path_to_uri;
use super::shell::shell_escape;
use super::{MAX_LSP_FILE_SIZE, ToolExecutor, utf16_col_to_char_idx};

impl ToolExecutor {
    // ─── LSP tool: unified language server interface ─────────────────────────────

    fn try_active_rename_workspace_edit(
        &self,
        file: &str,
        line: usize,
        column: usize,
        new_name: &str,
    ) -> Result<Option<Value>, String> {
        let (file_path, mut params) = self.lsp_position_params(file, line, column)?;
        if let Some(root) = params.as_object_mut() {
            root.insert("newName".to_string(), Value::String(new_name.to_string()));
        }
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/rename",
            params,
        )
    }

    fn workspace_path_from_uri(&self, uri: &str) -> Result<PathBuf, String> {
        let path = Url::parse(uri)
            .map_err(|e| format!("Invalid file URI in WorkspaceEdit: {uri}: {e}"))?
            .to_file_path()
            .map_err(|_| format!("WorkspaceEdit URI is not a local file path: {uri}"))?;
        let project_root = self
            .project_root
            .canonicalize()
            .unwrap_or_else(|_| self.project_root.clone());
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !canonical.starts_with(&project_root) && !path.starts_with(&project_root) {
            return Err(format!(
                "WorkspaceEdit attempted to modify a file outside the project: {}",
                path.display()
            ));
        }
        Ok(path)
    }

    fn parse_lsp_text_edit(edit: &Value) -> Result<(usize, usize, usize, usize, String), String> {
        let range = edit
            .get("range")
            .ok_or_else(|| "WorkspaceEdit text edit is missing range".to_string())?;
        let start = range
            .get("start")
            .ok_or_else(|| "WorkspaceEdit text edit is missing range.start".to_string())?;
        let end = range
            .get("end")
            .ok_or_else(|| "WorkspaceEdit text edit is missing range.end".to_string())?;
        let start_line = start
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| "WorkspaceEdit start.line must be an integer".to_string())?
            as usize;
        let start_char = start
            .get("character")
            .and_then(Value::as_u64)
            .ok_or_else(|| "WorkspaceEdit start.character must be an integer".to_string())?
            as usize;
        let end_line = end
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| "WorkspaceEdit end.line must be an integer".to_string())?
            as usize;
        let end_char = end
            .get("character")
            .and_then(Value::as_u64)
            .ok_or_else(|| "WorkspaceEdit end.character must be an integer".to_string())?
            as usize;
        let raw_new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| "WorkspaceEdit text edit is missing newText".to_string())?
            .to_string();
        let new_text = if edit.get("insertTextFormat").and_then(Value::as_u64) == Some(2) {
            Self::lsp_snippet_to_plain_text(&raw_new_text)
        } else {
            raw_new_text
        };
        Ok((start_line, start_char, end_line, end_char, new_text))
    }

    fn collect_workspace_edit_changes(
        &self,
        workspace_edit: &Value,
    ) -> Result<BTreeMap<PathBuf, Vec<(usize, usize, usize, usize, String)>>, String> {
        let mut edits_by_path = BTreeMap::new();
        if let Some(changes) = workspace_edit.get("changes").and_then(Value::as_object) {
            for (uri, edits) in changes {
                let path = self.workspace_path_from_uri(uri)?;
                let edit_array = edits
                    .as_array()
                    .ok_or_else(|| format!("WorkspaceEdit changes for {uri} must be an array"))?;
                let parsed = edit_array
                    .iter()
                    .map(Self::parse_lsp_text_edit)
                    .collect::<Result<Vec<_>, _>>()?;
                edits_by_path.insert(path, parsed);
            }
        }
        if let Some(document_changes) = workspace_edit
            .get("documentChanges")
            .and_then(Value::as_array)
        {
            for change in document_changes {
                let text_document = change
                    .get("textDocument")
                    .ok_or_else(|| {
                        "Unsupported WorkspaceEdit documentChanges entry (resource operations are not supported yet)".to_string()
                    })?;
                let uri = text_document
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "WorkspaceEdit documentChanges entry is missing textDocument.uri"
                            .to_string()
                    })?;
                let path = self.workspace_path_from_uri(uri)?;
                let edits = change
                    .get("edits")
                    .and_then(Value::as_array)
                    .ok_or_else(|| {
                        "WorkspaceEdit documentChanges entry is missing edits".to_string()
                    })?;
                let parsed = edits
                    .iter()
                    .map(Self::parse_lsp_text_edit)
                    .collect::<Result<Vec<_>, _>>()?;
                edits_by_path.entry(path).or_default().extend(parsed);
            }
        }
        Ok(edits_by_path)
    }

    fn lsp_text_edits_to_workspace_edit(uri: &str, edits: Value) -> Result<Value, String> {
        let edit_array = edits
            .as_array()
            .ok_or_else(|| {
                "LSP formatting response must be an array of TextEdit objects".to_string()
            })?
            .clone();
        Ok(json!({
            "changes": {
                uri: edit_array,
            }
        }))
    }

    fn lsp_position_to_byte_offset(
        content: &str,
        line: usize,
        character_utf16: usize,
    ) -> Result<usize, String> {
        let mut line_starts = vec![0usize];
        for (idx, byte) in content.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        let Some(&line_start) = line_starts.get(line) else {
            return Err(format!(
                "WorkspaceEdit line {} out of range (file has {} lines)",
                line + 1,
                line_starts.len()
            ));
        };
        let line_end = line_starts
            .get(line + 1)
            .map(|start| start.saturating_sub(1))
            .unwrap_or(content.len());
        let line_content = &content[line_start..line_end];
        let char_idx = utf16_col_to_char_idx(line_content, character_utf16);
        let byte_in_line = line_content
            .char_indices()
            .nth(char_idx)
            .map(|(idx, _)| idx)
            .unwrap_or(line_content.len());
        Ok(line_start + byte_in_line)
    }

    fn apply_lsp_workspace_edit(
        &self,
        operation: &str,
        method: &str,
        workspace_edit: &Value,
    ) -> Result<String, String> {
        let edits_by_path = self.collect_workspace_edit_changes(workspace_edit)?;
        if edits_by_path.is_empty() {
            return Ok(json!({
                "backend": "lsp",
                "operation": operation,
                "method": method,
                "applied": true,
                "files_changed": 0,
                "edits_applied": 0,
            })
            .to_string());
        }

        let turn_idx = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut files_changed = 0usize;
        let mut edits_applied = 0usize;
        let mut updated_files = Vec::new();

        for (path, edits) in edits_by_path {
            let mut content = std::fs::read_to_string(&path).map_err(|e| {
                format!(
                    "Failed to read {} for WorkspaceEdit apply: {e}",
                    path.display()
                )
            })?;
            let mut resolved = Vec::new();
            for (start_line, start_char, end_line, end_char, new_text) in edits {
                let start = Self::lsp_position_to_byte_offset(&content, start_line, start_char)?;
                let end = Self::lsp_position_to_byte_offset(&content, end_line, end_char)?;
                if start > end {
                    return Err(format!(
                        "WorkspaceEdit produced an invalid range for {}: start {} > end {}",
                        path.display(),
                        start,
                        end
                    ));
                }
                resolved.push((start, end, new_text));
            }
            resolved.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
            let mut last_start = usize::MAX;
            for (start, end, replacement) in &resolved {
                if *end > last_start {
                    return Err(format!(
                        "WorkspaceEdit contains overlapping edits for {}",
                        path.display()
                    ));
                }
                content.replace_range(*start..*end, replacement);
                last_start = *start;
            }

            let journal_call_id = format!("lsp_workspace_edit:{}", path.display());
            if let Ok(mut journal) = self.file_journal.lock() {
                journal.record_before(&path, &journal_call_id, turn_idx);
            }
            std::fs::write(&path, &content).map_err(|e| {
                format!(
                    "Failed to write {} for WorkspaceEdit apply: {e}",
                    path.display()
                )
            })?;
            self.record_write_with_content(&path, &content);
            if let Ok(mut journal) = self.file_journal.lock() {
                journal.record_after(&path, &journal_call_id, content.as_bytes());
            }
            files_changed += 1;
            edits_applied += resolved.len();
            updated_files.push(
                path.strip_prefix(&self.project_root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string(),
            );
        }

        Ok(json!({
            "backend": "lsp",
            "operation": operation,
            "method": method,
            "applied": true,
            "files_changed": files_changed,
            "edits_applied": edits_applied,
            "updated_files": updated_files,
        })
        .to_string())
    }

    fn active_lsp_response(operation: &str, method: &str, result: Value) -> String {
        json!({
            "backend": "lsp",
            "operation": operation,
            "method": method,
            "result": result,
        })
        .to_string()
    }

    fn resolve_lsp_file_path(&self, file: &str) -> PathBuf {
        if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            self.project_root.join(file)
        }
    }

    fn ensure_lsp_file_ready(&self, file: &str) -> Result<(PathBuf, String), String> {
        let file_path = self.resolve_lsp_file_path(file);
        if let Ok(metadata) = std::fs::metadata(&file_path)
            && metadata.len() > MAX_LSP_FILE_SIZE as u64
        {
            return Err(format!(
                "File too large for LSP operations ({} bytes, max {} bytes)",
                metadata.len(),
                MAX_LSP_FILE_SIZE
            ));
        }
        let uri = path_to_uri(&file_path)
            .ok_or_else(|| format!("Failed to create file URI for {}", file_path.display()))?;
        Ok((file_path, uri))
    }

    fn lsp_position_params(
        &self,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<(PathBuf, Value), String> {
        if line == 0 || column == 0 {
            return Err("line and column must be 1-based positive integers".to_string());
        }
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        Ok((
            file_path,
            json!({
                "textDocument": { "uri": uri },
                "position": {
                    "line": line.saturating_sub(1),
                    "character": column.saturating_sub(1),
                }
            }),
        ))
    }

    fn lsp_range_params(
        &self,
        file: &str,
        line: usize,
        column: usize,
        end_line: usize,
        end_column: usize,
    ) -> Result<(PathBuf, String, Value), String> {
        if line == 0 || column == 0 || end_line == 0 || end_column == 0 {
            return Err(
                "line, column, end_line, and end_column must be 1-based positive integers"
                    .to_string(),
            );
        }
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        Ok((
            file_path,
            uri.clone(),
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": {
                        "line": line.saturating_sub(1),
                        "character": column.saturating_sub(1),
                    },
                    "end": {
                        "line": end_line.saturating_sub(1),
                        "character": end_column.saturating_sub(1),
                    }
                }
            }),
        ))
    }

    fn lsp_full_document_range_params(&self, file: &str) -> Result<(PathBuf, Value), String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        let content = std::fs::read_to_string(&file_path)
            .map_err(|e| format!("Failed to read {}: {e}", file_path.display()))?;
        let lines: Vec<&str> = content.split('\n').collect();
        let end_line = lines.len().saturating_sub(1);
        let end_character = lines
            .last()
            .map(|line| line.encode_utf16().count())
            .unwrap_or(0);
        Ok((
            file_path,
            json!({
                "textDocument": { "uri": uri },
                "range": {
                    "start": {
                        "line": 0,
                        "character": 0,
                    },
                    "end": {
                        "line": end_line,
                        "character": end_character,
                    }
                }
            }),
        ))
    }

    fn try_active_file_request(
        &self,
        operation: &str,
        file: &str,
        method: &str,
        params: Value,
    ) -> Result<Option<String>, String> {
        let (file_path, _) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp
            .request_for_file(&self.project_root, &file_path, method, params)
            .map(|result| result.map(|value| Self::active_lsp_response(operation, method, value)))
    }

    fn try_active_position_request(
        &self,
        operation: &str,
        file: &str,
        line: usize,
        column: usize,
        method: &str,
        extra_params: Option<Value>,
    ) -> Result<Option<String>, String> {
        let (file_path, mut params) = self.lsp_position_params(file, line, column)?;
        if let Some(extra) = extra_params
            && let Some(root) = params.as_object_mut()
            && let Some(extra_obj) = extra.as_object()
        {
            for (key, value) in extra_obj {
                root.insert(key.clone(), value.clone());
            }
        }
        self.passive_lsp
            .request_for_file(&self.project_root, &file_path, method, params)
            .map(|result| result.map(|value| Self::active_lsp_response(operation, method, value)))
    }

    fn try_active_workspace_symbols(&self, query: &str) -> Result<Option<String>, String> {
        self.passive_lsp
            .request_workspace(
                &self.project_root,
                "workspace/symbol",
                json!({ "query": query }),
            )
            .map(|result| {
                result.map(|value| {
                    Self::active_lsp_response("workspace_symbols", "workspace/symbol", value)
                })
            })
    }

    fn try_active_file_diagnostics(&self, file: &str) -> Result<Option<String>, String> {
        let (file_path, _) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp
            .diagnostics_for_file(&self.project_root, &file_path)
            .map(|result| {
                result.map(|value| {
                    let method = value
                        .get("source_method")
                        .and_then(Value::as_str)
                        .unwrap_or("publishDiagnostics")
                        .to_string();
                    Self::active_lsp_response("diagnostics", &method, value)
                })
            })
    }

    fn try_active_code_actions(
        &self,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<Option<Value>, String> {
        let (file_path, position_params) = self.lsp_position_params(file, line, column)?;
        let uri = position_params
            .get("textDocument")
            .and_then(|doc| doc.get("uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| "failed to build textDocument URI for code_actions".to_string())?
            .to_string();
        let position = position_params
            .get("position")
            .cloned()
            .ok_or_else(|| "failed to build LSP position for code_actions".to_string())?;
        let diagnostics = match self
            .passive_lsp
            .diagnostics_for_file(&self.project_root, &file_path)?
        {
            Some(snapshot) => snapshot
                .get("diagnostics")
                .cloned()
                .unwrap_or_else(|| Value::Array(Vec::new())),
            None => return Ok(None),
        };
        self.passive_lsp
            .request_for_file(
                &self.project_root,
                &file_path,
                "textDocument/codeAction",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": position.clone(),
                        "end": position,
                    },
                    "context": {
                        "diagnostics": diagnostics,
                    }
                }),
            )
            .map_err(|e| e.to_string())
    }

    fn execute_lsp_command(&self, operation: &str, command: &Value) -> Result<String, String> {
        let title = command
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let command_name = command
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("selected {operation} command is missing command identifier"))?
            .to_string();
        if command_name == "astra.rust-analyzer.runnable" {
            return self.execute_rust_analyzer_runnable(
                operation,
                &title,
                command,
                "experimental/runnables",
                Some("textDocument/codeLens"),
            );
        }
        if matches!(
            command_name.as_str(),
            "rust-analyzer.runSingle" | "rust-analyzer.debugSingle"
        ) {
            return self.execute_rust_analyzer_runnable(
                operation,
                &title,
                command,
                "textDocument/codeLens",
                None,
            );
        }
        let arguments = command
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let result = self
            .passive_lsp
            .request_workspace(
                &self.project_root,
                "workspace/executeCommand",
                json!({
                    "command": command_name,
                    "arguments": arguments,
                }),
            )
            .map_err(|e| e.to_string())?;
        Ok(json!({
            "backend": "lsp",
            "operation": operation,
            "method": "workspace/executeCommand",
            "executed": true,
            "command": command_name,
            "title": title,
            "result": result.unwrap_or(Value::Null),
        })
        .to_string())
    }

    fn execute_rust_analyzer_runnable(
        &self,
        operation: &str,
        title: &str,
        command: &Value,
        method: &str,
        fallback_from: Option<&str>,
    ) -> Result<String, String> {
        let runnable = command
            .get("arguments")
            .and_then(Value::as_array)
            .and_then(|args| args.first())
            .ok_or_else(|| {
                "rust-analyzer runnable command is missing runnable payload".to_string()
            })?;
        let runnable_args = runnable
            .get("args")
            .ok_or_else(|| "rust-analyzer runnable is missing args".to_string())?;
        let cwd = runnable_args
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| "rust-analyzer runnable is missing args.cwd".to_string())?;
        let cargo_program = runnable_args
            .get("overrideCargo")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("cargo");
        let cargo_args = runnable_args
            .get("cargoArgs")
            .and_then(Value::as_array)
            .ok_or_else(|| "rust-analyzer runnable is missing args.cargoArgs".to_string())?;
        let executable_args = runnable_args
            .get("executableArgs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let environment = runnable_args
            .get("environment")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let mut parts = vec![
            format!("cd {}", shell_escape(cwd)),
            "&&".to_string(),
            "env".to_string(),
        ];
        for (key, value) in environment {
            // Validate env var name — reject anything that could inject shell commands.
            if !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') || key.is_empty() {
                return Err(format!(
                    "rust-analyzer runnable environment variable name is invalid: {key}"
                ));
            }
            let Some(value) = value.as_str() else {
                return Err(format!(
                    "rust-analyzer runnable environment value for {key} must be a string"
                ));
            };
            parts.push(format!("{key}={}", shell_escape(value)));
        }
        parts.push(shell_escape(cargo_program));
        for arg in cargo_args {
            let Some(arg) = arg.as_str() else {
                return Err("rust-analyzer runnable cargoArgs must be strings".to_string());
            };
            parts.push(shell_escape(arg));
        }
        let mut exec_args = Vec::new();
        for arg in &executable_args {
            let Some(arg) = arg.as_str() else {
                return Err("rust-analyzer runnable executableArgs must be strings".to_string());
            };
            if !arg.is_empty() {
                exec_args.push(arg.to_string());
            }
        }
        if !exec_args.is_empty() {
            parts.push("--".to_string());
            parts.extend(exec_args.iter().map(|arg| shell_escape(arg)));
        }
        let command_line = parts.join(" ");
        let runnable_kind = runnable.get("kind").cloned().unwrap_or(Value::Null);
        let output = self.run_shell_output(&command_line, 30.0)?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut response = json!({
            "backend": "lsp",
            "operation": operation,
            "method": method,
            "source": "rust-analyzer-runnables",
            "executed": output.status.success(),
            "title": title,
            "kind": runnable_kind,
            "command": cargo_program,
            "cargo_args": cargo_args,
            "executable_args": exec_args,
            "cwd": cwd,
            "command_line": command_line,
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
        });
        if let Some(fallback_from) = fallback_from
            && let Some(root) = response.as_object_mut()
        {
            root.insert(
                "fallback_from".to_string(),
                Value::String(fallback_from.to_string()),
            );
        }
        Ok(response.to_string())
    }

    fn try_resolve_code_action(&self, file: &str, action: &Value) -> Result<Option<Value>, String> {
        let (file_path, _) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp
            .request_for_file(
                &self.project_root,
                &file_path,
                "codeAction/resolve",
                action.clone(),
            )
            .map_err(|e| e.to_string())
    }

    fn try_active_document_formatting(&self, file: &str) -> Result<Option<Value>, String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/formatting",
            json!({
                "textDocument": { "uri": uri },
                "options": {
                    "tabSize": 4,
                    "insertSpaces": true,
                }
            }),
        )
    }

    fn try_active_range_formatting(
        &self,
        file: &str,
        line: usize,
        column: usize,
        end_line: usize,
        end_column: usize,
    ) -> Result<Option<Value>, String> {
        let (file_path, _, mut params) =
            self.lsp_range_params(file, line, column, end_line, end_column)?;
        if let Some(root) = params.as_object_mut() {
            root.insert(
                "options".to_string(),
                json!({
                    "tabSize": 4,
                    "insertSpaces": true,
                }),
            );
        }
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/rangeFormatting",
            params,
        )
    }

    fn try_active_on_type_formatting(
        &self,
        file: &str,
        line: usize,
        column: usize,
        trigger_character: &str,
    ) -> Result<Option<Value>, String> {
        if trigger_character.is_empty() {
            return Err("trigger_character must be a non-empty string".to_string());
        }
        let (file_path, mut params) = self.lsp_position_params(file, line, column)?;
        if let Some(root) = params.as_object_mut() {
            root.insert(
                "ch".to_string(),
                Value::String(trigger_character.to_string()),
            );
            root.insert(
                "options".to_string(),
                json!({
                    "tabSize": 4,
                    "insertSpaces": true,
                }),
            );
        }
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/onTypeFormatting",
            params,
        )
    }

    fn try_active_inlay_hints(
        &self,
        file: &str,
        range: Option<(usize, usize, usize, usize)>,
    ) -> Result<Option<Value>, String> {
        let (file_path, params) = if let Some((line, column, end_line, end_column)) = range {
            let (file_path, _, params) =
                self.lsp_range_params(file, line, column, end_line, end_column)?;
            (file_path, params)
        } else {
            self.lsp_full_document_range_params(file)?
        };
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/inlayHint",
            params,
        )
    }

    fn try_active_folding_ranges(&self, file: &str) -> Result<Option<Value>, String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/foldingRange",
            json!({
                "textDocument": { "uri": uri },
            }),
        )
    }

    fn try_active_document_colors(&self, file: &str) -> Result<Option<Value>, String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/documentColor",
            json!({
                "textDocument": { "uri": uri },
            }),
        )
    }

    fn try_active_semantic_tokens(&self, file: &str) -> Result<Option<Value>, String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/semanticTokens/full",
            json!({
                "textDocument": { "uri": uri },
            }),
        )
    }

    fn try_active_completions(
        &self,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<Option<Value>, String> {
        let (file_path, params) = self.lsp_position_params(file, line, column)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/completion",
            params,
        )
    }

    fn try_active_code_lenses(&self, file: &str) -> Result<Option<Value>, String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/codeLens",
            json!({
                "textDocument": { "uri": uri },
            }),
        )
    }

    fn try_active_rust_analyzer_runnables(&self, file: &str) -> Result<Option<Value>, String> {
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        if file_path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return Ok(None);
        }
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "experimental/runnables",
            json!({
                "textDocument": { "uri": uri },
                "position": { "line": 0, "character": 0 },
            }),
        )
    }

    fn rust_analyzer_runnable_strings(
        runnable: &Value,
    ) -> Option<(String, String, Vec<String>, Vec<String>)> {
        let args = runnable.get("args")?;
        let cargo_program = args
            .get("overrideCargo")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("cargo")
            .to_string();
        let cargo_args = args
            .get("cargoArgs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let executable_args = args
            .get("executableArgs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|arg| !arg.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut preview = vec![cargo_program.clone()];
        preview.extend(cargo_args.iter().cloned());
        if !executable_args.is_empty() {
            preview.push("--".to_string());
            preview.extend(executable_args.iter().cloned());
        }
        Some((
            cargo_program,
            preview.join(" "),
            cargo_args,
            executable_args,
        ))
    }

    fn rust_analyzer_runnable_priority(runnable: &Value) -> u8 {
        let label = runnable
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let cargo_args = Self::rust_analyzer_runnable_strings(runnable)
            .map(|(_, _, cargo_args, _)| cargo_args.join(" ").to_ascii_lowercase())
            .unwrap_or_default();

        // Check cargo subcommand — must be a whole word (followed by space or end)
        let cargo_cmd_is = |cmd: &str| -> bool {
            cargo_args == cmd
                || cargo_args.starts_with(&format!("{cmd} "))
                || cargo_args.contains(&format!(" {cmd} "))
                || cargo_args.ends_with(&format!(" {cmd}"))
        };

        if label.starts_with("run ") || cargo_cmd_is("run") {
            0
        } else if label.starts_with("test ") || cargo_cmd_is("test") {
            1
        } else if label.starts_with("bench ") || cargo_cmd_is("bench") {
            2
        } else if label.contains("check") || cargo_cmd_is("check") {
            3
        } else if label.contains("build") || cargo_cmd_is("build") {
            4
        } else {
            5
        }
    }

    fn rust_analyzer_runnables_to_code_lenses(runnables: &Value) -> Value {
        let Some(items) = runnables.as_array() else {
            return Value::Array(Vec::new());
        };
        let mut sorted = items.to_vec();
        sorted.sort_by(|left, right| {
            let left_priority = Self::rust_analyzer_runnable_priority(left);
            let right_priority = Self::rust_analyzer_runnable_priority(right);
            left_priority.cmp(&right_priority).then_with(|| {
                left.get("label")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .get("label")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
            })
        });
        Value::Array(
            sorted
                .iter()
                .map(|item| {
                    let range = item
                        .pointer("/location/targetRange")
                        .cloned()
                        .or_else(|| item.pointer("/location/targetSelectionRange").cloned())
                        .unwrap_or_else(|| {
                            json!({
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 0 },
                            })
                        });
                    let priority = Self::rust_analyzer_runnable_priority(item);
                    let (_, command_preview, cargo_args, executable_args) =
                        Self::rust_analyzer_runnable_strings(item).unwrap_or_else(|| {
                            (
                                "cargo".to_string(),
                                "cargo".to_string(),
                                Vec::new(),
                                Vec::new(),
                            )
                        });
                    json!({
                        "range": range,
                        "command": {
                            "title": item.get("label").and_then(Value::as_str).unwrap_or("rust-analyzer runnable"),
                            "command": "astra.rust-analyzer.runnable",
                            "arguments": [item.clone()],
                        },
                        "data": {
                            "source": "experimental/runnables",
                            "kind": item.get("kind").cloned().unwrap_or(Value::Null),
                            "preferred": priority <= 1,
                            "priority": priority,
                            "command_preview": command_preview,
                            "cargo_args": cargo_args,
                            "executable_args": executable_args,
                        }
                    })
                })
                .collect(),
        )
    }

    fn try_active_code_lenses_with_fallback(
        &self,
        file: &str,
    ) -> Result<Option<(&'static str, Value)>, String> {
        match self.try_active_code_lenses(file)? {
            Some(result) if result.as_array().is_some_and(|items| !items.is_empty()) => {
                Ok(Some(("textDocument/codeLens", result)))
            }
            Some(result) => match self.try_active_rust_analyzer_runnables(file)? {
                Some(runnables) if runnables.as_array().is_some_and(|items| !items.is_empty()) => {
                    Ok(Some((
                        "experimental/runnables",
                        Self::rust_analyzer_runnables_to_code_lenses(&runnables),
                    )))
                }
                _ => Ok(Some(("textDocument/codeLens", result))),
            },
            None => Ok(None),
        }
    }

    fn try_resolve_completion_item(
        &self,
        file: &str,
        item: &Value,
    ) -> Result<Option<Value>, String> {
        let file_path = self.resolve_lsp_file_path(file);
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "completionItem/resolve",
            item.clone(),
        )
    }

    fn lsp_snippet_to_plain_text(snippet: &str) -> String {
        fn parse_placeholder(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
            while chars.peek().is_some_and(|ch| ch.is_ascii_digit()) {
                chars.next();
            }
            match chars.peek().copied() {
                Some(':') => {
                    chars.next();
                    parse_until_brace(chars)
                }
                Some('|') => {
                    chars.next();
                    parse_choice(chars)
                }
                Some('}') => {
                    chars.next();
                    String::new()
                }
                _ => {
                    while let Some(ch) = chars.next() {
                        match ch {
                            '\\' => {
                                chars.next();
                            }
                            '}' => break,
                            _ => {}
                        }
                    }
                    String::new()
                }
            }
        }

        fn parse_until_brace(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
            let mut out = String::new();
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        if let Some(next) = chars.next() {
                            out.push(next);
                        }
                    }
                    '$' => match chars.peek().copied() {
                        Some('{') => {
                            chars.next();
                            out.push_str(&parse_placeholder(chars));
                        }
                        Some(next) if next.is_ascii_digit() => {
                            while chars.peek().is_some_and(|digit| digit.is_ascii_digit()) {
                                chars.next();
                            }
                        }
                        _ => out.push('$'),
                    },
                    '}' => break,
                    _ => out.push(ch),
                }
            }
            out
        }

        fn parse_choice(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
            let mut first = String::new();
            let mut collecting_first = true;
            while let Some(ch) = chars.next() {
                match ch {
                    '\\' => {
                        if let Some(next) = chars.next()
                            && collecting_first
                        {
                            first.push(next);
                        }
                    }
                    ',' => collecting_first = false,
                    '|' if chars.peek() == Some(&'}') => {
                        chars.next();
                        break;
                    }
                    _ if collecting_first => first.push(ch),
                    _ => {}
                }
            }
            first
        }

        let mut chars = snippet.chars().peekable();
        let mut out = String::new();
        while let Some(ch) = chars.next() {
            match ch {
                '\\' => {
                    if let Some(next) = chars.next() {
                        out.push(next);
                    }
                }
                '$' => match chars.peek().copied() {
                    Some('{') => {
                        chars.next();
                        out.push_str(&parse_placeholder(&mut chars));
                    }
                    Some(next) if next.is_ascii_digit() => {
                        while chars.peek().is_some_and(|digit| digit.is_ascii_digit()) {
                            chars.next();
                        }
                    }
                    _ => out.push('$'),
                },
                _ => out.push(ch),
            }
        }
        out
    }

    fn normalize_completion_text_edit(edit: &Value) -> Result<Value, String> {
        let new_text = Self::lsp_snippet_to_plain_text(
            edit.get("newText")
                .and_then(Value::as_str)
                .ok_or_else(|| "CompletionItem textEdit is missing newText".to_string())?,
        );
        if let Some(range) = edit.get("range") {
            return Ok(json!({
                "range": range,
                "newText": new_text,
            }));
        }
        if let Some(range) = edit.get("replace").or_else(|| edit.get("insert")) {
            return Ok(json!({
                "range": range,
                "newText": new_text,
            }));
        }
        Err("CompletionItem textEdit is missing range/replace/insert".to_string())
    }

    fn completion_item_to_workspace_edit(&self, file: &str, item: &Value) -> Result<Value, String> {
        let (_, uri) = self.ensure_lsp_file_ready(file)?;
        let mut edits = Vec::new();
        if let Some(text_edit) = item.get("textEdit") {
            edits.push(Self::normalize_completion_text_edit(text_edit)?);
        }
        if let Some(additional) = item.get("additionalTextEdits") {
            let additional = additional
                .as_array()
                .ok_or_else(|| "CompletionItem additionalTextEdits must be an array".to_string())?;
            edits.extend(
                additional
                    .iter()
                    .map(Self::normalize_completion_text_edit)
                    .collect::<Result<Vec<_>, _>>()?,
            );
        }
        if edits.is_empty() {
            return Err(
                "selected completion item does not include applyable text edits, even after resolve"
                    .to_string(),
            );
        }
        Self::lsp_text_edits_to_workspace_edit(&uri, Value::Array(edits))
    }

    fn try_resolve_code_lens(&self, file: &str, lens: &Value) -> Result<Option<Value>, String> {
        let file_path = self.resolve_lsp_file_path(file);
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "codeLens/resolve",
            lens.clone(),
        )
    }

    fn lsp_range_contains_position(range: &Value, line: usize, column: usize) -> bool {
        if line == 0 || column == 0 {
            return false;
        }
        let line = line.saturating_sub(1) as u64;
        let column = column.saturating_sub(1) as u64;
        let start_line = range
            .get("start")
            .and_then(|v| v.get("line"))
            .and_then(Value::as_u64);
        let start_character = range
            .get("start")
            .and_then(|v| v.get("character"))
            .and_then(Value::as_u64);
        let end_line = range
            .get("end")
            .and_then(|v| v.get("line"))
            .and_then(Value::as_u64);
        let end_character = range
            .get("end")
            .and_then(|v| v.get("character"))
            .and_then(Value::as_u64);
        match (start_line, start_character, end_line, end_character) {
            (Some(sl), Some(sc), Some(el), Some(ec)) => {
                (sl, sc) <= (line, column) && (line, column) < (el, ec)
            }
            _ => false,
        }
    }

    fn try_active_color_presentations(
        &self,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<Option<Value>, String> {
        let Some(colors) = self.try_active_document_colors(file)? else {
            return Ok(None);
        };
        let Some(color_entry) = colors.as_array().and_then(|items| {
            items.iter().find(|item| {
                item.get("range")
                    .map(|range| Self::lsp_range_contains_position(range, line, column))
                    .unwrap_or(false)
            })
        }) else {
            return Err("No document color found at the requested position".to_string());
        };
        let color = color_entry
            .get("color")
            .cloned()
            .ok_or_else(|| "documentColor entry missing color".to_string())?;
        let range = color_entry
            .get("range")
            .cloned()
            .ok_or_else(|| "documentColor entry missing range".to_string())?;
        let (file_path, uri) = self.ensure_lsp_file_ready(file)?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/colorPresentation",
            json!({
                "textDocument": { "uri": uri },
                "color": color,
                "range": range,
            }),
        )
    }

    fn try_active_selection_ranges(
        &self,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<Option<Value>, String> {
        let (file_path, position_params) = self.lsp_position_params(file, line, column)?;
        let uri = position_params
            .get("textDocument")
            .and_then(|doc| doc.get("uri"))
            .and_then(Value::as_str)
            .ok_or_else(|| "failed to build textDocument URI for selection_ranges".to_string())?
            .to_string();
        let position = position_params
            .get("position")
            .cloned()
            .ok_or_else(|| "failed to build LSP position for selection_ranges".to_string())?;
        self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/selectionRange",
            json!({
                "textDocument": { "uri": uri },
                "positions": [position],
            }),
        )
    }

    fn try_active_call_hierarchy(
        &self,
        operation: &str,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<Option<String>, String> {
        let (file_path, prepare_params) = self.lsp_position_params(file, line, column)?;
        let prepared = self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/prepareCallHierarchy",
            prepare_params,
        )?;
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        let Some(item) = prepared
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .or_else(|| prepared.as_object().map(|_| prepared.clone()))
        else {
            return Ok(Some(Self::active_lsp_response(
                operation,
                "textDocument/prepareCallHierarchy",
                prepared,
            )));
        };
        let method = match operation {
            "incoming_calls" => "callHierarchy/incomingCalls",
            _ => "callHierarchy/outgoingCalls",
        };
        self.passive_lsp
            .request_for_file(
                &self.project_root,
                &file_path,
                method,
                json!({ "item": item }),
            )
            .map(|result| result.map(|value| Self::active_lsp_response(operation, method, value)))
    }

    fn try_active_type_hierarchy(
        &self,
        operation: &str,
        file: &str,
        line: usize,
        column: usize,
    ) -> Result<Option<String>, String> {
        let (file_path, prepare_params) = self.lsp_position_params(file, line, column)?;
        let prepared = self.passive_lsp.request_for_file(
            &self.project_root,
            &file_path,
            "textDocument/prepareTypeHierarchy",
            prepare_params,
        )?;
        let Some(prepared) = prepared else {
            return Ok(None);
        };
        let Some(item) = prepared
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .or_else(|| prepared.as_object().map(|_| prepared.clone()))
        else {
            return Ok(Some(Self::active_lsp_response(
                operation,
                "textDocument/prepareTypeHierarchy",
                prepared,
            )));
        };
        let method = match operation {
            "supertypes" => "typeHierarchy/supertypes",
            _ => "typeHierarchy/subtypes",
        };
        self.passive_lsp
            .request_for_file(
                &self.project_root,
                &file_path,
                method,
                json!({ "item": item }),
            )
            .map(|result| result.map(|value| Self::active_lsp_response(operation, method, value)))
    }

    /// Unified LSP tool providing code intelligence operations.
    /// Prefers a real stdio LSP backend when one is available for the workspace/file,
    /// then falls back to the existing symbol/AST-based implementations.
    pub(crate) fn lsp(&self, args: &Value) -> String {
        let operation = match args.get("operation").and_then(Value::as_str) {
            Some(op) => op,
            None => return json!({
                "error": "Missing required 'operation' parameter",
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "declaration", "type_definition", "implementation", "supertypes", "subtypes", "prepare_rename", "rename", "code_actions", "completions", "signature_help", "document_highlight", "document_links", "inlay_hints", "folding_ranges", "document_colors", "color_presentations", "semantic_tokens", "code_lenses", "selection_ranges", "linked_editing_range", "format_document", "format_range", "format_on_type", "diagnostics"
                ]
            }).to_string(),
        };

        let file = args.get("file").and_then(Value::as_str);
        let line = args.get("line").and_then(Value::as_i64).map(|l| l as usize);
        let column = args
            .get("column")
            .and_then(Value::as_i64)
            .map(|c| c as usize);
        let end_line = args
            .get("end_line")
            .and_then(Value::as_i64)
            .map(|l| l as usize);
        let end_column = args
            .get("end_column")
            .and_then(Value::as_i64)
            .map(|c| c as usize);
        let symbol = args.get("symbol").and_then(Value::as_str);
        let query = args.get("query").and_then(Value::as_str);
        let new_name = args.get("new_name").and_then(Value::as_str);
        let trigger_character = args.get("trigger_character").and_then(Value::as_str);
        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);
        let action_index = args
            .get("action_index")
            .and_then(Value::as_u64)
            .map(|idx| idx as usize)
            .unwrap_or(0);
        let item_index = args
            .get("item_index")
            .and_then(Value::as_u64)
            .map(|idx| idx as usize);
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("file");
        let include_body = args
            .get("include_body")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match operation {
            "goto_definition" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        operation,
                        f,
                        l,
                        c,
                        "textDocument/definition",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => self.find_definition_at_position(f, l, c),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let Some(sym) = symbol {
                    self.find_definition(&json!({
                        "symbol": sym,
                        "file": file
                    }))
                } else {
                    json!({
                        "error": "goto_definition requires 'symbol' or 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "find_references" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        operation,
                        f,
                        l,
                        c,
                        "textDocument/references",
                        Some(json!({ "context": { "includeDeclaration": true } })),
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "find_references requires 'symbol' when no active LSP backend is available"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let Some(sym) = symbol {
                    self.find_references(&json!({
                        "symbol": sym,
                        "path": file,
                        "kind": "all",
                        "validate": true
                    }))
                } else {
                    json!({
                        "error": "find_references requires 'symbol' parameter"
                    }).to_string()
                }
            }

            "declaration" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        "declaration",
                        f,
                        l,
                        c,
                        "textDocument/declaration",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "declaration requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "declaration requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "type_definition" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        "type_definition",
                        f,
                        l,
                        c,
                        "textDocument/typeDefinition",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "type_definition requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "type_definition requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "implementation" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        operation,
                        f,
                        l,
                        c,
                        "textDocument/implementation",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => {
                            if let Some(sym) = symbol {
                                self.type_hierarchy(&json!({
                                    "name": sym,
                                    "direction": "implementations"
                                }))
                            } else {
                                json!({
                                    "error": "implementation requires an active LSP backend for position-based lookup, or 'symbol' for fallback type_hierarchy behavior"
                                }).to_string()
                            }
                        }
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let Some(sym) = symbol {
                    self.type_hierarchy(&json!({
                        "name": sym,
                        "direction": "implementations"
                    }))
                } else {
                    json!({
                        "error": "implementation requires either 'file'+'line'+'column' or 'symbol'"
                    }).to_string()
                }
            }

            "prepare_rename" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        "prepare_rename",
                        f,
                        l,
                        c,
                        "textDocument/prepareRename",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "prepare_rename requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "prepare_rename requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "hover" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        operation,
                        f,
                        l,
                        c,
                        "textDocument/hover",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => self.hover_info(&json!({
                            "file": f,
                            "line": l,
                            "column": c
                        })),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let (Some(f), Some(sym)) = (file, symbol) {
                    self.hover_info(&json!({
                        "file": f,
                        "symbol": sym
                    }))
                } else {
                    json!({
                        "error": "hover requires 'file' + ('line'+'column' or 'symbol')"
                    }).to_string()
                }
            }

            "document_symbols" => {
                if let Some(f) = file {
                    let _ = include_body;
                    match self.try_active_file_request(
                        operation,
                        f,
                        "textDocument/documentSymbol",
                        match self.ensure_lsp_file_ready(f) {
                            Ok((_, uri)) => json!({ "textDocument": { "uri": uri } }),
                            Err(error) => return json!({ "error": error }).to_string(),
                        },
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => self.symbols(&json!({
                            "path": f,
                            "include_body": include_body
                        })),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "document_symbols requires 'file' parameter"
                    }).to_string()
                }
            }

            "document_links" => {
                if let Some(f) = file {
                    match self.try_active_file_request(
                        "document_links",
                        f,
                        "textDocument/documentLink",
                        match self.ensure_lsp_file_ready(f) {
                            Ok((_, uri)) => json!({ "textDocument": { "uri": uri } }),
                            Err(error) => return json!({ "error": error }).to_string(),
                        },
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "document_links requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "document_links requires 'file' parameter"
                    }).to_string()
                }
            }

            "workspace_symbols" => {
                let search_query = query.or(symbol).unwrap_or("");
                match self.try_active_workspace_symbols(search_query) {
                    Ok(Some(result)) => result,
                    Ok(None) => self.symbol_search(&json!({
                        "query": search_query,
                        "limit": 50
                    })),
                    Err(error) => json!({ "error": error }).to_string(),
                }
            }

            "call_hierarchy" | "outgoing_calls" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_call_hierarchy(operation, f, l, c) {
                        Ok(Some(result)) => result,
                        Ok(None) => self.call_graph(&json!({
                            "path": f,
                            "symbol": symbol,
                            "start_line": Some(l),
                            "callers": false,
                            "scope": scope
                        })),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let Some(f) = file {
                    self.call_graph(&json!({
                        "path": f,
                        "symbol": symbol,
                        "start_line": line,
                        "callers": false,
                        "scope": scope
                    }))
                } else {
                    json!({
                        "error": "call_hierarchy/outgoing_calls requires 'file' parameter"
                    }).to_string()
                }
            }

            "incoming_calls" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_call_hierarchy(operation, f, l, c) {
                        Ok(Some(result)) => result,
                        Ok(None) => self.call_graph(&json!({
                            "path": f,
                            "symbol": symbol,
                            "start_line": Some(l),
                            "callers": true,
                            "scope": scope
                        })),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let Some(f) = file {
                    self.call_graph(&json!({
                        "path": f,
                        "symbol": symbol,
                        "start_line": line,
                        "callers": true,
                        "scope": scope
                    }))
                } else {
                    json!({
                        "error": "incoming_calls requires 'file' parameter"
                    }).to_string()
                }
            }

            "supertypes" | "subtypes" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_type_hierarchy(operation, f, l, c) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": format!(
                                "{} requires an active LSP backend for that file",
                                operation
                            )
                        })
                        .to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": format!("{} requires 'file'+'line'+'column'", operation)
                    })
                    .to_string()
                }
            }

            "rename" => {
                let Some(next_name) = new_name.filter(|name| !name.is_empty()) else {
                    return json!({
                        "error": "rename requires 'new_name'"
                    }).to_string();
                };
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_rename_workspace_edit(f, l, c, next_name) {
                        Ok(Some(result)) if dry_run => {
                            Self::active_lsp_response(operation, "textDocument/rename", result)
                        }
                        Ok(Some(result)) => {
                            match self.apply_lsp_workspace_edit(
                                "rename",
                                "textDocument/rename",
                                &result,
                            ) {
                                Ok(applied) => applied,
                                Err(error) => json!({ "error": error }).to_string(),
                            }
                        }
                        Ok(None) => {
                            if let Some(sym) = symbol {
                                self.rename_symbol(&json!({
                                    "symbol": sym,
                                    "new_name": next_name,
                                    "dry_run": dry_run
                                }))
                            } else {
                                json!({
                                    "error": "rename requires an active LSP backend for position-based preview, or 'symbol' for fallback rename_symbol behavior"
                                }).to_string()
                            }
                        }
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else if let Some(sym) = symbol {
                    self.rename_symbol(&json!({
                        "symbol": sym,
                        "new_name": next_name,
                        "dry_run": dry_run
                    }))
                } else {
                    json!({
                        "error": "rename requires either 'file'+'line'+'column'+'new_name' or 'symbol'+'new_name'"
                    }).to_string()
                }
            }

            "code_actions" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_code_actions(f, l, c) {
                        Ok(Some(result)) if dry_run => {
                            Self::active_lsp_response(
                                "code_actions",
                                "textDocument/codeAction",
                                result,
                            )
                        }
                        Ok(Some(result)) => {
                            let Some(actions) = result.as_array() else {
                                return json!({
                                    "error": "code_actions returned a non-array result from the active LSP backend"
                                }).to_string();
                            };
                            let Some(action) = actions.get(action_index) else {
                                return json!({
                                    "error": format!(
                                        "code_actions action_index {} out of range ({} actions)",
                                        action_index,
                                        actions.len()
                                    )
                                }).to_string();
                            };
                            let resolved_action = if action.get("edit").is_none()
                                && action.get("command").is_none()
                            {
                                match self.try_resolve_code_action(f, action) {
                                    Ok(Some(resolved)) => resolved,
                                    Ok(None) => action.clone(),
                                    Err(error) => return json!({ "error": error }).to_string(),
                                }
                            } else {
                                action.clone()
                            };
                            if let Some(workspace_edit) = resolved_action.get("edit") {
                                match self.apply_lsp_workspace_edit(
                                    "code_actions",
                                    "textDocument/codeAction",
                                    workspace_edit,
                                ) {
                                    Ok(applied) => applied,
                                    Err(error) => json!({ "error": error }).to_string(),
                                }
                            } else if let Some(command) = resolved_action.get("command") {
                                match self.execute_lsp_command("code_actions", command) {
                                    Ok(executed) => executed,
                                    Err(error) => json!({ "error": error }).to_string(),
                                }
                            } else {
                                json!({
                                    "error": "selected code action does not include an editable WorkspaceEdit or executable command, even after resolve"
                                }).to_string()
                            }
                        }
                        Ok(None) => json!({
                            "error": "code_actions requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "code_actions requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "completions" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    if !dry_run && item_index.is_none() {
                        return json!({
                            "error": "completions apply requires 'item_index' to choose a returned completion item"
                        }).to_string();
                    }
                    match self.try_active_completions(f, l, c) {
                        Ok(Some(result)) => {
                            if let Some(idx) = item_index {
                                let Some(item) = result
                                    .get("items")
                                    .and_then(Value::as_array)
                                    .or_else(|| result.as_array())
                                    .and_then(|items| items.get(idx))
                                else {
                                    return json!({
                                        "error": format!(
                                            "completions item_index {} out of range",
                                            idx
                                        )
                                    })
                                    .to_string();
                                };
                                let (method, selected_item) =
                                    match self.try_resolve_completion_item(f, item) {
                                        Ok(Some(resolved)) => {
                                            ("completionItem/resolve", resolved)
                                        }
                                        Ok(None) => ("textDocument/completion", item.clone()),
                                        Err(error) => {
                                            return json!({ "error": error }).to_string();
                                        }
                                    };
                                if dry_run {
                                    json!({
                                        "backend": "lsp",
                                        "operation": "completions",
                                        "method": method,
                                        "selected_index": idx,
                                        "result": selected_item,
                                    })
                                    .to_string()
                                } else {
                                    match self.completion_item_to_workspace_edit(f, &selected_item)
                                    {
                                        Ok(workspace_edit) => match self.apply_lsp_workspace_edit(
                                            "completions",
                                            method,
                                            &workspace_edit,
                                        ) {
                                            Ok(applied) => applied,
                                            Err(error) => json!({ "error": error }).to_string(),
                                        },
                                        Err(error) => json!({ "error": error }).to_string(),
                                    }
                                }
                            } else {
                                Self::active_lsp_response(
                                    "completions",
                                    "textDocument/completion",
                                    result,
                                )
                            }
                        }
                        Ok(None) => json!({
                            "error": "completions requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "completions requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "signature_help" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        "signature_help",
                        f,
                        l,
                        c,
                        "textDocument/signatureHelp",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "signature_help requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "signature_help requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "document_highlight" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        "document_highlight",
                        f,
                        l,
                        c,
                        "textDocument/documentHighlight",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "document_highlight requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "document_highlight requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "inlay_hints" => {
                if let Some(f) = file {
                    let range = match (line, column, end_line, end_column) {
                        (Some(l), Some(c), Some(end_l), Some(end_c)) => Some((l, c, end_l, end_c)),
                        (None, None, None, None) => None,
                        _ => {
                            return json!({
                                "error": "inlay_hints accepts either 'file' alone for the whole document or 'file'+'line'+'column'+'end_line'+'end_column' for a sub-range"
                            }).to_string();
                        }
                    };
                    match self.try_active_inlay_hints(f, range) {
                        Ok(Some(result)) => {
                            Self::active_lsp_response("inlay_hints", "textDocument/inlayHint", result)
                        }
                        Ok(None) => json!({
                            "error": "inlay_hints requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "inlay_hints requires 'file'"
                    }).to_string()
                }
            }

            "folding_ranges" => {
                if let Some(f) = file {
                    match self.try_active_folding_ranges(f) {
                        Ok(Some(result)) => Self::active_lsp_response(
                            "folding_ranges",
                            "textDocument/foldingRange",
                            result,
                        ),
                        Ok(None) => json!({
                            "error": "folding_ranges requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "folding_ranges requires 'file'"
                    }).to_string()
                }
            }

            "document_colors" => {
                if let Some(f) = file {
                    match self.try_active_document_colors(f) {
                        Ok(Some(result)) => Self::active_lsp_response(
                            "document_colors",
                            "textDocument/documentColor",
                            result,
                        ),
                        Ok(None) => json!({
                            "error": "document_colors requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "document_colors requires 'file'"
                    }).to_string()
                }
            }

            "color_presentations" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_color_presentations(f, l, c) {
                        Ok(Some(result)) => Self::active_lsp_response(
                            "color_presentations",
                            "textDocument/colorPresentation",
                            result,
                        ),
                        Ok(None) => json!({
                            "error": "color_presentations requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "color_presentations requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "semantic_tokens" => {
                if let Some(f) = file {
                    match self.try_active_semantic_tokens(f) {
                        Ok(Some(result)) => Self::active_lsp_response(
                            "semantic_tokens",
                            "textDocument/semanticTokens/full",
                            result,
                        ),
                        Ok(None) => json!({
                            "error": "semantic_tokens requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "semantic_tokens requires 'file'"
                    }).to_string()
                }
            }

            "code_lenses" => {
                if let Some(f) = file {
                    if !dry_run && item_index.is_none() {
                        return json!({
                            "error": "code_lenses execution requires 'item_index' to choose a returned code lens"
                        }).to_string();
                    }
                    match self.try_active_code_lenses_with_fallback(f) {
                        Ok(Some((preview_method, result))) => {
                            if let Some(idx) = item_index {
                                let Some(lens) = result.as_array().and_then(|items| items.get(idx))
                                else {
                                    return json!({
                                        "error": format!(
                                            "code_lenses item_index {} out of range",
                                            idx
                                        )
                                    })
                                    .to_string();
                                };
                                let (method, selected_lens) = if preview_method
                                    == "textDocument/codeLens"
                                    && lens.get("command").is_none()
                                {
                                    match self.try_resolve_code_lens(f, lens) {
                                        Ok(Some(resolved)) => ("codeLens/resolve", resolved),
                                        Ok(None) => ("textDocument/codeLens", lens.clone()),
                                        Err(error) => {
                                            return json!({ "error": error }).to_string();
                                        }
                                    }
                                } else {
                                    (preview_method, lens.clone())
                                };
                                if dry_run {
                                    let mut response = json!({
                                        "backend": "lsp",
                                        "operation": "code_lenses",
                                        "method": method,
                                        "selected_index": idx,
                                        "result": selected_lens,
                                    });
                                    if preview_method == "experimental/runnables"
                                        && let Some(root) = response.as_object_mut()
                                    {
                                        root.insert(
                                            "fallback_from".to_string(),
                                            Value::String("textDocument/codeLens".to_string()),
                                        );
                                    }
                                    response.to_string()
                                } else if let Some(command) = selected_lens.get("command") {
                                    match self.execute_lsp_command("code_lenses", command) {
                                        Ok(executed) => executed,
                                        Err(error) => json!({ "error": error }).to_string(),
                                    }
                                } else {
                                    json!({
                                        "error": "selected code lens does not include an executable command, even after resolve"
                                    }).to_string()
                                }
                            } else {
                                let mut response =
                                    json!({
                                        "backend": "lsp",
                                        "operation": "code_lenses",
                                        "method": preview_method,
                                        "result": result,
                                    });
                                if preview_method == "experimental/runnables"
                                    && let Some(root) = response.as_object_mut()
                                {
                                    root.insert(
                                        "fallback_from".to_string(),
                                        Value::String("textDocument/codeLens".to_string()),
                                    );
                                }
                                response.to_string()
                            }
                        }
                        Ok(None) => json!({
                            "error": "code_lenses requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "code_lenses requires 'file'"
                    }).to_string()
                }
            }

            "selection_ranges" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_selection_ranges(f, l, c) {
                        Ok(Some(result)) => Self::active_lsp_response(
                            "selection_ranges",
                            "textDocument/selectionRange",
                            result,
                        ),
                        Ok(None) => json!({
                            "error": "selection_ranges requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "selection_ranges requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "linked_editing_range" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    match self.try_active_position_request(
                        "linked_editing_range",
                        f,
                        l,
                        c,
                        "textDocument/linkedEditingRange",
                        None,
                    ) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "linked_editing_range requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "linked_editing_range requires 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "format_document" => {
                if let Some(f) = file {
                    match self.try_active_document_formatting(f) {
                        Ok(Some(result)) if dry_run => {
                            Self::active_lsp_response(
                                "format_document",
                                "textDocument/formatting",
                                result,
                            )
                        }
                        Ok(Some(result)) => match self.ensure_lsp_file_ready(f) {
                            Ok((_, uri)) => match Self::lsp_text_edits_to_workspace_edit(&uri, result)
                            {
                                Ok(workspace_edit) => self.apply_lsp_workspace_edit(
                                    "format_document",
                                    "textDocument/formatting",
                                    &workspace_edit,
                                ),
                                Err(error) => Err(error),
                            }
                            .unwrap_or_else(|error| json!({ "error": error }).to_string()),
                            Err(error) => json!({ "error": error }).to_string(),
                        },
                        Ok(None) => json!({
                            "error": "format_document requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "format_document requires 'file'"
                    }).to_string()
                }
            }

            "format_range" => {
                if let (Some(f), Some(l), Some(c), Some(end_l), Some(end_c)) =
                    (file, line, column, end_line, end_column)
                {
                    match self.try_active_range_formatting(f, l, c, end_l, end_c) {
                        Ok(Some(result)) if dry_run => {
                            Self::active_lsp_response(
                                "format_range",
                                "textDocument/rangeFormatting",
                                result,
                            )
                        }
                        Ok(Some(result)) => match self.ensure_lsp_file_ready(f) {
                            Ok((_, uri)) => {
                                match Self::lsp_text_edits_to_workspace_edit(&uri, result) {
                                    Ok(workspace_edit) => self.apply_lsp_workspace_edit(
                                        "format_range",
                                        "textDocument/rangeFormatting",
                                        &workspace_edit,
                                    ),
                                    Err(error) => Err(error),
                                }
                                .unwrap_or_else(|error| json!({ "error": error }).to_string())
                            }
                            Err(error) => json!({ "error": error }).to_string(),
                        },
                        Ok(None) => json!({
                            "error": "format_range requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "format_range requires 'file'+'line'+'column'+'end_line'+'end_column'"
                    }).to_string()
                }
            }

            "format_on_type" => {
                if let (Some(f), Some(l), Some(c), Some(ch)) =
                    (file, line, column, trigger_character)
                {
                    match self.try_active_on_type_formatting(f, l, c, ch) {
                        Ok(Some(result)) if dry_run => Self::active_lsp_response(
                            "format_on_type",
                            "textDocument/onTypeFormatting",
                            result,
                        ),
                        Ok(Some(result)) => match self.ensure_lsp_file_ready(f) {
                            Ok((_, uri)) => {
                                match Self::lsp_text_edits_to_workspace_edit(&uri, result) {
                                    Ok(workspace_edit) => self.apply_lsp_workspace_edit(
                                        "format_on_type",
                                        "textDocument/onTypeFormatting",
                                        &workspace_edit,
                                    ),
                                    Err(error) => Err(error),
                                }
                                .unwrap_or_else(|error| json!({ "error": error }).to_string())
                            }
                            Err(error) => json!({ "error": error }).to_string(),
                        },
                        Ok(None) => json!({
                            "error": "format_on_type requires an active LSP backend for that file"
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "error": "format_on_type requires 'file'+'line'+'column'+'trigger_character'"
                    }).to_string()
                }
            }

            "diagnostics" => {
                if let Some(f) = file {
                    match self.try_active_file_diagnostics(f) {
                        Ok(Some(result)) => result,
                        Ok(None) => json!({
                            "error": "diagnostics for a specific file require an active LSP backend for that workspace",
                            "active_backends": self.passive_lsp.active_status(&self.project_root),
                        }).to_string(),
                        Err(error) => json!({ "error": error }).to_string(),
                    }
                } else {
                    json!({
                        "capabilities": {
                            "goto_definition": true,
                            "find_references": true,
                            "hover": true,
                            "document_symbols": true,
                            "workspace_symbols": true,
                            "call_hierarchy": true,
                            "declaration": true,
                            "type_definition": true,
                            "implementation": true,
                            "supertypes": true,
                            "subtypes": true,
                            "prepare_rename": true,
                            "rename": true,
                            "code_actions": true,
                            "completions": true,
                            "signature_help": true,
                            "document_highlight": true,
                            "document_links": true,
                            "inlay_hints": true,
                            "folding_ranges": true,
                            "document_colors": true,
                            "color_presentations": true,
                            "semantic_tokens": true,
                            "code_lenses": true,
                            "selection_ranges": true,
                            "linked_editing_range": true,
                            "format_document": true,
                            "format_range": true,
                            "format_on_type": true
                        },
                        "recommended_operations": [
                            "goto_definition", "find_references", "hover", "document_symbols",
                            "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls",
                            "declaration", "type_definition", "implementation", "supertypes", "subtypes",
                            "prepare_rename", "rename", "code_actions", "completions", "signature_help",
                            "code_lenses", "format_document", "format_range", "format_on_type", "diagnostics"
                        ],
                        "advanced_editor_operations": [
                            "document_highlight", "document_links", "inlay_hints", "folding_ranges",
                            "document_colors", "color_presentations", "semantic_tokens", "selection_ranges",
                            "linked_editing_range"
                        ],
                        "active_backends": self.passive_lsp.active_status(&self.project_root),
                        "supported_languages": {
                            "active_lsp": ["rust", "typescript", "typescriptreact"],
                            "fallback_tools": ["rust", "python", "typescript", "javascript", "go", "java", "c", "cpp", "ruby"]
                        },
                        "note": "Without a file, diagnostics reports backend availability plus recommended-vs-advanced LSP operations. With a file, diagnostics first tries textDocument/diagnostic and falls back to the latest publishDiagnostics snapshot after syncing that file into the active backend."
                    }).to_string()
                }
            }

            _ => json!({
                "error": format!("Unknown operation: {}", operation),
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "declaration", "type_definition", "implementation", "supertypes", "subtypes", "prepare_rename", "rename", "code_actions", "completions", "signature_help", "document_highlight", "document_links", "inlay_hints", "folding_ranges", "document_colors", "color_presentations", "semantic_tokens", "code_lenses", "selection_ranges", "linked_editing_range", "format_document", "format_range", "format_on_type", "diagnostics"
                ]
            }).to_string()
        }
    }

    /// Find definition at a specific file position by extracting the symbol under cursor.
    /// Column is interpreted as UTF-16 code units (LSP protocol).
    pub(super) fn find_definition_at_position(
        &self,
        file: &str,
        line: usize,
        col_utf16: usize,
    ) -> String {
        // Read the file and extract symbol at position
        let file_path = if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            self.project_root.join(file)
        };

        // Check file size to prevent OOM
        if let Ok(metadata) = std::fs::metadata(&file_path) {
            if metadata.len() > MAX_LSP_FILE_SIZE as u64 {
                return json!({
                    "error": format!("File too large for LSP operations ({} bytes, max {} bytes)",
                        metadata.len(), MAX_LSP_FILE_SIZE)
                })
                .to_string();
            }
        }

        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) => {
                return json!({
                    "error": format!("Failed to read file: {}", e)
                })
                .to_string();
            }
        };

        // Get the line
        let lines: Vec<&str> = content.lines().collect();
        if line == 0 || line > lines.len() {
            return json!({
                "error": format!("Line {} out of range (file has {} lines)", line, lines.len())
            })
            .to_string();
        }

        let line_content = lines[line - 1];

        // Convert UTF-16 column to char index (LSP uses UTF-16 code units).
        // col_utf16 is 0-indexed per tool schema, same as utf16_col_to_char_idx expects.
        let col_idx = utf16_col_to_char_idx(line_content, col_utf16);
        let chars: Vec<char> = line_content.chars().collect();

        if col_idx >= chars.len() {
            return json!({
                "error": format!("Column {} (UTF-16) out of range for line {} (length {})",
                    col_utf16, line, line_content.len())
            })
            .to_string();
        }

        // Find word boundaries
        let mut start = col_idx;
        while start > 0 && Self::is_symbol_char(chars.get(start - 1).copied().unwrap_or(' ')) {
            start -= 1;
        }

        let mut end = col_idx;
        while end < chars.len() && Self::is_symbol_char(chars.get(end).copied().unwrap_or(' ')) {
            end += 1;
        }

        if start == end {
            return json!({
                "error": "No symbol found at position"
            })
            .to_string();
        }

        let symbol: String = chars[start..end].iter().collect();

        self.find_definition(&json!({
            "symbol": symbol,
            "file": file
        }))
    }

    /// Check if a character can be part of a symbol name.
    fn is_symbol_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
}
