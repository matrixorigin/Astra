//! Startup authority for an explicitly dedicated, evaluation-only Edge service.
//!
//! Deployment v1 is deliberately narrow: Linux x86-64, `/` on a read-only
//! SquashFS image, root-owned configuration/manifest/executables in that image,
//! and a private mode-0700 allocation directory on writable ext2/3/4 or tmpfs under
//! root-owned, non-writable ancestors. Toolchain directories have identical host
//! and guest paths. Their complete trees must contain only root-owned directories
//! and regular files, without symlinks, special files, set-ID bits or xattrs.
//! Build a flattened toolchain image; ordinary distribution trees may not qualify.
//!
//! Host root, kernel, image producer and image backing storage are trusted. The
//! operator MUST reserve the configured non-root UID exclusively for this service:
//! no login, other services, or ordinary unconfined same-UID processes. This is an
//! explicit deployment assumption, not something process enumeration can prove.
//! Root must keep image backing inaccessible to untrusted writers and must not
//! change service mounts while authority is live. Use NoNewPrivileges=yes, an
//! empty capability bounding set, no supplementary groups and KillMode=control-group.
//! Outer restrictions must permit the shared launcher's mandatory namespaces.
//! Mount real procfs at `/proc`, provide `/dev/null`, and private writable `/tmp`
//! for the existing launcher's anonymous status files. The allocation directory's
//! parent remains root-owned (unlike a UID-owned service home). Restart must not
//! adopt or delete old allocations until the existing process owner proves drain.
//!
//! Call the sandbox supervisor early entrypoint before normal Edge startup, then
//! await `EvaluationProviderAuthority::start` before registration. Retain the
//! authority for the connection/allocation lifetime. Failure MUST abort evaluation
//! startup, never select an ordinary executor. This module does not create a new
//! lifecycle or prove per-trial ownership: the existing allocation/dispatch owners
//! must bind retained directory authority to owner/trial/session/generation, keep
//! native staging outside guest mounts, serialize mutation and quarantine unsettled
//! allocations. Call `revalidate` before use; a contract/name alone grants nothing.
//!
//! Config is strict JSON with `schema_version: 1`, `deployment_id`, `expected_uid`,
//! `allocation_root`, `toolchain_manifest_path`, `toolchain_manifest_sha256` and
//! `dedicated_service_assumption: "exclusive_uid_trusted_host_v1"`. The manifest
//! is the shared `WorkspaceConfinementContract` JSON. Its file digest is SHA256 of
//! the exact JSON bytes (including whitespace), prefixed `sha256:`. Launcher and
//! supervisor digests are SHA256 of `/usr/bin/bwrap` and the running Edge binary;
//! the existing supervisor reexecs Edge, so its digest also identifies Edge.
//!
//! Each input content digest is SHA256 over `astra-eval-tree-v1\0` followed by
//! preorder records sorted by UTF-8 filename bytes (root name is empty). Each
//! record contains a length-prefixed relative path (u64 big endian), one byte
//! `d` or `f`, mode & 07777 (u32 big endian), UID and GID (u32 big endian).
//! A file additionally contains size (u64 big endian) and its raw 32-byte SHA256.
//! No timestamps, inode numbers or host paths enter the portable identity.

use std::path::Path;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationProviderConfig {
    pub schema_version: u32,
    pub deployment_id: String,
    pub expected_uid: u32,
    pub allocation_root: std::path::PathBuf,
    pub toolchain_manifest_path: std::path::PathBuf,
    pub toolchain_manifest_sha256: String,
    pub dedicated_service_assumption: String,
}

fn invalid(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, message.into())
}

fn canonical_path(path: &Path) -> std::io::Result<()> {
    let text = path.to_str().ok_or_else(|| invalid("path must be UTF-8"))?;
    if !text.starts_with('/')
        || text.len() > 4096
        || text[1..].split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
        })
    {
        return Err(invalid("path must be a canonical absolute name"));
    }
    Ok(())
}

