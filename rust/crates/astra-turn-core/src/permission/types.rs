//! Permission synchronization for parent-child agent communication.
//!
//! When a child agent needs permission for a tool that requires approval,
//! it sends a `PermissionRequest` to its parent. The parent can approve,
//! deny, or add persistent rules that apply to future requests.
//!
//! This module provides the types and utilities for:
//! - Permission request/response payloads
//! - Permission rule inheritance from parent to child
//! - Permission update propagation

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ─── Permission Mode ────────────────────────────────────────────────────────

/// Permission mode controls how tool approval decisions are handled.
///
/// Shared between parent and child agents; child inherits parent's mode
/// unless explicitly overridden with a more restrictive mode.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Auto-approve all tools (except bypass-immune safety checks).
    Auto,
    /// Read-only investigation/planning mode: allow read tools, deny mutations.
    Plan,
    /// Auto-approve safe workspace-local edit/write operations only.
    AcceptEdits,
    /// Prompt the user for write/execute tools (default interactive mode).
    #[default]
    Prompt,
    /// Deny all write/execute tools without prompting (CI/headless mode).
    Deny,
}

impl PermissionMode {
    /// Human label for the status-line mode chip.
    pub fn chip_text(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Plan => "Plan",
            Self::AcceptEdits => "Edits",
            Self::Prompt => "Ask",
            Self::Deny => "Deny",
        }
    }

    /// Color hint for the status-line mode chip.
    /// Returns `(red, green, blue)` for a ratatui-style `Color::Rgb`.
    pub fn chip_color_rgb(self) -> (u8, u8, u8) {
        // Blue for plan, cyan for edit, yellow for auto, red for deny, white for default.
        match self {
            Self::Auto => (255, 255, 0),
            Self::Plan => (100, 149, 237),
            Self::AcceptEdits => (0, 255, 255),
            Self::Prompt => (255, 255, 255),
            Self::Deny => (255, 0, 0),
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Plan => write!(f, "plan"),
            Self::AcceptEdits => write!(f, "accept_edits"),
            Self::Prompt => write!(f, "prompt"),
            Self::Deny => write!(f, "deny"),
        }
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "plan" => Ok(Self::Plan),
            "accept_edits" => Ok(Self::AcceptEdits),
            "prompt" => Ok(Self::Prompt),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "invalid permission mode '{s}': expected auto, plan, accept_edits, prompt, or deny"
            )),
        }
    }
}

// ─── Permission Rules ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPatternMatch {
    #[default]
    Prefix,
    Exact,
    PathPrefix,
}

impl PermissionPatternMatch {
    #[must_use]
    pub fn is_prefix(&self) -> bool {
        *self == Self::Prefix
    }
}

/// A permission rule that can be inherited or synchronized.
///
/// Persisted string form uses the current explicit grammar:
/// `Tool()` for broad rules or `Tool(key="value")` for scoped rules.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Tool name (lowercase).
    pub tool: String,
    /// Optional command prefix for execute tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Whether `pattern` is a prefix/glob, exact command/path, or
    /// literal path-prefix match.
    #[serde(default, skip_serializing_if = "PermissionPatternMatch::is_prefix")]
    pub pattern_match: PermissionPatternMatch,
    /// Optional operation kind constraint (`read`, `write`, `execute`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// Optional cwd/package-root constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_root: Option<String>,
    /// Optional current git branch constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Optional domain constraint for URL/network-shaped tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Optional capability constraint for MCP/capability-shaped tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<String>,
}

