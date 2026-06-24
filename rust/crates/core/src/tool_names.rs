/// Tool names that have been intentionally retired from every public and
/// runtime capability surface.
pub const RETIRED_TOOL_NAMES: &[&str] = &[
    "prioritize_tool",
    "deprioritize_tool",
    "delegate",
    "task_create",
    "task_update",
    "task_get",
    "task_archive",
    "job",
    "agent_job",
    "mo",
];

pub fn is_retired_tool_name(name: &str) -> bool {
    RETIRED_TOOL_NAMES.contains(&name)
}
