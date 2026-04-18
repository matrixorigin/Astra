use std::path::PathBuf;

fn settings_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|h| h.join(".astra").join("settings.json"))
        .ok_or_else(|| "Cannot determine home directory".to_string())
}

/// Read `api_url` from ~/.astra/settings.json, if set.
pub(crate) fn read_config_api_url() -> Result<Option<String>, String> {
    let path = settings_path()?;
    if !path.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let val: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse {}: {e}", path.display()))?;
    Ok(val
        .as_object()
        .and_then(|m| m.get("api_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string()))
}

const DEFAULT_API_URL: &str = "http://127.0.0.1:8000";

/// Resolve API URL with priority: flag > env var > config file > default.
pub(crate) fn resolve_api_url(flag: Option<&str>) -> String {
    resolve_api_url_with(
        flag,
        || std::env::var("ASTRA_API_URL").ok(),
        read_config_api_url,
    )
}

/// Testable core: resolve API URL with injectable env and config sources.
fn resolve_api_url_with(
    flag: Option<&str>,
    env_fn: impl FnOnce() -> Option<String>,
    config_fn: impl FnOnce() -> Result<Option<String>, String>,
) -> String {
    flag.map(|s| s.trim_end_matches('/').to_string())
        .or_else(|| env_fn().map(|s| s.trim_end_matches('/').to_string()))
        .or_else(|| match config_fn() {
            Ok(Some(url)) => Some(url.trim_end_matches('/').to_string()),
            Ok(None) => None,
            Err(e) => {
                eprintln!("warning: failed to read config for api_url: {e}");
                None
            }
        })
        .unwrap_or_else(|| DEFAULT_API_URL.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env() -> Option<String> {
        None
    }
    fn no_config() -> Result<Option<String>, String> {
        Ok(None)
    }
    fn env_val(url: &str) -> impl FnOnce() -> Option<String> {
        let s = url.to_string();
        move || Some(s)
    }
    fn config_val(url: &str) -> impl FnOnce() -> Result<Option<String>, String> {
        let s = url.to_string();
        move || Ok(Some(s))
    }

    #[test]
    fn flag_wins_over_env_and_config() {
        let url = resolve_api_url_with(
            Some("http://flag:8000"),
            env_val("http://env:8000"),
            config_val("http://config:8000"),
        );
        assert_eq!(url, "http://flag:8000");
    }

    #[test]
    fn env_wins_over_config() {
        let url = resolve_api_url_with(
            None,
            env_val("http://env:8000"),
            config_val("http://config:8000"),
        );
        assert_eq!(url, "http://env:8000");
    }

    #[test]
    fn config_wins_over_default() {
        let url = resolve_api_url_with(None, no_env, config_val("http://config:8000"));
        assert_eq!(url, "http://config:8000");
    }

    #[test]
    fn falls_back_to_default() {
        let url = resolve_api_url_with(None, no_env, no_config);
        assert_eq!(url, DEFAULT_API_URL);
    }

    #[test]
    fn trailing_slash_stripped_from_flag() {
        let url = resolve_api_url_with(Some("http://flag:8000/"), no_env, no_config);
        assert_eq!(url, "http://flag:8000");
    }

    #[test]
    fn trailing_slash_stripped_from_env() {
        let url = resolve_api_url_with(None, env_val("http://env:8000/"), no_config);
        assert_eq!(url, "http://env:8000");
    }

    #[test]
    fn config_error_warns_and_falls_through() {
        let url = resolve_api_url_with(None, no_env, || Err("broken".to_string()));
        assert_eq!(url, DEFAULT_API_URL);
    }

    #[test]
    fn read_config_api_url_reads_real_file() {
        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(
            astra_dir.join("settings.json"),
            r#"{"api_url":"http://from-disk:9999","default_model":"gpt-4"}"#,
        )
        .unwrap();

        let prev_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let result = read_config_api_url();
        assert_eq!(result.unwrap().as_deref(), Some("http://from-disk:9999"));

        if let Some(prev) = prev_home {
            unsafe { std::env::set_var("HOME", prev) };
        }
    }

    #[test]
    fn read_config_api_url_returns_none_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("settings.json"), r#"{}"#).unwrap();

        let prev_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let result = read_config_api_url();
        assert_eq!(result.unwrap(), None);

        if let Some(prev) = prev_home {
            unsafe { std::env::set_var("HOME", prev) };
        }
    }

    #[test]
    fn read_config_api_url_returns_none_when_no_file() {
        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let result = read_config_api_url();
        assert_eq!(result.unwrap(), None);

        if let Some(prev) = prev_home {
            unsafe { std::env::set_var("HOME", prev) };
        }
    }

    #[test]
    fn read_config_api_url_error_on_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let astra_dir = tmp.path().join(".astra");
        std::fs::create_dir_all(&astra_dir).unwrap();
        std::fs::write(astra_dir.join("settings.json"), "not json").unwrap();

        let prev_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.path()) };

        let result = read_config_api_url();
        assert!(result.is_err());

        if let Some(prev) = prev_home {
            unsafe { std::env::set_var("HOME", prev) };
        }
    }
}
