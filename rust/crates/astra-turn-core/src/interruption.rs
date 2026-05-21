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
    /// The loop reached a terminal state but never produced a final answer.
    EmptyCompletion,
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
    /// Harness verifier returned Fatal → session blocked.
    HarnessBlocked,
    /// Harness debug breakpoint hit → session paused.
    HarnessPaused,
}

impl InterruptionKind {
    /// Human-readable label for display.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::BudgetExhausted => "budget_exhausted",
            Self::EmptyCompletion => "empty_completion",
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
            Self::HarnessBlocked => "harness_blocked",
            Self::HarnessPaused => "harness_paused",
        }
    }

    /// Whether progress made before this interruption is preserved and resumable.
    #[must_use]
    pub fn is_resumable(self) -> bool {
        match self {
            Self::BudgetExhausted
            | Self::EmptyCompletion
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
            Self::HarnessBlocked => false,
            Self::HarnessPaused => true,
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
    /// Optional stall-signal breadcrumb. Populated when the
    /// interruption is caused by a measurable loop-guard condition so
    /// the resumed session can see *why* it was cut (e.g.,
    /// `"single_tool_streak=18"`) and the LLM can self-correct on
    /// continuation. `None` for interruptions with no such signal
    /// (rate limits, auth failures, user cancellations).
    ///
    /// Backwards compatible: older persisted records without this
    /// field deserialize with `stall_signal = None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stall_signal: Option<String>,
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
            stall_signal: state_summary.stall_signal,
        }
    }

    /// Attach a stall-signal breadcrumb (builder style). Typically
    /// called by the finalization layer when a loop-guard condition
    /// contributed to the interruption.
    #[must_use]
    pub fn with_stall_signal(mut self, signal: impl Into<String>) -> Self {
        self.stall_signal = Some(signal.into());
        self
    }

    /// Serialize to JSON for journal/checkpoint embedding.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::json!({
            "kind": self.kind.label(),
            "resumable": self.kind.is_resumable(),
            "resume_action": self.resume_action,
            "has_checkpoint": self.has_checkpoint,
            "tool_calls_completed": self.tool_calls_completed,
            "turns_completed": self.turns_completed,
            "remaining_turns": self.remaining_turns,
            "error_detail": self.error_detail,
            "user_message": self.user_message,
        });
        if let Some(ref sig) = self.stall_signal
            && let Some(obj) = v.as_object_mut()
        {
            obj.insert(
                "stall_signal".to_string(),
                serde_json::Value::String(sig.clone()),
            );
        }
        v
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
        match kind {
            InterruptionKind::EmptyCompletion => format!(
                "[{}] The run ended without a final answer.{tool_note}{checkpoint_note}{action_note}",
                kind.label()
            ),
            _ => format!(
                "[{kind}]{tool_note}{checkpoint_note}{action_note}",
                kind = kind.label()
            ),
        }
    }
}

/// Snapshot of loop state at interruption time (used to build `InterruptionRecord`).
#[derive(Debug, Clone, Default)]
pub struct InterruptionStateSummary {
    pub has_checkpoint: bool,
    pub tool_calls_completed: u32,
    pub turns_completed: u32,
    pub remaining_turns: u32,
    pub error_detail: Option<String>,
    /// Optional stall-signal breadcrumb. See
    /// [`InterruptionRecord::stall_signal`] for semantics. Leave `None`
    /// when no loop-guard condition was active at interruption time.
    pub stall_signal: Option<String>,
}

