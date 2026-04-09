use super::*;
use astra_runtime::str_preview::truncate_str;

/// Check if a path is a UNC path (Windows network path that could leak NTLM credentials).
fn is_unc_path(path: &str) -> bool {
    path.starts_with("\\\\") || path.starts_with("//")
}

/// Check if a path is a dangerous/sensitive file that should warn the user.
pub(crate) fn is_dangerous_write_target(rel_path: &str) -> Option<&'static str> {
    const DANGEROUS_FILES: &[(&str, &str)] = &[
        (
            ".gitconfig",
            "Git configuration — changes affect all git operations",
        ),
        (
            ".gitmodules",
            "Git submodule config — can change repository references",
        ),
        (
            ".bashrc",
            "Bash startup — changes affect all future shell sessions",
        ),
        (
            ".bash_profile",
            "Bash login — changes affect all future login sessions",
        ),
        (
            ".zshrc",
            "Zsh startup — changes affect all future shell sessions",
        ),
        (
            ".zprofile",
            "Zsh login — changes affect all future login sessions",
        ),
        (
            ".profile",
            "Shell profile — changes affect all future sessions",
        ),
        (
            ".ssh/config",
            "SSH config — changes affect all SSH connections",
        ),
        (
            ".ssh/authorized_keys",
            "SSH keys — changes affect server access",
        ),
        (".npmrc", "NPM config — can change registry or auth tokens"),
        (".env", "Environment variables — may contain secrets"),
        (".env.local", "Local env variables — may contain secrets"),
        (
            ".aws/credentials",
            "AWS credentials — changes affect cloud access",
        ),
        (".aws/config", "AWS config — changes affect cloud access"),
        (
            ".kube/config",
            "Kubernetes config — changes affect cluster access",
        ),
        (
            ".docker/config.json",
            "Docker config — may contain registry auth tokens",
        ),
    ];
    let filename = rel_path.rsplit('/').next().unwrap_or(rel_path);
    for (name, reason) in DANGEROUS_FILES {
        if filename == *name || rel_path.ends_with(name) {
            return Some(reason);
        }
    }
    // Check dangerous directories
    if rel_path.starts_with(".git/") || rel_path.contains("/.git/") {
        return Some("Git internals — corruption risk");
    }
    // .env.* variants (e.g. .env.production, .env.staging)
    if filename.starts_with(".env.") {
        return Some("Environment variables — may contain secrets");
    }
    // .ssh key files
    if rel_path.contains(".ssh/") && (filename.starts_with("id_") || filename == "authorized_keys2")
    {
        return Some("SSH key file — changes affect authentication");
    }
    None
}

impl ToolExecutor {
    /// Resolve path with explicit error when sandbox blocks it.
    pub(crate) fn resolve_checked(&self, path: &str) -> Result<PathBuf, String> {
        if is_unc_path(path) {
            return Err("Error: UNC/network paths are not supported (security risk)".to_string());
        }
        let p = Path::new(path);
        let resolved = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.project_root.join(p)
        };

        // Symlink loop / depth guard: canonicalize to detect circular symlinks.
        // Skip for non-existent paths (let the caller produce a clear NotFound).
        if resolved.exists() {
            match resolved.canonicalize() {
                Ok(_) => {}                                              // reachable — no loop
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {} // race — let caller handle
                Err(e) => {
                    return Err(format!(
                        "Error: cannot resolve '{}' (possible symlink loop or broken link): {e}",
                        path
                    ));
                }
            }
        }

        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
        {
            return validate_path(policy, path).map_err(|e| {
                if e.is_boundary_violation() {
                    // Use structured prefix so the agentic loop can detect sandbox
                    // denials and prompt the user for authorization instead of
                    // letting the model silently fall back to bash.
                    format!(
                        "{}Path '{}' is outside the project directory '{}'. \
                         Ask the user for permission before accessing files outside the project.",
                        super::SANDBOX_DENIED_PREFIX,
                        path,
                        policy.project_root.display(),
                    )
                } else {
                    format!("Sandbox: {e}")
                }
            });
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

        // Device file blocking — prevent hangs on infinite/blocking device files
        {
            let path_str_lower = path.to_string_lossy().to_lowercase();
            const BLOCKED_DEVICE_PATHS: &[&str] = &[
                "/dev/zero",
                "/dev/random",
                "/dev/urandom",
                "/dev/full",
                "/dev/stdin",
                "/dev/tty",
                "/dev/console",
                "/dev/stdout",
                "/dev/stderr",
                "/dev/null",
            ];
            for blocked in BLOCKED_DEVICE_PATHS {
                if path_str_lower.starts_with(blocked) {
                    return format!(
                        "Error: refusing to read device file '{}' (would block or produce infinite output)",
                        blocked
                    );
                }
            }
            if path_str_lower.starts_with("/proc/") && path_str_lower.contains("/fd/") {
                return "Error: refusing to read /proc fd paths (would block)".to_string();
            }
        }

        // Binary file guard — refuse to read known binary extensions, but allow images
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();

            // Image files: return base64 for vision models
            const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
            if IMAGE_EXTS.contains(&ext_lower.as_str()) {
                // Check file size before reading — base64 inflates by ~33%
                if let Ok(meta) = fs::metadata(&path)
                    && meta.len() > 1_500_000
                {
                    return format!(
                        "Error: image too large ({} bytes). Use bash to resize first.",
                        meta.len()
                    );
                }
                match fs::read(&path) {
                    Ok(bytes) => {
                        use base64::Engine;
                        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                        let mime = match ext_lower.as_str() {
                            "png" => "image/png",
                            "jpg" | "jpeg" => "image/jpeg",
                            "gif" => "image/gif",
                            "bmp" => "image/bmp",
                            "webp" => "image/webp",
                            _ => "application/octet-stream",
                        };
                        self.record_read(&path, false, ReadDedupKey::Full);
                        return format!("data:{mime};base64,{b64}");
                    }
                    Err(e) => return format!("Error reading image: {e}"),
                }
            }

            // Other binary files: block
            const BINARY_EXTS: &[&str] = &[
                "svg", "pdf", "zip", "gz", "tar", "bz2", "xz", "7z", "rar", "exe", "dll", "so",
                "dylib", "o", "a", "lib", "wasm", "class", "pyc", "pyo", "mp3", "mp4", "avi",
                "mov", "wav", "flac", "ogg", "ttf", "otf", "woff", "woff2", "eot", "sqlite", "db",
                "mdb", "ico",
            ];
            if BINARY_EXTS.contains(&ext_lower.as_str()) {
                return format!(
                    "Error: refusing to read binary file (.{ext}). Use bash with appropriate tools (e.g. file, xxd, strings) for binary analysis."
                );
            }
        }

        // Raw range keys (match Claude Code offset/limit identity for consecutive dedup).
        let start_raw = args.get("start_line").and_then(Value::as_u64);
        let end_raw = args.get("end_line").and_then(Value::as_u64);
        let has_range = start_raw.is_some() || end_raw.is_some();
        let has_outline = args
            .get("outline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let dedup_key = if has_outline {
            ReadDedupKey::Outline
        } else if has_range {
            ReadDedupKey::Range {
                start_line: start_raw,
                end_line: end_raw,
            }
        } else {
            ReadDedupKey::Full
        };

        // Consecutive identical outline/range request + unchanged file → stub before I/O
        // (Claude Code FileReadTool dedup for the same offset/limit as last read).
        if self.can_dedup_identical_partial_read(&path, &dedup_key) {
            return format!(
                "[Same read_file request as immediately before — file unchanged. \
                 Refer to the earlier read_file result for {path_str}.]"
            );
        }

        // Pre-read size gate: check file size before reading.
        // Large files without a line range should use outline or start_line/end_line.
        // Inspired by Claude Code's maxSizeBytes (256KB) pre-read check.
        if !has_range
            && !has_outline
            && let Ok(meta) = fs::metadata(&path)
        {
            let size = meta.len() as usize;
            let limit = self.scaled_output_limit();
            if size > limit {
                let total_lines = size / 40; // rough estimate
                return format!(
                    "Error: file is too large ({} bytes, ~{} lines). \
                     Use start_line/end_line to read a specific range, \
                     or outline=true to see definitions only.",
                    size, total_lines
                );
            }
        }

