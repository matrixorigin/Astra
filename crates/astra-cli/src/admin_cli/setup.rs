//! Guided first-run setup for a local or privately hosted Astra deployment.
//!
//! This wizard deliberately keeps secrets out of command-line arguments and
//! shell history. It configures only server-side admin/model state; local
//! infrastructure (Compose and embedding settings) is owned by `make
//! stack-setup`.

use astra_thin_client::{ThinClient, ThinClientError, paths};
use crossterm::style::Stylize;
use inquire::{Confirm, Password, Select, Text};

use super::http_helpers::map_thin_err;
use crate::cli::auth_flow::{parse_auth_tokens, save_profile_auth_tokens};
use crate::cli::cli_config::cli_utils::get_profile_and_token;
use crate::cli::session::session_runtime;

const DEFAULT_MODEL: &str = "gpt-4o-mini";
const DEFAULT_PROVIDER: &str = "openai";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_CONTEXT_WINDOW: i32 = 128_000;

fn prompt_text(message: &str, default: Option<&str>) -> Result<String, String> {
    let prompt = Text::new(message);
    let value = match default {
        Some(default) => prompt.with_default(default).prompt(),
        None => prompt.prompt(),
    }
    .map_err(|error| format!("setup cancelled: {error}"))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        Err(format!("{message} cannot be empty"))
    } else {
        Ok(value)
    }
}

fn prompt_secret(message: &str) -> Result<String, String> {
    Password::new(message)
        .without_confirmation()
        .prompt()
        .map_err(|error| format!("setup cancelled: {error}"))
        .and_then(|value| {
            let value = value.trim().to_string();
            if value.is_empty() {
                Err(format!("{message} cannot be empty"))
            } else {
                Ok(value)
            }
        })
}

fn prompt_optional_secret(message: &str) -> Result<String, String> {
    Password::new(message)
        .without_confirmation()
        .prompt()
        .map_err(|error| format!("setup cancelled: {error}"))
        .map(|value| value.trim().to_string())
}

fn prompt_password() -> Result<String, String> {
    loop {
        let password = prompt_secret("Administrator password")?;
        let confirmation = prompt_secret("Confirm administrator password")?;
        if password == confirmation {
            return Ok(password);
        }
        eprintln!("Passwords do not match; please try again.");
    }
}

fn prompt_context_window() -> Result<i32, String> {
    loop {
        let raw = prompt_text(
            "Context window (tokens)",
            Some(&DEFAULT_CONTEXT_WINDOW.to_string()),
        )?;
        match raw.parse::<i32>() {
            Ok(value) if value > 0 => return Ok(value),
            _ => eprintln!("Enter a positive whole number, for example 128000."),
        }
    }
}

async fn verify_administrator(api: &ThinClient, token: &str) -> Result<(), String> {
    match api
        .get_bearer_path_query_text(token, paths::ADMIN_CONFIG, &[])
        .await
    {
        Ok(_) => Ok(()),
        Err(ThinClientError::Api { status, .. }) if matches!(status.as_u16(), 401 | 403) => Err(
            "the authenticated account is not an administrator; no CLI profile was saved"
                .to_string(),
        ),
        Err(error) => Err(format!(
            "could not verify administrator access: {}",
            map_thin_err(error)
        )),
    }
}

async fn authenticate(api: &ThinClient, profile: Option<&str>) -> Result<(String, String), String> {
    if let Ok((_, profile_name, saved_profile, token)) = get_profile_and_token(profile) {
        let username = saved_profile
            .username
            .unwrap_or_else(|| profile_name.clone());
        stdout_println!(
            "  {} checking saved profile '{}'…",
            "→".cyan(),
            profile_name
        );
        if verify_administrator(api, &token).await.is_ok() {
            if Confirm::new(&format!("Use saved administrator '{username}'?"))
                .with_default(true)
                .prompt()
                .map_err(|error| format!("setup cancelled: {error}"))?
            {
                stdout_println!("  {} saved administrator is valid", "✓".green());
                return Ok((username, token));
            }
        } else {
            eprintln!("  Saved profile is missing, expired, or not an administrator.");
        }
    }

    let mode = Select::new(
        "Administrator account",
        vec![
            "Fresh database — create the initial administrator".to_string(),
            "Existing database — sign in as an administrator".to_string(),
        ],
    )
    .with_help_message("Use the second option when this server already has an admin.")
    .prompt()
    .map_err(|error| format!("setup cancelled: {error}"))?;

    let username = prompt_text("Administrator username", Some("admin"))?;
    let email = if mode.starts_with("Fresh") {
        Some(prompt_text(
            "Administrator email",
            Some(&format!("{username}@example.com")),
        )?)
    } else {
        None
    };
    let password = match mode.starts_with("Fresh") {
        true => prompt_password()?,
        false => prompt_secret("Administrator password")?,
    };

    stdout_println!("  {} authenticating administrator…", "→".cyan());
    let body = if mode.starts_with("Fresh") {
        let email = email.ok_or_else(|| "administrator email was not collected".to_string())?;
        api.post_path_json_text(
            paths::ADMIN_REGISTER,
            &serde_json::json!({
                "username": username,
                "email": email,
                "password": password,
            }),
            None,
        )
        .await
        .map_err(map_thin_err)?
    } else {
        api.post_auth_login_json(&serde_json::json!({
            "username": username,
            "password": password,
        }))
        .await
        .map_err(map_thin_err)?
    };
    if mode.starts_with("Fresh") {
        let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        if !value
            .get("is_admin")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            return Err(
                "server did not confirm the new account as an administrator; no CLI profile was saved"
                    .to_string(),
            );
        }
    }
    let tokens = parse_auth_tokens(&body)?;
    verify_administrator(api, &tokens.access_token).await?;
    save_profile_auth_tokens(profile, &username, &tokens)?;
    stdout_println!(
        "  {} administrator ready (profile saved locally)",
        "✓".green()
    );
    Ok((username, tokens.access_token))
}

