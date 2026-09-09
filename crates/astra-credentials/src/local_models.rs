use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The first published local-model schema. There is no pre-existing on-disk
/// format to migrate in this feature, so new installations start at version 1.
pub const LOCAL_MODELS_FILE_VERSION: u32 = 1;
const PROBE_FINGERPRINT_KEY_ID: &str = "probe_fingerprint_key_v1";
const PROBE_FINGERPRINT_PREFIX: &str = "hmac-sha256-v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalInferenceProtocol {
    OpenaiCompatible,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalCredentialRef {
    Environment { name: String },
    ProtectedFile { secret_id: String },
    SystemKeychain { service: String, account: String },
    None,
}

impl std::fmt::Debug for LocalCredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Environment { .. } => "environment",
            Self::ProtectedFile { .. } => "protected_file",
            Self::SystemKeychain { .. } => "system_keychain",
            Self::None => "none",
        };
        f.debug_struct("LocalCredentialRef")
            .field("kind", &kind)
            .finish()
    }
}

impl LocalCredentialRef {
    pub fn validate(&self) -> Result<(), LocalModelConfigError> {
        match self {
            Self::Environment { name } => validate_environment_name(name),
            Self::ProtectedFile { secret_id } => {
                validate_file_component("protected secret id", secret_id)
            }
            Self::SystemKeychain { service, account } => {
                validate_component("keychain service", service)?;
                validate_component("keychain account", account)
            }
            Self::None => Ok(()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelDefinition {
    pub protocol: LocalInferenceProtocol,
    pub base_url: String,
    pub model: String,
    /// Revision of this model's provider configuration. Unlike the enclosing
    /// file CAS revision, this only changes when this model changes, so an
    /// unrelated model update does not invalidate its Runner binding.
    #[serde(default = "default_binding_revision")]
    pub binding_revision: u64,
    /// User-declared provider capacity. These values are part of the binding
    /// revision and are never guessed from a mutable model name.
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub credential: LocalCredentialRef,
    /// Last explicit provider probe for this exact binding and credential
    /// material. Probe evidence is metadata, not binding material: recording
    /// it must not authorize a new provider configuration or invalidate an
    /// active Runner attachment.
    #[serde(default)]
    pub probe: LocalModelProbeState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalModelProbeState {
    #[default]
    NotRun,
    Passed {
        checked_at_unix_ms: u64,
        /// Keyed, secret-safe identity of the credential material used by the
        /// probe. The raw credential is never persisted. `None` is retained
        /// for compatibility with pre-fingerprint probe records and is not
        /// trusted for environment-backed credentials.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_fingerprint: Option<String>,
    },
    Failed {
        checked_at_unix_ms: u64,
        /// Stable, secret-safe classification such as `http_401` or
        /// `transport`. Raw provider messages and endpoint details never go
        /// into the local configuration file.
        code: String,
        /// Keyed, secret-safe identity of the credential material used by the
        /// probe, when credential resolution succeeded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_fingerprint: Option<String>,
    },
}

impl std::fmt::Debug for LocalModelDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalModelDefinition")
            .field("protocol", &self.protocol)
            .field("model_present", &!self.model.is_empty())
            .field("credential", &self.credential)
            .finish()
    }
}

impl LocalModelDefinition {
    pub fn validate(&self) -> Result<(), LocalModelConfigError> {
        validate_component("model", &self.model)?;
        validate_base_url(&self.base_url)?;
        if self.binding_revision == 0 {
            return Err(LocalModelConfigError::Invalid {
                field: "model binding revision",
                reason: "must be greater than zero".to_string(),
            });
        }
        if self.context_window == 0 {
            return Err(LocalModelConfigError::Invalid {
                field: "context window",
                reason: "must be greater than zero".to_string(),
            });
        }
        if self.max_output_tokens == 0 || self.max_output_tokens > self.context_window {
            return Err(LocalModelConfigError::Invalid {
                field: "maximum output tokens",
                reason: "must be greater than zero and no larger than the context window"
                    .to_string(),
            });
        }
        self.credential.validate()?;
        if let LocalModelProbeState::Failed { code, .. } = &self.probe {
            validate_component("probe failure code", code)?;
        }
        Ok(())
    }

    /// Compare only the material that determines the provider binding. Probe
    /// evidence is deliberately excluded so recording a check does not bump
    /// the binding revision or invalidate a concurrent execution lease.
    fn binding_material_eq(&self, other: &Self) -> bool {
        self.protocol == other.protocol
            && self.base_url == other.base_url
            && self.model == other.model
            && self.binding_revision == other.binding_revision
            && self.context_window == other.context_window
            && self.max_output_tokens == other.max_output_tokens
            && self.credential == other.credential
    }
}

fn default_binding_revision() -> u64 {
    1
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelConfig {
    pub version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub models: BTreeMap<String, LocalModelDefinition>,
}

impl std::fmt::Debug for LocalModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalModelConfig")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("model_count", &self.models.len())
            .finish()
    }
}

impl Default for LocalModelConfig {
    fn default() -> Self {
        Self {
            version: LOCAL_MODELS_FILE_VERSION,
            revision: 0,
            models: BTreeMap::new(),
        }
    }
}

impl LocalModelConfig {
    pub fn validate(&self) -> Result<(), LocalModelConfigError> {
        if self.version != LOCAL_MODELS_FILE_VERSION {
            return Err(LocalModelConfigError::UnsupportedVersion {
                actual: self.version,
                expected: LOCAL_MODELS_FILE_VERSION,
            });
        }
        for (name, model) in &self.models {
            validate_component("local model name", name)?;
            model
                .validate()
                .map_err(|source| LocalModelConfigError::Model {
                    name: name.clone(),
                    source: Box::new(source),
                })?;
        }
        Ok(())
    }
}

/// Provider authorization resolved for one local client attachment.
///
/// This value deliberately implements neither `Serialize` nor `Clone` and its
/// debug representation never reveals the credential. Callers should resolve
/// environment-backed values in the attaching process and pass the value over
/// authenticated local IPC, rather than letting a shared host inspect its own
/// startup environment.
pub struct ResolvedLocalCredential(String);

impl ResolvedLocalCredential {
    pub fn from_environment(
        reference: &LocalCredentialRef,
        mut read: impl FnMut(&str) -> Option<String>,
    ) -> Result<Option<Self>, LocalModelConfigError> {
        reference.validate()?;
        match reference {
            LocalCredentialRef::Environment { name } => {
                let value = read(name).ok_or_else(|| {
                    LocalModelConfigError::CredentialUnavailable(format!(
                        "environment variable {name} is not set in this terminal"
                    ))
                })?;
                if value.is_empty() {
                    return Err(LocalModelConfigError::CredentialUnavailable(format!(
                        "environment variable {name} is empty in this terminal"
                    )));
                }
                Ok(Some(Self(value)))
            }
            LocalCredentialRef::None => Ok(None),
            LocalCredentialRef::ProtectedFile { .. }
            | LocalCredentialRef::SystemKeychain { .. } => {
                Err(LocalModelConfigError::CredentialUnavailable(
                    "credential must be resolved by its configured local backend".to_string(),
                ))
            }
        }
    }

    pub fn expose_to_local_transport(&self) -> &str {
        &self.0
    }

    /// Return a stable, keyed identity for this credential material.
    ///
    /// Probe evidence stores this HMAC instead of the raw provider key so
    /// another terminal can tell whether its environment-backed attachment
    /// is the material that was actually checked. The key is kept in the
    /// owner-private secret store and never written to `models.json`.
    pub fn fingerprint_with_key(&self, key: &ResolvedLocalCredential) -> String {
        keyed_fingerprint(key.0.as_bytes(), self.0.as_bytes())
    }

    /// Return the non-secret generation identifier embedded in a keyed probe
    /// fingerprint. A new identifier means the owner-local key was replaced,
    /// so evidence produced with the previous key is no longer verifiable.
    pub fn fingerprint_key_generation(fingerprint: &str) -> Option<&str> {
        let rest = fingerprint.strip_prefix(PROBE_FINGERPRINT_PREFIX)?;
        let (generation, digest) = rest.strip_prefix(':')?.split_once(':')?;
        (generation.len() == 64
            && digest.len() == 64
            && generation.bytes().all(|byte| byte.is_ascii_hexdigit())
            && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then_some(generation)
    }

    /// Return the stable identity used for a keyless (`None`) credential.
    /// Keyless probes are not terminal-local, but using an explicit marker
    /// lets new records be distinguished from legacy records that have no
    /// fingerprint at all.
    pub fn no_credential_fingerprint() -> String {
        fingerprint_value("<none>")
    }
}

fn fingerprint_value(value: &str) -> String {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(b"astra-local-model-credential-v1\0");
    hasher.update(value.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}

fn keyed_fingerprint(key: &[u8], value: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};

    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .expect("HMAC-SHA256 accepts every owner-local key length");
    mac.update(b"astra-local-model-credential-v2\0");
    mac.update(value);
    let digest = mac.finalize().into_bytes();
    let mut generation = Sha256::new();
    generation.update(b"astra-local-model-probe-key-generation-v1\0");
    generation.update(key);
    format!(
        "{PROBE_FINGERPRINT_PREFIX}:{:x}:{digest:x}",
        generation.finalize()
    )
}

impl std::fmt::Debug for ResolvedLocalCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedLocalCredential")
            .field("present", &!self.0.is_empty())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum LocalModelConfigError {
    #[error("local model configuration I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("local model configuration JSON at {path}: {diagnostic}")]
    Json {
        path: PathBuf,
        diagnostic: JsonDecodeDiagnostic,
    },
    #[error("unsupported local model configuration version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u32, expected: u32 },
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("invalid local model {name}: {source}")]
    Model {
        name: String,
        source: Box<LocalModelConfigError>,
    },
    #[error("local model configuration revision changed; expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("local credential unavailable: {0}")]
    CredentialUnavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonDecodeDiagnostic {
    category: &'static str,
    line: usize,
    column: usize,
}

impl std::fmt::Display for JsonDecodeDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at line {}, column {}",
            self.category, self.line, self.column
        )
    }
}

