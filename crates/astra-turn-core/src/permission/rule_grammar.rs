//! Current permission rule grammar for `.astra/permissions.json` and
//! `~/.astra/permissions.json`.
//!
//! The grammar is intentionally explicit and fail-closed. Broad tool
//! rules use `Tool()`, scoped rules use `Tool(key="value")`, and
//! malformed or unsupported strings are rejected at settings-load time
//! instead of being silently migrated.
//!
//! ## Examples
//!
//! ```text
//! Bash(argv_exact="npm test -- --watch")
//! Bash(argv_prefix="npm test", cwd_root="packages/web")
//! Bash(argv_prefix="cargo test")
//! file_write(path_prefix="src/generated/", op="write")
//! file_write(path_glob="src/**/*.rs", op="write")
//! read_file(path_glob="src/auth/*.ts", op="read")
//! Network(tool="web_fetch", domain="api.github.com")
//! MCP(tool="mcp_jira_create_issue", capability="destructive=false")
//! deny: Bash(argv_prefix="rm -rf")
//! deny: file_write(path_glob=".env*", op="write")
//! ```
//!
//! Each rule serializes as `Tool(key="value", key="value")`. Keys
//! are quoted to allow embedded commas/parens. Unknown keys produce
//! a `RuleParseError::UnknownField` which the loader treats as a
//! load error.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A parsed permission rule in the current structured grammar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRuleSpec {
    /// Required: concrete tool or tool family (`Bash`, `Network`, `MCP`, etc.).
    /// Stored verbatim; matchers lowercase-compare.
    pub tool: String,

    /// Bash class: exact command line. Unlike `argv_prefix`, this
    /// does not allow extra args after the stored command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv_exact: Option<String>,

    /// Bash class: command-line argv prefix (`"npm test"`,
    /// `"cargo test"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv_prefix: Option<String>,

    /// File class: gitignore-style path glob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,

    /// File class: literal path prefix. Unlike `path_glob`, this
    /// does not interpret `*`, `?`, or braces as metacharacters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,

    /// Operation kind (`"read"` / `"write"` / `"execute"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,

    /// Scope: package/Cargo.toml/pkg.json directory the rule binds
    /// to, so `web/npm test` Always doesn't generalize to
    /// `api/npm test`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_root: Option<String>,

    /// Scope: git branch. P5 plan defaults on for git-destructive
    /// rules (force push) but off for ordinary edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,

    /// Network class: domain (e.g. `"api.github.com"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,

    /// MCP class: parsed capability annotation
    /// (`"destructive=false"`, `"read_only=true"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,

    /// Serializer-only extension slot. Unknown keys still error at
    /// parse time; this map is for callers that already own a parsed
    /// extension-aware spec.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

impl PermissionRuleSpec {
    /// Bash rule with normalized argv prefix.
    #[must_use]
    pub fn bash(argv_prefix: impl Into<String>) -> Self {
        Self {
            tool: "Bash".to_string(),
            argv_exact: None,
            argv_prefix: Some(argv_prefix.into()),
            path_glob: None,
            path_prefix: None,
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
            extra: BTreeMap::new(),
        }
    }

    /// File write rule with path glob.
    #[must_use]
    pub fn file_write(path_glob: impl Into<String>, op: impl Into<String>) -> Self {
        Self {
            tool: "file_write".to_string(),
            argv_exact: None,
            argv_prefix: None,
            path_glob: Some(path_glob.into()),
            path_prefix: None,
            op: Some(op.into()),
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
            extra: BTreeMap::new(),
        }
    }

    /// Network rule for a specific tool + domain.
    #[must_use]
    pub fn network(tool: impl Into<String>, domain: impl Into<String>) -> Self {
        Self {
            tool: "Network".to_string(),
            argv_exact: None,
            argv_prefix: None,
            path_glob: None,
            path_prefix: None,
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: Some(domain.into()),
            capability: None,
            extra: {
                let mut m = BTreeMap::new();
                m.insert("tool".to_string(), tool.into());
                m
            },
        }
    }
}

