use super::*;

impl ToolExecutor {
    /// Resolve a tool-provided path, enforcing sandbox boundary when active.
    pub(crate) fn resolve(&self, path: &str) -> PathBuf {
        let p = Path::new(path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_root.join(p)
        };

        // If sandbox is active (non-Permissive), validate the path
        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
        {
            match validate_path(policy, path) {
                Ok(safe) => return safe,
                Err(_) => return resolved, // let the caller handle the error naturally
            }
        }
        resolved
    }

    /// Resolve path with explicit error when sandbox blocks it.
    pub(crate) fn resolve_checked(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_root.join(p)
        };

        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
        {
            return validate_path(policy, path).map_err(|e| format!("Sandbox: {e}"));
        }
        Ok(resolved)
    }

    pub(crate) fn read_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'path'".to_string(),
        };
        let path = match self.resolve_checked(path_str) {
            Ok(safe) => safe,
            Err(e) => return e,
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Error: {e}");
                if e.kind() == std::io::ErrorKind::NotFound {
                    // Try to find similar files
                    let suggestions = self.find_similar_files(path_str);
                    let hint = if !suggestions.is_empty() {
                        format!("\nDid you mean: {}?", suggestions.join(", "))
                    } else {
                        String::new()
                    };
                    return format!("{msg}. Use list_dir or glob to find the correct path first.{hint}");
                }
                if e.kind() == std::io::ErrorKind::IsADirectory {
                    return format!("{msg}. Use list_dir instead for directories.");
                }
                return msg;
            }
        };

        // Outline mode: return only definition signatures with line numbers
        // Uses tree-sitter for supported languages (Rust, Python, TypeScript, Go)
        // Falls back to regex-based detection for others
        if args
            .get("outline")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let total_lines = content.lines().count();

            // Try tree-sitter first for accurate AST-based extraction
            if let Some(ts_lang) = super::code_intel::detect_language(&path) {
                let outline = super::code_intel::generate_outline(&content, ts_lang);
                if !outline.is_empty() {
                    let def_count = outline.lines().count();
                    return format!(
                        "# Outline ({total_lines} lines, {def_count} symbols)\n{}",
                        outline
                    );
                }
            }

            // Fall back to regex-based detection
            let lang = detect_language(ext);
            let outline = extract_outline(&content, lang);
            if outline.is_empty() {
                return format!("(no definitions found in {total_lines}-line file)");
            }
            return format!(
                "# Outline ({total_lines} lines total, {} definitions)\n{}",
                outline.len(),
                outline
                    .iter()
                    .map(|(line_no, sig)| format!("{line_no}: {sig}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }

        let start = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let end = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        if start.is_none() && end.is_none() {
            let total_lines = content.lines().count();
            if content.len() > 50_000 {
                let mut out = content[..50_000].to_string();
                out.push_str(&format!(
                    "\n[truncated — file has {total_lines} lines, use start_line/end_line or outline=true]"
                ));
                return out;
            }
            return content;
        }
        let lines: Vec<&str> = content.lines().collect();
        let s = start.unwrap_or(1).saturating_sub(1);
        let e = end.unwrap_or(lines.len()).min(lines.len());
        truncate_output(lines[s..e].join("\n"), global_output_limit())
    }

    /// Returns JSON with structured result for reliable parsing
    pub(crate) fn write_file(&self, args: &Value) -> String {
        use serde_json::json;

        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return json!({ "success": false, "error": e }).to_string(),
            },
            None => return json!({ "success": false, "error": "missing 'path'" }).to_string(),
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(c) => c,
            None => return json!({ "success": false, "error": "missing 'content'" }).to_string(),
        };
        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return json!({
                "success": false,
                "error": format!("failed to create parent directory {}: {e}", parent.display())
            })
            .to_string();
        }
        match fs::write(&path, content) {
            Ok(_) => json!({
                "success": true,
                "bytes_written": content.len(),
                "path": path.to_string_lossy().to_string()
            })
            .to_string(),
            Err(e) => json!({ "success": false, "error": e.to_string() }).to_string(),
        }
    }

    pub(crate) fn str_replace(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'path'".to_string(),
        };
        let old_str = match args.get("old_str").and_then(Value::as_str) {
            Some(s) => s,
            None => return "Error: missing 'old_str'".to_string(),
        };
        let new_str = match args.get("new_str").and_then(Value::as_str) {
            Some(s) => s,
            None => return "Error: missing 'new_str'".to_string(),
        };
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };
        let count = content.matches(old_str).count();
        if count == 0 {
            return str_replace_not_found_hint(&content, old_str);
        }
        if count > 1 {
            return str_replace_ambiguous_hint(&content, old_str, count);
        }
        let new_content = content.replacen(old_str, new_str, 1);
        match fs::write(&path, &new_content) {
            Ok(_) => {
                // Build a compact diff preview for the LLM and user
                let old_lines: Vec<&str> = old_str.lines().collect();
                let new_lines: Vec<&str> = new_str.lines().collect();
                let diff_lines = old_lines.len().max(new_lines.len());
                if diff_lines <= 10 {
                    let mut diff = String::from("Replaced successfully\n");
                    for l in &old_lines {
                        diff.push_str(&format!("- {l}\n"));
                    }
                    for l in &new_lines {
                        diff.push_str(&format!("+ {l}\n"));
                    }
                    diff
                } else {
                    format!(
                        "Replaced successfully ({} lines → {} lines)",
                        old_lines.len(),
                        new_lines.len()
                    )
                }
            }
            Err(e) => format!("Error writing file: {e}"),
        }
    }

    pub(crate) fn list_dir(&self, args: &Value) -> String {
        let dir = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.project_root.clone());
        let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
        let mut out = String::new();
        self.list_dir_recursive(&dir, &dir, depth, 0, &mut out);
        if out.is_empty() {
            "(empty)".to_string()
        } else {
            truncate_output(out, tool_output_limit())
        }
    }

    #[allow(clippy::only_used_in_recursion)]
    pub(crate) fn list_dir_recursive(
        &self,
        base: &Path,
        dir: &Path,
        max_depth: usize,
        cur: usize,
        out: &mut String,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let indent = "  ".repeat(cur);
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Skip hidden and common noise dirs
            if name.starts_with('.')
                || name == "target"
                || name == "node_modules"
                || name == "__pycache__"
            {
                continue;
            }
            let ft = entry.file_type().ok();
            let is_dir = ft.map(|t| t.is_dir()).unwrap_or(false);
            out.push_str(&format!(
                "{indent}{}{}\n",
                name,
                if is_dir { "/" } else { "" }
            ));
            if is_dir && cur < max_depth.saturating_sub(1) {
                self.list_dir_recursive(base, &entry.path(), max_depth, cur + 1, out);
            }
        }
    }

    /// Find files with similar names to a missing file.
    /// Returns up to 3 suggestions based on filename similarity.
    fn find_similar_files(&self, path_str: &str) -> Vec<String> {
        let path = Path::new(path_str);
        
        // Get the filename we're looking for
        let target_name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_lowercase(),
            None => return Vec::new(),
        };
        
        // Get the parent directory to search in
        let search_dir = if path.is_absolute() {
            path.parent().map(|p| p.to_path_buf())
        } else {
            Some(self.project_root.join(path.parent().unwrap_or(Path::new(""))))
        };
        
        let search_dir = match search_dir {
            Some(d) if d.exists() => d,
            _ => return Vec::new(),
        };
        
        // Find similar files in the directory
        let mut candidates: Vec<(String, usize)> = Vec::new();
        if let Ok(entries) = fs::read_dir(&search_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy().to_lowercase();
                
                // Skip directories unless they might be what we want
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir && !target_name.ends_with('/') {
                    continue;
                }
                
                // Calculate simple similarity (shared prefix + extension match)
                let score = similarity_score(&target_name, &name_str);
                if score > 0 {
                    let rel_path = entry.path();
                    let display = rel_path.strip_prefix(&self.project_root)
                        .unwrap_or(&rel_path)
                        .display()
                        .to_string();
                    candidates.push((display, score));
                }
            }
        }
        
        // Sort by score descending and take top 3
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates.into_iter()
            .take(3)
            .map(|(path, _)| path)
            .collect()
    }
}

