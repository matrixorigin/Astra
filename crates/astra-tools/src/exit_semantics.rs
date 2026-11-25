//! Facts about command execution. A shell's aggregate status does not identify
//! which part of a script ran or explain why it returned a non-zero status.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitSemantics {
    Success,
    /// A dedicated tool completed normally and found no matching results.
    EmptyResult,
    /// A dedicated tool completed normally with a negative domain answer.
    DomainNegative,
    /// The process owner observed a timeout.
    TimedOut,
    /// The process owner observed cancellation.
    Cancelled,
    /// The process owner observed termination by a signal.
    Signaled,
    /// A command returned a non-zero status or could not be executed.
    ExecutionError,
}

impl ExitSemantics {
    #[must_use]
    pub fn is_tool_error(self) -> bool {
        matches!(
            self,
            Self::TimedOut | Self::Cancelled | Self::Signaled | Self::ExecutionError
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResultClass {
    Success,
    EmptyResult,
    DomainNegative,
    TestFailure,
    EnvFailure,
    ExecutionError,
    Inconclusive,
}

impl CommandResultClass {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::EmptyResult => "empty_result",
            Self::DomainNegative => "domain_negative",
            Self::TestFailure => "test_failure",
            Self::EnvFailure => "env_failure",
            Self::ExecutionError => "execution_error",
            Self::Inconclusive => "inconclusive",
        }
    }

    #[must_use]
    pub fn is_tool_error(self) -> bool {
        matches!(self, Self::EnvFailure | Self::ExecutionError)
    }
}

/// Source-authored evidence for a command that did not complete successfully.
#[must_use]
pub fn command_failed_evidence(kind: astra_core::ErrorKind) -> astra_core::ToolFailureEvidence {
    astra_core::ToolFailureEvidence::new(
        kind,
        astra_core::ToolFailureCause::CommandFailed,
        false,
        vec![astra_core::ToolRecoveryAction::InspectStructuredFailure],
    )
}

/// Preserve a failed command's status evidence only when the executor has
/// an observed nonzero status. An absent status cannot certify an exit.
pub fn command_exit_failure_evidence(exit_code: Option<i32>) -> astra_core::ToolFailureEvidence {
    let kind = if exit_code.is_some_and(|code| code > 0) {
        astra_core::ErrorKind::Unknown
    } else {
        astra_core::ErrorKind::ToolUnavailable
    };
    command_failed_evidence(kind)
}

/// A shell status alone cannot identify the command or cause behind a failure.
#[must_use]
pub fn classify_exit(exit_code: i32) -> ExitSemantics {
    if exit_code == 0 {
        ExitSemantics::Success
    } else {
        ExitSemantics::ExecutionError
    }
}

#[must_use]
pub fn classify_command_result(exit_code: Option<i32>) -> CommandResultClass {
    match exit_code {
        Some(0) => CommandResultClass::Success,
        Some(_) => CommandResultClass::ExecutionError,
        None => CommandResultClass::Inconclusive,
    }
}

/// A drained pipe can end with a partial credential if its producer was
/// interrupted. Use this conservative check only for display redaction.
pub fn shell_output_tail_may_be_partial(exit_code: i32) -> bool {
    exit_code == 23 || (128..256).contains(&exit_code)
}

/// Context for an observed shell status, without guessing which process failed.
#[must_use]
pub fn shell_exit_hint(exit_code: i32) -> Option<&'static str> {
    (exit_code == 141).then_some(
        "Exit 141 can indicate SIGPIPE when a pipe closes early. Dependent commands may not have run; inspect the output and effects before retrying.",
    )
}

#[cfg(test)]
mod tests {
    use super::{CommandResultClass, ExitSemantics, classify_command_result, classify_exit};

    #[test]
    fn shell_status_does_not_infer_cause_or_success_from_command_text() {
        for code in [1, 23, 124, 126, 127, 128, 141, 143] {
            assert_eq!(classify_exit(code), ExitSemantics::ExecutionError, "{code}");
            assert_eq!(
                classify_command_result(Some(code)),
                CommandResultClass::ExecutionError,
                "{code}"
            );
        }
        assert_eq!(classify_exit(0), ExitSemantics::Success);
        assert_eq!(
            classify_command_result(Some(0)),
            CommandResultClass::Success
        );
        assert_eq!(
            classify_command_result(None),
            CommandResultClass::Inconclusive
        );
    }
}