async fn authenticate_with_retry(
    api: &ThinClient,
    profile: Option<&str>,
) -> Result<(String, String), String> {
    loop {
        match authenticate(api, profile).await {
            Ok(auth) => return Ok(auth),
            Err(error) => {
                eprintln!("Administrator setup failed: {error}");
                if !Confirm::new("Choose the account mode and try again?")
                    .with_default(true)
                    .with_help_message(
                        "Choose existing database when initial admin bootstrap already exists.",
                    )
                    .prompt()
                    .map_err(|prompt_error| format!("setup cancelled: {prompt_error}"))?
                {
                    return Err(error);
                }
            }
        }
    }
}

async fn active_model_names(api: &ThinClient, token: &str) -> Result<Vec<String>, String> {
    let (items, _) = session_runtime::load_server_model_catalog(api, token)
        .await
        .map_err(|error| format!("could not inspect current model catalog: {error}"))?;
    let mut names = items
        .into_iter()
        .filter(|item| item.is_active)
        .map(|item| item.name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    names.sort();
    names.dedup();
    Ok(names)
}

fn validate_model_probe_response(name: &str, body: &str) -> Result<(), String> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|error| format!("model check returned invalid JSON: {error}"))?;
    let active = value
        .get("is_active")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| "model check response did not include boolean is_active".to_string())?;
    if active {
        return Ok(());
    }
    let connectivity = value
        .get("connectivity")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("the provider did not report a reason");
    Err(format!(
        "model '{name}' is inactive after its connectivity check: {connectivity}"
    ))
}

async fn probe_model(api: &ThinClient, token: &str, name: &str) -> Result<(), String> {
    stdout_println!("  {} checking upstream connectivity…", "→".cyan());
    let body = api
        .post_bearer_path_empty_text(token, &paths::model_check(name))
        .await
        .map_err(|error| format!("model connectivity request failed: {}", map_thin_err(error)))?;
    validate_model_probe_response(name, &body)?;
    stdout_println!("  {} model '{name}' is active and ready", "✓".green());
    Ok(())
}

async fn configure_model_once(api: &ThinClient, token: &str) -> Result<(), String> {
    let provider = Select::new(
        "Model provider",
        vec![DEFAULT_PROVIDER.to_string(), "anthropic".to_string()],
    )
    .with_help_message("The openai adapter also works with most OpenAI-compatible gateways.")
    .prompt()
    .map_err(|error| format!("setup cancelled: {error}"))?;
    let default_url = if provider == DEFAULT_PROVIDER {
        DEFAULT_BASE_URL
    } else {
        "https://api.anthropic.com"
    };
    let (base_url, official_endpoint) = loop {
        let value = prompt_text("Model base URL", Some(default_url))?;
        match url::Url::parse(&value) {
            Ok(parsed)
                if matches!(parsed.scheme(), "http" | "https") && parsed.host_str().is_some() =>
            {
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    eprintln!("Put credentials in the API key field, not the model base URL.");
                    continue;
                }
                if parsed.query().is_some() || parsed.fragment().is_some() {
                    eprintln!("The model base URL cannot contain a query string or fragment.");
                    continue;
                }
                let host = parsed.host_str().unwrap_or_default();
                let official = (provider == "openai" && host == "api.openai.com")
                    || (provider == "anthropic" && host == "api.anthropic.com");
                break (value, official);
            }
            _ => eprintln!("Enter an absolute http:// or https:// model endpoint."),
        }
    };
    let default_model =
        (provider == DEFAULT_PROVIDER && official_endpoint).then_some(DEFAULT_MODEL);
    let name = prompt_text("Model name", default_model)?;
    let api_key = loop {
        let value = prompt_optional_secret(
            "Model API key (leave blank for an unauthenticated private endpoint)",
        )?;
        if official_endpoint && value.is_empty() {
            eprintln!("The selected hosted endpoint requires an API key.");
            continue;
        }
        break value;
    };
    let context_window = prompt_context_window()?;

    stdout_println!("  {} registering model…", "→".cyan());
    let payload = serde_json::json!({
        "name": name.clone(),
        "provider": provider.clone(),
        "api_key": api_key.clone(),
        "base_url": base_url.clone(),
        "context_window": context_window,
    });
    match api
        .post_bearer_path_json_text(token, paths::MODELS, &payload)
        .await
    {
        Ok(_) => {}
        Err(ThinClientError::Api { status, body })
            if status.as_u16() == 409 || body.contains("already exists") =>
        {
            stdout_println!("  Existing model found; updating it instead of creating a duplicate…");
            let mut update_payload = serde_json::json!({
                "provider": provider,
                "base_url": base_url,
                "context_window": context_window,
            });
            if !api_key.is_empty() {
                update_payload["api_key"] = serde_json::json!(api_key);
            }
            api.put_bearer_path_json_text(token, &paths::model(&name), &update_payload)
                .await
                .map_err(map_thin_err)?;
        }
        Err(error) => return Err(map_thin_err(error)),
    }
    stdout_println!("  {} model saved", "✓".green());
    probe_model(api, token, &name).await.map_err(|error| {
        format!("model was saved but is not ready: {error}\n  Run `astra admin model check {name}` to retry.")
    })
}

