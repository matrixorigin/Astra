//! Versioned tokenizer-independent weights for durable context evidence.
//!
//! These estimates are suitable for relative prompt-delta and cache
//! diagnostics. They never replace provider-reported usage or a concrete
//! provider tokenizer.

/// Stable cache key for the estimator below.
pub const CANONICAL_JSON_TOKENIZER_REVISION: &str = "astra_canonical_json_v1";

/// Conservative tokenizer-independent weight for canonical JSON.
///
/// ASCII JSON is approximately four bytes per token. Dense Unicode content
/// is weighted at 1.5 tokens per character so CJK and similar scripts are not
/// systematically treated as cheap.
#[must_use]
pub fn estimate_canonical_json_tokens(canonical: &str) -> u64 {
    let mut ascii_bytes = 0_u64;
    let mut non_ascii_chars = 0_u64;
    for ch in canonical.chars() {
        if ch.is_ascii() {
            ascii_bytes = ascii_bytes.saturating_add(1);
        } else {
            non_ascii_chars = non_ascii_chars.saturating_add(1);
        }
    }
    ascii_bytes
        .div_ceil(4)
        .saturating_add(non_ascii_chars.saturating_mul(3).div_ceil(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_unicode_is_not_weighted_like_ascii_bytes() {
        assert_eq!(estimate_canonical_json_tokens("abcd"), 1);
        assert_eq!(estimate_canonical_json_tokens("你好世界"), 6);
    }
}