/// Calculate similarity score between two filenames.
/// Higher score = more similar.
fn similarity_score(target: &str, candidate: &str) -> usize {
    let mut score = 0;
    
    // Exact match (shouldn't happen but handle it)
    if target == candidate {
        return 100;
    }
    
    // Shared prefix
    let common_prefix = target.chars()
        .zip(candidate.chars())
        .take_while(|(a, b)| a == b)
        .count();
    score += common_prefix * 3;
    
    // Same extension
    let target_ext = target.rsplit('.').next();
    let cand_ext = candidate.rsplit('.').next();
    if target_ext == cand_ext && target_ext.is_some() {
        score += 5;
    }
    
    // Contains target as substring
    if candidate.contains(target) || target.contains(candidate) {
        score += 10;
    }
    
    // Similar length
    let len_diff = (target.len() as isize - candidate.len() as isize).unsigned_abs();
    if len_diff < 5 {
        score += 5 - len_diff;
    }
    
    score
}

// ─── File outline extraction ────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Language {
    Rust,
    Python,
    TypeScript,
    Go,
    Java,
    CppLike,
    Unknown,
}

fn detect_language(ext: &str) -> Language {
    match ext {
        "rs" => Language::Rust,
        "py" | "pyi" => Language::Python,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "mts" => Language::TypeScript,
        "go" => Language::Go,
        "java" | "kt" | "scala" => Language::Java,
        "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "cs" => Language::CppLike,
        _ => Language::Unknown,
    }
}

