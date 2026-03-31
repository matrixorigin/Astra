use std::fs;

use clap::Parser;
use mo_thin_client::paths;
use mo_thin_client::ThinClient;

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
    let base = cli.api_url.trim_end_matches('/').to_string();
    let api = ThinClient::new(&base, None).map_err(|e| e.to_string())?;
    let command = cli.command.unwrap_or(Command::Interactive);

    match command {
        Command::Interactive => run_interactive(&api, cli.profile.as_deref()).await,
        Command::Login(args) => {
            let username = prompt_or("Username", args.username)?;
            let password = prompt_or("Password", args.password)?;
            let body = api
                .post_auth_login_json(&serde_json::json!({
                    "username": username,
                    "password": password
                }))
                .await
                .map_err(map_thin_err)?;
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
            let body = api
                .post_auth_register_json(&serde_json::json!({
                    "username": username,
                    "email": email,
                    "password": password
                }))
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Whoami => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api.get_auth_me_text(&token).await.map_err(map_thin_err)?;
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
            let body = api
                .post_auth_refresh_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
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
            let body = api
                .post_auth_logout_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
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
            let body = api
                .post_bearer_path_empty_text(&token, paths::ADMIN_INIT)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Audit(args) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut q: Vec<(&str, String)> = vec![("limit", args.limit.to_string())];
            if let Some(user_id) = args.user_id {
                q.push(("user_id", user_id));
            }
            if let Some(since) = args.since {
                q.push(("since", since));
            }
            let body = api
                .get_bearer_path_query_text(&token, paths::ADMIN_AUDIT, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::User(UserCmd::GrantRole(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::ADMIN_USERS_GRANT_ROLE,
                    &serde_json::json!({
                        "username": args.username,
                        "role_name": args.role_name
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::User(UserCmd::RevokeRole(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::ADMIN_USERS_REVOKE_ROLE,
                    &serde_json::json!({
                        "username": args.username,
                        "role_name": args.role_name
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::List) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api.get_models_text(&token).await.map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Add(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::MODELS,
                    &serde_json::json!({
                        "name": args.name,
                        "provider": args.provider,
                        "api_key": args.api_key,
                        "base_url": args.base_url
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Show(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .get_model_text(&token, &args.model_name)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Delete(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .delete_bearer_path_text(&token, &paths::model(&args.model_name))
                .await
                .map_err(map_thin_err)?;
            if body.is_empty() {
                println!("deleted");
            } else {
                print_json_or_raw(&body);
            }
            Ok(())
        }
        Command::Model(ModelCmd::Check(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_empty_text(&token, &paths::model_check(&args.model_name))
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Load(args)) => {
            let content = fs::read_to_string(&args.path).map_err(|e| e.to_string())?;
            let doc: serde_yaml::Value =
                serde_yaml::from_str(&content).map_err(|e| e.to_string())?;
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
                let payload = serde_json::json!({
                    "name": model_name,
                    "provider": provider,
                    "api_key": api_key,
                    "base_url": base_url
                });
                match api
                    .post_bearer_path_json_text(&token, paths::MODELS, &payload)
                    .await
                {
                    Ok(_) => println!("loaded model: {model_name}"),
                    Err(mo_thin_client::ThinClientError::Api { body, .. })
                        if body.contains("already exists") =>
                    {
                        println!("skipped (already exists): {model_name}");
                    }
                    Err(e) => return Err(map_thin_err(e)),
                }
            }
            Ok(())
        }
        Command::Token(TokenCmd::List(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(token_type) = args.token_type {
                q.push(("token_type", token_type));
            }
            if let Some(scope) = args.scope {
                q.push(("scope", scope));
            }
            let body = api
                .get_bearer_path_query_text(&token, paths::ADMIN_TOKENS, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Token(TokenCmd::Create(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::ADMIN_TOKENS,
                    &serde_json::json!({
                        "token_type": args.token_type,
                        "provider": args.provider,
                        "scope": args.scope,
                        "scope_id": args.scope_id,
                        "token_value": args.token_value
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Skill(SkillCmd::List(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let q = vec![
                ("limit", args.limit.to_string()),
                ("offset", args.offset.to_string()),
            ];
            let body = api
                .get_skills_query_text(&token, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Skill(SkillCmd::Show(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let q: Vec<(&str, String)> = if let Some(version) = args.version {
                vec![("version", version)]
            } else {
                vec![]
            };
            let body = api
                .get_skill_query_text(&token, &args.skill_id, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Skill(SkillCmd::Versions(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .get_bearer_path_query_text(
                    &token,
                    &paths::skill_versions(&args.skill_name),
                    &[],
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Prompt(PromptCmd::Optimize(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::ADMIN_PROMPTS_OPTIMIZE,
                    &serde_json::json!({
                        "agent_id": args.agent_id,
                        "optimization_type": args.optimization_type
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Feedback(FeedbackCmd::Stats(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let mut q: Vec<(&str, String)> = Vec::new();
            if let Some(agent_id) = args.agent_id {
                q.push(("agent_id", agent_id));
            }
            if let Some(since) = args.since {
                q.push(("since", since));
            }
            let body = api
                .get_bearer_path_query_text(&token, paths::ADMIN_FEEDBACK_STATS, &q)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Feedback(FeedbackCmd::Export(args)) => {
            let (_, _, _, token) = get_profile_and_token(cli.profile.as_deref())?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::ADMIN_FEEDBACK_EXPORT,
                    &serde_json::json!({
                        "agent_id": args.agent_id,
                        "format": args.format
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
    }
}
