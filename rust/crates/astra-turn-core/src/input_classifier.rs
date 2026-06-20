#[must_use]
pub fn is_correction_signal(message: &str) -> bool {
    astra_turn_types::is_user_correction_signal(message)
}

#[cfg(test)]
mod tests {
    use super::is_correction_signal;

    #[test]
    fn correction_signal_handles_english_and_chinese_redirects() {
        assert!(is_correction_signal("No, that's wrong."));
        assert!(is_correction_signal("不对，我的意思是改这里"));
        assert!(!is_correction_signal("please continue with the fix"));
    }

    #[test]
    fn correction_signal_handles_reanchor_nudges_without_chinese_question_false_positive() {
        assert!(is_correction_signal("不是修修补补，要系统性解决"));
        assert!(is_correction_signal("我想要的是长久健康运行，不是临时补丁"));
        assert!(is_correction_signal(
            "What I asked for is a durable fix, not a workaround"
        ));
        assert!(is_correction_signal(
            "You misunderstood the goal; keep the session healthy long-term"
        ));
        assert!(!is_correction_signal(
            "是不是可以让 web-agent 支持 taskboard?"
        ));
    }
}
