//! Local inference configuration belongs to an authenticated deployment/account,
//! not to a process-global credentials filename or a mutable display name.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{CredentialStore, LocalModelConfigStore, LocalSecretStore};

#[derive(Clone)]
pub struct LocalModelScope {
    root: PathBuf,
    identity: String,
    account_id: String,
}

impl LocalModelScope {
    pub fn for_owner(deployment: &str, account_id: &str) -> Result<Self, String> {
        if account_id.trim().is_empty() || account_id.len() > 255 {
            return Err(
                "Local model setup requires a signed-in Astra account; run astra login".into(),
            );
        }
        let mut url = url::Url::parse(deployment).map_err(|_| "Invalid Astra deployment URL")?;
        let scheme = match url.scheme() {
            "http" | "ws" => "http",
            "https" | "wss" => "https",
            _ => return Err("Invalid Astra deployment URL scheme".into()),
        };
        url.set_scheme(scheme)
            .map_err(|_| "Invalid Astra deployment URL")?;
        if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
            return Err("Invalid Astra deployment authority".into());
        }
        let path = url
            .path()
            .trim_end_matches('/')
            .strip_suffix("/edge/ws")
            .unwrap_or(url.path().trim_end_matches('/'))
            .to_owned();
        url.set_path(&path);
        url.set_query(None);
        url.set_fragment(None);
        let identity = format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(
                    "astra-local-inference-owner-v1",
                    url.as_str().trim_end_matches('/'),
                    account_id,
                ))
                .map_err(|_| "Cannot encode local inference owner")?
            )
        );
        let root = super::default_path()
            .with_file_name("local-models")
            .join(&identity);
        Ok(Self {
            root,
            identity,
            account_id: account_id.to_owned(),
        })
    }

    pub fn for_profile(deployment: &str, profile: Option<&str>) -> Result<Self, String> {
        let file = CredentialStore::new()
            .load()
            .map_err(|error| error.to_string())?;
        let profile =
            CredentialStore::resolve_profile_name(profile, file.current_profile.as_deref());
        let account = file.profiles.get(&profile)
            .filter(|profile| profile.access_token.as_deref().is_some_and(|token| !token.trim().is_empty()))
            .and_then(|profile| profile.account_id.as_deref())
            .ok_or("Local model setup requires a signed-in Astra account; run astra login with this profile")?;
        Self::for_owner(deployment, account)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn identity(&self) -> &str {
        &self.identity
    }
    pub fn account_id(&self) -> &str {
        &self.account_id
    }
    pub fn models(&self) -> LocalModelConfigStore {
        LocalModelConfigStore::with_path(self.root.join("models.json"))
    }
    pub fn secrets(&self) -> LocalSecretStore {
        LocalSecretStore::with_root(self.root.join("model-secrets"))
    }
}

impl std::fmt::Debug for LocalModelScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalModelScope")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_account_identity_after_logout_does_not_authorize_local_models() {
        let directory = tempfile::tempdir().unwrap();
        let _scope = crate::set_test_credentials_dir(directory.path().to_owned());
        let store = CredentialStore::new();
        store
            .mutate(|file| {
                file.current_profile = Some("fixture".into());
                file.profiles.insert(
                    "fixture".into(),
                    crate::Profile {
                        account_id: Some("owner".into()),
                        access_token: Some("synthetic-token".into()),
                        ..Default::default()
                    },
                );
            })
            .unwrap();
        assert!(LocalModelScope::for_profile("https://fixture.invalid", None).is_ok());
        store
            .mutate(|file| {
                file.profiles.get_mut("fixture").unwrap().access_token = None;
            })
            .unwrap();
        assert!(LocalModelScope::for_profile("https://fixture.invalid", None).is_err());
    }

    #[test]
    fn scopes_separate_deployments_accounts_and_path_prefixes() {
        let a = LocalModelScope::for_owner("https://astra.example/prefix", "account-a").unwrap();
        let ws =
            LocalModelScope::for_owner("wss://astra.example/prefix/edge/ws", "account-a").unwrap();
        assert_eq!(a.root(), ws.root());
        for (url, user) in [
            ("https://astra.example/prefix", "account-b"),
            ("https://other.example/prefix", "account-a"),
            ("https://astra.example/other", "account-a"),
        ] {
            assert_ne!(
                a.root(),
                LocalModelScope::for_owner(url, user).unwrap().root()
            );
        }
        assert!(LocalModelScope::for_owner("https://astra.example", "").is_err());
    }

    #[test]
    #[cfg(unix)]
    fn another_owner_cannot_load_the_configuration_or_resolve_its_secret() {
        let directory = tempfile::tempdir().unwrap();
        let _scope = crate::set_test_credentials_dir(directory.path().to_owned());
        let a = LocalModelScope::for_owner("https://astra.example", "a").unwrap();
        let b = LocalModelScope::for_owner("https://astra.example", "b").unwrap();
        let mut config = crate::LocalModelConfig::default();
        config.models.insert(
            "Work".into(),
            crate::LocalModelDefinition {
                protocol: crate::LocalInferenceProtocol::OpenaiCompatible,
                base_url: "https://provider.example/v1".into(),
                model: "fixture".into(),
                binding_revision: 1,
                context_window: 8192,
                max_output_tokens: 1024,
                credential: crate::LocalCredentialRef::ProtectedFile {
                    secret_id: "fixture".into(),
                },
                probe: crate::LocalModelProbeState::default(),
            },
        );
        a.models().replace(0, config).unwrap();
        a.secrets().put("fixture", "synthetic-non-secret").unwrap();
        assert!(b.models().load().unwrap().models.is_empty());
        assert!(
            b.secrets()
                .resolve(&crate::LocalCredentialRef::ProtectedFile {
                    secret_id: "fixture".into()
                })
                .is_err()
        );
    }
}
