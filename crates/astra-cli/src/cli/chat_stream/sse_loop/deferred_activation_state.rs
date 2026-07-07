use crate::edge_tools;

pub(super) fn restore_into_executor(
    slot: &Option<&mut Vec<String>>,
    executor: &edge_tools::ToolExecutor,
) {
    if let Some(names) = slot.as_ref() {
        executor.restore_activated_deferred_tool_names_for_session(names.as_slice());
    }
}

pub(super) fn snapshot_from_executor(
    slot: &mut Option<&mut Vec<String>>,
    executor: &edge_tools::ToolExecutor,
) {
    if let Some(slot) = slot.as_mut() {
        **slot = executor.activated_deferred_tool_names();
    }
}
