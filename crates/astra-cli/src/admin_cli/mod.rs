use std::fs;

use crate::cli::auth_flow::{
    parse_auth_tokens, save_profile_auth_tokens, save_refreshed_profile_tokens,
};
use crate::cli::cli_config::cli_utils::{
    bound_profile_access_token, credential_store, get_profile_and_token, load_credentials,
    profile_name,
};
use crate::cli::session::session_runtime;
use astra_thin_client::ThinClient;
use astra_thin_client::paths;
use clap::Parser;

pub mod cli_args;
mod config;
mod http_helpers;
mod input;
mod interactive;

pub use cli_args::AdminArgs;
use cli_args::*;
use config::resolve_api_url;
use http_helpers::*;
use input::*;
use interactive::run_interactive;

/// Parse `POST /models` or `PUT /models/{name}` JSON and print `is_active` / `connectivity`.
fn print_model_load_server_result(body: &str, model_name: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        stdout_println!("  (non-JSON response, len {} bytes)", body.len());
        return;
    };
    let active = value
        .get("is_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true); // fail-open: treat missing/invalid as active so user proceeds
    stdout_println!("  is_active: {active}");
    if let Some(c) = value
        .get("connectivity")
        .and_then(serde_json::Value::as_str)
    {
        stdout_println!("  connectivity: {c}");
    } else if !active {
        stdout_println!(
            "  connectivity: (not in response; run: astra admin model check {model_name})"
        );
    }
    if let Some(context_window) = value
        .get("context_window")
        .and_then(serde_json::Value::as_i64)
        .filter(|value| *value > 0)
    {
        stdout_println!("  context_window: {context_window}");
    } else {
        eprintln!("  warning: response did not include a positive context_window");
    }
    let thinking_cap = value
        .get("thinking_capability")
        .and_then(serde_json::Value::as_str);
    if let Some(cap) = thinking_cap {
        match cap {
            "both" => stdout_println!("  thinking: both (Normal/Thinking picker enabled) ✓"),
            "effort_only" => stdout_println!("  thinking: effort_only (Low/High/Max effort) ✓"),
            "native_only" => stdout_println!("  thinking: native_only (model always thinks)"),
            "none" => stdout_println!("  thinking: none"),
            other => stdout_println!("  thinking: {other}"),
        }
    }
    if !active {
        eprintln!(
            "  warning: model is inactive — server probe failed or was skipped; fix YAML then `astra admin model load <file> --update-existing`, or `astra admin model check {model_name}`"
        );
    }
}

fn yaml_str(entry: &serde_yaml_ng::Value, key: &str) -> Option<String> {
    entry
        .get(key)
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
}

fn yaml_i64(entry: &serde_yaml_ng::Value, key: &str) -> Option<i64> {
    entry.get(key).and_then(|v| v.as_i64())
}

fn require_yaml_positive_i64(entry: &serde_yaml_ng::Value, key: &str) -> Result<i64, String> {
    match yaml_i64(entry, key) {
        Some(value) if value > 0 => Ok(value),
        Some(value) => Err(format!(
            "model.{key} must be a positive integer, got {value}"
        )),
        None => Err(format!(
            "model.{key} missing; model registry metadata must declare {key}"
        )),
    }
}

fn require_positive_context_window(value: i32) -> Result<i32, String> {
    if value > 0 {
        Ok(value)
    } else {
        Err(format!(
            "context_window must be a positive token count, got {value}"
        ))
    }
}

fn yaml_f64(entry: &serde_yaml_ng::Value, key: &str) -> Option<f64> {
    entry.get(key).and_then(|v| v.as_f64())
}

fn yaml_str_vec(entry: &serde_yaml_ng::Value, key: &str) -> Option<Vec<String>> {
    entry.get(key).and_then(|v| v.as_sequence()).map(|seq| {
        seq.iter()
            .filter_map(|item| item.as_str().map(ToString::to_string))
            .collect()
    })
}

