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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_all_false_returns_1() {
        assert_eq!(count_persisted_turn_events(false, 0, 0, 0, false), 1);
    }

    #[test]
    fn count_user_content_only() {
        // user=1, no full_text and no tool_calls → no response event. max(1) → 1
        assert_eq!(count_persisted_turn_events(true, 0, 0, 0, false), 1);
    }

    #[test]
    fn count_with_tool_calls_adds_response() {
        // tool_calls_len > 0 triggers +1 for response
        assert_eq!(count_persisted_turn_events(false, 0, 3, 0, false), 4);
        // 0 + 0 + 3 + 0 + 1(tool_calls>0) = 4
    }

    #[test]
    fn count_with_full_text() {
        assert_eq!(count_persisted_turn_events(false, 0, 0, 0, true), 1);
        // 0 + 0 + 0 + 0 + 1(full_text) = 1
    }

    #[test]
    fn count_all_populated() {
        assert_eq!(count_persisted_turn_events(true, 2, 3, 1, true), 8);
        // 1 + 2 + 3 + 1 + 1(full_text||tool_calls) = 8
    }
}