/// Extract definition signatures from source code.
/// Returns Vec<(line_number, signature_text)>.
fn extract_outline(content: &str, lang: Language) -> Vec<(usize, String)> {
    let lines: Vec<&str> = content.lines().collect();
    let mut defs = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with('#') && lang != Language::Python
        {
            continue;
        }
        if is_definition(trimmed, line, lang) {
            // Trim trailing `{` and whitespace for cleaner output
            let sig = trimmed.trim_end_matches('{').trim_end();
            defs.push((i + 1, sig.to_string()));
        }
    }
    defs
}

fn is_definition(trimmed: &str, _original: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => is_rust_def(trimmed),
        Language::Python => is_python_def(trimmed),
        Language::TypeScript => is_typescript_def(trimmed),
        Language::Go => is_go_def(trimmed),
        Language::Java => is_java_def(trimmed),
        Language::CppLike => is_cpp_def(trimmed),
        Language::Unknown => is_generic_def(trimmed),
    }
}

fn is_rust_def(line: &str) -> bool {
    // Strip visibility/attribute prefixes
    let s = strip_rust_vis(line);
    s.starts_with("fn ")
        || s.starts_with("async fn ")
        || s.starts_with("unsafe fn ")
        || s.starts_with("const fn ")
        || s.starts_with("struct ")
        || s.starts_with("enum ")
        || s.starts_with("trait ")
        || s.starts_with("impl ")
        || s.starts_with("impl<")
        || s.starts_with("mod ")
        || s.starts_with("type ")
        || s.starts_with("const ")
        || s.starts_with("static ")
        || s.starts_with("macro_rules!")
        || s.starts_with("use ")
}

fn strip_rust_vis(line: &str) -> &str {
    let s = line.strip_prefix("pub(crate) ").unwrap_or(line);
    let s = s.strip_prefix("pub(super) ").unwrap_or(s);

    (s.strip_prefix("pub ").unwrap_or(s)) as _
}

fn is_python_def(line: &str) -> bool {
    line.starts_with("def ")
        || line.starts_with("async def ")
        || line.starts_with("class ")
        // Module-level assignments
        || (line.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false)
            && line.contains(" = "))
        // Decorators (include for context)
        || line.starts_with("@")
}

