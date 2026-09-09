// A healthy single-node database does not reliably reproduce a stale pooled
// snapshot. Guard the transaction boundary explicitly, independently of database
// timing. This is a source contract, not a cross-CN fault-injection test.
fn reads_insert_before_committing(source: &str) -> bool {
    let source: String = source
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect();
    let Some((_, implementation)) =
        source.split_once("implSessionServiceforDatabaseSessionService{")
    else {
        return false;
    };
    let Some((create, _)) = implementation.split_once("asyncfnlist_sessions(") else {
        return false;
    };
    let insert = create.find(".execute(&mut*tx).await.map_err(internal_error)?;");
    let read = create.find(".fetch_session_for_user(&mut*tx,&session_id,&user_id)");
    let commit = create.find("tx.commit().await.map_err(internal_error)?;");
    let returned = create.find("Ok(record)");
    matches!((insert, read, commit, returned), (Some(i), Some(r), Some(c), Some(o)) if i < r && r < c && c < o)
        && !create.contains(".fetch_session_for_user(&pool,")
}

const SOURCE: &str = include_str!("../src/auth/session.rs");

#[test]
fn session_creation_reads_its_insert_and_propagates_commit_failure() {
    assert!(reads_insert_before_committing(SOURCE));
}

#[test]
fn transaction_contract_rejects_pooled_read_and_ignored_commit_error() {
    assert!(!reads_insert_before_committing(&SOURCE.replace(
        ".fetch_session_for_user(&mut *tx, &session_id, &user_id)",
        ".fetch_session_for_user(&pool, &session_id, &user_id)",
    )));
    assert!(!reads_insert_before_committing(&SOURCE.replace(
        "tx.commit().await.map_err(internal_error)?;",
        "let _ = tx.commit().await;",
    )));
}
