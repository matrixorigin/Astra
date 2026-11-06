//! Transactional preimages for commands that may consume irreplaceable input.
//!
//! This module is deliberately opt-in. Ordinary shell calls keep their normal
//! semantics; a caller that needs a hard evidence-preservation guarantee adds
//! `source_artifacts: ["relative/path", ...]` to the bash arguments. The
//! preflight then resolves only existing regular files below the bound
//! workspace, stores content-addressed copies outside that workspace, and
//! refuses to spawn the command if the capture is incomplete or races with a
//! concurrent writer.

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const SOURCE_ARTIFACTS_FIELD: &str = "source_artifacts";
pub const MAX_SOURCE_ARTIFACTS: usize = 32;
pub const MAX_SOURCE_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_INFERRED_SOURCE_ARTIFACTS: usize = 8;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourcePreimageEntry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
    pub blob_id: String,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub post_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourcePreimageReceipt {
    pub schema_version: u32,
    pub receipt_id: String,
    pub scope: String,
    pub entries: Vec<SourcePreimageEntry>,
}

#[derive(Debug, Clone)]
pub struct PreparedSourcePreimages {
    root: PathBuf,
    #[cfg(unix)]
    inspection: astra_sandbox::PinnedWorkspaceInspection,
    store_root: PathBuf,
    receipt_path: PathBuf,
    mode: SourcePreimageMode,
    pub receipt: SourcePreimageReceipt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePreimageMode {
    Declared,
    Inferred,
}

impl SourcePreimageMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Inferred => "inferred_advisory",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePreimageStatus {
    Unchanged,
    Modified,
    Deleted,
    Unknown,
}

impl SourcePreimageStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unchanged => "unchanged",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Unknown => "unknown",
        }
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(not(unix))]
fn hash_file(path: &Path) -> Result<(Vec<u8>, String), String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read source artifact {}: {error}", path.display()))?;
    let hash = hash_bytes(&bytes);
    Ok((bytes, hash))
}

fn hash_opened_source(
    mut file: fs::File,
    relative: &str,
    limit: u64,
) -> Result<(Vec<u8>, String), String> {
    let before = file
        .metadata()
        .map_err(|error| format!("cannot stat source artifact `{relative}`: {error}"))?;
    if before.len() > limit {
        return Err("source_artifacts exceeds the total byte limit".into());
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read source artifact `{relative}`: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err("source_artifacts exceeds the total byte limit".into());
    }
    let after = file
        .metadata()
        .map_err(|error| format!("cannot restat source artifact `{relative}`: {error}"))?;
    if metadata_fingerprint(&before) != metadata_fingerprint(&after) {
        return Err(format!(
            "source artifact `{relative}` changed while being preserved; command was not run"
        ));
    }
    let hash = hash_bytes(&bytes);
    Ok((bytes, hash))
}

fn metadata_fingerprint(metadata: &fs::Metadata) -> (u64, Option<std::time::SystemTime>) {
    (metadata.len(), metadata.modified().ok())
}

fn scope_namespace(scope: &str) -> &str {
    scope.split(":run:").next().unwrap_or(scope)
}

fn scope_component(scope: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(scope_namespace(scope).as_bytes());
    format!("{:x}", hasher.finalize())
}

fn source_store_root(scope: &str) -> Result<PathBuf, String> {
    let base = std::env::var_os("_ASTRA_SOURCE_PREIMAGE_ROOT")
        .map(PathBuf::from)
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| {
            "source_artifacts requires a durable local data directory; configure _ASTRA_SOURCE_PREIMAGE_ROOT"
                .to_string()
        })?;
    Ok(base
        .join("astra")
        .join("source_preimages")
        .join(scope_component(scope)))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("receipt path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create source preimage store: {error}"))?;
    let temp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        // Captured source can contain private data, regardless of the
        // caller's umask or the permissions on its data directory.
        let mut file = options
            .open(&temp)
            .map_err(|error| format!("cannot create source preimage temporary file: {error}"))?;
        file.write_all(bytes)
            .map_err(|error| format!("cannot write source preimage temporary file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync source preimage temporary file: {error}"))?;
        fs::rename(&temp, path)
            .map_err(|error| format!("cannot commit source preimage file: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn ensure_blob(store_root: &Path, hash: &str, bytes: &[u8]) -> Result<String, String> {
    let blob_id = format!("sha256:{hash}");
    let blob_path = store_root
        .join("blobs")
        .join("sha256")
        .join(&hash[..2])
        .join(hash);
    if blob_path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&blob_path)
                .map_err(|error| format!("cannot inspect retained source: {error}"))?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file()
                || metadata.nlink() != 1
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err("retained source has an unsafe owner or file type".into());
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|error| format!("cannot protect retained source: {error}"))?;
        }
        return Ok(blob_id);
    }
    atomic_write(&blob_path, bytes)?;
    Ok(blob_id)
}

fn relative_artifact_path(root: &Path, raw: &str) -> Result<(String, PathBuf), String> {
    let path = Path::new(raw.trim());
    if raw.trim().is_empty() {
        return Err("source_artifacts entries must be non-empty relative paths".into());
    }
    if raw.contains('*') || raw.contains('?') || raw.contains('[') {
        return Err(format!(
            "source_artifacts entry `{raw}` must name one file; globs are not allowed"
        ));
    }
    if path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "source_artifacts entry `{raw}` must be relative to the workspace root"
        ));
    }
    let relative = path.to_string_lossy().replace('\\', "/");
    let candidate = root.join(path);
    Ok((relative, candidate))
}