impl PermissionRule {
    /// Create a rule for a tool with no pattern (matches all uses).
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            tool: name.into().to_lowercase(),
            pattern: None,
            pattern_match: PermissionPatternMatch::Prefix,
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
        }
    }

    /// Create a rule for a tool with a command prefix pattern.
    pub fn with_pattern(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            tool: name.into().to_lowercase(),
            pattern: Some(pattern.into()),
            pattern_match: PermissionPatternMatch::Prefix,
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
        }
    }

    /// Convert a parsed rule spec into the enforcement shape.
    #[must_use]
    pub fn from_rule_spec(rule: crate::permission::rule_grammar::PermissionRuleSpec) -> Self {
        let lower_family = rule.tool.to_lowercase();
        let tool = if matches!(lower_family.as_str(), "network" | "mcp") {
            rule.extra
                .get("tool")
                .cloned()
                .unwrap_or_else(|| lower_family.clone())
                .to_lowercase()
        } else {
            lower_family.clone()
        };
        let (pattern, pattern_match) = if lower_family == "bash" {
            if let Some(exact) = rule.argv_exact.clone() {
                (Some(exact), PermissionPatternMatch::Exact)
            } else {
                (rule.argv_prefix.clone(), PermissionPatternMatch::Prefix)
            }
        } else {
            (
                rule.argv_exact
                    .clone()
                    .or_else(|| rule.argv_prefix.clone())
                    .or_else(|| rule.path_prefix.clone())
                    .or_else(|| rule.path_glob.clone())
                    .or_else(|| rule.extra.get("pattern").cloned()),
                if rule.argv_exact.is_some() {
                    PermissionPatternMatch::Exact
                } else if rule.path_prefix.is_some() {
                    PermissionPatternMatch::PathPrefix
                } else {
                    PermissionPatternMatch::Prefix
                },
            )
        };
        Self {
            tool,
            pattern,
            pattern_match,
            op: rule.op,
            cwd_root: rule.cwd_root,
            git_branch: rule.git_branch,
            domain: rule.domain,
            capability: rule.capability,
        }
    }

    /// Parse a rule from the current explicit string format.
    pub fn try_parse(
        rule_str: &str,
    ) -> Result<Self, crate::permission::rule_grammar::RuleParseError> {
        crate::permission::rule_grammar::parse_rule(rule_str).map(Self::from_rule_spec)
    }

    /// Parse a rule from the current explicit string format.
    ///
    /// Invalid inputs become a non-matching sentinel. Settings loading
    /// validates rules first and surfaces the parse error to the user;
    /// this fallback keeps direct internal callers fail-closed.
    pub fn parse(rule_str: &str) -> Self {
        Self::try_parse(rule_str).unwrap_or_else(|_| Self::invalid())
    }

    fn invalid() -> Self {
        Self {
            tool: "__invalid_permission_rule__".to_string(),
            pattern: None,
            pattern_match: PermissionPatternMatch::Prefix,
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
        }
    }

    /// Check if this rule matches a tool call.
    ///
    /// A pattern that contains glob metacharacters (`*` / `**` / `?` /
    /// `{a,b}`) is matched against the command string via
    /// [`crate::permission::path_glob::glob_match`]. Plain command
    /// prefixes use word-boundary prefix matching so `npm test`
    /// allows `npm test --verbose` but not `npm testify`.
    pub fn matches(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.matches_with_context(tool_name, &RuleMatchContext::from_command_hint(command))
    }

    /// True when a bash allow rule is broad enough to bypass the safety
    /// classifier for arbitrary code execution. Deny rules may still use these
    /// shapes; callers should apply this only when evaluating allow rules.
    #[must_use]
    pub fn is_dangerous_bash_allow_shape(&self) -> bool {
        if self.tool != "bash" || self.pattern_match == PermissionPatternMatch::Exact {
            return false;
        }
        let Some(pattern) = self.pattern.as_deref() else {
            return true;
        };
        let pattern = pattern.trim();
        if pattern.is_empty() || pattern == "*" {
            return true;
        }
        let first = pattern.split_whitespace().next().unwrap_or_default();
        crate::tool::args::hints::is_unsafe_shell_prefix_token(first)
    }

    /// Check if this rule matches a fully-described tool call.
    pub fn matches_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        if self.tool != tool_name.to_lowercase()
            && !file_write_family_matches(&self.tool, tool_name, ctx)
        {
            return false;
        }
        if let Some(expected) = &self.op
            && ctx
                .op
                .as_deref()
                .is_none_or(|actual| !actual.eq_ignore_ascii_case(expected))
        {
            return false;
        }
        if let Some(expected) = &self.cwd_root
            && ctx
                .cwd
                .as_deref()
                .is_none_or(|cwd| !cwd_matches_root(cwd, expected))
        {
            return false;
        }
        if let Some(expected) = &self.git_branch
            && ctx
                .git_branch
                .as_deref()
                .is_none_or(|actual| actual != expected)
        {
            return false;
        }
        if let Some(expected) = &self.domain
            && ctx
                .domain
                .as_deref()
                .is_none_or(|actual| !domain_matches(expected, actual))
        {
            return false;
        }
        if let Some(expected) = &self.capability
            && !capability_matches(expected, ctx)
        {
            return false;
        }

        let target_is_constrained_path =
            self.tool != "bash" && ctx.path.is_some() && self.op.is_some();
        let resolved_constrained_path = if target_is_constrained_path && self.cwd_root.is_some() {
            ctx.path.as_deref().and_then(|path| {
                ctx.cwd
                    .as_deref()
                    .and_then(|cwd| {
                        crate::permission::memory_profile::resolve_write_path_from_cwd(cwd, path)
                    })
                    .map(|resolved| resolved.to_string_lossy().into_owned())
            })
        } else {
            None
        };
        let target = if self.tool == "bash" {
            ctx.command.as_deref()
        } else {
            resolved_constrained_path
                .as_deref()
                .or(ctx.path.as_deref())
                .or(ctx.command.as_deref())
        };
        match (&self.pattern, target) {
            (None, _) => true, // Bare tool name matches all
            (Some(pattern), Some(cmd)) => {
                if self.pattern_match == PermissionPatternMatch::Exact {
                    return pattern == cmd;
                }

                if target_is_constrained_path {
                    if self.pattern_match == PermissionPatternMatch::PathPrefix {
                        return cmd.starts_with(pattern);
                    }
                    if pattern_contains_glob_metachars(pattern) {
                        return crate::permission::path_glob::glob_match(pattern, cmd);
                    }
                    return pattern == cmd;
                }

                if self.pattern_match == PermissionPatternMatch::PathPrefix {
                    return cmd.starts_with(pattern);
                }

                if pattern_contains_glob_metachars(pattern) {
                    // Glob path: match the WHOLE command (post-
                    // lower-casing) against the user's pattern.
                    // We don't lowercase the pattern itself
                    // because path globs are case-sensitive on
                    // Linux.
                    let lower_cmd = cmd.to_lowercase();
                    crate::permission::path_glob::glob_match(pattern, &lower_cmd)
                } else {
                    // Word-boundary prefix path.
                    let lower_cmd = cmd.to_lowercase();
                    let lower_prefix = pattern.to_lowercase();
                    if !lower_cmd.starts_with(&lower_prefix) {
                        return false;
                    }
                    let rest = &lower_cmd[lower_prefix.len()..];
                    rest.is_empty()
                        || rest.starts_with(char::is_whitespace)
                        || rest.starts_with(&['-', '=', ';', '|', '&', '>', '<'][..])
                }
            }
            (Some(_), None) => false, // Pattern rule but no command to match
        }
    }
}

fn file_write_family_matches(rule_tool: &str, tool_name: &str, ctx: &RuleMatchContext) -> bool {
    rule_tool == "file_write"
        && matches!(
            crate::cloud::approval_policy::cloud_gated_tool_kind(tool_name),
            Some(crate::cloud::approval_policy::CloudGatedToolKind::Write)
        )
        && ctx
            .op
            .as_deref()
            .is_some_and(|op| op.eq_ignore_ascii_case("write"))
        && ctx.path.is_some()
        && crate::tool::categories::registry().is_file_op(tool_name)
}

/// Context for permission-rule matching.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleMatchContext {
    pub command: Option<String>,
    pub path: Option<String>,
    pub op: Option<String>,
    pub cwd: Option<PathBuf>,
    pub git_branch: Option<String>,
    pub domain: Option<String>,
    pub capability: Option<String>,
}

