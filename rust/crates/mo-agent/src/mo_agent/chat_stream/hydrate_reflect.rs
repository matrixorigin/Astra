use crate::cli_utils::urlencoding;

pub(super) async fn hydrate_reflect_placeholder_if_needed(
    api: &mo_thin_client::ThinClient,
    token: &str,
    current_session_id: Option<&String>,
    name: &str,
    args: &serde_json::Value,
    mut result_str: String,
) -> String {
    if name == "reflect"
        && result_str.contains("reflect_requires_session")
        && let Some(sid) = current_session_id
    {
        let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
        let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
        let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
        let mut qp: Vec<String> = Vec::new();
        if !focus.is_empty() && focus != "auto" {
            qp.push(format!("focus={focus}"));
        }
        if !question.is_empty() {
            qp.push(format!("question={}", urlencoding(question)));
        }
        qp.push(format!("last_n={last_n}"));
        let rel = format!(
            "{}?{}",
            mo_thin_client::paths::chat_session_reflect(sid).trim_start_matches('/'),
            qp.join("&")
        );
        match api.get_authed_path_text(token, &rel).await {
            Ok(text) => {
                result_str = text;
            }
            Err(mo_thin_client::ThinClientError::Api { status, .. }) => {
                result_str = format!("{{\"error\": \"reflect HTTP {status}\"}}");
            }
            Err(e) => {
                result_str = format!("{{\"error\": \"reflect failed: {e}\"}}");
            }
        }
    }
    result_str
}
