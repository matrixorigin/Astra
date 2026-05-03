//! CLI bridge — spawn any coding agent CLI per message.
//!
//! Supports multiple CLI backends (astra, claude, codex) via CliProfile.
//! Each profile defines how to construct the command, parse the output,
//! and extract session/text/metadata.

use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum CliProgress {
    Status(String),
    ToolCall(String),
    Stderr(String),
}

#[derive(Debug)]
pub struct CliResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub session_id: Option<String>,
    pub text: Option<String>,
    pub tool_calls_count: Option<u32>,
    pub tokens_prompt: Option<u64>,
    pub tokens_completion: Option<u64>,
}

// ─── CLI Profile ────────────────────────────────────────────────────────────

/// Defines how to invoke a specific CLI agent.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type")]
pub enum CliProfile {
    #[serde(rename = "astra")]
    Astra {
        #[serde(default = "default_astra_bin")]
        bin: String,
        model: Option<String>,
        #[serde(default = "default_permission_mode")]
        permission_mode: String,
    },
    #[serde(rename = "claude")]
    Claude {
        #[serde(default = "default_claude_bin")]
        bin: String,
        model: Option<String>,
    },
    #[serde(rename = "codex")]
    Codex {
        #[serde(default = "default_codex_bin")]
        bin: String,
        #[serde(default = "default_codex_approval")]
        approval_mode: String,
    },
    #[serde(rename = "custom")]
    Custom {
        bin: String,
        #[serde(default)]
        args_template: Vec<String>,
        #[serde(default)]
        json_output: bool,
        session_id_field: Option<String>,
        text_field: Option<String>,
    },
}

fn default_astra_bin() -> String { "astra".into() }
fn default_claude_bin() -> String { "claude".into() }
fn default_codex_bin() -> String { "codex".into() }
fn default_permission_mode() -> String { "auto".into() }
fn default_codex_approval() -> String { "full-auto".into() }

impl Default for CliProfile {
    fn default() -> Self {
        Self::Astra {
            bin: default_astra_bin(),
            model: None,
            permission_mode: default_permission_mode(),
        }
    }
}

/// What this CLI can do — gateway adapts behavior accordingly.
#[derive(Debug, Clone)]
pub struct CliCapabilities {
    pub supports_session: bool,
    pub supports_model_switch: bool,
    pub supports_json_output: bool,
    pub supports_harness: bool,
    pub supports_tools: bool,
}

impl CliProfile {
    pub fn capabilities(&self) -> CliCapabilities {
        match self {
            Self::Astra { .. } => CliCapabilities {
                supports_session: true,
                supports_model_switch: true,
                supports_json_output: true,
                supports_harness: true,
                supports_tools: true,
            },
            Self::Claude { .. } => CliCapabilities {
                supports_session: true,
                supports_model_switch: true,
                supports_json_output: true,
                supports_harness: false,
                supports_tools: true,
            },
            Self::Codex { .. } => CliCapabilities {
                supports_session: false,
                supports_model_switch: false,
                supports_json_output: true,
                supports_harness: false,
                supports_tools: true,
            },
            Self::Custom { json_output, .. } => CliCapabilities {
                supports_session: false,
                supports_model_switch: false,
                supports_json_output: *json_output,
                supports_harness: false,
                supports_tools: false,
            },
        }
    }

    /// Build the command to execute for a given message.
    pub fn build_command(
        &self,
        message: &str,
        session_id: Option<&str>,
        working_dir: Option<&std::path::Path>,
    ) -> Command {
        self.build_command_with_context(message, session_id, working_dir, None)
    }

