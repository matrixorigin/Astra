//! Host-selected process confinement. This wraps launch, not process ownership:
//! cancellation and descendant settlement remain with `BashInvocationOwner`.
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellProcessBoundary {
    pub workspace: PathBuf,
    /// Provisioned private directory on macOS. Linux launch plans use fresh
    /// tmpfs at /home/sandbox and never expose this host directory.
    pub home: PathBuf,
    /// Provisioned private directory on macOS. Linux uses fresh /tmp tmpfs.
    pub temp: PathBuf,
    /// Explicit toolchain inputs, never inferred from the user's HOME. This is
    /// the complete Linux system/toolchain manifest; macOS adds its system roots.
    pub read_only_paths: Vec<PathBuf>,
}

impl ShellProcessBoundary {
    /// Prepare the Linux restricted-root target. The returned plan owns mount
    /// and policy descriptors; it never spawns, waits, or settles processes.
    /// `read_only_paths` is the complete explicit system/toolchain manifest
    /// (normally /usr/bin and /usr/lib, plus /usr/lib64 where present).
    /// HOME and temporary storage are fresh tmpfs mounts, not host directories.
    /// This first profile accepts only the workspace root as cwd, avoiding a
    /// second, raceable lookup of a mutable workspace subdirectory.
    #[cfg(target_os = "linux")]
    pub fn launch_plan(
        &self,
        cwd: &Path,
        program: &str,
        args: &[String],
    ) -> std::io::Result<crate::ShellLaunchPlan> {
        self.launch_plan_with_protected_paths(cwd, program, args, &[])
    }

    /// Protect evaluator-owned directories within the writable workspace.
    /// Each source is pinned relative to the selected workspace handle.
    #[cfg(target_os = "linux")]
    pub fn launch_plan_with_protected_paths(
        &self,
        cwd: &Path,
        program: &str,
        args: &[String],
        protected_paths: &[PathBuf],
    ) -> std::io::Result<crate::ShellLaunchPlan> {
        crate::linux_shell_boundary::prepare(self, cwd, program, args, protected_paths)
    }

    /// Resolve path admission for the provisioned-directory representation.
    /// This is not race-safe launch enforcement; Linux callers must prepare a
    /// launch_plan, which pins inputs and uses fresh anonymous HOME/TMP mounts.
    /// No directory is created here; provisioning belongs to the caller.
    pub fn validate(&self, cwd: &Path) -> Result<Self, String> {
        fn directory(path: &Path) -> Result<PathBuf, String> {
            let path = path
                .canonicalize()
                .map_err(|e| format!("cannot resolve process boundary: {e}"))?;
            if !path.is_dir() {
                return Err("process boundary path must be a directory".into());
            }
            Ok(path)
        }
        let workspace = directory(&self.workspace)?;
        if workspace.parent().is_none() {
            return Err("filesystem root cannot be a confined workspace".into());
        }
        let cwd = directory(cwd)?;
        #[cfg(target_os = "linux")]
        let (home, temp) = {
            if cwd != workspace {
                return Err("restricted profile requires workspace-root cwd".into());
            }
            (PathBuf::from("/home/sandbox"), PathBuf::from("/tmp"))
        };
        #[cfg(not(target_os = "linux"))]
        let (home, temp) = {
            let home = directory(&self.home)?;
            let temp = directory(&self.temp)?;
            if !cwd.starts_with(&workspace)
                || home == workspace
                || temp == workspace
                || !home.starts_with(&workspace)
                || !temp.starts_with(&workspace)
                || home.starts_with(&temp)
                || temp.starts_with(&home)
            {
                return Err("cwd must be in the workspace; private HOME and TMP must be separate directories within it".into());
            }
            (home, temp)
        };
        let read_only_paths = self
            .read_only_paths
            .iter()
            .map(|p| directory(p))
            .collect::<Result<Vec<_>, _>>()?;
        if read_only_paths
            .iter()
            .any(|p| p.parent().is_none() || p.starts_with(&workspace) || workspace.starts_with(p))
        {
            return Err(
                "toolchain read roots must be separate from the workspace and filesystem root"
                    .into(),
            );
        }
        Ok(Self {
            workspace,
            home,
            temp,
            read_only_paths,
        })
    }

    /// Apply the same host roots to native file admission. This is a path
    /// check, not an atomic open: callers still own race-safe file access.
    pub fn validate_file_access(&self, path: &Path, write: bool) -> Result<PathBuf, String> {
        let mut policy = crate::SandboxPolicy::for_project(&self.workspace);
        policy.allowed_paths = if write {
            Vec::new()
        } else {
            self.read_only_paths.clone()
        };
        let path = path.to_str().ok_or("file access requires a UTF-8 path")?;
        crate::validate_path(&policy, path)
            .map_err(|_| "path is outside the immutable host file boundary; changing permission mode cannot grant access".to_string())
    }

    /// Return an argv wrapper; never interpolate the command into profile text.
    /// Unsupported hosts fail closed instead of executing an ordinary shell.
    pub fn wrap(&self, program: &str, args: &[String]) -> Result<(String, Vec<String>), String> {
        #[cfg(target_os = "macos")]
        {
            let mut wrapped = vec!["-p".into(), self.profile()?, program.into()];
            wrapped.extend_from_slice(args);
            Ok(("/usr/bin/sandbox-exec".into(), wrapped))
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (program, args);
            Err("tuple wrapping is unsupported on this host; Linux requires launch_plan and setup evidence".into())
        }
    }