fn is_typescript_def(line: &str) -> bool {
    let s = line.strip_prefix("export ").unwrap_or(line);
    let s = s.strip_prefix("default ").unwrap_or(s);
    let s = s.strip_prefix("declare ").unwrap_or(s);
    let s = s.strip_prefix("abstract ").unwrap_or(s);
    let s = s.strip_prefix("async ").unwrap_or(s);
    s.starts_with("function ")
        || s.starts_with("function*(")
        || s.starts_with("class ")
        || s.starts_with("interface ")
        || s.starts_with("type ")
        || s.starts_with("enum ")
        || s.starts_with("const ")
        || s.starts_with("let ")
        || s.starts_with("var ")
        // Method-like at class level (indent)
        || (line.starts_with("  ") && (s.contains("(") && !s.starts_with("if ") && !s.starts_with("for ") && !s.starts_with("while ")))
}

fn is_go_def(line: &str) -> bool {
    line.starts_with("func ")
        || line.starts_with("type ")
        || line.starts_with("var ")
        || (line.starts_with("const ") && !line.starts_with("const ("))
        || line == "const ("
        || line == "var ("
}

fn is_java_def(line: &str) -> bool {
    // Strip annotations (common above defs but on same logical line when collapsed)
    let s = line.strip_prefix("@").map(|_| line).unwrap_or(line);
    let stripped = strip_java_mods(s);
    stripped.starts_with("class ")
        || stripped.starts_with("interface ")
        || stripped.starts_with("enum ")
        || stripped.starts_with("record ")
        // Method declarations: have ( and either { or ;
        || (stripped.contains('(') && !stripped.starts_with("if ") && !stripped.starts_with("for ") && !stripped.starts_with("while ")
            && !stripped.starts_with("//") && !stripped.starts_with("*")
            && (stripped.ends_with('{') || stripped.ends_with(") {")))
        || s.starts_with("@")
}

fn strip_java_mods(line: &str) -> &str {
    let mut s = line;
    for m in &[
        "public ",
        "private ",
        "protected ",
        "static ",
        "final ",
        "abstract ",
        "synchronized ",
        "native ",
    ] {
        s = s.strip_prefix(m).unwrap_or(s);
    }
    s
}

fn is_cpp_def(line: &str) -> bool {
    // Minimal: detect function signatures, class/struct, namespace
    line.starts_with("class ")
        || line.starts_with("struct ")
        || line.starts_with("namespace ")
        || line.starts_with("enum ")
        || line.starts_with("typedef ")
        || line.starts_with("#define ")
        || line.starts_with("template")
        // Function-like: type name( with no leading spaces (top-level)
        || (!line.starts_with(' ') && !line.starts_with('\t') && line.contains('(')
            && !line.starts_with("//") && !line.starts_with("/*") && !line.starts_with("#")
            && !line.starts_with("if ") && !line.starts_with("for ") && !line.starts_with("while "))
}

fn is_generic_def(line: &str) -> bool {
    // Catch common patterns across languages
    line.starts_with("function ")
        || line.starts_with("def ")
        || line.starts_with("class ")
        || line.starts_with("struct ")
        || line.starts_with("pub fn ")
        || line.starts_with("fn ")
        || line.starts_with("impl ")
        || line.starts_with("trait ")
        || line.starts_with("type ")
        || line.starts_with("export ")
        || line.starts_with("module ")
        || line.starts_with("func ")
}

// ─── str_replace fuzzy matching ─────────────────────────────────────────────

