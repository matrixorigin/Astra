use std::io::{Read as _, Seek as _};
use std::path::{Component, Path, PathBuf};

#[cfg(test)]
use std::io::Write as _;

use astra_server_types::edge_ws_protocol::{
    RuntimeFileTransferAttachment, RuntimeFileTransferContext,
};
use astra_tools::ToolResult;
use futures_util::StreamExt;
use md5::{Digest as _, Md5};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::Sha256;
use tokio::io::AsyncWriteExt as _;
use tokio_util::io::ReaderStream;

pub(crate) async fn execute(
    tool: &str,
    args: &Value,
    context: Option<&RuntimeFileTransferContext>,
    call_id: &str,
) -> Option<ToolResult> {
    match tool {
        "materialize_attachment" => Some(materialize(args, context).await),
        "publish_artifact" => Some(publish(args, context, call_id).await),
        _ => None,
    }
}

async fn materialize(args: &Value, context: Option<&RuntimeFileTransferContext>) -> ToolResult {
    let Some(context) = context else {
        return ToolResult::error("Managed runtime file transfer is unavailable".to_string());
    };
    if let Err(error) = prepare_scope_dirs(context).await {
        return ToolResult::error(error);
    }
    let Some(file_id) = args.get("file_id").and_then(Value::as_str) else {
        return ToolResult::error("materialize_attachment requires file_id".to_string());
    };
    let Some(attachment) = context
        .attachments
        .iter()
        .find(|item| item.file_id == file_id)
    else {
        return ToolResult::error(
            "file_id is not in the current-turn attachment inventory".to_string(),
        );
    };
    let Some(filename) = materialized_filename(context, attachment) else {
        return ToolResult::error("attachment filename is invalid".to_string());
    };
    if attachment.size < 0 || attachment.size as u64 > context.max_file_bytes {
        return ToolResult::error("attachment exceeds the runtime transfer limit".to_string());
    }
    let destination = Path::new(&context.catalog_dir).join(&filename);
    let catalog_root = PathBuf::from(&context.catalog_dir);
    let trusted_root = trusted_sandbox_root(context);
    let cached_filename = filename.clone();
    let max_file_bytes = context.max_file_bytes;
    if let Ok(Ok(existing)) = tokio::task::spawn_blocking(move || {
        read_scoped_artifact(
            &catalog_root.join(cached_filename),
            &[catalog_root],
            &trusted_root,
            max_file_bytes,
        )
        .map(|artifact| (artifact.size, artifact.md5))
    })
    .await
        && existing.0 == attachment.size
        && existing.1 == attachment.md5
    {
        return materialized_result(attachment, &destination);
    }
    let mut url = match reqwest::Url::parse(&context.endpoint_url) {
        Ok(url) => url,
        Err(error) => {
            return ToolResult::error(format!("runtime file endpoint is invalid: {error}"));
        }
    };
    match url.path_segments_mut() {
        Ok(mut segments) => {
            segments.pop_if_empty().push(file_id);
        }
        Err(_) => {
            return ToolResult::error(
                "runtime file endpoint cannot accept a file path".to_string(),
            );
        }
    }
    let response = match reqwest::Client::new()
        .get(url)
        .header(reqwest::header::AUTHORIZATION, &context.authorization)
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ToolResult::error(format!("attachment download failed: {error}")),
    };
    if !response.status().is_success() {
        return ToolResult::error(format!(
            "attachment download failed with HTTP {}",
            response.status()
        ));
    }
    if response
        .content_length()
        .is_some_and(|size| size > context.max_file_bytes)
    {
        return ToolResult::error(
            "attachment download exceeds the runtime transfer limit".to_string(),
        );
    }
    let catalog_root = PathBuf::from(&context.catalog_dir);
    let trusted_root = trusted_sandbox_root(context);
    if let Err(error) = stream_attachment_beneath(
        response,
        &trusted_root,
        &catalog_root,
        &filename,
        context.max_file_bytes,
        attachment.size,
        &attachment.md5,
    )
    .await
    {
        return ToolResult::error(error);
    }
    materialized_result(attachment, &destination)
}