    /// Build the command with optional gateway context injected as system prompt.
    pub fn build_command_with_context(
        &self,
        message: &str,
        session_id: Option<&str>,
        working_dir: Option<&std::path::Path>,
        system_prompt: Option<&str>,
    ) -> Command {
        match self {
            Self::Astra {
                bin,
                model,
                permission_mode,
            } => {
                let mut cmd = Command::new(bin);
                // Ensure astra CLI connects directly to local server, not via HTTP proxy
                cmd.env("no_proxy", "127.0.0.1,localhost");
                cmd.arg("chat")
                    .arg("-m")
                    .arg(message)
                    .arg("--json")
                    .arg("--quiet")
                    .arg("--permission-mode")
                    .arg(permission_mode);
                if let Some(sid) = session_id {
                    cmd.arg("--session-id").arg(sid);
                }
                if let Some(m) = model {
                    cmd.arg("--model").arg(m);
                }
                if let Some(sp) = system_prompt {
                    cmd.arg("--append-system-prompt").arg(sp);
                }
                if let Some(dir) = working_dir {
                    cmd.current_dir(dir);
                }
                cmd
            }
            Self::Claude { bin, model } => {
                let mut cmd = Command::new(bin);
                cmd.arg("-p")
                    .arg(message)
                    .arg("--output-format")
                    .arg("json")
                    .arg("--dangerously-skip-permissions");
                if let Some(sid) = session_id {
                    cmd.arg("--resume").arg(sid);
                }
                if let Some(m) = model {
                    cmd.arg("--model").arg(m);
                }
                if let Some(sp) = system_prompt {
                    cmd.arg("--append-system-prompt").arg(sp);
                }
                if let Some(dir) = working_dir {
                    cmd.current_dir(dir);
                }
                cmd
            }
            Self::Codex {
                bin,
                approval_mode,
            } => {
                let mut cmd = Command::new(bin);
                cmd.arg(message)
                    .arg(format!("--{approval_mode}"))
                    .arg("--json");
                if let Some(dir) = working_dir {
                    cmd.current_dir(dir);
                }
                cmd
            }
            Self::Custom {
                bin,
                args_template,
                ..
            } => {
                let mut cmd = Command::new(bin);
                for arg in args_template {
                    let replaced = arg
                        .replace("{message}", message)
                        .replace("{session_id}", session_id.unwrap_or(""));
                    cmd.arg(replaced);
                }
                if let Some(dir) = working_dir {
                    cmd.current_dir(dir);
                }
                cmd
            }
        }
    }

    /// Parse stdout into structured result.
    pub fn parse_output(&self, stdout: &str, exit_code: i32) -> CliResult {
        match self {
            Self::Astra { .. } => parse_astra_json(stdout, exit_code),
            Self::Claude { .. } => parse_claude_json(stdout, exit_code),
            Self::Codex { .. } => parse_generic_json(stdout, exit_code, "result", "session_id"),
            Self::Custom {
                json_output,
                text_field,
                session_id_field,
                ..
            } => {
                if *json_output {
                    parse_generic_json(
                        stdout,
                        exit_code,
                        text_field.as_deref().unwrap_or("text"),
                        session_id_field.as_deref().unwrap_or("session_id"),
                    )
                } else {
                    CliResult {
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code,
                        session_id: None,
                        text: Some(stdout.to_string()),
                        tool_calls_count: None,
                        tokens_prompt: None,
                        tokens_completion: None,
                    }
                }
            }
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Astra { .. } => "astra",
            Self::Claude { .. } => "claude",
            Self::Codex { .. } => "codex",
            Self::Custom { bin, .. } => bin,
        }
    }
}

// ─── JSON parsers ───────────────────────────────────────────────────────────

fn parse_astra_json(stdout: &str, exit_code: i32) -> CliResult {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        CliResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: v["exit_code"].as_i64().unwrap_or(exit_code as i64) as i32,
            session_id: v["session_id"].as_str().map(String::from),
            text: v["text"].as_str().map(String::from),
            tool_calls_count: v["tool_calls_count"].as_u64().map(|n| n as u32),
            tokens_prompt: v["prompt_tokens"].as_u64(),
            tokens_completion: v["completion_tokens"].as_u64(),
        }
    } else {
        plain_result(stdout, exit_code)
    }
}

fn parse_claude_json(stdout: &str, exit_code: i32) -> CliResult {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        let usage = &v["usage"];
        CliResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code,
            session_id: v["session_id"].as_str().map(String::from),
            text: v["result"].as_str().map(String::from),
            tool_calls_count: v["num_turns"].as_u64().map(|n| n as u32),
            tokens_prompt: usage["input_tokens"]
                .as_u64()
                .or_else(|| usage["cache_creation_input_tokens"].as_u64()),
            tokens_completion: usage["output_tokens"].as_u64(),
        }
    } else {
        plain_result(stdout, exit_code)
    }
}

