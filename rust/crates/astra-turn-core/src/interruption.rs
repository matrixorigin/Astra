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
    /// Streaming transport failed after retry/recovery attempts.
    StreamTransport,
    /// Streaming response went idle and retry/recovery did not complete.
    StreamIdle,
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
            Self::StreamTransport => "stream_transport",
            Self::StreamIdle => "stream_idle",
            Self::HarnessBlocked => "harness_blocked",
            Self::HarnessPaused => "harness_paused",
        }
    }

    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "budget_exhausted" => Some(Self::BudgetExhausted),
            "empty_completion" => Some(Self::EmptyCompletion),
            "token_budget_exceeded" => Some(Self::TokenBudgetExceeded),
            "cumulative_budget_exceeded" => Some(Self::CumulativeBudgetExceeded),
            "rate_limited" => Some(Self::RateLimited),
            "cooldown_rejected" => Some(Self::CooldownRejected),
            "user_cancelled" => Some(Self::UserCancelled),
            "context_overflow" => Some(Self::ContextOverflow),
            "auth_failure" => Some(Self::AuthFailure),
            "critical_verdict" => Some(Self::CriticalVerdict),
            "approval_rejected" => Some(Self::ApprovalRejected),
            "server_overload" => Some(Self::ServerOverload),
            "stream_transport" => Some(Self::StreamTransport),
            "stream_idle" => Some(Self::StreamIdle),
            "harness_blocked" => Some(Self::HarnessBlocked),
            "harness_paused" => Some(Self::HarnessPaused),
            _ => None,
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
            | Self::ServerOverload
            | Self::StreamTransport
            | Self::StreamIdle => true,
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

/// Runtime mode the next turn should use when resuming this interruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResumeMode {
    /// Resume normal execution under the user's next instruction.
    #[default]
    Continue,
    /// Do not broaden execution; synthesize preserved state into user-visible
    /// output. Tool availability is enforced separately via
    /// `resume_restricted_tools`.
    Settle,
}

impl ResumeMode {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Settle => "settle",
        }
    }

    #[must_use]
    pub fn default_for_interruption(kind: InterruptionKind) -> Self {
        match kind {
            InterruptionKind::EmptyCompletion => Self::Settle,
            _ => Self::Continue,
        }
    }

    fn from_json_value(value: Option<&serde_json::Value>, kind: &str) -> Self {
        value
            .and_then(|value| value.as_str())
            .and_then(|raw| match raw {
                "continue" => Some(Self::Continue),
                "settle" => Some(Self::Settle),
                _ => None,
            })
            .unwrap_or_else(|| {
                InterruptionKind::from_label(kind)
                    .map(Self::default_for_interruption)
                    .unwrap_or_default()
            })
    }
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
    /// Runtime execution mode for the next resumed turn.
    #[serde(default)]
    pub resume_mode: ResumeMode,
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
    /// Tool names that should stay hidden on the resumed turn so the
    /// model cannot immediately re-enter the exploratory lane that
    /// already exhausted the budget.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resume_restricted_tools: Vec<String>,
}

