//! Notebook edit tool: Jupyter notebook cell editing operations.


use serde_json::{json, Value};

use super::ToolExecutor;

impl ToolExecutor {
    // ── Notebook edit tool: Jupyter notebook cell editing ─────────────────────

    /// Edit Jupyter notebook cells.
    /// Operations: replace, insert, delete
    pub(super) fn notebook_edit(&self, args: &Value) -> String {
        let notebook_path = match args.get("notebook_path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return json!({ "error": "Missing required parameter: notebook_path" }).to_string(),
        };

        let file_path = match self.resolve_checked(notebook_path) {
            Ok(path) => path,
            Err(e) => return json!({ "error": e }).to_string(),
        };
        
        // Validate file extension
        if !file_path.extension().map(|e| e == "ipynb").unwrap_or(false) {
            return json!({ 
                "error": "File must be a Jupyter notebook (.ipynb). For other files, use str_replace or write_file."
            }).to_string();
        }

        let edit_mode = args.get("edit_mode").and_then(|v| v.as_str()).unwrap_or("replace");
        if !matches!(edit_mode, "replace" | "insert" | "delete") {
            return json!({ "error": format!("Unknown edit_mode: {}. Use replace, insert, or delete", edit_mode) }).to_string();
        }

        let cell_id = args.get("cell_id").and_then(|v| v.as_str());
        let new_source = args.get("new_source").and_then(|v| v.as_str());
        let cell_type = args.get("cell_type").and_then(|v| v.as_str()).unwrap_or("code");

        let rel = file_path.strip_prefix(&self.project_root).unwrap_or(&file_path);
        let rel_str = rel.to_string_lossy();
        if let Some(warning) = super::fs_tools::is_dangerous_write_target(&rel_str) {
            return json!({
                "error": format!("⚠️ Warning: writing to sensitive file '{}' — {}. If intentional, use bash to bypass this guard.", rel_str, warning)
            }).to_string();
        }

        // Check staleness and read-before-write requirements only for existing files
        if file_path.exists() {
            if let Err(e) = self.check_staleness(&file_path) {
                return json!({ "error": e }).to_string();
            }
            if !self.was_fully_read(&file_path) {
                return json!({
                    "error": format!(
                        "File was only partially read (outline or line range). Read the full file before editing.\n\
                         → Action required: call read_file(\"{}\") (without start_line/end_line) first, then retry.",
                        rel_str
                    )
                }).to_string();
            }
        }
        
        // Read existing notebook
        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Create new notebook if it doesn't exist and we're inserting
                let edit_mode = args.get("edit_mode").and_then(|v| v.as_str()).unwrap_or("replace");
                if edit_mode != "insert" {
                    return json!({ "error": format!("Notebook not found: {}", file_path.display()) }).to_string();
                }
                // Create empty notebook structure
                r#"{"cells":[],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#.to_string()
            }
            Err(e) => return json!({ "error": format!("Failed to read notebook: {}", e) }).to_string(),
        };
        
        let mut notebook: Value = match serde_json::from_str(&content) {
            Ok(n) => n,
            Err(e) => return json!({ "error": format!("Invalid notebook JSON: {}", e) }).to_string(),
        };
        
        let cells = match notebook.get_mut("cells").and_then(|c| c.as_array_mut()) {
            Some(c) => c,
            None => return json!({ "error": "Notebook has no cells array" }).to_string(),
        };
        
        // Find cell index if cell_id provided
        let cell_index = if let Some(id) = cell_id {
            // Try to find by ID first
            let by_id = cells.iter().position(|c| {
                c.get("id").and_then(|i| i.as_str()) == Some(id)
            });
            if let Some(idx) = by_id {
                Some(idx)
            } else {
                // Try to parse as cell-N format
                if let Some(num_str) = id.strip_prefix("cell-") {
                    num_str.parse::<usize>().ok()
                } else {
                    id.parse::<usize>().ok()
                }
            }
        } else {
            None
        };
        
        match edit_mode {
            "delete" => {
                let idx = match cell_index {
                    Some(i) if i < cells.len() => i,
                    _ => return json!({ "error": "cell_id required for delete operation" }).to_string(),
                };
                cells.remove(idx);
            }
            "insert" => {
                let source = match new_source {
                    Some(s) => s,
                    None => return json!({ "error": "new_source required for insert operation" }).to_string(),
                };
                let new_cell = json!({
                    "cell_type": cell_type,
                    "id": format!("cell-{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                    "source": source,
                    "metadata": {},
                    "outputs": if cell_type == "code" { json!([]) } else { json!(null) },
                    "execution_count": if cell_type == "code" { json!(null) } else { json!(null) }
                });
                let insert_idx = cell_index.map(|i| i + 1).unwrap_or(0);
                if insert_idx <= cells.len() {
                    cells.insert(insert_idx, new_cell);
                } else {
                    cells.push(new_cell);
                }
            }
            "replace" => {
                let idx = match cell_index {
                    Some(i) if i < cells.len() => i,
                    _ => return json!({ "error": "Valid cell_id required for replace operation" }).to_string(),
                };
                let source = match new_source {
                    Some(s) => s,
                    None => return json!({ "error": "new_source required for replace operation" }).to_string(),
                };
                if let Some(cell) = cells.get_mut(idx) {
                    cell["source"] = json!(source);
                    if cell_type != cell.get("cell_type").and_then(|t| t.as_str()).unwrap_or("") {
                        cell["cell_type"] = json!(cell_type);
                    }
                    // Reset execution for code cells
                    if cell.get("cell_type").and_then(|t| t.as_str()) == Some("code") {
                        cell["execution_count"] = json!(null);
                        cell["outputs"] = json!([]);
                    }
                }
            }
            _ => return json!({ "error": format!("Unknown edit_mode: {}. Use replace, insert, or delete", edit_mode) }).to_string(),
        }
        
        // Get cell count before dropping mutable borrow
        let total_cells = cells.len();
        
        // Extract language before serializing (need to drop cells borrow first)
        let language = notebook
            .get("metadata")
            .and_then(|m| m.get("language_info"))
            .and_then(|l| l.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("python")
            .to_string();

        if file_path.exists() {
            if let Err(e) = self.check_staleness(&file_path) {
                return json!({ "error": format!("Pre-write staleness check failed: {e}") }).to_string();
            }
        }

        // Write back
        let updated_content = serde_json::to_string_pretty(&notebook).unwrap_or_default();
        if let Err(e) = std::fs::write(&file_path, &updated_content) {
            return json!({ "error": format!("Failed to write notebook: {}", e) }).to_string();
        }
        self.record_write(&file_path);
        
        json!({
            "success": true,
            "edit_mode": edit_mode,
            "cell_type": cell_type,
            "language": language,
            "total_cells": total_cells,
            "notebook_path": file_path.display().to_string()
        }).to_string()
    }

}
