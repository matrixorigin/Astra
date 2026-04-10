//! Passive stdio LSP: **rust-analyzer** and **typescript-language-server** (opt-in).
//!
//! Configuration can come from project-local `astra-lsp.json` and/or env overrides.
//!
//! - Rust env overrides: `ASTRA_LSP_RUST`, `ASTRA_RUST_ANALYZER_CMD`
//! - TS env overrides: `ASTRA_LSP_TYPESCRIPT`, `ASTRA_TYPESCRIPT_SERVER_CMD`
//!
//! Drain order: rust LSP, then TypeScript LSP (before `cargo` / `tsc` in the payload).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::sleep;

use super::lsp_stdio_session::{LanguageIdPolicy, LspSpawnSpec, LspStdioSession, path_to_uri};

pub(crate) const POST_SYNC_DRAIN_MS: u64 = 80;
pub(crate) const ACTIVE_LSP_REQUEST_TIMEOUT_MS: u64 = 3_000;
const ASTRA_LSP_CONFIG_FILE: &str = "astra-lsp.json";

fn normalize_pull_diagnostics(uri: &str, report: Value) -> Option<Value> {
    let items = report.get("items")?.as_array()?.clone();
    let result_id = report
        .get("resultId")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let mut normalized = json!({
        "uri": uri,
        "diagnostics": items,
        "source_method": "textDocument/diagnostic",
        "diagnostic_report": report,
    });
    if let Some(result_id) = result_id
        && let Some(root) = normalized.as_object_mut()
    {
        root.insert("result_id".to_string(), Value::String(result_id));
    }
    Some(normalized)
}

