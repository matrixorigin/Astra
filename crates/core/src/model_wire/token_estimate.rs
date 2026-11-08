//! Token estimates describe provider-visible content, not encrypted storage size.
use serde_json::Value;

/// Unicode-aware token estimation for a string.
///
/// BPE tokenizers (GPT-4, Claude, etc.) encode dense non-ASCII scripts
/// (CJK, kana, Hangul, Cyrillic, Arabic, etc.) at roughly 1.5 tokens per
/// character on average. A single character often splits into 2 BPE tokens,
/// while common bigrams merge back. We use 3/2 integer arithmetic for a
/// conservative estimate without floating point. ASCII text averages ~4 bytes
/// per token.
///
/// JSON content uses ~2 bytes per token due to many single-character tokens
/// (`{`, `}`, `:`, `,`, `"`). Detected by leading `{` or `[`.
///
/// Emoji typically consume 2-3 tokens each, so they use a separate 2.5-token
/// estimate. All non-ASCII text not classified as emoji uses the dense-script
/// estimate; this avoids brittle Unicode block allowlists.
pub fn estimate_str_tokens(s: &str) -> usize {
    let mut dense_unicode_tokens: usize = 0;
    let mut emoji_tokens: usize = 0;
    let mut ascii_bytes: usize = 0;

    for ch in s.chars() {
        if ch.is_ascii() {
            ascii_bytes += ch.len_utf8();
        } else if is_emoji_like(ch) {
            emoji_tokens += 1;
        } else {
            dense_unicode_tokens += 1;
        }
    }

    // Dense Unicode scripts: ~1.5 tokens per char (3*n/2).
    let dense_unicode_total = (dense_unicode_tokens * 3).div_ceil(2);
    // Emoji: ~2.5 tokens each (5*n/2)
    let emoji_total = (emoji_tokens * 5).div_ceil(2);

    // JSON-like content: ~2 bytes/token. Regular text: ~4 bytes/token.
    // Trim leading whitespace before peeking — tool results often have
    // newlines or spaces before the opening brace.
    let first = s
        .as_bytes()
        .iter()
        .find(|b| !b.is_ascii_whitespace())
        .copied()
        .unwrap_or(0);
    let ascii_divisor = if first == b'{' || first == b'[' { 2 } else { 4 };

    dense_unicode_total + emoji_total + ascii_bytes / ascii_divisor
}

fn is_emoji_like(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1F000..=0x1FAFF // pictographs, flags, symbols, supplemental emoji
            | 0x2600..=0x27BF // miscellaneous symbols and dingbats
            | 0xFE00..=0xFE0F // variation selectors used in emoji presentation
    )
}

pub fn estimate_json_tokens(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null | serde_json::Value::Bool(_) => 1,
        serde_json::Value::Number(number) => estimate_str_tokens(&number.to_string()).max(1),
        serde_json::Value::String(text) => estimate_str_tokens(text).saturating_add(1),
        serde_json::Value::Array(items) => items
            .iter()
            .map(estimate_json_tokens)
            .fold(1_usize, usize::saturating_add),
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(key, value)| {
                estimate_str_tokens(key)
                    .saturating_add(1)
                    .saturating_add(estimate_json_tokens(value))
            })
            .fold(1_usize, usize::saturating_add),
    }
}

/// Opaque continuation bytes have no known token conversion. Keep the rough
/// encoded-text estimate visible, but do not use it as a definitive overflow
/// verdict. The provider tokenizer resolves this uncertainty at dispatch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageTokenEstimate {
    pub text_tokens: usize,
    pub opaque_estimated_tokens: usize,
    pub opaque_bytes: usize,
    pub transport_metadata_bytes: usize,
}

pub fn measure_message(message: &Value) -> MessageTokenEstimate {
    let Some(object) = message.as_object() else {
        return MessageTokenEstimate {
            text_tokens: estimate_json_tokens(message),
            ..Default::default()
        };
    };
    let canonical = matches!(
        object.get("role").and_then(Value::as_str),
        Some("system" | "user" | "assistant" | "tool")
    );
    let reasoning = object.get("type").and_then(Value::as_str) == Some("reasoning");
    let mut result = MessageTokenEstimate {
        text_tokens: 1,
        ..Default::default()
    };
    for (key, value) in object {
        if canonical && key == super::responses::CONTINUATION_KEY {
            result.transport_metadata_bytes = json_string_bytes(value);
            if let Some(items) = value.get("items").and_then(Value::as_array) {
                for item in items.iter().filter(|item| item["type"] == "reasoning") {
                    let opaque = item
                        .get(super::responses::SEALED_ITEM)
                        .and_then(|sealed| sealed.get("ciphertext"))
                        .or_else(|| item.get("encrypted_content"))
                        .and_then(Value::as_str);
                    if let Some(opaque) = opaque {
                        result.add_opaque(opaque);
                    }
                }
            }
        } else if reasoning && key == "encrypted_content" && value.is_string() {
            result.add_opaque(value.as_str().expect("string checked"));
        } else {
            result.text_tokens = result
                .text_tokens
                .saturating_add(estimate_str_tokens(key))
                .saturating_add(1)
                .saturating_add(estimate_json_tokens(value));
        }
    }
    result
}

impl MessageTokenEstimate {
    fn add_opaque(&mut self, encoded: &str) {
        self.opaque_bytes = self.opaque_bytes.saturating_add(encoded.len());
        self.opaque_estimated_tokens = self
            .opaque_estimated_tokens
            .saturating_add(estimate_str_tokens(encoded));
    }
}

/// Storage metadata is not provider text. Byte accounting does not walk the
/// ciphertext as Unicode or serialize a second full request.
pub fn continuation_metadata_bytes(message: &Value) -> usize {
    message
        .get(super::responses::CONTINUATION_KEY)
        .map_or(0, json_string_bytes)
}

fn json_string_bytes(value: &Value) -> usize {
    match value {
        Value::String(value) => value.len(),
        Value::Array(values) => values
            .iter()
            .map(json_string_bytes)
            .fold(0, usize::saturating_add),
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len().saturating_add(json_string_bytes(value)))
            .fold(0, usize::saturating_add),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sealed_storage_is_uncertainty_not_plaintext_occupancy() {
        let plain = json!({"role":"assistant","content":"answer"});
        let mut sealed = plain.clone();
        sealed[super::super::responses::CONTINUATION_KEY] = json!({"items":[{"type":"reasoning","_astra_sealed_item":{"ciphertext":"a".repeat(100_000)}}]});
        let measured = measure_message(&sealed);
        assert_eq!(measured.text_tokens, measure_message(&plain).text_tokens);
        assert_eq!(measured.opaque_bytes, 100_000);
        assert!(measured.opaque_estimated_tokens > 0);
        assert!(measured.transport_metadata_bytes > measured.opaque_bytes);
    }

    #[test]
    fn opened_reasoning_keeps_text_and_reports_opaque_separately() {
        let measured = measure_message(
            &json!({"type":"reasoning","content":[{"type":"reasoning_text","text":"reasoning text"}],"encrypted_content":"opaque"}),
        );
        assert!(measured.text_tokens > 0);
        assert_eq!(measured.opaque_bytes, 6);
        let mut schema =
            json!({"type":"object","properties":{"_astra_responses":{"type":"string"}}});
        assert_eq!(
            measure_message(&schema).text_tokens,
            estimate_json_tokens(&schema)
        );
        schema["description"] = json!("more context");
        assert!(measure_message(&schema).text_tokens > 0);
    }
}
