//! LSP tool: unified language server interface for code intelligence.
//! Also includes find_definition_at_position for LSP-based goto-definition.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::{Value, json};
use url::Url;

use super::lsp_stdio_session::path_to_uri;
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
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| "WorkspaceEdit text edit is missing newText".to_string())?
            .to_string();
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
                    Self::active_lsp_response("diagnostics", "publishDiagnostics", value)
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

    /// Unified LSP tool providing code intelligence operations.
    /// Prefers a real stdio LSP backend when one is available for the workspace/file,
    /// then falls back to the existing symbol/AST-based implementations.
    pub(super) fn lsp(&self, args: &Value) -> String {
        let operation = match args.get("operation").and_then(Value::as_str) {
            Some(op) => op,
            None => return json!({
                "error": "Missing required 'operation' parameter",
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "declaration", "type_definition", "implementation", "rename", "code_actions", "completions", "signature_help", "document_highlight", "format_document", "format_range", "diagnostics"
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
        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);
        let action_index = args
            .get("action_index")
            .and_then(Value::as_u64)
            .map(|idx| idx as usize)
            .unwrap_or(0);
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
                            let Some(workspace_edit) = action.get("edit") else {
                                return json!({
                                    "error": "selected code action does not include an editable WorkspaceEdit"
                                }).to_string();
                            };
                            match self.apply_lsp_workspace_edit(
                                "code_actions",
                                "textDocument/codeAction",
                                workspace_edit,
                            ) {
                                Ok(applied) => applied,
                                Err(error) => json!({ "error": error }).to_string(),
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
                    match self.try_active_position_request(
                        "completions",
                        f,
                        l,
                        c,
                        "textDocument/completion",
                        None,
                    ) {
                        Ok(Some(result)) => result,
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
                            "rename": true,
                            "code_actions": true,
                            "completions": true,
                            "signature_help": true,
                            "document_highlight": true,
                            "format_document": true,
                            "format_range": true
                        },
                        "active_backends": self.passive_lsp.active_status(&self.project_root),
                        "supported_languages": {
                            "active_lsp": ["rust", "typescript", "typescriptreact"],
                            "fallback_tools": ["rust", "python", "typescript", "javascript", "go", "java", "c", "cpp", "ruby"]
                        },
                        "note": "Without a file, diagnostics reports LSP backend availability. With a file, it returns the latest publishDiagnostics snapshot after syncing that file into the active backend."
                    }).to_string()
                }
            }

            _ => json!({
                "error": format!("Unknown operation: {}", operation),
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "declaration", "type_definition", "implementation", "rename", "code_actions", "completions", "signature_help", "document_highlight", "format_document", "format_range", "diagnostics"
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
