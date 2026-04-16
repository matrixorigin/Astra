//! Declarative stop hooks: `.astra/stop-hooks.yaml` (or `.yml`).
//!
//! Shared by CLI and server: load from a project root, merge auto-detect, and classify by `when`.
//! See `docs/design/stop-hooks.md`.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use astra_services::edge_context::EdgeContext;
use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;

use crate::chat_turn_heuristics::TaskExecutionProfile;
use crate::stop_hooks::StopHook;

const CANDIDATE_NAMES: [&str; 2] = ["stop-hooks.yaml", "stop-hooks.yml"];

#[derive(Debug, Clone, Default)]
pub struct TurnHookSets {
    pub stop_hooks: Vec<StopHook>,
    pub teammate_idle_hooks: Vec<StopHook>,
}

#[derive(Debug, Clone, Deserialize)]
struct FileRoot {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default = "default_true")]
    auto_detect: bool,
    #[serde(default)]
    hooks: Vec<FileHook>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileHook {
    label: String,
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default = "default_when")]
    when: String,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
}

fn default_when() -> String {
    "stop".to_string()
}

impl Default for FileRoot {
    fn default() -> Self {
        Self {
            version: 1,
            auto_detect: true,
            hooks: Vec::new(),
        }
    }
}

/// Prefer git root, then cwd (edge workspace).
pub fn project_root_for_stop_hooks(ctx: &EdgeContext) -> Option<PathBuf> {
    let p = ctx
        .edge_profile
        .git_root
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ctx.edge_profile
                .cwd
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })?;
    Some(PathBuf::from(p))
}

/// Workspace hint from delegation `context` (optional keys used when edge mirrors them).
pub fn project_root_from_delegation_context(ctx: &HashMap<String, Value>) -> Option<PathBuf> {
    let pick = ctx
        .get("git_root")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            ctx.get("workspace_root")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            ctx.get("cwd")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })?;
    Some(PathBuf::from(pick))
}

