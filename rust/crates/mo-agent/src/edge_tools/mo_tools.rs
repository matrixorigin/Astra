//! MatrixOne convergence tools: SQL query, snapshot, and branch operations.
//!
//! These tools bridge code versioning (git) with data versioning (MatrixOne),
//! enabling coordinated management of code and database state.
//!
//! Connection details are read from environment variables:
//!   MATRIXONE_HOST (default: localhost)
//!   MATRIXONE_PORT (default: 6001)
//!   MATRIXONE_USER (default: root)
//!   MATRIXONE_PASSWORD (default: dev-only; set for production!)
//!   MATRIXONE_DATABASE (default: dev_agent)
//!
//! Uses the `mysql` CLI client (MySQL protocol compatible), same pattern as
//! git tools — shell out to native CLI for zero Rust-side connection overhead.

use std::process::Command;

use super::*;

// ─── MatrixOne connection helper ────────────────────────────────────────────

/// Build a mysql Command with connection parameters from environment.
fn mo_mysql_cmd(database: Option<&str>) -> Command {
    mo_agent_core::warn_default_credentials_once();
    let host = std::env::var("MATRIXONE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = std::env::var("MATRIXONE_PORT").unwrap_or_else(|_| "6001".to_string());
    let user = std::env::var("MATRIXONE_USER").unwrap_or_else(|_| "root".to_string());
    let password = std::env::var("MATRIXONE_PASSWORD")
        .unwrap_or_else(|_| mo_agent_core::DEV_MATRIXONE_PASSWORD.to_string());
    let db = database.map(String::from).unwrap_or_else(|| {
        std::env::var("MATRIXONE_DATABASE").unwrap_or_else(|_| "dev_agent".to_string())
    });

    let mut cmd = Command::new("mysql");
    cmd.arg(format!("-h{}", host))
        .arg(format!("-P{}", port))
        .arg(format!("-u{}", user))
        .env("MYSQL_PWD", &password) // pass via env, not CLI (hidden from ps)
        .arg(&db)
        .arg("--table"); // Pretty-print results
    cmd
}

/// Execute a SQL statement against MatrixOne via the mysql CLI.
fn mo_execute_sql(sql: &str, database: Option<&str>) -> String {
    let mut cmd = mo_mysql_cmd(database);
    cmd.arg("-e").arg(sql);

    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !out.status.success() {
                let err = if stderr.is_empty() {
                    stdout.to_string()
                } else {
                    stderr.to_string()
                };
                format!("Error: {}", err.trim())
            } else if stdout.is_empty() {
                "OK (no results)".to_string()
            } else {
                let result = stdout.to_string();
                if result.len() > 20_000 {
                    let rows_shown = result[..20_000].matches('\n').count();
                    let total_rows = result.matches('\n').count();
                    let mut t = result[..20_000].to_string();
                    t.push_str(&format!(
                        "\n[truncated at 20KB: showing ~{} of {} rows. Use LIMIT to narrow results.]",
                        rows_shown, total_rows
                    ));
                    t
                } else {
                    result
                }
            }
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                "Error: mysql client not found. Install mysql-client or mariadb-client to use MatrixOne tools.\nHint: apt install mariadb-client OR brew install mysql-client".to_string()
            } else {
                format!("Error: failed to execute mysql: {e}")
            }
        }
    }
}

// ─── SQL safety validation ──────────────────────────────────────────────────

