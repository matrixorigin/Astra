//! Bounded, byte-exact workspace capture for foreground Bash output.
//! These are ordinary workspace files, not immutable runtime artifacts.

use std::fs::OpenOptions;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

const MAX_CAPTURE_BYTES: u64 = 50 * 1024 * 1024;
pub(crate) const CAPTURE_DIRECTORY_NAME: &str = "astra-bash-outputs";
const CAPTURE_OK: u8 = 0;
const CAPTURE_LIMIT: u8 = 1;
const CAPTURE_IO: u8 = 2;

#[derive(Clone)]
pub(super) struct CaptureState {
    used: Arc<AtomicU64>,
    failure: Arc<AtomicU8>,
}

impl CaptureState {
    fn new() -> Self {
        Self {
            used: Arc::new(AtomicU64::new(0)),
            failure: Arc::new(AtomicU8::new(CAPTURE_OK)),
        }
    }

    pub(super) fn failed(&self) -> bool {
        self.failure.load(Ordering::Acquire) != CAPTURE_OK
    }

    fn mark_io_failure(&self) {
        let _ = self.failure.compare_exchange(
            CAPTURE_OK,
            CAPTURE_IO,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn reserve(&self, count: usize) -> Result<(), ()> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current
                    .checked_add(count as u64)
                    .filter(|total| *total <= MAX_CAPTURE_BYTES)
            })
            .map(|_| ())
            .map_err(|_| {
                let _ = self.failure.compare_exchange(
                    CAPTURE_OK,
                    CAPTURE_LIMIT,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            })
    }

    fn failure_reason(&self) -> Option<&'static str> {
        match self.failure.load(Ordering::Acquire) {
            CAPTURE_LIMIT => Some("output exceeded the 50 MiB capture limit"),
            CAPTURE_IO => Some("output capture I/O failed"),
            _ => None,
        }
    }
}

pub struct CaptureWriter {
    file: File,
    state: CaptureState,
}

impl CaptureWriter {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), ()> {
        self.state.reserve(bytes.len())?;
        self.file.write_all(bytes).await.map_err(|_| {
            self.state.mark_io_failure();
        })
    }

    pub async fn finish(mut self) -> Result<(), ()> {
        self.file.flush().await.map_err(|_| {
            self.state.mark_io_failure();
        })
    }

    pub fn read_failed(&self) {
        self.state.mark_io_failure();
    }
}

pub struct BashOutputCapture {
    stdout: PathBuf,
    stderr: PathBuf,
    state: CaptureState,
    published: bool,
    inline_limit: usize,
    stdout_reader: std::fs::File,
    stderr_reader: std::fs::File,
    #[cfg(unix)]
    directory_fd: std::fs::File,
}

#[derive(Debug)]
pub struct StreamReceipt {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub struct CaptureReceipt {
    pub stdout: StreamReceipt,
    pub stderr: StreamReceipt,
    pub complete: bool,
    pub failure: Option<&'static str>,
}

impl BashOutputCapture {
    /// The task registry observes these same files and counter from launch;
    /// changing foreground presentation never reopens or reseeds a stream.
    pub fn task_output_files(&self) -> (PathBuf, PathBuf, Arc<AtomicU64>) {
        (
            self.stdout.clone(),
            self.stderr.clone(),
            self.state.used.clone(),
        )
    }

    pub fn start(
        workspace: &Path,
        inline_limit: usize,
    ) -> Result<(Self, CaptureWriter, CaptureWriter), String> {
        let (capture, stdout, stderr) = Self::create(workspace, inline_limit)?;
        let state = capture.state.clone();
        Ok((
            capture,
            CaptureWriter {
                file: File::from_std(stdout),
                state: state.clone(),
            },
            CaptureWriter {
                file: File::from_std(stderr),
                state,
            },
        ))
    }

