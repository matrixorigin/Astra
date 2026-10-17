//! MCP configuration discovery for the standalone CLI.
//!
//! Skill parsing and skill/tool registration are owned by `astra-skills`.
//! This module intentionally reads only the optional `mcp_servers` extension
//! from `manifest.yaml`, plus Claude-compatible `.astra/mcp.json` files.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::cli::theme;
use crate::mcp_client::{McpServerConfig, RetryConfig, Transport};

#[derive(Debug, Default, Deserialize)]
struct SkillMcpManifest {
    #[serde(default)]
    mcp_servers: Vec<McpServerConfig>,
}

/// Winning runtime configuration and its file provenance. Connection values are
/// intentionally kept out of diagnostics and display projections.
pub(crate) struct DiscoveredMcpServer {
    pub config: McpServerConfig,
    pub scope: &'static str,
    pub path: PathBuf,
}

#[derive(Default)]
pub(crate) struct McpConfigDiscovery {
    pub servers: Vec<DiscoveredMcpServer>,
    pub diagnostics: Vec<String>,
}

impl McpConfigDiscovery {
    pub(crate) fn into_configs(self) -> Vec<McpServerConfig> {
        self.servers
            .into_iter()
            .map(|server| server.config)
            .collect()
    }
}

/// Collect MCP server configs from standalone config and skill manifests.
///
/// Discovery order is authoritative: project `.astra/mcp.json`, project and
/// user skill roots in `astra-skills` priority order, then user
/// `~/.astra/mcp.json`. The first enabled server with a given name wins.
pub fn collect_mcp_server_configs() -> Vec<McpServerConfig> {
    let discovery = discover_mcp_server_configs();
    for diagnostic in &discovery.diagnostics {
        eprintln!("  {} {diagnostic}", theme::icon_warn());
    }
    discovery.into_configs()
}

pub(crate) fn discover_mcp_server_configs() -> McpConfigDiscovery {
    discover_mcp_server_configs_from_paths(
        project_mcp_json_path().as_deref(),
        &astra_skills::loader::skill_search_paths(),
        global_mcp_json_path().as_deref(),
    )
}

/// Explicit inputs keep display and runtime on one merge path and allow tests
/// to cover discovery without changing process-global cwd, HOME or skill roots.
pub(crate) fn discover_mcp_server_configs_from_paths(
    project: Option<&Path>,
    skill_roots: &[PathBuf],
    user: Option<&Path>,
) -> McpConfigDiscovery {
    let mut discovery = McpConfigDiscovery::default();
    let mut seen = HashSet::new();
    if let Some(path) = project {
        load_mcp_json_into(path, "project", &mut seen, &mut discovery);
    }
    for root in skill_roots {
        for path in confined_skill_manifest_paths(root, &mut discovery.diagnostics) {
            match load_skill_mcp_servers(&path) {
                Ok(servers) => {
                    for config in servers {
                        if config.enabled && seen.insert(config.name.clone()) {
                            discovery.servers.push(DiscoveredMcpServer {
                                config,
                                scope: "skill",
                                path: path.clone(),
                            });
                        }
                    }
                }
                Err(diagnostic) => discovery.diagnostics.push(diagnostic),
            }
        }
    }
    if let Some(path) = user {
        load_mcp_json_into(path, "user", &mut seen, &mut discovery);
    }
    discovery
}

/// Enumerate manifests deterministically and reject symlinks that escape the
/// configured skill root. MCP entries can launch processes, so compatibility
/// discovery must preserve the same containment boundary as skill loading.
fn confined_skill_manifest_paths(search_dir: &Path, diagnostics: &mut Vec<String>) -> Vec<PathBuf> {
    let canonical_root = match search_dir.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(_) => {
            diagnostics.push(format!(
                "Failed to read MCP skill root {}",
                search_dir.display()
            ));
            return Vec::new();
        }
    };
    let entries = match std::fs::read_dir(search_dir) {
        Ok(entries) => entries,
        Err(_) => {
            diagnostics.push(format!(
                "Failed to read MCP skill root {}",
                search_dir.display()
            ));
            return Vec::new();
        }
    };

    let mut manifests = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                diagnostics.push(format!(
                    "Failed to read entry in MCP skill root {}",
                    search_dir.display()
                ));
                continue;
            }
        };
        let path = entry.path().join("manifest.yaml");
        // SKILL.md-only packages have no compatibility MCP manifest.
        let metadata = match path.metadata() {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(_) => {
                diagnostics.push(format!(
                    "Failed to read skill MCP manifest {}",
                    path.display()
                ));
                continue;
            }
        };
        if !metadata.is_file() {
            diagnostics.push(format!(
                "Failed to read skill MCP manifest {} (expected a regular file)",
                path.display()
            ));
            continue;
        }
        if path
            .canonicalize()
            .is_ok_and(|canonical| canonical.starts_with(&canonical_root))
        {
            manifests.push(path);
        }
    }
    manifests.sort();
    manifests
}