impl RuleMatchContext {
    #[must_use]
    pub fn from_command_hint(command: Option<&str>) -> Self {
        Self {
            command: command.map(ToOwned::to_owned),
            path: command.map(ToOwned::to_owned),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn from_tool_args(tool_name: &str, args: &serde_json::Value) -> Self {
        let cwd = std::env::current_dir().ok();
        let op = match crate::cloud::approval_policy::cloud_gated_tool_kind(tool_name) {
            Some(crate::cloud::approval_policy::CloudGatedToolKind::Execute) => Some("execute"),
            Some(crate::cloud::approval_policy::CloudGatedToolKind::Write) => Some("write"),
            None => Some("read"),
        }
        .map(str::to_string);

        let domain = string_arg(args, "domain")
            .or_else(|| string_arg(args, "host"))
            .or_else(|| string_arg(args, "url").and_then(|url| domain_from_urlish(&url)))
            .or_else(|| string_arg(args, "uri").and_then(|url| domain_from_urlish(&url)));
        let capability = string_arg(args, "capability").or_else(|| capability_from_args(args));
        let git_branch = cwd.as_deref().and_then(current_git_branch);
        Self {
            command: crate::tool::args::hints::command_hint_from_args(args).map(str::to_string),
            path: crate::tool::args::hints::path_hint_from_args(args),
            op,
            cwd,
            git_branch,
            domain,
            capability,
        }
    }
}

fn string_arg(args: &serde_json::Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn capability_from_args(args: &serde_json::Value) -> Option<String> {
    for key in ["destructive", "read_only", "open_world"] {
        if let Some(value) = args.get(key) {
            return match value {
                serde_json::Value::Bool(b) => Some(format!("{key}={b}")),
                serde_json::Value::String(s) => Some(format!("{key}={s}")),
                _ => None,
            };
        }
    }
    None
}

fn domain_from_urlish(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_scheme = trimmed
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(trimmed);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme)
        .trim();
    let host = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    let host = host
        .strip_prefix('[')
        .and_then(|s| s.split_once(']').map(|(host, _)| host))
        .unwrap_or_else(|| host.split(':').next().unwrap_or(host))
        .trim()
        .trim_end_matches('.');
    if host.is_empty() {
        None
    } else {
        Some(host.to_lowercase())
    }
}

fn domain_matches(expected: &str, actual: &str) -> bool {
    let expected = expected.trim().trim_end_matches('.').to_lowercase();
    let actual = actual.trim().trim_end_matches('.').to_lowercase();
    actual == expected || actual.ends_with(&format!(".{expected}"))
}

fn capability_matches(expected: &str, ctx: &RuleMatchContext) -> bool {
    let Some(actual) = ctx.capability.as_deref() else {
        return false;
    };
    if actual == expected {
        return true;
    }
    if let Some((key, value)) = expected.split_once('=') {
        return actual
            .split_once('=')
            .is_some_and(|(actual_key, actual_value)| {
                actual_key == key && actual_value.eq_ignore_ascii_case(value)
            });
    }
    false
}

fn cwd_matches_root(cwd: &Path, expected_root: &str) -> bool {
    let expected_root = expected_root.trim();
    if expected_root.is_empty() || expected_root == "." {
        return true;
    }
    let expected = Path::new(expected_root);
    if expected.is_absolute() {
        let canonical_expected = expected
            .canonicalize()
            .unwrap_or_else(|_| expected.to_path_buf());
        let canonical_cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        return canonical_cwd.starts_with(canonical_expected);
    }
    cwd.ancestors()
        .any(|ancestor| path_ends_with(ancestor, expected))
}

fn path_ends_with(path: &Path, suffix: &Path) -> bool {
    let path_components: Vec<_> = path.components().collect();
    let suffix_components: Vec<_> = suffix.components().collect();
    path_components.len() >= suffix_components.len()
        && path_components[path_components.len() - suffix_components.len()..] == suffix_components
}

fn current_git_branch(cwd: &Path) -> Option<String> {
    let git_entry = cwd
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|path| path.exists())?;
    let git_dir = if git_entry.is_dir() {
        git_entry
    } else {
        let content = std::fs::read_to_string(&git_entry).ok()?;
        let gitdir = content.strip_prefix("gitdir:")?.trim();
        let path = Path::new(gitdir);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            git_entry.parent()?.join(path)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(ToOwned::to_owned)
}

/// Cheap predicate: does `pattern` contain any of the glob
/// metacharacters [`crate::permission::path_glob`] recognizes?
/// Used to decide between glob-match and prefix-match.
fn pattern_contains_glob_metachars(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('{')
}

impl std::fmt::Display for PermissionRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut rule = crate::permission::rule_grammar::PermissionRuleSpec {
            tool: self.tool.clone(),
            argv_exact: None,
            argv_prefix: None,
            path_glob: None,
            path_prefix: None,
            op: self.op.clone(),
            cwd_root: self.cwd_root.clone(),
            git_branch: self.git_branch.clone(),
            domain: self.domain.clone(),
            capability: self.capability.clone(),
            extra: Default::default(),
        };
        if self.tool == "bash" {
            if self.pattern_match == PermissionPatternMatch::Exact {
                rule.argv_exact = self.pattern.clone();
            } else {
                rule.argv_prefix = self.pattern.clone();
            }
        } else if self.pattern_match == PermissionPatternMatch::PathPrefix {
            rule.path_prefix = self.pattern.clone();
        } else {
            rule.path_glob = self.pattern.clone();
        }
        write!(
            f,
            "{}",
            crate::permission::rule_grammar::serialize_rule(&rule)
        )
    }
}

// ─── Inherited Permission Context ───────────────────────────────────────────

