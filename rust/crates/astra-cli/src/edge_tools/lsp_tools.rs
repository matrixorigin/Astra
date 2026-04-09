//! LSP tool: unified language server interface for code intelligence.
//! Also includes find_definition_at_position for LSP-based goto-definition.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::{MAX_LSP_FILE_SIZE, ToolExecutor, utf16_col_to_char_idx};

impl ToolExecutor {
    // ─── LSP tool: unified language server interface ─────────────────────────────

    /// Unified LSP tool providing code intelligence operations.
    /// Routes to existing implementations (find_definition, find_references, etc.)
    /// but offers a consistent interface matching the LSP protocol.
    pub(super) fn lsp(&self, args: &Value) -> String {
        let operation = match args.get("operation").and_then(Value::as_str) {
            Some(op) => op,
            None => return json!({
                "error": "Missing required 'operation' parameter",
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "diagnostics"
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
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("file");
        let include_body = args
            .get("include_body")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match operation {
            "goto_definition" => {
                // Requires either symbol or file+position
                if let Some(sym) = symbol {
                    self.find_definition(&json!({
                        "symbol": sym,
                        "file": file
                    }))
                } else if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    // For position-based definition lookup, we extract symbol at position
                    self.find_definition_at_position(f, l, c)
                } else {
                    json!({
                        "error": "goto_definition requires 'symbol' or 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "find_references" => {
                if let Some(sym) = symbol {
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
                    self.hover_info(&json!({
                        "file": f,
                        "line": l,
                        "column": c
                    }))
                } else if let (Some(f), Some(sym)) = (file, symbol) {
                    // Find symbol in file and get hover for it
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
                    self.symbols(&json!({
                        "path": f,
                        "include_body": include_body
                    }))
                } else {
                    json!({
                        "error": "document_symbols requires 'file' parameter"
                    }).to_string()
                }
            }

            "workspace_symbols" => {
                let search_query = query.or(symbol).unwrap_or("");
                self.symbol_search(&json!({
                    "query": search_query,
                    "limit": 50
                }))
            }

            "call_hierarchy" | "outgoing_calls" => {
                if let Some(f) = file {
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
                if let Some(f) = file {
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

            "diagnostics" => {
                // Return diagnostic information about LSP capabilities
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
                    "supported_languages": [
                        "rust", "python", "typescript", "javascript",
                        "go", "java", "c", "cpp", "ruby"
                    ],
                    "note": "Uses tree-sitter AST parsing for accurate results. Some features may have reduced accuracy for unsupported languages."
                }).to_string()
            }

            _ => json!({
                "error": format!("Unknown operation: {}", operation),
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "diagnostics"
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
