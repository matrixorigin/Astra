//! Structured interruption records for resumable session recovery.
//!
//! Every early-exit path in the agentic loop (budget exhaustion, rate limiting,
//! cancellation, context overflow, auth failure, critical verdict) produces an
//! [`InterruptionRecord`] that captures *why* the loop stopped and *what* the
//! caller should do to resume. This record is persisted to both the heavy
//! checkpoint and the session journal, giving downstream consumers (CLI resume,
//! API continuation, observability) a machine-readable interruption contract.

use serde::{Deserialize, Serialize};

/// Classification of why the agentic loop was interrupted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionKind {
    /// Inner-loop turn budget exhausted (`remaining_turns == 0`).
    BudgetExhausted,
    /// Per-turn input token limit exceeded.
    TokenBudgetExceeded,
    /// Cumulative token budget across turns exceeded.
    CumulativeBudgetExceeded,
    /// LLM API returned 429 / TPM / RPM limit.
    RateLimited,
    /// Rate-limit cooldown rejected further requests.
    CooldownRejected,
    /// User cancelled via flag or cancellation token.
    UserCancelled,
    /// LLM context window overflow (prompt too long).
    ContextOverflow,
    /// Authentication or credential failure (401 / unauthorized).
    AuthFailure,
    /// TurnGuard verdict reached critical severity.
    CriticalVerdict,
    /// Approval rejected after tool progress.
    ApprovalRejected,
    /// Server overload (503 / 529).
    ServerOverload,
}

impl InterruptionKind {
    /// Human-readable label for display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BudgetExhausted => "budget_exhausted",
            Self::TokenBudgetExceeded => "token_budget_exceeded",
            Self::CumulativeBudgetExceeded => "cumulative_budget_exceeded",
            Self::RateLimited => "rate_limited",
            Self::CooldownRejected => "cooldown_rejected",
            Self::UserCancelled => "user_cancelled",
            Self::ContextOverflow => "context_overflow",
            Self::AuthFailure => "auth_failure",
            Self::CriticalVerdict => "critical_verdict",
            Self::ApprovalRejected => "approval_rejected",
            Self::ServerOverload => "server_overload",
        }
    }

    /// Whether progress made before this interruption is preserved and resumable.
    #[must_use]
    pub fn is_resumable(self) -> bool {
        match self {
            Self::BudgetExhausted
            | Self::TokenBudgetExceeded
            | Self::CumulativeBudgetExceeded
            | Self::RateLimited
            | Self::CooldownRejected
            | Self::UserCancelled
            | Self::CriticalVerdict
            | Self::ServerOverload => true,
            Self::ContextOverflow => true, // resumable with compaction
            Self::AuthFailure => false,    // needs external credential refresh
            Self::ApprovalRejected => true,
        }
    }
}

/// What the caller should do after an interruption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeAction {
    /// User can immediately send a new message to continue.
    ContinueImmediately,
    /// Wait for the specified duration (seconds) before retrying.
    WaitAndRetry { delay_seconds: u64 },
    /// External intervention required (e.g., credential refresh).
    RequiresIntervention { description: String },
    /// Context must be compacted before retry.
    CompactAndRetry,
    /// Session should be terminated; start a new one.
    StartNewSession,
}

/// Structured record of an agentic loop interruption.
///
/// Captures enough context for the next turn/session to understand what happened
/// and what to do. Persisted to checkpoint and journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InterruptionRecord {
    /// Classification of the interruption.
    pub kind: InterruptionKind,
    /// Recommended next action for the caller.
    pub resume_action: ResumeAction,
    /// Whether a heavy checkpoint was written before/during the interruption.
    pub has_checkpoint: bool,
    /// Number of tool calls completed before the interruption.
    pub tool_calls_completed: u32,
    /// Agentic turns completed before the interruption.
    pub turns_completed: u32,
    /// Remaining turns in the budget (0 for budget exhaustion).
    pub remaining_turns: u32,
    /// Optional error message from the provider/runtime.
    pub error_detail: Option<String>,
    /// Human-readable summary for the user.
    pub user_message: String,
}

impl InterruptionRecord {
    /// Create a new interruption record.
    pub fn new(
        kind: InterruptionKind,
        resume_action: ResumeAction,
        state_summary: InterruptionStateSummary,
    ) -> Self {
        let user_message = Self::format_user_message(kind, &resume_action, &state_summary);
        Self {
            kind,
            resume_action,
            has_checkpoint: state_summary.has_checkpoint,
            tool_calls_completed: state_summary.tool_calls_completed,
            turns_completed: state_summary.turns_completed,
            remaining_turns: state_summary.remaining_turns,
            error_detail: state_summary.error_detail,
            user_message,
        }
    }

