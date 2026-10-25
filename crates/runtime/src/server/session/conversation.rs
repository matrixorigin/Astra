//! A bounded display read over the two existing durable facts: admitted input
//! and committed transcript. This module owns no persistence or Run lifecycle.
use super::session_handlers::{
    TranscriptItemResponse, decode_transcript_item, hydrate_transcript_artifacts,
};
use crate::server::run::lifecycle::server_input_source_event_id;
use astra_core::SharedPool;
use astra_services::runs::decode_loaded_run_opening;
use serde::Serialize;
use sqlx::Row;

const HISTORY_LIMIT: usize = 200;
const HISTORICAL_INPUT_BYTES: usize = 256 * 1024;

#[derive(Serialize)]
pub(crate) struct ConversationResponse {
    pub kind: &'static str,
    pub session: astra_server_types::SessionResponse,
    pub selected_root: Option<ConversationRoot>,
    pub items: Vec<ConversationItem>,
    pub raw_page: RawPage,
    pub truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct ConversationRoot {
    pub run_id: String,
    pub status: String,
    pub waiting_for: Option<String>,
    pub created_at: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ConversationItem {
    Raw {
        #[serde(flatten)]
        item: Box<TranscriptItemResponse>,
    },
    Opening {
        id: String,
        run_id: String,
        source_event_id: String,
        created_at: String,
        content: String,
    },
}

impl ConversationItem {
    fn run_id(&self) -> Option<&str> {
        match self {
            Self::Raw { item } => item.run_id.as_deref(),
            Self::Opening { run_id, .. } => Some(run_id),
        }
    }
    fn source(&self) -> Option<&str> {
        match self {
            Self::Raw { item } => item.source_event_id.as_deref(),
            Self::Opening {
                source_event_id, ..
            } => Some(source_event_id),
        }
    }
    fn created_at(&self) -> &str {
        match self {
            Self::Raw { item } => &item.created_at,
            Self::Opening { created_at, .. } => created_at,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct RawPage {
    pub next_before_seq: Option<i64>,
    pub has_more: bool,
}

// Root identity/status and raw history share ONE statement snapshot, including
// a terminal commit that can change both. Metadata is read by SessionService;
// artifact previews retain the raw endpoint's subsequent enrichment boundary.
// Window limits bound returned candidates, not database rows examined.
const CONVERSATION_SQL: &str = r#"
WITH roots AS (
    SELECT user_id, run_id, session_id, status, waiting_for, created_at,
           ROW_NUMBER() OVER (PARTITION BY user_id, session_id ORDER BY created_at DESC, run_id DESC) AS history_rank,
           ROW_NUMBER() OVER (PARTITION BY user_id, session_id ORDER BY
               CASE WHEN status = 'waiting' OR (status = 'paused' AND waiting_for = 'user_input') THEN 4
                    WHEN status = 'running' THEN 2 WHEN status = 'paused' THEN 1 ELSE 0 END DESC,
               created_at DESC, run_id DESC) AS selected_rank
    FROM agent_runs WHERE user_id = ? AND session_id = ? AND parent_run_id IS NULL AND depth = 0
), raw_items AS (
    SELECT t.user_id, t.session_id, t.item_seq
    FROM session_transcript_items t
    LEFT JOIN agent_runs r ON r.user_id = ? AND r.session_id = ? AND r.run_id = t.run_id
    WHERE t.user_id = ? AND t.session_id = ?
      AND (t.run_id IS NULL OR (r.run_id IS NOT NULL AND r.parent_run_id IS NULL AND r.depth = 0))
    ORDER BY t.item_seq DESC LIMIT ?
)
SELECT 'root' AS record_kind, r.session_id, r.run_id, r.status, r.waiting_for,
       DATE_FORMAT(r.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
       history_rank, selected_rank,
       opening_hash, context_admission_hash,
       CASE WHEN history_rank <= 200 OR selected_rank = 1 THEN opening_events_json ELSE CAST(NULL AS CHAR) END AS opening_events_json,
       CASE WHEN history_rank <= 200 OR selected_rank = 1 THEN opening_context_json ELSE CAST(NULL AS CHAR) END AS opening_context_json,
       CAST(1 AS SIGNED) AS opening_body_loaded,
       CAST(NULL AS SIGNED) AS item_seq, CAST(NULL AS CHAR) AS role,
       CAST(NULL AS CHAR) AS content, CAST(NULL AS CHAR) AS payload_json, CAST(NULL AS CHAR) AS source_event_id
FROM roots r JOIN agent_runs b ON b.user_id = r.user_id AND b.run_id = r.run_id
WHERE b.user_id = ? AND b.session_id = ? AND (history_rank <= 201 OR selected_rank = 1)
UNION ALL
SELECT 'raw', t.session_id, t.run_id, CAST(NULL AS CHAR), CAST(NULL AS CHAR),
       DATE_FORMAT(t.created_at, '%Y-%m-%dT%H:%i:%s.%fZ'),
       CAST(0 AS SIGNED), CAST(0 AS SIGNED),
       CAST(NULL AS CHAR), CAST(NULL AS CHAR), CAST(NULL AS CHAR), CAST(NULL AS CHAR), CAST(0 AS SIGNED),
       t.item_seq, t.role, t.content, t.payload_json, t.source_event_id
FROM raw_items p JOIN session_transcript_items t
 ON t.user_id = p.user_id AND t.session_id = p.session_id AND t.item_seq = p.item_seq
WHERE t.user_id = ? AND t.session_id = ?
"#;

pub(crate) async fn read_conversation(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    limit: usize,
) -> Result<
    (
        Option<ConversationRoot>,
        Vec<ConversationItem>,
        RawPage,
        bool,
    ),
    String,
> {
    let limit = limit.clamp(1, HISTORY_LIMIT);
    let rows = sqlx::query(CONVERSATION_SQL)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind((limit + 1) as i64)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .fetch_all(pool.get())
        .await
        .map_err(|error| error.to_string())?;
    let mut selected = None;
    let mut openings = Vec::new();
    let mut raw = Vec::new();
    let mut truncated = false;
    for row in rows {
        if row
            .try_get::<String, _>("record_kind")
            .map_err(|e| e.to_string())?
            == "raw"
        {
            raw.push(decode_transcript_item(&row)?);
            continue;
        }
        let selected_rank: i64 = row.try_get("selected_rank").map_err(|e| e.to_string())?;
        let history_rank: i64 = row.try_get("history_rank").map_err(|e| e.to_string())?;
        if history_rank > HISTORY_LIMIT as i64 {
            truncated = true;
            if selected_rank != 1 {
                continue;
            }
        }
        let root = ConversationRoot {
            run_id: row.try_get("run_id").map_err(|e| e.to_string())?,
            status: row.try_get("status").map_err(|e| e.to_string())?,
            waiting_for: row.try_get("waiting_for").map_err(|e| e.to_string())?,
            created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
        };
        if let Some(input) = decode_loaded_run_opening(&row)?
            .and_then(|opening| opening.input().map(|input| input.original_input.clone()))
        {
            let source = server_input_source_event_id(session_id, &root.run_id);
            openings.push(ConversationItem::Opening {
                id: format!("input:{}:{source}", root.run_id),
                run_id: root.run_id.clone(),
                source_event_id: source,
                created_at: root.created_at.clone(),
                content: input,
            });
        } else if selected_rank == 1
            && matches!(root.status.as_str(), "running" | "waiting" | "paused")
        {
            return Err(format!(
                "immutable input is unavailable for active Run {}",
                root.run_id
            ));
        }
        if selected_rank == 1 {
            selected = Some(root);
        }
    }
    raw.sort_by_key(|item| item.item_seq);
    let has_more = raw.len() > limit;
    if has_more {
        raw.remove(0);
    }
    let raw_page = RawPage {
        next_before_seq: raw.first().map(|item| item.item_seq),
        has_more,
    };
    hydrate_transcript_artifacts(pool, user_id, session_id, &mut raw).await?;
    let (items, omitted) = compose_conversation(
        raw,
        openings,
        selected.as_ref().map(|root| root.run_id.as_str()),
        limit,
    )?;
    Ok((selected, items, raw_page, truncated || has_more || omitted))
}

fn compose_conversation(
    raw: Vec<TranscriptItemResponse>,
    mut openings: Vec<ConversationItem>,
    selected: Option<&str>,
    limit: usize,
) -> Result<(Vec<ConversationItem>, bool), String> {
    // Raw sequence is authoritative. An opening is anchored before its Run's
    // first retained row; absent that anchor, use immutable admission order.
    openings.sort_by(|a, b| (a.created_at(), a.run_id()).cmp(&(b.created_at(), b.run_id())));
    let admission: std::collections::HashMap<_, _> = openings
        .iter()
        .map(|item| {
            (
                item.run_id().unwrap().to_owned(),
                item.created_at().to_owned(),
            )
        })
        .collect();
    let selected_source = openings
        .iter()
        .find(|item| item.run_id() == selected)
        .and_then(|item| item.source())
        .map(str::to_owned);
    let mut entries: Vec<_> = raw
        .into_iter()
        .map(|item| ConversationItem::Raw {
            item: Box::new(item),
        })
        .collect();
    let mut bytes = 0;
    let mut omitted = false;
    // Prefer recent exact inputs within the historical byte budget.
    let mut retained = Vec::new();
    for opening in openings.into_iter().rev() {
        let ConversationItem::Opening {
            run_id,
            source_event_id,
            content,
            ..
        } = &opening
        else {
            unreachable!()
        };
        if let Some(stored) = entries
            .iter()
            .find(|item| item.source() == Some(source_event_id))
        {
            if !matches!(stored, ConversationItem::Raw { item } if item.run_id.as_ref() == Some(run_id) && item.role == "user" && item.content == *content)
            {
                return Err(format!("conflicting transcript input for Run {run_id}"));
            }
            continue;
        }
        let size = serde_json::to_vec(&opening)
            .map_err(|error| error.to_string())?
            .len();
        if Some(run_id.as_str()) != selected {
            if size > HISTORICAL_INPUT_BYTES.saturating_sub(bytes) {
                omitted = true;
                continue;
            }
            bytes += size;
        }
        retained.push(opening);
    }
    for opening in retained.into_iter().rev() {
        let index = entries
            .iter()
            .position(|entry| entry.run_id() == opening.run_id())
            .or_else(|| {
                entries.iter().position(|entry| {
                    let time = entry
                        .run_id()
                        .and_then(|run| admission.get(run))
                        .map(String::as_str)
                        .unwrap_or(entry.created_at());
                    (time, entry.run_id()) > (opening.created_at(), opening.run_id())
                })
            })
            .unwrap_or(entries.len());
        entries.insert(index, opening);
    }
    // Invisible tool scaffolding must not consume the Web's display window.
    entries.retain(|entry| match entry {
        ConversationItem::Opening { .. } => true,
        ConversationItem::Raw { item } => {
            matches!(item.role.as_str(), "user" | "system")
                || item.role == "assistant"
                    && (!item.content.trim().is_empty()
                        || item
                            .reasoning
                            .as_ref()
                            .is_some_and(|text| !text.trim().is_empty())
                        || !item.artifacts.is_empty())
        }
    });
    if entries.len() > limit {
        omitted = true;
        let start = entries.len() - limit;
        let current = selected_source.as_deref().and_then(|source| {
            entries[..start]
                .iter()
                .position(|item| item.source() == Some(source))
        });
        if let Some(index) = current {
            let input = entries.remove(index);
            entries.drain(..entries.len() - (limit - 1));
            entries.insert(0, input);
        } else {
            entries.drain(..start);
        }
    }
    Ok((entries, omitted))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn opening(run: &str, time: &str, content: &str) -> ConversationItem {
        ConversationItem::Opening {
            id: format!("input:{run}"),
            run_id: run.into(),
            source_event_id: format!("source:{run}"),
            created_at: time.into(),
            content: content.into(),
        }
    }
    fn raw(
        seq: i64,
        run: Option<&str>,
        source: Option<&str>,
        role: &str,
        content: &str,
        time: &str,
    ) -> TranscriptItemResponse {
        TranscriptItemResponse {
            model_item_id: None,
            session_id: "session".into(),
            item_seq: seq,
            run_id: run.map(str::to_owned),
            role: role.into(),
            content: content.into(),
            reasoning: None,
            reasoning_status: None,
            tool_calls: vec![],
            tool_result: None,
            evidence: None,
            artifacts: vec![],
            source_event_id: source.map(str::to_owned),
            created_at: time.into(),
        }
    }
    fn contents(items: &[ConversationItem]) -> Vec<&str> {
        items
            .iter()
            .map(|item| match item {
                ConversationItem::Opening { content, .. } => content.as_str(),
                ConversationItem::Raw { item } => item.content.as_str(),
            })
            .collect()
    }
    #[test]
    fn conversation_preserves_raw_sequence_admission_identity_and_microsecond_order() {
        let raw = vec![
            raw(
                1,
                Some("a"),
                Some("source:a"),
                "user",
                "same",
                "2026-10-25T03:00:00.100001Z",
            ),
            raw(
                2,
                Some("a"),
                None,
                "assistant",
                "answer a",
                "2026-10-25T03:00:00.900000Z",
            ),
            raw(
                3,
                None,
                None,
                "user",
                "imported",
                "2026-10-25T03:00:00.100002Z",
            ),
            raw(
                9,
                Some("d"),
                None,
                "user",
                "guidance",
                "2026-10-25T03:00:01.000000Z",
            ),
            raw(
                10,
                Some("d"),
                None,
                "assistant",
                "answer d",
                "2026-10-25T03:00:01.000000Z",
            ),
        ];
        let openings = vec![
            opening("d", "2026-10-25T03:00:00.100004Z", "last"),
            opening("b", "2026-10-25T03:00:00.100003Z", "same"),
            opening("a", "2026-10-25T03:00:00.100001Z", "same"),
            opening("c", "2026-10-25T03:00:00.100003Z", "third"),
        ];
        let (items, omitted) = compose_conversation(raw, openings, Some("d"), 200).unwrap();
        assert!(!omitted);
        assert_eq!(
            contents(&items),
            [
                "same", "answer a", "imported", "same", "third", "last", "guidance", "answer d"
            ]
        );
        let seqs: Vec<_> = items
            .iter()
            .filter_map(|item| match item {
                ConversationItem::Raw { item } => Some(item.item_seq),
                _ => None,
            })
            .collect();
        assert_eq!(seqs, [1, 2, 3, 9, 10]);
        let wire = serde_json::to_value(items).unwrap();
        assert!(wire[3].get("item_seq").is_none());
    }
    #[test]
    fn conversation_rejects_identity_collisions_instead_of_hiding_them() {
        for (run, role, content) in [
            ("other", "user", "exact"),
            ("current", "assistant", "exact"),
            ("current", "user", "changed"),
        ] {
            let result = compose_conversation(
                vec![raw(
                    1,
                    Some(run),
                    Some("source:current"),
                    role,
                    content,
                    "time",
                )],
                vec![opening("current", "time", "exact")],
                Some("current"),
                200,
            );
            assert!(
                result
                    .err()
                    .unwrap()
                    .contains("conflicting transcript input")
            );
        }
    }
    #[test]
    fn conversation_bounds_serialized_historical_inputs_but_keeps_exact_selected_input() {
        // Quotes, backslashes and multibyte UTF-8 challenge byte vs character accounting.
        let large = "\"\\你好\n".repeat(50_000);
        let history = "\"\\你好\n".repeat(800);
        let mut openings: Vec<_> = (0..200)
            .map(|n| {
                opening(
                    &format!("r{n:03}"),
                    &format!("2026-10-25T03:00:{n:06}Z"),
                    &history,
                )
            })
            .collect();
        openings.push(opening("current", "2026-10-25T00:00:00.000000Z", &large));
        let (items, omitted) =
            compose_conversation(vec![], openings, Some("current"), 200).unwrap();
        assert!(omitted);
        assert_eq!(contents(&items)[0], large);
        let historical: usize = items
            .iter()
            .filter(|item| item.run_id() != Some("current"))
            .map(|item| serde_json::to_vec(item).unwrap().len())
            .sum();
        assert!(historical <= HISTORICAL_INPUT_BYTES);
        assert!(items.len() < 200);
    }
    #[test]
    fn conversation_retains_current_input_when_guidance_fills_the_display_window() {
        for committed in [false, true] {
            let mut rows: Vec<_> = (2..=201)
                .map(|seq| raw(seq, Some("current"), None, "user", "guidance", "later"))
                .collect();
            if committed {
                rows.insert(
                    0,
                    raw(
                        1,
                        Some("current"),
                        Some("source:current"),
                        "user",
                        "",
                        "first",
                    ),
                );
            }
            let (items, omitted) = compose_conversation(
                rows,
                vec![opening("current", "first", "")],
                Some("current"),
                200,
            )
            .unwrap();
            assert!(omitted);
            assert_eq!(items.len(), 200);
            assert_eq!(contents(&items)[0], "");
            assert_eq!(items[0].source(), Some("source:current"));
            assert_eq!(items.last().unwrap().run_id(), Some("current"));
        }
    }
}
