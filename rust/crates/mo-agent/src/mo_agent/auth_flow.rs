use super::*;

pub(super) fn clear_profile_last_session(profile: Option<&str>) -> Result<(), String> {
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    if let Some(entry) = creds.profiles.get_mut(&name) {
        entry.last_session_id = None;
    }
    save_credentials(&creds)
}

pub(super) async fn do_login(
    api: &mo_thin_client::ThinClient,
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
    let mut creds = load_credentials();
    let name = profile_name(profile, &creds);
    creds.current_profile = Some(name.clone());
    creds.profiles.insert(
        name,
        Profile {
            username: Some(username.to_string()),
            access_token: Some(access.clone()),
            refresh_token: Some(refresh),
            last_session_id: None,
            memoria_api_key: None,
        },
    );
    save_credentials(&creds)?;
    Ok(access)
}

pub(super) async fn do_register(
    api: &mo_thin_client::ThinClient,
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
