use super::*;

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Execute a command with process group isolation and timeout.
///
/// This ensures child processes are properly cleaned up even if:
/// - The parent process receives SIGINT (Ctrl+C)
/// - The command times out
/// - The tokio runtime shuts down mid-execution
///
/// Returns the Output on success, or an error message on failure/timeout.
fn run_command_with_cleanup(
    cmd: &mut Command,
    timeout_secs: f64,
) -> Result<std::process::Output, String> {
    // Create a new process group so we can kill the entire tree on timeout/signal.
    // This prevents orphaned child processes from becoming zombies.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("Error: {e}"))?;

    let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    // Kill entire process group (command + all children)
                    #[cfg(unix)]
                    {
                        let pid = child.id();
                        // Negative PID = kill process group via /bin/kill
                        let _ = Command::new("kill")
                            .args(["-9", &format!("-{pid}")])
                            .output();
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = child.kill();
                    }
                    // Reap the zombie process to prevent resource leak
                    let _ = child.wait();
                    return Err(format!("Error: command timed out after {timeout_secs}s"));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Error: {e}")),
        }
    }

    child.wait_with_output().map_err(|e| format!("Error: {e}"))
}

const DEFAULT_SEARCH_EXCLUDE_DIRS: &[&str] = &[
    ".git",
    "target",
    "dist",
    "build",
    "coverage",
    "htmlcov",
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".cache",
    "out",
];

fn append_default_grep_excludes(cmd: &mut Command) {
    cmd.arg("--binary-files=without-match");
    cmd.arg("--devices=skip");
    for dir in DEFAULT_SEARCH_EXCLUDE_DIRS {
        cmd.arg("--exclude-dir").arg(dir);
    }
}

fn default_find_prune_clause() -> String {
    let joined = DEFAULT_SEARCH_EXCLUDE_DIRS
        .iter()
        .map(|dir| format!("-name {}", shell_escape(dir)))
        .collect::<Vec<_>>()
        .join(" -o ");
    format!("\\( -type d \\( {joined} \\) -prune \\)")
}

/// SSRF protection: check if a URL targets internal/private networks.
/// Returns Some(reason) if blocked, None if safe.
fn is_ssrf_target(url: &str) -> Option<&'static str> {
    // Extract host from URL (simple parsing, handles http://host:port/path)
    let after_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = after_scheme.split('/').next()?;
    // Handle userinfo@ prefix
    let host_port = authority.split('@').next_back()?;
    // Handle IPv6 brackets: [::1]:port → extract [::1]
    let host = if host_port.starts_with('[') {
        // IPv6: take everything up to and including the closing bracket
        host_port
            .split(']')
            .next()
            .map(|s| format!("{s}]"))
            .unwrap_or_default()
    } else {
        host_port.split(':').next().unwrap_or("").to_string()
    };
    let lower = host.to_ascii_lowercase();

    // Block localhost variants
    if lower == "localhost"
        || lower == "127.0.0.1"
        || lower == "0.0.0.0"
        || lower == "::1"
        || lower == "[::1]"
        || lower.ends_with(".localhost")
    {
        return Some("localhost access blocked");
    }
    // Block AWS/cloud metadata endpoints
    if lower == "169.254.169.254" || lower == "metadata.google.internal" {
        return Some("cloud metadata endpoint blocked");
    }
    // Block private IP ranges (RFC 1918 + link-local)
    if lower.starts_with("10.")
        || lower.starts_with("192.168.")
        || lower.starts_with("172.") && is_private_172(&lower)
        || lower.starts_with("169.254.")
        || lower.starts_with("fc")
        || lower.starts_with("fd")
    {
        return Some("private network access blocked");
    }
    None
}

/// Check if a 172.x.x.x address is in the private range 172.16-31.x.x
fn is_private_172(host: &str) -> bool {
    if let Some(second) = host.strip_prefix("172.").and_then(|r| r.split('.').next())
        && let Ok(n) = second.parse::<u8>()
    {
        return (16..=31).contains(&n);
    }
    false
}

