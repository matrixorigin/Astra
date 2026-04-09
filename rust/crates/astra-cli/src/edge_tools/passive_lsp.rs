//! Passive stdio LSP: **rust-analyzer** and **typescript-language-server** (opt-in).
//!
//! claudecode merges arbitrary servers from **plugins**; here we use env flags until
//! a `astra-lsp.json` config exists.
//!
//! - Rust: `ASTRA_LSP_RUST=1`, `ASTRA_RUST_ANALYZER_CMD` (default `rust-analyzer`)
//! - TS: `ASTRA_LSP_TYPESCRIPT=1`, `ASTRA_TYPESCRIPT_SERVER_CMD` (default `typescript-language-server`)
//!
//! Drain order: rust LSP, then TypeScript LSP (before `cargo` / `tsc` in the payload).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::time::sleep;

use super::lsp_stdio_session::{LanguageIdPolicy, LspSpawnSpec, LspStdioSession};

pub(crate) const POST_SYNC_DRAIN_MS: u64 = 80;
pub(crate) const ACTIVE_LSP_REQUEST_TIMEOUT_MS: u64 = 3_000;

fn env_truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => {
            let v = v.trim().to_lowercase();
            v == "1" || v == "true" || v == "on" || v == "yes"
        }
        Err(_) => false,
    }
}

pub(crate) fn lsp_rust_enabled() -> bool {
    env_truthy("ASTRA_LSP_RUST")
}

pub(crate) fn lsp_typescript_enabled() -> bool {
    env_truthy("ASTRA_LSP_TYPESCRIPT")
}

fn lsp_any_enabled() -> bool {
    lsp_rust_enabled() || lsp_typescript_enabled()
}

fn rust_analyzer_cmd() -> String {
    std::env::var("ASTRA_RUST_ANALYZER_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "rust-analyzer".to_string())
}

fn typescript_server_cmd() -> String {
    std::env::var("ASTRA_TYPESCRIPT_SERVER_CMD")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "typescript-language-server".to_string())
}

fn rust_spawn_spec() -> LspSpawnSpec {
    LspSpawnSpec {
        command: rust_analyzer_cmd(),
        args: Vec::new(),
        diagnostic_title: "rust-analyzer",
        attachment_source: "rust_analyzer_lsp",
        language_policy: LanguageIdPolicy::Fixed("rust"),
    }
}

fn typescript_spawn_spec() -> LspSpawnSpec {
    LspSpawnSpec {
        command: typescript_server_cmd(),
        args: vec!["--stdio".to_string()],
        diagnostic_title: "typescript-language-server",
        attachment_source: "typescript_lsp",
        language_policy: LanguageIdPolicy::TypeScript,
    }
}

#[must_use]
pub(crate) fn should_use_rust_lsp(project_root: &Path, edited: &Path) -> bool {
    lsp_rust_enabled()
        && edited.extension().and_then(|e| e.to_str()) == Some("rs")
        && project_root.join("Cargo.toml").is_file()
}

