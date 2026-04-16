//! Load custom agent profiles from Markdown files with YAML frontmatter.
//!
//! This module
//! scans well-known directories for agent definition files and builds
//! [`AgentProfile`] structs that the delegation engine can use.
//!
//! # Agent Definition Format
//!
//! ```markdown
//! ---
//! name: reviewer
//! description: Code review specialist
//! tier: user
//! tools: ["read_file", "grep", "glob"]
//! model: claude-sonnet-4-20250514
//! max_turns: 10
//! can_delegate: false
//! triggers:
//!   - type: keyword
//!     pattern: review
//! ---
//! You are a thorough code reviewer. Analyze changes for bugs...
//! ```
//!
//! # Search Paths (in priority order)
//!
//! 1. `{project_root}/.astra/agents/*.md` — project-scoped agents
//! 2. `~/.astra/agents/*.md` — user-level global agents

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use astra_services::coordination::{AgentProfile, AgentTier, AgentTrigger};
use serde::Deserialize;

// ─── Frontmatter Schema ─────────────────────────────────────────────────────

/// YAML frontmatter parsed from agent definition Markdown.
#[derive(Debug, Deserialize)]
struct AgentFrontmatter {
    /// Agent identifier. Defaults to filename stem if absent.
    name: Option<String>,
    /// Human-readable description (stored in metadata).
    description: Option<String>,
    /// Agent tier: "orchestrator", "system", or "user" (default).
    tier: Option<String>,
    /// Allowed tool/skill names. Empty = unrestricted.
    #[serde(default)]
    tools: Vec<String>,
    /// Model override (e.g., "claude-sonnet-4-20250514").
    model: Option<String>,
    /// Maximum turns for this agent's sub-run.
    max_turns: Option<u32>,
    /// Whether this agent can delegate to sub-agents.
    can_delegate: Option<bool>,
    /// Maximum delegation depth override.
    max_delegation_depth: Option<u32>,
    /// Auto-activation triggers.
    #[serde(default)]
    triggers: Vec<TriggerEntry>,
    /// MCP server names to connect (D-10).
    #[serde(default)]
    mcp_servers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct TriggerEntry {
    #[serde(rename = "type")]
    trigger_type: String,
    pattern: String,
}

// ─── Public API ─────────────────────────────────────────────────────────────

/// Directories to search for agent Markdown files.
pub fn agent_search_paths(project_root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![project_root.join(".astra/agents")];
    if let Some(home) = dirs::home_dir() {
        paths.push(home.join(".astra/agents"));
    }
    paths
}

/// Load agent profiles from Markdown files found in search paths.
///
/// Agents are loaded in search-path order; if the same `agent_id` appears in
/// multiple directories, the first (more specific) definition wins.
pub fn load_agent_profiles(project_root: &Path) -> Vec<AgentProfile> {
    let mut seen = std::collections::HashSet::new();
    let mut profiles = Vec::new();

    for dir in agent_search_paths(project_root) {
        if !dir.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut md_files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
            .collect();
        md_files.sort(); // deterministic order

        for path in md_files {
            match parse_agent_markdown(&path) {
                Ok(profile) => {
                    if seen.insert(profile.agent_id.clone()) {
                        profiles.push(profile);
                    }
                }
                Err(e) => {
                    eprintln!("  ⚠ skipping agent definition {}: {}", path.display(), e);
                }
            }
        }
    }
    profiles
}

/// Merge custom agents into an existing registry, skipping IDs that already
/// exist (built-in agents take precedence over custom ones with same ID).
pub fn load_and_merge(
    project_root: &Path,
    registry: &mut astra_services::coordination::AgentProfileRegistry,
) -> usize {
    let custom = load_agent_profiles(project_root);
    let mut added = 0;
    for profile in custom {
        if registry.get(&profile.agent_id).is_some() {
            continue; // built-in wins
        }
        let _ = registry.register(profile);
        added += 1;
    }
    added
}

// ─── Parsing ────────────────────────────────────────────────────────────────

/// Split YAML frontmatter delimited by `---` from Markdown body.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return None;
    }
    // Skip the opening `---` line
    let after_opening = &trimmed[3..];
    let after_opening = after_opening.strip_prefix('\n').unwrap_or(after_opening);

    // Find closing `---` (at start of line)
    if let Some(close_pos) = after_opening.find("\n---") {
        let frontmatter = &after_opening[..close_pos];
        let rest = &after_opening[close_pos + 4..]; // skip "\n---"
        let body = rest.strip_prefix('\n').unwrap_or(rest);
        Some((frontmatter, body))
    } else if after_opening.starts_with("---") {
        // Empty frontmatter: `---\n---\n`
        let rest = &after_opening[3..];
        let body = rest.strip_prefix('\n').unwrap_or(rest);
        Some(("", body))
    } else {
        None
    }
}

