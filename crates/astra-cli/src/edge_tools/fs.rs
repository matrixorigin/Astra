use std::fs;
use std::path::{Path, PathBuf};

use super::{
    AGGREGATE_OUTPUT_BUDGET, AGGREGATE_SOFT_LIMIT, SANDBOX_DENIED_PREFIX, ToolExecutor, code_intel,
    tool_output_limit, truncate_output,
};
use astra_runtime::tool_sandbox::validate_path;
use astra_tools::fs_ops::{
    PreparedWriteFile, normalize_read_file_line_range, read_to_string_lossy, unified_diff_raw,
    validate_read_file_args,
};
use astra_turn_core::file_edit_journal::EditType;
use astra_turn_core::tool_result_sanitize::READ_FILE_MODEL_RESULT_CHARS;
use serde_json::{Value, json};

/// Check if a path is a UNC path (Windows network path that could leak NTLM credentials).
fn is_unc_path(path: &str) -> bool {
    path.starts_with("\\\\") || path.starts_with("//")
}

fn expand_home_path_arg(path: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    if matches!(path, "~" | "$HOME" | "${HOME}") {
        return Some(home);
    }
    path.strip_prefix("~/")
        .or_else(|| path.strip_prefix("$HOME/"))
        .or_else(|| path.strip_prefix("${HOME}/"))
        .map(|suffix| home.join(suffix))
}

fn is_blocked_device_read_path(path_str_lower: &str) -> bool {
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
    BLOCKED_DEVICE_PATHS
        .iter()
        .any(|blocked| path_str_lower.starts_with(blocked))
        || (path_str_lower.starts_with("/proc/") && path_str_lower.contains("/fd/"))
}

fn is_read_file_image_extension(ext_lower: &str) -> bool {
    const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
    IMAGE_EXTS.contains(&ext_lower)
}

fn is_read_file_binary_extension(ext_lower: &str) -> bool {
    const BINARY_EXTS: &[&str] = &[
        "svg", "pdf", "zip", "gz", "tar", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib",
        "o", "a", "lib", "wasm", "class", "pyc", "pyo", "mp3", "mp4", "avi", "mov", "wav", "flac",
        "ogg", "ttf", "otf", "woff", "woff2", "eot", "sqlite", "db", "mdb", "ico",
    ];
    BINARY_EXTS.contains(&ext_lower)
}

fn edit_type_label(edit_type: astra_turn_core::file_edit_journal::EditType) -> &'static str {
    match edit_type {
        astra_turn_core::file_edit_journal::EditType::Create => "create",
        astra_turn_core::file_edit_journal::EditType::Overwrite => "overwrite",
        astra_turn_core::file_edit_journal::EditType::Patch => "patch",
        astra_turn_core::file_edit_journal::EditType::Delete => "delete",
    }
}

#[derive(Debug)]
pub(super) enum FsLeafError {
    SandboxDenied(String),
    NoEffect {
        output: String,
        evidence: astra_core::ToolFailureEvidence,
    },
    Other(String),
    Shared(astra_tools::ToolResult),
}

impl FsLeafError {
    fn sandbox_denied(message: String) -> Self {
        Self::SandboxDenied(message)
    }

    fn caller_correctable_no_effect(
        output: String,
        recovery_actions: Vec<astra_core::ToolRecoveryAction>,
    ) -> Self {
        Self::NoEffect {
            output,
            evidence: astra_core::ToolFailureEvidence::new(
                astra_core::ErrorKind::ToolInvalidArgs,
                astra_core::ToolFailureCause::InvalidArguments,
                false,
                recovery_actions,
            ),
        }
    }

    fn into_tool_result(self) -> astra_tools::ToolResult {
        match self {
            Self::SandboxDenied(message) => {
                let mut metadata =
                    crate::sandbox_retry::sandbox_denied_tool_result_fields(&message);
                metadata.insert("execution_started".to_string(), Value::Bool(false));
                metadata.insert(
                    "disposition".to_string(),
                    serde_json::to_value(
                        astra_services::session_journal::ToolCallDisposition::Rejected,
                    )
                    .expect("tool disposition must serialize"),
                );
                let mut result = astra_tools::ToolResult::error(format!("Error: {message}"));
                result.metadata = Some(metadata);
                result
            }
            Self::NoEffect { output, evidence } => astra_tools::ToolResult::error(output)
                .with_failure_evidence(evidence)
                .with_workspace_mutation_not_applied(),
            Self::Other(output) => astra_tools::ToolResult::error(output),
            Self::Shared(result) => result,
        }
    }

    pub(super) fn into_string_output(self) -> String {
        match self {
            Self::SandboxDenied(message) => format!("{SANDBOX_DENIED_PREFIX}{message}"),
            Self::NoEffect { output, .. } => output,
            Self::Other(output) => output,
            Self::Shared(result) => result.output,
        }
    }
}

impl From<String> for FsLeafError {
    fn from(output: String) -> Self {
        Self::Other(output)
    }
}

// Path access belongs to sandbox authorization. Approval display and tool-output
// redaction protect secrets without adding a parallel filename-based write policy.

impl ToolExecutor {
    fn read_file_model_output_limit(&self) -> usize {
        self.scaled_output_limit().min(READ_FILE_MODEL_RESULT_CHARS)
    }

    fn read_file_body_output_limit(&self) -> usize {
        let hard_limit = self.read_file_model_output_limit();
        hard_limit
            .saturating_sub(READ_FILE_DELIVERY_MARGIN_CHARS)
            .max(hard_limit.min(1024))
    }

    fn record_fuzzy_match_event(
        &self,
        path: &Path,
        strategy: &str,
        outcome: astra_runtime::observability::FuzzyMatchOutcome,
    ) {
        let Some(session) = &self.observability_session else {
            return;
        };
        let mut session = match session.write() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        session.record_fuzzy_match_event(path.display().to_string(), strategy, outcome);
    }