fn validate_source_artifact_args(args: &Value) -> Result<Option<Vec<String>>, String> {
    let Some(value) = args.get(SOURCE_ARTIFACTS_FIELD) else {
        return Ok(None);
    };
    let entries = value.as_array().ok_or_else(|| {
        format!("{SOURCE_ARTIFACTS_FIELD} must be a non-empty array of relative file paths")
    })?;
    if entries.is_empty() {
        return Err(format!(
            "{SOURCE_ARTIFACTS_FIELD} must contain at least one source file when provided"
        ));
    }
    if entries.len() > MAX_SOURCE_ARTIFACTS {
        return Err(format!(
            "{SOURCE_ARTIFACTS_FIELD} exceeds the maximum of {MAX_SOURCE_ARTIFACTS} files"
        ));
    }
    let mut paths = Vec::with_capacity(entries.len());
    let mut seen = HashSet::new();
    for entry in entries {
        let path = entry
            .as_str()
            .ok_or_else(|| format!("{SOURCE_ARTIFACTS_FIELD} entries must be strings"))?;
        let normalized = path.trim().to_string();
        if !seen.insert(normalized.clone()) {
            return Err(format!("duplicate source_artifacts entry `{normalized}`"));
        }
        paths.push(normalized);
    }
    Ok(Some(paths))
}

/// Capture all explicitly declared source artifacts before the command starts.
/// `scope` is an owner/session binding chosen by the actual executor; an empty
/// scope is rejected so receipts cannot silently become cross-session blobs.
pub fn prepare(
    workspace_root: &Path,
    args: &Value,
    scope: &str,
) -> Result<Option<PreparedSourcePreimages>, String> {
    if validate_source_artifact_args(args)?.is_none() {
        return Ok(None);
    }
    #[cfg(unix)]
    let inspection =
        astra_sandbox::PinnedWorkspaceInspection::from_paths(workspace_root, workspace_root)
            .map_err(|error| format!("cannot pin source artifact workspace: {error}"))?;
    prepare_with_inspection(
        workspace_root,
        args,
        scope,
        #[cfg(unix)]
        inspection,
    )
}

pub fn prepare_with_inspection(
    workspace_root: &Path,
    args: &Value,
    scope: &str,
    #[cfg(unix)] inspection: astra_sandbox::PinnedWorkspaceInspection,
) -> Result<Option<PreparedSourcePreimages>, String> {
    prepare_sources(
        workspace_root,
        args,
        scope,
        #[cfg(unix)]
        inspection,
        #[cfg(unix)]
        None,
    )
}

fn prepare_sources(
    workspace_root: &Path,
    args: &Value,
    scope: &str,
    #[cfg(unix)] inspection: astra_sandbox::PinnedWorkspaceInspection,
    #[cfg(unix)] mut opened: Option<
        std::collections::HashMap<String, astra_sandbox::OpenedWorkspaceFile>,
    >,
) -> Result<Option<PreparedSourcePreimages>, String> {
    let Some(raw_paths) = validate_source_artifact_args(args)? else {
        return Ok(None);
    };
    if crate::rpc_bridge::is_run_script_rpc_dispatch() {
        return Err("source_artifacts requires a separate foreground command; the parent script can modify the same files concurrently".into());
    }
    if scope.trim().is_empty() {
        return Err(
            "source_artifacts requires an active owner/session execution identity; command was not run"
                .into(),
        );
    }
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root for source_artifacts: {error}"))?;
    let store_root = source_store_root(scope)?;
    let receipt_id = Uuid::new_v4().to_string();
    let receipt_dir = store_root.join("receipts");
    let receipt_path = receipt_dir.join(format!("{receipt_id}.json"));
    let mut entries = Vec::with_capacity(raw_paths.len());
    let mut total_bytes = 0_u64;

    for raw in raw_paths {
        let (relative, candidate) = relative_artifact_path(&root, &raw)?;
        #[cfg(unix)]
        let (bytes, sha256, parent_identity) = {
            let source = if let Some(opened) = opened.as_mut() {
                opened
                    .remove(&relative)
                    .ok_or_else(|| format!("inferred source `{relative}` has no retained handle"))?
            } else {
                inspection
                    .open_source(Path::new(&relative), true)
                    .map_err(|error| {
                        format!("cannot inspect source artifact `{relative}`: {error}")
                    })?
            };
            let (bytes, hash) = hash_opened_source(
                source.file,
                &relative,
                MAX_SOURCE_ARTIFACT_BYTES.saturating_sub(total_bytes),
            )?;
            (bytes, hash, Some(source.parent_identity))
        };
        #[cfg(not(unix))]
        let (bytes, sha256) = {
            let link_metadata = fs::symlink_metadata(&candidate)
                .map_err(|error| format!("cannot inspect source artifact `{relative}`: {error}"))?;
            if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
                return Err(format!(
                    "source_artifacts entry `{relative}` must be an existing regular file (symlinks and directories are not allowed)"
                ));
            }
            let canonical = candidate
                .canonicalize()
                .map_err(|error| format!("cannot resolve source artifact `{relative}`: {error}"))?;
            if !canonical.starts_with(&root) {
                return Err(format!(
                    "source_artifacts entry `{relative}` escapes the workspace root"
                ));
            }
            let before =
                metadata_fingerprint(&fs::metadata(&canonical).map_err(|error| {
                    format!("cannot stat source artifact `{relative}`: {error}")
                })?);
            let (bytes, sha256) = hash_file(&canonical)?;
            let after_metadata = fs::metadata(&canonical)
                .map_err(|error| format!("cannot restat source artifact `{relative}`: {error}"))?;
            if before != metadata_fingerprint(&after_metadata) {
                return Err(format!(
                    "source artifact `{relative}` changed while being preserved; command was not run"
                ));
            }
            (bytes, sha256)
        };
        #[cfg(not(unix))]
        let parent_identity = None;
        let _ = candidate;
        let bytes_len = bytes.len() as u64;
        total_bytes = total_bytes
            .checked_add(bytes_len)
            .ok_or_else(|| "source_artifacts byte limit overflow".to_string())?;
        if total_bytes > MAX_SOURCE_ARTIFACT_BYTES {
            return Err(format!(
                "source_artifacts exceeds the total byte limit of {}",
                MAX_SOURCE_ARTIFACT_BYTES
            ));
        }
        let blob_id = ensure_blob(&store_root, &sha256, &bytes)?;
        entries.push(SourcePreimageEntry {
            path: relative,
            bytes: bytes_len,
            sha256,
            blob_id,
            status: Some("captured".into()),
            post_sha256: None,
            parent_identity,
        });
    }

    let receipt = SourcePreimageReceipt {
        schema_version: 1,
        receipt_id,
        scope: scope.to_string(),
        entries,
    };
    let manifest = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| format!("cannot encode source preimage receipt: {error}"))?;
    atomic_write(&receipt_path, &manifest)?;
    Ok(Some(PreparedSourcePreimages {
        root,
        #[cfg(unix)]
        inspection,
        store_root,
        receipt_path,
        mode: SourcePreimageMode::Declared,
        receipt,
    }))
}