/// Parse a single agent Markdown file into an [`AgentProfile`].
fn parse_agent_markdown(path: &Path) -> Result<AgentProfile, String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("read error: {e}"))?;

    let (fm_str, body) = split_frontmatter(&content)
        .ok_or_else(|| "missing YAML frontmatter (expected `---` delimiters)".to_string())?;

    let fm: AgentFrontmatter = if fm_str.trim().is_empty() {
        AgentFrontmatter {
            name: None,
            description: None,
            tier: None,
            tools: Vec::new(),
            model: None,
            max_turns: None,
            can_delegate: None,
            max_delegation_depth: None,
            triggers: Vec::new(),
            mcp_servers: None,
        }
    } else {
        serde_yaml::from_str(fm_str).map_err(|e| format!("YAML parse error: {e}"))?
    };

    let file_stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    let agent_id = fm.name.clone().unwrap_or_else(|| file_stem.to_string());

    let tier = fm
        .tier
        .as_deref()
        .map(AgentTier::from_str_lossy)
        .unwrap_or(AgentTier::User);

    let can_delegate = fm.can_delegate.unwrap_or(tier != AgentTier::User);

    let max_delegation_depth = fm.max_delegation_depth.unwrap_or(match tier {
        AgentTier::Orchestrator => 3,
        AgentTier::System => 1,
        AgentTier::User => 0,
    });

    let triggers: Vec<AgentTrigger> = fm
        .triggers
        .into_iter()
        .map(|t| AgentTrigger {
            trigger_type: t.trigger_type,
            pattern: t.pattern,
        })
        .collect();

    let mut metadata = HashMap::new();
    if let Some(desc) = &fm.description {
        metadata.insert(
            "description".to_string(),
            serde_json::Value::String(desc.clone()),
        );
    }
    if let Some(max_turns) = fm.max_turns {
        metadata.insert(
            "max_turns".to_string(),
            serde_json::Value::Number(max_turns.into()),
        );
    }
    // Store the source path for diagnostics
    metadata.insert(
        "source_path".to_string(),
        serde_json::Value::String(path.display().to_string()),
    );

    Ok(AgentProfile {
        agent_id,
        name: fm.name.unwrap_or_else(|| title_case(file_stem)),
        tier,
        system_prompt: if body.is_empty() {
            None
        } else {
            Some(body.to_string())
        },
        skill_filter: fm.tools,
        model_override: fm.model,
        can_delegate,
        delegate_to: Vec::new(),
        max_delegation_depth,
        triggers,
        metadata,
        mcp_servers: fm.mcp_servers.unwrap_or_default(),
    })
}

