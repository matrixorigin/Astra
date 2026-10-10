//! Selected CLI transport for one canonical child-run stage. Native thread IDs
//! are evidence returned to the shared run owner, not a CLI session registry.
//! The process owner controls physical cancellation/settlement; only Codex's
//! matching `turn/completed` notification establishes a native terminal result.

use super::{ApprovedNativeRuntime, ToolExecutor};
use astra_edge::EdgeInvocationInput;
use astra_sandbox::{
    BashInvocationOwner, FramedProcess, FramedProcessEnd, FramedProcessInput, FramedProcessLimits,
};
use astra_tools::{ProviderInteractionDecision, ProviderInteractionGate, ToolResult};
use astra_turn_types::{ProviderInteractionRequest, ProviderStageInput, ProviderStageInputAck};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub const TOOL_NAME: &str = "native_codex";
pub(crate) const FRAME_BYTES: usize = 256 * 1024;
pub(crate) const OUTPUT_BYTES: usize = 64 * 1024;
// The canonical interaction contract permits at most one hour per request.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(3600);
const PRE_ACK_EVENTS: usize = 8;
pub(crate) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const MODEL_LIST_REQUEST_ID: i64 = 4;
const INTERRUPT_REQUEST_ID: i64 = 5;
const STEER_REQUEST_ID: i64 = 6;
const ACCOUNT_READ_REQUEST_ID: i64 = 7;
const CONFIG_READ_REQUEST_ID: i64 = 8;
const MODEL_LIST_PAGE_LIMIT: u64 = 64;
const MODEL_LIST_MAX_PAGES: usize = 8;
const MODEL_LIST_MAX_ITEMS: usize = 512;
#[cfg(target_os = "linux")]
const PLATFORM_RUNTIME_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr",
    "/lib",
    "/lib64",
    "/nix/store",
    "/run/current-system/sw",
];

pub(crate) fn native_stage_remaining(
    invocation: astra_tools::tool_engine::ToolInvocationMetadata<'_>,
) -> Result<Duration, &'static str> {
    let deadline = invocation
        .admission_deadline
        .ok_or("native execution requires an admitted stage budget")?;
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    if remaining.is_zero() {
        return Err("native admitted stage budget expired before dispatch");
    }
    Ok(remaining)
}

// Reuse canonical workspace attribution if a consumer drops the invocation
// before collecting settlement. Physical cleanup stays owned by the process
// driver; releasing the async lease must not make that workspace look safe.
pub(crate) struct UnsettledOnDrop(
    pub(crate) Option<astra_tools::workspace_observation::WorkspaceAttributionState>,
);
impl Drop for UnsettledOnDrop {
    fn drop(&mut self) {
        if let Some(state) = &self.0 {
            state.mark_unsettled();
        }
    }
}

/// The provider adapters decide what their protocol considers a successful
/// turn. This owner decides the provider-independent physical facts: whether
/// the child scope settled, whether a successful exit was observed, and
/// whether the bounded post-terminal cancellation completed. Keeping those
/// facts here prevents each adapter from growing a second process lifecycle.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NativeProcessSettlement {
    pub(crate) authoritative: bool,
    pub(crate) exited_successfully: bool,
    pub(crate) cancelled_after_terminal: bool,
    pub(crate) target_released: Option<bool>,
}

impl NativeProcessSettlement {
    pub(crate) fn transport_settled(self, accept_cancelled: bool) -> bool {
        self.exited_successfully || (accept_cancelled && self.cancelled_after_terminal)
    }
}

pub(crate) async fn settle_native_process(
    process: FramedProcess,
    driven: Result<(), String>,
    token: &CancellationToken,
    unsettled: &mut UnsettledOnDrop,
) -> (Result<(), String>, NativeProcessSettlement) {
    let mut cancelled_after_terminal = false;
    let outcome = if driven.is_ok() {
        // A native terminal event is protocol evidence only. The process owner
        // still closes stdin and waits for authoritative descendant settlement.
        let completion = process.wait();
        tokio::pin!(completion);
        tokio::select! {
            result = &mut completion => result,
            _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
                cancelled_after_terminal = true;
                token.cancel();
                completion.await
            }
        }
    } else {
        process.cancel_and_wait().await
    };
    let authoritative = outcome
        .as_ref()
        .ok()
        .and_then(|outcome| outcome.settlement.as_ref())
        .is_some_and(|settlement| settlement.ownership.is_authoritative());
    if authoritative {
        unsettled.0.take();
    }
    let exited_successfully = outcome.as_ref().is_ok_and(|outcome| {
        matches!(outcome.end, FramedProcessEnd::Exited)
            && outcome.status.is_some_and(|status| status.success())
    });
    let target_released = outcome
        .as_ref()
        .ok()
        .and_then(|outcome| outcome.target_released);
    (
        driven,
        NativeProcessSettlement {
            authoritative,
            exited_successfully,
            cancelled_after_terminal,
            target_released,
        },
    )
}

pub(crate) fn prepare_native_process(
    executable: &std::path::Path,
    args: &[String],
) -> std::io::Result<(std::process::Command, BashInvocationOwner)> {
    let program = executable.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "native executable path is not UTF-8",
        )
    })?;
    // Libtest has no production early-main supervisor entrypoint. The live
    // test may select a freshly built real Astra binary as the existing
    // supervisor helper; it still runs the real target and requires the same
    // authenticated ownership handshake and descendant settlement.
    #[cfg(all(test, target_os = "linux"))]
    if let Some(helper) = std::env::var_os("ASTRA_NATIVE_HARNESS_SUPERVISOR_BIN") {
        return BashInvocationOwner::prepare_with_supervisor_helper(
            helper.into(),
            std::iter::empty::<String>(),
            program,
            args,
        );
    }
    BashInvocationOwner::prepare_framed(program, args)
}

/// Facts for the existing provider descriptor, not permission or a grant.
/// Choosing this provider does not approve these model-readable directories.
pub(crate) fn installed_runtime_requirements()
-> Result<astra_turn_types::ProviderRuntimeRequirements, &'static str> {
    runtime_requirements_for_executable(&native_executable()?)
}

pub(crate) fn runtime_requirements_for_executable(
    executable: &std::path::Path,
) -> Result<astra_turn_types::ProviderRuntimeRequirements, &'static str> {
    let executable = executable
        .canonicalize()
        .map_err(|_| "native executable is unavailable")?;
    if !executable.is_file() || !native_executable_is_runnable(&executable) {
        return Err("native executable is unavailable");
    }
    let mut paths = std::collections::BTreeSet::new();
    paths.insert(
        executable
            .to_str()
            .ok_or("native executable path is not UTF-8")?
            .to_owned(),
    );
    #[cfg(target_os = "linux")]
    for path in PLATFORM_RUNTIME_ROOTS {
        let path = std::path::Path::new(path);
        if path.exists() {
            let canonical = path
                .canonicalize()
                .map_err(|_| "native runtime path is unavailable")?;
            paths.insert(
                canonical
                    .to_str()
                    .ok_or("native runtime path is not UTF-8")?
                    .to_owned(),
            );
        }
    }
    let requirements = astra_turn_types::ProviderRuntimeRequirements {
        executable: executable
            .to_str()
            .ok_or("native executable path is not UTF-8")?
            .to_owned(),
        read_paths: paths.into_iter().collect(),
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        astra_turn_types::PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
        json!(requirements),
    );
    astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(&metadata)
        .map_err(|_| "native runtime requirements exceed the contract bounds")?
        .ok_or("native runtime requirements are missing")
}

/// Identity of the installed artifact used for a published capability. This
/// is not a release/version allowlist: it only detects replacement of the
/// executable that was actually probed, so a new compatible installation can
/// be discovered and negotiated normally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeExecutableIdentity {
    canonical_path: std::path::PathBuf,
    length: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