/// Permission context inherited from parent to child agent.
///
/// Contains the parent's permission mode and rules that the child should honor.
/// Child agents cannot escalate permissions beyond what parent allows.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InheritedPermissions {
    /// Permission mode inherited from parent.
    pub mode: PermissionMode,
    /// Tools explicitly allowed by parent (child can use without asking).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_rules: Vec<PermissionRule>,
    /// Tools explicitly denied by parent (child cannot use even if it wants).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny_rules: Vec<PermissionRule>,
    /// Tools that require parent approval (child must ask before using).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask_rules: Vec<PermissionRule>,
    /// Optional tool allowlist — if set, child can ONLY use these tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_tools: Option<HashSet<String>>,
    /// Whether this agent runs in background (cannot show interactive prompts).
    #[serde(default)]
    pub is_background: bool,
    /// Issue #326 P0 / R1 Major 10 / task #17:
    /// Fingerprinted session overrides from the parent. The
    /// `allow_rules` / `deny_rules` above carry **command-prefix-level**
    /// rules; this field carries **per-fingerprint** decisions
    /// (`Bash(argv_prefix="cargo test") → Allow` is **not** the same as
    /// `Bash() → Allow`). Children must consult this BEFORE the inherited
    /// rules so a "user pressed Always on cargo test" decision doesn't
    /// get downgraded to "Bash is fully allowed".
    ///
    /// Stored as a serialized JSON Value so we don't pull
    /// `astra_turn_core::approval_fingerprint::FingerprintedOverrides`
    /// into the public type signature (avoids the cyclic dependency
    /// between `astra-turn-core::permission_types` and
    /// `astra-turn-core::approval_fingerprint`). The deserialization
    /// is best-effort: if a child receives a payload that fails to
    /// parse, it falls back to the allow/deny rules and logs a
    /// warning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprinted_overrides: Option<serde_json::Value>,
}

impl InheritedPermissions {
    /// Create inherited permissions with the given mode.
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            ..Default::default()
        }
    }

    /// Create inherited permissions with auto-approve mode (no restrictions).
    pub fn auto_approve() -> Self {
        Self {
            mode: PermissionMode::Auto,
            ..Default::default()
        }
    }
    /// Check if a tool is explicitly allowed by inherited rules.
    pub fn is_allowed(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_allowed_with_context(tool_name, &RuleMatchContext::from_command_hint(command))
    }

    /// Check if a tool is explicitly allowed by inherited rules.
    pub fn is_allowed_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        self.allow_rules
            .iter()
            .any(|r| !r.is_dangerous_bash_allow_shape() && r.matches_with_context(tool_name, ctx))
    }

    /// Check if a tool is explicitly denied by inherited rules.
    pub fn is_denied(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_denied_with_context(tool_name, &RuleMatchContext::from_command_hint(command))
    }

    /// Check if a tool is explicitly denied by inherited rules.
    pub fn is_denied_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        self.deny_rules
            .iter()
            .any(|r| r.matches_with_context(tool_name, ctx))
    }

    /// Return the inherited ask rule matching this tool call, if any.
    pub fn ask_rule_with_context(
        &self,
        tool_name: &str,
        ctx: &RuleMatchContext,
    ) -> Option<&PermissionRule> {
        self.ask_rules
            .iter()
            .find(|r| r.matches_with_context(tool_name, ctx))
    }

    /// Check if a tool is in the allowed_tools set (if set).
    pub fn is_tool_allowed_by_allowlist(&self, tool_name: &str) -> bool {
        match &self.allowed_tools {
            Some(set) => set.contains(&tool_name.to_lowercase()),
            None => true, // No allowlist = all tools allowed
        }
    }

    /// Set the tool allowlist. When set, only these tools may execute.
    /// Used by the spawner to carry an agent type's `allowed_tools`
    /// into the permission engine so the `ToolAllowlist` evaluation
    /// step enforces it. This is the single source of truth for
    /// execution-time tool restriction.
    pub fn with_allowed_tools(mut self, tools: impl IntoIterator<Item = String>) -> Self {
        let set: HashSet<String> = tools
            .into_iter()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty() && t != "*")
            .collect();
        self.allowed_tools = if set.is_empty() { None } else { Some(set) };
        self
    }

    /// Add an allow rule.
    pub fn add_allow(&mut self, rule: PermissionRule) {
        if !self.allow_rules.contains(&rule) {
            self.allow_rules.push(rule);
        }
    }

    /// Add a deny rule.
    pub fn add_deny(&mut self, rule: PermissionRule) {
        if !self.deny_rules.contains(&rule) {
            self.deny_rules.push(rule);
        }
    }
}

// ─── Permission Request ─────────────────────────────────────────────────────

/// A permission request from child to parent agent.
///
/// When a child agent needs to use a tool that requires permission,
/// it sends this request to its parent via the messaging system.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// The tool name requiring permission.
    pub tool_name: String,
    /// Command or path hint for display (e.g., "git commit -m 'fix'").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Full tool arguments (for parent to inspect).
    pub args: serde_json::Value,
    /// Child's suggested rule if approved (e.g., `Bash(argv_prefix="git commit")`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_rule: Option<String>,
    /// Brief description of why the tool is needed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl PermissionRequest {
    /// Create a permission request for a tool call.
    pub fn new(tool_name: impl Into<String>, args: serde_json::Value) -> Self {
        Self {
            tool_name: tool_name.into(),
            hint: None,
            args,
            suggested_rule: None,
            reason: None,
        }
    }

    /// Add a display hint (command or path).
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// Add a suggested allow rule.
    pub fn with_suggested_rule(mut self, rule: impl Into<String>) -> Self {
        self.suggested_rule = Some(rule.into());
        self
    }

    /// Add a reason for the request.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

// ─── Permission Response ────────────────────────────────────────────────────

/// Response from parent to child's permission request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionResponse {
    /// Whether the request was approved.
    pub approved: bool,
    /// Optional denial reason (if not approved).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Permission updates to apply (new rules granted by parent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub updates: Vec<PermissionUpdate>,
}

impl PermissionResponse {
    /// Create an approval response.
    pub fn approve() -> Self {
        Self {
            approved: true,
            reason: None,
            updates: Vec::new(),
        }
    }

