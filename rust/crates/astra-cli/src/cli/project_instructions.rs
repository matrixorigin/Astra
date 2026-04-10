//! Project Instructions Discovery
//!
//! Discovers and formats project-level instructions from `.astra/instructions.md`
//! and `~/.astra/instructions.md` files. Also handles knowledge injection from
//! `.astra/knowledge.md` for cross-session learning.

use std::path::Path;

/// Resolve a system prompt, expanding `@file` syntax to file contents.
///
/// If the prompt starts with `@`, the rest is treated as a file path and
/// the file contents are returned. Otherwise, the prompt is returned as-is.
pub(crate) fn resolve_system_prompt(sp: String) -> Result<String, String> {
    if let Some(path) = sp.strip_prefix('@') {
        if path.is_empty() {
            return Err("Error: @file syntax requires a file path (e.g. @prompt.txt)".to_string());
        }
        match std::fs::read_to_string(path) {
            Ok(content) => Ok(content),
            Err(e) => Err(format!(
                "Error: cannot read system prompt file '{}': {}",
                path, e
            )),
        }
    } else {
        Ok(sp)
    }
}

/// Discover project-level instructions from `.astra/instructions.md` files.
///
/// Search order (first match per level wins):
/// 1. `.astra/instructions.md` in the current working directory (project-level)
/// 2. `~/.astra/instructions.md` in the user home (global/user-level)
///
/// Both levels are combined if present: project-level first, then global,
/// separated by a newline.
pub(crate) fn discover_project_instructions() -> Option<String> {
    let project_root = std::env::current_dir().ok();
    let home = dirs::home_dir();
    discover_instructions_from_paths(project_root.as_deref(), home.as_deref())
}

/// Core logic: discover instructions from explicit paths (testable without cwd mutation).
pub(crate) fn discover_instructions_from_paths(
    project_root: Option<&Path>,
    home: Option<&Path>,
) -> Option<String> {
    let mut parts = Vec::new();

    // Project-level: .astra/instructions.md
    if let Some(root) = project_root {
        let project_path = root.join(".astra").join("instructions.md");
        if let Ok(content) = std::fs::read_to_string(&project_path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                parts.push((project_path.display().to_string(), trimmed.to_string()));
            }
        }
        // Project-level: .astra/knowledge.md (auto-generated learnings)
        // Gated by MO_SESSION_KNOWLEDGE_INJECT (default: true). Allows users to disable
        // cross-session knowledge injection independently of MO_SESSION_PROJECT_CONTEXT.
        // Cap at 8KB to prevent unbounded token cost per turn.
        let knowledge_inject = std::env::var("MO_SESSION_KNOWLEDGE_INJECT")
            .map(|v| v != "0" && v.to_lowercase() != "false")
            .unwrap_or(true);
        if knowledge_inject {
            let knowledge_path = root.join(".astra").join("knowledge.md");
            if let Ok(content) = std::fs::read_to_string(&knowledge_path) {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    const KNOWLEDGE_MAX_BYTES: usize = 8 * 1024;
                    let capped = if trimmed.len() > KNOWLEDGE_MAX_BYTES {
                        // Walk back to a valid UTF-8 char boundary before truncating
                        let mut end = KNOWLEDGE_MAX_BYTES;
                        while end > 0 && !trimmed.is_char_boundary(end) {
                            end -= 1;
                        }
                        let slice = &trimmed[..end];
                        // Then truncate at last newline to avoid cutting mid-line
                        match slice.rfind('\n') {
                            Some(pos) => &slice[..pos],
                            None => slice,
                        }
                    } else {
                        trimmed
                    };
                    parts.push((knowledge_path.display().to_string(), capped.to_string()));
                }
            }
        } // knowledge_inject gate
    }

    // User-level: ~/.astra/instructions.md
    if let Some(h) = home {
        let user_path = h.join(".astra").join("instructions.md");
        if let Ok(content) = std::fs::read_to_string(&user_path) {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                parts.push((user_path.display().to_string(), trimmed.to_string()));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }

    let combined = parts
        .iter()
        .map(|(path, content)| format!("<!-- source: {} -->\n{}", path, content))
        .collect::<Vec<_>>()
        .join("\n\n");

    Some(combined)
}