fn materialized_result(
    attachment: &RuntimeFileTransferAttachment,
    destination: &Path,
) -> ToolResult {
    ToolResult {
        output: format!("Materialized attachment at {}", destination.display()),
        metadata: Some(Map::from_iter([
            (
                "file_id".to_string(),
                Value::String(attachment.file_id.clone()),
            ),
            (
                "path".to_string(),
                Value::String(destination.display().to_string()),
            ),
            ("size".to_string(), json!(attachment.size)),
            ("md5".to_string(), Value::String(attachment.md5.clone())),
        ])),
        is_error: false,
        exit_semantics: None,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishArgs {
    path: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    artifact_kind: Option<String>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct UploadResponse {
    file_id: String,
    filename: String,
    size: i64,
    md5: String,
    sha256: String,
    content_type: String,
    download_url: String,
}

async fn publish(
    args: &Value,
    context: Option<&RuntimeFileTransferContext>,
    call_id: &str,
) -> ToolResult {
    let Some(context) = context else {
        return ToolResult::error("Managed runtime file transfer is unavailable".to_string());
    };
    if let Err(error) = prepare_scope_dirs(context).await {
        return ToolResult::error(error);
    }
    let args: PublishArgs = match serde_json::from_value(args.clone()) {
        Ok(args) => args,
        Err(error) => {
            return ToolResult::error(format!("publish_artifact arguments are invalid: {error}"));
        }
    };
    let requested = PathBuf::from(&args.path);
    let requested = if requested.is_absolute() {
        requested
    } else {
        Path::new(&context.session_dir).join(requested)
    };
    let allowed_roots = [
        PathBuf::from(&context.catalog_dir),
        PathBuf::from(&context.session_dir),
        PathBuf::from(&context.scratch_dir),
    ];
    let max_file_bytes = context.max_file_bytes;
    let trusted_root = trusted_sandbox_root(context);
    let opened = match tokio::task::spawn_blocking(move || {
        read_scoped_artifact(&requested, &allowed_roots, &trusted_root, max_file_bytes)
    })
    .await
    {
        Ok(Ok(opened)) => opened,
        Ok(Err(error)) => return ToolResult::error(error),
        Err(error) => return ToolResult::error(format!("artifact read task failed: {error}")),
    };
    let digest = opened.sha256;
    let content_md5 = opened.md5;
    let content_size = opened.size;
    let filename = opened.filename;
    let upload = ReaderStream::new(tokio::fs::File::from_std(opened.file));
    let mut url = match reqwest::Url::parse(&context.endpoint_url) {
        Ok(url) => url,
        Err(error) => {
            return ToolResult::error(format!("runtime file endpoint is invalid: {error}"));
        }
    };
    url.query_pairs_mut()
        .append_pair("call_id", call_id)
        .append_pair("filename", &filename);
    let response = match reqwest::Client::new()
        .post(url)
        .header(reqwest::header::AUTHORIZATION, &context.authorization)
        .header("X-MOI-Content-SHA256", &digest)
        .header(reqwest::header::CONTENT_LENGTH, content_size)
        .header(
            reqwest::header::CONTENT_TYPE,
            args.content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        )
        .body(reqwest::Body::wrap_stream(upload))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return ToolResult::error(format!("artifact upload failed: {error}")),
    };
    if !response.status().is_success() {
        return ToolResult::error(format!(
            "artifact upload failed with HTTP {}",
            response.status()
        ));
    }
    let uploaded: UploadResponse = match bounded_json_response(response, 64 * 1024).await {
        Ok(uploaded) => uploaded,
        Err(error) => return ToolResult::error(error),
    };
    if uploaded.sha256 != digest || uploaded.md5 != content_md5 || uploaded.size != content_size {
        return ToolResult::error(
            "artifact upload response failed size or digest verification".to_string(),
        );
    }
    let artifact_id = uploaded.file_id.clone();
    let artifact_ref = format!("moi-file://{}", uploaded.file_id);
    let artifact = json!({
        "artifact_id": artifact_id,
        "name": uploaded.filename,
        "type": args.artifact_kind.as_deref().unwrap_or("file"),
        "description": args.description.as_deref().unwrap_or("Managed runtime artifact"),
        "parts": [{
            "kind": if uploaded.content_type.starts_with("image/") { "image" } else { "file" },
            "file": {"uri": uploaded.download_url, "mimeType": uploaded.content_type, "name": uploaded.filename},
            "metadata": {"file_id": uploaded.file_id, "byte_size": uploaded.size}
        }],
        "data": {"file_id": uploaded.file_id, "name": uploaded.filename, "mime_type": uploaded.content_type, "byte_size": uploaded.size, "download_url": uploaded.download_url},
        "metadata": {"source": "managed_edge", "tool_id": "publish_artifact", "file_id": uploaded.file_id, "sha256": uploaded.sha256}
    });
    let mut result_metadata = Map::from_iter([
        (
            "artifact_id".to_string(),
            Value::String(uploaded.file_id.clone()),
        ),
        ("artifact_ref".to_string(), Value::String(artifact_ref)),
        ("file_id".to_string(), Value::String(uploaded.file_id)),
        (
            "filename".to_string(),
            Value::String(uploaded.filename.clone()),
        ),
        ("byte_size".to_string(), json!(uploaded.size)),
        ("md5".to_string(), Value::String(uploaded.md5)),
        ("sha256".to_string(), Value::String(uploaded.sha256)),
        (
            "content_type".to_string(),
            Value::String(uploaded.content_type),
        ),
        (
            "download_url".to_string(),
            Value::String(uploaded.download_url),
        ),
        (
            "title".to_string(),
            args.title.map(Value::String).unwrap_or(Value::Null),
        ),
        (
            "artifact_kind".to_string(),
            args.artifact_kind
                .map(Value::String)
                .unwrap_or_else(|| Value::String("file".to_string())),
        ),
        (
            "description".to_string(),
            args.description.map(Value::String).unwrap_or(Value::Null),
        ),
    ]);
    result_metadata.insert("artifact".to_string(), artifact);
    ToolResult {
        output: format!("Published artifact '{}'", uploaded.filename),
        metadata: Some(result_metadata),
        is_error: false,
        exit_semantics: None,
    }
}

async fn bounded_json_response<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<T, String> {
    if response
        .content_length()
        .is_some_and(|size| size > max_bytes as u64)
    {
        return Err("artifact upload response exceeds the runtime transfer limit".to_string());
    }
    let mut content = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("artifact upload response failed: {error}"))?;
        if content.len().saturating_add(chunk.len()) > max_bytes {
            return Err("artifact upload response exceeds the runtime transfer limit".to_string());
        }
        content.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&content)
        .map_err(|error| format!("artifact upload response is invalid: {error}"))
}

