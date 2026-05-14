//! Issue #326 P1.5b / R2 Major 2: versioned rule grammar for
//! `.kiro/permissions.json` and `~/.astra/permissions.json`.
//!
//! ## Why version the grammar
//!
//! The legacy v1 grammar was a string with at most one optional
//! pattern slot:
//!
//! - `Bash` (matches every bash call)
//! - `Bash(git commit:*)` (matches any command starting with `git commit`)
//! - `Edit` (matches every edit)
//!
//! That's all the existing parser ([`crate::permission_types::PermissionRule::parse`])
//! can express. Plan v3 §P5 needs path globs, structured
//! argument constraints, cwd_root scoping, git_branch scoping, and
//! domain-level rules for network tools — none of which fit into a
//! single `pattern` slot. Without a real grammar the implementer
//! would either smuggle JSON into that one string (ad-hoc parser,
//! silent-failure mode) or break backward compat. Both are bad.
//!
//! ## Compatibility contract
//!
//! - `permissions.json` files written by older astra versions have
//!   no `grammar_version` field and use bare strings; we treat
//!   them as v1 and parse with the legacy parser. They are
//!   automatically read; nothing breaks for an existing user.
//!
//! - When the store next saves the file, it tags `grammar_version: 2`
//!   and writes a `.v1.bak.json` sibling so the user can always
//!   roll back. Migration is a one-shot per file.
//!
//! - The v2 parser is **strict** about unknown keys. A malformed
//!   rule logs `tracing::warn` and is skipped — but the diagnostic
//!   path is "loud, never silent". (Issue #326 P0 §load-error
//!   already wired LoadError up to the TUI banner / headless
//!   exit-1; corrupt rules surface there.)
//!
//! ## Examples (v2)
//!
//! ```text
//! Bash(argv_exact="npm test -- --watch")
//! Bash(argv_prefix="npm test", cwd_root="packages/web")
//! Bash(argv_prefix="cargo test")
//! Edit(path_prefix="src/generated/", op="write")
//! Edit(path_glob="src/**/*.rs", op="write")
//! Edit(path_glob="src/auth/*.ts", op="read")
//! Network(tool="web_fetch", domain="api.github.com")
//! Read(path_glob="**")
//! MCP(tool="mcp_jira_create_issue", capability="destructive=false")
//! deny: Bash(argv_prefix="rm -rf")
//! deny: Edit(path_glob=".env*", op="write")
//! ```
//!
//! Each rule serializes as `Tool(key="value", key="value")`. Keys
//! are quoted to allow embedded commas/parens. Unknown keys produce
//! a `RuleParseError::UnknownField` which the loader treats as a
//! load error (NOT silently dropped — that's the failure mode R1
//! Major 1 calls out).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Current grammar version — written into `permissions.json` by the
/// store layer. Older files (without a version tag) are migrated to
/// this version on the next save.
pub const GRAMMAR_VERSION: u32 = 2;

/// A v2 permission rule. Carries the structured fields plan v3 §P5
/// needs, plus a fallback `extra` map so the future can add fields
/// without breaking older parsers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRuleV2 {
    /// Required: tool family (`Bash`, `Edit`, `Read`, `Network`,
    /// `MCP`, etc.). Stored verbatim; matchers lowercase-compare.
    pub tool: String,

    /// Bash class: exact command line. Unlike `argv_prefix`, this
    /// does not allow extra args after the stored command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv_exact: Option<String>,

    /// Bash class: command-line argv prefix (`"npm test"`,
    /// `"cargo test"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv_prefix: Option<String>,

    /// Edit/Read class: gitignore-style path glob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_glob: Option<String>,

    /// Edit/Read class: literal path prefix. Unlike `path_glob`, this
    /// does not interpret `*`, `?`, or braces as metacharacters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,

    /// Edit class: operation kind (`"read"` / `"write"`). For Read
    /// this is implicit but P5 will allow restricting writes
    /// independently of reads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,

    /// Scope: package/Cargo.toml/pkg.json directory the rule binds
    /// to. P5 plan §cwd-fingerprint defaults this on for new rules
    /// so `web/npm test` Always doesn't generalize to `api/npm test`.
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

    /// Forward-compat slot: anything we don't recognize today is
    /// kept here so a newer astra writing v2-plus-extensions doesn't
    /// silently lose them on round-trip. Unknown keys still error
    /// at PARSE time (loud) — this map is for serializer round-trip
    /// only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

impl PermissionRuleV2 {
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

