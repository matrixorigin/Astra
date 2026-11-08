//! Bounded private capture for shell task output.

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const MAX_CAPTURE_BYTES: u64 = 50 * 1024 * 1024;
const LOCAL_TAIL_BYTES: usize = 4096;
pub(crate) const CAPTURE_DIRECTORY_NAME: &str = "astra-bash-outputs";
const CAPTURE_OK: u8 = 0;
const CAPTURE_LIMIT: u8 = 1;
const CAPTURE_IO: u8 = 2;

#[derive(Clone)]
pub(super) struct CaptureState {
    used: Arc<AtomicU64>,
    failure: Arc<AtomicU8>,
    activity: tokio::sync::watch::Sender<u64>,
    live: Arc<OnceLock<LiveCaptures>>,
}

#[derive(Default)]
struct LiveCaptures {
    stdout: Mutex<LiveCapture>,
    stderr: Mutex<LiveCapture>,
}

#[derive(Default)]
struct LiveCapture {
    redacted: astra_text_utils::live_redaction::LiveRedactor,
    // Raw output is only read by the explicitly local task view.
    local_tail: VecDeque<u8>,
}

impl LiveCaptures {
    fn stream(&self, stream: CaptureStream) -> &Mutex<LiveCapture> {
        match stream {
            CaptureStream::Stdout => &self.stdout,
            CaptureStream::Stderr => &self.stderr,
        }
    }
}

impl CaptureState {
    fn new() -> Self {
        let (activity, _) = tokio::sync::watch::channel(0);
        Self {
            used: Arc::new(AtomicU64::new(0)),
            failure: Arc::new(AtomicU8::new(CAPTURE_OK)),
            activity,
            live: Arc::new(OnceLock::new()),
        }
    }

    fn mark_io_failure(&self) {
        self.failure.store(CAPTURE_IO, Ordering::Release);
        self.activity
            .send_modify(|revision| *revision = revision.saturating_add(1));
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

    fn observe(&self, stream: CaptureStream, bytes: &[u8]) {
        if let Some(live) = self.live.get() {
            match live.stream(stream).lock() {
                Ok(mut output) => {
                    output.redacted.append(bytes);
                    if bytes.len() >= LOCAL_TAIL_BYTES {
                        output.local_tail.clear();
                        output
                            .local_tail
                            .extend(&bytes[bytes.len() - LOCAL_TAIL_BYTES..]);
                    } else {
                        let remove = output
                            .local_tail
                            .len()
                            .saturating_add(bytes.len())
                            .saturating_sub(LOCAL_TAIL_BYTES);
                        output.local_tail.drain(..remove);
                        output.local_tail.extend(bytes);
                    }
                }
                Err(error) => error.into_inner().redacted.withhold(),
            }
        }
    }

    fn observe_activity(&self, stream: CaptureStream, bytes: &[u8]) {
        self.observe(stream, bytes);
        self.activity
            .send_modify(|revision| *revision = revision.saturating_add(bytes.len() as u64));
    }
}

pub struct CaptureWriter {
    file: File,
    state: CaptureState,
    stream: CaptureStream,
}

impl CaptureWriter {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), ()> {
        if bytes.is_empty() {
            return Ok(());
        }
        // The capture budget limits observation, not the command itself.
        // Once full, continue draining its pipes without storing more bytes.
        match self.state.failure.load(Ordering::Acquire) {
            CAPTURE_LIMIT => {
                self.state.observe_activity(self.stream, bytes);
                return Ok(());
            }
            CAPTURE_IO => {
                self.state.observe_activity(self.stream, bytes);
                return Err(());
            }
            _ => {}
        }
        if self.state.reserve(bytes.len()).is_err() {
            self.state.observe_activity(self.stream, bytes);
            return if self.state.failure.load(Ordering::Acquire) == CAPTURE_LIMIT {
                Ok(())
            } else {
                Err(())
            };
        }
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let written = match self.file.write(remaining).await {
                Ok(written) => written,
                Err(_) => {
                    self.state.mark_io_failure();
                    self.state.observe_activity(self.stream, remaining);
                    return Err(());
                }
            };
            if written == 0 {
                self.state.mark_io_failure();
                self.state.observe_activity(self.stream, remaining);
                return Err(());
            }
            if self.state.activity.receiver_count() > 0 {
                // A live observation must not lead the readable file or its
                // activity signal, even if a later partial write fails.
                if self.file.flush().await.is_err() {
                    self.state.mark_io_failure();
                    self.state.observe_activity(self.stream, remaining);
                    return Err(());
                }
                self.state
                    .observe_activity(self.stream, &remaining[..written]);
            }
            remaining = &remaining[written..];
        }
        Ok(())
    }

    pub async fn finish(mut self) -> Result<(), ()> {
        self.file.flush().await.map_err(|_| {
            self.state.mark_io_failure();
        })
    }
}