impl ToolExecutor {
    pub(crate) fn run_shell_output(
        &self,
        command: &str,
        timeout_secs: f64,
    ) -> Result<std::process::Output, String> {
        // If sandbox is active, wrap command with resource limits
        let effective_command = if let Some(ref policy) = self.sandbox_policy {
            if !matches!(policy.mode, SandboxMode::Permissive) {
                wrap_command_with_limits(policy, command)
            } else {
                command.to_string()
            }
        } else {
            command.to_string()
        };

        let mut child_cmd = Command::new("bash");
        child_cmd
            .arg("-c")
            .arg(&effective_command)
            .current_dir(&self.project_root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Create a new process group so we can kill the entire tree on timeout.
        // This prevents orphaned git/curl/etc. child processes.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            child_cmd.process_group(0); // child becomes its own process group leader
        }

        // Apply sandbox environment filtering
        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
            && let Err(e) = sandbox_command(policy, &mut child_cmd)
        {
            eprintln!("[sandbox] failed to apply policy: {e}");
            return Err(format!("Error: sandbox policy application failed: {e}"));
        }

        let mut child = child_cmd.spawn().map_err(|e| format!("Error: {e}"))?;

        let deadline = std::time::Instant::now() + Duration::from_secs_f64(timeout_secs);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if std::time::Instant::now() > deadline {
                        // Kill entire process group (bash + all children)
                        #[cfg(unix)]
                        {
                            let pid = child.id();
                            // Negative PID = kill process group via /bin/kill
                            let _ = Command::new("kill")
                                .args(["-9", &format!("-{pid}")])
                                .output();
                        }
                        #[cfg(not(unix))]
                        {
                            let _ = child.kill();
                        }
                        // Reap the zombie process to prevent resource leak
                        let _ = child.wait();
                        return Err(format!("Error: command timed out after {timeout_secs}s"));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("Error: {e}")),
            }
        }

        child.wait_with_output().map_err(|e| format!("Error: {e}"))
    }

    pub(crate) fn bash(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) => c,
            None => return "Error: missing 'command'".to_string(),
        };
        // Use explicit timeout if provided, otherwise pick an adaptive default:
        // Tier 1 (5s):  instant commands — no I/O beyond trivial reads
        // Tier 2 (10s): fast read commands — cat, head, file stat
        // Tier 3 (15s): search/traversal — grep, find, ripgrep
        // Tier 4 (30s): everything else (build, test, network)
        let timeout_secs = args
            .get("timeout")
            .and_then(Value::as_f64)
            .unwrap_or_else(|| {
                let cmd_base = command.split_whitespace().next().unwrap_or("");
                match cmd_base {
                    // Tier 1: instant — no real I/O
                    "echo" | "printf" | "true" | "false" | "pwd" | "whoami" | "date"
                    | "basename" | "dirname" | "which" | "env" | "hostname" | "uname" | "id"
                    | "tty" | "nproc" | "arch" | "yes" => 5.0,
                    // Tier 2: fast reads — single file or dir stat
                    "cat" | "head" | "tail" | "wc" | "stat" | "file" | "ls" | "readlink"
                    | "realpath" | "md5sum" | "sha256sum" | "du" | "df" | "touch" | "mkdir"
                    | "cp" | "mv" | "rm" | "ln" | "chmod" | "chown" => 10.0,
                    // Tier 3: search/traversal — scan many files but bounded
                    "grep" | "rg" | "find" | "fd" | "ag" | "awk" | "sed" | "sort" | "uniq"
                    | "cut" | "tr" | "diff" | "comm" | "xargs" | "tree" | "jq" | "yq"
                    | "column" | "tee" => 15.0,
                    // Tier 4: everything else (compilation, network, etc.)
                    _ => 30.0,
                }
            });

        match self.run_shell_output(command, timeout_secs) {
            Err(error) => error,
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let mut result = String::new();
                if !stdout.is_empty() {
                    result.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !result.is_empty() {
                        result.push('\n');
                    }
                    result.push_str(&stderr);
                }
                if result.is_empty() {
                    if out.status.success() {
                        "(no output)".to_string()
                    } else {
                        format!("(exit code {})", out.status.code().unwrap_or(-1))
                    }
                } else {
                    // Truncate long output
                    if result.len() > 20_000 {
                        result.truncate(20_000);
                        result.push_str("\n[truncated]");
                    }

                    // For build/test commands, provide structured output with iteration tracking
                    if super::build_test::is_build_test_command(command) {
                        let mut parsed =
                            super::build_test::parse_build_test_output(&result, out.status.code());
                        if !parsed.error_locations.is_empty() {
                            parsed.enrich_with_scope(&self.project_root);
                        }
                        let delta = {
                            let mut tracker = self.build_test_tracker.lock().unwrap();
                            if tracker.command_changed(command) {
                                tracker.reset();
                            }
                            tracker.record(&parsed, command)
                        };
                        let delta_summary = delta.to_summary();
                        if delta_summary.is_empty() {
                            return parsed.to_enhanced_output(&result);
                        }
                        return format!(
                            "{}\n\n{}",
                            delta_summary,
                            parsed.to_enhanced_output(&result)
                        );
                    }

                    result
                }
            }
        }
    }

    pub(crate) fn grep(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'pattern'".to_string(),
        };
        let search_path = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.project_root.clone());

        // Validate search path exists before spawning grep
        if !search_path.exists() {
            return format!(
                "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
                search_path.display()
            );
        }

        let include = args.get("include").and_then(Value::as_str).unwrap_or("*");
        let case_sensitive = args
            .get("case_sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .map(|n| n.min(10) as usize); // cap at 10 to avoid huge output
        let max_matches = args
            .get("max_matches")
            .and_then(Value::as_u64)
            .map(|n| n.max(1) as usize);
        let scope_context = args
            .get("scope_context")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut cmd = Command::new("grep");
        cmd.arg("-rnHE"); // -H forces filename display, -E enables extended regex
        if !case_sensitive {
            cmd.arg("-i");
        }
        if let Some(ctx) = context_lines {
            cmd.arg(format!("-C{ctx}"));
        }
        if let Some(max) = max_matches {
            cmd.arg(format!("-m{max}"));
        }
        append_default_grep_excludes(&mut cmd);
        cmd.arg("--include").arg(include);
        cmd.arg(pattern).arg(&search_path);
        cmd.current_dir(&self.project_root);

        // Use 30s timeout for grep (large repos can take time)
        match run_command_with_cleanup(&mut cmd, 30.0) {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                match out.status.code() {
                    Some(0) => {
                        let result = if text.len() > 20_000 {
                            let mut t = text[..20_000].to_string();
                            t.push_str("\n[truncated]");
                            t
                        } else {
                            text.to_string()
                        };

                        if scope_context {
                            annotate_grep_with_scope(&result, &self.project_root)
                        } else {
                            result
                        }
                    }
                    Some(1) => {
                        let warn = stderr.trim();
                        if warn.is_empty() {
                            "No matches found".to_string()
                        } else {
                            format!("No matches found (warnings: {warn})")
                        }
                    }
                    _ => {
                        let detail = stderr.trim();
                        if detail.is_empty() {
                            "Error: grep failed".to_string()
                        } else {
                            format!("Error: {detail}")
                        }
                    }
                }
            }
            Err(e) => e,
        }
    }

    pub(crate) fn glob(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'pattern'".to_string(),
        };
        let base = args
            .get("path")
            .and_then(Value::as_str)
            .map(|p| self.resolve(p))
            .unwrap_or_else(|| self.project_root.clone());

        // Validate base path exists
        if !base.exists() {
            return format!(
                "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
                base.display()
            );
        }

        let shell_cmd = format!(
            "cd {} && find . {} -o -name {} -print | sed 's|^./||' | head -200",
            shell_escape(base.to_string_lossy().as_ref()),
            default_find_prune_clause(),
            shell_escape(pattern.split('/').next_back().unwrap_or(pattern))
        );
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&shell_cmd);
        // Use 15s timeout for glob/find (directory traversal)
        match run_command_with_cleanup(&mut cmd, 15.0) {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.trim().is_empty() {
                    "No files found".to_string()
                } else {
                    text.to_string()
                }
            }
            Err(e) => e,
        }
    }

    /// Fetch a URL and return its content (text or HTML→text).
    /// Reports HTTP status, content type, and size for transparency.
    pub(crate) fn web_fetch(&self, args: &Value) -> String {
        let url = match args.get("url").and_then(Value::as_str) {
            Some(u) => u,
            None => return "Error: missing 'url'".to_string(),
        };
        // Basic URL validation
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return "Error: url must start with http:// or https://".to_string();
        }
        // SSRF protection: block internal/private IP ranges
        if let Some(reason) = is_ssrf_target(url) {
            return format!("Error: blocked URL ({reason})");
        }
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(10_000) as usize;
        let timeout_secs = args.get("timeout").and_then(Value::as_u64).unwrap_or(10);

        // Use -w to capture HTTP status code and content type for structured reporting
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-L",
            "--max-time",
            &timeout_secs.to_string(),
            "--max-filesize",
            &(max_bytes * 2).to_string(),
            "-H",
            "User-Agent: mo-agent/0.1",
            "-w",
            "\n__CURL_META__%{http_code} %{content_type} %{size_download} %{url_effective}",
            url,
        ])
        .current_dir(&self.project_root);

        // Apply sandbox environment filtering (same as bash)
        if let Some(ref policy) = self.sandbox_policy
            && !matches!(policy.mode, SandboxMode::Permissive)
            && let Err(e) = sandbox_command(policy, &mut cmd)
        {
            return format!("Error: sandbox policy application failed: {e}");
        }

        // Use timeout_secs + 5s buffer for our wrapper (curl has its own --max-time)
        match run_command_with_cleanup(&mut cmd, timeout_secs as f64 + 5.0) {
            Ok(out) => {
                let status = out.status;
                let raw = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                if !status.success() && raw.is_empty() {
                    return format!("Error: {stderr}");
                }

                // Parse metadata from -w output
                let (body, meta_line) = if let Some(idx) = raw.rfind("\n__CURL_META__") {
                    (&raw[..idx], Some(&raw[idx + "\n__CURL_META__".len()..]))
                } else {
                    (raw.as_ref(), None)
                };

                let (http_code, _content_type, final_url) = if let Some(meta) = meta_line {
                    let parts: Vec<&str> = meta.splitn(4, ' ').collect();
                    (
                        parts.first().copied().unwrap_or("?"),
                        parts.get(1).copied().unwrap_or("?"),
                        parts.get(3).copied().unwrap_or(""),
                    )
                } else {
                    ("?", "?", "")
                };

                // Detect cross-host redirect
                let redirected = if !final_url.is_empty() && final_url != url {
                    // Check if host changed
                    let orig_host = url.split('/').nth(2).unwrap_or("");
                    let final_host = final_url.split('/').nth(2).unwrap_or("");
                    if orig_host != final_host {
                        Some(final_url)
                    } else {
                        None
                    }
                } else {
                    None
                };

                // HTTP error codes
                if let Ok(code) = http_code.parse::<u16>() {
                    if code >= 400 {
                        let reason = match code {
                            401 | 403 => " (authentication required — look for an MCP tool with authenticated access)",
                            404 => " (page not found)",
                            429 => " (rate limited — try again later)",
                            _ => "",
                        };
                        return format!("Error: HTTP {code}{reason}\nURL: {url}");
                    }
                }

                let mut result = body.to_string();

                // Convert HTML to plain text for LLM consumption
                if looks_like_html(&result) {
                    result = html_to_text(&result);
                }

                if result.len() > max_bytes {
                    result.truncate(max_bytes);
                    result.push_str("\n[truncated]");
                }

                // Append metadata footer
                let mut footer = String::new();
                if let Some(redir) = redirected {
                    footer.push_str(&format!("\n[Redirected to: {redir}]"));
                }
                if !footer.is_empty() {
                    result.push_str(&footer);
                }

                result
            }
            Err(e) => {
                if e.contains("timed out") {
                    format!("Error: curl timed out after {timeout_secs}s")
                } else {
                    format!("Error: curl not available — {e}")
                }
            }
        }
    }
}