    /// Serialize to JSON for journal/checkpoint embedding.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": self.kind.label(),
            "resumable": self.kind.is_resumable(),
            "resume_action": self.resume_action,
            "has_checkpoint": self.has_checkpoint,
            "tool_calls_completed": self.tool_calls_completed,
            "turns_completed": self.turns_completed,
            "remaining_turns": self.remaining_turns,
            "error_detail": self.error_detail,
            "user_message": self.user_message,
        })
    }

    fn format_user_message(
        kind: InterruptionKind,
        action: &ResumeAction,
        summary: &InterruptionStateSummary,
    ) -> String {
        let checkpoint_note = if summary.has_checkpoint {
            " A checkpoint was saved."
        } else {
            ""
        };
        let tool_note = if summary.tool_calls_completed > 0 {
            format!(" {} tool call(s) completed.", summary.tool_calls_completed)
        } else {
            String::new()
        };
        let action_note = match action {
            ResumeAction::ContinueImmediately => {
                " You can continue in the next message.".to_string()
            }
            ResumeAction::WaitAndRetry { delay_seconds } => {
                format!(" Please wait ~{delay_seconds}s before retrying.")
            }
            ResumeAction::RequiresIntervention { description } => {
                format!(" Action required: {description}")
            }
            ResumeAction::CompactAndRetry => " Context will be compacted before retry.".to_string(),
            ResumeAction::StartNewSession => " Please start a new session.".to_string(),
        };
        format!(
            "[{kind}]{tool_note}{checkpoint_note}{action_note}",
            kind = kind.label()
        )
    }
}

/// Snapshot of loop state at interruption time (used to build `InterruptionRecord`).
pub struct InterruptionStateSummary {
    pub has_checkpoint: bool,
    pub tool_calls_completed: u32,
    pub turns_completed: u32,
    pub remaining_turns: u32,
    pub error_detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interruption_kind_labels_are_snake_case() {
        let kinds = [
            InterruptionKind::BudgetExhausted,
            InterruptionKind::TokenBudgetExceeded,
            InterruptionKind::RateLimited,
            InterruptionKind::UserCancelled,
            InterruptionKind::ContextOverflow,
            InterruptionKind::AuthFailure,
            InterruptionKind::CriticalVerdict,
        ];
        for kind in kinds {
            let label = kind.label();
            assert!(!label.is_empty());
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label should be snake_case: {label}"
            );
        }
    }

    #[test]
    fn budget_exhausted_is_resumable() {
        assert!(InterruptionKind::BudgetExhausted.is_resumable());
        assert!(InterruptionKind::RateLimited.is_resumable());
        assert!(InterruptionKind::UserCancelled.is_resumable());
    }

    #[test]
    fn auth_failure_is_not_resumable() {
        assert!(!InterruptionKind::AuthFailure.is_resumable());
    }

    #[test]
    fn record_serializes_to_json() {
        let record = InterruptionRecord::new(
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 5,
                turns_completed: 15,
                remaining_turns: 0,
                error_detail: None,
            },
        );
        let json = record.to_json();
        assert_eq!(json["kind"], "budget_exhausted");
        assert_eq!(json["resumable"], true);
        assert_eq!(json["has_checkpoint"], true);
        assert_eq!(json["tool_calls_completed"], 5);
    }

    #[test]
    fn rate_limit_with_delay() {
        let record = InterruptionRecord::new(
            InterruptionKind::RateLimited,
            ResumeAction::WaitAndRetry { delay_seconds: 30 },
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 3,
                turns_completed: 2,
                remaining_turns: 8,
                error_detail: Some("429 Too Many Requests".to_string()),
            },
        );
        assert!(record.user_message.contains("rate_limited"));
        assert!(record.user_message.contains("30s"));
        assert!(record.error_detail.is_some());
    }

    #[test]
    fn context_overflow_suggests_compaction() {
        let record = InterruptionRecord::new(
            InterruptionKind::ContextOverflow,
            ResumeAction::CompactAndRetry,
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 10,
                turns_completed: 5,
                remaining_turns: 5,
                error_detail: Some("context_length_exceeded".to_string()),
            },
        );
        assert!(record.kind.is_resumable());
        assert!(record.user_message.contains("compacted"));
    }
}