pub struct BashOutputCapture {
    stdout: PathBuf,
    stderr: PathBuf,
    state: CaptureState,
    retained: bool,
    stdout_reader: Arc<std::fs::File>,
    stderr_reader: Arc<std::fs::File>,
    task_capture: Option<Arc<TaskCapture>>,
    #[cfg(unix)]
    directory_fd: std::fs::File,
}

/// Deletion authority for a live private task capture. Restored paths do not
/// obtain this owner, and workspace receipts never share it.
pub struct TaskCapture {
    activity: tokio::sync::watch::Receiver<u64>,
    failure: Arc<AtomicU8>,
    stored_bytes: Arc<AtomicU64>,
    removed: Mutex<bool>,
    live: Arc<OnceLock<LiveCaptures>>,
    readers: [Arc<std::fs::File>; 2],
    #[cfg(unix)]
    directory: Arc<TaskCaptureDirectory>,
    #[cfg(unix)]
    files: [(PathBuf, (u64, u64)); 2],
}

/// A read-only view of live output. It does not keep the task's file cleanup
/// authority alive while an automatic UI preview is queued.
#[derive(Clone)]
pub struct TaskLiveView {
    live: Arc<OnceLock<LiveCaptures>>,
    activity: tokio::sync::watch::Receiver<u64>,
}

impl TaskLiveView {
    pub fn bytes(&self) -> u64 {
        *self.activity.borrow()
    }

    pub fn live_output(
        &self,
    ) -> (
        astra_text_utils::live_redaction::LiveRedactedSnapshot,
        astra_text_utils::live_redaction::LiveRedactedSnapshot,
    ) {
        live_output(&self.live)
    }
}

/// A stream belonging to a task's original capture.
#[derive(Clone, Copy)]
pub enum CaptureStream {
    Stdout,
    Stderr,
}

/// An independent cursor over a pinned capture. Positional reads never change
/// the writer's or another observer's shared file offset.
pub struct TaskOutputReader {
    file: Arc<std::fs::File>,
    position: u64,
}

impl TaskOutputReader {
    pub fn new(file: std::fs::File) -> Self {
        Self {
            file: Arc::new(file),
            position: 0,
        }
    }

    pub fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        self.file.metadata()
    }
}

impl Read for TaskOutputReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        let count = std::os::unix::fs::FileExt::read_at(&*self.file, buf, self.position)?;
        #[cfg(windows)]
        let count = std::os::windows::fs::FileExt::seek_read(&*self.file, buf, self.position)?;
        self.position = self.position.saturating_add(count as u64);
        Ok(count)
    }
}

impl Seek for TaskOutputReader {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.file.metadata()?.len()) + i128::from(offset),
        };
        self.position = u64::try_from(position).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid capture seek")
        })?;
        Ok(self.position)
    }
}

/// One pinned directory can be shared by all tasks in the same namespace.
pub struct TaskCaptureDirectory {
    #[cfg(unix)]
    directory: std::fs::File,
    #[cfg(unix)]
    identity: (u64, u64),
}

impl TaskCaptureDirectory {
    pub fn same_directory(&self, other: &Self) -> bool {
        #[cfg(unix)]
        {
            self.identity == other.identity
        }
        #[cfg(not(unix))]
        {
            let _ = other;
            true
        }
    }
}

