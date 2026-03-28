const STRUCTURAL_MARKERS: &[&str] = &[
    "## Core Rules",
    "## Planning Protocol",
    "## Self-Model",
    "## Conversation History",
    "File editing rules:",
    "Tool selection rules:",
    "Reflection rules:",
    "Introspection rules:",
];

const REPEAT_THRESHOLD: usize = 8;

pub fn is_prompt_leaked(text: &str, fingerprints: &[String]) -> bool {
    if text.is_empty() {
        return false;
    }

    if STRUCTURAL_MARKERS
        .iter()
        .any(|marker| text.contains(marker))
    {
        return true;
    }

    if fingerprints.is_empty() {
        return false;
    }

    let lower = text.to_lowercase();
    fingerprints
        .iter()
        .any(|fingerprint| lower.contains(fingerprint))
}

pub fn is_repetition_loop(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }

    let words = text.split_whitespace().collect::<Vec<_>>();
    if words.len() < REPEAT_THRESHOLD {
        return false;
    }

    let mut count = 1usize;
    for pair in words.windows(2) {
        if pair[0].eq_ignore_ascii_case(pair[1]) {
            count += 1;
            if count >= REPEAT_THRESHOLD {
                return true;
            }
        } else {
            count = 1;
        }
    }
    false
}
