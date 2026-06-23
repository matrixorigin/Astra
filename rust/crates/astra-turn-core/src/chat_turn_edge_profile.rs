//! Pieces of `/chat` `edge_profile` built on the CLI edge (cwd, memoria, git branch, active skills).

use serde_json::{Value, json};

/// Protocol key for skill-listing text routed through `edge_profile` from
/// the CLI to the runtime bridge (volatile lane). Shared between writer
/// (`astra-cli` agentic loop) and reader (`runtime` bridge_inprocess) so a
/// typo on either side is a compile error rather than a silent regression.
pub const EDGE_PROFILE_KEY_SKILL_LISTING_TEXT: &str = "skill_listing_text";

/// Protocol key for CLI-side runtime volatile nudges routed as structured
/// edge metadata instead of being inlined into `messages[]`. The runtime
/// resolves model cache capability before deciding whether to inject or drop
/// this lane.
pub const EDGE_PROFILE_KEY_RUNTIME_VOLATILE_TEXTS: &str = "runtime_volatile_texts";

/// Protocol key for the session-stable deferred-tool manifest routed through
/// `edge_profile` from the CLI to the runtime bridge.
pub const EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT: &str = "deferred_tools_text";

/// Protocol key for the model context window used to render
/// [`EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT`].
pub const EDGE_PROFILE_KEY_DEFERRED_TOOLS_CONTEXT_WINDOW: &str = "deferred_tools_context_window";

/// Protocol key carrying the JSON array of names listed in this turn's
/// `<deferred_tools>` manifest. Pairs with
/// [`EDGE_PROFILE_KEY_DEFERRED_TOOLS_TEXT`] (which is the rendered XML used
/// for prompt assembly). The runtime reads the names from here so it can
/// branch the validator denial copy and let `tool_search(select:NAME)`
/// resolve deferred names without re-parsing the rendered XML.
pub const EDGE_PROFILE_KEY_DEFERRED_TOOL_NAMES: &str = "deferred_tool_names";

/// Protocol key carrying the JSON array of pinned (T1) tool names from the
/// CLI-side [`ToolSurface`]. The runtime uses this to place cache_control
/// markers at the correct pinned/dynamic boundary so the Anthropic prompt
/// cache prefix stays correct when the user overrides the default pinned set
/// in TOML (`runtime.tool_surface.pinned_tools`).
///
/// Without this key, the runtime falls back to a compile-time constant that
/// does not reflect user overrides, causing cache-prefix drift and ~500+ token
/// cache misses per turn.
pub const EDGE_PROFILE_KEY_PINNED_TOOL_NAMES: &str = "pinned_tool_names";

/// `git rev-parse --abbrev-ref HEAD` for edge_profile (best-effort).
pub fn read_git_branch_abbrev() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Memoria URL + API key from environment (same semantics as CLI `chat_stream`).
pub fn memoria_env_for_edge_profile() -> (String, String) {
    let mem = astra_core::MemoriaSettings::from_env();
    (mem.base_url, mem.master_key.unwrap_or_default())
}

/// Retrieval top_k from environment (same semantics as RuntimeConfig).
fn retrieval_top_k_from_env() -> u32 {
    std::env::var("ASTRA_RETRIEVAL_TOP_K")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(5) // default same as RuntimeConfig
}

/// Static `edge_profile` object before optional `active_skills` / skill context.
pub fn build_base_edge_profile_value(
    cwd: &str,
    git_branch: Option<String>,
    workspace: Value,
) -> Value {
    let (memoria_url, memoria_key) = memoria_env_for_edge_profile();
    let retrieval_top_k = retrieval_top_k_from_env();

    // Environment context split into two lanes for prompt caching:
    //   * `environment_static`  → Platform/Shell/CWD/Home (stable for
    //     the session, safe to sit inside the cached Session prefix).
    //   * `environment_volatile` → Git branch dirty state, staged /
    //     unstaged diff stats, recent commits. Churns every edit/commit
    //     and MUST stay out of the cached prefix.
    let project_root = std::path::Path::new(cwd);
    let env_static = crate::edge_prompt_context::build_static_environment_context(project_root);
    let env_volatile = crate::edge_prompt_context::build_volatile_environment_context(project_root);

    json!({
        "cwd": cwd,
        "git_branch": git_branch,
        "memoria_url": memoria_url,
        "memoria_key": memoria_key,
        "retrieval_top_k": retrieval_top_k,
        "workspace": workspace,
        "environment_static": env_static,
        "environment_volatile": env_volatile,
    })
}

fn title_case_first_ascii_word(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        None => String::new(),
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
    }
}

/// Detect built-in system skills advertised in the user message (Output Format / Constraint lines).
pub fn detect_active_system_skills_in_message(message: &str) -> Vec<&'static str> {
    const SKILLS: &[&str] = &["markdown", "concise"];
    SKILLS
        .iter()
        .copied()
        .filter(|name| {
            let titled = title_case_first_ascii_word(name);
            message.contains(&format!("Output Format: {titled}"))
                || message.contains(&format!("Output Constraint: {titled}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_profile_has_expected_keys() {
        let v = build_base_edge_profile_value("/proj", Some("main".into()), json!({"k": 1}));
        assert_eq!(v["cwd"], "/proj");
        assert_eq!(v["git_branch"], "main");
        assert!(v.get("memoria_url").is_some());
        assert!(v.get("memoria_key").is_some());
        assert_eq!(v["workspace"]["k"], 1);
        // Static environment (cache-safe) and volatile environment
        // (post-cache) are exposed as separate fields so the bridge can
        // route them to the correct cache scope without having to re-
        // parse a single blob.
        let env_static = v["environment_static"].as_str().unwrap();
        assert!(
            env_static.contains("## Environment"),
            "environment_static should carry the ## Environment header"
        );
        assert!(
            !env_static.contains("- Git branch:"),
            "environment_static must not contain git branch (would break cache)"
        );
        // environment_volatile may be empty outside a git repo but must
        // be present as a typed field so downstream can always read it.
        assert!(v.get("environment_volatile").is_some());
        // retrieval_top_k is included (default 5 unless ASTRA_RETRIEVAL_TOP_K set)
        assert!(
            v.get("retrieval_top_k").is_some(),
            "should include retrieval_top_k"
        );
    }

    #[test]
    fn detect_skills_from_output_format_line() {
        let msg = "Do x.\n\nOutput Format: Markdown\n";
        let s = detect_active_system_skills_in_message(msg);
        assert_eq!(s, vec!["markdown"]);
    }

    #[test]
    fn detect_skills_from_output_constraint() {
        let msg = "Output Constraint: Concise\n";
        let s = detect_active_system_skills_in_message(msg);
        assert_eq!(s, vec!["concise"]);
    }

    #[test]
    fn detect_skills_empty_when_no_marker() {
        assert!(detect_active_system_skills_in_message("hello").is_empty());
    }
}