fn yaml_json(entry: &serde_yaml_ng::Value, key: &str) -> Option<serde_json::Value> {
    entry.get(key).and_then(|v| serde_json::to_value(v).ok())
}

/// Merge optional YAML model fields into an existing JSON object in-place.
fn apply_optional_yaml_fields(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    entry: &serde_yaml_ng::Value,
) {
    if let Some(v) = yaml_str(entry, "description") {
        obj.insert("description".into(), serde_json::json!(v));
    }
    if let Some(v) = yaml_i64(entry, "max_completion_tokens") {
        obj.insert("max_completion_tokens".into(), serde_json::json!(v));
    }
    if let Some(v) = yaml_str_vec(entry, "tags") {
        obj.insert("tags".into(), serde_json::json!(v));
    }
    if let Some(v) = yaml_str_vec(entry, "supported_parameters") {
        obj.insert("supported_parameters".into(), serde_json::json!(v));
    }
    if let Some(v) = yaml_str(entry, "architecture") {
        obj.insert("architecture".into(), serde_json::json!(v));
    }
    let prompt_price = yaml_f64(entry, "pricing_prompt");
    let completion_price = yaml_f64(entry, "pricing_completion");
    if prompt_price.is_some() || completion_price.is_some() {
        obj.insert(
            "pricing".into(),
            serde_json::json!({
                "prompt": prompt_price.unwrap_or(0.0),
                "completion": completion_price.unwrap_or(0.0),
            }),
        );
    }
    if let Some(chain) = yaml_str_vec(entry, "fallback_chain") {
        let quirks = obj.entry("quirks").or_insert_with(|| serde_json::json!({}));
        if let Some(qobj) = quirks.as_object_mut() {
            qobj.insert("fallback_chain".into(), serde_json::json!(chain));
        }
    }
    // `wire_model_name` — literal name to send in the upstream LLM `model`
    // field when the local row's `name` differs (e.g. two rows pointing at
    // the same upstream model id but configured under different providers
    // or endpoints). Routed through `quirks_json` like `fallback_chain`,
    // so no DB migration was needed.
    if let Some(wire) = yaml_str(entry, "wire_model_name") {
        let quirks = obj.entry("quirks").or_insert_with(|| serde_json::json!({}));
        if let Some(qobj) = quirks.as_object_mut() {
            qobj.insert("wire_model_name".into(), serde_json::json!(wire));
        }
    }
    if let Some(cache_capability) = yaml_json(entry, "prompt_cache_capability") {
        if cache_capability.is_object() {
            let quirks = obj.entry("quirks").or_insert_with(|| serde_json::json!({}));
            if let Some(qobj) = quirks.as_object_mut() {
                qobj.insert("prompt_cache_capability".into(), cache_capability);
            }
        } else {
            eprintln!(
                "warning: prompt_cache_capability must be a JSON object; ignoring non-object value"
            );
        }
    }
    if let Some(overrides) = yaml_json(entry, "request_body_overrides") {
        if overrides.is_object() {
            let quirks = obj.entry("quirks").or_insert_with(|| serde_json::json!({}));
            if let Some(qobj) = quirks.as_object_mut() {
                qobj.insert("request_body_overrides".into(), overrides);
            }
        } else {
            eprintln!(
                "warning: request_body_overrides must be a JSON object; ignoring non-object value"
            );
        }
    }
    if let Some(headers) = yaml_json(entry, "probe_headers") {
        if headers.is_object() {
            let quirks = obj.entry("quirks").or_insert_with(|| serde_json::json!({}));
            if let Some(qobj) = quirks.as_object_mut() {
                qobj.insert("probe_headers".into(), headers);
            }
        } else {
            eprintln!("warning: probe_headers must be a JSON object; ignoring non-object value");
        }
    }
    if let Some(endpoint) = yaml_str(entry, "probe_endpoint") {
        let quirks = obj.entry("quirks").or_insert_with(|| serde_json::json!({}));
        if let Some(qobj) = quirks.as_object_mut() {
            qobj.insert("probe_endpoint".into(), serde_json::json!(endpoint));
        }
    }
}

