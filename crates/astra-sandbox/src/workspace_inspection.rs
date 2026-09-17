//! Handle-relative filesystem inspection for a prepared local invocation.
//!
//! Paths returned for evidence are labels, never authorities for subsequent IO.
#[cfg(unix)]
mod unix {
    use std::collections::VecDeque;
    use std::ffi::{CString, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::MetadataExt;
    use std::path::{Component, Path, PathBuf};
    use std::sync::Arc;

    const MAX_STEPS: usize = 512;

    #[derive(Clone, Debug)]
    pub struct PinnedWorkspaceInspection {
        root: Arc<File>,
        cwd: Arc<File>,
    }

    #[derive(Debug)]
    pub struct OpenedWorkspaceFile {
        pub file: File,
        pub workspace_relative: PathBuf,
        pub parent_identity: String,
    }

    fn identity(file: &File) -> io::Result<String> {
        let metadata = file.metadata()?;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }

    fn invalid(message: &str) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidInput, message)
    }

    fn same(left: &File, right: &File) -> io::Result<bool> {
        let l = left.metadata()?;
        let r = right.metadata()?;
        Ok(l.dev() == r.dev() && l.ino() == r.ino())
    }

    fn open_at(parent: &File, path: &Path, flags: i32) -> io::Result<File> {
        let name =
            CString::new(path.as_os_str().as_bytes()).map_err(|_| invalid("path contains NUL"))?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub fn open_directory_beneath(root: &File, relative: &Path) -> io::Result<File> {
        let mut current = root.try_clone()?;
        for component in relative.components() {
            match component {
                Component::Normal(name) => {
                    current = open_at(
                        &current,
                        Path::new(name),
                        libc::O_RDONLY | libc::O_DIRECTORY,
                    )?
                }
                Component::CurDir => {}
                _ => return Err(invalid("directory path is not relative and normalized")),
            }
        }
        Ok(current)
    }

    fn components(path: &Path) -> VecDeque<OsString> {
        path.components()
            .filter_map(|c| match c {
                Component::Normal(name) => Some(name.to_owned()),
                Component::ParentDir => Some(OsString::from("..")),
                _ => None,
            })
            .collect()
    }

    fn display_path(file: &File) -> io::Result<PathBuf> {
        #[cfg(target_os = "macos")]
        {
            let mut buffer = vec![0u8; libc::PATH_MAX as usize];
            if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            buffer.truncate(buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len()));
            Ok(PathBuf::from(OsString::from_vec(buffer)))
        }
        #[cfg(not(target_os = "macos"))]
        {
            std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
        }
    }

    impl PinnedWorkspaceInspection {
        pub fn new(root: Arc<File>, cwd: Arc<File>) -> Self {
            Self { root, cwd }
        }

        pub fn from_paths(root: &Path, cwd: &Path) -> io::Result<Self> {
            let filesystem = File::open("/")?;
            let root_path = root.canonicalize()?;
            let cwd_path = cwd.canonicalize()?;
            let root = open_directory_beneath(
                &filesystem,
                root_path
                    .strip_prefix("/")
                    .map_err(|_| invalid("workspace root is not absolute"))?,
            )?;
            let cwd_relative = cwd_path
                .strip_prefix(&root_path)
                .map_err(|_| invalid("working directory is outside workspace"))?;
            let cwd = open_directory_beneath(&root, cwd_relative)?;
            let result = Self::new(Arc::new(root), Arc::new(cwd));
            if !result.contains_directory(&result.cwd)? {
                return Err(invalid("working directory is outside workspace"));
            }
            Ok(result)
        }

        fn contains_directory(&self, directory: &File) -> io::Result<bool> {
            let mut current = directory.try_clone()?;
            for _ in 0..MAX_STEPS {
                if same(&current, &self.root)? {
                    return Ok(true);
                }
                let parent = open_at(
                    &current,
                    Path::new(".."),
                    libc::O_RDONLY | libc::O_DIRECTORY,
                )?;
                if same(&current, &parent)? {
                    return Ok(false);
                }
                current = parent;
            }
            Err(invalid("directory ancestry exceeds inspection limit"))
        }

        // Resolve actual components, including symlinks, from retained handles.
        // A missing suffix is accepted only for write-target classification.
        fn resolve(
            &self,
            path: &Path,
            from_root: bool,
            allow_missing: bool,
            no_final_symlink: bool,
        ) -> io::Result<(File, Option<OsString>)> {
            let mut directory = if path.is_absolute() {
                File::open("/")?
            } else if from_root {
                self.root.try_clone()?
            } else {
                self.cwd.try_clone()?
            };
            let mut pending = components(path);
            let mut steps = 0;
            let mut links = 0;
            while let Some(name) = pending.pop_front() {
                steps += 1;
                if steps > MAX_STEPS {
                    return Err(invalid("path exceeds inspection limit"));
                }
                let component = Path::new(&name);
                match open_at(&directory, component, libc::O_RDONLY | libc::O_DIRECTORY) {
                    Ok(next) => directory = next,
                    Err(error) => {
                        let c_name = CString::new(name.as_bytes())
                            .map_err(|_| invalid("path contains NUL"))?;
                        let mut buffer = vec![0u8; 16 * 1024];
                        let len = unsafe {
                            libc::readlinkat(
                                directory.as_raw_fd(),
                                c_name.as_ptr(),
                                buffer.as_mut_ptr().cast(),
                                buffer.len(),
                            )
                        };
                        if len >= 0 {
                            if no_final_symlink && pending.is_empty() {
                                return Err(invalid("source artifact must not be a symlink"));
                            }
                            links += 1;
                            if links > 40 || len as usize == buffer.len() {
                                return Err(invalid("symlink exceeds inspection limit"));
                            }
                            buffer.truncate(len as usize);
                            let target = PathBuf::from(OsString::from_vec(buffer));
                            if target.is_absolute() {
                                directory = File::open("/")?;
                            }
                            let mut expanded = components(&target);
                            expanded.append(&mut pending);
                            pending = expanded;
                        } else if pending.is_empty() && error.raw_os_error() == Some(libc::ENOTDIR)
                        {
                            if !self.contains_directory(&directory)? {
                                return Err(io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    "target is outside workspace",
                                ));
                            }
                            return Ok((directory, Some(name)));
                        } else if allow_missing && error.kind() == io::ErrorKind::NotFound {
                            // Resolve existing ancestors by handle, and normalize only the
                            // missing suffix. Execution-time writes remain sandboxed.
                            let mut missing_depth = 1usize;
                            while let Some(part) = pending.pop_front() {
                                steps += 1;
                                if steps > MAX_STEPS {
                                    return Err(invalid("path exceeds inspection limit"));
                                }
                                if part == ".." {
                                    missing_depth -= 1;
                                    if missing_depth == 0 {
                                        break;
                                    }
                                } else {
                                    missing_depth += 1;
                                }
                            }
                            // Once `..` returns to a real ancestor, subsequent
                            // components may exist (including symlinks). Resume
                            // actual lookup instead of treating them as missing.
                            if missing_depth == 0 {
                                continue;
                            }
                            if !self.contains_directory(&directory)? {
                                return Err(io::Error::new(
                                    io::ErrorKind::PermissionDenied,
                                    "target is outside workspace",
                                ));
                            }
                            return Ok((directory, Some(name)));
                        } else {
                            return Err(error);
                        }
                    }
                }
            }
            if !self.contains_directory(&directory)? {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "target is outside workspace",
                ));
            }
            Ok((directory, None))
        }

        pub fn target_is_inside(&self, path: &Path) -> bool {
            self.resolve(path, false, true, false).is_ok()
        }

        pub fn working_directory_relative(&self) -> io::Result<PathBuf> {
            self.directory_relative(&self.cwd)
        }

        fn directory_relative(&self, directory: &File) -> io::Result<PathBuf> {
            if same(directory, &self.root)? {
                return Ok(PathBuf::from("."));
            }
            let root_label = display_path(&self.root)?;
            let label = display_path(directory)?;
            let relative = label
                .strip_prefix(root_label)
                .map_err(|_| invalid("directory label is outside workspace"))?;
            let (verified, name) = self.resolve(relative, true, false, false)?;
            if name.is_some() || !same(&verified, directory)? {
                return Err(invalid("directory label changed during inspection"));
            }
            Ok(relative.to_path_buf())
        }

        pub fn matches_root_path(&self, path: &Path) -> io::Result<bool> {
            let filesystem = File::open("/")?;
            let absolute = path
                .strip_prefix("/")
                .map_err(|_| invalid("workspace root is not absolute"))?;
            let current = open_directory_beneath(&filesystem, absolute)?;
            same(&current, &self.root)
        }

        /// Restore uses the retained workspace authority and never an ambient
        /// manifest pathname. Missing targets must be created exclusively.
        pub fn open_for_restore(
            &self,
            relative: &Path,
            create_new: bool,
            expected_parent: Option<&str>,
        ) -> io::Result<File> {
            if relative.is_absolute()
                || relative
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            {
                return Err(invalid("restore path must be workspace-relative"));
            }
            let parent_path = relative.parent().unwrap_or_else(|| Path::new("."));
            let (parent, unresolved) = self.resolve(parent_path, true, false, false)?;
            if unresolved.is_some() {
                return Err(invalid("restore parent must be an existing directory"));
            }
            let name = relative.file_name().map(|name| name.to_os_string());
            if let Some(expected) = expected_parent
                && identity(&parent)? != expected
            {
                return Err(invalid("source parent changed since capture"));
            }
            let name = name.ok_or_else(|| invalid("restore target must be a regular file"))?;
            let c_name =
                CString::new(name.as_bytes()).map_err(|_| invalid("restore path contains NUL"))?;
            let flags = libc::O_RDWR
                | libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if create_new {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                };
            let fd = unsafe { libc::openat(parent.as_raw_fd(), c_name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err(invalid("restore target must be a regular file"));
            }
            Ok(file)
        }

        pub fn regular_siblings(
            &self,
            relative: &Path,
            expected: &File,
            limit: usize,
        ) -> io::Result<Vec<OpenedWorkspaceFile>> {
            let (parent, name) = self.resolve(relative, true, false, true)?;
            let name = name.ok_or_else(|| invalid("source is a directory"))?;
            let current = open_at(&parent, Path::new(&name), libc::O_RDONLY)?;
            if !same(&current, expected)? {
                return Err(invalid("source location changed during inference"));
            }

            let relative_parent = self.directory_relative(&parent)?;
            let duplicate = open_at(&parent, Path::new("."), libc::O_RDONLY | libc::O_DIRECTORY)?;
            use std::os::fd::IntoRawFd;
            let fd = duplicate.into_raw_fd();
            let stream = unsafe { libc::fdopendir(fd) };
            if stream.is_null() {
                let error = io::Error::last_os_error();
                unsafe {
                    libc::close(fd);
                }
                return Err(error);
            }
            struct DirectoryStream(*mut libc::DIR);
            impl Drop for DirectoryStream {
                fn drop(&mut self) {
                    unsafe {
                        libc::closedir(self.0);
                    }
                }
            }
            let stream = DirectoryStream(stream);
            let mut result = Vec::new();
            for _ in 0..limit {
                let entry = unsafe { libc::readdir(stream.0) };
                if entry.is_null() {
                    return Ok(result);
                }
                let entry_name =
                    unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
                if !entry_name.starts_with(name.as_bytes())
                    || !entry_name
                        .get(name.as_bytes().len())
                        .is_some_and(|b| matches!(*b, b'.' | b'-' | b'_' | b'~'))
                {
                    continue;
                }
                let entry_name = OsString::from_vec(entry_name.to_vec());
                let Ok(file) = open_at(&parent, Path::new(&entry_name), libc::O_RDONLY) else {
                    continue;
                };
                if file.metadata()?.is_file() {
                    let candidate = relative_parent.join(entry_name);
                    result.push(OpenedWorkspaceFile {
                        file,
                        workspace_relative: candidate
                            .strip_prefix(".")
                            .unwrap_or(&candidate)
                            .to_path_buf(),
                        parent_identity: identity(&parent)?,
                    });
                }
            }
            Err(invalid("sibling enumeration exceeds inspection limit"))
        }

        /// Open one source without following a final symlink. The returned
        /// handle, rather than its manifest label, is the read authority.
        pub fn open_regular(&self, path: &Path, from_root: bool) -> io::Result<(File, PathBuf)> {
            let source = self.open_source(path, from_root)?;
            Ok((source.file, source.workspace_relative))
        }

        pub fn open_source(&self, path: &Path, from_root: bool) -> io::Result<OpenedWorkspaceFile> {
            let (parent, name) = self.resolve(path, from_root, false, true)?;
            let name = name.ok_or_else(|| invalid("source artifact must be a regular file"))?;
            let file = open_at(&parent, Path::new(&name), libc::O_RDONLY)?;
            if !file.metadata()?.is_file() {
                return Err(invalid("source artifact must be a regular file"));
            }
            let relative = self.directory_relative(&parent)?.join(name);
            Ok(OpenedWorkspaceFile {
                file,
                workspace_relative: relative
                    .strip_prefix(".")
                    .unwrap_or(&relative)
                    .to_path_buf(),
                parent_identity: identity(&parent)?,
            })
        }
    }
}
#[cfg(unix)]
pub use unix::{OpenedWorkspaceFile, PinnedWorkspaceInspection, open_directory_beneath};