/// Best-effort protection for a command that did not explicitly declare
/// `source_artifacts`.  This intentionally has a much narrower scope than a
/// shell policy: only known stateful/mutating command families are inspected,
/// only exact existing regular-file operands below the workspace are kept,
/// and any ambiguity simply returns `None` so ordinary bash is unaffected.
///
/// The resulting receipt is advisory (`inferred_advisory`), not a substitute
/// for the explicit hard guarantee.  It nevertheless preserves the common
/// failure mode where a model copies/opens an input in one compound command
/// before it has a chance to react to an advisory.
pub fn prepare_inferred(
    workspace_root: &Path,
    execution_dir: &Path,
    command: &str,
    scope: &str,
) -> Result<Option<PreparedSourcePreimages>, String> {
    if crate::rpc_bridge::is_run_script_rpc_dispatch() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        let inspection =
            astra_sandbox::PinnedWorkspaceInspection::from_paths(workspace_root, execution_dir)
                .map_err(|error| error.to_string())?;
        prepare_inferred_with_inspection(workspace_root, inspection, command, scope)
    }
    #[cfg(not(unix))]
    {
        let paths = infer_source_artifacts(workspace_root, execution_dir, command);
        if paths.is_empty() {
            return Ok(None);
        }
        let args = json!({SOURCE_ARTIFACTS_FIELD: paths});
        let Some(mut plan) = prepare(workspace_root, &args, scope)? else {
            return Ok(None);
        };
        plan.mode = SourcePreimageMode::Inferred;
        Ok(Some(plan))
    }
}

#[cfg(unix)]
pub fn prepare_inferred_with_inspection(
    workspace_root: &Path,
    inspection: astra_sandbox::PinnedWorkspaceInspection,
    command: &str,
    scope: &str,
) -> Result<Option<PreparedSourcePreimages>, String> {
    if crate::rpc_bridge::is_run_script_rpc_dispatch() {
        return Ok(None);
    }
    let opened = std::cell::RefCell::new(std::collections::HashMap::new());
    let paths = infer_source_artifacts_with(
        command,
        |token| {
            if !is_inferable_operand(token) {
                return None;
            }
            let source = inspection.open_source(Path::new(token), false).ok()?;
            let relative = source.workspace_relative.to_str()?.replace('\\', "/");
            opened
                .borrow_mut()
                .entry(relative.clone())
                .or_insert(source);
            Some(relative)
        },
        |relative| {
            let siblings = {
                let opened = opened.borrow();
                let Some(source) = opened.get(relative) else {
                    return Vec::new();
                };
                inspection
                    .regular_siblings(Path::new(relative), &source.file, 256)
                    .unwrap_or_default()
            };
            siblings
                .into_iter()
                .filter_map(|source| {
                    let relative = source.workspace_relative.to_str()?.to_string();
                    opened
                        .borrow_mut()
                        .entry(relative.clone())
                        .or_insert(source);
                    Some(relative)
                })
                .collect()
        },
    );
    if paths.is_empty() {
        return Ok(None);
    }
    let args = json!({SOURCE_ARTIFACTS_FIELD: paths});
    let Some(mut plan) = prepare_sources(
        workspace_root,
        &args,
        scope,
        inspection,
        Some(opened.into_inner()),
    )?
    else {
        return Ok(None);
    };
    plan.mode = SourcePreimageMode::Inferred;
    Ok(Some(plan))
}

fn split_command_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    let chars: Vec<char> = command.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if escaped {
            current.push(ch);
            escaped = false;
            index += 1;
            continue;
        }
        if ch == '\\' && !single {
            current.push(ch);
            escaped = true;
            index += 1;
            continue;
        }
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            ';' | '\n' if !single && !double => {
                if !current.trim().is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                index += 1;
                continue;
            }
            '&' | '|' if !single && !double => {
                if !current.trim().is_empty() {
                    segments.push(std::mem::take(&mut current));
                }
                index += 1;
                if chars.get(index) == Some(&ch) {
                    index += 1;
                }
                continue;
            }
            _ => {}
        }
        current.push(ch);
        index += 1;
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

fn shell_words(segment: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for ch in segment.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !single {
            escaped = true;
            continue;
        }
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            c if c.is_whitespace() && !single && !double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn inferred_command_name(words: &[String]) -> Option<&str> {
    let mut index = 0;
    while let Some(word) = words.get(index) {
        if word.contains('=') && !word.starts_with('-') {
            index += 1;
            continue;
        }
        if matches!(
            word.as_str(),
            "sudo" | "env" | "command" | "builtin" | "time" | "nice"
        ) {
            index += 1;
            continue;
        }
        return word.rsplit('/').next();
    }
    None
}

/// A deliberately small proof set. Unknown commands are not classified by
/// name or task domain: they are treated as potentially stateful for the sole
/// purpose of a bounded, best-effort preimage. This never blocks execution.
fn command_is_proven_observation_only(name: &str) -> bool {
    matches!(
        name,
        "sha256sum"
            | "sha512sum"
            | "md5sum"
            | "b2sum"
            | "file"
            | "cat"
            | "head"
            | "tail"
            | "od"
            | "xxd"
            | "strings"
            | "grep"
            | "rg"
            | "wc"
            | "stat"
    )
}

fn command_has_unquoted_output_redirect(command: &str) -> bool {
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for ch in command.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && !single {
            escaped = true;
            continue;
        }
        match ch {
            '\'' if !double => single = !single,
            '"' if !single => double = !double,
            '>' if !single && !double => return true,
            _ => {}
        }
    }
    false
}