    /// Recover quota ownership only for private files in Astra's capture
    /// namespace. A saved path alone never authorizes deleting user files.
    pub fn retained_task_bytes(output_root: &Path, stdout: &Path, stderr: &Path) -> Option<u64> {
        let directory = stdout.parent()?;
        if stderr.parent()? != directory || directory.file_name()? != CAPTURE_DIRECTORY_NAME {
            return None;
        }
        let stem = stdout.file_stem()?.to_str()?;
        if stem.len() != 16
            || !stem.bytes().all(|byte| byte.is_ascii_hexdigit())
            || stdout.extension()? != "stdout"
            || stderr.file_stem()? != stem
            || stderr.extension()? != "stderr"
        {
            return None;
        }
        let root = directory.parent()?.canonicalize().ok()?;
        let expected = output_root.canonicalize().ok()?;
        if root != expected {
            return None;
        }
        if !std::fs::symlink_metadata(directory)
            .ok()?
            .file_type()
            .is_dir()
        {
            return None;
        }
        let mut bytes = 0_u64;
        for path in [stdout, stderr] {
            let metadata = std::fs::symlink_metadata(path).ok()?;
            if !metadata.file_type().is_file() {
                return None;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                // SAFETY: geteuid has no preconditions or borrowed resources.
                if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
                    return None;
                }
            }
            bytes = bytes.saturating_add(metadata.len());
        }
        Some(bytes)
    }

    /// Tap a blocking process runner without replacing its process owner.
    pub fn start_blocking(
        workspace: &Path,
        inline_limit: usize,
        preview_limit: usize,
    ) -> Result<(Self, BlockingCaptureWriter, BlockingCaptureWriter), String> {
        let (capture, stdout, stderr) = Self::create(workspace, inline_limit)?;
        let state = capture.state.clone();
        Ok((
            capture,
            BlockingCaptureWriter::new(stdout, state.clone(), preview_limit),
            BlockingCaptureWriter::new(stderr, state, preview_limit),
        ))
    }

    fn create(
        workspace: &Path,
        inline_limit: usize,
    ) -> Result<(Self, std::fs::File, std::fs::File), String> {
        let directory = workspace.join(CAPTURE_DIRECTORY_NAME);
        #[cfg(unix)]
        let directory_fd = open_capture_directory(workspace)?;
        #[cfg(not(unix))]
        {
            ensure_real_directory(workspace)?;
            ensure_real_directory(&directory)?;
        }
        let id = Uuid::new_v4().simple().to_string();
        let id = &id[..16];
        let stdout = directory.join(format!("{id}.stdout"));
        let stderr = directory.join(format!("{id}.stderr"));
        #[cfg(unix)]
        let stdout_file = create_private_file_at(&directory_fd, &stdout)?;
        #[cfg(not(unix))]
        let stdout_file = create_private_file(&stdout)?;
        #[cfg(unix)]
        let stderr_file = create_private_file_at(&directory_fd, &stderr);
        #[cfg(not(unix))]
        let stderr_file = create_private_file(&stderr);
        let stderr_file = match stderr_file {
            Ok(file) => file,
            Err(error) => {
                #[cfg(unix)]
                let _ = unlink_at(&directory_fd, &stdout);
                #[cfg(not(unix))]
                let _ = std::fs::remove_file(&stdout);
                return Err(error);
            }
        };
        let (stdout_reader, stderr_reader) =
            match (stdout_file.try_clone(), stderr_file.try_clone()) {
                (Ok(stdout_reader), Ok(stderr_reader)) => (stdout_reader, stderr_reader),
                _ => {
                    #[cfg(unix)]
                    {
                        let _ = unlink_at(&directory_fd, &stdout);
                        let _ = unlink_at(&directory_fd, &stderr);
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = std::fs::remove_file(&stdout);
                        let _ = std::fs::remove_file(&stderr);
                    }
                    return Err("Error: unable to retain Bash capture handle".into());
                }
            };
        let state = CaptureState::new();
        Ok((
            Self {
                stdout,
                stderr,
                state: state.clone(),
                published: false,
                inline_limit,
                stdout_reader,
                stderr_reader,
                #[cfg(unix)]
                directory_fd,
            },
            stdout_file,
            stderr_file,
        ))
    }

    pub(super) fn state(&self) -> CaptureState {
        self.state.clone()
    }