    /// A fresh environment, independent of user overlays and credential stores.
    pub fn environment(&self) -> Vec<(&'static str, std::ffi::OsString)> {
        #[cfg(target_os = "linux")]
        let (home, temp) = (Path::new("/home/sandbox"), Path::new("/tmp"));
        #[cfg(not(target_os = "linux"))]
        let (home, temp) = (self.home.as_path(), self.temp.as_path());
        vec![
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("HOME", home.as_os_str().into()),
            ("TMPDIR", temp.as_os_str().into()),
            ("TMP", temp.as_os_str().into()),
            ("TEMP", temp.as_os_str().into()),
            ("LC_ALL", "C".into()),
            ("TZ", "UTC".into()),
            ("GIT_CONFIG_NOSYSTEM", "1".into()),
            ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
            ("GIT_TERMINAL_PROMPT", "0".into()),
        ]
    }

    #[cfg(target_os = "macos")]
    fn profile(&self) -> Result<String, String> {
        fn quoted(path: &Path) -> Result<String, String> {
            let path = path
                .to_str()
                .ok_or("process boundary requires UTF-8 paths")?;
            if path.chars().any(char::is_control) {
                return Err("process boundary paths cannot contain control characters".into());
            }
            Ok(format!(
                "\"{}\"",
                path.replace('\\', "\\\\").replace('"', "\\\"")
            ))
        }
        let mut profile = String::from(
            "(version 1)(deny default)(allow process*)(allow sysctl-read)(allow file-read-metadata)(allow file-read-data (literal \"/\"))(allow file-read* (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/private/var/db/dyld\") (literal \"/dev/null\"))(allow file-write* (literal \"/dev/null\"))",
        );
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))",
            quoted(&self.workspace)?
        ));
        for path in &self.read_only_paths {
            profile.push_str(&format!("(allow file-read* (subpath {}))", quoted(path)?));
        }
        Ok(profile)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> ShellProcessBoundary {
        let workspace = root.join("workspace");
        let home = workspace.join("home");
        let temp = workspace.join("tmp");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&temp).unwrap();
        ShellProcessBoundary {
            workspace,
            home,
            temp,
            read_only_paths: vec![],
        }
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn rejects_shared_private_directories() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        boundary.home = root.path().to_path_buf();
        assert!(boundary.validate(&boundary.workspace).is_err());
        boundary.home = boundary.temp.clone();
        assert!(boundary.validate(&boundary.workspace).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_validation_uses_private_guest_storage_before_launch_preparation() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        boundary.home = root.path().join("unprovisioned-home");
        boundary.temp = root.path().join("unprovisioned-temp");
        let boundary = boundary.validate(&boundary.workspace).unwrap();
        assert_eq!(boundary.home, Path::new("/home/sandbox"));
        assert_eq!(boundary.temp, Path::new("/tmp"));
        let plan = boundary.launch_plan(&boundary.workspace, "/bin/true", &[]);
        #[cfg(target_arch = "x86_64")]
        assert!(plan.is_ok());
        #[cfg(not(target_arch = "x86_64"))]
        assert!(matches!(plan, Err(error) if error.kind() == std::io::ErrorKind::Unsupported));
        assert!(!root.path().join("unprovisioned-home").exists());
        assert!(!root.path().join("unprovisioned-temp").exists());
    }

    #[test]
    fn native_file_access_separates_read_roots_from_write_roots() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        let toolchain = root.path().join("toolchain");
        std::fs::create_dir(&toolchain).unwrap();
        std::fs::write(toolchain.join("input"), "data").unwrap();
        boundary.read_only_paths.push(toolchain.clone());
        let boundary = boundary.validate(&boundary.workspace).unwrap();
        assert!(
            boundary
                .validate_file_access(&boundary.workspace.join("new"), true)
                .is_ok()
        );
        assert!(
            boundary
                .validate_file_access(&toolchain.join("input"), false)
                .is_ok()
        );
        assert!(
            boundary
                .validate_file_access(&toolchain.join("new"), true)
                .is_err()
        );
        assert!(
            boundary
                .validate_file_access(&root.path().join("outside"), false)
                .is_err()
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), boundary.workspace.join("escape")).unwrap();
            assert!(
                boundary
                    .validate_file_access(&boundary.workspace.join("escape/new"), true)
                    .is_err()
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn seatbelt_enforces_files_and_network_with_real_processes() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let boundary = fixture(root.path());
        let boundary = boundary.validate(&boundary.workspace).unwrap();
        let sentinel = root.path().join("sentinel");
        std::fs::write(&sentinel, "private").unwrap();
        symlink(&sentinel, boundary.workspace.join("outside")).unwrap();
        let run = |script: &str, extra: &[String]| {
            let mut args = vec!["-c".into(), script.into(), "probe".into()];
            args.extend_from_slice(extra);
            let (program, args) = boundary.wrap("/bin/bash", &args).unwrap();
            std::process::Command::new(program)
                .args(args)
                .current_dir(&boundary.workspace)
                .env_clear()
                .envs(boundary.environment())
                .output()
                .unwrap()
        };
        let output = run(
            "printf ok > result; test \"$(cat result)\" = ok; test \"$HOME\" != /",
            &[],
        );
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!run("cat outside", &[]).status.success());
        assert!(!run("printf changed > outside", &[]).status.success());
        assert!(
            !run("cat \"$1\"", &[sentinel.display().to_string()])
                .status
                .success()
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "private");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let script = "exec 3<>/dev/tcp/127.0.0.1/$1";
        assert!(
            std::process::Command::new("/bin/bash")
                .args(["-c", script, "probe", &port])
                .status()
                .unwrap()
                .success()
        );
        assert!(!run(script, &[port]).status.success());
    }
}
