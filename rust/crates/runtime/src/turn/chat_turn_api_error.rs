//! User-visible strings for failed cloud `/chat` HTTP calls.

/// Default retry count for `POST /chat/turn` when the thin client backs off on HTTP 429.
pub const CHAT_TURN_POST_MAX_RETRIES: u32 = 3;

/// Same shape as the CLI `API Error (status): body` line.
#[must_use]
pub fn chat_turn_http_error_user_message(status_code: u16, response_body_for_user: &str) -> String {
    format!("API Error ({status_code}): {response_body_for_user}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_status_and_body() {
        let m = chat_turn_http_error_user_message(502, "bad");
        assert_eq!(m, "API Error (502): bad");
    }
}
