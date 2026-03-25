use std::fs;

use clap::Parser;
use reqwest::header::CONTENT_TYPE;

mod cli_args;
mod credentials;
mod http_helpers;
mod input;
mod interactive;

use cli_args::*;
use credentials::*;
use http_helpers::*;
use input::*;
use interactive::run_interactive;

#[tokio::main]
async fn main() -> Result<(), String> {
    let cli = Cli::parse();
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("http client should build");
    let base = cli.api_url.trim_end_matches('/').to_string();
    let command = cli.command.unwrap_or(Command::Interactive);

    match command {
        Command::Interactive => run_interactive(&client, &base, cli.profile.as_deref()).await,
        Command::Login(args) => {
            let username = prompt_or("Username", args.username)?;
            let password = prompt_or("Password", args.password)?;
            let resp = client
                .post(format!("{base}/auth/login"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({
                    "username": username,
                    "password": password
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            let value: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let access = value
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing access_token".to_string())?
                .to_string();
            let refresh = value
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing refresh_token".to_string())?
                .to_string();
            let mut creds = load_credentials();
            let name = profile_name(cli.profile.as_deref(), &creds);
            creds.current_profile = Some(name.clone());
            creds.profiles.insert(
                name,
                Profile {
                    username: Some(username),
                    access_token: Some(access),
                    refresh_token: Some(refresh),
                },
            );
            save_credentials(&creds)?;
            println!("logged in");
            Ok(())
        }
        Command::Register(args) => {
            let username = prompt_or("Username", args.username)?;
            let password = prompt_or("Password", args.password)?;
            let email = args
                .email
                .unwrap_or_else(|| format!("{username}@example.com"));
            let resp = client
                .post(format!("{base}/auth/register"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({
                    "username": username,
                    "email": email,
                    "password": password
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Whoami => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .get(format!("{base}/auth/me"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Refresh => {
            let mut creds = load_credentials();
            let name = profile_name(cli.profile.as_deref(), &creds);
            let profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
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
                return Err(read_api_error(status, &body));
            }
            let value: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let new_access = value
                .get("access_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing access_token".to_string())?;
            let new_refresh = value
                .get("refresh_token")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "missing refresh_token".to_string())?;
            let entry = creds.profiles.entry(name).or_default();
            entry.access_token = Some(new_access.to_string());
            entry.refresh_token = Some(new_refresh.to_string());
            save_credentials(&creds)?;
            println!("token refreshed");
            Ok(())
        }
        Command::Logout => {
            let mut creds = load_credentials();
            let name = profile_name(cli.profile.as_deref(), &creds);
            let profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
            let resp = client
                .post(format!("{base}/auth/logout"))
                .header(CONTENT_TYPE, "application/json")
                .json(&serde_json::json!({ "refresh_token": refresh_token }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            if let Some(entry) = creds.profiles.get_mut(&name) {
                entry.access_token = None;
                entry.refresh_token = None;
            }
            save_credentials(&creds)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Init => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/admin/init"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Audit(args) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut req = client
                .get(format!("{base}/admin/audit"))
                .headers(auth_headers(&token)?)
                .query(&[("limit", args.limit.to_string())]);
            if let Some(user_id) = args.user_id {
                req = req.query(&[("user_id", user_id)]);
            }
            if let Some(since) = args.since {
                req = req.query(&[("since", since)]);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::User(UserCmd::GrantRole(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/admin/users/grant-role"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "username": args.username,
                    "role_name": args.role_name
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::User(UserCmd::RevokeRole(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/admin/users/revoke-role"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "username": args.username,
                    "role_name": args.role_name
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::List) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .get(format!("{base}/models"))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Add(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/models"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "name": args.name,
                    "provider": args.provider,
                    "api_key": args.api_key,
                    "base_url": args.base_url
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Show(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .get(format!("{base}/models/{}", args.model_name))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Delete(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .delete(format!("{base}/models/{}", args.model_name))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            if body.is_empty() {
                println!("deleted");
            } else {
                print_json_or_raw(&body);
            }
            Ok(())
        }
        Command::Model(ModelCmd::Check(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/models/{}/check", args.model_name))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Load(args)) => {
            let content = fs::read_to_string(&args.path).map_err(|e| e.to_string())?;
            let doc: serde_yaml::Value =
                serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
            // Support both `models: [...]` and bare `- name: ...` array formats
            let models = if let Some(seq) = doc.as_sequence() {
                seq
            } else {
                doc.get("models")
                    .and_then(serde_yaml::Value::as_sequence)
                    .ok_or_else(|| "missing models list in yaml".to_string())?
            };

            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            for entry in models {
                let model_name = entry
                    .get("name")
                    .and_then(serde_yaml::Value::as_str)
                    .ok_or_else(|| "model.name missing".to_string())?;
                let provider = entry
                    .get("provider")
                    .and_then(serde_yaml::Value::as_str)
                    .ok_or_else(|| "model.provider missing".to_string())?;
                let api_key = entry
                    .get("api_key")
                    .and_then(serde_yaml::Value::as_str)
                    .unwrap_or("");
                let base_url = entry
                    .get("base_url")
                    .and_then(serde_yaml::Value::as_str)
                    .map(ToString::to_string);
                let resp = client
                    .post(format!("{base}/models"))
                    .headers(auth_headers(&token)?)
                    .json(&serde_json::json!({
                        "name": model_name,
                        "provider": provider,
                        "api_key": api_key,
                        "base_url": base_url
                    }))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                let status = resp.status();
                let body = resp.text().await.map_err(|e| e.to_string())?;
                if !status.is_success() {
                    return Err(read_api_error(status, &body));
                }
                println!("loaded model: {model_name}");
            }
            Ok(())
        }
        Command::Token(TokenCmd::List(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut req = client
                .get(format!("{base}/admin/tokens"))
                .headers(auth_headers(&token)?);
            if let Some(token_type) = args.token_type {
                req = req.query(&[("token_type", token_type)]);
            }
            if let Some(scope) = args.scope {
                req = req.query(&[("scope", scope)]);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Token(TokenCmd::Create(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/admin/tokens"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "token_type": args.token_type,
                    "provider": args.provider,
                    "scope": args.scope,
                    "scope_id": args.scope_id,
                    "token_value": args.token_value
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Skill(SkillCmd::List(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .get(format!("{base}/skills"))
                .headers(auth_headers(&token)?)
                .query(&[
                    ("limit", args.limit.to_string()),
                    ("offset", args.offset.to_string()),
                ])
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Skill(SkillCmd::Show(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut req = client
                .get(format!("{base}/skills/{}", args.skill_id))
                .headers(auth_headers(&token)?);
            if let Some(version) = args.version {
                req = req.query(&[("version", version)]);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Skill(SkillCmd::Versions(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .get(format!("{base}/skills/{}/versions", args.skill_name))
                .headers(auth_headers(&token)?)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Prompt(PromptCmd::Optimize(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/admin/prompts/optimize"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "agent_id": args.agent_id,
                    "optimization_type": args.optimization_type
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Feedback(FeedbackCmd::Stats(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut req = client
                .get(format!("{base}/admin/feedback/stats"))
                .headers(auth_headers(&token)?);
            if let Some(agent_id) = args.agent_id {
                req = req.query(&[("agent_id", agent_id)]);
            }
            if let Some(since) = args.since {
                req = req.query(&[("since", since)]);
            }
            let resp = req.send().await.map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Feedback(FeedbackCmd::Export(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let resp = client
                .post(format!("{base}/admin/feedback/export"))
                .headers(auth_headers(&token)?)
                .json(&serde_json::json!({
                    "agent_id": args.agent_id,
                    "format": args.format
                }))
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = resp.status();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(read_api_error(status, &body));
            }
            print_json_or_raw(&body);
            Ok(())
        }
    }
}
