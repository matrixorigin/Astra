use std::fs;

use crate::cli::auth_flow::{clear_profile_auth, parse_auth_tokens, save_profile_auth_tokens};
use crate::cli::cli_config::cli_utils::{
    bound_profile_access_token, get_profile_and_token, load_credentials, profile_name,
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
mod judgment_compare;
mod setup;

pub use cli_args::AdminArgs;
use cli_args::*;
use config::resolve_api_url;
use http_helpers::*;
use input::*;
use interactive::run_interactive;
use setup::run_setup;

/// Validate the opt-in default before any model or configuration mutation.
fn judgment_default_name(models: &[serde_yaml_ng::Value]) -> Result<Option<&str>, String> {
    let mut selected = None;
    for entry in models {
        let Some(value) = entry.get("judgment_default") else {
            continue;
        };
        let enabled = value
            .as_bool()
            .ok_or("model.judgment_default must be true or false")?;
        if enabled {
            let name = entry
                .get("name")
                .and_then(serde_yaml_ng::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or("judgment default model.name is missing")?;
            if selected.replace(name).is_some() {
                return Err("Only one model may set judgment_default: true".into());
            }
        }
    }
    Ok(selected)
}

async fn bind_judgment_default(api: &ThinClient, token: &str, name: &str) -> Result<(), String> {
    api.put_bearer_path_json_text(token, &paths::admin_config_key("judgment_model"), &serde_json::json!({"value":name}))
        .await.map_err(|e| format!("Could not confirm judgment model '{name}'. Check astra admin config get judgment_model before retrying: {}", map_thin_err(e)))?;
    stdout_println!(
        "Judgment model enabled: {name}. New request judgments use it; start a new session to refresh memory selection."
    );
    Ok(())
}

async fn load_models(
    api: &ThinClient,
    token: &str,
    models: &[serde_yaml_ng::Value],
    args: &ModelLoadArgs,
) -> Result<(), String> {
    let judgment_default = judgment_default_name(models)?;
    let prepared = models
        .iter()
        .map(|entry| {
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
            let payload = if metadata_only_update {
                build_model_update_payload(entry, provider, None, base_url.as_deref())?
            } else {
                build_model_create_payload(
                    entry,
                    model_name,
                    provider,
                    api_key.ok_or_else(|| {
                        format!("model.api_key missing or empty for new model {model_name}")
                    })?,
                    base_url.as_deref(),
                )?
            };
            let update = if args.update_existing && !metadata_only_update {
                let mut update = payload.clone();
                update
                    .as_object_mut()
                    .expect("prepared model payload is an object")
                    .remove("name");
                Some(update)
            } else {
                None
            };
            Ok((model_name, metadata_only_update, payload, update))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut judgment_available = false;
    for (model_name, metadata_only_update, payload, update) in prepared {
        let needs_check = if metadata_only_update {
            let body = api
                .put_bearer_path_json_text(token, &paths::model(model_name), &payload)
                .await
                .map_err(map_thin_err)?;
            stdout_println!("re-synced existing model metadata: {model_name}");
            print_model_load_server_result(&body, model_name);
            true
        } else {
            match api
                .post_bearer_path_json_text(token, paths::MODELS, &payload)
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
                    if let Some(update) = update.as_ref() {
                        let body = api
                            .put_bearer_path_json_text(token, &paths::model(model_name), update)
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
        if needs_check || judgment_default == Some(model_name) {
            if judgment_default == Some(model_name) {
                judgment_available = false;
            }
            match api
                .post_bearer_path_empty_text(token, &paths::model_check(model_name))
                .await
            {
                Ok(body) => {
                    let checked = serde_json::from_str::<serde_json::Value>(&body).ok();
                    if judgment_default == Some(model_name) {
                        judgment_available = checked
                            .as_ref()
                            .and_then(|v| v.get("is_active"))
                            .and_then(serde_json::Value::as_bool)
                            == Some(true);
                    }
                    let cap = checked
                        .and_then(|v| v.get("thinking_capability")?.as_str().map(String::from));
                    match cap.as_deref() {
                        Some("both") => {
                            stdout_println!("  thinking: both (Normal/Thinking picker) ✓")
                        }
                        Some("effort_only") => {
                            stdout_println!("  thinking: effort_only (Low/High/Max effort) ✓")
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
                    eprintln!("  model check failed for {model_name}: {e}");
                }
            }
        }
    }
    if let Some(name) = judgment_default {
        if judgment_available {
            bind_judgment_default(api, token, name).await?;
        } else {
            eprintln!(
                "warning: model catalog loaded, but optional judgment model '{name}' was not confirmed active; judgment binding unchanged. Check upstream availability and model configuration with `astra admin model check {name}`, then reload models."
            );
        }
    }
    Ok(())
}

/// Parse `POST /models` or `PUT /models/{name}` JSON and print `is_active` / `connectivity`.
fn print_model_load_server_result(body: &str, model_name: &str) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        stdout_println!("  (non-JSON response, len {} bytes)", body.len());
        return;
    };
    let active = value.get("is_active").and_then(serde_json::Value::as_bool);
    match active {
        Some(active) => stdout_println!("  is_active: {active}"),
        None => stdout_println!("  is_active: unknown"),
    }
    if let Some(c) = value
        .get("connectivity")
        .and_then(serde_json::Value::as_str)
    {
        stdout_println!("  connectivity: {c}");
    } else if active != Some(true) {
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
    if active != Some(true) {
        eprintln!(
            "  warning: model availability was not confirmed. Check upstream health, credentials and endpoint with `astra admin model check {model_name}`; reload models after resolving the cause."
        );
    }
}

fn take_yaml_field(entry: &mut serde_yaml_ng::Value, key: &str) -> Option<serde_yaml_ng::Value> {
    entry
        .as_mapping_mut()?
        .remove(serde_yaml_ng::Value::String(key.to_string()))
}

fn yaml_str(entry: &mut serde_yaml_ng::Value, key: &str) -> Result<Option<String>, String> {
    match take_yaml_field(entry, key) {
        None | Some(serde_yaml_ng::Value::Null) => Ok(None),
        Some(serde_yaml_ng::Value::String(value)) => Ok(Some(value)),
        _ => Err(format!("model.{key} must be a string")),
    }
}

fn yaml_i64(entry: &mut serde_yaml_ng::Value, key: &str) -> Result<Option<i64>, String> {
    take_yaml_field(entry, key)
        .map(|value| {
            value
                .as_i64()
                .ok_or_else(|| format!("model.{key} must be an integer"))
        })
        .transpose()
}

fn require_yaml_positive_i64(entry: &mut serde_yaml_ng::Value, key: &str) -> Result<i64, String> {
    match yaml_i64(entry, key)? {
        Some(value) if value > 0 => Ok(value),
        Some(_) => Err(format!("model.{key} must be a positive integer")),
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

fn yaml_f64(entry: &mut serde_yaml_ng::Value, key: &str) -> Result<Option<f64>, String> {
    take_yaml_field(entry, key)
        .map(|value| {
            value
                .as_f64()
                .ok_or_else(|| format!("model.{key} must be a number"))
        })
        .transpose()
}

fn yaml_str_vec(
    entry: &mut serde_yaml_ng::Value,
    key: &str,
) -> Result<Option<Vec<String>>, String> {
    take_yaml_field(entry, key)
        .map(|value| {
            let values = value
                .as_sequence()
                .ok_or_else(|| format!("model.{key} must be a string list"))?;
            values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(ToString::to_string)
                        .ok_or_else(|| format!("model.{key} must contain strings"))
                })
                .collect()
        })
        .transpose()
}

/// Merge optional YAML model fields into an existing JSON object in-place.
fn apply_optional_yaml_fields(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    entry: &mut serde_yaml_ng::Value,
) -> Result<(), String> {
    if let Some(v) = yaml_str(entry, "description")? {
        obj.insert("description".into(), serde_json::json!(v));
    }
    if let Some(v) = yaml_i64(entry, "max_completion_tokens")? {
        obj.insert("max_completion_tokens".into(), serde_json::json!(v));
    }
    for field in [
        "tags",
        "supported_parameters",
        "input_modalities",
        "output_modalities",
    ] {
        if let Some(value) = yaml_str_vec(entry, field)? {
            obj.insert(field.into(), serde_json::json!(value));
        }
    }
    if let Some(v) = yaml_str(entry, "architecture")? {
        obj.insert("architecture".into(), serde_json::json!(v));
    }
    let price_declared = entry.get("pricing_prompt").is_some()
        || entry.get("pricing_completion").is_some()
        || entry.get("pricing_currency").is_some()
        || entry.get("pricing_unit").is_some();
    let prompt_price = yaml_f64(entry, "pricing_prompt")?;
    let completion_price = yaml_f64(entry, "pricing_completion")?;
    if price_declared {
        let prompt = prompt_price.ok_or("pricing_prompt must be an explicit number")?;
        let completion = completion_price.ok_or("pricing_completion must be an explicit number")?;
        if yaml_str(entry, "pricing_currency")?.as_deref() != Some("USD")
            || yaml_str(entry, "pricing_unit")?.as_deref() != Some("per_token")
        {
            return Err(
                "pricing requires pricing_currency: USD and pricing_unit: per_token".into(),
            );
        }
        if !prompt.is_finite() || prompt < 0.0 || !completion.is_finite() || completion < 0.0 {
            return Err("pricing rates must be finite non-negative USD-per-token numbers".into());
        }
        obj.insert(
            "pricing".into(),
            serde_json::json!({
                "currency": "USD",
                "unit": "per_token",
                "prompt": prompt,
                "completion": completion,
            }),
        );
    }
    let remaining = entry
        .as_mapping()
        .ok_or("model definition must be a mapping")?;
    if !remaining.is_empty() {
        let fields = remaining
            .keys()
            .map(|key| key.as_str().unwrap_or("<non-string field>"))
            .collect::<Vec<_>>()
            .join(", ");
        let quirks = serde_json::to_value(&*entry)
            .map_err(|_| format!("invalid model quirks fields: {fields}"))?;
        serde_json::from_value::<astra_services::models::QuirksData>(quirks.clone())
            .map_err(|_| format!("invalid or unknown model quirks fields: {fields}"))?;
        obj.insert("quirks".into(), quirks);
    }
    Ok(())
}

fn build_model_update_payload(
    entry: &serde_yaml_ng::Value,
    provider: &str,
    api_key: Option<&str>,
    base_url: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut remaining = entry.clone();
    for key in ["name", "provider", "api_key", "base_url"] {
        yaml_str(&mut remaining, key)?;
    }
    if let Some(value) = take_yaml_field(&mut remaining, "judgment_default") {
        if !value.is_bool() {
            return Err("model.judgment_default must be true or false".into());
        }
    }
    let mut obj = serde_json::Map::new();
    obj.insert("provider".into(), serde_json::json!(provider));
    obj.insert(
        "context_window".into(),
        serde_json::json!(require_yaml_positive_i64(&mut remaining, "context_window")?),
    );
    if let Some(value) = api_key.filter(|value| !value.is_empty()) {
        obj.insert("api_key".into(), serde_json::json!(value));
    }
    if let Some(value) = base_url {
        obj.insert("base_url".into(), serde_json::json!(value));
    }
    apply_optional_yaml_fields(&mut obj, &mut remaining)?;
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
    let mut payload = build_model_update_payload(entry, provider, Some(api_key), base_url)?;
    payload["name"] = serde_json::json!(name);
    Ok(payload)
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
        Command::Setup => run_setup(&api, profile.as_deref()).await,
        Command::Login(args) => {
            let username = prompt_or("Username", args.username)?;
            let password = input::prompt_secret_or("Password", args.password)?;
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
            let password = input::prompt_secret_or("Password", args.password)?;
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
            if saved_profile
                .account_id
                .as_deref()
                .is_none_or(|id| id.trim().is_empty())
            {
                return Err("refresh requires a server-issued account_id; log in again".into());
            }
            session_runtime::try_refresh_token(&api, &name, &saved_profile, None)
                .await
                .map_err(|error| format!("refresh failed: {error:?}"))?;
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
            clear_profile_auth(profile.as_deref())?;
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
        Command::Model(ModelCmd::Compare(args)) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            judgment_compare::run(&api, &token, &args).await
        }
        Command::Model(ModelCmd::List) => {
            let (_, _, _, token) = get_profile_and_token(profile.as_deref())?;
            let body = session_runtime::load_server_model_catalog_json(
                &api,
                &token,
                astra_core::model_wire::purpose::ModelCatalogPurpose::All,
            )
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
            load_models(&api, &token, models, &args).await
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
    fn judgment_default_rejects_invalid_or_multiple_selections() {
        for source in [
            "[{name: jev, judgment_default: 'true'}]",
            "[{judgment_default: true}]",
            "[{name: a, judgment_default: true}, {name: b, judgment_default: true}]",
        ] {
            let value = yaml(source);
            assert!(judgment_default_name(value.as_sequence().unwrap()).is_err());
        }
    }

    #[tokio::test]
    async fn model_load_rejects_invalid_later_entry_before_any_request() {
        for invalid in [
            "fallback_chain: [other]",
            "context_wid: 1000",
            "fixed_temperature: invalid",
        ] {
            for update_existing in [false, true] {
                let server = wiremock::MockServer::start().await;
                let doc = yaml(&format!(
                    "- name: first\n  provider: openai\n  api_key: test-key\n  context_window: 1000\n- name: second\n  provider: openai\n  api_key: test-key\n  context_window: 1000\n  {invalid}\n"
                ));
                let args = ModelLoadArgs {
                    path: "unused.yaml".into(),
                    update_existing,
                };
                let api = ThinClient::new(&server.uri(), None).unwrap();
                let result =
                    load_models(&api, "fake-token", doc.as_sequence().unwrap(), &args).await;
                assert!(result.is_err(), "{invalid}: {result:?}");
                assert!(server.received_requests().await.unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn model_load_binds_default_only_after_successful_checks() {
        use wiremock::matchers::{body_json, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        for (default, check_status, active, binding_status, expected_ok) in [
            (true, 200, Some(true), 200, true),
            (false, 200, Some(true), 200, true),
            (true, 503, Some(true), 200, true),
            (true, 200, Some(true), 400, false),
            (true, 200, Some(false), 200, true),
            (true, 200, None, 200, true),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("PUT"))
                .and(path("/models/jev"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"is_active":true,"context_window":1000})),
                )
                .expect(1)
                .mount(&server)
                .await;
            for model in ["primary", "last"] {
                Mock::given(method("PUT"))
                    .and(path(format!("/models/{model}")))
                    .respond_with(
                        ResponseTemplate::new(200).set_body_json(
                            serde_json::json!({"is_active":true,"context_window":1000}),
                        ),
                    )
                    .expect(1)
                    .mount(&server)
                    .await;
                Mock::given(method("POST"))
                    .and(path(format!("/models/{model}/check")))
                    .respond_with(ResponseTemplate::new(200).set_body_json(
                        serde_json::json!({"is_active":true,"thinking_capability":"both"}),
                    ))
                    .expect(1)
                    .mount(&server)
                    .await;
            }
            Mock::given(method("POST"))
                .and(path("/models/jev/check"))
                .respond_with(
                    ResponseTemplate::new(check_status)
                        .set_body_json(serde_json::json!({"is_active":active})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/admin/config/judgment_model"))
                .and(body_json(serde_json::json!({"value":"jev"})))
                .respond_with(
                    ResponseTemplate::new(binding_status)
                        .set_body_json(serde_json::json!({"value":"jev"})),
                )
                .expect(if default && check_status == 200 && active == Some(true) {
                    1
                } else {
                    0
                })
                .mount(&server)
                .await;
            // No credentials or profiles: metadata-only load uses the stored server key.
            let doc = yaml(&format!(
                "[{{name: jev, provider: typesafe, context_window: 1000, judgment_default: {default}}}, {{name: primary, provider: openai, context_window: 1000}}, {{name: last, provider: openai, context_window: 1000}}]"
            ));
            let args = ModelLoadArgs {
                path: "unused.yaml".into(),
                update_existing: true,
            };
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let result = load_models(&api, "fake-token", doc.as_sequence().unwrap(), &args).await;
            assert_eq!(result.is_ok(), expected_ok, "{result:?}");
            let requests = server.received_requests().await.unwrap();
            if default && check_status == 200 && active == Some(true) {
                assert_eq!(
                    requests.last().unwrap().url.path(),
                    "/admin/config/judgment_model"
                );
            }
        }
    }

    #[tokio::test]
    async fn skipped_default_still_requires_a_successful_health_check() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        for active in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/models"))
                .respond_with(ResponseTemplate::new(400).set_body_string("already exists"))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("POST"))
                .and(path("/models/jev/check"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"is_active":active})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path("/admin/config/judgment_model"))
                .respond_with(ResponseTemplate::new(200))
                .expect(if active { 1 } else { 0 })
                .mount(&server)
                .await;
            let doc = yaml(
                "[{name: jev, provider: typesafe, api_key: test-only-placeholder, context_window: 1000, judgment_default: true}]",
            );
            let api = ThinClient::new(&server.uri(), None).unwrap();
            let args = ModelLoadArgs {
                path: "unused.yaml".into(),
                update_existing: false,
            };
            assert!(
                load_models(&api, "fake-token", doc.as_sequence().unwrap(), &args)
                    .await
                    .is_ok()
            );
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                2 + usize::from(active)
            );
        }
    }

    #[tokio::test]
    async fn invalid_judgment_default_makes_no_requests() {
        let server = wiremock::MockServer::start().await;
        let api = ThinClient::new(&server.uri(), None).unwrap();
        let doc = yaml("[{name: a, judgment_default: true}, {name: b, judgment_default: true}]");
        let args = ModelLoadArgs {
            path: "unused.yaml".into(),
            update_existing: true,
        };
        assert!(
            load_models(&api, "fake-token", doc.as_sequence().unwrap(), &args)
                .await
                .is_err()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

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
            input_modalities: [text, image]
            output_modalities: [text]
            fixed_temperature: 0.7
            request_headers: {X-Provider-Option: enabled}
            architecture: transformer
            pricing_currency: USD
            pricing_unit: per_token
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
        assert_eq!(
            payload["input_modalities"],
            serde_json::json!(["text", "image"])
        );
        assert_eq!(payload["output_modalities"], serde_json::json!(["text"]));
        assert_eq!(payload["quirks"]["fixed_temperature"], 0.7);
        assert_eq!(
            payload["quirks"]["request_headers"]["X-Provider-Option"],
            "enabled"
        );
        assert_eq!(payload["architecture"], "transformer");
        assert!(payload["pricing"]["prompt"].as_f64().unwrap() > 0.0);
        assert_eq!(payload["pricing"]["currency"], "USD");
    }

    #[test]
    fn model_price_import_never_invents_missing_rates_or_currency() {
        for fields in [
            "pricing_prompt: 0",
            "pricing_prompt: 0\npricing_completion: 0",
            "pricing_currency: CNY\npricing_unit: per_token\npricing_prompt: 0\npricing_completion: 0",
            "pricing_currency: USD\npricing_unit: per_token\npricing_prompt: 0",
        ] {
            let entry = yaml(&format!("context_window: 1000\n{fields}"));
            assert!(
                build_model_create_payload(&entry, "priced", "mock", "key", None).is_err(),
                "{fields}"
            );
        }
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