/// Strip SQL comments (both `--` line comments and `/* ... */` block comments).
fn strip_sql_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '-' && chars.peek() == Some(&'-') {
            // Line comment: skip to end of line
            for ch in chars.by_ref() {
                if ch == '\n' {
                    out.push(' ');
                    break;
                }
            }
        } else if c == '/' && chars.peek() == Some(&'*') {
            // Block comment: skip to */
            chars.next(); // consume '*'
            let mut depth = 1u32;
            while depth > 0 {
                match chars.next() {
                    Some('/') if chars.peek() == Some(&'*') => {
                        chars.next();
                        depth += 1;
                    }
                    Some('*') if chars.peek() == Some(&'/') => {
                        chars.next();
                        depth -= 1;
                    }
                    None => break,
                    _ => {}
                }
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

const DESTRUCTIVE_KEYWORDS: &[&str] = &["DROP", "DELETE", "TRUNCATE", "ALTER", "GRANT", "REVOKE"];

/// Check if a SQL string contains destructive operations.
/// Handles block comments, line comments, and multi-statement (semicolons).
/// Returns Some(kind) if blocked, None if safe.
fn check_sql_safety(sql: &str) -> Option<&'static str> {
    let stripped = strip_sql_comments(sql).to_uppercase();
    // Check each statement (split on ';')
    for stmt in stripped.split(';') {
        let first_word = stmt.split_whitespace().next().unwrap_or("");
        for &kw in DESTRUCTIVE_KEYWORDS {
            if first_word == kw {
                return Some(kw);
            }
        }
    }
    None
}

// ─── Snapshot name validation ───────────────────────────────────────────────

/// Validate snapshot name: alphanumeric + underscore + hyphen only.
fn is_valid_snapshot_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

// ─── Tool implementations ───────────────────────────────────────────────────

impl ToolExecutor {
    /// `mo_query`: Execute a SQL query against MatrixOne.
    /// Foundation tool for all database operations.
    /// Blocks destructive DDL/DML (DROP, DELETE, TRUNCATE, ALTER, GRANT, REVOKE)
    /// unless the caller explicitly passes `"allow_destructive": true`.
    pub(crate) fn mo_query(&self, args: &Value) -> String {
        let sql = match args.get("sql").and_then(Value::as_str) {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: missing or empty 'sql' parameter".to_string(),
        };

        // Safety gate: block destructive operations unless explicitly allowed
        let allow_destructive = args
            .get("allow_destructive")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !allow_destructive && let Some(kind) = check_sql_safety(sql) {
            return format!(
                "Error: {kind} statements are blocked by default. \
                     Pass \"allow_destructive\": true to confirm execution."
            );
        }

        let database = args.get("database").and_then(Value::as_str);
        mo_execute_sql(sql, database)
    }

    /// `mo_snapshot`: Create, list, or delete MatrixOne snapshots.
    /// Snapshots capture point-in-time database state for rollback and branching.
    pub(crate) fn mo_snapshot(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");

        match action {
            "create" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid snapshot name '{}'. Use alphanumeric, underscore, or hyphen (max 64 chars)",
                            n
                        );
                    }
                    None => return "Error: missing 'name' for snapshot creation".to_string(),
                };
                // MatrixOne uses account-level snapshots
                let sql = format!("CREATE SNAPSHOT `{}` FOR ACCOUNT root", name);
                mo_execute_sql(&sql, None)
            }
            "list" => mo_execute_sql("SHOW SNAPSHOTS", None),
            "drop" | "delete" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid snapshot name '{}'. Use alphanumeric, underscore, or hyphen",
                            n
                        );
                    }
                    None => return "Error: missing 'name' for snapshot deletion".to_string(),
                };
                let sql = format!("DROP SNAPSHOT IF EXISTS `{}`", name);
                mo_execute_sql(&sql, None)
            }
            "restore" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid snapshot name '{}'. Use alphanumeric, underscore, or hyphen",
                            n
                        );
                    }
                    None => return "Error: missing 'name' for snapshot restore".to_string(),
                };
                let sql = format!("RESTORE ACCOUNT root FROM SNAPSHOT `{}`", name);
                mo_execute_sql(&sql, None)
            }
            other => format!(
                "Error: unknown action '{}'. Supported: create, list, drop, restore",
                other
            ),
        }
    }

    /// `mo_branch`: Coordinate git branches with MatrixOne data branches.
    /// Creates/lists data branches that mirror git branches for experiment isolation.
    pub(crate) fn mo_branch(&self, args: &Value) -> String {
        let action = args.get("action").and_then(Value::as_str).unwrap_or("list");

        match action {
            "list" => {
                // Show snapshots as "branches" + current git branch for context
                let mut result = String::new();

                // Current git branch
                let git_branch = super::git_gix::current_branch(&self.project_root);
                if !git_branch.is_empty() {
                    result.push_str(&format!("## Current Git Branch: {}\n\n", git_branch));
                }

                // MatrixOne snapshots (serve as data branches)
                let snapshots = mo_execute_sql("SHOW SNAPSHOTS", None);
                result.push_str("## MatrixOne Snapshots (Data Branches)\n");
                result.push_str(&snapshots);

                result
            }
            "create" => {
                let name = match args.get("name").and_then(Value::as_str) {
                    Some(n) if is_valid_snapshot_name(n) => n,
                    Some(n) => {
                        return format!(
                            "Error: invalid branch name '{}'. Use alphanumeric, underscore, or hyphen",
                            n
                        );
                    }
                    None => {
                        // Auto-generate name from git branch
                        let branch = super::git_gix::current_branch(&self.project_root);
                        if branch.is_empty() {
                            return "Error: missing 'name' and no git branch detected".to_string();
                        }
                        // Sanitize: keep only alphanumeric and underscore
                        let auto_name: String = format!("br_{}", branch.replace(['/', '-'], "_"))
                            .chars()
                            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                            .collect();
                        if !is_valid_snapshot_name(&auto_name) {
                            return format!(
                                "Error: git branch '{}' produced invalid snapshot name '{}'",
                                branch, auto_name
                            );
                        }
                        let sql = format!("CREATE SNAPSHOT `{}` FOR ACCOUNT root", auto_name);
                        return format!(
                            "Creating data branch '{}' aligned with git branch '{}'\n\n{}",
                            auto_name,
                            branch,
                            mo_execute_sql(&sql, None)
                        );
                    }
                };
                let sql = format!("CREATE SNAPSHOT `{}` FOR ACCOUNT root", name);
                mo_execute_sql(&sql, None)
            }
            "sync" => {
                // Show the relationship between git state and MO state
                let mut result = String::from("## Git ↔ MatrixOne Sync Status\n\n");

                let git_branch = super::git_gix::current_branch(&self.project_root);
                let git_head = super::git_gix::head_short(&self.project_root);
                result.push_str(&format!("Git: branch={}, head={}\n", git_branch, git_head));

                let snapshots = mo_execute_sql("SHOW SNAPSHOTS", None);
                result.push_str(&format!("MatrixOne snapshots:\n{}\n", snapshots));

                // Check if there's a matching snapshot for current branch
                let expected_name = format!("br_{}", git_branch.replace(['/', '-'], "_"));
                if snapshots.contains(&expected_name) {
                    result.push_str(&format!(
                        "\n✅ Data branch '{}' exists for git branch '{}'",
                        expected_name, git_branch
                    ));
                } else {
                    result.push_str(&format!(
                        "\n⚠ No data branch found for git branch '{}'. Create with: mo_branch action=create",
                        git_branch
                    ));
                }

                result
            }
            other => format!(
                "Error: unknown action '{}'. Supported: list, create, sync",
                other
            ),
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn env_guard() -> MutexGuard<'static, ()> {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env lock poisoned")
    }

    // ── Validation ──

    #[test]
    fn valid_snapshot_names() {
        assert!(is_valid_snapshot_name("my_snapshot"));
        assert!(is_valid_snapshot_name("snapshot-123"));
        assert!(is_valid_snapshot_name("br_main"));
        assert!(is_valid_snapshot_name("abc"));
    }

    #[test]
    fn invalid_snapshot_names() {
        assert!(!is_valid_snapshot_name(""));
        assert!(!is_valid_snapshot_name("snap shot")); // space
        assert!(!is_valid_snapshot_name("snap;shot")); // semicolon
        assert!(!is_valid_snapshot_name("snap'shot")); // quote
        assert!(!is_valid_snapshot_name(&"a".repeat(65))); // too long
    }

    // ── Parameter validation ──

    #[test]
    fn mo_query_missing_sql() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_query(&serde_json::json!({}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn mo_query_empty_sql() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_query(&serde_json::json!({"sql": ""}));
        assert!(result.contains("Error"), "should error on empty: {result}");
    }

    #[test]
    fn mo_snapshot_invalid_action() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_snapshot(&serde_json::json!({"action": "fly"}));
        assert!(result.contains("Error"), "should error: {result}");
        assert!(
            result.contains("create, list, drop, restore"),
            "should show supported actions: {result}"
        );
    }

    #[test]
    fn mo_snapshot_create_missing_name() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_snapshot(&serde_json::json!({"action": "create"}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    #[test]
    fn mo_snapshot_create_invalid_name() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result =
            executor.mo_snapshot(&serde_json::json!({"action": "create", "name": "bad;name"}));
        assert!(
            result.contains("Error"),
            "should reject SQL-injection name: {result}"
        );
    }

    #[test]
    fn mo_branch_invalid_action() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_branch(&serde_json::json!({"action": "merge"}));
        assert!(result.contains("Error"), "should error: {result}");
    }

    // ── mo_execute_sql tests (mysql client may not be available) ──

    #[test]
    fn mo_execute_sql_returns_graceful_error_if_no_mysql() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "nonexistent-host-12345");
            std::env::set_var("MATRIXONE_PORT", "6001");
        }
        let result = mo_execute_sql("SELECT 1", None);
        assert!(
            result.contains("Error") || result.contains("error") || result.contains("not found"),
            "should handle missing mysql gracefully: {result}"
        );
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
            std::env::remove_var("MATRIXONE_PORT");
        }
    }

    #[test]
    fn mo_mysql_cmd_uses_env_vars() {
        let _guard = env_guard();
        unsafe {
            std::env::set_var("MATRIXONE_HOST", "testhost");
            std::env::set_var("MATRIXONE_PORT", "7001");
        }
        let cmd = mo_mysql_cmd(Some("testdb"));
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a.contains("testhost")),
            "should use MATRIXONE_HOST"
        );
        assert!(
            args.iter().any(|a| a.contains("7001")),
            "should use MATRIXONE_PORT"
        );
        assert!(
            args.iter().any(|a| a == "testdb"),
            "should use specified database"
        );
        // Password should NOT appear in args (security: hidden from ps)
        assert!(
            !args.iter().any(|a| a.contains("-p")),
            "password should not be in CLI args: {args:?}"
        );
        // Password should be in environment instead
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(
            envs.iter().any(|(k, _)| *k == "MYSQL_PWD"),
            "password should be in MYSQL_PWD env var"
        );
        unsafe {
            std::env::remove_var("MATRIXONE_HOST");
            std::env::remove_var("MATRIXONE_PORT");
        }
    }

    #[test]
    fn mo_mysql_cmd_default_database() {
        let _guard = env_guard();
        unsafe {
            std::env::remove_var("MATRIXONE_DATABASE");
        }
        let cmd = mo_mysql_cmd(None);
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        assert!(
            args.iter().any(|a| a == "dev_agent"),
            "should default to dev_agent: {:?}",
            args
        );
    }

    // ── SQL safety validation ──

    #[test]
    fn sql_safety_blocks_destructive_operations() {
        assert_eq!(check_sql_safety("DROP TABLE users"), Some("DROP"));
        assert_eq!(check_sql_safety("delete from users"), Some("DELETE"));
        assert_eq!(check_sql_safety("TRUNCATE TABLE logs"), Some("TRUNCATE"));
        assert_eq!(
            check_sql_safety("ALTER TABLE users ADD col INT"),
            Some("ALTER")
        );
        assert_eq!(
            check_sql_safety("  GRANT ALL ON *.* TO root"),
            Some("GRANT")
        );
        assert_eq!(
            check_sql_safety("REVOKE INSERT ON db.t FROM u"),
            Some("REVOKE")
        );
    }

    #[test]
    fn sql_safety_allows_safe_operations() {
        assert_eq!(check_sql_safety("SELECT * FROM users"), None);
        assert_eq!(check_sql_safety("SHOW TABLES"), None);
        assert_eq!(check_sql_safety("EXPLAIN SELECT 1"), None);
        assert_eq!(check_sql_safety("INSERT INTO logs VALUES (1)"), None);
        assert_eq!(check_sql_safety("UPDATE users SET name='x'"), None);
        assert_eq!(check_sql_safety("CREATE TABLE t (id INT)"), None);
    }

    #[test]
    fn sql_safety_ignores_leading_comments() {
        assert_eq!(
            check_sql_safety("-- comment\nDROP TABLE users"),
            Some("DROP")
        );
        assert_eq!(check_sql_safety("-- safe comment\nSELECT 1"), None);
    }

    #[test]
    fn mo_query_blocks_destructive_by_default() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        let result = executor.mo_query(&serde_json::json!({"sql": "DROP TABLE users"}));
        assert!(result.contains("blocked"), "should block DROP: {result}");
        assert!(
            result.contains("allow_destructive"),
            "should mention opt-in: {result}"
        );
    }

    #[test]
    fn mo_query_allows_destructive_with_opt_in() {
        let executor = ToolExecutor::new(std::env::temp_dir());
        // This will fail at the mysql connection level, but NOT at the safety check
        let result = executor.mo_query(
            &serde_json::json!({"sql": "DROP TABLE IF EXISTS _test_table", "allow_destructive": true}),
        );
        // Should NOT contain "blocked" — it should attempt execution
        assert!(
            !result.contains("blocked"),
            "should not block with opt-in: {result}"
        );
    }

    // ── SQL safety bypass prevention ──

    #[test]
    fn sql_safety_blocks_block_comments() {
        assert_eq!(
            check_sql_safety("/* harmless */ DROP TABLE users"),
            Some("DROP")
        );
        assert_eq!(check_sql_safety("/**/ DELETE FROM users"), Some("DELETE"));
    }

    #[test]
    fn sql_safety_blocks_multi_statement() {
        assert_eq!(check_sql_safety("SELECT 1; DROP TABLE users"), Some("DROP"));
        assert_eq!(
            check_sql_safety("SHOW TABLES; DELETE FROM logs; SELECT 1"),
            Some("DELETE")
        );
    }

    #[test]
    fn sql_safety_blocks_nested_block_comments() {
        assert_eq!(
            check_sql_safety("/* /* nested */ */ TRUNCATE TABLE t"),
            Some("TRUNCATE")
        );
    }

    #[test]
    fn sql_safety_blocks_mixed_comments_and_semicolons() {
        assert_eq!(
            check_sql_safety("-- safe\nSELECT 1; /* comment */ ALTER TABLE t ADD c INT"),
            Some("ALTER")
        );
    }

    #[test]
    fn strip_sql_comments_preserves_content() {
        assert_eq!(
            strip_sql_comments("SELECT /* col */ name FROM t").trim(),
            "SELECT   name FROM t"
        );
        assert_eq!(
            strip_sql_comments("SELECT 1 -- inline\nFROM t").trim(),
            "SELECT 1  FROM t"
        );
    }
}
