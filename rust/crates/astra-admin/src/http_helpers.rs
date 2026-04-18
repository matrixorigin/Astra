use super::credentials::{load_credentials, profile_name, CredentialsFile, Profile};

pub(crate) fn read_api_error(status: u16, body: &str) -> String {
    format!("request failed ({status}): {}", compact_or_raw(body))
}

pub(crate) fn map_thin_err(e: astra_thin_client::ThinClientError) -> String {
    match e {
        astra_thin_client::ThinClientError::Api { status, body } => {
            read_api_error(status.as_u16(), &body)
        }
        other => other.to_string(),
    }
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
        let err = read_api_error(403, "{\"detail\":\"denied\"}");
        assert!(err.contains("403"), "got: {err}");
        assert!(err.contains("denied"), "got: {err}");
    }

    #[test]
    fn get_profile_and_token_missing_profile() {
        let result = get_profile_and_token(Some("nonexistent"));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no profile"));
    }
}