#[must_use]
pub(crate) fn should_use_typescript_lsp(project_root: &Path, edited: &Path) -> bool {
    if !lsp_typescript_enabled() {
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
    root: PathBuf,
    spec: LspSpawnSpec,
) -> Option<Arc<LspStdioSession>> {
    let mut g = slot.lock().ok()?;
    if g.is_none() {
        match LspStdioSession::try_spawn(root, spec) {
            Ok(Some(s)) => *g = Some(Arc::clone(&s)),
            Ok(None) | Err(_) => return None,
        }
    }
    g.as_ref().map(Arc::clone)
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
}

impl PassiveLspManager {
    pub fn new() -> Self {
        Self {
            rust: Mutex::new(None),
            typescript: Mutex::new(None),
        }
    }

    pub fn sync_after_write(&self, root: &Path, path: &Path) {
        let root_buf = root.to_path_buf();
        if should_use_rust_lsp(root, path)
            && let Some(s) = ensure_session(&self.rust, root_buf.clone(), rust_spawn_spec())
        {
            let _ = s.sync_document_from_disk(path);
        }
        if should_use_typescript_lsp(root, path)
            && let Some(s) = ensure_session(&self.typescript, root_buf, typescript_spawn_spec())
        {
            let _ = s.sync_document_from_disk(path);
        }
    }

    pub fn sync_after_write_with_content(&self, root: &Path, path: &Path, content: &str) {
        let root_buf = root.to_path_buf();
        if should_use_rust_lsp(root, path)
            && let Some(s) = ensure_session(&self.rust, root_buf.clone(), rust_spawn_spec())
        {
            let _ = s.sync_document_text(path, content);
        }
        if should_use_typescript_lsp(root, path)
            && let Some(s) = ensure_session(&self.typescript, root_buf, typescript_spawn_spec())
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
            ensure_session(&self.rust, root_buf.clone(), rust_spawn_spec())
        } else if typescript_active_supported(root, Some(path)) {
            ensure_session(&self.typescript, root_buf, typescript_spawn_spec())
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
            ensure_session(&self.rust, root_buf.clone(), rust_spawn_spec())
        } else if typescript_active_supported(root, None) {
            ensure_session(&self.typescript, root_buf, typescript_spawn_spec())
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
            ensure_session(&self.rust, root_buf.clone(), rust_spawn_spec())
        } else if typescript_active_supported(root, Some(path)) {
            ensure_session(&self.typescript, root_buf, typescript_spawn_spec())
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
        session
            .latest_diagnostics_for_path(path)
            .map(Some)
            .map_err(|e| format!("failed to read LSP diagnostics: {e}"))
    }

    pub fn active_status(&self, root: &Path) -> Value {
        serde_json::json!({
            "rust": {
                "workspace_detected": rust_active_supported(root, None),
                "passive_diagnostics_enabled": lsp_rust_enabled(),
                "command": rust_analyzer_cmd(),
            },
            "typescript": {
                "workspace_detected": typescript_active_supported(root, None),
                "passive_diagnostics_enabled": lsp_typescript_enabled(),
                "command": typescript_server_cmd(),
            }
        })
    }

    pub async fn take_diagnostic_messages(&self, tool_results_nonempty: bool) -> Vec<Value> {
        if !tool_results_nonempty || !lsp_any_enabled() {
            return Vec::new();
        }
        sleep(Duration::from_millis(POST_SYNC_DRAIN_MS)).await;
        let mut out = Vec::new();
        if lsp_rust_enabled()
            && let Ok(g) = self.rust.lock()
            && let Some(s) = g.as_ref()
        {
            out.extend(s.take_formatted_diagnostic_messages());
        }
        if lsp_typescript_enabled()
            && let Ok(g) = self.typescript.lock()
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
        struct SetEnv;
        impl Drop for SetEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("ASTRA_LSP_RUST");
                }
            }
        }
        unsafe {
            std::env::set_var("ASTRA_LSP_RUST", "1");
        }
        let _g = SetEnv;
        assert!(should_use_rust_lsp(root, Path::new("src/a.rs")));
        assert!(!should_use_rust_lsp(root, Path::new("src/a.ts")));
    }

    #[test]
    fn should_use_ts_requires_env_tsconfig_or_package() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!should_use_typescript_lsp(root, Path::new("a.ts")));
        struct SetEnv;
        impl Drop for SetEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("ASTRA_LSP_TYPESCRIPT");
                }
            }
        }
        unsafe {
            std::env::set_var("ASTRA_LSP_TYPESCRIPT", "1");
        }
        let _g = SetEnv;
        std::fs::write(root.join("tsconfig.json"), "{}").unwrap();
        assert!(should_use_typescript_lsp(root, Path::new("a.ts")));
        assert!(should_use_typescript_lsp(root, Path::new("b.tsx")));
        assert!(!should_use_typescript_lsp(root, Path::new("c.js")));
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
        let Ok(status) = Command::new(rust_analyzer_cmd())
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
        struct SetEnv;
        impl Drop for SetEnv {
            fn drop(&mut self) {
                unsafe {
                    std::env::remove_var("ASTRA_LSP_RUST");
                }
            }
        }
        unsafe {
            std::env::set_var("ASTRA_LSP_RUST", "1");
        }
        let _g = SetEnv;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname=\"ra_smoke\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn f() -> i32 { 1 }\n").unwrap();

        let sess = match LspStdioSession::try_spawn(root.clone(), rust_spawn_spec()) {
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
        let cmd = typescript_server_cmd();
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
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true},"include":["*.ts"]}"#,
        )
        .unwrap();
        std::fs::write(root.join("ok.ts"), "export const x = 1;\n").unwrap();

        let sess = match LspStdioSession::try_spawn(root.clone(), typescript_spawn_spec()) {
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
