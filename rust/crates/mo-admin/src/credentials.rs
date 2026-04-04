use std::{collections::HashMap, fs, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Default)]
pub(crate) struct CredentialsFile {
    pub current_profile: Option<String>,
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct Profile {
    pub username: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

pub(crate) fn credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("credentials.json")
}

pub(crate) fn load_credentials() -> CredentialsFile {
    let path = credentials_path();
    let Ok(content) = fs::read_to_string(path) else {
        return CredentialsFile::default();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub(crate) fn save_credentials(data: &CredentialsFile) -> Result<(), String> {
    let path = credentials_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
    fs::write(path, body).map_err(|e| e.to_string())
}

pub(crate) fn profile_name(cli_profile: Option<&str>, data: &CredentialsFile) -> String {
    cli_profile
        .map(ToString::to_string)
        .or_else(|| data.current_profile.clone())
        .unwrap_or_else(|| "admin".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_name_cli_override() {
        let creds = CredentialsFile::default();
        assert_eq!(profile_name(Some("staging"), &creds), "staging");
    }

    #[test]
    fn profile_name_from_creds() {
        let creds = CredentialsFile {
            current_profile: Some("prod".to_string()),
            ..Default::default()
        };
        assert_eq!(profile_name(None, &creds), "prod");
    }

    #[test]
    fn profile_name_default_admin() {
        let creds = CredentialsFile::default();
        assert_eq!(profile_name(None, &creds), "admin");
    }

    #[test]
    fn credentials_roundtrip() {
        let creds = CredentialsFile {
            current_profile: Some("test".to_string()),
            profiles: HashMap::from([(
                "test".to_string(),
                Profile {
                    username: Some("user1".to_string()),
                    access_token: Some("tok".to_string()),
                    refresh_token: None,
                },
            )]),
        };
        let json = serde_json::to_string(&creds).unwrap();
        let parsed: CredentialsFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.current_profile, Some("test".to_string()));
        assert!(parsed.profiles.contains_key("test"));
    }
}
