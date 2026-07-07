use astra_core::SharedPool;
use serde_json::Value;
use sqlx::Row;

pub(crate) struct SessionHistoryContext<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) pool: Option<&'a SharedPool>,
}

pub(crate) fn context<'a>(
    user_id: &'a str,
    session_id: &'a str,
    pool: Option<&'a SharedPool>,
) -> SessionHistoryContext<'a> {
    SessionHistoryContext {
        user_id,
        session_id,
        pool,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionHistoryRow {
    item_seq: i64,
    source: String,
    role: String,
    content: String,
    run_id: Option<String>,
    created_at: Option<String>,
}

const ROOT_CONVERSATION_TRANSCRIPT_JOIN: &str = " LEFT JOIN agent_runs ON agent_runs.user_id = session_transcript_items.user_id AND agent_runs.session_id = session_transcript_items.session_id AND agent_runs.run_id = session_transcript_items.run_id";

const ROOT_CONVERSATION_TRANSCRIPT_FILTER: &str = " AND (session_transcript_items.run_id IS NULL OR (agent_runs.run_id IS NOT NULL AND agent_runs.parent_run_id IS NULL))";

const ROOT_CONVERSATION_CHUNK_JOIN: &str = " LEFT JOIN agent_runs ON session_history_chunks.source_table = 'agent_runs' AND agent_runs.user_id = session_history_chunks.user_id AND agent_runs.session_id = session_history_chunks.session_id AND agent_runs.run_id = session_history_chunks.source_id";

const ROOT_CONVERSATION_CHUNK_FILTER: &str =
    " AND (agent_runs.run_id IS NULL OR agent_runs.parent_run_id IS NULL)";

fn json_str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn json_i64_arg(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

fn json_usize_arg(args: &Value, key: &str, default: usize, min: usize, max: usize) -> usize {
    let value = args
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| {
            usize::try_from(value)
                .inspect_err(|e| {
                    tracing::warn!(
                        key,
                        value,
                        error = %e,
                        "json_usize_arg: type overflow, falling back to default={default}"
                    );
                })
                .ok()
        })
        .unwrap_or(default);
    value.clamp(min, max)
}

fn normalized_history_role(args: &Value) -> Option<String> {
    match json_str_arg(args, "role").unwrap_or("all") {
        "user" => Some("user".to_string()),
        "assistant" => Some("assistant".to_string()),
        "system" => Some("system".to_string()),
        _ => None,
    }
}

fn compact_history_content(content: &str, max_chars: usize) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for ch in compact.chars().take(max_chars) {
        out.push(ch);
    }
    if compact.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

pub(crate) fn session_history_match_score(query: &str, content: &str) -> i32 {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return 0;
    }

    let content = content.to_lowercase();
    let mut score = 0;
    if content.contains(&query) {
        score += 100 + i32::try_from(query.chars().count().min(50)).unwrap_or(0);
    }

    for token in query
        .split(|ch: char| ch.is_whitespace() || ",.;:!?()[]{}\"'`/\\|".contains(ch))
        .filter(|token| token.chars().count() >= 2)
    {
        if content.contains(token) {
            score += 10 + i32::try_from(token.chars().count().min(30)).unwrap_or(0);
        }
    }

    if score == 0 && !query.is_ascii() {
        let mut seen = std::collections::HashSet::new();
        let mut char_hits = 0;
        for ch in query.chars().filter(|ch| !ch.is_whitespace()) {
            if seen.insert(ch) && content.contains(ch) {
                char_hits += 1;
            }
        }
        if char_hits >= 3 {
            score += char_hits;
        }
    }

    score
}