    /// Create a denial response.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self {
            approved: false,
            reason: Some(reason.into()),
            updates: Vec::new(),
        }
    }

    /// Add a permission update (rule to apply).
    pub fn with_update(mut self, update: PermissionUpdate) -> Self {
        self.updates.push(update);
        self
    }
}

// ─── Permission Update ──────────────────────────────────────────────────────

/// A permission update to propagate from parent to child.
///
/// When parent approves a request, it may also grant persistent rules
/// that the child can apply to future similar requests.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PermissionUpdate {
    /// The type of update.
    pub action: PermissionAction,
    /// The rule to add/remove.
    pub rule: PermissionRule,
    /// Whether to persist this rule (vs. session-only).
    #[serde(default)]
    pub persist: bool,
}

/// The action to take for a permission update.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    /// Add to allow list.
    Allow,
    /// Add to deny list.
    Deny,
    /// Remove from allow list.
    RevokeAllow,
    /// Remove from deny list.
    RevokeDeny,
}

impl PermissionUpdate {
    /// Create an allow update.
    pub fn allow(rule: PermissionRule) -> Self {
        Self {
            action: PermissionAction::Allow,
            rule,
            persist: false,
        }
    }

    /// Create a deny update.
    pub fn deny(rule: PermissionRule) -> Self {
        Self {
            action: PermissionAction::Deny,
            rule,
            persist: false,
        }
    }

    /// Mark this update as persistent.
    pub fn persistent(mut self) -> Self {
        self.persist = true;
        self
    }
}

// ─── Sync Context ───────────────────────────────────────────────────────────

/// Context for managing permission synchronization during an agent's execution.
///
/// Tracks:
/// - Inherited permissions from parent
/// - Session-level overrides
/// - Permission request telemetry for UI/status reporting
#[derive(Debug, Clone, Default)]
pub struct PermissionTelemetry {
    pub permission_requests: u32,
    pub permission_requests_approved: u32,
    pub tools_blocked: u32,
    pub recent_denials: Vec<String>,
    /// Most-recent `(tool, reason)` denials, newest at the back.
    /// Mirrors `recent_denials` but preserves *why* the call was refused.
    pub recent_denials_with_reasons: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct PermissionSyncContext {
    /// Permissions inherited from parent agent.
    pub inherited: InheritedPermissions,
    /// Session-level overrides (approved/denied during this run).
    session_allow: Vec<PermissionRule>,
    session_deny: Vec<PermissionRule>,
    telemetry: PermissionTelemetry,
}

/// Shared runtime permission context handle.
///
/// Runtime entrypoints should construct permission state through
/// [`PermissionSyncContext::shared`] or [`PermissionSyncContext::shared_root`]
/// instead of open-coding `Arc<RwLock<PermissionSyncContext>>`. Keeping the
/// handle shape centralized prevents new CLI/server/edge entrypoints from
/// silently reintroducing a missing-context path.
pub type PermissionSyncHandle = std::sync::Arc<tokio::sync::RwLock<PermissionSyncContext>>;

impl PermissionSyncContext {
    /// Create a new sync context with inherited permissions.
    pub fn new(inherited: InheritedPermissions) -> Self {
        Self {
            inherited,
            session_allow: Vec::new(),
            session_deny: Vec::new(),
            telemetry: PermissionTelemetry::default(),
        }
    }

    /// Create a shared runtime permission context from an explicit inherited
    /// permissions envelope.
    pub fn shared(inherited: InheritedPermissions) -> PermissionSyncHandle {
        Self::new(inherited).into_shared()
    }

    /// Convert an already-built context into a shared runtime handle.
    pub fn into_shared(self) -> PermissionSyncHandle {
        std::sync::Arc::new(tokio::sync::RwLock::new(self))
    }

    /// Create a context with no inherited permissions (root agent).
    pub fn root(mode: PermissionMode) -> Self {
        Self::new(InheritedPermissions {
            mode,
            ..Default::default()
        })
    }

    /// Create a shared root runtime permission context.
    pub fn shared_root(mode: PermissionMode) -> PermissionSyncHandle {
        Self::shared(InheritedPermissions::new(mode))
    }

    /// Get the effective permission mode.
    pub fn mode(&self) -> PermissionMode {
        self.inherited.mode
    }

    /// Check if a tool is allowed (by inherited or session rules).
    pub fn is_allowed(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_allowed_with_context(tool_name, &RuleMatchContext::from_command_hint(command))
    }

    /// Check if a tool is allowed (by inherited or session rules).
    pub fn is_allowed_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        // Check session overrides first
        if self
            .session_allow
            .iter()
            .any(|r| !r.is_dangerous_bash_allow_shape() && r.matches_with_context(tool_name, ctx))
        {
            return true;
        }
        // Check inherited rules
        self.inherited.is_allowed_with_context(tool_name, ctx)
    }

    /// Check if a tool is denied (by inherited or session rules).
    pub fn is_denied(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_denied_with_context(tool_name, &RuleMatchContext::from_command_hint(command))
    }