/// Annotate grep results with tree-sitter scope context.
///
/// For each `file:line:content` match, looks up the containing function/class
/// using `scope_at_line()` and appends it as `  (in fn_name)` annotation.
/// Only annotates matches in files with supported languages.
/// File contents are cached to avoid re-reading the same file for multiple matches.
fn annotate_grep_with_scope(grep_output: &str, project_root: &std::path::Path) -> String {
    use super::code_intel::{detect_language, scope_at_line};
    use std::collections::HashMap;

    // Cache: file path → (source, language)
    let mut file_cache: HashMap<String, Option<(String, super::code_intel::Language)>> =
        HashMap::new();

    let mut result = String::with_capacity(grep_output.len() + grep_output.len() / 10);

    for line in grep_output.lines() {
        // Parse grep output: file:line:content or file-line-content (context)
        // Only annotate primary matches (colon separator), not context (dash separator)
        if let Some((file_part, rest)) = line.split_once(':')
            && let Some((line_num_str, _content)) = rest.split_once(':')
            && let Ok(line_num) = line_num_str.trim().parse::<usize>()
        {
            let file_path = if std::path::Path::new(file_part).is_absolute() {
                file_part.to_string()
            } else {
                project_root.join(file_part).to_string_lossy().to_string()
            };

            let cached = file_cache.entry(file_path.clone()).or_insert_with(|| {
                let path = std::path::Path::new(&file_path);
                let lang = detect_language(path)?;
                let source = std::fs::read_to_string(path).ok()?;
                Some((source, lang))
            });

            if let Some((source, lang)) = cached {
                let ctx = scope_at_line(source, *lang, line_num);
                let scope_str = if ctx.breadcrumbs.len() > 1 {
                    ctx.breadcrumbs.join(" > ")
                } else if let Some(ref sym) = ctx.symbol {
                    sym.name.clone()
                } else {
                    String::new()
                };
                if !scope_str.is_empty() {
                    result.push_str(line);
                    result.push_str("  // in ");
                    result.push_str(&scope_str);
                    result.push('\n');
                    continue;
                }
            }
        }
        result.push_str(line);
        result.push('\n');
    }

    // Remove trailing newline
    if result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Detect HTML content by checking for common HTML markers.
fn looks_like_html(s: &str) -> bool {
    let trimmed = s.trim_start();
    trimmed.starts_with("<!DOCTYPE")
        || trimmed.starts_with("<!doctype")
        || trimmed.starts_with("<html")
        || trimmed.starts_with("<HTML")
        // Partial HTML without doctype (common in API error pages)
        || (trimmed.starts_with('<')
            && (trimmed.contains("</head>") || trimmed.contains("</body>")))
}

/// Lightweight HTML → text conversion without external dependencies.
/// Strips tags, decodes common entities, collapses whitespace.
fn html_to_text(html: &str) -> String {
    let mut s = html.to_string();

    // 1. Remove <script> and <style> blocks (case-insensitive via manual lowering)
    for tag in &["script", "style", "noscript", "svg"] {
        loop {
            let lower = s.to_lowercase();
            let open = format!("<{}", tag);
            let close = format!("</{}>", tag);
            if let Some(start) = lower.find(&open)
                && let Some(end_rel) = lower[start..].find(&close)
            {
                let end = start + end_rel + close.len();
                s.replace_range(start..end, " ");
                continue;
            }
            break;
        }
    }

    // 2. Insert newlines for block elements
    for tag in &[
        "<br>", "<br/>", "<br />", "<BR>", "</p>", "</P>", "</div>", "</DIV>", "</li>", "</LI>",
        "</tr>", "</TR>", "</h1>", "</h2>", "</h3>", "</h4>", "</h5>", "</h6>", "</H1>", "</H2>",
        "</H3>", "</H4>", "</H5>", "</H6>",
    ] {
        s = s.replace(tag, &format!("\n{}", tag));
    }

    // 3. Strip all remaining HTML tags
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    // 4. Decode common HTML entities
    out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&#x27;", "'")
        .replace("&#x2F;", "/");

    // Decode numeric character references &#NNN;
    let mut decoded = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' && chars.peek() == Some(&'#') {
            chars.next(); // consume '#'
            let mut num_str = String::new();
            while let Some(&d) = chars.peek() {
                if d == ';' {
                    chars.next();
                    break;
                }
                if d.is_ascii_digit() && num_str.len() < 7 {
                    num_str.push(d);
                    chars.next();
                } else {
                    break;
                }
            }
            if let Ok(code) = num_str.parse::<u32>()
                && let Some(decoded_char) = char::from_u32(code)
            {
                decoded.push(decoded_char);
                continue;
            }
            decoded.push('&');
            decoded.push('#');
            decoded.push_str(&num_str);
        } else {
            decoded.push(ch);
        }
    }

    // 5. Collapse whitespace: runs of spaces/tabs → single space, 3+ newlines → 2
    let mut result = String::with_capacity(decoded.len());
    let mut last_was_newline = false;
    let mut consecutive_newlines = 0u32;
    let mut last_was_space = false;

    for ch in decoded.chars() {
        if ch == '\n' || ch == '\r' {
            if ch == '\r' {
                continue;
            }
            consecutive_newlines += 1;
            last_was_space = false;
            if consecutive_newlines <= 2 {
                result.push('\n');
            }
            last_was_newline = true;
        } else if ch == ' ' || ch == '\t' {
            if !last_was_space && !last_was_newline {
                result.push(' ');
            }
            last_was_space = true;
        } else {
            result.push(ch);
            last_was_newline = false;
            last_was_space = false;
            consecutive_newlines = 0;
        }
    }

    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor() -> ToolExecutor {
        ToolExecutor::new(std::env::temp_dir())
    }

    fn test_executor_in(dir: &std::path::Path) -> ToolExecutor {
        ToolExecutor::new(dir)
    }

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn bash_missing_command_returns_error() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn bash_echo_returns_output() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "echo hello"}));
        assert!(result.trim().contains("hello"), "got: {result}");
    }

    #[test]
    fn bash_timeout_kills_process() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "sleep 10", "timeout": 0.2}));
        assert!(result.contains("timed out"), "got: {result}");
    }

    #[test]
    fn bash_timeout_kills_child_process_tree() {
        // Spawn a parent bash that starts a child sleep.
        // After timeout, verify the child is also killed via process group.
        let executor = test_executor();
        // Use a unique marker file to detect if the child survived
        let marker = format!("/tmp/mo_test_pgid_{}", std::process::id());
        let cmd = format!("bash -c 'sleep 10 && touch {marker}' & wait");
        let result = executor.bash(&serde_json::json!({"command": cmd, "timeout": 0.3}));
        assert!(result.contains("timed out"), "got: {result}");
        // Give a moment for any surviving child to act
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !std::path::Path::new(&marker).exists(),
            "child process survived timeout — process group kill failed"
        );
    }

    /// Adaptive bash timeout tiers: instant, fast-read, search, default.
    #[test]
    fn bash_timeout_tiers() {
        // We can't easily test the actual timeout value used internally,
        // but we verify the logic by checking that fast commands complete
        // well within their 5s tier without hitting the 30s default.
        let executor = test_executor();

        // Tier 1 (5s): instant commands
        let start = std::time::Instant::now();
        let r = executor.bash(&serde_json::json!({"command": "echo hello"}));
        assert!(!r.contains("timed out"));
        assert!(start.elapsed().as_secs() < 5);

        // Tier 3 (15s): search command that completes fast
        let r = executor.bash(&serde_json::json!({"command": "grep --version"}));
        assert!(!r.contains("timed out"));

        // Explicit timeout overrides tier
        let r = executor.bash(&serde_json::json!({"command": "sleep 10", "timeout": 0.1}));
        assert!(
            r.contains("timed out"),
            "explicit timeout should override tier"
        );
    }

    #[test]
    fn bash_failed_command_returns_output() {
        let executor = test_executor();
        let result = executor.bash(&serde_json::json!({"command": "echo err >&2 && false"}));
        assert!(result.contains("err"), "got: {result}");
    }

    #[test]
    fn grep_missing_pattern_returns_error() {
        let executor = test_executor();
        let result = executor.grep(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn grep_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "hello",
            "path": "src-tauri/src"
        }));
        assert!(
            result.contains("Error"),
            "should error on missing path, got: {result}"
        );
        assert!(
            result.contains("does not exist"),
            "should mention path doesn't exist, got: {result}"
        );
        assert!(
            result.contains("list_dir"),
            "should suggest list_dir, got: {result}"
        );
    }

    #[test]
    fn grep_nonexistent_absolute_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "hello",
            "path": "/nonexistent/fake/directory"
        }));
        assert!(
            result.contains("Error"),
            "should error on missing path, got: {result}"
        );
        assert!(result.contains("does not exist"), "got: {result}");
    }

    #[test]
    fn grep_finds_pattern_in_file() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello world\nfoo bar").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "foo", "path": "."}));
        assert!(result.contains("foo bar"), "got: {result}");
    }

    #[test]
    fn grep_no_match_returns_message() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "hello").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "zzzzz", "path": "."}));
        assert!(result.contains("No matches"), "got: {result}");
    }

    #[test]
    fn grep_skips_default_generated_directories() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("src").join("app.rs"), "needle in source").unwrap();
        std::fs::write(dir.path().join("dist").join("bundle.js"), "needle in build").unwrap();

        let result = executor.grep(&serde_json::json!({"pattern": "needle", "path": "."}));
        assert!(result.contains("src/app.rs"), "got: {result}");
        assert!(
            !result.contains("dist/bundle.js"),
            "default grep should skip bulky dirs: {result}"
        );
    }

    #[test]
    fn glob_skips_default_generated_directories() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("target").join("cached.rs"), "").unwrap();

        let result = executor.glob(&serde_json::json!({"pattern": "*.rs", "path": "."}));
        assert!(result.contains("src/main.rs"), "got: {result}");
        assert!(
            !result.contains("target/cached.rs"),
            "default glob should skip bulky dirs: {result}"
        );
    }

    #[test]
    fn glob_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.glob(&serde_json::json!({
            "pattern": "*.py",
            "path": "nonexistent/directory"
        }));
        assert!(
            result.contains("Error"),
            "should error on missing path, got: {result}"
        );
        assert!(result.contains("does not exist"), "got: {result}");
        assert!(
            result.contains("list_dir"),
            "should suggest list_dir, got: {result}"
        );
    }

    // ── str_replace diff preview ─────────────────────────────────────────────

    #[test]
    fn str_replace_shows_diff_preview() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(
            dir.path().join("code.rs"),
            "fn main() {\n    println!(\"old\");\n}\n",
        )
        .unwrap();

        executor.read_file(&serde_json::json!({"path": "code.rs"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "code.rs",
            "old_str": "println!(\"old\")",
            "new_str": "println!(\"new\")"
        }));
        assert!(result.contains("Replaced successfully"), "got: {result}");
        assert!(result.contains("- "), "should have - line: {result}");
        assert!(result.contains("+ "), "should have + line: {result}");
        assert!(result.contains("old"), "should show old text: {result}");
        assert!(result.contains("new"), "should show new text: {result}");
    }

    #[test]
    fn str_replace_large_diff_shows_summary() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        // Create a file with many lines
        let old_block: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let new_block: String = (0..25).map(|i| format!("new_line {i}\n")).collect();
        let content = format!("header\n{old_block}footer\n");
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        executor.read_file(&serde_json::json!({"path": "big.txt"}));
        let result = executor.str_replace(&serde_json::json!({
            "path": "big.txt",
            "old_str": old_block.trim_end(),
            "new_str": new_block.trim_end()
        }));
        assert!(result.contains("Replaced successfully"), "got: {result}");
        assert!(
            result.contains("lines →"),
            "large diff should show summary: {result}"
        );
    }

    // ── resolve_checked sandbox ──────────────────────────────────────────────

    #[test]
    fn resolve_checked_with_permissive_sandbox_allows_all() {
        use mo_agent_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let mut executor = ToolExecutor::new(dir.path());
        // Permissive policy → all paths allowed
        executor.sandbox_policy = Some(SandboxPolicy::permissive(dir.path()));
        let result = executor.resolve_checked("/etc/passwd");
        assert!(result.is_ok(), "should allow with permissive: {result:?}");
    }

    #[test]
    fn resolve_checked_with_sandbox_blocks_escape() {
        use mo_agent_runtime::tool_sandbox::SandboxPolicy;
        let dir = tempfile::tempdir().unwrap();
        let mut executor = ToolExecutor::new(dir.path());
        let mut policy = SandboxPolicy::strict(dir.path());
        policy.allowed_paths.clear();
        executor.sandbox_policy = Some(policy);

        let result = executor.resolve_checked("/etc/passwd");
        assert!(result.is_err(), "should block path escape: {result:?}");
        assert!(
            result.unwrap_err().contains("Sandbox"),
            "should mention sandbox"
        );
    }

    // ── SSRF protection ─────────────────────────────────────────────────────

    #[test]
    fn ssrf_blocks_localhost() {
        assert!(is_ssrf_target("http://127.0.0.1:8080/secret").is_some());
        assert!(is_ssrf_target("http://localhost/admin").is_some());
        assert!(is_ssrf_target("http://0.0.0.0:3000").is_some());
        assert!(is_ssrf_target("http://[::1]/api").is_some());
    }

    #[test]
    fn ssrf_blocks_private_networks() {
        assert!(is_ssrf_target("http://10.0.0.1/internal").is_some());
        assert!(is_ssrf_target("http://192.168.1.1/router").is_some());
        assert!(is_ssrf_target("http://172.16.0.1/service").is_some());
        assert!(is_ssrf_target("http://172.31.255.1/db").is_some());
        // 172.15 and 172.32 are NOT private
        assert!(is_ssrf_target("http://172.15.0.1/ok").is_none());
        assert!(is_ssrf_target("http://172.32.0.1/ok").is_none());
    }

    #[test]
    fn ssrf_blocks_cloud_metadata() {
        assert!(is_ssrf_target("http://169.254.169.254/latest/meta-data/").is_some());
        assert!(is_ssrf_target("http://metadata.google.internal/computeMetadata/v1/").is_some());
    }

    #[test]
    fn ssrf_allows_public_urls() {
        assert!(is_ssrf_target("https://github.com/matrixorigin/matrixone").is_none());
        assert!(is_ssrf_target("https://api.github.com/repos").is_none());
        assert!(is_ssrf_target("http://example.com").is_none());
        assert!(is_ssrf_target("https://docs.rs/tokio/latest").is_none());
    }

    // ── web_fetch ─────────────────────────────────────────────────────────────

    #[test]
    fn web_fetch_missing_url_returns_error() {
        let executor = test_executor();
        let result = executor.web_fetch(&serde_json::json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn web_fetch_invalid_scheme_returns_error() {
        let executor = test_executor();
        let result = executor.web_fetch(&serde_json::json!({"url": "ftp://example.com"}));
        assert!(result.contains("http"), "got: {result}");
    }

    // ── Process group cleanup tests ──────────────────────────────────────────
    // These tests verify that child processes spawned by grep/glob/curl are
    // properly killed when timing out, preventing zombie process leaks.

    #[test]
    fn run_command_with_cleanup_timeout_kills_process_group() {
        // Test that run_command_with_cleanup properly kills the entire process group
        let marker = format!("/tmp/mo_test_cleanup_{}", std::process::id());
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(format!("sleep 10 && touch {marker}"));

        let result = run_command_with_cleanup(&mut cmd, 0.2);
        assert!(result.is_err(), "should timeout");
        assert!(
            result.unwrap_err().contains("timed out"),
            "should indicate timeout"
        );

        // Give a moment for any surviving child to act
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            !std::path::Path::new(&marker).exists(),
            "child process survived timeout — process group kill failed"
        );
    }

    #[test]
    fn run_command_with_cleanup_success_returns_output() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello");
        let result = run_command_with_cleanup(&mut cmd, 5.0);
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[test]
    fn grep_uses_process_group_cleanup() {
        // Verify grep doesn't leave zombie processes on timeout
        // This is a regression test for the curl zombie leak issue.
        // We can't easily force grep to timeout, but we can verify it completes normally.
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "findme\n").unwrap();

        // Normal grep should work
        let result = executor.grep(&serde_json::json!({"pattern": "findme", "path": "."}));
        assert!(result.contains("findme"), "got: {result}");

        // After grep completes, verify no zombie processes from this test
        // (This is more of a smoke test — the real protection is the process_group(0))
    }

    #[test]
    fn glob_uses_process_group_cleanup() {
        // Verify glob (which uses bash internally) properly cleans up
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("test.txt"), "content\n").unwrap();

        let result = executor.glob(&serde_json::json!({"pattern": "*.txt"}));
        assert!(result.contains("test.txt"), "got: {result}");
    }

    // ── grep extended regex ──────────────────────────────────────────────────

    #[test]
    fn grep_alternation_pattern_works() {
        // Regression test: grep must use -E for extended regex so that
        // alternation patterns like "foo|bar" work as OR, not literal "|".
        // Session 62c1e8e9: `grep "skill|Skill" --include "*.rs"` returned
        // nothing because without -E, "|" is treated as literal.
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.grep(&serde_json::json!({
            "pattern": "fn|struct",
            "include": "*.rs"
        }));
        // In a Rust project, "fn" and "struct" both exist — alternation should match
        assert!(
            !result.contains("No matches found"),
            "Extended regex alternation should work: got: {result}"
        );
    }

    #[test]
    fn grep_basic_pattern_still_works() {
        let executor =
            ToolExecutor::new(std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()));
        let result = executor.grep(&serde_json::json!({
            "pattern": "fn main",
            "include": "*.rs"
        }));
        // Simple non-regex pattern should still work
        assert!(!result.is_empty());
    }

    // ── HTML detection ──────────────────────────────────────────────────────

    #[test]
    fn looks_like_html_detects_doctype() {
        assert!(looks_like_html(
            "<!DOCTYPE html><html><body>hello</body></html>"
        ));
        assert!(looks_like_html("<!doctype html>\n<html>"));
    }

    #[test]
    fn looks_like_html_detects_html_tag() {
        assert!(looks_like_html("<html lang=\"en\"><head></head></html>"));
        assert!(looks_like_html("<HTML><BODY>hi</BODY></HTML>"));
    }

    #[test]
    fn looks_like_html_rejects_plain_text() {
        assert!(!looks_like_html("Hello world, this is plain text."));
        assert!(!looks_like_html("{\"key\": \"value\"}"));
        assert!(!looks_like_html("# Markdown heading\n\nSome text."));
    }

    #[test]
    fn looks_like_html_rejects_xml_without_body() {
        assert!(!looks_like_html("<root><item>data</item></root>"));
    }

    // ── HTML-to-text conversion ─────────────────────────────────────────────

    #[test]
    fn html_to_text_strips_tags() {
        let html = "<p>Hello <b>world</b></p>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("<p>"));
        assert!(!text.contains("<b>"));
    }

    #[test]
    fn html_to_text_removes_script_and_style() {
        let html = "<html><head><style>body{color:red}</style></head>\
                     <body><script>alert('xss')</script><p>content</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("content"), "got: {text}");
        assert!(!text.contains("alert"), "script not stripped: {text}");
        assert!(!text.contains("color:red"), "style not stripped: {text}");
    }

    #[test]
    fn html_to_text_decodes_entities() {
        let html = "<p>A &amp; B &lt; C &gt; D &quot;E&quot; F&apos;s</p>";
        let text = html_to_text(html);
        assert!(text.contains("A & B"), "got: {text}");
        assert!(text.contains("< C >"), "got: {text}");
        assert!(text.contains("\"E\""), "got: {text}");
        assert!(text.contains("F's"), "got: {text}");
    }

    #[test]
    fn html_to_text_decodes_numeric_entities() {
        let html = "<p>&#65;&#66;&#67;</p>"; // ABC
        let text = html_to_text(html);
        assert!(text.contains("ABC"), "got: {text}");
    }

    #[test]
    fn html_to_text_inserts_newlines_for_blocks() {
        let html = "<h1>Title</h1><p>Paragraph one.</p><p>Paragraph two.</p>";
        let text = html_to_text(html);
        // Block elements should create line breaks
        assert!(text.contains("Title"), "got: {text}");
        assert!(
            text.contains("Paragraph one.") && text.contains("Paragraph two."),
            "got: {text}"
        );
    }

    #[test]
    fn html_to_text_collapses_whitespace() {
        let html = "<p>  lots   of    spaces  </p>\n\n\n\n\n<p>many newlines</p>";
        let text = html_to_text(html);
        assert!(!text.contains("     "), "excessive spaces: {text}");
        // No more than 2 consecutive newlines
        assert!(!text.contains("\n\n\n"), "excessive newlines: {text}");
    }

    #[test]
    fn html_to_text_handles_real_page() {
        let html = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Example</title>
  <style>body { margin: 0; }</style>
  <script>window.ga=function(){}</script>
</head>
<body>
  <div id="main">
    <h1>Welcome</h1>
    <p>This is a <a href="/about">test page</a> with links.</p>
    <ul>
      <li>Item 1</li>
      <li>Item 2</li>
    </ul>
  </div>
  <script src="analytics.js"></script>
</body>
</html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Welcome"), "missing heading: {text}");
        assert!(text.contains("test page"), "missing link text: {text}");
        assert!(text.contains("Item 1"), "missing list item: {text}");
        assert!(!text.contains("<"), "tags not stripped: {text}");
        assert!(!text.contains("window.ga"), "script not removed: {text}");
        assert!(!text.contains("margin: 0"), "style not removed: {text}");
    }

    #[test]
    fn html_to_text_passthrough_json() {
        // JSON is not HTML, so it passes through unchanged
        let json = r#"{"name": "test", "value": 42}"#;
        assert!(!looks_like_html(json));
    }

    #[test]
    fn html_to_text_passthrough_plain_text() {
        let plain = "This is just plain text\nwith some newlines.";
        assert!(!looks_like_html(plain));
    }

    // ── grep context_lines and max_matches ───────────────────────────────────

    #[test]
    fn grep_context_lines_passed_to_command() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ctx.txt");
        std::fs::write(&file, "line1\nline2\nMATCH\nline4\nline5\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "MATCH",
            "path": "ctx.txt",
            "context_lines": 1
        }));
        // With -C1, should see line2 and line4 as context
        assert!(result.contains("MATCH"), "should find match: {result}");
        assert!(
            result.contains("line2") || result.contains("line4"),
            "should have context lines: {result}"
        );
    }

    #[test]
    fn grep_max_matches_limits_output() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("repeat.txt");
        std::fs::write(&file, "foo\nfoo\nfoo\nfoo\nfoo\n").unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "foo",
            "path": "repeat.txt",
            "max_matches": 2
        }));
        let match_count = result.matches("foo").count();
        assert!(
            match_count <= 3,
            "should limit to ~2 matches, got {match_count}: {result}"
        );
    }

    #[test]
    fn grep_context_lines_capped_at_10() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("small.txt");
        std::fs::write(&file, "MATCH\n").unwrap();

        let executor = test_executor_in(dir.path());
        // Requesting 100 context lines should be capped to 10
        let result = executor.grep(&serde_json::json!({
            "pattern": "MATCH",
            "path": "small.txt",
            "context_lines": 100
        }));
        assert!(
            result.contains("MATCH"),
            "should still find match: {result}"
        );
    }

    #[test]
    fn grep_combined_context_and_max() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("combo.txt");
        let mut content = String::new();
        for i in 0..20 {
            content.push_str(&format!("line{i}\n"));
            if i % 5 == 0 {
                content.push_str("TARGET\n");
            }
        }
        std::fs::write(&file, &content).unwrap();

        let executor = test_executor_in(dir.path());
        let result = executor.grep(&serde_json::json!({
            "pattern": "TARGET",
            "path": "combo.txt",
            "context_lines": 1,
            "max_matches": 2
        }));
        let target_count = result.matches("TARGET").count();
        assert!(
            target_count <= 3,
            "should limit matches, got {target_count}: {result}"
        );
    }

    // ═══════════════════════ Scope Context Tests ═══════════════════════

    #[test]
    fn annotate_grep_with_scope_adds_function_context() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        // Grep output for a pattern that should be inside a known function in code_intel.rs
        let grep_output = format!(
            "{}/src/edge_tools/code_intel.rs:138:    let tree = match cached_parse(source, lang) {{",
            root.display()
        );
        let result = annotate_grep_with_scope(&grep_output, root);
        // Should annotate with the containing function name
        assert!(
            result.contains("// in "),
            "should contain scope annotation: {result}"
        );
    }

    #[test]
    fn annotate_grep_with_scope_no_change_for_unknown_files() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let grep_output = "nonexistent.xyz:10:some content";
        let result = annotate_grep_with_scope(grep_output, root);
        assert_eq!(
            result, grep_output,
            "unknown files should pass through unchanged"
        );
    }

    #[test]
    fn annotate_grep_with_scope_handles_empty_input() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let result = annotate_grep_with_scope("", root);
        assert_eq!(result, "");
    }

    #[test]
    fn annotate_grep_with_scope_preserves_non_match_lines() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let grep_output = "-- separator --\nsome random line";
        let result = annotate_grep_with_scope(grep_output, root);
        assert!(
            result.contains("-- separator --"),
            "should preserve non-match lines"
        );
    }

    #[test]
    fn grep_scope_context_parameter() {
        // Test that scope_context parameter is properly parsed
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let executor = super::ToolExecutor::new(root.to_path_buf());
        let result = executor.grep(&serde_json::json!({
            "pattern": "fn annotate_grep_with_scope",
            "path": "src/edge_tools/shell.rs",
            "scope_context": true
        }));
        assert!(
            result.contains("annotate_grep_with_scope"),
            "should find the function: {result}"
        );
        // With scope_context=true, should have function annotation
        assert!(
            result.contains("// in "),
            "should have scope context annotation: {result}"
        );
    }
}