pub(crate) fn native_executable_identity(
    executable: &std::path::Path,
) -> Result<NativeExecutableIdentity, &'static str> {
    let canonical_path = executable
        .canonicalize()
        .map_err(|_| "native executable is unavailable")?;
    let metadata = std::fs::metadata(&canonical_path)
        .map_err(|_| "native executable metadata is unavailable")?;
    if !metadata.is_file() || !native_executable_is_runnable(&canonical_path) {
        return Err("native executable is unavailable");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(NativeExecutableIdentity {
            canonical_path,
            length: metadata.len(),
            modified: metadata.modified().ok(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        Ok(NativeExecutableIdentity {
            canonical_path,
            length: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

pub(crate) fn validate_runtime_grant(
    granted: &[String],
    expected: &astra_turn_types::ProviderRuntimeRequirements,
    policy: Option<&astra_sandbox::SandboxPolicy>,
) -> Result<(), &'static str> {
    let supplied = astra_turn_types::ProviderRuntimeRequirements {
        executable: expected.executable.clone(),
        read_paths: granted.to_vec(),
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert(
        astra_turn_types::PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
        json!(supplied),
    );
    astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(&metadata)
        .map_err(|_| "native runtime grant exceeds the contract bounds")?;
    let expected_paths: std::collections::BTreeSet<_> = expected.read_paths.iter().collect();
    let granted_paths: std::collections::BTreeSet<_> = granted.iter().collect();
    if granted_paths != expected_paths || !granted_paths.contains(&expected.executable) {
        return Err(
            "native runtime grant does not match the current installed provider requirements",
        );
    }
    let policy = policy.ok_or("native runtime grant requires a selected local sandbox policy")?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .and_then(|path| path.canonicalize().ok());
    for text in granted {
        let path = std::path::Path::new(text);
        if !path.is_absolute()
            || path == std::path::Path::new("/")
            || text.contains(['*', '?', '[', ']', '{', '}', '~', '$'])
            || path.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
            || path.canonicalize().ok().as_deref() != Some(path)
            || home.as_deref() == Some(path)
            || !policy.is_path_allowed(path)
        {
            return Err("native runtime grant exceeds the selected local path authority");
        }
    }
    Ok(())
}

pub(crate) fn native_executable_candidates() -> Vec<std::path::PathBuf> {
    std::env::var_os("PATH")
        .as_deref()
        .map(native_executable_candidates_for_path)
        .unwrap_or_default()
}

pub(crate) fn native_executable_candidates_for_names(names: &[&str]) -> Vec<std::path::PathBuf> {
    std::env::var_os("PATH")
        .as_deref()
        .map(|path| native_executable_candidates_for_path_and_names(path, names))
        .unwrap_or_default()
}

pub(crate) fn native_executable_snapshot() -> Vec<NativeExecutableIdentity> {
    native_executable_candidates()
        .into_iter()
        .filter_map(|path| native_executable_identity(&path).ok())
        .collect()
}

pub(crate) fn native_executable_snapshot_for_names(
    names: &[&str],
) -> Vec<NativeExecutableIdentity> {
    native_executable_candidates_for_names(names)
        .into_iter()
        .filter_map(|path| native_executable_identity(&path).ok())
        .collect()
}

/// Snapshot every installed native client that this CLI can select. The
/// delivery supervisor uses one snapshot for invalidation regardless of which
/// protocol won discovery; it must not have a Codex-only or Claude-only
/// invalidation path.
pub(crate) fn native_provider_executable_snapshot() -> Vec<NativeExecutableIdentity> {
    // Codex is the only native collaborator adapter advertised by this
    // capability. Other protocol translators remain isolated until their
    // provider-specific permission and live-contract tests are complete.
    native_executable_snapshot()
}

fn native_executable() -> Result<std::path::PathBuf, &'static str> {
    native_executable_candidates()
        .into_iter()
        .next()
        .ok_or("native executable is unavailable")
}

fn native_executable_candidates_for_path(path: &std::ffi::OsStr) -> Vec<std::path::PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["codex.exe", "codex.cmd", "codex.bat", "codex"]
    } else {
        &["codex"]
    };
    native_executable_candidates_for_path_and_names(path, names)
}

pub(crate) fn native_executable_candidates_for_path_and_names(
    path: &std::ffi::OsStr,
    names: &[&str],
) -> Vec<std::path::PathBuf> {
    let mut seen = std::collections::HashSet::new();
    std::env::split_paths(path)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .filter_map(|path| {
            let canonical = path.canonicalize().ok()?;
            if canonical.is_file()
                && native_executable_is_runnable(&canonical)
                && seen.insert(canonical.clone())
            {
                Some(canonical)
            } else {
                None
            }
        })
        .collect()
}

fn native_executable_is_runnable(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Project Astra's already-approved path rules into the native provider's
/// managed profile. The provider needs an enforceable child boundary because
/// `SandboxPolicy::sandbox_command` only filters environment/cwd; it is not a
/// filesystem mount. This is a projection of the canonical rules, not a new
/// matcher or a second source of authority.
fn permission_profile(
    cwd: &str,
    write: bool,
    network: bool,
    requirements: &astra_turn_types::ProviderRuntimeRequirements,
) -> Result<Value, &'static str> {
    let id = format!("astra_admitted_{}", uuid::Uuid::new_v4().simple());
    let mut filesystem = serde_json::Map::new();
    // These are literal grant roots, not user-supplied glob expressions.
    // Codex discovers masks from the static prefix; escaping that prefix would
    // change its discovery semantics, so reject unrepresentable boundaries.
    if std::iter::once(cwd)
        .chain(requirements.read_paths.iter().map(String::as_str))
        .any(|root| root.contains(['*', '?', '[', ']', '{', '}', '\\']))
    {
        return Err("native runtime boundary contains glob syntax");
    }
    // Never inherit a root-readable built-in profile. Platform bootstrap is
    // enabled below only after proving it is covered by the runtime grant.
    if cwd == "/" || astra_sandbox::is_never_readable_path(std::path::Path::new(cwd)) {
        return Err("native workspace boundary is not readable by policy");
    }
    filesystem.insert(cwd.into(), json!(if write { "write" } else { "read" }));
    for path in &requirements.read_paths {
        if path == "/" || astra_sandbox::is_sensitive_system_dir(std::path::Path::new(path)) {
            return Err("native runtime boundary contains a sensitive system root");
        }
        filesystem
            .entry(path.clone())
            .or_insert_with(|| json!("read"));
    }

    // Ordinary top-level aliases are canonicalized by Codex. Its platform
    // bootstrap preserves loader paths, but also includes /etc (and may
    // include inherited /proc): those remain denied outside our grant.
    #[cfg(target_os = "linux")]
    let platform_bootstrap = PLATFORM_RUNTIME_ROOTS.iter().all(|root| {
        let path = std::path::Path::new(root);
        !path.exists()
            || path.canonicalize().is_ok_and(|target| {
                requirements
                    .read_paths
                    .iter()
                    .any(|approved| std::path::Path::new(approved) == target)
            })
    });
    #[cfg(target_os = "linux")]
    if platform_bootstrap {
        filesystem.insert(":minimal".into(), json!("read"));
        filesystem.insert("/etc".into(), json!("deny"));
        filesystem.insert("/proc".into(), json!("deny"));
    }

    let rules = astra_sandbox::sensitive_path_rules();
    let mut descendants = std::collections::BTreeSet::new();
    for substring in rules.path_substrings {
        let substring = substring.trim_start_matches('/');
        if !substring.is_empty() {
            descendants.insert(format!("*{substring}*"));
        }
    }
    descendants.extend(
        rules
            .credential_directories
            .iter()
            .map(|marker| case_insensitive_glob_literal(marker.trim_start_matches('/'))),
    );
    let mut entries = descendants.clone();
    entries.extend(
        rules
            .credential_file_names
            .iter()
            .map(|name| (*name).to_owned()),
    );
    let entries = entries.into_iter().collect::<Vec<_>>().join(",");
    let descendants = descendants.into_iter().collect::<Vec<_>>().join(",");
    // The only roots exposed to the provider are the selected workspace and
    // the explicitly captured installed-runtime paths. Scope the canonical
    // sensitive-path rules to those roots, using prefixes accepted by Codex's
    // glob scanner. System-sensitive roots are not exposed at all, so they do
    // not need a broad deny glob that would conflict with platform bootstrap.
    let mut scoped_roots = std::collections::BTreeSet::from([cwd.to_owned()]);
    scoped_roots.extend(requirements.read_paths.iter().cloned());
    #[cfg(target_os = "linux")]
    if platform_bootstrap {
        scoped_roots.extend(PLATFORM_RUNTIME_ROOTS.iter().map(|root| (*root).to_owned()));
    }
    // A parent directory's recursive rules already cover its descendants.
    // Preserve distinct logical aliases, but do not duplicate their canonical
    // subtrees or generate recursive rules beneath an executable file.
    let directory_roots = scoped_roots
        .iter()
        .filter(|root| root.as_str() == cwd || std::path::Path::new(root).is_dir())
        .collect::<Vec<_>>();
    let scoped_roots = directory_roots.iter().filter(|root| {
        root.as_str() == cwd
            || !directory_roots.iter().any(|ancestor| {
                ancestor != *root && std::path::Path::new(root).starts_with(ancestor)
            })
    });
    for root in scoped_roots {
        let root = root.trim_end_matches('/');
        if root.is_empty() || root == "/" {
            return Err("native runtime boundary cannot project a filesystem root");
        }
        filesystem.insert(format!("{root}/**/{{{entries}}}"), json!("deny"));
        filesystem.insert(format!("{root}/**/{{{descendants}}}/**"), json!("deny"));
        for name in rules.credential_file_names {
            // Literal denies survive a later cwd write grant, including when
            // the cwd is beneath another approved runtime root.
            filesystem.insert(format!("{root}/{name}"), json!("deny"));
        }
    }

    let profile = json!({
        "filesystem": filesystem,
        "network": {"enabled": network}
    });
    Ok(json!({
        "profileId": id,
        "config": {
            "default_permissions": id,
            "permissions": {id: profile}
        }
    }))
}

fn case_insensitive_glob_literal(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphabetic() {
                format!("[{}{}]", ch.to_ascii_lowercase(), ch.to_ascii_uppercase())
            } else {
                ch.to_string()
            }
        })
        .collect()
}

fn requested_profile_config(requested: &Value) -> Result<&Value, &'static str> {
    let id = requested
        .get("profileId")
        .and_then(Value::as_str)
        .ok_or("native permission profile identity missing")?;
    requested
        .get("config")
        .and_then(|config| config.pointer(&format!("/permissions/{id}")))
        .ok_or("native permission profile configuration missing")
}

fn expected_profile_sandbox(
    requested: &Value,
    cwd: &str,
) -> Result<(&'static str, bool), &'static str> {
    let profile = requested_profile_config(requested)?;
    if profile.get("extends").is_some() {
        return Err("native permission profile must not inherit provider authority");
    }
    let sandbox_type = match profile
        .get("filesystem")
        .and_then(Value::as_object)
        .and_then(|filesystem| filesystem.get(cwd))
        .and_then(Value::as_str)
    {
        Some("read") => "readOnly",
        Some("write") => "workspaceWrite",
        _ => return Err("native permission profile workspace authority missing"),
    };
    let network = profile
        .pointer("/network/enabled")
        .and_then(Value::as_bool)
        .ok_or("native permission profile network setting missing")?;
    Ok((sandbox_type, network))
}

/// Verify the installed provider by using its structured protocol, not by
/// binding capability to a particular CLI release. Version output is useful
/// telemetry, but only an accepted `initialize` response proves that this
/// executable speaks the protocol Astra is about to use.
async fn verify_installed_protocol(
    executable: &std::path::Path,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
    deadline: std::time::Instant,
) -> Result<Option<astra_turn_types::ProviderModelCatalog>, &'static str> {
    let started = std::time::Instant::now();
    let timeout = deadline
        .saturating_duration_since(std::time::Instant::now())
        .min(Duration::from_secs(2));
    if timeout.is_zero() {
        return Err("native protocol probe deadline expired");
    }
    let (mut command, owner) = prepare_native_process(executable, &["app-server".into()])
        .map_err(|_| "native protocol probe ownership unavailable")?;
    command.current_dir(cwd);
    let mut process = owner
        .spawn_framed(
            command,
            FramedProcessLimits {
                max_frame_bytes: FRAME_BYTES,
                max_queued_frames: 4,
                max_stderr_bytes: 4096,
                // The physical owner covers the whole discovery, not only
                // handshake/authentication. Each phase remains bounded below.
                timeout: deadline.saturating_duration_since(std::time::Instant::now()),
            },
            cancel.child_token(),
        )
        .map_err(|_| "native protocol probe unavailable")?;

    let mut evidence = Evidence::default();
    let input = process.input();
    let probe = match tokio::time::timeout(timeout, async {
        initialize_protocol(
            &mut process,
            &input,
            &mut evidence,
            OUTPUT_BYTES,
            None,
            cancel,
        )
        .await?;
        verify_provider_authentication(&mut process, &input, &mut evidence, cancel, OUTPUT_BYTES)
            .await
    })
    .await
    {
        Ok(probe) => probe,
        Err(_) => Err("native protocol probe deadline expired"),
    };
    let model_catalog = if probe.is_ok() && !cancel.is_cancelled() {
        let catalog_timeout = deadline.saturating_duration_since(std::time::Instant::now());
        if catalog_timeout.is_zero() {
            Some(astra_turn_types::ProviderModelCatalog::unavailable())
        } else {
            match tokio::time::timeout(
                catalog_timeout,
                fetch_model_catalog(&mut process, &input, &mut evidence, OUTPUT_BYTES, cancel),
            )
            .await
            {
                Ok(Ok(catalog)) => Some(catalog),
                failure => {
                    tracing::debug!(
                        phase = "model_catalog",
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        remaining_ms = deadline
                            .saturating_duration_since(std::time::Instant::now())
                            .as_millis() as u64,
                        timed_out = failure.is_err(),
                        cancelled = cancel.is_cancelled(),
                        provider_error_code = evidence.provider_error_code,
                        provider_error_class = evidence.provider_error_class,
                        "native discovery did not obtain a validated catalog"
                    );
                    Some(astra_turn_types::ProviderModelCatalog::unavailable())
                }
            }
        }
    } else {
        None
    };
    // `FramedProcess` owns the physical cleanup deadline and descendant
    // settlement. Do not wrap this future in another timeout: dropping the
    // join future here would abandon the only owner that can prove cleanup,
    // allowing the next PATH candidate to be probed while the old process is
    // still alive.
    let outcome = process
        .cancel_and_wait()
        .await
        .map_err(|_| "native protocol probe settlement unavailable")?;
    tracing::debug!(
        phase = "discovery_settlement",
        elapsed_ms = started.elapsed().as_millis() as u64,
        remaining_ms = deadline
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as u64,
        protocol_authenticated = probe.is_ok(),
        catalog_available = model_catalog
            .as_ref()
            .is_some_and(|catalog| catalog.is_complete()),
        authoritative = outcome
            .settlement
            .as_ref()
            .is_some_and(|settlement| settlement.ownership.is_authoritative()),
        "native discovery settled"
    );
    if probe.is_err()
        || !outcome
            .settlement
            .is_some_and(|settlement| settlement.ownership.is_authoritative())
    {
        return Err(probe
            .err()
            .unwrap_or("native protocol probe did not settle"));
    }
    Ok(model_catalog)
}

/// Provider-owned schema for the selected CLI edge, not a built-in tool or an
/// Offering. Only an admitted capability may advertise this contract.
pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": TOOL_NAME,
        "description": "Execute one admitted native Codex collaborator stage in the selected CLI workspace. If supplied, model must be the exact provider selector, declared alias, or declared display name from the current model/list catalog; never guess a partial name. Omit it to use the provider default. Resume only an exact acknowledged native_session_id. Run/control/deadline authority comes from the invocation, never arguments.",
            "parameters": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "task": {"type": "string", "minLength": 1, "maxLength": 65536},
                    "anchor_run_id": {"type": "string", "minLength": 1, "maxLength": 256},
                    "native_session_id": {"type": "string", "minLength": 1, "maxLength": 256},
                    "model": {"type": "string", "minLength": 1, "maxLength": 256},
                    "effort": {"type": "string", "enum": ["none", "minimal", "low", "medium", "high", "xhigh"]}
                },
                "required": ["task", "anchor_run_id"]
            }
        }
    })
}