#[cfg(not(test))]
fn trusted_sandbox_root(_context: &RuntimeFileTransferContext) -> PathBuf {
    PathBuf::from("/sandbox")
}

// Unit tests use isolated temporary directories while production always
// anchors path resolution at the immutable `/sandbox` mount.
#[cfg(test)]
fn trusted_sandbox_root(context: &RuntimeFileTransferContext) -> PathBuf {
    let sandbox = Path::new("/sandbox");
    if Path::new(&context.root).starts_with(sandbox)
        && Path::new(&context.catalog_dir).starts_with(sandbox)
        && Path::new(&context.session_dir).starts_with(sandbox)
        && Path::new(&context.scratch_dir).starts_with(sandbox)
    {
        sandbox.to_path_buf()
    } else {
        PathBuf::from(&context.root)
    }
}

fn path_beneath_trusted_root(trusted_root: &Path, path: &Path) -> Result<PathBuf, String> {
    let relative = path
        .strip_prefix(trusted_root)
        .map_err(|_| "runtime path is outside the trusted sandbox root".to_string())?;
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("runtime path escapes the trusted sandbox root".to_string());
    }
    Ok(relative.to_path_buf())
}

#[cfg(unix)]
fn open_trusted_root(root: &Path) -> Result<std::os::fd::OwnedFd, String> {
    use nix::fcntl::{OFlag, open};
    use nix::sys::stat::Mode;

    open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| format!("trusted sandbox root is unavailable: {error}"))
}