pub fn is_plan_subtask_from_context_map(m: &Map<String, Value>) -> bool {
    if m.get("is_plan_subtask").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    m.get("plan_subtask_id")
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

pub fn is_plan_subtask_from_chat_context(context: &Option<Map<String, Value>>) -> bool {
    context
        .as_ref()
        .map(is_plan_subtask_from_context_map)
        .unwrap_or(false)
}

pub fn is_plan_subtask_from_delegation_context(ctx: &HashMap<String, Value>) -> bool {
    if ctx.get("is_plan_subtask").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    ctx.get("plan_subtask_id")
        .and_then(Value::as_str)
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

fn load_declarative_config(project_root: &Path) -> FileRoot {
    let dir = project_root.join(".astra");
    for name in CANDIDATE_NAMES {
        let path = dir.join(name);
        if !path.is_file() {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                astra_core::agent_warn!("stop_hooks", "read {}: {e}", path.display());
                return FileRoot::default();
            }
        };
        match serde_yaml::from_str::<FileRoot>(&raw) {
            Ok(cfg) => {
                if cfg.version != 1 {
                    astra_core::agent_warn!(
                        "stop_hooks",
                        "{}: unsupported version {}, expected 1 — using defaults",
                        path.display(),
                        cfg.version
                    );
                    return FileRoot::default();
                }
                return cfg;
            }
            Err(e) => {
                astra_core::agent_warn!(
                    "stop_hooks",
                    "parse {}: {e} — ignoring declarative hooks",
                    path.display()
                );
                return FileRoot::default();
            }
        }
    }
    FileRoot::default()
}

fn resolve_working_dir(project_root: &Path, wd: Option<&str>) -> String {
    let rel = wd.map(|s| s.trim()).filter(|s| !s.is_empty() && *s != ".");
    let Some(rel) = rel else {
        return project_root.to_string_lossy().into_owned();
    };

    let mut acc = project_root.to_path_buf();
    for c in Path::new(rel).components() {
        match c {
            Component::Normal(x) => acc.push(x),
            Component::ParentDir => {
                acc.pop();
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {}
        }
    }

    if let (Ok(root), Ok(can)) = (project_root.canonicalize(), acc.canonicalize()) {
        if can.starts_with(&root) {
            return can.to_string_lossy().into_owned();
        }
        astra_core::agent_warn!(
            "stop_hooks",
            "working_dir '{rel}' escapes project root — using project root"
        );
        return project_root.to_string_lossy().into_owned();
    }

    if acc.starts_with(project_root) {
        acc.to_string_lossy().into_owned()
    } else {
        astra_core::agent_warn!(
            "stop_hooks",
            "working_dir '{rel}' could not be anchored — using project root"
        );
        project_root.to_string_lossy().into_owned()
    }
}

fn normalize_when(raw: &str) -> String {
    raw.trim().to_lowercase().replace('-', "_")
}

fn declarative_hooks_for_when(project_root: &Path, cfg: &FileRoot, phase: &str) -> Vec<StopHook> {
    let want = normalize_when(phase);
    let mut out = Vec::new();
    for h in &cfg.hooks {
        if !h.enabled {
            continue;
        }
        if normalize_when(&h.when) != want {
            continue;
        }
        let label = h.label.trim();
        let command = h.command.trim();
        if label.is_empty() || command.is_empty() {
            astra_core::agent_warn!("stop_hooks", "skip hook with empty label or command");
            continue;
        }
        let wd = resolve_working_dir(project_root, h.working_dir.as_deref());
        out.push(StopHook {
            label: label.to_string(),
            command: command.to_string(),
            working_dir: Some(wd),
            depends_on: Vec::new(),
            timeout_secs: None,
            cache_key: None,
        });
    }
    out
}

fn declarative_stop_hooks(project_root: &Path, cfg: &FileRoot) -> Vec<StopHook> {
    declarative_hooks_for_when(project_root, cfg, "stop")
}

fn auto_detect_verify_changes_hook(
    project_root: &Path,
    task_profile: TaskExecutionProfile,
) -> Vec<StopHook> {
    if !task_profile.verification_required {
        return Vec::new();
    }

    let mut tool_hints = Vec::new();
    if project_root.join("rust/Cargo.toml").exists() || project_root.join("Cargo.toml").exists() {
        tool_hints.push("Rust/Cargo (cargo check, cargo test)");
    }
    if project_root.join("package.json").exists() {
        tool_hints.push("Node.js/npm (npm run build, npm test)");
    }
    if project_root.join("go.mod").exists() {
        tool_hints.push("Go (go vet, go test)");
    }
    if project_root.join("pyproject.toml").exists() || project_root.join("setup.py").exists() {
        tool_hints.push("Python (pytest, mypy, ruff)");
    }

    if tool_hints.is_empty() {
        return Vec::new();
    }

    let tools_list = tool_hints.join(", ");
    vec![StopHook {
        label: "verify-changes".into(),
        command: format!(
            "Based on the files you actually modified, run ONLY the relevant checks. \
             Available tools: {tools_list}. \
             Skip checks unrelated to your changes. \
             If you only modified files outside the project (e.g. /tmp), skip all project checks."
        ),
        working_dir: Some(project_root.to_string_lossy().to_string()),
        depends_on: Vec::new(),
        timeout_secs: None,
        cache_key: None,
    }]
}

/// Merge declarative YAML + optional auto-detect into completion and teammate-idle sets.
pub fn detect_turn_hook_sets(
    project_root: &Path,
    task_profile: TaskExecutionProfile,
    is_plan_subtask: bool,
) -> TurnHookSets {
    let cfg = load_declarative_config(project_root);
    let teammate_idle_hooks = declarative_hooks_for_when(project_root, &cfg, "teammate_idle");

    if is_plan_subtask {
        let mut hooks = declarative_hooks_for_when(project_root, &cfg, "task_completed");
        if task_profile.verification_required && cfg.auto_detect {
            hooks.extend(auto_detect_verify_changes_hook(project_root, task_profile));
        }
        if !task_profile.verification_required && hooks.is_empty() {
            return TurnHookSets {
                stop_hooks: Vec::new(),
                teammate_idle_hooks,
            };
        }
        return TurnHookSets {
            stop_hooks: hooks,
            teammate_idle_hooks,
        };
    }

    let mut hooks = declarative_stop_hooks(project_root, &cfg);
    if task_profile.verification_required && cfg.auto_detect {
        hooks.extend(auto_detect_verify_changes_hook(project_root, task_profile));
    }

    if !task_profile.verification_required && hooks.is_empty() {
        return TurnHookSets {
            stop_hooks: Vec::new(),
            teammate_idle_hooks,
        };
    }

    TurnHookSets {
        stop_hooks: hooks,
        teammate_idle_hooks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn declarative_parses_minimal_yaml() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"
version: 1
auto_detect: false
hooks:
  - label: test
    command: cargo test -q
"#,
        )
        .unwrap();
        let prof = TaskExecutionProfile::default();
        let s = detect_turn_hook_sets(dir.path(), prof, false);
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "test");
        assert_eq!(s.stop_hooks[0].command, "cargo test -q");
    }

    #[test]
    fn plan_subtask_uses_task_completed_phase() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: global
    command: echo a
    when: stop
  - label: sub
    command: echo b
    when: task_completed
"#,
        )
        .unwrap();
        let prof = TaskExecutionProfile {
            mutates_workspace: true,
            verification_required: true,
            ..TaskExecutionProfile::default()
        };
        let s = detect_turn_hook_sets(dir.path(), prof, true);
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "sub");
    }

    #[test]
    fn context_map_detects_plan_subtask_id() {
        let mut m = Map::new();
        m.insert("plan_subtask_id".into(), Value::String("t1".into()));
        assert!(is_plan_subtask_from_context_map(&m));
    }

    // ──────────────────────────────────────────────────────────
    // normalize_when
    // ──────────────────────────────────────────────────────────

    #[test]
    fn normalize_when_lowercase_and_replace_dash() {
        assert_eq!(normalize_when("Task-Completed"), "task_completed");
    }

    #[test]
    fn normalize_when_trims_whitespace() {
        assert_eq!(normalize_when("  stop  "), "stop");
    }

    #[test]
    fn normalize_when_already_normalized() {
        assert_eq!(normalize_when("teammate_idle"), "teammate_idle");
    }

    #[test]
    fn normalize_when_empty() {
        assert_eq!(normalize_when(""), "");
    }

    // ──────────────────────────────────────────────────────────
    // is_plan_subtask_from_context_map
    // ──────────────────────────────────────────────────────────

    #[test]
    fn context_map_detects_is_plan_subtask_flag() {
        let mut m = Map::new();
        m.insert("is_plan_subtask".into(), Value::Bool(true));
        assert!(is_plan_subtask_from_context_map(&m));
    }

    #[test]
    fn context_map_false_flag() {
        let mut m = Map::new();
        m.insert("is_plan_subtask".into(), Value::Bool(false));
        assert!(!is_plan_subtask_from_context_map(&m));
    }

    #[test]
    fn context_map_empty_subtask_id() {
        let mut m = Map::new();
        m.insert("plan_subtask_id".into(), Value::String("".into()));
        assert!(!is_plan_subtask_from_context_map(&m));
    }

    #[test]
    fn context_map_empty() {
        let m = Map::new();
        assert!(!is_plan_subtask_from_context_map(&m));
    }

    // ──────────────────────────────────────────────────────────
    // is_plan_subtask_from_chat_context
    // ──────────────────────────────────────────────────────────

    #[test]
    fn chat_context_none() {
        assert!(!is_plan_subtask_from_chat_context(&None));
    }

    #[test]
    fn chat_context_with_flag() {
        let mut m = Map::new();
        m.insert("is_plan_subtask".into(), Value::Bool(true));
        assert!(is_plan_subtask_from_chat_context(&Some(m)));
    }

    // ──────────────────────────────────────────────────────────
    // is_plan_subtask_from_delegation_context
    // ──────────────────────────────────────────────────────────

    #[test]
    fn delegation_context_flag() {
        let mut ctx = HashMap::new();
        ctx.insert("is_plan_subtask".into(), Value::Bool(true));
        assert!(is_plan_subtask_from_delegation_context(&ctx));
    }

    #[test]
    fn delegation_context_subtask_id() {
        let mut ctx = HashMap::new();
        ctx.insert("plan_subtask_id".into(), Value::String("build-1".into()));
        assert!(is_plan_subtask_from_delegation_context(&ctx));
    }

    #[test]
    fn delegation_context_empty() {
        let ctx = HashMap::new();
        assert!(!is_plan_subtask_from_delegation_context(&ctx));
    }

    // ──────────────────────────────────────────────────────────
    // project_root_from_delegation_context
    // ──────────────────────────────────────────────────────────

    #[test]
    fn delegation_root_from_git_root() {
        let mut ctx = HashMap::new();
        ctx.insert(
            "git_root".into(),
            Value::String("/home/user/project".into()),
        );
        let r = project_root_from_delegation_context(&ctx).unwrap();
        assert_eq!(r, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn delegation_root_from_workspace_root() {
        let mut ctx = HashMap::new();
        ctx.insert(
            "workspace_root".into(),
            Value::String("/home/user/ws".into()),
        );
        let r = project_root_from_delegation_context(&ctx).unwrap();
        assert_eq!(r, PathBuf::from("/home/user/ws"));
    }

    #[test]
    fn delegation_root_from_cwd() {
        let mut ctx = HashMap::new();
        ctx.insert("cwd".into(), Value::String("/tmp".into()));
        let r = project_root_from_delegation_context(&ctx).unwrap();
        assert_eq!(r, PathBuf::from("/tmp"));
    }

    #[test]
    fn delegation_root_prefers_git_root_over_cwd() {
        let mut ctx = HashMap::new();
        ctx.insert(
            "git_root".into(),
            Value::String("/home/user/project".into()),
        );
        ctx.insert("cwd".into(), Value::String("/tmp".into()));
        let r = project_root_from_delegation_context(&ctx).unwrap();
        assert_eq!(r, PathBuf::from("/home/user/project"));
    }

    #[test]
    fn delegation_root_empty_strings_skipped() {
        let mut ctx = HashMap::new();
        ctx.insert("git_root".into(), Value::String("".into()));
        ctx.insert("cwd".into(), Value::String("/fallback".into()));
        let r = project_root_from_delegation_context(&ctx).unwrap();
        assert_eq!(r, PathBuf::from("/fallback"));
    }

    #[test]
    fn delegation_root_empty_context() {
        let ctx = HashMap::new();
        assert!(project_root_from_delegation_context(&ctx).is_none());
    }

    #[test]
    fn delegation_root_whitespace_only_skipped() {
        let mut ctx = HashMap::new();
        ctx.insert("git_root".into(), Value::String("   ".into()));
        assert!(project_root_from_delegation_context(&ctx).is_none());
    }

    // ──────────────────────────────────────────────────────────
    // resolve_working_dir
    // ──────────────────────────────────────────────────────────

    #[test]
    fn resolve_working_dir_none() {
        let root = tempdir().unwrap();
        let wd = resolve_working_dir(root.path(), None);
        assert_eq!(wd, root.path().to_string_lossy());
    }

    #[test]
    fn resolve_working_dir_dot() {
        let root = tempdir().unwrap();
        let wd = resolve_working_dir(root.path(), Some("."));
        assert_eq!(wd, root.path().to_string_lossy());
    }

    #[test]
    fn resolve_working_dir_empty() {
        let root = tempdir().unwrap();
        let wd = resolve_working_dir(root.path(), Some(""));
        assert_eq!(wd, root.path().to_string_lossy());
    }

    #[test]
    fn resolve_working_dir_subdir() {
        let root = tempdir().unwrap();
        let sub = root.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let wd = resolve_working_dir(root.path(), Some("subdir"));
        assert!(wd.contains("subdir"));
    }

    // ──────────────────────────────────────────────────────────
    // auto_detect_verify_changes_hook
    // ──────────────────────────────────────────────────────────

    #[test]
    fn auto_detect_no_verification_needed() {
        let root = tempdir().unwrap();
        let prof = TaskExecutionProfile::default(); // verification_required = false
        let hooks = auto_detect_verify_changes_hook(root.path(), prof);
        assert!(hooks.is_empty());
    }

    #[test]
    fn auto_detect_no_markers() {
        let root = tempdir().unwrap();
        let prof = TaskExecutionProfile {
            verification_required: true,
            ..Default::default()
        };
        let hooks = auto_detect_verify_changes_hook(root.path(), prof);
        assert!(hooks.is_empty()); // No Cargo.toml, package.json, etc.
    }

    #[test]
    fn auto_detect_cargo_toml() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "[package]").unwrap();
        let prof = TaskExecutionProfile {
            verification_required: true,
            ..Default::default()
        };
        let hooks = auto_detect_verify_changes_hook(root.path(), prof);
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].command.contains("Rust/Cargo"));
    }

    #[test]
    fn auto_detect_multiple_markers() {
        let root = tempdir().unwrap();
        std::fs::write(root.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(root.path().join("package.json"), "{}").unwrap();
        let prof = TaskExecutionProfile {
            verification_required: true,
            ..Default::default()
        };
        let hooks = auto_detect_verify_changes_hook(root.path(), prof);
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].command.contains("Rust/Cargo"));
        assert!(hooks[0].command.contains("Node.js"));
    }

    // ──────────────────────────────────────────────────────────
    // detect_turn_hook_sets (disabled hooks, teammate_idle)
    // ──────────────────────────────────────────────────────────

    #[test]
    fn disabled_hook_is_skipped() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: disabled-hook
    command: echo nope
    enabled: false
  - label: active-hook
    command: echo yes