fn provider_declaration(
    requirements: astra_turn_types::ProviderRuntimeRequirements,
    model_catalog: Option<astra_turn_types::ProviderModelCatalog>,
) -> Result<astra_turn_types::ProviderToolDeclaration, astra_turn_types::ProviderContractError> {
    let schema = schema();
    let mut extension_fields = serde_json::Map::new();
    extension_fields.insert(
        astra_turn_types::PROVIDER_RUNTIME_REQUIREMENTS_KEY.into(),
        json!(requirements),
    );
    extension_fields.insert(
        astra_turn_types::PROVIDER_COLLABORATOR_STAGE_KEY.into(),
        json!(true),
    );
    extension_fields.insert(
        astra_turn_core::provider_resolution::NativeCollaboratorProtocol::EXTENSION_KEY.into(),
        json!(
            astra_turn_core::provider_resolution::NativeCollaboratorProtocol::CodexAppServer
                .extension_value()
        ),
    );
    if let Some(catalog) = model_catalog {
        extension_fields.insert(
            astra_turn_types::PROVIDER_MODEL_CATALOG_KEY.into(),
            json!(catalog),
        );
    }
    astra_turn_types::ProviderRuntimeRequirements::from_extension_fields(&extension_fields)?;
    let declaration = astra_turn_types::ProviderToolDeclaration {
        native_tool_id: astra_turn_types::NativeToolId::new(TOOL_NAME)?,
        native_tool_name: TOOL_NAME.into(),
        stable_tool_alias: Some(astra_turn_types::StableToolAlias::new(TOOL_NAME)?),
        title: Some("Native Codex collaborator".into()),
        description: schema["function"]["description"]
            .as_str()
            .map(str::to_owned),
        input_schema: schema["function"]["parameters"].clone(),
        output_schema: None,
        // A collaborator stage is not an ordinary read-only tool. Its
        // workspace effect is fixed by the invocation's execution ceiling;
        // claiming read-only here would incorrectly make a writable stage
        // cacheable and approval-free.
        claims: Default::default(),
        // This declaration is an agent-stage capacity, not an ordinary tool.
        // The shared runtime uses this typed fact to expose it in the
        // provider-owned collaborator directory without name matching.
        task_support: astra_turn_types::ProviderTaskSupport::Required,
        extension_fields,
    };
    declaration.validate()?;
    Ok(declaration)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stage {
    task: String,
    anchor_run_id: String,
    native_session_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

impl Stage {
    fn parse(args: &Value) -> Result<Self, &'static str> {
        let stage: Self =
            serde_json::from_value(args.clone()).map_err(|_| "invalid native stage arguments")?;
        if !valid_id(&stage.anchor_run_id)
            || stage.task.trim().is_empty()
            || stage.task.len() > OUTPUT_BYTES
            || [&stage.native_session_id, &stage.model]
                .into_iter()
                .flatten()
                .any(|id| !valid_id(id))
            || stage.effort.as_deref().is_some_and(|effort| {
                !matches!(
                    effort,
                    "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
                )
            })
        {
            return Err("invalid native stage arguments");
        }
        Ok(stage)
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.trim() == id && !id.chars().any(char::is_control)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeInitializeResponse {
    user_agent: String,
    codex_home: String,
    platform_family: String,
    platform_os: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModelListItem {
    id: String,
    model: String,
    display_name: String,
    #[serde(skip)]
    aliases: Vec<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    supported_reasoning_efforts: Vec<NativeReasoningEffort>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeReasoningEffort {
    reasoning_effort: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeModelListPage {
    data: Vec<NativeModelListItem>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeAccountReadResponse {
    account: Option<NativeAccount>,
    requires_openai_auth: bool,
}

/// The current protocol requires an account object to carry a discriminator.
/// Keep the rest of the provider-owned fields opaque so a new account kind
/// does not become a release/version allowlist in Astra, while malformed
/// values such as `account: false` still fail closed.
#[derive(Debug, Deserialize)]
struct NativeAccount {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    _provider_fields: serde_json::Map<String, Value>,
}

fn validate_model_list_item(
    model: &NativeModelListItem,
    existing: &[NativeModelListItem],
) -> Result<(), String> {
    if !valid_id(&model.id)
        || !valid_id(&model.model)
        || model.display_name.trim().is_empty()
        || model.display_name.len() > 256
        || model.display_name.chars().any(char::is_control)
        || model
            .supported_reasoning_efforts
            .iter()
            .any(|effort| !valid_id(&effort.reasoning_effort))
        || existing
            .iter()
            .any(|candidate| candidate.id == model.id || candidate.model == model.model)
    {
        return Err("native model catalog contains an invalid or duplicate model".into());
    }
    Ok(())
}

fn provider_model_catalog(
    models: Vec<NativeModelListItem>,
) -> Result<astra_turn_types::ProviderModelCatalog, String> {
    let models = models
        .into_iter()
        .map(|model| astra_turn_types::ProviderModelDescriptor {
            selector: model.model,
            display_name: model.display_name,
            aliases: std::iter::once(model.id).chain(model.aliases).collect(),
            reasoning_efforts: model
                .supported_reasoning_efforts
                .into_iter()
                .map(|effort| effort.reasoning_effort)
                .collect(),
            hidden: model.hidden,
        })
        .collect();
    astra_turn_types::ProviderModelCatalog::new(models).map_err(|error| error.to_string())
}

fn native_models_from_provider_catalog(
    catalog: &astra_turn_types::ProviderModelCatalog,
) -> Vec<NativeModelListItem> {
    catalog
        .models
        .iter()
        .map(|model| NativeModelListItem {
            id: model
                .aliases
                .first()
                .cloned()
                .unwrap_or_else(|| model.selector.clone()),
            model: model.selector.clone(),
            display_name: model.display_name.clone(),
            aliases: model.aliases.clone(),
            hidden: model.hidden,
            supported_reasoning_efforts: model
                .reasoning_efforts
                .iter()
                .cloned()
                .map(|reasoning_effort| NativeReasoningEffort { reasoning_effort })
                .collect(),
        })
        .collect()
}

async fn fetch_model_catalog(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    evidence: &mut Evidence,
    output_limit: usize,
    cancel: &CancellationToken,
) -> Result<astra_turn_types::ProviderModelCatalog, String> {
    let mut models = Vec::new();
    let mut cursor = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for page_index in 0..MODEL_LIST_MAX_PAGES {
        let response = rpc(
            process,
            input,
            model_list_request(cursor.as_deref()),
            evidence,
            output_limit,
            None,
            Some(cancel),
            None,
            None,
        )
        .await
        .map_err(str::to_owned)?;
        let page: NativeModelListPage = serde_json::from_value(response)
            .map_err(|_| "native model catalog response is invalid".to_owned())?;
        if models
            .len()
            .checked_add(page.data.len())
            .is_none_or(|total| total > MODEL_LIST_MAX_ITEMS)
        {
            return Err("native model catalog exceeds the bounded selection limit".into());
        }
        for model in page.data {
            validate_model_list_item(&model, &models)?;
            models.push(model);
        }
        let Some(next_cursor) = page.next_cursor else {
            return provider_model_catalog(models);
        };
        if !valid_id(&next_cursor) || !seen_cursors.insert(next_cursor.clone()) {
            return Err("native model catalog pagination is invalid".into());
        }
        if page_index + 1 == MODEL_LIST_MAX_PAGES {
            return Err("native model catalog pagination exceeds the bounded limit".into());
        }
        cursor = Some(next_cursor);
    }
    Err("native model catalog pagination did not terminate".into())
}

fn normalized_model_selector(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn model_choice_labels(models: &[&NativeModelListItem]) -> Vec<String> {
    let visible: Vec<_> = models
        .iter()
        .copied()
        .filter(|model| !model.hidden)
        .collect();
    let mut choices = Vec::new();
    for model in visible {
        let duplicate_display_name = models
            .iter()
            .filter(|candidate| !candidate.hidden)
            .filter(|candidate| candidate.display_name == model.display_name)
            .count()
            > 1;
        let choice = if duplicate_display_name {
            format!("{} ({})", model.display_name, model.model)
        } else {
            model.display_name.clone()
        };
        if !choices.iter().any(|existing| existing == &choice) {
            choices.push(choice);
        }
        if choices.len() == 8 {
            break;
        }
    }
    choices
}

#[derive(Debug)]
enum ModelSelectorError {
    Ambiguous {
        requested: String,
        choices: Vec<String>,
    },
    Unavailable {
        requested: String,
        choices: Vec<String>,
    },
}

impl ModelSelectorError {
    fn observation(&self) -> Value {
        match self {
            Self::Ambiguous { requested, choices } => json!({
                "status": "requires_user_choice",
                "requested": requested,
                "choices": choices,
            }),
            Self::Unavailable { requested, choices } => json!({
                "status": "unavailable",
                "requested": requested,
                "choices": choices,
            }),
        }
    }
}

impl std::fmt::Display for ModelSelectorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous { requested, choices } => write!(
                formatter,
                "The model name '{requested}' is ambiguous; please choose one: {}",
                choices.join(", ")
            ),
            Self::Unavailable { requested, choices } if choices.is_empty() => write!(
                formatter,
                "The requested model '{requested}' is not available from this provider, and no choices were returned"
            ),
            Self::Unavailable { requested, choices } => write!(
                formatter,
                "The requested model '{requested}' is not available from this provider. Available choices: {}",
                choices.join(", ")
            ),
        }
    }
}

fn resolve_model_selector_diagnostic(
    requested: &str,
    models: &[NativeModelListItem],
) -> Result<String, ModelSelectorError> {
    // A provider selector is an opaque identity. Preserve a byte-exact
    // selection when the catalog contains case-distinct identities; relaxed
    // matching is only a convenience after no exact selector was found.
    let exact: Vec<_> = models
        .iter()
        .filter(|model| {
            model.model == requested || model.aliases.iter().any(|alias| alias == requested)
        })
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0].model.clone());
    }
    if exact.len() > 1 {
        return Err(ModelSelectorError::Ambiguous {
            requested: requested.to_owned(),
            choices: model_choice_labels(&exact),
        });
    }

    let normalized = normalized_model_selector(requested);
    let normalized_selector_matches: Vec<_> = models
        .iter()
        .filter(|model| {
            normalized_model_selector(&model.model) == normalized
                || model
                    .aliases
                    .iter()
                    .any(|alias| normalized_model_selector(alias) == normalized)
        })
        .collect();
    if normalized_selector_matches.len() == 1 {
        return Ok(normalized_selector_matches[0].model.clone());
    }
    if normalized_selector_matches.len() > 1 {
        return Err(ModelSelectorError::Ambiguous {
            requested: requested.to_owned(),
            choices: model_choice_labels(&normalized_selector_matches),
        });
    }

    let display_matches: Vec<_> = models
        .iter()
        .filter(|model| normalized_model_selector(&model.display_name) == normalized)
        .collect();
    if display_matches.len() == 1 {
        return Ok(display_matches[0].model.clone());
    }
    if display_matches.len() > 1 {
        return Err(ModelSelectorError::Ambiguous {
            requested: requested.to_owned(),
            choices: model_choice_labels(&display_matches),
        });
    }

    Err(ModelSelectorError::Unavailable {
        requested: requested.to_owned(),
        choices: model_choice_labels(&models.iter().collect::<Vec<_>>()),
    })
}

fn validate_requested_effort(
    requested: Option<&str>,
    model: &NativeModelListItem,
) -> Result<(), String> {
    let Some(requested) = requested else {
        return Ok(());
    };
    if model.supported_reasoning_efforts.is_empty()
        || model
            .supported_reasoning_efforts
            .iter()
            .any(|effort| effort.reasoning_effort == requested)
    {
        return Ok(());
    }
    let choices = model
        .supported_reasoning_efforts
        .iter()
        .map(|effort| effort.reasoning_effort.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Model '{}' does not support reasoning effort '{}'. Available levels: {choices}",
        model.display_name, requested
    ))
}

#[derive(Default)]
struct Evidence {
    thread: Option<String>,
    turn: Option<String>,
    /// The last provider-stage input accepted by the native turn. The server
    /// may still be completing its durable apply when the provider asks the
    /// next question, so keep this provisional fence until a later input
    /// replaces it. The server remains the authority for whether it exists.
    last_accepted_stage_input_id: Option<String>,
    resolved_model: Option<String>,
    acknowledged_model: Option<String>,
    resumed: bool,
    turn_queued: bool,
    terminal: Option<String>,
    usage: Option<Value>,
    usage_baseline: Option<Value>,
    output: String,
    final_output: Option<String>,
    output_capped: bool,
    /// The provider itself failed a typed capability/readiness contract. The
    /// delivery owner uses this fact to withdraw capacity after publishing
    /// the current result; ordinary task errors do not withdraw the client.
    capability_unavailable: bool,
    /// A bounded provider-owned JSON-RPC error code. Keep the code for
    /// explain/trace and recovery classification without copying provider
    /// messages, which may contain account or environment details.
    provider_error_code: Option<i64>,
    provider_error_class: Option<&'static str>,
    /// A bounded provider subsystem label, when the protocol supplies one.
    /// This keeps recovery explainable without exposing the provider's raw
    /// error payload.
    provider_error_service: Option<String>,
    model_selection: Option<Value>,
    pre_ack: Vec<Value>,
}

fn mark_transport_failure(evidence: &mut Evidence, cancel: Option<&CancellationToken>) {
    if !cancel.is_some_and(CancellationToken::is_cancelled) {
        evidence.capability_unavailable = true;
    }
}

fn classify_provider_error(message: &str) -> &'static str {
    let message = message.to_ascii_lowercase();
    if ["auth", "login", "credential", "token"]
        .iter()
        .any(|needle| message.contains(needle))
    {
        "authentication"
    } else if ["directory", "cwd", "working directory", "workspace"]
        .iter()
        .any(|needle| message.contains(needle))
    {
        "workspace"
    } else if message.contains("config") {
        "configuration"
    } else if ["method", "unsupported", "capability"]
        .iter()
        .any(|needle| message.contains(needle))
    {
        "capability"
    } else {
        "internal"
    }
}

