//! User-visible strings for failed cloud `/chat` HTTP calls.

/// Default retry count for `POST /chat/turn` when the thin client backs off on HTTP 429.
pub const CHAT_TURN_POST_MAX_RETRIES: u32 = 3;

/// Same shape as the CLI `API Error (status): body` line, with helpful hints.
#[must_use]
pub fn chat_turn_http_error_user_message(status_code: u16, response_body_for_user: &str) -> String {
    let hint = match status_code {
        400 => Some("Check your request parameters"),
        401 => Some("Session expired — try /login"),
        403 => Some("Access denied"),
        408 | 504 => Some("Request timed out — retrying may help"),
        429 => Some("Rate limited — wait and retry"),
        500 => Some("Server error — please report this issue"),
        502 | 503 => Some("Service temporarily unavailable"),
        _ => None,
    };
    match hint {
        Some(h) => format!("API Error ({status_code}): {response_body_for_user}\n  Hint: {h}"),
        None => format!("API Error ({status_code}): {response_body_for_user}"),
    }
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
        assert!(m.contains("API Error (502): bad"));
        assert!(m.contains("Hint:"));
    }

    #[test]
    fn compact_body_pipe() {
        let m = chat_turn_http_error_with_compact_body(400, "  x  ", |s| s.trim().to_string());
        assert!(m.contains("API Error (400): x"));
    }
    
    #[test]
    fn no_hint_for_unknown_status() {
        let m = chat_turn_http_error_user_message(418, "teapot");
        assert_eq!(m, "API Error (418): teapot");
        assert!(!m.contains("Hint:"));
    }
}
