use astra_core::SharedPool;

pub(crate) async fn infer_session_turn(
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
) -> u32 {
    let Some(shared_pool) = shared_pool else {
        return 1;
    };
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_events \
         WHERE user_id = ? AND session_id = ? AND event_type = 'user_query'",
    )
    .bind(user_id)
    .bind(session_id)
    .fetch_one(shared_pool.get())
    .await
    .unwrap_or(0);
    (count.max(0) as u32).saturating_add(1)
}

#[cfg(test)]
mod tests {
    use super::infer_session_turn;

    #[tokio::test]
    async fn infer_session_turn_without_pool_defaults_to_first_turn() {
        assert_eq!(infer_session_turn(None, "user", "session").await, 1);
    }
}
