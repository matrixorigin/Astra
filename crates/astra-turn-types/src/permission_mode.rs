//! Shared permission interaction modes; enforcement remains in turn-core.

use serde::{Deserialize, Serialize};

// ─── Permission Mode ────────────────────────────────────────────────────────

/// Permission mode controls how tool approval decisions are handled.
///
/// Shared between parent and child agents; child inherits parent's mode
/// unless explicitly overridden with a more restrictive mode.
///
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Auto-resolve ordinary approval prompts; git/sensitive gates may still stop.
    Auto,
    /// Skip human approval prompts; absolute safety denies and policy allowlists still apply.
    Bypass,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ManualApprovalPolicy {
    Plan,
    AcceptEdits,
    Prompt,
    Deny,
}

/// Permission mode safe for child-agent inheritance.
///
/// `Bypass` is intentionally excluded: root-user interaction choices
/// must not become transitive child-agent safety policy. The compiler
/// guarantees that no match on this type can ever receive a Bypass
/// variant, unlike the previous approach where an `unreachable!()` arm
/// served the same guarantee at runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChildPermissionMode {
    Auto,
    Plan,
    AcceptEdits,
    Prompt,
    Deny,
}

impl From<ChildPermissionMode> for PermissionMode {
    fn from(m: ChildPermissionMode) -> Self {
        match m {
            ChildPermissionMode::Auto => Self::Auto,
            ChildPermissionMode::Plan => Self::Plan,
            ChildPermissionMode::AcceptEdits => Self::AcceptEdits,
            ChildPermissionMode::Prompt => Self::Prompt,
            ChildPermissionMode::Deny => Self::Deny,
        }
    }
}

impl PermissionMode {
    /// True when ordinary approval prompts can be resolved without blocking.
    ///
    /// This is the approval-interaction axis, not the safety-policy axis:
    /// hard denies and explicit deny/allowlist policy can still apply.
    pub fn auto_resolves_approval_prompts(self) -> bool {
        matches!(self, Self::Auto | Self::Bypass)
    }

    /// True for the explicit "do not interrupt me for approval" mode.
    ///
    /// This skips git/sensitive approval gates, but not absolute safety
    /// denies, explicit deny rules, or child-agent allowlists.
    pub fn skips_human_approval_prompts(self) -> bool {
        matches!(self, Self::Bypass)
    }

    /// True when soft, noisy shell-obfuscation guards are treated as advisory.
    pub fn relaxes_soft_shell_obfuscation(self) -> bool {
        matches!(self, Self::Auto | Self::Bypass)
    }

    /// True when soft git policy findings may be auto-allowed.
    pub fn auto_allows_soft_git_policy(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// Permission mode to export into child agents.
    ///
    /// `Bypass` is a root user-interaction choice: skip approval prompts in
    /// the current UI session. It must not become a transitive child-agent
    /// safety policy because spawned/fan-out agents have no direct user in the
    /// loop and should not inherit the root session's broad prompt bypass.
    ///
    /// Returns a [`ChildPermissionMode`] that cannot represent Bypass,
    /// making the downgrade a compile-time guarantee.
    #[must_use]
    pub fn child_inherited_mode(self) -> ChildPermissionMode {
        match self {
            Self::Bypass => ChildPermissionMode::Auto,
            Self::Auto => ChildPermissionMode::Auto,
            Self::Plan => ChildPermissionMode::Plan,
            Self::AcceptEdits => ChildPermissionMode::AcceptEdits,
            Self::Prompt => ChildPermissionMode::Prompt,
            Self::Deny => ChildPermissionMode::Deny,
        }
    }

    pub fn manual_approval_policy(self) -> Option<ManualApprovalPolicy> {
        match self {
            Self::Auto => None,
            Self::Bypass => None,
            Self::Plan => Some(ManualApprovalPolicy::Plan),
            Self::AcceptEdits => Some(ManualApprovalPolicy::AcceptEdits),
            Self::Prompt => Some(ManualApprovalPolicy::Prompt),
            Self::Deny => Some(ManualApprovalPolicy::Deny),
        }
    }

    /// Stable compact encoding used by CLI/TUI atomic mirrors.
    pub fn mirror_code(self) -> u8 {
        match self {
            Self::Prompt => 0,
            Self::Auto => 1,
            Self::Plan => 2,
            Self::AcceptEdits => 3,
            Self::Deny => 4,
            Self::Bypass => 5,
        }
    }

    /// Decode the compact CLI/TUI mirror encoding. Unknown values fail closed
    /// to Prompt rather than inheriting an accidentally permissive mode.
    pub fn from_mirror_code(value: u8) -> Self {
        match value {
            1 => Self::Auto,
            2 => Self::Plan,
            3 => Self::AcceptEdits,
            4 => Self::Deny,
            5 => Self::Bypass,
            _ => Self::Prompt,
        }
    }

    /// Human label for the active tool-policy chip.
    ///
    /// `Plan` is the internal read-only preset used by the policy engine. The
    /// product label deliberately names the capability rather than pretending
    /// it is the session's Plan lifecycle.
    pub fn chip_text(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Bypass => "Bypass",
            Self::Plan => "Read-only",
            Self::AcceptEdits => "Edits",
            Self::Prompt => "Ask",
            Self::Deny => "Deny",
        }
    }

    /// Color hint for the status-line mode chip.
    /// Returns `(red, green, blue)` for a ratatui-style `Color::Rgb`.
    pub fn chip_color_rgb(self) -> (u8, u8, u8) {
        // Blue for plan, cyan for edit, yellow for auto, magenta for bypass,
        // red for deny, white for default.
        match self {
            Self::Auto => (255, 255, 0),
            Self::Bypass => (255, 0, 255),
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
            Self::Bypass => write!(f, "bypass"),
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
        match s {
            "auto" => Ok(Self::Auto),
            "bypass" => Ok(Self::Bypass),
            "plan" => Ok(Self::Plan),
            "accept_edits" => Ok(Self::AcceptEdits),
            "prompt" => Ok(Self::Prompt),
            "deny" => Ok(Self::Deny),
            _ => Err(format!(
                "invalid permission mode '{s}': expected auto, bypass, plan, accept_edits, prompt, or deny"
            )),
        }
    }
}
