use super::*;

pub(super) async fn handle_account_command(
    cmd: &str,
    arg: &str,
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
) -> Result<(), String> {
    match cmd {
        "/register" => {
            eprintln!(
                "{}",
                "  ── Register a new account ─────────────────────".cyan()
            );
            let username = prompt_or("Username", None)?;
            let email = prompt_or("Email   ", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_register(client, base, &username, &email, &password).await {
                Ok(_) => {
                    eprintln!("{}", "  \u{2713}  Registered! Logging in…".green());
                    match do_login(client, base, profile, &username, &password).await {
                        Ok(_) => eprintln!("{}", "  \u{2713}  Logged in".green()),
                        Err(e) => {
                            eprintln!("{}", format!("  \u{2717}  Login failed: {}", e).red())
                        }
                    }
                }
                Err(e) => eprintln!("{}", format!("  \u{2717}  Register failed: {}", e).red()),
            }
        }

        "/login" => {
            let username = prompt_or("Username", None)?;
            let password = prompt_password_masked("Password", None)?;
            match do_login(client, base, profile, &username, &password).await {
                Ok(_) => eprintln!("{}", "  \u{2713}  Logged in".green()),
                Err(e) => eprintln!("{}", format!("  \u{2717}  Login failed: {}", e).red()),
            }
        }

        "/logout" => {
            let mut creds = load_credentials();
            let pname = profile_name(profile, &creds);
            if let Some(p) = creds.profiles.get(&pname).cloned()
                && let Some(refresh) = p.refresh_token
            {
                let _ = client
                    .post(format!("{base}/auth/logout"))
                    .header(CONTENT_TYPE, "application/json")
                    .json(&serde_json::json!({ "refresh_token": refresh }))
                    .send()
                    .await;
            }
            if let Some(p) = creds.profiles.get_mut(&pname) {
                p.access_token = None;
                p.refresh_token = None;
                p.last_session_id = None;
            }
            let _ = save_credentials(&creds);
            eprintln!("{}", "  \u{2713}  Logged out".green());
        }

        "/memory-setup" => {
            if arg.is_empty() {
                eprintln!("  Usage: /memory-setup <api_key>");
                eprintln!(
                    "  Get a key from Memoria: curl -X POST http://localhost:8100/auth/keys -H 'Authorization: Bearer <master_key>' -H 'Content-Type: application/json' -d '{{\"user_id\":\"<user>\",\"name\":\"mo-agent\"}}'"
                );
            } else {
                let mut creds = load_credentials();
                let pname = profile_name(profile, &creds);
                let p = creds.profiles.entry(pname).or_default();
                p.memoria_api_key = Some(arg.to_string());
                let _ = save_credentials(&creds);
                unsafe {
                    std::env::set_var("MEMORIA_API_KEY", arg);
                }
                eprintln!("{}", "  \u{2713}  Memoria API key saved".green());
            }
        }
        _ => unreachable!("unexpected account command: {cmd}"),
    }

    Ok(())
}
