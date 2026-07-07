#[must_use]
pub fn is_direct_correction_signal(message: &str) -> bool {
    matches!(
        classify_correction_signal(message),
        Some(astra_turn_types::UserCorrectionSignalKind::Correction)
    )
}

#[must_use]
pub fn is_reanchor_signal(message: &str) -> bool {
    classify_correction_signal(message).is_some()
}

#[must_use]
pub fn classify_correction_signal(
    message: &str,
) -> Option<astra_turn_types::UserCorrectionSignalKind> {
    astra_turn_types::classify_user_correction_signal(message)
}

#[cfg(test)]
mod tests {
    use super::{classify_correction_signal, is_direct_correction_signal, is_reanchor_signal};
    use astra_turn_types::UserCorrectionSignalKind;

    #[test]
    fn correction_signal_handles_english_and_chinese_redirects() {
        assert!(is_direct_correction_signal("No, that's wrong."));
        assert!(is_direct_correction_signal("不对，我的意思是改这里"));
        assert!(!is_direct_correction_signal("please continue with the fix"));
        assert!(is_reanchor_signal("No, that's wrong."));
    }

    #[test]
    fn correction_signal_handles_reanchor_nudges_without_chinese_question_false_positive() {
        assert!(!is_direct_correction_signal("不是修修补补，要系统性解决"));
        assert_eq!(
            classify_correction_signal("不是修修补补，要系统性解决"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert!(is_reanchor_signal("不是修修补补，要系统性解决"));
        assert!(!is_direct_correction_signal(
            "我想要的是长久健康运行，不是临时补丁"
        ));
        assert_eq!(
            classify_correction_signal("我想要的是长久健康运行，不是临时补丁"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert!(!is_direct_correction_signal(
            "What I asked for is a durable fix, not a workaround"
        ));
        assert_eq!(
            classify_correction_signal("What I asked for is a durable fix, not a workaround"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert!(!is_direct_correction_signal(
            "You misunderstood the goal; keep the session healthy long-term"
        ));
        assert_eq!(
            classify_correction_signal(
                "You misunderstood the goal; keep the session healthy long-term"
            ),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert_eq!(
            classify_correction_signal("actually, i wanted X"),
            Some(UserCorrectionSignalKind::Reanchor)
        );
        assert!(!is_direct_correction_signal(
            "是不是可以让 web-agent 支持 taskboard?"
        ));
    }
}