fn live_output(
    state: &OnceLock<LiveCaptures>,
) -> (
    astra_text_utils::live_redaction::LiveRedactedSnapshot,
    astra_text_utils::live_redaction::LiveRedactedSnapshot,
) {
    let live = state
        .get()
        .expect("task capture live output is initialized");
    let snapshot = |stream| match live.stream(stream).lock() {
        Ok(output) => output.redacted.snapshot(),
        Err(error) => {
            let mut output = error.into_inner();
            output.redacted.withhold();
            output.redacted.snapshot()
        }
    };
    (
        snapshot(CaptureStream::Stdout),
        snapshot(CaptureStream::Stderr),
    )
}

impl TaskCapture {
    pub fn output_incomplete(&self) -> bool {
        self.failure.load(Ordering::Acquire) != CAPTURE_OK
    }
    pub fn live_view(&self) -> TaskLiveView {
        TaskLiveView {
            live: self.live.clone(),
            activity: self.activity.clone(),
        }
    }

    /// Snapshot the safe live projections without rereading the capture file.
    pub fn live_output(
        &self,
    ) -> (
        astra_text_utils::live_redaction::LiveRedactedSnapshot,
        astra_text_utils::live_redaction::LiveRedactedSnapshot,
    ) {
        live_output(&self.live)
    }

    /// Read the original file, even if its published name has changed.
    pub fn open_output(&self, stream: CaptureStream) -> TaskOutputReader {
        let index = match stream {
            CaptureStream::Stdout => 0,
            CaptureStream::Stderr => 1,
        };
        TaskOutputReader {
            file: self.readers[index].clone(),
            position: 0,
        }
    }

    pub fn output_activity(&self) -> tokio::sync::watch::Receiver<u64> {
        self.activity.clone()
    }

    pub fn bytes(&self) -> u64 {
        self.stored_bytes.load(Ordering::Acquire)
    }

    pub fn output_revision(&self) -> u64 {
        *self.activity.borrow()
    }

    /// Latest bounded raw output for a local user, never for model context.
    pub fn local_output(&self) -> (Vec<u8>, Vec<u8>) {
        let live = self
            .live
            .get()
            .expect("task capture live output is initialized");
        let tail = |stream| match live.stream(stream).lock() {
            Ok(output) => output.local_tail.iter().copied().collect(),
            Err(_) => Vec::new(),
        };
        (tail(CaptureStream::Stdout), tail(CaptureStream::Stderr))
    }

    pub fn remove_files(&self) -> std::io::Result<()> {
        let mut removed = self
            .removed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *removed {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let mut first_error = None;
            for (path, identity) in &self.files {
                if let Err(error) = unlink_owned_at(&self.directory.directory, path, *identity)
                    && error.kind() != std::io::ErrorKind::NotFound
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        // On platforms without a pinned-directory unlink, do not follow a
        // mutable path when relinquishing private deletion authority.
        *removed = true;
        Ok(())
    }
}

impl BashOutputCapture {
    /// The task registry observes these same files and counter from launch;
    /// changing foreground presentation never reopens or reseeds a stream.
    pub fn task_capture_directory(&self) -> Result<Arc<TaskCaptureDirectory>, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let directory = self
                .directory_fd
                .try_clone()
                .map_err(|_| "Error: unable to retain Bash capture directory")?;
            let metadata = directory
                .metadata()
                .map_err(|_| "Error: unable to inspect Bash capture directory")?;
            Ok(Arc::new(TaskCaptureDirectory {
                directory,
                identity: (metadata.dev(), metadata.ino()),
            }))
        }
        #[cfg(not(unix))]
        {
            Ok(Arc::new(TaskCaptureDirectory {}))
        }
    }

