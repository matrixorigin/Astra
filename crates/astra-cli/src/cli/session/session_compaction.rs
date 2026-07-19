use astra_runtime::prompts;

/// Pull a few Memoria hits after compact so the shortened context keeps
/// session-relevant recall as an anchor after compaction.
const COMPACT_ANCHOR_QUERY_MAX: usize = 220;
const COMPACT_ANCHOR_TOP_K: u32 = 3;
const COMPACT_ANCHOR_MAX_LINES: usize = 3;
const COMPACT_ANCHOR_LINE_MAX: usize = 120;
const COMPACT_ANCHOR_TOTAL_MAX: usize = 400;

pub(crate) async fn fetch_compact_memory_anchor_snippet(
    api: &astra_thin_client::ThinClient,
    token: &str,
    session_id: Option<&str>,
    summary_seed: &str,
) -> Option<String> {
    let seed: String = summary_seed
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(summary_seed)
        .chars()
        .take(COMPACT_ANCHOR_QUERY_MAX)
        .collect();
    let seed = seed.trim();
    if seed.is_empty() {
        return None;
    }

    let mut query = String::new();
    if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) {
        query.push_str(session_id);
        query.push(' ');
    }
    query.push_str(seed);

    let payload = serde_json::json!({
        "query": query,
        "top_k": COMPACT_ANCHOR_TOP_K,
    });
    let response = api.post_memory_search_json(token, &payload).await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let body = response.text().await.ok()?;
    let hits: Vec<serde_json::Value> = serde_json::from_str(&body).ok()?;
    if hits.is_empty() {
        return None;
    }

    let mut lines = Vec::new();
    let mut total = 0usize;
    for hit in hits.iter().take(COMPACT_ANCHOR_MAX_LINES) {
        let content = hit
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or("");
        let line = if let Some(entry) = prompts::memory_proto::MemoryEntry::parse(content) {
            entry.display_line()
        } else {
            let memory_type = hit
                .get("memory_type")
                .and_then(|value| value.as_str())
                .unwrap_or("note");
            let preview: String = content.chars().take(100).collect();
            format!("[{memory_type}] {preview}")
        };
        let line: String = line.chars().take(COMPACT_ANCHOR_LINE_MAX).collect();
        if line.trim().is_empty() {
            continue;
        }
        let next_len = total + line.len() + 1;
        if next_len > COMPACT_ANCHOR_TOTAL_MAX {
            break;
        }
        total = next_len;
        lines.push(format!("- {line}"));
    }

    (!lines.is_empty()).then_some(lines.join("\n"))
}

pub(crate) fn compact_assistant_message(
    trimmed: usize,
    summary: &str,
    anchor: Option<&str>,
) -> String {
    let mut output = String::new();
    if let Some(anchor) = anchor.filter(|anchor| !anchor.trim().is_empty()) {
        output.push_str("[Session memory anchor]\n");
        output.push_str(anchor.trim());
        output.push_str("\n\n");
    }
    output.push_str(&format!(
        "[Prior context — {trimmed} turns compacted]\n\n{summary}"
    ));
    output
}

#[cfg(test)]
mod tests {
    use super::compact_assistant_message;
    use crate::cli::session::session_state::{ContinuationAnchor, SessionState};

    #[test]
    fn continuation_anchor_survives_simulated_compaction() {
        let mut history: Vec<(String, String)> = (1..=10)
            .map(|i| (format!("user msg {i}"), format!("assistant reply {i}")))
            .collect();

        let keep = 3;
        let trimmed = history.len().saturating_sub(keep);
        let summary = "User explored Rust async patterns, asked about pinning, \
                       debugged a lifetime issue, and reviewed tokio spawn.";
        let anchor_text =
            "- [fact] Rust Pin<T> prevents moves\n- [fact] tokio::spawn requires 'static";
        let compact_msg = compact_assistant_message(trimmed, summary, Some(anchor_text));
        let summary_entry = (String::new(), compact_msg);

        let mut new_history = vec![summary_entry];
        new_history.extend_from_slice(&history[trimmed..]);
        history = new_history;

        assert_eq!(history.len(), 4);
        assert!(history[0].0.is_empty(), "compacted entry has empty user");
        assert!(history[0].1.contains("[Prior context — 7 turns compacted]"));
        assert!(history[0].1.contains("[Session memory anchor]"));
        assert!(history[0].1.contains("Rust Pin<T>"));

        let state = SessionState {
            continuation_anchor: Some(ContinuationAnchor::rendered_for_test(
                "Latest user input: debug lifetime in tokio::spawn\n\
                 Latest assistant direction: add 'static bound to closure"
                    .to_string(),
            )),
            history,
            ..SessionState::default()
        };

        let effective = crate::cli::session::session_input::prepare_input(
            "继续",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert_eq!(effective.user_message, "继续");
        assert!(effective.runtime_required_texts.is_empty());

        let normal = crate::cli::session::session_input::prepare_input(
            "explain Pin in detail",
            &state,
            &mut crate::cli::ui_adapter::LineUiAdapter,
        );
        assert!(
            normal.runtime_required_texts.is_empty(),
            "normal prompt must not inject anchor"
        );
    }

    #[test]
    fn history_as_messages_post_compaction_preserves_order() {
        let summary = compact_assistant_message(
            5,
            "User built a REST API with axum, added auth middleware.",
            Some("- [fact] axum uses tower layers"),
        );
        let history: Vec<(String, String)> = vec![
            (String::new(), summary),
            ("add rate limiting".into(), "use tower RateLimit".into()),
            ("show example".into(), "```rust\nuse tower...```".into()),
            ("deploy it".into(), "docker build...".into()),
        ];

        let messages = crate::cli::session::session_projection::history_as_messages(&history);

        assert_eq!(messages[0]["role"], "assistant");
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("5 turns compacted")
        );
        assert!(
            messages[0]["content"]
                .as_str()
                .unwrap()
                .contains("[Session memory anchor]")
        );

        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "add rate limiting");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"], "use tower RateLimit");
        assert_eq!(messages.len(), 7);
    }
}