impl InterruptionRecord {
    /// Create a new interruption record.
    pub fn new(
        kind: InterruptionKind,
        resume_action: ResumeAction,
        state_summary: InterruptionStateSummary,
    ) -> Self {
        let user_message = Self::format_user_message(kind, &resume_action, &state_summary);
        let resume_mode = ResumeMode::default_for_interruption(kind);
        Self {
            kind,
            resume_action,
            resume_mode,
            has_checkpoint: state_summary.has_checkpoint,
            tool_calls_completed: state_summary.tool_calls_completed,
            turns_completed: state_summary.turns_completed,
            remaining_turns: state_summary.remaining_turns,
            error_detail: state_summary.error_detail,
            user_message,
            stall_signal: state_summary.stall_signal,
            resume_restricted_tools: dedup_sorted_tool_names(
                &state_summary.resume_restricted_tools,
            ),
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
            "resume_mode": self.resume_mode,
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
        if !self.resume_restricted_tools.is_empty()
            && let Some(obj) = v.as_object_mut()
        {
            obj.insert(
                "resume_restricted_tools".to_string(),
                serde_json::Value::Array(
                    self.resume_restricted_tools
                        .iter()
                        .cloned()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
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
        let cause_note = match kind {
            InterruptionKind::BudgetExhausted
            | InterruptionKind::TokenBudgetExceeded
            | InterruptionKind::CumulativeBudgetExceeded => summary
                .stall_signal
                .as_deref()
                .and_then(summarize_stall_signal_for_user)
                .map(|s| format!(" Cause: {}.", s.cause)),
            _ => None,
        }
        .unwrap_or_default();
        match kind {
            InterruptionKind::EmptyCompletion => format!(
                "[{}] The run ended without a final answer.{tool_note}{checkpoint_note}{action_note}",
                kind.label()
            ),
            _ => format!(
                "[{kind}]{tool_note}{checkpoint_note}{cause_note}{action_note}",
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
    /// Tool names that should stay restricted when the session resumes.
    pub resume_restricted_tools: Vec<String>,
}

/// Dedup, trim, lowercase, and sort tool names for resume restriction lists.
///
/// Distinct from [`crate::tool_allowlist::normalize_tool_names`] which returns
/// a [`HashSet`] and delegates to ASCII sanitization; this version preserves
/// order determinism via sort and is tailored for serialization into
/// interruption records.
fn dedup_sorted_tool_names(tools: &[String]) -> Vec<String> {
    let mut vec: Vec<String> = tools
        .iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    vec.sort();
    vec.dedup();
    vec
}

#[must_use]
pub fn resume_tool_family_for_exploration(family: &str) -> &'static [&'static str] {
    match family {
        "read" => &["read_file", "view"],
        "search" => &["glob", "grep", "rg"],
        "diff" => &["git_diff", "git_log"],
        _ => &[],
    }
}

#[must_use]
pub fn resume_tool_family_for_tool(tool_name: &str) -> &'static [&'static str] {
    match tool_name {
        // Read family
        "read_file" | "view" => &["read_file", "view"],
        // Search family — file search and web search
        "glob" | "grep" | "rg" | "web_search" => &["glob", "grep", "rg", "web_search"],
        // Diff family
        "git_diff" | "git_log" => &["git_diff", "git_log"],
        // Bash
        "bash" => &["bash"],
        // Memory family
        "memory" | "memory_search" | "memory_retrieve" | "memory_profile" => &[
            "memory",
            "memory_search",
            "memory_retrieve",
            "memory_profile",
        ],
        // Skill / consultative family
        "skill" | "discover_skills" => &["skill", "discover_skills"],
        _ => &[],
    }
}

/// Extract resume restricted tools from an interruption JSON blob.
///
/// **Prefer the typed [`InterruptionRecord::resume_restricted_tools`] field**
/// when it is available.  This function exists for backward compatibility
/// with older checkpoints that only carry a `stall_signal` string.
///
/// The primary source is the `resume_restricted_tools` array, computed by the
/// server at interruption time.  Older interruption records may only carry a
/// `stall_signal` string; those are accepted as-is and produce an empty list
/// (the server now always embeds the resolved tool list).
#[must_use]
pub fn resume_restricted_tools_from_interruption_json(
    interruption_json: &serde_json::Value,
) -> Vec<String> {
    interruption_json
        .get("resume_restricted_tools")
        .and_then(|v| v.as_array())
        .map(|arr| {
            let names: Vec<String> = arr
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect();
            dedup_sorted_tool_names(&names)
        })
        .unwrap_or_default()
}

fn parse_kv_stall_signal(signal: &str) -> std::collections::BTreeMap<&str, &str> {
    signal
        .split(';')
        .filter_map(|segment| segment.split_once('='))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect()
}

struct StallSummary {
    cause: String,
    correction: String,
}

fn summarize_stall_signal_for_user(signal: &str) -> Option<StallSummary> {
    if signal.starts_with("single_tool_streak=") {
        let streak = signal.trim_start_matches("single_tool_streak=");
        return Some(StallSummary {
            cause: format!(
                "the run stayed in one-tool-per-round mode for {streak} consecutive rounds"
            ),
            correction: format!(
                "batch independent calls (different files / greps / reads) \
                 into a single parallel round instead of another {streak}-round \
                 single-tool streak"
            ),
        });
    }
    if signal.starts_with("exploration_family=") {
        let kv = parse_kv_stall_signal(signal);
        if let (Some(family), Some(streak)) = (kv.get("exploration_family"), kv.get("streak")) {
            let (family_hint_cause, family_hint_correction) = match *family {
                "read" => (
                    "kept reopening files already in context",
                    "kept reopening files that were already in context",
                ),
                "search" => (
                    "kept expanding search instead of converging",
                    "kept expanding search instead of converging on a target",
                ),
                "diff" => (
                    "kept diff-scanning without switching to action",
                    "kept diff-scanning without switching to synthesis or action",
                ),
                _ => ("stayed inside one exploratory lane", ""),
            };
            let cause = format!(
                "the run stayed in the {family} exploration family for {streak} consecutive rounds and {family_hint_cause}"
            );
            let correction = if family_hint_correction.is_empty() {
                format!(
                    "synthesize what is already known after {streak} consecutive {family} exploration rounds, \
                     and switch tool families only if one specific fact is still missing"
                )
            } else {
                format!(
                    "reuse the evidence already gathered after {streak} consecutive {family} exploration rounds where the run {family_hint_correction}, \
                     and only fetch one specific missing fact if you still cannot finish"
                )
            };
            return Some(StallSummary { cause, correction });
        }
    }
    if signal.starts_with("redundant_reads=") {
        let count = signal.trim_start_matches("redundant_reads=");
        return Some(StallSummary {
            cause: format!(
                "the run re-read overlapping file ranges {count} time(s) without any intervening edit"
            ),
            correction:
                "reuse the file content already in context instead of reopening overlapping ranges"
                    .to_string(),
        });
    }
    None
}

/// Parsed fields from an interruption record, used to build resume guidance.
#[derive(Debug)]
struct ResumeInput<'j> {
    kind: &'j str,
    resume_mode: ResumeMode,
    turns: u64,
    tool_calls: u64,
    has_checkpoint: bool,
    user_msg: &'j str,
    stall_signal: Option<&'j str>,
    error_detail: Option<&'j str>,
    resume_restricted_tools: Vec<String>,
}

impl<'j> ResumeInput<'j> {
    fn from_json(v: &'j serde_json::Value) -> Option<Self> {
        let kind = v.get("kind")?.as_str()?;
        let resume_mode = ResumeMode::from_json_value(v.get("resume_mode"), kind);
        let turns = v
            .get("turns_completed")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let tool_calls = v
            .get("tool_calls_completed")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let has_checkpoint = v
            .get("has_checkpoint")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let user_msg = v.get("user_message").and_then(|x| x.as_str()).unwrap_or("");
        let stall_signal = v.get("stall_signal").and_then(|x| x.as_str());
        let error_detail = v.get("error_detail").and_then(|x| x.as_str());
        let resume_restricted_tools = resume_restricted_tools_from_interruption_json(v);
        Some(Self {
            kind,
            resume_mode,
            turns,
            tool_calls,
            has_checkpoint,
            user_msg,
            stall_signal,
            error_detail,
            resume_restricted_tools,
        })
    }
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
    let inp = ResumeInput::from_json(interruption_json)?;
    let resumable = interruption_json
        .get("resumable")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !resumable {
        return None;
    }

    let mut g = String::new();
    g.push_str("[RESUME CONTEXT] This session was previously interrupted.\n");
    use std::fmt::Write;
    writeln!(g, "  Reason: {}", inp.kind).ok();
    writeln!(
        g,
        "  Progress: {} turn(s), {} tool call(s) completed",
        inp.turns, inp.tool_calls
    )
    .ok();
    if inp.has_checkpoint {
        g.push_str("  Checkpoint: saved — prior tool results are preserved in context\n");
    }

    if inp.resume_mode == ResumeMode::Settle {
        g.push_str(
            "  Action: Enter settlement: synthesize the preserved evidence into a direct \
             user-visible answer. Do not create new tasks, spawn agents, or use tools.\n",
        );
    } else {
        // Kind-specific advice
        match inp.kind {
            "budget_exhausted" | "token_budget_exceeded" | "cumulative_budget_exceeded" => {
                g.push_str(
                    "  Action: Prioritize completing the most important remaining work first. \
                  Avoid exploratory tool calls — focus on delivering a result.\n",
                );
                if let Some(sig) = inp.stall_signal.and_then(summarize_stall_signal_for_user) {
                    writeln!(g, "  Cause: {}.", sig.cause).ok();
                    writeln!(g, "  Correction: {}.", sig.correction).ok();
                }
                if let Some(detail) = inp.error_detail.filter(|d| d.contains("Likely cause:")) {
                    writeln!(g, "  Runtime detail: {detail}").ok();
                }
            }
            "rate_limited" | "cooldown_rejected" | "server_overload" => g.push_str(
                "  Action: The rate limit has likely expired. Resume normally, \
             but batch tool calls to minimize API round-trips.\n",
            ),
            "context_overflow" => {
                g.push_str(
                    "  Action: Context was compacted. Some older tool results may be \
                 summarized. Re-read any files you need before making edits.\n",
                );
                if let Some(ctx) = compaction_context.filter(|c| c.compaction_attempts > 0) {
                    write!(
                        g,
                        "  Compaction: {} attempt(s), ~{} tokens freed total",
                        ctx.compaction_attempts, ctx.total_tokens_freed
                    )
                    .ok();
                    if ctx.last_was_insufficient {
                        g.push_str(
                            " (last compaction was insufficient — context may still be tight)",
                        );
                    }
                    g.push('\n');
                    g.push_str(
                        "  Tip: Keep responses concise and avoid requesting large file dumps.\n",
                    );
                }
            }
            "user_cancelled" => g.push_str(
                "  Action: The user cancelled the previous run. Wait for their \
             instructions before proceeding.\n",
            ),
            "critical_verdict" => g.push_str(
                "  Action: The previous run was stopped by TurnGuard due to repeated \
             errors and stalls. Review what went wrong, try a different approach, \
             and avoid the tool patterns that caused failures.\n",
            ),
            "harness_paused" => g.push_str(
                "  Action: The previous run was paused by the harness due to a \
             read-heavy stall without any mutation. Reuse the evidence already \
             gathered and take one concrete next action: edit the relevant file, \
             run targeted verification, or explicitly report why the task cannot \
             be completed. If one specific fact is still missing, fetch only that \
             fact instead of reopening broad or overlapping reads.\n",
            ),
            "approval_rejected" => g.push_str(
                "  Action: Tool approvals were repeatedly denied. Use only read-only \
             tools or ask the user for explicit permission before attempting \
             write operations.\n",
            ),
            _ => {
                if !inp.user_msg.is_empty() {
                    writeln!(g, "  Detail: {}", inp.user_msg).ok();
                }
            }
        }
    }

    if !inp.resume_restricted_tools.is_empty() {
        writeln!(
            g,
            "  Resume guard: these tools are blocked on resume to avoid repeating the failed path: {}",
            inp.resume_restricted_tools.join(", ")
        )
        .ok();
    }

    Some(g)
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
        ErrorKind::StreamTransport | ErrorKind::Network => Some((
            InterruptionKind::StreamTransport,
            ResumeAction::ContinueImmediately,
        )),
        ErrorKind::StreamIdle => Some((
            InterruptionKind::StreamIdle,
            ResumeAction::ContinueImmediately,
        )),
        _ => None,
    }
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
            InterruptionKind::StreamTransport,
            InterruptionKind::StreamIdle,
        ];
        for kind in kinds {
            let label = kind.label();
            assert!(!label.is_empty());
            assert!(
                label.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "label should be snake_case: {label}"
            );
            assert_eq!(InterruptionKind::from_label(label), Some(kind));
        }
    }

    #[test]
    fn budget_exhausted_is_resumable() {
        assert!(InterruptionKind::BudgetExhausted.is_resumable());
        assert!(InterruptionKind::EmptyCompletion.is_resumable());
        assert!(InterruptionKind::RateLimited.is_resumable());
        assert!(InterruptionKind::UserCancelled.is_resumable());
        assert!(InterruptionKind::StreamTransport.is_resumable());
        assert!(InterruptionKind::StreamIdle.is_resumable());
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
                resume_restricted_tools: Vec::new(),
            },
        );
        let json = record.to_json();
        assert_eq!(json["kind"], "budget_exhausted");
        assert_eq!(record.resume_mode, ResumeMode::Continue);
        assert_eq!(json["resume_mode"], "continue");
        assert_eq!(json["resumable"], true);
        assert_eq!(json["has_checkpoint"], true);
        assert_eq!(json["tool_calls_completed"], 5);
        assert!(
            json.get("stall_signal").is_none(),
            "stall_signal omitted from JSON when None for backwards compat: {json:?}"
        );
    }

    #[test]
    fn empty_completion_records_settlement_resume_mode() {
        let record = InterruptionRecord::new(
            InterruptionKind::EmptyCompletion,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 5,
                turns_completed: 3,
                remaining_turns: 4,
                error_detail: None,
                stall_signal: None,
                resume_restricted_tools: vec!["agent".to_string(), "bash".to_string()],
            },
        );

        let json = record.to_json();
        assert_eq!(record.resume_mode, ResumeMode::Settle);
        assert_eq!(json["resume_mode"], "settle");
        assert_eq!(
            json["resume_restricted_tools"],
            serde_json::json!(["agent", "bash"])
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
                resume_restricted_tools: Vec::new(),
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
                resume_restricted_tools: Vec::new(),
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
                resume_restricted_tools: Vec::new(),
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
                resume_restricted_tools: Vec::new(),
            },
        );
        assert!(record.user_message.contains("without a final answer"));
        assert!(record.user_message.contains("continue"));
    }

    #[test]
    fn budget_exhausted_user_message_surfaces_stall_cause() {
        let record = InterruptionRecord::new(
            InterruptionKind::BudgetExhausted,
            ResumeAction::ContinueImmediately,
            InterruptionStateSummary {
                has_checkpoint: true,
                tool_calls_completed: 20,
                turns_completed: 14,
                remaining_turns: 0,
                error_detail: None,
                stall_signal: Some("redundant_reads=5".to_string()),
                resume_restricted_tools: Vec::new(),
            },
        );
        assert!(record.user_message.contains("Cause:"));
        assert!(
            record
                .user_message
                .contains("re-read overlapping file ranges 5 time(s)")
        );
        assert!(record.user_message.contains("You can continue"));
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
    fn resume_guidance_surfaces_resume_guard_tools() {
        let irj = serde_json::json!({
            "kind": "budget_exhausted",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 20,
            "turns_completed": 14,
            "remaining_turns": 135,
            "user_message": "[budget_exhausted] 20 tool call(s) completed.",
            "resume_restricted_tools": ["bash", "read_file"]
        });
        let guidance = build_resume_guidance(&irj).expect("should produce guidance");
        assert!(guidance.contains("Resume guard:"));
        assert!(guidance.contains("bash, read_file"));
    }

    #[test]
    fn resume_guidance_harness_paused_allows_single_missing_fact() {
        let irj = serde_json::json!({
            "kind": "harness_paused",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 28,
            "turns_completed": 16,
            "remaining_turns": 134,
            "user_message": "[harness_paused] 28 tool call(s) completed. A checkpoint was saved."
        });
        let guidance = build_resume_guidance(&irj).expect("should produce guidance");
        assert!(guidance.contains("read-heavy stall"), "{guidance}");
        assert!(guidance.contains("one concrete next action"), "{guidance}");
        assert!(
            guidance.contains("one specific fact is still missing"),
            "{guidance}"
        );
        assert!(
            !guidance.contains("Do NOT continue broad or duplicate reading"),
            "{guidance}"
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
        assert!(guidance.contains("Enter settlement"));
        assert!(guidance.contains("synthesize the preserved evidence"));
        assert!(guidance.contains("Do not create new tasks"));
        assert!(guidance.contains("or use tools"));
    }

    #[test]
    fn resume_guidance_settlement_mode_overrides_kind_specific_advice() {
        let irj = serde_json::json!({
            "kind": "budget_exhausted",
            "resume_mode": "settle",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 20,
            "turns_completed": 10,
            "remaining_turns": 0,
            "user_message": ""
        });
        let guidance = build_resume_guidance(&irj).unwrap();
        assert!(guidance.contains("Enter settlement"), "{guidance}");
        assert!(
            !guidance.contains("Prioritize completing"),
            "settlement mode should not fall back to execute-mode budget advice: {guidance}"
        );
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
    fn error_kind_stream_errors_are_resumable_interruptions() {
        let (idle, _) = interruption_from_error_kind(astra_core::ErrorKind::StreamIdle).unwrap();
        assert_eq!(idle, InterruptionKind::StreamIdle);
        let (transport, _) =
            interruption_from_error_kind(astra_core::ErrorKind::StreamTransport).unwrap();
        assert_eq!(transport, InterruptionKind::StreamTransport);
        let (network, _) = interruption_from_error_kind(astra_core::ErrorKind::Network).unwrap();
        assert_eq!(network, InterruptionKind::StreamTransport);
    }

    #[test]
    fn error_kind_non_retryable_non_interruption() {
        assert!(interruption_from_error_kind(astra_core::ErrorKind::InvalidRequest).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ToolInvalidArgs).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ResourceLimit).is_none());
        assert!(interruption_from_error_kind(astra_core::ErrorKind::ToolRoundsExhausted).is_none());
    }
}