#[cfg(target_os = "linux")]
fn open_directory_beneath<Fd: std::os::fd::AsFd>(
    trusted_fd: Fd,
    relative: &Path,
) -> nix::Result<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};

    let relative = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    openat2(
        trusted_fd,
        relative,
        OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS),
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_directory_beneath<Fd: std::os::fd::AsFd>(
    trusted_fd: Fd,
    relative: &Path,
) -> nix::Result<std::os::fd::OwnedFd> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;

    let mut current = openat(
        trusted_fd,
        Path::new("."),
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(nix::errno::Errno::EINVAL);
        };
        current = openat(
            &current,
            Path::new(name),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )?;
    }
    Ok(current)
}

#[cfg(unix)]
fn create_dir_all_beneath(trusted_root: &Path, path: &Path) -> Result<(), String> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::{Mode, mkdirat};

    let relative = path_beneath_trusted_root(trusted_root, path)?;
    let mut current = open_trusted_root(trusted_root)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err("runtime path escapes the trusted sandbox root".to_string());
        };
        match mkdirat(&current, Path::new(name), Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(error) => return Err(format!("failed to prepare runtime scope: {error}")),
        }
        current = openat(
            &current,
            Path::new(name),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| format!("runtime scope is unavailable or unsafe: {error}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_dir_all_beneath(_trusted_root: &Path, _path: &Path) -> Result<(), String> {
    Err("secure runtime scope preparation is unsupported on this platform".to_string())
}

#[cfg(unix)]
struct AtomicStagedFile {
    root_fd: std::os::fd::OwnedFd,
    temporary: String,
    destination: String,
    file: Option<std::fs::File>,
    committed: bool,
}

#[cfg(unix)]
impl AtomicStagedFile {
    fn take_file(&mut self) -> Result<std::fs::File, String> {
        self.file
            .take()
            .ok_or_else(|| "attachment staging file is unavailable".to_string())
    }

    fn restore_file(&mut self, file: std::fs::File) {
        self.file = Some(file);
    }

    fn commit(mut self) -> Result<(), String> {
        use nix::fcntl::renameat;

        let staged = self
            .file
            .take()
            .ok_or_else(|| "attachment staging file is unavailable".to_string())?;
        staged
            .sync_all()
            .map_err(|error| format!("attachment staging sync failed: {error}"))?;
        drop(staged);
        renameat(
            &self.root_fd,
            self.temporary.as_str(),
            &self.root_fd,
            self.destination.as_str(),
        )
        .map_err(|error| format!("attachment rename failed: {error}"))?;
        self.committed = true;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for AtomicStagedFile {
    fn drop(&mut self) {
        if !self.committed {
            use nix::unistd::{UnlinkatFlags, unlinkat};
            let _ = unlinkat(
                &self.root_fd,
                self.temporary.as_str(),
                UnlinkatFlags::NoRemoveDir,
            );
        }
    }
}

#[cfg(unix)]
fn begin_atomic_write_beneath(
    trusted_root: &Path,
    root: &Path,
    filename: &str,
) -> Result<AtomicStagedFile, String> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;

    let relative_root = path_beneath_trusted_root(trusted_root, root)?;
    let trusted_fd = open_trusted_root(trusted_root)?;
    let root_fd = open_directory_beneath(&trusted_fd, &relative_root)
        .map_err(|error| format!("runtime catalog is unavailable: {error}"))?;
    let temporary = format!(".moi-transfer-{:016x}", fastrand::u64(..));
    let staged_fd = openat(
        &root_fd,
        temporary.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|error| format!("attachment staging open failed: {error}"))?;
    Ok(AtomicStagedFile {
        root_fd,
        temporary,
        destination: filename.to_string(),
        file: Some(std::fs::File::from(staged_fd)),
        committed: false,
    })
}

#[cfg(not(unix))]
fn begin_atomic_write_beneath(
    _trusted_root: &Path,
    _root: &Path,
    _filename: &str,
) -> Result<(), String> {
    Err("secure attachment materialization is unsupported on this platform".to_string())
}

#[cfg(unix)]
async fn stream_attachment_beneath(
    response: reqwest::Response,
    trusted_root: &Path,
    catalog_root: &Path,
    filename: &str,
    max_file_bytes: u64,
    expected_size: i64,
    expected_md5: &str,
) -> Result<(), String> {
    let trusted_root = trusted_root.to_path_buf();
    let catalog_root = catalog_root.to_path_buf();
    let filename = filename.to_string();
    let mut staged = tokio::task::spawn_blocking(move || {
        begin_atomic_write_beneath(&trusted_root, &catalog_root, &filename)
    })
    .await
    .map_err(|error| format!("attachment staging task failed: {error}"))??;
    let mut file = tokio::fs::File::from_std(staged.take_file()?);
    let mut received = 0_u64;
    let mut md5 = Md5::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("attachment download body failed: {error}"))?;
        received = received
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| "attachment download exceeds the runtime transfer limit".to_string())?;
        if received > max_file_bytes {
            return Err("attachment download exceeds the runtime transfer limit".to_string());
        }
        md5.update(&chunk);
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("attachment staging write failed: {error}"))?;
    }
    file.flush()
        .await
        .map_err(|error| format!("attachment staging write failed: {error}"))?;
    staged.restore_file(file.into_std().await);
    if received as i64 != expected_size || format!("{:x}", md5.finalize()) != expected_md5 {
        return Err("attachment content failed size or digest verification".to_string());
    }
    tokio::task::spawn_blocking(move || staged.commit())
        .await
        .map_err(|error| format!("attachment staging task failed: {error}"))?
        .map_err(|error| format!("attachment atomic publish failed: {error}"))
}

