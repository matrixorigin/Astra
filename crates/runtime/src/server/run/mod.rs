pub mod binding_resolution;
pub(crate) mod cloud_workspace_provisioning;
pub mod engine;
pub(crate) mod handlers;
pub mod lifecycle;
pub(crate) mod workspace_provisioning;

#[cfg(test)]
pub(crate) async fn insert_active_run_session_fixture(
    pool: &astra_core::SharedPool,
    user_id: &str,
    session_id: &str,
) {
    sqlx::query(
        "INSERT INTO agent_sessions
         (session_id, user_id, agent_id, title, status, metadata, created_at, updated_at)
         VALUES (?, ?, 'run-test-agent', 'run test session', 'active', '{}', NOW(6), NOW(6))",
    )
    .bind(session_id)
    .bind(user_id)
    .execute(pool.get())
    .await
    .expect("run test fixture must establish an active exact-owner session");
}

#[cfg(test)]
pub(crate) async fn cleanup_run_session_fixture(
    pool: &astra_core::SharedPool,
    user_id: &str,
    session_id: &str,
) {
    sqlx::query("DELETE FROM agent_sessions WHERE user_id = ? AND session_id = ?")
        .bind(user_id)
        .bind(session_id)
        .execute(pool.get())
        .await
        .expect("cleanup run test session fixture");
}

/// Corrupt serialized admission in negative tests without exposing a mutable
/// production opening API. Delivery events remain a separate projection.
#[cfg(test)]
pub(crate) fn mutate_run_opening_fixture(
    run: &mut astra_services::runs::DurableRunRecord,
    mutate: impl FnOnce(&mut serde_json::Value),
) {
    let mut opening = serde_json::to_value(run.opening.as_ref().unwrap()).unwrap();
    mutate(&mut opening);
    run.opening = Some(serde_json::from_value(opening).unwrap());
}