fn build_model_update_payload(
    entry: &serde_yaml_ng::Value,
    provider: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut obj = serde_json::Map::new();
    obj.insert("provider".into(), serde_json::json!(provider));
    obj.insert(
        "context_window".into(),
        serde_json::json!(require_yaml_positive_i64(entry, "context_window")?),
    );
    if let Some(v) = api_key.filter(|v| !v.is_empty()) {
        obj.insert("api_key".into(), serde_json::json!(v));
    }
    if let Some(v) = base_url {
        obj.insert("base_url".into(), serde_json::json!(v));
    }
    apply_optional_yaml_fields(&mut obj, entry);
    Ok(serde_json::Value::Object(obj))
}

fn build_model_create_payload(
    entry: &serde_yaml_ng::Value,
    name: &str,
    provider: &str,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<serde_json::Value, String> {
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return Err(format!(
            "model.api_key missing or empty for new model {name}"
        ));
    }
    let mut obj = serde_json::Map::new();
    obj.insert("name".into(), serde_json::json!(name));
    obj.insert("provider".into(), serde_json::json!(provider));
    obj.insert("api_key".into(), serde_json::json!(api_key));
    obj.insert("base_url".into(), serde_json::json!(base_url));
    obj.insert(
        "context_window".into(),
        serde_json::json!(require_yaml_positive_i64(entry, "context_window")?),
    );
    apply_optional_yaml_fields(&mut obj, entry);
    Ok(serde_json::Value::Object(obj))
}

pub async fn run_from_env() -> Result<(), String> {
    let cli = Cli::parse();
    run(cli.args, None, None).await
}