#[cfg(not(unix))]
async fn stream_attachment_beneath(
    _response: reqwest::Response,
    _trusted_root: &Path,
    _catalog_root: &Path,
    _filename: &str,
    _max_file_bytes: u64,
    _expected_size: i64,
    _expected_md5: &str,
) -> Result<(), String> {
    Err("secure attachment materialization is unsupported on this platform".to_string())
}

#[cfg(test)]
fn atomic_write_beneath(
    trusted_root: &Path,
    root: &Path,
    filename: &str,
    content: &[u8],
) -> Result<(), String> {
    #[cfg(unix)]
    {
        let mut staged = begin_atomic_write_beneath(trusted_root, root, filename)?;
        staged
            .file
            .as_mut()
            .ok_or_else(|| "attachment staging file is unavailable".to_string())?
            .write_all(content)
            .map_err(|error| format!("attachment staging write failed: {error}"))?;
        staged.commit()
    }
    #[cfg(not(unix))]
    {
        let _ = (trusted_root, root, filename, content);
        Err("secure attachment materialization is unsupported on this platform".to_string())
    }
}

fn safe_filename(name: &str) -> Option<&str> {
    let trimmed = name.trim();
    (!trimmed.is_empty()
        // Duplicate names receive a 13-byte hash prefix; keep the final path
        // component below the common 255-byte filesystem limit.
        && name.len() <= 240
        && !name.contains('\0')
        && trimmed == name
        && Path::new(name).file_name().and_then(|part| part.to_str()) == Some(name))
    .then_some(name)
}

fn materialized_filename(
    context: &RuntimeFileTransferContext,
    attachment: &RuntimeFileTransferAttachment,
) -> Option<String> {
    let filename = safe_filename(&attachment.name)?;
    let duplicate = context
        .attachments
        .iter()
        .filter(|candidate| candidate.name == attachment.name)
        .take(2)
        .count()
        > 1;
    if !duplicate {
        return Some(filename.to_string());
    }
    let id_hash = format!("{:x}", Sha256::digest(attachment.file_id.as_bytes()));
    Some(format!("{}-{filename}", &id_hash[..12]))
}