impl EvaluationProviderConfig {
    fn validate(&self) -> std::io::Result<()> {
        if self.schema_version != 1
            || self.expected_uid == 0
            || self.deployment_id.is_empty()
            || self.deployment_id.len() > 128
            || !self
                .deployment_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || self.dedicated_service_assumption != "exclusive_uid_trusted_host_v1"
        {
            return Err(invalid("unsupported dedicated evaluation deployment"));
        }
        canonical_path(&self.allocation_root)?;
        canonical_path(&self.toolchain_manifest_path)?;
        if !self.allocation_root.starts_with("/var/lib/astra-eval")
            || self.allocation_root == Path::new("/var/lib/astra-eval")
            || !self.toolchain_manifest_path.starts_with("/etc/astra")
        {
            return Err(invalid(
                "allocation/configuration path outside fixed deployment layout",
            ));
        }
        if !self
            .toolchain_manifest_sha256
            .strip_prefix("sha256:")
            .is_some_and(|s| {
                s.len() == 64
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
        {
            return Err(invalid("manifest digest must be canonical SHA256"));
        }
        Ok(())
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use linux::EvaluationProviderAuthority;

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
pub enum EvaluationProviderAuthority {}

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
impl EvaluationProviderAuthority {
    pub async fn start(_: &Path, _: &Path) -> std::io::Result<Self> {
        Err(invalid("dedicated evaluation requires Linux x86-64"))
    }

    pub fn deployment_id(&self) -> &str {
        match *self {}
    }
    pub fn contract(&self) -> &astra_runtime_env::WorkspaceConfinementContract {
        match *self {}
    }
    pub fn contract_fingerprint(&self) -> &str {
        match *self {}
    }
    pub fn allocation_root(&self) -> &Path {
        match *self {}
    }
    pub fn allocation_directory(&self) -> &std::fs::File {
        match *self {}
    }
    pub fn read_only_paths(&self) -> Vec<std::path::PathBuf> {
        match *self {}
    }
    pub fn revalidate(&self) -> std::io::Result<()> {
        match *self {}
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux {
    use super::*;
    use astra_runtime_env::WorkspaceConfinementContract;
    use astra_sandbox::{ShellProcessBoundary, open_directory_beneath};
    use nix::libc;
    use sha2::{Digest, Sha256};
    use std::fs::{File, OpenOptions};
    use std::io::{self, Read};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::PathBuf;
    use std::time::Duration;

    const SQUASHFS: i64 = 0x73717368;
    const PROCFS: i64 = 0x9fa0;
    const MAX_JSON: u64 = 1024 * 1024;

    /// Only constructed after mounted-content verification and a real confined
    /// launch with verified receipt and authoritative descendant settlement.
    pub struct EvaluationProviderAuthority {
        config: EvaluationProviderConfig,
        contract: WorkspaceConfinementContract,
        fingerprint: String,
        allocation: File,
        retained: Vec<(PathBuf, File)>,
        mounts: Vec<Mount>,
    }

    impl EvaluationProviderAuthority {
        pub async fn start(config_path: &Path, source_root: &Path) -> io::Result<Self> {
            canonical_path(config_path)?;
            if !config_path.starts_with("/etc/astra") {
                return Err(invalid("evaluation config must reside in /etc/astra"));
            }
            let root = File::open("/")?;
            require_image(&root)?;
            let mounts = read_mounts()?;
            let root_mount = mount_for(&mounts, Path::new("/"))?;
            if root_mount.fs != "squashfs" || !root_mount.read_only {
                return Err(invalid("service root must be a read-only SquashFS mount"));
            }
            let config_file = image_file(config_path, &root, &mounts)?;
            let config: EvaluationProviderConfig =
                serde_json::from_slice(&bounded_read(&config_file)?)?;
            config.validate()?;
            check_process(config.expected_uid)?;
            let manifest_file = image_file(&config.toolchain_manifest_path, &root, &mounts)?;
            let bytes = bounded_read(&manifest_file)?;
            require_digest(&bytes, &config.toolchain_manifest_sha256)?;
            let contract: WorkspaceConfinementContract = serde_json::from_slice(&bytes)?;
            contract.validate().map_err(invalid)?;

            let allocation = trusted_directory(&config.allocation_root, Some(config.expected_uid))?;
            require_writable_allocation(&allocation)?;
            canonical_path(source_root)?;
            if source_root.parent() != Some(config.allocation_root.as_path()) {
                return Err(invalid(
                    "source must be a direct child mount of allocation_root",
                ));
            }
            let source_mount = mount_for(&mounts, source_root)?;
            if source_mount.path != source_root
                || source_mount.fs != "squashfs"
                || !source_mount.read_only
                || mounts.iter().any(|mount| {
                    mount.path != config.allocation_root
                        && mount.path.starts_with(&config.allocation_root)
                        && mount.path != source_root
                })
            {
                return Err(invalid(
                    "allocation root permits only the frozen read-only source mount",
                ));
            }
            let source =
                open_directory_beneath(&allocation, Path::new(source_root.file_name().unwrap()))?;
            check_directory(&source, 0, false)?;
            require_image(&source)?;
            let mut retained = vec![
                (PathBuf::from("/"), root),
                (source_root.to_path_buf(), source),
                (config_path.to_path_buf(), config_file),
                (config.toolchain_manifest_path.clone(), manifest_file),
            ];
            for input in &contract.toolchain_manifest.inputs {
                // Use the canonical contract's path rules, including reserved roots.
                astra_runtime_env::validate_confined_toolchain_mount(&input.guest_mount_path)
                    .map_err(invalid)?;
                let path = Path::new(&input.guest_mount_path);
                require_image_mount(&mounts, path)?;
                let directory = trusted_directory(path, None)?;
                require_image(&directory)?;
                if tree_digest(path, &directory)? != input.content_digest {
                    return Err(invalid("mounted toolchain content does not match manifest"));
                }
                retained.push((path.to_path_buf(), directory));
            }
            let launcher_path = Path::new("/usr/bin/bwrap");
            let launcher = image_file(launcher_path, &retained[0].1, &mounts)?;
            let executable_path = std::env::current_exe()?;
            canonical_path(&executable_path)?;
            let executable = image_file(&executable_path, &retained[0].1, &mounts)?;
            let running = File::open("/proc/self/exe")?;
            if !same(&running, &executable)? {
                return Err(invalid(
                    "running Edge executable differs from supervisor executable",
                ));
            }
            for (file, digest) in [
                (&launcher, &contract.toolchain_manifest.launcher_digest),
                (&executable, &contract.toolchain_manifest.supervisor_digest),
            ] {
                if file.metadata()?.mode() & 0o111 == 0 || file_digest(file)? != *digest {
                    return Err(invalid("launcher/supervisor executable identity mismatch"));
                }
            }
            retained.push((launcher_path.to_path_buf(), launcher));
            retained.push((executable_path, executable));
            let fingerprint = contract.fingerprint().map_err(invalid)?;
            let authority = Self {
                config,
                contract,
                fingerprint,
                allocation,
                retained,
                mounts,
            };
            authority.revalidate()?;
            authority.probe().await?;
            authority.revalidate()?;
            Ok(authority)
        }

        pub fn deployment_id(&self) -> &str {
            &self.config.deployment_id
        }
        pub fn contract(&self) -> &WorkspaceConfinementContract {
            &self.contract
        }
        pub fn contract_fingerprint(&self) -> &str {
            &self.fingerprint
        }
        /// Label only. Allocate via the retained directory handle, then retain
        /// each trial's own authority in the existing materialization owner.
        pub fn allocation_root(&self) -> &Path {
            &self.config.allocation_root
        }
        pub fn allocation_directory(&self) -> &File {
            &self.allocation
        }
        pub fn read_only_paths(&self) -> Vec<PathBuf> {
            self.contract
                .toolchain_manifest
                .inputs
                .iter()
                .map(|input| PathBuf::from(&input.guest_mount_path))
                .collect()
        }

        /// Refuse drift. Immutable image bytes need not be rehashed: retained
        /// inode identities and the original mount topology must still agree.
        /// This does not make subsequent pathname IO safe against host root.
        pub fn revalidate(&self) -> io::Result<()> {
            check_process(self.config.expected_uid)?;
            if read_mounts()? != self.mounts {
                return Err(invalid("service mount topology changed"));
            }
            for (path, file) in &self.retained {
                require_image(file)?;
                if !same(file, &open_nofollow(path)?)? {
                    return Err(invalid("retained image authority was replaced"));
                }
            }
            let allocation =
                trusted_directory(&self.config.allocation_root, Some(self.config.expected_uid))?;
            require_writable_allocation(&allocation)?;
            if !same(&self.allocation, &allocation)? {
                return Err(invalid("allocation authority was replaced"));
            }
            Ok(())
        }

        async fn probe(&self) -> io::Result<()> {
            // Persist before awaiting. Cancellation/drop or unproven settlement
            // must leave this directory quarantined, not recycle live storage.
            let probe = tempfile::Builder::new()
                .prefix(".startup-probe-")
                .tempdir_in(self.allocation_root())?
                .keep();
            let boundary = ShellProcessBoundary {
                workspace: probe.clone(),
                home: "/home/sandbox".into(),
                temp: "/tmp".into(),
                read_only_paths: self.read_only_paths(),
            };
            // Fixed executable and argv: no config text is interpreted as shell.
            let plan = match boundary.launch_plan(&probe, "/usr/bin/true", &[]) {
                Ok(plan) => plan,
                Err(error) => {
                    std::fs::remove_dir(&probe)?;
                    return Err(error);
                }
            };
            let mut limits = astra_sandbox::IsolationConfig::strict(probe.clone());
            limits.timeout = Duration::from_secs(15);
            limits.max_output_bytes = 4096;
            let output = astra_sandbox::execute_confined_with_cancel(plan, &limits, None).await;
            let process = &output.process;
            let settled = process.scope_settled
                && process.scope_ownership
                    == Some(astra_sandbox::ScopeOwnership::InvocationSupervisor);
            if settled {
                std::fs::remove_dir(&probe)?;
            }
            if !settled
                || !process.execution_started
                || !process.namespace_active
                || process.timed_out
                || process.cancelled
                || process.exit_code != Some(0)
                || !matches!(
                    output.confinement,
                    astra_sandbox::ShellConfinementEvidence::LinuxRestrictedRootV1 {
                        receipt: Ok(0)
                    }
                )
            {
                return Err(invalid(
                    "actual confined startup probe failed; no evaluation authority admitted (unsettled probe storage is quarantined)",
                ));
            }
            Ok(())
        }
    }

    fn bounded_read(mut file: &File) -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_JSON + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_JSON {
            return Err(invalid("oversized provider metadata"));
        }
        Ok(bytes)
    }

    fn require_digest(bytes: &[u8], expected: &str) -> io::Result<()> {
        if format!("sha256:{:x}", Sha256::digest(bytes)) != expected {
            return Err(invalid("manifest file identity mismatch"));
        }
        Ok(())
    }

    fn open_nofollow(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
    }

    fn same(a: &File, b: &File) -> io::Result<bool> {
        let a = a.metadata()?;
        let b = b.metadata()?;
        Ok((a.dev(), a.ino(), a.mode()) == (b.dev(), b.ino(), b.mode()))
    }

    fn no_xattrs(file: &File) -> io::Result<()> {
        // Unsupported xattr inspection is not evidence of absence.
        let size = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
        if size < 0 {
            return Err(io::Error::last_os_error());
        }
        if size != 0 {
            return Err(invalid(
                "deployment v1 forbids xattrs, including ACLs and file capabilities",
            ));
        }
        Ok(())
    }

    fn trusted_directory(path: &Path, leaf_uid: Option<u32>) -> io::Result<File> {
        canonical_path(path)?;
        let mut current = File::open("/")?;
        check_directory(&current, 0, false)?;
        let parts: Vec<_> = path
            .strip_prefix("/")
            .map_err(|_| invalid("absolute path required"))?
            .iter()
            .collect();
        for (index, part) in parts.iter().enumerate() {
            current = open_directory_beneath(&current, Path::new(part))?;
            let private = index + 1 == parts.len() && leaf_uid.is_some();
            check_directory(
                &current,
                if private { leaf_uid.unwrap() } else { 0 },
                private,
            )?;
        }
        Ok(current)
    }

    fn check_directory(file: &File, uid: u32, private: bool) -> io::Result<()> {
        let metadata = file.metadata()?;
        if !metadata.is_dir()
            || metadata.uid() != uid
            || metadata.mode() & 0o7022 != 0
            || (private && metadata.mode() & 0o777 != 0o700)
        {
            return Err(invalid(
                "directory ownership/mode violates dedicated deployment",
            ));
        }
        no_xattrs(file)
    }

    fn filesystem(file: &File) -> io::Result<(i64, bool)> {
        let mut fs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let mut vfs = std::mem::MaybeUninit::<libc::statvfs>::uninit();
        if unsafe { libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) } != 0
            || unsafe { libc::fstatvfs(file.as_raw_fd(), vfs.as_mut_ptr()) } != 0
        {
            return Err(io::Error::last_os_error());
        }
        let fs = unsafe { fs.assume_init() };
        let vfs = unsafe { vfs.assume_init() };
        Ok((fs.f_type, vfs.f_flag & libc::ST_RDONLY != 0))
    }

    fn require_image(file: &File) -> io::Result<()> {
        if filesystem(file)? != (SQUASHFS, true) {
            return Err(invalid("immutable read-only SquashFS image required"));
        }
        Ok(())
    }

    fn require_writable_allocation(file: &File) -> io::Result<()> {
        let (kind, read_only) = filesystem(file)?;
        if read_only || !matches!(kind, 0xef53 | 0x01021994) {
            return Err(invalid(
                "private allocation requires writable ext2/3/4 or tmpfs",
            ));
        }
        Ok(())
    }

    fn image_file(path: &Path, root: &File, mounts: &[Mount]) -> io::Result<File> {
        canonical_path(path)?;
        require_image_mount(mounts, path)?;
        let parent = path
            .parent()
            .ok_or_else(|| invalid("missing image parent"))?;
        if parent != Path::new("/") {
            trusted_directory(parent, None)?;
        }
        let file = open_nofollow(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != 0
            || metadata.mode() & 0o7022 != 0
            || metadata.dev() != root.metadata()?.dev()
        {
            return Err(invalid(
                "image input must be a root-owned, unprivileged regular file",
            ));
        }
        require_image(&file)?;
        no_xattrs(&file)?;
        Ok(file)
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Mount {
        id: String,
        path: PathBuf,
        fs: String,
        read_only: bool,
        line: String,
    }

    fn proc_text(path: &str) -> io::Result<String> {
        let file = File::open(path)?;
        if filesystem(&file)?.0 != PROCFS {
            return Err(invalid("kernel procfs required"));
        }
        String::from_utf8(bounded_read(&file)?).map_err(|_| invalid("non-UTF-8 kernel metadata"))
    }

    fn read_mounts() -> io::Result<Vec<Mount>> {
        parse_mounts(&proc_text("/proc/self/mountinfo")?)
    }

    fn parse_mounts(text: &str) -> io::Result<Vec<Mount>> {
        let mut mounts = Vec::new();
        for line in text.lines() {
            let (left, right) = line
                .split_once(" - ")
                .ok_or_else(|| invalid("invalid mountinfo"))?;
            let left: Vec<_> = left.split_whitespace().collect();
            let right: Vec<_> = right.split_whitespace().collect();
            if left.len() < 6
                || right.len() < 3
                || left[4].contains('\\')
                || !left[4].starts_with('/')
            {
                return Err(invalid("unsupported mountinfo layout"));
            }
            let path = PathBuf::from(left[4]);
            if mounts.iter().any(|m: &Mount| m.path == path) {
                return Err(invalid("stacked mounts unsupported"));
            }
            mounts.push(Mount {
                id: left[0].into(),
                path,
                fs: right[0].into(),
                read_only: left[5].split(',').any(|v| v == "ro")
                    && right[2].split(',').any(|v| v == "ro"),
                line: line.into(),
            });
        }
        if mounts.is_empty() {
            return Err(invalid("empty mount table"));
        }
        Ok(mounts)
    }

    fn mount_for<'a>(mounts: &'a [Mount], path: &Path) -> io::Result<&'a Mount> {
        mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.path))
            .max_by_key(|mount| mount.path.components().count())
            .ok_or_else(|| invalid("missing mount identity"))
    }

    fn reject_submounts(mounts: &[Mount], path: &Path) -> io::Result<()> {
        if mounts
            .iter()
            .any(|mount| mount.path != path && mount.path.starts_with(path))
        {
            return Err(invalid(
                "nested mounts in authority subtree are unsupported",
            ));
        }
        Ok(())
    }

    fn require_image_mount(mounts: &[Mount], path: &Path) -> io::Result<()> {
        let mount = mount_for(mounts, path)?;
        if mount.path != Path::new("/") || mount.fs != "squashfs" || !mount.read_only {
            return Err(invalid("input is not on the immutable service root mount"));
        }
        reject_submounts(mounts, path)
    }

    fn check_process(uid: u32) -> io::Result<()> {
        validate_status(&proc_text("/proc/self/status")?, uid)
    }

    fn validate_status(status: &str, uid: u32) -> io::Result<()> {
        let field = |key: &str| -> io::Result<&str> {
            let mut values = status.lines().filter_map(|line| line.strip_prefix(key));
            let value = values
                .next()
                .ok_or_else(|| invalid("missing kernel process credential field"))?;
            if values.next().is_some() {
                return Err(invalid("duplicate kernel process credential field"));
            }
            Ok(value.trim())
        };
        let ids: Vec<_> = field("Uid:")?.split_whitespace().collect();
        if uid == 0
            || ids.len() != 4
            || ids.iter().any(|id| id.parse::<u32>().ok() != Some(uid))
            || !field("Groups:")?.is_empty()
            || field("NoNewPrivs:")? != "1"
        {
            return Err(invalid(
                "dedicated UID, no supplementary groups and no-new-privileges required",
            ));
        }
        for key in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
            if u64::from_str_radix(field(key)?, 16).ok() != Some(0) {
                return Err(invalid("all service capability sets must be empty"));
            }
        }
        Ok(())
    }

    fn raw_file_digest(mut file: &File) -> io::Result<[u8; 32]> {
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash.update(&buffer[..count]);
        }
        Ok(hash.finalize().into())
    }

