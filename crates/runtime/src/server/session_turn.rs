use astra_core::SharedPool;

const INFER_SESSION_TURN_SQL: &str = "\
    SELECT MAX(turn_seq) \
    FROM agent_events \
    WHERE user_id = ? AND session_id = ?";

fn next_session_turn_from_max_turn_seq(max_turn_seq: Option<i64>) -> u32 {
    let current = max_turn_seq.unwrap_or(0).max(0) as u64;
    u32::try_from(current.saturating_add(1)).unwrap_or(u32::MAX)
}

pub(crate) async fn infer_session_turn(
    shared_pool: Option<&SharedPool>,
    user_id: &str,
    session_id: &str,
) -> u32 {
    let Some(shared_pool) = shared_pool else {
        return 1;
    };
    let max_turn_seq: Option<i64> = sqlx::query_scalar(INFER_SESSION_TURN_SQL)
        .bind(user_id)
        .bind(session_id)
        .fetch_one(shared_pool.get())
        .await
        .unwrap_or(None);
    next_session_turn_from_max_turn_seq(max_turn_seq)
}

#[cfg(test)]
mod tests {
    use super::{INFER_SESSION_TURN_SQL, infer_session_turn, next_session_turn_from_max_turn_seq};

    #[tokio::test]
    async fn infer_session_turn_without_pool_defaults_to_first_turn() {
        assert_eq!(infer_session_turn(None, "user", "session").await, 1);
    }

    #[test]
    fn next_session_turn_uses_max_turn_seq() {
        assert_eq!(next_session_turn_from_max_turn_seq(None), 1);
        assert_eq!(next_session_turn_from_max_turn_seq(Some(-4)), 1);
        assert_eq!(next_session_turn_from_max_turn_seq(Some(0)), 1);
        assert_eq!(next_session_turn_from_max_turn_seq(Some(7)), 8);
        assert_eq!(
            next_session_turn_from_max_turn_seq(Some(i64::MAX)),
            u32::MAX
        );
    }

    #[test]
    fn infer_session_turn_query_uses_indexable_max_turn_seq_not_count() {
        assert!(INFER_SESSION_TURN_SQL.contains("MAX(turn_seq)"));
        assert!(INFER_SESSION_TURN_SQL.contains("user_id = ?"));
        assert!(INFER_SESSION_TURN_SQL.contains("session_id = ?"));
        assert!(!INFER_SESSION_TURN_SQL.contains("COUNT(*)"));
        assert!(!INFER_SESSION_TURN_SQL.contains("event_type = 'user_query'"));
    }
}
