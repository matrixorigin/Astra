//! Declarative stop hooks: `.astra/stop-hooks.yaml` (or `.yml`).
//!
//! Shared by CLI and server: load from a project root, merge auto-detect, and classify by `when`.
//! See `docs/design/stop-hooks.md`.

use std::collections::HashMap;
use std::io::Read;

use std::path::{Component, Path};

use serde::Deserialize;
use serde_json::Map;
use serde_json::Value;

use crate::chat_turn_heuristics::TaskExecutionProfile;
use crate::stop_hooks::StopHook;
use astra_turn_types::{CompletionCheckDeclarations, CompletionCheckPhase as HookPhase};

const CANDIDATE_NAMES: [&str; 2] = ["stop-hooks.yaml", "stop-hooks.yml"];

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileRoot {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default = "default_true")]
    auto_detect: bool,
    #[serde(default, deserialize_with = "deserialize_file_hooks")]
    hooks: Vec<FileHook>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileHook {
    label: String,
    command: String,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    when: HookPhase,
    #[serde(default = "default_true")]
    enabled: bool,
}

fn deserialize_file_hooks<'de, D>(deserializer: D) -> Result<Vec<FileHook>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Hooks;
    impl<'de> serde::de::Visitor<'de> for Hooks {
        type Value = Vec<FileHook>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a bounded list of completion checks")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            use serde::de::Error;
            let mut hooks = Vec::new();
            while let Some(hook) = sequence.next_element()? {
                if hooks.len() == super::types::MAX_COMPLETION_CHECKS {
                    return Err(A::Error::custom("too many completion checks"));
                }
                hooks.push(hook);
            }
            Ok(hooks)
        }
    }
    deserializer.deserialize_seq(Hooks)
}

fn default_version() -> u32 {
    1
}

fn default_true() -> bool {
    true
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

fn load_declarative_config(project_root: &Path) -> Result<FileRoot, String> {
    let dir = project_root.join(".astra");
    for name in CANDIDATE_NAMES {
        let path = dir.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("inspect {}: {error}", path.display())),
        }
        let root = project_root
            .canonicalize()
            .map_err(|error| format!("resolve {}: {error}", project_root.display()))?;
        let source = path
            .canonicalize()
            .map_err(|error| format!("resolve {}: {error}", path.display()))?;
        if !source.starts_with(&root) {
            return Err(format!(
                "{}: completion config escapes workspace",
                path.display()
            ));
        }
        if !std::fs::metadata(&source)
            .map_err(|error| format!("inspect {}: {error}", path.display()))?
            .is_file()
        {
            return Err(format!(
                "{}: completion config must be a regular file",
                path.display()
            ));
        }
        let limit = super::types::MAX_COMPLETION_DECLARATION_BYTES;
        let mut raw = String::new();
        std::fs::File::open(&source)
            .and_then(|file| file.take((limit + 1) as u64).read_to_string(&mut raw))
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        if raw.len() > limit {
            return Err(format!(
                "{}: completion config exceeds {limit} bytes",
                path.display()
            ));
        }
        let cfg: FileRoot = serde_yaml_ng::from_str(&raw)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        if cfg.version != 1 {
            return Err(format!(
                "{}: unsupported version {}, expected 1",
                path.display(),
                cfg.version
            ));
        }
        if cfg
            .hooks
            .iter()
            .any(|hook| hook.label.trim().is_empty() || hook.command.trim().is_empty())
        {
            return Err(format!(
                "{}: hook label and command must not be empty",
                path.display()
            ));
        }
        return Ok(cfg);
    }
    Ok(FileRoot::default())
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

fn declarative_hooks_for_when(
    project_root: &Path,
    cfg: &FileRoot,
    phase: HookPhase,
) -> Vec<StopHook> {
    let mut out = Vec::new();
    for h in &cfg.hooks {
        if !h.enabled {
            continue;
        }
        if h.when != phase {
            continue;
        }
        let label = h.label.trim();
        let command = h.command.trim();
        let wd = resolve_working_dir(project_root, h.working_dir.as_deref());
        out.push(StopHook {
            label: label.to_string(),
            command: command.to_string(),
            working_dir: Some(wd),
            depends_on: Vec::new(),
            timeout_secs: None,

            authoritative: true,
        });
    }
    out
}