/// When old_str not found, try to find close matches and report locations.
fn str_replace_not_found_hint(content: &str, old_str: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_str.lines().collect();
    let mut msg = String::from("Error: old_str not found in file.\n");

    // Strategy 1: Try whitespace-normalized match
    let normalized_old = normalize_ws(old_str);
    let normalized_content = normalize_ws(content);
    if normalized_content.contains(&normalized_old) {
        msg.push_str(
            "Hint: A whitespace-normalized match exists. Check indentation/trailing spaces.\n",
        );
        // Find which line in the file the first old line matches (normalized)
        if let Some(first_line) = old_lines.first() {
            let norm_first = normalize_ws(first_line);
            for (i, line) in lines.iter().enumerate() {
                if normalize_ws(line) == norm_first {
                    msg.push_str(&format!("  Possible match at line {}\n", i + 1));
                    // Show a few lines of actual content
                    let end = (i + old_lines.len().min(5)).min(lines.len());
                    for (j, line_content) in lines[i..end].iter().enumerate() {
                        msg.push_str(&format!("  {}: {}\n", i + j + 1, line_content));
                    }
                    break;
                }
            }
        }
        return msg;
    }

    // Strategy 2: Find the first line of old_str in the file
    if let Some(first_line) = old_lines.first() {
        let needle = first_line.trim();
        if !needle.is_empty() {
            let mut matches: Vec<usize> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if line.trim() == needle || line.contains(needle) {
                    matches.push(i + 1);
                    if matches.len() >= 5 {
                        break;
                    }
                }
            }
            if !matches.is_empty() {
                msg.push_str(&format!(
                    "Hint: First line of old_str ('{}') found at line(s): {:?}\n",
                    truncate_str(needle, 60),
                    matches
                ));
                // Show context around first match
                let line_idx = matches[0] - 1;
                let start = line_idx;
                let end = (line_idx + old_lines.len() + 1).min(lines.len());
                msg.push_str("Actual file content:\n");
                for (j, line_content) in lines[start..end].iter().enumerate() {
                    msg.push_str(&format!("  {}: {}\n", start + j + 1, line_content));
                }
            }
        }
    }

    // Strategy 3: If multi-line, check how many lines match
    if old_lines.len() > 1 {
        let matching_count = old_lines
            .iter()
            .filter(|ol| {
                let trimmed = ol.trim();
                !trimmed.is_empty() && lines.iter().any(|fl| fl.trim() == trimmed)
            })
            .count();
        if matching_count > 0 {
            msg.push_str(&format!(
                "Hint: {matching_count}/{} lines from old_str exist individually in the file.\n",
                old_lines.len()
            ));
        }
    }

    if msg.ends_with("not found in file.\n") {
        msg.push_str("Hint: Use read_file with start_line/end_line to verify the exact content before retrying.\n");
    }
    msg
}