    pub async fn finish(
        mut self,
        streams_complete: bool,
        command_complete: bool,
        retain_for_preview: bool,
    ) -> Result<Option<CaptureReceipt>, String> {
        let stdout = stream_receipt(
            self.stdout.clone(),
            self.stdout_reader
                .try_clone()
                .map_err(|_| "Error: unable to read Bash capture")?,
        )
        .await?;
        let stderr = stream_receipt(
            self.stderr.clone(),
            self.stderr_reader
                .try_clone()
                .map_err(|_| "Error: unable to read Bash capture")?,
        )
        .await?;
        let failure = self.state.failure_reason();
        let complete = streams_complete && command_complete && failure.is_none();
        if complete
            && !retain_for_preview
            && stdout.bytes.saturating_add(stderr.bytes) <= self.inline_limit as u64
        {
            self.remove_file(&self.stdout)
                .map_err(|_| "Error: unable to remove small Bash capture".to_string())?;
            self.remove_file(&self.stderr)
                .map_err(|_| "Error: unable to remove small Bash capture".to_string())?;
            return Ok(None);
        }
        self.published = true;
        Ok(Some(CaptureReceipt {
            stdout,
            stderr,
            complete,
            failure,
        }))
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            unlink_at(&self.directory_fd, path)
        }
        #[cfg(not(unix))]
        {
            std::fs::remove_file(path)
        }
    }
}

/// A byte-exact tap with a bounded in-memory preview for synchronous runners.
pub struct BlockingCaptureWriter {
    file: std::fs::File,
    state: CaptureState,
    preview: Vec<u8>,
    preview_limit: usize,
    preview_capped: bool,
}

impl BlockingCaptureWriter {
    fn new(file: std::fs::File, state: CaptureState, preview_limit: usize) -> Self {
        Self {
            file,
            state,
            preview: Vec::new(),
            preview_limit,
            preview_capped: false,
        }
    }

    pub fn failed(&self) -> bool {
        self.state.failed()
    }

    pub fn read_failed(&self) {
        self.state.mark_io_failure();
    }

    pub fn into_preview(mut self) -> Vec<u8> {
        // Never expose a credential split at the preview boundary: the
        // redactor cannot recognize a value whose final bytes are absent.
        if self.preview_capped {
            let end = self
                .preview
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |index| index + 1);
            self.preview.truncate(end);
        }
        self.preview
    }
}

impl std::io::Write for BlockingCaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.state
            .reserve(bytes.len())
            .map_err(|_| std::io::Error::other("Bash capture limit reached"))?;
        self.file
            .write_all(bytes)
            .inspect_err(|_| self.state.mark_io_failure())?;
        let keep = bytes
            .len()
            .min(self.preview_limit.saturating_sub(self.preview.len()));
        self.preview.extend_from_slice(&bytes[..keep]);
        self.preview_capped |= keep < bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file
            .flush()
            .inspect_err(|_| self.state.mark_io_failure())
    }
}

impl CaptureReceipt {
    /// The receipt is mandatory recovery data, not disposable preview text.
    /// The configured output limit bounds the preview, never a saved path.
    pub fn present(&self, preview: &str, terminal: &str, output_limit: usize) -> String {
        let mut note = format!(
            "[bash full output: stdout={} ({} bytes, sha256={}); stderr={} ({} bytes, sha256={}); complete={}]",
            self.stdout.path.display(),
            self.stdout.bytes,
            self.stdout.sha256,
            self.stderr.path.display(),
            self.stderr.bytes,
            self.stderr.sha256,
            self.complete,
        );
        if self.complete {
            note.push_str("\n[Copy the saved paths exactly; filter or format these files locally, emitting only the fields or sections needed for the answer. Do not rerun the original command merely to change presentation. Compute reported counts, groups and totals with code, not by estimating rows.]");
        }
        if let Some(failure) = self.failure {
            note.push_str(&format!(
                "\n[bash output capture failed: {failure}; files are incomplete]"
            ));
        }
        if !terminal.is_empty() {
            note.push('\n');
            note.push_str(terminal);
        }
        let target =
            output_limit.min(astra_core::runtime_limits::DEFAULT_TOOL_RESULT_INLINE_CHARS / 2);
        let budget = target.saturating_sub(note.len() + 2);
        let preview = if preview.len() <= budget {
            preview.to_string()
        } else if budget > 12 {
            astra_text_utils::credential_redaction::truncate_redacted_output(
                preview.to_string(),
                budget - 12,
            )
        } else {
            String::new()
        };
        if !preview.is_empty() {
            note.push_str("\n\n");
            note.push_str(&preview);
        }
        note
    }

    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "stdout": {"path":self.stdout.path, "bytes":self.stdout.bytes, "sha256":self.stdout.sha256},
            "stderr": {"path":self.stderr.path, "bytes":self.stderr.bytes, "sha256":self.stderr.sha256},
            "complete":self.complete, "failure":self.failure,
        })
    }

    pub fn result_fields(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut fields = crate::ToolResult::text(String::new())
            .with_source_bounded_model_projection()
            .metadata
            .expect("producer presentation contract");
        fields.insert("bash_output_capture".into(), self.metadata());
        fields
    }
}

