//! A bounded display read over the two existing durable facts: admitted input
//! and committed transcript. This module owns no persistence or Run lifecycle.
use super::session_handlers::{
    TranscriptItemResponse, decode_transcript_item, hydrate_transcript_artifacts,
};
use crate::server::run::lifecycle::server_input_source_event_id;
use astra_core::SharedPool;
use astra_services::runs::{MAX_RUN_CHECKPOINT_BYTES, decode_loaded_run_opening};
use serde::Serialize;
use sqlx::Row;

const HISTORY_LIMIT: usize = 200;
const HISTORICAL_INPUT_BYTES: usize = 256 * 1024;

#[derive(Clone, Serialize)]
pub(crate) struct ConversationFailure {
    pub message: String,
    pub error_code: Option<String>,
}

struct RunFailure {
    run_id: String,
    created_at: String,
    terminal_assistant_source: Option<String>,
    failure: ConversationFailure,
    detail_truncated: bool,
}

#[derive(Serialize)]
pub(crate) struct ConversationResponse {
    pub kind: &'static str,
    pub session: astra_server_types::SessionResponse,
    pub selected_root: Option<ConversationRoot>,
    pub items: Vec<ConversationItem>,
    pub history_loaded: bool,
    pub raw_page: Option<RawPage>,
    pub truncated: Option<bool>,
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
        #[serde(skip_serializing_if = "Option::is_none")]
        failure: Option<ConversationFailure>,
    },
    Opening {
        id: String,
        run_id: String,
        source_event_id: String,
        created_at: String,
        content: String,
    },
    Failure {
        id: String,
        run_id: String,
        created_at: String,
        failure: ConversationFailure,
    },
}

