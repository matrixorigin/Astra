//! Guided first-run setup for a local or privately hosted Astra deployment.
//!
//! This wizard deliberately keeps secrets out of command-line arguments and
//! shell history. It configures only server-side admin/model state; local
//! infrastructure (Compose and embedding settings) is owned by `make
//! stack-setup`.

use astra_thin_client::{ThinClient, paths};
use crossterm::style::Stylize;
use inquire::{Confirm, Password, Select, Text};

use super::http_helpers::map_thin_err;
use crate::cli::auth_flow::{parse_auth_tokens, save_profile_auth_tokens};

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

async fn authenticate(api: &ThinClient, profile: Option<&str>) -> Result<(String, String), String> {
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
    save_profile_auth_tokens(profile, &username, &tokens)?;
    stdout_println!(
        "  {} administrator ready (profile saved locally)",
        "✓".green()
    );
    Ok((username, tokens.access_token))
}

async fn configure_model(api: &ThinClient, token: &str) -> Result<(), String> {
    let name = prompt_text("Model name", Some(DEFAULT_MODEL))?;
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
    let base_url = prompt_text("Model base URL", Some(default_url))?;
    let api_key = prompt_optional_secret(
        "Model API key (leave blank for an unauthenticated private endpoint)",
    )?;
    let context_window = prompt_context_window()?;

    stdout_println!("  {} registering model…", "→".cyan());
    let payload = serde_json::json!({
        "name": name.clone(),
        "provider": provider.clone(),
        "api_key": api_key.clone(),
        "base_url": base_url.clone(),
        "context_window": context_window,
    });
    let body = match api
        .post_bearer_path_json_text(token, paths::MODELS, &payload)
        .await
    {
        Ok(body) => body,
        Err(error)
            if error.to_string().contains("(409)")
                || error.to_string().contains("already exists") =>
        {
            stdout_println!("  Existing model found; updating it instead of creating a duplicate…");
            let update_payload = serde_json::json!({
                "provider": provider,
                "api_key": api_key,
                "base_url": base_url,
                "context_window": context_window,
            });
            api.put_bearer_path_json_text(token, &paths::model(&name), &update_payload)
                .await
                .map_err(map_thin_err)?
        }
        Err(error) => return Err(map_thin_err(error)),
    };
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let active = value
        .get("is_active")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    stdout_println!("  {} model saved", "✓".green());

    stdout_println!("  {} checking upstream connectivity…", "→".cyan());
    match api
        .post_bearer_path_empty_text(token, &paths::model_check(&name))
        .await
    {
        Ok(check_body) => {
            let check: serde_json::Value = serde_json::from_str(&check_body).unwrap_or_default();
            let check_active = check
                .get("is_active")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(active);
            if check_active {
                stdout_println!("  {} model is active and ready", "✓".green());
                Ok(())
            } else {
                Err(format!(
                    "model was saved but the connectivity check failed; run `astra admin model check {name}` after fixing the endpoint or key"
                ))
            }
        }
        Err(error) => Err(format!(
            "model was saved but the connectivity check failed: {error}\n  Run `astra admin model check {name}` to retry."
        )),
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
    let (_, token) = authenticate(api, profile).await?;

    if Confirm::new("Configure an LLM model now?")
        .with_default(true)
        .with_help_message("You can skip this and run `astra admin setup` again later.")
        .prompt()
        .map_err(|error| format!("setup cancelled: {error}"))?
    {
        stdout_println!("[3/3] Configuring model…");
        configure_model(api, &token).await?;
    } else {
        stdout_println!("[3/3] Model setup skipped (you can resume this wizard later).");
    }

    stdout_println!();
    stdout_println!("{}", "Setup complete".bold().green());
    stdout_println!("Try: astra chat -m \"Hello Astra\"");
    stdout_println!("More admin commands: astra admin --help");
    Ok(())
}