fn render_session_history_rows(
    label: &str,
    rows: &[SessionHistoryRow],
    note: Option<String>,
) -> String {
    let mut out = String::new();
    out.push_str(label);
    out.push_str(&format!(
        " session_id={} rows={}\n",
        "<current>",
        rows.len()
    ));
    if let Some(note) = note {
        out.push_str(&note);
        out.push('\n');
    }
    if rows.is_empty() {
        out.push_str("No transcript rows matched. Try a broader query, a larger scan_limit, or page by before_seq.\n");
        return out;
    }

    let min_seq = rows.iter().map(|row| row.item_seq).min().unwrap_or(0);
    let max_seq = rows.iter().map(|row| row.item_seq).max().unwrap_or(0);
    out.push_str(&format!(
        "cursor_hints: older before_seq={}, newer after_seq={}\n",
        min_seq, max_seq
    ));
    for row in rows {
        let created = row.created_at.as_deref().unwrap_or("unknown_time");
        let run = row.run_id.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "[{}] {} source={} role={} ref={}: {}\n",
            row.item_seq,
            created,
            row.source,
            row.role,
            run,
            compact_history_content(&row.content, 700)
        ));
        if out.chars().count() > 12_000 {
            out.push_str("... truncated by session history tool output budget\n");
            break;
        }
    }
    out
}

async fn query_session_history_rows(
    context: &SessionHistoryContext<'_>,
    before_seq: Option<i64>,
    after_seq: Option<i64>,
    limit: usize,
    order: &str,
    role_filter: Option<&str>,
) -> Result<Vec<SessionHistoryRow>, sqlx::Error> {
    let Some(pool) = context.pool else {
        return Ok(Vec::new());
    };

    let mut sql = String::from(
        "SELECT session_transcript_items.item_seq, session_transcript_items.role, \
                session_transcript_items.content, session_transcript_items.run_id, \
                DATE_FORMAT(session_transcript_items.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
         FROM session_transcript_items",
    );
    sql.push_str(ROOT_CONVERSATION_TRANSCRIPT_JOIN);
    sql.push_str(
        " WHERE session_transcript_items.user_id = ? AND session_transcript_items.session_id = ?",
    );
    sql.push_str(ROOT_CONVERSATION_TRANSCRIPT_FILTER);
    if before_seq.is_some() {
        sql.push_str(" AND session_transcript_items.item_seq < ?");
    }
    if after_seq.is_some() {
        sql.push_str(" AND session_transcript_items.item_seq > ?");
    }
    sql.push_str(" ORDER BY session_transcript_items.item_seq ");
    sql.push_str(if order == "asc" { "ASC" } else { "DESC" });
    sql.push_str(&format!(" LIMIT {}", limit.max(1)));

    let mut query = sqlx::query(&sql)
        .bind(context.user_id)
        .bind(context.session_id);
    if let Some(before_seq) = before_seq {
        query = query.bind(before_seq);
    }
    if let Some(after_seq) = after_seq {
        query = query.bind(after_seq);
    }

    let rows = query.fetch_all(pool.get()).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let role: String = row.try_get("role")?;
        if !matches!(role.as_str(), "user" | "assistant" | "system") {
            continue;
        }
        if let Some(filter) = role_filter
            && filter != "all"
            && role != filter
        {
            continue;
        }
        let content: String = row.try_get("content")?;
        if content.trim().is_empty() {
            continue;
        }
        out.push(SessionHistoryRow {
            item_seq: row.try_get("item_seq")?,
            source: "transcript".to_string(),
            role,
            content,
            run_id: row.try_get::<Option<String>, _>("run_id")
                .inspect_err(|e| tracing::warn!(column="run_id", session_id=%context.session_id, error=%e, "session_history: column type mismatch"))
                .ok()
                .flatten(),
            created_at: row
                .try_get::<Option<String>, _>("created_at")
                .inspect_err(|e| tracing::warn!(column="created_at", session_id=%context.session_id, error=%e, "session_history: column type mismatch"))
                .ok()
                .flatten(),
        });
    }
    Ok(out)
}