#[cfg(test)]
fn md5_hex(content: &[u8]) -> String {
    format!("{:x}", Md5::digest(content))
}

struct ScopedArtifact {
    filename: String,
    file: std::fs::File,
    size: i64,
    md5: String,
    sha256: String,
}

fn read_scoped_artifact(
    requested: &Path,
    allowed_roots: &[PathBuf],
    trusted_root: &Path,
    max_file_bytes: u64,
) -> Result<ScopedArtifact, String> {
    if max_file_bytes == 0 {
        return Err("runtime transfer limit is invalid".to_string());
    }
    let filename = requested
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(safe_filename)
        .ok_or_else(|| "artifact filename is invalid".to_string())?
        .to_string();
    let (root, relative) = allowed_roots
        .iter()
        .find_map(|root| {
            requested
                .strip_prefix(root)
                .ok()
                .map(|relative| (root, relative))
        })
        .ok_or_else(|| "artifact path escapes the current runtime scope".to_string())?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("artifact path escapes the current runtime scope".to_string());
    }
    let mut file = open_beneath_without_symlinks(trusted_root, root, relative)?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("artifact metadata failed: {error}"))?;
    if !metadata.is_file() {
        return Err("artifact path must be a regular file".to_string());
    }
    if metadata.len() == 0 || metadata.len() > max_file_bytes {
        return Err(format!(
            "artifact must be between 1 and {max_file_bytes} bytes"
        ));
    }
    let mut md5 = Md5::new();
    let mut sha256 = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("artifact read failed: {error}"))?;
        if read == 0 {
            break;
        }
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| "artifact changed size while it was being read".to_string())?;
        if size > max_file_bytes {
            return Err("artifact changed size while it was being read".to_string());
        }
        md5.update(&buffer[..read]);
        sha256.update(&buffer[..read]);
    }
    if size == 0 || size != metadata.len() {
        return Err("artifact changed size while it was being read".to_string());
    }
    file.seek(std::io::SeekFrom::Start(0))
        .map_err(|error| format!("artifact rewind failed: {error}"))?;
    Ok(ScopedArtifact {
        filename,
        file,
        size: size as i64,
        md5: format!("{:x}", md5.finalize()),
        sha256: format!("sha256:{:x}", sha256.finalize()),
    })
}

#[cfg(target_os = "linux")]
fn open_beneath_without_symlinks(
    trusted_root: &Path,
    root: &Path,
    relative: &Path,
) -> Result<std::fs::File, String> {
    use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};

    let trusted_fd = open_trusted_root(trusted_root)?;
    let root_relative = path_beneath_trusted_root(trusted_root, root)?;
    let full_relative = root_relative.join(relative);
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS);
    let fd = openat2(&trusted_fd, &full_relative, how)
        .map_err(|error| format!("artifact path is unavailable or unsafe: {error}"))?;
    Ok(std::fs::File::from(fd))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_beneath_without_symlinks(
    trusted_root: &Path,
    root: &Path,
    relative: &Path,
) -> Result<std::fs::File, String> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;

    let mut current = open_trusted_root(trusted_root)?;
    let root_relative = path_beneath_trusted_root(trusted_root, root)?;
    let full_relative = root_relative.join(relative);
    let components: Vec<_> = full_relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err("artifact path escapes the current runtime scope".to_string());
        };
        let final_component = index + 1 == components.len();
        let flags = if final_component {
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW
        } else {
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW
        };
        current = openat(&current, Path::new(name), flags, Mode::empty())
            .map_err(|error| format!("artifact path is unavailable or unsafe: {error}"))?;
    }
    Ok(std::fs::File::from(current))
}

#[cfg(not(unix))]
fn open_beneath_without_symlinks(
    _trusted_root: &Path,
    _root: &Path,
    _relative: &Path,
) -> Result<std::fs::File, String> {
    Err("secure artifact publication is unsupported on this platform".to_string())
}

