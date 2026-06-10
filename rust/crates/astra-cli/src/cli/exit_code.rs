/// Exit codes for CLI commands (for scripting integration).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitCode {
    /// Success (0)
    Success = 0,
    /// Tool execution failure (1) - at least one tool call failed
    ToolFailure = 1,
    /// Force stop (2) - agent was force-stopped due to errors/stalls
    ForceStop = 2,
    /// API/network error (3) - failed to communicate with server
    ApiError = 3,
    /// Local session durability failure after the turn itself succeeded (4)
    PersistenceError = 4,
    /// Turn produced a partial/interrupted result without a harder failure (5)
    Partial = 5,
    /// Job result was requested before the job had finished (6)
    Unfinished = 6,
}