    fn file_digest(file: &File) -> io::Result<String> {
        Ok(format!(
            "sha256:{}",
            raw_file_digest(file)?
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }

    fn tree_digest(path: &Path, root: &File) -> io::Result<String> {
        let mut hash = Sha256::new();
        hash.update(b"astra-eval-tree-v1\0");
        let mut remaining = 1_000_000usize;
        hash_node(
            path,
            Path::new(""),
            root,
            root.metadata()?.dev(),
            &mut hash,
            &mut remaining,
            0,
        )?;
        Ok(format!("sha256:{:x}", hash.finalize()))
    }

    fn hash_node(
        path: &Path,
        relative: &Path,
        file: &File,
        device: u64,
        hash: &mut Sha256,
        remaining: &mut usize,
        depth: usize,
    ) -> io::Result<()> {
        if *remaining == 0 || depth > 128 {
            return Err(invalid("toolchain tree exceeds validation bound"));
        }
        *remaining -= 1;
        let metadata = file.metadata()?;
        if metadata.dev() != device
            || metadata.uid() != 0
            || metadata.mode() & 0o7022 != 0
            || !(metadata.is_dir() || metadata.is_file())
        {
            return Err(invalid("unsupported mutable or special toolchain input"));
        }
        require_image(file)?;
        no_xattrs(file)?;
        hash_record(
            hash,
            relative,
            &metadata,
            if metadata.is_file() {
                Some(raw_file_digest(file)?)
            } else {
                None
            },
        )?;
        if metadata.is_dir() {
            let mut children = std::fs::read_dir(path)?
                .map(|entry| entry.map(|e| e.file_name()))
                .collect::<io::Result<Vec<_>>>()?;
            children.sort();
            for child in children {
                if child.to_str().is_none() {
                    return Err(invalid("toolchain names must be UTF-8"));
                }
                let child_path = path.join(&child);
                // Do not open devices/FIFOs even briefly. Image immutability
                // and the checked mount topology make this metadata stable.
                let kind = std::fs::symlink_metadata(&child_path)?.file_type();
                if !kind.is_file() && !kind.is_dir() {
                    return Err(invalid(
                        "symlinks and special toolchain files are unsupported",
                    ));
                }
                let child_file = open_nofollow(&child_path)?;
                hash_node(
                    &child_path,
                    &relative.join(child),
                    &child_file,
                    device,
                    hash,
                    remaining,
                    depth + 1,
                )?;
            }
        }
        Ok(())
    }

    fn hash_record(
        hash: &mut Sha256,
        relative: &Path,
        metadata: &std::fs::Metadata,
        digest: Option<[u8; 32]>,
    ) -> io::Result<()> {
        let name = relative
            .to_str()
            .ok_or_else(|| invalid("toolchain names must be UTF-8"))?
            .as_bytes();
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name);
        hash.update(if digest.is_some() { b"f" } else { b"d" });
        hash.update((metadata.mode() & 0o7777).to_be_bytes());
        hash.update(metadata.uid().to_be_bytes());
        hash.update(metadata.gid().to_be_bytes());
        if let Some(digest) = digest {
            hash.update(metadata.len().to_be_bytes());
            hash.update(digest);
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn credentials_require_every_kernel_fact() {
            let status = "Uid:\t1001 1001 1001 1001\nGroups:\nNoNewPrivs:\t1\nCapInh:\t0\nCapPrm:\t0\nCapEff:\t0\nCapBnd:\t0\nCapAmb:\t0\n";
            assert!(validate_status(status, 1001).is_ok());
            assert!(validate_status(status, 1002).is_err());
            for (before, after) in [
                ("Groups:\n", "Groups:\t1001\n"),
                ("CapEff:\t0", "CapEff:\t1"),
                ("CapBnd:\t0", "CapBnd:\t1"),
                ("NoNewPrivs:\t1", "NoNewPrivs:\t0"),
                ("1001 1001 1001 1001", "1001 1001 0 1001"),
                ("CapAmb:\t0\n", ""),
            ] {
                assert!(validate_status(&status.replace(before, after), 1001).is_err());
            }
        }

        #[test]
        fn mount_validation_rejects_overmounts_and_writable_images() {
            let root = "1 0 7:0 / / ro - squashfs /dev/loop0 ro\n";
            let mounts = parse_mounts(root).unwrap();
            assert!(require_image_mount(&mounts, Path::new("/usr/bin")).is_ok());
            let mounts = parse_mounts(&format!(
                "{root}2 1 0:1 / /usr/bin/sub rw - tmpfs tmpfs rw\n"
            ))
            .unwrap();
            assert!(require_image_mount(&mounts, Path::new("/usr/bin")).is_err());
            let mounts = parse_mounts(&root.replace(" ro", " rw")).unwrap();
            assert!(require_image_mount(&mounts, Path::new("/usr/bin")).is_err());
        }

        #[test]
        fn manifest_identity_is_exact_and_content_sensitive() {
            let bytes = b"{\"schema_version\":1}";
            let expected = format!("sha256:{:x}", Sha256::digest(bytes));
            assert!(require_digest(bytes, &expected).is_ok());
            assert!(require_digest(b"{\"schema_version\":2}", &expected).is_err());
            assert!(require_digest(b"{\"schema_version\":1}\n", &expected).is_err());
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("tool");
            std::fs::write(&path, b"tool-v1").unwrap();
            let identity = |name: &str| {
                let file = File::open(&path).unwrap();
                let mut hash = Sha256::new();
                hash_record(
                    &mut hash,
                    Path::new(name),
                    &file.metadata().unwrap(),
                    Some(raw_file_digest(&file).unwrap()),
                )
                .unwrap();
                hash.finalize()
            };
            let original = identity("tool");
            assert_ne!(original, identity("renamed"));
            std::fs::write(&path, b"tool-v2").unwrap();
            assert_ne!(original, identity("tool"));
        }

        #[test]
        fn manifest_uses_shared_paths_and_binds_both_executables() {
            let digest = format!("sha256:{}", "a".repeat(64));
            let wire = serde_json::json!({
                "profile_id": astra_runtime_env::WORKSPACE_CONFINEMENT_PROFILE,
                "toolchain_manifest": {
                    "schema_version": 1,
                    "inputs": [{"guest_mount_path": "/usr/bin", "content_digest": digest}],
                    "launcher_digest": digest,
                    "supervisor_digest": digest
                }
            });
            let contract: WorkspaceConfinementContract =
                serde_json::from_value(wire.clone()).unwrap();
            for field in ["launcher_digest", "supervisor_digest"] {
                let mut changed = wire.clone();
                changed["toolchain_manifest"][field] = format!("sha256:{}", "b".repeat(64)).into();
                let changed: WorkspaceConfinementContract =
                    serde_json::from_value(changed).unwrap();
                assert_ne!(
                    contract.fingerprint().unwrap(),
                    changed.fingerprint().unwrap()
                );
            }
            for path in ["/etc", "/usr", "/usr/../bin", "/workspace", "/usr//bin"] {
                let mut invalid = wire.clone();
                invalid["toolchain_manifest"]["inputs"][0]["guest_mount_path"] = path.into();
                assert!(serde_json::from_value::<WorkspaceConfinementContract>(invalid).is_err());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_explicit_deployment_assumption_fail_closed() {
        for path in [
            "relative",
            "/",
            "/var//lib",
            "/var/./lib",
            "/var/../lib",
            "/var/lib/",
            "/var/lib;id",
        ] {
            assert!(canonical_path(Path::new(path)).is_err(), "{path}");
        }
        let mut config = EvaluationProviderConfig {
            schema_version: 1,
            deployment_id: "eval-1".into(),
            expected_uid: 1001,
            allocation_root: "/var/lib/astra-eval/allocations".into(),
            toolchain_manifest_path: "/etc/astra/toolchain.json".into(),
            toolchain_manifest_sha256: format!("sha256:{}", "a".repeat(64)),
            dedicated_service_assumption: "exclusive_uid_trusted_host_v1".into(),
        };
        assert!(config.validate().is_ok());
        config.expected_uid = 0;
        assert!(config.validate().is_err());
        config.expected_uid = 1001;
        config.dedicated_service_assumption.clear();
        assert!(config.validate().is_err());
    }
}
