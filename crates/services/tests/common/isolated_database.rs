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

/// Extra guard for destructive schema rehearsals, not ordinary row fixtures.
pub fn is_schema_rehearsal_database(database: &str) -> bool {
    database
        .strip_prefix("astra_test_probe_")
        .is_some_and(|suffix| !suffix.is_empty())
        && is_designated_database(database, database)
}

// This shared module is also included by the default memoria_auth_db_it target:
// the guard test runs without external-contract-tests or a database connection.
#[test]
fn schema_rehearsal_rejects_broad_or_non_test_targets() {
    assert!(is_schema_rehearsal_database("astra_test_probe_20260910"));
    for name in [
        "production",
        "review_local",
        "astra_runtime",
        "astra_test_probe_",
        "astra_test_probe_x;DROP",
        "mysql",
    ] {
        assert!(!is_schema_rehearsal_database(name));
    }
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