impl Evidence {
    #[cfg(test)]
    fn stage_usage(&self) -> Result<Option<astra_turn_types::CanonicalTokenUsage>, &'static str> {
        Ok(self.stage_usage_with_inclusive()?.map(|(_, usage)| usage))
    }

    fn stage_usage_with_inclusive(
        &self,
    ) -> Result<Option<(Option<u64>, astra_turn_types::CanonicalTokenUsage)>, &'static str> {
        let Some(snapshot) = &self.usage else {
            return Ok(None);
        };
        // A resume replay is the producer-owned cumulative baseline. Without
        // it, neither thread total nor last-response usage proves stage usage.
        if self.resumed && self.usage_baseline.is_none() {
            return Ok(None);
        }
        let total = &snapshot["total"];
        let baseline = self.usage_baseline.as_ref().map(|usage| &usage["total"]);
        let delta = |key: &str| -> Result<Option<u64>, &'static str> {
            let Some(current) = total.get(key).and_then(Value::as_u64) else {
                return Ok(None);
            };
            let before = match baseline {
                Some(before) => match before.get(key).and_then(Value::as_u64) {
                    Some(before) => before,
                    None => return Ok(None),
                },
                None => 0, // Only a newly acknowledged thread starts at zero.
            };
            current
                .checked_sub(before)
                .map(Some)
                .ok_or("native usage counters regressed")
        };
        let inclusive_input = delta("inputTokens")?;
        let cached = delta("cachedInputTokens")?;
        let creation = delta("cacheWriteInputTokens")?;
        let output = delta("outputTokens")?;
        if inclusive_input.is_some_and(|input| {
            cached
                .unwrap_or_default()
                .checked_add(creation.unwrap_or_default())
                .is_none_or(|known| known > input)
        }) {
            return Err("native usage input lanes overlap inconsistently");
        }
        // Native cached/write input are subsets of input; reasoning is a
        // subset of output. Persist disjoint lanes, never add those twice.
        let input = match (inclusive_input, cached, creation) {
            (Some(input), Some(cached), Some(creation)) => Some(
                input
                    .checked_sub(cached)
                    .and_then(|input| input.checked_sub(creation))
                    .ok_or("native usage input lanes overlap inconsistently")?,
            ),
            _ => None,
        };
        astra_turn_types::CanonicalTokenUsage::new(input, cached, creation, output)
            .map(|usage| Some((inclusive_input, usage)))
            .map_err(|_| "native usage exceeds canonical accounting bounds")
    }

    fn require_scope(&self, params: &Value) -> Result<(), &'static str> {
        if params.get("threadId").and_then(Value::as_str) != self.thread.as_deref()
            || params.get("turnId").and_then(Value::as_str) != self.turn.as_deref()
            || self.thread.is_none()
            || self.turn.is_none()
        {
            return Err("native event thread/turn identity mismatch");
        }
        Ok(())
    }

    fn notification(
        &mut self,
        method: &str,
        params: &Value,
        output_limit: usize,
    ) -> Result<(), &'static str> {
        match method {
            "turn/completed" => {
                let turn = params.get("turn").ok_or("missing native terminal turn")?;
                self.require_scope(
                    &json!({"threadId": params.get("threadId"), "turnId": turn.get("id")}),
                )?;
                let status = turn
                    .get("status")
                    .and_then(Value::as_str)
                    .ok_or("missing native terminal status")?;
                if !matches!(status, "completed" | "interrupted" | "failed")
                    || self.terminal.is_some()
                {
                    return Err("invalid or repeated native terminal event");
                }
                self.terminal = Some(status.to_owned());
            }
            "item/agentMessage/delta" => {
                self.require_scope(params)?;
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .ok_or("invalid native message delta")?;
                let mut keep = delta
                    .len()
                    .min(output_limit.saturating_sub(self.output.len()));
                while !delta.is_char_boundary(keep) {
                    keep -= 1;
                }
                self.output.push_str(&delta[..keep]);
                self.output_capped |= keep < delta.len();
            }
            "item/completed" => {
                self.require_scope(params)?;
                let item = params.get("item").ok_or("missing native completed item")?;
                if item.get("type").and_then(Value::as_str) == Some("agentMessage") {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .ok_or("invalid native completed agent message")?;
                    let mut keep = text.len().min(output_limit);
                    while !text.is_char_boundary(keep) {
                        keep -= 1;
                    }
                    self.final_output = Some(text[..keep].to_owned());
                    self.output_capped |= keep < text.len();
                }
            }
            "thread/tokenUsage/updated" => {
                if self.thread.is_none()
                    || params.get("threadId").and_then(Value::as_str) != self.thread.as_deref()
                    || !params
                        .get("turnId")
                        .and_then(Value::as_str)
                        .is_some_and(valid_id)
                {
                    return Err("native usage thread/turn identity mismatch");
                }
                let historical =
                    params.get("turnId").and_then(Value::as_str) != self.turn.as_deref();
                if historical && (!self.resumed || self.usage.is_some()) {
                    return Err("native usage replay outside resumed stage baseline");
                }
                let usage = params
                    .get("tokenUsage")
                    .ok_or("missing native usage snapshot")?;
                for part in ["total", "last"] {
                    let counters = usage.get(part).ok_or("invalid native usage snapshot")?;
                    for counter in [
                        "inputTokens",
                        "cachedInputTokens",
                        "outputTokens",
                        "reasoningOutputTokens",
                        "totalTokens",
                    ] {
                        if counters.get(counter).and_then(Value::as_u64).is_none() {
                            return Err("invalid native usage counter");
                        }
                    }
                }
                // Native total/last are overlapping snapshots, not additive
                // stage charges. Missing usage/cost stays unknown.
                let mut snapshot = json!({"total": {}, "last": {}});
                for part in ["total", "last"] {
                    for counter in [
                        "inputTokens",
                        "cachedInputTokens",
                        "outputTokens",
                        "reasoningOutputTokens",
                        "totalTokens",
                        "cacheWriteInputTokens",
                    ] {
                        if let Some(value) = usage[part].get(counter) {
                            if value.as_u64().is_none() {
                                return Err("invalid native usage counter");
                            }
                            snapshot[part][counter] = value.clone();
                        }
                    }
                }
                if let Some(window) = usage.get("modelContextWindow") {
                    if !window.is_null() && window.as_u64().is_none() {
                        return Err("invalid native context window");
                    }
                    snapshot["modelContextWindow"] = window.clone();
                }
                for part in ["total", "last"] {
                    let counters = &snapshot[part];
                    let input = counters["inputTokens"].as_u64().unwrap();
                    let cached = counters["cachedInputTokens"].as_u64().unwrap();
                    if input.checked_sub(cached).is_none()
                        || counters
                            .get("cacheWriteInputTokens")
                            .and_then(Value::as_u64)
                            .is_some_and(|creation| creation > input - cached)
                        || counters["reasoningOutputTokens"].as_u64().unwrap()
                            > counters["outputTokens"].as_u64().unwrap()
                    {
                        return Err("inconsistent native usage subsets");
                    }
                }
                if historical {
                    self.usage_baseline = Some(snapshot);
                } else {
                    if let Some(previous) = self.usage.as_ref().or(self.usage_baseline.as_ref()) {
                        for key in [
                            "inputTokens",
                            "cachedInputTokens",
                            "cacheWriteInputTokens",
                            "outputTokens",
                            "reasoningOutputTokens",
                            "totalTokens",
                        ] {
                            if previous["total"]
                                .get(key)
                                .and_then(Value::as_u64)
                                .zip(snapshot["total"].get(key).and_then(Value::as_u64))
                                .is_some_and(|(before, after)| after < before)
                            {
                                return Err("native usage counters regressed");
                            }
                        }
                    }
                    self.usage = Some(snapshot);
                    self.stage_usage_with_inclusive()?;
                }
            }
            _ => {} // Non-outcome notifications do not establish run facts.
        }
        Ok(())
    }
}

/// Decode only JSON-RPC envelopes. Never inspect free text for control flow.
pub(crate) fn decode(frame: &[u8]) -> Result<Value, &'static str> {
    let value: Value = serde_json::from_slice(frame).map_err(|_| "invalid native JSON frame")?;
    if !value.is_object() {
        return Err("invalid native JSON-RPC envelope");
    }
    let method = value.get("method");
    if let Some(method) = method {
        if method.as_str().is_none()
            || !value.get("params").is_some_and(Value::is_object)
            || value.get("result").is_some()
            || value.get("error").is_some()
        {
            return Err("invalid native request/notification");
        }
    } else if value.get("id").is_none()
        || (value.get("result").is_some() == value.get("error").is_some())
    {
        return Err("invalid native response");
    }
    if let Some(id) = value.get("id") {
        if !id.is_string() && !id.is_i64() && !id.is_u64() {
            return Err("invalid native request ID");
        }
    }
    Ok(value)
}

pub(crate) async fn send(input: &FramedProcessInput, value: Value) -> Result<(), &'static str> {
    let frame = serde_json::to_vec(&value).map_err(|_| "native request serialization failed")?;
    input
        .send_frame(&frame)
        .await
        .map_err(|_| "native process input failed")
}

fn initialize() -> Value {
    json!({"id": 1, "method": "initialize", "params": {
        "clientInfo": {"name": "astra", "version": env!("CARGO_PKG_VERSION")},
        "capabilities": {
            "experimentalApi": true,
            // Require an explicit user login before the provider may start a
            // gateway OAuth flow. Capability discovery must never open a
            // browser or mutate authentication state as a side effect.
            "explicitGatewayOauth": true
        }
    }})
}

fn account_read_request() -> Value {
    // `refreshToken: false` is a read-only readiness check. It observes the
    // provider's current auth state without starting a login or refresh flow.
    json!({
        "id": ACCOUNT_READ_REQUEST_ID,
        "method": "account/read",
        "params": {"refreshToken": false}
    })
}

fn config_read_request(cwd: &str) -> Value {
    // Read the provider's effective config through its protocol, rather than
    // opening the user's config or credentials from Astra. The returned MCP
    // names are only used to apply the provider's existing per-server disable
    // semantics in the admitted thread.
    json!({
        "id": CONFIG_READ_REQUEST_ID,
        "method": "config/read",
        "params": {"includeLayers": false, "cwd": cwd}
    })
}

async fn disabled_mcp_servers(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    cwd: &str,
    evidence: &mut Evidence,
    output_limit: usize,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let response = rpc(
        process,
        input,
        config_read_request(cwd),
        evidence,
        output_limit,
        None,
        Some(cancel),
        None,
        None,
    )
    .await
    .map_err(str::to_owned)?;
    let Some(servers) = response
        .pointer("/config/additional/mcp_servers")
        .and_then(Value::as_object)
    else {
        return Ok(json!({}));
    };
    if servers.len() > MODEL_LIST_MAX_ITEMS {
        return Err("native MCP configuration exceeds the bounded selection limit".into());
    }
    let mut disabled = serde_json::Map::with_capacity(servers.len());
    for name in servers.keys() {
        if !valid_id(name) {
            return Err("native MCP configuration contains an invalid server name".into());
        }
        disabled.insert(name.clone(), json!({"enabled": false}));
    }
    Ok(Value::Object(disabled))
}

fn model_list_request(cursor: Option<&str>) -> Value {
    let mut params = json!({
        "limit": MODEL_LIST_PAGE_LIMIT,
        // Explicit model selection is an execution lookup, not a picker. A
        // hidden model is still a valid provider identity; hidden models are
        // excluded only from the interactive picker.
        "includeHidden": true
    });
    if let Some(cursor) = cursor {
        params["cursor"] = json!(cursor);
    }
    json!({
        "id": MODEL_LIST_REQUEST_ID,
        "method": "model/list",
        "params": params
    })
}

