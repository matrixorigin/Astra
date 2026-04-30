//! Output Style System
//!
//! Allows customization of how the agent formats its responses.
//! Supports built-in styles (default, explanatory, concise) and
//! user-defined styles via markdown files in `~/.astra/output-styles/`.
//!
//! # Usage
//!
//! Custom styles live at `~/.astra/output-styles/my-style.md`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

/// A single output style definition.
#[derive(Clone, Debug)]
pub struct OutputStyle {
    /// Style name (e.g., "concise", "explanatory")
    pub name: String,
    /// Description of what this style does
    pub description: String,
    /// The prompt text injected into the system prompt
    pub prompt: String,
    /// Source of the style definition
    pub source: StyleSource,
    /// Whether to keep the default coding instructions (true) or replace them (false)
    pub keep_coding_instructions: bool,
}

/// Where a style definition comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StyleSource {
    /// Built into the runtime
    BuiltIn,
    /// Loaded from user's config directory (~/.astra/output-styles/)
    User,
    /// Loaded from project directory (.astra/output-styles/)
    Project,
}

// ── Built-in Styles ─────────────────────────────────────────────────────────

const EXPLANATORY_PROMPT: &str = r#"# Output Style: Explanatory

You should be clear and educational, providing helpful explanations while remaining focused on the task.
Balance educational content with task completion. When providing insights, you may exceed typical length
constraints, but remain focused and relevant.

## Insights
Before and after writing code, provide brief educational explanations about implementation choices:
- Explain WHY you chose this approach over alternatives
- Point out patterns specific to this codebase
- Note trade-offs and considerations

Focus on interesting insights that are specific to the codebase or the code you just wrote,
rather than general programming concepts."#;

const CONCISE_PROMPT: &str = r#"# Output Style: Concise

Be extremely brief. Minimize explanations. Focus on action.

Rules:
- Lead with the answer or action. Skip preamble.
- Show only changed lines, no surrounding context unless essential.
- One sentence per point maximum.
- No bullet points for single items.
- Skip "I'll now..." or "Let me..." phrases.
- Error messages: show just the key line.
- If the user didn't ask for explanation, don't give one."#;

const VERBOSE_PROMPT: &str = r#"# Output Style: Verbose

Provide comprehensive, detailed responses. Show your reasoning.

Rules:
- Explain your thought process step by step.
- Show full context around code changes.
- Quote relevant code sections liberally.
- Discuss alternatives you considered and why you rejected them.
- For errors, show full stack traces and surrounding context.
- Include relevant documentation snippets when helpful.
- Summarize findings in structured tables when appropriate."#;

/// Load built-in styles.
fn builtin_styles() -> HashMap<String, OutputStyle> {
    let mut styles = HashMap::new();

    // Default style = no extra instructions
    styles.insert(
        "default".to_string(),
        OutputStyle {
            name: "default".to_string(),
            description: "Standard output with no extra styling rules".to_string(),
            prompt: String::new(),
            source: StyleSource::BuiltIn,
            keep_coding_instructions: true,
        },
    );

    styles.insert(
        "explanatory".to_string(),
        OutputStyle {
            name: "explanatory".to_string(),
            description: "Educational style with insights about implementation choices".to_string(),
            prompt: EXPLANATORY_PROMPT.to_string(),
            source: StyleSource::BuiltIn,
            keep_coding_instructions: true,
        },
    );

    styles.insert(
        "concise".to_string(),
        OutputStyle {
            name: "concise".to_string(),
            description: "Minimal output, just the essentials".to_string(),
            prompt: CONCISE_PROMPT.to_string(),
            source: StyleSource::BuiltIn,
            keep_coding_instructions: true,
        },
    );

    styles.insert(
        "verbose".to_string(),
        OutputStyle {
            name: "verbose".to_string(),
            description: "Comprehensive, detailed responses with full context".to_string(),
            prompt: VERBOSE_PROMPT.to_string(),
            source: StyleSource::BuiltIn,
            keep_coding_instructions: true,
        },
    );

    styles
}

// ── Style Loading ───────────────────────────────────────────────────────────

/// Load custom styles from a directory.
///
/// Each `.md` file in the directory becomes a style.
/// The file name (without extension) is the style name.
fn load_styles_from_dir(dir: &PathBuf, source: StyleSource) -> Vec<OutputStyle> {
    let mut styles = Vec::new();

    let Ok(entries) = fs::read_dir(dir) else {
        return styles;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md")
            && let Some(name) = path.file_stem().and_then(|s| s.to_str())
            && let Ok(content) = fs::read_to_string(&path)
        {
            // Parse frontmatter if present (---\nkey: value\n---\ncontent)
            let (description, prompt) = parse_style_file(&content);
            styles.push(OutputStyle {
                name: name.to_string(),
                description,
                prompt,
                source: source.clone(),
                keep_coding_instructions: true,
            });
        }
    }

    styles
}

