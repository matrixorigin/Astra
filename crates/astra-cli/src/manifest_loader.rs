//! MCP configuration discovery for the standalone CLI.
//!
//! Skill parsing and skill/tool registration are owned by `astra-skills`.
//! This module intentionally reads only the optional `mcp_servers` extension
//! from `manifest.yaml`, plus Claude-compatible `.astra/mcp.json` files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cli::theme;
use crate::mcp_client::{McpServerConfig, RetryConfig, Transport};

#[derive(Debug, Default, Deserialize)]
struct SkillMcpManifest {
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,
}

/// Collect MCP server configs from standalone config and skill manifests.
///
/// Discovery order is authoritative: project `.astra/mcp.json`, project and
/// user skill roots in `astra-skills` priority order, then user
/// `~/.astra/mcp.json`. The first enabled server with a given name wins.
pub fn collect_mcp_server_configs() -> Vec<McpServerConfig> {
    let mut seen = HashSet::new();
    let mut configs = Vec::new();

    if let Some(project_mcp) = project_mcp_json_path() {
        load_mcp_json_into(&project_mcp, &mut seen, &mut configs);
    }

    for search_dir in astra_skills::loader::skill_search_paths() {
        for manifest_path in confined_skill_manifest_paths(&search_dir) {
            for server in load_skill_mcp_servers(&manifest_path) {
                if server.enabled && seen.insert(server.name.clone()) {
                    configs.push(server);
                }
            }
        }
    }

    if let Some(global_mcp) = global_mcp_json_path() {
        load_mcp_json_into(&global_mcp, &mut seen, &mut configs);
    }

    configs
}

/// Enumerate manifests deterministically and reject symlinks that escape the
/// configured skill root. MCP entries can launch processes, so compatibility
/// discovery must preserve the same containment boundary as skill loading.
fn confined_skill_manifest_paths(search_dir: &Path) -> Vec<PathBuf> {
    let Ok(canonical_root) = search_dir.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(search_dir) else {
        return Vec::new();
    };

    let mut manifests = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("manifest.yaml"))
        .filter(|path| path.is_file())
        .filter(|path| {
            path.canonicalize()
                .is_ok_and(|canonical| canonical.starts_with(&canonical_root))
        })
        .collect::<Vec<_>>();
    manifests.sort();
    manifests
}

fn load_skill_mcp_servers(path: &Path) -> Vec<McpServerConfig> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read skill MCP manifest");
            return Vec::new();
        }
    };
    match serde_yaml_ng::from_str::<SkillMcpManifest>(&content) {
        Ok(manifest) => manifest.mcp_servers,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to parse skill MCP manifest");
            Vec::new()
        }
    }
}

// ─── Standalone .astra/mcp.json support ───────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpJsonConfig {
    #[serde(default)]
    mcp_servers: HashMap<String, McpJsonServerEntry>,
}

#[derive(Debug, Deserialize)]
struct McpJsonServerEntry {
    #[serde(default = "default_stdio")]
    r#type: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: HashMap<String, String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    auth_token: Option<String>,
    #[serde(default)]
    disabled: bool,
}

fn default_stdio() -> String {
    "stdio".to_owned()
}

fn json_entry_to_config(name: &str, entry: &McpJsonServerEntry) -> Option<McpServerConfig> {
    if entry.disabled {
        return None;
    }
    let transport = match entry.r#type.as_str() {
        "stdio" | "" => {
            let mut command = vec![entry.command.clone()?];
            command.extend(entry.args.iter().cloned());
            Transport::Stdio {
                command,
                args: Vec::new(),
                env: entry.env.clone(),
            }
        }
        "sse" => Transport::Sse {
            url: entry.url.clone()?,
            auth_token: entry.auth_token.clone(),
            headers: entry.headers.clone(),
        },
        "http" | "streamable_http" | "streamable-http" => Transport::StreamableHttp {
            url: entry.url.clone()?,
            auth_token: entry.auth_token.clone(),
            headers: entry.headers.clone(),
        },
        "ws" | "websocket" => Transport::Ws {
            url: entry.url.clone()?,
            auth_token: entry.auth_token.clone(),
            headers: entry.headers.clone(),
        },
        other => {
            eprintln!(
                "  {} mcp.json: unknown transport type '{}' for server '{}'",
                theme::icon_warn(),
                other,
                name
            );
            return None;
        }
    };
    Some(McpServerConfig {
        name: name.to_owned(),
        transport,
        description: String::new(),
        enabled: true,
        retry: RetryConfig::default(),
    })
}

fn load_mcp_json_into(path: &Path, seen: &mut HashSet<String>, configs: &mut Vec<McpServerConfig>) {
    if !path.is_file() {
        return;
    }
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) => {
            eprintln!(
                "  {} Failed to read {}: {error}",
                theme::icon_warn(),
                path.display()
            );
            return;
        }
    };
    let json_config: McpJsonConfig = match serde_json::from_str(&content) {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "  {} Failed to parse {}: {error}",
                theme::icon_warn(),
                path.display()
            );
            return;
        }
    };
    for (name, entry) in json_config.mcp_servers {
        if seen.contains(&name) {
            continue;
        }
        if let Some(config) = json_entry_to_config(&name, &entry) {
            seen.insert(name);
            configs.push(config);
        }
    }
}