impl Drop for BashOutputCapture {
    fn drop(&mut self) {
        if !self.published {
            let _ = self.remove_file(&self.stdout);
            let _ = self.remove_file(&self.stderr);
        }
    }
}

#[cfg(unix)]
fn open_capture_directory(workspace: &Path) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;

    let root = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(workspace)
        .map_err(|_| "Error: Bash workspace is not a real directory")?;
    mkdir_at(&root, CAPTURE_DIRECTORY_NAME)?;
    astra_sandbox::open_directory_beneath(&root, Path::new(CAPTURE_DIRECTORY_NAME))
        .map_err(|_| "Error: Bash capture path is not a real directory".into())
}

#[cfg(unix)]
fn mkdir_at(parent: &std::fs::File, name: &str) -> Result<(), String> {
    use std::os::fd::AsRawFd;

    let name = std::ffi::CString::new(name).expect("static capture directory name");
    let status = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if status != 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists {
        return Err("Error: unable to create Bash capture directory".into());
    }
    Ok(())
}

#[cfg(unix)]
fn capture_file_name(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("missing capture filename"))?;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::other("invalid capture filename"))
}

#[cfg(unix)]
fn create_private_file_at(directory: &std::fs::File, path: &Path) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let name = capture_file_name(path).map_err(|_| "Error: invalid Bash capture filename")?;
    let flags = libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err("Error: unable to create private Bash capture file".into());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn unlink_at(directory: &std::fs::File, path: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let name = capture_file_name(path)?;
    let status = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_real_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
        Ok(_) => Err("Error: Bash capture path is not a real directory".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path)
                .map_err(|_| "Error: unable to create Bash capture directory".to_string())?;
            ensure_real_directory(path)
        }
        Err(_) => Err("Error: unable to inspect Bash capture directory".into()),
    }
}

#[cfg(not(unix))]
fn create_private_file(path: &Path) -> Result<std::fs::File, String> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|_| "Error: unable to create private Bash capture file".into())
}