impl ConversationItem {
    fn run_id(&self) -> Option<&str> {
        match self {
            Self::Raw { item, .. } => item.run_id.as_deref(),
            Self::Opening { run_id, .. } | Self::Failure { run_id, .. } => Some(run_id),
        }
    }
    fn source(&self) -> Option<&str> {
        match self {
            Self::Raw { item, .. } => item.source_event_id.as_deref(),
            Self::Failure { .. } => None,
            Self::Opening {
                source_event_id, ..
            } => Some(source_event_id),
        }
    }
    fn created_at(&self) -> &str {
        match self {
            Self::Raw { item, .. } => &item.created_at,
            Self::Opening { created_at, .. } | Self::Failure { created_at, .. } => created_at,
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
// Historical admission counts BOTH stored JSON bodies plus a conservative
// display envelope: two fully escaped run IDs (<= 6 bytes per source byte),
// two fixed trace:user:<64 hex> identities, timestamp and JSON field syntax.
// This bounds transfer before decoding. Recent history is a contiguous prefix;
// inputs are never sliced. The selected opening keeps its existing 8 MiB bound.
// A matching reusable continuation needs only root metadata in this snapshot.
// Keep the already-fixed owner/Session partition on SUM: it is semantically
// identical and avoids MatrixOne prepared-window failure (matrixone#31089).
const CONVERSATION_SQL: &str = r#"
WITH roots AS (
    SELECT user_id, run_id, session_id, status, waiting_for, created_at,
           ROW_NUMBER() OVER (PARTITION BY user_id, session_id ORDER BY created_at DESC, run_id DESC) AS history_rank,
           ROW_NUMBER() OVER (PARTITION BY user_id, session_id ORDER BY
               CASE WHEN status = 'waiting' OR (status = 'paused' AND waiting_for = 'user_input') THEN 4
                    WHEN status = 'running' THEN 2 WHEN status = 'paused' THEN 1 ELSE 0 END DESC,
               created_at DESC, run_id DESC) AS selected_rank
    FROM agent_runs WHERE user_id = ? AND session_id = ? AND parent_run_id IS NULL AND depth = 0
), selection AS (
    SELECT CASE WHEN MAX(run_id) = ? AND
        (MAX(status) IN ('running', 'waiting', 'paused') OR ? = 1)
        THEN 0 ELSE 1 END AS history_loaded
    FROM roots WHERE selected_rank = 1
), candidates AS (
    SELECT r.*, b.opening_hash, b.context_admission_hash,
           COALESCE(OCTET_LENGTH(b.opening_events_json), 0) +
               COALESCE(OCTET_LENGTH(b.opening_context_json), 4) AS stored_bytes,
           s.history_loaded,
           CASE WHEN r.status = 'failed' THEN LEFT(b.error_message, 256) ELSE NULL END AS failure_message,
           CASE WHEN r.status = 'failed' THEN b.error_code ELSE NULL END AS failure_code,
           CASE WHEN r.status = 'failed' THEN
               NULLIF(JSON_UNQUOTE(JSON_EXTRACT(b.terminal_receipt_json, '$.receipt.transcript.assistant_source_event_id')), 'null')
               ELSE NULL END AS terminal_assistant_source,
           CASE WHEN r.status = 'failed' AND CHAR_LENGTH(b.error_message) > 256 THEN 1 ELSE 0 END AS failure_detail_truncated
    FROM roots r JOIN agent_runs b ON b.user_id = r.user_id AND b.run_id = r.run_id
    CROSS JOIN selection s
    WHERE b.user_id = ? AND b.session_id = ? AND (history_rank <= 201 OR selected_rank = 1)
      AND (s.history_loaded = 1 OR selected_rank = 1)
), budgeted AS (
    SELECT c.*, SUM(CASE WHEN selected_rank = 1 THEN 0 ELSE
        stored_bytes + 512 + 12 * OCTET_LENGTH(run_id) +
        CASE WHEN status = 'failed' THEN 1024 + 6 *
            (COALESCE(OCTET_LENGTH(failure_message), 0) +
             COALESCE(OCTET_LENGTH(failure_code), 0) +
             COALESCE(OCTET_LENGTH(terminal_assistant_source), 0)) ELSE 0 END END)
        OVER (PARTITION BY user_id, session_id ORDER BY history_rank ROWS UNBOUNDED PRECEDING) AS history_bytes
    FROM candidates c
), admitted AS (
    SELECT b.*, CASE WHEN history_loaded = 1 AND stored_bytes <= ? AND
        (selected_rank = 1 OR (history_rank <= 200 AND history_bytes <= ?))
        THEN 1 ELSE 0 END AS body_loaded
    FROM budgeted b
), raw_items AS (
    SELECT t.user_id, t.session_id, t.item_seq
    FROM session_transcript_items t
    LEFT JOIN agent_runs r ON r.user_id = ? AND r.session_id = ? AND r.run_id = t.run_id
    WHERE t.user_id = ? AND t.session_id = ?
      AND (SELECT history_loaded FROM selection) = 1
      AND (t.run_id IS NULL OR (r.run_id IS NOT NULL AND r.parent_run_id IS NULL AND r.depth = 0))
    ORDER BY t.item_seq DESC LIMIT ?
)
SELECT 'root' AS record_kind, r.session_id, r.run_id, r.status, r.waiting_for,
       DATE_FORMAT(r.created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
       history_rank, selected_rank,
       r.opening_hash, r.context_admission_hash,
       CASE WHEN body_loaded = 1 THEN b.opening_events_json ELSE CAST(NULL AS CHAR) END AS opening_events_json,
       CASE WHEN body_loaded = 1 THEN b.opening_context_json ELSE CAST(NULL AS CHAR) END AS opening_context_json,
       body_loaded AS opening_body_loaded, stored_bytes, history_loaded,
       CASE WHEN body_loaded = 1 THEN failure_message ELSE NULL END AS failure_message,
       failure_code, terminal_assistant_source,
       CASE WHEN failure_detail_truncated = 1 OR (body_loaded = 0 AND failure_message IS NOT NULL)
            THEN 1 ELSE 0 END AS failure_detail_truncated,
       CAST(NULL AS SIGNED) AS item_seq, CAST(NULL AS CHAR) AS role,
       CAST(NULL AS CHAR) AS content, CAST(NULL AS CHAR) AS payload_json, CAST(NULL AS CHAR) AS source_event_id
FROM admitted r JOIN agent_runs b
 ON b.user_id = r.user_id AND b.session_id = r.session_id AND b.run_id = r.run_id
UNION ALL
SELECT 'raw', t.session_id, t.run_id, r.status, CAST(NULL AS CHAR),
       DATE_FORMAT(t.created_at, '%Y-%m-%dT%H:%i:%s.%fZ'),
       CAST(0 AS SIGNED), CAST(0 AS SIGNED),
       CAST(NULL AS CHAR), CAST(NULL AS CHAR), CAST(NULL AS CHAR), CAST(NULL AS CHAR), CAST(0 AS SIGNED), CAST(0 AS SIGNED), CAST(1 AS SIGNED),
       CAST(NULL AS CHAR), CASE WHEN r.status = 'failed' THEN r.error_code ELSE NULL END,
       CASE WHEN r.status = 'failed' THEN
           NULLIF(JSON_UNQUOTE(JSON_EXTRACT(r.terminal_receipt_json, '$.receipt.transcript.assistant_source_event_id')), 'null')
           ELSE NULL END,
       CASE WHEN r.status = 'failed' AND r.error_message IS NOT NULL THEN 1 ELSE 0 END,
       t.item_seq, t.role, t.content, t.payload_json, t.source_event_id
FROM raw_items p JOIN session_transcript_items t
 ON t.user_id = p.user_id AND t.session_id = p.session_id AND t.item_seq = p.item_seq
LEFT JOIN agent_runs r ON r.user_id = p.user_id AND r.session_id = p.session_id AND r.run_id = t.run_id
WHERE t.user_id = ? AND t.session_id = ?
"#;

// Keep bind order beside the SQL, shared by the reader and its database
// regression, which measures returned bodies before any Rust materialization.
pub(crate) fn conversation_query<'a>(
    user_id: &'a str,
    session_id: &'a str,
    limit: usize,
    reuse_run_id: Option<&'a str>,
    reuse_terminal: bool,
) -> sqlx::query::Query<'a, sqlx::MySql, sqlx::mysql::MySqlArguments> {
    sqlx::query(CONVERSATION_SQL)
        .bind(user_id)
        .bind(session_id)
        .bind(reuse_run_id)
        .bind(i32::from(reuse_terminal))
        .bind(user_id)
        .bind(session_id)
        .bind(MAX_RUN_CHECKPOINT_BYTES as i64)
        .bind(HISTORICAL_INPUT_BYTES as i64)
        .bind(user_id)
        .bind(session_id)
        .bind(user_id)
        .bind(session_id)
        .bind((limit + 1) as i64)
        .bind(user_id)
        .bind(session_id)
}

pub(crate) async fn read_conversation(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
    limit: usize,
    reuse_run_id: Option<&str>,
    reuse_terminal: bool,
) -> Result<
    (
        Option<ConversationRoot>,
        Vec<ConversationItem>,
        RawPage,
        bool,
        bool,
    ),
    String,
> {
    let limit = limit.clamp(1, HISTORY_LIMIT);
    let rows = conversation_query(user_id, session_id, limit, reuse_run_id, reuse_terminal)
        .fetch_all(pool.get())
        .await
        .map_err(|error| error.to_string())?;
    let mut selected = None;
    let mut openings = Vec::new();
    let mut raw = Vec::new();
    let mut failures = std::collections::HashMap::new();
    let mut truncated = false;
    let mut history_loaded = true;
    for row in rows {
        if row
            .try_get::<Option<String>, _>("status")
            .map_err(|e| e.to_string())?
            .as_deref()
            == Some("failed")
        {
            let run_id: String = row.try_get("run_id").map_err(|e| e.to_string())?;
            let message: Option<String> =
                row.try_get("failure_message").map_err(|e| e.to_string())?;
            let detail_truncated = row
                .try_get::<i64, _>("failure_detail_truncated")
                .map_err(|e| e.to_string())?
                == 1;
            // A raw page can reach a Run outside the bounded opening window.
            // Its compact outcome still comes from this statement snapshot.
            if message.is_some() || !failures.contains_key(&run_id) {
                failures.insert(
                    run_id.clone(),
                    RunFailure {
                        run_id,
                        created_at: row.try_get("created_at").map_err(|e| e.to_string())?,
                        terminal_assistant_source: row
                            .try_get("terminal_assistant_source")
                            .map_err(|e| e.to_string())?,
                        detail_truncated,
                        failure: ConversationFailure {
                            message: message
                                .filter(|text| !text.trim().is_empty())
                                .unwrap_or_else(|| "Astra run failed.".into()),
                            error_code: row.try_get("failure_code").map_err(|e| e.to_string())?,
                        },
                    },
                );
            }
        }
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
        history_loaded = row
            .try_get::<i64, _>("history_loaded")
            .map_err(|e| e.to_string())?
            == 1;
        let loaded = row
            .try_get::<i64, _>("opening_body_loaded")
            .map_err(|e| e.to_string())?
            == 1;
        if selected_rank == 1 {
            selected = Some(ConversationRoot {
                run_id: root.run_id.clone(),
                status: root.status.clone(),
                waiting_for: root.waiting_for.clone(),
                created_at: root.created_at.clone(),
            });
            if history_loaded
                && row
                    .try_get::<i64, _>("stored_bytes")
                    .map_err(|e| e.to_string())?
                    > MAX_RUN_CHECKPOINT_BYTES as i64
            {
                return Err("Run opening exceeds the bounded snapshot size".into());
            }
        }
        if !loaded {
            truncated |= history_loaded;
            continue;
        }
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
    truncated |= failures.values().any(|failure| failure.detail_truncated);
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
        if history_loaded {
            failures.into_values().collect()
        } else {
            Vec::new()
        },
        selected.as_ref().map(|root| root.run_id.as_str()),
        limit,
    )?;
    Ok((
        selected,
        items,
        raw_page,
        truncated || has_more || omitted,
        history_loaded,
    ))
}

fn compose_conversation(
    raw: Vec<TranscriptItemResponse>,
    mut openings: Vec<ConversationItem>,
    mut failures: Vec<RunFailure>,
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
            failure: None,
        })
        .collect();
    let mut omitted = false;
    // SQL has already admitted a bounded prefix of complete opening bodies.
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
            if !matches!(stored, ConversationItem::Raw { item, .. } if item.run_id.as_ref() == Some(run_id) && item.role == "user" && item.content == *content)
            {
                return Err(format!("conflicting transcript input for Run {run_id}"));
            }
            continue;
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
        ConversationItem::Opening { .. } | ConversationItem::Failure { .. } => true,
        ConversationItem::Raw { item, .. } => {
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
    failures.sort_by(|a, b| (&a.created_at, &a.run_id).cmp(&(&b.created_at, &b.run_id)));
    for outcome in failures {
        if let Some(ConversationItem::Raw { failure, .. }) = entries.iter_mut().find(|entry| {
            matches!(entry, ConversationItem::Raw { item, .. } if item.role == "assistant"
                && item.run_id.as_deref() == Some(&outcome.run_id)
                && outcome.terminal_assistant_source.is_some()
                && item.source_event_id == outcome.terminal_assistant_source)
        }) {
            *failure = Some(outcome.failure);
        } else if let Some(index) = entries
            .iter()
            .rposition(|entry| entry.run_id() == Some(&outcome.run_id))
        {
            entries.insert(
                index + 1,
                ConversationItem::Failure {
                    id: format!("failure:{}", outcome.run_id),
                    run_id: outcome.run_id,
                    created_at: outcome.created_at,
                    failure: outcome.failure,
                },
            );
        }
    }
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
                ConversationItem::Raw { item, .. } => item.content.as_str(),
                ConversationItem::Failure { failure, .. } => &failure.message,
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
        let (items, omitted) =
            compose_conversation(raw, openings, Vec::new(), Some("d"), 200).unwrap();
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
                ConversationItem::Raw { item, .. } => Some(item.item_seq),
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
                Vec::new(),
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
    fn historical_sql_envelope_bounds_json_escaping_and_fixed_identities() {
        for run in ["run".to_owned(), "\"\n\t\\你好".repeat(100)] {
            let source = server_input_source_event_id("session", &run);
            let item = ConversationItem::Opening {
                id: format!("input:{run}:{source}"),
                run_id: run.clone(),
                source_event_id: source,
                created_at: "2026-10-25T03:00:00.123456Z".into(),
                content: "\"\n\\你好".repeat(100),
            };
            let encoded = serde_json::to_vec(&item).unwrap().len();
            let ConversationItem::Opening { content, .. } = &item else {
                unreachable!()
            };
            let stored_content = serde_json::to_vec(content).unwrap().len();
            assert!(encoded <= stored_content + 512 + 12 * run.len());
        }
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
                Vec::new(),
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

    #[test]
    fn failure_projection_uses_exact_terminal_identity_and_preserves_intermediate_replies() {
        for terminal in [Some("terminal"), None, Some("not-in-this-page")] {
            let rows = vec![
                raw(
                    1,
                    Some("old"),
                    Some("earlier"),
                    "assistant",
                    "completed reply",
                    "first",
                ),
                raw(
                    2,
                    Some("old"),
                    Some("terminal"),
                    "assistant",
                    "partial answer",
                    "second",
                ),
                raw(
                    3,
                    Some("next"),
                    Some("next-answer"),
                    "assistant",
                    "new answer",
                    "third",
                ),
            ];
            let (items, omitted) = compose_conversation(
                rows,
                vec![
                    opening("old", "first", "old input"),
                    opening("next", "third", "next input"),
                ],
                vec![RunFailure {
                    run_id: "old".into(),
                    created_at: "first".into(),
                    terminal_assistant_source: terminal.map(str::to_owned),
                    detail_truncated: false,
                    failure: ConversationFailure {
                        message: "provider disconnected".into(),
                        error_code: Some("stream_transport".into()),
                    },
                }],
                Some("next"),
                200,
            )
            .unwrap();
            assert!(!omitted);
            let wire = serde_json::to_value(&items).unwrap();
            assert!(
                wire[1].get("failure").is_none(),
                "completed intermediate reply stays complete"
            );
            if terminal == Some("terminal") {
                assert_eq!(wire[2]["failure"]["message"], "provider disconnected");
                assert_eq!(wire[2]["content"], "partial answer");
                assert_eq!(
                    wire.as_array()
                        .unwrap()
                        .iter()
                        .filter(|item| item.get("failure").is_some())
                        .count(),
                    1
                );
            } else {
                assert!(wire[2].get("failure").is_none());
                assert_eq!(wire[3]["kind"], "failure");
                assert!(wire[3].get("item_seq").is_none());
            }
            assert!(
                wire.as_array()
                    .unwrap()
                    .last()
                    .unwrap()
                    .get("failure")
                    .is_none()
            );
        }
    }

    #[test]
    fn opening_only_failure_notice_obeys_the_display_window_and_preserves_selected_input() {
        let (items, truncated) = compose_conversation(
            Vec::new(),
            vec![
                opening("old", "first", "old input"),
                opening("current", "second", "exact input"),
            ],
            vec![RunFailure {
                run_id: "current".into(),
                created_at: "second".into(),
                terminal_assistant_source: None,
                detail_truncated: false,
                failure: ConversationFailure {
                    message: "failure".into(),
                    error_code: None,
                },
            }],
            Some("current"),
            2,
        )
        .unwrap();
        assert!(truncated);
        assert_eq!(contents(&items), ["exact input", "failure"]);
        let wire = serde_json::to_value(items).unwrap();
        assert_eq!(wire[1]["id"], "failure:current");
        assert!(wire[1].get("item_seq").is_none());
    }
}