    fn resolve_checked_result(&self, path: &str) -> Result<PathBuf, FsLeafError> {
        if is_unc_path(path) {
            return Err("Error: UNC/network paths are not supported (security risk)"
                .to_string()
                .into());
        }
        let expanded_home_path = expand_home_path_arg(path);
        let path_for_validation = expanded_home_path
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned());
        let validation_path = path_for_validation.as_deref().unwrap_or(path);
        let p = expanded_home_path
            .as_deref()
            .unwrap_or_else(|| Path::new(path));
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
                    )
                    .into());
                }
            }
        }

        {
            let sp_guard = self
                .sandbox_policy
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(ref policy) = *sp_guard {
                return validate_path(policy, validation_path).map_err(|e| {
                    if e.is_boundary_violation() {
                        FsLeafError::sandbox_denied(format!(
                            "Path '{}' is outside the project directory '{}'; \
                             sandbox approval is required for this external path.",
                            validation_path,
                            policy.project_root.display(),
                        ))
                    } else {
                        format!("Sandbox: {e}").into()
                    }
                });
            }
        }
        Ok(resolved)
    }

    /// String API adapter for current filesystem callers that still consume
    /// the sandbox-denial wire prefix.
    pub(crate) fn resolve_checked(&self, path: &str) -> Result<PathBuf, String> {
        self.resolve_checked_result(path)
            .map_err(FsLeafError::into_string_output)
    }

    /// Bind the already-authorized spelling to its actual publication target.
    /// Atomic rename must replace the referent, not a symlink directory entry.
    pub(super) fn bind_file_mutation_target(&self, path: &Path) -> Result<PathBuf, FsLeafError> {
        let target = match path.canonicalize() {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink()) {
                    return Err(
                        format!("Error: Cannot bind dangling symlink {}", path.display()).into(),
                    );
                }
                astra_sandbox::canonicalize_parent_and_append(path)
                    .map_err(|error| format!("Error: Cannot bind file target: {error}"))?
            }
            Err(error) => {
                return Err(
                    format!("Error: Cannot bind file target {}: {error}", path.display()).into(),
                );
            }
        };
        if !self.is_within_sandbox_boundary(&target) {
            return Err(FsLeafError::sandbox_denied(format!(
                "Path '{}' resolves outside the approved sandbox boundary",
                path.display()
            )));
        }
        Ok(target)
    }

    pub(super) fn verify_file_mutation_binding(
        &self,
        path: &Path,
        target: &Path,
    ) -> Result<(), FsLeafError> {
        if self.bind_file_mutation_target(path)? != target {
            return Err(format!(
                "Error: File target binding changed for {}; re-read the file before editing",
                path.display()
            )
            .into());
        }
        Ok(())
    }

    fn record_committed_file_edit(
        &self,
        path: &Path,
        call_id: &str,
        before: Option<&[u8]>,
        after: &[u8],
        edit_type: EditType,
    ) {
        let turn = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        self.file_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_committed(path, call_id, turn, before, after, edit_type);
        self.record_write_with_content(
            path,
            std::str::from_utf8(after).expect("prepared text publication is UTF-8"),
        );
    }

    pub(super) fn apply_prepared_file_edit(
        &self,
        prepared: &PreparedWriteFile,
        call_id: &str,
        edit_type: EditType,
    ) -> astra_tools::ToolResult {
        let result = prepared.apply();
        if !result.is_error && !prepared.is_already_desired() {
            self.record_committed_file_edit(
                prepared.path(),
                call_id,
                prepared.original_content_bytes(),
                prepared.content_bytes(),
                if prepared.original_content_bytes().is_none() {
                    EditType::Create
                } else {
                    edit_type
                },
            );
        }
        result
    }

    pub(crate) fn read_file(&self, args: &Value) -> String {
        self.read_file_with_metadata(args).output
    }

    pub(crate) fn read_file_with_metadata(&self, args: &Value) -> astra_tools::ToolResult {
        if let Err(error) = validate_read_file_args(args) {
            return FsLeafError::caller_correctable_no_effect(
                error,
                vec![astra_core::ToolRecoveryAction::CorrectArguments],
            )
            .into_tool_result()
            .with_execution_not_started();
        }
        match self.read_file_impl(args) {
            Ok(output) => astra_tools::ToolResult::text(output),
            Err(error) => error.into_tool_result(),
        }
    }

    fn read_file_impl(&self, args: &Value) -> Result<String, FsLeafError> {
        let path_str = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => {
                return Err("Error: missing required field `path` for read_file. Valid fields: path, start_line, end_line, outline."
                    .to_string().into());
            }
        };
        let path = self.resolve_checked_result(path_str)?;

        // Device file blocking — prevent hangs on infinite/blocking device files
        {
            let path_str_lower = path.to_string_lossy().to_lowercase();
            if is_blocked_device_read_path(&path_str_lower) {
                return Err(format!(
                    "Error: refusing to read device file '{}' (would block or produce infinite output)",
                    path.display()
                ).into());
            }
        }

        // Binary file guard — refuse to read known binary extensions, but allow images
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();

            // Image files: return base64 for vision models
            if is_read_file_image_extension(&ext_lower) {
                // Check file size before reading — base64 inflates by ~33%
                if let Ok(meta) = fs::metadata(&path)
                    && meta.len() > 1_500_000
                {
                    return Err(format!(
                        "Error: image too large ({} bytes). Use bash to resize first.",
                        meta.len()
                    )
                    .into());
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
                        self.record_read(&path, false);
                        return Ok(format!("data:{mime};base64,{b64}"));
                    }
                    Err(e) => return Err(format!("Error reading image: {e}").into()),
                }
            }

            // Other binary files: block
            if is_read_file_binary_extension(&ext_lower) {
                return Err(format!(
                    "Error: refusing to read binary file (.{ext}). Use bash with appropriate tools (e.g. file, xxd, strings) for binary analysis."
                ).into());
            }
        }

        let start_raw = args.get("start_line").and_then(Value::as_u64);
        let end_raw = args.get("end_line").and_then(Value::as_u64);
        let has_range = start_raw.is_some() || end_raw.is_some();
        let has_outline = args
            .get("outline")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        // Pre-read size gate: large files without a line range auto-degrade
        // to outline mode + guidance. Old behavior was a hard refusal ("Error:
        // file is too large") which gave the LLM no useful content — it then
        // had to guess start_line/end_line blind. New behavior: read the file
        // anyway (for outline only), return the outline + explicit instructions
        // on how to drill into specific ranges.
        if !has_range
            && !has_outline
            && let Ok(meta) = fs::metadata(&path)
        {
            let size = meta.len() as usize;
            let limit = self.scaled_output_limit();
            if size > limit {
                // Capture a bounded prefix for a partial outline; it is not
                // evidence of definitions or line counts in the unread suffix.
                let cap = limit.saturating_mul(2).min(size);
                let content = read_capped_to_string_lossy(&path, cap)
                    .map_err(|e| format!("Error reading file: {e}"))?;
                let lines_in_cap = content.lines().count();
                let safe_outline_text =
                    astra_tools::fs_ops::render_outline(&path, &content, lines_in_cap)
                        .unwrap_or_else(|| {
                            format!("(no definitions found in {lines_in_cap}-line sample)")
                        });
                let coverage = if cap < size {
                    format!("partial content: sampled first {cap} bytes")
                } else {
                    format!("{lines_in_cap} lines")
                };

                return Ok(format!(
                    "File is large ({size} bytes, {coverage}). \
                     Outline of the captured content below.\n\n\
                     {safe_outline_text}\n\n\
                     To read specific sections, use:\n\
                     • read_file(path=\"{path_str}\", start_line=1, end_line=100) — first 100 lines\n\
                     • read_file(path=\"{path_str}\", start_line=N, end_line=M) — specific range\n\
                     • grep(pattern=\"keyword\", path=\"{path_str}\") — find specific symbols",
                ));
            }
        }

        // Aggregate output gate: when cumulative tool output this turn is
        // already high and a full-file read would be too large, auto-downgrade
        // to outline mode instead of blocking. Ranged reads are always allowed
        // (they're already targeted). Degrades gracefully instead of blocking.
        if !has_range && !has_outline {
            let agg = self
                .aggregate_output_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
            if agg > AGGREGATE_SOFT_LIMIT {
                if let Ok(meta) = fs::metadata(&path) {
                    let size = meta.len() as usize;
                    let remaining = AGGREGATE_OUTPUT_BUDGET.saturating_sub(agg);
                    if size > remaining {
                        // Auto-downgrade: return outline instead of full content
                        let content_for_outline =
                            if let Some(cached) = self.get_cached_content(&path) {
                                cached
                            } else {
                                read_to_string_lossy(&path).map_err(|e| format!("Error: {e}"))?
                            };
                        let total_lines = content_for_outline.lines().count();
                        let (safe_content_for_outline, _) =
                            astra_text_utils::credential_redaction::redact_credentials_in_text(
                                &content_for_outline,
                            );
                        self.record_read_cached(&path, true, content_for_outline.clone());

                        if let Some(outline) = astra_tools::fs_ops::render_outline(
                            &path,
                            &content_for_outline,
                            total_lines,
                        ) {
                            return Ok(format!(
                                "[Auto-downgraded to outline — aggregate output budget is high \
                                 ({agg} bytes used). Use start_line/end_line to read specific sections.]\n{outline}"
                            ));
                        }

                        // No outline available — return truncated content with hint
                        let marker = format!(
                            "\n[Auto-truncated — aggregate output budget is high \
                             ({agg} bytes used, file has {total_lines} lines). \
                             Use start_line/end_line to read specific sections.]"
                        );
                        let budget = self
                            .read_file_body_output_limit()
                            .saturating_sub(marker.chars().count());
                        let lines: Vec<&str> = safe_content_for_outline.split('\n').collect();
                        let mut delivered = add_line_numbers_budgeted(&lines, 1, budget).output;
                        push_suffix_if_fits(
                            &mut delivered,
                            &marker,
                            self.read_file_model_output_limit(),
                        );
                        return Ok(delivered);
                    }
                }
            }
        }

        // Try in-memory content cache before disk I/O.
        // Cache hit when file was previously read/written and mtime is unchanged.
        let raw_content = if let Some(cached) = self.get_cached_content(&path) {
            cached
        } else {
            match read_to_string_lossy(&path) {
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
                        return Err(format!(
                            "{msg}. Note: current working directory is {cwd}. Use list_dir or glob to find the correct path first.{hint}"
                        ).into());
                    }
                    if e.kind() == std::io::ErrorKind::IsADirectory {
                        return Err(format!("{msg}. Use list_dir instead for directories.").into());
                    }
                    if e.kind() == std::io::ErrorKind::PermissionDenied {
                        return Err(format!(
                            "{msg}. Check file permissions or use bash with `sudo cat` if appropriate."
                        ).into());
                    }
                    return Err(msg.into());
                }
            }
        };
        // This executor owns the workspace read, so it is the only boundary
        // allowed to issue an edit-capable redaction reference.  Redact the
        // full content before any output budget/window is applied; otherwise
        // a credential split at the model limit becomes an unrecognisable
        // partial secret.  Keep `raw_content` for AST parsing, line-range
        // accounting, and the staleness cache.
        let (safe_content, _) =
            astra_text_utils::credential_redaction::redact_credentials_in_text(&raw_content);

        // Outline isolation: return only definition signatures with line numbers
        if has_outline {
            let total_lines = raw_content.lines().count();
            // An outline does not authorize a full overwrite, even when cached.
            self.record_read_cached(&path, true, raw_content.clone());
            return Ok(
                astra_tools::fs_ops::render_outline(&path, &raw_content, total_lines)
                    .unwrap_or_else(|| {
                        format!("(no definitions found in {total_lines}-line file)")
                    }),
            );
        }

        let is_ranged = has_range;
        let normalized_range = is_ranged.then(|| {
            normalize_read_file_line_range(
                start_raw.map(|n| n as usize),
                end_raw.map(|n| n as usize),
                raw_content.lines().count(),
            )
        });
        // Auto-expand: promote ranged reads to full-file reads when the file
        // is small enough. This eliminates fragmented multi-range reads that
        // waste tool calls (e.g., 6 read_file calls for different hunks of
        // a 200-line file). Works on FIRST read too, not just subsequent reads.
        // Hard cap at 16 KB (~4000 tokens) to prevent large files from
        // exploding context even if they fit the dynamic output budget. The
        // rendered tool result must also fit the model-side read_file budget;
        // otherwise it would be compressed after we had incorrectly recorded
        // the file as fully delivered.
        const AUTO_EXPAND_MAX_BYTES: usize = 16_384;
        if is_ranged {
            if let Ok(meta) = fs::metadata(&path)
                && (meta.len() as usize) <= AUTO_EXPAND_MAX_BYTES
                && raw_content.len() <= AUTO_EXPAND_MAX_BYTES
            {
                let total_lines = raw_content.lines().count();
                let expanded_content = astra_text_utils::credential_redaction::redact_line_window(
                    &raw_content,
                    1,
                    total_lines,
                );
                let numbered = add_line_numbers(&expanded_content, 1);
                let expanded = format!(
                    "[Auto-expanded to full file ({total_lines} lines) — \
                     small enough to read entirely. Use this content for all \
                     references to {path_str}; do not re-read.]\n\
                     {numbered}"
                );
                if expanded.chars().count() <= self.read_file_model_output_limit() {
                    let delivered_range = (total_lines > 0
                        && expanded_content.lines().count() == total_lines)
                        .then_some(super::file_state::DeliveredLineRange {
                            start: 1,
                            end: total_lines as u64,
                        });
                    let _ = self.record_read_with_delivery_range_cached(
                        &path,
                        false,
                        raw_content.clone(),
                        delivered_range,
                    );
                    return Ok(expanded);
                }
            }
        }

        if !is_ranged {
            let lines: Vec<&str> = safe_content.split('\n').collect();
            let total_lines = raw_content.lines().count();
            let numbered = add_line_numbers(&safe_content, 1);
            let mut output;
            let is_partial_delivery;
            let delivered_line_end;

            if numbered.chars().count() <= self.read_file_model_output_limit() {
                output = numbered;
                is_partial_delivery = false;
                delivered_line_end = (total_lines > 0).then_some(total_lines as u64);
            } else {
                let delivery =
                    add_line_numbers_budgeted(&lines, 1, self.read_file_body_output_limit());
                let delivered_end = delivery.complete_lines as u64;
                delivered_line_end = (delivered_end > 0).then_some(delivered_end);
                output = delivery.output;
                let marker = if delivered_end > 0 {
                    format!(
                        "\n[truncated — file has {total_lines} lines; delivered through line \
                         {delivered_end}. Use read_file(path=\"{path_str}\", start_line={}, \
                         end_line=...) or outline=true to read specific sections.]",
                        delivered_end + 1
                    )
                } else {
                    format!(
                        "\n[truncated — first line exceeds the read_file result budget for \
                         {path_str}. Use grep or a narrower start_line/end_line range.]"
                    )
                };
                push_suffix_if_fits(&mut output, &marker, self.read_file_model_output_limit());
                is_partial_delivery = true;
            }

            let delivered_range = delivered_line_end
                .filter(|_| safe_content.lines().count() == total_lines)
                .map(|end| super::file_state::DeliveredLineRange { start: 1, end });
            let overlaps_prior_delivery = self.record_read_with_delivery_range_cached(
                &path,
                is_partial_delivery,
                raw_content.clone(),
                delivered_range,
            );

            let read_warning = Self::read_warning_for(overlaps_prior_delivery);
            push_suffix_if_fits(
                &mut output,
                &read_warning,
                self.read_file_model_output_limit(),
            );
            return Ok(output);
        }

        // Keep range coordinates tied to the raw file.  Redaction can
        // collapse a multi-line PEM into one marker, so slicing the already
        // redacted view would shift user-requested line numbers.
        let lines: Vec<&str> = raw_content.lines().collect();
        let Some(range) = normalized_range else {
            return Err("(internal error: ranged read without normalized range)"
                .to_string()
                .into());
        };
        let s = range.start_line.saturating_sub(1).min(lines.len());
        let e = range.end_line.min(lines.len());
        if s >= lines.len() {
            return Err(format!(
                "Error: start_line {} exceeds file length {}",
                range.start_line,
                lines.len()
            )
            .into());
        }
        if s >= e {
            return Ok(format!(
                "(empty range: start_line {} >= end_line {} or file has only {} lines)",
                s + 1,
                e,
                lines.len()
            ));
        }
        let actual_start_line = s + 1; // 1-indexed
        let safe_range = astra_text_utils::credential_redaction::redact_line_window(
            &raw_content,
            actual_start_line,
            e,
        );
        let source_line_count = e.saturating_sub(s);
        let line_numbers_preserve_source = safe_range.lines().count() == source_line_count;
        let numbered = add_line_numbers(&safe_range, actual_start_line);
        let mut result;
        let mut delivered_line_end = None;

        if numbered.chars().count() <= self.read_file_model_output_limit() {
            result = numbered;
            if line_numbers_preserve_source {
                delivered_line_end = Some(e as u64);
            }
        } else {
            let safe_lines: Vec<&str> = safe_range.split('\n').collect();
            let delivery = add_line_numbers_budgeted(
                &safe_lines,
                actual_start_line,
                self.read_file_body_output_limit(),
            );
            let delivered_end = if delivery.complete_lines > 0 {
                Some(actual_start_line as u64 + delivery.complete_lines as u64 - 1)
            } else {
                None
            };
            if line_numbers_preserve_source {
                delivered_line_end = delivered_end;
            }
            result = delivery.output;
            let marker = if let Some(end_line) = delivered_end {
                format!(
                    "\n[truncated — requested lines {actual_start_line}–{e}; delivered through \
                     line {end_line}. Continue with read_file(path=\"{path_str}\", \
                     start_line={}, end_line=...).]",
                    end_line + 1
                )
            } else {
                format!(
                    "\n[truncated — line {actual_start_line} exceeds the read_file result \
                     budget for {path_str}. Use grep or a narrower range.]"
                )
            };
            push_suffix_if_fits(&mut result, &marker, self.read_file_model_output_limit());
        }
        let delivered_range = delivered_line_end.map(|end| super::file_state::DeliveredLineRange {
            start: actual_start_line as u64,
            end,
        });
        let overlaps_prior_delivery = self.record_read_with_delivery_range_cached(
            &path,
            true,
            raw_content.clone(),
            delivered_range,
        );

        let read_warning = Self::read_warning_for(overlaps_prior_delivery);
        push_suffix_if_fits(
            &mut result,
            &read_warning,
            self.read_file_model_output_limit(),
        );
        Ok(result)
    }

    fn read_warning_for(overlaps_prior_delivery: bool) -> String {
        if overlaps_prior_delivery {
            "\n\nNote: Some returned content overlaps lines already returned for the same captured file content. \
             Earlier output may no longer be in context; authorized rereads can still be necessary."
                .to_string()
        } else {
            String::new()
        }
    }

    /// Returns JSON with structured result for reliable parsing.
    pub(crate) fn write_file(&self, args: &Value) -> String {
        self.write_file_with_applied(args).0
    }

    /// Execute a direct file write and retain the owner-side commit fact.
    ///
    /// The Edge transport must not reconstruct this from the display body or
    /// a scan of unrelated workspace contents. `applied` is true only when
    /// the successful commit changed the target bytes.
    pub(crate) fn write_file_with_applied(&self, args: &Value) -> (String, bool, bool) {
        let mut applied = false;
        let mut already_desired = false;
        let output = self.write_file_impl(args, &mut applied, &mut already_desired);
        (output, applied, already_desired)
    }

    fn write_file_impl(
        &self,
        args: &Value,
        applied: &mut bool,
        already_desired: &mut bool,
    ) -> String {
        use serde_json::json;

        let path_arg = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return json!({ "success": false, "error": "missing 'path'" }).to_string(),
        };
        let path = match self.resolve_checked(path_arg) {
            Ok(safe) => safe,
            Err(e) => return json!({ "success": false, "error": e }).to_string(),
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(c) => c,
            None => return json!({ "success": false, "error": "missing 'content'" }).to_string(),
        };

        let target = match self.bind_file_mutation_target(&path) {
            Ok(target) => target,
            Err(error) => {
                return json!({"success": false, "error": error.into_string_output()}).to_string();
            }
        };
        let prior_bytes = match fs::read(&target) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return json!({"success": false, "error": format!("Error reading file: {error}")})
                    .to_string();
            }
        };
        let prepared =
            PreparedWriteFile::from_authorized_preimage(target, path_arg, content, prior_bytes);
        let content = std::str::from_utf8(prepared.content_bytes())
            .expect("prepared text publication is UTF-8");

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

        // The shared owner verifies the captured preimage even for an exact
        // no-op, before any journal entry or read-before-overwrite requirement.
        if prepared.is_already_desired() {
            if let Err(error) = self.verify_file_mutation_binding(&path, prepared.path()) {
                return json!({"success": false, "error": error.into_string_output()}).to_string();
            }
            let mut result = prepared.apply();
            // This display adapter returns only the no-op fact. The outer
            // execution boundary mints its own invocation-bound marker.
            if let Some(fields) = result.metadata.as_mut() {
                astra_tools::workspace_observation::discard_workspace_desired_state_convergence_marker(fields);
            }
            if result.is_error {
                return json!({"success": false, "error": result.output}).to_string();
            }
            *already_desired = true;
            return json!({
                "success": true,
                "state": "already_desired",
                "bytes_written": 0,
                "path": path.to_string_lossy().to_string(),
            })
            .to_string();
        }

        // Staleness check: if file exists, it must have been read first and not modified since
        if prepared.original_content_bytes().is_some() {
            if let Err(e) = self.check_staleness(&path) {
                return json!({ "success": false, "error": e }).to_string();
            }
            // Require full read (not outline/partial) before overwriting
            if !self.was_fully_read(&path) {
                return json!({
                    "success": false,
                    "error": format!(
                        "File was only partially read (outline or line range). Read the full file before overwriting.\n\
                         → Action required: call read_file(\"{}\") (without start_line/end_line) first, then retry.\n\
                         If this file is too large for a full read, use str_replace or multi_edit with an exact old_str copied from a fresh range read; do not retry the same outline overwrite.",
                        self.project_relative_display(&path)
                    )
                }).to_string();
            }
        }

        let prior_for_diff = prepared
            .original_content_bytes()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned());

        if let Err(error) = self.verify_file_mutation_binding(&path, prepared.path()) {
            return json!({"success": false, "error": error.into_string_output()}).to_string();
        }

        let journal_call_id = format!("write_file:{}", path.display());
        let publication =
            self.apply_prepared_file_edit(&prepared, &journal_call_id, EditType::Overwrite);
        if !publication.is_error {
            *applied = true;
            let old_slice = prior_for_diff.as_deref().unwrap_or("");
            let cli_diff = cap_cli_unified_diff(unified_diff_raw(old_slice, content, &path));
            let lsp_diag = self.inline_lsp_diagnostics(&path);
            let mut obj = json!({
                "success": true,
                "bytes_written": content.len(),
                "path": path.to_string_lossy().to_string(),
                "_cli_unified_diff": cli_diff,
            });
            if let Some(diag) = lsp_diag {
                obj["lsp_diagnostics"] = Value::String(diag);
            }
            obj.to_string()
        } else {
            json!({ "success": false, "error": publication.output }).to_string()
        }
    }

    pub(crate) fn str_replace(&self, args: &Value) -> String {
        self.str_replace_with_applied(args).0.output
    }

    /// Execute a direct replacement and retain its owner-side commit fact.
    pub(crate) fn str_replace_with_applied(&self, args: &Value) -> (astra_tools::ToolResult, bool) {
        let mut applied = false;
        let result = match self.str_replace_impl(args, &mut applied) {
            Ok(output) => astra_tools::ToolResult::text(output),
            Err(error) => error.into_tool_result(),
        };
        (result, applied)
    }

    fn str_replace_impl(&self, args: &Value, applied: &mut bool) -> Result<String, FsLeafError> {
        let path_arg = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return Err("Error: missing 'path'".to_string().into()),
        };
        let path = self.resolve_checked_result(path_arg)?;
        astra_tools::fs_ops::validate_str_replace_args(args).map_err(FsLeafError::Shared)?;
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let replace_all = args
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let target = self.bind_file_mutation_target(&path)?;
        let original_bytes = fs::read(&target).map_err(|e| format!("Error reading file: {e}"))?;
        let content = std::str::from_utf8(&original_bytes)
            .map_err(|_| {
                "Error: File is not valid UTF-8; text edits cannot preserve its bytes".to_owned()
            })?
            .to_owned();
        let candidate = astra_tools::fs_ops::PreparedStrReplace::from_authorized_preimage(
            target.clone(),
            content.clone(),
            args,
        )
        .map_err(|error| {
            if let Some(metadata) = &error.metadata {
                let outcome = match metadata
                    .get("replacement_match_outcome")
                    .and_then(Value::as_str)
                {
                    Some("ambiguous") => {
                        Some(astra_runtime::observability::FuzzyMatchOutcome::Ambiguous)
                    }
                    Some("not_found") => {
                        Some(astra_runtime::observability::FuzzyMatchOutcome::NotFound)
                    }
                    _ => None,
                };
                if let (Some(strategy), Some(outcome)) = (
                    metadata
                        .get("replacement_match_strategy")
                        .and_then(Value::as_str),
                    outcome,
                ) {
                    self.record_fuzzy_match_event(&path, strategy, outcome);
                }
            }
            FsLeafError::Shared(error)
        })?;
        let actual = candidate.matched_text();
        let replacement = candidate.replacement();
        let strategy = candidate.match_strategy().unwrap_or("exact");
        let count = candidate.occurrence_count();
        let prepared = candidate.publication();
        let new_content = std::str::from_utf8(prepared.content_bytes())
            .expect("prepared text publication is UTF-8");

        // A matched anchor authorizes this localized edit, not a full-file read.
        self.record_read_cached(prepared.path(), true, content.clone());
        if dry_run {
            self.record_fuzzy_match_event(
                &path,
                strategy,
                astra_runtime::observability::FuzzyMatchOutcome::Matched,
            );
            return Ok(unified_diff(&content, new_content, &path));
        }
        self.check_staleness(&path)
            .map_err(|e| format!("Error: Pre-write staleness check failed: {e}"))?;
        self.verify_file_mutation_binding(&path, prepared.path())?;
        let journal_prefix = if strategy == "exact" {
            "str_replace"
        } else if strategy == astra_tools::fuzzy_replacer::STRATEGY_QUOTE_NORMALIZED {
            "str_replace_quote_norm"
        } else {
            "str_replace_fuzzy"
        };
        let journal_call_id = format!("{journal_prefix}:{}", path.display());
        let publication =
            self.apply_prepared_file_edit(prepared, &journal_call_id, EditType::Patch);
        if publication.is_error {
            return Err(publication.output.into());
        }
        *applied = true;
        let old_lines: Vec<&str> = actual.lines().collect();
        let new_lines: Vec<&str> = replacement.lines().collect();
        let small_edit = old_lines.len().max(new_lines.len()) <= 10;
        let mut result = if strategy == astra_tools::fuzzy_replacer::STRATEGY_QUOTE_NORMALIZED {
            "Replaced successfully (matched after normalizing curly quotes → ASCII)\n".to_string()
        } else if strategy != "exact" {
            format!("Replaced successfully (matched via {strategy})\n")
        } else if small_edit {
            "Replaced successfully\n".to_string()
        } else {
            format!(
                "Replaced successfully ({} lines → {} lines)",
                old_lines.len(),
                new_lines.len()
            )
        };
        if small_edit {
            for line in old_lines {
                result.push_str(&format!("- {line}\n"));
            }
            for line in new_lines {
                result.push_str(&format!("+ {line}\n"));
            }
        }
        if strategy == "exact" {
            if replace_all && count > 1 {
                result = format!("Replaced {count} occurrences\n{result}");
            }
            if let Some(lang) = code_intel::detect_language(&path) {
                let edit_line = content[..candidate.first_edit_start()]
                    .matches('\n')
                    .count()
                    + 1;
                let scope = code_intel::scope_at_line(new_content, lang, edit_line);
                if !scope.breadcrumbs.is_empty() {
                    result.push_str(&format!("\n📍 {}", scope.breadcrumbs.join(" > ")));
                }
            }
        }
        append_str_replace_cli_unified_diff(&mut result, &content, new_content, &path);
        if let Some(diag) = self.inline_lsp_diagnostics(&path) {
            result.push_str(&diag);
        }
        self.record_fuzzy_match_event(
            &path,
            strategy,
            astra_runtime::observability::FuzzyMatchOutcome::Matched,
        );
        Ok(result)
    }

    pub(crate) fn delete_file(&self, args: &Value) -> String {
        self.delete_file_with_applied(args).0
    }

    /// Execute a direct deletion and retain its owner-side commit fact.
    pub(crate) fn delete_file_with_applied(&self, args: &Value) -> (String, bool) {
        let mut applied = false;
        let output = self.delete_file_impl(args, &mut applied);
        (output, applied)
    }

    fn delete_file_impl(&self, args: &Value, applied: &mut bool) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'path'".to_string(),
        };

        // Safety: refuse .git/ contents
        let rel_str = self.project_relative_display(&path);
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

        let before_content = match fs::read(&path) {
            Ok(content) => content,
            Err(error) => return format!("Error reading file before delete: {error}"),
        };
        let turn_idx = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let journal_call_id = format!("delete_file:{}", path.display());

        match fs::remove_file(&path) {
            Ok(_) => {
                *applied = true;
                self.remove_file_state(&path);
                match self.file_journal.lock() {
                    Ok(mut journal) => {
                        journal.record_delete(&path, &journal_call_id, turn_idx, before_content)
                    }
                    Err(poisoned) => poisoned.into_inner().record_delete(
                        &path,
                        &journal_call_id,
                        turn_idx,
                        before_content,
                    ),
                }
                format!("Deleted: {}", rel_str)
            }
            Err(e) => format!("Error deleting file: {e}"),
        }
    }

    fn rollback_display_path(&self, path: &Path) -> String {
        self.project_relative_display(path)
    }

    fn parse_rollback_tool_output(tool_name: &str, output: String) -> Value {
        serde_json::from_str(&output).unwrap_or_else(|error| {
            json!({
                "success": false,
                "error": format!("invalid {tool_name} output: {error}"),
                "raw_output": output,
            })
        })
    }

    fn refresh_rolled_back_file_state(&self, path: &Path) {
        if path.exists() {
            match read_to_string_lossy(path) {
                Ok(content) => self.record_write_with_content(path, &content),
                Err(_) => self.record_write(path),
            }
        } else {
            self.remove_file_state(path);
        }
    }

    pub(crate) fn file_journal_checkpoint(&self) -> u64 {
        match self.file_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    /// Rollback all file edits recorded since `checkpoint` within the
    /// given turn, transactionally (if any undo fails, already-undone
    /// entries are re-applied so disk state stays consistent).
    ///
    /// Used by `str_replace_batch` to recover from partial multi-file
    /// write failures.
    pub(crate) fn rollback_files_since_checkpoint(&self, turn_index: u32, checkpoint: u64) {
        let result = match self.file_journal.lock() {
            Ok(journal) => journal.undo_turn_since_transactional(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .undo_turn_since_transactional(turn_index, checkpoint),
        };
        // Refresh file-state tracking for every reverted path so
        // subsequent reads don't hit stale-timestamp guards.
        if let Ok(paths) = &result {
            for path in paths {
                self.refresh_rolled_back_file_state(path);
            }
        }
        if let Err(error) = result {
            astra_core::agent_warn!(
                "file_edit",
                "str_replace_batch rollback failed for turn {turn_index} checkpoint {checkpoint}: {error}"
            );
        }
    }

    pub(crate) async fn rollback_recorded_turn_mutations(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let explicit_turn_index = if scope == "turn" {
            match args.get("turn_index").and_then(Value::as_u64) {
                Some(turn_index) => Some(turn_index),
                None => {
                    return json!({
                        "success": false,
                        "error": "missing 'turn_index' for scope=turn",
                    })
                    .to_string();
                }
            }
        } else {
            None
        };

        match scope {
            "list" => {
                let file_result = Self::parse_rollback_tool_output(
                    "rollback_file_edits",
                    self.rollback_file_edits(args),
                );
                let database_result = Self::parse_rollback_tool_output(
                    "rollback_database_snapshots",
                    self.rollback_database_snapshots(args),
                );

                let worktree_result = Self::parse_rollback_tool_output(
                    "rollback_git_worktrees",
                    self.rollback_git_worktrees(args),
                );
                let session_state_result = Self::parse_rollback_tool_output(
                    "rollback_session_state",
                    self.rollback_session_state(args).await,
                );
                let file_entries = file_result
                    .get("entries")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let database_entries = database_result
                    .get("entries")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));

                let worktree_entries = worktree_result
                    .get("entries")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let session_state_entries = session_state_result
                    .get("entries")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let total_file_entries = file_result
                    .get("total_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        file_entries
                            .as_array()
                            .map(|entries| entries.len() as u64)
                            .unwrap_or(0)
                    });
                let total_database_entries = database_result
                    .get("total_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        database_entries
                            .as_array()
                            .map(|entries| entries.len() as u64)
                            .unwrap_or(0)
                    });

                let total_worktree_entries = worktree_result
                    .get("total_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        worktree_entries
                            .as_array()
                            .map(|entries| entries.len() as u64)
                            .unwrap_or(0)
                    });
                let total_session_state_entries = session_state_result
                    .get("total_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        session_state_entries
                            .as_array()
                            .map(|entries| entries.len() as u64)
                            .unwrap_or(0)
                    });
                json!({
                    "success": file_result.get("success").and_then(Value::as_bool).unwrap_or(false)
                        && database_result.get("success").and_then(Value::as_bool).unwrap_or(false)


                        && worktree_result.get("success").and_then(Value::as_bool).unwrap_or(false)
                        && session_state_result.get("success").and_then(Value::as_bool).unwrap_or(false),
                    "scope": "list",
                    "total_file_entries": total_file_entries,
                    "total_database_entries": total_database_entries,


                    "total_git_worktree_entries": total_worktree_entries,
                    "total_session_state_entries": total_session_state_entries,
                    "file_entries": file_entries,
                    "database_entries": database_entries,


                    "git_worktree_entries": worktree_entries,
                    "session_state_entries": session_state_entries,
                    "files": file_result,
                    "database_snapshots": database_result,


                    "git_worktrees": worktree_result,
                    "session_state": session_state_result,
                    "summary": format!(
                        "Listed {total_file_entries} file rollback entr{}, {total_database_entries} database snapshot entr{}, {total_worktree_entries} git worktree rollback entr{}, and {total_session_state_entries} session-state rollback entr{}",
                        if total_file_entries == 1 { "y" } else { "ies" },
                        if total_database_entries == 1 { "y" } else { "ies" },


                        if total_worktree_entries == 1 { "y" } else { "ies" },
                        if total_session_state_entries == 1 { "y" } else { "ies" }
                    ),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let database_result = Self::parse_rollback_tool_output(
                    "rollback_database_snapshots",
                    self.rollback_database_snapshots(args),
                );
                let file_result = Self::parse_rollback_tool_output(
                    "rollback_file_edits",
                    self.rollback_file_edits(args),
                );

                let worktree_result = Self::parse_rollback_tool_output(
                    "rollback_git_worktrees",
                    self.rollback_git_worktrees(args),
                );
                let session_state_result = Self::parse_rollback_tool_output(
                    "rollback_session_state",
                    self.rollback_session_state(args).await,
                );
                let turn_index = database_result
                    .get("turn_index")
                    .and_then(Value::as_u64)
                    .or_else(|| file_result.get("turn_index").and_then(Value::as_u64))
                    .or_else(|| worktree_result.get("turn_index").and_then(Value::as_u64))
                    .or_else(|| {
                        session_state_result
                            .get("turn_index")
                            .and_then(Value::as_u64)
                    })
                    .or(explicit_turn_index)
                    .unwrap_or_else(|| {
                        self.journal_turn_index
                            .load(std::sync::atomic::Ordering::Relaxed)
                            as u64
                    });
                let reverted_files = file_result
                    .get("reverted")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let restored_snapshots = database_result
                    .get("restored")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let failed_file_rollbacks = file_result
                    .get("failed")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let failed_database_rollbacks = database_result
                    .get("failed")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));

                let restored_git_worktrees = worktree_result
                    .get("restored")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let failed_git_worktree_rollbacks = worktree_result
                    .get("failed")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let restored_session_state = session_state_result
                    .get("restored")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let failed_session_state_rollbacks = session_state_result
                    .get("failed")
                    .cloned()
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                let reverted_file_count = reverted_files
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let restored_snapshot_count = restored_snapshots
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let failed_file_count = failed_file_rollbacks
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let failed_database_count = failed_database_rollbacks
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);

                let restored_git_worktree_count = restored_git_worktrees
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let failed_git_worktree_count = failed_git_worktree_rollbacks
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let restored_session_state_count = restored_session_state
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let failed_session_state_count = failed_session_state_rollbacks
                    .as_array()
                    .map(|entries| entries.len())
                    .unwrap_or(0);
                let restored_total = reverted_file_count
                    + restored_snapshot_count
                    + restored_git_worktree_count
                    + restored_session_state_count;
                let failed_total = failed_file_count
                    + failed_database_count
                    + failed_git_worktree_count
                    + failed_session_state_count;
                let success = restored_total > 0 && failed_total == 0;
                let summary = if restored_total == 0 {
                    format!("No recorded rollback actions found for turn {turn_index}")
                } else if failed_total == 0 {
                    format!(
                        "Rolled back {reverted_file_count} file edit{}, restored {restored_snapshot_count} database snapshot{}, removed {restored_git_worktree_count} recorded git worktree{}, and restored {restored_session_state_count} session-state mutation{} from turn {turn_index}",
                        if reverted_file_count == 1 { "" } else { "s" },
                        if restored_snapshot_count == 1 {
                            ""
                        } else {
                            "s"
                        },
                        if restored_git_worktree_count == 1 {
                            ""
                        } else {
                            "s"
                        },
                        if restored_session_state_count == 1 {
                            ""
                        } else {
                            "s"
                        },
                    )
                } else {
                    format!(
                        "Rolled back {reverted_file_count} file edit{}, restored {restored_snapshot_count} database snapshot{}, removed {restored_git_worktree_count} recorded git worktree{}, and restored {restored_session_state_count} session-state mutation{} from turn {turn_index} with {failed_total} failure{}",
                        if reverted_file_count == 1 { "" } else { "s" },
                        if restored_snapshot_count == 1 {
                            ""
                        } else {
                            "s"
                        },
                        if restored_git_worktree_count == 1 {
                            ""
                        } else {
                            "s"
                        },
                        if restored_session_state_count == 1 {
                            ""
                        } else {
                            "s"
                        },
                        if failed_total == 1 { "" } else { "s" }
                    )
                };
                json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "reverted_files": reverted_files,
                    "restored_database_snapshots": restored_snapshots,


                    "restored_git_worktrees": restored_git_worktrees,
                    "restored_session_state": restored_session_state,
                    "failed_file_rollbacks": failed_file_rollbacks,
                    "failed_database_rollbacks": failed_database_rollbacks,


                    "failed_git_worktree_rollbacks": failed_git_worktree_rollbacks,
                    "failed_session_state_rollbacks": failed_session_state_rollbacks,
                    "files": file_result,
                    "database_snapshots": database_result,


                    "git_worktrees": worktree_result,
                    "session_state": session_state_result,
                    "summary": summary,
                })
                .to_string()
            }
            other => json!({
                "success": false,
                "error": format!(
                    "invalid 'scope': {other} (expected one of current_turn, turn, list)"
                ),
            })
            .to_string(),
        }
    }

    pub(crate) fn rollback_file_edits(&self, args: &Value) -> String {
        if args.get("after_sequence").is_some() {
            return json!({
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
            "source_receipt" => {
                let receipt_id = match args.get("receipt_id").and_then(Value::as_str) {
                    Some(receipt_id) if !receipt_id.trim().is_empty() => receipt_id,
                    _ => {
                        return json!({
                            "success": false,
                            "scope": "source_receipt",
                            "error": "missing 'receipt_id' for scope=source_receipt",
                        })
                        .to_string();
                    }
                };
                let Some(session_id) = self
                    .active_session_id()
                    .filter(|id| !id.trim().is_empty())
                else {
                    return json!({
                        "success": false,
                        "scope": "source_receipt",
                        "error": "source receipt restore requires an active CLI session",
                    })
                    .to_string();
                };
                let owner_scope = format!("cli:{session_id}");
                match astra_tools::source_preimage::restore_receipt(
                    &self.project_root,
                    &owner_scope,
                    receipt_id,
                ) {
                    Ok(()) => json!({
                        "success": true,
                        "scope": "source_receipt",
                        "receipt_id": receipt_id,
                        "summary": "Restored the retained source preimage.",
                    })
                    .to_string(),
                    Err(error) => json!({
                        "success": false,
                        "scope": "source_receipt",
                        "receipt_id": receipt_id,
                        "error": error,
                    })
                    .to_string(),
                }
            }
            "list" => {
                let summary = match self.file_journal.lock() {
                    Ok(journal) => journal.summary(),
                    Err(poisoned) => poisoned.into_inner().summary(),
                };
                let entries: Vec<Value> = summary
                    .into_iter()
                    .map(|(path, turn_index, edit_type)| {
                        json!({
                            "path": self.rollback_display_path(&path),
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
            "file" => {
                let raw_path = match args.get("path").and_then(Value::as_str) {
                    Some(path) => path,
                    None => {
                        return json!({
                            "success": false,
                            "error": "missing 'path' for scope=file",
                        })
                        .to_string();
                    }
                };
                let path = match self.resolve_checked(raw_path) {
                    Ok(path) => path,
                    Err(error) => return error,
                };
                let mut candidates = vec![path.clone(), self.file_state_key(&path)];
                let aliased = self.prefer_project_root_alias(&path);
                if !candidates.iter().any(|candidate| candidate == &aliased) {
                    candidates.push(aliased);
                }
                let undo_result = match self.file_journal.lock() {
                    Ok(journal) => {
                        let mut outcome = Ok(None);
                        for candidate in &candidates {
                            outcome = journal.undo_file(candidate);
                            if !matches!(outcome, Ok(None)) {
                                break;
                            }
                        }
                        outcome
                    }
                    Err(poisoned) => {
                        let journal = poisoned.into_inner();
                        let mut outcome = Ok(None);
                        for candidate in &candidates {
                            outcome = journal.undo_file(candidate);
                            if !matches!(outcome, Ok(None)) {
                                break;
                            }
                        }
                        outcome
                    }
                };
                match undo_result {
                    Ok(Some(edit_type)) => {
                        self.refresh_rolled_back_file_state(&path);
                        json!({
                            "success": true,
                            "scope": "file",
                            "path": self.rollback_display_path(&path),
                            "edit_type": edit_type_label(edit_type),
                            "summary": format!(
                                "Rolled back the latest recorded edit for {}",
                                self.rollback_display_path(&path)
                            ),
                        })
                        .to_string()
                    }
                    Ok(None) => json!({
                        "success": false,
                        "scope": "file",
                        "path": self.rollback_display_path(&path),
                        "error": "no recorded file edit found for that path",
                    })
                    .to_string(),
                    Err(error) => json!({
                        "success": false,
                        "scope": "file",
                        "path": self.rollback_display_path(&path),
                        "error": error.to_string(),
                    })
                    .to_string(),
                }
            }
            "turn" | "current_turn" => {
                let turn_index = if scope == "turn" {
                    match args.get("turn_index").and_then(Value::as_u64) {
                        Some(turn_index) => turn_index as u32,
                        None => {
                            return json!({
                                "success": false,
                                "error": "missing 'turn_index' for scope=turn",
                            })
                            .to_string();
                        }
                    }
                } else {
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed)
                };
                let checkpoint = args
                    .get("file_after_sequence")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let result = match self.file_journal.lock() {
                    Ok(journal) => journal.undo_turn_since(turn_index, checkpoint),
                    Err(poisoned) => poisoned
                        .into_inner()
                        .undo_turn_since(turn_index, checkpoint),
                };
                for path in &result.reverted {
                    self.refresh_rolled_back_file_state(path);
                }
                let reverted: Vec<String> = result
                    .reverted
                    .iter()
                    .map(|path| self.rollback_display_path(path))
                    .collect();
                let failed: Vec<Value> = result
                    .failed
                    .iter()
                    .map(|(path, error)| {
                        json!({
                            "path": self.rollback_display_path(path),
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
            other => json!({
                "success": false,
                "error": format!(
                    "invalid 'scope': {other} (expected one of current_turn, turn, file, list, source_receipt)"
                ),
            })
            .to_string(),
        }
    }

    /// Project each committed file into CLI undo/cache state, including the
    /// committed prefix when a later file fails publication.
    pub(crate) fn str_replace_batch_result(&self, args: &Value) -> astra_tools::ToolResult {
        let top_path = args.get("path").and_then(Value::as_str);
        let edits = match args.get("edits").and_then(Value::as_array) {
            Some(e) => e,
            None => return astra_tools::ToolResult::error("Error: missing 'edits' array".into()),
        };
        if edits.is_empty() {
            return astra_tools::ToolResult::error("Error: 'edits' array is empty".into());
        }

        // Fast-path: same-file batch with top-level path and no per-edit paths
        if top_path.filter(|path| !path.trim().is_empty()).is_some()
            && edits
                .iter()
                .all(|edit| edit.get("path").and_then(Value::as_str).is_none())
        {
            let (result, applied) = self.multi_edit_with_applied(args);
            return if applied {
                result.with_workspace_mutation_applied()
            } else {
                result
            };
        }

        let groups = match astra_tools::fs_ops::partition_edits_by_path(edits, top_path) {
            Ok(g) => g,
            Err(e) => return astra_tools::ToolResult::error(e),
        };

        let mut bound_groups = Vec::with_capacity(groups.len());
        for (spelling, edits) in groups {
            let path = match self.resolve_checked_result(&spelling) {
                Ok(path) => path,
                Err(error) => return error.into_tool_result(),
            };
            let target = match self.bind_file_mutation_target(&path) {
                Ok(target) => target,
                Err(error) => return error.into_tool_result(),
            };
            bound_groups.push((spelling, path, target, edits));
        }
        let mut candidates = Vec::with_capacity(bound_groups.len());
        for (spelling, _, target, edits) in &bound_groups {
            let content = match fs::read_to_string(target) {
                Ok(content) => content,
                Err(error) => {
                    return astra_tools::ToolResult::error(format!(
                        "Error reading batch target: {error}"
                    ))
                    .with_workspace_mutation_not_applied();
                }
            };
            let scoped = json!({
                "path": spelling, "edits": edits,
                "dry_run": args.get("dry_run").and_then(Value::as_bool).unwrap_or(false),
                "allow_structural_change": args.get("allow_structural_change").and_then(Value::as_bool).unwrap_or(false),
            });
            let candidate = match astra_tools::fs_ops::PreparedMultiEdit::from_authorized_preimage(
                target.clone(),
                content,
                &scoped,
            ) {
                Ok(candidate) => candidate,
                Err(error) => return error,
            };
            candidates.push(candidate);
        }
        // Candidate construction for the complete group precedes binding
        // revalidation; preparation of a later file cannot hide alias changes.
        for ((spelling, _, _, _), candidate) in bound_groups.iter().zip(&candidates) {
            let path = match self.resolve_checked_result(spelling) {
                Ok(path) => path,
                Err(error) => return error.into_tool_result(),
            };
            if let Err(error) = self.check_staleness(&path) {
                return astra_tools::ToolResult::error(error);
            }
            if let Err(error) = self.verify_file_mutation_binding(&path, candidate.path()) {
                return error.into_tool_result();
            }
        }
        let prepared =
            match astra_tools::fs_ops::PreparedMultiPathEdit::from_authorized_edits(candidates) {
                Ok(prepared) => prepared,
                Err(error) => return error,
            };
        prepared.apply_with_committed(|edit| {
            self.record_committed_file_edit(
                edit.path(),
                &format!("batch_edit:{}", edit.path().display()),
                Some(edit.original_content_bytes()),
                edit.new_content_bytes(),
                EditType::Patch,
            );
        })
    }

    pub(crate) fn multi_edit(&self, args: &Value) -> String {
        self.multi_edit_with_applied(args).0.output
    }

    fn multi_edit_with_applied(&self, args: &Value) -> (astra_tools::ToolResult, bool) {
        let mut applied = false;
        let result = match self.multi_edit_impl(args, &mut applied) {
            Ok(output) => astra_tools::ToolResult::text(output),
            Err(error) => error.into_tool_result(),
        };
        (result, applied)
    }

    fn multi_edit_impl(&self, args: &Value, applied: &mut bool) -> Result<String, FsLeafError> {
        let path_arg = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return Err("Error: missing 'path'".to_string().into()),
        };
        let path = self.resolve_checked_result(path_arg)?;
        let edits = match args.get("edits").and_then(Value::as_array) {
            Some(e) => e,
            None => return Err("Error: missing 'edits' array".to_string().into()),
        };
        if edits.is_empty() {
            return Err("Error: 'edits' array is empty".to_string().into());
        }
        let dry_run = args
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let target = self.bind_file_mutation_target(&path)?;
        let original_bytes = fs::read(&target).map_err(|e| format!("Error reading file: {e}"))?;
        let content = std::str::from_utf8(&original_bytes)
            .map_err(|_| {
                "Error: File is not valid UTF-8; text edits cannot preserve its bytes".to_owned()
            })?
            .to_owned();

        self.check_staleness(&path)
            .map_err(|e| format!("Error: {e}"))?;

        let candidate = astra_tools::fs_ops::PreparedMultiEdit::from_authorized_preimage(
            target.clone(),
            content.clone(),
            args,
        )
        .map_err(FsLeafError::Shared)?;
        let fuzzy_applications = candidate.fuzzy_applications();
        let first_edit_start_byte = candidate.first_edit_start();

        let prepared = candidate.publication();
        let working = std::str::from_utf8(prepared.content_bytes())
            .expect("prepared text publication is UTF-8");

        // Dry run: show the same normalized candidate used by publication.
        if dry_run {
            return Ok(unified_diff(&content, working, &path));
        }

        self.verify_file_mutation_binding(&path, prepared.path())?;

        let journal_call_id = format!("batch_edit:{}", path.display());
        let publication =
            self.apply_prepared_file_edit(prepared, &journal_call_id, EditType::Patch);
        if !publication.is_error {
            // Owner-side commit boundary. This exact write succeeded with
            // a buffer that validation proved differs from the preimage;
            // transport must not reconstruct that fact from prose or a
            // bounded whole-workspace fingerprint.
            *applied = true;
            let mut result = format!("Applied {} edit(s) successfully", edits.len());
            // Disclose any edits that required fuzzy matching so
            // the caller sees the old_str wasn't byte-exact.
            // Format: one bullet per fuzzy edit, tagged with the
            // strategy name (whitespace-normalized, line-trimmed,
            // etc.) and the 1-based edit index.
            if !fuzzy_applications.is_empty() {
                result.push_str("\n⚠ fuzzy match used (old_str did not match byte-exactly):");
                for (idx, strategy) in fuzzy_applications {
                    result.push_str(&format!("\n  edit[{idx}]: {strategy}"));
                }
            }

            // Scope context for the first edit location
            if let Some(lang) = code_intel::detect_language(&path)
                && let Some(first_edit_start) = first_edit_start_byte
            {
                let edit_line = content[..first_edit_start].matches('\n').count() + 1;
                let scope = code_intel::scope_at_line(working, lang, edit_line);
                if !scope.breadcrumbs.is_empty() {
                    result.push_str(&format!("\n📍 {}", scope.breadcrumbs.join(" > ")));
                }
            }

            append_str_replace_cli_unified_diff(&mut result, &content, working, &path);
            Ok(result)
        } else {
            Err(publication.output.into())
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
        self.list_dir_recursive(&dir, depth, 0, &mut out, &mut visited);
        if out.is_empty() {
            "(empty)".to_string()
        } else {
            truncate_output(out, tool_output_limit())
        }
    }

    pub(crate) fn list_dir_recursive(
        &self,
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
                self.list_dir_recursive(&entry.path(), max_depth, cur + 1, out, visited);
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

        let mut candidates: Vec<(String, usize)> = Vec::new();

        if let Some(search_dir) = search_dir.filter(|d| d.exists()) {
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
        candidates.sort_by_key(|x| std::cmp::Reverse(x.1));
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

    /// Query LSP diagnostics for a file after a write/edit and return a compact
    /// inline summary. Returns None if LSP is not available or has no diagnostics.
    fn inline_lsp_diagnostics(&self, path: &std::path::Path) -> Option<String> {
        let diag_value = self
            .passive_lsp
            .diagnostics_for_file(&self.project_root, path)
            .ok()??;
        let items = diag_value.get("diagnostics")?.as_array()?;
        if items.is_empty() {
            return None;
        }
        let mut errors = 0u32;
        let mut warnings = 0u32;
        let mut details: Vec<String> = Vec::new();
        for item in items {
            let severity = item.get("severity").and_then(Value::as_u64).unwrap_or(4);
            let message = item.get("message").and_then(Value::as_str).unwrap_or("");
            let line = item
                .get("range")
                .and_then(|r| r.get("start"))
                .and_then(|s| s.get("line"))
                .and_then(Value::as_u64)
                .map(|l| l + 1) // LSP lines are 0-based
                .unwrap_or(0);
            match severity {
                1 => {
                    errors += 1;
                    if details.len() < 5 {
                        details.push(format!("  L{line} [error] {message}"));
                    }
                }
                2 => {
                    warnings += 1;
                    if details.len() < 5 {
                        details.push(format!("  L{line} [warn] {message}"));
                    }
                }
                _ => {} // info/hint — not surfaced inline
            }
        }
        if errors == 0 && warnings == 0 {
            return None;
        }
        let file_name = path.file_name().and_then(|f| f.to_str()).unwrap_or("file");
        let mut summary = format!("\n🔍 LSP diagnostics ({file_name}):");
        if errors > 0 {
            summary.push_str(&format!(
                " {errors} error{}",
                if errors > 1 { "s" } else { "" }
            ));
        }
        if warnings > 0 {
            if errors > 0 {
                summary.push(',');
            }
            summary.push_str(&format!(
                " {warnings} warning{}",
                if warnings > 1 { "s" } else { "" }
            ));
        }
        for d in &details {
            summary.push('\n');
            summary.push_str(d);
        }
        if (errors + warnings) as usize > details.len() {
            summary.push_str(&format!(
                "\n  ... and {} more",
                (errors + warnings) as usize - details.len()
            ));
        }
        Some(summary)
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

/// Generate a unified diff between old and new content for a given file path.
fn unified_diff(old_content: &str, new_content: &str, path: &std::path::Path) -> String {
    format!(
        "[DRY RUN] Preview of changes (not applied):\n{}",
        unified_diff_raw(old_content, new_content, path)
    )
}

fn append_str_replace_cli_unified_diff(out: &mut String, before: &str, after: &str, path: &Path) {
    use astra_turn_core::tool_result_sanitize::{STR_REPLACE_DIFF_END, STR_REPLACE_DIFF_START};
    out.push_str(STR_REPLACE_DIFF_START);
    out.push_str(&cap_cli_unified_diff(unified_diff_raw(before, after, path)));
    out.push_str(STR_REPLACE_DIFF_END);
}

/// Like `read_to_string_lossy` but reads at most `max_bytes` from the
/// file. Used by the large-file auto-outline path to avoid OOM when the
/// file is enormous (multi-GB). The returned string may be truncated
/// mid-line; callers doing line analysis should be aware of the tail.
fn read_capped_to_string_lossy(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file).take(max_bytes as u64);
    let mut bytes = Vec::with_capacity(max_bytes.min(1 << 20)); // cap alloc at 1MB initial
    reader.read_to_end(&mut bytes)?;
    match String::from_utf8(bytes) {
        Ok(s) => Ok(s),
        Err(e) => Ok(String::from_utf8_lossy(e.as_bytes()).into_owned()),
    }
}

// ─── Line numbers ───────────────────────────────────────────────────────────

const READ_FILE_DELIVERY_MARGIN_CHARS: usize = 1024;

struct NumberedReadDelivery {
    output: String,
    complete_lines: usize,
}

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

fn add_line_numbers_budgeted(
    lines: &[&str],
    start_line: usize,
    max_chars: usize,
) -> NumberedReadDelivery {
    if lines.is_empty() {
        return NumberedReadDelivery {
            output: String::new(),
            complete_lines: 0,
        };
    }

    let max_num = start_line + lines.len().saturating_sub(1);
    let width = max_num.to_string().len().max(1);
    let mut output = String::new();
    let mut output_chars = 0usize;
    let mut complete_lines = 0usize;

    for (i, line) in lines.iter().enumerate() {
        let rendered = format!("{:>width$}\t{line}", start_line + i);
        let separator_chars = usize::from(!output.is_empty());
        let rendered_chars = rendered.chars().count();
        if output_chars + separator_chars + rendered_chars > max_chars {
            if complete_lines == 0 && max_chars > 0 {
                // A single long line may contain a complete edit-capable
                // marker.  Do not cut that marker in half merely because the
                // line exceeds the model budget; the shared redacted
                // truncator drops/keeps the atomic span safely.
                output.push_str(
                    &astra_text_utils::credential_redaction::truncate_redacted_output(
                        rendered, max_chars,
                    ),
                );
            }
            return NumberedReadDelivery {
                output,
                complete_lines,
            };
        }
        if !output.is_empty() {
            output.push('\n');
            output_chars += 1;
        }
        output.push_str(&rendered);
        output_chars += rendered_chars;
        complete_lines += 1;
    }

    NumberedReadDelivery {
        output,
        complete_lines,
    }
}

fn push_suffix_if_fits(output: &mut String, suffix: &str, max_chars: usize) {
    if suffix.is_empty() {
        return;
    }
    if output.chars().count() + suffix.chars().count() <= max_chars {
        output.push_str(suffix);
    }
}

#[cfg(test)]
mod tests {
    use super::super::ToolExecutor;
    use super::{add_line_numbers, is_unc_path, similarity_score};
    use astra_text_utils::str_preview::truncate_str;
    use astra_turn_core::tool_result_sanitize::READ_FILE_MODEL_RESULT_CHARS;
    use serde_json::{Value, json};
    use std::io::Write;

    fn test_executor_in(dir: &std::path::Path) -> ToolExecutor {
        ToolExecutor::new(dir)
    }

    /// Create a file large enough (>16KB) to avoid auto-expand on ranged reads.
    /// Returns known content lines like "line 1", "line 2", etc.
    fn write_large_file(dir: &std::path::Path, name: &str, num_lines: usize) {
        use std::io::Write;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        // Each line is ~90 chars → 200 lines ≈ 18KB > 16KB auto-expand threshold
        for i in 1..=num_lines {
            writeln!(f, "line {i}: {}", "x".repeat(80)).unwrap();
        }
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
    fn read_file_outline_preserves_language_aliases_and_partial_read_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let executor = test_executor_in(dir.path());
        for (name, source, signature) in [
            (
                "types.pyi",
                "# def invented(): pass\n\ndef actual(key=\"AKIAIOSFODNN7EXAMPLE\"): ...\n",
                "def actual",
            ),
            (
                "module.mjs",
                "// function invented() {}\n\nexport function actual() {}\n",
                "function actual",
            ),
            (
                "module.mts",
                "// function invented() {}\n\nexport function actual(): void {}\n",
                "function actual",
            ),
        ] {
            std::fs::write(dir.path().join(name), source).unwrap();
            let result = executor.read_file(&serde_json::json!({"path": name, "outline": true}));
            assert!(result.contains(signature), "{result}");
            assert!(result.contains("3:"), "{result}");
            assert!(!result.contains("invented"), "{result}");
            assert!(!result.contains("AKIAIOSFODNN7EXAMPLE"), "{result}");
            if name == "types.pyi" {
                assert!(result.contains("[REDACTED:AWS_ACCESS_KEY]"), "{}", result);
            }
            let overwrite =
                executor.write_file(&serde_json::json!({"path": name, "content": "replacement"}));
            let rejected: Value = serde_json::from_str(&overwrite).unwrap();
            assert_eq!(rejected["success"], false, "{overwrite}");
            assert!(
                rejected["error"]
                    .as_str()
                    .unwrap()
                    .contains("partially read"),
                "{overwrite}"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join(name)).unwrap(),
                source
            );
        }
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

    // ── str_replace: integration via ToolExecutor ────────────────────────────

    #[test]
    fn str_replace_not_found_returns_hints() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        std::fs::write(&file_path, "  fn hello() {\n    println!(\"hi\");\n  }\n").unwrap();

        let executor = test_executor_in(dir.path());
        executor.read_file(&serde_json::json!({"path": "code.rs"}));

        // With fuzzy matching, slightly wrong indentation now succeeds via line-trimmed
        let result = executor.str_replace(&serde_json::json!({
            "path": "code.rs",
            "old_str": "fn hello() {\n  println!(\"hi\");\n}",
            "new_str": "fn hello() {}"
        }));
        assert!(
            result.contains("Replaced successfully") && result.contains("line-trimmed"),
            "should auto-fix via fuzzy matching: {result}"
        );

        // Truly non-matching content still returns hints
        std::fs::write(&file_path, "  fn hello() {\n    println!(\"hi\");\n  }\n").unwrap();
        executor.read_file(&serde_json::json!({"path": "code.rs"}));
        let result2 = executor.str_replace(&serde_json::json!({
            "path": "code.rs",
            "old_str": "fn totally_different() {\n  xyz();\n}",
            "new_str": "fn replaced() {}"
        }));
        assert!(
            result2.contains("STR_REPLACE FAILED"),
            "must include unified banner sentinel: {result2}"
        );
        assert!(
            result2.contains("WHAT:"),
            "must include WHAT line: {result2}"
        );
        assert!(
            result2.contains("NEXT:"),
            "must include NEXT line: {result2}"
        );
    }

    #[tokio::test]
    async fn str_replace_resolves_sanitized_credential_reference() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.txt");
        let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n";
        std::fs::write(&path, raw).unwrap();
        let session = std::sync::Arc::new(std::sync::RwLock::new(
            astra_runtime::observability::ObservabilitySession::new_simple("marker-edit"),
        ));
        let executor =
            test_executor_in(directory.path()).with_observability_session(session.clone());
        let read = executor
            .execute_with_metadata("read_file", &json!({"path":"settings.txt"}))
            .await;
        assert!(!read.is_error, "{}", read.output);
        assert!(!read.output.contains("AKIAIOSFODNN7EXAMPLE"));
        let start = read
            .output
            .find("[REDACTED:")
            .expect("real read returns an opaque marker");
        let end = read.output[start..].find(']').unwrap() + start + 1;
        let marker = &read.output[start..end];
        let result = executor
            .execute_with_metadata(
                "str_replace",
                &json!({
                    "path":"settings.txt", "old_str":marker, "new_str":"[configured-access-key]"
                }),
            )
            .await;
        assert!(!result.is_error, "{}", result.output);
        assert!(!result.output.contains("AKIAIOSFODNN7EXAMPLE"));
        let metadata = result.tool_result_fields.unwrap();
        assert!(
            !serde_json::to_string(&metadata)
                .unwrap()
                .contains("AKIAIOSFODNN7EXAMPLE")
        );
        assert_eq!(metadata["workspace_mutation_applied"], true);
        let expected = b"AWS_ACCESS_KEY_ID=[configured-access-key]\n";
        assert_eq!(std::fs::read(path).unwrap(), expected);
        let journal = executor.file_journal.lock().unwrap();
        let entries: Vec<_> = journal.entries().collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].before_content.as_deref(), Some(raw.as_bytes()));
        assert_eq!(entries[0].after_content, expected);
        let observed = session.read().unwrap();
        assert_eq!(observed.fuzzy_match_events.len(), 1);
        assert_eq!(
            observed.fuzzy_match_events[0].outcome,
            astra_runtime::observability::FuzzyMatchOutcome::Matched
        );
    }

    #[test]
    fn str_replace_resolved_marker_equal_to_new_text_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("settings.txt");
        let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n";
        std::fs::write(&file_path, raw).unwrap();
        let (redacted, count) =
            astra_text_utils::credential_redaction::redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        let marker = redacted
            .split_once('=')
            .and_then(|(_, value)| value.lines().next())
            .expect("marker should be present");

        let executor = test_executor_in(dir.path());
        let result = executor.str_replace(&serde_json::json!({
            "path": "settings.txt",
            "old_str": marker,
            "new_str": "AKIAIOSFODNN7EXAMPLE"
        }));
        assert!(
            !result.contains("Replaced successfully"),
            "resolved no-op must not report success: {result}"
        );
        assert!(result.contains("no change needed"), "{result}");
        assert_eq!(std::fs::read_to_string(file_path).unwrap(), raw);
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
    fn read_file_rejects_unknown_fields_before_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let executor = test_executor_in(dir.path());

        let result = executor.read_file(&serde_json::json!({
            "file": "numbered.txt",
            "start_line": 1,
            "end_line": 300
        }));

        assert!(result.contains("unknown field `file`"), "{result}");
        assert!(result.contains("Valid fields: path"), "{result}");
        assert!(result.contains("Use `path` for the file path"), "{result}");
        assert!(
            !result.contains("missing 'path'"),
            "unknown-field contract should fire before legacy missing-path text: {result}"
        );
    }

    #[test]
    fn read_file_argument_failures_preserve_recovery_evidence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("test.txt"),
            "Error: ordinary file content\n",
        )
        .unwrap();
        let executor = test_executor_in(dir.path());
        for args in [
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!({"path": ""}),
            serde_json::json!({"path": "missing.txt", "unknown": true}),
            serde_json::json!({"path": "missing.txt", "start_line": 0}),
            serde_json::json!({"path": "missing.txt", "start_line": -1}),
            serde_json::json!({"path": "missing.txt", "start_line": 1.5}),
            serde_json::json!({"path": "missing.txt", "end_line": "2"}),
            serde_json::json!({"path": "missing.txt", "start_line": 3, "end_line": 1}),
            serde_json::json!({"path": "missing.txt", "outline": 1}),
        ] {
            let result = executor.read_file_with_metadata(&args);
            assert!(result.is_error, "{args}: {result:?}");
            let metadata = result.metadata.as_ref().unwrap();
            assert_eq!(metadata["execution_started"], false);
            assert_eq!(metadata["disposition"], "rejected");
            assert_eq!(metadata["execution_fact"], "not_executed");
            assert_eq!(
                metadata["error_kind"],
                astra_core::ErrorKind::ToolInvalidArgs.as_str()
            );
            let evidence: astra_core::ToolFailureEvidence =
                serde_json::from_value(metadata["recovery_evidence"].clone()).unwrap();
            assert_eq!(
                evidence.cause,
                astra_core::ToolFailureCause::InvalidArguments
            );
            assert_eq!(
                evidence.recovery_actions,
                vec![astra_core::ToolRecoveryAction::CorrectArguments]
            );
        }
        let read = |args: &Value| executor.read_file_with_metadata(args);
        let corrected =
            read(&serde_json::json!({"path": "test.txt", "start_line": 1, "end_line": 1}));
        assert!(!corrected.is_error, "{corrected:?}");
        assert!(corrected.output.contains("Error: ordinary file content"));
        let missing = read(&serde_json::json!({"path": "missing.txt"}));
        assert!(missing.is_error);
        assert!(missing.metadata.as_ref().is_none_or(
            |metadata| metadata.get("disposition") != Some(&serde_json::json!("rejected"))
        ));
        assert!(
            missing
                .metadata
                .as_ref()
                .is_none_or(|metadata| metadata.get("error_kind")
                    != Some(&serde_json::json!(
                        astra_core::ErrorKind::ToolInvalidArgs.as_str()
                    )))
        );
    }

    #[test]
    fn read_file_rejects_invalid_optional_arg_types() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.txt"), "a\nb\nc").unwrap();
        let executor = test_executor_in(dir.path());

        let result = executor.read_file(&serde_json::json!({
            "path": "test.txt",
            "start_line": "1"
        }));
        assert!(result.contains("`start_line`"), "{result}");
        assert!(result.contains("positive integer"), "{result}");

        let result = executor.read_file(&serde_json::json!({
            "path": "test.txt",
            "outline": 1
        }));
        assert!(result.contains("`outline`"), "{result}");
        assert!(result.contains("boolean"), "{result}");

        let result = executor.read_file(&serde_json::json!({
            "path": "test.txt",
            "start_line": 3,
            "end_line": 1
        }));
        assert!(
            result.contains("must not exceed"),
            "reversed range must be rejected, got: {result}"
        );
    }

    #[test]
    fn read_file_rejects_bad_range_on_single_line_json_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let payload = serde_json::json!({
            "status": "completed",
            "results": [
                {"slot_index": 0, "result": {"summary": "first review"}},
                {"slot_index": 1, "result": {"summary": "second review"}}
            ]
        })
        .to_string();
        std::fs::write(dir.path().join("fanout-result.txt"), payload).unwrap();
        let executor = test_executor_in(dir.path());

        let result = executor.read_file(&serde_json::json!({
            "path": "fanout-result.txt",
            "start_line": 2782,
            "end_line": 300
        }));

        // Structural validation runs before small-file auto-expansion: a
        // reversed range is malformed even when the file has one line.
        assert!(
            result.contains("must not exceed"),
            "reversed range must be rejected before auto-expand, got: {result}"
        );
        assert!(
            !result.contains("Auto-expanded"),
            "a malformed range must not produce a misleading full-file read: {result}"
        );
    }

    #[test]
    fn read_file_ranged_preserves_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "ranged.txt", 200);

        let executor = test_executor_in(dir.path());
        let result = executor.read_file(&serde_json::json!({
            "path": "ranged.txt",
            "start_line": 3,
            "end_line": 4
        }));
        assert!(
            result.contains("3\tline 3:"),
            "should have line 3: {result}"
        );
        assert!(
            result.contains("4\tline 4:"),
            "should have line 4: {result}"
        );
        assert!(!result.contains("5\t"), "should not have line 5: {result}");
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
        assert_eq!(actual, "let x = \"world\";\n");
    }

    #[test]
    fn str_replace_preserves_curly_quotes_from_file_on_quote_normalized_match() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("curly-preserve.rs");
        std::fs::write(&file_path, "let x = \u{201C}hello\u{201D};").unwrap();

        let executor = test_executor_in(dir.path());
        executor.read_file(&serde_json::json!({"path": "curly-preserve.rs"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "curly-preserve.rs",
            "old_str": "let x = \"hello\";",
            "new_str": "let x = \"world\";"
        }));
        assert!(
            result.contains("curly quotes"),
            "should mention normalization: {result}"
        );
        let actual = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(actual, "let x = \u{201C}world\u{201D};\n");
    }

    // Issue #1: replace_all + mixed curly-quote forms → specific error (not generic hint)
    #[test]
    fn str_replace_replace_all_mixed_curly_quotes_gives_specific_error() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("mixed-curly.txt");
        // Two occurrences with different curly-quote forms
        std::fs::write(
            &file_path,
            "say \u{201C}a\u{201D} and \u{201C}a\u{201C} done",
        )
        .unwrap();

        let executor = test_executor_in(dir.path());
        executor.read_file(&serde_json::json!({"path": "mixed-curly.txt"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "mixed-curly.txt",
            "old_str": "\"a\"",
            "new_str": "\"b\"",
            "replace_all": true
        }));
        assert!(
            result.contains("STR_REPLACE FAILED"),
            "must include unified banner sentinel: {result}"
        );
        assert!(result.contains("WHAT:"), "must include WHAT line: {result}");
        assert!(
            result.contains("curly quote"),
            "error should mention curly quotes, got: {result}"
        );
    }

    #[test]
    fn str_replace_with_line_number_prefixed_old_str() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("numbered.rs");
        std::fs::write(&file_path, "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

        let executor = test_executor_in(dir.path());
        executor.read_file(&serde_json::json!({"path": "numbered.rs"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "numbered.rs",
            "old_str": "1. fn main() {\n2.     println!(\"hello\");\n3. }",
            "new_str": "fn main() {\n    println!(\"world\");\n}"
        }));
        assert!(
            result.contains("matched via line-number-stripped"),
            "should use line-number stripping: {result}"
        );
        let actual = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(actual, "fn main() {\n    println!(\"world\");\n}\n");
    }

    #[test]
    fn str_replace_with_sequence_similarity_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("similar.rs");
        std::fs::write(&file_path, "let count = 1;\nprintln!(\"hello\");\n").unwrap();

        let executor = test_executor_in(dir.path());
        executor.read_file(&serde_json::json!({"path": "similar.rs"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "similar.rs",
            "old_str": "let count = 2;\nprintln!(\"hullo\");",
            "new_str": "let count = 3;\nprintln!(\"world\");"
        }));
        assert!(
            result.contains("matched via sequence-similarity"),
            "should use sequence similarity fallback: {result}"
        );
        let actual = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(actual, "let count = 3;\nprintln!(\"world\");\n");
    }

    // ── read_file large file truncation hint ─────────────────────────────────

    #[test]
    fn read_file_large_file_truncation_includes_hint() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let executor = test_executor_in(dir.path());
        let line_count = executor.scaled_output_limit().saturating_mul(3) / 30 + 1;
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..line_count {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let result = executor.read_file(&serde_json::json!({"path": "big.txt"}));

        assert!(result.contains("partial content"), "{result}");
        let captured = super::read_capped_to_string_lossy(
            &file_path,
            executor.scaled_output_limit().saturating_mul(2),
        )
        .unwrap();
        assert!(
            result.contains(&format!("{}-line sample", captured.lines().count())),
            "{result}"
        );
        assert!(!result.contains(&format!("{line_count} lines")), "{result}");

        // Pre-read size gate: large files without a range now auto-degrade
        // to an outline + guidance, not a hard refusal.
        assert!(
            result.contains("File is large") || result.contains("outline"),
            "should auto-generate outline for large file: last 200 chars: {}",
            &result[result.len().saturating_sub(200)..]
        );
        assert!(
            result.contains("start_line") || result.contains("read_file"),
            "should suggest how to drill into specific ranges: last 200 chars: {}",
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
        // Full read should auto-degrade to outline (not a hard rejection)
        let full = executor.read_file(&serde_json::json!({"path": "big.txt"}));
        assert!(
            full.contains("File is large"),
            "full read should auto-degrade: {}",
            &full[..200.min(full.len())]
        );

        // Ranged read should succeed (bypass the size gate entirely)
        let ranged = executor
            .read_file(&serde_json::json!({"path": "big.txt", "start_line": 1, "end_line": 10}));
        assert!(
            !ranged.contains("File is large"),
            "ranged read must not trigger size gate"
        );
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
        // First ranged read — should NOT auto-expand (file too large >16KB)
        let r1 = executor
            .read_file(&serde_json::json!({"path": "big.txt", "start_line": 1, "end_line": 5}));
        assert!(r1.contains("line 0"), "first range should work");
        assert!(
            !r1.contains("Auto-expanded"),
            "large file should NOT auto-expand on first read"
        );

        // Second ranged read — should also NOT auto-expand (file too large)
        let r2 = executor
            .read_file(&serde_json::json!({"path": "big.txt", "start_line": 10, "end_line": 15}));
        assert!(
            !r2.contains("Auto-expanded"),
            "should NOT auto-expand large file: {}",
            &r2[..100.min(r2.len())]
        );
    }

    #[test]
    fn read_file_auto_expand_small_file_on_first_read() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("small.txt");
        // ~500 bytes — well under 16KB threshold
        std::fs::write(&file_path, "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();

        let executor = test_executor_in(dir.path());
        // First ranged read of small file should auto-expand to full
        let r1 = executor
            .read_file(&serde_json::json!({"path": "small.txt", "start_line": 2, "end_line": 3}));
        assert!(
            r1.contains("Auto-expanded"),
            "small file should auto-expand on first ranged read: {}",
            &r1[..100.min(r1.len())]
        );
        assert!(r1.contains("alpha"), "should contain all lines");
        assert!(r1.contains("epsilon"), "should contain all lines");

        // A later request is served from cached bytes but still returns
        // evidence to the caller.
        let r2 = executor
            .read_file(&serde_json::json!({"path": "small.txt", "start_line": 4, "end_line": 5}));
        assert!(
            r2.contains("delta") && r2.contains("epsilon"),
            "second read should return the requested content: {r2}"
        );
    }

    // ── Read observations preserve legitimate repeated and ranged reads ─────

    #[test]
    fn read_file_disjoint_pagination_does_not_claim_redundant_reads() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..3000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        // Three disjoint pages are useful pagination, even when they exceed
        // the old count-based warning threshold.
        for start in [1, 20] {
            let out = executor.read_file(&serde_json::json!({
                "path": "big.txt",
                "start_line": start,
                "end_line": start + 5
            }));
            assert!(
                !out.contains("overlaps lines already returned"),
                "disjoint pages must not be called redundant"
            );
        }
        let third = executor.read_file(&serde_json::json!({
            "path": "big.txt",
            "start_line": 40,
            "end_line": 45
        }));
        assert!(
            !third.contains("overlaps lines already returned"),
            "third disjoint page must not warn, got: {}",
            &third[third.len().saturating_sub(200)..]
        );
    }

    #[test]
    fn repeated_full_reads_preserve_content_without_count_based_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observations.txt");
        std::fs::write(&path, "first observation\n").unwrap();
        let executor = test_executor_in(dir.path());
        for index in 0..6 {
            let output = executor.read_file(&serde_json::json!({"path": "observations.txt"}));
            assert!(output.contains("first observation"));
            if index > 0 {
                assert!(output.contains("Earlier output may no longer be in context"));
            }
        }
        std::fs::write(&path, "changed observation\n").unwrap();
        let changed = executor.read_file(&serde_json::json!({"path": "observations.txt"}));
        assert!(changed.contains("changed observation"));
        assert!(!changed.contains("overlaps lines already returned"));
    }

    #[test]
    fn read_file_warns_only_when_a_successfully_delivered_range_overlaps() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..3000 {
            writeln!(f, "line {i}: {}", "x".repeat(30)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());
        for (start, end) in [(1, 40), (41, 80), (75, 100)] {
            let output = executor.read_file(&serde_json::json!({
                "path": "big.txt",
                "start_line": start,
                "end_line": end
            }));
            if start == 75 {
                assert!(
                    output.contains("line 99:"),
                    "new range content must remain available"
                );
                assert!(output.contains("authorized rereads can still be necessary"));
                assert!(
                    output.contains("overlaps lines already returned"),
                    "an actual overlap should be called out, got: {output}"
                );
            } else {
                assert!(
                    !output.contains("overlaps lines already returned"),
                    "disjoint pages should not warn, got: {output}"
                );
            }
        }
    }

    #[test]
    fn read_file_tracks_full_and_auto_expanded_content_as_delivered_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let large_path = dir.path().join("large.txt");
        let mut large_file = std::fs::File::create(&large_path).unwrap();
        for line in 0..500 {
            writeln!(large_file, "line {line}: {}", "x".repeat(80)).unwrap();
        }
        drop(large_file);

        let small_path = dir.path().join("small.txt");
        let mut small_file = std::fs::File::create(&small_path).unwrap();
        for line in 0..100 {
            writeln!(small_file, "line {line}: {}", "y".repeat(20)).unwrap();
        }
        drop(small_file);

        let executor = test_executor_in(dir.path());
        let full_read = executor.read_file(&serde_json::json!({"path": "large.txt"}));
        assert!(full_read.contains("delivered through line"));
        let repeated_large_range = executor.read_file(&serde_json::json!({
            "path": "large.txt",
            "start_line": 1,
            "end_line": 40
        }));
        assert!(
            repeated_large_range.contains("overlaps lines already returned"),
            "a complete-line prefix delivered by a truncated full read counts as covered"
        );

        let first_small_range = executor.read_file(&serde_json::json!({
            "path": "small.txt",
            "start_line": 1,
            "end_line": 5
        }));
        assert!(first_small_range.contains("Auto-expanded to full file"));
        let small_state = executor.shared_file_state();
        let small_key = executor.file_state_key(&small_path);
        assert_eq!(
            small_state
                .lock()
                .unwrap()
                .get(&small_key)
                .unwrap()
                .delivered_line_ranges,
            vec![crate::edge_tools::file_state::DeliveredLineRange { start: 1, end: 100 }],
            "auto-expanded content must be retained as successfully delivered"
        );
    }

    #[test]
    fn full_reads_dont_increment_ranged_count() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("big.txt");
        // Must be >16KB to avoid auto-expand
        let mut f = std::fs::File::create(&file_path).unwrap();
        for i in 0..500 {
            writeln!(f, "line {i}: {}", "x".repeat(80)).unwrap();
        }
        drop(f);

        let executor = test_executor_in(dir.path());

        // Three disjoint ranged reads are not redundant.
        for start in [1, 20, 40] {
            executor.read_file(&serde_json::json!({
                "path": "big.txt", "start_line": start, "end_line": start + 5
            }));
        }
        let fourth_ranged = executor.read_file(&serde_json::json!({
            "path": "big.txt", "start_line": 60, "end_line": 65
        }));
        assert!(
            !fourth_ranged.contains("overlaps lines already returned"),
            "4th disjoint page must not trigger a redundant-read warning"
        );
        assert!(
            !fourth_ranged.contains("read 4+ times"),
            "ranged reads should not trigger full-read warning"
        );
    }

    #[test]
    fn disjoint_range_coverage_is_tracked_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        // Files must be >16KB to avoid auto-expand upgrading ranged reads to full reads
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
        // 3rd disjoint ranged read of file a — should not warn.
        let third_a = executor.read_file(&serde_json::json!({
            "path": "a.txt", "start_line": 40, "end_line": 45
        }));
        assert!(
            !third_a.contains("overlaps lines already returned"),
            "3rd disjoint ranged read of file a should not warn, got: {third_a}"
        );
        // A third disjoint page of file b remains useful pagination too.
        let third_b = executor.read_file(&serde_json::json!({
            "path": "b.txt", "start_line": 40, "end_line": 45
        }));
        assert!(
            !third_b.contains("overlaps lines already returned") && !third_b.contains("Use grep"),
            "3rd disjoint ranged read of file b should not trigger a grep nudge"
        );
    }

    // ── Repeated reads replay evidence from the content cache ───────────

    #[test]
    fn read_file_consecutive_identical_range_replays_content() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "dup.txt", 200);

        let executor = test_executor_in(dir.path());
        let args = serde_json::json!({
            "path": "dup.txt",
            "start_line": 1,
            "end_line": 5
        });
        let first = executor.read_file_with_metadata(&args);
        assert!(
            first.output.contains("line 1"),
            "first read should return content: {}",
            first.output
        );
        assert!(!first.is_error);
        assert!(first.metadata.is_none());

        let second = executor.read_file_with_metadata(&args);
        assert!(
            second.output.contains("line 1"),
            "second identical range should replay content: {}",
            second.output
        );
        assert!(!second.is_error);
        assert!(
            second.metadata.is_none(),
            "content-cache replay must not be classified as suppression"
        );
    }

    #[test]
    fn read_file_treats_error_prefixed_file_content_as_success() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("message.txt"),
            "Error: literal file content\n",
        )
        .unwrap();
        let executor = test_executor_in(dir.path());

        let result = executor.read_file_with_metadata(&serde_json::json!({"path": "message.txt"}));

        assert!(!result.is_error, "file content is not an execution error");
        assert!(result.output.contains("Error: literal file content"));
    }

    #[test]
    fn read_file_reports_actual_io_failure_structurally() {
        let dir = tempfile::tempdir().unwrap();
        let executor = test_executor_in(dir.path());

        let result = executor.read_file_with_metadata(&serde_json::json!({"path": "missing.txt"}));

        assert!(result.is_error);
        assert!(result.output.starts_with("Error:"));
    }

    #[test]
    fn read_file_preserves_typed_sandbox_rejection_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let executor = test_executor_in(dir.path());
        let external_path = "/var/astra-fs-leaf-sandbox-test-nonexistent";

        let result = executor.read_file_with_metadata(&serde_json::json!({"path": external_path}));

        assert!(result.is_error);
        assert!(result.output.starts_with("Error: "));
        assert!(
            !result
                .output
                .contains(crate::edge_tools::SANDBOX_DENIED_PREFIX)
        );
        let metadata = result.metadata.expect("sandbox denial metadata");
        assert_eq!(
            metadata.get("error_kind").and_then(Value::as_str),
            Some(crate::sandbox_retry::SANDBOX_DENIED_ERROR_KIND)
        );
        assert_eq!(
            metadata.get("disposition").and_then(Value::as_str),
            Some("rejected")
        );
        assert_eq!(
            metadata.get("execution_started").and_then(Value::as_bool),
            Some(false)
        );

        let string_error = executor
            .resolve_checked(external_path)
            .expect_err("legacy string resolver must reject the same path");
        assert!(string_error.starts_with(crate::edge_tools::SANDBOX_DENIED_PREFIX));
    }

    #[test]
    fn read_file_full_read_reemits_content_across_turns_when_mtime_unchanged() {
        // Disk content remains cached by mtime, but a new visible turn is a
        // new model boundary. The file body must be delivered again rather
        // than replaced by a claim that prior tool output is still visible.
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("full.txt");
        std::fs::write(&file_path, "MARK_A\nMARK_B\n").unwrap();

        let executor = test_executor_in(dir.path());
        executor
            .journal_turn_index
            .store(1, std::sync::atomic::Ordering::Release);

        let first = executor.read_file(&serde_json::json!({ "path": "full.txt" }));
        assert!(
            first.contains("MARK_A"),
            "first read should return content: {first}"
        );

        let second_same_turn = executor.read_file(&serde_json::json!({ "path": "full.txt" }));
        assert!(
            second_same_turn.contains("MARK_A"),
            "same-turn repeat must replay the file body: {second_same_turn}"
        );

        executor
            .journal_turn_index
            .store(2, std::sync::atomic::Ordering::Release);

        let next_turn = executor.read_file(&serde_json::json!({ "path": "full.txt" }));
        assert!(
            next_turn.contains("MARK_A"),
            "later turn must receive the unchanged file body: {next_turn}"
        );
    }

    #[test]
    fn read_file_consecutive_identical_outline_replays_content() {
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
        let second = executor.read_file(&args);
        assert!(
            second.contains("Outline") || second.contains("fn "),
            "second outline should replay content: {second}"
        );
    }

    #[test]
    fn read_file_nonconsecutive_same_range_replays_content() {
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
            r_a_again.contains("MARK_A"),
            "range 1-2 should replay its content: {r_a_again}"
        );
    }

    #[test]
    fn read_file_replays_covered_ranges_across_model_turns() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "turns.txt", 200);

        let executor = test_executor_in(dir.path());
        executor
            .journal_turn_index
            .store(1, std::sync::atomic::Ordering::Release);
        let args = serde_json::json!({
            "path": "turns.txt",
            "start_line": 1,
            "end_line": 5
        });

        let first = executor.read_file(&args);
        assert!(
            first.contains("line 1"),
            "first turn should read content: {first}"
        );

        let second_same_turn = executor.read_file(&args);
        assert!(
            second_same_turn.contains("line 1"),
            "same-turn repeat should replay content: {second_same_turn}"
        );

        executor
            .journal_turn_index
            .store(2, std::sync::atomic::Ordering::Release);
        let next_turn = executor.read_file(&serde_json::json!({
            "path": "turns.txt",
            "start_line": 2,
            "end_line": 4
        }));
        assert!(
            next_turn.contains("line 2"),
            "a range covered only in a prior turn must be delivered again: {next_turn}"
        );
    }

    #[test]
    fn read_file_partially_overlapping_range_returns_requested_content() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "partial.txt", 200);

        let executor = test_executor_in(dir.path());
        let _ = executor.read_file(&serde_json::json!({
            "path": "partial.txt",
            "start_line": 1,
            "end_line": 5
        }));
        let r2 = executor.read_file(&serde_json::json!({
            "path": "partial.txt",
            "start_line": 3,
            "end_line": 10
        }));
        assert!(
            r2.contains("line 6"),
            "partially overlapping range should return content: {r2}"
        );
    }

    #[test]
    fn truncated_full_read_allows_targeted_tail_and_prefix_reads() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "budgeted.txt", 260);

        let executor = test_executor_in(dir.path());
        let first = executor.read_file(&serde_json::json!({ "path": "budgeted.txt" }));
        assert!(
            first.contains("truncated"),
            "full read should truncate: {first}"
        );
        assert!(
            first.chars().count() <= READ_FILE_MODEL_RESULT_CHARS,
            "read_file output must fit model read budget"
        );

        let later = executor.read_file(&serde_json::json!({
            "path": "budgeted.txt",
            "start_line": 200,
            "end_line": 205
        }));
        assert!(
            later.contains("line 200"),
            "tail range must remain readable: {later}"
        );

        let covered = executor.read_file(&serde_json::json!({
            "path": "budgeted.txt",
            "start_line": 1,
            "end_line": 5
        }));
        assert!(
            covered.contains("line 1"),
            "a previously delivered prefix must still return evidence: {covered}"
        );
    }

    #[test]
    fn truncated_ranged_read_allows_a_later_targeted_tail_read() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "range-budgeted.txt", 260);

        let executor = test_executor_in(dir.path());
        let first = executor.read_file(&serde_json::json!({
            "path": "range-budgeted.txt",
            "start_line": 1,
            "end_line": 220
        }));
        assert!(
            first.contains("truncated"),
            "range should truncate: {first}"
        );
        assert!(
            first.chars().count() <= READ_FILE_MODEL_RESULT_CHARS,
            "ranged read_file output must fit model read budget"
        );

        let later = executor.read_file(&serde_json::json!({
            "path": "range-budgeted.txt",
            "start_line": 200,
            "end_line": 205
        }));
        assert!(
            later.contains("line 200"),
            "later targeted range must return content: {later}"
        );
    }

    #[test]
    fn truncated_full_read_does_not_satisfy_overwrite_guard() {
        let dir = tempfile::tempdir().unwrap();
        write_large_file(dir.path(), "overwrite.txt", 260);

        let executor = test_executor_in(dir.path());
        let read = executor.read_file(&serde_json::json!({ "path": "overwrite.txt" }));
        assert!(
            read.contains("truncated"),
            "setup should be a partial delivery: {read}"
        );

        let write = executor.write_file(&serde_json::json!({
            "path": "overwrite.txt",
            "content": "replacement\n"
        }));
        assert!(
            write.contains("only partially read"),
            "truncated full read must not permit overwrite: {write}"
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
    fn text_utility_functions() {
        // truncate_str within limit returns unchanged
        assert_eq!(truncate_str("short", 10), "short");
        // truncate_str over limit truncates with ellipsis
        assert_eq!(truncate_str("this is a long string", 7), "this is…");

        // similarity_score: exact match = 100
        assert_eq!(similarity_score("test.rs", "test.rs"), 100);
        // same extension scores higher than different extension
        let with_ext = similarity_score("config.rs", "setting.rs");
        let without_ext = similarity_score("config.rs", "setting.py");
        assert!(with_ext > without_ext, "same ext should score higher");
    }

    // ── file_outline: strips trailing braces ─────────────────────────────────

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
    fn similarity_and_utility_functions() {
        // similarity_score: exact match = 100
        assert_eq!(similarity_score("test.rs", "test.rs"), 100);
        // same extension scores higher than different extension
        let with_ext = similarity_score("config.rs", "setting.rs");
        let without_ext = similarity_score("config.rs", "setting.py");
        assert!(with_ext > without_ext, "same ext should score higher");
    }

    #[test]
    fn file_edits_preserve_candidates_in_formatter_configured_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"edit-fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let path = dir.path().join("main.rs");
        let exe = test_executor_in(dir.path());
        let created = "fn   main( ){let   value=1;}\n";
        let (write, applied, already_desired) = exe.write_file_with_applied(&json!({
            "path": "main.rs",
            "content": "fn   main( ){let   value=1;}\r\n"
        }));
        let write: Value = serde_json::from_str(&write).unwrap();
        assert_eq!(write["success"], true);
        assert!(applied);
        assert!(!already_desired);
        // Deterministic LF normalization stays; project-configured formatters
        // must not rewrite whitespace outside the requested candidate.
        assert_eq!(std::fs::read(&path).unwrap(), created.as_bytes());
        assert!(
            write["_cli_unified_diff"]
                .as_str()
                .unwrap()
                .contains(created.trim_end())
        );

        let (replace, applied) = exe.str_replace_with_applied(&json!({
            "path": "main.rs", "old_str": "value=1", "new_str": "value=2"
        }));
        let replaced = "fn   main( ){let   value=2;}\n";
        assert!(!replace.is_error, "{}", replace.output);
        assert!(applied);
        assert_eq!(std::fs::read(&path).unwrap(), replaced.as_bytes());
        assert!(replace.output.contains(replaced.trim_end()));

        let (batch, applied) = exe.multi_edit_with_applied(&json!({
            "path": "main.rs",
            "edits": [
                {"old_str": "value=2", "new_str": "value=3"},
                {"old_str": "main", "new_str": "entry"}
            ]
        }));
        let batched = "fn   entry( ){let   value=3;}\n";
        assert!(!batch.is_error, "{}", batch.output);
        assert!(applied);
        assert_eq!(std::fs::read(&path).unwrap(), batched.as_bytes());
        assert!(batch.output.contains(batched.trim_end()));

        let journal = exe.file_journal.lock().unwrap();
        let entries: Vec<_> = journal.entries().collect();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].before_content, None);
        assert_eq!(entries[0].after_content, created.as_bytes());
        assert_eq!(
            entries[1].before_content.as_deref(),
            Some(created.as_bytes())
        );
        assert_eq!(entries[1].after_content, replaced.as_bytes());
        assert_eq!(
            entries[2].before_content.as_deref(),
            Some(replaced.as_bytes())
        );
        assert_eq!(entries[2].after_content, batched.as_bytes());
        journal.undo_turn_transactional(0).unwrap();
        assert!(!path.exists());
        journal.restore_turn_transactional(0).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), batched.as_bytes());
    }

    #[test]
    fn cli_prepared_publication_respects_explicit_external_authorization() {
        let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let project = base.path().join("project");
        let outside = base.path().join("outside");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let path = outside.join("edit.txt");
        let exe = test_executor_in(&project);
        exe.sandbox_policy
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .allowed_paths
            .clear();
        let args = json!({"path": path, "content": "alpha\r\n"});
        let (denied, applied, converged) = exe.write_file_with_applied(&args);
        assert_eq!(
            serde_json::from_str::<Value>(&denied).unwrap()["success"],
            false
        );
        assert!(!applied && !converged);
        assert!(!path.exists());
        exe.expand_sandbox_path(outside).unwrap();
        let (write, applied, converged) = exe.write_file_with_applied(&args);
        assert_eq!(
            serde_json::from_str::<Value>(&write).unwrap()["success"],
            true
        );
        assert!(applied && !converged);
        let (replace, applied) = exe.str_replace_with_applied(&json!({
            "path": path, "old_str": "alpha", "new_str": "beta"
        }));
        assert!(!replace.is_error && applied, "{}", replace.output);
        let (batch, applied) = exe.multi_edit_with_applied(&json!({
            "path": path, "edits": [{"old_str": "beta", "new_str": "gamma"}]
        }));
        assert!(!batch.is_error && applied, "{}", batch.output);
        assert_eq!(std::fs::read(path).unwrap(), b"gamma\n");
    }

    #[tokio::test]
    async fn cli_multi_path_batch_uses_explicit_authority_and_rejects_group_denials() {
        let base = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let project = base.path().join("project");
        let first_dir = base.path().join("first");
        let second_dir = base.path().join("second");
        for directory in [&project, &first_dir, &second_dir] {
            std::fs::create_dir(directory).unwrap();
        }
        let first = first_dir.join("a.txt");
        let second = second_dir.join("b.txt");
        for path in [&first, &second] {
            std::fs::write(path, "before\n").unwrap();
        }
        let executor = test_executor_in(&project);
        executor
            .sandbox_policy
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .allowed_paths
            .clear();
        let arguments = json!({"edits":[
            {"path":first,"old_str":"before","new_str":"after"},
            {"path":second,"old_str":"before","new_str":"after"}
        ]});
        executor.expand_sandbox_path(first_dir.clone()).unwrap();
        assert!(
            astra_tools::fs_ops::resolve_path(&project, first.to_str().unwrap()).is_err(),
            "fixture must be outside the shared default allowed roots"
        );
        let read = executor
            .execute_with_metadata("read_file", &json!({"path":first}))
            .await;
        assert!(!read.is_error, "{}", read.output);
        let denied = executor
            .execute_with_metadata("str_replace", &arguments)
            .await;
        assert!(denied.is_error, "{}", denied.output);
        assert_eq!(
            denied.tool_result_fields.as_ref().unwrap()["error_kind"],
            "sandbox_denied"
        );
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "before\n");
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "before\n");
        assert_eq!(executor.file_journal.lock().unwrap().entries().count(), 0);
        executor.expand_sandbox_path(second_dir.clone()).unwrap();
        for path in [&first, &second] {
            let read = executor
                .execute_with_metadata("read_file", &json!({"path":path}))
                .await;
            assert!(!read.is_error, "{}", read.output);
        }
        let applied = executor
            .execute_with_metadata("str_replace", &arguments)
            .await;
        assert!(!applied.is_error, "{}", applied.output);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "after\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "after\n");
        assert_eq!(executor.file_journal.lock().unwrap().entries().count(), 2);
        executor
            .sandbox_policy
            .write()
            .unwrap()
            .as_mut()
            .unwrap()
            .allowed_paths
            .clear();
        let denied = executor
            .execute_with_metadata(
                "str_replace",
                &json!({"edits":[
                    {"path":first,"old_str":"after","new_str":"changed"},
                    {"path":second,"old_str":"after","new_str":"changed"}
                ]}),
            )
            .await;
        assert!(denied.is_error, "{}", denied.output);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "after\n");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "after\n");
        assert_eq!(executor.file_journal.lock().unwrap().entries().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn cli_multi_path_rechecks_alias_after_all_preimages_are_captured() {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::{OpenOptionsExt, symlink},
        };
        use std::sync::{Arc, mpsc};
        use std::time::{Duration, Instant};
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.txt");
        let other = directory.path().join("other.txt");
        let second = directory.path().join("second.txt");
        let alias = directory.path().join("alias.txt");
        for path in [&first, &other, &second] {
            std::fs::write(path, "before\n").unwrap();
        }
        symlink(&first, &alias).unwrap();
        let executor = Arc::new(test_executor_in(directory.path()));
        for path in [&first, &other, &second] {
            assert!(executor.read_file(&json!({"path":path})).contains("before"));
        }
        std::fs::remove_file(&second).unwrap();
        let fifo_path = std::ffi::CString::new(second.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let worker = executor.clone();
        let (sender, receiver) = mpsc::channel();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let outcome = runtime.block_on(worker.execute_with_metadata(
                "str_replace",
                &json!({"edits":[
                    {"path":"alias.txt","old_str":"before","new_str":"after"},
                    {"path":"second.txt","old_str":"before","new_str":"after"}
                ]}),
            ));
            sender.send(outcome).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut writer = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&second)
            {
                Ok(writer) => break writer,
                Err(error)
                    if error.raw_os_error() == Some(libc::ENXIO) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("batch did not reach the second preimage: {error}"),
            }
        };
        // The second reader is now open, proving the first candidate exists.
        // Restore a regular file before later staleness/prehash reads, so the
        // test cannot pass by blocking on an unrelated second FIFO open.
        std::fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        std::fs::remove_file(&second).unwrap();
        std::fs::write(&second, "before\n").unwrap();
        assert!(
            executor
                .read_file(&json!({"path":second}))
                .contains("before")
        );
        writer.write_all(b"before\n").unwrap();
        drop(writer);
        let outcome = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
        assert!(outcome.is_error, "{}", outcome.output);
        assert!(
            outcome.output.contains("binding changed"),
            "{}",
            outcome.output
        );
        for path in [&first, &other, &second] {
            assert_eq!(std::fs::read_to_string(path).unwrap(), "before\n");
        }
        assert_eq!(executor.file_journal.lock().unwrap().entries().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_multi_path_batch_rejects_duplicate_bound_aliases_without_journal() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.txt");
        std::fs::write(&target, "before\n").unwrap();
        for name in ["first.txt", "second.txt"] {
            symlink(&target, directory.path().join(name)).unwrap();
        }
        let executor = test_executor_in(directory.path());
        executor
            .execute_with_metadata("read_file", &json!({"path":"first.txt"}))
            .await;
        let result = executor
            .execute_with_metadata(
                "str_replace",
                &json!({"edits":[
                    {"path":"first.txt","old_str":"before","new_str":"first"},
                    {"path":"second.txt","old_str":"before","new_str":"second"}
                ]}),
            )
            .await;
        assert!(result.is_error, "{}", result.output);
        assert_eq!(
            result.tool_result_fields.as_ref().unwrap()["workspace_mutation_applied"],
            false
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "before\n");
        assert_eq!(executor.file_journal.lock().unwrap().entries().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cli_batch_symlink_publishes_shared_candidate_and_journals_exact_bytes() {
        use std::os::unix::fs::symlink;
        for (alias_name, target_name, expected) in [
            ("alias.txt", "target.data", "ALPHA\r\nbeta"),
            ("alias.data", "target.txt", "ALPHA\nbeta\n"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let target = directory.path().join(target_name);
            let alias = directory.path().join(alias_name);
            let original = b"alpha\r\nbeta";
            std::fs::write(&target, original).unwrap();
            symlink(&target, &alias).unwrap();
            let executor = test_executor_in(directory.path());
            executor
                .execute_with_metadata("read_file", &json!({"path":alias_name}))
                .await;
            let result = executor
                .execute_with_metadata(
                    "str_replace",
                    &json!({
                        "path":alias_name, "edits":[{"old_str":"alpha", "new_str":"ALPHA"}]
                    }),
                )
                .await;
            assert!(!result.is_error, "{}", result.output);
            assert_eq!(
                result.tool_result_fields.as_ref().unwrap()["workspace_mutation_applied"],
                true
            );
            assert_eq!(std::fs::read(&target).unwrap(), expected.as_bytes());
            assert_eq!(std::fs::read_link(&alias).unwrap(), target);
            let bound = executor.bind_file_mutation_target(&alias).unwrap();
            let journal = executor.file_journal.lock().unwrap();
            let entries: Vec<_> = journal.entries().collect();
            assert_eq!(entries.len(), 1);
            assert_eq!(
                entries[0].before_content.as_deref(),
                Some(original.as_slice())
            );
            assert_eq!(entries[0].after_content, expected.as_bytes());
            journal.undo_file(&bound).unwrap();
            assert_eq!(std::fs::read(&target).unwrap(), original);
            assert_eq!(std::fs::read_link(&alias).unwrap(), target);
        }
    }

    #[cfg(unix)]
    #[test]
    fn cli_prepared_publication_updates_symlink_referent_without_replacing_alias() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.data");
        let alias = dir.path().join("edit.rs");
        std::fs::write(&target, b"old\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&target, &alias).unwrap();
        let exe = test_executor_in(dir.path());
        *exe.sandbox_policy.write().unwrap() = Some(
            astra_runtime::tool_sandbox::SandboxPolicy::permissive(dir.path()),
        );
        exe.read_file(&json!({"path": "edit.rs"}));
        let (write, applied, converged) = exe.write_file_with_applied(&json!({
            "path": "edit.rs", "content": "alpha\r\n"
        }));
        assert_eq!(
            serde_json::from_str::<Value>(&write).unwrap()["success"],
            true
        );
        assert!(applied && !converged);
        assert_eq!(std::fs::read(&target).unwrap(), b"alpha\n");
        let (replace, applied) = exe.str_replace_with_applied(&json!({
            "path": "edit.rs", "old_str": "alpha", "new_str": "beta"
        }));
        assert!(!replace.is_error && applied, "{}", replace.output);
        let (batch, applied) = exe.multi_edit_with_applied(&json!({
            "path": "edit.rs", "edits": [{"old_str": "beta", "new_str": "gamma"}]
        }));
        assert!(!batch.is_error && applied, "{}", batch.output);
        assert_eq!(std::fs::read(&target).unwrap(), b"gamma\n");
        assert_eq!(std::fs::read_link(&alias).unwrap(), target);
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        let bound = exe.bind_file_mutation_target(&alias).unwrap();
        exe.file_journal.lock().unwrap().undo_file(&bound).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"beta\n");
        assert_eq!(std::fs::read_link(&alias).unwrap(), target);
        let other = dir.path().join("other.data");
        std::fs::write(&other, b"untouched").unwrap();
        std::fs::remove_file(&alias).unwrap();
        symlink(&other, &alias).unwrap();
        assert!(exe.verify_file_mutation_binding(&alias, &bound).is_err());
        assert_eq!(std::fs::read(other).unwrap(), b"untouched");
    }

    #[test]
    fn cli_text_edits_reject_invalid_utf8_without_changing_bytes_or_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edit.txt");
        let raw = b"alpha \xff\n";
        std::fs::write(&path, raw).unwrap();
        let exe = test_executor_in(dir.path());
        exe.read_file(&json!({"path": "edit.txt"}));
        let checkpoint = exe.file_journal_checkpoint();
        for dry_run in [false, true] {
            let results = [
                exe.str_replace_with_applied(&json!({
                    "path": "edit.txt", "old_str": "alpha", "new_str": "beta",
                    "dry_run": dry_run
                })),
                exe.multi_edit_with_applied(&json!({
                    "path": "edit.txt", "edits": [{"old_str": "alpha", "new_str": "gamma"}],
                    "dry_run": dry_run
                })),
            ];
            for (result, applied) in results {
                assert!(result.is_error && !applied, "{}", result.output);
                assert!(result.output.contains("not valid UTF-8"));
            }
            assert_eq!(std::fs::read(&path).unwrap(), raw);
            assert_eq!(exe.file_journal_checkpoint(), checkpoint);
        }
    }

    #[test]
    fn cli_prepared_write_does_not_treat_read_error_as_missing_preimage() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        let checkpoint = exe.file_journal_checkpoint();
        let (result, applied, converged) = exe.write_file_with_applied(&json!({
            "path": ".", "content": "replacement"
        }));
        assert_eq!(
            serde_json::from_str::<Value>(&result).unwrap()["success"],
            false
        );
        assert!(!applied && !converged);
        assert_eq!(exe.file_journal_checkpoint(), checkpoint);
        assert!(dir.path().is_dir());
    }

    #[test]
    fn rejected_prepared_publication_cannot_create_an_undo_of_external_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        for (name, before) in [
            ("existing.txt", Some(b"original".to_vec())),
            ("new.txt", None),
        ] {
            let path = dir.path().join(name);
            if let Some(bytes) = &before {
                std::fs::write(&path, bytes).unwrap();
            }
            let prepared = astra_tools::fs_ops::PreparedWriteFile::from_authorized_preimage(
                path.clone(),
                name,
                "candidate",
                before,
            );
            let checkpoint = exe.file_journal_checkpoint();
            std::fs::write(&path, b"external owner").unwrap();
            let result = exe.apply_prepared_file_edit(
                &prepared,
                "rejected-edit",
                astra_turn_core::file_edit_journal::EditType::Overwrite,
            );
            assert!(result.is_error, "{}", result.output);
            assert_eq!(exe.file_journal_checkpoint(), checkpoint);
            exe.file_journal
                .lock()
                .unwrap()
                .undo_turn_since(0, checkpoint);
            assert_eq!(std::fs::read(&path).unwrap(), b"external owner");
        }
    }

    #[test]
    fn file_edit_preview_and_noop_preserve_formatter_configured_preimage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"edit-fixture\"\n",
        )
        .unwrap();
        let path = dir.path().join("main.rs");
        let source = "fn   main( ){let   value=1;}\n";
        std::fs::write(&path, source).unwrap();
        let exe = test_executor_in(dir.path());
        let checkpoint = exe.file_journal_checkpoint();

        let (write, applied, already_desired) = exe.write_file_with_applied(&json!({
            "path": "main.rs", "content": source
        }));
        let write: Value = serde_json::from_str(&write).unwrap();
        assert_eq!(write["state"], "already_desired");
        assert!(!applied);
        assert!(already_desired);
        exe.read_file(&json!({"path": "main.rs"}));
        let (preview, applied) = exe.str_replace_with_applied(&json!({
            "path": "main.rs", "old_str": "value=1", "new_str": "value=2", "dry_run": true
        }));
        assert!(!preview.is_error, "{}", preview.output);
        assert!(!applied);
        assert!(preview.output.contains("[DRY RUN]"));
        assert!(preview.output.contains("fn   main( ){let   value=2;}"));
        let (preview, applied) = exe.multi_edit_with_applied(&json!({
            "path": "main.rs", "dry_run": true,
            "edits": [{"old_str": "value=1", "new_str": "value=2"}]
        }));
        assert!(!preview.is_error, "{}", preview.output);
        assert!(!applied);
        assert!(preview.output.contains("[DRY RUN]"));
        let (noop, applied) = exe.str_replace_with_applied(&json!({
            "path": "main.rs", "old_str": "value=1", "new_str": "value=1"
        }));
        assert!(noop.is_error);
        assert!(!applied);
        let (normalized_noop, applied) = exe.str_replace_with_applied(&json!({
            "path": "main.rs", "old_str": "\n", "new_str": "\r\n"
        }));
        assert!(normalized_noop.is_error);
        assert!(!applied);
        let (cancelled_batch, applied) = exe.multi_edit_with_applied(&json!({
            "path": "main.rs",
            "edits": [
                {"old_str": "value=1", "new_str": "value=2"},
                {"old_str": "value=2", "new_str": "value=1"}
            ]
        }));
        assert!(cancelled_batch.is_error);
        assert!(!applied);
        assert_eq!(std::fs::read(path).unwrap(), source.as_bytes());
        assert_eq!(exe.file_journal_checkpoint(), checkpoint);
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
    fn str_replace_dry_run_is_success_without_applied_fact() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::write(tmpdir.path().join("test.txt"), "before\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path());

        let (result, applied) = exe.str_replace_with_applied(&json!({
            "path": "test.txt",
            "old_str": "before",
            "new_str": "after",
            "dry_run": true
        }));

        assert!(!result.is_error);
        assert!(!applied);
    }

    #[test]
    fn str_replace_noop_is_error_without_applied_fact() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::write(tmpdir.path().join("test.txt"), "same\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path());

        let (result, applied) = exe.str_replace_with_applied(&json!({
            "path": "test.txt",
            "old_str": "same",
            "new_str": "same"
        }));

        assert!(result.is_error);
        assert!(!applied);
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
    fn delete_file_rollback_restores_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("victim.txt");
        std::fs::write(&file, "restore me").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.journal_turn_index
            .store(7, std::sync::atomic::Ordering::Relaxed);

        let result = exe.delete_file(&json!({"path": "victim.txt"}));
        assert!(result.starts_with("Deleted:"), "result: {result}");
        assert!(!file.exists());

        let rollback = exe.rollback_file_edits(&json!({
            "scope": "file",
            "path": "victim.txt"
        }));
        let parsed: Value = serde_json::from_str(&rollback).unwrap();
        assert_eq!(parsed["success"], true, "rollback: {rollback}");
        assert_eq!(parsed["edit_type"], "delete", "rollback: {rollback}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "restore me");
    }

    #[test]
    fn delete_file_rejects_invalid_targets() {
        let tmpdir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());

        // Missing file
        assert!(
            exe.delete_file(&json!({"path": "nope.txt"}))
                .contains("not found")
        );

        // Directory
        std::fs::create_dir(tmpdir.path().join("subdir")).unwrap();
        assert!(
            exe.delete_file(&json!({"path": "subdir"}))
                .contains("refusing to delete a directory")
        );

        // Path inside .git
        let git_dir = tmpdir.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(
            exe.delete_file(&json!({"path": ".git/HEAD"}))
                .contains("refusing to delete .git")
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
    fn same_file_batch_uses_typed_error_status() {
        let tmpdir = tempfile::tempdir().unwrap();
        std::fs::write(tmpdir.path().join("code.rs"), "fn present() {}\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path());
        exe.read_file(&json!({"path": "code.rs"}));

        let result = exe.str_replace_batch_result(&json!({
            "path": "code.rs",
            "edits": [{"old_str": "missing", "new_str": "replacement"}]
        }));

        assert!(result.is_error);
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("recovery_evidence"))
                .and_then(|evidence| evidence.get("cause"))
                .and_then(Value::as_str),
            Some("invalid_arguments"),
            "same-file batch validation failures must carry typed no-effect evidence"
        );
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(Value::as_bool),
            Some(false)
        );
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
    fn str_replace_batch_accepts_per_edit_paths_for_multi_file_batch() {
        let tmpdir = tempfile::tempdir().unwrap();
        let a = tmpdir.path().join("a.txt");
        let b = tmpdir.path().join("b.txt");
        std::fs::write(&a, "alpha beta").unwrap();
        std::fs::write(&b, "gamma delta").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        let args = json!({
            "edits": [
                {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                {"path": "b.txt", "old_str": "delta", "new_str": "DELTA"}
            ]
        });
        let checkpoint = exe.file_journal_checkpoint();
        let unread = exe.str_replace_batch_result(&args);
        assert!(unread.is_error && unread.output.contains("has not been read"));
        exe.read_file(&json!({"path": "a.txt"}));
        exe.read_file(&json!({"path": "b.txt"}));
        std::fs::write(&b, "external delta").unwrap();
        let stale = exe.str_replace_batch_result(&args);
        assert!(stale.is_error && stale.output.contains("modified since last read"));
        assert_eq!(std::fs::read(&a).unwrap(), b"alpha beta");
        assert_eq!(std::fs::read(&b).unwrap(), b"external delta");
        assert_eq!(exe.file_journal_checkpoint(), checkpoint);
        std::fs::write(&b, "gamma delta").unwrap();
        exe.read_file(&json!({"path": "b.txt"}));

        let result = exe.str_replace_batch_result(&args).output;

        assert!(
            result.contains("Successfully applied edits to 2 file(s)"),
            "result: {result}"
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ALPHA beta\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "gamma DELTA\n");
        assert_eq!(exe.get_cached_content(&a).as_deref(), Some("ALPHA beta\n"));
        assert_eq!(exe.get_cached_content(&b).as_deref(), Some("gamma DELTA\n"));
        let journal = exe.file_journal.lock().unwrap();
        assert_eq!(journal.entries().count(), 2);
        journal.undo_turn_transactional(0).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"alpha beta");
        assert_eq!(std::fs::read(&b).unwrap(), b"gamma delta");
        journal.restore_turn_transactional(0).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), b"ALPHA beta\n");
        assert_eq!(std::fs::read(&b).unwrap(), b"gamma DELTA\n");
    }

    #[test]
    fn str_replace_batch_prevalidates_all_files_before_writing() {
        let tmpdir = tempfile::tempdir().unwrap();
        let a = tmpdir.path().join("a.txt");
        let b = tmpdir.path().join("b.txt");
        std::fs::write(&a, "alpha beta").unwrap();
        std::fs::write(&b, "gamma delta").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&json!({"path": "a.txt"}));
        exe.read_file(&json!({"path": "b.txt"}));

        let result = exe
            .str_replace_batch_result(&json!({
                "edits": [
                    {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                    {"path": "b.txt", "old_str": "missing", "new_str": "MISSING"}
                ]
            }))
            .output;

        assert!(
            result.contains("old_str not found"),
            "missing old_str should be surfaced: {result}"
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "alpha beta");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "gamma delta");
    }

    #[cfg(unix)]
    #[test]
    fn str_replace_batch_atomic_rename_handles_readonly_dest() {
        use std::os::unix::fs::PermissionsExt;

        let tmpdir = tempfile::tempdir().unwrap();
        let a = tmpdir.path().join("a.txt");
        let b = tmpdir.path().join("b.txt");
        std::fs::write(&a, "alpha beta").unwrap();
        std::fs::write(&b, "gamma delta").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&json!({"path": "a.txt"}));
        exe.read_file(&json!({"path": "b.txt"}));

        let mut readonly = std::fs::metadata(&b).unwrap().permissions();
        readonly.set_mode(0o444);
        std::fs::set_permissions(&b, readonly).unwrap();

        let _result = exe
            .str_replace_batch_result(&json!({
                "edits": [
                    {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                    {"path": "b.txt", "old_str": "gamma", "new_str": "GAMMA"}
                ]
            }))
            .output;

        let mut writable = std::fs::metadata(&b).unwrap().permissions();
        writable.set_mode(0o644);
        std::fs::set_permissions(&b, writable).unwrap();

        // staging + rename() replaces the directory entry atomically,
        // so read-only destination files do not block the operation.
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ALPHA beta\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "GAMMA delta\n");
    }

    #[test]
    fn str_replace_batch_requires_some_path_source() {
        let tmpdir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());

        let result = exe
            .str_replace_batch_result(&json!({
                "edits": [
                    {"old_str": "alpha", "new_str": "ALPHA"}
                ]
            }))
            .output;

        assert!(
            result.contains("top-level path") && result.contains("path inside every edit"),
            "result: {result}"
        );
    }

    #[test]
    fn multi_edit_auto_applies_whitespace_normalized_match() {
        // Regression (session 5933ebce turn 4 rounds 17+20): LLM
        // submitted old_str with 20-space indent but the actual
        // file had 24-space indent. Single-edit `str_replace`
        // already falls through to the fuzzy cascade for this case;
        // `multi_edit` used to die with
        // "Error: edit[0] old_str not found" even though the
        // cascade's whitespace-normalized strategy has a unique
        // match.
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        // Actual file: 24-space indent.
        std::fs::write(
            &file,
            "fn outer() {\n                        Some(body.clone()),\n}\n",
        )
        .unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        // LLM supplies 20-space indent — intent is clear, whitespace
        // is the only mismatch.
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {
                    "old_str": "                    Some(body.clone()),",
                    "new_str": "                    Some(&body),"
                }
            ]
        }));
        assert!(
            result.contains("Applied 1 edit(s)"),
            "whitespace-only mismatch should auto-apply: {result}"
        );
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(
            content.contains("Some(&body)"),
            "new_str must be in file: {content}"
        );
        assert!(
            !content.contains("Some(body.clone())"),
            "old_str must be gone: {content}"
        );
        // Indentation of the replaced line must match the actual
        // file's 24-space indent, not the LLM's 20-space version.
        assert!(
            content.contains("                        Some(&body),"),
            "replacement must preserve file's own indentation: {content}"
        );
    }

    #[test]
    fn multi_edit_reports_fuzzy_strategy_in_success_message() {
        // When multi_edit falls back to fuzzy matching, the result
        // should say so — users/LLM can then see that the old_str
        // didn't match exactly and inspect the diff, instead of
        // silently losing awareness.
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        std::fs::write(&file, "fn f() {\n    let  x = 1;\n}\n").unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {
                    // Only one space between `let` and `x`, file has two.
                    "old_str": "    let x = 1;",
                    "new_str": "    let x = 2;"
                }
            ]
        }));
        assert!(
            result.contains("Applied 1 edit(s)"),
            "whitespace-normalized fuzzy match should succeed: {result}"
        );
        assert!(
            result.to_ascii_lowercase().contains("fuzzy") || result.contains("whitespace"),
            "result must disclose that fuzzy matching was used: {result}"
        );
    }

    #[test]
    fn multi_edit_rejects_fuzzy_match_when_ambiguous() {
        // If the fuzzy cascade finds two+ candidates, multi_edit
        // must NOT auto-apply — that's unsafe. User has to give
        // more context.
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("code.rs");
        // Two locations with whitespace-equivalent "Some(body.clone())".
        std::fs::write(
            &file,
            "fn a() {\n    Some(body.clone())\n}\nfn b() {\n    Some(body.clone())\n}\n",
        )
        .unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "code.rs",
            "edits": [
                {
                    "old_str": "Some(body.clone())",
                    "new_str": "Some(&body)"
                }
            ]
        }));
        // With 2 exact matches, the original ambiguity path already
        // handles this — keep it failing safely.
        assert!(
            result.starts_with("Error") || result.contains("must be unique"),
            "ambiguous matches must not silently auto-apply: {result}"
        );
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
        assert_eq!(content, "AB gamma\n");
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
    fn multi_edit_fuzzy_match_scope_uses_actual_location() {
        let tmpdir = tempfile::tempdir().unwrap();
        let file = tmpdir.path().join("lib.rs");
        std::fs::write(
            &file,
            "fn top_level() {\n    let unrelated = 1;\n}\n\nfn outer() {\n    fn inner() {\n        let  value = compute();\n    }\n}\n",
        )
        .unwrap();
        let exe = ToolExecutor::new(tmpdir.path().to_path_buf());
        exe.read_file(&serde_json::json!({"path": "lib.rs"}));
        let result = exe.multi_edit(&json!({
            "path": "lib.rs",
            "edits": [
                {
                    "old_str": "        let value = compute();",
                    "new_str": "        let value = finish();"
                }
            ]
        }));

        assert!(result.contains("Applied 1 edit(s)"), "result: {result}");
        assert!(result.contains("📍"), "should show scope: {result}");
        assert!(
            result.contains("outer") && result.contains("inner"),
            "scope should use actual fuzzy match location, not line 1: {result}"
        );
        assert!(
            !result.contains("top_level"),
            "scope must not fall back to the beginning of the file: {result}"
        );
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
        assert_eq!(content, "qux bar qux baz qux\n");
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

    /// An exact str_replace anchor is itself a current-content precondition.
    #[test]
    fn str_replace_self_authorizes_exact_current_snapshot() {
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
            result.contains("Replaced"),
            "exact edit should succeed: {result}"
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "key = \"new_value\"\n"
        );
    }

    #[test]
    fn failed_str_replace_does_not_unlock_full_file_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "key = \"current\"\nother = true\n").unwrap();

        let replace = exe.str_replace(&json!({
            "path": "config.toml",
            "old_str": "key = \"stale\"",
            "new_str": "key = \"new\""
        }));
        assert!(replace.contains("old_str not found"), "{replace}");

        let overwrite = exe.write_file(&json!({
            "path": "config.toml",
            "content": "key = \"new\"\n"
        }));
        assert!(
            overwrite.contains("has not been read yet"),
            "a failed localized edit cannot authorize a full overwrite: {overwrite}"
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "key = \"current\"\nother = true\n"
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
        assert_eq!(on_disk, "updated content\n");
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
        assert_eq!(on_disk, "AAA BBB ccc\n");
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

    /// An external reformat invalidates the exact replacement anchor.
    #[test]
    fn str_replace_rejects_anchor_invalidated_by_external_format() {
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
            result.contains("old_str not found"),
            "should reject the invalidated anchor, got: {result}"
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), "fn main() {\n}\n");
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

    #[test]
    fn large_file_outline_does_not_loop_into_impossible_full_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());
        let path = dir.path().join("large.txt");
        let content = "line with enough bytes to exceed the model read budget\n".repeat(600);
        std::fs::write(&path, content).unwrap();

        let preview = exe.read_file(&json!({"path": "large.txt"}));
        assert!(preview.contains("File is large") || preview.contains("outline"));

        let result = exe.write_file(&json!({
            "path": "large.txt",
            "content": "replacement\n"
        }));
        assert!(
            result.contains("partially read"),
            "unexpected result: {result}"
        );
        assert!(
            result.contains("str_replace") && result.contains("multi_edit"),
            "large-file guidance must offer an actionable exact-edit path: {result}"
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

    /// A stale exact anchor must never overwrite externally changed content.
    #[test]
    fn str_replace_stale_anchor_preserves_external_change() {
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

        // str_replace reads the current bytes and rejects the now-stale anchor.
        let result = exe.str_replace(&json!({
            "path": "race.txt",
            "old_str": "original",
            "new_str": "agent version"
        }));
        assert!(
            result.contains("not found") || result.contains("No exact match"),
            "should reject the stale anchor, got: {result}"
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

        // Exact str_replace does not need a redundant prior read: its anchor
        // is checked against the full current file and snapshotted before the
        // guarded write.
        let r2 = exe.str_replace(&json!({
            "path": "target.rs",
            "old_str": "fn main() {}",
            "new_str": "fn main() { println!(\"hi\"); }"
        }));
        assert!(r2.contains("Replaced"), "str_replace should succeed: {r2}");
    }

    /// Verify that the partial-read error for write_file also contains
    /// actionable guidance.
    #[test]
    fn partial_read_error_contains_actionable_guidance() {
        let dir = tempfile::tempdir().unwrap();
        let exe = test_executor_in(dir.path());

        // File must be >16KB to avoid auto-expand promoting ranged read to full
        write_large_file(dir.path(), "partial_target.rs", 200);

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