fn parse_generic_json(
    stdout: &str,
    exit_code: i32,
    text_field: &str,
    session_field: &str,
) -> CliResult {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(stdout) {
        CliResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code,
            session_id: v[session_field].as_str().map(String::from),
            text: v[text_field].as_str().map(String::from),
            tool_calls_count: None,
            tokens_prompt: None,
            tokens_completion: None,
        }
    } else {
        plain_result(stdout, exit_code)
    }
}

fn plain_result(stdout: &str, exit_code: i32) -> CliResult {
    CliResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code,
        session_id: None,
        text: if stdout.trim().is_empty() {
            None
        } else {
            Some(stdout.trim().to_string())
        },
        tool_calls_count: None,
        tokens_prompt: None,
        tokens_completion: None,
    }
}

// ─── Run ────────────────────────────────────────────────────────────────────

pub async fn run_cli(
    profile: &CliProfile,
    message: &str,
    session_id: Option<&str>,
    working_dir: Option<&std::path::Path>,
    progress_tx: Option<mpsc::Sender<CliProgress>>,
) -> Result<CliResult, String> {
    run_cli_with_context(profile, message, session_id, working_dir, progress_tx, None).await
}

pub async fn run_cli_with_context(
    profile: &CliProfile,
    message: &str,
    session_id: Option<&str>,
    working_dir: Option<&std::path::Path>,
    progress_tx: Option<mpsc::Sender<CliProgress>>,
    system_prompt: Option<&str>,
) -> Result<CliResult, String> {
    let mut cmd = profile.build_command_with_context(message, session_id, working_dir, system_prompt);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| format!("failed to spawn {}: {e}", profile.name()))?;

    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;

    let progress_tx_clone = progress_tx;
    let stderr_task = tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();
        let mut collected = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            if !collected.is_empty() {
                collected.push('\n');
            }
            collected.push_str(&line);
            let event = if line.contains('⚡') || line.contains("tool") {
                CliProgress::ToolCall(line)
            } else {
                CliProgress::Status(line)
            };
            if let Some(ref tx) = progress_tx_clone {
                let _ = tx.send(event).await;
            }
        }
        collected
    });

    let stdout_task = tokio::spawn(async move {
        let reader = BufReader::new(stdout);
        let mut lines = reader.lines();
        let mut output = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&line);
        }
        output
    });

    let status = child.wait().await.map_err(|e| format!("wait failed: {e}"))?;
    let stderr_text = stderr_task.await.unwrap_or_default();
    let stdout_text = stdout_task.await.unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    let mut result = profile.parse_output(&stdout_text, exit_code);
    result.stdout = stdout_text;
    result.stderr = stderr_text;
    Ok(result)
}

// ─── CLI Availability ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliAvailability {
    Available { version: Option<String> },
    NotInstalled,
    NotExecutable(String),
}

impl CliAvailability {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available { .. })
    }
}

use std::sync::LazyLock;
use std::time::Instant;

static CLI_PROBE_CACHE: LazyLock<dashmap::DashMap<String, (CliAvailability, Instant)>> =
    LazyLock::new(dashmap::DashMap::new);

const PROBE_CACHE_TTL_SECS: u64 = 300;

pub async fn probe_cli(profile: &CliProfile) -> CliAvailability {
    let cache_key = profile.name().to_string();
    if let Some(entry) = CLI_PROBE_CACHE.get(&cache_key) {
        let (ref avail, created) = *entry;
        if created.elapsed().as_secs() < PROBE_CACHE_TTL_SECS {
            return avail.clone();
        }
    }
    let result = probe_cli_uncached(profile).await;
    CLI_PROBE_CACHE.insert(cache_key, (result.clone(), Instant::now()));
    result
}

