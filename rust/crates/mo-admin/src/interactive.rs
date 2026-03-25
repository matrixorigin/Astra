use std::{borrow::Cow, fs, path::PathBuf};

use crossterm::style::Stylize;
use reqwest::header::CONTENT_TYPE;
use rustyline::{
    CompletionType, Config, Context, Editor, Helper,
    completion::{Completer, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::FileHistory,
    validate::{ValidationContext, ValidationResult, Validator},
};

use super::credentials::{load_credentials, profile_name, save_credentials};
use super::http_helpers::{auth_headers, get_profile_and_token, print_json_or_raw, read_api_error};

const ADMIN_COMMANDS: &[(&str, &str)] = &[
    ("whoami", "Show current user info"),
    ("health", "Check API server health"),
    ("init", "Initialize admin system"),
    ("refresh", "Refresh access token"),
    ("logout", "Logout and clear tokens"),
    ("audit", "List audit log  (e.g. audit 50)"),
    ("token list", "List API tokens"),
    ("skill list", "List registered skills"),
    (
        "prompt optimize",
        "Optimize agent prompt  (e.g. prompt optimize <agent_id>)",
    ),
    ("feedback stats", "Show feedback statistics"),
    ("model list", "List available models"),
    (
        "model check",
        "Check model health  (e.g. model check <name>)",
    ),
    (
        "user grant-role",
        "Grant role to user  (e.g. user grant-role <user> <role>)",
    ),
    (
        "user revoke-role",
        "Revoke role from user  (e.g. user revoke-role <user> <role>)",
    ),
    ("help", "Show this help"),
    ("exit", "Exit admin REPL"),
];

struct AdminHelper;

impl Completer for AdminHelper {
    type Candidate = Pair;
    fn complete(
        &self,
        line: &str,
        _pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let matches = ADMIN_COMMANDS
            .iter()
            .filter(|(cmd, _)| cmd.starts_with(line) && !line.is_empty())
            .map(|(cmd, desc)| Pair {
                display: format!("{cmd}  {}", (*desc).dim()),
                replacement: cmd.to_string(),
            })
            .collect();
        Ok((0, matches))
    }
}
impl Hinter for AdminHelper {
    type Hint = String;
    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<String> {
        None
    }
}
impl Highlighter for AdminHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        Cow::Borrowed(line)
    }
    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        false
    }
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("{}", hint.dim()))
    }
}
impl Validator for AdminHelper {
    fn validate(&self, _ctx: &mut ValidationContext) -> rustyline::Result<ValidationResult> {
        Ok(ValidationResult::Valid(None))
    }
}
impl Helper for AdminHelper {}

