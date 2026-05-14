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
    /// Prompt the user for write/execute tools (default interactive mode).
    #[default]
    Prompt,
    /// Deny all write/execute tools without prompting (CI/headless mode).
    Deny,
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
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
            "prompt" => Ok(Self::Prompt),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "invalid permission mode '{s}': expected auto, prompt, or deny"
            )),
        }
    }
}

// ─── Permission Rules ───────────────────────────────────────────────────────

/// A permission rule that can be inherited or synchronized.
///
/// Format: `tool_name` or `tool_name(pattern:*)` for prefix matching.
/// Examples:
/// - `bash` — matches all bash commands
/// - `bash(git commit:*)` — matches bash commands starting with "git commit"
/// - `edit` — matches all edit operations
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Tool name (lowercase).
    pub tool: String,
    /// Optional command prefix for execute tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
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
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
        }
    }

    /// Convert a parsed grammar-v2 rule into the enforcement shape.
    #[must_use]
    pub fn from_rule_v2(rule: crate::permission_rule_grammar::PermissionRuleV2) -> Self {
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
        let pattern = if lower_family == "bash" {
            rule.argv_prefix.clone()
        } else if matches!(lower_family.as_str(), "edit" | "read" | "view") {
            rule.path_glob.clone()
        } else {
            rule.argv_prefix
                .clone()
                .or_else(|| rule.path_glob.clone())
                .or_else(|| rule.extra.get("pattern").cloned())
        };
        Self {
            tool,
            pattern,
            op: rule.op,
            cwd_root: rule.cwd_root,
            git_branch: rule.git_branch,
            domain: rule.domain,
            capability: rule.capability,
        }
    }

    /// Parse a rule from string format: `Tool` or `Tool(pattern:*)`.
    pub fn parse(rule_str: &str) -> Self {
        if let Ok(rule) = crate::permission_rule_grammar::parse_rule_v2(rule_str) {
            return Self::from_rule_v2(rule);
        }
        if let Some(paren_start) = rule_str.find('(') {
            if let Some(paren_end) = rule_str.rfind(')') {
                let tool = rule_str[..paren_start].to_lowercase();
                let inner = &rule_str[paren_start + 1..paren_end];
                let pattern = inner.trim_end_matches(":*").trim_end_matches('*');
                return Self {
                    tool,
                    pattern: Some(pattern.to_string()),
                    op: None,
                    cwd_root: None,
                    git_branch: None,
                    domain: None,
                    capability: None,
                };
            }
        }
        Self {
            tool: rule_str.to_lowercase(),
            pattern: None,
            op: None,
            cwd_root: None,
            git_branch: None,
            domain: None,
            capability: None,
        }
    }

    /// Check if this rule matches a tool call.
    ///
    /// Issue #326 P5 / R2 Major 2: a pattern that contains glob
    /// metacharacters (`*` / `**` / `?` / `{a,b}`) is matched
    /// against the command string via [`crate::permission_path_glob::glob_match`].
    /// Patterns without metacharacters fall back to the legacy
    /// word-boundary prefix match so existing rules continue to
    /// behave the same way (`Bash(npm test:*)` still allows
    /// `npm test --verbose`).
    pub fn matches(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.matches_with_context(tool_name, &RuleMatchContext::legacy(command))
    }

    /// Check if this rule matches a fully-described tool call.
    pub fn matches_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        if self.tool != tool_name.to_lowercase() {
            return false;
        }
        if let Some(expected) = &self.op
            && !ctx
                .op
                .as_deref()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
        {
            return false;
        }
        if let Some(expected) = &self.cwd_root
            && !ctx
                .cwd
                .as_deref()
                .is_some_and(|cwd| cwd_matches_root(cwd, expected))
        {
            return false;
        }
        if let Some(expected) = &self.git_branch
            && !ctx
                .git_branch
                .as_deref()
                .is_some_and(|actual| actual == expected)
        {
            return false;
        }
        if let Some(expected) = &self.domain
            && !ctx
                .domain
                .as_deref()
                .is_some_and(|actual| domain_matches(expected, actual))
        {
            return false;
        }
        if let Some(expected) = &self.capability
            && !capability_matches(expected, ctx)
        {
            return false;
        }

        let target = if self.tool == "bash" {
            ctx.command.as_deref()
        } else {
            ctx.path.as_deref().or(ctx.command.as_deref())
        };
        match (&self.pattern, target) {
            (None, _) => true, // Bare tool name matches all
            (Some(pattern), Some(cmd)) => {
                if pattern_contains_glob_metachars(pattern) {
                    // Glob path: match the WHOLE command (post-
                    // lower-casing) against the user's pattern.
                    // We don't lowercase the pattern itself
                    // because path globs are case-sensitive on
                    // Linux.
                    let lower_cmd = cmd.to_lowercase();
                    crate::permission_path_glob::glob_match(pattern, &lower_cmd)
                } else {
                    // Legacy word-boundary prefix path.
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

/// Context for v2 permission-rule matching.
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
    pub fn legacy(command: Option<&str>) -> Self {
        Self {
            command: command.map(ToOwned::to_owned),
            path: command.map(ToOwned::to_owned),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn from_tool_args(tool_name: &str, args: &serde_json::Value) -> Self {
        let cwd = std::env::current_dir().ok();
        let op = match crate::cloud_approval_policy::cloud_gated_tool_kind_with_args(
            tool_name,
            Some(args),
        ) {
            Some(crate::cloud_approval_policy::CloudGatedToolKind::Execute) => Some("execute"),
            Some(crate::cloud_approval_policy::CloudGatedToolKind::Write) => Some("write"),
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
            command: crate::tool_argument_hints::command_hint_from_args(args).map(str::to_string),
            path: crate::tool_argument_hints::path_hint_from_args(args),
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
/// metacharacters [`crate::permission_path_glob`] recognizes?
/// Used to decide between glob-match and legacy prefix-match.
fn pattern_contains_glob_metachars(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('{')
}

impl std::fmt::Display for PermissionRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let has_v2_constraints = self.op.is_some()
            || self.cwd_root.is_some()
            || self.git_branch.is_some()
            || self.domain.is_some()
            || self.capability.is_some();
        if has_v2_constraints {
            let mut rule = crate::permission_rule_grammar::PermissionRuleV2 {
                tool: self.tool.clone(),
                argv_prefix: None,
                path_glob: None,
                op: self.op.clone(),
                cwd_root: self.cwd_root.clone(),
                git_branch: self.git_branch.clone(),
                domain: self.domain.clone(),
                capability: self.capability.clone(),
                extra: Default::default(),
            };
            if self.tool == "bash" {
                rule.argv_prefix = self.pattern.clone();
            } else {
                rule.path_glob = self.pattern.clone();
            }
            write!(
                f,
                "{}",
                crate::permission_rule_grammar::serialize_rule_v2(&rule)
            )
        } else {
            match &self.pattern {
                Some(pat) => write!(f, "{}({}:*)", self.tool, pat),
                None => write!(f, "{}", self.tool),
            }
        }
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
    /// Fingerprinted session overrides from the parent. The legacy
    /// `allow_rules` / `deny_rules` above carry **command-prefix-level**
    /// rules; this field carries **per-fingerprint** decisions
    /// (`Bash(cargo test:*) → Allow` is **not** the same as
    /// `Bash(*) → Allow`). Children must consult this BEFORE the legacy
    /// rules so a "user pressed Always on cargo test" decision doesn't
    /// get downgraded to "Bash is fully allowed".
    ///
    /// Stored as a serialized JSON Value so we don't pull
    /// `astra_turn_core::approval_fingerprint::FingerprintedOverrides`
    /// into the public type signature (avoids the cyclic dependency
    /// between `astra-turn-core::permission_types` and
    /// `astra-turn-core::approval_fingerprint`). The deserialization
    /// is best-effort: if a child receives a payload that fails to
    /// parse, it falls back to the legacy allow/deny rules and logs a
    /// warning — never silently downgrades.
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
        self.is_allowed_with_context(tool_name, &RuleMatchContext::legacy(command))
    }

    /// Check if a tool is explicitly allowed by inherited rules.
    pub fn is_allowed_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        self.allow_rules
            .iter()
            .any(|r| r.matches_with_context(tool_name, ctx))
    }

    /// Check if a tool is explicitly denied by inherited rules.
    pub fn is_denied(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_denied_with_context(tool_name, &RuleMatchContext::legacy(command))
    }

    /// Check if a tool is explicitly denied by inherited rules.
    pub fn is_denied_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        self.deny_rules
            .iter()
            .any(|r| r.matches_with_context(tool_name, ctx))
    }

    /// Check if a tool is in the allowed_tools set (if set).
    pub fn is_tool_allowed_by_allowlist(&self, tool_name: &str) -> bool {
        match &self.allowed_tools {
            Some(set) => set.contains(&tool_name.to_lowercase()),
            None => true, // No allowlist = all tools allowed
        }
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
    /// Child's suggested rule if approved (e.g., "bash(git commit:*)").
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

    /// Create a context with no inherited permissions (root agent).
    pub fn root(mode: PermissionMode) -> Self {
        Self::new(InheritedPermissions {
            mode,
            ..Default::default()
        })
    }

    /// Get the effective permission mode.
    pub fn mode(&self) -> PermissionMode {
        self.inherited.mode
    }

    /// Check if a tool is allowed (by inherited or session rules).
    pub fn is_allowed(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_allowed_with_context(tool_name, &RuleMatchContext::legacy(command))
    }

    /// Check if a tool is allowed (by inherited or session rules).
    pub fn is_allowed_with_context(&self, tool_name: &str, ctx: &RuleMatchContext) -> bool {
        // Check session overrides first
        if self
            .session_allow
            .iter()
            .any(|r| r.matches_with_context(tool_name, ctx))
        {
            return true;
        }
        // Check inherited rules
        self.inherited.is_allowed_with_context(tool_name, ctx)
    }

    /// Check if a tool is denied (by inherited or session rules).
    pub fn is_denied(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.is_denied_with_context(tool_name, &RuleMatchContext::legacy(command))
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
    fn test_permission_rule_parse() {
        let rule = PermissionRule::parse("bash");
        assert_eq!(rule.tool, "bash");
        assert!(rule.pattern.is_none());

        let rule = PermissionRule::parse("Bash(git commit:*)");
        assert_eq!(rule.tool, "bash");
        assert_eq!(rule.pattern, Some("git commit".to_string()));
    }

    #[test]
    fn test_permission_rule_matches() {
        let rule = PermissionRule::parse("bash(git commit:*)");

        assert!(rule.matches("bash", Some("git commit -m 'fix'")));
        assert!(rule.matches("Bash", Some("git commit --amend")));
        assert!(!rule.matches("bash", Some("git commitizen")));
        assert!(!rule.matches("bash", Some("git push")));
        assert!(!rule.matches("edit", Some("git commit")));
    }

    #[test]
    fn rule_matches_with_glob_pattern() {
        // Issue #326 P5 / R2 Major 2: glob metacharacters in
        // the pattern switch from prefix-match to full glob.
        let rule = PermissionRule::with_pattern("edit", "src/**/*.rs");
        assert!(rule.matches("edit", Some("src/lib.rs")));
        assert!(rule.matches("edit", Some("src/auth/login.rs")));
        assert!(rule.matches("edit", Some("src/a/b/c/d.rs")));
        assert!(!rule.matches("edit", Some("docs/readme.md")));
    }

    #[test]
    fn rule_matches_with_brace_alternatives() {
        let rule = PermissionRule::with_pattern("edit", "**/*.{rs,ts,js}");
        assert!(rule.matches("edit", Some("lib.rs")));
        assert!(rule.matches("edit", Some("ui/component.ts")));
        assert!(rule.matches("edit", Some("server.js")));
        assert!(!rule.matches("edit", Some("config.toml")));
    }

    #[test]
    fn rule_matches_falls_back_to_prefix_when_no_metachars() {
        // No * / ? / { → legacy word-boundary prefix path
        // remains unchanged so existing v1 rules still work.
        let rule = PermissionRule::parse("bash(npm test:*)");
        assert!(rule.matches("bash", Some("npm test")));
        assert!(rule.matches("bash", Some("npm test --watch")));
        assert!(!rule.matches("bash", Some("npm run deploy")));
    }

    #[test]
    fn v2_rule_enforces_cwd_root_constraint() {
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
    fn v2_rule_enforces_git_branch_constraint() {
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
    fn v2_rule_enforces_network_domain_constraint() {
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
    fn v2_rule_enforces_mcp_capability_constraint() {
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
    fn v2_rule_enforces_op_constraint() {
        let rule = PermissionRule::parse(r#"Edit(path_glob="src/**/*.rs", op="write")"#);
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

        assert!(rule.matches_with_context("edit", &write_ctx));
        assert!(!rule.matches_with_context("edit", &read_ctx));
    }

    #[test]
    fn sync_context_uses_v2_constraints_for_allow_rules() {
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
        inherited.add_allow(PermissionRule::parse("bash(git:*)"));
        inherited.add_deny(PermissionRule::parse("bash(rm -rf:*)"));

        assert!(inherited.is_allowed("bash", Some("git status")));
        assert!(!inherited.is_allowed("bash", Some("npm install")));
        assert!(inherited.is_denied("bash", Some("rm -rf /")));
        assert!(!inherited.is_denied("bash", Some("rm file.txt")));
    }

    #[test]
    fn test_permission_sync_context() {
        let inherited = InheritedPermissions {
            mode: PermissionMode::Prompt,
            allow_rules: vec![PermissionRule::parse("bash(git:*)")],
            deny_rules: vec![],
            ..Default::default()
        };

        let mut ctx = PermissionSyncContext::new(inherited);
        assert!(ctx.is_allowed("bash", Some("git status")));

        let update = PermissionUpdate::allow(PermissionRule::parse("bash(npm:*)"));
        ctx.apply_update(&update);
        assert!(ctx.is_allowed("bash", Some("npm install")));

        let child_perms = ctx.for_child(true);
        assert!(child_perms.is_background);
        assert!(child_perms.is_allowed("bash", Some("git status")));
        assert!(child_perms.is_allowed("bash", Some("npm install")));
    }

    #[test]
    fn test_permission_response() {
        let response = PermissionResponse::approve()
            .with_update(PermissionUpdate::allow(PermissionRule::tool("edit")).persistent());

        assert!(response.approved);
        assert_eq!(response.updates.len(), 1);
        assert!(response.updates[0].persist);
    }

    #[test]
    fn test_tool_allowlist() {
        let inherited = InheritedPermissions {
            allowed_tools: Some(
                ["view", "grep", "glob"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            ),
            ..Default::default()
        };

        assert!(inherited.is_tool_allowed_by_allowlist("view"));
        assert!(inherited.is_tool_allowed_by_allowlist("grep"));
        assert!(!inherited.is_tool_allowed_by_allowlist("bash"));
        assert!(!inherited.is_tool_allowed_by_allowlist("edit"));
    }
}
