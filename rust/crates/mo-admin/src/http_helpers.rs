use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};

use super::credentials::{CredentialsFile, Profile, load_credentials, profile_name};

pub(crate) fn auth_headers(token: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}")).map_err(|e| e.to_string())?,
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(headers)
}

pub(crate) fn read_api_error(status: reqwest::StatusCode, body: &str) -> String {
    format!("request failed ({}): {}", status, compact_or_raw(body))
}

pub(crate) fn compact_or_raw(body: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(value) => value.to_string(),
        Err(_) => body.to_string(),
    }
}

pub(crate) fn print_json_or_raw(body: &str) {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        println!(
            "{}",
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
        );
    } else {
        println!("{body}");
    }
}

pub(crate) fn get_profile_and_token(
    cli_profile: Option<&str>,
) -> Result<(CredentialsFile, String, Profile, String), String> {
    let creds = load_credentials();
    let name = profile_name(cli_profile, &creds);
    let profile = creds
        .profiles
        .get(&name)
        .cloned()
        .ok_or_else(|| format!("no profile '{name}', run login first"))?;
    let token = profile
        .access_token
        .clone()
        .ok_or_else(|| format!("profile '{name}' is not logged in"))?;
    Ok((creds, name, profile, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_sets_bearer() {
        let headers = auth_headers("my-token").unwrap();
        assert_eq!(
            headers.get("authorization").unwrap().to_str().unwrap(),
            "Bearer my-token"
        );
    }

    #[test]
    fn auth_headers_sets_content_type() {
        let headers = auth_headers("tok").unwrap();
        assert_eq!(
            headers.get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn compact_or_raw_valid_json() {
        let result = compact_or_raw("{\"a\": 1}");
        assert!(result.contains("\"a\""));
    }

    #[test]
    fn compact_or_raw_invalid_json() {
        assert_eq!(compact_or_raw("not json"), "not json");
    }

    #[test]
    fn read_api_error_includes_status_and_body() {
        let err = read_api_error(reqwest::StatusCode::FORBIDDEN, "{\"detail\":\"denied\"}");
        assert!(err.contains("403"), "got: {err}");
        assert!(err.contains("denied"), "got: {err}");
    }

    #[test]
    fn get_profile_and_token_missing_profile() {
        // Empty creds → no profile → error
        let result = get_profile_and_token(Some("nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no profile"));
    }
}
