//! Diagnose tool: system diagnostics and health information.

use serde_json::{Value, json};

use super::{AGGREGATE_OUTPUT_BUDGET, ToolExecutor, all_tool_schemas};

impl ToolExecutor {
    // ─── Diagnose tool ────────────────────────────────────────────────────────────

    /// Get system diagnostics and health information.
    pub(super) async fn diagnose(&self, args: &Value) -> String {
        let category = args
            .get("category")
            .and_then(Value::as_str)
            .unwrap_or("all");
        let verbose = args
            .get("verbose")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut result = serde_json::Map::new();

        // System info
        if category == "all" || category == "system" {
            let mut sys_info = serde_json::Map::new();

            // OS info
            sys_info.insert("os".to_string(), json!(std::env::consts::OS));
            sys_info.insert("arch".to_string(), json!(std::env::consts::ARCH));

            // Current working directory
            if let Ok(cwd) = std::env::current_dir() {
                sys_info.insert("cwd".to_string(), json!(cwd.display().to_string()));
            }

            // Project root (sandbox)
            sys_info.insert(
                "project_root".to_string(),
                json!(self.project_root.display().to_string()),
            );

            // Memory info (read from /proc/meminfo on Linux)
            #[cfg(target_os = "linux")]
            {
                if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
                    let mut mem = serde_json::Map::new();
                    for line in meminfo.lines().take(3) {
                        if let Some((key, val)) = line.split_once(':') {
                            mem.insert(key.trim().to_string(), json!(val.trim()));
                        }
                    }
                    sys_info.insert("memory".to_string(), json!(mem));
                }
            }

            // Memory info on macOS (using vm_stat)
            #[cfg(target_os = "macos")]
            {
                if let Ok(output) = std::process::Command::new("vm_stat").output() {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let mut mem = serde_json::Map::new();
                        for line in stdout.lines().take(5) {
                            if let Some((key, val)) = line.split_once(':') {
                                mem.insert(key.trim().to_string(), json!(val.trim()));
                            }
                        }
                        sys_info.insert("memory".to_string(), json!(mem));
                    }
                }
            }

