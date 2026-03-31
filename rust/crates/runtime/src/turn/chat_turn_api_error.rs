//! User-visible strings for failed cloud `/chat` HTTP calls.

/// Default retry count for `POST /chat/turn` when the thin client backs off on HTTP 429.
pub const CHAT_TURN_POST_MAX_RETRIES: u32 = 3;

/// Same shape as the CLI `API Error (status): body` line.
#[must_use]
pub fn chat_turn_http_error_user_message(status_code: u16, response_body_for_user: &str) -> String {
    format!("API Error ({status_code}): {response_body_for_user}")
}

/// Build [`chat_turn_http_error_user_message`] after transforming the raw body (e.g. CLI JSON compact).
#[must_use]
pub fn chat_turn_http_error_with_compact_body<F>(
    status_code: u16,
    raw_body: &str,
    compact: F,
) -> String
where
    F: FnOnce(&str) -> String,
{
    let display = compact(raw_body);
    chat_turn_http_error_user_message(status_code, display.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_status_and_body() {
        let m = chat_turn_http_error_user_message(502, "bad");
        assert_eq!(m, "API Error (502): bad");
    }

    #[test]
    fn compact_body_pipe() {
        let m = chat_turn_http_error_with_compact_body(400, "  x  ", |s| s.trim().to_string());
        assert_eq!(m, "API Error (400): x");
    }
}
