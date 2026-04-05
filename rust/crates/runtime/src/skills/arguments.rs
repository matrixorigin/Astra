//! Argument substitution for skill instructions.
//!
//! Supports `$ARGUMENTS` (raw arguments string), `${ARG_NAME}` (named argument),
//! and `${SKILL_DIR}` (skill directory path) substitutions.

use std::collections::HashMap;

/// Substitute argument placeholders in skill instruction text.
///
/// Supported placeholders:
/// - `$ARGUMENTS` — replaced with the raw arguments string
/// - `${ARG_NAME}` — replaced with the value of a named argument
/// - `${SKILL_DIR}` — replaced with the skill directory path
pub fn substitute_arguments(
    text: &str,
    raw_args: &str,
    named_args: &HashMap<String, String>,
    skill_dir: Option<&str>,
) -> String {
    let mut result = text.to_string();

    // Replace ${ARG_NAME} for each named argument
    for (name, value) in named_args {
        let placeholder = format!("${{{name}}}");
        result = result.replace(&placeholder, value);
    }

    // Replace ${SKILL_DIR}
    if let Some(dir) = skill_dir {
        result = result.replace("${SKILL_DIR}", dir);
    }

    // Replace $ARGUMENTS last to prevent re-expansion of user input
    // (e.g. if args contain "${SKILL_DIR}", it won't be expanded)
    result = result.replace("$ARGUMENTS", raw_args);

    result
}

/// Parse a raw arguments string into named arguments.
///
/// Supports two formats:
/// - Positional: `"value1 value2"` (matched against argument definitions by order)
/// - Named: `"name1=value1 name2=value2"`
pub fn parse_arguments(
    raw: &str,
    arg_defs: &[super::manifest::SkillArgument],
) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let raw = raw.trim();
    if raw.is_empty() {
        // Fill in defaults
        for def in arg_defs {
            if let Some(ref default) = def.default {
                result.insert(def.name.clone(), default.clone());
            }
        }
        return result;
    }

    // Try named format first (key=value)
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let is_named = parts.iter().any(|p| p.contains('='));

    if is_named {
        for part in &parts {
            if let Some(eq_pos) = part.find('=') {
                let key = part[..eq_pos].to_string();
                let value = part[eq_pos + 1..].to_string();
                result.insert(key, value);
            }
        }
    } else {
        // Positional matching
        for (i, def) in arg_defs.iter().enumerate() {
            if i < parts.len() {
                result.insert(def.name.clone(), parts[i].to_string());
            }
        }
    }

    // Fill in defaults for missing arguments
    for def in arg_defs {
        if !result.contains_key(&def.name) {
            if let Some(ref default) = def.default {
                result.insert(def.name.clone(), default.clone());
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_raw_arguments() {
        let text = "Review $ARGUMENTS carefully.";
        let result = substitute_arguments(text, "src/main.rs", &HashMap::new(), None);
        assert_eq!(result, "Review src/main.rs carefully.");
    }

    #[test]
    fn substitute_named_arguments() {
        let text = "File: ${FILE}, Branch: ${BRANCH}";
        let mut args = HashMap::new();
        args.insert("FILE".into(), "main.rs".into());
        args.insert("BRANCH".into(), "develop".into());
        let result = substitute_arguments(text, "", &args, None);
        assert_eq!(result, "File: main.rs, Branch: develop");
    }

    #[test]
    fn substitute_skill_dir() {
        let text = "Scripts at ${SKILL_DIR}/scripts/run.sh";
        let result = substitute_arguments(
            text,
            "",
            &HashMap::new(),
            Some("/home/user/.astra/skills/review"),
        );
        assert_eq!(
            result,
            "Scripts at /home/user/.astra/skills/review/scripts/run.sh"
        );
    }

    #[test]
    fn parse_named_args() {
        let defs = vec![];
        let result = parse_arguments("file=main.rs branch=develop", &defs);
        assert_eq!(result.get("file").unwrap(), "main.rs");
        assert_eq!(result.get("branch").unwrap(), "develop");
    }

    #[test]
    fn parse_positional_args() {
        let defs = vec![
            super::super::manifest::SkillArgument {
                name: "file".into(),
                description: String::new(),
                required: true,
                default: None,
            },
            super::super::manifest::SkillArgument {
                name: "branch".into(),
                description: String::new(),
                required: false,
                default: Some("main".into()),
            },
        ];
        let result = parse_arguments("src/lib.rs", &defs);
        assert_eq!(result.get("file").unwrap(), "src/lib.rs");
        assert_eq!(result.get("branch").unwrap(), "main"); // default
    }

    #[test]
    fn parse_empty_uses_defaults() {
        let defs = vec![super::super::manifest::SkillArgument {
            name: "mode".into(),
            description: String::new(),
            required: false,
            default: Some("fast".into()),
        }];
        let result = parse_arguments("", &defs);
        assert_eq!(result.get("mode").unwrap(), "fast");
    }
}