    /// Edit rule with path glob.
    #[must_use]
    pub fn edit(path_glob: impl Into<String>, op: impl Into<String>) -> Self {
        Self {
            tool: "Edit".to_string(),
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

/// Errors that can occur while parsing a v2 rule string.
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
    /// Recognized as v2 syntax but used a key the parser doesn't know.
    /// We surface this loudly rather than silently dropping —
    /// otherwise typos like `cwd_roott="..."` would become a
    /// non-firing rule. (Issue #326 P5b loud-load-errors policy.)
    UnknownField { tool: String, key: String },
    /// A required field (e.g. tool name) was empty.
    MissingTool,
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
            Self::ConflictingFields { first, second } => {
                write!(f, "`{first}` and `{second}` cannot be set together")
            }
        }
    }
}

impl std::error::Error for RuleParseError {}

/// Parse a v2-style rule:
///
/// ```text
/// Bash(argv_prefix="npm test", cwd_root="packages/web")
/// Edit(path_glob="src/**/*.rs", op="write")
/// ```
///
/// Falls back to the legacy v1 parser for inputs that don't contain
/// `=` or `"` (i.e. `Bash(npm:*)`, `Edit`). The fallback always
/// succeeds — that's what makes the file format backward-compat.
pub fn parse_rule_v2(s: &str) -> Result<PermissionRuleV2, RuleParseError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(RuleParseError::Empty);
    }

    // No paren → bare tool, e.g. `Bash`.
    let Some(paren_start) = s.find('(') else {
        return Ok(PermissionRuleV2 {
            tool: s.to_string(),
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
        });
    };

    let tool = s[..paren_start].trim().to_string();
    if tool.is_empty() {
        return Err(RuleParseError::MissingTool);
    }

    let Some(paren_end) = s.rfind(')') else {
        return Err(RuleParseError::UnterminatedParen);
    };
    if paren_end <= paren_start {
        return Err(RuleParseError::UnterminatedParen);
    }
    let body = s[paren_start + 1..paren_end].trim();

    // Detect v1 syntax: `pattern:*` with no `=` and no `"`.
    if !body.contains('=') && !body.contains('"') {
        // v1 fallback: treat the whole body as a command/path prefix.
        let pattern = body.trim_end_matches(":*").trim_end_matches('*').trim();
        let mut rule = PermissionRuleV2 {
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
        if !pattern.is_empty() {
            // Choose the best-fit field for the legacy pattern by
            // tool family. Bash → argv_prefix; Edit/Read → path_glob;
            // anything else (Network, MCP, custom) → extra so we
            // don't lose it.
            let lower_tool = tool.to_lowercase();
            if lower_tool == "bash" {
                rule.argv_prefix = Some(pattern.to_string());
            } else if lower_tool == "edit" || lower_tool == "read" || lower_tool == "view" {
                rule.path_glob = Some(pattern.to_string());
            } else {
                rule.extra
                    .insert("pattern".to_string(), pattern.to_string());
            }
        }
        return Ok(rule);
    }

    // v2 path: parse comma-separated key="value" pairs.
    let mut rule = PermissionRuleV2 {
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

/// Format a rule back into v2 string form. Stable: keys are emitted
/// in a fixed order so roundtrip-by-string is deterministic.
#[must_use]
pub fn serialize_rule_v2(rule: &PermissionRuleV2) -> String {
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
        return rule.tool.clone();
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

    // ── Roundtrip ──────────────────────────────────────────────────

    #[test]
    fn v2_bash_argv_roundtrip() {
        let rule = PermissionRuleV2::bash("npm test");
        let s = serialize_rule_v2(&rule);
        assert_eq!(s, "Bash(argv_prefix=\"npm test\")");
        let parsed = parse_rule_v2(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn v2_edit_path_glob_op_roundtrip() {
        let rule = PermissionRuleV2::edit("src/**/*.rs", "write");
        let s = serialize_rule_v2(&rule);
        assert!(s.contains("path_glob=\"src/**/*.rs\""));
        assert!(s.contains("op=\"write\""));
        let parsed = parse_rule_v2(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn v2_edit_path_prefix_op_roundtrip() {
        let rule = PermissionRuleV2 {
            tool: "write_file".to_string(),
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
        let s = serialize_rule_v2(&rule);
        assert_eq!(s, r#"write_file(path_prefix="zzz", op="write")"#);
        let parsed = parse_rule_v2(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn v2_full_field_roundtrip() {
        let rule = PermissionRuleV2 {
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
        let s = serialize_rule_v2(&rule);
        let parsed = parse_rule_v2(&s).unwrap();
        assert_eq!(parsed, rule);
    }

    #[test]
    fn v2_value_with_comma_roundtrips_through_quoting() {
        let rule = PermissionRuleV2::bash("echo a, b, c");
        let s = serialize_rule_v2(&rule);
        let parsed = parse_rule_v2(&s).unwrap();
        assert_eq!(parsed.argv_prefix.as_deref(), Some("echo a, b, c"));
    }

    #[test]
    fn v2_value_with_quotes_roundtrips() {
        let rule = PermissionRuleV2::bash(r#"echo "hello""#);
        let s = serialize_rule_v2(&rule);
        let parsed = parse_rule_v2(&s).unwrap();
        assert_eq!(parsed.argv_prefix.as_deref(), Some(r#"echo "hello""#));
    }

    // ── v1 → v2 fallback ──────────────────────────────────────────

    #[test]
    fn legacy_bash_pattern_migrates_to_argv_prefix() {
        let rule = parse_rule_v2("Bash(npm:*)").unwrap();
        assert_eq!(rule.tool, "Bash");
        assert_eq!(rule.argv_prefix.as_deref(), Some("npm"));
        assert!(rule.path_glob.is_none());
    }

    #[test]
    fn legacy_bash_with_command_and_args() {
        let rule = parse_rule_v2("Bash(git commit:*)").unwrap();
        assert_eq!(rule.argv_prefix.as_deref(), Some("git commit"));
    }

    #[test]
    fn legacy_edit_pattern_migrates_to_path_glob() {
        let rule = parse_rule_v2("Edit(src/lib.rs)").unwrap();
        assert_eq!(rule.tool, "Edit");
        assert_eq!(rule.path_glob.as_deref(), Some("src/lib.rs"));
    }

    #[test]
    fn legacy_bare_tool_no_paren() {
        let rule = parse_rule_v2("Bash").unwrap();
        assert_eq!(rule.tool, "Bash");
        assert!(rule.argv_prefix.is_none());
    }

    #[test]
    fn legacy_unknown_tool_keeps_pattern_in_extra() {
        // For Network/MCP/etc the legacy `pattern` slot has no
        // obvious mapping; we keep it in `extra` rather than
        // silently dropping.
        let rule = parse_rule_v2("CustomTool(some-pattern:*)").unwrap();
        assert_eq!(rule.tool, "CustomTool");
        assert_eq!(
            rule.extra.get("pattern").map(String::as_str),
            Some("some-pattern")
        );
    }

    #[test]
    fn network_and_mcp_rules_accept_concrete_tool_key() {
        let network = parse_rule_v2(r#"Network(tool="web_fetch", domain="github.com")"#).unwrap();
        assert_eq!(
            network.extra.get("tool").map(String::as_str),
            Some("web_fetch")
        );
        assert_eq!(network.domain.as_deref(), Some("github.com"));

        let mcp =
            parse_rule_v2(r#"MCP(tool="mcp_jira_create_issue", capability="destructive=false")"#)
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
        let err = parse_rule_v2(r#"Bash(cwd_roott="x")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::UnknownField { .. }));
    }

    #[test]
    fn duplicate_field_errors_loudly() {
        let err = parse_rule_v2(r#"Bash(argv_prefix="a", argv_prefix="b")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::DuplicateKey { .. }));
    }

    #[test]
    fn path_glob_and_path_prefix_conflict_loudly() {
        let err =
            parse_rule_v2(r#"write_file(path_glob="src/**/*.rs", path_prefix="src/", op="write")"#)
                .unwrap_err();
        assert!(matches!(err, RuleParseError::ConflictingFields { .. }));
    }

    #[test]
    fn unterminated_paren_errors() {
        let err = parse_rule_v2("Bash(argv_prefix=\"a\"").unwrap_err();
        assert!(matches!(err, RuleParseError::UnterminatedParen));
    }

    #[test]
    fn empty_rule_errors() {
        assert!(matches!(parse_rule_v2(""), Err(RuleParseError::Empty)));
        assert!(matches!(parse_rule_v2("   "), Err(RuleParseError::Empty)));
    }

    #[test]
    fn missing_tool_errors() {
        let err = parse_rule_v2("(argv_prefix=\"x\")").unwrap_err();
        assert!(matches!(err, RuleParseError::MissingTool));
    }

    #[test]
    fn malformed_field_no_eq_errors() {
        let err = parse_rule_v2("Bash(argv_prefix\"x\")").unwrap_err();
        assert!(matches!(err, RuleParseError::MalformedField { .. }));
    }

    #[test]
    fn malformed_field_unterminated_quote_errors() {
        let err = parse_rule_v2(r#"Bash(argv_prefix="abc)"#).unwrap_err();
        assert!(matches!(
            err,
            RuleParseError::MalformedField { .. } | RuleParseError::UnterminatedParen
        ));
    }

    // ── Forward-compat ───────────────────────────────────────────

    #[test]
    fn extra_fields_roundtrip_via_extra_map() {
        // A future version writes `priority="high"`; today we don't
        // know that key. The error path is loud (UnknownField), so
        // a future-extensions reader is expected to use a more
        // permissive parser. This test confirms that the *strict*
        // parser surfaces the unknown key rather than silently
        // dropping it.
        let err = parse_rule_v2(r#"Bash(argv_prefix="x", priority="high")"#).unwrap_err();
        assert!(matches!(err, RuleParseError::UnknownField { .. }));
    }
}
