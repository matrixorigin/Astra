//! Browser UX for provider linking. Astra Server owns the OAuth verifier and
//! token exchange; the CLI holds only a short-lived callback and Astra bearer.
use crate::cli::cli_config::{cli_args::ModelConnectArgs, cli_utils::map_thin_err};
use astra_thin_client::ThinClient;
use axum::{
    Router,
    extract::{RawQuery, State},
    http::StatusCode,
    response::Html,
    routing::get,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub const CONNECTIONS: &str = "/me/model-connections";
const AUTHORIZATIONS: &str = "/me/model-connections/chatgpt/authorizations";

pub fn connection_path(id: &str, action: &str) -> Result<String, String> {
    let id = uuid::Uuid::parse_str(id).map_err(|_| "Use the exact model connection UUID")?;
    Ok(format!("{CONNECTIONS}/{id}/{action}"))
}

#[derive(Clone)]
struct Callback {
    state: String,
    sender: Arc<Mutex<Option<oneshot::Sender<Value>>>>,
}
async fn callback(
    State(callback): State<Callback>,
    RawQuery(raw): RawQuery,
) -> (StatusCode, [(String, String); 2], Html<&'static str>) {
    let headers = [
        ("cache-control".into(), "no-store".into()),
        ("referrer-policy".into(), "no-referrer".into()),
    ];
    let mut values = HashMap::new();
    let mut duplicate = false;
    for (key, value) in url::form_urlencoded::parse(raw.as_deref().unwrap_or("").as_bytes()) {
        duplicate |= values
            .insert(key.into_owned(), value.into_owned())
            .is_some();
    }
    if duplicate
        || values.get("state") != Some(&callback.state)
        || values.contains_key("code") == values.contains_key("error")
    {
        return (
            StatusCode::BAD_REQUEST,
            headers,
            Html("Invalid authorization callback."),
        );
    }
    let value = json!({"state":values.get("state"),"code":values.get("code"),"client_id":values.get("client_id"),"error":values.get("error")});
    let sender = callback
        .sender
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(sender) = sender {
        let _ = sender.send(value);
    }
    (
        StatusCode::OK,
        headers,
        Html("Authorization received. Return to the Astra terminal."),
    )
}

pub fn validate_authorization_url(value: &str, redirect: &str, state: &str) -> Result<(), String> {
    let url = url::Url::parse(value).map_err(|_| "Invalid ChatGPT authorization URL")?;
    if url.scheme() != "https"
        || url.host_str() != Some("auth.openai.com")
        || url.port_or_known_default() != Some(443)
        || url.path() != "/api/accounts/authorize"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err("Server returned an untrusted ChatGPT authorization origin".into());
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    for (key, expected) in [
        ("redirect_uri", redirect),
        ("state", state),
        ("code_challenge_method", "S256"),
    ] {
        let found: Vec<_> = pairs.iter().filter(|(name, _)| name == key).collect();
        if found.len() != 1 || found[0].1 != expected {
            return Err("Server returned an invalid ChatGPT authorization binding".into());
        }
    }
    Ok(())
}

pub async fn connect(
    api: &ThinClient,
    token: &str,
    args: ModelConnectArgs,
    profile: Option<&str>,
) -> Result<Value, String> {
    if args.provider != "chatgpt" {
        return Err("Unsupported subscription provider".into());
    }
    let base = api.base_url();
    if base.scheme() != "https"
        && !(base.scheme() == "http"
            && matches!(
                base.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            ))
    {
        return Err("Remote model credential linking requires HTTPS".into());
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| "Cannot listen for the ChatGPT callback")?;
    let redirect = format!(
        "http://127.0.0.1:{}/auth/callback",
        listener
            .local_addr()
            .map_err(|_| "Cannot read callback port")?
            .port()
    );
    let body = api
        .post_bearer_path_json_text(
            token,
            AUTHORIZATIONS,
            &json!({"redirect_uri":redirect,"label":args.label,"connection_id":args.connection,"retry_authorization_id":args.retry_authorization_id}),
        )
        .await
        .map_err(map_thin_err)?;
    let started: Value =
        serde_json::from_str(&body).map_err(|_| "Invalid authorization response")?;
    let id = started["authorization_id"]
        .as_str()
        .ok_or("Authorization ID missing")?;
    uuid::Uuid::parse_str(id).map_err(|_| "Invalid authorization ID")?;
    let state = started["state"]
        .as_str()
        .ok_or("Authorization state missing")?
        .to_owned();
    let url = started["authorization_url"]
        .as_str()
        .ok_or("Authorization URL missing")?;
    validate_authorization_url(url, &redirect, &state)?;
    let (sender, receiver) = oneshot::channel();
    let router = Router::new()
        .route("/auth/callback", get(callback))
        .with_state(Callback {
            state,
            sender: Arc::new(Mutex::new(Some(sender))),
        });
    let stop = CancellationToken::new();
    let stop_server = stop.clone();
    let mut server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(stop_server.cancelled_owned())
            .await
    });
    if crate::cli::auth_flow::browser_launcher::launch_login_url(url.to_owned())
        .await
        .is_err()
    {
        // No ID/access/refresh token is included in this new-registration URL.
        eprintln!("Open this OpenAI authorization page in your browser:\n{url}");
    }
    eprintln!(
        "Waiting for ChatGPT authorization; credentials will be stored encrypted on the selected Astra Server."
    );
    let result = tokio::select! {
        result=receiver=>result.map_err(|_| "Authorization callback closed".to_owned()),
        _=tokio::time::sleep(Duration::from_secs(900))=>Err("ChatGPT authorization timed out".into()),
        _=tokio::signal::ctrl_c()=>Err("ChatGPT authorization cancelled".into()),
    };
    stop.cancel();
    if tokio::time::timeout(Duration::from_secs(2), &mut server)
        .await
        .is_err()
    {
        server.abort();
        let _ = server.await;
    }
    let complete = match result {
        Ok(value) => value,
        Err(error) => {
            let _ = api
                .delete_bearer_path_text(token, &format!("{AUTHORIZATIONS}/{id}"))
                .await;
            return Err(error);
        }
    };
    // Browser consent can outlive an Astra access token. Refresh through the
    // existing Astra session owner before submitting the one-time callback;
    // provider errors never drive Astra login refresh.
    let token = crate::cli::session::session_runtime::fresh_access_token(api, profile)
        .await
        .ok_or("Astra session expired during authorization; log in and reconnect")?;
    let body = api
        .post_bearer_path_json_text(&token, &format!("{AUTHORIZATIONS}/{id}/complete"), &complete)
        .await
        .map_err(|failure| format!("{}; inspect `astra model connections`. For a failed code exchange retry with `astra model connect chatgpt --retry {id}`",map_thin_err(failure)))?;
    serde_json::from_str(&body).map_err(|_| "Invalid model connection response".into())
}