            // Load average on Unix
            #[cfg(target_os = "linux")]
            {
                if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
                    let parts: Vec<&str> = loadavg.split_whitespace().take(3).collect();
                    if parts.len() >= 3 {
                        sys_info.insert(
                            "load_avg".to_string(),
                            json!({
                                "1min": parts[0],
                                "5min": parts[1],
                                "15min": parts[2]
                            }),
                        );
                    }
                }
            }

            // Load average on macOS (using sysctl)
            #[cfg(target_os = "macos")]
            {
                if let Ok(output) = std::process::Command::new("sysctl")
                    .args(["-n", "vm.loadavg"])
                    .output()
                {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let parts: Vec<&str> = stdout
                            .trim()
                            .trim_start_matches('{')
                            .trim_end_matches('}')
                            .split_whitespace()
                            .take(3)
                            .collect();
                        if parts.len() >= 3 {
                            sys_info.insert(
                                "load_avg".to_string(),
                                json!({
                                    "1min": parts[0],
                                    "5min": parts[1],
                                    "15min": parts[2]
                                }),
                            );
                        }
                    }
                }
            }

            // Note for Windows users
            #[cfg(target_os = "windows")]
            {
                sys_info.insert("note".to_string(), json!(
                    "System memory/load info requires external tools on Windows. Use Task Manager or 'systeminfo' command."
                ));
            }

            result.insert("system".to_string(), json!(sys_info));
        }

        // Environment info (only safe vars)
        if category == "all" || category == "environment" {
            let mut env_info = serde_json::Map::new();
            let safe_vars = [
                "PATH",
                "HOME",
                "USER",
                "SHELL",
                "TERM",
                "LANG",
                "PWD",
                "RUST_LOG",
                "MO_MODEL",
                "MO_API_KEY", // show if set (not value)
            ];

            for var in safe_vars {
                if let Ok(val) = std::env::var(var) {
                    // For sensitive vars, just show presence
                    if var.contains("KEY") || var.contains("TOKEN") || var.contains("SECRET") {
                        env_info.insert(var.to_string(), json!("[SET]"));
                    } else if verbose {
                        env_info.insert(var.to_string(), json!(val));
                    } else {
                        // Truncate long values
                        let display = if val.len() > 100 {
                            format!("{}...", val.chars().take(100).collect::<String>())
                        } else {
                            val
                        };
                        env_info.insert(var.to_string(), json!(display));
                    }
                }
            }

            result.insert("environment".to_string(), json!(env_info));
        }

        // Available tools info
        if category == "all" || category == "tools" {
            let tools = all_tool_schemas();
            let tool_names: Vec<&str> = tools
                .iter()
                .filter_map(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                })
                .collect();

            let mut tools_info = serde_json::Map::new();
            tools_info.insert("count".to_string(), json!(tool_names.len()));

            if verbose {
                tools_info.insert("available".to_string(), json!(tool_names));
            } else {
                // Just show categories
                let categories = vec![
                    (
                        "file_ops",
                        vec!["read_file", "write_file", "str_replace", "list_dir"],
                    ),
                    (
                        "search",
                        vec!["grep", "glob", "find_definition", "find_references"],
                    ),
                    ("git", vec!["git_status", "git_diff", "git_log", "git_show"]),
                    (
                        "tasks",
                        vec!["task_create", "task_list", "task_update", "task_stop"],
                    ),
                    ("utility", vec!["bash", "web_fetch", "sleep", "ask_user"]),
                ];
                let mut cat_status = serde_json::Map::new();
                for (cat, expected) in categories {
                    let available = expected.iter().filter(|t| tool_names.contains(t)).count();
                    cat_status.insert(
                        cat.to_string(),
                        json!(format!("{}/{}", available, expected.len())),
                    );
                }
                tools_info.insert("categories".to_string(), json!(cat_status));
            }

            // MCP tools
            if self.mcp_manager.is_some() {
                tools_info.insert("mcp_enabled".to_string(), json!(true));
            }

            result.insert("tools".to_string(), json!(tools_info));
        }

        // Task status
        if category == "all" || category == "tasks" {
            let tasks = self.task_manager.snapshot();

            let mut tasks_info = serde_json::Map::new();
            tasks_info.insert("total".to_string(), json!(tasks.len()));

            let pending = tasks.iter().filter(|t| t.status == "pending").count();
            let in_progress = tasks.iter().filter(|t| t.status == "in_progress").count();
            let completed = tasks.iter().filter(|t| t.status == "completed").count();
            let failed = tasks
                .iter()
                .filter(|t| t.status == "failed" || t.status == "cancelled")
                .count();

            tasks_info.insert("pending".to_string(), json!(pending));
            tasks_info.insert("in_progress".to_string(), json!(in_progress));
            tasks_info.insert("completed".to_string(), json!(completed));
            tasks_info.insert("failed_or_cancelled".to_string(), json!(failed));

            if verbose && !tasks.is_empty() {
                let task_list: Vec<Value> = tasks
                    .iter()
                    .map(|t| {
                        json!({
                            "id": t.id,
                            "title": t.title,
                            "status": t.status,
                            "subtasks": t.subtasks.len()
                        })
                    })
                    .collect();
                tasks_info.insert("list".to_string(), json!(task_list));
            }

            result.insert("tasks".to_string(), json!(tasks_info));
        }

        // Session info
        if category == "all" || category == "session" {
            let mut session_info = serde_json::Map::new();

            // Aggregate output tracking (AtomicUsize uses load, not lock)
            let bytes = self
                .aggregate_output_bytes
                .load(std::sync::atomic::Ordering::Relaxed);
            session_info.insert("output_bytes_this_turn".to_string(), json!(bytes));
            session_info.insert("output_budget".to_string(), json!(AGGREGATE_OUTPUT_BUDGET));
            session_info.insert(
                "output_utilization".to_string(),
                json!(format!(
                    "{:.1}%",
                    (bytes as f64 / AGGREGATE_OUTPUT_BUDGET as f64) * 100.0
                )),
            );

            // Sandbox policy
            {
                let sp_guard = self
                    .sandbox_policy
                    .read()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(ref policy) = *sp_guard {
                    session_info.insert(
                        "sandbox_mode".to_string(),
                        json!(format!("{:?}", policy.mode)),
                    );
                    if verbose {
                        let paths: Vec<String> = policy
                            .allowed_paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect();
                        session_info.insert("allowed_paths".to_string(), json!(paths));
                    }
                } else {
                    session_info.insert("sandbox_mode".to_string(), json!("disabled"));
                }
            }

            result.insert("session".to_string(), json!(session_info));
        }

        serde_json::to_string_pretty(&result)
            .unwrap_or_else(|_| "Error: serialization failed".to_string())
    }
}
