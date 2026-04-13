//! Code analysis tools: symbols, find_definition, find_references, rename_symbol,
//! dead_code, extract_members, type_hierarchy, hover_info, symbol_search,
//! call_graph, run_build_test, and supporting helpers.
//!
//! These are all `impl ToolExecutor` methods extracted from the hub module.

use std::fs;
use std::path::{Path, PathBuf};

use astra_runtime::str_preview::truncate_str;
use serde_json::Value;

use super::{
    ToolExecutor, build_test, categorize_reference, code_intel, parse_grep_file_line,
    per_tool_output_limit, tool_output_limit, truncate_output, validate_path,
};

impl ToolExecutor {
    /// Extract code symbols (functions, classes, structs) from a file using Tree-sitter.
    ///
    /// Returns structured symbol info with signatures and line numbers.
    pub(super) fn symbols(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'path' parameter".to_string(),
        };

        let path = if path_str.starts_with('/') {
            PathBuf::from(path_str)
        } else {
            self.project_root.join(path_str)
        };

        // Sandbox check
        if let Some(ref policy) = self.sandbox_policy
            && let Err(e) = validate_path(policy, path_str)
        {
            return format!("Sandbox: path blocked: {e}");
        }

        if !path.exists() {
            return format!("Error: No such file: {}", path.display());
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error: Failed to read file: {e}"),
        };

        // Detect language from path
        let lang = match code_intel::detect_language(&path) {
            Some(l) => l,
            None => {
                return format!(
                    "Error: Unsupported language for {}. Supports: Rust, Python, TypeScript/JavaScript, Go",
                    path.display()
                );
            }
        };

        // Extract symbols
        let mut symbols = code_intel::extract_symbols(&content, lang);

        // Apply pattern filter if provided
        if let Some(pattern) = args.get("pattern").and_then(Value::as_str)
            && let Ok(re) = regex::Regex::new(pattern)
        {
            symbols.retain(|s| re.is_match(&s.name));
        }

        // Apply kind filter if provided
        if let Some(kinds_arr) = args.get("kinds").and_then(Value::as_array) {
            let kinds: Vec<&str> = kinds_arr.iter().filter_map(Value::as_str).collect();
            if !kinds.is_empty() {
                symbols.retain(|s| {
                    let kind_str = s.kind.as_str();
                    kinds.iter().any(|k| k.eq_ignore_ascii_case(kind_str))
                });
            }
        }

        if symbols.is_empty() {
            return "No symbols found matching criteria.".to_string();
        }

        // Format output
        let lang_name = match lang {
            code_intel::Language::Rust => "Rust",
            code_intel::Language::Python => "Python",
            code_intel::Language::TypeScript => "TypeScript",
            code_intel::Language::JavaScript => "JavaScript",
            code_intel::Language::Go => "Go",
            code_intel::Language::Java => "Java",
            code_intel::Language::C => "C",
            code_intel::Language::Cpp => "C++",
            code_intel::Language::Ruby => "Ruby",
        };

        let show_calls = args.get("calls").and_then(Value::as_bool).unwrap_or(false);

        let mut output = format!(
            "# Symbols in {} ({}, {} found)\n\n",
            path.file_name().unwrap_or_default().to_string_lossy(),
            lang_name,
            symbols.len()
        );