fn auto_detect_verify_changes_hook(project_root: &Path) -> Vec<StopHook> {
    let mut tool_hints = Vec::new();
    if project_root.join("Cargo.toml").exists() {
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
	             For Cargo, pass at most one test filter per `cargo test` command; use separate commands for multiple exact tests. \
	             Skip checks unrelated to your changes. \
	             If you only modified files outside the project (e.g. /tmp), skip all project checks."
        ),
        working_dir: Some(project_root.to_string_lossy().to_string()),
        depends_on: Vec::new(),
        timeout_secs: None,
        authoritative: false,
    }]
}

/// Select the declared completion phase and add advisory project checks.
/// Invalid configuration is an admission error, never an empty verification contract.
pub fn detect_turn_stop_hooks(
    project_root: &Path,
    task_profile: TaskExecutionProfile,
    is_plan_subtask: bool,
) -> Result<Vec<StopHook>, String> {
    Ok(load_completion_check_declarations(project_root)?
        .into_selected(is_plan_subtask, task_profile.verification_required))
}

/// Read the workspace configuration once, retaining both completion phases
/// for callers that delegate execution after admitting the initial turn.
pub fn load_completion_check_declarations(
    project_root: &Path,
) -> Result<CompletionCheckDeclarations, String> {
    let cfg = load_declarative_config(project_root)?;
    let mut declarations = CompletionCheckDeclarations {
        stop: declarative_hooks_for_when(project_root, &cfg, HookPhase::Stop),
        task_completed: declarative_hooks_for_when(project_root, &cfg, HookPhase::TaskCompleted),
    };
    if cfg.auto_detect {
        let advisory = auto_detect_verify_changes_hook(project_root);
        for checks in [&mut declarations.stop, &mut declarations.task_completed] {
            for hint in &advisory {
                if !checks.iter().any(|check| check.label == hint.label) {
                    checks.push(hint.clone());
                }
            }
        }
    }
    super::types::validate_completion_check_declarations(&declarations).map_err(|error| {
        format!(
            "{}: {error}",
            project_root.join(".astra/stop-hooks").display()
        )
    })?;
    Ok(declarations)
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
        let s = detect_turn_stop_hooks(dir.path(), prof, false).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label, "test");
        assert_eq!(s[0].command, "cargo test -q");
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
        let declarations = load_completion_check_declarations(dir.path()).unwrap();
        assert_eq!(declarations.stop[0].label, "global");
        assert_eq!(declarations.task_completed[0].label, "sub");
        let wire = serde_json::to_value(&declarations).unwrap();
        let restored: CompletionCheckDeclarations = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored, declarations);
        for field in ["stop", "task_completed"] {
            let mut incomplete = wire.clone();
            incomplete.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<CompletionCheckDeclarations>(incomplete).is_err());
        }
        let s = restored.into_selected(true, prof.verification_required);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label, "sub");
        assert_eq!(
            detect_turn_stop_hooks(dir.path(), prof, false).unwrap(),
            declarations.stop
        );
    }

    #[test]
    fn context_map_detects_plan_subtask_id() {
        let mut m = Map::new();
        m.insert("plan_subtask_id".into(), Value::String("t1".into()));
        assert!(is_plan_subtask_from_context_map(&m));
    }

    // ──────────────────────────────────────────────────────────
    // Context selectors
    // ──────────────────────────────────────────────────────────

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
        std::fs::write(root.path().join("Cargo.toml"), "[package]").unwrap();
        let prof = TaskExecutionProfile::default(); // verification_required = false
        let hooks = detect_turn_stop_hooks(root.path(), prof, false).unwrap();
        assert!(hooks.is_empty());
        let declarations = load_completion_check_declarations(root.path()).unwrap();
        assert!(declarations.clone().into_selected(false, false).is_empty());
        assert_eq!(declarations.clone().into_selected(false, true).len(), 1);
        assert_eq!(declarations.into_selected(true, true).len(), 1);
    }

    #[test]
    fn auto_detect_no_markers() {
        let root = tempdir().unwrap();
        let prof = TaskExecutionProfile {
            verification_required: true,
            ..Default::default()
        };
        let hooks = detect_turn_stop_hooks(root.path(), prof, false).unwrap();
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
        let hooks = detect_turn_stop_hooks(root.path(), prof, false).unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].command.contains("Rust/Cargo"));
        assert!(hooks[0].command.contains("at most one test filter"));
        std::fs::create_dir(root.path().join(".astra")).unwrap();
        std::fs::write(
            root.path().join(".astra/stop-hooks.yaml"),
            "hooks:\n  - label: verify-changes\n    command: make explicit-check\n",
        )
        .unwrap();
        let declarations = load_completion_check_declarations(root.path()).unwrap();
        assert_eq!(declarations.stop.len(), 1);
        assert!(declarations.stop[0].authoritative);
        assert_eq!(declarations.stop[0].command, "make explicit-check");
        assert!(!declarations.task_completed[0].authoritative);
        assert_eq!(
            detect_turn_stop_hooks(root.path(), TaskExecutionProfile::default(), false)
                .unwrap()
                .len(),
            1
        );
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
        let hooks = detect_turn_stop_hooks(root.path(), prof, false).unwrap();
        assert_eq!(hooks.len(), 1);
        assert!(hooks[0].command.contains("Rust/Cargo"));
        assert!(hooks[0].command.contains("Node.js"));
    }

    // ──────────────────────────────────────────────────────────
    // detect_turn_stop_hooks (disabled hooks)
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
        let s = detect_turn_stop_hooks(dir.path(), TaskExecutionProfile::default(), false).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].label, "active-hook");
    }

    #[test]
    fn no_yaml_file_returns_empty() {
        let dir = tempdir().unwrap();
        let s = detect_turn_stop_hooks(dir.path(), TaskExecutionProfile::default(), false).unwrap();
        assert!(s.is_empty());
    }

    #[test]
    fn invalid_declared_hooks_do_not_become_an_empty_contract() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".astra")).unwrap();
        let path = dir.path().join(".astra/stop-hooks.yaml");
        for invalid in [
            "version: 2\nhooks: []",
            "hooks: [",
            "auto_detect: false\nhooks_list: []",
            "hooks:\n  - label: check\n    command: true\n    when: teammate_idle",
            "hooks:\n  - label: check\n    command: true\n    when: unknown_phase",
            "hooks:\n  - label: ''\n    command: true",
            "hooks:\n  - label: check\n    command: ''",
        ] {
            std::fs::write(&path, invalid).unwrap();
            for plan_subtask in [false, true] {
                let error = detect_turn_stop_hooks(
                    dir.path(),
                    TaskExecutionProfile::default(),
                    plan_subtask,
                )
                .expect_err("invalid configuration must reject admission");
                assert!(error.contains("stop-hooks.yaml"), "{error}");
            }
        }
        std::fs::write(
            &path,
            " ".repeat(super::super::types::MAX_COMPLETION_DECLARATION_BYTES + 1),
        )
        .unwrap();
        assert!(
            load_completion_check_declarations(dir.path())
                .unwrap_err()
                .contains("exceeds")
        );
        let aliases = format!(
            "hooks:\n  - &check {{label: check, command: true}}\n{}",
            "  - *check\n".repeat(64)
        );
        std::fs::write(&path, aliases).unwrap();
        assert!(
            load_completion_check_declarations(dir.path())
                .unwrap_err()
                .contains("too many")
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            detect_turn_stop_hooks(dir.path(), TaskExecutionProfile::default(), false).is_err()
        );
        #[cfg(unix)]
        {
            std::fs::remove_dir(&path).unwrap();
            std::os::unix::fs::symlink(dir.path().join("missing-config"), &path).unwrap();
            assert!(
                detect_turn_stop_hooks(dir.path(), TaskExecutionProfile::default(), false).is_err()
            );
            std::fs::remove_file(&path).unwrap();
            let outside = tempdir().unwrap();
            let source = outside.path().join("checks.yaml");
            std::fs::write(&source, "hooks: []").unwrap();
            std::os::unix::fs::symlink(source, &path).unwrap();
            assert!(
                load_completion_check_declarations(dir.path())
                    .unwrap_err()
                    .contains("escapes workspace")
            );
        }
    }
}
