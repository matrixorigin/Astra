//! Current clients cross the production router and fresh storage.

use std::{path::Path, process::Stdio, time::Duration};

use astra_core::config::AppSettings;
use astra_runtime::{build_app, build_server_state};
use sqlx::{Connection, MySqlConnection};
use tokio::process::Command;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "real disposable MatrixOne/Memoria and current linux/amd64 clients; dedicated integration CI"]
async fn candidate_clients_use_the_fresh_server() {
    assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
    let current_cli = std::env::var("ASTRA_COMPAT_CURRENT_CLI_BIN")
        .expect("candidate integration must provide the current CLI");
    let current_edge = std::env::var("ASTRA_COMPAT_CURRENT_EDGE_BIN")
        .expect("candidate integration must provide the current Edge");
    let report = std::env::var("ASTRA_COMPAT_REPORT")
        .expect("dedicated compatibility CI must provide its report path");
    let source = std::env::var("ASTRA_COMPAT_SERVER_SOURCE")
        .expect("dedicated compatibility CI must identify its Server source");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut settings = AppSettings::from_explicit_env().expect("explicit isolated CI settings");
    settings.matrixone.database = "mysql".into();
    let mut admin = MySqlConnection::connect(&settings.matrixone.database_url_with_password())
        .await
        .expect("isolated MatrixOne");
    let database = format!("astra_test_clients_{}", Uuid::new_v4().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .expect("create isolated client compatibility database");
    settings.matrixone.database = database.clone();
    let state = build_server_state(settings)
        .await
        .expect("fresh production Server bootstrap");
    let app = build_app(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("isolated Server listener");
    let origin = format!("http://{}", listener.local_addr().expect("Server address"));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let mut driver = Command::new("python3");
    driver
        .arg(root.join("scripts/ci/client_compatibility.py"))
        .args([
            "--api-url",
            &origin,
            "--report",
            &report,
            "--current-cli",
            &current_cli,
            "--current-edge",
            &current_edge,
            "--disposable-server",
        ])
        .env("ASTRA_COMPAT_SERVER_SOURCE", &source)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let result = tokio::time::timeout(Duration::from_secs(10 * 60), driver.output()).await;
    let settled = state.drain_background_runs(Duration::from_secs(15)).await;
    server.abort();
    let _ = server.await;
    state.close_database_pools().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database}`")))
        .execute(&mut admin)
        .await
        .expect("remove the fixture-owned database");
    assert!(settled, "compatibility runs must settle before cleanup");
    let output = result
        .expect("candidate client driver exceeded its bound")
        .expect("candidate client driver must start");
    let report: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(report).expect("driver must create its categorical report"),
    )
    .expect("compatibility report must be valid JSON");
    assert!(
        output.status.success(),
        "candidate client integration failed: client={}, case={}, category={}, diagnostics={}, provider_errors={}, provider_phases={}; inspect the job summary",
        report["active_client"],
        report["active_case"],
        report["failure_category"],
        report["diagnostics"],
        report["provider_error_categories"],
        report["provider_phases"],
    );
    assert_eq!(report["status"], "passed");
    assert_eq!(report["server_source"], source);
}