/// Errors that can occur while parsing a permission rule string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleParseError {
    /// Empty / whitespace-only input.
    Empty,
    /// `Tool(...` had no closing `)`.
    UnterminatedParen,
    /// Argument is missing `=` or has a malformed quote.
    MalformedField { raw: String },
    /// A key appeared more than once (`argv_prefix=... argv_prefix=...`).
    DuplicateKey { key: String },
    /// Recognized as current syntax but used a key the parser doesn't know.
    /// We surface this loudly rather than silently dropping;
    /// otherwise typos like `cwd_roott="..."` would become a
    /// non-firing rule.
    UnknownField { tool: String, key: String },
    /// A required field (e.g. tool name) was empty.
    MissingTool,
    /// Current rules must spell the argument list explicitly, even
    /// for broad tool rules (`Tool()`).
    MissingArgumentList { raw: String },
    /// Abstract tool families are no longer accepted as persisted
    /// rules. Persist `file_write` for file-write families or a
    /// concrete tool such as `write_file` / `read_file` instead.
    UnsupportedToolFamily { tool: String },
    /// Two fields that describe the same dimension were set together.
    ConflictingFields {
        first: &'static str,
        second: &'static str,
    },
}

impl std::fmt::Display for RuleParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty rule"),
            Self::UnterminatedParen => write!(f, "missing `)` in rule"),
            Self::MalformedField { raw } => {
                write!(f, "malformed key=value field: {raw}")
            }
            Self::DuplicateKey { key } => write!(f, "duplicate key in rule: {key}"),
            Self::UnknownField { tool, key } => {
                write!(f, "unknown key `{key}` for tool `{tool}`")
            }
            Self::MissingTool => write!(f, "rule has no tool name"),
            Self::MissingArgumentList { raw } => {
                write!(
                    f,
                    "permission rule must use Tool(key=\"value\") or Tool() form: {raw}"
                )
            }
            Self::UnsupportedToolFamily { tool } => {
                write!(
                    f,
                    "unsupported permission tool `{tool}`; use file_write or a current exact tool rule"
                )
            }
            Self::ConflictingFields { first, second } => {
                write!(f, "`{first}` and `{second}` cannot be set together")
            }
        }
    }
}

impl std::error::Error for RuleParseError {}

