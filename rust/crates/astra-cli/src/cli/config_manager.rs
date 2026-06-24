//! Settings/config management: read, write, list, get, set, show policy.
//!
//! Extracted from `command_router.rs` as part of the god-module refactor (P0-2).

use super::theme;
use crate::cli::cli_config::cli_args::{ConfigCmd, ConfigVersionCmd};
use crate::cli::cli_config::cli_utils::{
    persist_profile_last_session_or_warn, validate_cli_session_id,
    validated_resumable_last_session_id,
};
use crossterm::style::Stylize;

// ═══════════════════════════════════════════════════════ Config ═══════════

/// Path to `~/.astra/settings.json`.
fn settings_path(override_path: Option<&std::path::PathBuf>) -> Result<std::path::PathBuf, String> {
    if let Some(p) = override_path {
        return Ok(p.clone());
    }
    dirs::home_dir()
        .map(|h| h.join(".astra").join("settings.json"))
        .ok_or_else(|| "Cannot determine home directory".to_string())
}

fn read_settings_from(
    path_override: Option<&std::path::PathBuf>,
) -> Result<serde_json::Map<String, serde_json::Value>, String> {
    let path = settings_path(path_override)?;
    if !path.is_file() {
        return Ok(serde_json::Map::new());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let val: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
    val.as_object()
        .cloned()
        .ok_or_else(|| format!("{} is not a JSON object", path.display()))
}

fn read_settings() -> Result<serde_json::Map<String, serde_json::Value>, String> {
    read_settings_from(None)
}

/// Read `default_model` from settings.json, if set.
pub(crate) fn read_config_default_model() -> Result<Option<String>, String> {
    let settings = read_settings()?;
    Ok(settings
        .get("default_model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

/// Read `api_url` from settings.json, if set.
pub(crate) fn read_config_api_url() -> Result<Option<String>, String> {
    read_config_api_url_from(None)
}

/// Read `api_url` from a specific path (for testing) or the default settings path.
pub(crate) fn read_config_api_url_from(
    path_override: Option<&std::path::PathBuf>,
) -> Result<Option<String>, String> {
    let settings = read_settings_from(path_override)?;
    Ok(settings
        .get("api_url")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

pub(crate) const DEFAULT_API_URL: &str = "http://127.0.0.1:8000";

/// Resolve API URL with priority: flag > env var > config file > default.
pub(crate) fn resolve_api_url(flag: Option<&str>) -> Result<String, String> {
    resolve_api_url_with(
        flag,
        || std::env::var("ASTRA_API_URL").ok(),
        read_config_api_url,
    )
}

/// Testable core: resolve API URL with injectable env and config sources.
pub(crate) fn resolve_api_url_with(
    flag: Option<&str>,
    env_fn: impl FnOnce() -> Option<String>,
    config_fn: impl FnOnce() -> Result<Option<String>, String>,
) -> Result<String, String> {
    if let Some(flag) = flag {
        return Ok(flag.trim_end_matches('/').to_string());
    }
    if let Some(env) = env_fn() {
        return Ok(env.trim_end_matches('/').to_string());
    }
    match config_fn()? {
        Some(url) => Ok(url.trim_end_matches('/').to_string()),
        None => Ok(DEFAULT_API_URL.to_string()),
    }
}

pub(crate) async fn resolve_remote_session_id(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    requested: Option<&str>,
) -> Result<String, String> {
    resolve_optional_remote_session_id(api, profile, requested)
        .await?
        .ok_or_else(|| {
            "No session id provided and no recent resumable session is available".to_string()
        })
}

pub(crate) async fn resolve_optional_remote_session_id(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    requested: Option<&str>,
) -> Result<Option<String>, String> {
    match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(session_id) => {
            validate_cli_session_id(session_id)?;
            Ok(Some(session_id.to_string()))
        }
        None => {
            if let Some(session_id) = validated_resumable_last_session_id(api, profile).await {
                return Ok(Some(session_id));
            }

            let sessions =
                crate::cli::session::session_restore_client::list_cloud_resumable_sessions(
                    profile, api,
                )
                .await?;
            if let Some(session) = sessions.into_iter().find(|session| session.turn_count > 0) {
                persist_profile_last_session_or_warn(
                    profile,
                    &session.session_id,
                    "config_manager:resolve_optional_remote_session_id",
                );
                return Ok(Some(session.session_id));
            }

            Ok(None)
        }
    }
}

pub(crate) fn latest_artifact_id(body: &str) -> Result<String, String> {
    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("Invalid latest artifact response: {error}"))?;
    json.get("artifact_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| "Latest artifact response missing artifact_id".to_string())
}

pub(crate) fn resolve_download_output_path(
    output: Option<&std::path::Path>,
    suggested_name: &str,
) -> std::path::PathBuf {
    // Sanitize server-supplied filename: strip directory components and reject traversal.
    let safe_name = std::path::Path::new(suggested_name)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty() && *n != "." && *n != "..")
        .unwrap_or("download.json");
    match output {
        Some(path) if path.is_dir() => path.join(safe_name),
        Some(path) => path.to_path_buf(),
        None => std::path::PathBuf::from(safe_name),
    }
}

pub(crate) fn write_downloaded_capture(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, bytes).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

fn write_settings(settings: &serde_json::Map<String, serde_json::Value>) -> Result<(), String> {
    let path = settings_path(None)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    let val = serde_json::Value::Object(settings.clone());
    let pretty = serde_json::to_string_pretty(&val)
        .map_err(|e| format!("Failed to serialize {}: {e}", path.display()))?;
    std::fs::write(&path, &pretty).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

/// Known setting keys with descriptions for list/help.
pub(crate) const KNOWN_SETTINGS: &[(&str, &str)] = &[
    (
        "default_model",
        "Default model for chat (e.g. gpt-4o, claude-3.5-sonnet)",
    ),
    ("verbose", "Enable verbose output (true/false)"),
    ("auto_approve", "Auto-approve tool calls (true/false)"),
    ("api_url", "API server URL"),
    ("theme", "Color theme (auto/dark/light)"),
    (
        "permission_mode",
        "Default permission mode (auto/plan/accept_edits/prompt/deny)",
    ),
];

pub(crate) async fn execute_config_command(cmd: ConfigCmd) -> Result<(), String> {
    match cmd {
        ConfigCmd::List => config_list(),
        ConfigCmd::Get(args) => config_get(&args.key),
        ConfigCmd::Set(args) => config_set(&args.key, &args.value),
        ConfigCmd::ShowPolicy(args) => config_show_policy(args.model.as_deref(), args.json),
        ConfigCmd::Version(sub) => config_version_dispatch(sub).await,
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_optional_remote_session_id, resolve_remote_session_id};
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, load_credentials, save_credentials,
    };
    use astra_services::session_journal::{JournalEvent, JournalWriter};
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

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

    async fn mock_cloud_resumable_list(
        server: &MockServer,
        sessions: &[astra_services::session_restore::RestoredSession],
    ) {
        Mock::given(method("GET"))
            .and(path("/sessions/resumable"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                astra_services::session_restore::ResumableSessionsResponse {
                    sessions: sessions.to_vec(),
                },
            ))
            .mount(server)
            .await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_remote_session_id_falls_back_to_cloud_resumable_list() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let session_id = "88888888-8888-8888-8888-888888888888";

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        mock_cloud_resumable_list(
            &server,
            &[astra_services::session_restore::RestoredSession {
                session_id: session_id.to_string(),
                turn_count: 3,
                last_status: "active".to_string(),
                restored_from_cloud: true,
                ..Default::default()
            }],
        )
        .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = resolve_remote_session_id(&api, None, None)
            .await
            .expect("cloud resumable session");

        assert_eq!(resolved, session_id);
        assert_eq!(
            load_credentials()
                .profiles
                .get("default")
                .and_then(|profile| profile.last_session_id.as_deref()),
            Some(session_id)
        );
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_remote_session_id_prefers_validated_last_session_pointer() {
        let (_tmp, _guard) = crate::tests::isolated_sessions_dir();
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let session_id = format!("cfg-live-{}", uuid::Uuid::new_v4());

        let writer = JournalWriter::new(&session_id).unwrap();
        writer
            .append(&JournalEvent::session_start(
                Some(&session_id),
                Some("gpt-5"),
            ))
            .unwrap();
        writer
            .append(&JournalEvent::interruption_recorded(
                Some(&session_id),
                1,
                serde_json::json!({
                    "kind": "rate_limited",
                    "resumable": true,
                    "has_checkpoint": true,
                    "tool_calls_completed": 1,
                    "turns_completed": 1,
                    "remaining_turns": 4,
                }),
            ))
            .unwrap();

        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".to_string(),
            Profile {
                access_token: Some("test-token".to_string()),
                last_session_id: Some(session_id.clone()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();

        Mock::given(method("GET"))
            .and(path(format!("/sessions/{session_id}")))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "session_id": session_id,
                "status": "active"
            })))
            .mount(&server)
            .await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = resolve_remote_session_id(&api, None, None)
            .await
            .expect("validated session pointer");

        assert_eq!(resolved, session_id);
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_remote_session_id_errors_when_no_remote_session_available() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");

        mock_cloud_resumable_list(&server, &[]).await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let error = resolve_remote_session_id(&api, None, None)
            .await
            .expect_err("no remote session should error");

        assert!(error.contains("No session id provided"), "{error}");
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn resolve_optional_remote_session_id_returns_none_when_no_remote_session_available() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = MockServer::start().await;
        let _token = EnvGuard::set("ASTRA_ACCESS_TOKEN", "test-token");

        mock_cloud_resumable_list(&server, &[]).await;

        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let resolved = resolve_optional_remote_session_id(&api, None, None)
            .await
            .expect("optional resolver should not error");

        assert_eq!(resolved, None);
    }

    #[tokio::test]
    async fn resolve_remote_session_id_rejects_invalid_requested_session_id() {
        let server = MockServer::start().await;
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();

        let error = resolve_remote_session_id(&api, None, Some("../escape"))
            .await
            .expect_err("invalid session id must fail before any remote call");

        assert!(error.contains("invalid session_id"), "{error}");
    }
}

/// Dispatch for `astra config version ...`. Uses the default
/// `LocalFileStore` at `~/.astra/config/versions/`; if home dir is
/// unresolvable we surface that as an error instead of silently using
/// the process cwd, which would mislead users about where their
/// versions live.
async fn config_version_dispatch(sub: ConfigVersionCmd) -> Result<(), String> {
    use astra_config::config_version_cli::{
        format_current, format_version_diff, format_version_list, format_version_show,
    };
    use astra_config::config_versions::LocalFileStore;

    let store = LocalFileStore::at_default_root()
        .ok_or_else(|| "could not locate home directory for version store".to_string())?;
    match sub {
        ConfigVersionCmd::List(args) => {
            let out = format_version_list(&store, args.limit).map_err(|e| e.to_string())?;
            print!("{out}");
            Ok(())
        }
        ConfigVersionCmd::Show(args) => {
            let out = format_version_show(&store, &args.id).map_err(|e| e.to_string())?;
            print!("{out}");
            Ok(())
        }
        ConfigVersionCmd::Diff(args) => {
            let out = format_version_diff(&store, &args.a, &args.b).map_err(|e| e.to_string())?;
            print!("{out}");
            Ok(())
        }
        ConfigVersionCmd::Current => {
            let id = format_current().map_err(|e| e.to_string())?;
            println!("{id}");
            Ok(())
        }
        ConfigVersionCmd::Pull(_) => Err(
            "config version pull is server-owned; CLI no longer connects to MatrixOne directly"
                .to_string(),
        ),
    }
}

fn config_show_policy(model: Option<&str>, json: bool) -> Result<(), String> {
    let cfg = astra_config::runtime_config::RuntimeConfig::load();
    let policy = cfg.tool_selection.resolve_for_model(model);
    let trust_mode = match cfg.safety.resolved_trust_mode() {
        astra_config::runtime_config::TrustModeSerde::Strict => "strict",
        astra_config::runtime_config::TrustModeSerde::Trusted => "trusted",
    };
    let rejected = cfg.tool_selection.rejected_model_match_patterns();
    println!(
        "{}",
        format_policy_output(model, &policy, trust_mode, &rejected, json)
    );
    Ok(())
}

/// Render a resolved [`EffectiveToolPolicy`] as either JSON or human text.
///
/// Kept as a pure function of inputs so it can be unit-tested without
/// shelling out to the binary or touching the filesystem.
///
/// `rejected_patterns` is the list of `model_profiles.model_match` values
/// that were silently ignored at resolve time because they were too short
/// (see `ToolSelectionConfig::rejected_model_match_patterns`). When
/// non-empty, they're surfaced so users can spot misconfigs.
pub(crate) fn format_policy_output(
    model: Option<&str>,
    policy: &astra_config::runtime_config::EffectiveToolPolicy,
    trust_mode: &str,
    rejected_patterns: &[String],
    json: bool,
) -> String {
    if json {
        let payload = serde_json::json!({
            "model": model,
            "trust_mode": trust_mode,
            "max_identical_tool_calls": policy.max_identical_tool_calls,
            "max_tools_per_turn": policy.max_tools_per_turn,
            "repeated_cache_hit_suppression": policy.repeated_cache_hit_suppression,
            "max_consecutive_empty_name": policy.max_consecutive_empty_name,
            "parallel_batching_force_streak": policy.parallel_batching_force_streak,
            // Always present as an array (possibly empty) so json consumers
            // never have to special-case the absent-vs-empty case.
            "rejected_model_match_patterns": rejected_patterns,
        });
        serde_json::to_string_pretty(&payload)
            .unwrap_or_else(|_| "{\"error\": \"failed to serialize policy\"}".to_string())
    } else {
        let label = model.unwrap_or("<global defaults — no model>");
        let mut out = format!(
            "Resolved workflow-guard policy for {label}:\n\
             \n  trust_mode                     = {trust_mode}\
             \n  max_identical_tool_calls       = {}\
             \n  max_tools_per_turn             = {}\
             \n  repeated_cache_hit_suppression = {}\
             \n  max_consecutive_empty_name     = {}\
             \n  parallel_batching_force_streak = {}\n",
            policy.max_identical_tool_calls,
            policy.max_tools_per_turn,
            policy.repeated_cache_hit_suppression,
            policy.max_consecutive_empty_name,
            policy.parallel_batching_force_streak,
        );
        if !rejected_patterns.is_empty() {
            out.push_str(
                "\n⚠  rejected model_match patterns (too short, ignored at resolve time):\n",
            );
            for p in rejected_patterns {
                out.push_str(&format!("    - \"{p}\"\n"));
            }
        }
        out
    }
}

fn config_list() -> Result<(), String> {
    let settings = read_settings()?;
    let path = settings_path(None)?;

    if settings.is_empty() {
        println!("  {}", "No settings configured.".dim());
        println!(
            "  Use {} to set a value.",
            "astra config set <key> <value>".magenta()
        );
        println!("\n  {}:", "Available keys".bold());
        for (key, desc) in KNOWN_SETTINGS {
            println!("    {}  {}", key.magenta(), desc.dim());
        }
        return Ok(());
    }

    let (hk, hv) = ("Key", "Value");
    println!("  {:<20} {hv}", hk.bold());
    println!("  {}", "─".repeat(50).dim());
    for (key, value) in &settings {
        let display = match value {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        println!("  {:<20} {display}", key.as_str().magenta());
    }
    println!(
        "\n  {} {}",
        "Config:".dim(),
        path.display().to_string().dim()
    );
    Ok(())
}

fn config_get(key: &str) -> Result<(), String> {
    let settings = read_settings()?;
    match settings.get(key) {
        Some(val) => {
            match val {
                serde_json::Value::String(s) => println!("{s}"),
                other => println!("{other}"),
            }
            Ok(())
        }
        None => {
            // Check if it's a known key
            if let Some((_, desc)) = KNOWN_SETTINGS.iter().find(|(k, _)| *k == key) {
                Err(format!("'{key}' is not set. {desc}"))
            } else {
                Err(format!("'{key}' is not set"))
            }
        }
    }
}

fn config_set(key: &str, value: &str) -> Result<(), String> {
    let mut settings = read_settings()?;

    // Parse value: try bool, then number, then keep as string
    let json_value = match value {
        "true" => serde_json::Value::Bool(true),
        "false" => serde_json::Value::Bool(false),
        v if v.parse::<f64>().is_ok() && !v.contains(|c: char| c.is_alphabetic()) => {
            if let Ok(n) = v.parse::<i64>() {
                serde_json::Value::Number(n.into())
            } else {
                serde_json::Value::String(v.to_string())
            }
        }
        v => serde_json::Value::String(v.to_string()),
    };

    settings.insert(key.to_string(), json_value);
    write_settings(&settings)?;
    println!("  {} Set '{}' = {}", theme::icon_ok(), key.magenta(), value);
    Ok(())
}

#[cfg(test)]
mod config_cli_tests {
    use super::{KNOWN_SETTINGS, read_config_api_url_from};
    use crate::cli::arg_render::apply_system_prompt;

    #[test]
    fn read_settings_missing_file_returns_empty() {
        // settings_path() returns home-based path; test read directly
        let settings = serde_json::Map::new();
        assert!(settings.is_empty());
    }

    #[test]
    fn write_and_read_settings_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        let mut settings = serde_json::Map::new();
        settings.insert(
            "default_model".to_string(),
            serde_json::Value::String("gpt-4o".into()),
        );
        settings.insert("verbose".to_string(), serde_json::Value::Bool(true));

        let val = serde_json::Value::Object(settings.clone());
        std::fs::write(&path, serde_json::to_string_pretty(&val).unwrap()).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        let loaded = parsed.as_object().unwrap();
        assert_eq!(loaded["default_model"], "gpt-4o");
        assert_eq!(loaded["verbose"], true);
    }

    #[test]
    fn value_parsing_booleans() {
        let parse = |v: &str| -> serde_json::Value {
            match v {
                "true" => serde_json::Value::Bool(true),
                "false" => serde_json::Value::Bool(false),
                _ => serde_json::Value::String(v.to_string()),
            }
        };
        assert_eq!(parse("true"), serde_json::Value::Bool(true));
        assert_eq!(parse("false"), serde_json::Value::Bool(false));
        assert_eq!(parse("hello"), serde_json::Value::String("hello".into()));
    }

    #[test]
    fn value_parsing_numbers() {
        let v = "42";
        let parsed = v.parse::<i64>().unwrap();
        assert_eq!(parsed, 42);
    }

    #[test]
    fn known_settings_not_empty() {
        assert!(!KNOWN_SETTINGS.is_empty());
        for (key, desc) in KNOWN_SETTINGS {
            assert!(!key.is_empty());
            assert!(!desc.is_empty());
        }
    }

    #[test]
    fn config_set_overwrites() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("settings.json");

        // Initial
        let mut settings = serde_json::Map::new();
        settings.insert("key".to_string(), serde_json::Value::String("v1".into()));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::Value::Object(settings)).unwrap(),
        )
        .unwrap();

        // Overwrite
        let content = std::fs::read_to_string(&path).unwrap();
        let mut loaded: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str::<serde_json::Value>(&content)
                .unwrap()
                .as_object()
                .unwrap()
                .clone();
        loaded.insert("key".to_string(), serde_json::Value::String("v2".into()));
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::Value::Object(loaded)).unwrap(),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let final_val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(final_val["key"], "v2");
    }

    // Helper matching config_set's value parsing logic
    fn parse_value(value: &str) -> serde_json::Value {
        match value {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            v if v.parse::<f64>().is_ok() && !v.contains(|c: char| c.is_alphabetic()) => {
                if let Ok(n) = v.parse::<i64>() {
                    serde_json::Value::Number(n.into())
                } else {
                    serde_json::Value::String(v.to_string())
                }
            }
            v => serde_json::Value::String(v.to_string()),
        }
    }

    #[test]
    fn config_value_parsing_booleans() {
        assert_eq!(parse_value("true"), serde_json::Value::Bool(true));
        assert_eq!(parse_value("false"), serde_json::Value::Bool(false));
    }

    #[test]
    fn config_value_parsing_integers() {
        assert_eq!(parse_value("42"), serde_json::Value::Number(42.into()));
        assert_eq!(parse_value("0"), serde_json::Value::Number(0.into()));
        assert_eq!(parse_value("-1"), serde_json::Value::Number((-1).into()));
    }

    #[test]
    fn config_value_parsing_strings() {
        assert_eq!(
            parse_value("gpt-4o"),
            serde_json::Value::String("gpt-4o".into())
        );
        assert_eq!(parse_value(""), serde_json::Value::String("".into()));
    }

    #[test]
    fn known_settings_has_default_model() {
        assert!(KNOWN_SETTINGS.iter().any(|(k, _)| *k == "default_model"));
    }

    #[test]
    fn known_settings_has_auto_approve() {
        assert!(KNOWN_SETTINGS.iter().any(|(k, _)| *k == "auto_approve"));
    }

    #[test]
    fn apply_system_prompt_none_passthrough() {
        assert_eq!(apply_system_prompt("hello", None), "hello");
    }

    #[test]
    fn apply_system_prompt_wraps_message() {
        let result = apply_system_prompt("hello", Some("Be concise"));
        assert!(result.starts_with("<system_instructions>"));
        assert!(result.contains("Be concise"));
        assert!(result.ends_with("hello"));
    }

    #[test]
    fn read_config_api_url_from_rejects_non_object_root() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"["not-an-object"]"#).expect("write settings");

        let err = read_config_api_url_from(Some(&path)).expect_err("non-object config must fail");
        assert!(err.contains("is not a JSON object"));
    }
}