pub async fn run(
    args: AdminArgs,
    inherited_api_url: Option<&str>,
    inherited_profile: Option<&str>,
) -> Result<(), String> {
    // Resolve API URL: --api-url flag > ASTRA_API_URL env > config file > default
    let base = resolve_api_url(args.api_url.as_deref().or(inherited_api_url));
    let api = ThinClient::new(&base, None).map_err(|e| e.to_string())?;
    let profile = args
        .profile
        .clone()
        .or_else(|| inherited_profile.map(ToString::to_string));
    let command = args.command.unwrap_or(Command::Interactive);

    match command {
        Command::Interactive => run_interactive(&api, profile.as_deref()).await,
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
            let tokens = parse_auth_tokens(&body)?;
            save_profile_auth_tokens(profile.as_deref(), &username, &tokens)?;
            stdout_println!("logged in");
            Ok(())
        }
        Command::Register(args) => {
            let username = prompt_or("Username", args.username)?;
            let password = prompt_or("Password", args.password)?;
            let email = args
                .email
                .unwrap_or_else(|| format!("{username}@example.com"));
            let existing_token = load_credentials();
            let existing_profile_name = profile_name(profile.as_deref(), &existing_token);
            let existing_token = existing_token
                .profiles
                .get(&existing_profile_name)
                .and_then(bound_profile_access_token);
            let had_existing_token = existing_token.is_some();
            let body = api
                .post_path_json_text(
                    paths::ADMIN_REGISTER,
                    &serde_json::json!({
                        "username": username,
                        "email": email,
                        "password": password
                    }),
                    existing_token,
                )
                .await
                .map_err(map_thin_err)?;
            let value: serde_json::Value =
                serde_json::from_str(&body).map_err(|e| e.to_string())?;
            let tokens = parse_auth_tokens(&body)?;
            let is_admin = value
                .get("is_admin")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            if !is_admin {
                return Err("admin registration did not return an admin account".to_string());
            }
            save_profile_auth_tokens(profile.as_deref(), &username, &tokens)?;
            if had_existing_token {
                stdout_println!("registered and logged in (admin)");
            } else {
                stdout_println!("registered and logged in (initial admin)");
            }
            Ok(())
        }
        Command::Whoami => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api.get_auth_me_text(&token).await.map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Refresh => {
            let creds = load_credentials();
            let name = profile_name(profile.as_deref(), &creds);
            let saved_profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = saved_profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
            let body = api
                .post_auth_refresh_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
            let tokens = parse_auth_tokens(&body)?;
            save_refreshed_profile_tokens(profile.as_deref(), &tokens)?;
            stdout_println!("token refreshed");
            Ok(())
        }
        Command::Logout => {
            let creds = load_credentials();
            let name = profile_name(profile.as_deref(), &creds);
            let saved_profile = creds
                .profiles
                .get(&name)
                .cloned()
                .ok_or_else(|| format!("no profile '{name}'"))?;
            let refresh_token = saved_profile
                .refresh_token
                .ok_or_else(|| format!("profile '{name}' has no refresh token"))?;
            let body = api
                .post_auth_logout_json(&serde_json::json!({ "refresh_token": refresh_token }))
                .await
                .map_err(map_thin_err)?;
            let cli_profile = profile.clone();
            credential_store()
                .mutate(|creds| {
                    let name = profile_name(cli_profile.as_deref(), creds);
                    if let Some(entry) = creds.profiles.get_mut(&name) {
                        entry.access_token = None;
                        entry.refresh_token = None;
                    }
                })
                .map_err(|e| e.to_string())?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Init => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .post_bearer_path_empty_text(&token, paths::ADMIN_INIT)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Audit(args) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = session_runtime::load_server_model_catalog_json(&api, &token)
                .await
                .map_err(|error| error.to_string())?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Add(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let context_window = require_positive_context_window(args.context_window)?;
            let body = api
                .post_bearer_path_json_text(
                    &token,
                    paths::MODELS,
                    &serde_json::json!({
                        "name": args.name,
                        "provider": args.provider,
                        "api_key": args.api_key,
                        "context_window": context_window,
                        "base_url": args.base_url
                    }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Show(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_model_text(&token, &args.model_name)
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Delete(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .delete_bearer_path_text(&token, &paths::model(&args.model_name))
                .await
                .map_err(map_thin_err)?;
            if body.is_empty() {
                stdout_println!("deleted");
            } else {
                print_json_or_raw(&body);
            }
            Ok(())
        }
        Command::Model(ModelCmd::Check(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .post_bearer_path_empty_text(&token, &paths::model_check(&args.model_name))
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Model(ModelCmd::Load(args)) => {
            let content = fs::read_to_string(&args.path).map_err(|e| e.to_string())?;
            let doc: serde_yaml_ng::Value =
                serde_yaml_ng::from_str(&content).map_err(|e| e.to_string())?;
            let models = if let Some(seq) = doc.as_sequence() {
                seq
            } else {
                doc.get("models")
                    .and_then(serde_yaml_ng::Value::as_sequence)
                    .ok_or_else(|| "missing models list in yaml".to_string())?
            };

            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            for entry in models {
                let model_name = entry
                    .get("name")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .ok_or_else(|| "model.name missing".to_string())?;
                let provider = entry
                    .get("provider")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .ok_or_else(|| "model.provider missing".to_string())?;
                let api_key = entry
                    .get("api_key")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let base_url = entry
                    .get("base_url")
                    .and_then(serde_yaml_ng::Value::as_str)
                    .map(ToString::to_string);
                let metadata_only_update = args.update_existing && api_key.is_none();
                let needs_check = if metadata_only_update {
                    let upd =
                        build_model_update_payload(entry, provider, None, base_url.as_deref())?;
                    let body = api
                        .put_bearer_path_json_text(&token, &paths::model(model_name), &upd)
                        .await
                        .map_err(map_thin_err)?;
                    stdout_println!("re-synced existing model metadata: {model_name}");
                    print_model_load_server_result(&body, model_name);
                    true
                } else {
                    let api_key = api_key.ok_or_else(|| {
                        format!("model.api_key missing or empty for new model {model_name}")
                    })?;
                    let payload = build_model_create_payload(
                        entry,
                        model_name,
                        provider,
                        api_key,
                        base_url.as_deref(),
                    )?;
                    match api
                        .post_bearer_path_json_text(&token, paths::MODELS, &payload)
                        .await
                    {
                        Ok(body) => {
                            stdout_println!("loaded model: {model_name}");
                            print_model_load_server_result(&body, model_name);
                            true
                        }
                        Err(astra_thin_client::ThinClientError::Api { body, .. })
                            if body.contains("already exists") =>
                        {
                            if args.update_existing {
                                let upd = build_model_update_payload(
                                    entry,
                                    provider,
                                    Some(api_key),
                                    base_url.as_deref(),
                                )?;
                                let body = api
                                    .put_bearer_path_json_text(
                                        &token,
                                        &paths::model(model_name),
                                        &upd,
                                    )
                                    .await
                                    .map_err(map_thin_err)?;
                                stdout_println!("re-synced existing model: {model_name}");
                                print_model_load_server_result(&body, model_name);
                                true
                            } else {
                                stdout_println!(
                                    "skipped (already exists): {model_name} — use `astra admin model load {} --update-existing` to push YAML credentials and re-run connectivity",
                                    args.path
                                );
                                false
                            }
                        }
                        Err(e) => return Err(map_thin_err(e)),
                    }
                };
                if needs_check {
                    match api
                        .post_bearer_path_empty_text(&token, &paths::model_check(model_name))
                        .await
                    {
                        Ok(body) => {
                            let cap = serde_json::from_str::<serde_json::Value>(&body)
                                .ok()
                                .and_then(|v| {
                                    v.get("thinking_capability")?.as_str().map(String::from)
                                });
                            match cap.as_deref() {
                                Some("both") => {
                                    stdout_println!("  thinking: both (Normal/Thinking picker) ✓")
                                }
                                Some("effort_only") => {
                                    stdout_println!(
                                        "  thinking: effort_only (Low/High/Max effort) ✓"
                                    )
                                }
                                Some("native_only") => {
                                    stdout_println!("  thinking: native_only (always thinks)")
                                }
                                Some("none") => stdout_println!("  thinking: none"),
                                Some(other) => stdout_println!("  thinking: {other}"),
                                None => stdout_println!("  thinking: probe returned no capability"),
                            }
                        }
                        Err(e) => {
                            eprintln!("  thinking probe failed for {model_name}: {e}");
                        }
                    }
                }
            }
            Ok(())
        }
        Command::Model(ModelCmd::Update(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let mut payload = serde_json::Map::new();
            if let Some(key) = args.api_key {
                payload.insert("api_key".into(), serde_json::json!(key));
            }
            if let Some(url) = args.base_url {
                payload.insert("base_url".into(), serde_json::json!(url));
            }
            if let Some(active) = args.active {
                payload.insert("is_active".into(), serde_json::json!(active));
            }
            if let Some(quirks_str) = args.quirks {
                let quirks: serde_json::Value = serde_json::from_str(&quirks_str)
                    .map_err(|e| format!("invalid quirks JSON: {e}"))?;
                payload.insert("quirks".into(), quirks);
            }
            if payload.is_empty() {
                return Err(
                    "no fields to update (use --api-key, --base-url, --active, or --quirks)".into(),
                );
            }
            let body = api
                .put_bearer_path_json_text(
                    &token,
                    &paths::model(&args.model_name),
                    &serde_json::Value::Object(payload),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Token(TokenCmd::List(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_bearer_path_query_text(&token, &paths::skill_versions(&args.skill_name), &[])
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Prompt(PromptCmd::Optimize(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
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
        Command::Config(ConfigCmd::List) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_bearer_path_query_text(&token, paths::ADMIN_CONFIG, &[])
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Config(ConfigCmd::Get(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .get_bearer_path_query_text(&token, &paths::admin_config_key(&args.key), &[])
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Config(ConfigCmd::Set(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .put_bearer_path_json_text(
                    &token,
                    &paths::admin_config_key(&args.key),
                    &serde_json::json!({ "value": args.value }),
                )
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
        Command::Config(ConfigCmd::Unset(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = api
                .delete_bearer_path_text(&token, &paths::admin_config_key(&args.key))
                .await
                .map_err(map_thin_err)?;
            print_json_or_raw(&body);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> serde_yaml_ng::Value {
        serde_yaml_ng::from_str(s).unwrap()
    }

    // ── existing field propagation (regression guard) ───────────────────

    #[test]
    fn create_payload_includes_all_optional_fields() {
        let entry = yaml(
            r#"
            name: test-model
            provider: bedrock
            api_key: k
            description: "test description"
            context_window: 200000
            max_completion_tokens: 4096
            tags: [code, chat]
            supported_parameters: [tools]
            architecture: transformer
            pricing_prompt: 0.001
            pricing_completion: 0.002
            "#,
        );
        let payload = build_model_create_payload(
            &entry,
            "test-model",
            "bedrock",
            "k",
            Some("https://example.com"),
        )
        .unwrap();
        assert_eq!(payload["description"], "test description");
        assert_eq!(payload["context_window"], 200000);
        assert_eq!(payload["max_completion_tokens"], 4096);
        assert_eq!(payload["tags"], serde_json::json!(["code", "chat"]));
        assert_eq!(
            payload["supported_parameters"],
            serde_json::json!(["tools"])
        );
        assert_eq!(payload["architecture"], "transformer");
        assert!(payload["pricing"]["prompt"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn create_payload_routes_prompt_cache_capability_into_quirks() {
        let entry = yaml(
            r#"
            name: strict-openai-compatible
            provider: openai
            api_key: k
            context_window: 200000
            prompt_cache_capability:
              protocol: strict_history_match
              volatile_placement: current_user_only
              reuse_scope: conversation_turns
            "#,
        );

        let payload =
            build_model_create_payload(&entry, "strict-openai-compatible", "openai", "k", None)
                .unwrap();

        assert_eq!(
            payload["quirks"]["prompt_cache_capability"],
            serde_json::json!({
                "protocol": "strict_history_match",
                "volatile_placement": "current_user_only",
                "reuse_scope": "conversation_turns",
            })
        );
    }

    #[test]
    fn model_load_payload_requires_context_window() {
        let entry = yaml(
            r#"
            name: missing-window
            provider: openai
            api_key: k
            "#,
        );
        let error =
            build_model_create_payload(&entry, "missing-window", "openai", "k", None).unwrap_err();

        assert!(error.contains("model.context_window missing"));
    }

    #[test]
    fn model_create_payload_requires_non_empty_api_key() {
        let entry = yaml(
            r#"
            name: missing-key
            provider: openai
            context_window: 200000
            "#,
        );
        let error =
            build_model_create_payload(&entry, "missing-key", "openai", "", None).unwrap_err();

        assert!(error.contains("model.api_key missing or empty"));
    }

    #[test]
    fn model_add_context_window_must_be_positive() {
        assert_eq!(
            require_positive_context_window(1_000_000).unwrap(),
            1_000_000
        );
        let error = require_positive_context_window(0).unwrap_err();
        assert!(error.contains("positive token count"));
    }

    #[test]
    fn update_existing_payload_syncs_context_window_without_new_key() {
        let entry = yaml(
            r#"
            name: existing-model
            provider: openai
            context_window: 1000000
            "#,
        );
        let payload = build_model_update_payload(&entry, "openai", Some(""), None).unwrap();

        assert_eq!(payload["context_window"], 1000000);
        assert!(payload.get("api_key").is_none());
    }
}