fn thread_request(
    stage: &Stage,
    cwd: &str,
    sandbox: &Value,
    resolved_model: Option<&str>,
    disabled_mcp_servers: &Value,
) -> Value {
    // Approval is not an authorization to expand the admitted sandbox. Native
    // commands within this ceiling still run; unsandboxed retries must not.
    let mut config = sandbox["config"].clone();
    config["mcp_servers"] = disabled_mcp_servers.clone();
    // Apps and plugins can contribute MCP servers that are not present in the
    // ordinary configured-server map. They are not part of Astra's admitted
    // native capability, so keep Codex's existing feature switches off for
    // this thread as well.
    config["features.apps"] = json!(false);
    config["features.plugins"] = json!(false);
    let mut params = json!({"cwd": cwd, "permissions": sandbox["profileId"], "config": config, "approvalPolicy": "never", "approvalsReviewer": "user"});
    if let Some(model) = resolved_model {
        params["model"] = json!(model);
    }
    let method = if let Some(thread) = &stage.native_session_id {
        params["threadId"] = json!(thread);
        params["excludeTurns"] = json!(true);
        "thread/resume"
    } else {
        "thread/start"
    };
    json!({"id": 2, "method": method, "params": params})
}

fn turn_request(
    stage: &Stage,
    thread: &str,
    cwd: &str,
    sandbox: &Value,
    resolved_model: Option<&str>,
) -> Value {
    let mut params = json!({"threadId": thread, "cwd": cwd,
        "input": [{"type": "text", "text": stage.task, "text_elements": []}],
        "permissions": sandbox["profileId"], "approvalPolicy": "never", "approvalsReviewer": "user"});
    if let Some(model) = resolved_model {
        params["model"] = json!(model);
    }
    if let Some(effort) = &stage.effort {
        params["effort"] = json!(effort);
    }
    json!({"id": 3, "method": "turn/start", "params": params})
}

fn turn_steer_request(
    evidence: &Evidence,
    input: &ProviderStageInput,
) -> Result<Value, &'static str> {
    let ProviderStageInput::Text {
        input_id,
        content,
        expected_turn_id,
        ..
    } = input;
    let thread = evidence
        .thread
        .as_deref()
        .ok_or("native thread is not acknowledged")?;
    let turn = evidence
        .turn
        .as_deref()
        .ok_or("native turn is not acknowledged")?;
    if expected_turn_id
        .as_ref()
        .is_some_and(|expected| expected != turn)
    {
        return Err("provider input expected a different active turn");
    }
    Ok(json!({
        "id": STEER_REQUEST_ID,
        "method": "turn/steer",
        "params": {
            "threadId": thread,
            "clientUserMessageId": input_id,
            "input": [{"type": "text", "text": content, "text_elements": []}],
            "expectedTurnId": turn,
        }
    }))
}

#[allow(clippy::too_many_arguments)]
async fn submit_stage_input(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    evidence: &mut Evidence,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: &CancellationToken,
    stage_input: ProviderStageInput,
    output_limit: usize,
    ack_sender: tokio::sync::oneshot::Sender<ProviderStageInputAck>,
    input_rx: &mut Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
) -> Result<(), &'static str> {
    if let Err(error) = stage_input.validate() {
        let _ = ack_sender.send(ProviderStageInputAck::rejected(
            &stage_input,
            error.to_string(),
        ));
        return Ok(());
    }
    let request = match turn_steer_request(evidence, &stage_input) {
        Ok(request) => request,
        Err(reason) => {
            let _ = ack_sender.send(ProviderStageInputAck::rejected(&stage_input, reason));
            return Ok(());
        }
    };
    let mut pending_ack = PendingStageInputAck {
        input: stage_input,
        sender: Some(ack_sender),
        confirmed: None,
        failure: None,
    };
    match rpc(
        process,
        input,
        request,
        evidence,
        output_limit,
        gate,
        Some(cancel),
        Some(&mut pending_ack),
        Some(input_rx),
    )
    .await
    {
        Ok(response) => {
            pending_ack.confirm(&response, evidence)?;
            Ok(())
        }
        // A terminal notification can race the steer response. It does not
        // prove whether the provider accepted the input, so keep the result
        // transport-ambiguous rather than inventing a negative ACK.
        Err(_reason) if evidence.terminal.is_some() => {
            // A terminal notification raced the steer response. The input
            // was not acknowledged, so dropping its sender is the truthful
            // result; the completed provider stage owns the final outcome.
            Ok(())
        }
        Err(reason) => Err(reason),
    }
}

struct PendingStageInputAck {
    input: ProviderStageInput,
    sender: Option<tokio::sync::oneshot::Sender<ProviderStageInputAck>>,
    confirmed: Option<ProviderStageInputAck>,
    failure: Option<&'static str>,
}

impl PendingStageInputAck {
    fn confirm(
        &mut self,
        response: &Value,
        evidence: &mut Evidence,
    ) -> Result<ProviderStageInputAck, &'static str> {
        if let Some(ack) = &self.confirmed {
            return Ok(ack.clone());
        }
        if let Some(reason) = self.failure {
            return Err(reason);
        }
        let result = if evidence.terminal.is_some() {
            Err("native turn completed before the input was acknowledged")
        } else {
            let Some(turn_id) = response.get("turnId").and_then(Value::as_str) else {
                return self.fail("native turn/steer acknowledgement has no turnId");
            };
            if !valid_id(turn_id) || evidence.turn.as_deref() != Some(turn_id) {
                return self.fail("native turn/steer acknowledgement changed the active turn");
            }
            Ok(ProviderStageInputAck::accepted(
                &self.input,
                Some(turn_id.to_owned()),
            ))
        }?;
        if result.accepted {
            evidence.last_accepted_stage_input_id = Some(self.input.input_id().to_owned());
        }
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(result.clone());
        }
        self.confirmed = Some(result.clone());
        Ok(result)
    }

    fn fail(&mut self, reason: &'static str) -> Result<ProviderStageInputAck, &'static str> {
        self.sender.take();
        self.failure = Some(reason);
        Err(reason)
    }
}

