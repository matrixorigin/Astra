// A healthy single-node database does not reliably reproduce a stale pooled
// snapshot. Guard the transaction boundary explicitly, independently of database
// timing. This is a source contract, not a cross-CN fault-injection test.
const TRANSACTION_READ: &str =
    ".fetch_session_for_user_with_admission_facts(&mut*tx,&session_id,&user_id";
const COMMIT: &str = "tx.commit().await.map_err(internal_error)?;";

fn session_creation_after_insert() -> String {
    let source: String = include_str!("../src/auth/session.rs")
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .flat_map(str::chars)
        .filter(|c| !c.is_whitespace())
        .collect();
    let (_, implementation) = source.split_once("asyncfncreate_session_record(").unwrap();
    let (create, _) = implementation.split_once("pubfnnew(").unwrap();
    let (_, after_insert) = create.split_once("INSERTINTOagent_sessions").unwrap();
    after_insert.to_owned()
}

fn reads_insert_before_committing(create: &str) -> bool {
    let insert = create.find(".execute(&mut*tx).await.map_err(internal_error)?;");
    // The executor and row identity own this contract. Additional projection
    // arguments do not change which transaction observes the inserted Session.
    let read = create
        .find(TRANSACTION_READ)
        .filter(|i| create[i + TRANSACTION_READ.len()..].starts_with([',', ')']));
    let commit = create.find(COMMIT);
    let returned =
        create.find("Ok(SessionCreationResult{session:record,created:true,admission_facts,})");
    matches!((insert, read, commit, returned), (Some(i), Some(r), Some(c), Some(o)) if i < r && r < c && c < o)
        && !create.contains(".fetch_session_for_user(&pool,")
        && !create.contains(".fetch_session_for_user_with_admission_facts(&pool,")
}

#[test]
fn session_creation_reads_its_insert_and_propagates_commit_failure() {
    assert!(reads_insert_before_committing(
        &session_creation_after_insert()
    ));
}

#[test]
fn transaction_contract_rejects_pooled_read_and_ignored_commit_error() {
    let create = session_creation_after_insert();
    assert!(reads_insert_before_committing(&create));
    for (original, replacement) in [
        (
            TRANSACTION_READ,
            ".fetch_session_for_user_with_admission_facts(&pool,&session_id,&user_id",
        ),
        (COMMIT, "let_=tx.commit().await;"),
    ] {
        let mutated = create.replacen(original, replacement, 1);
        assert_ne!(mutated, create, "mutation must change the insert path");
        assert!(!reads_insert_before_committing(&mutated));
    }
}
