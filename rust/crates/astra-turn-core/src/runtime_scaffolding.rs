#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeScaffoldingKind {
    SystemReminderWrapper,
    AttentionManifest,
    WorkingSetManifest,
    SessionAnchor,
    ObsoleteActiveTaskAttachment,
    AlreadyFetchedInventory,
    CrossSessionProjectContext,
    PreviousRoundSummary,
    SequentialToolCallsWarning,
    VerificationRequired,
    ErrorBudgetDirective,
    GenericRuntimeScaffolding,
}

pub const SYSTEM_REMINDER_WRAPPER_PREFIX: &str = "<system-reminder>";
pub const ATTENTION_MANIFEST_PREFIX: &str = "[attention:v1]";
pub const WORKING_SET_MANIFEST_PREFIX: &str = "[working-set:v1]";
pub const SESSION_ANCHOR_PREFIX: &str = "[session-anchor]";
pub const OBSOLETE_ACTIVE_TASK_ATTACHMENT_PREFIX: &str = "[Active task attachment]";
pub const ALREADY_FETCHED_PREFIX: &str = "## Already Fetched";
pub const CROSS_SESSION_PROJECT_CONTEXT_PREFIX: &str = "## Cross-Session Project Context";
pub const PREVIOUS_ROUND_PREFIX: &str = "✓ Previous round:";
pub const SEQUENTIAL_TOOL_CALLS_PREFIX: &str = "## ⚠ Sequential Tool Calls Detected";
pub const VERIFICATION_REQUIRED_PREFIX: &str = "⚠️ VERIFICATION REQUIRED";
pub const ERROR_BUDGET_PREFIX: &str = "🔄 ERROR BUDGET";

pub fn detect_runtime_scaffolding(content: &str) -> Option<RuntimeScaffoldingKind> {
    let trimmed = content.trim_start();
    if trimmed.starts_with(SYSTEM_REMINDER_WRAPPER_PREFIX) {
        Some(RuntimeScaffoldingKind::SystemReminderWrapper)
    } else if trimmed.starts_with(ATTENTION_MANIFEST_PREFIX) {
        Some(RuntimeScaffoldingKind::AttentionManifest)
    } else if trimmed.starts_with(WORKING_SET_MANIFEST_PREFIX) {
        Some(RuntimeScaffoldingKind::WorkingSetManifest)
    } else if trimmed.starts_with(SESSION_ANCHOR_PREFIX) {
        Some(RuntimeScaffoldingKind::SessionAnchor)
    } else if trimmed.starts_with(OBSOLETE_ACTIVE_TASK_ATTACHMENT_PREFIX) {
        Some(RuntimeScaffoldingKind::ObsoleteActiveTaskAttachment)
    } else if trimmed.starts_with(ALREADY_FETCHED_PREFIX) {
        Some(RuntimeScaffoldingKind::AlreadyFetchedInventory)
    } else if trimmed.starts_with(CROSS_SESSION_PROJECT_CONTEXT_PREFIX) {
        Some(RuntimeScaffoldingKind::CrossSessionProjectContext)
    } else if trimmed.starts_with(PREVIOUS_ROUND_PREFIX) {
        Some(RuntimeScaffoldingKind::PreviousRoundSummary)
    } else if trimmed.starts_with(SEQUENTIAL_TOOL_CALLS_PREFIX) {
        Some(RuntimeScaffoldingKind::SequentialToolCallsWarning)
    } else if trimmed.starts_with(VERIFICATION_REQUIRED_PREFIX) {
        Some(RuntimeScaffoldingKind::VerificationRequired)
    } else if trimmed.starts_with(ERROR_BUDGET_PREFIX) {
        Some(RuntimeScaffoldingKind::ErrorBudgetDirective)
    } else if astra_turn_types::scaffolding_body_prefixes_for_filtering()
        .any(|prefix| trimmed.starts_with(prefix))
    {
        Some(RuntimeScaffoldingKind::GenericRuntimeScaffolding)
    } else {
        None
    }
}

