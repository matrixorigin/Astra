pub(crate) use astra_credentials::{CredentialStore, CredentialsFile, Profile};

pub(crate) fn store() -> CredentialStore {
    CredentialStore::new()
}

/// Load credentials, falling back to defaults on error.
///
/// Surface the underlying error instead of silently swallowing it — a
/// transient failure (e.g. fd exhaustion, permission denied) used to be
/// indistinguishable from "no profile configured", which surfaces upstream
/// as a misleading "Not logged in" prompt.
pub(crate) fn load_credentials() -> CredentialsFile {
    use std::sync::Mutex;
    use std::sync::OnceLock;

    static LAST_ERR: OnceLock<Mutex<Option<String>>> = OnceLock::new();

    match store().load() {
        Ok(creds) => creds,
        Err(err) => {
            let msg = err.to_string();
            let last = LAST_ERR.get_or_init(|| Mutex::new(None));
            let mut guard = last.lock().unwrap_or_else(|e| e.into_inner());
            if guard.as_deref() != Some(msg.as_str()) {
                eprintln!("  ⚠ failed to read credentials: {msg}");
                *guard = Some(msg);
            }
            CredentialsFile::default()
        }
    }
}

pub(crate) fn profile_name(cli_profile: Option<&str>, data: &CredentialsFile) -> String {
    CredentialStore::resolve_profile_name_with_default(
        cli_profile,
        data.current_profile.as_deref(),
        "admin",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_remains_admin_for_compatibility() {
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            assert_eq!(profile_name(None, &CredentialsFile::default()), "admin");
        });
    }
}
