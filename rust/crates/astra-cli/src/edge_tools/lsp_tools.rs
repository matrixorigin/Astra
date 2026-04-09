//! LSP tool: unified language server interface for code intelligence.
//! Also includes find_definition_at_position for LSP-based goto-definition.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::lsp_stdio_session::path_to_uri;
use super::{MAX_LSP_FILE_SIZE, ToolExecutor, utf16_col_to_char_idx};

impl ToolExecutor {
    // ─── LSP tool: unified language server interface ─────────────────────────────

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
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "rename", "diagnostics"
                ]
            }).to_string(),
        };

        let file = args.get("file").and_then(Value::as_str);
        let line = args.get("line").and_then(Value::as_i64).map(|l| l as usize);
        let column = args
            .get("column")
            .and_then(Value::as_i64)
            .map(|c| c as usize);
        let symbol = args.get("symbol").and_then(Value::as_str);
        let query = args.get("query").and_then(Value::as_str);
        let new_name = args.get("new_name").and_then(Value::as_str);
        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);
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
                    if !dry_run && symbol.is_none() {
                        return json!({
                            "error": "position-based rename currently applies as a preview first. For an immediate apply fallback, also provide 'symbol' with dry_run=false."
                        }).to_string();
                    }
                    match self.try_active_position_request(
                        operation,
                        f,
                        l,
                        c,
                        "textDocument/rename",
                        Some(json!({ "newName": next_name })),
                    ) {
                        Ok(Some(result)) => result,
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

            "diagnostics" => {
                json!({
                    "capabilities": {
                        "goto_definition": true,
                        "find_references": true,
                        "hover": true,
                        "document_symbols": true,
                        "workspace_symbols": true,
                        "call_hierarchy": true,
                        "rename": true
                    },
                    "active_backends": self.passive_lsp.active_status(&self.project_root),
                    "supported_languages": {
                        "active_lsp": ["rust", "typescript", "typescriptreact"],
                        "fallback_tools": ["rust", "python", "typescript", "javascript", "go", "java", "c", "cpp", "ruby"]
                    },
                    "note": "The lsp tool now prefers a real stdio LSP backend when a matching workspace/server is available, and falls back to the existing AST/symbol tools otherwise."
                }).to_string()
            }

            _ => json!({
                "error": format!("Unknown operation: {}", operation),
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "rename", "diagnostics"
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
