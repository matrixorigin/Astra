use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IsolationIntent {
    /// No isolation — runs directly in-process.
    None,
    /// Isolate via a child process.
    Process,
    /// Isolate via a container (Docker, etc.).
    Container,
    /// General sandbox isolation (Firecracker microVM, gVisor, etc.).
    Sandbox,
    /// Strict gVisor-based isolation.
    GVisor,
    /// Let the provider decide the isolation level.
    ///
    /// This is a delegation of authority, not an isolation level — it is
    /// incomparable with the concrete levels above.  A `ProviderEnforced`
    /// policy does NOT satisfy any concrete isolation requirement.
    ProviderEnforced,
}

impl IsolationIntent {
    /// Rank for concrete isolation levels (lower = less isolation).
    fn ordinal(self) -> u8 {
        match self {
            IsolationIntent::None => 0,
            IsolationIntent::Process => 1,
            IsolationIntent::Container => 2,
            IsolationIntent::Sandbox => 3,
            IsolationIntent::GVisor => 4,
            IsolationIntent::ProviderEnforced => 5,
        }
    }

    /// Returns true if `self` is at least as strict as `required`.
    ///
    /// `ProviderEnforced` never satisfies any concrete level because it
    /// delegates the decision to the provider at runtime.
    pub fn satisfies(self, required: IsolationIntent) -> bool {
        // ProviderEnforced delegates authority — it guarantees nothing.
        if matches!(self, IsolationIntent::ProviderEnforced)
            || matches!(required, IsolationIntent::ProviderEnforced)
        {
            return self == required;
        }
        self.ordinal() >= required.ordinal()
    }
}

impl PartialOrd for IsolationIntent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for IsolationIntent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.ordinal().cmp(&other.ordinal())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemPolicy {
    NoAccess,
    ReadOnlyWorkspace,
    ReadWriteWorkspace,
    ExplicitAllowList,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPolicy {
    Disabled,
    AllowList,
    Open,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialPolicy {
    Disabled,
    UserApproved,
    ScopedInjection,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Never,
    OnRisk,
    Always,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ResourceLimits {
    #[serde(default)]
    pub max_execution_secs: Option<f64>,
    #[serde(default)]
    pub max_output_bytes: Option<usize>,
    #[serde(default)]
    pub max_background_session_secs: Option<f64>,
}

impl ResourceLimits {
    pub fn interactive() -> Self {
        Self {
            max_execution_secs: Some(30.0),
            max_output_bytes: Some(1_048_576),
            max_background_session_secs: None,
        }
    }

    pub fn long_session() -> Self {
        Self {
            max_execution_secs: Some(300.0),
            max_output_bytes: Some(8_388_608),
            max_background_session_secs: Some(86_400.0),
        }
    }

    /// Validate that all f64 fields are finite (not NaN or ±∞).
    /// Returns `self` on success, or an error describing the invalid field.
    pub fn validate_finite(self) -> Result<Self, &'static str> {
        if let Some(v) = self.max_execution_secs
            && !v.is_finite()
        {
            return Err("max_execution_secs must be finite");
        }
        if let Some(v) = self.max_background_session_secs
            && !v.is_finite()
        {
            return Err("max_background_session_secs must be finite");
        }
        Ok(self)
    }
}

impl PartialEq for ResourceLimits {
    fn eq(&self, other: &Self) -> bool {
        let f64_eq = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (None, None) => true,
            (Some(x), Some(y)) => {
                // NaN == NaN is false in IEEE754, but for structural equality
                // we treat NaN as equal to NaN.
                if x.is_nan() && y.is_nan() {
                    return true;
                }
                x == y
            }
            _ => false,
        };
        f64_eq(self.max_execution_secs, other.max_execution_secs)
            && self.max_output_bytes == other.max_output_bytes
            && f64_eq(
                self.max_background_session_secs,
                other.max_background_session_secs,
            )
    }
}

impl<'de> Deserialize<'de> for ResourceLimits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            max_execution_secs: Option<f64>,
            #[serde(default)]
            max_output_bytes: Option<usize>,
            #[serde(default)]
            max_background_session_secs: Option<f64>,
        }
        let raw = Raw::deserialize(deserializer)?;
        // Build manually so we can validate finiteness.
        let limits = ResourceLimits {
            max_execution_secs: raw.max_execution_secs,
            max_output_bytes: raw.max_output_bytes,
            max_background_session_secs: raw.max_background_session_secs,
        };
        limits.validate_finite().map_err(serde::de::Error::custom)
    }
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self::interactive()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuditPolicy {
    pub record_invocations: bool,
    pub record_artifacts: bool,
    pub record_denials: bool,
}

impl AuditPolicy {
    pub fn required() -> Self {
        Self {
            record_invocations: true,
            record_artifacts: true,
            record_denials: true,
        }
    }
}

