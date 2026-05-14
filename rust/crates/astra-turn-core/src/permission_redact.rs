//! Issue #326 P3 / scenario #8: redact secret-looking values
//! when rendering tool args inside the approval card.
//!
//! When the LLM proposes
//!
//! ```text
//! write_file(path=".env", content="OPENAI_API_KEY=sk-12345…")
//! ```
//!
//! the approval card needs to show the path and detail so the
//! user can decide, but the *literal secret value* must be
//! masked — otherwise approving once leaks the key into TUI
//! history, into screen-recordings, into the audit log
//! export, and so on.
//!
//! The actual approval gate continues to receive the un-
//! redacted args (it still needs the full content to apply the
//! patch / call the tool). Redaction is purely for human
//! display.
//!
//! ## Patterns
//!
//! Reuses the existing redaction patterns from
//! `astra_turn_core::safety_middleware::redact_credentials_in_text`
//! so the masked output is byte-identical between "tool output
//! shown in chat" and "tool args shown in approval prompt".
//! That keeps the user's mental model consistent.
//!
//! On top of that, we add detection for *file content* that
//! looks like a secrets file (e.g. `.env`, `.aws/credentials`,
//! `id_rsa`):
//!
//! - When the path matches a sensitive pattern (P5 path glob),
//!   the WHOLE content body is collapsed to a single
//!   `<N bytes redacted>` line with a `.gitignore` reminder.
//! - When the path is anywhere else, only individual
//!   `KEY=value` lines that look like credentials are masked;
//!   the rest of the content shows through so users can see
//!   what's being written.

use crate::safety_middleware::redact_credentials_in_text;

/// Result of redacting an approval-card detail block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedDetail {
    /// The text the UI should display.
    pub display: String,
    /// Whether any redactions were applied. The UI uses this
    /// to render a small `🔒 redacted` chip next to the detail
    /// so users know they're not seeing the raw bytes.
    pub redacted: bool,
    /// Whether the path matched a sensitive-file pattern. When
    /// true, the UI also shows the
    /// "Make sure secrets aren't committed" tip.
    pub sensitive_path: bool,
}

/// Sensitive path patterns. A subset of the gitignore-recommended
/// list; matching here forces full-body redaction.
const SENSITIVE_PATH_PATTERNS: &[&str] = &[
    "**/.env",
    "**/.env.*",
    "**/*.pem",
    "**/*.key",
    "**/id_rsa",
    "**/id_dsa",
    "**/id_ecdsa",
    "**/id_ed25519",
    "**/.ssh/**",
    "**/.aws/credentials",
    "**/.aws/config",
    "**/.npmrc",
    "**/.pypirc",
    "**/credentials.json",
    "**/secrets.toml",
    "**/secrets.yaml",
    "**/secrets.yml",
];

/// Redact an approval-card detail block.
///
/// `content` is the file/tool detail text to display. `path`
/// is the target path (when known) — used to detect
/// sensitive-file paths for full-body redaction.
#[must_use]
pub fn redact_for_approval_display(content: &str, path: Option<&str>) -> RedactedDetail {
    // Step 1: sensitive-path full-body collapse.
    if let Some(p) = path {
        if matches_sensitive_path(p) {
            let bytes = content.len();
            return RedactedDetail {
                display: format!(
                    "<{bytes} bytes redacted>\n⚠ This path is on the gitignore-recommended list. \
                     Make sure secrets aren't committed."
                ),
                redacted: true,
                sensitive_path: true,
            };
        }
    }

    // Step 2: line-level credential redaction via the shared
    // pattern set.
    let (redacted_text, redaction_count) = redact_credentials_in_text(content);
    RedactedDetail {
        display: redacted_text,
        redacted: redaction_count > 0,
        sensitive_path: false,
    }
}

/// Returns true if `path` matches any of the sensitive-file
/// glob patterns. Uses [`crate::permission_path_glob::glob_match`]
/// so the pattern semantics are identical to the rule grammar's
/// `path_glob` matcher.
#[must_use]
pub fn matches_sensitive_path(path: &str) -> bool {
    SENSITIVE_PATH_PATTERNS
        .iter()
        .any(|p| crate::permission_path_glob::glob_match(p, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Sensitive path: full-body redaction ───────────────────

    #[test]
    fn dotenv_content_is_fully_redacted() {
        let content = "OPENAI_API_KEY=sk-1234567890abcdef\nDB_PASSWORD=hunter2";
        let r = redact_for_approval_display(content, Some(".env"));
        assert!(r.sensitive_path);
        assert!(r.redacted);
        assert!(r.display.contains("redacted"));
        assert!(r.display.contains("gitignore"));
        // Raw values must NOT appear.
        assert!(!r.display.contains("sk-1234567890abcdef"));
        assert!(!r.display.contains("hunter2"));
    }

    #[test]
    fn env_local_is_fully_redacted() {
        let r = redact_for_approval_display("X=y", Some("config/.env.local"));
        assert!(r.sensitive_path);
    }

    #[test]
    fn pem_file_is_fully_redacted() {
        let r = redact_for_approval_display("-----BEGIN", Some("ssl/server.pem"));
        assert!(r.sensitive_path);
    }

    #[test]
    fn ssh_key_is_fully_redacted() {
        let r = redact_for_approval_display("ssh-rsa AAA...", Some(".ssh/id_rsa"));
        assert!(r.sensitive_path);
    }

    #[test]
    fn aws_credentials_file_is_fully_redacted() {
        let r = redact_for_approval_display("[default]\nkey", Some(".aws/credentials"));
        assert!(r.sensitive_path);
    }

    // ── Non-sensitive path: line-level redaction ─────────────

    #[test]
    fn ordinary_path_with_secret_assignment_is_line_redacted() {
        let content = "// production config\nOPENAI_API_KEY=sk-123456789abcdef\nport = 8080";
        let r = redact_for_approval_display(content, Some("src/config.rs"));
        assert!(!r.sensitive_path);
        assert!(r.redacted);
        assert!(r.display.contains("port = 8080"), "non-secret lines stay");
        assert!(
            !r.display.contains("sk-123456789abcdef"),
            "secret value masked"
        );
    }

    #[test]
    fn ordinary_path_without_secrets_is_unchanged() {
        let content = "fn main() {\n    println!(\"hello\");\n}";
        let r = redact_for_approval_display(content, Some("src/main.rs"));
        assert!(!r.sensitive_path);
        assert!(!r.redacted);
        assert_eq!(r.display, content);
    }

    // ── Path-less detail (tool args without a path field) ─────

    #[test]
    fn no_path_falls_back_to_line_redaction() {
        let content = "Set OPENAI_API_KEY=sk-1234567890abcdef in your env";
        let r = redact_for_approval_display(content, None);
        assert!(!r.sensitive_path);
        // The shared redactor catches this pattern.
        assert!(
            r.redacted,
            "pattern in plain text should be redacted, got: {}",
            r.display
        );
    }
}