/// Format project instructions for injection into the effective message.
pub(crate) fn format_project_instructions(instructions: &str) -> String {
    format!(
        "<project_instructions>\nThe following are project-level instructions that apply to all interactions in this workspace.\n\n{instructions}\n</project_instructions>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolve_system_prompt_plain_text() {
        let result = resolve_system_prompt("Hello world".to_string());
        assert_eq!(result, Ok("Hello world".to_string()));
    }

    #[test]
    fn resolve_system_prompt_empty_at_path() {
        let result = resolve_system_prompt("@".to_string());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("requires a file path"));
    }

    #[test]
    fn resolve_system_prompt_missing_file() {
        let result = resolve_system_prompt("@/nonexistent/path/prompt.txt".to_string());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cannot read"));
    }

    #[test]
    fn resolve_system_prompt_from_file() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("prompt.txt");
        fs::write(&file_path, "Test prompt content").unwrap();

        let result = resolve_system_prompt(format!("@{}", file_path.display()));
        assert_eq!(result, Ok("Test prompt content".to_string()));
    }

    #[test]
    fn discover_instructions_empty_dirs() {
        let dir = TempDir::new().unwrap();
        let result = discover_instructions_from_paths(Some(dir.path()), Some(dir.path()));
        assert!(result.is_none());
    }

    #[test]
    fn discover_instructions_project_only() {
        let dir = TempDir::new().unwrap();
        let astra_dir = dir.path().join(".astra");
        fs::create_dir(&astra_dir).unwrap();
        fs::write(astra_dir.join("instructions.md"), "Project instructions").unwrap();

        let result = discover_instructions_from_paths(Some(dir.path()), None);
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("Project instructions"));
        assert!(content.contains("<!-- source:"));
    }

    #[test]
    fn discover_instructions_user_only() {
        let dir = TempDir::new().unwrap();
        let astra_dir = dir.path().join(".astra");
        fs::create_dir(&astra_dir).unwrap();
        fs::write(astra_dir.join("instructions.md"), "User instructions").unwrap();

        let result = discover_instructions_from_paths(None, Some(dir.path()));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("User instructions"));
    }

    #[test]
    fn discover_instructions_combined() {
        let project_dir = TempDir::new().unwrap();
        let user_dir = TempDir::new().unwrap();

        let project_astra = project_dir.path().join(".astra");
        fs::create_dir(&project_astra).unwrap();
        fs::write(project_astra.join("instructions.md"), "Project rules").unwrap();

        let user_astra = user_dir.path().join(".astra");
        fs::create_dir(&user_astra).unwrap();
        fs::write(user_astra.join("instructions.md"), "User defaults").unwrap();

        let result =
            discover_instructions_from_paths(Some(project_dir.path()), Some(user_dir.path()));
        assert!(result.is_some());
        let content = result.unwrap();
        assert!(content.contains("Project rules"));
        assert!(content.contains("User defaults"));
    }

    #[test]
    fn discover_instructions_ignores_empty() {
        let dir = TempDir::new().unwrap();
        let astra_dir = dir.path().join(".astra");
        fs::create_dir(&astra_dir).unwrap();
        fs::write(astra_dir.join("instructions.md"), "   \n\n  ").unwrap();

        let result = discover_instructions_from_paths(Some(dir.path()), None);
        assert!(result.is_none());
    }

    #[test]
    fn format_project_instructions_wraps_content() {
        let result = format_project_instructions("Test content");
        assert!(result.starts_with("<project_instructions>"));
        assert!(result.ends_with("</project_instructions>"));
        assert!(result.contains("Test content"));
    }
}