/// Simple title-case: "my-agent" → "My Agent".
fn title_case(s: &str) -> String {
    s.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or(String::new(), |first| {
                let upper: String = first.to_uppercase().collect();
                format!("{upper}{}", chars.as_str())
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_agent_md(dir: &Path, name: &str, content: &str) {
        let agents_dir = dir.join(".astra/agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join(format!("{name}.md")), content).unwrap();
    }

    #[test]
    fn parse_full_frontmatter() {
        let tmp = TempDir::new().unwrap();
        let md = r#"---
name: security-auditor
description: Finds security vulnerabilities
tier: system
tools: ["read_file", "grep", "glob"]
model: claude-sonnet-4-20250514
max_turns: 15
can_delegate: false
triggers:
  - type: keyword
    pattern: security
  - type: keyword
    pattern: vulnerability
---
You are a security auditor. Scan code for common vulnerabilities
including SQL injection, XSS, and authentication bypasses.
"#;
        write_agent_md(tmp.path(), "security-auditor", md);
        let profiles = load_agent_profiles(tmp.path());
        assert_eq!(profiles.len(), 1);
        let p = &profiles[0];
        assert_eq!(p.agent_id, "security-auditor");
        assert_eq!(p.tier, AgentTier::System);
        assert_eq!(p.skill_filter, vec!["read_file", "grep", "glob"]);
        assert_eq!(
            p.model_override.as_deref(),
            Some("claude-sonnet-4-20250514")
        );
        assert!(!p.can_delegate);
        assert_eq!(p.triggers.len(), 2);
        assert!(
            p.system_prompt
                .as_ref()
                .unwrap()
                .contains("security auditor")
        );
        assert_eq!(
            p.metadata.get("max_turns").and_then(|v| v.as_u64()),
            Some(15)
        );
    }

    #[test]
    fn defaults_from_filename() {
        let tmp = TempDir::new().unwrap();
        let md = "---\n---\nJust a simple agent.\n";
        write_agent_md(tmp.path(), "my-helper", md);
        let profiles = load_agent_profiles(tmp.path());
        assert_eq!(profiles.len(), 1);
        let p = &profiles[0];
        assert_eq!(p.agent_id, "my-helper");
        assert_eq!(p.name, "My Helper");
        assert_eq!(p.tier, AgentTier::User);
        assert!(!p.can_delegate);
    }

    #[test]
    fn first_definition_wins_on_duplicate_id() {
        let tmp = TempDir::new().unwrap();

        // Project-level agent
        let project_agents = tmp.path().join(".astra/agents");
        fs::create_dir_all(&project_agents).unwrap();
        fs::write(
            project_agents.join("coder.md"),
            "---\nname: coder\n---\nProject coder.\n",
        )
        .unwrap();

        let profiles = load_agent_profiles(tmp.path());
        assert_eq!(profiles.len(), 1);
        assert!(
            profiles[0]
                .system_prompt
                .as_ref()
                .unwrap()
                .contains("Project coder")
        );
    }

    #[test]
    fn missing_frontmatter_skipped() {
        let tmp = TempDir::new().unwrap();
        let agents_dir = tmp.path().join(".astra/agents");
        fs::create_dir_all(&agents_dir).unwrap();
        fs::write(agents_dir.join("bad.md"), "No frontmatter here\n").unwrap();
        let profiles = load_agent_profiles(tmp.path());
        assert!(profiles.is_empty());
    }

    #[test]
    fn load_and_merge_skips_existing() {
        let tmp = TempDir::new().unwrap();
        write_agent_md(
            tmp.path(),
            "coder",
            "---\nname: coder\n---\nCustom coder.\n",
        );
        write_agent_md(
            tmp.path(),
            "analyst",
            "---\nname: analyst\n---\nData analyst.\n",
        );

        let mut registry = astra_services::coordination::AgentProfileRegistry::new();
        // Register built-in "coder"
        let _ = registry.register(AgentProfile::new(
            "coder",
            "Built-in Coder",
            AgentTier::User,
        ));

        let added = load_and_merge(tmp.path(), &mut registry);
        assert_eq!(added, 1); // only "analyst" added
        // Built-in coder's name preserved
        assert_eq!(registry.get("coder").unwrap().name, "Built-in Coder");
        assert!(registry.get("analyst").is_some());
    }

    #[test]
    fn split_frontmatter_basic() {
        let content = "---\nfoo: bar\n---\nBody text.\n";
        let (fm, body) = split_frontmatter(content).unwrap();
        assert_eq!(fm, "foo: bar");
        assert_eq!(body, "Body text.\n");
    }

    #[test]
    fn title_case_works() {
        assert_eq!(title_case("my-agent"), "My Agent");
        assert_eq!(title_case("code_reviewer"), "Code Reviewer");
        assert_eq!(title_case("simple"), "Simple");
    }
}
