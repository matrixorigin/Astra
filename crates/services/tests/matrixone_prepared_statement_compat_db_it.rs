mod common;

use astra_core::{matrixone_statement_with_null_shape, push_matrixone_bound_string_set};
use sqlx::{MySql, QueryBuilder, Row};

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn nullable_string_parameters_round_trip_across_cached_statement_shapes() {
    let pool = common::setup_pool().await;
    let mut connection = pool.get().acquire().await.unwrap();
    let table = format!("bind_compat_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE TABLE {table} (id VARCHAR(64) PRIMARY KEY, c2 VARCHAR(64), c3 VARCHAR(64), c4 VARCHAR(64), c5 VARCHAR(64), c6 VARCHAR(64), c7 VARCHAR(64), c8 VARCHAR(64), c9 VARCHAR(64), c10 VARCHAR(64), CONSTRAINT chk_stage_status CHECK ((c8 = 'accepted' AND c9 IS NULL) OR (c8 = 'terminal' AND c9 IN ('succeeded', 'failed'))))"
    ))
    .execute(&mut *connection)
    .await
    .unwrap();
    let insert_sql = format!(
        "INSERT INTO {table} (id, c2, c3, c4, c5, c6, c7, c8, c9, c10) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );
    let accepted_insert = matrixone_statement_with_null_shape(&insert_sql, [false, false]);
    sqlx::query(&accepted_insert)
        .bind("accepted-id")
        .bind("two")
        .bind("three")
        .bind("four")
        .bind("five")
        .bind("six")
        .bind(Option::<&str>::None)
        .bind("accepted")
        .bind(Option::<&str>::None)
        .bind("ten")
        .execute(&mut *connection)
        .await
        .unwrap();
    let terminal_insert = matrixone_statement_with_null_shape(&insert_sql, [false, true]);
    sqlx::query(&terminal_insert)
        .bind("terminal-id")
        .bind("two")
        .bind("three")
        .bind("four")
        .bind("five")
        .bind("six")
        .bind(Option::<&str>::None)
        .bind("terminal")
        .bind(Some("succeeded"))
        .bind("ten")
        .execute(&mut *connection)
        .await
        .unwrap();
    let row = sqlx::query(&format!(
        "SELECT c7, c8, c9, c10 FROM {table} WHERE id = 'terminal-id'"
    ))
    .fetch_one(&mut *connection)
    .await
    .unwrap();
    assert_eq!(row.try_get::<Option<String>, _>("c7").unwrap(), None);
    assert_eq!(
        row.try_get::<Option<String>, _>("c8").unwrap().as_deref(),
        Some("terminal")
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("c9").unwrap().as_deref(),
        Some("succeeded")
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("c10").unwrap().as_deref(),
        Some("ten")
    );
    sqlx::query(&format!("DROP TABLE {table}"))
        .execute(&mut *connection)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires live MatrixOne"]
async fn bound_string_relation_returns_every_requested_row() {
    let pool = common::setup_pool().await;
    let mut connection = pool.get().acquire().await.unwrap();
    let table = format!("bound_set_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!(
        "CREATE TABLE {table} (id VARCHAR(64) PRIMARY KEY, value VARCHAR(64) NOT NULL)"
    ))
    .execute(&mut *connection)
    .await
    .unwrap();
    for value in ["alpha", "bravo", "charlie", "delta"] {
        sqlx::query(&format!("INSERT INTO {table} (id, value) VALUES (?, ?)"))
            .bind(value)
            .bind(value)
            .execute(&mut *connection)
            .await
            .unwrap();
    }

    let requested = ["delta", "alpha", "charlie", "bravo"];
    let mut query = QueryBuilder::<MySql>::new(format!(
        "SELECT source.value FROM {table} AS source INNER JOIN "
    ));
    push_matrixone_bound_string_set(&mut query, requested);
    query.push(" AS requested ON requested.value = source.id ORDER BY source.value");
    let rows = query.build().fetch_all(&mut *connection).await.unwrap();
    let values = rows
        .iter()
        .map(|row| row.try_get::<String, _>("value").unwrap())
        .collect::<Vec<_>>();
    assert_eq!(values, ["alpha", "bravo", "charlie", "delta"]);

    sqlx::query(&format!("DROP TABLE {table}"))
        .execute(&mut *connection)
        .await
        .unwrap();
}
