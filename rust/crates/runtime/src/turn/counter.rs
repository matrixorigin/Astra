pub fn count_persisted_turn_events(
    has_user_content: bool,
    tool_results_len: usize,
    tool_calls_len: usize,
    cloud_tool_results_len: usize,
    has_full_text: bool,
) -> usize {
    let mut n_events = 0usize;
    if has_user_content {
        n_events += 1;
    }
    n_events += tool_results_len;
    n_events += tool_calls_len;
    n_events += cloud_tool_results_len;
    if has_full_text || tool_calls_len > 0 {
        n_events += 1;
    }
    n_events.max(1)
}