async fn configure_model(api: &ThinClient, token: &str) -> Result<(), String> {
    loop {
        match configure_model_once(api, token).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("Model setup failed: {error}");
                if !Confirm::new("Edit the model settings and retry?")
                    .with_default(true)
                    .prompt()
                    .map_err(|prompt_error| format!("setup cancelled: {prompt_error}"))?
                {
                    return Err(error);
                }
            }
        }
    }
}

pub(crate) async fn run_setup(api: &ThinClient, profile: Option<&str>) -> Result<(), String> {
    stdout_println!();
    stdout_println!("{}", "Astra guided setup".bold().cyan());
    stdout_println!("Connect an administrator and one LLM model in a few safe steps.");
    stdout_println!("Secrets are hidden while typing and are never printed by this wizard.");
    stdout_println!();

    stdout_println!("[1/3] Checking the Astra API…");
    api.get_health_text().await.map_err(|error| {
        format!("API is not reachable: {error}\n  Check the server URL and try again.")
    })?;
    stdout_println!("  {} API server reachable", "✓".green());

    stdout_println!("[2/3] Configuring administrator…");
    let (_, token) = authenticate_with_retry(api, profile).await?;

    let active_models = active_model_names(api, &token).await?;
    let existing_model = if active_models.is_empty() {
        None
    } else {
        stdout_println!("  Active models: {}", active_models.join(", "));
        let keep_existing = Confirm::new("Keep and verify an active model configuration?")
            .with_default(true)
            .with_help_message("A live provider check runs before setup reports success.")
            .prompt()
            .map_err(|error| format!("setup cancelled: {error}"))?;
        if keep_existing {
            if active_models.len() == 1 {
                active_models.first().cloned()
            } else {
                Some(
                    Select::new("Model to verify", active_models)
                        .prompt()
                        .map_err(|error| format!("setup cancelled: {error}"))?,
                )
            }
        } else {
            None
        }
    };

    stdout_println!("[3/3] Verifying model access…");
    if let Some(name) = existing_model {
        if let Err(error) = probe_model(api, &token, &name).await {
            eprintln!("Existing model is not ready: {error}");
            stdout_println!("  Reconfigure a model to complete setup.");
            configure_model(api, &token).await?;
        }
    } else {
        configure_model(api, &token).await?;
    }

    stdout_println!();
    stdout_println!("{}", "Setup complete".bold().green());
    stdout_println!("Try: astra chat -m \"Hello Astra\"");
    stdout_println!("More admin commands: astra admin --help");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn model_probe_requires_explicit_active_state() {
        let error = validate_model_probe_response("demo", r#"{"connectivity":"ok"}"#)
            .expect_err("missing is_active must not inherit stale model state");
        assert!(error.contains("is_active"));

        let error = validate_model_probe_response(
            "demo",
            r#"{"is_active":false,"connectivity":"invalid key"}"#,
        )
        .expect_err("inactive model must fail setup");
        assert!(error.contains("invalid key"));

        validate_model_probe_response("demo", r#"{"is_active":true,"connectivity":"ok"}"#)
            .expect("an explicitly active model is ready");
    }

    #[tokio::test]
    async fn administrator_verification_rejects_non_admin_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(paths::ADMIN_CONFIG))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        let api = ThinClient::new(&server.uri(), None).expect("test client");

        let error = verify_administrator(&api, "ordinary-user-token")
            .await
            .expect_err("ordinary users must not be saved as administrators");
        assert!(error.contains("not an administrator"), "{error}");
    }
}
