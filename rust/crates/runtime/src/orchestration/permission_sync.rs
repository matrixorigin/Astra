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

// ─── Permission Mode ────────────────────────────────────────────────────────

/// Permission mode controls how tool approval decisions are handled.
///
/// Shared between parent and child agents; child inherits parent's mode
/// unless explicitly overridden with a more restrictive mode.
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
}

impl PermissionRule {
    /// Create a rule for a tool with no pattern (matches all uses).
    pub fn tool(name: impl Into<String>) -> Self {
        Self {
            tool: name.into().to_lowercase(),
            pattern: None,
        }
    }

    /// Create a rule for a tool with a command prefix pattern.
    pub fn with_pattern(name: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self {
            tool: name.into().to_lowercase(),
            pattern: Some(pattern.into()),
        }
    }

    /// Parse a rule from string format: `Tool` or `Tool(pattern:*)`.
    pub fn parse(rule_str: &str) -> Self {
        if let Some(paren_start) = rule_str.find('(') {
            if let Some(paren_end) = rule_str.rfind(')') {
                let tool = rule_str[..paren_start].to_lowercase();
                let inner = &rule_str[paren_start + 1..paren_end];
                let pattern = inner.trim_end_matches(":*").trim_end_matches('*');
                return Self {
                    tool,
                    pattern: Some(pattern.to_string()),
                };
            }
        }
        Self {
            tool: rule_str.to_lowercase(),
            pattern: None,
        }
    }

    /// Check if this rule matches a tool call.
    pub fn matches(&self, tool_name: &str, command: Option<&str>) -> bool {
        if self.tool != tool_name.to_lowercase() {
            return false;
        }
        match (&self.pattern, command) {
            (None, _) => true, // Bare tool name matches all
            (Some(prefix), Some(cmd)) => {
                let lower_cmd = cmd.to_lowercase();
                let lower_prefix = prefix.to_lowercase();
                // Prefix match with word boundary
                if !lower_cmd.starts_with(&lower_prefix) {
                    return false;
                }
                let rest = &lower_cmd[lower_prefix.len()..];
                rest.is_empty()
                    || rest.starts_with(char::is_whitespace)
                    || rest.starts_with(&['-', '=', ';', '|', '&', '>', '<'][..])
            }
            (Some(_), None) => false, // Pattern rule but no command to match
        }
    }
}

impl std::fmt::Display for PermissionRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.pattern {
            Some(pat) => write!(f, "{}({}:*)", self.tool, pat),
            None => write!(f, "{}", self.tool),
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
}

impl InheritedPermissions {
    /// Create inherited permissions with auto-approve mode (no restrictions).
    pub fn auto_approve() -> Self {
        Self {
            mode: PermissionMode::Auto,
            ..Default::default()
        }
    }

    /// Create inherited permissions with deny mode (all write/execute denied).
    pub fn deny_all() -> Self {
        Self {
            mode: PermissionMode::Deny,
            ..Default::default()
        }
    }

    /// Check if a tool is explicitly allowed by inherited rules.
    pub fn is_allowed(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.allow_rules
            .iter()
            .any(|r| r.matches(tool_name, command))
    }

    /// Check if a tool is explicitly denied by inherited rules.
    pub fn is_denied(&self, tool_name: &str, command: Option<&str>) -> bool {
        self.deny_rules
            .iter()
            .any(|r| r.matches(tool_name, command))
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
/// - Pending permission requests
pub struct PermissionSyncContext {
    /// Permissions inherited from parent agent.
    pub inherited: InheritedPermissions,
    /// Session-level overrides (approved/denied during this run).
    session_allow: Vec<PermissionRule>,
    session_deny: Vec<PermissionRule>,
}

impl PermissionSyncContext {
    /// Create a new sync context with inherited permissions.
    pub fn new(inherited: InheritedPermissions) -> Self {
        Self {
            inherited,
            session_allow: Vec::new(),
            session_deny: Vec::new(),
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
        // Check session overrides first
        if self
            .session_allow
            .iter()
            .any(|r| r.matches(tool_name, command))
        {
            return true;
        }
        // Check inherited rules
        self.inherited.is_allowed(tool_name, command)
    }

    /// Check if a tool is denied (by inherited or session rules).
    pub fn is_denied(&self, tool_name: &str, command: Option<&str>) -> bool {
        // Check session overrides first
        if self
            .session_deny
            .iter()
            .any(|r| r.matches(tool_name, command))
        {
            return true;
        }
        // Check inherited rules
        self.inherited.is_denied(tool_name, command)
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

    /// Export current session rules as updates (for propagation to children).
    pub fn export_session_rules(&self) -> Vec<PermissionUpdate> {
        let mut updates = Vec::new();
        for rule in &self.session_allow {
            updates.push(PermissionUpdate::allow(rule.clone()));
        }
        for rule in &self.session_deny {
            updates.push(PermissionUpdate::deny(rule.clone()));
        }
        updates
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
}

// ─── Tests ──────────────────────────────────────────────────────────────────

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

        // Inherited rule works
        assert!(ctx.is_allowed("bash", Some("git status")));

        // Apply session override
        let update = PermissionUpdate::allow(PermissionRule::parse("bash(npm:*)"));
        ctx.apply_update(&update);
        assert!(ctx.is_allowed("bash", Some("npm install")));

        // Export for child
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
            allowed_tools: Some(["view", "grep", "glob"].iter().map(|s| s.to_string()).collect()),
            ..Default::default()
        };

        assert!(inherited.is_tool_allowed_by_allowlist("view"));
        assert!(inherited.is_tool_allowed_by_allowlist("grep"));
        assert!(!inherited.is_tool_allowed_by_allowlist("bash"));
        assert!(!inherited.is_tool_allowed_by_allowlist("edit"));
    }
}