async fn prepare_scope_dirs(context: &RuntimeFileTransferContext) -> Result<(), String> {
    let trusted_root = trusted_sandbox_root(context);
    for path in [
        &context.catalog_dir,
        &context.session_dir,
        &context.scratch_dir,
    ] {
        let trusted_root = trusted_root.clone();
        let path = PathBuf::from(path);
        tokio::task::spawn_blocking(move || create_dir_all_beneath(&trusted_root, &path))
            .await
            .map_err(|error| format!("runtime scope preparation task failed: {error}"))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn context(root: &Path) -> RuntimeFileTransferContext {
        RuntimeFileTransferContext {
            endpoint_url: "http://127.0.0.1:1/api/v1/runtime-files".to_string(),
            authorization: "Bearer secret-that-must-be-redacted".to_string(),
            task_id: "task-1".to_string(),
            root: root.display().to_string(),
            catalog_dir: root.join("catalog").display().to_string(),
            session_dir: root.join("session").display().to_string(),
            scratch_dir: root.join("scratch").display().to_string(),
            max_file_bytes: 1024,
            attachments: vec![RuntimeFileTransferAttachment {
                file_id: "file-1".to_string(),
                name: "input.txt".to_string(),
                size: 5,
                md5: md5_hex(b"hello"),
            }],
        }
    }

    #[tokio::test]
    async fn materialize_rejects_file_outside_current_attachment_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let result = materialize(&json!({"file_id": "other"}), Some(&context(temp.path()))).await;
        assert!(result.is_error);
        assert!(result.output.contains("current-turn attachment inventory"));
    }

    #[tokio::test]
    async fn materialize_downloads_and_verifies_authorized_attachment() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/runtime-files/file-1"))
            .and(header(
                "authorization",
                "Bearer secret-that-must-be-redacted",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello"))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let mut transfer = context(temp.path());
        transfer.endpoint_url = format!("{}/api/v1/runtime-files", server.uri());

        let result = execute(
            "materialize_attachment",
            &json!({"file_id": "file-1"}),
            Some(&transfer),
            "call-1",
        )
        .await
        .expect("file-transfer tool must be dispatched");

        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            std::fs::read(temp.path().join("catalog/input.txt")).unwrap(),
            b"hello"
        );
    }

    #[tokio::test]
    async fn materialize_rejects_digest_mismatch_and_oversized_streams() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/runtime-files/file-1"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"world"))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let mut transfer = context(temp.path());
        transfer.endpoint_url = format!("{}/api/v1/runtime-files", server.uri());

        let digest_result = materialize(&json!({"file_id": "file-1"}), Some(&transfer)).await;
        assert!(digest_result.is_error);
        assert!(digest_result.output.contains("digest verification"));

        transfer.max_file_bytes = 4;
        let oversized = materialize(&json!({"file_id": "file-1"}), Some(&transfer)).await;
        assert!(oversized.is_error);
        assert!(oversized.output.contains("transfer limit"));
    }

    #[tokio::test]
    async fn publish_uploads_relative_session_artifact_with_bounded_response() {
        let server = MockServer::start().await;
        let content = b"report";
        let sha256 = format!("sha256:{:x}", Sha256::digest(content));
        let md5 = md5_hex(content);
        Mock::given(method("POST"))
            .and(path("/api/v1/runtime-files"))
            .and(query_param("call_id", "call-1"))
            .and(query_param("filename", "report.txt"))
            .and(header(
                "authorization",
                "Bearer secret-that-must-be-redacted",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "file_id": "out-1",
                "filename": "report.txt",
                "size": content.len(),
                "md5": md5,
                "sha256": sha256,
                "content_type": "text/plain",
                "download_url": "https://example.invalid/out-1"
            })))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let mut transfer = context(temp.path());
        transfer.endpoint_url = format!("{}/api/v1/runtime-files", server.uri());
        prepare_scope_dirs(&transfer).await.unwrap();
        std::fs::write(temp.path().join("session/report.txt"), content).unwrap();

        let result = publish(
            &json!({"path": "report.txt", "content_type": "text/plain"}),
            Some(&transfer),
            "call-1",
        )
        .await;

        assert!(!result.is_error, "{result:?}");
        assert_eq!(
            result.metadata.as_ref().and_then(|m| m.get("file_id")),
            Some(&Value::String("out-1".to_string()))
        );
    }

    #[tokio::test]
    async fn publish_rejects_oversized_upload_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 65 * 1024]))
            .mount(&server)
            .await;
        let temp = tempfile::tempdir().unwrap();
        let mut transfer = context(temp.path());
        transfer.endpoint_url = format!("{}/api/v1/runtime-files", server.uri());
        prepare_scope_dirs(&transfer).await.unwrap();
        std::fs::write(temp.path().join("session/report.txt"), b"report").unwrap();

        let result = publish(&json!({"path": "report.txt"}), Some(&transfer), "call-1").await;

        assert!(result.is_error);
        assert!(result.output.contains("response exceeds"));
    }

    #[tokio::test]
    async fn publish_rejects_symlink_that_escapes_runtime_scope() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        prepare_scope_dirs(&context(temp.path())).await.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), temp.path().join("catalog/escape.txt")).unwrap();
        let result = publish(
            &json!({"path": temp.path().join("catalog/escape.txt")}),
            Some(&context(temp.path())),
            "call-1",
        )
        .await;
        assert!(result.is_error);
        assert!(result.output.contains("unavailable or unsafe"));
    }

    #[cfg(unix)]
    #[test]
    fn opened_artifact_descriptor_is_stable_when_path_is_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        let artifact = catalog.join("result.txt");
        std::fs::write(&artifact, b"trusted").unwrap();

        let mut file =
            open_beneath_without_symlinks(temp.path(), &catalog, Path::new("result.txt")).unwrap();
        std::fs::remove_file(&artifact).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), &artifact).unwrap();

        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();
        assert_eq!(content, "trusted");
    }

    #[cfg(unix)]
    #[test]
    fn materialization_rejects_symlinked_ancestor_beneath_trusted_root() {
        let trusted = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let runtime = trusted.path().join(".moi/runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        std::os::unix::fs::symlink(outside.path(), runtime.join("task-1")).unwrap();
        let catalog = runtime.join("task-1/catalog");

        let prepare = create_dir_all_beneath(trusted.path(), &catalog);
        assert!(
            prepare.is_err(),
            "symlinked task directory must be rejected"
        );
        let publish = atomic_write_beneath(
            trusted.path(),
            &catalog,
            "input.txt",
            b"untrusted destination",
        );
        assert!(
            publish.is_err(),
            "symlinked catalog ancestor must be rejected"
        );
        assert!(!outside.path().join("catalog/input.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn artifact_read_rejects_symlinked_scope_ancestor_beneath_trusted_root() {
        let trusted = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("report.txt"), b"outside").unwrap();
        let sessions = trusted.path().join(".moi/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        std::os::unix::fs::symlink(outside.path(), sessions.join("session-1")).unwrap();
        let session = sessions.join("session-1");

        let result =
            open_beneath_without_symlinks(trusted.path(), &session, Path::new("report.txt"));
        assert!(
            result.is_err(),
            "symlinked session ancestor must be rejected"
        );
    }

    #[test]
    fn transfer_context_debug_redacts_authorization() {
        let temp = tempfile::tempdir().unwrap();
        let rendered = format!("{:?}", context(temp.path()));
        assert!(rendered.contains("authorization_present"));
        assert!(!rendered.contains("secret-that-must-be-redacted"));
    }

    #[test]
    fn duplicate_attachment_names_receive_stable_distinct_paths() {
        let temp = tempfile::tempdir().unwrap();
        let mut transfer = context(temp.path());
        transfer.attachments.push(RuntimeFileTransferAttachment {
            file_id: "file-2".to_string(),
            name: "input.txt".to_string(),
            size: 5,
            md5: md5_hex(b"world"),
        });
        let first = materialized_filename(&transfer, &transfer.attachments[0]).unwrap();
        let second = materialized_filename(&transfer, &transfer.attachments[1]).unwrap();
        assert_ne!(first, second);
        assert!(first.ends_with("-input.txt"));
        assert!(second.ends_with("-input.txt"));
    }
}
