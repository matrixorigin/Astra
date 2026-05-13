pub(crate) use astra_credentials::{CredentialStore, CredentialsFile, Profile};

pub(crate) fn store() -> CredentialStore {
    CredentialStore::new()
}

pub(crate) fn load_credentials() -> CredentialsFile {
    store().load().unwrap_or_default()
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
