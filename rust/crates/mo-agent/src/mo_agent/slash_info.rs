use super::*;

fn copy_to_clipboard(text: &str) -> bool {
    let candidates: &[(&str, &[&str])] = &[
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
        ("pbcopy", &[]),
    ];
    for (cmd, args) in candidates {
        if let Ok(mut child) = SysCommand::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return true;
            }
        }
    }
    false
}

pub(super) async fn handle_info_command(
    cmd: &str,
    client: &reqwest::Client,
    base: &str,
    state: &ReplState,
    token: Option<&str>,
) -> Result<(), String> {
    match cmd {
        "/history" => {
            if state.history.is_empty() {
                eprintln!("{}", "  No history yet".dim());
            } else {
                eprintln!(
                    "\n{}",
                    "─── Conversation History ─────────────────────────────────────".bold()
                );
                for (i, (user, asst)) in state.history.iter().enumerate() {
                    let turn_n = i + 1;
                    let u = if user.len() > 80 {
                        format!("{}…", &user[..80])
                    } else {
                        user.clone()
                    };
                    let a = if asst.len() > 80 {
                        format!("{}…", &asst[..80])
                    } else {
                        asst.clone()
                    };
                    eprintln!("  {}", format!("Turn {turn_n}").bold());
                    eprintln!("  {} {}", "❯".cyan(), u);
                    eprintln!("    {}", a.dim());
                    if i + 1 < state.history.len() {
                        eprintln!();
                    }
                }
                eprintln!();
            }
        }

        "/copy" => match &state.last_response {
            Some(text) => {
                let text = text.clone();
                let n = text.chars().count();
                let preview: String = text.chars().take(60).collect();
                let preview_display = if text.chars().count() > 60 {
                    format!("{}…", preview)
                } else {
                    preview
                };
                if copy_to_clipboard(&text) {
                    eprintln!("{}", format!("  ✓ Copied ({n} chars)").green());
                    eprintln!("  {}", preview_display.dim());
                } else {
                    eprintln!(
                        "{}",
                        "  ✗ No clipboard tool found (install xclip or xsel)".yellow()
                    );
                }
            }
            None => eprintln!("{}", "  ✗ No response to copy yet".yellow()),
        },

        "/doctor" => {
            eprintln!(
                "\n{}",
                "─── Diagnostics ──────────────────────────────────────────────".bold()
            );

            // Accumulate rows: (ok: bool, label: &str, detail: String)
            let mut rows: Vec<(bool, &'static str, String)> = Vec::new();

            // Binary version
            rows.push((
                true,
                "binary",
                format!("mo-agent v{}", env!("CARGO_PKG_VERSION")),
            ));

            // API health
            match client.get(format!("{base}/health")).send().await {
                Ok(r) => {
                    let status = r.status();
                    rows.push((status.is_success(), "api health", format!("HTTP {status}")));
                }
                Err(e) => {
                    rows.push((false, "api health", e.to_string()));
                }
            }

            // Auth status
            if let Some(tok) = token {
                match client
                    .get(format!("{base}/auth/me"))
                    .headers(auth_headers(tok)?)
                    .send()
                    .await
                {
                    Ok(r) if r.status().is_success() => {
                        let b = r.text().await.unwrap_or_default();
                        let v: serde_json::Value = serde_json::from_str(&b).unwrap_or_default();
                        let un = v.get("username").and_then(|u| u.as_str()).unwrap_or("?");
                        rows.push((true, "auth", format!("logged in as {un}")));
                    }
                    Ok(r) if r.status().as_u16() == 401 => {
                        rows.push((false, "auth", "token expired — run /login".to_string()));
                    }
                    Ok(r) => {
                        rows.push((false, "auth", format!("HTTP {}", r.status())));
                    }
                    Err(e) => {
                        rows.push((false, "auth", e.to_string()));
                    }
                }
            } else {
                rows.push((false, "auth", "not logged in".to_string()));
            }

            // Git repo
            match std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .output()
            {
                Ok(out) if out.status.success() => {
                    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    rows.push((true, "git repo", path));
                }
                _ => {
                    rows.push((false, "git repo", "not a git repo".to_string()));
                }
            }

            // Memoria
            let memoria_key_set = std::env::var("MEMORIA_API_KEY")
                .or_else(|_| std::env::var("MEMORIA_MASTER_KEY"))
                .is_ok();
            if memoria_key_set {
                let memoria_base = std::env::var("MEMORIA_BASE_URL")
                    .unwrap_or_else(|_| "http://localhost:8100".to_string());
                match client.get(format!("{memoria_base}/health")).send().await {
                    Ok(r) if r.status().is_success() => {
                        rows.push((true, "memoria", format!("reachable at {memoria_base}")));
                    }
                    Ok(r) => {
                        rows.push((
                            false,
                            "memoria",
                            format!("HTTP {} at {memoria_base}", r.status()),
                        ));
                    }
                    Err(_) => {
                        rows.push((false, "memoria", format!("unreachable ({memoria_base})")));
                    }
                }
            } else {
                rows.push((false, "memoria", "MEMORIA_API_KEY not set".to_string()));
            }

            // Print table
            let label_w = rows.iter().map(|(_, l, _)| l.len()).max().unwrap_or(10);
            for (ok, label, detail) in &rows {
                let icon = if *ok {
                    "✓".green().to_string()
                } else {
                    "✗".red().to_string()
                };
                eprintln!("  {}  {:<label_w$}  {}", icon, label, detail.clone().dim());
            }

            let fail_count = rows.iter().filter(|(ok, _, _)| !ok).count();
            eprintln!();
            if fail_count == 0 {
                eprintln!("  {}", "All checks passed".green().bold());
            } else {
                eprintln!("  {}", format!("{fail_count} check(s) failed").red().bold());
            }
            eprintln!();
        }

        "/context" => {
            let sep = "─".repeat(38);
            eprintln!("\n  {}", format!("─── Context Window {sep}").cyan());
            let session_display = state
                .session_id
                .as_deref()
                .map(|s| {
                    if s.len() > 8 {
                        s[..8].to_string()
                    } else {
                        s.to_string()
                    }
                })
                .unwrap_or_else(|| "none".to_string());
            let model_display = state.model.clone().unwrap_or_else(|| "default".to_string());
            let run_display = state
                .run_id
                .as_deref()
                .map(|s| {
                    if s.len() > 8 {
                        s[..8].to_string()
                    } else {
                        s.to_string()
                    }
                })
                .unwrap_or_else(|| "none".to_string());
            let msg_count = state.history.len() * 2;
            // Estimate tokens from history
            let est_messages: Vec<serde_json::Value> = state
                .history
                .iter()
                .flat_map(|(u, a)| {
                    if u.is_empty() {
                        vec![serde_json::json!({"role":"assistant","content":a})]
                    } else {
                        vec![
                            serde_json::json!({"role":"user","content":u}),
                            serde_json::json!({"role":"assistant","content":a}),
                        ]
                    }
                })
                .collect();
            let est_tokens = prompts::estimate_tokens(&est_messages);
            let budget = &state.context_budget;
            let usage_pct = if budget.model_limit > 0 {
                (est_tokens as f64 / budget.model_limit as f64 * 100.0) as u32
            } else {
                0
            };
            let compact_trigger_k = budget.compact_trigger() / 1000;
            eprintln!("  {:<10}  {}", "session".cyan(), session_display.dim());
            eprintln!("  {:<10}  {}", "model".cyan(), model_display.dim());
            eprintln!("  {:<10}  {}", "turn".cyan(), state.turn.to_string().dim());
            eprintln!(
                "  {:<10}  {}",
                "history".cyan(),
                format!("{msg_count} messages").dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "tokens".cyan(),
                format!(
                    "~{}k / {}k ({usage_pct}%)",
                    est_tokens / 1000,
                    budget.model_limit / 1000
                )
                .dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "compact".cyan(),
                format!(
                    "auto at ~{compact_trigger_k}k tokens, keep {} turns",
                    budget.keep_recent_turns
                )
                .dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "explain".cyan(),
                state.explain.to_string().dim()
            );
            eprintln!(
                "  {:<10}  {}",
                "verbose".cyan(),
                state.verbose_mode.to_string().dim()
            );
            eprintln!("  {:<10}  {}", "run_id".cyan(), run_display.dim());
            eprintln!("  {}", "─".repeat(56).cyan().dim());
            eprintln!();
        }

        "/version" => {
            eprintln!("{}", "  mo-agent version 0.1.0 (Rust)".bold());
        }
        _ => unreachable!("unexpected info command: {cmd}"),
    }

    Ok(())
}