fn is_inferable_operand(token: &str) -> bool {
    !(token.is_empty()
        || token.starts_with('-')
        || token == "."
        || token == ".."
        || token.contains('*')
        || token.contains('?')
        || token.contains('[')
        || token.contains('='))
}

#[cfg(any(test, not(unix)))]
fn inferred_operand_path(root: &Path, execution_dir: &Path, token: &str) -> Option<String> {
    if !is_inferable_operand(token) {
        return None;
    }
    let raw = Path::new(token);
    let candidate = if raw.is_absolute() {
        let canonical_root = root.canonicalize().ok()?;
        let canonical = raw.canonicalize().ok()?;
        if !canonical.starts_with(&canonical_root) {
            return None;
        }
        canonical
    } else {
        execution_dir.join(raw)
    };
    let link_metadata = fs::symlink_metadata(&candidate).ok()?;
    if link_metadata.file_type().is_symlink() || !link_metadata.is_file() {
        return None;
    }
    let canonical_root = root.canonicalize().ok()?;
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    canonical
        .strip_prefix(canonical_root)
        .ok()?
        .to_str()
        .map(|value| value.replace('\\', "/"))
}

/// Include bounded, adjacent sidecars for an inferred source. Many tools keep
/// transactional journals, locks, or recovery fragments beside the primary
/// file; treating an exact operand as only one inode is an incomplete
/// preimage. This is purely a best-effort advisory expansion, so ambiguous or
/// over-large sibling sets are ignored rather than blocking the command.
#[cfg(any(test, not(unix)))]
fn inferred_companion_paths(root: &Path, relative: &str) -> Vec<String> {
    let path = Path::new(relative);
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Vec::new();
    };
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    let directory = root.join(parent);
    let mut companions = fs::read_dir(directory)
        .ok()
        .into_iter()
        .flat_map(|entries| entries.filter_map(Result::ok))
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if name == file_name
                || !name.starts_with(file_name)
                || !name
                    .as_bytes()
                    .get(file_name.len())
                    .is_some_and(|byte| matches!(*byte, b'.' | b'-' | b'_' | b'~'))
            {
                return None;
            }
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return None;
            }
            Some(if parent.as_os_str().is_empty() {
                name
            } else {
                format!("{}/{}", parent.to_string_lossy().replace('\\', "/"), name)
            })
        })
        .collect::<Vec<_>>();
    companions.sort();
    companions.truncate(MAX_INFERRED_SOURCE_ARTIFACTS);
    companions
}

#[cfg(any(test, not(unix)))]
fn infer_source_artifacts(root: &Path, execution_dir: &Path, command: &str) -> Vec<String> {
    infer_source_artifacts_with(
        command,
        |token| inferred_operand_path(root, execution_dir, token),
        |relative| inferred_companion_paths(root, relative),
    )
}

fn infer_source_artifacts_with(
    command: &str,
    mut operand: impl FnMut(&str) -> Option<String>,
    mut companions_for: impl FnMut(&str) -> Vec<String>,
) -> Vec<String> {
    let segments = split_command_segments(command);
    let words_by_segment: Vec<Vec<String>> = segments
        .iter()
        .map(|segment| shell_words(segment))
        .collect();
    // A command that is not proven observation-only anywhere in a compound
    // invocation is the boundary. Preserve exact existing file operands from
    // adjacent observation commands too, without interpreting the task domain
    // or maintaining a vocabulary of stateful programs.
    if !command_has_unquoted_output_redirect(command)
        && !words_by_segment.iter().any(|words| {
            inferred_command_name(words)
                .is_some_and(|name| !command_is_proven_observation_only(name))
        })
    {
        return Vec::new();
    }
    let mut paths = Vec::new();
    for words in words_by_segment {
        let Some(_name) = inferred_command_name(&words) else {
            continue;
        };
        for token in words.iter().skip(1) {
            let Some(path) = operand(token) else {
                continue;
            };
            let companions = companions_for(&path);
            if !paths.contains(&path) {
                paths.push(path);
            }
            for companion in companions {
                if !paths.contains(&companion) {
                    paths.push(companion);
                }
            }
            if paths.len() > MAX_INFERRED_SOURCE_ARTIFACTS {
                return Vec::new();
            }
        }
    }
    paths
}

/// Load a completed receipt for the current owner/session. The caller
/// supplies only its stable owner scope (not another call's full invocation
/// scope); the receipt store is namespaced by that stable prefix and the
/// receipt itself still records the original full binding for audit.
pub fn prepare_receipt_restore(
    workspace_root: &Path,
    owner_scope: &str,
    receipt_id: &str,
) -> Result<PreparedSourcePreimages, String> {
    if owner_scope.trim().is_empty() {
        return Err("source receipt restore requires an active owner/session identity".into());
    }
    let receipt_uuid = Uuid::parse_str(receipt_id)
        .map_err(|_| "source receipt restore requires a valid receipt_id".to_string())?;
    let root = workspace_root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root for source receipt: {error}"))?;
    let store_root = source_store_root(owner_scope)?;
    let receipt_path = store_root
        .join("receipts")
        .join(format!("{receipt_uuid}.json"));
    let bytes = fs::read(&receipt_path)
        .map_err(|error| format!("cannot read source receipt {receipt_id}: {error}"))?;
    let receipt: SourcePreimageReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot decode source receipt {receipt_id}: {error}"))?;
    if scope_namespace(&receipt.scope) != scope_namespace(owner_scope) {
        return Err("source receipt belongs to a different owner/session".into());
    }
    if receipt.entries.iter().any(|entry| {
        !matches!(
            entry.status.as_deref(),
            Some("unchanged" | "modified" | "deleted")
        )
    }) {
        return Err("source receipt was not completed; no files were restored".into());
    }
    Ok(PreparedSourcePreimages {
        #[cfg(unix)]
        inspection: astra_sandbox::PinnedWorkspaceInspection::from_paths(&root, &root)
            .map_err(|error| error.to_string())?,
        root,
        store_root,
        receipt_path,
        mode: SourcePreimageMode::Declared,
        receipt,
    })
}