pub fn project_mcp_json_path() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .map(|cwd| cwd.join(".astra").join("mcp.json"))
}

pub fn global_mcp_json_path() -> Option<PathBuf> {
    Some(astra_runtime_env::local_state_root().join("mcp.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_json_server(name: &str, body: &str) -> Option<McpServerConfig> {
        let config: McpJsonConfig = serde_json::from_str(body).expect("valid test config");
        json_entry_to_config(name, &config.mcp_servers[name])
    }

    #[test]
    fn skill_manifest_reads_only_enabled_mcp_servers() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("skill");
        std::fs::create_dir(&skill_dir).unwrap();
        let path = skill_dir.join("manifest.yaml");
        std::fs::write(
            &path,
            r#"
name: ignored-by-this-boundary
tools: [also-ignored]
mcp_servers:
  - name: enabled
    transport:
      type: stdio
      command: ["echo", "ready"]
  - name: disabled
    enabled: false
    transport:
      type: stdio
      command: ["echo", "disabled"]
"#,
        )
        .unwrap();

        let servers = load_skill_mcp_servers(&path);
        assert_eq!(servers.len(), 2);
        assert!(servers[0].enabled);
        assert!(!servers[1].enabled);
    }

    #[test]
    fn manifest_discovery_is_sorted_and_confined() {
        let root = tempfile::tempdir().unwrap();
        for name in ["zeta", "alpha"] {
            let skill = root.path().join(name);
            std::fs::create_dir(&skill).unwrap();
            std::fs::write(skill.join("manifest.yaml"), "mcp_servers: []\n").unwrap();
        }

        let manifests = confined_skill_manifest_paths(root.path());
        assert_eq!(manifests.len(), 2);
        assert!(manifests[0].ends_with("alpha/manifest.yaml"));
        assert!(manifests[1].ends_with("zeta/manifest.yaml"));
    }

    #[cfg(unix)]
    #[test]
    fn manifest_discovery_rejects_symlink_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("manifest.yaml"), "mcp_servers: []\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("escaped")).unwrap();

        assert!(confined_skill_manifest_paths(root.path()).is_empty());
    }

    #[test]
    fn parses_stdio_sse_http_and_websocket_transports() {
        let stdio = parse_json_server(
            "stdio",
            r#"{"mcpServers":{"stdio":{"command":"tool","args":["--flag"]}}}"#,
        )
        .unwrap();
        assert!(matches!(
            stdio.transport,
            Transport::Stdio { ref command, .. } if command == &["tool", "--flag"]
        ));

        let sse = parse_json_server(
            "sse",
            r#"{"mcpServers":{"sse":{"type":"sse","url":"http://localhost/sse"}}}"#,
        )
        .unwrap();
        assert!(matches!(sse.transport, Transport::Sse { .. }));

        let http = parse_json_server(
            "http",
            r#"{"mcpServers":{"http":{"type":"streamable-http","url":"http://localhost/mcp"}}}"#,
        )
        .unwrap();
        assert!(matches!(http.transport, Transport::StreamableHttp { .. }));

        let websocket = parse_json_server(
            "ws",
            r#"{"mcpServers":{"ws":{"type":"websocket","url":"ws://localhost/mcp"}}}"#,
        )
        .unwrap();
        assert!(matches!(websocket.transport, Transport::Ws { .. }));
    }

    #[test]
    fn rejects_disabled_incomplete_and_unknown_servers() {
        assert!(
            parse_json_server(
                "disabled",
                r#"{"mcpServers":{"disabled":{"command":"tool","disabled":true}}}"#,
            )
            .is_none()
        );
        assert!(parse_json_server("missing", r#"{"mcpServers":{"missing":{}}}"#).is_none());
        assert!(
            parse_json_server(
                "unknown",
                r#"{"mcpServers":{"unknown":{"type":"magic","url":"x"}}}"#,
            )
            .is_none()
        );
    }

    #[test]
    fn standalone_config_deduplicates_against_higher_priority_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"a":{"command":"cmd-a"},"b":{"command":"cmd-b"}}}"#,
        )
        .unwrap();

        let mut seen = HashSet::from(["a".to_owned()]);
        let mut configs = Vec::new();
        load_mcp_json_into(&path, &mut seen, &mut configs);

        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "b");
    }

    #[test]
    fn missing_or_invalid_standalone_config_is_non_fatal() {
        let mut seen = HashSet::new();
        let mut configs = Vec::new();
        load_mcp_json_into(Path::new("/nonexistent/mcp.json"), &mut seen, &mut configs);
        assert!(configs.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "not json").unwrap();
        load_mcp_json_into(&path, &mut seen, &mut configs);
        assert!(configs.is_empty());
    }
}