    pub fn task_output_files(
        &mut self,
        directory: Arc<TaskCaptureDirectory>,
    ) -> Result<(PathBuf, PathBuf, Arc<TaskCapture>), String> {
        if let Some(owner) = &self.task_capture {
            return Ok((self.stdout.clone(), self.stderr.clone(), owner.clone()));
        }
        self.state.live.get_or_init(LiveCaptures::default);
        if self.state.used.load(Ordering::Acquire) != 0 {
            return Err("Error: Bash capture must be registered before output starts".into());
        }
        #[cfg(unix)]
        let files = {
            use std::os::unix::fs::MetadataExt;
            let current = self
                .directory_fd
                .metadata()
                .map_err(|_| "Error: unable to inspect Bash capture directory")?;
            if (current.dev(), current.ino()) != directory.identity {
                return Err("Error: Bash capture directory identity changed".into());
            }
            let stdout = self
                .stdout_reader
                .metadata()
                .map_err(|_| "Error: unable to inspect Bash capture")?;
            let stderr = self
                .stderr_reader
                .metadata()
                .map_err(|_| "Error: unable to inspect Bash capture")?;
            [
                (self.stdout.clone(), (stdout.dev(), stdout.ino())),
                (self.stderr.clone(), (stderr.dev(), stderr.ino())),
            ]
        };
        #[cfg(not(unix))]
        let _ = directory;
        let owner = Arc::new(TaskCapture {
            activity: self.state.activity.subscribe(),
            failure: self.state.failure.clone(),
            stored_bytes: self.state.used.clone(),
            removed: Mutex::new(false),
            live: self.state.live.clone(),
            readers: [self.stdout_reader.clone(), self.stderr_reader.clone()],
            #[cfg(unix)]
            directory,
            #[cfg(unix)]
            files,
        });
        self.task_capture = Some(owner.clone());
        Ok((self.stdout.clone(), self.stderr.clone(), owner))
    }

    /// Keep private capture files for the task registry to observe and evict.
    pub fn retain_for_task(mut self) {
        self.retained = true;
    }

    pub fn captured_bytes(&self) -> u64 {
        self.state.used.load(Ordering::Acquire)
    }

    pub fn truncated(&self) -> bool {
        self.state.failure.load(Ordering::Acquire) == CAPTURE_LIMIT
    }

    pub fn start(workspace: &Path) -> Result<(Self, CaptureWriter, CaptureWriter), String> {
        let (capture, stdout, stderr) = Self::create(workspace)?;
        let state = capture.state.clone();
        Ok((
            capture,
            CaptureWriter {
                file: File::from_std(stdout),
                state: state.clone(),
                stream: CaptureStream::Stdout,
            },
            CaptureWriter {
                file: File::from_std(stderr),
                state,
                stream: CaptureStream::Stderr,
            },
        ))
    }

    fn create(workspace: &Path) -> Result<(Self, std::fs::File, std::fs::File), String> {
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
                retained: false,
                stdout_reader: Arc::new(stdout_reader),
                stderr_reader: Arc::new(stderr_reader),
                task_capture: None,
                #[cfg(unix)]
                directory_fd,
            },
            stdout_file,
            stderr_file,
        ))
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