/// Produce a concise model-facing advisory only when best-effort inference
/// observed a source change. Host storage paths and bytes remain private; the
/// opaque receipt id is consumed by the receipt-aware rollback surface.
pub fn advisory_text(metadata: &Map<String, Value>) -> Option<String> {
    let preimage = metadata.get("source_preimage")?.as_object()?;
    if preimage.get("restore_available").and_then(Value::as_bool) == Some(false) {
        return Some(
            "Source recovery could not be verified; automatic restoration is unavailable.".into(),
        );
    }
    if preimage.get("mode").and_then(Value::as_str) != Some("inferred_advisory")
        || preimage.get("status").and_then(Value::as_str) != Some("changed")
    {
        return None;
    }
    let receipt_id = preimage
        .get("receipt_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let changed_paths = preimage
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            matches!(
                entry.get("status").and_then(Value::as_str),
                Some("modified" | "deleted")
            )
        })
        .filter_map(|entry| entry.get("path").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if changed_paths.is_empty() {
        return None;
    }
    Some(format!(
        "[A pre-command copy was retained for {} (receipt {}). The current files differ, but the cause is unknown; inspect them before deciding whether to restore the copy.]",
        changed_paths.join(", "),
        receipt_id
    ))
}

impl PreparedSourcePreimages {
    /// Paths that a completed receipt would restore.
    pub fn restore_paths(&self) -> impl Iterator<Item = PathBuf> + '_ {
        self.receipt
            .entries
            .iter()
            .filter(|entry| matches!(entry.status.as_deref(), Some("modified" | "deleted")))
            .map(|entry| self.root.join(&entry.path))
    }

    /// Compare the declared sources after command execution and persist the
    /// terminal receipt. The returned object is safe to expose as tool
    /// metadata: it contains no host storage paths or copied bytes.
    pub fn finish(&mut self) -> Map<String, Value> {
        let mut statuses = Vec::with_capacity(self.receipt.entries.len());
        for entry in &mut self.receipt.entries {
            #[cfg(unix)]
            let status = match self.inspection.open_source(Path::new(&entry.path), true) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    SourcePreimageStatus::Deleted
                }
                Err(_) => SourcePreimageStatus::Unknown,
                Ok(source)
                    if entry
                        .parent_identity
                        .as_deref()
                        .is_some_and(|expected| expected != source.parent_identity) =>
                {
                    SourcePreimageStatus::Unknown
                }
                Ok(source) => {
                    match hash_opened_source(source.file, &entry.path, MAX_SOURCE_ARTIFACT_BYTES) {
                        Ok((_, hash)) if hash == entry.sha256 => SourcePreimageStatus::Unchanged,
                        Ok((_, hash)) => {
                            entry.post_sha256 = Some(hash);
                            SourcePreimageStatus::Modified
                        }
                        Err(_) => SourcePreimageStatus::Unknown,
                    }
                }
            };
            #[cfg(not(unix))]
            let status = {
                let path = self.root.join(Path::new(&entry.path));
                let status = match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                        SourcePreimageStatus::Unknown
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        SourcePreimageStatus::Deleted
                    }
                    Err(_) => SourcePreimageStatus::Unknown,
                    Ok(_) => match hash_file(&path) {
                        Ok((_, hash)) if hash == entry.sha256 => SourcePreimageStatus::Unchanged,
                        Ok((_, hash)) => {
                            entry.post_sha256 = Some(hash);
                            SourcePreimageStatus::Modified
                        }
                        Err(_) => SourcePreimageStatus::Unknown,
                    },
                };
                status
            };
            entry.status = Some(status.as_str().to_string());
            statuses.push(json!({
                "path": entry.path,
                "status": status.as_str(),
                "bytes": entry.bytes,
                "sha256": entry.sha256,
                "blob_id": entry.blob_id,
                "post_sha256": entry.post_sha256,
            }));
        }
        let postimages_known = self.receipt.entries.iter().all(|entry| {
            matches!(
                entry.status.as_deref(),
                Some("unchanged" | "modified" | "deleted")
            )
        });
        let persisted = serde_json::to_vec_pretty(&self.receipt)
            .map_err(|error| error.to_string())
            .and_then(|manifest| atomic_write(&self.receipt_path, &manifest))
            .is_ok();
        let restore_available = persisted && postimages_known;
        let changed = statuses.iter().any(|status| {
            matches!(
                status.get("status").and_then(Value::as_str),
                Some("modified" | "deleted")
            )
        });
        let mut result = Map::new();
        result.insert(
            "source_preimage".into(),
            json!({
                "schema_version": 1,
                "source": "astra_source_preimage_store",
                "receipt_id": self.receipt.receipt_id,
                "mode": self.mode.as_str(),
                "guarantee": matches!(self.mode, SourcePreimageMode::Declared),
                "status": if !postimages_known { "unknown" } else if changed { "changed" } else { "unchanged" },
                "entries": statuses,
                "restore_available": restore_available,
            }),
        );
        result
    }

    /// Restore only sources that were changed/deleted and only when the
    /// current file still matches the recorded post-image. A third-party edit
    /// after the command therefore fails closed instead of being overwritten.
    pub fn restore(&self) -> Result<(), String> {
        if self.receipt.entries.iter().any(|entry| {
            !matches!(
                entry.status.as_deref(),
                Some("unchanged" | "modified" | "deleted")
            )
        }) {
            return Err("source receipt was not completed; no files were restored".into());
        }
        #[cfg(unix)]
        if !self
            .inspection
            .matches_root_path(&self.root)
            .unwrap_or(false)
        {
            return Err("restore conflict: workspace binding changed".into());
        }
        let mut restored = Vec::new();
        for entry in &self.receipt.entries {
            let Some(status) = entry.status.as_deref() else {
                continue;
            };
            if !matches!(status, "modified" | "deleted") {
                continue;
            }
            let result = (|| -> Result<(), String> {
                let blob_hash = entry
                    .blob_id
                    .strip_prefix("sha256:")
                    .filter(|hash| hash.len() == 64 && hash.bytes().all(|c| c.is_ascii_hexdigit()))
                    .ok_or_else(|| "invalid source preimage blob id".to_string())?;
                let blob = self
                    .store_root
                    .join("blobs")
                    .join("sha256")
                    .join(&blob_hash[..2])
                    .join(blob_hash);
                let bytes = fs::read(&blob)
                    .map_err(|error| format!("cannot read source preimage blob: {error}"))?;
                if hash_bytes(&bytes) != blob_hash || entry.sha256 != blob_hash {
                    return Err("source preimage blob is corrupted".into());
                }
                #[cfg(unix)]
                {
                    let parent_id = entry.parent_identity.as_deref().ok_or_else(|| {
                        format!("restore conflict: `{}` has no parent identity", entry.path)
                    })?;
                    let relative = Path::new(&entry.path);
                    let (parent, name) = self
                        .inspection
                        .open_restore_parent(relative, Some(parent_id))
                        .map_err(|error| format!("restore conflict: `{}`: {error}", entry.path))?;
                    let current = if status == "modified" {
                        let expected = entry.post_sha256.as_deref().ok_or_else(|| {
                            format!(
                                "restore conflict: `{}` has no recorded post-image",
                                entry.path
                            )
                        })?;
                        let source =
                            self.inspection
                                .open_source(relative, true)
                                .map_err(|error| {
                                    format!("restore conflict: `{}`: {error}", entry.path)
                                })?;
                        if source.parent_identity != parent_id {
                            return Err(format!(
                                "restore conflict: `{}` parent changed",
                                entry.path
                            ));
                        }
                        let (current, hash) = hash_opened_source(
                            source.file,
                            &entry.path,
                            MAX_SOURCE_ARTIFACT_BYTES,
                        )?;
                        // A previous attempt may already have restored this
                        // path before a later path conflicted.
                        if hash == blob_hash {
                            return Ok(());
                        }
                        if hash != expected {
                            return Err(format!(
                                "restore conflict: `{}` changed after the command",
                                entry.path
                            ));
                        }
                        Some(current)
                    } else if entry.post_sha256.is_none() {
                        match self.inspection.open_source(relative, true) {
                            Ok(source) if source.parent_identity == parent_id => {
                                let (_, hash) = hash_opened_source(
                                    source.file,
                                    &entry.path,
                                    MAX_SOURCE_ARTIFACT_BYTES,
                                )?;
                                if hash == blob_hash {
                                    return Ok(());
                                }
                                return Err(format!(
                                    "restore conflict: `{}` was recreated",
                                    entry.path
                                ));
                            }
                            Ok(_) => {
                                return Err(format!(
                                    "restore conflict: `{}` parent changed",
                                    entry.path
                                ));
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                            Err(error) => {
                                return Err(format!("restore conflict: `{}`: {error}", entry.path));
                            }
                        }
                    } else {
                        return Err(format!(
                            "restore conflict: `{}` has inconsistent post-image",
                            entry.path
                        ));
                    };
                    crate::fs_ops::restore_file_in_open_parent(
                        &parent,
                        Path::new(&name),
                        current.as_deref(),
                        Some(&bytes),
                    )
                    .map_err(|error| format!("restore conflict: `{}`: {error}", entry.path))?;
                }
                #[cfg(not(unix))]
                {
                    let path = self.root.join(Path::new(&entry.path));
                    if status == "modified" {
                        let expected = entry.post_sha256.as_deref().ok_or_else(|| {
                            format!(
                                "restore conflict: `{}` has no recorded post-image",
                                entry.path
                            )
                        })?;
                        let current = hash_file(&path).map(|(_, hash)| hash).map_err(|_| {
                            format!("restore conflict: `{}` is no longer readable", entry.path)
                        })?;
                        if current == blob_hash {
                            return Ok(());
                        }
                        if current != expected {
                            return Err(format!(
                                "restore conflict: `{}` changed after the command",
                                entry.path
                            ));
                        }
                    } else if path.exists() {
                        if hash_file(&path).is_ok_and(|(_, hash)| hash == blob_hash) {
                            return Ok(());
                        }
                        return Err(format!("restore conflict: `{}` was recreated", entry.path));
                    }
                    fs::write(&path, bytes)
                        .map_err(|error| format!("cannot restore `{}`: {error}", entry.path))?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                return if restored.is_empty() {
                    Err(error)
                } else {
                    Err(format!(
                        "{error}; already restored: {}",
                        restored.join(", ")
                    ))
                };
            }
            restored.push(entry.path.as_str());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn args(paths: &[&str]) -> Value {
        json!({"command": "true", "source_artifacts": paths})
    }

    fn with_store<T>(f: impl FnOnce(&TempDir) -> T) -> T {
        let store = TempDir::new().unwrap();
        // Rust 2024 marks process-environment mutation unsafe because it can
        // race another thread's getenv. These tests are serialized below and
        // the production path never mutates the environment.
        unsafe { std::env::set_var("_ASTRA_SOURCE_PREIMAGE_ROOT", store.path()) };
        let result = f(&store);
        unsafe { std::env::remove_var("_ASTRA_SOURCE_PREIMAGE_ROOT") };
        result
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn source_restore_reports_partial_results_and_refuses_hardlinks() {
        use std::os::unix::fs::PermissionsExt;
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            let first = workspace.path().join("first");
            let second = workspace.path().join("second");
            let linked = workspace.path().join("linked");
            fs::write(&first, b"first before").unwrap();
            fs::write(&second, b"second before").unwrap();
            let mut plan = prepare(workspace.path(), &args(&["first", "second"]), "cli:test")
                .unwrap()
                .unwrap();
            fs::write(&first, b"first after").unwrap();
            fs::write(&second, b"second after").unwrap();
            plan.finish();
            fs::hard_link(&second, &linked).unwrap();

            let error = plan.restore().unwrap_err();
            assert!(error.contains("already restored: first"), "{error}");
            assert_eq!(fs::read(&first).unwrap(), b"first before");
            assert_eq!(fs::read(&second).unwrap(), b"second after");
            assert_eq!(fs::read(&linked).unwrap(), b"second after");
            fs::remove_file(&linked).unwrap();
            plan.restore().unwrap();
            assert_eq!(fs::read(&first).unwrap(), b"first before");
            assert_eq!(fs::read(&second).unwrap(), b"second before");
            let hash = plan.receipt.entries[0]
                .blob_id
                .strip_prefix("sha256:")
                .unwrap();
            let blob = plan
                .store_root
                .join("blobs/sha256")
                .join(&hash[..2])
                .join(hash);
            assert_eq!(
                fs::metadata(blob).unwrap().permissions().mode() & 0o777,
                0o600
            );
        });
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn deleted_private_source_stays_private_when_restored() {
        use std::os::unix::fs::PermissionsExt;
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            let path = workspace.path().join("private");
            fs::write(&path, b"private data").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let mut plan = prepare(workspace.path(), &args(&["private"]), "cli:test")
                .unwrap()
                .unwrap();
            assert!(plan.restore().is_err());
            fs::remove_file(&path).unwrap();
            plan.finish();
            plan.restore().unwrap();
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        });
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn incomplete_or_unpersisted_receipt_never_claims_restore_is_available() {
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            let path = workspace.path().join("input");
            fs::write(&path, b"before").unwrap();
            let mut plan = prepare(workspace.path(), &args(&["input"]), "cli:test")
                .unwrap()
                .unwrap();
            let id = plan.receipt.receipt_id.clone();
            assert!(prepare_receipt_restore(workspace.path(), "cli:test", &id).is_err());
            fs::write(&path, b"after").unwrap();
            fs::remove_file(&plan.receipt_path).unwrap();
            fs::remove_dir(plan.receipt_path.parent().unwrap()).unwrap();
            fs::write(plan.receipt_path.parent().unwrap(), b"not a directory").unwrap();
            let metadata = plan.finish();
            assert_eq!(metadata["source_preimage"]["restore_available"], false);
            assert!(advisory_text(&metadata).is_some());
            assert!(prepare_receipt_restore(workspace.path(), "cli:test", &id).is_err());
            assert_eq!(fs::read(path).unwrap(), b"after");
        });
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn unknown_postimage_preserves_snapshot_but_cannot_be_restored() {
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            let path = workspace.path().join("input");
            fs::write(&path, b"before").unwrap();
            let mut plan = prepare(workspace.path(), &args(&["input"]), "cli:test")
                .unwrap()
                .unwrap();
            fs::remove_file(&path).unwrap();
            fs::create_dir(&path).unwrap();
            let metadata = plan.finish();
            assert_eq!(metadata["source_preimage"]["status"], "unknown");
            assert_eq!(metadata["source_preimage"]["restore_available"], false);
            assert!(
                prepare_receipt_restore(workspace.path(), "cli:test", &plan.receipt.receipt_id)
                    .is_err()
            );
            assert!(path.is_dir());
            assert!(plan.store_root.join("blobs/sha256").exists());
        });
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn inferred_capture_keeps_prepared_directory_and_refuses_replaced_parent_restore() {
        with_store(|_| {
            let workspace = tempfile::tempdir().unwrap();
            let nested = workspace.path().join("nested");
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("input"), b"original").unwrap();
            fs::write(nested.join("input-wal"), b"sidecar").unwrap();
            let inspection =
                astra_sandbox::PinnedWorkspaceInspection::from_paths(workspace.path(), &nested)
                    .unwrap();
            fs::rename(&nested, workspace.path().join("retained")).unwrap();
            fs::create_dir(&nested).unwrap();
            fs::write(nested.join("input"), b"foreign").unwrap();
            let mut plan = prepare_inferred_with_inspection(
                workspace.path(),
                inspection,
                "custom-transform input",
                "test:pinned-source",
            )
            .unwrap()
            .unwrap();
            assert!(
                plan.receipt
                    .entries
                    .iter()
                    .any(|entry| entry.path == "retained/input"
                        && entry.sha256 == hash_bytes(b"original"))
            );
            assert!(
                plan.receipt
                    .entries
                    .iter()
                    .any(|entry| entry.path == "retained/input-wal")
            );
            fs::rename(
                workspace.path().join("retained"),
                workspace.path().join("original-parent"),
            )
            .unwrap();
            fs::create_dir(workspace.path().join("retained")).unwrap();
            fs::write(workspace.path().join("retained/input"), b"replacement").unwrap();
            plan.finish();
            assert!(plan.restore().is_err());
            assert_eq!(
                fs::read(workspace.path().join("retained/input")).unwrap(),
                b"replacement"
            );
            assert_eq!(fs::read(nested.join("input")).unwrap(), b"foreign");
        });
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn oversized_source_is_rejected_before_allocating_its_content() {
        with_store(|_| {
            let workspace = tempfile::tempdir().unwrap();
            let file = fs::File::create(workspace.path().join("large")).unwrap();
            file.set_len(MAX_SOURCE_ARTIFACT_BYTES + 1).unwrap();
            let error = prepare(
                workspace.path(),
                &json!({"source_artifacts": ["large"]}),
                "test:bounded-source",
            )
            .unwrap_err();
            assert!(error.contains("byte limit"), "{error}");
        });
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn absent_source_artifacts_preserves_normal_bash_path() {
        let workspace = TempDir::new().unwrap();
        assert!(
            prepare(workspace.path(), &json!({"command": "true"}), "session")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn capture_is_content_addressed_and_reports_deletion() {
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            fs::write(workspace.path().join("a.bin"), [1_u8, 2, 3]).unwrap();
            let plan = prepare(workspace.path(), &args(&["a.bin"]), "owner/session")
                .unwrap()
                .unwrap();
            assert_eq!(plan.receipt.entries[0].bytes, 3);
            fs::remove_file(workspace.path().join("a.bin")).unwrap();
            let mut plan = plan;
            let metadata = plan.finish();
            assert_eq!(metadata["source_preimage"]["status"], "changed");
            assert_eq!(
                metadata["source_preimage"]["entries"][0]["status"],
                "deleted"
            );
            plan.restore().unwrap();
            assert_eq!(
                fs::read(workspace.path().join("a.bin")).unwrap(),
                vec![1, 2, 3]
            );
        });
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn explicit_paths_fail_closed_for_glob_escape_directory_and_symlink() {
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            fs::write(workspace.path().join("a"), b"a").unwrap();
            fs::create_dir(workspace.path().join("dir")).unwrap();
            for raw in ["*.db", "../a", "dir"] {
                assert!(
                    prepare(workspace.path(), &args(&[raw]), "session").is_err(),
                    "{raw}"
                );
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::symlink(
                    workspace.path().join("a"),
                    workspace.path().join("link"),
                )
                .unwrap();
                assert!(prepare(workspace.path(), &args(&["link"]), "session").is_err());
            }
        });
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn missing_identity_is_rejected_before_any_capture() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("a"), b"a").unwrap();
        assert!(prepare(workspace.path(), &args(&["a"]), "").is_err());
    }

    #[test]
    fn inference_preserves_exact_operands_at_a_stateful_boundary() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("evidence.bin"), b"evidence").unwrap();
        fs::write(workspace.path().join("evidence.bin-journal"), b"journal").unwrap();
        let paths = infer_source_artifacts(
            workspace.path(),
            workspace.path(),
            "sha256sum evidence.bin evidence.bin-journal; custom-transform evidence.bin",
        );
        assert_eq!(paths, vec!["evidence.bin", "evidence.bin-journal"]);
    }

    #[test]
    fn inference_includes_adjacent_transactional_sidecars() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("record.bin"), b"record").unwrap();
        fs::write(workspace.path().join("record.bin-wal"), b"journal").unwrap();
        fs::write(workspace.path().join("record.bin-shm"), b"shared").unwrap();
        fs::write(workspace.path().join("unrelated.txt"), b"other").unwrap();
        let paths =
            infer_source_artifacts(workspace.path(), workspace.path(), "custom-open record.bin");
        assert_eq!(
            paths,
            vec!["record.bin", "record.bin-shm", "record.bin-wal"]
        );
    }

    #[test]
    fn inference_resolves_relative_operands_from_execution_directory() {
        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("input.bin"), b"input").unwrap();

        assert_eq!(
            infer_source_artifacts(workspace.path(), &nested, "custom-transform input.bin"),
            vec!["nested/input.bin"]
        );
    }

    #[test]
    fn inference_is_silent_for_read_only_or_ambiguous_commands() {
        let workspace = TempDir::new().unwrap();
        fs::write(workspace.path().join("input.bin"), b"input").unwrap();
        assert!(
            infer_source_artifacts(
                workspace.path(),
                workspace.path(),
                "cat input.bin; sha256sum input.bin"
            )
            .is_empty()
        );
        assert!(
            infer_source_artifacts(workspace.path(), workspace.path(), "grep '>' input.bin")
                .is_empty()
        );
        assert_eq!(
            infer_source_artifacts(
                workspace.path(),
                workspace.path(),
                "cat input.bin > input.bin"
            ),
            vec!["input.bin"]
        );
        assert!(
            infer_source_artifacts(workspace.path(), workspace.path(), "custom-open *.bin")
                .is_empty()
        );
        assert_eq!(
            infer_source_artifacts(workspace.path(), workspace.path(), "unknown_tool input.bin"),
            vec!["input.bin"]
        );
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn inferred_receipts_are_advisory_not_hard_guarantees() {
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            fs::write(workspace.path().join("input.bin"), b"input").unwrap();
            let mut plan = prepare_inferred(
                workspace.path(),
                workspace.path(),
                "cp input.bin output.bin",
                "owner/session",
            )
            .unwrap()
            .unwrap();
            let metadata = plan.finish();
            assert_eq!(metadata["source_preimage"]["mode"], "inferred_advisory");
            assert_eq!(metadata["source_preimage"]["guarantee"], false);
        });
    }

    #[test]
    fn source_snapshot_advisory_does_not_attribute_another_writer_or_direct_a_restore() {
        let fields = Map::from_iter([(
            "source_preimage".into(),
            json!({
                "mode": "inferred_advisory", "status": "changed", "restore_available": true,
                "receipt_id": "snapshot", "entries": [{"path": "input.bin", "status": "modified"}],
            }),
        )]);
        let advisory = advisory_text(&fields).unwrap();
        assert!(advisory.contains("cause is unknown"));
        assert!(!advisory.contains("before continuing"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn inference_follows_the_pinned_execution_directory_after_path_retarget() {
        use std::os::fd::AsRawFd;

        let workspace = TempDir::new().unwrap();
        let nested = workspace.path().join("nested");
        let retained = workspace.path().join("retained");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("input"), b"pinned").unwrap();
        let directory = fs::File::open(&nested).unwrap();

        fs::rename(&nested, &retained).unwrap();
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("input"), b"replacement").unwrap();
        let inspection_path =
            PathBuf::from("/proc/self/fd").join(directory.as_raw_fd().to_string());

        assert_eq!(
            infer_source_artifacts(workspace.path(), &inspection_path, "cp input output"),
            vec!["retained/input"]
        );
    }

    #[test]
    #[serial_test::serial(source_preimage_env)]
    fn receipt_restore_is_owner_scoped_and_compare_and_swap() {
        with_store(|_| {
            let workspace = TempDir::new().unwrap();
            fs::write(workspace.path().join("a"), b"before").unwrap();
            let mut plan = prepare(
                workspace.path(),
                &args(&["a"]),
                "cli:session-1:run:r1:turn:t1:call:c1",
            )
            .unwrap()
            .unwrap();
            fs::remove_file(workspace.path().join("a")).unwrap();
            let receipt_id = plan.receipt.receipt_id.clone();
            plan.finish();
            prepare_receipt_restore(workspace.path(), "cli:session-1", &receipt_id)
                .unwrap()
                .restore()
                .unwrap();
            assert_eq!(fs::read(workspace.path().join("a")).unwrap(), b"before");
            assert!(
                prepare_receipt_restore(workspace.path(), "cli:session-2", &receipt_id).is_err()
            );
        });
    }
}