/// Parse a current permission rule:
///
/// ```text
/// Bash(argv_prefix="npm test", cwd_root="packages/web")
/// file_write(path_glob="src/**/*.rs", op="write")
/// ```
pub fn parse_rule(s: &str) -> Result<PermissionRuleSpec, RuleParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(RuleParseError::Empty);
    }

    let Some(paren_start) = s.find('(') else {
        return Err(RuleParseError::MissingArgumentList { raw: s.to_string() });
    };

    let tool = s[..paren_start].trim().to_string();
    if tool.is_empty() {
        return Err(RuleParseError::MissingTool);
    }
    if matches!(tool.to_ascii_lowercase().as_str(), "edit" | "read") {
        return Err(RuleParseError::UnsupportedToolFamily { tool });
    }

    let Some(paren_end) = s.rfind(')') else {
        return Err(RuleParseError::UnterminatedParen);
    };
    if paren_end <= paren_start {
        return Err(RuleParseError::UnterminatedParen);
    }
    if !s[paren_end + 1..].trim().is_empty() {
        return Err(RuleParseError::MalformedField {
            raw: s[paren_end + 1..].trim().to_string(),
        });
    }
    let body = s[paren_start + 1..paren_end].trim();

    let mut rule = PermissionRuleSpec {
        tool: tool.clone(),
        argv_exact: None,
        argv_prefix: None,
        path_glob: None,
        path_prefix: None,
        op: None,
        cwd_root: None,
        git_branch: None,
        domain: None,
        capability: None,
        extra: BTreeMap::new(),
    };

    for raw_field in split_top_level_commas(body) {
        let raw_field = raw_field.trim();
        if raw_field.is_empty() {
            continue;
        }
        let Some(eq) = raw_field.find('=') else {
            return Err(RuleParseError::MalformedField {
                raw: raw_field.to_string(),
            });
        };
        let key = raw_field[..eq].trim().to_string();
        let raw_value = raw_field[eq + 1..].trim();
        let value =
            strip_optional_quotes(raw_value).ok_or_else(|| RuleParseError::MalformedField {
                raw: raw_field.to_string(),
            })?;

        // Reject duplicate keys.
        let already_set = match key.as_str() {
            "argv_exact" => rule.argv_exact.is_some(),
            "argv_prefix" => rule.argv_prefix.is_some(),
            "path_glob" => rule.path_glob.is_some(),
            "path_prefix" => rule.path_prefix.is_some(),
            "op" => rule.op.is_some(),
            "cwd_root" => rule.cwd_root.is_some(),
            "git_branch" => rule.git_branch.is_some(),
            "domain" => rule.domain.is_some(),
            "capability" => rule.capability.is_some(),
            _ => rule.extra.contains_key(&key),
        };
        if already_set {
            return Err(RuleParseError::DuplicateKey { key });
        }

        match key.as_str() {
            "argv_exact" => rule.argv_exact = Some(value.to_string()),
            "argv_prefix" => rule.argv_prefix = Some(value.to_string()),
            "path_glob" => rule.path_glob = Some(value.to_string()),
            "path_prefix" => rule.path_prefix = Some(value.to_string()),
            "op" => rule.op = Some(value.to_string()),
            "cwd_root" => rule.cwd_root = Some(value.to_string()),
            "git_branch" => rule.git_branch = Some(value.to_string()),
            "domain" => rule.domain = Some(value.to_string()),
            "capability" => rule.capability = Some(value.to_string()),
            // Network/MCP family rules may carry the concrete tool
            // name in a structured `tool` slot.
            "tool"
                if rule.tool.eq_ignore_ascii_case("network")
                    || rule.tool.eq_ignore_ascii_case("mcp") =>
            {
                rule.extra.insert(key, value.to_string());
            }
            _ => {
                return Err(RuleParseError::UnknownField {
                    tool: rule.tool.clone(),
                    key,
                });
            }
        }
    }

    if rule.path_glob.is_some() && rule.path_prefix.is_some() {
        return Err(RuleParseError::ConflictingFields {
            first: "path_glob",
            second: "path_prefix",
        });
    }

    Ok(rule)
}