fn load_skill_mcp_servers(path: &Path) -> Result<Vec<McpServerConfig>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|_| format!("Failed to read skill MCP manifest {}", path.display()))?;
    serde_yaml_ng::from_str::<SkillMcpManifest>(&content)
        .map(|manifest| manifest.mcp_servers)
        // Serde errors can quote a rejected credential value. Never print them.
        .map_err(|_| format!("Failed to parse skill MCP manifest {}", path.display()))
}

// ─── Standalone .astra/mcp.json support ───────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpJsonConfig {
    #[serde(default)]
    mcp_servers: BTreeMap<String, McpJsonServerEntry>,
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
        "streamable_http" => Transport::StreamableHttp {
            url: entry.url.clone()?,
            auth_token: entry.auth_token.clone(),
            headers: entry.headers.clone(),
        },
        "ws" => Transport::Ws {
            url: entry.url.clone()?,
            auth_token: entry.auth_token.clone(),
            headers: entry.headers.clone(),
        },
        _ => return None,
    };
    Some(McpServerConfig {
        name: name.to_owned(),
        transport,
        description: String::new(),
        enabled: true,
        retry: RetryConfig::default(),
    })
}

/// Missing optional files are normal; directories, FIFOs, devices and read
/// failures are diagnostics. Check the file kind before reading so discovery
/// never opens a configured FIFO and waits for a writer.
pub(crate) fn read_optional_mcp_config_file(path: &Path) -> Result<Option<String>, String> {
    match path.metadata() {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Err(format!(
                "Failed to read MCP config {} (expected a regular file)",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("Failed to read MCP config {}", path.display())),
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|_| format!("Failed to read MCP config {}", path.display()))
}

fn load_mcp_json_into(
    path: &Path,
    scope: &'static str,
    seen: &mut HashSet<String>,
    discovery: &mut McpConfigDiscovery,
) {
    let content = match read_optional_mcp_config_file(path) {
        Ok(Some(content)) => content,
        Ok(None) => return,
        Err(diagnostic) => {
            discovery.diagnostics.push(diagnostic);
            return;
        }
    };
    let json_config: McpJsonConfig =
        match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(&content)
            .and_then(|object| serde_json::from_value(serde_json::Value::Object(object)))
        {
            Ok(config) => config,
            Err(error) => {
                let location = if error.line() > 0 {
                    format!(" (line {}, column {})", error.line(), error.column())
                } else {
                    String::new()
                };
                discovery.diagnostics.push(format!(
                    "Failed to parse MCP config {}{location}",
                    path.display()
                ));
                return;
            }
        };
    for (name, entry) in json_config.mcp_servers {
        if seen.contains(&name) || entry.disabled {
            continue;
        }
        if let Some(config) = json_entry_to_config(&name, &entry) {
            seen.insert(name);
            discovery.servers.push(DiscoveredMcpServer {
                config,
                scope,
                path: path.to_path_buf(),
            });
        } else {
            discovery.diagnostics.push(format!(
                "Skipped invalid MCP server in {} (unsupported transport or missing command/url)",
                path.display()
            ));
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

    fn write_json(path: &Path, servers: serde_json::Value) {
        std::fs::write(path, serde_json::json!({"mcpServers": servers}).to_string()).unwrap();
    }

    fn write_skill(root: &Path, directory: &str, name: &str, command: &str) -> PathBuf {
        let dir = root.join(directory);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.yaml");
        std::fs::write(&path, format!(
            "mcp_servers:\n  - name: {name}\n    transport:\n      type: stdio\n      command: [{command}]\n"
        )).unwrap();
        path
    }

    #[test]
    fn effective_discovery_matches_runtime_configs_and_preserves_first_enabled_source() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project.json");
        let user = dir.path().join("user.json");
        let skills = dir.path().join("skills");
        std::fs::create_dir(&skills).unwrap();
        write_json(
            &project,
            serde_json::json!({
                "duplicate": {"command": "project-wins"},
                "disabled": {"command": "ignored", "disabled": true},
                "incomplete": {},
                "unsupported": {"type": "http", "url": "ignored"}
            }),
        );
        let first = write_skill(&skills, "alpha", "duplicate", "skill-loses");
        write_skill(&skills, "beta", "skill-only", "skill-wins");
        write_skill(&skills, "gamma", "skill-only", "later-skill-loses");
        write_skill(&skills, "delta", "disabled", "disabled-fallback");
        write_json(
            &user,
            serde_json::json!({
                "duplicate": {"command": "user-loses"},
                "skill-only": {"command": "user-loses"},
                "incomplete": {"command": "user-fallback"},
                "unsupported": {"command": "user-fallback"},
                "user-only": {"type": "ws", "url": "ws://example.test/mcp"}
            }),
        );
        let discovery =
            discover_mcp_server_configs_from_paths(Some(&project), &[skills], Some(&user));
        let names = discovery
            .servers
            .iter()
            .map(|s| s.config.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "duplicate",
                "skill-only",
                "disabled",
                "incomplete",
                "unsupported",
                "user-only"
            ]
        );
        assert_eq!(discovery.servers[0].scope, "project");
        assert_eq!(discovery.servers[0].path, project);
        assert_eq!(discovery.servers[1].scope, "skill");
        assert!(discovery.servers[1].path.ends_with("beta/manifest.yaml"));
        assert_ne!(discovery.servers[1].path, first);
        assert_eq!(discovery.servers[3].scope, "user");
        assert_eq!(discovery.diagnostics.len(), 2);
        let projected = discovery
            .servers
            .iter()
            .map(|s| serde_json::to_value(&s.config).unwrap())
            .collect::<Vec<_>>();
        let runtime = discovery
            .into_configs()
            .iter()
            .map(|s| serde_json::to_value(s).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(projected, runtime);
        assert_eq!(runtime[0]["transport"]["command"][0], "project-wins");
        assert_eq!(runtime[1]["transport"]["command"][0], "skill-wins");
    }

    #[test]
    fn skill_root_priority_precedes_manifest_name_order_and_user_config() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let winner = write_skill(&first, "zeta", "shared", "first-root");
        write_skill(&second, "alpha", "shared", "second-root");
        let discovery = discover_mcp_server_configs_from_paths(None, &[first, second], None);
        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].path, winner);
    }

    #[test]
    fn missing_empty_malformed_and_unreadable_sources_are_distinct_and_redacted() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.json");
        let empty = discover_mcp_server_configs_from_paths(Some(&missing), &[], None);
        assert!(empty.servers.is_empty());
        assert!(empty.diagnostics.is_empty());
        let path = dir.path().join("mcp.json");
        write_json(&path, serde_json::json!({}));
        assert!(
            discover_mcp_server_configs_from_paths(Some(&path), &[], None)
                .diagnostics
                .is_empty()
        );
        std::fs::write(
            &path,
            r#"{"mcpServers":{"bad":{"args":"synthetic-private-arg"}}}"#,
        )
        .unwrap();
        let malformed = discover_mcp_server_configs_from_paths(Some(&path), &[], None);
        assert_eq!(malformed.diagnostics.len(), 1);
        assert!(malformed.diagnostics[0].contains("Failed to parse"));
        assert!(!malformed.diagnostics[0].contains("synthetic-private-arg"));
        let unreadable = discover_mcp_server_configs_from_paths(Some(dir.path()), &[], None);
        assert!(unreadable.diagnostics[0].contains("Failed to read"));
        let skills = dir.path().join("skills");
        std::fs::create_dir(&skills).unwrap();
        let skill = write_skill(&skills, "bad", "bad", "ok");
        std::fs::write(&skill, "mcp_servers:\n  - name: bad\n    transport:\n      type: synthetic-private-transport\n").unwrap();
        let malformed_skill = discover_mcp_server_configs_from_paths(None, &[skills], None);
        assert_eq!(malformed_skill.diagnostics.len(), 1);
        assert!(malformed_skill.diagnostics[0].contains("Failed to parse skill"));
        assert!(!malformed_skill.diagnostics[0].contains("synthetic-private-transport"));
    }

    #[test]
    fn standalone_config_rejects_non_object_roots_and_invalid_server_maps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        for content in [
            "[]",
            "[{}]",
            "null",
            "42",
            "true",
            "\"synthetic-private-root\"",
            r#"{"mcpServers":[]}"#,
            r#"{"mcpServers":null}"#,
            r#"{"mcpServers":"synthetic-private-map"}"#,
        ] {
            std::fs::write(&path, content).unwrap();
            let discovery = discover_mcp_server_configs_from_paths(Some(&path), &[], None);
            assert!(discovery.servers.is_empty(), "{content}");
            assert_eq!(discovery.diagnostics.len(), 1, "{content}");
            assert!(discovery.diagnostics[0].contains("Failed to parse"));
            assert!(!discovery.diagnostics[0].contains("synthetic-private"));
        }
    }

    #[test]
    fn skill_manifest_rejects_non_mapping_roots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.yaml");
        for content in ["[]", "[{}]", "null", "42", "synthetic-private-root"] {
            std::fs::write(&path, content).unwrap();
            let error = load_skill_mcp_servers(&path).unwrap_err();
            assert!(error.contains("Failed to parse skill MCP manifest"));
            assert!(!error.contains("synthetic-private"));
        }
        for content in ["", "{}", "mcp_servers: []"] {
            std::fs::write(&path, content).unwrap();
            assert!(load_skill_mcp_servers(&path).unwrap().is_empty());
        }
    }

    #[test]
    fn absent_skill_roots_are_normal_but_unreadable_roots_are_diagnostic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let discovery = discover_mcp_server_configs_from_paths(None, &[missing], None);
        assert!(discovery.diagnostics.is_empty());
        let not_directory = dir.path().join("not-directory");
        std::fs::write(&not_directory, "not a directory").unwrap();
        let discovery = discover_mcp_server_configs_from_paths(None, &[not_directory], None);
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("Failed to read MCP skill root"));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_skill_root_is_not_an_empty_discovery() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // Root can read mode-000 directories; the non-directory case remains deterministic.
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
        let discovery =
            discover_mcp_server_configs_from_paths(None, &[dir.path().to_path_buf()], None);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("Failed to read MCP skill root"));
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_mcp_sources_are_rejected_before_reading_and_regular_symlinks_work() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("mcp.json");
        nix::unistd::mkfifo(
            &fifo,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        // Production metadata guards are in place: no writer or FIFO read is started.
        let error = read_optional_mcp_config_file(&fifo).unwrap_err();
        assert!(error.contains("expected a regular file"));
        let discovery = discover_mcp_server_configs_from_paths(Some(&fifo), &[], None);
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.servers.is_empty());
        let regular = dir.path().join("regular.json");
        write_json(
            &regular,
            serde_json::json!({"example": {"command": "echo"}}),
        );
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        let discovery = discover_mcp_server_configs_from_paths(Some(&link), &[], None);
        assert_eq!(discovery.servers.len(), 1);
        assert!(discovery.diagnostics.is_empty());
        let root = dir.path().join("skills");
        std::fs::create_dir_all(root.join("bad/manifest.yaml")).unwrap();
        let discovery = discover_mcp_server_configs_from_paths(None, &[root], None);
        assert_eq!(discovery.diagnostics.len(), 1);
        assert!(discovery.diagnostics[0].contains("expected a regular file"));
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

        let servers = load_skill_mcp_servers(&path).unwrap();
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

        let manifests = confined_skill_manifest_paths(root.path(), &mut Vec::new());
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

        assert!(confined_skill_manifest_paths(root.path(), &mut Vec::new()).is_empty());
    }

    #[test]
    fn parses_only_canonical_stdio_sse_streamable_http_and_ws_transports() {
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
            r#"{"mcpServers":{"http":{"type":"streamable_http","url":"http://localhost/mcp"}}}"#,
        )
        .unwrap();
        assert!(matches!(http.transport, Transport::StreamableHttp { .. }));

        let websocket = parse_json_server(
            "ws",
            r#"{"mcpServers":{"ws":{"type":"ws","url":"ws://localhost/mcp"}}}"#,
        )
        .unwrap();
        assert!(matches!(websocket.transport, Transport::Ws { .. }));

        for retired in ["http", "streamable-http", "websocket"] {
            let json = format!(
                r#"{{"mcpServers":{{"retired":{{"type":"{retired}","url":"http://localhost/mcp"}}}}}}"#
            );
            assert!(parse_json_server("retired", &json).is_none(), "{retired}");
        }
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
        let mut discovery = McpConfigDiscovery::default();
        load_mcp_json_into(&path, "project", &mut seen, &mut discovery);

        assert_eq!(discovery.servers.len(), 1);
        assert_eq!(discovery.servers[0].config.name, "b");
    }

    #[test]
    fn missing_or_invalid_standalone_config_is_non_fatal() {
        let mut seen = HashSet::new();
        let mut discovery = McpConfigDiscovery::default();
        load_mcp_json_into(
            Path::new("/nonexistent/mcp.json"),
            "project",
            &mut seen,
            &mut discovery,
        );
        assert!(discovery.servers.is_empty());
        assert!(discovery.diagnostics.is_empty());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(&path, "not json").unwrap();
        load_mcp_json_into(&path, "project", &mut seen, &mut discovery);
        assert!(discovery.servers.is_empty());
        assert_eq!(discovery.diagnostics.len(), 1);
    }
}
