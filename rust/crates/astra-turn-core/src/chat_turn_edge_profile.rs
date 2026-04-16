//! Pieces of `/chat` `edge_profile` built on the CLI edge (cwd, memoria, git branch, active skills).

use serde_json::{Value, json};

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
    let memoria_url = std::env::var("MEMORIA_BASE_URL")
        .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
    let memoria_key = std::env::var("MEMORIA_API_KEY")
        .ok()
        .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
        .unwrap_or_default();
    (memoria_url, memoria_key)
}

/// Retrieval top_k from environment (same semantics as RuntimeConfig).
fn retrieval_top_k_from_env() -> u32 {
    std::env::var("MO_RETRIEVAL_TOP_K")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(5) // default same as RuntimeConfig
}

/// Static `edge_profile` object before optional `active_skills` / selector hints / skills text.
pub fn build_base_edge_profile_value(
    cwd: &str,
    git_branch: Option<String>,
    workspace: Value,
) -> Value {
    let (memoria_url, memoria_key) = memoria_env_for_edge_profile();
    let retrieval_top_k = retrieval_top_k_from_env();

    // Collect environment context for the LLM
    let env_context =
        crate::edge_prompt_context::build_environment_context(std::path::Path::new(cwd));

    json!({
        "cwd": cwd,
        "git_branch": git_branch,
        "memoria_url": memoria_url,
        "memoria_key": memoria_key,
        "retrieval_top_k": retrieval_top_k,
        "workspace": workspace,
        "environment_context": env_context,
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
        // Environment context is now included
        assert!(
            v.get("environment_context").is_some(),
            "should include environment_context"
        );
        let env_ctx = v["environment_context"].as_str().unwrap();
        assert!(
            env_ctx.contains("## Environment"),
            "environment_context should have section header"
        );
        // retrieval_top_k is included (default 5 unless MO_RETRIEVAL_TOP_K set)
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