impl From<&serde_json::Error> for JsonDecodeDiagnostic {
    fn from(error: &serde_json::Error) -> Self {
        Self {
            category: match error.classify() {
                serde_json::error::Category::Io => "I/O error",
                serde_json::error::Category::Syntax => "syntax error",
                serde_json::error::Category::Data => "invalid data",
                serde_json::error::Category::Eof => "unexpected EOF",
            },
            line: error.line(),
            column: error.column(),
        }
    }
}

pub struct LocalModelConfigStore {
    path: PathBuf,
}

/// Validated configuration snapshot with the store's shared OS lock retained.
/// Keeping this value alive prevents a concurrent CLI writer from replacing
/// the binding material between validation and a local execution fence.
pub struct LocalModelConfigLease {
    config: LocalModelConfig,
    _lock: File,
}

impl LocalModelConfigLease {
    pub fn config(&self) -> &LocalModelConfig {
        &self.config
    }

    pub fn into_config(self) -> LocalModelConfig {
        self.config
    }
}

impl LocalModelConfigStore {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<LocalModelConfig, LocalModelConfigError> {
        Ok(self.lease()?.into_config())
    }

    /// Load a validated snapshot and retain the shared revision lock until
    /// the returned lease is dropped.
    pub fn lease(&self) -> Result<LocalModelConfigLease, LocalModelConfigError> {
        if let Some(parent) = self.path.parent() {
            ensure_private_directory(parent)?;
        }
        let lock = open_lock(&self.lock_path())?;
        lock.lock_shared().map_err(|source| self.io(source))?;
        let config = self.load_unlocked()?;
        Ok(LocalModelConfigLease {
            config,
            _lock: lock,
        })
    }

