use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("json parse error on {path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("json serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CredentialsFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_profile: Option<String>,

    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

#[derive(Clone, Serialize, Deserialize, Default)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memoria_api_key: Option<String>,
}

impl std::fmt::Debug for Profile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Profile")
            .field("username", &self.username)
            .field("access_token", &self.access_token.as_ref().map(|_| "***"))
            .field("refresh_token", &self.refresh_token.as_ref().map(|_| "***"))
            .field("last_session_id", &self.last_session_id)
            .field(
                "memoria_api_key",
                &self.memoria_api_key.as_ref().map(|_| "***"),
            )
            .finish()
    }
}

pub struct CredentialStore {
    path: PathBuf,
}

impl CredentialStore {
    pub fn new() -> Self {
        Self {
            path: default_path(),
        }
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Resolve active profile name.
    /// Priority: cli_override > ASTRA_PROFILE env > file current_profile > "default"
    pub fn resolve_profile_name(cli_override: Option<&str>, file_current: Option<&str>) -> String {
        Self::resolve_profile_name_with_default(cli_override, file_current, "default")
    }

    /// Resolve active profile name with a caller-specific legacy default.
    /// Priority: cli_override > ASTRA_PROFILE env > file current_profile > fallback_default.
    pub fn resolve_profile_name_with_default(
        cli_override: Option<&str>,
        file_current: Option<&str>,
        fallback_default: &str,
    ) -> String {
        if let Some(name) = cli_override {
            return name.to_string();
        }
        if let Ok(env_val) = std::env::var("ASTRA_PROFILE")
            && !env_val.is_empty()
        {
            return env_val;
        }
        if let Some(name) = file_current {
            return name.to_string();
        }
        fallback_default.to_string()
    }

    /// Load credentials with a shared (read) lock.
    ///
    /// Returns `CredentialsFile::default()` without creating the sibling `.lock`
    /// file when the credentials file does not yet exist — pure readers must not
    /// litter the credentials directory on first run.
    pub fn load(&self) -> Result<CredentialsFile, CredentialError> {
        if !self.path.exists() {
            return Ok(CredentialsFile::default());
        }
        let _lock_file = self.lock_for_read_if_possible()?;
        let content = fs::read_to_string(&self.path).map_err(|e| CredentialError::Io {
            path: self.path.clone(),
            source: e,
        })?;
        if content.trim().is_empty() {
            return Ok(CredentialsFile::default());
        }
        serde_json::from_str(&content).map_err(|e| CredentialError::Parse {
            path: self.path.clone(),
            source: e,
        })
    }

    fn lock_for_read_if_possible(&self) -> Result<Option<File>, CredentialError> {
        if let Some(parent) = self.path.parent()
            && !parent.exists()
        {
            return Ok(None);
        }
        let lock_path = self.path.with_extension("json.lock");
        let lock_file = open_lock_file(&lock_path)?;
        lock_file.lock_shared().map_err(|e| CredentialError::Io {
            path: lock_path.clone(),
            source: e,
        })?;
        Ok(Some(lock_file))
    }

    /// Acquire exclusive lock, read current state, apply mutator, write back atomically.
    pub fn mutate<F, R>(&self, f: F) -> Result<R, CredentialError>
    where
        F: FnOnce(&mut CredentialsFile) -> R,
    {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| CredentialError::Io {
                path: parent.to_path_buf(),
                source: e,
            })?;
        }

        // Use a separate .lock file so that rename-based atomicity doesn't
        // invalidate the lock fd (renaming the data file would leave the old
        // fd pointing at the unlinked inode).
        let lock_path = self.path.with_extension("json.lock");
        let lock_file = open_lock_file(&lock_path)?;

        lock_file
            .lock_exclusive()
            .map_err(|e| CredentialError::Io {
                path: lock_path.clone(),
                source: e,
            })?;

        // Read current data from the actual credentials file
        let mut data: CredentialsFile = if self.path.exists() {
            let content = fs::read_to_string(&self.path).map_err(|e| CredentialError::Io {
                path: self.path.clone(),
                source: e,
            })?;
            if content.trim().is_empty() {
                CredentialsFile::default()
            } else {
                serde_json::from_str(&content).map_err(|e| CredentialError::Parse {
                    path: self.path.clone(),
                    source: e,
                })?
            }
        } else {
            CredentialsFile::default()
        };

        let result = f(&mut data);

        let body = serde_json::to_string_pretty(&data)?;

        // Write to tmp file then rename for atomicity. On any failure between
        // creating the tmp file and the successful rename we best-effort remove
        // the tmp — a stale `.tmp` would otherwise leave sensitive bytes behind
        // (mode 0o600, but still) across process restarts.
        let tmp_path = self.path.with_extension("json.tmp");
        if let Err(e) = write_private_file(&tmp_path, &body) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }

        if let Err(e) = fs::rename(&tmp_path, &self.path) {
            let _ = fs::remove_file(&tmp_path);
            return Err(CredentialError::Io {
                path: self.path.clone(),
                source: e,
            });
        }

        // lock released when `lock_file` drops
        Ok(result)
    }
}

impl Default for CredentialStore {
    fn default() -> Self {
        Self::new()
    }
}