impl Default for AuditPolicy {
    fn default() -> Self {
        Self::required()
    }
}

// ---------------------------------------------------------------------------
// ToolName — validated, trimmed tool name newtype
// ---------------------------------------------------------------------------

/// A validated, trimmed tool name.
///
/// Construction trims whitespace and rejects empty strings.
/// `"*"` is reserved as the wildcard sentinel and is rejected —
/// use `has_restricted_tool_allowlist()` to detect open-access policies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(from = "String", into = "String")]
pub struct ToolName(String);

impl ToolName {
    /// Create a new tool name, trimming whitespace.
    ///
    /// Returns `None` if the trimmed name is empty or `"*"`.
    pub fn new(name: impl Into<String>) -> Option<Self> {
        let trimmed = name.into().trim().to_string();
        if trimmed.is_empty() || trimmed == "*" {
            return None;
        }
        Some(Self(trimmed))
    }

    /// The inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consume and return the inner string.
    pub fn into_inner(self) -> String {
        self.0
    }
}

impl From<ToolName> for String {
    fn from(t: ToolName) -> Self {
        t.0
    }
}

impl From<String> for ToolName {
    /// Deserialization guard.  Returns an empty `ToolName` for invalid input
    /// so deserialization can surface an error message; serialization of
    /// such a sentinel is undefined.  Prefer `ToolName::new()`.
    fn from(raw: String) -> Self {
        // When called from serde deserialization, serde validates after
        // the conversion — see the Deserialize impl below.
        Self(raw)
    }
}

impl<'de> Deserialize<'de> for ToolName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        ToolName::new(&raw).ok_or_else(|| {
            serde::de::Error::invalid_value(
                serde::de::Unexpected::Str(&raw),
                &"a non-empty tool name that is not \"*\"",
            )
        })
    }
}

impl std::fmt::Display for ToolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for ToolName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// PolicyIntent
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PolicyIntent {
    pub isolation: IsolationIntent,
    pub filesystem: FilesystemPolicy,
    pub network: NetworkPolicy,
    pub credentials: CredentialPolicy,
    pub approval: ApprovalPolicy,
    pub allowed_tools: Vec<ToolName>,
    pub resources: ResourceLimits,
    pub audit: AuditPolicy,
}

impl PolicyIntent {
    pub fn cloud_control_plane() -> Self {
        Self {
            isolation: IsolationIntent::ProviderEnforced,
            filesystem: FilesystemPolicy::NoAccess,
            network: NetworkPolicy::AllowList,
            credentials: CredentialPolicy::Disabled,
            approval: ApprovalPolicy::OnRisk,
            allowed_tools: Vec::new(),
            resources: ResourceLimits::interactive(),
            audit: AuditPolicy::required(),
        }
    }

    pub fn local_developer() -> Self {
        Self {
            isolation: IsolationIntent::Process,
            filesystem: FilesystemPolicy::ReadWriteWorkspace,
            network: NetworkPolicy::Open,
            credentials: CredentialPolicy::UserApproved,
            approval: ApprovalPolicy::OnRisk,
            allowed_tools: Vec::new(),
            resources: ResourceLimits::long_session(),
            audit: AuditPolicy::required(),
        }
    }

    pub fn read_only_review() -> Self {
        Self {
            isolation: IsolationIntent::ProviderEnforced,
            filesystem: FilesystemPolicy::ReadOnlyWorkspace,
            network: NetworkPolicy::AllowList,
            credentials: CredentialPolicy::Disabled,
            approval: ApprovalPolicy::OnRisk,
            allowed_tools: Vec::new(),
            resources: ResourceLimits::interactive(),
            audit: AuditPolicy::required(),
        }
    }

    pub fn strict_runner() -> Self {
        Self {
            isolation: IsolationIntent::GVisor,
            filesystem: FilesystemPolicy::ReadWriteWorkspace,
            network: NetworkPolicy::AllowList,
            credentials: CredentialPolicy::ScopedInjection,
            approval: ApprovalPolicy::OnRisk,
            allowed_tools: Vec::new(),
            resources: ResourceLimits::long_session(),
            audit: AuditPolicy::required(),
        }
    }

    pub fn with_allowed_tools<I, S>(mut self, tools: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_tools = tools.into_iter().filter_map(ToolName::new).collect();
        self
    }

    pub fn has_restricted_tool_allowlist(&self) -> bool {
        !self.allowed_tools.is_empty()
    }

    pub fn allows_tool(&self, tool_name: &str) -> bool {
        if !self.has_restricted_tool_allowlist() {
            return true;
        }
        self.allowed_tools.iter().any(|t| t.as_str() == tool_name)
    }

    pub fn disallowed_tool_reason(tool_name: &str) -> String {
        format!("tool '{tool_name}' is not in allowed_tools")
    }
}

impl Default for PolicyIntent {
    fn default() -> Self {
        Self::cloud_control_plane()
    }
}