pub fn is_continuation_scaffolding_for_role(role: &str, content: &str) -> bool {
    matches!(role, "user" | "assistant" | "system") && detect_runtime_scaffolding(content).is_some()
}

pub fn is_trailing_user_runtime_scaffolding(content: &str) -> bool {
    matches!(
        detect_runtime_scaffolding(content),
        Some(
            RuntimeScaffoldingKind::SystemReminderWrapper
                | RuntimeScaffoldingKind::AttentionManifest
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_known_runtime_scaffolding_markers() {
        assert_eq!(
            detect_runtime_scaffolding("<system-reminder>\nBackground task updates"),
            Some(RuntimeScaffoldingKind::SystemReminderWrapper)
        );
        assert_eq!(
            detect_runtime_scaffolding("[attention:v1]\ngoal: ship auth"),
            Some(RuntimeScaffoldingKind::AttentionManifest)
        );
        assert_eq!(
            detect_runtime_scaffolding("[working-set:v1]\ngoal: ship auth"),
            Some(RuntimeScaffoldingKind::WorkingSetManifest)
        );
        assert_eq!(
            detect_runtime_scaffolding("[Active task attachment]\nResume the active task"),
            Some(RuntimeScaffoldingKind::ObsoleteActiveTaskAttachment)
        );
        assert_eq!(
            detect_runtime_scaffolding("## Already Fetched (do NOT re-read)\nfoo.rs"),
            Some(RuntimeScaffoldingKind::AlreadyFetchedInventory)
        );
        assert_eq!(detect_runtime_scaffolding("plain user message"), None);
    }

    #[test]
    fn detects_stop_hook_and_error_budget_directives() {
        assert_eq!(
            detect_runtime_scaffolding(
                "⚠️ VERIFICATION REQUIRED: Before you finish, run missing checks"
            ),
            Some(RuntimeScaffoldingKind::VerificationRequired)
        );
        assert_eq!(
            detect_runtime_scaffolding("🔄 ERROR BUDGET EXHAUSTED: You've hit Unknown errors"),
            Some(RuntimeScaffoldingKind::ErrorBudgetDirective)
        );
    }

    #[test]
    fn falls_back_to_turn_types_scaffolding_prefixes() {
        assert_eq!(
            detect_runtime_scaffolding("Tools used: bash, grep, read_file"),
            Some(RuntimeScaffoldingKind::GenericRuntimeScaffolding)
        );
        assert_eq!(
            detect_runtime_scaffolding("[compact session=sess-1 turn=4]\nsummary"),
            Some(RuntimeScaffoldingKind::GenericRuntimeScaffolding)
        );
    }

    #[test]
    fn continuation_scaffolding_is_filtered_for_prompt_facing_roles() {
        for role in ["user", "assistant", "system"] {
            assert!(is_continuation_scaffolding_for_role(
                role,
                "⚠️ VERIFICATION REQUIRED: Before you finish"
            ));
            assert!(is_continuation_scaffolding_for_role(
                role,
                "Tools used: bash"
            ));
        }
        assert!(!is_continuation_scaffolding_for_role(
            "tool",
            "Tools used: bash"
        ));
    }

    #[test]
    fn plain_user_message_is_not_scaffolding() {
        assert_eq!(detect_runtime_scaffolding("plain user message"), None);
    }

    #[test]
    fn non_versioned_attention_prefix_is_not_scaffolding() {
        // "[attention:" alone without "v1]" could appear in user content
        // about attention mechanisms — require the version marker.
        assert_eq!(
            detect_runtime_scaffolding("[attention:span] user query about ML"),
            None
        );
        assert_eq!(detect_runtime_scaffolding("[working-set:foo]"), None);
    }

    #[test]
    fn trailing_user_scaffolding_only_matches_runtime_user_wrappers() {
        assert!(is_trailing_user_runtime_scaffolding(
            "<system-reminder>\nBackground task updates"
        ));
        assert!(is_trailing_user_runtime_scaffolding(
            "[attention:v1]\ngoal: ship auth"
        ));
        assert!(!is_trailing_user_runtime_scaffolding(
            "## ⚠ Sequential Tool Calls Detected"
        ));
    }
}