        for sym in &symbols {
            let parent_suffix = sym
                .parent
                .as_ref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}:{}-{} [{}]{}: {}\n",
                path.file_name().unwrap_or_default().to_string_lossy(),
                sym.start_line,
                sym.end_line,
                sym.kind.as_str(),
                parent_suffix,
                sym.signature
            ));

            // If calls=true, show what this symbol calls
            if show_calls
                && matches!(
                    sym.kind,
                    code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
                )
            {
                let calls = code_intel::extract_calls(&content, lang, sym.start_line, sym.end_line);
                if !calls.is_empty() {
                    for call in calls.iter().take(8) {
                        if let Some(ref recv) = call.receiver {
                            output.push_str(&format!(
                                "    → {}.{}() L{}\n",
                                recv, call.callee, call.line
                            ));
                        } else {
                            output.push_str(&format!("    → {}() L{}\n", call.callee, call.line));
                        }
                    }
                    if calls.len() > 8 {
                        output.push_str(&format!("    ... and {} more calls\n", calls.len() - 8));
                    }
                }
            }
        }

        output
    }

    /// AST-validate grep matches: filter out references in comments and string literals.
    ///
    /// Groups matches by file, parses each file once with tree-sitter, and checks
    /// if the symbol at each match position falls inside a non-code node.
    pub(super) fn ast_validate_references<'a>(
        &self,
        lines: &[&'a str],
        symbol: &str,
    ) -> Vec<&'a str> {
        use std::collections::HashMap;

        // Group lines by file path for efficient per-file parsing
        let mut by_file: HashMap<&str, Vec<(usize, &'a str)>> = HashMap::new();
        for line in lines {
            if let Some((file, line_num)) = parse_grep_file_line(line) {
                by_file.entry(file).or_default().push((line_num, line));
            }
        }

        let mut result = Vec::with_capacity(lines.len());

        for (file, matches) in &by_file {
            let file_path = self.project_root.join(file);
            let lang = match code_intel::detect_language(&file_path) {
                Some(l) => l,
                None => {
                    // Can't validate — keep all matches for this file
                    result.extend(matches.iter().map(|(_, line)| *line));
                    continue;
                }
            };
            let content = match fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => {
                    result.extend(matches.iter().map(|(_, line)| *line));
                    continue;
                }
            };

            for &(line_num, line) in matches {
                // Find the column where the symbol appears in this line
                let line_content = content
                    .lines()
                    .nth(line_num.saturating_sub(1))
                    .unwrap_or("");
                let col = match line_content.find(symbol) {
                    Some(c) => c,
                    None => {
                        result.push(line); // Can't find symbol in line, keep it
                        continue;
                    }
                };

                if !code_intel::is_in_comment_or_string(&content, lang, line_num, col) {
                    result.push(line);
                }
            }
        }

        // Also keep lines that couldn't be parsed
        for line in lines {
            if parse_grep_file_line(line).is_none() {
                result.push(line);
            }
        }

        result
    }

    /// Walk project files and find all functions that call `target` symbol.
    /// Returns Vec of (relative_path, caller_name, caller_signature, call_line).
    pub(super) fn find_callers_cross_file(
        &self,
        target: &str,
        _origin_file: &std::path::Path,
    ) -> Vec<(String, String, String, usize)> {
        let skip_names = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let extensions = [
            "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp", "rb",
        ];
        let max_files = 300;

        // Step 1: Use ripgrep to pre-filter files containing the target symbol (fast)
        let candidate_files = self.prefilter_files_with_symbol(target, &extensions);

        // Step 2: For each candidate, parse with tree-sitter and find callers
        let mut callers = Vec::new();
        let mut files_scanned = 0;

        let files_to_scan: Vec<PathBuf> = if candidate_files.is_empty() {
            // Fallback: walk all files (ripgrep not available)
            self.collect_project_files(&skip_names, &extensions, max_files)
        } else {
            candidate_files.into_iter().take(max_files).collect()
        };

        for file_path in &files_to_scan {
            files_scanned += 1;
            if files_scanned > max_files {
                break;
            }

            let lang = match code_intel::detect_language(file_path) {
                Some(l) => l,
                None => continue,
            };
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let symbols = code_intel::extract_symbols(&content, lang);
            let rel_path = file_path
                .strip_prefix(&self.project_root)
                .unwrap_or(file_path)
                .display()
                .to_string();

            for sym in &symbols {
                if sym.name == target {
                    continue; // Skip the target's own definition
                }
                if !matches!(
                    sym.kind,
                    code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
                ) {
                    continue;
                }
                let sym_calls =
                    code_intel::extract_calls(&content, lang, sym.start_line, sym.end_line);
                for call in &sym_calls {
                    if call.callee == target {
                        callers.push((
                            rel_path.clone(),
                            sym.name.clone(),
                            sym.signature.clone(),
                            call.line,
                        ));
                        break;
                    }
                }
            }
        }

        callers
    }

    /// Use ripgrep to quickly find files that contain a symbol name (pre-filter).
    pub(super) fn prefilter_files_with_symbol(
        &self,
        symbol: &str,
        extensions: &[&str],
    ) -> Vec<PathBuf> {
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--files-with-matches")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("-w") // word boundary
            .current_dir(&self.project_root);

        // Add extension filters
        for ext in extensions {
            cmd.arg("--glob").arg(format!("*.{ext}"));
        }

        // Exclude noise
        for dir in &[".git", "node_modules", "target", "vendor", "dist"] {
            cmd.arg("--glob").arg(format!("!{dir}/"));
        }

        cmd.arg(symbol);

        match cmd.output() {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| self.project_root.join(l.trim()))
                .filter(|p| p.exists())
                .collect(),
            Err(_) => Vec::new(), // Fallback handled by caller
        }
    }

    /// Collect project files by walking directories (fallback when ripgrep unavailable).
    pub(super) fn collect_project_files(
        &self,
        skip_names: &[&str],
        extensions: &[&str],
        max_files: usize,
    ) -> Vec<PathBuf> {
        let mut result = Vec::new();
        let mut dirs_to_visit = vec![self.project_root.clone()];

        while let Some(dir) = dirs_to_visit.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || skip_names.contains(&name_str.as_ref()) {
                    continue;
                }
                let ft = entry.file_type().ok();
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs_to_visit.push(entry.path());
                } else if ft.map(|t| t.is_file()).unwrap_or(false)
                    && let Some(ext) = entry.path().extension().and_then(|e| e.to_str())
                    && extensions.contains(&ext)
                {
                    result.push(entry.path());
                    if result.len() >= max_files {
                        return result;
                    }
                }
            }
        }

        result
    }

    pub(super) fn collect_files_with_glob(
        &self,
        root: &std::path::Path,
        glob_pat: &str,
        files: &mut Vec<std::path::PathBuf>,
    ) {
        let skip_dirs = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let pat = glob_pat.trim_start_matches('*');

        let mut dirs_to_visit = vec![root.to_path_buf()];
        while let Some(dir) = dirs_to_visit.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || skip_dirs.contains(&name_str.as_ref()) {
                    continue;
                }
                let ft = entry.file_type().ok();
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs_to_visit.push(entry.path());
                } else if ft.map(|t| t.is_file()).unwrap_or(false) {
                    let file_name = entry
                        .path()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if file_name.ends_with(pat) {
                        files.push(entry.path());
                        if files.len() >= 500 {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Resolve an import path to candidate file paths.
    ///
    /// Given an import path (e.g., "std::collections::HashMap" for Rust,
    /// "os.path" for Python, "./config" for TS), returns file paths within
    /// the project that likely define the imported symbol.
    pub(super) fn resolve_import_to_files(
        &self,
        import: &code_intel::ImportStatement,
        lang: code_intel::Language,
        file_paths: &[std::path::PathBuf],
    ) -> Vec<usize> {
        let mut candidates: Vec<usize> = Vec::new();

        // Convert import path to file path segments
        let path_segments: Vec<&str> = match lang {
            code_intel::Language::Rust => {
                // "crate::utils::helper" → ["utils", "helper"]
                // "super::config" → ["config"]
                let cleaned = import
                    .path
                    .trim_start_matches("crate::")
                    .trim_start_matches("super::");
                cleaned.split("::").collect()
            }
            code_intel::Language::Python => {
                // "os.path" → ["os", "path"]
                // ".utils" → ["utils"]
                import.path.trim_start_matches('.').split('.').collect()
            }
            code_intel::Language::TypeScript | code_intel::Language::JavaScript => {
                // "./config" → ["config"]
                // "../utils/helper" → ["utils", "helper"]
                let cleaned = import
                    .path
                    .trim_start_matches("./")
                    .trim_start_matches("../");
                cleaned.split('/').collect()
            }
            code_intel::Language::Go => {
                // "path/filepath" → ["path", "filepath"]
                import.path.split('/').collect()
            }
            _ => return candidates,
        };

        if path_segments.is_empty() {
            return candidates;
        }

        // Match file paths that contain import path segments
        let last_segment = path_segments.last().unwrap_or(&"");
        for (idx, file_path) in file_paths.iter().enumerate() {
            let path_str = file_path.to_string_lossy();
            let file_stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            // Exact stem match (e.g., import "config" → config.rs/config.py)
            if file_stem.eq_ignore_ascii_case(last_segment) {
                candidates.push(idx);
                continue;
            }

            // Check if the path contains all segments in order
            // e.g., "crate::utils::helper" matches "src/utils/helper.rs"
            if path_segments.len() > 1 {
                let lower_path = path_str.to_lowercase();
                let all_match = path_segments
                    .iter()
                    .all(|seg| lower_path.contains(&seg.to_lowercase()));
                if all_match {
                    candidates.push(idx);
                    continue;
                }
            }

            // For Rust: mod.rs in a directory matching the segment
            if matches!(lang, code_intel::Language::Rust) && file_stem == "mod" {
                let parent_name = file_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if parent_name.eq_ignore_ascii_case(last_segment) {
                    candidates.push(idx);
                }
            }

            // For Python: __init__.py in a directory matching the segment
            if matches!(lang, code_intel::Language::Python) && file_stem == "__init__" {
                let parent_name = file_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if parent_name.eq_ignore_ascii_case(last_segment) {
                    candidates.push(idx);
                }
            }

            // For TS: index.ts in a directory matching the segment
            if matches!(
                lang,
                code_intel::Language::TypeScript | code_intel::Language::JavaScript
            ) && file_stem == "index"
            {
                let parent_name = file_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if parent_name.eq_ignore_ascii_case(last_segment) {
                    candidates.push(idx);
                }
            }
        }

        candidates.dedup();
        candidates
    }

    /// Find where a symbol is defined across the codebase using tree-sitter.
    /// When a `file` parameter is provided, analyzes imports in that file to
    /// prioritize the most likely definition (import-aware resolution).
    pub(super) fn find_definition(&self, args: &Value) -> String {
        let symbol = match args.get("symbol").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'symbol' parameter is required".to_string(),
        };

        let search_root = if let Some(p) = args.get("path").and_then(Value::as_str) {
            self.project_root.join(p)
        } else {
            self.project_root.clone()
        };

        // Determine file extensions to search
        let lang_filter = args.get("language").and_then(Value::as_str);
        let extensions = match lang_filter {
            Some("rust") => vec!["rs"],
            Some("python") => vec!["py"],
            Some("typescript") => vec!["ts", "tsx"],
            Some("javascript") => vec!["js", "jsx"],
            Some("go") => vec!["go"],
            Some("java") => vec!["java"],
            Some("c") => vec!["c", "h"],
            Some("cpp") => vec!["cpp", "cc", "cxx", "hpp", "h"],
            Some("ruby") => vec!["rb"],
            None => vec![
                "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp",
                "rb",
            ],
            Some(other) => {
                return format!(
                    "Error: unsupported language '{other}'. Supported: rust, python, typescript, javascript, go, java, c, cpp, ruby"
                );
            }
        };

        // Build regex for matching symbol name
        let pattern = if symbol.contains('*') || symbol.contains('(') || symbol.contains('[') {
            match regex::Regex::new(symbol) {
                Ok(re) => re,
                Err(e) => return format!("Error: invalid regex pattern: {e}"),
            }
        } else {
            // Exact match
            match regex::Regex::new(&format!(r"^{}$", regex::escape(symbol))) {
                Ok(re) => re,
                Err(e) => return format!("Error: regex construction failed: {e}"),
            }
        };

        let definition_kinds = [
            "fn",
            "method",
            "class",
            "struct",
            "trait",
            "interface",
            "enum",
            "type",
            "const",
            "var",
            "mod",
        ];

        let mut results: Vec<String> = Vec::new();
        let mut import_results: Vec<String> = Vec::new();
        let max_files = 500;
        let mut files_scanned = 0;

        // Collect matching files using a simple recursive walker
        let mut dirs_to_visit = vec![search_root.clone()];
        let skip_names = ["node_modules", "target", "vendor", "dist", "__pycache__"];
        let mut file_paths: Vec<std::path::PathBuf> = Vec::new();

        while let Some(dir) = dirs_to_visit.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || skip_names.contains(&name_str.as_ref()) {
                    continue;
                }
                let ft = entry.file_type().ok();
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs_to_visit.push(entry.path());
                } else if ft.map(|t| t.is_file()).unwrap_or(false) {
                    let ext = entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_string();
                    if extensions.contains(&ext.as_str()) {
                        file_paths.push(entry.path());
                    }
                }
            }
        }

        // ── Import-aware resolution ────────────────────────────────────
        // When `file` parameter is provided, extract imports from that file
        // and prioritize files matching the import paths.
        let mut import_priority_indices: Vec<usize> = Vec::new();
        let context_file = args.get("file").and_then(Value::as_str);

        if let Some(ctx_file) = context_file {
            let ctx_path = if ctx_file.starts_with('/') {
                PathBuf::from(ctx_file)
            } else {
                self.project_root.join(ctx_file)
            };
            if ctx_path.exists()
                && let Some(ctx_lang) = code_intel::detect_language(&ctx_path)
                && let Ok(ctx_content) = fs::read_to_string(&ctx_path)
            {
                let imports = code_intel::extract_imports(&ctx_content, ctx_lang);
                // Find imports that reference the target symbol
                for import in &imports {
                    let matches_symbol = import.names.iter().any(|n| n == symbol)
                        || import.is_wildcard
                        || import.path.ends_with(symbol);
                    if matches_symbol {
                        let candidates =
                            self.resolve_import_to_files(import, ctx_lang, &file_paths);
                        import_priority_indices.extend(candidates);
                    }
                }
            }
        }
        import_priority_indices.sort_unstable();
        import_priority_indices.dedup();

        // Helper closure: scan a file for matching definitions
        let scan_file = |path: &std::path::PathBuf, project_root: &Path| -> Vec<(String, bool)> {
            let mut hits = Vec::new();
            let lang = match code_intel::detect_language(path) {
                Some(l) => l,
                None => return hits,
            };
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return hits,
            };
            let symbols = code_intel::extract_symbols(&content, lang);
            for sym in &symbols {
                if pattern.is_match(&sym.name) && definition_kinds.contains(&sym.kind.as_str()) {
                    let rel_path = path.strip_prefix(project_root).unwrap_or(path).display();
                    let parent_info = sym
                        .parent
                        .as_ref()
                        .map(|p| format!(" (in {p})"))
                        .unwrap_or_default();

                    let doc = code_intel::extract_doc_comment(&content, lang, sym.start_line);
                    let doc_info = if doc.is_empty() {
                        String::new()
                    } else {
                        let doc_lines: Vec<&str> = doc.lines().take(5).collect();
                        let truncated = if doc.lines().count() > 5 {
                            "\n    ..."
                        } else {
                            ""
                        };
                        format!("\n    📝 {}{}", doc_lines.join("\n    "), truncated)
                    };

                    hits.push((
                        format!(
                            "{}:{} [{}]{} {}{}",
                            rel_path,
                            sym.start_line,
                            sym.kind.as_str(),
                            parent_info,
                            sym.signature,
                            doc_info
                        ),
                        false,
                    ));
                }
            }
            hits
        };

        // Scan import-priority files first
        let mut scanned_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        for &idx in &import_priority_indices {
            if idx < file_paths.len() {
                scanned_indices.insert(idx);
                files_scanned += 1;
                for (hit, _) in scan_file(&file_paths[idx], &self.project_root) {
                    import_results.push(hit);
                }
            }
        }

        // Then scan remaining files
        for (idx, path) in file_paths.iter().enumerate() {
            if scanned_indices.contains(&idx) {
                continue;
            }
            files_scanned += 1;
            if files_scanned > max_files {
                results.push(format!("\n[stopped after scanning {max_files} files]"));
                break;
            }
            for (hit, _) in scan_file(path, &self.project_root) {
                results.push(hit);
            }
        }

        let total_found = import_results.len() + results.len();
        if total_found == 0 {
            format!("No definitions found for '{symbol}' ({files_scanned} files scanned)")
        } else {
            let mut body_parts: Vec<String> = Vec::new();

            // Show import-resolved results first with marker
            if !import_results.is_empty() {
                body_parts.push(format!(
                    "## 📦 Import-resolved ({} via import analysis)\n",
                    import_results.len()
                ));
                body_parts.push(import_results.join("\n"));
                if !results.is_empty() {
                    body_parts.push(format!("\n\n## Other definitions ({})\n", results.len()));
                    body_parts.push(results.join("\n"));
                }
            } else {
                body_parts.push(results.join("\n"));
            }

            let header = format!(
                "# Definitions of '{}' ({} found, {} files scanned)\n\n",
                symbol, total_found, files_scanned
            );
            truncate_output(
                format!("{}{}", header, body_parts.join("")),
                per_tool_output_limit("find_definition"),
            )
        }
    }

    /// Find all references to a symbol across the codebase.
    /// Uses grep for speed, with word-boundary matching for precision.
    pub(super) fn find_references(&self, args: &Value) -> String {
        let symbol = match args.get("symbol").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'symbol' parameter is required".to_string(),
        };

        let search_path = if let Some(p) = args.get("path").and_then(Value::as_str) {
            self.project_root.join(p)
        } else {
            self.project_root.clone()
        };

        // Build ripgrep command for word-boundary matching
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--no-heading")
            .arg("--line-number")
            .arg("--color=never")
            .arg("--max-count=5") // Max per file
            .arg("-w") // Word boundary
            .current_dir(&self.project_root);

        // Apply include filter
        if let Some(include) = args.get("include").and_then(Value::as_str) {
            cmd.arg("--glob").arg(include);
        }

        // Exclude common noise directories
        cmd.arg("--glob")
            .arg("!.git/")
            .arg("--glob")
            .arg("!node_modules/")
            .arg("--glob")
            .arg("!target/")
            .arg("--glob")
            .arg("!vendor/")
            .arg("--glob")
            .arg("!dist/")
            .arg("--glob")
            .arg("!*.min.js")
            .arg("--glob")
            .arg("!*.min.css");

        // Use fixed string for exact symbol (faster), word-bounded
        cmd.arg(symbol);
        cmd.arg(search_path.to_string_lossy().to_string());

        let kind_filter = args.get("kind").and_then(Value::as_str).unwrap_or("all");
        let ast_validate = args
            .get("validate")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        match cmd.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.is_empty() {
                    return format!("No references found for '{symbol}'");
                }

                let lines: Vec<&str> = stdout.lines().collect();
                let total_grep = lines.len();

                // AST validation: filter out matches in comments/strings
                let validated_lines: Vec<&str> = if ast_validate {
                    self.ast_validate_references(&lines, symbol)
                } else {
                    lines
                };
                let ast_filtered = total_grep - validated_lines.len();

                // Categorize each reference line
                let categorized: Vec<(&str, &str)> = validated_lines
                    .iter()
                    .map(|line| {
                        let category = categorize_reference(line, symbol);
                        (*line, category)
                    })
                    .collect();

                // Apply kind filter
                let filtered: Vec<(&str, &str)> = if kind_filter == "all" {
                    categorized
                } else {
                    categorized
                        .into_iter()
                        .filter(|(_, cat)| *cat == kind_filter)
                        .collect()
                };

                if filtered.is_empty() {
                    return format!("No {kind_filter} references found for '{symbol}'");
                }

                let total = filtered.len();

                // Group by file for cleaner output
                let ast_note = if ast_filtered > 0 {
                    format!(", {} in comments/strings filtered", ast_filtered)
                } else {
                    String::new()
                };
                let mut output = format!(
                    "# References to '{}' ({} found{}{})\n\n",
                    symbol,
                    total,
                    if kind_filter != "all" {
                        format!(", kind={kind_filter}")
                    } else {
                        String::new()
                    },
                    ast_note
                );
                let mut current_file = "";
                for (line, cat) in filtered.iter().take(50) {
                    if let Some(colon_pos) = line.find(':') {
                        let file = &line[..colon_pos];
                        if file != current_file {
                            if !current_file.is_empty() {
                                output.push('\n');
                            }
                            current_file = file;
                        }
                    }
                    output.push_str(&format!("[{cat}] {line}\n"));
                }

                if total > 50 {
                    output.push_str(&format!("\n[{} more references not shown]", total - 50));
                }

                truncate_output(output, per_tool_output_limit("find_references"))
            }
            Err(_) => {
                // Fallback to grep if rg not available
                let out = std::process::Command::new("grep")
                    .args([
                        "-rnw",
                        "--include=*.rs",
                        "--include=*.py",
                        "--include=*.ts",
                        "--include=*.go",
                        "--include=*.java",
                        symbol,
                    ])
                    .arg(search_path.to_string_lossy().to_string())
                    .current_dir(&self.project_root)
                    .output();
                match out {
                    Ok(o) => {
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        if stdout.is_empty() {
                            format!("No references found for '{symbol}'")
                        } else {
                            let lines: Vec<&str> = stdout.lines().take(50).collect();
                            let header =
                                format!("# References to '{}' ({} found)\n\n", symbol, lines.len());
                            truncate_output(
                                format!("{header}{}", lines.join("\n")),
                                per_tool_output_limit("find_references"),
                            )
                        }
                    }
                    Err(e) => format!("Error: search failed: {e}"),
                }
            }
        }
    }

    /// Smart rename: find all AST-validated references to a symbol and replace them.
    pub(super) fn rename_symbol(&self, args: &Value) -> String {
        let symbol = match args.get("symbol").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'symbol' (current name) is required".into(),
        };
        let new_name = match args.get("new_name").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'new_name' is required".into(),
        };
        if symbol == new_name {
            return "Error: symbol and new_name are the same".into();
        }

        // Validate new_name is a valid identifier
        if !new_name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            || !new_name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            return format!("Error: '{}' is not a valid identifier", new_name);
        }

        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);

        // Step 1: Find all references using AST-validated find_references
        let search_path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let include = args.get("include").and_then(Value::as_str);

        let search_dir = self.project_root.join(search_path);
        if !search_dir.exists() {
            return format!("Error: path '{}' not found", search_path);
        }

        // Build search command — try ripgrep first, fall back to grep
        let output = {
            let mut cmd = std::process::Command::new("rg");
            cmd.arg("-n")
                .arg("-w")
                .arg("--no-heading")
                .arg("--max-count")
                .arg("1000")
                .arg(symbol)
                .current_dir(&search_dir);
            if let Some(inc) = include {
                cmd.arg("-g").arg(inc);
            }
            for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                cmd.arg("--glob").arg(format!("!{}", exc));
            }
            match cmd.output() {
                Ok(o) => o,
                Err(_) => {
                    // Fallback to grep
                    let mut cmd = std::process::Command::new("grep");
                    cmd.arg("-rnw").arg(symbol).current_dir(&search_dir);
                    if let Some(inc) = include {
                        cmd.arg("--include").arg(inc);
                    }
                    for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                        cmd.arg("--exclude-dir").arg(*exc);
                    }
                    match cmd.output() {
                        Ok(o) => o,
                        Err(_) => return "Error: neither rg nor grep available".into(),
                    }
                }
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return format!("No references to '{}' found", symbol);
        }

        let lines: Vec<&str> = stdout.lines().collect();
        let total_grep = lines.len();

        // Step 2: AST-validate to filter comments/strings
        let validated = self.ast_validate_references(&lines, symbol);
        let filtered_count = total_grep - validated.len();

        if validated.is_empty() {
            return format!(
                "No code references to '{}' found (all {} matches were in comments/strings)",
                symbol, total_grep
            );
        }

        // Step 3: Group by file and collect line numbers
        let mut by_file: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        for line in &validated {
            if let Some((file, line_num)) = parse_grep_file_line(line) {
                by_file.entry(file.to_string()).or_default().push(line_num);
            }
        }

        // Step 4: Apply or preview replacements
        let mut output = String::new();
        let mut total_replacements = 0usize;
        let mut files_changed = 0usize;

        if dry_run {
            output.push_str(&format!("🔍 Rename preview: {} → {}\n", symbol, new_name));
        } else {
            output.push_str(&format!("✏️  Renaming: {} → {}\n", symbol, new_name));
        }

        for (rel_path, line_nums) in &by_file {
            let abs_path = search_dir.join(rel_path);
            let content = match fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(e) => {
                    output.push_str(&format!("  ⚠ {}: read error: {}\n", rel_path, e));
                    continue;
                }
            };

            let content_lines: Vec<&str> = content.lines().collect();
            let mut replacements_in_file = 0;
            let mut new_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

            // Build word-boundary regex for precise replacement
            let pattern = format!(r"\b{}\b", regex::escape(symbol));
            let re = match regex::Regex::new(&pattern) {
                Ok(r) => r,
                Err(_) => {
                    output.push_str(&format!("  ⚠ {}: invalid regex for symbol\n", rel_path));
                    continue;
                }
            };

            for &line_num in line_nums {
                let idx = line_num.saturating_sub(1);
                if idx >= new_lines.len() {
                    continue;
                }

                // Check this specific occurrence via AST validation before replacing
                let old_line = &new_lines[idx];
                let replaced = re.replace_all(old_line, new_name).to_string();
                if replaced != *old_line {
                    if dry_run {
                        output.push_str(&format!("  {}:{}:\n", rel_path, line_num));
                        output.push_str(&format!("    - {}\n", old_line.trim()));
                        output.push_str(&format!("    + {}\n", replaced.trim()));
                    }
                    new_lines[idx] = replaced;
                    replacements_in_file += 1;
                }
            }

            if replacements_in_file > 0 {
                files_changed += 1;
                total_replacements += replacements_in_file;

                if !dry_run {
                    // Reconstruct file content preserving original line endings
                    let has_trailing_newline = content.ends_with('\n');
                    let mut new_content = new_lines.join("\n");
                    if has_trailing_newline {
                        new_content.push('\n');
                    }

                    if let Err(e) = fs::write(&abs_path, &new_content) {
                        output.push_str(&format!("  ⚠ {}: write error: {}\n", rel_path, e));
                        continue;
                    }
                    output.push_str(&format!(
                        "  ✓ {} ({} replacement{})\n",
                        rel_path,
                        replacements_in_file,
                        if replacements_in_file == 1 { "" } else { "s" }
                    ));
                }
            }
        }

        output.push_str(&format!(
            "\n{} replacement{} in {} file{}",
            total_replacements,
            if total_replacements == 1 { "" } else { "s" },
            files_changed,
            if files_changed == 1 { "" } else { "s" }
        ));

        if filtered_count > 0 {
            output.push_str(&format!(
                " ({} comment/string matches skipped)",
                filtered_count
            ));
        }

        if dry_run {
            output.push_str("\n\n💡 This is a dry run. Set dry_run=false to apply changes.");
        }

        output
    }

    /// Dead code detection: find symbols with zero external references.
    pub(super) fn dead_code(&self, args: &Value) -> String {
        let scan_path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let include = args.get("include").and_then(Value::as_str);
        let kind_filter = args.get("kind").and_then(Value::as_str).unwrap_or("all");

        let scan_dir = self.project_root.join(scan_path);
        if !scan_dir.exists() {
            return format!("Error: path '{}' not found", scan_path);
        }

        // Step 1: Collect files to scan
        let extensions = [
            "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp", "rb",
        ];
        let skip_dirs = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let max_files = 200;

        let files: Vec<std::path::PathBuf> = if scan_dir.is_file() {
            vec![scan_dir.clone()]
        } else {
            self.collect_project_files(&skip_dirs, &extensions, max_files)
                .into_iter()
                .filter(|p| p.starts_with(&scan_dir))
                .filter(|p| {
                    if let Some(inc) = include {
                        let name = p.file_name().unwrap_or_default().to_string_lossy();
                        let pat = inc.trim_start_matches('*');
                        name.ends_with(pat)
                    } else {
                        true
                    }
                })
                .collect()
        };

        if files.is_empty() {
            return format!("No source files found in '{}'", scan_path);
        }

        // Step 2: Extract all symbols from scanned files
        struct SymbolInfo {
            name: String,
            kind: String,
            file: String,
            line: usize,
            is_public: bool,
            is_test: bool,
            is_main: bool,
        }

        let mut symbols: Vec<SymbolInfo> = Vec::new();

        for file_path in &files {
            let lang = match code_intel::detect_language(file_path) {
                Some(l) => l,
                None => continue,
            };
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = file_path
                .strip_prefix(&self.project_root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let extracted = code_intel::extract_symbols(&content, lang);
            for sym in extracted {
                let kind_str = match sym.kind {
                    code_intel::SymbolKind::Function | code_intel::SymbolKind::Method => "function",
                    code_intel::SymbolKind::Struct
                    | code_intel::SymbolKind::Class
                    | code_intel::SymbolKind::Enum
                    | code_intel::SymbolKind::Trait
                    | code_intel::SymbolKind::Interface
                    | code_intel::SymbolKind::Type => "type",
                    code_intel::SymbolKind::Constant => "constant",
                    _ => continue, // skip variables, imports, constructors, modules
                };

                // Apply kind filter
                if kind_filter != "all" && kind_str != kind_filter {
                    continue;
                }

                // Check for known entry points and special patterns
                let is_main = sym.name == "main" || sym.name == "Main";
                let sig = &sym.signature;
                let is_test = sym.name.starts_with("test_")
                    || sym.name.ends_with("_test")
                    || sym.name.starts_with("Test")
                    || sig.contains("#[test]")
                    || sig.contains("#[cfg(test)]");

                // Check visibility
                let is_public = sig.starts_with("pub ")
                    || sig.starts_with("pub(")
                    || sig.starts_with("export ");

                symbols.push(SymbolInfo {
                    name: sym.name,
                    kind: kind_str.to_string(),
                    file: rel.clone(),
                    line: sym.start_line,
                    is_public,
                    is_test,
                    is_main,
                });
            }
        }

        if symbols.is_empty() {
            return format!(
                "No symbols of kind '{}' found in '{}'",
                kind_filter, scan_path
            );
        }

        // Step 3: For each symbol, count references project-wide
        let mut dead: Vec<&SymbolInfo> = Vec::new();
        let mut checked = 0;

        for sym in &symbols {
            // Skip known entry points
            if sym.is_main || sym.is_test {
                continue;
            }

            checked += 1;

            // Quick grep count
            let ref_count = self.count_symbol_references(&sym.name);

            // A symbol with only 1 reference (its own definition) is dead
            // A symbol with 0 references means grep couldn't find it (unlikely but safe)
            if ref_count <= 1 {
                dead.push(sym);
            }
        }

        // Step 4: Format output
        let mut output = String::new();
        if dead.is_empty() {
            output.push_str(&format!(
                "✓ No dead code found ({} symbols checked in {} files)\n",
                checked,
                files.len()
            ));
        } else {
            output.push_str(&format!(
                "⚠ {} potentially unused symbol{} ({} checked in {} files):\n\n",
                dead.len(),
                if dead.len() == 1 { "" } else { "s" },
                checked,
                files.len()
            ));

            // Group by file
            let mut by_file: std::collections::BTreeMap<&str, Vec<&SymbolInfo>> =
                std::collections::BTreeMap::new();
            for sym in &dead {
                by_file.entry(&sym.file).or_default().push(sym);
            }

            for (file, syms) in &by_file {
                output.push_str(&format!("{}:\n", file));
                for sym in syms {
                    let pub_marker = if sym.is_public { " (pub)" } else { "" };
                    output.push_str(&format!(
                        "  L{}: {} {}{}\n",
                        sym.line, sym.kind, sym.name, pub_marker
                    ));
                }
            }

            if dead.iter().any(|s| s.is_public) {
                output.push_str(
                    "\n💡 Public symbols marked (pub) may be used by external consumers.\n",
                );
            }
        }

        output
    }

    /// Count how many times a symbol appears in the project (word-boundary match).
    pub(super) fn count_symbol_references(&self, symbol: &str) -> usize {
        // Try ripgrep first, fall back to grep
        let output = {
            let mut cmd = std::process::Command::new("rg");
            cmd.arg("-c")
                .arg("-w")
                .arg("--no-heading")
                .arg(symbol)
                .current_dir(&self.project_root);
            for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                cmd.arg("--glob").arg(format!("!{}", exc));
            }
            match cmd.output() {
                Ok(o) => o,
                Err(_) => {
                    let mut cmd = std::process::Command::new("grep");
                    cmd.arg("-rcw").arg(symbol).current_dir(&self.project_root);
                    for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                        cmd.arg("--exclude-dir").arg(*exc);
                    }
                    match cmd.output() {
                        Ok(o) => o,
                        Err(_) => return usize::MAX, // can't count, assume referenced
                    }
                }
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Each line is "file:count" — sum all counts
        stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.rsplitn(2, ':').collect();
                parts.first().and_then(|s| s.parse::<usize>().ok())
            })
            .sum()
    }

    // ── extract_members tool ─────────────────────────────────────────────────

    pub(super) fn extract_members(&self, args: &Value) -> String {
        let file = match args.get("file").and_then(Value::as_str) {
            Some(f) => match self.resolve_checked(f) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'file'".to_string(),
        };

        let line = match args.get("line").and_then(Value::as_u64) {
            Some(l) => l as usize,
            None => return "Error: missing 'line'".to_string(),
        };

        let lang = match code_intel::detect_language(&file) {
            Some(l) => l,
            None => {
                return "Error: unsupported language (supported: rs, py, ts, js, go)".to_string();
            }
        };

        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };

        let members = code_intel::extract_members(&source, lang, line);

        if members.is_empty() {
            return format!(
                "No type definition found at line {line} in {}",
                file.display()
            );
        }

        let mut parts = Vec::new();
        let rel_path = file
            .strip_prefix(&self.project_root)
            .unwrap_or(&file)
            .display();
        parts.push(format!("Members of type at {}:{}", rel_path, line));
        parts.push(String::new());

        for m in &members {
            let vis = if m.visibility.is_empty() {
                String::new()
            } else {
                format!("{} ", m.visibility)
            };
            let type_str = if m.type_annotation.is_empty() {
                String::new()
            } else {
                format!(": {}", m.type_annotation)
            };
            let default_str = if m.default_value.is_empty() {
                String::new()
            } else {
                format!(" = {}", m.default_value)
            };
            parts.push(format!(
                "  L{:<4} {}{}{}{} ({})",
                m.line, vis, m.name, type_str, default_str, m.kind
            ));
        }

        parts.push(format!("\nTotal: {} members", members.len()));
        parts.join("\n")
    }

    // ── type_hierarchy tool ──────────────────────────────────────────────────

    pub(super) fn type_hierarchy(&self, args: &Value) -> String {
        let name = match args.get("name").and_then(Value::as_str) {
            Some(n) if !n.trim().is_empty() => n.trim(),
            _ => return "Error: missing 'name'".to_string(),
        };
        let direction = args
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("implementations");
        let include_glob = args
            .get("include")
            .and_then(Value::as_str)
            .unwrap_or("*.rs");

        // Collect Rust source files
        let mut files = Vec::new();
        self.collect_files_with_glob(&self.project_root, include_glob, &mut files);

        let mut all_impls: Vec<code_intel::ImplRelation> = Vec::new();
        for file in &files {
            if let Ok(source) = std::fs::read_to_string(file) {
                let rel_path = file
                    .strip_prefix(&self.project_root)
                    .unwrap_or(file)
                    .to_string_lossy()
                    .to_string();
                let impls = code_intel::find_rust_impls(&source, &rel_path);
                all_impls.extend(impls);
            }
        }

        let mut results: Vec<String> = Vec::new();

        match direction {
            "supertypes" => {
                // Find traits that `name` implements
                results.push(format!("Traits implemented by `{}`:", name));
                let mut found = false;
                for imp in &all_impls {
                    if imp.type_name == name {
                        results.push(format!(
                            "  impl {} — {}:{}",
                            imp.trait_name, imp.file, imp.line
                        ));
                        found = true;
                    }
                }
                if !found {
                    results.push(format!("  (no trait implementations found for `{}`)", name));
                }
            }
            _ => {
                // Find types that implement `name`
                results.push(format!("Types implementing `{}`:", name));
                let mut found = false;
                for imp in &all_impls {
                    if imp.trait_name == name {
                        results.push(format!("  {} — {}:{}", imp.type_name, imp.file, imp.line));
                        found = true;
                    }
                }
                if !found {
                    results.push(format!("  (no implementations found for `{}`)", name));
                }
            }
        }

        results.push(format!("\nScanned {} files", files.len()));
        results.join("\n")
    }

    // ── hover_info tool ──────────────────────────────────────────────────

    pub(super) fn hover_info(&self, args: &Value) -> String {
        let file = match args.get("file").and_then(Value::as_str) {
            Some(f) => match self.resolve_checked(f) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'file'".to_string(),
        };
        let line = match args.get("line").and_then(Value::as_u64) {
            Some(l) => l as usize,
            None => return "Error: missing 'line'".to_string(),
        };
        let column = args.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;

        let lang = match code_intel::detect_language(&file) {
            Some(l) => l,
            None => return "Error: unsupported language".to_string(),
        };
        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };
        let rel_path = file.strip_prefix(&self.project_root).unwrap_or(&file);

        let mut parts = Vec::new();

        // Step 1: Identify what's at cursor
        let cursor_ident = code_intel::identifier_at_position(&source, lang, line, column);
        if let Some((ref name, ref node_kind)) = cursor_ident {
            parts.push(format!("🔍 `{}` ({})", name, node_kind));
        }

        // Step 2: Scope breadcrumbs
        let scope = code_intel::scope_at_line(&source, lang, line);
        if !scope.breadcrumbs.is_empty() {
            parts.push(format!("📍 {}", scope.breadcrumbs.join(" → ")));
        }

        // Step 3: Symbol definition at this line
        let symbols = code_intel::extract_symbols(&source, lang);
        let at_line: Vec<&code_intel::Symbol> =
            symbols.iter().filter(|s| s.start_line == line).collect();

        // Also try to find the definition of the cursor identifier
        let cursor_def = cursor_ident
            .as_ref()
            .and_then(|(name, _)| symbols.iter().find(|s| &s.name == name));

        let primary_sym = at_line.first().copied().or(cursor_def);

        if let Some(sym) = primary_sym {
            let parent_info = sym
                .parent
                .as_ref()
                .map(|p| format!(" (in {})", p))
                .unwrap_or_default();
            parts.push(String::new());
            parts.push(format!(
                "▸ {} {}{}",
                sym.kind.as_str(),
                sym.signature,
                parent_info
            ));
            parts.push(format!(
                "  {}:{}–{}",
                rel_path.display(),
                sym.start_line,
                sym.end_line
            ));

            // Doc comment
            let doc = code_intel::extract_doc_comment(&source, lang, sym.start_line);
            if !doc.is_empty() {
                parts.push(String::new());
                for doc_line in doc.lines().take(5) {
                    parts.push(format!("  📝 {}", doc_line));
                }
            }

            // If it's a type, show members preview
            if matches!(
                sym.kind,
                code_intel::SymbolKind::Struct
                    | code_intel::SymbolKind::Enum
                    | code_intel::SymbolKind::Class
                    | code_intel::SymbolKind::Interface
                    | code_intel::SymbolKind::Trait
            ) {
                let members = code_intel::extract_members(&source, lang, sym.start_line);
                if !members.is_empty() {
                    parts.push(String::new());
                    parts.push(format!("  Members ({}):", members.len()));
                    for m in members.iter().take(10) {
                        let type_str = if m.type_annotation.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", m.type_annotation)
                        };
                        parts.push(format!("    {} {}{}", m.kind, m.name, type_str));
                    }
                    if members.len() > 10 {
                        parts.push(format!("    ... +{} more", members.len() - 10));
                    }
                }
            }

            // Calls made by this function
            if matches!(
                sym.kind,
                code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
            ) {
                let calls = code_intel::extract_calls(&source, lang, sym.start_line, sym.end_line);
                if !calls.is_empty() {
                    parts.push(String::new());
                    let call_names: Vec<String> = calls
                        .iter()
                        .take(8)
                        .map(|c| {
                            if let Some(ref r) = c.receiver {
                                format!("{}.{}", r, c.callee)
                            } else {
                                c.callee.clone()
                            }
                        })
                        .collect();
                    parts.push(format!("  Calls: {}", call_names.join(", ")));
                    if calls.len() > 8 {
                        parts.push(format!("    +{} more", calls.len() - 8));
                    }
                }
            }

            // Usage count
            let ref_count = self.count_symbol_references(&sym.name);
            if ref_count < usize::MAX {
                parts.push(format!("  Referenced: {} times in project", ref_count));
            }
        } else if let Some((name, _)) = &cursor_ident {
            // Not a definition line, but we found an identifier — show usage count
            let ref_count = self.count_symbol_references(name);
            if ref_count < usize::MAX {
                parts.push(format!("  Referenced: {} times in project", ref_count));
            }
        } else {
            // Show source line for context
            let lines: Vec<&str> = source.lines().collect();
            if line > 0 && line <= lines.len() {
                parts.push(format!("Line {}: {}", line, lines[line - 1].trim()));
            }
        }

        if parts.is_empty() {
            return format!("No symbol information at {}:{}", rel_path.display(), line);
        }

        parts.join("\n")
    }

    // ── symbol_search tool ───────────────────────────────────────────────

    pub(super) fn symbol_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.trim().to_lowercase(),
            _ => return "Error: missing 'query'".to_string(),
        };
        let kind_filter = args.get("kind").and_then(Value::as_str).unwrap_or("all");
        let include_glob = args.get("include").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;

        let extensions = [
            "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp", "rb",
        ];
        let skip_dirs = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let files = self.collect_project_files(&skip_dirs, &extensions, 300);

        struct Match {
            name: String,
            kind: String,
            file: String,
            line: usize,
            signature: String,
            score: usize, // lower = better
        }

        let mut matches: Vec<Match> = Vec::new();

        for file_path in &files {
            // Apply glob filter
            if let Some(inc) = include_glob {
                let name = file_path.file_name().unwrap_or_default().to_string_lossy();
                let pat = inc.trim_start_matches('*');
                if !name.ends_with(pat) {
                    continue;
                }
            }

            let lang = match code_intel::detect_language(file_path) {
                Some(l) => l,
                None => continue,
            };
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = file_path
                .strip_prefix(&self.project_root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let symbols = code_intel::extract_symbols(&content, lang);
            for sym in symbols {
                // Kind filter
                let kind_str = sym.kind.as_str();
                match kind_filter {
                    "function" if kind_str != "fn" && kind_str != "method" => continue,
                    "type"
                        if !matches!(
                            sym.kind,
                            code_intel::SymbolKind::Struct
                                | code_intel::SymbolKind::Class
                                | code_intel::SymbolKind::Enum
                                | code_intel::SymbolKind::Interface
                                | code_intel::SymbolKind::Trait
                                | code_intel::SymbolKind::Type
                        ) =>
                    {
                        continue;
                    }
                    "method" if kind_str != "method" => continue,
                    "constant" if kind_str != "const" && kind_str != "var" => continue,
                    _ => {}
                }

                let name_lower = sym.name.to_lowercase();
                if !name_lower.contains(&query) {
                    continue;
                }

                // Score: exact match = 0, starts-with = 1, contains = 2
                let score = if name_lower == query {
                    0
                } else if name_lower.starts_with(&query) {
                    1
                } else {
                    2
                };

                matches.push(Match {
                    name: sym.name,
                    kind: kind_str.to_string(),
                    file: rel.clone(),
                    line: sym.start_line,
                    signature: sym.signature,
                    score,
                });
            }
        }

        // Sort by score (exact first), then by name
        matches.sort_by(|a, b| a.score.cmp(&b.score).then(a.name.cmp(&b.name)));
        matches.truncate(limit);

        if matches.is_empty() {
            return format!("No symbols matching '{}' found", query);
        }

        let mut parts = Vec::new();
        parts.push(format!(
            "Symbols matching '{}' ({} results):",
            query,
            matches.len()
        ));
        parts.push(String::new());

        for m in &matches {
            let sig = if m.signature.len() > 80 {
                let mut end = 80;
                while !m.signature.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                format!("{}...", &m.signature[..end])
            } else {
                m.signature.clone()
            };
            parts.push(format!("  [{}] {} — {}:{}", m.kind, sig, m.file, m.line));
        }

        parts.join("\n")
    }

    pub(super) fn call_graph(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'path'".to_string(),
        };

        let lang = match code_intel::detect_language(&path) {
            Some(l) => l,
            None => {
                return "Error: unsupported language (supported: rs, py, ts, go, java, c, cpp, rb)"
                    .to_string();
            }
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };

        // Determine the line range to analyze
        let (start_line, end_line) = if let Some(sym_name) =
            args.get("symbol").and_then(Value::as_str)
        {
            // Find the symbol by name
            let symbols = code_intel::extract_symbols(&content, lang);
            let matches: Vec<_> = symbols.iter().filter(|s| s.name == sym_name).collect();
            match matches.len() {
                0 => return format!("Error: symbol '{sym_name}' not found in file"),
                1 => (matches[0].start_line, matches[0].end_line),
                _ => {
                    // Multiple matches — show them and ask for disambiguation
                    let mut msg = format!("Multiple symbols named '{sym_name}':\n");
                    for s in &matches {
                        msg.push_str(&format!(
                            "  L{}-{}: {} {}\n",
                            s.start_line,
                            s.end_line,
                            s.kind.as_str(),
                            s.signature
                        ));
                    }
                    msg.push_str("Use start_line/end_line to specify which one.");
                    return msg;
                }
            }
        } else if let (Some(sl), Some(el)) = (
            args.get("start_line").and_then(Value::as_u64),
            args.get("end_line").and_then(Value::as_u64),
        ) {
            (sl as usize, el as usize)
        } else {
            return "Error: provide either 'symbol' name or 'start_line'+'end_line'".to_string();
        };

        let calls = code_intel::extract_calls(&content, lang, start_line, end_line);

        let fname = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let show_callers = args
            .get("callers")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut out = String::new();

        // Outgoing calls (what this function calls)
        if !calls.is_empty() {
            out.push_str(&format!(
                "# Calls FROM {} (lines {}-{})\n\n",
                fname, start_line, end_line
            ));
            for call in &calls {
                if let Some(ref recv) = call.receiver {
                    out.push_str(&format!("  → L{}: {}.{}()\n", call.line, recv, call.callee));
                } else {
                    out.push_str(&format!("  → L{}: {}()\n", call.line, call.callee));
                }
            }
            out.push_str(&format!("\n{} outgoing call(s)\n", calls.len()));
        } else {
            out.push_str(&format!(
                "No outgoing calls in lines {start_line}-{end_line}\n"
            ));
        }

        // Callers search
        if show_callers {
            let sym_name = args.get("symbol").and_then(Value::as_str);
            if let Some(target) = sym_name {
                let scope = args.get("scope").and_then(Value::as_str).unwrap_or("file");

                if scope == "project" {
                    // Cross-file caller search
                    out.push_str(&format!("\n# Callers OF '{}' (project-wide)\n\n", target));
                    let callers = self.find_callers_cross_file(target, &path);
                    if callers.is_empty() {
                        out.push_str("  (none found in project)\n");
                    } else {
                        for (file, name, sig, line) in callers.iter().take(30) {
                            out.push_str(&format!("  ← {}:L{}: {} ({})\n", file, line, name, sig));
                        }
                        if callers.len() > 30 {
                            out.push_str(&format!(
                                "\n  ... and {} more callers\n",
                                callers.len() - 30
                            ));
                        }
                        out.push_str(&format!("\n{} caller(s) across project\n", callers.len()));
                    }
                } else {
                    // Same-file caller search (fast)
                    let all_symbols = code_intel::extract_symbols(&content, lang);
                    let mut callers_found = Vec::new();

                    for sym in &all_symbols {
                        if sym.name == target {
                            continue;
                        }
                        if !matches!(
                            sym.kind,
                            code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
                        ) {
                            continue;
                        }
                        let sym_calls =
                            code_intel::extract_calls(&content, lang, sym.start_line, sym.end_line);
                        for call in &sym_calls {
                            if call.callee == target {
                                callers_found.push((
                                    sym.name.clone(),
                                    sym.signature.clone(),
                                    call.line,
                                ));
                                break;
                            }
                        }
                    }

                    out.push_str(&format!("\n# Callers OF '{}' (same file)\n\n", target));
                    if callers_found.is_empty() {
                        out.push_str("  (none found in this file)\n");
                    } else {
                        for (name, sig, line) in &callers_found {
                            out.push_str(&format!("  ← L{}: {} ({})\n", line, name, sig));
                        }
                        out.push_str(&format!("\n{} caller(s) in file\n", callers_found.len()));
                    }
                }
            } else {
                out.push_str("\nNote: callers=true requires symbol name (not line range)\n");
            }
        }

        out
    }

    /// Run a build/test command with structured error parsing and auto-context.
    ///
    /// Returns structured errors with file:line:col locations plus surrounding
    /// source code for each error, enabling single-shot fix without extra read_file calls.
    pub(super) fn run_build_test(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) if !c.trim().is_empty() => c.trim(),
            _ => return "Error: 'command' parameter is required".to_string(),
        };
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(5) as usize;
        let auto_fix = args
            .get("auto_fix")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let abort_on_regression = args
            .get("abort_on_regression")
            .and_then(Value::as_bool)
            .unwrap_or(true); // default: abort on regression
        let report_only = args
            .get("report_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Run the initial build
        let (initial_output, initial_fixes, initial_errors) =
            self.run_build_test_core(command, context_lines);

        // Report-only mode: show what auto-fix would do, but don't apply
        if report_only && !initial_fixes.is_empty() {
            let eligible: Vec<&build_test::FixSuggestion> = initial_fixes
                .iter()
                .filter(|f| f.confidence >= build_test::AUTO_FIX_CONFIDENCE_THRESHOLD)
                .collect();
            if !eligible.is_empty() {
                let mut preview = initial_output;
                preview.push_str("\n\n─── Auto-Fix Preview (report_only=true, not applied) ───\n");
                for (i, fix) in eligible.iter().enumerate() {
                    let conf = format!("{:.0}%", fix.confidence * 100.0);
                    preview.push_str(&format!(
                        "  {}. [{}] {}:{} — {} ({})\n",
                        i + 1,
                        fix.action,
                        fix.file,
                        fix.line,
                        fix.explanation,
                        conf,
                    ));
                    if !fix.new_text.is_empty() {
                        let text = truncate_str(&fix.new_text, 77);
                        preview.push_str(&format!("     + {}\n", text));
                    }
                }
                preview.push_str(&format!(
                    "\n{} fix(es) eligible. Re-run with auto_fix=true to apply.\n",
                    eligible.len()
                ));
                return truncate_output(preview, tool_output_limit());
            }
            return initial_output;
        }

        if !auto_fix {
            return initial_output;
        }

        // Auto-fix loop: apply high-confidence fixes and re-run
        let mut output = initial_output.clone();
        let mut current_fixes: Vec<build_test::FixSuggestion> = initial_fixes;
        let mut all_reports = Vec::new();
        let mut prev_error_count = initial_errors;

        for iteration in 1..=build_test::AUTO_FIX_MAX_ITERATIONS {
            let eligible_count = current_fixes
                .iter()
                .filter(|f| f.confidence >= build_test::AUTO_FIX_CONFIDENCE_THRESHOLD)
                .count();

            if eligible_count == 0 {
                break;
            }

            let (applied, errors) =
                build_test::apply_auto_fixes(&current_fixes, &self.project_root);
            let report = build_test::format_auto_fix_report(&applied, &errors, iteration);
            all_reports.push(report);

            if applied.is_empty() {
                break;
            }

            // Re-run the build after applying fixes
            let (new_output, new_fixes, new_error_count) =
                self.run_build_test_core(command, context_lines);

            // Check for regression: more errors after fix attempt
            if abort_on_regression && new_error_count > prev_error_count && prev_error_count > 0 {
                // Revert applied fixes via git checkout
                let reverted = self.revert_auto_fixes(&applied);
                all_reports.push(format!(
                    "\n⚠ REGRESSION: {} → {} errors. Auto-fix aborted.{}\n",
                    prev_error_count,
                    new_error_count,
                    if reverted {
                        " Files reverted to pre-fix state."
                    } else {
                        " Manual revert may be needed."
                    }
                ));
                // Re-run to get clean output after revert
                let (reverted_output, _, _) = self.run_build_test_core(command, context_lines);
                output = reverted_output;
                break;
            }

            prev_error_count = new_error_count;
            output = new_output;
            current_fixes = new_fixes;
        }

        if all_reports.is_empty() {
            return output;
        }

        // Prepend auto-fix reports to the final build output
        let mut final_output = all_reports.join("");
        final_output.push_str("\n── Final Build Result ──\n");
        final_output.push_str(&output);
        truncate_output(final_output, tool_output_limit())
    }

    /// Revert files modified by auto-fix using git checkout.
    /// Returns true if revert succeeded.
    pub(super) fn revert_auto_fixes(&self, applied: &[build_test::AppliedFix]) -> bool {
        let files: std::collections::HashSet<&str> =
            applied.iter().map(|a| a.file.as_str()).collect();
        let mut all_ok = true;
        for file in files {
            let file_path = if std::path::Path::new(file).is_absolute() {
                file.to_string()
            } else {
                self.project_root.join(file).display().to_string()
            };
            let status = std::process::Command::new("git")
                .args(["checkout", "--", &file_path])
                .current_dir(&self.project_root)
                .status();
            if status.map(|s| !s.success()).unwrap_or(true) {
                all_ok = false;
            }
        }
        all_ok
    }

    /// Core build+parse logic extracted for auto-fix loop reuse.
    /// Returns (formatted_output, fix_suggestions, error_count).
    pub(super) fn run_build_test_core(
        &self,
        command: &str,
        context_lines: usize,
    ) -> (String, Vec<build_test::FixSuggestion>, usize) {
        // Run the command
        let output = std::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(&self.project_root)
            .output();

        let (stdout, stderr, exit_code) = match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let code = out.status.code();
                (stdout, stderr, code)
            }
            Err(e) => return (format!("Error: failed to run command: {e}"), Vec::new(), 0),
        };

        let combined = format!("{stdout}\n{stderr}");
        let mut result = build_test::parse_build_test_output(&combined, exit_code);
        let error_count = result.error_count;

        // Enrich error locations with tree-sitter scope context
        if !result.error_locations.is_empty() {
            result.enrich_with_scope(&self.project_root);
        }

        // Track iteration deltas — reset if command changed
        let delta = {
            let mut tracker = self
                .build_test_tracker
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if tracker.command_changed(command) {
                tracker.reset();
            }
            tracker.record(&result, command)
        };

        // Build the structured output
        let mut parts = Vec::new();

        // Prepend delta summary for iterations > 0
        let delta_summary = delta.to_summary();
        if !delta_summary.is_empty() {
            parts.push(delta_summary);
            parts.push(String::new());
        }

        parts.push(result.to_enhanced_output(&combined));

        // Auto-read source context for each error location
        if !result.error_locations.is_empty() {
            parts.push(String::new());
            parts.push("─── Source Context ───".to_string());

            let mut seen_files: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for loc in result.error_locations.iter().take(5) {
                let file_path = self.project_root.join(&loc.file);
                let file_key = format!("{}:{}", loc.file, loc.line);
                if seen_files.contains(&file_key) {
                    continue;
                }
                seen_files.insert(file_key);

                if let Ok(content) = std::fs::read_to_string(&file_path) {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = loc.line.saturating_sub(context_lines + 1);
                    let end = (loc.line + context_lines).min(lines.len());

                    let code_part = if loc.error_code.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", loc.error_code)
                    };
                    parts.push(format!(
                        "\n// {}:{}{} — {}",
                        loc.file, loc.line, code_part, loc.message
                    ));

                    for (idx, line) in lines[start..end].iter().enumerate() {
                        let line_num = start + idx + 1;
                        let marker = if line_num == loc.line { "→" } else { " " };
                        parts.push(format!("{marker} {line_num:>4} │ {line}"));
                    }
                }
            }

            if result.error_locations.len() > 5 {
                parts.push(format!(
                    "\n[{} more error locations — use read_file to inspect]",
                    result.error_locations.len() - 5
                ));
            }
        }

        // Generate concrete fix suggestions
        let mut all_fixes: Vec<(usize, build_test::FixSuggestion)> = Vec::new();
        for (i, loc) in result.error_locations.iter().enumerate().take(10) {
            let file_path = self.project_root.join(&loc.file);
            if let Ok(content) = std::fs::read_to_string(&file_path) {
                let source_lines: Vec<&str> = content.lines().collect();
                let fixes = build_test::suggest_fix(loc, &source_lines);
                for fix in fixes {
                    all_fixes.push((i, fix));
                }
            }
        }

        // Collect fix suggestions for return
        let fix_list: Vec<build_test::FixSuggestion> =
            all_fixes.iter().map(|(_, f)| f.clone()).collect();

        if !all_fixes.is_empty() {
            parts.push(String::new());
            parts.push("─── Suggested Fixes ───".to_string());
            for (err_idx, fix) in all_fixes.iter().take(8) {
                let confidence_bar = match fix.confidence {
                    c if c >= 0.8 => "●●●",
                    c if c >= 0.5 => "●●○",
                    _ => "●○○",
                };
                parts.push(format!(
                    "\n{}  [{}] {}",
                    confidence_bar, fix.action, fix.explanation
                ));
                parts.push(format!("  → {}:{}", fix.file, fix.line));
                if !fix.new_text.is_empty() {
                    // Show what to insert/replace
                    let preview = truncate_str(&fix.new_text, 77);
                    parts.push(format!("  + {}", preview));
                }
                let _ = err_idx; // used for ordering
            }
            if all_fixes.len() > 8 {
                parts.push(format!("\n[{} more suggestions]", all_fixes.len() - 8));
            }
        }

        (
            truncate_output(parts.join("\n"), tool_output_limit()),
            fix_list,
            error_count,
        )
    }
}
