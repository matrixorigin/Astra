//! Shared CJK-aware tokenizer for TF-IDF scoring.
//!
//! Used by both `tool_registry::scoring` (tool selection) and `turn::retrieval`
//! (context retrieval). Consolidated here to eliminate duplication.

use std::collections::HashMap;

/// Tokenize text into lowercase terms, handling both CJK and ASCII.
///
/// - **ASCII/Latin**: splits on non-alphanumeric (except `_`), keeps words ≥ 2 chars.
///   Each word is also stemmed; if the stem differs from the original, both forms
///   are emitted so that "preferences" matches both "preferences" and "prefer".
/// - **CJK** (U+4E00–U+9FFF): emits each character as a unigram and consecutive
///   pairs as bigrams for phrase matching (e.g., "记忆" → `["记", "忆", "记忆"]`).
pub fn tokenize(text: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let lower = text.to_lowercase();
    let mut ascii_buf = String::new();
    let mut cjk_chars: Vec<char> = Vec::new();

    for ch in lower.chars() {
        if ('\u{4e00}'..='\u{9fff}').contains(&ch) {
            if !ascii_buf.is_empty() {
                flush_ascii(&ascii_buf, &mut terms);
                ascii_buf.clear();
            }
            terms.push(ch.to_string());
            if let Some(&prev) = cjk_chars.last() {
                terms.push(format!("{}{}", prev, ch));
            }
            cjk_chars.push(ch);
        } else {
            cjk_chars.clear();
            ascii_buf.push(ch);
        }
    }
    if !ascii_buf.is_empty() {
        flush_ascii(&ascii_buf, &mut terms);
    }
    terms
}

/// Build a raw term-frequency map from a token list.
pub fn build_tf(tokens: &[String]) -> HashMap<String, f64> {
    let mut tf: HashMap<String, f64> = HashMap::new();
    for t in tokens {
        *tf.entry(t.clone()).or_insert(0.0) += 1.0;
    }
    tf
}

fn flush_ascii(buf: &str, terms: &mut Vec<String>) {
    for word in buf.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let w = word.trim();
        if w.len() >= 2 {
            terms.push(w.to_string());
            // Emit stemmed form if different (improves "prefer" ↔ "preferences" overlap)
            let stemmed = stem(w);
            if stemmed != w && stemmed.len() >= 2 {
                terms.push(stemmed);
            }
        }
    }
}