async fn query_session_history_chunk_rows(
    context: &SessionHistoryContext<'_>,
    query_text: &str,
    limit: usize,
) -> Result<Vec<SessionHistoryRow>, sqlx::Error> {
    let Some(pool) = context.pool else {
        return Ok(Vec::new());
    };

    let mut patterns = vec![format!("%{}%", query_text.trim())];
    for token in query_text
        .split(|ch: char| ch.is_whitespace() || ",.;:!?()[]{}\"'`/\\|".contains(ch))
        .filter(|token| token.chars().count() >= 2)
        .take(4)
    {
        patterns.push(format!("%{token}%"));
    }
    patterns.sort();
    patterns.dedup();

    let mut sql = String::from(
        "SELECT session_history_chunks.chunk_type, session_history_chunks.source_id, \
                session_history_chunks.content_text, \
                COALESCE(session_history_chunks.item_seq_start, session_history_chunks.seq_start) AS item_seq, \
                DATE_FORMAT(session_history_chunks.created_at, '%Y-%m-%dT%H:%i:%s') AS created_at \
         FROM session_history_chunks",
    );
    sql.push_str(ROOT_CONVERSATION_CHUNK_JOIN);
    sql.push_str(
        " WHERE session_history_chunks.user_id = ? AND session_history_chunks.session_id = ?",
    );
    sql.push_str(ROOT_CONVERSATION_CHUNK_FILTER);
    sql.push_str(" AND (");
    for idx in 0..patterns.len() {
        if idx > 0 {
            sql.push_str(" OR ");
        }
        sql.push_str("content_text LIKE ?");
    }
    sql.push_str(&format!(
        ") ORDER BY created_at DESC LIMIT {}",
        limit.max(1)
    ));

    let mut query = sqlx::query(&sql)
        .bind(context.user_id)
        .bind(context.session_id);
    for pattern in patterns {
        query = query.bind(pattern);
    }

    let rows = query.fetch_all(pool.get()).await?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let content: String = row.try_get("content_text")?;
        if content.trim().is_empty() {
            continue;
        }
        let chunk_type: String = row.try_get("chunk_type")?;
        out.push(SessionHistoryRow {
            item_seq: row.try_get("item_seq")?,
            source: "history_chunk".to_string(),
            role: chunk_type,
            content,
            run_id: row.try_get::<Option<String>, _>("source_id")
                .inspect_err(|e| tracing::warn!(column="source_id", session_id=%context.session_id, error=%e, "session_history: column type mismatch"))
                .ok()
                .flatten(),
            created_at: row
                .try_get::<Option<String>, _>("created_at")
                .inspect_err(|e| tracing::warn!(column="created_at", session_id=%context.session_id, error=%e, "session_history: column type mismatch"))
                .ok()
                .flatten(),
        });
    }
    Ok(out)
}

pub(crate) async fn history_page(
    context: SessionHistoryContext<'_>,
    args: &Value,
) -> astra_tools::ToolResult {
    if context.pool.is_none() {
        return astra_tools::ToolResult::error(
            "Error: session_history_page failed operation=preflight reason=database_pool_not_configured".to_string(),
        );
    }
    let before_seq = json_i64_arg(args, "before_seq");
    let after_seq = json_i64_arg(args, "after_seq");
    let limit = json_usize_arg(args, "limit", 20, 1, 50);
    let order = json_str_arg(args, "order").unwrap_or("desc");
    let order = if order == "asc" { "asc" } else { "desc" };
    let role = normalized_history_role(args);

    match query_session_history_rows(
        &context,
        before_seq,
        after_seq,
        limit,
        order,
        role.as_deref(),
    )
    .await
    {
        Ok(rows) => astra_tools::ToolResult::text(render_session_history_rows(
            "session_history_page",
            &rows,
            Some(format!(
                "cursor before_seq={:?} after_seq={:?} order={}",
                before_seq, after_seq, order
            )),
        )),
        Err(error) => astra_tools::ToolResult::error(format!(
            "Error: session_history_page failed for session_id={} operation=query_transcript_page: {}",
            context.session_id, error
        )),
    }
}