async fn probe_cli_uncached(profile: &CliProfile) -> CliAvailability {
    let bin = match profile {
        CliProfile::Astra { bin, .. } => bin.as_str(),
        CliProfile::Claude { bin, .. } => bin.as_str(),
        CliProfile::Codex { bin, .. } => bin.as_str(),
        CliProfile::Custom { bin, .. } => bin.as_str(),
    };

    let version_arg = match profile {
        CliProfile::Astra { .. } => "--version",
        CliProfile::Claude { .. } => "--version",
        CliProfile::Codex { .. } => "--version",
        CliProfile::Custom { .. } => "--version",
    };

    match tokio::process::Command::new(bin)
        .arg(version_arg)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CliAvailability::NotInstalled,
        Err(e) => CliAvailability::NotExecutable(e.to_string()),
        Ok(child) => match child.wait_with_output().await {
            Ok(output) => {
                let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                CliAvailability::Available {
                    version: if version.is_empty() { None } else { Some(version) },
                }
            }
            Err(e) => CliAvailability::NotExecutable(e.to_string()),
        },
    }
}

pub fn onboarding_message(profile: &CliProfile, availability: &CliAvailability) -> String {
    let name = profile.name();
    match availability {
        CliAvailability::Available { .. } => String::new(),
        CliAvailability::NotInstalled => format!(
            "⚠️ CLI `{name}` 未安装\n\n\
             请先安装对应的 CLI 工具:\n\
             - **astra**: `cargo install astra-cli`\n\
             - **claude**: `npm install -g @anthropic-ai/claude-code`\n\
             - **codex**: `npm install -g @openai/codex`\n\n\
             安装完成后发送任意消息即可开始对话。\n\
             或使用 `/cli` 切换到其他已安装的 CLI。"
        ),
        CliAvailability::NotExecutable(err) => format!(
            "⚠️ CLI `{name}` 无法执行: {err}\n\n\
             请检查文件权限或 PATH 配置。\n\
             使用 `/cli` 查看可用的 CLI 选项。"
        ),
    }
}

pub fn translate_cli_error(profile: &CliProfile, exit_code: i32, stderr: &str) -> String {
    let name = profile.name();
    if stderr.contains("could not validate credentials")
        || stderr.contains("Could not validate credentials")
        || stderr.contains("invalid_api_key")
        || stderr.contains("401")
    {
        return format!(
            "🔑 `{name}` API 密钥无效或过期\n\n\
             请检查对应的环境变量或配置:\n\
             - **astra**: ASTRA_API_KEY\n\
             - **claude**: ANTHROPIC_API_KEY\n\
             - **codex**: OPENAI_API_KEY"
        );
    }
    if stderr.contains("rate limit") || stderr.contains("429") {
        return format!("⏳ `{name}` 请求过于频繁，请稍后再试。");
    }
    if stderr.contains("timeout") || stderr.contains("timed out") {
        return format!("⏰ `{name}` 响应超时，请重试。");
    }
    format!("⚠️ `{name}` 执行失败 (exit={exit_code})")
}

// Legacy compat
pub type CliBridgeConfig = CliProfile;