    /// Check if a tool is denied (by inherited or session rules).
    pub fn is_denied_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        // Check session overrides first
        if self
            .session_deny
            .iter()
            .any(|r| r.matches_with_context(tool_name, ctx))
        {
            return true;
        }
        // Check inherited rules
        self.inherited.is_denied_with_context(tool_name, ctx)
    }

    /// Apply a permission update from parent.
    pub fn apply_update(&mut self, update: &PermissionUpdate) {
        match update.action {
            PermissionAction::Allow => {
                if !self.session_allow.contains(&update.rule) {
                    self.session_allow.push(update.rule.clone());
                }
            }
            PermissionAction::Deny => {
                if !self.session_deny.contains(&update.rule) {
                    self.session_deny.push(update.rule.clone());
                }
            }
            PermissionAction::RevokeAllow => {
                self.session_allow.retain(|r| r != &update.rule);
            }
            PermissionAction::RevokeDeny => {
                self.session_deny.retain(|r| r != &update.rule);
            }
        }
    }

    /// Apply all updates from a permission response.
    pub fn apply_response(&mut self, response: &PermissionResponse) {
        for update in &response.updates {
            self.apply_update(update);
        }
    }
    /// Refresh inherited (policy) state from a fresher context while
    /// preserving this handle's own runtime telemetry and session overrides.
    ///
    /// Used by `refresh_root_permission_context` so that a periodic policy
    /// refresh doesn't wipe the self-model feedback loop (`tools_blocked`,
    /// `recent_denials`, ...) or forget in-session allow/deny decisions the
    /// user already made this turn. Only the policy half (`inherited`) is
    /// replaced; runtime accumulators stay.
    pub fn merge_policy_from(&mut self, fresh: &PermissionSyncContext) {
        self.inherited = fresh.inherited.clone();
        // session_allow / session_deny / telemetry are intentionally
        // preserved — they belong to this handle's runtime, not the policy.
    }

    /// Create inherited permissions for a child agent.
    pub fn for_child(&self, is_background: bool) -> InheritedPermissions {
        let mut inherited = self.inherited.clone();
        inherited.is_background = is_background;
        // Add session rules to inherited rules for child
        for rule in &self.session_allow {
            inherited.add_allow(rule.clone());
        }
        for rule in &self.session_deny {
            inherited.add_deny(rule.clone());
        }
        inherited
    }

    pub fn effective_allow_rule_count(&self) -> u32 {
        (self.inherited.allow_rules.len() + self.session_allow.len()) as u32
    }

    pub fn effective_deny_rule_count(&self) -> u32 {
        (self.inherited.deny_rules.len() + self.session_deny.len()) as u32
    }

    pub fn telemetry(&self) -> PermissionTelemetry {
        self.telemetry.clone()
    }

    pub fn record_permission_request(&mut self) {
        self.telemetry.permission_requests = self.telemetry.permission_requests.saturating_add(1);
    }

    pub fn record_permission_approved(&mut self) {
        self.telemetry.permission_requests_approved = self
            .telemetry
            .permission_requests_approved
            .saturating_add(1);
    }
    pub fn record_blocked_tool_with_reason(&mut self, tool_name: &str, reason: Option<&str>) {
        self.telemetry.tools_blocked = self.telemetry.tools_blocked.saturating_add(1);
        self.telemetry
            .recent_denials
            .retain(|name| name != tool_name);
        self.telemetry.recent_denials.push(tool_name.to_string());
        const MAX_RECENT_DENIALS: usize = 5;
        if self.telemetry.recent_denials.len() > MAX_RECENT_DENIALS {
            let drop_count = self.telemetry.recent_denials.len() - MAX_RECENT_DENIALS;
            self.telemetry.recent_denials.drain(0..drop_count);
        }
        if let Some(reason_text) = reason {
            let entry = (tool_name.to_string(), reason_text.to_string());
            self.telemetry
                .recent_denials_with_reasons
                .retain(|(t, r)| !(t == &entry.0 && r == &entry.1));
            self.telemetry.recent_denials_with_reasons.push(entry);
            if self.telemetry.recent_denials_with_reasons.len() > MAX_RECENT_DENIALS {
                let drop_count =
                    self.telemetry.recent_denials_with_reasons.len() - MAX_RECENT_DENIALS;
                self.telemetry
                    .recent_denials_with_reasons
                    .drain(0..drop_count);
            }
        }
    }
}

/// Result of a permission decision.
#[derive(Clone, Debug)]
pub enum PermissionDecision {
    /// Approve the request, optionally with rules to propagate.
    Approve { updates: Vec<PermissionUpdate> },
    /// Deny the request with a reason.
    Deny { reason: String },
    /// Escalate to user interaction (only valid for non-background agents).
    Escalate,
}

impl PermissionDecision {
    /// Create an approval decision.
    pub fn approve() -> Self {
        Self::Approve {
            updates: Vec::new(),
        }
    }

    /// Create an approval decision with a persistent allow rule.
    pub fn approve_with_rule(rule: PermissionRule) -> Self {
        Self::Approve {
            updates: vec![PermissionUpdate::allow(rule)],
        }
    }

    /// Create a denial decision.
    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }
}