pub(crate) async fn history_search(
    context: SessionHistoryContext<'_>,
    args: &Value,
) -> astra_tools::ToolResult {
    if context.pool.is_none() {
        return astra_tools::ToolResult::error(
            "Error: session_history_search failed operation=preflight reason=database_pool_not_configured".to_string(),
        );
    }
    let pattern = json_str_arg(args, "pattern").unwrap_or("").trim();
    if pattern.is_empty() {
        return astra_tools::ToolResult::error(
            "Error: session_history_search requires a non-empty pattern".to_string(),
        );
    }

    let before_seq = json_i64_arg(args, "before_seq");
    let after_seq = json_i64_arg(args, "after_seq");
    let limit = json_usize_arg(args, "limit", 8, 1, 20);
    let scan_limit = json_usize_arg(args, "scan_limit", 400, 50, 1000);
    let role = normalized_history_role(args);

    let mut rows = match query_session_history_rows(
        &context,
        before_seq,
        after_seq,
        scan_limit,
        "desc",
        role.as_deref(),
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            return astra_tools::ToolResult::error(format!(
                "Error: session_history_search failed for session_id={} operation=query_transcript_scan: {}",
                context.session_id, error
            ));
        }
    };

    let scanned = rows.len();
    let chunk_rows = if role.is_none() {
        match query_session_history_chunk_rows(&context, pattern, limit.saturating_mul(4).max(20))
            .await
        {
            Ok(chunk_rows) => chunk_rows,
            Err(error) => {
                tracing::warn!(
                    target: "astra_runtime::session_history",
                    session_id = %context.session_id,
                    error = %error,
                    "failed to query session_history_chunks during history search"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let chunk_candidates = chunk_rows.len();
    rows.extend(chunk_rows);
    rows.retain(|row| session_history_match_score(pattern, &row.content) > 0);
    rows.sort_by(|left, right| {
        let right_score = session_history_match_score(pattern, &right.content);
        let left_score = session_history_match_score(pattern, &left.content);
        right_score
            .cmp(&left_score)
            .then_with(|| right.item_seq.cmp(&left.item_seq))
    });
    rows.truncate(limit);

    astra_tools::ToolResult::text(render_session_history_rows(
        "session_history_search",
        &rows,
        Some(format!(
            "pattern={pattern:?} scanned_transcript_rows={scanned} chunk_candidates={chunk_candidates}; call session_history_around(item_seq=<seq>) to inspect exact surrounding turns"
        )),
    ))
}

pub(crate) async fn history_around(
    context: SessionHistoryContext<'_>,
    args: &Value,
) -> astra_tools::ToolResult {
    if context.pool.is_none() {
        return astra_tools::ToolResult::error(
            "Error: session_history_around failed operation=preflight reason=database_pool_not_configured".to_string(),
        );
    }
    let Some(item_seq) = json_i64_arg(args, "item_seq") else {
        return astra_tools::ToolResult::error(
            "Error: session_history_around requires item_seq".to_string(),
        );
    };
    let radius = json_usize_arg(args, "radius", 3, 0, 10) as i64;
    let role = normalized_history_role(args);

    match query_session_history_rows(
        &context,
        Some(item_seq.saturating_add(radius).saturating_add(1)),
        Some(item_seq.saturating_sub(radius).saturating_sub(1)),
        (radius as usize).saturating_mul(2).saturating_add(1),
        "asc",
        role.as_deref(),
    )
    .await
    {
        Ok(rows) => astra_tools::ToolResult::text(render_session_history_rows(
            "session_history_around",
            &rows,
            Some(format!("anchor_item_seq={item_seq} radius={radius}")),
        )),
        Err(error) => astra_tools::ToolResult::error(format!(
            "Error: session_history_around failed for session_id={} operation=query_transcript_window: {}",
            context.session_id, error
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_preserves_identity_without_pool() {
        let context = context("user-1", "session-1", None);

        assert_eq!(context.user_id, "user-1");
        assert_eq!(context.session_id, "session-1");
        assert!(context.pool.is_none());
    }

    #[tokio::test]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    async fn session_history_tools_filter_child_agent_rows_on_matrixone() {
        let _ = dotenvy::dotenv();
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored MatrixOne tests"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure schema");
        let pool = astra_core::SharedPool::new(&settings)
            .await
            .expect("SharedPool::new");

        let user_id = format!("hist-user-{}", uuid::Uuid::new_v4());
        let session_id = format!("hist-session-{}", uuid::Uuid::new_v4());
        let root_run = format!("root-{}", uuid::Uuid::new_v4());
        let child_run = format!("child-{}", uuid::Uuid::new_v4());

        sqlx::query("INSERT INTO agent_sessions (user_id, session_id, title) VALUES (?, ?, ?)")
            .bind(&user_id)
            .bind(&session_id)
            .bind("history-root-boundary")
            .execute(pool.get())
            .await
            .expect("insert session");
        sqlx::query(
            "INSERT INTO agent_runs
             (user_id, session_id, run_id, parent_run_id, root_run_id, ancestor_path, depth, status)
             VALUES (?, ?, ?, NULL, ?, ?, 0, 'completed')",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&root_run)
        .bind(&root_run)
        .bind(&root_run)
        .execute(pool.get())
        .await
        .expect("insert root run");
        sqlx::query(
            "INSERT INTO agent_runs
             (user_id, session_id, run_id, parent_run_id, root_run_id, ancestor_path, depth, status)
             VALUES (?, ?, ?, ?, ?, ?, 1, 'completed')",
        )
        .bind(&user_id)
        .bind(&session_id)
        .bind(&child_run)
        .bind(&root_run)
        .bind(&root_run)
        .bind(format!("{root_run}/{child_run}"))
        .execute(pool.get())
        .await
        .expect("insert child run");

        for (seq, run_id, role, content) in [
            (1_i64, &root_run, "user", "root user request"),
            (2_i64, &child_run, "assistant", "child agent private answer"),
            (3_i64, &root_run, "assistant", "root assistant answer"),
        ] {
            sqlx::query(
                "INSERT INTO session_transcript_items
                 (user_id, session_id, item_seq, run_id, role, content, content_hash)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&user_id)
            .bind(&session_id)
            .bind(seq)
            .bind(run_id)
            .bind(role)
            .bind(content)
            .bind(format!("hash-{seq}"))
            .execute(pool.get())
            .await
            .expect("insert transcript row");
        }
        for (chunk_id, source_table, source_id, content) in [
            ("root-chunk", "agent_runs", &root_run, "root chunk keep"),
            ("child-chunk", "agent_runs", &child_run, "child chunk leak"),
            (
                "non-run-chunk",
                "runtime_messages",
                &session_id,
                "non run chunk keep",
            ),
            (
                "non-run-collision",
                "runtime_messages",
                &child_run,
                "non run collision chunk keep",
            ),
        ] {
            sqlx::query(
                "INSERT INTO session_history_chunks
                 (chunk_id, user_id, session_id, seq_start, seq_end, chunk_type,
                  source_table, source_id, content_text, content_hash, token_estimate)
                 VALUES (?, ?, ?, 1, 3, 'summary', ?, ?, ?, ?, 1)",
            )
            .bind(format!("{chunk_id}-{}", uuid::Uuid::new_v4()))
            .bind(&user_id)
            .bind(&session_id)
            .bind(source_table)
            .bind(source_id)
            .bind(content)
            .bind(format!("hash-{chunk_id}"))
            .execute(pool.get())
            .await
            .expect("insert history chunk");
        }

        let result = history_page(
            context(&user_id, &session_id, Some(&pool)),
            &json!({"order": "asc", "limit": 10}),
        )
        .await;
        let rendered = result.output;
        assert!(rendered.contains("root user request"), "{rendered}");
        assert!(rendered.contains("root assistant answer"), "{rendered}");
        assert!(
            !rendered.contains("child agent private answer"),
            "child transcript row leaked into root session history: {rendered}"
        );

        let search = history_search(
            context(&user_id, &session_id, Some(&pool)),
            &json!({"pattern": "chunk", "limit": 10, "scan_limit": 50}),
        )
        .await
        .output;
        assert!(search.contains("root chunk keep"), "{search}");
        assert!(search.contains("non run chunk keep"), "{search}");
        assert!(
            search.contains("non run collision chunk keep"),
            "non-run chunk source_id collisions must not be filtered as child runs: {search}"
        );
        assert!(
            !search.contains("child chunk leak"),
            "child run chunk leaked into session history search: {search}"
        );

        let around = history_around(
            context(&user_id, &session_id, Some(&pool)),
            &json!({"item_seq": 2, "radius": 1}),
        )
        .await
        .output;
        assert!(around.contains("root user request"), "{around}");
        assert!(around.contains("root assistant answer"), "{around}");
        assert!(
            !around.contains("child agent private answer"),
            "child transcript row leaked into session history around: {around}"
        );
    }
}
