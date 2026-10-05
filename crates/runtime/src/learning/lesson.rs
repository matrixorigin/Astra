//! Lesson payload and content quality checks for Memoria storage.

/// A single reusable learning extracted from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedLesson {
    /// Memoria category, including "working" for turn-checkpoint lessons.
    pub memory_type: &'static str,
    /// The content to store in Memoria.
    pub content: String,
    /// Memoria trust tier; turn-checkpoint lessons use T4.
    pub trust_tier: &'static str,
}

/// Reject lesson content that is too short, too long, or hedged.
pub fn is_high_quality_lesson(text: &str) -> bool {
    if !(10..=500).contains(&text.len()) {
        return false;
    }
    let lower = text.to_lowercase();
    // Reject hedging — low-confidence observations shouldn't become lessons.
    if lower.contains("maybe")
        || lower.contains("might")
        || lower.contains("not sure")
        || lower.contains("possibly")
        || lower.contains("i think")
    {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_gate_rejects_short_and_hedging() {
        assert!(!is_high_quality_lesson("hi"));
        assert!(!is_high_quality_lesson("x".repeat(501).as_str()));
        assert!(!is_high_quality_lesson("maybe use rg instead"));
        assert!(!is_high_quality_lesson("I think grep is slow"));
        assert!(is_high_quality_lesson(
            "Use rg instead of grep in this repo"
        ));
    }
}