async fn stream_receipt(path: PathBuf, pinned: std::fs::File) -> Result<StreamReceipt, String> {
    let path_meta =
        std::fs::symlink_metadata(&path).map_err(|_| "Error: Bash capture path changed")?;
    let pinned_meta = pinned
        .metadata()
        .map_err(|_| "Error: unable to read Bash capture")?;
    if !path_meta.is_file() || !pinned_meta.is_file() {
        return Err("Error: Bash capture path changed".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if path_meta.dev() != pinned_meta.dev() || path_meta.ino() != pinned_meta.ino() {
            return Err("Error: Bash capture path changed".into());
        }
    }
    let mut file = File::from_std(pinned);
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|_| "Error: unable to read Bash capture")?;
    let mut hasher = Sha256::new();
    let mut bytes = 0u64;
    // The receipt future is embedded in CLI tool dispatch even for non-Bash
    // branches. Keep the I/O buffer off those deeply composed future frames.
    let mut chunk = vec![0u8; 8192].into_boxed_slice();
    loop {
        let read = file
            .read(&mut chunk)
            .await
            .map_err(|_| "Error: unable to read Bash capture".to_string())?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or("Error: Bash capture size overflow")?;
        if bytes > MAX_CAPTURE_BYTES {
            return Err("Error: Bash capture exceeds 50 MiB".into());
        }
        hasher.update(&chunk[..read]);
    }
    Ok(StreamReceipt {
        path,
        bytes,
        sha256: hex::encode(hasher.finalize()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn byte_exact_capture_and_small_cleanup() {
        let workspace = tempfile::tempdir().unwrap();
        let (capture, mut stdout, stderr) = BashOutputCapture::start(workspace.path(), 2).unwrap();
        stdout.write(&[0, 255, 254, 10]).await.unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        let receipt = capture.finish(true, true, false).await.unwrap().unwrap();
        assert!(receipt.complete);
        assert_eq!(
            receipt.stdout.path.parent(),
            Some(workspace.path().join("astra-bash-outputs").as_path())
        );
        assert_eq!(
            std::fs::read(&receipt.stdout.path).unwrap(),
            [0, 255, 254, 10]
        );
        assert_eq!(receipt.stdout.bytes, 4);
        let stem = receipt.stdout.path.file_stem().unwrap().to_str().unwrap();
        assert_eq!(stem.len(), 16);
        assert!(
            stem.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_eq!(
            receipt.stderr.path.file_stem(),
            receipt.stdout.path.file_stem()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&receipt.stdout.path)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        let (capture, stdout, stderr) = BashOutputCapture::start(workspace.path(), 2).unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        assert!(capture.finish(true, true, false).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn over_limit_is_explicit_and_never_complete() {
        let workspace = tempfile::tempdir().unwrap();
        let (capture, mut stdout, stderr) =
            BashOutputCapture::start(workspace.path(), 1024).unwrap();
        capture
            .state
            .used
            .store(MAX_CAPTURE_BYTES, Ordering::Release);
        assert!(stdout.write(b"x").await.is_err());
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        let receipt = capture.finish(true, true, false).await.unwrap().unwrap();
        assert!(!receipt.complete);
        assert_eq!(
            receipt.failure,
            Some("output exceeded the 50 MiB capture limit")
        );
    }

    #[tokio::test]
    async fn blocking_tap_bounds_preview_without_losing_bytes_or_splitting_credentials() {
        let workspace = tempfile::tempdir().unwrap();
        let (capture, mut stdout, mut stderr) =
            BashOutputCapture::start_blocking(workspace.path(), 64, 16).unwrap();
        let bytes = b"first\nSECRET_KEY=secret-value-that-crosses-the-preview-boundary\n";
        stdout.write_all(bytes).unwrap();
        stderr.write_all(&[0, 255, 10]).unwrap();
        assert_eq!(stdout.into_preview(), b"first\n");
        assert_eq!(stderr.into_preview(), [0, 255, 10]);
        let receipt = capture.finish(true, true, true).await.unwrap().unwrap();
        assert!(receipt.complete);
        assert_eq!(std::fs::read(&receipt.stdout.path).unwrap(), bytes);
        let presented = receipt.present("preview", "[exit code: 7]", 64);
        for stream in [&receipt.stdout, &receipt.stderr] {
            assert!(presented.contains(stream.path.to_str().unwrap()));
            assert!(presented.contains(&stream.sha256));
        }
        assert!(presented.contains("complete=true"));
        assert!(presented.contains("[exit code: 7]"));
        assert!(!presented.contains("preview"));
        assert_eq!(
            crate::model_result_presentation(Some(&receipt.result_fields())),
            crate::ModelResultPresentation::SourceBounded
        );
    }

    #[tokio::test]
    async fn post_execution_receipt_read_failure_is_explicit() {
        let workspace = tempfile::tempdir().unwrap();
        let (capture, stdout, stderr) = BashOutputCapture::start(workspace.path(), 1024).unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        std::fs::remove_file(&capture.stdout).unwrap();
        assert_eq!(
            capture.finish(true, true, false).await.unwrap_err(),
            "Error: Bash capture path changed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn existing_capture_file_or_symlink_is_never_overwritten() {
        let workspace = tempfile::tempdir().unwrap();
        let directory = open_capture_directory(workspace.path()).unwrap();
        let path = workspace
            .path()
            .join(CAPTURE_DIRECTORY_NAME)
            .join("0123456789abcdef.stdout");
        std::fs::write(&path, b"existing output").unwrap();
        assert!(create_private_file_at(&directory, &path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing output");

        let link = path.with_extension("stderr");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(create_private_file_at(&directory, &link).is_err());
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"existing output");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_capture_directory_is_rejected() {
        let workspace = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(target.path(), workspace.path().join("astra-bash-outputs"))
            .unwrap();
        assert!(BashOutputCapture::start(workspace.path(), 1024).is_err());
    }
}