impl Drop for BashOutputCapture {
    fn drop(&mut self) {
        if !self.retained {
            if let Some(owner) = &self.task_capture {
                let _ = owner.remove_files();
            } else {
                let _ = self.remove_file(&self.stdout);
                let _ = self.remove_file(&self.stderr);
            }
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

#[cfg(unix)]
fn unlink_owned_at(
    directory: &std::fs::File,
    path: &Path,
    identity: (u64, u64),
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let name = capture_file_name(path)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: fstatat initializes stat on success and never follows the leaf.
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    // dev_t differs across Unix targets, although both fields are u64 on Linux.
    #[allow(clippy::unnecessary_cast)]
    let actual = (stat.st_dev as u64, stat.st_ino as u64);
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG || actual != identity {
        return Err(std::io::Error::other("private Bash capture was replaced"));
    }
    unlink_at(directory, path)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observing_capture_does_not_move_the_writer_or_other_readers() {
        let root = tempfile::tempdir().unwrap();
        let (mut capture, mut stdout, stderr) = BashOutputCapture::start(root.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (_, _, owner) = capture.task_output_files(directory).unwrap();
        stdout.write(b"first").await.unwrap();
        let mut first = owner.open_output(CaptureStream::Stdout);
        let mut second = owner.open_output(CaptureStream::Stdout);
        let mut prefix = [0; 2];
        first.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"fi");
        stdout.write(b"second").await.unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        let mut tail = String::new();
        first.read_to_string(&mut tail).unwrap();
        let mut all = String::new();
        second.read_to_string(&mut all).unwrap();
        assert_eq!(tail, "rstsecond");
        assert_eq!(all, "firstsecond");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_cleanup_never_unlinks_a_replaced_file() {
        let root = tempfile::tempdir().unwrap();
        let (mut capture, mut stdout, stderr) = BashOutputCapture::start(root.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (stdout_path, stderr_path, owner) = capture.task_output_files(directory).unwrap();
        stdout.write(b"original").await.unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        let moved = stdout_path.with_extension("moved");
        std::fs::rename(&stdout_path, &moved).unwrap();
        std::fs::write(&stdout_path, b"replacement").unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let (owner, barrier) = (owner.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    owner.remove_files()
                })
            })
            .collect();
        barrier.wait();
        for worker in workers {
            assert!(worker.join().unwrap().is_err());
        }
        assert_eq!(std::fs::read(&stdout_path).unwrap(), b"replacement");
        assert!(!stderr_path.exists());
        std::fs::remove_file(&stdout_path).unwrap();
        std::fs::rename(moved, &stdout_path).unwrap();
        owner.remove_files().unwrap();
        drop(capture);
        assert!(!stdout_path.exists());
    }

    #[tokio::test]
    async fn private_capture_is_byte_exact_and_removed_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let (capture, mut stdout, stderr) = BashOutputCapture::start(root.path()).unwrap();
        let path = capture.stdout.clone();
        stdout.write(&[0, 255, 254, 10]).await.unwrap();
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), [0, 255, 254, 10]);
        drop(capture);
        assert!(!path.exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn activity_observers_see_concurrent_stream_writes() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut capture, mut stdout, mut stderr) =
            BashOutputCapture::start(workspace.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (stdout_path, stderr_path, owner) = capture.task_output_files(directory).unwrap();
        let mut first = owner.output_activity();
        let mut second = owner.output_activity();
        let out = tokio::spawn(async move { stdout.write(b"ready").await });
        let err = tokio::spawn(async move { stderr.write(b"warning").await });
        tokio::time::timeout(std::time::Duration::from_secs(2), first.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            std::fs::read(&stdout_path).unwrap() == b"ready"
                || std::fs::read(&stderr_path).unwrap() == b"warning"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), second.changed())
            .await
            .unwrap()
            .unwrap();
        out.await.unwrap().unwrap();
        err.await.unwrap().unwrap();
        assert_eq!(std::fs::read(stdout_path).unwrap(), b"ready");
        assert_eq!(std::fs::read(stderr_path).unwrap(), b"warning");
        assert_eq!(owner.bytes(), 12);
    }

    #[tokio::test]
    async fn live_projection_tracks_written_bytes_and_redacts_split_secrets() {
        let workspace = tempfile::tempdir().unwrap();
        let (mut capture, mut stdout, mut stderr) =
            BashOutputCapture::start(workspace.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (_, _, owner) = capture.task_output_files(directory).unwrap();
        let mut written = String::new();
        for part in ["password=", "ZZ", "ZZ", "\nREADY\n"] {
            stdout.write(part.as_bytes()).await.unwrap();
            written.push_str(part);
            let (out, _) = owner.live_output();
            let expected =
                astra_text_utils::credential_redaction::redact_credentials_for_display_streaming(
                    &written,
                    usize::MAX,
                )
                .unwrap()
                .0;
            assert_eq!(out.text, expected);
            assert!(!out.text.contains('Z'));
            assert_eq!(owner.bytes(), written.len() as u64);
        }
        stderr.write(b"warning\n").await.unwrap();
        let (out, err) = owner.live_output();
        assert!(out.text.contains("READY") && err.text.contains("warning"));
        assert_eq!(owner.bytes(), (written.len() + "warning\n".len()) as u64);
    }

    #[tokio::test]
    async fn private_capture_stops_storing_at_its_budget() {
        let root = tempfile::tempdir().unwrap();
        let (capture, mut stdout, stderr) = BashOutputCapture::start(root.path()).unwrap();
        capture
            .state
            .used
            .store(MAX_CAPTURE_BYTES, Ordering::Release);
        assert!(stdout.write(b"x").await.is_ok());
        assert!(capture.truncated());
        stdout.finish().await.unwrap();
        stderr.finish().await.unwrap();
    }

    #[tokio::test]
    async fn live_views_continue_after_the_capture_limit() {
        let root = tempfile::tempdir().unwrap();
        let (mut capture, mut stdout, mut stderr) = BashOutputCapture::start(root.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (stdout_path, stderr_path, owner) = capture.task_output_files(directory).unwrap();
        capture
            .state
            .used
            .store(MAX_CAPTURE_BYTES - 3, Ordering::Release);
        stdout.write(b"abc").await.unwrap();
        let revision = owner.output_revision();
        stdout.write(b"\npassword=secret").await.unwrap();
        stderr.write(b"warning after cap\n").await.unwrap();
        let (out, err) = owner.live_output();
        assert!(out.text.contains("password=") && !out.text.contains("secret"));
        assert!(err.text.contains("warning after cap"));
        let (local_out, local_err) = owner.local_output();
        assert!(String::from_utf8_lossy(&local_out).contains("secret"));
        assert!(String::from_utf8_lossy(&local_err).contains("warning after cap"));
        assert!(owner.output_revision() > revision);
        assert_eq!(owner.bytes(), MAX_CAPTURE_BYTES);
        assert_eq!(std::fs::read(stdout_path).unwrap(), b"abc");
        assert!(std::fs::read(stderr_path).unwrap().is_empty());
        assert!(owner.output_incomplete());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_views_continue_after_a_capture_io_failure() {
        let root = tempfile::tempdir().unwrap();
        let (mut capture, mut stdout, mut stderr) = BashOutputCapture::start(root.path()).unwrap();
        let directory = capture.task_capture_directory().unwrap();
        let (_, _, owner) = capture.task_output_files(directory).unwrap();
        let mut early = owner.output_activity();
        // A read-only file deterministically fails a write without affecting
        // any real task files or relying on a full filesystem.
        stdout.file = tokio::fs::File::open("/dev/null").await.unwrap();
        assert!(stdout.write(b"first prompt\n").await.is_err());
        early.changed().await.unwrap();
        let mut late = owner.output_activity();
        late.borrow_and_update();
        assert!(stderr.write(b"second prompt\n").await.is_err());
        tokio::time::timeout(std::time::Duration::from_secs(1), late.changed())
            .await
            .unwrap()
            .unwrap();
        let (out, err) = owner.live_output();
        assert!(out.text.contains("first prompt"));
        assert!(err.text.contains("second prompt"));
        assert!(owner.output_incomplete());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_streams_keep_draining_when_capture_budget_is_exhausted() {
        let root = tempfile::tempdir().unwrap();
        let (capture, mut stdout, mut stderr) = BashOutputCapture::start(root.path()).unwrap();
        capture
            .state
            .used
            .store(MAX_CAPTURE_BYTES - 3, Ordering::Release);
        let start = Arc::new(std::sync::Barrier::new(3));
        let runtime = tokio::runtime::Handle::current();
        let a = start.clone();
        let out_runtime = runtime.clone();
        let out = std::thread::spawn(move || {
            a.wait();
            out_runtime.block_on(async move {
                stdout.write(b"abc").await.unwrap();
                stdout.write(b"after-limit").await.unwrap();
                stdout.finish().await.unwrap();
            });
        });
        let b = start.clone();
        let err = std::thread::spawn(move || {
            b.wait();
            runtime.block_on(async move {
                stderr.write(b"xyz").await.unwrap();
                stderr.write(b"after-limit").await.unwrap();
                stderr.finish().await.unwrap();
            });
        });
        start.wait();
        out.join().unwrap();
        err.join().unwrap();
        assert!(capture.truncated());
        assert_eq!(capture.captured_bytes(), MAX_CAPTURE_BYTES);
        let written = std::fs::metadata(&capture.stdout).unwrap().len()
            + std::fs::metadata(&capture.stderr).unwrap().len();
        assert_eq!(written, 3);
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
        assert!(BashOutputCapture::start(workspace.path()).is_err());
    }
}