    /// Atomically replace the desired configuration under a revision CAS.
    /// Invalid candidates never alter the previously working file.
    pub fn replace(
        &self,
        expected_revision: u64,
        mut candidate: LocalModelConfig,
    ) -> Result<LocalModelConfig, LocalModelConfigError> {
        candidate.validate()?;
        if let Some(parent) = self.path.parent() {
            ensure_private_directory(parent)?;
        }
        let lock_path = self.lock_path();
        let lock = open_lock(&lock_path)?;
        lock.lock_exclusive()
            .map_err(|source| LocalModelConfigError::Io {
                path: lock_path,
                source,
            })?;
        let current = self.load_unlocked()?;
        if current.revision != expected_revision {
            return Err(LocalModelConfigError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        candidate.revision = current
            .models
            .values()
            .fold(expected_revision, |revision, model| {
                revision.max(model.binding_revision)
            })
            .checked_add(1)
            .ok_or_else(|| LocalModelConfigError::Invalid {
                field: "revision",
                reason: "revision is exhausted".to_string(),
            })?;
        // The file revision is also the generation high-water mark. A deleted
        // name must never regain a profile revision that authorized an older
        // endpoint or credential, including when another terminal missed the
        // intermediate deletion.
        for (name, definition) in &mut candidate.models {
            let binding_changed = current
                .models
                .get(name)
                .is_none_or(|previous| !previous.binding_material_eq(definition));
            if binding_changed {
                definition.binding_revision = definition.binding_revision.max(candidate.revision);
            }
        }
        candidate.revision = candidate
            .models
            .values()
            .fold(candidate.revision, |revision, model| {
                revision.max(model.binding_revision)
            });
        let body = serde_json::to_vec_pretty(&candidate).map_err(|source| {
            LocalModelConfigError::Json {
                path: self.path.clone(),
                diagnostic: JsonDecodeDiagnostic::from(&source),
            }
        })?;
        write_atomic_private(&self.path, &body)?;
        Ok(candidate)
    }

    fn load_unlocked(&self) -> Result<LocalModelConfig, LocalModelConfigError> {
        if !self.path.exists() {
            return Ok(LocalModelConfig::default());
        }
        let bytes = read_private_file(&self.path)?;
        let config: LocalModelConfig =
            serde_json::from_slice(&bytes).map_err(|source| LocalModelConfigError::Json {
                path: self.path.clone(),
                diagnostic: JsonDecodeDiagnostic::from(&source),
            })?;
        config.validate()?;
        Ok(config)
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    fn io(&self, source: std::io::Error) -> LocalModelConfigError {
        LocalModelConfigError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

/// Owner-private storage for provider credentials selected explicitly by a user.
///
/// Configuration contains only an opaque reference. This backend deliberately
/// makes no encryption claim: on Unix it relies on the local account's
/// filesystem boundary and rejects unsafe ownership or modes when reading.
/// Other platforms fail closed until a native protected-storage backend is
/// available; callers may still use environment-variable credentials there.
pub struct LocalSecretStore {
    root: PathBuf,
}

impl LocalSecretStore {
    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn put(&self, secret_id: &str, value: &str) -> Result<(), LocalModelConfigError> {
        ensure_protected_store_supported()?;
        validate_file_component("protected secret id", secret_id)?;
        if value.is_empty() {
            return Err(LocalModelConfigError::CredentialUnavailable(
                "credential is empty".to_string(),
            ));
        }
        if value.len() > 1024 * 1024 {
            return Err(LocalModelConfigError::CredentialUnavailable(
                "credential exceeds the 1 MiB limit".to_string(),
            ));
        }
        ensure_private_directory(&self.root)?;
        write_new_private(&self.path(secret_id), value.as_bytes())
    }

    /// Load the owner-local key used to bind probe evidence to credential
    /// material, creating it once during an explicit provider check. Status
    /// reads must use [`Self::resolve_probe_fingerprint_key`] so they never
    /// mutate local state just by displaying a model.
    pub fn ensure_probe_fingerprint_key(
        &self,
    ) -> Result<Option<ResolvedLocalCredential>, LocalModelConfigError> {
        let reference = LocalCredentialRef::ProtectedFile {
            secret_id: PROBE_FINGERPRINT_KEY_ID.to_string(),
        };
        match self.resolve(&reference) {
            Ok(Some(key)) => Ok(Some(key)),
            Ok(None) => Err(LocalModelConfigError::CredentialUnavailable(
                "probe fingerprint key reference resolved without key material".to_string(),
            )),
            Err(LocalModelConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                let generated = uuid::Uuid::new_v4().simple().to_string();
                match self.put(PROBE_FINGERPRINT_KEY_ID, &generated) {
                    Ok(()) => self.resolve(&reference),
                    Err(LocalModelConfigError::Io { source, .. })
                        if source.kind() == std::io::ErrorKind::AlreadyExists =>
                    {
                        self.resolve(&reference)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Read the owner-local probe key without creating it. A missing key means
    /// previously persisted environment-backed evidence cannot be trusted and
    /// must be shown as stale until the next explicit check recreates it.
    pub fn resolve_probe_fingerprint_key(
        &self,
    ) -> Result<Option<ResolvedLocalCredential>, LocalModelConfigError> {
        let reference = LocalCredentialRef::ProtectedFile {
            secret_id: PROBE_FINGERPRINT_KEY_ID.to_string(),
        };
        match self.resolve(&reference) {
            Ok(value) => Ok(value),
            Err(LocalModelConfigError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub fn resolve(
        &self,
        reference: &LocalCredentialRef,
    ) -> Result<Option<ResolvedLocalCredential>, LocalModelConfigError> {
        reference.validate()?;
        match reference {
            LocalCredentialRef::ProtectedFile { secret_id } => {
                ensure_protected_store_supported()?;
                let path = self.path(secret_id);
                let bytes = read_private_file(&path)?;
                let value =
                    String::from_utf8(bytes).map_err(|_| LocalModelConfigError::Invalid {
                        field: "protected credential",
                        reason: "must be UTF-8".to_string(),
                    })?;
                if value.is_empty() {
                    return Err(LocalModelConfigError::CredentialUnavailable(
                        "protected credential is empty".to_string(),
                    ));
                }
                Ok(Some(ResolvedLocalCredential(value)))
            }
            LocalCredentialRef::None => Ok(None),
            LocalCredentialRef::Environment { .. } => {
                Err(LocalModelConfigError::CredentialUnavailable(
                    "environment credential must be resolved by the attaching process".to_string(),
                ))
            }
            LocalCredentialRef::SystemKeychain { .. } => {
                Err(LocalModelConfigError::CredentialUnavailable(
                    "system keychain backend is not available on this build".to_string(),
                ))
            }
        }
    }

    pub fn remove(&self, secret_id: &str) -> Result<bool, LocalModelConfigError> {
        ensure_protected_store_supported()?;
        validate_file_component("protected secret id", secret_id)?;
        let path = self.path(secret_id);
        match fs::remove_file(&path) {
            Ok(()) => {
                #[cfg(unix)]
                File::open(&self.root)
                    .and_then(|directory| directory.sync_all())
                    .map_err(|source| LocalModelConfigError::Io {
                        path: self.root.clone(),
                        source,
                    })?;
                Ok(true)
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(LocalModelConfigError::Io { path, source }),
        }
    }

    fn path(&self, secret_id: &str) -> PathBuf {
        self.root.join(secret_id)
    }
}

fn ensure_protected_store_supported() -> Result<(), LocalModelConfigError> {
    #[cfg(unix)]
    {
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(LocalModelConfigError::CredentialUnavailable(
            "protected-file credentials are not supported on this platform; use an environment credential"
                .to_string(),
        ))
    }
}

fn validate_component(field: &'static str, value: &str) -> Result<(), LocalModelConfigError> {
    if value.trim().is_empty() {
        return Err(LocalModelConfigError::Invalid {
            field,
            reason: "must not be empty".to_string(),
        });
    }
    if value.len() > 512 || value.chars().any(char::is_control) {
        return Err(LocalModelConfigError::Invalid {
            field,
            reason: "must be at most 512 bytes and contain no control characters".to_string(),
        });
    }
    Ok(())
}

fn validate_file_component(field: &'static str, value: &str) -> Result<(), LocalModelConfigError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(LocalModelConfigError::Invalid {
            field,
            reason: "must contain only ASCII letters, digits, '-' or '_' and be at most 128 bytes"
                .to_string(),
        });
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), LocalModelConfigError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|source| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
            .open(path)
            .map_err(|source| LocalModelConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        let metadata = directory
            .metadata()
            .map_err(|source| LocalModelConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
            return Err(LocalModelConfigError::CredentialUnavailable(
                "local model directory must be owned by this user and not a symlink".into(),
            ));
        }
        // Tighten an existing owned directory via its descriptor, never by
        // following a replaceable symlink to an unrelated directory.
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|source| LocalModelConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

fn read_private_file(path: &Path) -> Result<Vec<u8>, LocalModelConfigError> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(not(unix))]
    let file = OpenOptions::new().read(true).open(path);
    let file = file.map_err(|source| LocalModelConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = file
            .metadata()
            .map_err(|source| LocalModelConfigError::Io {
                path: path.to_path_buf(),
                source,
            })?;
        if !metadata.is_file()
            || metadata.permissions().mode() & 0o077 != 0
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(LocalModelConfigError::CredentialUnavailable(
                "protected credential has unsafe ownership, type, or permissions".to_string(),
            ));
        }
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > 1024 * 1024 {
        return Err(LocalModelConfigError::CredentialUnavailable(
            "protected credential exceeds the 1 MiB limit".to_string(),
        ));
    }
    Ok(bytes)
}

fn validate_environment_name(name: &str) -> Result<(), LocalModelConfigError> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if !valid || name.len() > 128 {
        return Err(LocalModelConfigError::Invalid {
            field: "environment credential name",
            reason: "must be a portable environment variable name".to_string(),
        });
    }
    Ok(())
}

fn validate_base_url(value: &str) -> Result<(), LocalModelConfigError> {
    validate_component("base URL", value)?;
    let parsed = url::Url::parse(value).map_err(|_| LocalModelConfigError::Invalid {
        field: "base URL",
        reason: "must be an absolute HTTP(S) URL".to_string(),
    })?;
    let loopback = parsed.host().is_some_and(|host| match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    });
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(LocalModelConfigError::Invalid {
            field: "base URL",
            reason: "must use HTTPS, except for an explicit loopback endpoint".to_string(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(LocalModelConfigError::Invalid {
            field: "base URL",
            reason: "must not contain userinfo or a fragment".to_string(),
        });
    }
    for (name, _) in parsed.query_pairs() {
        let name = name.to_ascii_lowercase();
        if [
            "key",
            "token",
            "auth",
            "signature",
            "password",
            "credential",
        ]
        .iter()
        .any(|sensitive| name.contains(sensitive))
        {
            return Err(LocalModelConfigError::Invalid {
                field: "base URL",
                reason: "credential-like query parameters must use the credential backend"
                    .to_string(),
            });
        }
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File, LocalModelConfigError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn write_atomic_private(path: &Path, body: &[u8]) -> Result<(), LocalModelConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| LocalModelConfigError::Invalid {
            field: "models path",
            reason: "must have a parent directory".to_string(),
        })?;
    // A unique create-new file prevents a pre-planted `.tmp` symlink or
    // permissive inode from redirecting or weakening secret-adjacent config.
    let mut temporary = tempfile::Builder::new()
        .prefix(".astra-models-")
        .tempfile_in(parent)
        .map_err(|source| LocalModelConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(body)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| LocalModelConfigError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| LocalModelConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    Ok(())
}

fn write_new_private(path: &Path, body: &[u8]) -> Result<(), LocalModelConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| LocalModelConfigError::Invalid {
            field: "secret path",
            reason: "must have a parent directory".to_string(),
        })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".astra-secret-")
        .tempfile_in(parent)
        .map_err(|source| LocalModelConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(body)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| LocalModelConfigError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| LocalModelConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn model_directory_symlink_is_rejected_without_changing_its_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("unrelated");
        fs::create_dir(&target).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
        let alias = directory.path().join("models-alias");
        symlink(&target, &alias).unwrap();
        assert!(super::ensure_private_directory(&alias).is_err());
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    use super::*;

    fn model(credential: LocalCredentialRef) -> LocalModelDefinition {
        LocalModelDefinition {
            protocol: LocalInferenceProtocol::OpenaiCompatible,
            base_url: "https://provider.example/v1".to_string(),
            model: "coding-model".to_string(),
            binding_revision: 1,
            context_window: 128_000,
            max_output_tokens: 8_192,
            credential,
            probe: LocalModelProbeState::default(),
        }
    }

    #[test]
    fn configuration_serializes_only_credential_reference() {
        let mut config = LocalModelConfig::default();
        config.models.insert(
            "work".to_string(),
            model(LocalCredentialRef::Environment {
                name: "WORK_LLM_API_KEY".to_string(),
            }),
        );
        let resolved =
            ResolvedLocalCredential::from_environment(&config.models["work"].credential, |_| {
                Some("provider-secret-canary".to_string())
            })
            .expect("resolve current attachment")
            .expect("secret is present");
        assert_eq!(
            resolved.expose_to_local_transport(),
            "provider-secret-canary"
        );
        let json = serde_json::to_string(&config).expect("serialize config");
        assert!(json.contains("WORK_LLM_API_KEY"));
        assert!(!json.contains("provider-secret-canary"));
        assert!(serde_json::from_str::<LocalModelConfig>(&json).is_ok());
        let inline = r#"{"version":1,"revision":0,"models":{"work":{"protocol":"openai_compatible","base_url":"https://provider.example/v1","model":"coding-model","context_window":128000,"max_output_tokens":8192,"credential":{"kind":"environment","name":"WORK_LLM_API_KEY","value":"provider-secret-canary"}}}}"#;
        assert!(serde_json::from_str::<LocalModelConfig>(inline).is_err());
    }

    #[test]
    fn pre_probe_configurations_default_to_not_run_evidence() {
        let legacy = r#"{"version":1,"revision":1,"models":{"work":{"protocol":"openai_compatible","base_url":"https://provider.example/v1","model":"coding-model","binding_revision":1,"context_window":128000,"max_output_tokens":8192,"credential":{"kind":"none"}}}}"#;
        let config: LocalModelConfig = serde_json::from_str(legacy).expect("legacy config");
        assert!(matches!(
            config.models["work"].probe,
            LocalModelProbeState::NotRun
        ));
    }

    #[test]
    fn environment_credentials_are_attachment_scoped_and_secret_safe() {
        let reference = LocalCredentialRef::Environment {
            name: "WORK_LLM_API_KEY".to_string(),
        };
        let first = ResolvedLocalCredential::from_environment(&reference, |name| {
            (name == "WORK_LLM_API_KEY").then(|| "first-terminal-secret".to_string())
        })
        .expect("first terminal resolves")
        .expect("credential present");
        let second = ResolvedLocalCredential::from_environment(&reference, |_| {
            Some("second-terminal-secret".to_string())
        })
        .expect("second terminal resolves")
        .expect("credential present");
        assert_ne!(
            first.expose_to_local_transport(),
            second.expose_to_local_transport()
        );
        for debug in [format!("{first:?}"), format!("{second:?}")] {
            assert!(!debug.contains("terminal-secret"));
            assert!(debug.contains("present: true"));
        }
    }

    #[test]
    fn credential_fingerprint_is_stable_and_does_not_reveal_material() {
        let key = ResolvedLocalCredential::from_environment(
            &LocalCredentialRef::Environment {
                name: "ASTRA_PROBE_KEY".to_string(),
            },
            |_| Some("owner-local-hmac-key".to_string()),
        )
        .unwrap()
        .unwrap();
        let first = ResolvedLocalCredential::from_environment(
            &LocalCredentialRef::Environment {
                name: "WORK_LLM_API_KEY".to_string(),
            },
            |_| Some("first-terminal-secret".to_string()),
        )
        .unwrap()
        .unwrap();
        let same = ResolvedLocalCredential::from_environment(
            &LocalCredentialRef::Environment {
                name: "WORK_LLM_API_KEY".to_string(),
            },
            |_| Some("first-terminal-secret".to_string()),
        )
        .unwrap()
        .unwrap();
        let rotated = ResolvedLocalCredential::from_environment(
            &LocalCredentialRef::Environment {
                name: "WORK_LLM_API_KEY".to_string(),
            },
            |_| Some("rotated-terminal-secret".to_string()),
        )
        .unwrap()
        .unwrap();

        let first_fingerprint = first.fingerprint_with_key(&key);
        assert_eq!(first_fingerprint, same.fingerprint_with_key(&key));
        assert_ne!(first_fingerprint, rotated.fingerprint_with_key(&key));
        assert!(!first_fingerprint.contains("first-terminal-secret"));
        assert_ne!(
            first_fingerprint,
            ResolvedLocalCredential::no_credential_fingerprint()
        );
    }

    #[test]
    #[cfg(unix)]
    fn probe_fingerprint_key_is_created_once_in_private_storage() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalSecretStore::with_root(root.path().join("model-secrets"));
        let first = store
            .ensure_probe_fingerprint_key()
            .unwrap()
            .expect("supported platforms create a probe key");
        let second = store
            .resolve_probe_fingerprint_key()
            .unwrap()
            .expect("the key remains available");
        assert_eq!(
            first.expose_to_local_transport(),
            second.expose_to_local_transport()
        );
        assert_eq!(first.expose_to_local_transport().len(), 32);
        assert!(
            root.path()
                .join("model-secrets")
                .join(PROBE_FINGERPRINT_KEY_ID)
                .is_file()
        );
    }

    #[test]
    #[cfg(not(unix))]
    fn probe_fingerprint_key_fails_closed_without_protected_storage() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalSecretStore::with_root(root.path().join("model-secrets"));
        assert!(matches!(
            store.ensure_probe_fingerprint_key(),
            Err(LocalModelConfigError::CredentialUnavailable(_))
        ));
    }

    #[test]
    fn invalid_candidate_and_stale_revision_preserve_previous_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = LocalModelConfigStore::with_path(root.path().join("models.json"));
        let mut first = LocalModelConfig::default();
        first
            .models
            .insert("work".to_string(), model(LocalCredentialRef::None));
        let applied = store.replace(0, first).expect("apply first revision");
        assert_eq!(applied.revision, 1);

        let mut invalid = applied.clone();
        invalid.models.get_mut("work").unwrap().base_url = "http://provider.example/v1".to_string();
        assert!(store.replace(1, invalid).is_err());
        assert_eq!(store.load().expect("load after invalid"), applied);

        let mut stale = applied.clone();
        stale.models.get_mut("work").unwrap().model = "other".to_string();
        assert!(matches!(
            store.replace(0, stale),
            Err(LocalModelConfigError::RevisionConflict {
                expected: 0,
                actual: 1
            })
        ));
        assert_eq!(store.load().expect("load after conflict"), applied);
    }

    #[test]
    fn deleting_and_recreating_a_model_never_reuses_its_generation() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalModelConfigStore::with_path(root.path().join("models.json"));
        let mut first = LocalModelConfig::default();
        first
            .models
            .insert("work".into(), model(LocalCredentialRef::None));
        first.models.get_mut("work").unwrap().binding_revision = 100;
        let first = store.replace(0, first).unwrap();
        let empty = store
            .replace(first.revision, LocalModelConfig::default())
            .unwrap();
        let mut recreated = LocalModelConfig::default();
        recreated
            .models
            .insert("work".into(), model(LocalCredentialRef::None));
        let recreated = store.replace(empty.revision, recreated).unwrap();
        assert!(recreated.models["work"].binding_revision > first.models["work"].binding_revision);
        assert!(recreated.revision >= recreated.models["work"].binding_revision);
    }

    #[test]
    fn probe_evidence_does_not_change_provider_binding_revision() {
        let root = tempfile::tempdir().unwrap();
        let store = LocalModelConfigStore::with_path(root.path().join("models.json"));
        let mut config = LocalModelConfig::default();
        config
            .models
            .insert("work".into(), model(LocalCredentialRef::None));
        let first = store.replace(0, config).unwrap();
        let binding_revision = first.models["work"].binding_revision;
        let mut with_probe = first.clone();
        with_probe.models.get_mut("work").unwrap().probe = LocalModelProbeState::Passed {
            checked_at_unix_ms: 1,
            credential_fingerprint: Some(ResolvedLocalCredential::no_credential_fingerprint()),
        };
        let recorded = store.replace(first.revision, with_probe).unwrap();
        assert_eq!(recorded.models["work"].binding_revision, binding_revision);
        assert!(recorded.revision > first.revision);
        assert!(matches!(
            recorded.models["work"].probe,
            LocalModelProbeState::Passed {
                checked_at_unix_ms: 1,
                credential_fingerprint: Some(_),
            }
        ));
    }

    #[test]
    fn leased_snapshot_fences_concurrent_revision_replacement() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("models.json");
        let store = LocalModelConfigStore::with_path(path.clone());
        let mut first = LocalModelConfig::default();
        first
            .models
            .insert("work".to_string(), model(LocalCredentialRef::None));
        let applied = store.replace(0, first).expect("first revision");
        let lease = store.lease().expect("lease first revision");
        assert_eq!(lease.config().revision, 1);

        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut next = applied;
        next.models.get_mut("work").unwrap().model = "next-model".to_string();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let result = LocalModelConfigStore::with_path(path).replace(1, next);
            done_tx.send(result).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        drop(lease);
        assert_eq!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("writer unblocked")
                .expect("second revision")
                .revision,
            2
        );
        writer.join().unwrap();
    }

    #[test]
    fn config_rejects_unknown_fields_and_unsafe_sources() {
        let unknown = r#"{"version":1,"revision":0,"models":{},"secret":"leak"}"#;
        assert!(serde_json::from_str::<LocalModelConfig>(unknown).is_err());
        assert!(
            model(LocalCredentialRef::Environment {
                name: "BAD-NAME".to_string()
            })
            .validate()
            .is_err()
        );
        let mut unsafe_model = model(LocalCredentialRef::None);
        unsafe_model.base_url = "https://user:secret@provider.example/v1".to_string();
        assert!(unsafe_model.validate().is_err());
    }

    #[test]
    fn invalid_environment_reference_never_reaches_the_environment_reader() {
        let reference = LocalCredentialRef::Environment {
            name: "INVALID-NAME".to_string(),
        };
        let mut reads = 0;
        assert!(
            ResolvedLocalCredential::from_environment(&reference, |_| {
                reads += 1;
                Some("must-not-be-read".to_string())
            })
            .is_err()
        );
        assert_eq!(reads, 0);
    }

    #[test]
    fn private_configuration_debug_omits_endpoint_and_reference_details() {
        let definition = LocalModelDefinition {
            base_url: "https://private.example/v1?account=secret-account".to_string(),
            credential: LocalCredentialRef::SystemKeychain {
                service: "secret-service".to_string(),
                account: "secret-account".to_string(),
            },
            ..model(LocalCredentialRef::None)
        };
        let mut config = LocalModelConfig::default();
        config
            .models
            .insert("secret-alias".to_string(), definition.clone());
        for debug in [
            format!("{definition:?}"),
            format!("{:?}", definition.credential),
            format!("{config:?}"),
        ] {
            for secret in [
                "private.example",
                "secret-service",
                "secret-account",
                "secret-alias",
            ] {
                assert!(!debug.contains(secret));
            }
        }
    }

    #[test]
    fn malformed_private_json_error_is_content_free() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("models.json");
        let canary = "private-provider-token-canary";
        fs::write(
            &path,
            format!(
                r#"{{"version":1,"revision":0,"models":{{"work":{{"protocol":"{canary}","base_url":"https://provider.example/v1","model":"m","context_window":128000,"max_output_tokens":8192,"credential":{{"kind":"none"}}}}}}}}"#
            ),
        )
        .unwrap();
        let raw = serde_json::from_str::<LocalModelConfig>(&fs::read_to_string(&path).unwrap())
            .unwrap_err();
        assert!(
            raw.to_string().contains(canary),
            "fixture must prove raw leak"
        );
        let error = LocalModelConfigStore::with_path(path).load().unwrap_err();
        assert!(!error.to_string().contains(canary));
        assert!(!format!("{error:?}").contains(canary));
    }

    #[test]
    fn credential_like_url_queries_are_rejected() {
        let mut definition = model(LocalCredentialRef::None);
        definition.base_url = "https://provider.example/v1?api-version=2026-01-01".to_string();
        definition.validate().expect("non-secret provider query");
        definition.base_url = "https://provider.example/v1?api_key=secret".to_string();
        assert!(definition.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_never_follows_a_preexisting_temp_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("tempdir");
        let victim = root.path().join("victim");
        fs::write(&victim, "do-not-touch").expect("write victim");
        symlink(&victim, root.path().join("models.json.tmp")).expect("plant preexisting temp link");
        let store = LocalModelConfigStore::with_path(root.path().join("models.json"));
        store
            .replace(0, LocalModelConfig::default())
            .expect("unique temporary file ignores planted link");
        assert_eq!(fs::read_to_string(victim).unwrap(), "do-not-touch");
    }

    #[cfg(unix)]
    #[test]
    fn models_file_is_private_despite_a_permissive_preexisting_temp_file() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let root = tempfile::tempdir().expect("tempdir");
        let preexisting = root.path().join("models.json.tmp");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&preexisting)
            .expect("create permissive preexisting temp");
        fs::set_permissions(&preexisting, fs::Permissions::from_mode(0o666)).unwrap();
        let path = root.path().join("models.json");
        LocalModelConfigStore::with_path(path.clone())
            .replace(0, LocalModelConfig::default())
            .expect("write private config");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(preexisting).unwrap().permissions().mode() & 0o777,
            0o666
        );
    }

    #[test]
    fn protected_secret_roundtrips_without_entering_configuration() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = LocalSecretStore::with_root(root.path().join("secrets"));
        let reference = LocalCredentialRef::ProtectedFile {
            secret_id: "model_01".to_string(),
        };
        store
            .put("model_01", "private-provider-canary")
            .expect("store secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(root.path().join("secrets"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(root.path().join("secrets/model_01"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let resolved = store
            .resolve(&reference)
            .expect("resolve secret")
            .expect("secret is present");
        assert_eq!(
            resolved.expose_to_local_transport(),
            "private-provider-canary"
        );
        assert!(!format!("{resolved:?}").contains("private-provider-canary"));
        assert!(store.remove("model_01").expect("remove secret"));
        assert!(!store.remove("model_01").expect("idempotent remove"));
    }

    #[test]
    fn protected_secret_generation_is_immutable() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = LocalSecretStore::with_root(root.path().join("secrets"));
        store.put("generation_1", "first").unwrap();
        assert!(store.put("generation_1", "second").is_err());
        assert_eq!(
            store
                .resolve(&LocalCredentialRef::ProtectedFile {
                    secret_id: "generation_1".to_string(),
                })
                .unwrap()
                .unwrap()
                .expose_to_local_transport(),
            "first"
        );
    }