pub async fn run_astra_chat(
    config: &CliBridgeConfig,
    message: &str,
    session_id: Option<&str>,
    progress_tx: Option<mpsc::Sender<CliProgress>>,
) -> Result<CliResult, String> {
    run_cli(config, message, session_id, None, progress_tx).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_is_astra() {
        let p = CliProfile::default();
        assert_eq!(p.name(), "astra");
    }

    // ── Astra JSON parsing ──────────────────────────────────────────

    #[test]
    fn parse_astra_valid() {
        let r = parse_astra_json(
            r#"{"session_id":"ses-1","text":"Hello","tool_calls_count":2,"prompt_tokens":100,"completion_tokens":50}"#,
            0,
        );
        assert_eq!(r.session_id.as_deref(), Some("ses-1"));
        assert_eq!(r.text.as_deref(), Some("Hello"));
        assert_eq!(r.tool_calls_count, Some(2));
    }

    #[test]
    fn parse_astra_real_output() {
        let json = r#"{
            "session_id": "a0fc41a0-3176-480d-99fd-d52007cdb2ce",
            "text": "\nHello! 👋",
            "tool_calls_count": 0,
            "prompt_tokens": 7367,
            "completion_tokens": 44,
            "exit_code": 0,
            "success": true
        }"#;
        let r = parse_astra_json(json, 0);
        assert!(r.text.as_ref().unwrap().contains("Hello!"));
        assert_eq!(r.tokens_prompt, Some(7367));
    }

    #[test]
    fn parse_astra_malformed() {
        let r = parse_astra_json("not json", 1);
        assert!(r.text.is_some()); // plain text fallback
        assert_eq!(r.text.as_deref(), Some("not json"));
    }

    // ── Claude JSON parsing ─────────────────────────────────────────

    #[test]
    fn parse_claude_real_output() {
        let json = r#"{
            "type": "result",
            "subtype": "success",
            "result": "Hello!",
            "session_id": "28246761-888a-4d37-a694-c740f843f49d",
            "num_turns": 1,
            "duration_ms": 3322,
            "total_cost_usd": 0.15,
            "usage": {
                "input_tokens": 3,
                "cache_creation_input_tokens": 24984,
                "output_tokens": 5
            }
        }"#;
        let r = parse_claude_json(json, 0);
        assert_eq!(r.text.as_deref(), Some("Hello!"));
        assert_eq!(r.session_id.as_deref(), Some("28246761-888a-4d37-a694-c740f843f49d"));
        assert_eq!(r.tool_calls_count, Some(1));
        assert_eq!(r.tokens_prompt, Some(3));
        assert_eq!(r.tokens_completion, Some(5));
    }

    #[test]
    fn parse_claude_plain_text() {
        let r = parse_claude_json("Just plain text output", 0);
        assert_eq!(r.text.as_deref(), Some("Just plain text output"));
    }

    // ── Generic JSON parsing ────────────────────────────────────────

    #[test]
    fn parse_generic() {
        let r = parse_generic_json(
            r#"{"result":"ok","session_id":"s2"}"#,
            0,
            "result",
            "session_id",
        );
        assert_eq!(r.text.as_deref(), Some("ok"));
        assert_eq!(r.session_id.as_deref(), Some("s2"));
    }

    // ── Command building ────────────────────────────────────────────

    #[test]
    fn build_astra_command() {
        let p = CliProfile::Astra {
            bin: "astra".into(),
            model: Some("MiniMax-M2.7".into()),
            permission_mode: "auto".into(),
        };
        let cmd = p.build_command("hello", Some("ses-1"), None);
        let prog = cmd.as_std().get_program().to_str().unwrap();
        assert_eq!(prog, "astra");
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("--json")));
        assert!(args.contains(&std::ffi::OsStr::new("--session-id")));
        assert!(args.contains(&std::ffi::OsStr::new("MiniMax-M2.7")));
    }

    #[test]
    fn build_claude_command() {
        let p = CliProfile::Claude {
            bin: "claude".into(),
            model: Some("claude-sonnet-4-6".into()),
        };
        let cmd = p.build_command("fix bug", None, None);
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("-p")));
        assert!(args.contains(&std::ffi::OsStr::new("fix bug")));
        assert!(args.contains(&std::ffi::OsStr::new("--output-format")));
    }

    #[test]
    fn claude_uses_append_system_prompt() {
        let p = CliProfile::Claude {
            bin: "claude".into(),
            model: None,
        };
        let cmd = p.build_command_with_context("hi", None, None, Some("gateway context"));
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("--append-system-prompt")));
        assert!(args.contains(&std::ffi::OsStr::new("gateway context")));
        assert!(!args.contains(&std::ffi::OsStr::new("--system-prompt")));
    }

    #[test]
    fn astra_uses_append_system_prompt() {
        let p = CliProfile::Astra {
            bin: "astra".into(),
            model: None,
            permission_mode: "auto".into(),
        };
        let cmd = p.build_command_with_context("hi", None, None, Some("gateway context"));
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("--append-system-prompt")));
        assert!(args.contains(&std::ffi::OsStr::new("gateway context")));
    }

    #[test]
    fn build_custom_command() {
        let p = CliProfile::Custom {
            bin: "my-agent".into(),
            args_template: vec!["--msg".into(), "{message}".into(), "--sid".into(), "{session_id}".into()],
            json_output: true,
            text_field: Some("output".into()),
            session_id_field: Some("id".into()),
        };
        let cmd = p.build_command("hello", Some("s1"), None);
        let args: Vec<_> = cmd.as_std().get_args().collect();
        assert!(args.contains(&std::ffi::OsStr::new("hello")));
        assert!(args.contains(&std::ffi::OsStr::new("s1")));
    }

    // ── Profile deserialization ──────────────────────────────────────

    #[test]
    fn deserialize_astra_profile() {
        let yaml = r#"type: astra
bin: /usr/local/bin/astra
model: MiniMax-M2.7"#;
        let p: CliProfile = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(p.name(), "astra");
    }

    #[test]
    fn deserialize_claude_profile() {
        let yaml = r#"type: claude
model: claude-sonnet-4-6"#;
        let p: CliProfile = serde_yaml_ng::from_str(yaml).unwrap();
        assert_eq!(p.name(), "claude");
    }

    // ── Run tests ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_echo() {
        let p = CliProfile::Custom {
            bin: "echo".into(),
            args_template: vec!["hello".into()],
            json_output: false,
            text_field: None,
            session_id_field: None,
        };
        let r = run_cli(&p, "ignored", None, None, None).await.unwrap();
        assert_eq!(r.exit_code, 0);
        assert!(r.text.as_ref().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn run_captures_stderr() {
        // sh -c writes to stderr and exits non-zero
        let p = CliProfile::Custom {
            bin: "sh".into(),
            args_template: vec!["-c".into(), "echo errmsg >&2; exit 3".into()],
            json_output: false,
            text_field: None,
            session_id_field: None,
        };
        let r = run_cli(&p, "ignored", None, None, None).await.unwrap();
        assert_eq!(r.exit_code, 3);
        assert!(r.stderr.contains("errmsg"), "stderr={}", r.stderr);
    }

    #[tokio::test]
    async fn run_nonexistent() {
        let p = CliProfile::Custom {
            bin: "/nonexistent/xyz".into(),
            args_template: vec![],
            json_output: false,
            text_field: None,
            session_id_field: None,
        };
        let r = run_cli(&p, "test", None, None, None).await;
        assert!(r.is_err());
    }

    // ── CLI availability ──────────────────────────────────────────

    #[tokio::test]
    async fn probe_existing_binary() {
        let p = CliProfile::Custom {
            bin: "echo".into(),
            args_template: vec![],
            json_output: false,
            text_field: None,
            session_id_field: None,
        };
        let avail = probe_cli(&p).await;
        assert!(avail.is_available());
    }

    #[tokio::test]
    async fn probe_nonexistent_binary() {
        let p = CliProfile::Custom {
            bin: "/nonexistent/no-such-cli-12345".into(),
            args_template: vec![],
            json_output: false,
            text_field: None,
            session_id_field: None,
        };
        let avail = probe_cli(&p).await;
        assert_eq!(avail, CliAvailability::NotInstalled);
    }

    #[test]
    fn onboarding_not_installed() {
        let p = CliProfile::default();
        let msg = onboarding_message(&p, &CliAvailability::NotInstalled);
        assert!(msg.contains("未安装"));
        assert!(msg.contains("/cli"));
    }

    #[test]
    fn onboarding_available_is_empty() {
        let p = CliProfile::default();
        let msg = onboarding_message(&p, &CliAvailability::Available { version: Some("1.0".into()) });
        assert!(msg.is_empty());
    }

    #[test]
    fn translate_auth_error() {
        let p = CliProfile::default();
        let msg = translate_cli_error(&p, 1, "Error: Could not validate credentials");
        assert!(msg.contains("API 密钥"));
    }

    #[test]
    fn translate_rate_limit() {
        let p = CliProfile::default();
        let msg = translate_cli_error(&p, 1, "429 rate limit exceeded");
        assert!(msg.contains("频繁"));
    }

    #[test]
    fn translate_generic_error() {
        let p = CliProfile::default();
        let msg = translate_cli_error(&p, 42, "some random error");
        assert!(msg.contains("exit=42"));
    }

    #[tokio::test]
    async fn progress_channel_closed_after_exit() {
        let p = CliProfile::Custom {
            bin: "echo".into(),
            args_template: vec!["done".into()],
            json_output: false,
            text_field: None,
            session_id_field: None,
        };
        let (tx, mut rx) = mpsc::channel(64);
        let _ = run_cli(&p, "", None, None, Some(tx)).await;
        while rx.try_recv().is_ok() {}
        assert!(rx.recv().await.is_none());
    }
}