/// Callback type for permission decision.
pub type PermissionCallback =
    Box<dyn Fn(&PermissionRequest, &PermissionSyncContext) -> PermissionDecision + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_mode_roundtrip_includes_accept_edits() {
        let parsed = "accept_edits".parse::<PermissionMode>().unwrap();
        assert_eq!(parsed, PermissionMode::AcceptEdits);
        assert_eq!(parsed.to_string(), "accept_edits");
        assert_eq!(parsed.chip_text(), "Edits");
        assert!("accept-edits".parse::<PermissionMode>().is_err());
    }

    #[test]
    fn permission_mode_roundtrip_includes_plan() {
        let parsed = "plan".parse::<PermissionMode>().unwrap();
        assert_eq!(parsed, PermissionMode::Plan);
        assert_eq!(parsed.to_string(), "plan");
    }

    #[test]
    fn test_permission_rule_parse() {
        let rule = PermissionRule::parse("bash()");
        assert_eq!(rule.tool, "bash");
        assert!(rule.pattern.is_none());

        let rule = PermissionRule::parse(r#"Bash(argv_prefix="git commit")"#);
        assert_eq!(rule.tool, "bash");
        assert_eq!(rule.pattern, Some("git commit".to_string()));

        assert!(PermissionRule::try_parse("Bash(git commit:*)").is_err());
        assert!(
            !PermissionRule::parse("Bash(git commit:*)").matches("bash", Some("git commit -m fix"))
        );
    }

    #[test]
    fn permission_mode_rejects_removed_aliases() {
        for alias in ["yolo", "bypass-safety", "bypass_safety"] {
            assert!(alias.parse::<PermissionMode>().is_err());
        }
    }

    #[test]
    fn test_permission_rule_matches() {
        let rule = PermissionRule::parse(r#"Bash(argv_prefix="git commit")"#);

        assert!(rule.matches("bash", Some("git commit -m 'fix'")));
        assert!(rule.matches("Bash", Some("git commit --amend")));
        assert!(!rule.matches("bash", Some("git commitizen")));
        assert!(!rule.matches("bash", Some("git push")));
        assert!(!rule.matches("str_replace", Some("git commit")));
    }

    #[test]
    fn dangerous_bash_allow_shapes_are_not_honored_as_allow_rules() {
        let mut inherited = InheritedPermissions::new(PermissionMode::Prompt);
        inherited.add_allow(PermissionRule::parse("bash()"));
        inherited.add_allow(PermissionRule::parse(
            r#"Bash(argv_prefix="python", op="execute")"#,
        ));
        inherited.add_allow(PermissionRule::parse(
            r#"Bash(argv_prefix="npm test", op="execute")"#,
        ));
        inherited.add_deny(PermissionRule::parse(r#"Bash(argv_prefix="python")"#));

        assert!(!inherited.is_allowed_with_context(
            "bash",
            &RuleMatchContext::from_tool_args(
                "bash",
                &serde_json::json!({"command": "python -c 'print(1)'"})
            )
        ));
        assert!(inherited.is_allowed_with_context(
            "bash",
            &RuleMatchContext::from_tool_args(
                "bash",
                &serde_json::json!({"command": "npm test -- --watch"})
            )
        ));
        assert!(
            inherited.is_denied("bash", Some("python -c 'print(1)'")),
            "dangerous bash shapes remain valid for deny rules"
        );
    }

    #[test]
    fn rule_matches_with_glob_pattern() {
        let rule = PermissionRule::with_pattern("file_write", "src/**/*.rs");
        assert!(rule.matches("file_write", Some("src/lib.rs")));
        assert!(rule.matches("file_write", Some("src/auth/login.rs")));
        assert!(rule.matches("file_write", Some("src/a/b/c/d.rs")));
        assert!(!rule.matches("file_write", Some("docs/readme.md")));
    }

    #[test]
    fn rule_matches_with_brace_alternatives() {
        let rule = PermissionRule::with_pattern("file_write", "**/*.{rs,ts,js}");
        assert!(rule.matches("file_write", Some("lib.rs")));
        assert!(rule.matches("file_write", Some("ui/component.ts")));
        assert!(rule.matches("file_write", Some("server.js")));
        assert!(!rule.matches("file_write", Some("config.toml")));
    }

    #[test]
    fn rule_matches_plain_prefix_when_no_metachars() {
        let rule = PermissionRule::parse(r#"Bash(argv_prefix="npm test")"#);
        assert!(rule.matches("bash", Some("npm test")));
        assert!(rule.matches("bash", Some("npm test --watch")));
        assert!(!rule.matches("bash", Some("npm run deploy")));
    }

    #[test]
    fn current_rule_enforces_cwd_root_constraint() {
        let rule =
            PermissionRule::parse(r#"Bash(argv_prefix="npm test", cwd_root="packages/web")"#);
        let matching = RuleMatchContext {
            command: Some("npm test --watch".into()),
            cwd: Some(PathBuf::from("/repo/packages/web/src")),
            ..Default::default()
        };
        let other_package = RuleMatchContext {
            command: Some("npm test --watch".into()),
            cwd: Some(PathBuf::from("/repo/packages/api")),
            ..Default::default()
        };

        assert!(rule.matches_with_context("bash", &matching));
        assert!(!rule.matches_with_context("bash", &other_package));
    }

    #[test]
    fn current_rule_enforces_git_branch_constraint() {
        let rule = PermissionRule::parse(r#"Bash(argv_prefix="git push", git_branch="main")"#);
        let main_branch = RuleMatchContext {
            command: Some("git push origin main".into()),
            git_branch: Some("main".into()),
            ..Default::default()
        };
        let feature_branch = RuleMatchContext {
            command: Some("git push origin main".into()),
            git_branch: Some("feature".into()),
            ..Default::default()
        };

        assert!(rule.matches_with_context("bash", &main_branch));
        assert!(!rule.matches_with_context("bash", &feature_branch));
    }

    #[test]
    fn current_rule_enforces_network_domain_constraint() {
        let rule = PermissionRule::parse(r#"Network(tool="web_fetch", domain="github.com")"#);
        let github = RuleMatchContext {
            domain: Some("api.github.com".into()),
            ..Default::default()
        };
        let other = RuleMatchContext {
            domain: Some("example.com".into()),
            ..Default::default()
        };

        assert!(rule.matches_with_context("web_fetch", &github));
        assert!(!rule.matches_with_context("web_fetch", &other));
    }

    #[test]
    fn current_rule_enforces_mcp_capability_constraint() {
        let rule = PermissionRule::parse(
            r#"MCP(tool="mcp_jira_create_issue", capability="destructive=false")"#,
        );
        let safe_capability = RuleMatchContext {
            capability: Some("destructive=false".into()),
            ..Default::default()
        };
        let destructive_capability = RuleMatchContext {
            capability: Some("destructive=true".into()),
            ..Default::default()
        };

        assert!(rule.matches_with_context("mcp_jira_create_issue", &safe_capability));
        assert!(!rule.matches_with_context("mcp_jira_create_issue", &destructive_capability));
    }

    #[test]
    fn current_rule_enforces_op_constraint() {
        let rule = PermissionRule::parse(r#"file_write(path_glob="src/**/*.rs", op="write")"#);
        let write_ctx = RuleMatchContext {
            path: Some("src/main.rs".into()),
            op: Some("write".into()),
            ..Default::default()
        };
        let read_ctx = RuleMatchContext {
            path: Some("src/main.rs".into()),
            op: Some("read".into()),
            ..Default::default()
        };

        assert!(rule.matches_with_context("write_file", &write_ctx));
        assert!(rule.matches_with_context("str_replace", &write_ctx));
        assert!(!rule.matches_with_context("read_file", &read_ctx));
        assert!(!rule.matches_with_context("read_file", &write_ctx));
        assert!(!rule.matches_with_context("bash", &write_ctx));
        assert!(!rule.matches_with_context("github", &write_ctx));
    }

    #[test]
    fn current_path_rule_without_glob_matches_exact_path() {
        let rule = PermissionRule::parse(r#"file_write(path_glob="zzzz3.md", op="write")"#);
        let approved = RuleMatchContext {
            path: Some("zzzz3.md".into()),
            op: Some("write".into()),
            ..Default::default()
        };
        let sibling = RuleMatchContext {
            path: Some("zzzz4.md".into()),
            op: Some("write".into()),
            ..Default::default()
        };

        assert!(rule.matches_with_context("write_file", &approved));
        assert!(!rule.matches_with_context("write_file", &sibling));
    }

    #[test]
    fn current_path_prefix_rule_matches_sibling_prefixes() {
        let rule = PermissionRule::parse(r#"file_write(path_prefix="zzz", op="write")"#);
        let approved = RuleMatchContext {
            path: Some("zzz2.md".into()),
            op: Some("write".into()),
            ..Default::default()
        };
        let other = RuleMatchContext {
            path: Some("abc.md".into()),
            op: Some("write".into()),
            ..Default::default()
        };

        assert!(rule.matches_with_context("write_file", &approved));
        assert!(!rule.matches_with_context("write_file", &other));
    }

    #[test]
    fn sync_context_uses_current_constraints_for_allow_rules() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![PermissionRule::parse(
                r#"Bash(argv_prefix="npm test", cwd_root="packages/web")"#,
            )],
            ..Default::default()
        };
        let sync = PermissionSyncContext::new(inherited);
        let web_ctx = RuleMatchContext {
            command: Some("npm test --watch".into()),
            cwd: Some(PathBuf::from("/repo/packages/web")),
            ..Default::default()
        };
        let api_ctx = RuleMatchContext {
            command: Some("npm test --watch".into()),
            cwd: Some(PathBuf::from("/repo/packages/api")),
            ..Default::default()
        };

        assert!(sync.is_allowed_with_context("bash", &web_ctx));
        assert!(!sync.is_allowed_with_context("bash", &api_ctx));
    }

    #[test]
    fn test_inherited_permissions() {
        let mut inherited = InheritedPermissions::default();
        inherited.add_allow(PermissionRule::parse(r#"Bash(argv_prefix="git")"#));
        inherited.add_deny(PermissionRule::parse(r#"Bash(argv_prefix="rm -rf")"#));

        assert!(inherited.is_allowed("bash", Some("git status")));
        assert!(!inherited.is_allowed("bash", Some("npm install")));
        assert!(inherited.is_denied("bash", Some("rm -rf /")));
        assert!(!inherited.is_denied("bash", Some("rm file.txt")));
    }

    #[test]
    fn test_permission_sync_context() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![PermissionRule::parse(r#"Bash(argv_prefix="git")"#)],
            deny_rules: vec![],
            ..Default::default()
        };

        let mut ctx = PermissionSyncContext::new(inherited);
        assert!(ctx.is_allowed("bash", Some("git status")));

        let update = PermissionUpdate::allow(PermissionRule::parse(r#"Bash(argv_prefix="npm")"#));
        ctx.apply_update(&update);
        assert!(ctx.is_allowed("bash", Some("npm install")));

        let child_perms = ctx.for_child(true);
        assert!(child_perms.is_background);
        assert!(child_perms.is_allowed("bash", Some("git status")));
        assert!(child_perms.is_allowed("bash", Some("npm install")));
    }

    #[tokio::test]
    async fn permission_sync_shared_handle_preserves_explicit_envelope() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Deny,
            allowed_tools: Some(["read_file".to_string()].into_iter().collect()),
            ..Default::default()
        };

        let handle = PermissionSyncContext::shared(inherited);
        let ctx = handle.read().await;

        assert_eq!(ctx.mode(), PermissionMode::Deny);
        assert!(ctx.inherited.is_tool_allowed_by_allowlist("read_file"));
        assert!(!ctx.inherited.is_tool_allowed_by_allowlist("bash"));
    }

    #[tokio::test]
    async fn permission_sync_shared_root_uses_root_envelope() {
        let handle = PermissionSyncContext::shared_root(PermissionMode::AcceptEdits);
        let ctx = handle.read().await;

        assert_eq!(ctx.mode(), PermissionMode::AcceptEdits);
        assert!(ctx.inherited.allow_rules.is_empty());
        assert!(ctx.inherited.deny_rules.is_empty());
    }

    #[tokio::test]
    async fn permission_sync_into_shared_preserves_session_updates() {
        let mut ctx = PermissionSyncContext::root(PermissionMode::Prompt);
        ctx.apply_update(&PermissionUpdate::allow(PermissionRule::parse(
            r#"Bash(argv_prefix="cargo test")"#,
        )));

        let handle = ctx.into_shared();
        let shared = handle.read().await;

        assert!(shared.is_allowed("bash", Some("cargo test -p astra-runtime")));
    }

    #[test]
    fn test_permission_response() {
        let response = PermissionResponse::approve()
            .with_update(PermissionUpdate::allow(PermissionRule::tool("write_file")).persistent());

        assert!(response.approved);
        assert_eq!(response.updates.len(), 1);
        assert!(response.updates[0].persist);
    }

    #[test]
    fn test_tool_allowlist() {
        let inherited = InheritedPermissions {
            allowed_tools: Some(
                ["read_file", "grep", "glob"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..Default::default()
        };

        assert!(inherited.is_tool_allowed_by_allowlist("read_file"));
        assert!(inherited.is_tool_allowed_by_allowlist("grep"));
        assert!(!inherited.is_tool_allowed_by_allowlist("bash"));
        assert!(!inherited.is_tool_allowed_by_allowlist("str_replace"));
    }
}
