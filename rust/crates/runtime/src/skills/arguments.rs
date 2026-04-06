//! Argument substitution for skill instructions.
//!
//! Supports:
//! - `$ARGUMENTS` — raw arguments string
//! - `$ARGUMENTS[n]` — nth token (0-indexed) from shell-quoted argument parsing
//! - `$0`, `$1`, ... `$9` — positional shorthand (same as `$ARGUMENTS[0]` etc.)
//! - `${ARG_NAME}` — named argument value
//! - `${SKILL_DIR}` — skill directory path

use std::collections::HashMap;

/// Tokenize a raw argument string respecting shell quoting.
///
/// Handles double quotes, single quotes, and backslash escapes.
/// E.g. `hello "world foo" 'bar baz'` → `["hello", "world foo", "bar baz"]`
fn shell_tokenize(input: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = input.chars().peekable();
    let mut in_double = false;
    let mut in_single = false;

    while let Some(c) = chars.next() {
        match c {
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() && !in_double && !in_single => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Substitute argument placeholders in skill instruction text.
///
/// Supported placeholders:
/// - `$ARGUMENTS` — replaced with the raw arguments string
/// - `$ARGUMENTS[n]` — replaced with the nth shell-quoted token (0-indexed)
/// - `$0` .. `$9` — shorthand for `$ARGUMENTS[0]` .. `$ARGUMENTS[9]`
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

    // Tokenize for positional access (lazy — only if placeholders exist)
    let needs_positional = result.contains("$ARGUMENTS[") || {
        // Check for $0..$9 not preceded by ${ (to avoid matching ${ARG0} etc.)
        let bytes = result.as_bytes();
        bytes
            .windows(2)
            .enumerate()
            .any(|(i, w)| w[0] == b'$' && w[1].is_ascii_digit() && (i == 0 || bytes[i - 1] != b'{'))
    };

    if needs_positional {
        let tokens = shell_tokenize(raw_args);

        // Replace $ARGUMENTS[n] (must come before $ARGUMENTS)
        for (i, token) in tokens.iter().enumerate().take(tokens.len().min(100)) {
            let placeholder = format!("$ARGUMENTS[{i}]");
            result = result.replace(&placeholder, token);
        }
        // Replace remaining $ARGUMENTS[n] with empty string
        while let Some(start) = result.find("$ARGUMENTS[") {
            if let Some(end) = result[start..].find(']') {
                result.replace_range(start..start + end + 1, "");
            } else {
                break;
            }
        }

        // Replace $0..$9 (careful not to match inside ${...})
        for i in (0..=9).rev() {
            let value = tokens.get(i).map(|s| s.as_str()).unwrap_or("");
            // Only replace $N that isn't part of ${...}
            let mut pos = 0;
            let mut new_result = String::with_capacity(result.len());
            let bytes = result.as_bytes();
            while pos < bytes.len() {
                if bytes[pos] == b'$' && pos + 1 < bytes.len() && bytes[pos + 1] == (b'0' + i as u8)
                {
                    // Check it's not inside ${...}
                    if pos + 2 < bytes.len() && bytes[pos + 1] == b'{' {
                        new_result.push(bytes[pos] as char);
                        pos += 1;
                        continue;
                    }
                    new_result.push_str(value);
                    pos += 2;
                } else {
                    new_result.push(bytes[pos] as char);
                    pos += 1;
                }
            }
            result = new_result;
        }
    }

    // Replace $ARGUMENTS last to prevent re-expansion of user input
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
    fn substitute_positional_shorthand() {
        let text = "File: $0, Mode: $1";
        let result = substitute_arguments(text, "main.rs fast", &HashMap::new(), None);
        assert_eq!(result, "File: main.rs, Mode: fast");
    }

    #[test]
    fn substitute_positional_with_quotes() {
        let text = "Message: $0, File: $1";
        let result =
            substitute_arguments(text, r#""hello world" src/lib.rs"#, &HashMap::new(), None);
        assert_eq!(result, "Message: hello world, File: src/lib.rs");
    }

    #[test]
    fn substitute_arguments_indexed() {
        let text = "First: $ARGUMENTS[0], Second: $ARGUMENTS[1], Third: $ARGUMENTS[2]";
        let result = substitute_arguments(text, "a b c", &HashMap::new(), None);
        assert_eq!(result, "First: a, Second: b, Third: c");
    }

    #[test]
    fn substitute_missing_positional_is_empty() {
        let text = "A: $0, B: $1, C: $2";
        let result = substitute_arguments(text, "only-one", &HashMap::new(), None);
        assert_eq!(result, "A: only-one, B: , C: ");
    }

    #[test]
    fn substitute_missing_indexed_is_empty() {
        let text = "A: $ARGUMENTS[0], B: $ARGUMENTS[5]";
        let result = substitute_arguments(text, "hello", &HashMap::new(), None);
        assert_eq!(result, "A: hello, B: ");
    }

    #[test]
    fn positional_does_not_match_named_args() {
        // ${FILE} should not be affected by positional substitution
        let text = "${FILE} and $0";
        let mut named = HashMap::new();
        named.insert("FILE".into(), "readme.md".into());
        let result = substitute_arguments(text, "arg0", &named, None);
        assert_eq!(result, "readme.md and arg0");
    }

    #[test]
    fn shell_tokenize_basic() {
        assert_eq!(shell_tokenize("a b c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn shell_tokenize_double_quotes() {
        assert_eq!(
            shell_tokenize(r#"hello "world foo" bar"#),
            vec!["hello", "world foo", "bar"]
        );
    }

    #[test]
    fn shell_tokenize_single_quotes() {
        assert_eq!(
            shell_tokenize("hello 'world foo' bar"),
            vec!["hello", "world foo", "bar"]
        );
    }

    #[test]
    fn shell_tokenize_backslash_escape() {
        assert_eq!(
            shell_tokenize(r#"hello\ world foo"#),
            vec!["hello world", "foo"]
        );
    }

    #[test]
    fn shell_tokenize_empty() {
        assert_eq!(shell_tokenize(""), Vec::<String>::new());
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