/// Format a rule back into current string form. Stable: keys are emitted
/// in a fixed order so roundtrip-by-string is deterministic.
#[must_use]
pub fn serialize_rule(rule: &PermissionRuleSpec) -> String {
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(v) = &rule.argv_exact {
        fields.push(("argv_exact".into(), v.clone()));
    }
    if let Some(v) = &rule.argv_prefix {
        fields.push(("argv_prefix".into(), v.clone()));
    }
    if let Some(v) = &rule.path_glob {
        fields.push(("path_glob".into(), v.clone()));
    }
    if let Some(v) = &rule.path_prefix {
        fields.push(("path_prefix".into(), v.clone()));
    }
    if let Some(v) = &rule.op {
        fields.push(("op".into(), v.clone()));
    }
    if let Some(v) = &rule.cwd_root {
        fields.push(("cwd_root".into(), v.clone()));
    }
    if let Some(v) = &rule.git_branch {
        fields.push(("git_branch".into(), v.clone()));
    }
    if let Some(v) = &rule.domain {
        fields.push(("domain".into(), v.clone()));
    }
    if let Some(v) = &rule.capability {
        fields.push(("capability".into(), v.clone()));
    }
    for (k, v) in &rule.extra {
        fields.push((k.clone(), v.clone()));
    }

    if fields.is_empty() {
        return format!("{}()", rule.tool);
    }

    let body = fields
        .iter()
        .map(|(k, v)| format!("{k}=\"{}\"", escape_quotes(v)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}({body})", rule.tool)
}

fn escape_quotes(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn unescape_quotes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Strip surrounding double quotes if present; otherwise return
/// the raw slice (allowing unquoted simple values like `op=read`).
/// Returns `None` for malformed input (`"abc` with no closing quote).
fn strip_optional_quotes(s: &str) -> Option<String> {
    if s.starts_with('"') {
        if !s.ends_with('"') || s.len() < 2 {
            return None;
        }
        Some(unescape_quotes(&s[1..s.len() - 1]))
    } else {
        Some(s.to_string())
    }
}

/// Split a string on commas that are NOT inside double quotes.
/// Needed because rule values like `argv_prefix="echo a, b"` should
/// not be split.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth_quote = false;
    let mut last = 0;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if b == b'"' {
            depth_quote = !depth_quote;
        } else if b == b',' && !depth_quote {
            out.push(&s[last..i]);
            last = i + 1;
        }
        i += 1;
    }
    out.push(&s[last..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_bash_argv_roundtrip() {
        let rule = PermissionRuleSpec::bash("npm test");
        let s = serialize_rule(&rule);
        assert_eq!(s, "Bash(argv_prefix=\"npm test\")");
        let parsed = parse_rule(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn current_file_path_glob_op_roundtrip() {
        let rule = PermissionRuleSpec::file_write("src/**/*.rs", "write");
        let s = serialize_rule(&rule);
        assert!(s.contains("path_glob=\"src/**/*.rs\""));
        assert!(s.contains("op=\"write\""));
        let parsed = parse_rule(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn current_file_path_prefix_op_roundtrip() {
        let rule = PermissionRuleSpec {
            tool: "file_write".to_string(),
            argv_exact: None,
            argv_prefix: None,
            path_glob: None,
            path_prefix: Some("zzz".to_string()),
            op: Some("write".to_string()),
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
            extra: BTreeMap::new(),
        };
        let s = serialize_rule(&rule);
        assert_eq!(s, r#"file_write(path_prefix="zzz", op="write")"#);
        let parsed = parse_rule(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn current_full_field_roundtrip() {
        let rule = PermissionRuleSpec {
            tool: "Bash".to_string(),
            argv_exact: None,
            argv_prefix: Some("cargo test".to_string()),
            path_glob: None,
            path_prefix: None,
            op: None,
            cwd_root: Some("packages/web".to_string()),
            git_branch: Some("main".to_string()),
            domain: None,
            capability: None,
            extra: BTreeMap::new(),
        };
        let s = serialize_rule(&rule);
        let parsed = parse_rule(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn current_value_with_comma_roundtrips_through_quoting() {
        let rule = PermissionRuleSpec::bash("echo a, b, c");
        let s = serialize_rule(&rule);
        let parsed = parse_rule(&s).unwrap();
        assert_eq!(parsed.argv_prefix.as_deref(), Some("echo a, b, c"));
    }

    #[test]
    fn current_value_with_quotes_roundtrips() {
        let rule = PermissionRuleSpec::bash(r#"echo "hello""#);
        let s = serialize_rule(&rule);
        let parsed = parse_rule(&s).unwrap();
        assert_eq!(parsed.argv_prefix.as_deref(), Some(r#"echo "hello""#));
    }

    #[test]
    fn unsupported_bash_pattern_is_rejected() {
        let err = parse_rule("Bash(npm:*)").unwrap_err();
        assert!(matches!(err, RuleParseError::MalformedField { .. }));
    }

    #[test]
    fn unsupported_bash_with_command_and_args_is_rejected() {
        let err = parse_rule("Bash(git commit:*)").unwrap_err();
        assert!(matches!(err, RuleParseError::MalformedField { .. }));
    }

    #[test]
    fn unsupported_edit_family_is_rejected() {
        let err = parse_rule("Edit(src/lib.rs)").unwrap_err();
        assert!(matches!(err, RuleParseError::UnsupportedToolFamily { .. }));
    }

    #[test]
    fn unsupported_read_family_is_rejected() {
        let err = parse_rule(r#"Read(path_glob="**")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::UnsupportedToolFamily { .. }));
    }

    #[test]
    fn unsupported_bare_tool_no_paren_is_rejected() {
        let err = parse_rule("Bash").unwrap_err();
        assert!(matches!(err, RuleParseError::MissingArgumentList { .. }));
    }

    #[test]
    fn broad_rule_uses_explicit_empty_argument_list() {
        let rule = parse_rule("Bash()").unwrap();
        assert_eq!(rule.tool, "Bash");
        assert!(rule.argv_prefix.is_none());
        assert_eq!(serialize_rule(&rule), "Bash()");
    }

    #[test]
    fn unsupported_unknown_tool_pattern_is_rejected() {
        let err = parse_rule("CustomTool(some-pattern:*)").unwrap_err();
        assert!(matches!(err, RuleParseError::MalformedField { .. }));
    }

    #[test]
    fn network_and_mcp_rules_accept_concrete_tool_key() {
        let network = parse_rule(r#"Network(tool="web_fetch", domain="github.com")"#).unwrap();
        assert_eq!(
            network.extra.get("tool").map(String::as_str),
            Some("web_fetch")
        );
        assert_eq!(network.domain.as_deref(), Some("github.com"));

        let mcp =
            parse_rule(r#"MCP(tool="mcp_jira_create_issue", capability="destructive=false")"#)
                .unwrap();
        assert_eq!(
            mcp.extra.get("tool").map(String::as_str),
            Some("mcp_jira_create_issue")
        );
        assert_eq!(mcp.capability.as_deref(), Some("destructive=false"));
    }

    // ── Loud failure modes ───────────────────────────────────────

    #[test]
    fn unknown_field_errors_loudly() {
        let err = parse_rule(r#"Bash(cwd_roott="x")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::UnknownField { .. }));
    }

    #[test]
    fn duplicate_field_errors_loudly() {
        let err = parse_rule(r#"Bash(argv_prefix="a", argv_prefix="b")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::DuplicateKey { .. }));
    }

    #[test]
    fn path_glob_and_path_prefix_conflict_loudly() {
        let err =
            parse_rule(r#"file_write(path_glob="src/**/*.rs", path_prefix="src/", op="write")"#)
                .unwrap_err();
        assert!(matches!(err, RuleParseError::ConflictingFields { .. }));
    }

    #[test]
    fn unterminated_paren_errors() {
        let err = parse_rule("Bash(argv_prefix=\"a\"").unwrap_err();
        assert!(matches!(err, RuleParseError::UnterminatedParen));
    }

    #[test]
    fn empty_rule_errors() {
        assert!(matches!(parse_rule(""), Err(RuleParseError::Empty)));
        assert!(matches!(parse_rule("   "), Err(RuleParseError::Empty)));
    }

    #[test]
    fn missing_tool_errors() {
        let err = parse_rule("(argv_prefix=\"x\")").unwrap_err();
        assert!(matches!(err, RuleParseError::MissingTool));
    }

    #[test]
    fn malformed_field_no_eq_errors() {
        let err = parse_rule("Bash(argv_prefix\"x\")").unwrap_err();
        assert!(matches!(err, RuleParseError::MalformedField { .. }));
    }

    #[test]
    fn malformed_field_unterminated_quote_errors() {
        let err = parse_rule(r#"Bash(argv_prefix="abc)"#).unwrap_err();
        assert!(matches!(
            err,
            RuleParseError::MalformedField { .. } | RuleParseError::UnterminatedParen
        ));
    }

    #[test]
    fn trailing_content_after_rule_errors() {
        let err = parse_rule(r#"Bash(argv_prefix="x") trailing"#).unwrap_err();
        assert!(matches!(err, RuleParseError::MalformedField { .. }));
    }

    #[test]
    fn unknown_extension_fields_error_loudly() {
        let err = parse_rule(r#"Bash(argv_prefix="x", priority="high")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::UnknownField { .. }));
    }
}