fn parse_kv_stall_signal(signal: &str) -> std::collections::BTreeMap<&str, &str> {
    signal
        .split(';')
        .filter_map(|segment| segment.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect()
}

/// Build a system-level resume guidance message from a persisted interruption record.
///
/// When a session is restored from a checkpoint that was written during an
/// interruption, this function produces a system message that tells the LLM
/// what happened and how to proceed. Injected at the top of the context
/// window so the model can adjust its plan accordingly.
///
/// Returns `None` if the interruption JSON is missing or unparseable.
#[must_use]
pub fn build_resume_guidance(interruption_json: &serde_json::Value) -> Option<String> {
    build_resume_guidance_with_context(interruption_json, None)
}

/// Like [`build_resume_guidance`] but accepts optional compaction context for
/// richer context-overflow recovery advice.
#[must_use]
pub fn build_resume_guidance_with_context(
    interruption_json: &serde_json::Value,
    compaction_context: Option<&CompactionResumeContext>,
) -> Option<String> {
    let kind = interruption_json.get("kind")?.as_str()?;
    let resumable = interruption_json
        .get("resumable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let tool_calls = interruption_json
        .get("tool_calls_completed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let turns = interruption_json
        .get("turns_completed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let has_checkpoint = interruption_json
        .get("has_checkpoint")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let user_msg = interruption_json
        .get("user_message")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !resumable {
        return None;
    }

    let mut guidance = String::new();
    guidance.push_str("[RESUME CONTEXT] This session was previously interrupted.\n");
    guidance.push_str(&format!("  Reason: {kind}\n"));
    guidance.push_str(&format!(
        "  Progress: {turns} turn(s), {tool_calls} tool call(s) completed\n"
    ));
    if has_checkpoint {
        guidance.push_str("  Checkpoint: saved — prior tool results are preserved in context\n");
    }

    let stall_signal = interruption_json
        .get("stall_signal")
        .and_then(|v| v.as_str());

    // Kind-specific advice
    match kind {
        "empty_completion" => {
            guidance.push_str(
                "  Action: Continue from the preserved context and produce a direct final answer. \
                 Do not stop after hidden reasoning with no user-visible output.\n",
            );
        }
        "budget_exhausted" | "token_budget_exceeded" | "cumulative_budget_exceeded" => {
            guidance.push_str(
                "  Action: Prioritize completing the most important remaining work first. \
                  Avoid exploratory tool calls — focus on delivering a result.\n",
            );
            if let Some(sig) = stall_signal {
                if sig.starts_with("single_tool_streak=") {
                    let streak = sig.trim_start_matches("single_tool_streak=");
                    guidance.push_str(&format!(
                        "  Cause: the previous run used exactly ONE tool per round for {streak} \
                         consecutive rounds, which exhausted the per-turn round budget. On \
                         resume, batch independent calls (different files / greps / reads) \
                         into a single parallel round instead.\n"
                    ));
                } else if sig.starts_with("exploration_family=") {
                    let kv = parse_kv_stall_signal(sig);
                    if let (Some(family), Some(streak)) =
                        (kv.get("exploration_family"), kv.get("streak"))
                    {
                        let family_hint = match *family {
                            "read" => "kept reopening files that were already in context",
                            "search" => "kept expanding search instead of converging on a target",
                            "diff" => "kept diff-scanning without switching to synthesis or action",
                            other => {
                                guidance.push_str(&format!(
                                    "  Cause: the previous run stayed inside the same {other} exploration family for {streak} consecutive rounds. \
                                     On resume, synthesize what is already known and switch tool families only if one specific fact is still missing.\n"
                                ));
                                ""
                            }
                        };
                        if !family_hint.is_empty() {
                            guidance.push_str(&format!(
                                "  Cause: the previous run stayed inside the same {family} exploration family for {streak} consecutive rounds and {family_hint}. \
                                 On resume, reuse the evidence already gathered and only fetch one specific missing fact if you still cannot finish.\n"
                            ));
                        }
                    }
                } else if sig.starts_with("redundant_reads=") {
                    let count = sig.trim_start_matches("redundant_reads=");
                    guidance.push_str(&format!(
                        "  Cause: the previous run re-read overlapping file ranges {count} time(s) without any intervening edit. \
                         On resume, reuse the file content already in context instead of reopening the same ranges.\n"
                    ));
                }
            }
            if let Some(detail) = interruption_json
                .get("error_detail")
                .and_then(|v| v.as_str())
                .filter(|detail| detail.contains("Likely cause:"))
            {
                guidance.push_str(&format!("  Runtime detail: {detail}\n"));
            }
        }
        "rate_limited" | "cooldown_rejected" | "server_overload" => {
            guidance.push_str(
                "  Action: The rate limit has likely expired. Resume normally, \
                 but batch tool calls to minimize API round-trips.\n",
            );
        }
        "context_overflow" => {
            guidance.push_str(
                "  Action: Context was compacted. Some older tool results may be \
                 summarized. Re-read any files you need before making edits.\n",
            );
            // Enrich with compaction effectiveness context if available.
            if let Some(ctx) = compaction_context {
                if ctx.compaction_attempts > 0 {
                    guidance.push_str(&format!(
                        "  Compaction: {} attempt(s), ~{} tokens freed total",
                        ctx.compaction_attempts, ctx.total_tokens_freed
                    ));
                    if ctx.last_was_insufficient {
                        guidance.push_str(
                            " (last compaction was insufficient — context may still be tight)",
                        );
                    }
                    guidance.push('\n');
                    guidance.push_str(
                        "  Tip: Keep responses concise and avoid requesting large file dumps.\n",
                    );
                }
            }
        }
        "user_cancelled" => {
            guidance.push_str(
                "  Action: The user cancelled the previous run. Wait for their \
                 instructions before proceeding.\n",
            );
        }
        "critical_verdict" => {
            guidance.push_str(
                "  Action: The previous run was stopped by TurnGuard due to repeated \
                 errors and stalls. Review what went wrong, try a different approach, \
                 and avoid the tool patterns that caused failures.\n",
            );
        }
        "approval_rejected" => {
            guidance.push_str(
                "  Action: Tool approvals were repeatedly denied. Use only read-only \
                 tools or ask the user for explicit permission before attempting \
                 write operations.\n",
            );
        }
        _ => {
            if !user_msg.is_empty() {
                guidance.push_str(&format!("  Detail: {user_msg}\n"));
            }
        }
    }

    Some(guidance)
}

/// Context about compaction history for enriching resume guidance.
#[derive(Debug, Clone, Default)]
pub struct CompactionResumeContext {
    /// How many compaction attempts were made before interruption.
    pub compaction_attempts: u32,
    /// Total tokens freed across all compaction attempts.
    pub total_tokens_freed: u64,
    /// Whether the last compaction was insufficient (still got a context error after).
    pub last_was_insufficient: bool,
}

/// Map an [`ErrorKind`] to an [`InterruptionKind`] and [`ResumeAction`].
///
/// This is the structured replacement for [`classify_error`]. When the caller
/// already has a [`ClassifiedError`], use this instead of re-parsing the string.
#[must_use]
pub fn interruption_from_error_kind(
    kind: astra_core::ErrorKind,
) -> Option<(InterruptionKind, ResumeAction)> {
    use astra_core::ErrorKind;
    match kind {
        ErrorKind::Auth => Some((
            InterruptionKind::AuthFailure,
            ResumeAction::RequiresIntervention {
                description: "API key or credentials are invalid — please refresh.".into(),
            },
        )),
        ErrorKind::RateLimit => Some((
            InterruptionKind::RateLimited,
            ResumeAction::WaitAndRetry { delay_seconds: 30 },
        )),
        ErrorKind::ContextWindow => Some((
            InterruptionKind::ContextOverflow,
            ResumeAction::CompactAndRetry,
        )),
        ErrorKind::ServerError => Some((
            InterruptionKind::ServerOverload,
            ResumeAction::WaitAndRetry { delay_seconds: 60 },
        )),
        ErrorKind::BudgetExhausted => Some((
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
        )),
        ErrorKind::Cancelled => Some((
            InterruptionKind::UserCancelled,
            ResumeAction::ContinueImmediately,
        )),
        _ => None,
    }
}

/// Classify a streaming/API error string into an [`InterruptionKind`] and
/// [`ResumeAction`], if the error matches a known pattern.
///
/// Used as a catch-all at the end of the fatal-error path so that *every*
/// early exit produces a structured interruption record rather than a bare
/// string error.
///
/// Prefer [`interruption_from_error_kind`] when a [`ClassifiedError`] is available.
#[must_use]
pub fn classify_error(error: &str) -> Option<(InterruptionKind, ResumeAction)> {
    let lower = error.to_lowercase();

    // Auth / credential failures. Keep this list synced with the
    // providers we ship (Bedrock / Anthropic direct / OpenAI / MiniMax).
    // Session f5d6ef02 regression: Bedrock emits "Could not validate
    // credentials" which none of the original patterns matched — five
    // turn_errors in that session were labelled [unknown] with vacuous
    // guidance.
    //
    // NOTE: `403` / `forbidden` also covers IAM/region-permission denials
    // that aren't strictly a credential-refresh problem. The resume
    // description below is worded generically ("invalid — please refresh")
    // because for our UX both cases need user intervention, not a retry.
    let has_token = lower.contains("token");
    let has_expired = lower.contains("expired");
    // NOTE: bare "401" / "403" substring match is intentionally avoided —
    // unrelated error payloads (timeouts in ms, byte offsets, body snippets
    // containing the digits) produced spurious credential-refresh prompts.
    // We require the status code to appear in an HTTP-shaped phrase.
    if lower.contains("401 unauthorized")
        || lower.contains("status: 401")
        || lower.contains("status code: 401")
        || lower.contains("http 401")
        || lower.contains("403 forbidden")
        || lower.contains("status: 403")
        || lower.contains("status code: 403")
        || lower.contains("http 403")
        || lower.contains("forbidden")
        || lower.contains("unauthorized")
        || lower.contains("authentication")
        // Tightened: require the verb context so we don't match benign
        // strings like "credential helper not found" from git/keystore.
        || lower.contains("validate credentials")
        || lower.contains("invalid credentials")
        || lower.contains("missing credentials")
        || lower.contains("credentials are")
        // AWS STS / OAuth session-token expiry — covers all shapes:
        // "expired token", "token is expired", "token has expired",
        // "security token ... is expired".
        || (has_token && has_expired)
        || (lower.contains("invalid") && lower.contains("key"))
        || lower.contains("api key")
    {
        return Some((
            InterruptionKind::AuthFailure,
            ResumeAction::RequiresIntervention {
                description: "API key or credentials are invalid — please refresh.".into(),
            },
        ));
    }

    // Rate limiting (429 / TPM / RPM)
    if lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || lower.contains("too many requests")
        || lower.contains("tpm")
        || lower.contains("rpm")
    {
        return Some((
            InterruptionKind::RateLimited,
            ResumeAction::WaitAndRetry { delay_seconds: 30 },
        ));
    }

    // Context window overflow
    if lower.contains("context_length_exceeded")
        || lower.contains("context window")
        || lower.contains("context_window")
        || lower.contains("prompt is too long")
        || lower.contains("too many tokens")
        || lower.contains("maximum context length")
    {
        return Some((
            InterruptionKind::ContextOverflow,
            ResumeAction::CompactAndRetry,
        ));
    }

    // Server overload (503 / 529)
    if lower.contains("503")
        || lower.contains("529")
        || lower.contains("overload")
        || lower.contains("service unavailable")
    {
        return Some((
            InterruptionKind::ServerOverload,
            ResumeAction::WaitAndRetry { delay_seconds: 60 },
        ));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interruption_kind_labels_are_snake_case() {
        let kinds = [
            InterruptionKind::BudgetExhausted,
            InterruptionKind::EmptyCompletion,
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
        assert!(InterruptionKind::EmptyCompletion.is_resumable());
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
                stall_signal: None,
            },
        );
        let json = record.to_json();
        assert_eq!(json["kind"], "budget_exhausted");
        assert_eq!(json["resumable"], true);
        assert_eq!(json["has_checkpoint"], true);
        assert_eq!(json["tool_calls_completed"], 5);
        assert!(
            json.get("stall_signal").is_none(),
            "stall_signal omitted from JSON when None for backwards compat: {json:?}"
        );
    }

    #[test]
    fn record_with_stall_signal_serializes_the_breadcrumb() {
        let record = InterruptionRecord::new(
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 18,
                turns_completed: 18,
                remaining_turns: 131,
                error_detail: None,
                stall_signal: Some("single_tool_streak=18".to_string()),
            },
        );
        let json = record.to_json();
        assert_eq!(
            json["stall_signal"], "single_tool_streak=18",
            "the resumed session must see *why* the loop was cut so the LLM can self-correct"
        );
    }

    #[test]
    fn with_stall_signal_builder_attaches_breadcrumb() {
        let record = InterruptionRecord::new(
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary::default(),
        )
        .with_stall_signal("single_tool_streak=7");
        assert_eq!(record.stall_signal.as_deref(), Some("single_tool_streak=7"));
        assert_eq!(record.to_json()["stall_signal"], "single_tool_streak=7");
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
                stall_signal: None,
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
                stall_signal: None,
            },
        );
        assert!(record.kind.is_resumable());
        assert!(record.user_message.contains("compacted"));
    }

    #[test]
    fn empty_completion_has_explicit_user_message() {
        let record = InterruptionRecord::new(
            InterruptionKind::EmptyCompletion,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 4,
                turns_completed: 3,
                remaining_turns: 7,
                error_detail: Some("agentic loop completed without final text".to_string()),
                stall_signal: None,
            },
        );
        assert!(record.user_message.contains("without a final answer"));
        assert!(record.user_message.contains("continue"));
    }

    // ── resume guidance tests ──

    #[test]
    fn resume_guidance_budget_exhausted() {
        let irj = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 12,
            "turns_completed": 15,
            "remaining_turns": 0,
            "user_message": "[budget_exhausted] 12 tool call(s) completed. A checkpoint was saved."
        });
        let guidance = build_resume_guidance(&irj).expect("should produce guidance");
        assert!(guidance.contains("[RESUME CONTEXT]"));
        assert!(guidance.contains("budget_exhausted"));
        assert!(guidance.contains("15 turn(s)"));
        assert!(guidance.contains("12 tool call(s)"));
        assert!(guidance.contains("Checkpoint: saved"));
        assert!(guidance.contains("Prioritize"));
        // With no stall_signal, no cause line.
        assert!(
            !guidance.contains("Cause:"),
            "Cause line should be absent when no stall_signal present: {guidance}"
        );
    }

    #[test]
    fn resume_guidance_budget_exhausted_with_stall_signal_surfaces_cause() {
        let irj = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 18,
            "turns_completed": 18,
            "remaining_turns": 131,
            "user_message": "[budget_exhausted] 18 tool call(s) completed.",
            "stall_signal": "single_tool_streak=18"
        });
        let guidance = build_resume_guidance(&irj).expect("should produce guidance");
        assert!(
            guidance.contains("Cause:"),
            "resumed session must see *why* it was cut so the LLM can self-correct: {guidance}"
        );
        assert!(
            guidance.contains("18 consecutive rounds"),
            "streak count must be interpolated into the cause line: {guidance}"
        );
        assert!(
            guidance.contains("batch"),
            "corrective advice must tell the model to batch next: {guidance}"
        );
    }

    #[test]
    fn resume_guidance_budget_exhausted_with_exploration_family_signal_surfaces_cause() {
        let irj = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 22,
            "turns_completed": 14,
            "remaining_turns": 135,
            "user_message": "[budget_exhausted] 22 tool call(s) completed.",
            "stall_signal": "exploration_family=read;streak=5"
        });
        let guidance = build_resume_guidance(&irj).expect("should produce guidance");
        assert!(
            guidance.contains("read exploration family"),
            "guidance should surface the dominant churn family: {guidance}"
        );
        assert!(
            guidance.contains("5 consecutive rounds"),
            "streak should be preserved in resume guidance: {guidance}"
        );
        assert!(
            guidance.contains("reuse the evidence already gathered"),
            "guidance should tell the resumed turn to stop re-reading: {guidance}"
        );
    }

    #[test]
    fn resume_guidance_budget_exhausted_with_redundant_reads_signal_surfaces_cause() {
        let irj = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 20,
            "turns_completed": 14,
            "remaining_turns": 135,
            "user_message": "[budget_exhausted] 20 tool call(s) completed.",
            "stall_signal": "redundant_reads=5"
        });
        let guidance = build_resume_guidance(&irj).expect("should produce guidance");
        assert!(
            guidance.contains("re-read overlapping file ranges 5 time"),
            "guidance should explain the redundant-read cause: {guidance}"
        );
        assert!(
            guidance.contains("already in context"),
            "guidance should steer the resumed turn toward reuse: {guidance}"
        );
    }

    #[test]
    fn resume_guidance_rate_limited() {
        let irj = serde_json::json!({
            "kind": "rate_limited",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 5,
            "turns_completed": 3,
            "remaining_turns": 7,
            "user_message": ""
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("rate_limited"));
        assert!(guidance.contains("batch tool calls"));
    }

    #[test]
    fn resume_guidance_context_overflow() {
        let irj = serde_json::json!({
            "kind": "context_overflow",
            "resumable": true,
            "has_checkpoint": false,
            "tool_calls_completed": 0,
            "turns_completed": 1,
            "remaining_turns": 9,
            "user_message": ""
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("context_overflow"));
        assert!(guidance.contains("compacted"));
    }

    #[test]
    fn resume_guidance_empty_completion() {
        let irj = serde_json::json!({
            "kind": "empty_completion",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 2,
            "turns_completed": 5,
            "remaining_turns": 3,
            "user_message": "[empty_completion] The run ended without a final answer."
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("empty_completion"));
        assert!(guidance.contains("direct final answer"));
    }

    #[test]
    fn resume_guidance_non_resumable_returns_none() {
        let irj = serde_json::json!({
            "kind": "auth_failure",
            "resumable": false,
            "has_checkpoint": false,
            "tool_calls_completed": 0,
            "turns_completed": 0,
            "remaining_turns": 10,
            "user_message": ""
        });
        assert!(build_resume_guidance(&irj).is_none());
    }

    #[test]
    fn resume_guidance_missing_fields_returns_none() {
        let irj = serde_json::json!({});
        assert!(build_resume_guidance(&irj).is_none());
    }

    // ── classify_error tests ──

    #[test]
    fn classify_error_auth_401() {
        let (kind, action) = classify_error("HTTP 401 Unauthorized").unwrap();
        assert_eq!(kind, InterruptionKind::AuthFailure);
        matches!(action, ResumeAction::RequiresIntervention { .. });
    }

    #[test]
    fn classify_error_server_503() {
        let (kind, action) = classify_error("503 Service Unavailable").unwrap();
        assert_eq!(kind, InterruptionKind::ServerOverload);
        matches!(action, ResumeAction::WaitAndRetry { .. });
    }

    #[test]
    fn classify_error_overload_529() {
        let (kind, _) = classify_error("Error: 529 overloaded").unwrap();
        assert_eq!(kind, InterruptionKind::ServerOverload);
    }

    #[test]
    fn classify_error_unknown_returns_none() {
        assert!(classify_error("some random error").is_none());
    }

    /// Session f5d6ef02 regression: Bedrock/AWS auth layers emit
    /// "Could not validate credentials" which the classifier missed
    /// — it contains neither "401", "unauthorized", "authentication",
    /// nor "api key". Five consecutive turn_errors in one session were
    /// labelled `[unknown]` with vacuous guidance, hiding what was
    /// actually a recoverable credential-refresh situation.
    #[test]
    fn classify_error_bedrock_validate_credentials() {
        let (kind, _) = classify_error("Error: Could not validate credentials").unwrap();
        assert_eq!(
            kind,
            InterruptionKind::AuthFailure,
            "Bedrock-style 'Could not validate credentials' must map to AuthFailure \
             so the UI can surface 'please re-authenticate' instead of 'unknown error'"
        );
    }

    #[test]
    fn classify_error_aws_expired_token() {
        // Another common AWS STS shape that ends up in the same
        // code path when session tokens expire mid-run.
        let (kind, _) =
            classify_error("The security token included in the request is expired").unwrap();
        assert_eq!(kind, InterruptionKind::AuthFailure);
    }

    #[test]
    fn classify_error_forbidden_403_maps_to_auth() {
        // 403 is closer to "auth" than to "server error" for our UX —
        // it's a permissions/credentials problem the user can fix,
        // not a transient outage.
        let (kind, _) = classify_error("HTTP 403 Forbidden").unwrap();
        assert_eq!(kind, InterruptionKind::AuthFailure);
    }

    #[test]
    fn classify_error_rate_limit_429() {
        let (kind, action) = classify_error("Error 429: Too Many Requests").unwrap();
        assert_eq!(kind, InterruptionKind::RateLimited);
        assert!(matches!(action, ResumeAction::WaitAndRetry { .. }));
    }

    #[test]
    fn classify_error_context_overflow() {
        let (kind, action) = classify_error("context_length_exceeded: prompt is too long").unwrap();
        assert_eq!(kind, InterruptionKind::ContextOverflow);
        assert!(matches!(action, ResumeAction::CompactAndRetry));
    }

    #[test]
    fn classify_error_maximum_context_length() {
        let (kind, _) = classify_error("maximum context length exceeded").unwrap();
        assert_eq!(kind, InterruptionKind::ContextOverflow);
    }

    // ── new interruption kind tests ──

    #[test]
    fn critical_verdict_is_resumable() {
        assert!(InterruptionKind::CriticalVerdict.is_resumable());
    }

    #[test]
    fn approval_rejected_is_resumable() {
        assert!(InterruptionKind::ApprovalRejected.is_resumable());
    }

    #[test]
    fn cumulative_budget_exceeded_is_resumable() {
        assert!(InterruptionKind::CumulativeBudgetExceeded.is_resumable());
    }

    #[test]
    fn server_overload_is_resumable() {
        assert!(InterruptionKind::ServerOverload.is_resumable());
    }

    #[test]
    fn cooldown_rejected_is_resumable() {
        assert!(InterruptionKind::CooldownRejected.is_resumable());
    }

    #[test]
    fn resume_guidance_critical_verdict() {
        let irj = serde_json::json!({
            "kind": "critical_verdict",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 8,
            "turns_completed": 4,
            "remaining_turns": 0,
            "user_message": ""
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("critical_verdict"));
        assert!(guidance.contains("TurnGuard"));
        assert!(guidance.contains("different approach"));
    }

    #[test]
    fn resume_guidance_approval_rejected() {
        let irj = serde_json::json!({
            "kind": "approval_rejected",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 3,
            "turns_completed": 2,
            "remaining_turns": 8,
            "user_message": ""
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("approval_rejected"));
        assert!(guidance.contains("read-only"));
    }

    #[test]
    fn resume_guidance_cumulative_budget() {
        let irj = serde_json::json!({
            "kind": "cumulative_budget_exceeded",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 20,
            "turns_completed": 10,
            "remaining_turns": 0,
            "user_message": ""
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("cumulative_budget_exceeded"));
        assert!(guidance.contains("Prioritize"));
    }

    #[test]
    fn all_interruption_kinds_have_labels() {
        let kinds = [
            InterruptionKind::BudgetExhausted,
            InterruptionKind::EmptyCompletion,
            InterruptionKind::TokenBudgetExceeded,
            InterruptionKind::CumulativeBudgetExceeded,
            InterruptionKind::RateLimited,
            InterruptionKind::CooldownRejected,
            InterruptionKind::UserCancelled,
            InterruptionKind::ContextOverflow,
            InterruptionKind::AuthFailure,
            InterruptionKind::CriticalVerdict,
            InterruptionKind::ApprovalRejected,
            InterruptionKind::ServerOverload,
        ];
        for kind in kinds {
            let label = kind.label();
            assert!(!label.is_empty(), "{kind:?} should have a label");
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label should be snake_case: {label}"
            );
        }
    }

    #[test]
    fn resume_guidance_with_compaction_context() {
        let irj = serde_json::json!({
            "kind": "context_overflow",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 10,
            "turns_completed": 5,
            "remaining_turns": 5,
            "user_message": ""
        });
        let ctx = CompactionResumeContext {
            compaction_attempts: 3,
            total_tokens_freed: 15000,
            last_was_insufficient: true,
        };
        let guidance = build_resume_guidance_with_context(&irj, Some(&ctx)).unwrap();
        assert!(guidance.contains("3 attempt(s)"));
        assert!(guidance.contains("15000 tokens freed"));
        assert!(guidance.contains("insufficient"));
        assert!(guidance.contains("concise"));
    }

    #[test]
    fn resume_guidance_with_compaction_context_sufficient() {
        let irj = serde_json::json!({
            "kind": "context_overflow",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 5,
            "turns_completed": 3,
            "remaining_turns": 7,
            "user_message": ""
        });
        let ctx = CompactionResumeContext {
            compaction_attempts: 1,
            total_tokens_freed: 8000,
            last_was_insufficient: false,
        };
        let guidance = build_resume_guidance_with_context(&irj, Some(&ctx)).unwrap();
        assert!(guidance.contains("1 attempt(s)"));
        assert!(guidance.contains("8000 tokens freed"));
        assert!(!guidance.contains("insufficient"));
    }

    #[test]
    fn resume_guidance_without_compaction_context_unchanged() {
        let irj = serde_json::json!({
            "kind": "context_overflow",
            "resumable": true,
            "has_checkpoint": false,
            "tool_calls_completed": 0,
            "turns_completed": 1,
            "remaining_turns": 9,
            "user_message": ""
        });
        let with = build_resume_guidance_with_context(&irj, None).unwrap();
        let without = build_resume_guidance(&irj).unwrap();
        assert_eq!(with, without);
    }

    // ── interruption_from_error_kind tests ──

    #[test]
    fn error_kind_auth_maps_to_auth_failure() {
        let (kind, action) = interruption_from_error_kind(astra_core::ErrorKind::Auth).unwrap();
        assert_eq!(kind, InterruptionKind::AuthFailure);
        assert!(matches!(action, ResumeAction::RequiresIntervention { .. }));
    }

    #[test]
    fn error_kind_rate_limit_maps_to_rate_limited() {
        let (kind, action) =
            interruption_from_error_kind(astra_core::ErrorKind::RateLimit).unwrap();
        assert_eq!(kind, InterruptionKind::RateLimited);
        assert!(matches!(action, ResumeAction::WaitAndRetry { .. }));
    }

    #[test]
    fn error_kind_context_window_maps_to_context_overflow() {
        let (kind, action) =
            interruption_from_error_kind(astra_core::ErrorKind::ContextWindow).unwrap();
        assert_eq!(kind, InterruptionKind::ContextOverflow);
        assert!(matches!(action, ResumeAction::CompactAndRetry));
    }

    #[test]
    fn error_kind_server_error_maps_to_server_overload() {
        let (kind, action) =
            interruption_from_error_kind(astra_core::ErrorKind::ServerError).unwrap();
        assert_eq!(kind, InterruptionKind::ServerOverload);
        assert!(matches!(action, ResumeAction::WaitAndRetry { .. }));
    }

    #[test]
    fn error_kind_budget_exhausted_maps_to_budget_exhausted() {
        let (kind, _) =
            interruption_from_error_kind(astra_core::ErrorKind::BudgetExhausted).unwrap();
        assert_eq!(kind, InterruptionKind::BudgetExhausted);
    }

    #[test]
    fn error_kind_cancelled_maps_to_user_cancelled() {
        let (kind, _) = interruption_from_error_kind(astra_core::ErrorKind::Cancelled).unwrap();
        assert_eq!(kind, InterruptionKind::UserCancelled);
    }

    #[test]
    fn error_kind_unknown_returns_none() {
        assert!(interruption_from_error_kind(astra_core::ErrorKind::Unknown).is_none());
    }

    #[test]
    fn error_kind_tool_errors_return_none() {
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ToolNotFound).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ToolTimeout).is_none());
    }

    #[test]
    fn error_kind_stream_errors_return_none() {
        // Stream errors are retryable at the LLM layer, not session interruptions
        assert!(interruption_from_error_kind(astra_core::ErrorKind::StreamIdle).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::StreamTransport).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::Network).is_none());
    }

    #[test]
    fn error_kind_non_retryable_non_interruption() {
        assert!(interruption_from_error_kind(astra_core::ErrorKind::InvalidRequest).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ToolInvalidArgs).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ResourceLimit).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ToolRoundsExhausted).is_none());
    }
}