    #[test]
    fn protected_secret_id_cannot_escape_its_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = LocalSecretStore::with_root(root.path().join("secrets"));
        for id in ["../outside", "nested/value", ".", "with space", ""] {
            assert!(
                store.put(id, "canary").is_err(),
                "accepted unsafe id {id:?}"
            );
        }
        assert!(!root.path().join("outside").exists());
    }

    #[cfg(unix)]
    #[test]
    fn protected_secret_rejects_symlinks_and_permissive_files() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().expect("tempdir");
        let secrets = root.path().join("secrets");
        fs::create_dir(&secrets).unwrap();
        let store = LocalSecretStore::with_root(secrets.clone());

        let outside = root.path().join("outside");
        fs::write(&outside, "symlink-canary").unwrap();
        symlink(&outside, secrets.join("linked")).unwrap();
        assert!(
            store
                .resolve(&LocalCredentialRef::ProtectedFile {
                    secret_id: "linked".to_string()
                })
                .is_err()
        );

        let permissive = secrets.join("permissive");
        fs::write(&permissive, "mode-canary").unwrap();
        fs::set_permissions(&permissive, fs::Permissions::from_mode(0o644)).unwrap();
        let error = store
            .resolve(&LocalCredentialRef::ProtectedFile {
                secret_id: "permissive".to_string(),
            })
            .unwrap_err();
        assert!(!error.to_string().contains("mode-canary"));
    }
}
