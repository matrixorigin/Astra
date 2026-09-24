//! Filesystem authority for the canonical native file operations.
//! Restricted paths are labels beneath a retained root, never ambient IO paths.
#[cfg(unix)]
use std::collections::BTreeMap;
use std::fs::{File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;

#[derive(Clone, Debug, Default)]
pub(crate) struct FileAccess {
    confined: Option<Arc<Confined>>,
    #[cfg(unix)]
    operation: Arc<Mutex<Operation>>,
}

/// Executor-lifetime authority contains no invocation state.
#[derive(Clone, Debug)]
pub(crate) struct FileAuthority(Arc<Confined>);

impl FileAuthority {
    pub(crate) fn operation(&self) -> FileAccess {
        FileAccess {
            confined: Some(self.0.clone()),
            #[cfg(unix)]
            operation: Default::default(),
        }
    }

    pub(crate) fn matches_root(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            self.0.inspection.matches_root_path(path).unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct Operation {
    directories: BTreeMap<PathBuf, Arc<File>>,
    staging: BTreeMap<PathBuf, StagedFile>,
    private_staging: Option<Arc<PrivateStaging>>,
}

#[cfg(unix)]
#[derive(Debug)]
struct StagedFile {
    parent: Arc<PrivateStaging>,
    name: std::ffi::CString,
    file: File,
}

#[cfg(unix)]
impl Drop for StagedFile {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // Only the trusted host can address this namespace. Never unlink a
        // workspace label, including after a successful commit.
        unsafe { libc::unlinkat(self.parent.directory.as_raw_fd(), self.name.as_ptr(), 0) };
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct PrivateStaging {
    allocation_parent: Arc<File>,
    directory: File,
    name: std::ffi::CString,
}

#[cfg(unix)]
impl Drop for PrivateStaging {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        unsafe {
            libc::unlinkat(
                self.allocation_parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        };
    }
}

#[cfg(unix)]
fn same_file(a: &File, b: &File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let (a, b) = (a.metadata()?, b.metadata()?);
    Ok(a.dev() == b.dev() && a.ino() == b.ino())
}

#[derive(Debug)]
struct Confined {
    #[cfg(unix)]
    root: Arc<File>,
    #[cfg(unix)]
    allocation_parent: Arc<File>,
    #[cfg(unix)]
    inspection: astra_sandbox::PinnedWorkspaceInspection,
    label: PathBuf,
    protected: Vec<PathBuf>,
    #[cfg(unix)]
    protected_objects: Vec<Arc<File>>,
}

fn denied(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message.into())
}

impl FileAccess {
    pub(crate) fn restricted(root: &Path, protected: &[PathBuf]) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let label = root.canonicalize()?;
            let root = Arc::new(astra_sandbox::open_directory_beneath(
                &File::open("/")?,
                label
                    .strip_prefix("/")
                    .map_err(|_| denied("invalid workspace root"))?,
            )?);
            // The host allocation parent is outside the mounted workspace.
            // Host/Runner is trusted; deployment must keep this parent out of
            // confined shell system roots. This does not isolate an unconfined
            // same-UID host process.
            let allocation_parent = Arc::new(astra_sandbox::open_directory_beneath(
                &File::open("/")?,
                label
                    .parent()
                    .ok_or_else(|| denied("workspace needs a host allocation parent"))?
                    .strip_prefix("/")
                    .map_err(|_| denied("invalid allocation parent"))?,
            )?);
            if same_file(&root, &allocation_parent)? {
                return Err(denied("staging must be outside workspace"));
            }
            check_filesystem(&root, &allocation_parent)?;
            let inspection =
                astra_sandbox::PinnedWorkspaceInspection::new(root.clone(), root.clone());
            strict_directory(&root, Path::new("."))?;
            let protected: Vec<PathBuf> = protected
                .iter()
                .map(|path| {
                    let path = if path.is_absolute() {
                        path.clone()
                    } else {
                        label.join(path)
                    };
                    let relative = path
                        .strip_prefix(&label)
                        .map_err(|_| denied("protected path is outside workspace"))?;
                    normalized(relative)
                })
                .collect::<io::Result<_>>()?;
            // Protection must name an existing, unambiguous directory. Retain
            // its identity so renames and case aliases cannot remove protection.
            let protected_objects = protected
                .iter()
                .map(|path| strict_directory(&root, path).map(Arc::new))
                .collect::<io::Result<Vec<_>>>()?;
            Ok(Self {
                operation: Default::default(),
                confined: Some(Arc::new(Confined {
                    root,
                    allocation_parent,
                    inspection,
                    label,
                    protected,
                    protected_objects,
                })),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (root, protected);
            Err(denied("native descriptor confinement is unavailable"))
        }
    }

    pub(crate) fn into_authority(self) -> FileAuthority {
        FileAuthority(self.confined.expect("restricted authority"))
    }

    pub(crate) fn observation_target(&self, path: &Path) -> io::Result<PathBuf> {
        self.relative(path)
    }

    pub(crate) fn observation_open(&self, path: &Path) -> io::Result<File> {
        self.open(path)
    }

    #[cfg(unix)]
    fn check_directory(&self, directory: &File) -> io::Result<()> {
        let confined = self.confined.as_ref().unwrap();
        check_filesystem(&confined.root, directory)?;
        for protected in &confined.protected_objects {
            if same_file(protected, directory)? {
                return Err(denied("target is a host-owned managed runtime object"));
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn directory(&self, relative: &Path, create: bool) -> io::Result<File> {
        use std::os::unix::ffi::OsStrExt;
        let confined = self.confined.as_ref().unwrap();
        let mut current = confined.root.try_clone()?;
        let mut prefix = PathBuf::new();
        self.check_directory(&current)?;
        for component in relative.components() {
            prefix.push(component);
            let next = match strict_directory(&current, Path::new(component.as_os_str())) {
                Ok(next) => next,
                Err(error) if create && error.kind() == io::ErrorKind::NotFound => {
                    if self
                        .operation
                        .lock()
                        .unwrap()
                        .directories
                        .contains_key(&prefix)
                    {
                        return Err(denied("prepared parent directory disappeared"));
                    }
                    // Validate filesystem authority BEFORE the first mutation.
                    self.check_directory(&current)?;
                    let name = std::ffi::CString::new(component.as_os_str().as_bytes())
                        .map_err(|_| denied("path contains NUL"))?;
                    mkdir_checked(&confined.root, &current, &name)?;
                    strict_directory(&current, Path::new(component.as_os_str()))?
                }
                Err(error) => return Err(error),
            };
            self.check_directory(&next)?;
            let mut operation = self.operation.lock().unwrap();
            if let Some(previous) = operation.directories.get(&prefix) {
                if !same_file(previous, &next)? {
                    return Err(denied("prepared parent directory was replaced"));
                }
                current = previous.try_clone()?;
            } else {
                current = next.try_clone()?;
                operation.directories.insert(prefix.clone(), Arc::new(next));
            }
        }
        Ok(current)
    }

    pub(crate) fn is_restricted(&self) -> bool {
        self.confined.is_some()
    }

    fn relative(&self, path: &Path) -> io::Result<PathBuf> {
        let confined = self.confined.as_ref().expect("restricted access");
        let relative = if path.is_absolute() {
            path.strip_prefix(&confined.label)
                .map_err(|_| denied("target is outside pinned workspace"))?
        } else {
            path
        };
        let relative = normalized(relative)?;
        if confined
            .protected
            .iter()
            .any(|protected| relative.starts_with(protected))
        {
            return Err(denied("target is a host-owned managed runtime path"));
        }
        Ok(relative)
    }

    pub(super) fn resolve_existing(
        &self,
        root: &Path,
        path: &str,
        tool: &str,
    ) -> Result<PathBuf, crate::ToolResult> {
        if !self.is_restricted() {
            return super::resolve_existing_path_for_tool(root, path, tool);
        }
        let relative = self.relative(Path::new(path)).map_err(tool_error)?;
        self.open(&relative).map_err(tool_error)?;
        Ok(relative)
    }

    pub(super) fn resolve_write(
        &self,
        root: &Path,
        path: &str,
        tool: &str,
    ) -> Result<PathBuf, crate::ToolResult> {
        if !self.is_restricted() {
            return super::resolve_write_target_path(root, path, tool);
        }
        self.relative(Path::new(path)).map_err(tool_error)
    }

    #[cfg(unix)]
    fn parent(&self, path: &Path) -> io::Result<(File, std::ffi::CString)> {
        use std::os::unix::ffi::OsStrExt;
        let relative = self.relative(path)?;
        let parent = self.directory(relative.parent().unwrap_or(Path::new("")), false)?;
        let name = relative
            .file_name()
            .ok_or_else(|| denied("target must be a file"))?;
        let name =
            std::ffi::CString::new(name.as_bytes()).map_err(|_| denied("path contains NUL"))?;
        Ok((parent, name))
    }

    fn open(&self, path: &Path) -> io::Result<File> {
        let Some(confined) = &self.confined else {
            return File::open(path);
        };
        #[cfg(unix)]
        {
            // No symlink directory traversal, including aliases of protected roots.
            let (parent, _) = self.parent(path)?;
            let relative = self.relative(path)?;
            let authority = astra_sandbox::PinnedWorkspaceInspection::new(
                confined.root.clone(),
                Arc::new(parent),
            );
            let (file, _) = authority.open_regular(
                Path::new(
                    relative
                        .file_name()
                        .ok_or_else(|| denied("target must be a file"))?,
                ),
                false,
            )?;
            use std::os::unix::fs::MetadataExt;
            #[cfg(target_os = "linux")]
            if mount_id(&file)? != mount_id(&confined.root)? {
                return Err(denied("nested mounts are unsupported"));
            }
            let metadata = file.metadata()?;
            if metadata.nlink() != 1 || metadata.dev() != confined.root.metadata()?.dev() {
                return Err(denied("hard links and nested filesystems are unsupported"));
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        {
            let _ = confined;
            Err(denied("native descriptor confinement is unavailable"))
        }
    }

    pub(super) fn metadata(&self, path: &Path) -> io::Result<Metadata> {
        if !self.is_restricted() {
            return std::fs::metadata(path);
        }
        self.open(path)?.metadata()
    }
    pub(super) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open(path)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
    pub(super) fn read_to_string(&self, path: &Path) -> io::Result<String> {
        String::from_utf8(self.read(path)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    pub(super) fn read_lossy(&self, path: &Path) -> io::Result<String> {
        Ok(String::from_utf8_lossy(&self.read(path)?).into_owned())
    }
    pub(super) fn verify(&self, path: &Path, expected: Option<&str>) -> Result<(), String> {
        if !self.is_restricted() {
            return super::verify_expected_original_hash(path, expected);
        }
        match (expected, self.read(path)) {
            (None, Err(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            (Some(expected), Ok(bytes)) => {
                use sha2::{Digest, Sha256};
                if format!("{:x}", Sha256::digest(bytes)) == expected {
                    Ok(())
                } else {
                    Err("Error: file was modified since it was read (hash mismatch)".into())
                }
            }
            _ => Err("Error: cannot verify file preimage before commit".into()),
        }
    }

    pub(super) fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        if !self.is_restricted() {
            return std::fs::create_dir_all(path);
        }
        #[cfg(unix)]
        {
            let relative = self.relative(path)?;
            self.directory(&relative, true)?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(denied("native descriptor confinement is unavailable"))
        }
    }

    pub(super) fn stage(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        if !self.is_restricted() {
            return std::fs::write(path, content);
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let relative = self.relative(path)?;
            let confined = self.confined.as_ref().unwrap();
            let mut operation = self.operation.lock().unwrap();
            if operation.staging.contains_key(&relative) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "stage already exists",
                ));
            }
            if operation.private_staging.is_none() {
                let name = std::ffi::CString::new(format!(".astra-stage-{}", uuid::Uuid::new_v4()))
                    .unwrap();
                check_filesystem(&confined.root, &confined.allocation_parent)?;
                // Exclusive creation: an existing entry is never adopted.
                if unsafe {
                    libc::mkdirat(confined.allocation_parent.as_raw_fd(), name.as_ptr(), 0o700)
                } < 0
                {
                    return Err(io::Error::last_os_error());
                }
                let directory = strict_directory(
                    &confined.allocation_parent,
                    Path::new(name.to_str().unwrap()),
                )?;
                operation.private_staging = Some(Arc::new(PrivateStaging {
                    allocation_parent: confined.allocation_parent.clone(),
                    directory,
                    name,
                }));
            }
            let parent = operation.private_staging.as_ref().unwrap().clone();
            let name = std::ffi::CString::new(uuid::Uuid::new_v4().to_string()).unwrap();
            let authority = astra_sandbox::PinnedWorkspaceInspection::new(
                Arc::new(parent.directory.try_clone()?),
                Arc::new(parent.directory.try_clone()?),
            );
            let file = authority.open_for_restore(Path::new(name.to_str().unwrap()), true, None)?;
            let staged = StagedFile { parent, name, file };
            operation.staging.insert(relative.clone(), staged);
            operation
                .staging
                .get_mut(&relative)
                .unwrap()
                .file
                .write_all(content)
        }
        #[cfg(not(unix))]
        {
            Err(denied("native descriptor confinement is unavailable"))
        }
    }

    pub(super) fn read_stage(&self, path: &Path) -> io::Result<Vec<u8>> {
        if !self.is_restricted() {
            return self.read(path);
        }
        #[cfg(unix)]
        {
            use std::io::{Seek, SeekFrom};
            let mut operation = self.operation.lock().unwrap();
            let staged = operation
                .staging
                .get_mut(&self.relative(path)?)
                .ok_or_else(|| denied("unknown staging key"))?;
            staged.file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            staged.file.read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        #[cfg(not(unix))]
        Err(denied("native descriptor confinement is unavailable"))
    }

    pub(super) fn cleanup_stage(&self, path: &Path) -> io::Result<()> {
        if !self.is_restricted() {
            return std::fs::remove_file(path);
        }
        #[cfg(unix)]
        self.operation
            .lock()
            .unwrap()
            .staging
            .remove(&self.relative(path)?);
        Ok(())
    }

    pub(super) fn remove_file(&self, path: &Path) -> io::Result<()> {
        if !self.is_restricted() {
            return std::fs::remove_file(path);
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let (parent, name) = self.parent(path)?;
            // unlinkat never follows a retargeted final symlink.
            if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(denied("native descriptor confinement is unavailable"))
        }
    }

    pub(super) fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        if !self.is_restricted() {
            return std::fs::rename(from, to);
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let (to_parent, to_name) = self.parent(to)?;
            let key = self.relative(from)?;
            let mut operation = self.operation.lock().unwrap();
            let staged = operation
                .staging
                .get(&key)
                .ok_or_else(|| denied("unknown staging key"))?;
            check_filesystem(&staged.parent.directory, &to_parent)?;

            if unsafe {
                libc::renameat(
                    staged.parent.directory.as_raw_fd(),
                    staged.name.as_ptr(),
                    to_parent.as_raw_fd(),
                    to_name.as_ptr(),
                )
            } < 0
            {
                return Err(io::Error::last_os_error());
            }
            operation.staging.remove(&key);
            Ok(())
        }
        #[cfg(not(unix))]
        {
            Err(denied("native descriptor confinement is unavailable"))
        }
    }

    pub(crate) fn matches_root(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            self.confined
                .as_ref()
                .is_none_or(|c| c.inspection.matches_root_path(path).unwrap_or(false))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            false
        }
    }
}

fn normalized(path: &Path) -> io::Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => result.push(name),
            std::path::Component::CurDir => {}
            _ => {
                return Err(denied(
                    "path must be workspace-relative without parent traversal",
                ));
            }
        }
    }
    Ok(result)
}
fn tool_error(error: io::Error) -> crate::ToolResult {
    crate::ToolResult::error(format!("SANDBOX_DENIED: native filesystem access: {error}"))
}

// Linux's resolution constraints also reject bind mounts (device equality
// alone cannot detect those). There is deliberately no weaker fallback.
#[cfg(target_os = "linux")]
fn strict_directory(root: &File, relative: &Path) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let name = if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    };
    let name = std::ffi::CString::new(name.as_os_str().as_bytes())
        .map_err(|_| denied("path contains NUL"))?;
    let how = OpenHow {
        flags: (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: 0x01 | 0x02 | 0x04 | 0x08, // NO_XDEV | NO_MAGICLINKS | NO_SYMLINKS | BENEATH
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            name.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}
#[cfg(all(unix, not(target_os = "linux")))]
fn strict_directory(root: &File, relative: &Path) -> io::Result<File> {
    astra_sandbox::open_directory_beneath(root, relative)
}

#[cfg(target_os = "linux")]
fn mount_id(file: &File) -> io::Result<u64> {
    use std::os::fd::AsRawFd;
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    if unsafe {
        libc::statx(
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(denied("mount identity is unavailable"));
    }
    Ok(stat.stx_mnt_id)
}

#[cfg(unix)]
fn check_filesystem(root: &File, directory: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if root.metadata()?.dev() != directory.metadata()?.dev() {
        return Err(denied("nested filesystem is unsupported"));
    }
    #[cfg(target_os = "linux")]
    if mount_id(root)? != mount_id(directory)? {
        return Err(denied("nested mounts are unsupported"));
    }
    Ok(())
}

/// All Unix creation paths validate authority before issuing mkdirat, even
/// where the platform's directory opener cannot express NO_XDEV.
#[cfg(unix)]
fn mkdir_checked(root: &File, parent: &File, name: &std::ffi::CStr) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    check_filesystem(root, parent)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn confined_native_staging_and_cleanup_retain_parent_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"original").unwrap();
        let access = FileAccess::restricted(root.path(), &[]).unwrap();
        assert_eq!(access.read(Path::new("dir/file")).unwrap(), b"original");
        access.stage(Path::new("dir/staged"), b"new").unwrap();
        std::fs::rename(root.path().join("dir"), root.path().join("retained")).unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/file"), b"unchecked").unwrap();
        std::fs::write(root.path().join("dir/staged"), b"unowned").unwrap();
        assert!(
            access
                .rename(Path::new("dir/staged"), Path::new("dir/file"))
                .is_err()
        );
        access.cleanup_stage(Path::new("dir/staged")).unwrap();
        assert!(!root.path().join("retained/staged").exists());
        assert_eq!(
            std::fs::read(root.path().join("retained/file")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.path().join("dir/staged")).unwrap(),
            b"unowned"
        );
        assert_eq!(
            std::fs::read(root.path().join("dir/file")).unwrap(),
            b"unchecked"
        );
    }

    #[test]
    fn confined_native_private_staging_ignores_forged_workspace_entries() {
        let allocation = tempfile::tempdir().unwrap();
        let workspace = allocation.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("file"), b"original").unwrap();
        let access = FileAccess::restricted(&workspace, &[]).unwrap();
        access.stage(Path::new("staged"), b"new").unwrap();
        assert!(!workspace.join("staged").exists());
        let private_name = {
            let op = access.operation.lock().unwrap();
            op.private_staging
                .as_ref()
                .unwrap()
                .name
                .to_str()
                .unwrap()
                .to_owned()
        };
        assert!(allocation.path().join(&private_name).is_dir());
        assert!(
            access
                .read(&allocation.path().join(&private_name).join("anything"))
                .is_err()
        );
        std::os::unix::fs::symlink(
            allocation.path().join(&private_name),
            workspace.join("escape"),
        )
        .unwrap();
        assert!(access.read(Path::new("escape/anything")).is_err());
        std::fs::write(workspace.join("staged"), b"forged").unwrap();
        assert_eq!(access.read_stage(Path::new("staged")).unwrap(), b"new");
        assert_eq!(std::fs::read(workspace.join("file")).unwrap(), b"original");
        access
            .rename(Path::new("staged"), Path::new("file"))
            .unwrap();
        access.cleanup_stage(Path::new("staged")).unwrap();
        assert_eq!(std::fs::read(workspace.join("file")).unwrap(), b"new");
        assert_eq!(std::fs::read(workspace.join("staged")).unwrap(), b"forged");
        access.stage(Path::new("staged"), b"abandoned").unwrap();
        access.cleanup_stage(Path::new("staged")).unwrap();
        assert_eq!(std::fs::read(workspace.join("staged")).unwrap(), b"forged");
        assert_eq!(std::fs::read(workspace.join("file")).unwrap(), b"new");
        // Last operation owner also cleans abandoned candidates and its private directory.
        access.stage(Path::new("abandoned"), b"abandoned").unwrap();
        drop(access);
        assert!(!allocation.path().join(private_name).exists());
    }

    #[test]
    fn confined_native_private_staging_retains_host_allocation_parent() {
        let host = tempfile::tempdir().unwrap();
        let allocation = host.path().join("allocation");
        let workspace = allocation.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let access = FileAccess::restricted(&workspace, &[]).unwrap();
        access.stage(Path::new("staged"), b"new").unwrap();
        let private_name = access
            .operation
            .lock()
            .unwrap()
            .private_staging
            .as_ref()
            .unwrap()
            .name
            .to_str()
            .unwrap()
            .to_owned();
        let retained = host.path().join("retained");
        // This rename is performed by the trusted host, outside shell authority.
        std::fs::rename(&allocation, &retained).unwrap();
        std::fs::create_dir_all(allocation.join(&private_name)).unwrap();
        std::fs::write(
            allocation.join(&private_name).join("unrelated"),
            b"untouched",
        )
        .unwrap();
        access
            .rename(Path::new("staged"), Path::new("file"))
            .unwrap();
        assert_eq!(
            std::fs::read(retained.join("workspace/file")).unwrap(),
            b"new"
        );
        drop(access);
        assert!(!retained.join(&private_name).exists());
        assert_eq!(
            std::fs::read(allocation.join(private_name).join("unrelated")).unwrap(),
            b"untouched"
        );
    }

    #[test]
    fn confined_native_protection_rejects_symlink_configuration_and_tracks_renames() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("owned")).unwrap();
        std::fs::write(root.path().join("owned/file"), b"protected").unwrap();
        std::os::unix::fs::symlink("owned", root.path().join("protected-link")).unwrap();
        assert!(FileAccess::restricted(root.path(), &[PathBuf::from("protected-link")]).is_err());
        let access = FileAccess::restricted(root.path(), &[PathBuf::from("owned")]).unwrap();
        std::fs::rename(root.path().join("owned"), root.path().join("alias")).unwrap();
        assert!(access.read(Path::new("alias/file")).is_err());
        assert!(access.create_dir_all(Path::new("alias/new")).is_err());
        assert!(!root.path().join("alias/new").exists());
    }

    #[test]
    fn confined_native_mkdir_checks_filesystem_before_any_mutation() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let authority = File::open(root.path()).unwrap();
        // /dev is a separate filesystem on the Unix hosts supporting this
        // boundary; no mount privilege or race timing is required.
        let foreign = File::open("/dev").unwrap();
        assert_ne!(
            authority.metadata().unwrap().dev(),
            foreign.metadata().unwrap().dev()
        );
        assert!(FileAccess::restricted(Path::new("/dev"), &[]).is_err());
        let name =
            std::ffi::CString::new(format!("astra-mkdir-denied-{}", uuid::Uuid::new_v4())).unwrap();
        let error = mkdir_checked(&authority, &foreign, &name).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("nested filesystem"));
        assert!(!Path::new("/dev").join(name.to_str().unwrap()).exists());
    }
}