        // Aggregate output gate: when cumulative tool output this turn is
        // already high and a full-file read would be too large, auto-downgrade
        // to outline mode instead of blocking. Ranged reads are always allowed
        // (they're already targeted). Inspired by Claude Code's approach of
        // never blocking tool calls but degrading gracefully.
        if !has_range && !has_outline {
            let agg = self
                .aggregate_output_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
            if agg > super::AGGREGATE_SOFT_LIMIT {
                if let Ok(meta) = fs::metadata(&path) {
                    let size = meta.len() as usize;
                    let remaining = super::AGGREGATE_OUTPUT_BUDGET.saturating_sub(agg);
                    if size > remaining {
                        // Auto-downgrade: return outline instead of full content
                        let content_for_outline = match read_to_string_lossy(&path) {
                            Ok(c) => c,
                            Err(e) => return format!("Error: {e}"),
                        };
                        let total_lines = content_for_outline.lines().count();
                        self.record_read(&path, true, ReadDedupKey::Outline);

                        if let Some(ts_lang) = super::code_intel::detect_language(&path) {
                            let outline =
                                super::code_intel::generate_outline(&content_for_outline, ts_lang);
                            if !outline.is_empty() {
                                let def_count = outline.lines().count();
                                return format!(
                                    "[Auto-downgraded to outline — aggregate output budget is high \
                                     ({agg} bytes used). Use start_line/end_line to read specific sections.]\n\
                                     # Outline ({total_lines} lines, {def_count} symbols)\n{outline}"
                                );
                            }
                        }

                        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        let lang = detect_language(ext);
                        let outline = extract_outline(&content_for_outline, lang);
                        if !outline.is_empty() {
                            return format!(
                                "[Auto-downgraded to outline — aggregate output budget is high \
                                 ({agg} bytes used). Use start_line/end_line to read specific sections.]\n\
                                 # Outline ({total_lines} lines, {} definitions)\n{}",
                                outline.len(),
                                outline
                                    .iter()
                                    .map(|(line_no, sig)| format!("{line_no}: {sig}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            );
                        }

                        // No outline available — return truncated content with hint
                        let limit = self.scaled_output_limit();
                        let truncated =
                            &content_for_outline[..content_for_outline.floor_char_boundary(limit)];
                        let numbered = add_line_numbers(truncated, 1);
                        return format!(
                            "{numbered}\n[Auto-truncated — aggregate output budget is high \
                             ({agg} bytes used, file has {total_lines} lines). \
                             Use start_line/end_line to read specific sections.]"
                        );
                    }
                }
            }
        }

        let content = match read_to_string_lossy(&path) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("Error: {e}");
                if e.kind() == std::io::ErrorKind::NotFound {
                    let suggestions = self.find_similar_files(path_str);
                    let hint = if !suggestions.is_empty() {
                        format!("\nDid you mean: {}?", suggestions.join(", "))
                    } else {
                        String::new()
                    };
                    let cwd = self.project_root.display();
                    return format!(
                        "{msg}. Note: current working directory is {cwd}. Use list_dir or glob to find the correct path first.{hint}"
                    );
                }
                if e.kind() == std::io::ErrorKind::IsADirectory {
                    return format!("{msg}. Use list_dir instead for directories.");
                }
                if e.kind() == std::io::ErrorKind::PermissionDenied {
                    return format!(
                        "{msg}. Check file permissions or use bash with `sudo cat` if appropriate."
                    );
                }
                return msg;
            }
        };

        // Outline mode: return only definition signatures with line numbers
        if has_outline {
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            let total_lines = content.lines().count();

            // Record as partial read (outline)
            self.record_read(&path, true, ReadDedupKey::Outline);

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

        let start = start_raw.map(|n| n as usize);
        let end = end_raw.map(|n| n as usize);

        let is_ranged = has_range;

        // Dedup: if file was fully read before and hasn't changed, return stub.
        // Works for both ranged and non-ranged reads — once the model has the
        // full file content, any further reads are wasteful.
        if self.can_dedup_read(&path) {
            let msg = if is_ranged {
                format!(
                    "[File already fully read and unchanged — refer to the earlier \
                     read_file result for {path_str}. Do not re-read the same file \
                     in multiple small ranges.]"
                )
            } else {
                format!(
                    "[File unchanged since last read — refer to the earlier \
                     read_file result for {path_str}]"
                )
            };
            return msg;
        }

        // Auto-expand: if same file was previously read in a different range
        // (partial read, mtime unchanged) and file fits in output budget,
        // return the full file to eliminate fragmented multi-range reads.
        // Hard cap at 8 KB (~2000 tokens) to prevent large files from
        // exploding context even if they fit the dynamic output budget.
        const AUTO_EXPAND_MAX_BYTES: usize = 8192;
        if is_ranged && self.was_partially_read_unchanged(&path) {
            let max_chars = self.scaled_output_limit().min(AUTO_EXPAND_MAX_BYTES);
            if let Ok(meta) = fs::metadata(&path)
                && (meta.len() as usize) <= max_chars
                && content.len() <= max_chars
            {
                // Upgrade to full read — future reads will hit can_dedup_read
                self.record_read(&path, false, ReadDedupKey::Full);
                let total_lines = content.lines().count();
                let numbered = add_line_numbers(&content, 1);
                return format!(
                    "[Auto-expanded to full file — this file was already partially read. \
                     {total_lines} lines total. Avoid reading the same file in many small ranges.]\n\
                     {numbered}"
                );
            }
        }

        // Record the read state
        let record_key = if is_ranged {
            ReadDedupKey::Range {
                start_line: start_raw,
                end_line: end_raw,
            }
        } else {
            ReadDedupKey::Full
        };
        self.record_read(&path, is_ranged, record_key);

        // Escalating warning when the same file is read too many times.
        // Inspired by Claude Code's dedup stub behavior: train the model
        // to stop re-reading by making the cost of repetition visible.
        let read_count = self.file_read_count(&path);
        let ranged_count = self.file_ranged_read_count(&path);
        let read_warning = if read_count >= 4 {
            "\n\n⚠ WARNING: This file has been read 4+ times this session. You already \
             have this content — stop re-reading and use the information from earlier reads."
                .to_string()
        } else if read_count >= 3 {
            "\n\n⚠ Note: This file has been read 3 times. Consider using content from \
             earlier reads instead of requesting more ranges."
                .to_string()
        } else if is_ranged && ranged_count >= 3 {
            // Large file read in 3+ different ranges — nudge toward grep
            "\n\n⚠ This file has been read in 3+ different ranges. Use grep to find \
             specific content instead of reading more sections — it uses far fewer tokens."
                .to_string()
        } else {
            String::new()
        };

        if !is_ranged {
            let total_lines = content.lines().count();
            let max_chars = self.scaled_output_limit();
            if content.len() > max_chars {
                let truncated = &content[..content.floor_char_boundary(max_chars)];
                let numbered = add_line_numbers(truncated, 1);
                let mut out = numbered;
                out.push_str(&format!(
                    "\n[truncated — file has {total_lines} lines, use start_line/end_line or outline=true]"
                ));
                if !read_warning.is_empty() {
                    out.push_str(&read_warning);
                }
                return out;
            }
            let numbered = add_line_numbers(&content, 1);
            if read_warning.is_empty() {
                return numbered;
            }
            return format!("{numbered}{read_warning}");
        }
        let lines: Vec<&str> = content.lines().collect();
        let s = start.unwrap_or(1).saturating_sub(1).min(lines.len());
        let e = end.unwrap_or(lines.len()).min(lines.len());
        // Auto-swap if the LLM accidentally reversed start/end.
        let (s, e) = if s > e { (e, s) } else { (s, e) };
        if s >= e {
            return format!(
                "(empty range: start_line {} >= end_line {} or file has only {} lines)",
                s + 1,
                e,
                lines.len()
            );
        }
        let actual_start_line = s + 1; // 1-indexed
        let slice = lines[s..e].join("\n");
        let mut result = truncate_output(
            add_line_numbers(&slice, actual_start_line),
            self.scaled_output_limit(),
        );
        if !read_warning.is_empty() {
            result.push_str(&read_warning);
        }
        result
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

        // Content size guard — prevent writing extremely large files that
        // could exhaust disk space.  10 MB is generous for source files.
        const MAX_WRITE_BYTES: usize = 10 * 1024 * 1024; // 10 MB
        if content.len() > MAX_WRITE_BYTES {
            return json!({
                "success": false,
                "error": format!(
                    "Content too large ({} bytes, limit {}). Break into smaller files or use bash for large writes.",
                    content.len(), MAX_WRITE_BYTES
                )
            }).to_string();
        }

        // Dangerous file guard
        let rel = path.strip_prefix(&self.project_root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy();
        if let Some(warning) = is_dangerous_write_target(&rel_str) {
            return json!({
                "success": false,
                "error": format!("⚠️ Warning: writing to sensitive file '{}' — {}. If intentional, use bash 'echo ... > file' to bypass this guard.", rel_str, warning)
            }).to_string();
        }

        // Staleness check: if file exists, it must have been read first and not modified since
        if path.exists() {
            if let Err(e) = self.check_staleness(&path) {
                return json!({ "success": false, "error": e }).to_string();
            }
            // Require full read (not outline/partial) before overwriting
            if !self.was_fully_read(&path) {
                let rel = path.strip_prefix(&self.project_root).unwrap_or(&path);
                return json!({
                    "success": false,
                    "error": format!(
                        "File was only partially read (outline or line range). Read the full file before overwriting.\n\
                         → Action required: call read_file(\"{}\") (without start_line/end_line) first, then retry.",
                        rel.to_string_lossy()
                    )
                }).to_string();
            }
        }

        if let Some(parent) = path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return json!({
                "success": false,
                "error": format!("failed to create parent directory {}: {e}", parent.display())
            })
            .to_string();
        }
        let prior_for_diff = if path.exists() {
            read_to_string_lossy(&path).ok()
        } else {
            None
        };

        // Defense-in-depth: re-check staleness right before writing to catch
        // race conditions between the initial validation and the actual write
        // (e.g. a linter or user modified the file in between).
        if path.exists() {
            if let Err(e) = self.check_staleness(&path) {
                return json!({ "success": false, "error": format!("Pre-write staleness check failed: {e}") }).to_string();
            }
        }

        // Defense-in-depth: re-canonicalize immediately before write to detect
        // symlink swaps (TOCTOU) between the initial resolve_checked and now.
        if path.exists() {
            if let Ok(canonical) = path.canonicalize() {
                if !canonical.starts_with(&self.project_root) {
                    return json!({
                        "success": false,
                        "error": format!(
                            "Security: path '{}' was replaced with a symlink pointing outside the project",
                            path.display()
                        )
                    }).to_string();
                }
            }
        }

        // Journal: snapshot before-state for undo
        let turn_idx = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let journal_call_id = format!("write_file:{}", path.display());
        if let Ok(mut journal) = self.file_journal.lock() {
            journal.record_before(&path, &journal_call_id, turn_idx);
        }

        match fs::write(&path, content) {
            Ok(_) => {
                // Record write state so subsequent reads/edits know the mtime
                self.record_write(&path);
                // Journal: record after-state
                if let Ok(mut journal) = self.file_journal.lock() {
                    journal.record_after(&path, &journal_call_id, content.as_bytes());
                }
                let old_slice = prior_for_diff.as_deref().unwrap_or("");
                let cli_diff = cap_cli_unified_diff(unified_diff_raw(old_slice, content, &path));
                json!({
                    "success": true,
                    "bytes_written": content.len(),
                    "path": path.to_string_lossy().to_string(),
                    "_cli_unified_diff": cli_diff,
                })
                .to_string()
            }
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
        if old_str == new_str {
            return "Error: old_str and new_str are identical — no change needed".to_string();
        }
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Dangerous file guard
        let rel = path.strip_prefix(&self.project_root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy();
        if let Some(warning) = is_dangerous_write_target(&rel_str) {
            return format!(
                "⚠️ Warning: writing to sensitive file '{}' — {}. If intentional, use bash 'echo ... > file' to bypass this guard.",
                rel_str, warning
            );
        }

        // Staleness check
        if let Err(e) = self.check_staleness(&path) {
            return format!("Error: {e}");
        }

        let content = match read_to_string_lossy(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };
        let count = content.matches(old_str).count();
        if count == 0 {
            // Fallback: try matching with normalized curly/smart quotes.
            // LLMs and copy-paste from docs/web often produce curly quotes
            // (U+2018/2019/201C/201D) that don't match the file's ASCII quotes.
            let norm_count = count_with_quote_normalization(&content, old_str);
            if norm_count == 1 {
                // Found exactly one match after quote normalization
                if let Some(actual) = find_with_quote_normalization(&content, old_str) {
                    let new_content = content.replacen(actual, new_str, 1);
                    if dry_run {
                        return unified_diff(&content, &new_content, &path);
                    }
                    // Defense-in-depth: re-check staleness right before writing.
                    if let Err(e) = self.check_staleness(&path) {
                        return format!("Error: Pre-write staleness check failed: {e}");
                    }
                    match fs::write(&path, &new_content) {
                        Ok(_) => {
                            self.record_write(&path);
                            let format_result = auto_format_file(&path, &self.project_root);
                            if format_result.is_some() {
                                self.record_write(&path);
                            }
                            let mut result = String::from(
                                "Replaced successfully (matched after normalizing curly quotes → ASCII)\n",
                            );
                            let old_lines: Vec<&str> = actual.lines().collect();
                            let new_lines: Vec<&str> = new_str.lines().collect();
                            if old_lines.len().max(new_lines.len()) <= 10 {
                                for l in &old_lines {
                                    result.push_str(&format!("- {l}\n"));
                                }
                                for l in &new_lines {
                                    result.push_str(&format!("+ {l}\n"));
                                }
                            }
                            if let Some(fmt_note) = format_result {
                                result.push_str(&format!("\n{fmt_note}"));
                            }
                            append_str_replace_cli_unified_diff(
                                &mut result,
                                &content,
                                &new_content,
                                &path,
                            );
                            return result;
                        }
                        Err(e) => return format!("Error writing file: {e}"),
                    }
                }
            } else if norm_count > 1 {
                return format!(
                    "Error: old_str found {norm_count} times (after normalizing curly quotes) — must be unique.\n\
                     Hint: Add more surrounding context to old_str to make it unique.\n"
                );
            }
            return str_replace_not_found_hint(&content, old_str);
        }
        if count > 1 && !replace_all {
            return str_replace_ambiguous_hint(&content, old_str, count);
        }

        let new_content = if replace_all {
            content.replace(old_str, new_str)
        } else {
            content.replacen(old_str, new_str, 1)
        };

        // Dry run: show unified diff without writing
        if dry_run {
            return unified_diff(&content, &new_content, &path);
        }

        // Defense-in-depth: re-check staleness right before writing.
        if let Err(e) = self.check_staleness(&path) {
            return format!("Error: Pre-write staleness check failed: {e}");
        }

        // Journal: snapshot before-state for undo
        let turn_idx = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let journal_call_id = format!("str_replace:{}", path.display());
        if let Ok(mut journal) = self.file_journal.lock() {
            journal.record_before_patch(&path, &journal_call_id, turn_idx);
        }

        match fs::write(&path, &new_content) {
            Ok(_) => {
                // Record write state for staleness tracking
                self.record_write(&path);
                // Journal: record after-state
                if let Ok(mut journal) = self.file_journal.lock() {
                    journal.record_after(&path, &journal_call_id, new_content.as_bytes());
                }

                // Auto-format if formatter is available
                let format_result = auto_format_file(&path, &self.project_root);
                // Re-record after format (mtime may have changed)
                if format_result.is_some() {
                    self.record_write(&path);
                }

                // Build a compact diff preview for the LLM and user
                let old_lines: Vec<&str> = old_str.lines().collect();
                let new_lines: Vec<&str> = new_str.lines().collect();
                let diff_lines = old_lines.len().max(new_lines.len());
                let mut result = if diff_lines <= 10 {
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
                };
                if let Some(fmt_note) = format_result {
                    result.push_str(&format!("\n{fmt_note}"));
                }
                if replace_all && count > 1 {
                    result = format!("Replaced {count} occurrences\n{result}");
                }

                // Scope context: show where in the code structure this edit landed
                if let Some(lang) = super::code_intel::detect_language(&path) {
                    let edit_line = content[..content.find(old_str).unwrap_or(0)]
                        .matches('\n')
                        .count()
                        + 1;
                    let scope = super::code_intel::scope_at_line(&new_content, lang, edit_line);
                    if !scope.breadcrumbs.is_empty() {
                        result.push_str(&format!("\n📍 {}", scope.breadcrumbs.join(" > ")));
                    }
                }

                append_str_replace_cli_unified_diff(&mut result, &content, &new_content, &path);
                result
            }
            Err(e) => format!("Error writing file: {e}"),
        }
    }

    pub(crate) fn delete_file(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'path'".to_string(),
        };

        // Safety: refuse .git/ contents
        let rel = path.strip_prefix(&self.project_root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy();
        if rel_str.starts_with(".git/") || rel_str.starts_with(".git\\") || rel_str == ".git" {
            return "Error: refusing to delete .git contents".to_string();
        }

        // Refuse directories
        if path.is_dir() {
            return "Error: refusing to delete a directory. Use bash 'rm -r' if you really need this.".to_string();
        }

        if !path.exists() {
            return format!("Error: file not found: {}", rel_str);
        }

        match fs::remove_file(&path) {
            Ok(_) => {
                self.remove_file_state(&path);
                format!("Deleted: {}", rel_str)
            }
            Err(e) => format!("Error deleting file: {e}"),
        }
    }

    pub(crate) fn multi_edit(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'path'".to_string(),
        };
        let edits = match args.get("edits").and_then(Value::as_array) {
            Some(e) => e,
            None => return "Error: missing 'edits' array".to_string(),
        };
        if edits.is_empty() {
            return "Error: 'edits' array is empty".to_string();
        }
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Staleness check
        if let Err(e) = self.check_staleness(&path) {
            return format!("Error: {e}");
        }

        let content = match read_to_string_lossy(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };

        // Validate all edits first (atomic: all or nothing)
        let mut working = content.clone();
        for (i, edit) in edits.iter().enumerate() {
            let old_str = match edit.get("old_str").and_then(Value::as_str) {
                Some(s) => s,
                None => return format!("Error: edit[{i}] missing 'old_str'"),
            };
            let new_str = match edit.get("new_str").and_then(Value::as_str) {
                Some(s) => s,
                None => return format!("Error: edit[{i}] missing 'new_str'"),
            };
            if old_str == new_str {
                return format!(
                    "Error: edit[{i}] old_str and new_str are identical — no change needed"
                );
            }
            let count = working.matches(old_str).count();
            if count == 0 {
                return format!(
                    "Error: edit[{i}] old_str not found. Aborting all edits.\n{}",
                    str_replace_not_found_hint(&working, old_str)
                );
            }
            if count > 1 {
                return format!(
                    "Error: edit[{i}] old_str matches {count} times (must be unique). Aborting all edits.\n{}",
                    str_replace_ambiguous_hint(&working, old_str, count)
                );
            }
            working = working.replacen(old_str, new_str, 1);
        }

        // Dry run: show diff
        if dry_run {
            return unified_diff(&content, &working, &path);
        }

        // Defense-in-depth: re-check staleness right before writing.
        if let Err(e) = self.check_staleness(&path) {
            return format!("Error: Pre-write staleness check failed: {e}");
        }

        // Apply
        match fs::write(&path, &working) {
            Ok(_) => {
                self.record_write(&path);
                let format_result = auto_format_file(&path, &self.project_root);
                if format_result.is_some() {
                    self.record_write(&path);
                }
                let mut result = format!("Applied {} edit(s) successfully", edits.len());
                if let Some(fmt_note) = format_result {
                    result.push_str(&format!("\n{fmt_note}"));
                }

                // Scope context for the first edit location
                if let Some(lang) = super::code_intel::detect_language(&path)
                    && let Some(first_old) = edits
                        .first()
                        .and_then(|e| e.get("old_str"))
                        .and_then(Value::as_str)
                {
                    let edit_line = content[..content.find(first_old).unwrap_or(0)]
                        .matches('\n')
                        .count()
                        + 1;
                    let scope = super::code_intel::scope_at_line(&working, lang, edit_line);
                    if !scope.breadcrumbs.is_empty() {
                        result.push_str(&format!("\n📍 {}", scope.breadcrumbs.join(" > ")));
                    }
                }

                append_str_replace_cli_unified_diff(&mut result, &content, &working, &path);
                result
            }
            Err(e) => format!("Error writing file: {e}"),
        }
    }

    pub(crate) fn list_dir(&self, args: &Value) -> String {
        let dir = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => self.project_root.clone(),
        };
        let depth = args
            .get("depth")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .min(10) as usize; // hard cap at 10 to prevent abuse
        let mut out = String::new();
        let mut visited = std::collections::HashSet::new();
        // Seed with the root dir's canonical path to prevent symlink loops.
        if let Ok(canon) = dir.canonicalize() {
            visited.insert(canon);
        }
        self.list_dir_recursive(&dir, &dir, depth, 0, &mut out, &mut visited);
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
        visited: &mut std::collections::HashSet<std::path::PathBuf>,
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
            let is_dir = ft.as_ref().map(|t| t.is_dir()).unwrap_or(false);
            let is_symlink = ft.as_ref().map(|t| t.is_symlink()).unwrap_or(false);
            out.push_str(&format!(
                "{indent}{}{}\n",
                name,
                if is_dir { "/" } else { "" }
            ));
            if is_dir && cur < max_depth.saturating_sub(1) {
                // Symlink loop guard: canonicalize the target and skip if already visited.
                if is_symlink {
                    if let Ok(canon) = entry.path().canonicalize() {
                        if !visited.insert(canon) {
                            // Already traversed this directory via a different path.
                            continue;
                        }
                    } else {
                        // Broken symlink — skip.
                        continue;
                    }
                }
                self.list_dir_recursive(base, &entry.path(), max_depth, cur + 1, out, visited);
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
            Some(
                self.project_root
                    .join(path.parent().unwrap_or(Path::new(""))),
            )
        };

        // When the parent directory doesn't exist (e.g. crate renamed from
        // mo-agent → astra-cli), fall back to a project-wide filename search
        // so the error message can suggest the correct path immediately instead
        // of forcing the LLM through a glob → read recovery loop.
        let dir_exists = matches!(search_dir, Some(ref d) if d.exists());

        let mut candidates: Vec<(String, usize)> = Vec::new();

        if dir_exists {
            // Fast path: search within the same directory
            let search_dir = search_dir.unwrap();
            Self::collect_similar_in_dir(
                &search_dir,
                &target_name,
                &self.project_root,
                &mut candidates,
            );
        }

        // If no candidates found locally (or dir missing), do a project-wide
        // search for exact filename matches using a bounded walk.
        if candidates.is_empty() {
            Self::collect_exact_name_in_tree(
                &self.project_root,
                &target_name,
                &self.project_root,
                &mut candidates,
            );
        }

        // Sort by score descending and take top 3
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates
            .into_iter()
            .take(3)
            .map(|(path, _)| path)
            .collect()
    }

    /// Collect similar filenames from a single directory.
    fn collect_similar_in_dir(
        dir: &Path,
        target_name: &str,
        project_root: &Path,
        candidates: &mut Vec<(String, usize)>,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let name_str = entry.file_name().to_string_lossy().to_lowercase();
            let mut best = similarity_score(target_name, &name_str);
            if let Some(without_dot) = name_str.strip_prefix('.') {
                best = best.max(similarity_score(target_name, without_dot));
            }
            const MIN_SIMILARITY: usize = 5;
            if best >= MIN_SIMILARITY {
                let display = entry
                    .path()
                    .strip_prefix(project_root)
                    .unwrap_or(&entry.path())
                    .display()
                    .to_string();
                candidates.push((display, best));
            }
        }
    }