enum NativeRpcResponse {
    Result(Value),
    Error(&'static str),
}

/// Serve one provider request while another JSON-RPC response is pending.
/// Every protocol wait uses this same dispatcher so steering cannot swallow a
/// question, approval, or terminal event. If the request response arrives
/// while an interaction is outstanding, retain it until the interaction reply
/// is sent; the provider may still be blocked on that reply.
#[allow(clippy::too_many_arguments)]
async fn serve_native_interaction(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    evidence: &mut Evidence,
    envelope: &Value,
    gate: &dyn ProviderInteractionGate,
    cancel: Option<&CancellationToken>,
    output_limit: usize,
    expected_response_id: Option<&Value>,
    mut early_stage_ack: Option<&mut PendingStageInputAck>,
    mut input_rx: Option<&mut Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>>,
) -> Result<Option<NativeRpcResponse>, &'static str> {
    let method = envelope
        .get("method")
        .and_then(Value::as_str)
        .ok_or("invalid native request method")?;
    let provider_stage_input_id = early_stage_ack
        .as_deref()
        .map(|pending| pending.input.input_id().to_owned())
        .or_else(|| evidence.last_accepted_stage_input_id.clone());
    let request = interaction_request(envelope, evidence, provider_stage_input_id)?;
    let decision = tokio::time::timeout(INTERACTION_TIMEOUT, gate.request_interaction(&request));
    tokio::pin!(decision);
    let mut pending_response = None;
    loop {
        let input_ready = input_rx.as_ref().is_some_and(|receiver| receiver.is_some());
        tokio::select! {
            biased;
            _ = async {
                match cancel {
                    Some(cancel) => {
                        cancel.cancelled().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => return Err("native interaction cancelled"),
            decision_result = &mut decision => {
                let decision = decision_result.map_err(|_| "native interaction timed out")?;
                match decision {
                    ProviderInteractionDecision::Submitted(payload) if payload.is_object() => {
                        validate_interaction_response(method, &envelope["params"], &payload)?;
                        if let Err(error) = send(input, json!({"id": envelope["id"], "result": payload})).await {
                            mark_transport_failure(evidence, cancel);
                            return Err(error);
                        }
                        return Ok(pending_response);
                    }
                    ProviderInteractionDecision::Submitted(_) => {
                        return Err("invalid native interaction response");
                    }
                    ProviderInteractionDecision::Cancelled => {
                        return Err("native interaction cancelled");
                    }
                    ProviderInteractionDecision::Timeout => {
                        return Err("native interaction timed out");
                    }
                    ProviderInteractionDecision::Error(_) => {
                        return Err("native interaction failed");
                    }
                }
            }
            stage_input = async {
                match input_rx.as_deref_mut() {
                    Some(Some(receiver)) => receiver.recv().await,
                    None => None,
                    Some(None) => None,
                }
            }, if input_ready => {
                match stage_input {
                    Some(stage_input) => {
                        let _ = stage_input.ack.send(ProviderStageInputAck::rejected(
                            &stage_input.input,
                            "native provider interaction is pending; guidance remains queued for a later boundary",
                        ));
                    }
                    None => {
                        if let Some(receiver) = input_rx.as_deref_mut() {
                            *receiver = None;
                        }
                    }
                }
            }
            event = next_envelope(process, evidence) => {
                let event = event?;
                if event.get("id").is_some() {
                    if expected_response_id.is_some_and(|expected| event["id"] == *expected) {
                        let response = if event.get("error").is_some() {
                            NativeRpcResponse::Error("native request rejected")
                        } else if let Some(early_stage_ack) = early_stage_ack.as_deref_mut() {
                            match early_stage_ack.confirm(&event["result"], evidence) {
                                Ok(_) => NativeRpcResponse::Result(event["result"].clone()),
                                Err(reason) => NativeRpcResponse::Error(reason),
                            }
                        } else {
                            NativeRpcResponse::Result(event["result"].clone())
                        };
                        if pending_response.replace(response).is_some() {
                            return Err("native request produced multiple responses while interaction was pending");
                        }
                        continue;
                    }
                    return Err("native concurrent interaction exceeds invocation bound");
                }
                let method = event
                    .get("method")
                    .and_then(Value::as_str)
                    .ok_or("unexpected native response during interaction")?;
                evidence.notification(method, &event["params"], output_limit)?;
                if evidence.terminal.is_some() {
                    // The caller observes terminal evidence and closes the
                    // stage; it must not keep waiting for a user response.
                    return Ok(pending_response);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn rpc(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    request: Value,
    evidence: &mut Evidence,
    output_limit: usize,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: Option<&CancellationToken>,
    mut early_stage_ack: Option<&mut PendingStageInputAck>,
    mut input_rx: Option<&mut Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>>,
) -> Result<Value, &'static str> {
    let id = request["id"].clone();
    if let Err(error) = send(input, request).await {
        mark_transport_failure(evidence, cancel);
        return Err(error);
    }
    if id == json!(3) {
        evidence.turn_queued = true;
    }
    loop {
        let frame = match process.recv_frame().await {
            Some(frame) => frame,
            None => {
                evidence.capability_unavailable = true;
                return Err("native EOF before request acknowledgement");
            }
        };
        let envelope = match decode(&frame) {
            Ok(envelope) => envelope,
            Err(error) => {
                mark_transport_failure(evidence, cancel);
                return Err(error);
            }
        };
        if let Some(method) = envelope.get("method").and_then(Value::as_str) {
            if evidence.turn.is_none()
                && (id == json!(3) || (id == json!(2) && method == "thread/tokenUsage/updated"))
            {
                // Thread/configuration chatter can arrive between a turn
                // request and its ACK. It is not a turn fact and must not
                // consume the bounded pre-ACK evidence budget. Retain only
                // notifications carrying the canonical turn identity; these
                // may contain output, usage, or an interaction request that
                // arrived before the ACK.
                let params = &envelope["params"];
                let has_turn_identity = params.get("turnId").and_then(Value::as_str).is_some()
                    || params.pointer("/turn/id").and_then(Value::as_str).is_some();
                if !has_turn_identity {
                    if envelope.get("id").is_some() {
                        return Err("native request before active turn acknowledgement");
                    }
                    continue;
                }
                if evidence.pre_ack.len() == PRE_ACK_EVENTS {
                    return Err("native pre-ack event budget exceeded");
                }
                evidence.pre_ack.push(envelope);
            } else if envelope.get("id").is_some() {
                let gate = gate.ok_or("native interaction gate is not connected")?;
                let response = serve_native_interaction(
                    process,
                    input,
                    evidence,
                    &envelope,
                    gate,
                    cancel,
                    output_limit,
                    Some(&id),
                    early_stage_ack.as_deref_mut(),
                    input_rx.as_deref_mut(),
                )
                .await?;
                if let Some(response) = response {
                    return match response {
                        NativeRpcResponse::Result(result) => {
                            if let Some(early_stage_ack) = early_stage_ack.as_deref_mut() {
                                early_stage_ack.confirm(&result, evidence)?;
                            }
                            Ok(result)
                        }
                        NativeRpcResponse::Error(reason) => Err(reason),
                    };
                }
                if evidence.terminal.is_some() {
                    return Err("native stage completed before request acknowledgement");
                }
            } else if evidence.turn.is_some() {
                evidence.notification(method, &envelope["params"], output_limit)?;
                if evidence.terminal.is_some() {
                    return Err("native stage completed before request acknowledgement");
                }
            }
        } else {
            if envelope["id"] != id {
                return Err("native acknowledgement request ID mismatch");
            }
            if envelope.get("error").is_some() {
                let error = &envelope["error"];
                evidence.provider_error_code = error.get("code").and_then(Value::as_i64);
                evidence.provider_error_class = error
                    .get("message")
                    .and_then(Value::as_str)
                    .map(classify_provider_error);
                evidence.provider_error_service = error
                    .pointer("/data/service")
                    .and_then(Value::as_str)
                    .filter(|service| {
                        !service.is_empty()
                            && service.len() <= 64
                            && service
                                .chars()
                                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
                    })
                    .map(str::to_owned);
                if matches!(id.as_i64(), Some(1 | 7)) {
                    evidence.capability_unavailable = true;
                }
                return Err(match id.as_i64() {
                    Some(1) => "native initialize request rejected",
                    Some(2) => "native thread request rejected",
                    Some(3) => "native turn request rejected",
                    Some(4) => "native model catalog request rejected",
                    Some(5) => "native interrupt request rejected",
                    Some(7) => "native account readiness request rejected",
                    Some(8) => "native config read request rejected",
                    _ => "native request rejected",
                });
            }
            let result = envelope["result"].clone();
            if let Some(early_stage_ack) = early_stage_ack.as_deref_mut() {
                early_stage_ack.confirm(&result, evidence)?;
            }
            return Ok(result);
        }
    }
}

/// Perform the one protocol handshake shared by discovery and execution.
/// Discovery proves only that this executable can speak the protocol; the
/// actual stage still creates its own process and repeats the handshake before
/// selecting a model or starting a turn.
#[allow(clippy::too_many_arguments)]
async fn initialize_protocol(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    evidence: &mut Evidence,
    output_limit: usize,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: &CancellationToken,
) -> Result<(), &'static str> {
    let response = rpc(
        process,
        input,
        initialize(),
        evidence,
        output_limit,
        gate,
        Some(cancel),
        None,
        None,
    )
    .await?;
    let initialized: NativeInitializeResponse = serde_json::from_value(response).map_err(|_| {
        evidence.capability_unavailable = true;
        "native initialize returned an invalid result"
    })?;
    if !valid_id(&initialized.user_agent)
        || !std::path::Path::new(&initialized.codex_home).is_absolute()
        || !valid_id(&initialized.platform_family)
        || !valid_id(&initialized.platform_os)
    {
        evidence.capability_unavailable = true;
        return Err("native initialize returned an invalid result");
    }
    if let Err(error) = send(input, json!({"method": "initialized"})).await {
        mark_transport_failure(evidence, Some(cancel));
        return Err(error);
    }
    Ok(())
}

/// Initialization and account/read prove that the installed client is an
/// authenticated provider boundary. Model availability is deliberately
/// separate: a provider may be usable with its default model even when its
/// catalog is empty or temporarily unavailable, while an explicitly requested
/// model is resolved against the live catalog immediately before its turn.
async fn verify_provider_authentication(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    evidence: &mut Evidence,
    cancel: &CancellationToken,
    output_limit: usize,
) -> Result<(), &'static str> {
    let response = rpc(
        process,
        input,
        account_read_request(),
        evidence,
        output_limit,
        None,
        Some(cancel),
        None,
        None,
    )
    .await?;
    let account: NativeAccountReadResponse = serde_json::from_value(response).map_err(|_| {
        evidence.capability_unavailable = true;
        "native account readiness response is invalid"
    })?;
    // Codex's account/read contract explicitly distinguishes providers which
    // need OpenAI authentication from providers which do not. A nonempty
    // model catalog is not sufficient: it may be a cached catalog after
    // logout. Keep this typed check independent of account display strings.
    if account
        .account
        .as_ref()
        .is_some_and(|account| !valid_id(&account.kind))
        || (account.requires_openai_auth && account.account.is_none())
    {
        evidence.capability_unavailable = true;
        return Err("native provider authentication is unavailable");
    }
    Ok(())
}

async fn resolve_requested_model(
    process: &mut FramedProcess,
    input: &FramedProcessInput,
    stage: &Stage,
    evidence: &mut Evidence,
    output_limit: usize,
    cancel: &CancellationToken,
    cached_catalog: Option<&astra_turn_types::ProviderModelCatalog>,
) -> Result<Option<String>, String> {
    let Some(requested) = stage.model.as_deref() else {
        // Omitting the selector is the provider-default path. It must not
        // pay for discovery or turn a normal stage into a catalog dependency.
        return Ok(None);
    };

    let can_refresh_cached_catalog = cached_catalog.is_some_and(|catalog| catalog.is_complete());
    let mut models = if let Some(catalog) = cached_catalog.filter(|catalog| catalog.is_complete()) {
        native_models_from_provider_catalog(catalog)
    } else {
        fetch_model_catalog(process, input, evidence, output_limit, cancel)
            .await
            .map(|catalog| native_models_from_provider_catalog(&catalog))?
    };
    let mut refreshed_cached_catalog = false;
    loop {
        let resolved = match resolve_model_selector_diagnostic(requested, &models) {
            Ok(resolved) => resolved,
            Err(_error)
                if can_refresh_cached_catalog
                    && !refreshed_cached_catalog
                    && !cancel.is_cancelled() =>
            {
                // A discovery snapshot is a bounded optimization, not
                // authority that either model identity or its capabilities
                // are unchanged. Refresh once for any cached rejection.
                let refreshed =
                    fetch_model_catalog(process, input, evidence, output_limit, cancel).await?;
                models = native_models_from_provider_catalog(&refreshed);
                refreshed_cached_catalog = true;
                continue;
            }
            Err(error) => {
                evidence.model_selection = Some(error.observation());
                return Err(error.to_string());
            }
        };
        if let Some(model) = models.iter().find(|model| model.model == resolved)
            && let Err(error) = validate_requested_effort(stage.effort.as_deref(), model)
        {
            if can_refresh_cached_catalog && !refreshed_cached_catalog && !cancel.is_cancelled() {
                let refreshed =
                    fetch_model_catalog(process, input, evidence, output_limit, cancel).await?;
                models = native_models_from_provider_catalog(&refreshed);
                refreshed_cached_catalog = true;
                continue;
            }
            return Err(error);
        }
        evidence.resolved_model = Some(resolved.clone());
        return Ok(Some(resolved));
    }
}

async fn next_envelope(
    process: &mut FramedProcess,
    evidence: &mut Evidence,
) -> Result<Value, &'static str> {
    if !evidence.pre_ack.is_empty() {
        return Ok(evidence.pre_ack.remove(0));
    }
    let frame = match process.recv_frame().await {
        Some(frame) => frame,
        None => {
            evidence.capability_unavailable = true;
            return Err("native EOF without matching terminal evidence");
        }
    };
    match decode(&frame) {
        Ok(envelope) => Ok(envelope),
        Err(error) => {
            evidence.capability_unavailable = true;
            Err(error)
        }
    }
}

fn acknowledge_thread(
    stage: &Stage,
    result: &Value,
    evidence: &mut Evidence,
) -> Result<(), &'static str> {
    let thread = result
        .get("thread")
        .ok_or("missing acknowledged native thread")?;
    let id = thread
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or("invalid acknowledged native thread ID")?;
    if stage
        .native_session_id
        .as_deref()
        .is_some_and(|expected| expected != id)
    {
        return Err("native resume acknowledged a different thread");
    }
    evidence.thread = Some(id.to_owned());
    evidence.resumed = stage.native_session_id.is_some();
    if let Some(model) = result
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| valid_id(model))
    {
        evidence.acknowledged_model = Some(model.to_owned());
    }
    if stage.model.is_some()
        && evidence.acknowledged_model.as_deref() != evidence.resolved_model.as_deref()
    {
        return Err("native acknowledged a different model");
    }
    // Do not turn/start into an existing active native turn: that method can
    // steer it, which would falsely attribute old work to a new child run.
    if thread.pointer("/status/type").and_then(Value::as_str) != Some("idle") {
        return Err("native thread is not idle; recovery requires the shared run owner");
    }
    Ok(())
}

fn acknowledge_turn(result: &Value, evidence: &mut Evidence) -> Result<(), &'static str> {
    let turn = result
        .get("turn")
        .ok_or("missing acknowledged native turn")?;
    let id = turn
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))
        .ok_or("invalid acknowledged native turn ID")?;
    evidence.turn = Some(id.to_owned());
    if turn.get("status").and_then(Value::as_str) != Some("inProgress") {
        return Err("invalid native turn start status");
    }
    Ok(())
}

fn verify_sandbox(result: &Value, requested: &Value, cwd: &str) -> Result<(), &'static str> {
    if result.get("cwd").and_then(Value::as_str) != Some(cwd)
        || result.get("approvalPolicy").and_then(Value::as_str) != Some("never")
        || result.get("approvalsReviewer").and_then(Value::as_str) != Some("user")
    {
        return Err("native acknowledged different workspace/approval authority");
    }
    let requested_id = requested
        .get("profileId")
        .and_then(Value::as_str)
        .ok_or("native permission profile identity missing")?;
    let (expected_type, expected_network) = expected_profile_sandbox(requested, cwd)?;
    let active = result
        .get("activePermissionProfile")
        .and_then(|profile| profile.get("id"))
        .and_then(Value::as_str);
    let active_has_parent = result
        .pointer("/activePermissionProfile/extends")
        .is_some_and(|value| !value.is_null());
    let sandbox = result
        .get("sandbox")
        .ok_or("native sandbox acknowledgement missing")?;
    if active != Some(requested_id)
        || active_has_parent
        || sandbox.get("type").and_then(Value::as_str) != Some(expected_type)
        || sandbox.get("networkAccess").and_then(Value::as_bool) != Some(expected_network)
    {
        return Err("native acknowledged a different permission profile");
    }
    Ok(())
}

fn interaction_request(
    envelope: &Value,
    evidence: &Evidence,
    provider_stage_input_id: Option<String>,
) -> Result<ProviderInteractionRequest, &'static str> {
    let method = envelope["method"]
        .as_str()
        .ok_or("invalid native request method")?;
    if !matches!(
        method,
        "item/tool/requestUserInput"
            | "item/commandExecution/requestApproval"
            | "item/fileChange/requestApproval"
            | "item/permissions/requestApproval"
            | "mcpServer/elicitation/request"
    ) {
        return Err("unsupported native server request");
    }
    evidence.require_scope(&envelope["params"])?;
    // Preserve the native ID's JSON type in the payload and wire response.
    // The outer ID is an exact canonical string, not a lossy numeric cast.
    let request = ProviderInteractionRequest {
        request_id: serde_json::to_string(&envelope["id"])
            .map_err(|_| "invalid native request ID")?,
        payload: json!({"provider": "codex", "native_request_id": envelope["id"], "method": method, "params": envelope["params"]}),
        timeout_ms: Some(INTERACTION_TIMEOUT.as_millis() as u64),
        provider_stage_input_id,
    };
    request
        .validate()
        .map_err(|_| "invalid provider interaction request")?;
    Ok(request)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeQuestion {
    id: String,
    header: String,
    question: String,
    options: Option<Vec<NativeQuestionOption>>,
    #[serde(default)]
    is_other: bool,
    #[serde(default)]
    is_secret: bool,
}

#[derive(Deserialize)]
struct NativeQuestionOption {
    label: String,
    description: String,
}