fn attach_publish_snapshot_metadata(mut snapshot: Value, pull_error: Option<String>) -> Value {
    if let Some(root) = snapshot.as_object_mut() {
        root.insert(
            "source_method".to_string(),
            Value::String("publishDiagnostics".to_string()),
        );
        if let Some(error) = pull_error {
            root.insert("pull_diagnostics_error".to_string(), Value::String(error));
        }
    }
    snapshot
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct ProjectLanguageLspConfig {
    enabled: Option<bool>,
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct ProjectLspConfig {
    #[serde(default)]
    rust: ProjectLanguageLspConfig,
    #[serde(default, alias = "ts")]
    typescript: ProjectLanguageLspConfig,
}

#[derive(Clone, Debug)]
struct ResolvedLspConfig {
    enabled: bool,
    command: String,
    args: Vec<String>,
    config_file: Option<String>,
    config_error: Option<String>,
    enabled_source: &'static str,
    command_source: &'static str,
}

fn env_bool(name: &str) -> Option<bool> {
    let value = std::env::var(name).ok()?;
    let value = value.trim().to_lowercase();
    match value.as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn astra_lsp_config_path(project_root: &Path) -> PathBuf {
    project_root.join(ASTRA_LSP_CONFIG_FILE)
}

fn read_project_lsp_config(project_root: &Path) -> Result<Option<ProjectLspConfig>, String> {
    let path = astra_lsp_config_path(project_root);
    if !path.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn resolve_lsp_config(
    project_root: &Path,
    env_enabled_name: &str,
    env_command_name: &str,
    default_command: &str,
    default_args: &[&str],
    select_project: impl FnOnce(&ProjectLspConfig) -> ProjectLanguageLspConfig,
) -> ResolvedLspConfig {
    let config_path = astra_lsp_config_path(project_root);
    let (project_config, config_file, config_error) = match read_project_lsp_config(project_root) {
        Ok(Some(config)) => (
            select_project(&config),
            Some(config_path.display().to_string()),
            None,
        ),
        Ok(None) => (ProjectLanguageLspConfig::default(), None, None),
        Err(error) => (
            ProjectLanguageLspConfig::default(),
            Some(config_path.display().to_string()),
            Some(error),
        ),
    };

    let (enabled, enabled_source) = if let Some(value) = env_bool(env_enabled_name) {
        (value, "env")
    } else if let Some(value) = project_config.enabled {
        (value, "project")
    } else {
        (false, "default")
    };

    let (command, command_source) = if let Some(command) = env_nonempty(env_command_name) {
        (command, "env")
    } else if let Some(command) = project_config
        .command
        .filter(|value| !value.trim().is_empty())
    {
        (command, "project")
    } else {
        (default_command.to_string(), "default")
    };

    let args = if project_config.args.is_empty() {
        default_args.iter().map(|arg| (*arg).to_string()).collect()
    } else {
        project_config.args
    };

    ResolvedLspConfig {
        enabled,
        command,
        args,
        config_file,
        config_error,
        enabled_source,
        command_source,
    }
}

pub(crate) fn lsp_rust_enabled(project_root: &Path) -> bool {
    resolve_lsp_config(
        project_root,
        "ASTRA_LSP_RUST",
        "ASTRA_RUST_ANALYZER_CMD",
        "rust-analyzer",
        &[],
        |config| config.rust.clone(),
    )
    .enabled
}

pub(crate) fn lsp_typescript_enabled(project_root: &Path) -> bool {
    resolve_lsp_config(
        project_root,
        "ASTRA_LSP_TYPESCRIPT",
        "ASTRA_TYPESCRIPT_SERVER_CMD",
        "typescript-language-server",
        &["--stdio"],
        |config| config.typescript.clone(),
    )
    .enabled
}

fn rust_lsp_config(project_root: &Path) -> ResolvedLspConfig {
    resolve_lsp_config(
        project_root,
        "ASTRA_LSP_RUST",
        "ASTRA_RUST_ANALYZER_CMD",
        "rust-analyzer",
        &[],
        |config| config.rust.clone(),
    )
}

fn typescript_lsp_config(project_root: &Path) -> ResolvedLspConfig {
    resolve_lsp_config(
        project_root,
        "ASTRA_LSP_TYPESCRIPT",
        "ASTRA_TYPESCRIPT_SERVER_CMD",
        "typescript-language-server",
        &["--stdio"],
        |config| config.typescript.clone(),
    )
}

fn command_available(command: &str) -> bool {
    let path = Path::new(command);
    if path.is_absolute() || command.contains(std::path::MAIN_SEPARATOR) {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(command);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

fn rust_spawn_spec(project_root: &Path) -> LspSpawnSpec {
    let resolved = rust_lsp_config(project_root);
    let configuration = serde_json::json!({
        "lens": {
            "enable": true,
            "run": { "enable": true },
            "debug": { "enable": true },
            "references": { "enable": true },
            "impls": { "enable": true }
        },
        "runnables": {
            "extraArgs": [],
            "extraEnv": {}
        }
    });
    LspSpawnSpec {
        command: resolved.command,
        args: resolved.args,
        diagnostic_title: "rust-analyzer",
        attachment_source: "rust_analyzer_lsp",
        language_policy: LanguageIdPolicy::Fixed("rust"),
        initialization_options: Some(configuration.clone()),
        configuration_section: Some("rust-analyzer"),
        configuration_value: Some(configuration),
        did_change_configuration: Some(serde_json::json!({
            "rust-analyzer": {
                "lens": {
                    "enable": true,
                    "run": { "enable": true },
                    "debug": { "enable": true },
                    "references": { "enable": true },
                    "impls": { "enable": true }
                },
                "runnables": {
                    "extraArgs": [],
                    "extraEnv": {}
                }
            }
        })),
        experimental_capabilities: Some(serde_json::json!({
            "snippetTextEdit": true,
            "hoverActions": true,
            "commands": {
                "commands": [
                    "rust-analyzer.runSingle",
                    "rust-analyzer.debugSingle",
                    "rust-analyzer.showReferences",
                    "rust-analyzer.gotoLocation"
                ]
            }
        })),
    }
}

fn typescript_spawn_spec(project_root: &Path) -> LspSpawnSpec {
    let resolved = typescript_lsp_config(project_root);
    LspSpawnSpec {
        command: resolved.command,
        args: resolved.args,
        diagnostic_title: "typescript-language-server",
        attachment_source: "typescript_lsp",
        language_policy: LanguageIdPolicy::TypeScript,
        initialization_options: None,
        configuration_section: None,
        configuration_value: None,
        did_change_configuration: None,
        experimental_capabilities: None,
    }
}

#[must_use]
pub(crate) fn should_use_rust_lsp(project_root: &Path, edited: &Path) -> bool {
    lsp_rust_enabled(project_root)
        && edited.extension().and_then(|e| e.to_str()) == Some("rs")
        && project_root.join("Cargo.toml").is_file()
}

#[must_use]
pub(crate) fn should_use_typescript_lsp(project_root: &Path, edited: &Path) -> bool {
    if !lsp_typescript_enabled(project_root) {
        return false;
    }
    let ext = edited.extension().and_then(|e| e.to_str());
    if !matches!(ext, Some("ts" | "tsx")) {
        return false;
    }
    project_root.join("tsconfig.json").is_file() || project_root.join("package.json").is_file()
}

fn ensure_session(
    slot: &Mutex<Option<Arc<LspStdioSession>>>,
    error_slot: &Mutex<Option<String>>,
    root: PathBuf,
    spec: LspSpawnSpec,
) -> Result<Arc<LspStdioSession>, String> {
    let command = spec.command.clone();
    let diagnostic_title = spec.diagnostic_title;
    let mut g = slot
        .lock()
        .map_err(|_| format!("{diagnostic_title} LSP session mutex poisoned"))?;
    if g.is_none() {
        match LspStdioSession::try_spawn(root, spec) {
            Ok(Some(s)) => {
                *g = Some(Arc::clone(&s));
                if let Ok(mut last_error) = error_slot.lock() {
                    *last_error = None;
                }
            }
            Ok(None) => {
                let error = format!(
                    "failed to start {diagnostic_title} LSP session: command `{command}` was not found"
                );
                if let Ok(mut last_error) = error_slot.lock() {
                    *last_error = Some(error.clone());
                }
                return Err(error);
            }
            Err(error) => {
                let error = format!("failed to start {diagnostic_title} LSP session: {error}");
                if let Ok(mut last_error) = error_slot.lock() {
                    *last_error = Some(error.clone());
                }
                return Err(error);
            }
        }
    }
    if let Ok(mut last_error) = error_slot.lock() {
        *last_error = None;
    }
    g.as_ref()
        .map(Arc::clone)
        .ok_or_else(|| format!("{diagnostic_title} LSP session was not created"))
}

fn rust_active_supported(project_root: &Path, path: Option<&Path>) -> bool {
    project_root.join("Cargo.toml").is_file()
        && path
            .map(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
            .unwrap_or(true)
}

fn typescript_active_supported(project_root: &Path, path: Option<&Path>) -> bool {
    let ts_workspace =
        project_root.join("tsconfig.json").is_file() || project_root.join("package.json").is_file();
    if !ts_workspace {
        return false;
    }
    path.map(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("ts" | "tsx")))
        .unwrap_or(true)
}

/// Holds one lazy stdio session per language server kind.
pub(crate) struct PassiveLspManager {
    rust: Mutex<Option<Arc<LspStdioSession>>>,
    typescript: Mutex<Option<Arc<LspStdioSession>>>,
    rust_last_error: Mutex<Option<String>>,
    typescript_last_error: Mutex<Option<String>>,
}

impl PassiveLspManager {
    pub fn new() -> Self {
        Self {
            rust: Mutex::new(None),
            typescript: Mutex::new(None),
            rust_last_error: Mutex::new(None),
            typescript_last_error: Mutex::new(None),
        }
    }

    pub fn sync_after_write(&self, root: &Path, path: &Path) {
        let root_buf = root.to_path_buf();
        if should_use_rust_lsp(root, path)
            && let Ok(s) = ensure_session(
                &self.rust,
                &self.rust_last_error,
                root_buf.clone(),
                rust_spawn_spec(root),
            )
        {
            let _ = s.sync_document_from_disk(path);
        }
        if should_use_typescript_lsp(root, path)
            && let Ok(s) = ensure_session(
                &self.typescript,
                &self.typescript_last_error,
                root_buf,
                typescript_spawn_spec(root),
            )
        {
            let _ = s.sync_document_from_disk(path);
        }
    }

    pub fn sync_after_write_with_content(&self, root: &Path, path: &Path, content: &str) {
        let root_buf = root.to_path_buf();
        if should_use_rust_lsp(root, path)
            && let Ok(s) = ensure_session(
                &self.rust,
                &self.rust_last_error,
                root_buf.clone(),
                rust_spawn_spec(root),
            )
        {
            let _ = s.sync_document_text(path, content);
        }
        if should_use_typescript_lsp(root, path)
            && let Ok(s) = ensure_session(
                &self.typescript,
                &self.typescript_last_error,
                root_buf,
                typescript_spawn_spec(root),
            )
        {
            let _ = s.sync_document_text(path, content);
        }
    }

    pub fn request_for_file(
        &self,
        root: &Path,
        path: &Path,
        method: &str,
        params: Value,
    ) -> Result<Option<Value>, String> {
        let root_buf = root.to_path_buf();
        let session = if rust_active_supported(root, Some(path)) {
            Some(ensure_session(
                &self.rust,
                &self.rust_last_error,
                root_buf.clone(),
                rust_spawn_spec(root),
            )?)
        } else if typescript_active_supported(root, Some(path)) {
            Some(ensure_session(
                &self.typescript,
                &self.typescript_last_error,
                root_buf,
                typescript_spawn_spec(root),
            )?)
        } else {
            None
        };
        let Some(session) = session else {
            return Ok(None);
        };
        session
            .sync_document_from_disk(path)
            .map_err(|e| format!("failed to sync file into LSP: {e}"))?;
        session
            .request(
                method,
                params,
                Duration::from_millis(ACTIVE_LSP_REQUEST_TIMEOUT_MS),
            )
            .map(Some)
            .map_err(|e| format!("LSP request {method} failed: {e}"))
    }

    pub fn request_workspace(
        &self,
        root: &Path,
        method: &str,
        params: Value,
    ) -> Result<Option<Value>, String> {
        let root_buf = root.to_path_buf();
        let session = if rust_active_supported(root, None) {
            Some(ensure_session(
                &self.rust,
                &self.rust_last_error,
                root_buf.clone(),
                rust_spawn_spec(root),
            )?)
        } else if typescript_active_supported(root, None) {
            Some(ensure_session(
                &self.typescript,
                &self.typescript_last_error,
                root_buf,
                typescript_spawn_spec(root),
            )?)
        } else {
            None
        };
        let Some(session) = session else {
            return Ok(None);
        };
        session
            .request(
                method,
                params,
                Duration::from_millis(ACTIVE_LSP_REQUEST_TIMEOUT_MS),
            )
            .map(Some)
            .map_err(|e| format!("LSP request {method} failed: {e}"))
    }

    pub fn diagnostics_for_file(&self, root: &Path, path: &Path) -> Result<Option<Value>, String> {
        let root_buf = root.to_path_buf();
        let session = if rust_active_supported(root, Some(path)) {
            Some(ensure_session(
                &self.rust,
                &self.rust_last_error,
                root_buf.clone(),
                rust_spawn_spec(root),
            )?)
        } else if typescript_active_supported(root, Some(path)) {
            Some(ensure_session(
                &self.typescript,
                &self.typescript_last_error,
                root_buf,
                typescript_spawn_spec(root),
            )?)
        } else {
            None
        };
        let Some(session) = session else {
            return Ok(None);
        };
        session
            .sync_document_from_disk(path)
            .map_err(|e| format!("failed to sync file into LSP: {e}"))?;
        std::thread::sleep(Duration::from_millis(POST_SYNC_DRAIN_MS));
        let pull_result = path_to_uri(path)
            .ok_or_else(|| format!("failed to create file URI for {}", path.display()))
            .and_then(|uri| {
                session
                    .request(
                        "textDocument/diagnostic",
                        json!({
                            "textDocument": { "uri": uri.clone() }
                        }),
                        Duration::from_millis(ACTIVE_LSP_REQUEST_TIMEOUT_MS),
                    )
                    .map(|report| (uri, report))
                    .map_err(|e| format!("LSP request textDocument/diagnostic failed: {e}"))
            });
        let pull_error = match pull_result {
            Ok((uri, report)) => {
                if let Some(normalized) = normalize_pull_diagnostics(&uri, report) {
                    return Ok(Some(normalized));
                }
                Some("textDocument/diagnostic returned no usable diagnostic items".to_string())
            }
            Err(error) => Some(error),
        };
        session
            .latest_diagnostics_for_path(path)
            .map(|snapshot| Some(attach_publish_snapshot_metadata(snapshot, pull_error)))
            .map_err(|e| format!("failed to read LSP diagnostics: {e}"))
    }

    pub fn active_status(&self, root: &Path) -> Value {
        let rust_config = rust_lsp_config(root);
        let rust_enabled = rust_config.enabled;
        let rust_workspace_detected = rust_active_supported(root, None);
        let rust_command = rust_config.command.clone();
        let rust_command_available = command_available(&rust_command);
        let rust_session_started = self
            .rust
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
            .is_some();
        let rust_last_error = self.rust_last_error.lock().ok().and_then(|g| g.clone());
        let rust_session_state = if rust_config.config_error.is_some() {
            "config_error"
        } else if !rust_enabled {
            "disabled"
        } else if !rust_workspace_detected {
            "workspace_not_detected"
        } else if rust_session_started {
            "running"
        } else if !rust_command_available {
            "command_missing"
        } else if rust_last_error.is_some() {
            "error"
        } else {
            "idle"
        };

        let typescript_config = typescript_lsp_config(root);
        let typescript_enabled = typescript_config.enabled;
        let typescript_workspace_detected = typescript_active_supported(root, None);
        let typescript_command = typescript_config.command.clone();
        let typescript_command_available = command_available(&typescript_command);
        let typescript_session_started = self
            .typescript
            .lock()
            .ok()
            .and_then(|g| g.as_ref().cloned())
            .is_some();
        let typescript_last_error = self
            .typescript_last_error
            .lock()
            .ok()
            .and_then(|g| g.clone());
        let typescript_session_state = if typescript_config.config_error.is_some() {
            "config_error"
        } else if !typescript_enabled {
            "disabled"
        } else if !typescript_workspace_detected {
            "workspace_not_detected"
        } else if typescript_session_started {
            "running"
        } else if !typescript_command_available {
            "command_missing"
        } else if typescript_last_error.is_some() {
            "error"
        } else {
            "idle"
        };

        serde_json::json!({
            "rust": {
                "enabled": rust_enabled,
                "workspace_detected": rust_workspace_detected,
                "passive_diagnostics_enabled": rust_enabled,
                "command": rust_command,
                "command_available": rust_command_available,
                "session_started": rust_session_started,
                "session_state": rust_session_state,
                "config_file": rust_config.config_file,
                "config_error": rust_config.config_error,
                "enabled_source": rust_config.enabled_source,
                "command_source": rust_config.command_source,
                "last_start_error": rust_last_error,
            },
            "typescript": {
                "enabled": typescript_enabled,
                "workspace_detected": typescript_workspace_detected,
                "passive_diagnostics_enabled": typescript_enabled,
                "command": typescript_command,
                "command_available": typescript_command_available,
                "session_started": typescript_session_started,
                "session_state": typescript_session_state,
                "config_file": typescript_config.config_file,
                "config_error": typescript_config.config_error,
                "enabled_source": typescript_config.enabled_source,
                "command_source": typescript_config.command_source,
                "last_start_error": typescript_last_error,
            }
        })
    }

    pub async fn take_diagnostic_messages(&self, tool_results_nonempty: bool) -> Vec<Value> {
        if !tool_results_nonempty {
            return Vec::new();
        }
        sleep(Duration::from_millis(POST_SYNC_DRAIN_MS)).await;
        let mut out = Vec::new();
        if let Ok(g) = self.rust.lock()
            && let Some(s) = g.as_ref()
        {
            out.extend(s.take_formatted_diagnostic_messages());
        }
        if let Ok(g) = self.typescript.lock()
            && let Some(s) = g.as_ref()
        {
            out.extend(s.take_formatted_diagnostic_messages());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, old }
        }

        fn unset(key: &'static str) -> Self {
            let old = std::env::var(key).ok();
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.old {
                Some(value) => unsafe {
                    std::env::set_var(self.key, value);
                },
                None => unsafe {
                    std::env::remove_var(self.key);
                },
            }
        }
    }

    #[test]
    fn should_use_rust_requires_env_rs_and_cargo() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        assert!(!should_use_rust_lsp(root, Path::new("src/a.rs")));
        let _g = EnvGuard::set("ASTRA_LSP_RUST", "1");
        assert!(should_use_rust_lsp(root, Path::new("src/a.rs")));
        assert!(!should_use_rust_lsp(root, Path::new("src/a.ts")));
    }

    #[test]
    fn should_use_ts_requires_env_tsconfig_or_package() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!should_use_typescript_lsp(root, Path::new("a.ts")));
        let _g = EnvGuard::set("ASTRA_LSP_TYPESCRIPT", "1");
        std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
        assert!(should_use_typescript_lsp(root, Path::new("a.ts")));
        assert!(should_use_typescript_lsp(root, Path::new("b.tsx")));
        assert!(!should_use_typescript_lsp(root, Path::new("c.js")));
    }

    #[test]
    fn should_use_rust_accepts_project_config_without_env() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _rust_env = EnvGuard::unset("ASTRA_LSP_RUST");
        let _cmd_env = EnvGuard::unset("ASTRA_RUST_ANALYZER_CMD");
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"x\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join(ASTRA_LSP_CONFIG_FILE),
            r#"{"rust":{"enabled":true,"command":"custom-ra","args":["--flag"]}}"#,
        )
        .unwrap();

        assert!(should_use_rust_lsp(root, Path::new("src/a.rs")));
        let resolved = rust_lsp_config(root);
        assert_eq!(resolved.command, "custom-ra");
        assert_eq!(resolved.args, vec!["--flag"]);
        assert_eq!(resolved.enabled_source, "project");
        assert_eq!(resolved.command_source, "project");
    }

    #[test]
    fn active_status_reports_project_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _rust_env = EnvGuard::unset("ASTRA_LSP_RUST");
        let _ts_env = EnvGuard::unset("ASTRA_LSP_TYPESCRIPT");
        std::fs::write(root.join(ASTRA_LSP_CONFIG_FILE), "{ invalid json").unwrap();

        let manager = PassiveLspManager::new();
        let status = manager.active_status(root);
        assert_eq!(
            status["rust"]["session_state"].as_str(),
            Some("config_error")
        );
        assert!(
            status["rust"]["config_error"]
                .as_str()
                .unwrap_or("")
                .contains(ASTRA_LSP_CONFIG_FILE)
        );
    }

    #[tokio::test]
    async fn no_sessions_take_empty() {
        let m = PassiveLspManager::new();
        let v = m.take_diagnostic_messages(true).await;
        assert!(v.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rust_spawn_sync_smoke() {
        let _g = EnvGuard::set("ASTRA_LSP_RUST", "1");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let Ok(status) = Command::new(rust_lsp_config(&root).command)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            return;
        };
        if !status.success() {
            return;
        }
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"ra_smoke\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let sess = match LspStdioSession::try_spawn(root.clone(), rust_spawn_spec(&root)) {
            Ok(Some(s)) => s,
            Ok(None) | Err(_) => return,
        };
        sess.sync_document_from_disk(&root.join("src/lib.rs"))
            .expect("sync");
        sleep(Duration::from_millis(500)).await;
        let msgs = sess.take_formatted_diagnostic_messages();
        for m in msgs {
            assert!(m["content"].as_str().unwrap().contains("rust-analyzer"));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn typescript_server_spawn_sync_smoke() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let cmd = typescript_lsp_config(&root).command;
        let Ok(status) = Command::new(&cmd)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            return;
        };
        if !status.success() {
            return;
        }
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true},"include":["*.ts"]}"#,
        )
        .unwrap();
        std::fs::write(root.join("ok.ts"), "export const x = 1;\n").unwrap();

        let sess = match LspStdioSession::try_spawn(root.clone(), typescript_spawn_spec(&root)) {
            Ok(Some(s)) => s,
            Ok(None) | Err(_) => return,
        };
        sess.sync_document_from_disk(&root.join("ok.ts"))
            .expect("sync");
        sleep(Duration::from_millis(400)).await;
        let msgs = sess.take_formatted_diagnostic_messages();
        for m in msgs {
            assert!(
                m["content"]
                    .as_str()
                    .unwrap()
                    .contains("typescript-language-server")
            );
        }
    }
}