/// Parse a style file, extracting optional YAML frontmatter.
///
/// Format:
/// ```text
/// ---
/// description: My custom style
/// keep_coding_instructions: false
/// ---
/// # Style Name
/// The actual prompt content...
/// ```
fn parse_style_file(content: &str) -> (String, String) {
    let content = content.trim();

    if let Some(rest) = content.strip_prefix("---") {
        // Has frontmatter
        if let Some(end_idx) = rest.find("---") {
            let frontmatter = &rest[..end_idx];
            let prompt = rest[end_idx + 3..].trim().to_string();

            // Simple YAML parsing for description
            let description = frontmatter
                .lines()
                .find(|line| line.starts_with("description:"))
                .map(|line| line.trim_start_matches("description:").trim().to_string())
                .unwrap_or_else(|| "Custom output style".to_string());

            return (description, prompt);
        }
    }

    // No frontmatter - use entire content as prompt
    ("Custom output style".to_string(), content.to_string())
}

/// Get the user's output styles directory (~/.astra/output-styles/).
fn user_styles_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("output-styles")
}

/// Get the project's output styles directory (.astra/output-styles/).
fn project_styles_dir() -> PathBuf {
    PathBuf::from(".astra").join("output-styles")
}

// ── Global Style Registry ───────────────────────────────────────────────────

/// Global registry of all available styles.
static STYLE_REGISTRY: OnceLock<HashMap<String, OutputStyle>> = OnceLock::new();

/// Get or initialize the style registry.
///
/// Loads styles in priority order (lowest to highest):
/// 1. Built-in styles
/// 2. User styles (~/.astra/output-styles/)
/// 3. Project styles (.astra/output-styles/)
///
/// Higher priority styles override lower ones with the same name.
pub fn get_style_registry() -> &'static HashMap<String, OutputStyle> {
    STYLE_REGISTRY.get_or_init(|| {
        let mut registry = builtin_styles();

        // Load user styles (override built-in)
        for style in load_styles_from_dir(&user_styles_dir(), StyleSource::User) {
            registry.insert(style.name.clone(), style);
        }

        // Load project styles (override user)
        for style in load_styles_from_dir(&project_styles_dir(), StyleSource::Project) {
            registry.insert(style.name.clone(), style);
        }

        registry
    })
}

/// Get a specific output style by name.
///
/// Returns `None` for "default" (no extra styling), or the style definition
/// if found. Returns built-in "default" if the requested style doesn't exist.
pub fn get_output_style(name: &str) -> Option<&'static OutputStyle> {
    let registry = get_style_registry();

    // "default" means no extra styling
    if name == "default" || name.is_empty() {
        return None;
    }

    registry.get(name).or_else(|| {
        eprintln!(
            "[output_style] Unknown style '{}', falling back to default",
            name
        );
        None
    })
}

/// Get the current output style. Always returns None (no env-controlled style).
pub fn current_output_style() -> Option<&'static OutputStyle> {
    None
}

/// List all available output styles.
pub fn list_styles() -> Vec<(&'static str, &'static str)> {
    get_style_registry()
        .iter()
        .map(|(name, style)| (name.as_str(), style.description.as_str()))
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_styles_exist() {
        let registry = get_style_registry();
        assert!(registry.contains_key("default"));
        assert!(registry.contains_key("concise"));
        assert!(registry.contains_key("explanatory"));
        assert!(registry.contains_key("verbose"));
    }

    #[test]
    fn test_default_returns_none() {
        // "default" should return None (no extra styling)
        assert!(get_output_style("default").is_none());
        assert!(get_output_style("").is_none());
    }

    #[test]
    fn test_concise_has_content() {
        let style = get_output_style("concise").expect("concise style should exist");
        assert!(!style.prompt.is_empty());
        assert!(style.prompt.contains("Concise"));
    }

    #[test]
    fn test_parse_style_file_no_frontmatter() {
        let content = "# My Style\nBe brief.";
        let (desc, prompt) = parse_style_file(content);
        assert_eq!(desc, "Custom output style");
        assert_eq!(prompt, "# My Style\nBe brief.");
    }

    #[test]
    fn test_parse_style_file_with_frontmatter() {
        let content = "---\ndescription: A test style\n---\n# Test\nContent here.";
        let (desc, prompt) = parse_style_file(content);
        assert_eq!(desc, "A test style");
        assert_eq!(prompt, "# Test\nContent here.");
    }

    #[test]
    fn test_unknown_style_returns_none() {
        assert!(get_output_style("nonexistent_style_xyz").is_none());
    }
}