fn native_questions(
    interaction: &ProviderInteractionRequest,
) -> Result<Vec<NativeQuestion>, String> {
    if interaction.payload["provider"] != "codex"
        || interaction.payload["method"] != "item/tool/requestUserInput"
    {
        return Err("native interaction is not supported by the question UI; approvals cannot expand the sandbox".into());
    }
    let questions: Vec<NativeQuestion> =
        serde_json::from_value(interaction.payload["params"]["questions"].clone())
            .map_err(|_| "native question protocol shape is invalid".to_string())?;
    let mut ids = std::collections::HashSet::new();
    for question in &questions {
        if question.is_secret {
            return Err(
                "native secret questions are unsupported by the current question UI".into(),
            );
        }
        if question.id.trim().is_empty() || !ids.insert(question.id.as_str()) {
            return Err("native question IDs must be nonempty and unique".into());
        }
    }
    Ok(questions)
}

/// Project only the native questionnaire onto the canonical user-question UI.
/// Never include secret content or protocol values in projection errors.
pub(crate) fn question_prompt(
    interaction: &ProviderInteractionRequest,
) -> Result<astra_tools::AskUserPrompt, String> {
    let questions = native_questions(interaction)?;
    let questions: Vec<Value> = questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q
                .options
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|o| json!({"label": o.label, "description": o.description}))
                .collect();
            json!({"header":q.header, "question":q.question, "options":options,
            "multi_select":false, "allow_freeform":q.is_other || q.options.is_none()})
        })
        .collect();
    astra_tools::parse_ask_user_prompt(&json!({"questions":questions}))
        .map_err(|_| "native questionnaire is unsupported by the existing question UI (duplicate question/header or invalid option shape)".into())
}

/// Use canonical answer normalization, then zip its ordered vector to native
/// IDs. Question text is never used as a fuzzy native protocol identity.
pub(crate) fn question_response(
    interaction: &ProviderInteractionRequest,
    prompt: &astra_tools::AskUserPrompt,
    answers: &astra_tools::AskUserAnswers,
) -> Result<Value, String> {
    // Context and display timeout belong to the UI; only the questionnaire
    // carries the native answer contract.
    if question_prompt(interaction)?.questions != prompt.questions {
        return Err("native questionnaire changed before answer submission".into());
    }
    let normalized = astra_tools::normalize_ask_user_answers(prompt, answers).map_err(|_| {
        "native question answers do not match the canonical questionnaire".to_string()
    })?;
    if normalized
        .answers
        .iter()
        .any(|answer| answer.annotation.is_some())
    {
        return Err("native question protocol cannot represent answer annotations".into());
    }
    let questions = native_questions(interaction)?;
    let answers: serde_json::Map<String, Value> = questions
        .iter()
        .zip(&normalized.answers)
        .map(|(question, answer)| (question.id.clone(), json!({"answers":answer.answers})))
        .collect();
    Ok(json!({"answers":answers}))
}

fn validate_interaction_response(
    method: &str,
    params: &Value,
    payload: &Value,
) -> Result<(), &'static str> {
    match method {
        "item/tool/requestUserInput" => {
            let answers = payload
                .get("answers")
                .and_then(Value::as_object)
                .ok_or("invalid native question response")?;
            let questions = params
                .get("questions")
                .and_then(Value::as_array)
                .ok_or("invalid native questions")?;
            if answers.len() != questions.len() {
                return Err("native answer/question identity mismatch");
            }
            for question in questions {
                let id = question
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("invalid native question ID")?;
                let answer = answers
                    .get(id)
                    .and_then(|answer| answer.get("answers"))
                    .and_then(Value::as_array)
                    .ok_or("native answer/question identity mismatch")?;
                if !answer.iter().all(Value::is_string) {
                    return Err("invalid native question answer");
                }
            }
        }
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision = payload
                .get("decision")
                .and_then(Value::as_str)
                .ok_or("unsupported native approval decision")?;
            // The wire response does not prove execution remains sandboxed;
            // accept and session/policy amendments can authorize bypass.
            if !matches!(decision, "decline" | "cancel") {
                return Err("native approval exceeds immutable sandbox ceiling");
            }
        }
        "item/permissions/requestApproval" => {
            let permissions = payload
                .get("permissions")
                .and_then(Value::as_object)
                .ok_or("invalid native permission response")?;
            if payload
                .get("scope")
                .is_some_and(|scope| !matches!(scope.as_str(), Some("turn" | "session")))
            {
                return Err("invalid native permission scope");
            }
            // Additional permissions are additive, not a restatement of the
            // configured sandbox. Only the protocol's empty grant is supported.
            if permissions.iter().any(|(key, value)| {
                !matches!(key.as_str(), "fileSystem" | "network") || !value.is_null()
            }) {
                return Err("native permission grant exceeds immutable sandbox ceiling");
            }
        }
        "mcpServer/elicitation/request" => {
            if !matches!(
                payload.get("action").and_then(Value::as_str),
                Some("accept" | "decline" | "cancel")
            ) {
                return Err("invalid native elicitation response");
            }
        }
        _ => return Err("unsupported native interaction response"),
    }
    Ok(())
}

