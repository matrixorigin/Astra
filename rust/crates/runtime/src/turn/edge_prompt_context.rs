//! Edge-side prompt context shared by CLI `chat_stream` and any code building `edge_profile.workspace` / file context.
//!
//! Part of Phase 0: move cognition-adjacent **pure** helpers out of `astra` toward `runtime` so
//! in-process bridge and thin clients can converge on one implementation.
//!
//! [`make_args_preview`] reuses [`super::tool_argument_hints`] (`path` / `command` only) so journal
//! previews match CLI permission lines and cloud path hints.

use std::path::Path;

use serde_json::{Value, json};

use super::tool_argument_hints::{command_hint_from_args, path_hint_from_args};

/// Build a compact workspace context object for the LLM / server (`edge_profile.workspace`).
/// Detects project type, key files, and top-level directory structure. Capped implicitly by listing limits.
pub fn detect_workspace_context(project_root: &Path) -> Value {
    let mut project_type = Vec::new();
    let mut key_files = Vec::new();

    let markers = [
        ("Cargo.toml", "rust"),
        ("package.json", "node/javascript"),
        ("go.mod", "go"),
        ("pyproject.toml", "python"),
        ("requirements.txt", "python"),
        ("pom.xml", "java/maven"),
        ("build.gradle", "java/gradle"),
        ("Makefile", "make"),
        ("Dockerfile", "docker"),
        ("docker-compose.yml", "docker-compose"),
        ("docker-compose.yaml", "docker-compose"),
    ];
    for (file, ptype) in markers {
        if project_root.join(file).exists() {
            if !project_type.contains(&ptype) {
                project_type.push(ptype);
            }
            key_files.push(file.to_string());
        }
    }

    let mut top_dirs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(project_root) {
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.')
                || name_str == "target"
                || name_str == "node_modules"
                || name_str == "__pycache__"
                || name_str == "dist"
                || name_str == "build"
                || name_str == "htmlcov"
            {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                top_dirs.push(format!("{name_str}/"));
            }
            if top_dirs.len() >= 15 {
                break;
            }
        }
    }

    json!({
        "project_types": project_type,
        "key_files": key_files,
        "top_directories": top_dirs,
    })
}

/// Detect project languages/frameworks from workspace marker files.
/// Returns tags like `"rust"`, `"typescript"`, `"python"`, etc.
pub fn detect_project_languages(root: &Path) -> Vec<String> {
    let markers: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("package.json", "javascript"),
        ("tsconfig.json", "typescript"),
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("requirements.txt", "python"),
        ("go.mod", "go"),
        ("pom.xml", "java"),
        ("build.gradle", "java"),
        ("build.gradle.kts", "kotlin"),
        ("Gemfile", "ruby"),
        ("mix.exs", "elixir"),
        ("CMakeLists.txt", "cpp"),
        ("Makefile", "make"),
        (".csproj", "csharp"),
        ("composer.json", "php"),
        ("Dockerfile", "docker"),
    ];
    let mut langs = Vec::new();
    for &(file, lang) in markers {
        if root.join(file).exists() {
            langs.push(lang.to_string());
        }
    }
    if langs.iter().all(|l| l != "csharp")
        && let Ok(entries) = std::fs::read_dir(root)
    {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str()
                && (name.ends_with(".csproj") || name.ends_with(".sln"))
            {
                langs.push("csharp".to_string());
                break;
            }
        }
    }
    langs.dedup();
    langs
}