/// When old_str found multiple times, show locations.
fn str_replace_ambiguous_hint(content: &str, old_str: &str, count: usize) -> String {
    let mut msg = format!("Error: old_str found {count} times — must be unique.\n");
    // Find line numbers of each occurrence
    let lines: Vec<&str> = content.lines().collect();
    let first_line = old_str.lines().next().unwrap_or("");
    let needle = first_line.trim();
    if !needle.is_empty() {
        let mut locs: Vec<usize> = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            if line.contains(needle) {
                locs.push(i + 1);
            }
        }
        if !locs.is_empty() {
            msg.push_str(&format!("Locations (first line matches): {:?}\n", locs));
            msg.push_str("Hint: Add more surrounding context to old_str to make it unique.\n");
        }
    }
    msg
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_executor_in(dir: &std::path::Path) -> ToolExecutor {
        ToolExecutor::new(dir)
    }

    // ── file_outline: Rust ───────────────────────────────────────────────────

    #[test]
    fn outline_rust_functions_and_structs() {
        let rust_code = r#"
use std::collections::HashMap;

pub struct Config {
    name: String,
}

pub enum Status {
    Active,
    Inactive,
}

impl Config {
    pub fn new(name: &str) -> Self {
        Config { name: name.to_string() }
    }

    pub(crate) fn validate(&self) -> bool {
        true
    }
}

pub trait Handler {
    fn handle(&self);
}

async fn fetch_data(url: &str) -> String {
    url.to_string()
}

mod inner {
    pub fn helper() {}
}
"#;
        let defs = extract_outline(rust_code, Language::Rust);
        let names: Vec<&str> = defs.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            names.iter().any(|s| s.contains("use std::collections")),
            "should find use: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("pub struct Config")),
            "should find struct: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("pub enum Status")),
            "should find enum: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("impl Config")),
            "should find impl: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("pub fn new")),
            "should find pub fn: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("validate")),
            "should find validate: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("pub trait Handler")),
            "should find trait: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("async fn fetch_data")),
            "should find async fn: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("mod inner")),
            "should find mod: {names:?}"
        );
    }

    #[test]
    fn outline_rust_preserves_line_numbers() {
        let code = "pub fn first() {}\n\nfn second() {}";
        let defs = extract_outline(code, Language::Rust);
        assert_eq!(defs[0].0, 1, "first fn should be line 1");
        assert_eq!(defs[1].0, 3, "second fn should be line 3");
    }

    // ── file_outline: Python ─────────────────────────────────────────────────

    #[test]
    fn outline_python_classes_and_functions() {
        let py_code = r#"
import os

class MyClass:
    def __init__(self):
        pass

    def method(self):
        pass

def standalone():
    return 42

async def async_handler(request):
    pass

MAX_SIZE = 100
"#;
        let defs = extract_outline(py_code, Language::Python);
        let names: Vec<&str> = defs.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            names.iter().any(|s| s.contains("class MyClass")),
            "should find class: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("def standalone")),
            "should find def: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("async def async_handler")),
            "should find async def: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("MAX_SIZE")),
            "should find constant: {names:?}"
        );
    }

    // ── file_outline: TypeScript ─────────────────────────────────────────────

    #[test]
    fn outline_typescript_exports_and_classes() {
        let ts_code = r#"
export function fetchData(url: string): Promise<string> {
  return fetch(url);
}

export class UserService {
  constructor() {}
}

export interface Config {
  name: string;
}

export type ID = string | number;

const helper = () => {};

export default class App {
"#;
        let defs = extract_outline(ts_code, Language::TypeScript);
        let names: Vec<&str> = defs.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            names
                .iter()
                .any(|s| s.contains("export function fetchData")),
            "should find export function: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("export class UserService")),
            "should find export class: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("export interface Config")),
            "should find interface: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("export type ID")),
            "should find type: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("export default class App")),
            "should find default class: {names:?}"
        );
    }

    // ── file_outline: Go ─────────────────────────────────────────────────────

    #[test]
    fn outline_go_funcs_and_types() {
        let go_code = r#"
package main

func main() {
    fmt.Println("hello")
}

type Config struct {
    Name string
}

func (c *Config) Validate() bool {
    return true
}

type Handler interface {
    Handle()
}
"#;
        let defs = extract_outline(go_code, Language::Go);
        let names: Vec<&str> = defs.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            names.iter().any(|s| s.contains("func main")),
            "should find func main: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("type Config struct")),
            "should find type struct: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|s| s.contains("func (c *Config) Validate")),
            "should find method: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("type Handler interface")),
            "should find interface: {names:?}"
        );
    }

    // ── file_outline: language detection ─────────────────────────────────────

    #[test]
    fn detect_language_from_extension() {
        assert_eq!(detect_language("rs"), Language::Rust);
        assert_eq!(detect_language("py"), Language::Python);
        assert_eq!(detect_language("ts"), Language::TypeScript);
        assert_eq!(detect_language("tsx"), Language::TypeScript);
        assert_eq!(detect_language("go"), Language::Go);
        assert_eq!(detect_language("java"), Language::Java);
        assert_eq!(detect_language("cpp"), Language::CppLike);
        assert_eq!(detect_language("txt"), Language::Unknown);
    }

    // ── file_outline: empty/no-defs ──────────────────────────────────────────

    #[test]
    fn outline_empty_file() {
        let defs = extract_outline("", Language::Rust);
        assert!(defs.is_empty());
    }

    #[test]
    fn outline_no_definitions() {
        let code = "// just comments\n// nothing here\n";
        let defs = extract_outline(code, Language::Rust);
        assert!(defs.is_empty());
    }

    // ── file_outline: integration via read_file ──────────────────────────────

    #[test]
    fn read_file_outline_mode() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.rs");
        std::fs::write(
            &file_path,
            "pub fn hello() {}\n\nstruct Foo {\n    x: i32\n}\n",
        )
        .unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({
            "path": "test.rs",
            "outline": true
        }));

        assert!(
            result.contains("Outline"),
            "should have outline header: {result}"
        );
        assert!(
            result.contains("pub fn hello"),
            "should contain fn: {result}"
        );
        assert!(
            result.contains("struct Foo"),
            "should contain struct: {result}"
        );
        assert!(result.contains("1:"), "should have line numbers: {result}");
    }

    #[test]
    fn read_file_outline_empty_result() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, "just plain text\nnothing here\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({
            "path": "test.txt",
            "outline": true
        }));

        assert!(
            result.contains("no definitions found"),
            "should report empty: {result}"
        );
    }

    // ── str_replace: fuzzy matching ──────────────────────────────────────────

    #[test]
    fn str_replace_not_found_whitespace_hint() {
        let content = "  fn hello() {\n    println!(\"hi\");\n  }\n";
        let old_str = "fn hello() {\n  println!(\"hi\");\n}";
        let msg = str_replace_not_found_hint(content, old_str);
        assert!(
            msg.contains("whitespace-normalized"),
            "should hint whitespace: {msg}"
        );
        assert!(msg.contains("line"), "should show line number: {msg}");
    }

    #[test]
    fn str_replace_not_found_first_line_hint() {
        let content = "line one\nfn target() {\n    body\n}\nline five\n";
        let old_str = "fn target() {\n    wrong body\n}";
        let msg = str_replace_not_found_hint(content, old_str);
        assert!(
            msg.contains("fn target()"),
            "should show first line match: {msg}"
        );
        assert!(
            msg.contains("2") || msg.contains("line"),
            "should show line number: {msg}"
        );
    }

    #[test]
    fn str_replace_not_found_no_match_at_all() {
        let content = "fn hello() {}\n";
        let old_str = "completely_nonexistent_text";
        let msg = str_replace_not_found_hint(content, old_str);
        assert!(msg.contains("Error"), "should be error: {msg}");
        assert!(
            msg.contains("read_file") || msg.contains("Hint"),
            "should give guidance: {msg}"
        );
    }

    #[test]
    fn str_replace_ambiguous_shows_locations() {
        let content = "fn foo() {}\nsome stuff\nfn foo() {}\n";
        let old_str = "fn foo() {}";
        let msg = str_replace_ambiguous_hint(content, old_str, 2);
        assert!(msg.contains("2 times"), "should show count: {msg}");
        assert!(msg.contains("Locations"), "should show locations: {msg}");
        assert!(
            msg.contains("unique"),
            "should hint about uniqueness: {msg}"
        );
    }

    // ── str_replace: integration via ToolExecutor ────────────────────────────

    #[test]
    fn str_replace_not_found_returns_hints() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        std::fs::write(&file_path, "  fn hello() {\n    println!(\"hi\");\n  }\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.str_replace(&serde_json::json!({
            "path": "code.rs",
            "old_str": "fn hello() {\n  println!(\"hi\");\n}",
            "new_str": "fn hello() {}"
        }));

        assert!(result.contains("Error"), "should be error: {result}");
        assert!(result.contains("Hint"), "should have hints: {result}");
    }

    #[test]
    fn str_replace_ambiguous_returns_locations() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("dup.rs");
        std::fs::write(&file_path, "let x = 1;\nlet y = 2;\nlet x = 1;\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.str_replace(&serde_json::json!({
            "path": "dup.rs",
            "old_str": "let x = 1;",
            "new_str": "let x = 42;"
        }));

        assert!(result.contains("2 times"), "should show count: {result}");
        assert!(
            result.contains("Locations"),
            "should show locations: {result}"
        );
    }

    // ── str_replace multi-line partial match ─────────────────────────────────

    #[test]
    fn str_replace_not_found_multiline_partial() {
        let content = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let old_str = "fn alpha() {}\nfn WRONG() {}\nfn gamma() {}";
        let msg = str_replace_not_found_hint(content, old_str);
        assert!(
            msg.contains("lines from old_str exist"),
            "should report partial: {msg}"
        );
    }

    // ── read_file large file truncation hint ─────────────────────────────────

    #[test]
    fn read_file_large_file_truncation_includes_hint() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..2000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({"path": "big.txt"}));

        assert!(
            result.contains("truncated"),
            "should be truncated: last 100 chars: {}",
            &result[result.len().saturating_sub(100)..]
        );
        assert!(
            result.contains("outline") || result.contains("start_line"),
            "should suggest alternatives: last 200 chars: {}",
            &result[result.len().saturating_sub(200)..]
        );
    }

    // ── read_file not-found hints ────────────────────────────────────────────

    #[test]
    fn read_file_not_found_suggests_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({"path": "nonexistent.rs"}));

        assert!(result.contains("Error"), "should be error: {result}");
        assert!(
            result.contains("list_dir") || result.contains("glob"),
            "should suggest list_dir/glob: {result}"
        );
    }

    // ── normalize_ws ─────────────────────────────────────────────────────────

    #[test]
    fn normalize_ws_collapses_whitespace() {
        assert_eq!(normalize_ws("  fn   hello(  ) "), "fn hello( )");
    }

    #[test]
    fn truncate_str_within_limit() {
        assert_eq!(truncate_str("short", 10), "short");
    }

    #[test]
    fn truncate_str_over_limit() {
        let result = truncate_str("this is a long string", 7);
        assert!(result.ends_with("..."), "should end with ...: {result}");
        assert!(result.len() <= 10);
    }

    // ── file_outline: generic fallback ───────────────────────────────────────

    #[test]
    fn outline_generic_catches_common_keywords() {
        let code = "function greet(name) {\n  console.log(name);\n}\n\nclass Animal {\n}\n";
        let defs = extract_outline(code, Language::Unknown);
        let names: Vec<&str> = defs.iter().map(|(_, s)| s.as_str()).collect();
        assert!(
            names.iter().any(|s| s.contains("function greet")),
            "should find function: {names:?}"
        );
        assert!(
            names.iter().any(|s| s.contains("class Animal")),
            "should find class: {names:?}"
        );
    }

    // ── file_outline: strips trailing braces ─────────────────────────────────

    #[test]
    fn outline_strips_trailing_brace() {
        let code = "pub fn hello() {\n    body\n}\n";
        let defs = extract_outline(code, Language::Rust);
        assert!(!defs.is_empty());
        // Should have "pub fn hello()" not "pub fn hello() {"
        assert!(
            !defs[0].1.ends_with('{'),
            "should strip brace: {:?}",
            defs[0].1
        );
        assert!(
            defs[0].1.contains("pub fn hello()"),
            "signature: {:?}",
            defs[0].1
        );
    }

    // ── read_file: similar file suggestions ──────────────────────────────────

    #[test]
    fn read_file_not_found_suggests_similar() {
        let dir = tempfile::tempdir().unwrap();
        // Create some files with similar names
        std::fs::write(dir.path().join("config.rs"), "// config").unwrap();
        std::fs::write(dir.path().join("config.toml"), "# config").unwrap();
        std::fs::write(dir.path().join("other.rs"), "// other").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({
            "path": "confg.rs"  // typo
        }));

        assert!(
            result.contains("No such file"),
            "should report not found: {result}"
        );
        assert!(
            result.contains("config.rs") || result.contains("Did you mean"),
            "should suggest similar: {result}"
        );
    }

    #[test]
    fn read_file_directory_error_suggests_list_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({
            "path": "subdir"
        }));

        assert!(
            result.contains("directory") || result.contains("Is a directory"),
            "should mention directory: {result}"
        );
        assert!(
            result.contains("list_dir"),
            "should suggest list_dir: {result}"
        );
    }

    #[test]
    fn similarity_score_exact_match_highest() {
        assert_eq!(similarity_score("test.rs", "test.rs"), 100);
    }

    #[test]
    fn similarity_score_same_extension_bonus() {
        let with_ext = similarity_score("config.rs", "setting.rs");
        let without_ext = similarity_score("config.rs", "setting.py");
        assert!(with_ext > without_ext, "same ext should score higher");
    }
}