// Protocol transport, output budget and interaction authority have distinct owners.
#[allow(clippy::too_many_arguments)]
async fn drive(
    process: &mut FramedProcess,
    stage: &Stage,
    cwd: &str,
    sandbox: &Value,
    evidence: &mut Evidence,
    output_limit: usize,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    drive_with_input(
        process,
        stage,
        cwd,
        sandbox,
        evidence,
        output_limit,
        gate,
        cancel,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_with_input(
    process: &mut FramedProcess,
    stage: &Stage,
    cwd: &str,
    sandbox: &Value,
    evidence: &mut Evidence,
    output_limit: usize,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: &CancellationToken,
    input_rx: Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
) -> Result<(), String> {
    drive_with_cached_catalog(
        process,
        stage,
        cwd,
        sandbox,
        evidence,
        output_limit,
        gate,
        cancel,
        input_rx,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn drive_with_cached_catalog(
    process: &mut FramedProcess,
    stage: &Stage,
    cwd: &str,
    sandbox: &Value,
    evidence: &mut Evidence,
    output_limit: usize,
    gate: Option<&dyn ProviderInteractionGate>,
    cancel: &CancellationToken,
    mut input_rx: Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
    cached_catalog: Option<&astra_turn_types::ProviderModelCatalog>,
) -> Result<(), String> {
    let input = process.input();
    initialize_protocol(process, &input, evidence, output_limit, gate, cancel).await?;
    verify_provider_authentication(process, &input, evidence, cancel, output_limit)
        .await
        .map_err(str::to_owned)?;
    let disabled_mcp_servers =
        disabled_mcp_servers(process, &input, cwd, evidence, output_limit, cancel).await?;
    let resolved_model = resolve_requested_model(
        process,
        &input,
        stage,
        evidence,
        output_limit,
        cancel,
        cached_catalog,
    )
    .await?;
    let response = rpc(
        process,
        &input,
        thread_request(
            stage,
            cwd,
            sandbox,
            resolved_model.as_deref(),
            &disabled_mcp_servers,
        ),
        evidence,
        output_limit,
        gate,
        Some(cancel),
        None,
        None,
    )
    .await?;
    acknowledge_thread(stage, &response, evidence)?;
    verify_sandbox(&response, sandbox, cwd)?;
    // A queued write does not prove dispatch. Until turn/start ACK the
    // conservative fact is unknown, not a zero-call/zero-cost execution.
    let response = rpc(
        process,
        &input,
        turn_request(
            stage,
            evidence.thread.as_deref().unwrap(),
            cwd,
            sandbox,
            resolved_model.as_deref(),
        ),
        evidence,
        output_limit,
        gate,
        Some(cancel),
        None,
        None,
    )
    .await?;
    acknowledge_turn(&response, evidence)?;
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("native invocation cancelled".into()),
            envelope = next_envelope(process, evidence) => Some(envelope?),
            stage_input = async {
                match input_rx.as_mut() {
                    Some(receiver) => receiver.recv().await,
                    None => None,
                }
            }, if input_rx.is_some() => {
                if let Some(stage_input) = stage_input {
                    let ack = submit_stage_input(
                        process,
                        &input,
                        evidence,
                        gate,
                        cancel,
                        stage_input.input,
                        output_limit,
                        stage_input.ack,
                        &mut input_rx,
                    )
                    .await;
                    if let Err(reason) = ack {
                        return Err(reason.into());
                    }
                    if evidence.terminal.is_some() {
                        return Ok(());
                    }
                } else {
                    input_rx = None;
                }
                None
            }
        };
        let Some(envelope) = next else {
            continue;
        };
        let method = envelope
            .get("method")
            .and_then(Value::as_str)
            .ok_or("unexpected native response")?;
        if envelope.get("id").is_some() {
            let gate = gate.ok_or("native interaction gate is not connected")?;
            serve_native_interaction(
                process,
                &input,
                evidence,
                &envelope,
                gate,
                Some(cancel),
                output_limit,
                None,
                None,
                Some(&mut input_rx),
            )
            .await?;
            if evidence.terminal.is_some() {
                return Ok(());
            }
        } else {
            evidence.notification(method, &envelope["params"], output_limit)?;
            if evidence.terminal.is_some() {
                return Ok(());
            }
        }
    }
}

fn failure(reason: &'static str) -> ToolResult {
    ToolResult { output: format!("Error: {reason}"), is_error: true,
        metadata: Some(json!({"native_collaborator": {"provider": "codex", "dispatch_state": "not_dispatched", "native_session_id": null, "native_turn_id": null, "session_acknowledged": false, "turn_acknowledged": false, "target_released": false, "usage_snapshot": null, "cost_usd": null}, "workspace_effect_settled": true}).as_object().unwrap().clone()), exit_semantics: None }
}

impl ToolExecutor {
    /// Publish through the authenticated local-provider snapshot adapter.
    /// This declaration carries requirements, never permission or claim trust.
    /// The connection owner must prove consumer readiness before publishing it.
    pub(crate) async fn native_collaborator_declarations_if_available(
        &self,
        cancel: Option<&CancellationToken>,
        deadline: std::time::Instant,
    ) -> Option<(
        Vec<(
            astra_turn_types::ProviderToolDeclaration,
            NativeExecutableIdentity,
        )>,
        Vec<NativeExecutableIdentity>,
    )> {
        let supported = astra_core::sync_poison::recover_rwlock_read(&self.sandbox_policy)
            .as_ref()
            .is_some_and(|policy| policy.isolation != astra_sandbox::IsolationLevel::Strict);
        if !supported {
            return None;
        }
        let root = self.effective_project_root().canonicalize().ok()?;
        let token = cancel.map_or_else(CancellationToken::new, CancellationToken::child_token);
        let initial_executables = native_provider_executable_snapshot();
        let mut declarations = Vec::new();
        for executable in native_executable_candidates() {
            if token.is_cancelled() {
                return None;
            }
            if deadline <= std::time::Instant::now() {
                return None;
            }
            let Ok(requirements) = runtime_requirements_for_executable(&executable) else {
                continue;
            };
            let Some(expected_identity) =
                native_executable_identity(std::path::Path::new(&requirements.executable)).ok()
            else {
                continue;
            };
            let Ok(model_catalog) =
                verify_installed_protocol(&executable, &root, &token, deadline).await
            else {
                continue;
            };
            if token.is_cancelled() {
                continue;
            }
            let current_identity =
                native_executable_identity(std::path::Path::new(&requirements.executable));
            let current_executables = native_provider_executable_snapshot();
            if current_identity.as_ref().ok() != Some(&expected_identity)
                || current_executables != initial_executables
            {
                return None;
            }
            if let Ok(declaration) = provider_declaration(requirements, model_catalog) {
                declarations.push((declaration, expected_identity));
            }
        }
        (!declarations.is_empty()).then_some((declarations, initial_executables))
    }

    /// Existing selected CLI execution entrypoint owns workspace and sandbox.
    // Keep the invocation, ceiling and locally approved runtime authority explicit.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_native_codex(
        &self,
        args: &Value,
        invocation: astra_tools::tool_engine::ToolInvocationMetadata<'_>,
        cancel: Option<&CancellationToken>,
        execution_root: &std::path::Path,
        gate: Option<&dyn ProviderInteractionGate>,
        execution_ceiling: Option<&astra_server_types::edge_ws_protocol::EdgeExecutionCeiling>,
        runtime_approval: Option<&ApprovedNativeRuntime>,
        input_rx: Option<tokio::sync::mpsc::Receiver<EdgeInvocationInput>>,
    ) -> ToolResult {
        let stage = match Stage::parse(args) {
            Ok(stage) => stage,
            Err(reason) => return failure(reason),
        };
        let cached_catalog = runtime_approval
            .and_then(|approval| approval.snapshot.tool_declarations.first())
            .and_then(|declaration| declaration.model_catalog().ok())
            .flatten();
        let Some(gate) = gate else {
            return failure("canonical native interaction route is not connected");
        };
        let Some(ceiling) = execution_ceiling else {
            return failure("native execution requires an immutable execution grant");
        };
        if invocation.run_id.filter(|id| valid_id(id)).is_none()
            || invocation.tool_call_id.filter(|id| valid_id(id)).is_none()
            || invocation.admission_source.is_none()
        {
            return failure("native execution requires canonical run and invocation admission");
        }
        if cancel.is_some_and(CancellationToken::is_cancelled)
            || invocation
                .admission_deadline
                .is_some_and(|deadline| deadline <= std::time::Instant::now())
        {
            return failure("native invocation admission cancelled or expired");
        }
        // Check whole-job authority before capability/config preparation.
        if let Err(reason) = native_stage_remaining(invocation) {
            return failure(reason);
        }
        let mut policy = astra_core::sync_poison::recover_rwlock_read(&self.sandbox_policy).clone();
        if policy
            .as_ref()
            .is_some_and(|policy| policy.isolation == astra_sandbox::IsolationLevel::Strict)
        {
            return failure("native protocol cannot prove the selected strict isolation boundary");
        }
        let read_only = !ceiling.workspace_write_allowed
            || self.read_only_execution
            || self.plan_mode_authoring_active().await;
        let network =
            ceiling.network_allowed && policy.as_ref().is_some_and(|policy| policy.network_allowed);
        let cwd = match execution_root.canonicalize() {
            Ok(cwd) => cwd,
            Err(_) => return failure("native workspace is unavailable"),
        };
        // Do not canonicalize a different grant root into the selected root:
        // the admission owner freezes the canonical path before transport.
        if std::path::Path::new(&ceiling.workspace_root) != cwd {
            return failure("native execution grant does not match the selected workspace");
        }
        let Some(cwd_text) = cwd.to_str() else {
            return failure("native workspace path is not UTF-8");
        };
        let requirements = match runtime_approval {
            Some(approved) => match runtime_requirements_for_executable(std::path::Path::new(
                &approved.requirements.executable,
            )) {
                Ok(requirements) => requirements,
                Err(reason) => return failure(reason),
            },
            None => match installed_runtime_requirements() {
                Ok(requirements) => requirements,
                Err(reason) => return failure(reason),
            },
        };
        // Apply read-only bootstrap approval to this invocation's private
        // policy copy, never to the shared executor's general file authority.
        if let Some(approved) = runtime_approval {
            if approved.requirements != requirements || approved.workspace_root != cwd {
                return failure("native runtime approval no longer matches the selected provider");
            }
            let Some(local_policy) = policy.as_mut() else {
                return failure("native runtime approval requires a selected local sandbox policy");
            };
            for path in &approved.requirements.read_paths {
                if astra_sandbox::is_never_readable_path(std::path::Path::new(path)) {
                    return failure("native runtime approval includes a forbidden path");
                }
                local_policy.allowed_paths.push(path.into());
            }
        }
        if let Err(reason) =
            validate_runtime_grant(&ceiling.runtime_read_paths, &requirements, policy.as_ref())
        {
            return failure(reason);
        }
        let executable = std::path::Path::new(&requirements.executable);
        let sandbox = match permission_profile(cwd_text, !read_only, network, &requirements) {
            Ok(sandbox) => sandbox,
            Err(reason) => return failure(reason),
        };
        // Work authority is the immutable admission cutoff. A Bash command
        // ceiling cannot authorize or truncate this multi-command native job.
        let output_limit = policy.as_ref().map_or(OUTPUT_BYTES, |policy| {
            policy.max_output_bytes.min(OUTPUT_BYTES)
        });
        let token = cancel.map_or_else(CancellationToken::new, CancellationToken::child_token);
        let Some(attribution) =
            astra_tools::workspace_observation::WorkspaceAttributionState::capture(&cwd)
        else {
            return failure("native workspace attribution is unavailable");
        };
        let (mut command, owner) = match prepare_native_process(executable, &["app-server".into()])
        {
            Ok(prepared) => prepared,
            Err(_) => return failure("native structured process ownership is unavailable"),
        };
        command.current_dir(&cwd);
        if let Some(policy) = &mut policy {
            policy.project_root = cwd.clone();
            if astra_sandbox::sandbox_command(policy, &mut command).is_err() {
                return failure("native sandbox preparation failed");
            }
        }
        // Recheck after preflight/workspace preparation: none of those waits
        // can renew the original whole-job deadline.
        if self.effective_project_root().as_path() != execution_root
            || token.is_cancelled()
            || invocation
                .admission_deadline
                .is_some_and(|deadline| deadline <= std::time::Instant::now())
        {
            return failure("native invocation admission cancelled or expired");
        }
        let timeout = match native_stage_remaining(invocation) {
            Ok(remaining) => remaining,
            Err(reason) => return failure(reason),
        };
        let limits = FramedProcessLimits {
            max_frame_bytes: FRAME_BYTES,
            max_queued_frames: 4,
            max_stderr_bytes: 4096,
            timeout,
        };
        let mut process = match owner.spawn_framed(command, limits, token.clone()) {
            Ok(process) => process,
            Err(_) => return failure("native structured process spawn failed"),
        };
        let mut unsettled_on_drop = UnsettledOnDrop(Some(attribution));
        let mut evidence = Evidence::default();
        let driven = tokio::select! {
            biased;
            _ = token.cancelled() => Err("native invocation cancelled".into()),
            result = drive_with_cached_catalog(&mut process, &stage, cwd_text, &sandbox, &mut evidence, output_limit, Some(gate), &token, input_rx, cached_catalog.as_ref()) => result,
        };
        // EOF after a terminal notification closes the native server normally.
        // Any protocol failure/cancellation uses the same physical owner; both
        // paths await authoritative descendant settlement before returning.
        let mut interrupt_acknowledged = false;
        if driven.is_err()
            && let (Some(thread), Some(turn)) = (&evidence.thread, &evidence.turn)
        {
            // Best effort protocol interruption, fenced to the actual ACKs.
            // Its ACK is not terminal evidence and never replaces settlement.
            let request = json!({"id": INTERRUPT_REQUEST_ID, "method": "turn/interrupt", "params": {"threadId": thread, "turnId": turn}});
            let input = process.input();
            interrupt_acknowledged = tokio::time::timeout(
                Duration::from_secs(1),
                rpc(
                    &mut process,
                    &input,
                    request,
                    &mut evidence,
                    output_limit,
                    None,
                    None,
                    None,
                    None,
                ),
            )
            .await
            .is_ok_and(|result| result.is_ok_and(|result| result.is_object()));
        }
        let mut cleanup_cancelled = false;
        let outcome = if driven.is_ok() {
            // Close stdin after native terminal evidence. Bound only this
            // shutdown phase, not the admitted hours/day-long task. Keep the
            // completion future owned while requesting physical cancellation.
            let completion = process.wait();
            tokio::pin!(completion);
            tokio::select! {
                result = &mut completion => result,
                _ = tokio::time::sleep(SHUTDOWN_GRACE) => {
                    cleanup_cancelled = true;
                    token.cancel();
                    completion.await
                }
            }
        } else {
            process.cancel_and_wait().await
        };
        let settled = outcome
            .as_ref()
            .ok()
            .and_then(|outcome| outcome.settlement.as_ref())
            .is_some_and(|settlement| settlement.ownership.is_authoritative());
        if settled {
            unsettled_on_drop.0.take();
        }
        let transport_ok = outcome.as_ref().is_ok_and(|outcome| {
            (matches!(outcome.end, FramedProcessEnd::Exited)
                && outcome.status.is_some_and(|status| status.success()))
                || (cleanup_cancelled && matches!(outcome.end, FramedProcessEnd::Cancelled))
        });
        let target_released = outcome
            .as_ref()
            .ok()
            .and_then(|outcome| outcome.target_released);
        let is_error = driven.is_err()
            || evidence.terminal.as_deref() != Some("completed")
            || !settled
            || !transport_ok;
        let stage_accounting = evidence.stage_usage_with_inclusive().ok().flatten();
        let stage_usage = stage_accounting.map(|(_, usage)| usage);
        let session_acknowledged = evidence.thread.is_some();
        let turn_acknowledged = evidence.turn.is_some();
        let observation = astra_turn_types::NativeCollaboratorObservation {
            native_session_id: evidence.thread.clone(),
            native_turn_id: evidence.turn.clone(),
            dispatch_state: if turn_acknowledged {
                astra_turn_types::NativeStageDispatchState::Acknowledged
            } else if evidence.turn_queued {
                astra_turn_types::NativeStageDispatchState::Unknown
            } else {
                astra_turn_types::NativeStageDispatchState::NotDispatched
            },
            native_terminal: evidence.terminal.clone(),
            settlement_authoritative: settled,
            stage_inclusive_input_tokens: stage_accounting.and_then(|(input, _)| input),
            stage_usage,
            last_request_input_tokens: evidence
                .usage
                .as_ref()
                .and_then(|usage| usage["last"]["inputTokens"].as_u64()),
            model_context_window: evidence
                .usage
                .as_ref()
                .and_then(|usage| usage["modelContextWindow"].as_u64()),
            acknowledged_model: evidence.acknowledged_model.clone(),
            provider_error_code: evidence.provider_error_code,
            provider_error_class: evidence.provider_error_class.map(str::to_owned),
        };
        let native_session =
            evidence
                .thread
                .as_ref()
                .map(|id| astra_services::runs::CollaboratorNativeSession {
                    anchor_run_id: stage.anchor_run_id,
                    provider: astra_services::runs::CollaboratorProvider::Codex,
                    native_session_id: id.clone(),
                });
        // Delta notifications are progress, not the authoritative final
        // message. The completed item replaces a possibly truncated progress
        // buffer while retaining the same byte/UTF-8 budget.
        let mut output = evidence.final_output.take().unwrap_or(evidence.output);
        if is_error {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!(
                "Error: {}",
                driven.err().unwrap_or_else(|| {
                    "native stage did not complete with settled transport".into()
                })
            ));
        }
        let mut metadata = json!({
                "native_collaborator": {
                    "provider": "codex", "protocol": "codex-app-server",
                    "native_session_id": evidence.thread, "native_turn_id": evidence.turn,
                    "requested_model": stage.model, "resolved_model": evidence.resolved_model,
                    "acknowledged_model": evidence.acknowledged_model,
                    "model_resolution": if stage.model.is_some() {"provider_catalog"} else {"provider_default"},
                    "session_acknowledged": session_acknowledged, "turn_acknowledged": turn_acknowledged,
                    "dispatch_state": if turn_acknowledged {"acknowledged"} else if evidence.turn_queued {"unknown"} else {"not_dispatched"},
                    "native_terminal": evidence.terminal, "usage_snapshot": evidence.usage, "cost_usd": null,
                    "provider_error_code": evidence.provider_error_code,
                    "provider_error_class": evidence.provider_error_class,
                    "provider_error_service": evidence.provider_error_service,
                    "model_selection": evidence.model_selection,
                    "output_capped": evidence.output_capped, "target_released": target_released,
                    "interrupt_acknowledged": interrupt_acknowledged,
                    "settlement_authoritative": settled, "transport_settled_after_terminal": transport_ok,
                    "cleanup_cancelled": cleanup_cancelled
                },
                "workspace_effect_settled": settled
            }).as_object().unwrap().clone();
        if let Some(session) = native_session {
            metadata.insert(
                astra_services::runs::COLLABORATOR_NATIVE_SESSION_METADATA_KEY.into(),
                json!(session),
            );
        }
        if let Some(usage) = stage_usage {
            metadata.insert("collaborator_usage".into(), json!(usage));
        }
        if let Some(observation) =
            astra_turn_types::project_native_collaborator_observation(&json!(observation))
        {
            metadata.insert(
                astra_turn_types::NATIVE_COLLABORATOR_OBSERVATION_KEY.into(),
                observation,
            );
        }
        if evidence.capability_unavailable {
            metadata.insert("native_capability_unavailable".into(), Value::Bool(true));
        }
        ToolResult {
            output,
            is_error,
            exit_semantics: None,
            metadata: Some(metadata),
        }
    }
}

#[cfg(test)]
#[path = "native_codex/tests.rs"]
mod tests;