    /// Walk the project tree (bounded depth) looking for exact filename matches.
    /// Used when the requested parent directory doesn't exist (e.g. crate rename).
    fn collect_exact_name_in_tree(
        root: &Path,
        target_name: &str,
        project_root: &Path,
        candidates: &mut Vec<(String, usize)>,
    ) {
        const MAX_DEPTH: usize = 8;
        const SKIP_DIRS: &[&str] = &[".git", "node_modules", "target", ".astra", "dist", "build"];

        fn walk(
            dir: &Path,
            target: &str,
            project_root: &Path,
            candidates: &mut Vec<(String, usize)>,
            depth: usize,
        ) {
            if depth > MAX_DEPTH || candidates.len() >= 5 {
                return;
            }
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                if is_dir {
                    if SKIP_DIRS.contains(&name_str.as_ref()) {
                        continue;
                    }
                    walk(&entry.path(), target, project_root, candidates, depth + 1);
                } else if name_str.to_lowercase() == target {
                    let display = entry
                        .path()
                        .strip_prefix(project_root)
                        .unwrap_or(&entry.path())
                        .display()
                        .to_string();
                    // Exact name match in different directory gets high score
                    candidates.push((display, 90));
                }
            }
        }

        walk(root, target_name, project_root, candidates, 0);
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
    let common_prefix = target
        .chars()
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

// ─── Auto-format after edit ────────────────────────────────────────────────

/// Detect project formatter and run it on the edited file.
/// Returns Some(note) if formatter ran, None otherwise.
fn auto_format_file(file_path: &Path, project_root: &Path) -> Option<String> {
    let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");

    let (cmd, args): (&str, Vec<&str>) = match ext {
        "rs" => {
            // Only format if Cargo.toml exists (we're in a Rust project)
            if !project_root.join("Cargo.toml").exists()
                && !project_root.join("rust/Cargo.toml").exists()
            {
                return None;
            }
            ("rustfmt", vec!["--edition", "2021"])
        }
        "py" => {
            // Only if pyproject.toml or .black config exists
            if !project_root.join("pyproject.toml").exists()
                && !project_root.join("setup.cfg").exists()
            {
                return None;
            }
            ("black", vec!["--quiet"])
        }
        "go" => ("gofmt", vec!["-w"]),
        "ts" | "tsx" | "js" | "jsx" | "json" | "css" | "scss" | "html" | "md" | "yaml" | "yml" => {
            // Only if prettier config or package.json exists
            if !project_root.join("package.json").exists()
                && !project_root.join(".prettierrc").exists()
                && !project_root.join(".prettierrc.json").exists()
            {
                return None;
            }
            ("npx", vec!["prettier", "--write"])
        }
        _ => return None,
    };

    let file_str = file_path.to_string_lossy();
    let mut full_args: Vec<&str> = args;
    full_args.push(&file_str);

    let result = std::process::Command::new(cmd)
        .args(&full_args)
        .current_dir(project_root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output();

    match result {
        Ok(out) if out.status.success() => Some(format!("✓ Auto-formatted with {cmd}")),
        Ok(_) => None,  // Formatter failed silently — don't report
        Err(_) => None, // Formatter not available — don't report
    }
}

// ─── unified diff generation ────────────────────────────────────────────────

const CLI_UNIFIED_DIFF_MAX_LINES: usize = 400;

fn cap_cli_unified_diff(s: String) -> String {
    let n = s.lines().count();
    if n <= CLI_UNIFIED_DIFF_MAX_LINES {
        return s;
    }
    s.lines()
        .take(CLI_UNIFIED_DIFF_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n... (_cli_unified_diff truncated)\n"
}

/// Unified diff body (no dry-run banner) for CLI previews and `_cli_unified_diff`.
fn unified_diff_raw(old_content: &str, new_content: &str, path: &std::path::Path) -> String {
    let fname = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());

    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let mut out = format!("--- a/{fname}\n+++ b/{fname}\n");

    // Find first and last differing line
    let max_len = old_lines.len().max(new_lines.len());
    let mut first_diff = max_len;
    let mut last_diff = 0;
    for i in 0..max_len {
        let old_line = old_lines.get(i).copied().unwrap_or("");
        let new_line = new_lines.get(i).copied().unwrap_or("");
        if old_line != new_line {
            if i < first_diff {
                first_diff = i;
            }
            last_diff = i;
        }
    }

    if first_diff > last_diff {
        out.push_str("(no changes)\n");
        return out;
    }

    // Show context around the diff (3 lines before/after)
    let ctx = 3;
    let start = first_diff.saturating_sub(ctx);
    let end = (last_diff + ctx + 1).min(max_len);

    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        end.min(old_lines.len()).saturating_sub(start),
        start + 1,
        end.min(new_lines.len()).saturating_sub(start),
    ));

