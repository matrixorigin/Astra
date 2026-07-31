//! Edge-side prompt context shared by CLI `chat_stream` and any code building `edge_profile.workspace` / file context.
//!
//! Part of Phase 0: move cognition-adjacent **pure** helpers out of `astra` toward `runtime` so
//! in-process bridge and thin clients can converge on one implementation.
//!
//! [`make_args_preview`] reuses [`crate::tool_argument_hints`] (`path` / `command` only) so journal
//! previews match CLI permission lines and cloud path hints.

use std::path::Path;

use serde_json::{Value, json};

use crate::tool::args::hints::{command_hint_from_args, path_hint_from_args};

const WORKSPACE_MARKERS: &[(&str, &str)] = &[
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

/// Build a compact workspace context object for the LLM / server (`edge_profile.workspace`).
/// Detects project type, key files, and top-level directory structure. Capped implicitly by listing limits.
pub fn detect_workspace_context(project_root: &Path) -> Value {
    let mut project_type = Vec::new();
    let mut key_files = Vec::new();
    let mut manifest_roots = Vec::new();

    for &(file, ptype) in WORKSPACE_MARKERS {
        if project_root.join(file).exists() {
            if !project_type.contains(&ptype) {
                project_type.push(ptype);
            }
            key_files.push(file.to_string());
            manifest_roots.push(json!({
                "path": ".",
                "manifest": file,
                "kind": ptype,
            }));
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
                if manifest_roots.len() < 20 {
                    for &(file, ptype) in WORKSPACE_MARKERS {
                        if entry.path().join(file).exists() {
                            if !project_type.contains(&ptype) {
                                project_type.push(ptype);
                            }
                            key_files.push(format!("{name_str}/{file}"));
                            manifest_roots.push(json!({
                                "path": name_str.to_string(),
                                "manifest": file,
                                "kind": ptype,
                            }));
                        }
                    }
                }
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
        "manifest_roots": manifest_roots,
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

/// Keep only a small sample of changed paths in the volatile Git tail.
const MAX_GIT_CHANGED_FILES: usize = 5;

#[derive(Debug, Default, PartialEq, Eq)]
struct GitDiffSummary {
    shortstat: Option<String>,
    files: Vec<String>,
}

fn format_git_shortstat(files_changed: usize, insertions: u64, deletions: u64) -> Option<String> {
    let mut parts = Vec::new();
    parts.push(format!(
        "{files_changed} {} changed",
        if files_changed == 1 { "file" } else { "files" }
    ));
    if insertions > 0 {
        parts.push(format!(
            "{insertions} {}(+)",
            if insertions == 1 {
                "insertion"
            } else {
                "insertions"
            }
        ));
    }
    if deletions > 0 {
        parts.push(format!(
            "{deletions} {}(-)",
            if deletions == 1 {
                "deletion"
            } else {
                "deletions"
            }
        ));
    }
    Some(parts.join(", "))
}

fn parse_git_numstat_summary(output: &str) -> GitDiffSummary {
    let mut files = Vec::new();
    let mut insertions = 0_u64;
    let mut deletions = 0_u64;
    let mut saw_numeric_counts = false;

    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut parts = line.splitn(3, '\t');
        let Some(added) = parts.next() else {
            continue;
        };
        let Some(removed) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next() else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        files.push(path.to_owned());
        if let Ok(count) = added.parse::<u64>() {
            insertions += count;
            saw_numeric_counts = true;
        }
        if let Ok(count) = removed.parse::<u64>() {
            deletions += count;
            saw_numeric_counts = true;
        }
    }

    GitDiffSummary {
        shortstat: if files.is_empty() || !saw_numeric_counts {
            None
        } else {
            format_git_shortstat(files.len(), insertions, deletions)
        },
        files,
    }
}

/// Run one `git diff --numstat` (staged or unstaged) and derive both the
/// compact summary and changed-file sample from the same snapshot.
fn git_diff_summary(project_root: &Path, staged: bool) -> Option<GitDiffSummary> {
    let mut args = vec!["diff", "--numstat", "--no-color"];
    if staged {
        args.push("--cached");
    }
    std::process::Command::new("git")
        .args(&args)
        .current_dir(project_root)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| parse_git_numstat_summary(&s))
}

fn render_git_change_summary(
    label: &str,
    shortstat: Option<&str>,
    files: &[String],
) -> Option<String> {
    let shortstat = shortstat.map(str::trim).filter(|s| !s.is_empty());
    let mut files = files.iter().filter(|s| !s.is_empty());
    let sample: Vec<&str> = files
        .by_ref()
        .take(MAX_GIT_CHANGED_FILES)
        .map(String::as_str)
        .collect();
    let extra_count = files.count();
    if shortstat.is_none() && sample.is_empty() {
        return None;
    }

    let mut line = format!("- {label}:");
    if let Some(shortstat) = shortstat {
        line.push(' ');
        line.push_str(shortstat);
    } else {
        let changed = sample.len() + extra_count;
        line.push_str(&format!(" {changed} file(s) changed"));
    }

    if !sample.is_empty() {
        line.push_str(" [files: ");
        line.push_str(&sample.join(", "));
        if extra_count > 0 {
            line.push_str(&format!(", +{extra_count} more"));
        }
        line.push(']');
    }

    Some(line)
}

fn recent_commit_limit_for_dirty(dirty: bool) -> usize {
    if dirty { 1 } else { 3 }
}

/// Run `git log --oneline -N` to get recent commit summaries.
fn git_recent_commits(project_root: &Path, n: usize) -> Option<String> {
    std::process::Command::new("git")
        .args(["log", "--oneline", "--no-color", &format!("-{n}")])
        .current_dir(project_root)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Session-stable environment facts: Platform, Shell, CWD, Home.
///
/// Safe to place inside a Session-scoped cache block — the content does
/// not change during a normal session (OS doesn't swap, shell doesn't
/// change, cwd is fixed for the lifetime of the runtime).
///
/// Returns an empty string if no fields can be populated.
pub fn build_static_environment_context(project_root: &Path) -> String {
    let mut lines = Vec::new();

    lines.push(format!(
        "- Platform: {} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));

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

    lines.push(format!("- CWD: {}", project_root.display()));

    let workspace_context = detect_workspace_context(project_root);
    if let Some(manifest_roots) = workspace_context
        .get("manifest_roots")
        .and_then(Value::as_array)
        .filter(|roots| !roots.is_empty())
    {
        let entries: Vec<String> = manifest_roots
            .iter()
            .take(8)
            .filter_map(|root| {
                let path = root.get("path").and_then(Value::as_str)?;
                let manifest = root.get("manifest").and_then(Value::as_str)?;
                let kind = root
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("project");
                let display = if path == "." {
                    manifest.to_string()
                } else {
                    format!("{path}/{manifest}")
                };
                Some(format!("{display} ({kind})"))
            })
            .collect();
        if !entries.is_empty() {
            lines.push(format!("- Workspace manifests: {}", entries.join(", ")));
        }
    }

    let local_root = astra_runtime_env::local_state_root_override();
    let home = std::env::var("HOME").ok();
    let user_profile = std::env::var("USERPROFILE").ok();
    if let Some(home) = visible_environment_home(
        local_root.as_deref(),
        home.as_deref(),
        user_profile.as_deref(),
    ) {
        lines.push(format!("- Home: {home}"));
    }

    format!("\n\n## Environment\n{}", lines.join("\n"))
}

/// A process-local Astra root is an orchestration isolation boundary. In that
/// mode the host account's unrelated home path must not enter model input.
fn visible_environment_home<'a>(
    local_root: Option<&Path>,
    home: Option<&'a str>,
    user_profile: Option<&'a str>,
) -> Option<&'a str> {
    if local_root.is_some() {
        None
    } else {
        home.filter(|value| !value.is_empty())
            .or_else(|| user_profile.filter(|value| !value.is_empty()))
    }
}

/// Turn-volatile environment facts: branch dirty state, staged/unstaged
/// diff stats, recent commits. Must NOT go into the cached Session prefix
/// — any edit / commit flips the content and invalidates the cache for
/// every subsequent turn.
///
/// Returns an empty string when the project isn't a git repo or the
/// commands fail.
pub fn build_volatile_environment_context(project_root: &Path) -> String {
    let mut lines = Vec::new();
    let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(project_root)
        .output()
    else {
        return String::new();
    };
    let Ok(branch) = String::from_utf8(output.stdout) else {
        return String::new();
    };
    let branch = branch.trim();
    if branch.is_empty() {
        return String::new();
    }

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

    if dirty {
        let staged = git_diff_summary(project_root, true).unwrap_or_default();
        if let Some(staged) =
            render_git_change_summary("Staged changes", staged.shortstat.as_deref(), &staged.files)
        {
            lines.push(staged);
        }
        let unstaged = git_diff_summary(project_root, false).unwrap_or_default();
        if let Some(unstaged) = render_git_change_summary(
            "Unstaged changes",
            unstaged.shortstat.as_deref(),
            &unstaged.files,
        ) {
            lines.push(unstaged);
        }
    }

    // 3 commits is the sweet spot: enough for the model to orient on
    // recent work ("what did I just do?") without spending ~160c/turn on
    // ancient history that git(action=log/show) can fetch on demand. The cap
    // was 5; observed volatile-block sessions (69657ca7) showed commits
    // 4-5 were always just context noise the model never cited.
    let recent_commit_limit = recent_commit_limit_for_dirty(dirty);
    if let Some(log) = git_recent_commits(project_root, recent_commit_limit) {
        if !log.is_empty() {
            lines.push(format!("- Recent commits:\n{log}"));
        }
    }

    if lines.is_empty() {
        String::new()
    } else {
        format!("\n\n## Git State\n{}", lines.join("\n"))
    }
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
        "git" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            match action {
                "diff" => {
                    let base = args.get("base").and_then(|v| v.as_str()).unwrap_or("HEAD");
                    let file = args.get("file").and_then(|v| v.as_str());
                    match file {
                        Some(f) => Some(format!("{base} -- {f}")),
                        None => Some(base.to_string()),
                    }
                }
                "log" | "show" => args
                    .get("ref")
                    .or_else(|| args.get("commit"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                "blame" => args
                    .get("file")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            }
        }
        "memory" => {
            let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
            match action {
                "recall" => args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                "remember" => args
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                "focus" => args
                    .get("focus_value")
                    .or_else(|| args.get("value"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            }
        }
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
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    fn run_git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .expect("spawn git");
        assert!(
            output.status.success(),
            "git {:?} failed: stdout={} stderr={}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_recent_commits_repo(dirty: bool) -> tempfile::TempDir {
        let tmp = tempdir().unwrap();
        run_git(tmp.path(), &["init", "-q"]);
        run_git(tmp.path(), &["config", "user.name", "Astra Test"]);
        run_git(
            tmp.path(),
            &["config", "user.email", "astra-test@example.com"],
        );

        let tracked = tmp.path().join("tracked.txt");
        for i in 1..=4 {
            std::fs::write(&tracked, format!("version {i}\n")).unwrap();
            run_git(tmp.path(), &["add", "tracked.txt"]);
            let msg = format!("commit-{i}");
            run_git(tmp.path(), &["commit", "-q", "-m", &msg]);
        }

        if dirty {
            std::fs::write(&tracked, "dirty working tree\n").unwrap();
        }

        tmp
    }

    fn count_recent_commit_lines(ctx: &str) -> usize {
        let Some(start) = ctx.find("- Recent commits:") else {
            return 0;
        };
        ctx[start..]
            .lines()
            .skip(1)
            .take_while(|line| line.chars().take(7).all(|c| c.is_ascii_hexdigit()))
            .count()
    }

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
        assert_eq!(ctx["manifest_roots"][0]["path"], ".");
        assert_eq!(ctx["manifest_roots"][0]["manifest"], "Cargo.toml");
        let dirs = ctx["top_directories"].as_array().unwrap();
        assert!(
            dirs.iter().any(|v| v.as_str() == Some("src/")),
            "should list src/, got: {ctx}"
        );
    }

    #[test]
    fn workspace_context_detects_nested_manifest_roots() {
        let tmp = tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("rust")).unwrap();
        std::fs::write(tmp.path().join("rust").join("Cargo.toml"), "[workspace]").unwrap();

        let ctx = detect_workspace_context(tmp.path());

        assert!(
            ctx["project_types"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("rust")),
            "should detect nested rust workspace, got: {ctx}"
        );
        assert!(
            ctx["key_files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v.as_str() == Some("rust/Cargo.toml")),
            "should list nested Cargo.toml, got: {ctx}"
        );
        assert_eq!(ctx["manifest_roots"][0]["path"], "rust");
        assert_eq!(ctx["manifest_roots"][0]["manifest"], "Cargo.toml");

        let env = build_static_environment_context(tmp.path());
        assert!(
            env.contains("Workspace manifests: rust/Cargo.toml (rust)"),
            "{env}"
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

    // ── environment context split: static + volatile ─────────────────

    #[test]
    fn static_context_contains_platform_cwd_and_no_git() {
        let tmp = tempdir().unwrap();
        let ctx = build_static_environment_context(tmp.path());
        assert!(ctx.contains("## Environment"));
        assert!(ctx.contains("- Platform:"));
        assert!(ctx.contains(std::env::consts::OS));
        assert!(ctx.contains("- CWD:"));
        let tmp_str = tmp.path().to_string_lossy();
        assert!(ctx.contains(&*tmp_str));
        // Static path MUST NOT include git fields — those are the source
        // of cache invalidation.
        assert!(
            !ctx.contains("- Git branch:"),
            "static ctx must not contain git branch: {ctx}"
        );
        assert!(
            !ctx.contains("- Recent commits:"),
            "static ctx must not contain recent commits: {ctx}"
        );
        assert!(
            !ctx.contains("- Staged changes:"),
            "static ctx must not contain staged diff: {ctx}"
        );
    }

    #[test]
    fn isolated_local_root_suppresses_host_home_from_environment_context() {
        assert_eq!(
            visible_environment_home(
                Some(Path::new("/isolated/astra")),
                Some("/developer/home"),
                Some("C:\\Users\\developer"),
            ),
            None
        );
        assert_eq!(
            visible_environment_home(None, Some("/developer/home"), None),
            Some("/developer/home")
        );
    }

    #[test]
    fn volatile_context_contains_git_branch_in_repo() {
        let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .unwrap();
        let ctx = build_volatile_environment_context(repo_root);
        assert!(ctx.contains("## Git State"), "should have section header");
        assert!(ctx.contains("- Git branch:"));
    }

    #[test]
    #[serial_test::serial]
    fn volatile_context_empty_outside_git() {
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("GIT_CEILING_DIRECTORIES", tmp.path().parent().unwrap());
        }
        let ctx = build_volatile_environment_context(tmp.path());
        unsafe {
            std::env::remove_var("GIT_CEILING_DIRECTORIES");
        }
        assert!(
            ctx.is_empty(),
            "outside a git repo the volatile ctx must be empty (nothing to route through volatile lane): {ctx}"
        );
    }

    #[test]
    fn volatile_context_includes_recent_commits_in_repo() {
        let cwd = std::env::current_dir().unwrap();
        let ctx = build_volatile_environment_context(&cwd);
        assert!(ctx.contains("- Recent commits:"));
    }

    /// Pin the recent-commit cap at ≤3 so the Git State section stays lean.
    /// The volatile lane runs on every turn; each extra commit adds ~80c.
    /// Trim from 5→3 saved ~160c per session (observed in 69657ca7) and
    /// 3 has consistently been the "what did I just do?" sweet spot for
    /// the model — anything older is better fetched via git(action=log) on demand.
    #[test]
    fn volatile_context_caps_recent_commits_at_three() {
        let cwd = std::env::current_dir().unwrap();
        let ctx = build_volatile_environment_context(&cwd);
        let start = ctx
            .find("- Recent commits:\n")
            .expect("has commits section");
        let after = &ctx[start + "- Recent commits:\n".len()..];
        let commit_lines = after
            .lines()
            .take_while(|l| !l.is_empty() && !l.starts_with("- "))
            .count();
        assert!(
            commit_lines <= 3,
            "volatile Git State should cap recent commits at 3, got {commit_lines}:\n{ctx}"
        );
    }

    #[test]
    fn render_git_change_summary_includes_shortstat_and_file_sample() {
        let summary = render_git_change_summary(
            "Unstaged changes",
            Some("10 files changed, 128 insertions(+), 16 deletions(-)"),
            &[
                "a.rs".to_string(),
                "b.rs".to_string(),
                "c.rs".to_string(),
                "d.rs".to_string(),
                "e.rs".to_string(),
                "f.rs".to_string(),
            ],
        )
        .expect("summary");
        assert!(summary.contains("10 files changed, 128 insertions(+), 16 deletions(-)"));
        assert!(summary.contains("[files: a.rs, b.rs, c.rs, d.rs, e.rs, +1 more]"));
        assert!(!summary.contains('\n'));
    }

    #[test]
    fn render_git_change_summary_uses_file_count_without_shortstat() {
        let summary = render_git_change_summary(
            "Staged changes",
            None,
            &["one.rs".to_string(), "two.rs".to_string()],
        )
        .expect("summary");
        assert!(summary.contains("2 file(s) changed"));
        assert!(summary.contains("[files: one.rs, two.rs]"));
    }

    /// Regression: even without a shortstat the file sample MUST be capped at
    /// `MAX_GIT_CHANGED_FILES` so a wide-touch commit (rename across the
    /// repo, codemod, generated-file refresh) cannot blow up volatile context.
    /// Pins both the cap and the "+N more" overflow rendering.
    #[test]
    fn render_git_change_summary_truncates_files_at_cap_without_shortstat() {
        // 12 files, no shortstat → expect 5 listed + "+7 more".
        let files: Vec<String> = (0..12).map(|i| format!("file_{i:02}.rs")).collect();
        let summary = render_git_change_summary("Unstaged changes", None, &files).expect("summary");

        assert!(
            summary.contains("12 file(s) changed"),
            "total count must reflect all files: {summary}"
        );
        for kept in &files[..MAX_GIT_CHANGED_FILES] {
            assert!(
                summary.contains(kept),
                "first {MAX_GIT_CHANGED_FILES} files must appear: missing {kept} in {summary}"
            );
        }
        for dropped in &files[MAX_GIT_CHANGED_FILES..] {
            assert!(
                !summary.contains(dropped),
                "files past the cap must NOT appear: leaked {dropped} in {summary}"
            );
        }
        assert!(
            summary.contains(&format!("+{} more", files.len() - MAX_GIT_CHANGED_FILES)),
            "overflow tag must reflect dropped count: {summary}"
        );
        assert!(
            !summary.contains('\n'),
            "rendered summary must stay single-line for volatile-context budget"
        );
    }

    #[test]
    fn render_git_change_summary_keeps_shortstat_without_files() {
        let summary = render_git_change_summary(
            "Staged changes",
            Some("2 files changed, 9 insertions(+)"),
            &[],
        )
        .expect("summary");
        assert_eq!(
            summary,
            "- Staged changes: 2 files changed, 9 insertions(+)"
        );
    }

    #[test]
    fn render_git_change_summary_returns_none_when_empty() {
        assert!(
            render_git_change_summary("Staged changes", None, &[]).is_none(),
            "empty shortstat + empty file sample should not render a placeholder line"
        );
    }

    #[test]
    fn parse_git_numstat_summary_collects_shortstat_and_files_together() {
        let summary = parse_git_numstat_summary("10\t2\ta.rs\n3\t0\tb.rs\n-\t-\tbinary.dat\n");
        assert_eq!(
            summary.shortstat.as_deref(),
            Some("3 files changed, 13 insertions(+), 2 deletions(-)")
        );
        assert_eq!(summary.files, vec!["a.rs", "b.rs", "binary.dat"]);
    }

    #[test]
    fn parse_git_numstat_summary_falls_back_to_files_when_counts_are_non_numeric() {
        let summary = parse_git_numstat_summary("-\t-\tbinary.dat\n");
        assert_eq!(summary.shortstat, None);
        assert_eq!(summary.files, vec!["binary.dat"]);
    }

    #[test]
    fn recent_commit_limit_prefers_tighter_budget_for_dirty_repos() {
        assert_eq!(recent_commit_limit_for_dirty(true), 1);
    }

    #[test]
    fn recent_commit_limit_keeps_longer_budget_for_clean_repos() {
        assert_eq!(recent_commit_limit_for_dirty(false), 3);
    }

    /// Regression: the recent-commits cap is intentionally lower when the
    /// working tree is dirty (the diff itself dominates the model's
    /// attention budget). A regression that swaps the two limits would
    /// silently waste tokens on every dirty render.
    #[test]
    fn build_volatile_environment_context_caps_recent_commits_lower_when_dirty() {
        let clean_repo = init_recent_commits_repo(false);
        let dirty_repo = init_recent_commits_repo(true);

        let clean_ctx = build_volatile_environment_context(clean_repo.path());
        let dirty_ctx = build_volatile_environment_context(dirty_repo.path());

        let clean_commits = count_recent_commit_lines(&clean_ctx);
        let dirty_commits = count_recent_commit_lines(&dirty_ctx);

        assert!(
            clean_commits <= 3,
            "clean repo should cap recent commits at 3, got {clean_commits}\n{clean_ctx}"
        );
        assert!(
            dirty_commits <= 1,
            "dirty repo should cap recent commits at 1, got {dirty_commits}\n{dirty_ctx}"
        );
    }

    #[test]
    fn git_diff_summary_returns_some_in_git_repo() {
        let cwd = std::env::current_dir().unwrap();
        // Should return Some (possibly empty summary) or None — just ensure no panic
        let _ = git_diff_summary(&cwd, true);
        let _ = git_diff_summary(&cwd, false);
    }

    #[test]
    #[serial_test::serial]
    fn git_diff_summary_returns_none_outside_repo() {
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("GIT_CEILING_DIRECTORIES", tmp.path().parent().unwrap());
        }
        let result = git_diff_summary(tmp.path(), false);
        unsafe {
            std::env::remove_var("GIT_CEILING_DIRECTORIES");
        }
        // Outside a git repo, git diff fails — returns None
        assert!(result.is_none());
    }

    #[test]
    fn git_recent_commits_returns_some_in_git_repo() {
        let cwd = std::env::current_dir().unwrap();
        let log = git_recent_commits(&cwd, 3);
        assert!(log.is_some(), "should have recent commits in project repo");
        let log = log.unwrap();
        assert!(log.lines().count() <= 3);
    }

    #[test]
    #[serial_test::serial]
    fn git_recent_commits_returns_none_outside_repo() {
        let tmp = tempdir().unwrap();
        unsafe {
            std::env::set_var("GIT_CEILING_DIRECTORIES", tmp.path().parent().unwrap());
        }
        let result = git_recent_commits(tmp.path(), 3);
        unsafe {
            std::env::remove_var("GIT_CEILING_DIRECTORIES");
        }
        assert!(result.is_none());
    }
}