pub(crate) async fn run_interactive(
    client: &reqwest::Client,
    base: &str,
    profile: Option<&str>,
) -> Result<(), String> {
    let history_path = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
        .join("admin_history");
    if let Some(parent) = history_path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let rl_config = Config::builder()
        .completion_type(CompletionType::List)
        .max_history_size(500)
        .unwrap()
        .build();

    let mut editor: Editor<AdminHelper, FileHistory> =
        Editor::with_config(rl_config).map_err(|e| e.to_string())?;
    editor.set_helper(Some(AdminHelper));
    let _ = editor.load_history(&history_path);

    eprintln!();
    eprintln!(
        "{}",
        "mo-admin interactive mode  (type help for commands, Ctrl+D to exit)".dim()
    );
    eprintln!();

    loop {
        let readline_result = tokio::task::block_in_place(|| {
            editor.readline(&format!("{} ", "mo-admin❯".yellow().bold()))
        });

        let line = match readline_result {
            Err(ReadlineError::Interrupted) => {
                eprintln!("^C");
                continue;
            }
            Err(ReadlineError::Eof) => {
                eprintln!("{}", "Bye!".dim());
                break;
            }
            Err(e) => {
                eprintln!("{}", format!("Readline error: {}", e).red());
                break;
            }
            Ok(l) => l,
        };

        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let _ = editor.add_history_entry(&line);

        if line.eq_ignore_ascii_case("exit") || line.eq_ignore_ascii_case("quit") {
            eprintln!("{}", "Bye!".dim());
            break;
        }

        let result: Result<(), String> = if line.eq("help") {
            eprintln!();
            eprintln!("{}", "Admin Commands".bold().yellow());
            eprintln!("{}", "─".repeat(60).dim());
            for (cmd, desc) in ADMIN_COMMANDS {
                eprintln!("  {:35}  {}", cmd.yellow(), desc.dim());
            }
            eprintln!();
            Ok(())
        } else if line.eq("whoami") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .get(format!("{base}/auth/me"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if line.eq("health") {
            let resp = client
                .get(format!("{base}/health"))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if status.is_success() {
                eprintln!("{}", "✓ API server is healthy".green());
                print_json_or_raw(&body);
                Ok(())
            } else {
                Err(read_api_error(status, &body))
            }
        } else if line.eq("init") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .post(format!("{base}/admin/init"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                eprintln!("{}", "✓ Admin initialized".green());
                print_json_or_raw(&body);
                Ok(())
            }
        } else if line.eq("refresh") {
            let mut creds = load_credentials();
            let name = profile_name(profile, &creds);
            let saved = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = saved
                .refresh_token
                .ok_or_else(|| "no refresh token".to_string())?;
            let resp = client
                .post(format!("{base}/auth/refresh"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({ "refresh_token": refresh_token }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                let value: serde_json::Value =
                    serde_json::from_str(&body).map_err(|e| e.to_string())?;
                let new_access = value
                    .get("access_token")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing access_token")?;
                let new_refresh = value
                    .get("refresh_token")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("missing refresh_token")?;
                let entry = creds.profiles.entry(name).or_default();
                entry.access_token = Some(new_access.to_string());
                entry.refresh_token = Some(new_refresh.to_string());
                save_credentials(&creds)?;
                eprintln!("{}", "✓ Token refreshed".green());
                Ok(())
            }
        } else if line.eq("logout") {
            let mut creds = load_credentials();
            let name = profile_name(profile, &creds);
            let saved = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| "not logged in".to_string())?;
            let refresh_token = saved
                .refresh_token
                .ok_or_else(|| "no refresh token".to_string())?;
            let _ = client
                .post(format!("{base}/auth/logout"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({ "refresh_token": refresh_token }))
                .send()
                .await;
            if let Some(entry) = creds.profiles.get_mut(&name) {
                entry.access_token = None;
                entry.refresh_token = None;
            }
            save_credentials(&creds)?;
            eprintln!("{}", "✓ Logged out".green());
            Ok(())
        } else if line.starts_with("audit") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let limit = line
                .split_whitespace()
                .nth(1)
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(100);
            let resp = client
                .get(format!("{base}/admin/audit"))
                .headers(auth_headers(&token)?)
                .query(&[("limit", limit.to_string())])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if line.eq("token list") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .get(format!("{base}/admin/tokens"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if line.eq("skill list") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .get(format!("{base}/skills"))
                .headers(auth_headers(&token)?)
                .query(&[("limit", "50"), ("offset", "0")])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if line.eq("feedback stats") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .get(format!("{base}/admin/feedback/stats"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if line.eq("model list") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .get(format!("{base}/models"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if let Some(model_name) = line.strip_prefix("model check ") {
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .post(format!("{base}/models/{}/check", model_name.trim()))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if let Some(rest) = line.strip_prefix("prompt optimize ") {
            let mut parts = rest.split_whitespace();
            let agent_id = parts.next().ok_or_else(|| {
                "usage: prompt optimize <agent_id> [optimization_type]".to_string()
            })?;
            let optimization_type = parts.next().unwrap_or("quality");
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client.post(format!("{base}/admin/prompts/optimize"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({"agent_id": agent_id, "optimization_type": optimization_type}))
                .send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                print_json_or_raw(&body);
                Ok(())
            }
        } else if let Some(rest) = line.strip_prefix("user grant-role ") {
            let mut parts = rest.split_whitespace();
            let username = parts
                .next()
                .ok_or_else(|| "usage: user grant-role <username> <role_name>".to_string())?;
            let role_name = parts
                .next()
                .ok_or_else(|| "usage: user grant-role <username> <role_name>".to_string())?;
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .post(format!("{base}/admin/users/grant-role"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({"username": username, "role_name": role_name}))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                eprintln!(
                    "{}",
                    format!("✓ Role '{role_name}' granted to '{username}'").green()
                );
                Ok(())
            }
        } else if let Some(rest) = line.strip_prefix("user revoke-role ") {
            let mut parts = rest.split_whitespace();
            let username = parts
                .next()
                .ok_or_else(|| "usage: user revoke-role <username> <role_name>".to_string())?;
            let role_name = parts
                .next()
                .ok_or_else(|| "usage: user revoke-role <username> <role_name>".to_string())?;
            let (_, _, _, token) = get_profile_and_token(profile)?;
            let resp = client
                .post(format!("{base}/admin/users/revoke-role"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({"username": username, "role_name": role_name}))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                Err(read_api_error(status, &body))
            } else {
                eprintln!(
                    "{}",
                    format!("✓ Role '{role_name}' revoked from '{username}'").green()
                );
                Ok(())
            }
        } else {
            Err(format!(
                "unknown command '{line}' — type 'help' to list commands"
            ))
        };

        if let Err(err) = result {
            eprintln!("{}", format!("❌ {err}").red());
        }
    }

    let _ = editor.save_history(&history_path);
    Ok(())
}