fn open_lock_file(path: &PathBuf) -> Result<File, CredentialError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| CredentialError::Io {
            path: path.clone(),
            source: e,
        })
}

fn write_private_file(path: &PathBuf, body: &str) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| CredentialError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.write_all(body.as_bytes())
            .map_err(|e| CredentialError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.sync_all().map_err(|e| CredentialError::Io {
            path: path.clone(),
            source: e,
        })?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // TODO(windows): this fallback does not restrict ACLs and does not
        // fsync before the caller renames. On shared Windows machines the
        // tmp file may be readable by other users, and a crash between
        // write + rename can leave a zero-length credentials file. Revisit
        // if/when Windows becomes a supported target — likely needs
        // `CreateFileW` with a restrictive SECURITY_ATTRIBUTES and an
        // explicit `FlushFileBuffers`.
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(|e| CredentialError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.write_all(body.as_bytes())
            .map_err(|e| CredentialError::Io {
                path: path.clone(),
                source: e,
            })?;
        file.sync_all().map_err(|e| CredentialError::Io {
            path: path.clone(),
            source: e,
        })?;
        Ok(())
    }
}

fn default_path() -> PathBuf {
    if let Ok(dir) = std::env::var("ASTRA_CLI_CREDENTIALS_DIR") {
        return PathBuf::from(dir).join("credentials.json");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("credentials.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn store_in(dir: &TempDir) -> CredentialStore {
        CredentialStore::with_path(dir.path().join("credentials.json"))
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir);
        let creds = store.load().unwrap();
        assert!(creds.profiles.is_empty());
    }

    #[test]
    fn mutate_creates_file_and_roundtrips() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir);

        store
            .mutate(|data| {
                data.profiles.insert(
                    "test".to_string(),
                    Profile {
                        username: Some("alice".to_string()),
                        access_token: Some("tok123".to_string()),
                        ..Default::default()
                    },
                );
            })
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.profiles["test"].username.as_deref(), Some("alice"));
        assert_eq!(
            loaded.profiles["test"].access_token.as_deref(),
            Some("tok123")
        );
    }

    #[test]
    fn mutate_is_atomic_under_contention() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");

        let handles: Vec<_> = (0..10)
            .map(|i| {
                let p = path.clone();
                std::thread::spawn(move || {
                    let store = CredentialStore::with_path(p);
                    store
                        .mutate(|data| {
                            let entry = data.profiles.entry("shared".to_string()).or_default();
                            let current: i32 = entry
                                .username
                                .as_deref()
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0);
                            entry.username = Some((current + 1).to_string());
                            // Simulate some work
                            std::thread::sleep(std::time::Duration::from_millis(i));
                        })
                        .unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        let store = CredentialStore::with_path(path);
        let data = store.load().unwrap();
        let final_val: i32 = data.profiles["shared"]
            .username
            .as_deref()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(final_val, 10);
    }

    #[test]
    fn resolve_profile_name_priority_and_legacy_fallback() {
        // cli_override wins
        assert_eq!(
            CredentialStore::resolve_profile_name(Some("staging"), Some("prod")),
            "staging"
        );

        // env var wins over file current_profile
        temp_env::with_var("ASTRA_PROFILE", Some("from_env"), || {
            assert_eq!(
                CredentialStore::resolve_profile_name(None, Some("prod")),
                "from_env"
            );
        });

        // file current_profile fallback
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            assert_eq!(
                CredentialStore::resolve_profile_name(None, Some("prod")),
                "prod"
            );
        });

        // default fallback
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            assert_eq!(CredentialStore::resolve_profile_name(None, None), "default");
        });

        // Legacy admin default
        temp_env::with_var("ASTRA_PROFILE", None::<&str>, || {
            assert_eq!(
                CredentialStore::resolve_profile_name_with_default(None, None, "admin"),
                "admin"
            );
        });
        temp_env::with_var("ASTRA_PROFILE", Some("from-env"), || {
            assert_eq!(
                CredentialStore::resolve_profile_name_with_default(None, None, "admin"),
                "from-env"
            );
        });
    }

    #[test]
    fn invalid_json_returns_parse_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("credentials.json");
        fs::write(&path, "{not-json").unwrap();
        let store = CredentialStore::with_path(path.clone());

        let err = store.load().unwrap_err();

        assert!(matches!(err, CredentialError::Parse { path: p, .. } if p == path));
    }

    #[test]
    fn current_profile_roundtrips() {
        let dir = TempDir::new().unwrap();
        let store = store_in(&dir);

        store
            .mutate(|d| {
                d.current_profile = Some("staging".to_string());
                d.profiles.insert("staging".to_string(), Profile::default());
            })
            .unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.current_profile.as_deref(), Some("staging"));
    }

    #[test]
    fn file_permissions_are_restricted_0600() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = TempDir::new().unwrap();

            // CredentialStore creates restricted files
            let store = store_in(&dir);
            store.mutate(|_| {}).unwrap();
            let meta = fs::metadata(store.path()).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);

            // Private tmp writer also creates restricted files
            let tmp = dir.path().join("credentials.json.tmp");
            write_private_file(&tmp, "secret-token").unwrap();
            let meta = fs::metadata(&tmp).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::read_to_string(&tmp).unwrap(), "secret-token");
        }
    }
}