#[cfg(all(test, unix))]
mod tests {
    use super::PinnedWorkspaceInspection;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[test]
    fn retained_directory_is_used_for_targets_sources_and_labels() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("input"), "original").unwrap();
        fs::write(outside.path().join("input"), "foreign").unwrap();
        let inspection = PinnedWorkspaceInspection::from_paths(root.path(), &nested).unwrap();
        fs::rename(&nested, root.path().join("retained")).unwrap();
        symlink(outside.path(), &nested).unwrap();
        assert!(inspection.target_is_inside(Path::new("new/file")));
        assert!(inspection.target_is_inside(Path::new("../sibling")));
        assert!(!inspection.target_is_inside(Path::new("../../outside")));
        let (mut file, relative) = inspection.open_regular(Path::new("input"), false).unwrap();
        use std::io::Read;
        let mut bytes = String::new();
        file.read_to_string(&mut bytes).unwrap();
        assert_eq!(bytes, "original");
        assert_eq!(relative, Path::new("retained/input"));
        assert_eq!(
            inspection.working_directory_relative().unwrap(),
            Path::new("retained")
        );
    }

    #[test]
    fn symlinks_and_non_regular_sources_do_not_expand_authority() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        fs::write(root.path().join("real/input"), "input").unwrap();
        symlink("real", root.path().join("inside")).unwrap();
        symlink(outside.path(), root.path().join("outside")).unwrap();
        symlink("loop", root.path().join("loop")).unwrap();
        symlink("real/input", root.path().join("source-link")).unwrap();
        let inspection = PinnedWorkspaceInspection::from_paths(root.path(), root.path()).unwrap();
        assert!(inspection.target_is_inside(Path::new("inside/new")));
        assert!(!inspection.target_is_inside(Path::new("outside/new")));
        assert!(!inspection.target_is_inside(Path::new("missing/../outside/new")));
        assert!(inspection.target_is_inside(Path::new("missing/../inside/new")));

        assert!(!inspection.target_is_inside(Path::new("loop/new")));
        assert!(
            inspection
                .open_regular(Path::new("source-link"), true)
                .is_err()
        );
        assert!(
            inspection
                .open_regular(Path::new("inside/input"), true)
                .is_ok()
        );
        let fifo = root.path().join("fifo");
        use std::os::unix::ffi::OsStrExt;
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        assert!(inspection.open_regular(Path::new("fifo"), true).is_err());
    }
}