    for i in start..end {
        let old_line = old_lines.get(i).copied();
        let new_line = new_lines.get(i).copied();
        match (old_line, new_line) {
            (Some(o), Some(n)) if o == n => {
                out.push_str(&format!(" {o}\n"));
            }
            (Some(o), Some(n)) => {
                out.push_str(&format!("-{o}\n"));
                out.push_str(&format!("+{n}\n"));
            }
            (Some(o), None) => {
                out.push_str(&format!("-{o}\n"));
            }
            (None, Some(n)) => {
                out.push_str(&format!("+{n}\n"));
            }
            (None, None) => {}
        }
    }

    out
}

/// Generate a unified diff between old and new content for a given file path.
fn unified_diff(old_content: &str, new_content: &str, path: &std::path::Path) -> String {
    format!(
        "[DRY RUN] Preview of changes (not applied):\n{}",
        unified_diff_raw(old_content, new_content, path)
    )
}

fn append_str_replace_cli_unified_diff(out: &mut String, before: &str, after: &str, path: &Path) {
    use astra_runtime::turn::tool_result_sanitize::{STR_REPLACE_DIFF_END, STR_REPLACE_DIFF_START};
    out.push_str(STR_REPLACE_DIFF_START);
    out.push_str(&cap_cli_unified_diff(unified_diff_raw(before, after, path)));
    out.push_str(STR_REPLACE_DIFF_END);
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

// ─── UTF-8 lossy file reading ───────────────────────────────────────────────

/// Read a file to a String, falling back to lossy UTF-8 conversion for
/// non-UTF-8 files (e.g. Latin-1, UTF-16 with BOM stripped by the OS).
/// Returns a standard `io::Error` for I/O failures.
fn read_to_string_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => Ok(String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

// ─── Line numbers ───────────────────────────────────────────────────────────

/// Add line numbers to content in compact tab-separated format.
/// Example output: `  1\tline content\n  2\tnext line`
fn add_line_numbers(content: &str, start_line: usize) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let max_num = start_line + lines.len().saturating_sub(1);
    let width = max_num.to_string().len().max(1);
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>width$}\t{line}", start_line + i))
        .collect::<Vec<_>>()
        .join("\n")
}

// ─── Quote normalization for str_replace fuzzy matching ──────────────────────

/// Normalize curly/smart quotes to straight ASCII quotes.
/// Handles: ' ' → '  and  " " → "
fn normalize_quotes(s: &str) -> String {
    s.replace(['\u{2018}', '\u{2019}'], "'") // curly single → straight
        .replace(['\u{201C}', '\u{201D}'], "\"") // curly double → straight
}

/// Find the actual substring in `content` that matches `search` after quote
/// normalization. Returns the original bytes from `content` (preserving the
/// file's actual quote characters), or `None` if no match.
fn find_with_quote_normalization<'a>(content: &'a str, search: &str) -> Option<&'a str> {
    let norm_search = normalize_quotes(search);
    let norm_content = normalize_quotes(content);

    let norm_byte_start = norm_content.find(&norm_search)?;
    // Convert byte offset in normalized → char offset (same in both since
    // normalization maps each char 1:1, only byte widths differ).
    let char_start = norm_content[..norm_byte_start].chars().count();
    let char_len = norm_search.chars().count();

    // Map char offset back to byte positions in the original content.
    let byte_start = content.char_indices().nth(char_start).map(|(i, _)| i)?;
    let byte_end = content
        .char_indices()
        .nth(char_start + char_len)
        .map(|(i, _)| i)
        .unwrap_or(content.len());

    Some(&content[byte_start..byte_end])
}