"#,
        )
        .unwrap();
        let s = detect_turn_hook_sets(dir.path(), TaskExecutionProfile::default(), false);
        assert_eq!(s.stop_hooks.len(), 1);
        assert_eq!(s.stop_hooks[0].label, "active-hook");
    }

    #[test]
    fn teammate_idle_hooks_parsed() {
        let dir = tempdir().unwrap();
        let mo = dir.path().join(".astra");
        std::fs::create_dir_all(&mo).unwrap();
        std::fs::write(
            mo.join("stop-hooks.yaml"),
            r#"version: 1
auto_detect: false
hooks:
  - label: idle-check
    command: echo idle
    when: teammate_idle
"#,
        )
        .unwrap();
        let s = detect_turn_hook_sets(dir.path(), TaskExecutionProfile::default(), false);
        assert!(s.stop_hooks.is_empty());
        assert_eq!(s.teammate_idle_hooks.len(), 1);
        assert_eq!(s.teammate_idle_hooks[0].label, "idle-check");
    }

    #[test]
    fn no_yaml_file_returns_empty() {
        let dir = tempdir().unwrap();
        let s = detect_turn_hook_sets(dir.path(), TaskExecutionProfile::default(), false);
        assert!(s.stop_hooks.is_empty());
        assert!(s.teammate_idle_hooks.is_empty());
    }
}
