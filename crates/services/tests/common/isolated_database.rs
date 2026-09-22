//! Shared isolation contract for tests that exercise identity/credential writes.
//! The online runner explicitly designates its database via ASTRA_TEST_DATABASE.

pub fn require_isolated_database(database: &str) {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let designated = std::env::var("ASTRA_TEST_DATABASE")
        .expect("explicitly designate an isolated database with ASTRA_TEST_DATABASE");
    assert!(
        is_designated_database(database, &designated),
        "effective ASTRA_DATABASE must match the explicitly designated ASTRA_TEST_DATABASE"
    );
}

fn is_designated_database(database: &str, designated: &str) -> bool {
    !database.is_empty()
        && database == designated
        && database.len() <= 64
        && database
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !matches!(
            database,
            "mysql" | "information_schema" | "mo_catalog" | "system" | "astra_runtime"
        )
}

#[test]
fn isolated_database_contract_accepts_runner_names_and_rejects_implicit_targets() {
    for name in [
        "astra_runtime_test_integration",
        "astra_runtime_test_runtime_ignored",
        "review_local",
    ] {
        assert!(is_designated_database(name, name));
    }
    for name in [
        "",
        "mysql",
        "mo_catalog",
        "astra_runtime",
        "test;DROP DATABASE mysql",
    ] {
        assert!(!is_designated_database(name, name));
    }
    assert!(!is_designated_database(
        "production",
        "astra_runtime_test_integration"
    ));
}