/// Lightweight English stemmer. Strips common inflectional suffixes.
///
/// NOT a full Porter/Snowball stemmer — intentionally simple to avoid
/// false positives (e.g., "community" should NOT stem to "commit").
/// Only handles suffixes that are safe for tool-matching vocabulary.
pub fn stem(word: &str) -> String {
    let w = word;
    let len = w.len();

    // Too short to stem
    if len < 4 {
        return w.to_string();
    }

    // Order matters: try longer suffixes first

    // -ation → remove (e.g., "authorization" → "authoriz")
    if len > 6 && w.ends_with("ation") {
        return w[..len - 5].to_string();
    }
    // -tion → remove (e.g., "execution" → "execu")
    if len > 5 && w.ends_with("tion") {
        return w[..len - 4].to_string();
    }
    // -ment → remove (e.g., "deployment" → "deploy")
    if len > 5 && w.ends_with("ment") {
        return w[..len - 4].to_string();
    }
    // -ness → remove (e.g., "freshness" → "fresh")
    if len > 5 && w.ends_with("ness") {
        return w[..len - 4].to_string();
    }
    // -ings → truncate (e.g., "mergings" → "merg")
    if len > 5 && w.ends_with("ings") {
        return w[..len - 4].to_string();
    }
    // -ences/-ances → truncate (e.g., "preferences" → "prefer")
    if len > 6 && (w.ends_with("ences") || w.ends_with("ances")) {
        return w[..len - 5].to_string();
    }
    // -ence/-ance → truncate (e.g., "preference" → "prefer")
    if len > 5 && (w.ends_with("ence") || w.ends_with("ance")) {
        return w[..len - 4].to_string();
    }
    // -ing → handle consonant doubling
    if len > 4 && w.ends_with("ing") {
        let base = &w[..len - 3];
        // Consonant doubling: "committing" → "commit" (tt→t)
        let base_bytes = base.as_bytes();
        if base_bytes.len() >= 2
            && base_bytes[base_bytes.len() - 1] == base_bytes[base_bytes.len() - 2]
            && base_bytes[base_bytes.len() - 1].is_ascii_alphabetic()
        {
            return base[..base.len() - 1].to_string();
        }
        return base.to_string();
    }
    // -ers → truncate (e.g., "workers" → "work")
    if len > 5 && w.ends_with("ers") {
        return w[..len - 3].to_string();
    }
    // -ed → handle consonant doubling
    if len > 4 && w.ends_with("ed") {
        let base = &w[..len - 2];
        // Consonant doubling: "committed" → "commit"
        let base_bytes = base.as_bytes();
        if base_bytes.len() >= 2
            && base_bytes[base_bytes.len() - 1] == base_bytes[base_bytes.len() - 2]
            && base_bytes[base_bytes.len() - 1].is_ascii_alphabetic()
        {
            return base[..base.len() - 1].to_string();
        }
        return base.to_string();
    }
    // -es → truncate (e.g., "issues" → "issu", "branches" → "branch")
    if len > 4 && w.ends_with("es") {
        return w[..len - 2].to_string();
    }
    // -ly → remove (e.g., "recently" → "recent")
    if len > 4 && w.ends_with("ly") {
        return w[..len - 2].to_string();
    }
    // -s (simple plural) — only if word is long enough and not ending in "ss"
    if len > 4 && w.ends_with('s') && !w.ends_with("ss") {
        return w[..len - 1].to_string();
    }

    w.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_tokenization() {
        let t = tokenize("Hello World foo_bar baz");
        assert!(t.contains(&"hello".to_string()));
        assert!(t.contains(&"world".to_string()));
        assert!(t.contains(&"foo_bar".to_string()));
        assert!(t.contains(&"baz".to_string()));
        // Short words filtered
        assert!(tokenize("a b c").is_empty());
        assert!(tokenize("").is_empty());
    }

    #[test]
    fn cjk_tokenization() {
        let t = tokenize("帮我分析");
        for term in ["帮", "我", "分", "析", "帮我", "我分", "分析"] {
            assert!(t.contains(&term.to_string()), "missing CJK term: {term}");
        }
        // Single CJK char
        assert_eq!(tokenize("我"), vec!["我".to_string()]);
        // Mixed CJK + ASCII
        let mixed = tokenize("分析 repository code 仓库");
        assert!(mixed.contains(&"分析".to_string()));
        assert!(mixed.contains(&"repository".to_string()));
        assert!(mixed.contains(&"code".to_string()));
        assert!(mixed.contains(&"仓库".to_string()));
    }

    #[test]
    fn build_tf_counts() {
        let tf = build_tf(&tokenize("hello hello world"));
        assert!(tf.get("hello").unwrap() >= &2.0);
        assert!(tf.get("world").unwrap() >= &1.0);
    }

    // ── Stemming: data-driven test ──

    #[test]
    fn stem_rules() {
        let cases: &[(&str, &str)] = &[
            // Plurals
            ("issues", "issu"),
            ("commits", "commit"),
            ("branches", "branch"),
            ("preferences", "prefer"), // -ences rule
            // Past tense
            ("committed", "commit"),
            ("merged", "merg"),
            ("analyzed", "analyz"),
            // Gerund
            ("committing", "commit"),
            ("merging", "merg"),
            ("debugging", "debug"),
            // Derivational
            ("deployment", "deploy"),
            ("authorization", "authoriz"),
            ("execution", "execu"),
            ("freshness", "fresh"),
            ("recently", "recent"),
            // Short words unchanged
            ("git", "git"),
            ("pr", "pr"),
            ("fix", "fix"),
            // Special: ss-ending words unchanged
            ("class", "class"),
        ];
        for (input, expected) in cases {
            assert_eq!(
                stem(input),
                *expected,
                "stem({input:?}) should be {expected:?}"
            );
        }
    }

    #[test]
    fn tokenize_emits_and_matches_stems() {
        // Emits both original and stemmed forms
        let t = tokenize("preferences");
        assert!(t.contains(&"preferences".to_string()));
        assert!(t.contains(&"prefer".to_string()));

        // Cross-form: prefer ↔ preferences share tokens
        let shared: Vec<_> = tokenize("prefer")
            .iter()
            .filter(|x| tokenize("preferences").contains(x))
            .collect();
        assert!(
            !shared.is_empty(),
            "prefer and preferences should share tokens"
        );

        // commit ↔ committed
        let shared: Vec<_> = tokenize("commit")
            .iter()
            .filter(|x| tokenize("committed").contains(x))
            .collect();
        assert!(
            !shared.is_empty(),
            "commit and committed should share tokens"
        );
    }

    #[test]
    fn stem_no_false_positive_community() {
        // "community" should NOT stem to "commit"
        assert_ne!(
            stem("community"),
            "commit",
            "community should not stem to commit"
        );
    }
}
