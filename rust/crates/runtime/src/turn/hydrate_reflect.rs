//! Replace `reflect` placeholder output with a real session reflect fetch when needed (headless path).

use mo_thin_client::{ThinClient, ThinClientError};
use serde_json::Value;

/// Minimal query-value encoding for reflect URL (matches legacy CLI behavior).
fn reflect_query_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => "%20".to_string(),
            '&' => "%26".to_string(),
            '=' => "%3D".to_string(),
            '#' => "%23".to_string(),
            _ => c.to_string(),
        })
        .collect()
}

/// Relative path + query for `GET /chat/session/{id}/reflect?...` (no leading slash).
#[must_use]
pub fn reflect_hydration_rel_path(session_id: &str, args: &Value) -> String {
    let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
    let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
    let mut qp: Vec<String> = Vec::new();
    if !focus.is_empty() && focus != "auto" {
        qp.push(format!("focus={focus}"));
    }
    if !question.is_empty() {
        qp.push(format!("question={}", reflect_query_encode(question)));
    }
    qp.push(format!("last_n={last_n}"));
    let path = mo_thin_client::paths::chat_session_reflect(session_id);
    let base = path.trim_start_matches('/');
    format!("{base}?{}", qp.join("&"))
}

/// If `reflect` returned a session placeholder, fetch the real reflect body from the API.
pub async fn hydrate_reflect_placeholder_if_needed(
    api: &ThinClient,
    token: &str,
    current_session_id: Option<&String>,
    name: &str,
    args: &Value,
    mut result_str: String,
) -> String {
    if name == "reflect"
        && result_str.contains("reflect_requires_session")
        && let Some(sid) = current_session_id
    {
        let rel = reflect_hydration_rel_path(sid, args);
        match api.get_authed_path_text(token, &rel).await {
            Ok(text) => {
                result_str = text;
            }
            Err(ThinClientError::Api { status, .. }) => {
                result_str = format!("{{\"error\": \"reflect HTTP {status}\"}}");
            }
            Err(e) => {
                result_str = format!("{{\"error\": \"reflect failed: {e}\"}}");
            }
        }
    }
    result_str
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reflect_query_encode_specials() {
        assert_eq!(reflect_query_encode("a b"), "a%20b");
        assert_eq!(reflect_query_encode("a&b=c#d"), "a%26b%3Dc%23d");
    }

    #[test]
    fn reflect_hydration_rel_path_minimal() {
        let p = reflect_hydration_rel_path("sid-1", &json!({}));
        assert_eq!(p, "chat/session/sid-1/reflect?last_n=20");
    }

    #[test]
    fn reflect_hydration_rel_path_with_question() {
        let p = reflect_hydration_rel_path(
            "abc",
            &json!({"question": "q & x", "focus": "auto", "last_n": 3}),
        );
        assert!(p.contains("question=q%20%26%20x"));
        assert!(p.contains("last_n=3"));
        assert!(!p.contains("focus="));
    }

    #[test]
    fn reflect_hydration_rel_path_includes_non_auto_focus() {
        let p = reflect_hydration_rel_path("s", &json!({"focus": "bugs"}));
        assert!(p.starts_with("chat/session/s/reflect?"));
        assert!(p.contains("focus=bugs"));
    }
}