pub async fn select_connection(
    api: &ThinClient,
    token: &str,
    provided: Option<String>,
    interactive: bool,
    profile: Option<&str>,
) -> Result<String, String> {
    if let Some(id) = provided {
        uuid::Uuid::parse_str(&id).map_err(|_| "Use the exact connection UUID")?;
        return Ok(id);
    }
    if !interactive {
        return Err(
            "--connection is required for subscription models in non-interactive mode".into(),
        );
    }
    let body = api
        .get_bearer_path_query_text(token, CONNECTIONS, &[])
        .await
        .map_err(map_thin_err)?;
    let value: Value = serde_json::from_str(&body).map_err(|_| "Invalid connection list")?;
    let items = value["items"].as_array().ok_or("Invalid connection list")?;
    let mut labels: Vec<String> = items
        .iter()
        .filter(|item| item["status"] == "connected")
        .map(|item| {
            format!(
                "{} · {}",
                item["label"].as_str().unwrap_or("ChatGPT"),
                item["connection_id"].as_str().unwrap_or("")
            )
        })
        .collect();
    labels.push("Link another ChatGPT account".into());
    let choice = inquire::Select::new("ChatGPT connection:", labels)
        .prompt()
        .map_err(|e| e.to_string())?;
    if choice == "Link another ChatGPT account" {
        let value = connect(
            api,
            token,
            ModelConnectArgs {
                provider: "chatgpt".into(),
                label: None,
                connection: None,
                retry_authorization_id: None,
            },
            profile,
        )
        .await?;
        return value["connection"]["connection_id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "Connection ID missing".into());
    }
    choice
        .rsplit_once(" · ")
        .map(|(_, id)| id.to_owned())
        .ok_or_else(|| "Invalid connection selection".into())
}

pub async fn add_model(
    api: &ThinClient,
    token: &str,
    args: crate::cli::cli_config::cli_args::ModelAddArgs,
    interactive: bool,
    profile: Option<&str>,
) -> Result<String, String> {
    if args.api_key_stdin || args.base_url.is_some() {
        return Err(
            "ChatGPT models use a linked subscription connection and fixed OpenAI endpoint".into(),
        );
    }
    let wizard = interactive
        && (args.name.is_none() || args.model.is_none() || args.connection_id.is_none());
    let id = select_connection(api, token, args.connection_id, interactive, profile).await?;
    let token = crate::cli::session::session_runtime::fresh_access_token(api, profile)
        .await
        .ok_or("Unable to refresh Astra session; run `astra login`")?;
    let catalog = api
        .get_bearer_path_query_text(&token, &connection_path(&id, "models")?, &[])
        .await
        .map_err(map_thin_err)?;
    let catalog: Value =
        serde_json::from_str(&catalog).map_err(|_| "Invalid subscription model catalog")?;
    let model = match args.model {
        Some(model) => model,
        None if interactive => {
            let labels: Vec<String> = catalog["items"]
                .as_array()
                .ok_or("Invalid subscription catalog")?
                .iter()
                .filter_map(|item| item["slug"].as_str().map(str::to_owned))
                .collect();
            if labels.is_empty() {
                return Err("This ChatGPT account exposes no selectable models".into());
            }
            inquire::Select::new("Subscription model:", labels)
                .prompt()
                .map_err(|e| e.to_string())?
        }
        _ => return Err("--model is required in non-interactive mode".into()),
    };
    let name = match args.name {
        Some(name) => name,
        None if interactive => inquire::Text::new("Astra model alias:")
            .with_default(&model)
            .prompt()
            .map_err(|e| e.to_string())?,
        _ => return Err("A model alias is required in non-interactive mode".into()),
    };
    let context = if wizard {
        inquire::CustomType::<i32>::new("Context window (tokens):")
            .with_default(args.context_window)
            .prompt()
            .map_err(|e| e.to_string())?
    } else {
        args.context_window
    };
    let default = if wizard && !args.default {
        inquire::Confirm::new("Use as your default model?")
            .with_default(false)
            .prompt()
            .map_err(|e| e.to_string())?
    } else {
        args.default
    };
    api.post_bearer_path_json_text(&token,"/me/models",&json!({"name":name,"model":model,"provider":"chatgpt","connection_id":id,"context_window":context,"is_default":default})).await.map_err(map_thin_err)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_link_must_match_origin_state_and_exact_callback() {
        let mut url = url::Url::parse("https://auth.openai.com/api/accounts/authorize").unwrap();
        let redirect = "http://127.0.0.1:12345/auth/callback";
        url.query_pairs_mut()
            .append_pair("state", "secret-state")
            .append_pair("redirect_uri", redirect)
            .append_pair("code_challenge_method", "S256");
        assert!(validate_authorization_url(url.as_str(), redirect, "secret-state").is_ok());
        assert!(validate_authorization_url(url.as_str(), redirect, "different-state").is_err());
        let mut other = url.clone();
        other.set_host(Some("example.test")).unwrap();
        assert!(validate_authorization_url(other.as_str(), redirect, "secret-state").is_err());
        url.query_pairs_mut().append_pair("state", "secret-state");
        assert!(validate_authorization_url(url.as_str(), redirect, "secret-state").is_err());
    }
    #[tokio::test]
    async fn invalid_callback_does_not_consume_the_pending_attempt() {
        let (sender, receiver) = oneshot::channel();
        let state = Callback {
            state: "expected".into(),
            sender: Arc::new(Mutex::new(Some(sender))),
        };
        let bad = RawQuery(Some("state=wrong&code=code".into()));
        assert_eq!(
            callback(State(state.clone()), bad).await.0,
            StatusCode::BAD_REQUEST
        );
        assert!(state.sender.lock().unwrap().is_some());
        assert_eq!(
            callback(
                State(state.clone()),
                RawQuery(Some("state=expected&state=expected&code=code".into()))
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        let good = RawQuery(Some("state=expected&code=code".into()));
        assert_eq!(callback(State(state), good).await.0, StatusCode::OK);
        assert_eq!(receiver.await.unwrap()["code"], "code");
    }
}