/// Build a human-readable environment context string for the system prompt.
/// Includes OS, architecture, shell, CWD, git branch/status, and terminal info.
/// This gives the LLM awareness of the runtime environment for better tool usage.
pub fn build_environment_context(project_root: &Path) -> String {
    let mut lines = Vec::new();

    // OS and architecture
    lines.push(format!(
        "- Platform: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

    // Shell
    let shell = std::env::var("SHELL")
        .or_else(|_| std::env::var("COMSPEC"))
        .unwrap_or_default();
    if !shell.is_empty() {
        let shell_name = std::path::Path::new(&shell)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or(shell);
        lines.push(format!("- Shell: {shell_name}"));
    }

    // CWD
    lines.push(format!("- CWD: {}", project_root.display()));

    // Git branch and status (best-effort, non-blocking)
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(project_root)
        .output()
        && let Ok(branch) = String::from_utf8(output.stdout)
    {
        let branch = branch.trim();
        if !branch.is_empty() {
            // Check if repo is dirty
            let dirty = std::process::Command::new("git")
                .args(["status", "--porcelain", "--untracked-files=no"])
                .current_dir(project_root)
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            let status = if dirty { " (dirty)" } else { "" };
            lines.push(format!("- Git branch: {branch}{status}"));
        }
    }

    // Home directory
    if let Ok(home) = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
        lines.push(format!("- Home: {home}"));
    }

    format!("\n\n## Environment\n{}", lines.join("\n"))
}

/// Max Unicode scalar values (`char`s) kept before appending `…` (U+2026).
/// Char-based truncation avoids splitting UTF-8 bytes (which would panic on e.g. CJK).
const MAX_ARGS_PREVIEW_CHARS: usize = 79;

/// Compact preview of tool arguments for observability (journal, stderr).
pub fn make_args_preview(tool_name: &str, args: &Value) -> Option<String> {
    let preview = match tool_name {
        "read_file" | "write_file" | "delete_file" | "str_replace" | "multi_edit" => {
            path_hint_from_args(args)
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            let path = path_hint_from_args(args).unwrap_or_else(|| ".".to_string());
            Some(format!("/{pattern}/ in {path}"))
        }
        "glob" => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "shell_exec" | "bash" => command_hint_from_args(args).map(String::from),
        "git_diff" => {
            let base = args.get("base").and_then(|v| v.as_str()).unwrap_or("HEAD");
            let file = args.get("file").and_then(|v| v.as_str());
            match file {
                Some(f) => Some(format!("{base} -- {f}")),
                None => Some(base.to_string()),
            }
        }
        "git_log" | "git_show" => args
            .get("ref")
            .or_else(|| args.get("commit"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "git_blame" => args
            .get("file")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "memory_search" | "memory_retrieve" => args
            .get("query")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "memory_store" => args
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        "web_fetch" => args
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        _ => args.as_object().and_then(|obj| {
            obj.values()
                .filter_map(|v| v.as_str())
                .next()
                .map(|s| s.to_string())
        }),
    };

    preview.map(|s| {
        if s.chars().count() > MAX_ARGS_PREVIEW_CHARS {
            let body: String = s.chars().take(MAX_ARGS_PREVIEW_CHARS).collect();
            format!("{body}…")
        } else {
            s
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn make_args_preview_file_tool_uses_path() {
        let v = json!({"path": "crates/foo/src/lib.rs"});
        assert_eq!(
            make_args_preview("read_file", &v).as_deref(),
            Some("crates/foo/src/lib.rs")
        );
    }

    #[test]
    fn make_args_preview_grep_uses_path() {
        let v = json!({"pattern": "TODO", "path": "src/main.rs"});
        assert_eq!(
            make_args_preview("grep", &v).as_deref(),
            Some("/TODO/ in src/main.rs")
        );
    }

    #[test]
    fn make_args_preview_bash_uses_command() {
        let v = json!({"command": "cargo test -p astra-runtime"});
        assert_eq!(
            make_args_preview("bash", &v).as_deref(),
            Some("cargo test -p astra-runtime")
        );
    }

    #[test]
    fn make_args_preview_truncates_long_path_with_ellipsis() {
        let long = "a".repeat(100);
        let v = json!({"path": long});
        let prev = make_args_preview("read_file", &v).expect("preview");
        assert!(prev.ends_with('…'));
        // 79 ASCII chars from source + U+2026 (3 UTF-8 bytes).
        assert_eq!(prev.len(), 82);
        assert_eq!(prev.chars().count(), 80);
    }

    #[test]
    fn make_args_preview_truncates_utf8_on_char_boundary() {
        // Long ASCII + CJK: old byte-based slice could panic inside a multibyte char.
        let cmd = format!("{}{}", "a".repeat(30), "在".repeat(50));
        let v = json!({"command": cmd});
        let prev = make_args_preview("bash", &v).expect("preview");
        assert!(prev.ends_with('…'));
        assert_eq!(prev.chars().count(), 80);
        let expected = format!("{}{}", "a".repeat(30), "在".repeat(49));
        assert_eq!(prev.strip_suffix('…'), Some(expected.as_str()));
    }

    #[test]
    fn detect_project_languages_finds_cargo_toml() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.contains(&"rust".to_string()));
    }

    #[test]
    fn detect_project_languages_finds_multiple() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), "{}").unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM rust").unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.contains(&"javascript".to_string()));
        assert!(langs.contains(&"docker".to_string()));
    }

    #[test]
    fn detect_project_languages_empty_for_unknown() {
        let tmp = tempdir().unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.is_empty());
    }

    #[test]
    fn detect_project_languages_typescript_from_tsconfig() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("tsconfig.json"), "{}").unwrap();
        let langs = detect_project_languages(tmp.path());
        assert!(langs.contains(&"typescript".to_string()));
    }

    #[test]
    fn workspace_context_detects_rust_project() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::create_dir(tmp.path().join("tests")).unwrap();

        let ctx = detect_workspace_context(tmp.path());
        let types = ctx["project_types"].as_array().unwrap();
        assert!(
            types.iter().any(|v| v.as_str() == Some("rust")),
            "should detect rust, got: {ctx}"
        );
        assert!(
            ctx["key_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("Cargo.toml")),
            "should list Cargo.toml, got: {ctx}"
        );
        let dirs = ctx["top_directories"].as_array().unwrap();
        assert!(
            dirs.iter().any(|v| v.as_str() == Some("src/")),
            "should list src/, got: {ctx}"
        );
    }

    #[test]
    fn workspace_context_detects_multiple_project_types() {
        let tmp = tempdir().unwrap();
        std::fs::write(tmp.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(tmp.path().join("Makefile"), "").unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "").unwrap();

        let ctx = detect_workspace_context(tmp.path());
        let types = ctx["project_types"].as_array().unwrap();
        assert!(types.len() >= 3, "should detect 3+ types, got: {ctx}");
    }

    #[test]
    fn workspace_context_empty_dir() {
        let tmp = tempdir().unwrap();
        let ctx = detect_workspace_context(tmp.path());
        let types = ctx["project_types"].as_array().unwrap();
        assert!(types.is_empty(), "empty dir should have no project types");
        let dirs = ctx["top_directories"].as_array().unwrap();
        assert!(dirs.is_empty(), "empty dir should have no dirs");
    }

    #[test]
    fn workspace_context_skips_hidden_and_noise() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::create_dir(tmp.path().join("target")).unwrap();
        std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();

        let ctx = detect_workspace_context(tmp.path());
        let dirs = ctx["top_directories"].as_array().unwrap();
        let dir_strs: Vec<&str> = dirs.iter().filter_map(|v| v.as_str()).collect();
        assert!(!dir_strs.contains(&".git/"), "should skip .git");
        assert!(!dir_strs.contains(&"target/"), "should skip target");
        assert!(
            !dir_strs.contains(&"node_modules/"),
            "should skip node_modules"
        );
        assert!(dir_strs.contains(&"src/"), "should include src/");
    }

    // ── build_environment_context tests ──────────────────────────────────

    #[test]
    fn environment_context_contains_platform() {
        let tmp = tempdir().unwrap();
        let ctx = build_environment_context(tmp.path());
        assert!(ctx.contains("## Environment"), "should have section header");
        assert!(ctx.contains("- Platform:"), "should contain platform info");
        assert!(
            ctx.contains(std::env::consts::OS),
            "should contain current OS"
        );
    }

    #[test]
    fn environment_context_contains_cwd() {
        let tmp = tempdir().unwrap();
        let ctx = build_environment_context(tmp.path());
        assert!(ctx.contains("- CWD:"), "should contain CWD line");
        let tmp_str = tmp.path().to_string_lossy();
        assert!(
            ctx.contains(&*tmp_str),
            "should contain actual CWD path: {ctx}"
        );
    }

    #[test]
    fn environment_context_in_git_repo() {
        // Use the actual repo root (we know it's a git repo)
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap();
        let ctx = build_environment_context(repo_root);
        assert!(
            ctx.contains("- Git branch:"),
            "should detect git branch in repo: {ctx}"
        );
    }

    #[test]
    fn environment_context_no_git_in_temp() {
        let tmp = tempdir().unwrap();
        // Override GIT_CEILING_DIRECTORIES so git won't discover any repo
        // above the temp dir (e.g. if /tmp itself is inside a worktree).
        // SAFETY: test is single-threaded for this env var; no concurrent readers.
        unsafe {
            std::env::set_var("GIT_CEILING_DIRECTORIES", tmp.path().parent().unwrap());
        }
        let ctx = build_environment_context(tmp.path());
        unsafe {
            std::env::remove_var("GIT_CEILING_DIRECTORIES");
        }
        assert!(
            !ctx.contains("- Git branch:"),
            "temp dir should not have git branch"
        );
    }
}