/// Count occurrences of `search` in `content` after normalizing curly quotes.
fn count_with_quote_normalization(content: &str, search: &str) -> usize {
    let norm_search = normalize_quotes(search);
    let norm_content = normalize_quotes(content);
    norm_content.matches(&norm_search).count()
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
        executor.read_file(&serde_json::json!({"path": "code.rs"}));
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
        executor.read_file(&serde_json::json!({"path": "dup.rs"}));
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

    // ── line numbers ─────────────────────────────────────────────────────────

    #[test]
    fn add_line_numbers_basic() {
        assert_eq!(add_line_numbers("a\nb\nc", 1), "1\ta\n2\tb\n3\tc");
    }

    #[test]
    fn add_line_numbers_with_offset() {
        assert_eq!(add_line_numbers("x\ny", 10), "10\tx\n11\ty");
    }

    #[test]
    fn add_line_numbers_padding() {
        // Lines 99-101 should pad to 3 digits
        assert_eq!(add_line_numbers("a\nb\nc", 99), " 99\ta\n100\tb\n101\tc");
    }

    #[test]
    fn read_file_has_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("numbered.txt");
        std::fs::write(&file_path, "hello\nworld\nfoo").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({"path": "numbered.txt"}));
        assert_eq!(result, "1\thello\n2\tworld\n3\tfoo");
    }

    #[test]
    fn read_file_ranged_preserves_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("ranged.txt");
        std::fs::write(&file_path, "a\nb\nc\nd\ne").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({
            "path": "ranged.txt",
            "start_line": 3,
            "end_line": 4
        }));
        assert_eq!(result, "3\tc\n4\td");
    }

    // ── quote normalization ──────────────────────────────────────────────────

    #[test]
    fn normalize_quotes_curly_to_straight() {
        assert_eq!(
            normalize_quotes("say \u{201C}hello\u{201D}"),
            "say \"hello\""
        );
        assert_eq!(normalize_quotes("it\u{2019}s"), "it's");
    }

    #[test]
    fn find_with_quote_normalization_basic() {
        let content = "let x = \u{201C}hello\u{201D};";
        let search = "\"hello\"";
        let found = find_with_quote_normalization(content, search);
        assert!(found.is_some(), "should find after normalization");
        // The returned string should be the ORIGINAL curly quotes from content
        assert_eq!(found.unwrap(), "\u{201C}hello\u{201D}");
    }

    #[test]
    fn find_with_quote_normalization_no_match() {
        let content = "let x = 42;";
        assert!(find_with_quote_normalization(content, "\"hello\"").is_none());
    }

    #[test]
    fn str_replace_with_curly_quotes() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("curly.rs");
        // File has straight quotes
        std::fs::write(&file_path, "let x = \"hello\";").unwrap();

        let executor = test_executor_in(dir.path());
        executor.read_file(&serde_json::json!({"path": "curly.rs"}));
        // Model sends curly quotes (common from LLM output)
        let result = executor.str_replace(&serde_json::json!({
            "path": "curly.rs",
            "old_str": "let x = \u{201C}hello\u{201D};",
            "new_str": "let x = \"world\";"
        }));
        assert!(
            result.contains("Replaced"),
            "should succeed with quote normalization: {result}"
        );
        assert!(
            result.contains("curly quotes"),
            "should mention normalization: {result}"
        );
        // Verify actual file content
        let actual = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(actual, "let x = \"world\";");
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

        // Pre-read size gate: large files without a range return an error
        // directing the LLM to use start_line/end_line or outline.
        assert!(
            result.contains("too large") || result.contains("truncated"),
            "should reject or truncate large file: last 100 chars: {}",
            &result[result.len().saturating_sub(100)..]
        );
        assert!(
            result.contains("outline") || result.contains("start_line"),
            "should suggest alternatives: last 200 chars: {}",
            &result[result.len().saturating_sub(200)..]
        );
    }

    // ── Bug fix: pre-read size gate allows ranged reads ──────────────────────

    #[test]
    fn read_file_size_gate_allows_ranged_read_of_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..3000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        // Full read should be rejected
        let full = executor.read_file(&serde_json::json!({"path": "big.txt"}));
        assert!(
            full.contains("too large"),
            "full read should be rejected: {}",
            &full[..100.min(full.len())]
        );

        // Ranged read should succeed
        let ranged = executor
            .read_file(&serde_json::json!({"path": "big.txt", "start_line": 1, "end_line": 10}));
        assert!(!ranged.contains("too large"), "ranged read should succeed");
        assert!(ranged.contains("line 0"), "should contain first line");
    }

    #[test]
    fn read_file_size_gate_allows_outline_of_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.rs");
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "fn main() {{}}").unwrap();
        for i in 0..3000 {
            writeln!(f, "// line {i} {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        let outline = executor.read_file(&serde_json::json!({"path": "big.rs", "outline": true}));
        assert!(
            !outline.contains("too large"),
            "outline should bypass size gate"
        );
    }

    // ── Bug fix: auto-expand respects size gate ──────────────────────────────

    #[test]
    fn read_file_auto_expand_blocked_for_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..3000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        // First ranged read
        let r1 = executor
            .read_file(&serde_json::json!({"path": "big.txt", "start_line": 1, "end_line": 5}));
        assert!(r1.contains("line 0"), "first range should work");

        // Second ranged read — should NOT auto-expand (file too large)
        let r2 = executor
            .read_file(&serde_json::json!({"path": "big.txt", "start_line": 10, "end_line": 15}));
        assert!(
            !r2.contains("Auto-expanded"),
            "should NOT auto-expand large file: {}",
            &r2[..100.min(r2.len())]
        );
    }

    // ── Bug fix: ranged reads don't increment read_count ─────────────────────

    #[test]
    fn read_file_ranged_reads_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..3000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        // 5 ranged reads of different sections — should NOT trigger "read 4+ times" warning
        for start in (1..=50).step_by(10) {
            executor.read_file(&serde_json::json!({
                "path": "big.txt",
                "start_line": start,
                "end_line": start + 5
            }));
        }
        let last = executor.read_file(&serde_json::json!({
            "path": "big.txt",
            "start_line": 60,
            "end_line": 65
        }));
        assert!(
            !last.contains("read 4+ times"),
            "ranged reads should not trigger warning"
        );
        assert!(
            !last.contains("read 3 times"),
            "ranged reads should not trigger warning"
        );
    }

    #[test]
    fn read_file_ranged_reads_trigger_grep_nudge() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..3000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        // First 2 ranged reads — no warning yet
        for start in [1, 20] {
            let out = executor.read_file(&serde_json::json!({
                "path": "big.txt",
                "start_line": start,
                "end_line": start + 5
            }));
            assert!(
                !out.contains("3+ different ranges"),
                "should not warn before 3 ranged reads"
            );
        }
        // 3rd ranged read — should trigger the grep nudge
        let third = executor.read_file(&serde_json::json!({
            "path": "big.txt",
            "start_line": 40,
            "end_line": 45
        }));
        assert!(
            third.contains("3+ different ranges") || third.contains("Use grep"),
            "3rd ranged read should nudge toward grep, got: {}",
            &third[third.len().saturating_sub(200)..]
        );
    }

    #[test]
    fn full_reads_dont_increment_ranged_count() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        // Must be >8KB to avoid auto-expand
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..500 {
            writeln!(f, "line {i}: {}", "x".repeat(80)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());

        // 3 ranged reads — should trigger grep nudge
        for start in [1, 20, 40] {
            executor.read_file(&serde_json::json!({
                "path": "big.txt", "start_line": start, "end_line": start + 5
            }));
        }
        let fourth_ranged = executor.read_file(&serde_json::json!({
            "path": "big.txt", "start_line": 60, "end_line": 65
        }));
        assert!(
            fourth_ranged.contains("3+ different ranges") || fourth_ranged.contains("Use grep"),
            "4th ranged read should trigger grep nudge"
        );
        assert!(
            !fourth_ranged.contains("read 4+ times"),
            "ranged reads should not trigger full-read warning"
        );
    }

    #[test]
    fn ranged_read_count_resets_on_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        // Files must be >8KB to avoid auto-expand upgrading ranged reads to full reads
        let mut f = std::fs::File::create(&file_a).unwrap();
        for i in 0..500 {
            writeln!(f, "a line {i}: {}", "x".repeat(80)).unwrap();
        }
        drop(f);
        let mut f = std::fs::File::create(&file_b).unwrap();
        for i in 0..500 {
            writeln!(f, "b line {i}: {}", "x".repeat(80)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        // 2 ranged reads of file a
        for start in [1, 20] {
            executor.read_file(&serde_json::json!({
                "path": "a.txt", "start_line": start, "end_line": start + 5
            }));
        }
        // 2 ranged reads of file b — should be independent
        for start in [1, 20] {
            executor.read_file(&serde_json::json!({
                "path": "b.txt", "start_line": start, "end_line": start + 5
            }));
        }
        // 3rd ranged read of file a — should trigger
        let third_a = executor.read_file(&serde_json::json!({
            "path": "a.txt", "start_line": 40, "end_line": 45
        }));
        assert!(
            third_a.contains("3+ different ranges") || third_a.contains("Use grep"),
            "3rd ranged read of file a should trigger grep nudge, got: {third_a}"
        );
        // 3rd ranged read of file b — should also trigger independently
        let third_b = executor.read_file(&serde_json::json!({
            "path": "b.txt", "start_line": 40, "end_line": 45
        }));
        assert!(
            third_b.contains("3+ different ranges") || third_b.contains("Use grep"),
            "3rd ranged read of file b should trigger grep nudge"
        );
    }

    // ── Claude Code–style consecutive identical partial read dedup ───────────

    #[test]
    fn read_file_consecutive_identical_range_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("dup.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 1..=20 {
            writeln!(f, "row {i}").unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        let args = serde_json::json!({
            "path": "dup.txt",
            "start_line": 1,
            "end_line": 5
        });
        let first = executor.read_file(&args);
        assert!(
            first.contains("row 1") && !first.contains("Same read_file request"),
            "first read should return content: {first}"
        );

        let second = executor.read_file(&args);
        assert!(
            second.contains("Same read_file request"),
            "second identical range should stub: {second}"
        );
    }

    #[test]
    fn read_file_consecutive_identical_outline_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("o.rs");
        std::fs::write(&file_path, "fn alpha() {}\nfn beta() {}\n").unwrap();

        let executor = test_executor_in(dir.path());
        let args = serde_json::json!({ "path": "o.rs", "outline": true });
        let first = executor.read_file(&args);
        assert!(
            first.contains("Outline") || first.contains("fn "),
            "first outline read: {first}"
        );
        assert!(!first.contains("Same read_file request"));

        let second = executor.read_file(&args);
        assert!(
            second.contains("Same read_file request"),
            "second outline should stub: {second}"
        );
    }

    #[test]
    fn read_file_nonconsecutive_same_range_not_deduped() {
        // > AUTO_EXPAND_MAX_BYTES so the second ranged read does not upgrade to a full read
        // (which would make a third ranged read hit can_dedup_read instead of this scenario).
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("ab.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        writeln!(f, "MARK_A").unwrap();
        writeln!(f, "MARK_B").unwrap();
        writeln!(f, "MARK_C").unwrap();
        writeln!(f, "MARK_D").unwrap();
        for i in 0..300 {
            writeln!(f, "pad {i}: {}", "x".repeat(40)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        let r_a = executor.read_file(&serde_json::json!({
            "path": "ab.txt",
            "start_line": 1,
            "end_line": 2
        }));
        assert!(r_a.contains("MARK_A"));

        let _r_b = executor.read_file(&serde_json::json!({
            "path": "ab.txt",
            "start_line": 3,
            "end_line": 4
        }));

        let r_a_again = executor.read_file(&serde_json::json!({
            "path": "ab.txt",
            "start_line": 1,
            "end_line": 2
        }));
        assert!(
            !r_a_again.contains("Same read_file request"),
            "last read was a different range — should re-fetch lines: {r_a_again}"
        );
        assert!(r_a_again.contains("MARK_A"));
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
    fn truncate_str_over_limit_uses_runtime_helper() {
        let result = truncate_str("this is a long string", 7);
        assert_eq!(result, "this is…");
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

    // ── auto_format_file tests ──────────────────────────────────────────────

    #[test]
    fn auto_format_unknown_extension_returns_none() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("data.xyz");
        std::fs::write(&file, "content").unwrap();
        assert!(auto_format_file(&file, tmpdir.path()).is_none());
    }

    #[test]
    fn auto_format_rs_without_cargo_toml_returns_none() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();
        // No Cargo.toml in tmpdir → should skip
        assert!(auto_format_file(&file, tmpdir.path()).is_none());
    }

    #[test]
    fn auto_format_py_without_pyproject_returns_none() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("main.py");
        std::fs::write(&file, "print('hello')").unwrap();
        assert!(auto_format_file(&file, tmpdir.path()).is_none());
    }

    #[test]
    fn auto_format_ts_without_package_json_returns_none() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("app.ts");
        std::fs::write(&file, "const x = 1;").unwrap();
        assert!(auto_format_file(&file, tmpdir.path()).is_none());
    }

    #[test]
    fn auto_format_rs_with_cargo_toml_tries_rustfmt() {
        let tmpdir = tempfile::tempdir().unwrap();
        // Create Cargo.toml so the guard passes
        std::fs::write(
            tmpdir.path().join("Cargo.toml"),
            "[package]\nname = \"test\"",
        )
        .unwrap();
        let file = tmpdir.path().join("main.rs");
        std::fs::write(&file, "fn   main  (  )  {  }").unwrap();
        let result = auto_format_file(&file, tmpdir.path());
        // rustfmt may or may not be installed — either formatted or None is ok
        if let Some(r) = &result {
            assert!(r.contains("rustfmt"), "should mention rustfmt: {r}");
        }
    }

    // ─── dry_run / diff preview tests ───────────────────────────────────────

    #[test]
    fn str_replace_dry_run_shows_diff_without_applying() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("test.txt");
        std::fs::write(&file, "line1\nline2\nline3\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "test.txt"}));
        let result = exe.str_replace(&json!({
            "path": "test.txt",
            "old_str": "line2",
            "new_str": "REPLACED",
            "dry_run": true
        }));
        assert!(
            result.contains("[DRY RUN]"),
            "should show dry run marker: {result}"
        );
        assert!(
            result.contains("-line2"),
            "should show removed line: {result}"
        );
        assert!(
            result.contains("+REPLACED"),
            "should show added line: {result}"
        );
        // File should NOT be modified
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");
    }

    #[test]
    fn str_replace_dry_run_false_still_applies() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("test.txt");
        std::fs::write(&file, "hello world").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "test.txt"}));
        let result = exe.str_replace(&json!({
            "path": "test.txt",
            "old_str": "hello",
            "new_str": "bye",
            "dry_run": false
        }));
        assert!(result.contains("Replaced successfully"));
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("bye"));
    }

    // ─── delete_file tests ──────────────────────────────────────────────────

    #[test]
    fn delete_file_removes_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("victim.txt");
        std::fs::write(&file, "delete me").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        let result = exe.delete_file(&json!({"path": "victim.txt"}));
        assert!(result.starts_with("Deleted:"), "result: {result}");
        assert!(!file.exists());
    }

    #[test]
    fn delete_file_rejects_missing() {
        let tmpdir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        let result = exe.delete_file(&json!({"path": "nope.txt"}));
        assert!(result.contains("not found"), "result: {result}");
    }

    #[test]
    fn delete_file_rejects_directory() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmpdir.path().join("subdir")).unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        let result = exe.delete_file(&json!({"path": "subdir"}));
        assert!(
            result.contains("refusing to delete a directory"),
            "result: {result}"
        );
    }

    #[test]
    fn delete_file_rejects_git_contents() {
        let tmpdir = tempfile::tempdir().unwrap();
        let git_dir = tmpdir.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        let result = exe.delete_file(&json!({"path": ".git/HEAD"}));
        assert!(
            result.contains("refusing to delete .git"),
            "result: {result}"
        );
    }

    // ─── multi_edit tests ───────────────────────────────────────────────────

    #[test]
    fn multi_edit_applies_all_edits_atomically() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        std::fs::write(&file, "fn foo() {}\nfn bar() {}\nfn baz() {}\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {"old_str": "fn foo() {}", "new_str": "fn foo_renamed() {}"},
                {"old_str": "fn baz() {}", "new_str": "fn baz_renamed() {}"}
            ]
        }));
        assert!(result.contains("Applied 2 edit(s)"), "result: {result}");
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("fn foo_renamed() {}"));
        assert!(content.contains("fn bar() {}"));
        assert!(content.contains("fn baz_renamed() {}"));
    }

    #[test]
    fn multi_edit_aborts_on_first_failure() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        std::fs::write(&file, "fn foo() {}\nfn bar() {}\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {"old_str": "fn foo() {}", "new_str": "fn renamed() {}"},
                {"old_str": "fn NONEXISTENT() {}", "new_str": "fn nope() {}"}
            ]
        }));
        assert!(
            result.contains("edit[1]"),
            "should identify failing edit: {result}"
        );
        assert!(result.contains("not found"), "should explain why: {result}");
        // File should NOT be modified (atomic rollback)
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("fn foo() {}"),
            "original should be preserved"
        );
    }

    #[test]
    fn multi_edit_rejects_ambiguous_match() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        std::fs::write(&file, "aaa\naaa\nbbb\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {"old_str": "aaa", "new_str": "ccc"}
            ]
        }));
        assert!(
            result.contains("edit[0]"),
            "should identify the edit: {result}"
        );
        assert!(result.contains("2 times"), "should report count: {result}");
    }

    #[test]
    fn multi_edit_dry_run_shows_diff() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        std::fs::write(&file, "fn foo() {}\nfn bar() {}\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {"old_str": "fn foo() {}", "new_str": "fn renamed() {}"}
            ],
            "dry_run": true
        }));
        assert!(
            result.contains("[DRY RUN]"),
            "should show dry run marker: {result}"
        );
        assert!(
            result.contains("-fn foo() {}"),
            "should show removed: {result}"
        );
        assert!(
            result.contains("+fn renamed() {}"),
            "should show added: {result}"
        );
        // File should NOT be modified
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(content.contains("fn foo() {}"));
    }

    #[test]
    fn multi_edit_empty_edits_rejected() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::write(tmpdir.path().join("f.txt"), "x").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        let result = exe.multi_edit(&json!({"path": "f.txt", "edits": []}));
        assert!(result.contains("empty"), "result: {result}");
    }

    #[test]
    fn multi_edit_sequential_edits_see_previous_results() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("chain.txt");
        std::fs::write(&file, "alpha beta gamma").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "chain.txt"}));
        let result = exe.multi_edit(&json!({
            "path": "chain.txt",
            "edits": [
                {"old_str": "alpha", "new_str": "ALPHA"},
                {"old_str": "ALPHA beta", "new_str": "AB"}
            ]
        }));
        assert!(result.contains("Applied 2 edit(s)"), "result: {result}");
        let content = std::fs::read_to_string(&file).unwrap();
        assert_eq!(content, "AB gamma");
    }

    // ─── unified_diff tests ─────────────────────────────────────────────────

    #[test]
    fn unified_diff_shows_context() {
        let old = "line1\nline2\nline3\nline4\nline5\n";
        let new = "line1\nline2\nLINE3\nline4\nline5\n";
        let path = std::path::PathBuf::from("test.txt");
        let diff = super::unified_diff(old, new, &path);
        assert!(diff.contains("--- a/test.txt"));
        assert!(diff.contains("+++ b/test.txt"));
        assert!(diff.contains("-line3"));
        assert!(diff.contains("+LINE3"));
        assert!(diff.contains(" line2"), "should have context around change");
    }

    #[test]
    fn unified_diff_no_changes() {
        let s = "same content\n";
        let path = std::path::PathBuf::from("f.txt");
        let diff = super::unified_diff(s, s, &path);
        assert!(diff.contains("(no changes)"));
    }

    // ─── scope context in str_replace ───────────────────────────────────

    #[test]
    fn str_replace_shows_scope_context_for_rust() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("lib.rs");
        std::fs::write(
            &file,
            "struct Foo {\n    x: i32,\n}\n\nimpl Foo {\n    fn bar(&self) -> i32 {\n        self.x + 1\n    }\n}\n",
        )
        .unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "lib.rs"}));
        let result = exe.str_replace(&json!({
            "path": "lib.rs",
            "old_str": "self.x + 1",
            "new_str": "self.x + 2"
        }));
        assert!(result.contains("Replaced successfully"), "result: {result}");
        assert!(result.contains("📍"), "should show scope icon: {result}");
        assert!(
            result.contains("bar"),
            "should mention the function: {result}"
        );
    }

    #[test]
    fn str_replace_no_scope_for_unsupported_language() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("config.toml");
        std::fs::write(&file, "[package]\nname = \"old\"\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "config.toml"}));
        let result = exe.str_replace(&json!({
            "path": "config.toml",
            "old_str": "\"old\"",
            "new_str": "\"new\""
        }));
        assert!(result.contains("Replaced successfully"), "result: {result}");
        assert!(
            !result.contains("📍"),
            "should not show scope for .toml: {result}"
        );
    }

    #[test]
    fn str_replace_scope_for_python() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("app.py");
        std::fs::write(
            &file,
            "class Handler:\n    def process(self, data):\n        return data.strip()\n",
        )
        .unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "app.py"}));
        let result = exe.str_replace(&json!({
            "path": "app.py",
            "old_str": "data.strip()",
            "new_str": "data.strip().lower()"
        }));
        assert!(result.contains("📍"), "should show scope: {result}");
        assert!(
            result.contains("process"),
            "should mention function: {result}"
        );
    }

    #[test]
    fn multi_edit_shows_scope_context() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("main.rs");
        std::fs::write(&file, "fn main() {\n    let x = 1;\n    let y = 2;\n}\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "main.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "main.rs",
            "edits": [
                {"old_str": "let x = 1", "new_str": "let x = 10"},
                {"old_str": "let y = 2", "new_str": "let y = 20"}
            ]
        }));
        assert!(result.contains("Applied 2 edit(s)"), "result: {result}");
        assert!(result.contains("📍"), "should show scope: {result}");
        assert!(result.contains("main"), "should mention fn main: {result}");
    }

    #[test]
    fn test_is_dangerous_write_target() {
        assert!(is_dangerous_write_target(".bashrc").is_some());
        assert!(is_dangerous_write_target(".git/config").is_some());
        assert!(is_dangerous_write_target(".env").is_some());
        assert!(is_dangerous_write_target("src/main.rs").is_none());
        assert!(is_dangerous_write_target("README.md").is_none());
    }

    #[test]
    fn test_dangerous_write_target_expanded_list() {
        // New entries
        assert!(is_dangerous_write_target(".env.local").is_some());
        assert!(is_dangerous_write_target(".env.production").is_some());
        assert!(is_dangerous_write_target(".env.staging").is_some());
        assert!(is_dangerous_write_target(".aws/credentials").is_some());
        assert!(is_dangerous_write_target(".aws/config").is_some());
        assert!(is_dangerous_write_target(".kube/config").is_some());
        assert!(is_dangerous_write_target(".docker/config.json").is_some());
        assert!(is_dangerous_write_target(".ssh/id_rsa").is_some());
        assert!(is_dangerous_write_target(".ssh/id_ed25519").is_some());
        assert!(is_dangerous_write_target(".ssh/authorized_keys2").is_some());
        // Still safe
        assert!(is_dangerous_write_target("package.json").is_none());
        assert!(is_dangerous_write_target("Cargo.toml").is_none());
    }

    #[test]
    fn test_is_unc_path() {
        assert!(is_unc_path("\\\\server\\share"));
        assert!(is_unc_path("//server/share"));
        assert!(!is_unc_path("/home/user/file"));
        assert!(!is_unc_path("src/main.rs"));
    }

    #[test]
    fn test_str_replace_identity_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "hello world").unwrap();
        // Read it first
        exe.read_file(&json!({"path": test_file.to_str().unwrap()}));
        let result = exe.str_replace(&json!({
            "path": test_file.to_str().unwrap(),
            "old_str": "hello",
            "new_str": "hello"
        }));
        assert!(
            result.contains("identical"),
            "should reject identical: {result}"
        );
    }

    #[test]
    fn test_str_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        let test_file = dir.path().join("test.txt");
        std::fs::write(&test_file, "foo bar foo baz foo").unwrap();
        exe.read_file(&json!({"path": test_file.to_str().unwrap()}));
        let result = exe.str_replace(&json!({
            "path": test_file.to_str().unwrap(),
            "old_str": "foo",
            "new_str": "qux",
            "replace_all": true
        }));
        assert!(
            result.contains("Replaced 3 occurrences"),
            "should report count: {result}"
        );
        let content = std::fs::read_to_string(&test_file).unwrap();
        assert_eq!(content, "qux bar qux baz qux");
    }

    #[test]
    fn test_resolve_checked_blocks_unc() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        assert!(exe.resolve_checked("\\\\server\\share").is_err());
        assert!(exe.resolve_checked("//server/share").is_err());
        assert!(exe.resolve_checked("src/main.rs").is_ok());
    }

    // ── Read-before-write guard: realistic session scenarios ─────────────────
    //
    // These tests reproduce the exact failure patterns observed in real agentic
    // sessions (e.g. session 1e627e9a) where the LLM attempts write_file or
    // str_replace on files it hasn't read yet, or on files that became stale
    // between reads and writes.

    /// Scenario from session 1e627e9a Turn 2: LLM calls skill("say-hello")
    /// which returns SKILL.md content, then immediately tries write_file on
    /// SKILL.md without calling read_file first. The guard must reject this.
    #[test]
    fn write_file_blocked_on_unread_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        // Simulate: file exists on disk (created by `astra init` or prior session)
        let skill_path = dir.path().join(".astra/skills/say-hello/SKILL.md");
        std::fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
        std::fs::write(
            &skill_path,
            "---\nname: say-hello\ndescription: \"\"\n---\n# say-hello\n",
        )
        .unwrap();

        // LLM tries to overwrite without reading first
        let result = exe.write_file(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "content": "---\nname: say-hello\ndescription: \"updated\"\n---\n# say-hello\n"
        }));

        assert!(
            result.contains("has not been read yet"),
            "should reject unread file, got: {result}"
        );
        // Must contain actionable guidance with the concrete path
        assert!(
            result.contains("read_file"),
            "error should mention read_file, got: {result}"
        );
        assert!(
            result.contains("SKILL.md"),
            "error should mention the file path, got: {result}"
        );
    }

    /// Same scenario but for str_replace: LLM tries to edit a file it hasn't read.
    #[test]
    fn str_replace_blocked_on_unread_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("config.toml");
        std::fs::write(&path, "key = \"old_value\"\n").unwrap();

        let result = exe.str_replace(&json!({
            "path": "config.toml",
            "old_str": "key = \"old_value\"",
            "new_str": "key = \"new_value\""
        }));

        assert!(
            result.contains("has not been read yet"),
            "should reject unread file, got: {result}"
        );
        assert!(
            result.contains("read_file(\"config.toml\")"),
            "error should contain actionable read_file call, got: {result}"
        );
    }

    /// After read_file, write_file should succeed (the happy path).
    #[test]
    fn write_file_succeeds_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("hello.txt");
        std::fs::write(&path, "original content").unwrap();

        // Step 1: read the file (as the LLM should)
        let read_result = exe.read_file(&json!({"path": "hello.txt"}));
        assert!(
            read_result.contains("original content"),
            "read should work: {read_result}"
        );

        // Step 2: now write should succeed
        let write_result = exe.write_file(&json!({
            "path": "hello.txt",
            "content": "updated content"
        }));
        assert!(
            write_result.contains("\"success\":true") || write_result.contains("\"success\": true"),
            "write should succeed after read, got: {write_result}"
        );

        // Verify content on disk
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "updated content");
    }

    /// After read_file, str_replace should succeed.
    #[test]
    fn str_replace_succeeds_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("code.rs");
        std::fs::write(&path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        exe.read_file(&json!({"path": "code.rs"}));

        let result = exe.str_replace(&json!({
            "path": "code.rs",
            "old_str": "println!(\"hello\")",
            "new_str": "println!(\"world\")"
        }));
        assert!(
            result.contains("Replaced"),
            "str_replace should succeed after read, got: {result}"
        );

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("println!(\"world\")"));
    }

    /// Scenario: write_file creates a new file (no prior read needed).
    #[test]
    fn write_file_creates_new_file_without_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let result = exe.write_file(&json!({
            "path": "brand_new.txt",
            "content": "fresh content"
        }));
        assert!(
            result.contains("\"success\":true") || result.contains("\"success\": true"),
            "new file write should not require read, got: {result}"
        );
    }

    /// Scenario: after write_file creates a file, a subsequent write_file
    /// should succeed without needing read_file (write records state).
    #[test]
    fn consecutive_writes_without_intermediate_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        // First write creates the file
        exe.write_file(&json!({"path": "iter.txt", "content": "v1"}));

        // Second write should succeed because record_write updated file_state
        let result = exe.write_file(&json!({"path": "iter.txt", "content": "v2"}));
        assert!(
            result.contains("\"success\":true") || result.contains("\"success\": true"),
            "second write should succeed (record_write tracks state), got: {result}"
        );
    }

    /// Scenario: after str_replace edits a file, a subsequent str_replace
    /// should succeed without needing read_file again.
    #[test]
    fn consecutive_str_replace_without_intermediate_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        std::fs::write(dir.path().join("chain.txt"), "aaa bbb ccc").unwrap();
        exe.read_file(&json!({"path": "chain.txt"}));

        // First edit
        let r1 = exe.str_replace(&json!({
            "path": "chain.txt",
            "old_str": "aaa",
            "new_str": "AAA"
        }));
        assert!(r1.contains("Replaced"), "first edit should work: {r1}");

        // Second edit on the same file — should succeed because record_write
        // updated file_state after the first edit
        let r2 = exe.str_replace(&json!({
            "path": "chain.txt",
            "old_str": "bbb",
            "new_str": "BBB"
        }));
        assert!(
            r2.contains("Replaced"),
            "second edit should succeed without re-read, got: {r2}"
        );

        let on_disk = std::fs::read_to_string(dir.path().join("chain.txt")).unwrap();
        assert_eq!(on_disk, "AAA BBB ccc");
    }

    /// Scenario from session 1e627e9a Turn 2: LLM sends str_replace with
    /// old_str that was valid before a prior str_replace in the same turn
    /// modified the file. The second str_replace should fail with a helpful
    /// hint showing the actual file content.
    #[test]
    fn str_replace_fails_on_stale_old_str_after_prior_edit() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        std::fs::write(
            dir.path().join("skill.md"),
            "---\nname: say-hello\nversion: \"0.1.0\"\n---\n# Steps\n1. Say hello\n",
        )
        .unwrap();
        exe.read_file(&json!({"path": "skill.md"}));

        // First edit: change version
        let r1 = exe.str_replace(&json!({
            "path": "skill.md",
            "old_str": "version: \"0.1.0\"",
            "new_str": "version: \"0.2.0\""
        }));
        assert!(r1.contains("Replaced"), "first edit: {r1}");

        // Second edit: LLM still thinks old content has "version: \"0.1.0\""
        let r2 = exe.str_replace(&json!({
            "path": "skill.md",
            "old_str": "version: \"0.1.0\"",
            "new_str": "version: \"0.3.0\""
        }));
        assert!(
            r2.contains("not found"),
            "should fail because old_str no longer exists, got: {r2}"
        );
    }

    /// Scenario: external modification between read and write (linter, user edit).
    /// The staleness guard must catch this.
    #[test]
    fn write_file_blocked_on_externally_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("modified.txt");
        std::fs::write(&path, "original").unwrap();

        // Read the file
        exe.read_file(&json!({"path": "modified.txt"}));

        // Simulate external modification (linter, user, etc.)
        // Need to ensure mtime actually changes — sleep briefly
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "externally modified").unwrap();

        // Write should be blocked
        let result = exe.write_file(&json!({
            "path": "modified.txt",
            "content": "agent's version"
        }));
        assert!(
            result.contains("modified since last read")
                || result.contains("modified since")
                || result.contains("staleness"),
            "should detect external modification, got: {result}"
        );
        assert!(
            result.contains("read_file"),
            "error should suggest re-reading, got: {result}"
        );
    }

    /// Scenario: external modification between read and str_replace.
    #[test]
    fn str_replace_blocked_on_externally_modified_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("linted.rs");
        std::fs::write(&path, "fn main() { }").unwrap();

        exe.read_file(&json!({"path": "linted.rs"}));

        // Simulate linter reformatting
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "fn main() {\n}\n").unwrap();

        let result = exe.str_replace(&json!({
            "path": "linted.rs",
            "old_str": "fn main() { }",
            "new_str": "fn main() { println!(\"hi\"); }"
        }));
        assert!(
            result.contains("modified since last read")
                || result.contains("modified since")
                || result.contains("staleness"),
            "should detect linter modification, got: {result}"
        );
    }

    /// Scenario: partial read (outline) should NOT allow write_file overwrite.
    #[test]
    fn write_file_blocked_after_outline_only_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("big.rs");
        std::fs::write(&path, "pub fn foo() {}\npub fn bar() {}\npub fn baz() {}\n").unwrap();

        // Read with outline=true (partial view)
        exe.read_file(&json!({"path": "big.rs", "outline": true}));

        // write_file should be blocked — outline is not a full read
        let result = exe.write_file(&json!({
            "path": "big.rs",
            "content": "pub fn foo() { /* changed */ }\n"
        }));
        assert!(
            result.contains("partially read") || result.contains("partial"),
            "should reject write after outline-only read, got: {result}"
        );
        assert!(
            result.contains("read_file"),
            "error should suggest full read, got: {result}"
        );
    }

    /// Scenario: partial read (line range) should still allow str_replace
    /// (str_replace doesn't require full read, only write_file does).
    #[test]
    fn str_replace_allowed_after_partial_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("partial.txt");
        std::fs::write(&path, "line1\nline2\nline3\n").unwrap();

        // Read with line range (partial)
        exe.read_file(&json!({"path": "partial.txt", "start_line": 1, "end_line": 2}));

        // str_replace should work — it only needs the file to be in file_state
        let result = exe.str_replace(&json!({
            "path": "partial.txt",
            "old_str": "line2",
            "new_str": "LINE2"
        }));
        assert!(
            result.contains("Replaced"),
            "str_replace should work after partial read, got: {result}"
        );
    }

    /// Scenario: register_external_read allows subsequent writes without
    /// explicit read_file. This is the key improvement for skill execution.
    #[test]
    fn register_external_read_enables_subsequent_write() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let skill_path = dir.path().join(".astra/skills/say-hello/SKILL.md");
        std::fs::create_dir_all(skill_path.parent().unwrap()).unwrap();
        std::fs::write(
            &skill_path,
            "---\nname: say-hello\n---\n# say-hello\nFollow these steps:\n",
        )
        .unwrap();

        // Simulate: skill execution loaded and returned the file content.
        // The skill runner calls register_external_read.
        exe.register_external_read(std::path::Path::new(".astra/skills/say-hello/SKILL.md"));

        // Now write_file should succeed without explicit read_file
        let result = exe.write_file(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "content": "---\nname: say-hello\n---\n# say-hello\nUpdated steps:\n"
        }));
        assert!(
            result.contains("\"success\":true") || result.contains("\"success\": true"),
            "write should succeed after register_external_read, got: {result}"
        );
    }

    /// Scenario: register_external_read also enables str_replace.
    #[test]
    fn register_external_read_enables_subsequent_str_replace() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("ext_read.txt");
        std::fs::write(&path, "hello world").unwrap();

        exe.register_external_read(std::path::Path::new("ext_read.txt"));

        let result = exe.str_replace(&json!({
            "path": "ext_read.txt",
            "old_str": "hello",
            "new_str": "goodbye"
        }));
        assert!(
            result.contains("Replaced"),
            "str_replace should work after external read, got: {result}"
        );
    }

    /// Scenario: multi_edit blocked on unread file.
    #[test]
    fn multi_edit_blocked_on_unread_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("multi.txt");
        std::fs::write(&path, "aaa\nbbb\nccc\n").unwrap();

        let result = exe.multi_edit(&json!({
            "path": "multi.txt",
            "edits": [
                {"old_str": "aaa", "new_str": "AAA"},
                {"old_str": "bbb", "new_str": "BBB"}
            ]
        }));
        assert!(
            result.contains("has not been read yet"),
            "multi_edit should be blocked on unread file, got: {result}"
        );
    }

    /// Scenario: multi_edit succeeds after read, applying all edits atomically.
    #[test]
    fn multi_edit_succeeds_after_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("atomic.txt");
        std::fs::write(&path, "aaa\nbbb\nccc\n").unwrap();

        exe.read_file(&json!({"path": "atomic.txt"}));

        let result = exe.multi_edit(&json!({
            "path": "atomic.txt",
            "edits": [
                {"old_str": "aaa", "new_str": "AAA"},
                {"old_str": "bbb", "new_str": "BBB"}
            ]
        }));
        assert!(
            result.contains("Applied 2 edit(s)"),
            "multi_edit should succeed, got: {result}"
        );

        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "AAA\nBBB\nccc\n");
    }

    /// Scenario: defense-in-depth — external modification happens BETWEEN
    /// the initial staleness check and the actual write. The pre-write
    /// re-check should catch this race condition.
    #[test]
    fn defense_in_depth_catches_race_between_check_and_write() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("race.txt");
        std::fs::write(&path, "original").unwrap();

        // Read the file to pass the initial staleness check
        exe.read_file(&json!({"path": "race.txt"}));

        // Now simulate: the initial check_staleness passes, but before
        // fs::write happens, the file is modified externally.
        // We can't truly race in a unit test, but we can verify that
        // check_staleness is called at the right point by modifying
        // the file and then calling str_replace (which reads content
        // between check and write).
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "modified by linter").unwrap();

        // str_replace: the initial check_staleness catches this
        let result = exe.str_replace(&json!({
            "path": "race.txt",
            "old_str": "original",
            "new_str": "agent version"
        }));
        assert!(
            result.contains("modified since") || result.contains("staleness"),
            "should catch external modification, got: {result}"
        );

        // Verify file was NOT corrupted
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "modified by linter");
    }

    /// Scenario: the full realistic session flow from session 1e627e9a.
    /// Turn 1: skill returns content → write blocked → read → edit succeeds.
    /// Turn 2: verify edit → re-edit succeeds without re-read.
    #[test]
    fn realistic_session_skill_edit_flow() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        // Setup: SKILL.md exists from `astra init`
        let skill_dir = dir.path().join(".astra/skills/say-hello");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: say-hello\nversion: \"0.1.0\"\n---\n# say-hello\n\n1. Say hello\n",
        )
        .unwrap();

        // Turn 1, Step 1: LLM tries write_file without reading (BLOCKED)
        let r1 = exe.write_file(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "content": "---\nname: say-hello\nversion: \"0.2.0\"\n---\n# say-hello\n\n1. Greet user\n"
        }));
        assert!(r1.contains("has not been read yet"), "step 1: {r1}");

        // Turn 1, Step 2: LLM reads the file
        let r2 = exe.read_file(&json!({"path": ".astra/skills/say-hello/SKILL.md"}));
        assert!(r2.contains("say-hello"), "step 2: {r2}");

        // Turn 1, Step 3: LLM edits with str_replace (SUCCESS)
        let r3 = exe.str_replace(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "old_str": "version: \"0.1.0\"",
            "new_str": "version: \"0.2.0\""
        }));
        assert!(r3.contains("Replaced"), "step 3: {r3}");

        // Turn 1, Step 4: LLM makes another edit (SUCCESS — no re-read needed)
        let r4 = exe.str_replace(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "old_str": "1. Say hello",
            "new_str": "1. Greet user warmly"
        }));
        assert!(r4.contains("Replaced"), "step 4: {r4}");

        // Verify final content
        let on_disk = std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap();
        assert!(on_disk.contains("version: \"0.2.0\""));
        assert!(on_disk.contains("1. Greet user warmly"));
    }

    /// Scenario: the improved flow with register_external_read.
    /// Skill execution registers the read, so the LLM can edit immediately.
    #[test]
    fn improved_session_flow_with_external_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let skill_dir = dir.path().join(".astra/skills/say-hello");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: say-hello\nversion: \"0.1.0\"\n---\n# say-hello\n\n1. Say hello\n",
        )
        .unwrap();

        // Skill execution loads the file and registers it
        exe.register_external_read(std::path::Path::new(".astra/skills/say-hello/SKILL.md"));

        // LLM can now edit immediately — no read_file needed!
        let r1 = exe.str_replace(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "old_str": "version: \"0.1.0\"",
            "new_str": "version: \"0.2.0\""
        }));
        assert!(
            r1.contains("Replaced"),
            "should succeed after external read, got: {r1}"
        );

        // And write_file also works
        let r2 = exe.write_file(&json!({
            "path": ".astra/skills/say-hello/SKILL.md",
            "content": "---\nname: say-hello\nversion: \"0.3.0\"\n---\n# say-hello\n\nNew content\n"
        }));
        assert!(
            r2.contains("\"success\":true") || r2.contains("\"success\": true"),
            "write_file should also work, got: {r2}"
        );
    }

    /// Verify that error messages contain actionable "→ Action required" text
    /// with the concrete file path, so the LLM can act without reasoning.
    #[test]
    fn error_messages_contain_actionable_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        std::fs::write(dir.path().join("target.rs"), "fn main() {}").unwrap();

        // write_file on unread file
        let r1 = exe.write_file(&json!({
            "path": "target.rs",
            "content": "fn main() { println!(\"hi\"); }"
        }));
        assert!(
            r1.contains("Action required"),
            "write_file error should have actionable guidance, got: {r1}"
        );
        assert!(
            r1.contains("read_file") && r1.contains("target.rs"),
            "should contain read_file and file path, got: {r1}"
        );

        // str_replace on unread file
        let r2 = exe.str_replace(&json!({
            "path": "target.rs",
            "old_str": "fn main() {}",
            "new_str": "fn main() { println!(\"hi\"); }"
        }));
        assert!(
            r2.contains("Action required"),
            "str_replace error should have actionable guidance, got: {r2}"
        );
        assert!(
            r2.contains("read_file") && r2.contains("target.rs"),
            "should contain read_file and file path, got: {r2}"
        );
    }

    /// Verify that the partial-read error for write_file also contains
    /// actionable guidance.
    #[test]
    fn partial_read_error_contains_actionable_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("partial_target.rs");
        std::fs::write(&path, "line1\nline2\nline3\nline4\nline5\n").unwrap();

        // Read with line range (partial)
        exe.read_file(&json!({"path": "partial_target.rs", "start_line": 1, "end_line": 2}));

        // write_file should fail with actionable message
        let result = exe.write_file(&json!({
            "path": "partial_target.rs",
            "content": "completely new content"
        }));
        assert!(
            result.contains("Action required"),
            "partial read error should have actionable guidance, got: {result}"
        );
        assert!(
            result.contains("without start_line/end_line"),
            "should tell user to do full read, got: {result}"
        );
    }

    /// Verify staleness error also contains actionable guidance with path.
    #[test]
    fn staleness_error_contains_actionable_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        let path = dir.path().join("stale.txt");
        std::fs::write(&path, "v1").unwrap();
        exe.read_file(&json!({"path": "stale.txt"}));

        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "v2").unwrap();

        let result = exe.write_file(&json!({
            "path": "stale.txt",
            "content": "v3"
        }));
        assert!(
            result.contains("Action required"),
            "staleness error should have actionable guidance, got: {result}"
        );
        assert!(
            result.contains("read_file") && result.contains("stale.txt"),
            "should contain read_file and file path, got: {result}"
        );
    }

    // ── find_similar_files: cross-directory fallback ────────────────────────
    //
    // When the requested parent directory doesn't exist (e.g. crate renamed
    // from mo-agent → astra-cli), find_similar_files should search the
    // project tree and suggest the correct path.

    /// Core scenario: file exists under a different parent directory.
    /// read_file("old_dir/foo.rs") should suggest "new_dir/foo.rs".
    #[test]
    fn read_file_suggests_file_in_different_directory() {
        let dir = tempfile::tempdir().unwrap();
        // File lives under new_crate/, but LLM will ask for old_crate/
        let new_dir = dir.path().join("src/new_crate/src");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("edge_tools.rs"), "// tools").unwrap();

        let exe = test_executor_in(dir.path());
        let result = exe.read_file(&json!({
            "path": "src/old_crate/src/edge_tools.rs"
        }));

        assert!(
            result.contains("No such file"),
            "should report not found: {result}"
        );
        assert!(
            result.contains("edge_tools.rs") && result.contains("new_crate"),
            "should suggest the file under new_crate, got: {result}"
        );
    }

    /// Deeply nested file found via project-wide search.
    #[test]
    fn read_file_suggests_deeply_nested_renamed_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c/d")).unwrap();
        std::fs::write(dir.path().join("a/b/c/d/config.toml"), "# cfg").unwrap();

        let exe = test_executor_in(dir.path());
        let result = exe.read_file(&json!({
            "path": "x/y/config.toml"
        }));

        assert!(
            result.contains("Did you mean"),
            "should suggest alternative: {result}"
        );
        assert!(
            result.contains("a/b/c/d/config.toml"),
            "should find deeply nested file, got: {result}"
        );
    }

    /// No match anywhere — should not crash, just return generic error.
    #[test]
    fn read_file_no_suggestion_when_truly_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("unrelated.py"), "pass").unwrap();

        let exe = test_executor_in(dir.path());
        let result = exe.read_file(&json!({
            "path": "nonexistent/totally_unique_name.rs"
        }));

        assert!(
            result.contains("No such file"),
            "should report not found: {result}"
        );
        assert!(
            !result.contains("Did you mean"),
            "should NOT suggest unrelated files, got: {result}"
        );
    }

    /// Skipped directories (.git, node_modules, target) should not be searched.
    #[test]
    fn read_file_skips_ignored_dirs_in_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        // Put file only inside .git — should not be suggested
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::write(dir.path().join(".git/objects/handler.rs"), "// git").unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/handler.rs"), "// nm").unwrap();

        let exe = test_executor_in(dir.path());
        let result = exe.read_file(&json!({
            "path": "old/handler.rs"
        }));

        assert!(
            !result.contains("Did you mean"),
            "should not suggest files from .git or node_modules, got: {result}"
        );
    }

    /// Same-directory suggestion still works after refactor (regression guard).
    #[test]
    fn read_file_same_dir_suggestion_still_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.rs"), "// cfg").unwrap();

        let exe = test_executor_in(dir.path());
        let result = exe.read_file(&json!({
            "path": "confg.rs"
        }));

        assert!(
            result.contains("config.rs"),
            "same-dir typo suggestion should still work, got: {result}"
        );
    }
}
