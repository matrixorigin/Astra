use super::cli_utils::{credential_store, CredentialStore, Profile};
use super::*;

pub(super) fn clear_profile_last_session(profile: Option<&str>) -> Result<(), String> {
    credential_store()
        .mutate(|creds| {
            let name = profile_name(profile, creds);
            if let Some(entry) = creds.profiles.get_mut(&name) {
                entry.last_session_id = None;
            }
        })
        .map_err(|e| e.to_string())
}

pub(super) fn clear_profile_auth(profile: Option<&str>) -> Result<(), String> {
    credential_store()
        .mutate(|creds| {
            let name = profile_name(profile, creds);
            if let Some(entry) = creds.profiles.get_mut(&name) {
                entry.access_token = None;
                entry.refresh_token = None;
                entry.last_session_id = None;
            }
        })
        .map_err(|e| e.to_string())
}

pub(super) async fn do_login(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    username: &str,
    password: &str,
) -> Result<String, String> {
    let body = api
        .post_auth_login_json(&serde_json::json!({ "username": username, "password": password }))
        .await
        .map_err(map_thin_err)?;
    let value: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let access = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or("missing access_token")?
        .to_string();
    let refresh = value
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or("missing refresh_token")?
        .to_string();
    let username = username.to_string();
    let access_clone = access.clone();
    credential_store()
        .mutate(|creds| {
            let name =
                CredentialStore::resolve_profile_name(profile, creds.current_profile.as_deref());
            let existing = creds.profiles.get(&name).cloned().unwrap_or_default();
            let prev_session = if existing.username.as_deref() == Some(&username) {
                existing.last_session_id
            } else {
                None
            };
            let updated = Profile {
                username: Some(username.clone()),
                access_token: Some(access_clone.clone()),
                refresh_token: Some(refresh.clone()),
                last_session_id: prev_session,
                memoria_api_key: existing.memoria_api_key,
            };
            creds.current_profile = Some(name.clone());
            creds.profiles.insert(name, updated);
        })
        .map_err(|e| e.to_string())?;
    Ok(access)
}

pub(super) async fn do_register(
    api: &astra_thin_client::ThinClient,
    username: &str,
    email: &str,
    password: &str,
) -> Result<(), String> {
    api.post_auth_register_json(&serde_json::json!({
        "username": username,
        "email": email,
        "password": password,
    }))
    .await
    .map_err(map_thin_err)?;
    Ok(())
}
