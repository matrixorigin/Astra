//! Host-selected process confinement. This wraps launch, not process ownership:
//! cancellation and descendant settlement remain with `BashInvocationOwner`.
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellProcessBoundary {
    pub workspace: PathBuf,
    pub home: PathBuf,
    pub temp: PathBuf,
    /// Explicit additional toolchain inputs. Never inferred from the user's HOME.
    pub read_only_paths: Vec<PathBuf>,
}

impl ShellProcessBoundary {
    /// Resolve and validate paths immediately before launch. No directory is
    /// created here; provisioning and ownership belong to the caller.
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
            Err("this host does not support the selected shell process boundary".into())
        }
    }

    /// A fresh environment, independent of user overlays and credential stores.
    pub fn environment(&self) -> Vec<(&'static str, std::ffi::OsString)> {
        vec![
            ("PATH", "/usr/bin:/bin:/usr/sbin:/sbin".into()),
            ("HOME", self.home.as_os_str().into()),
            ("TMPDIR", self.temp.as_os_str().into()),
            ("TMP", self.temp.as_os_str().into()),
            ("TEMP", self.temp.as_os_str().into()),
            ("LC_ALL", "C".into()),
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

    #[test]
    fn rejects_shared_private_directories() {
        let root = tempfile::tempdir().unwrap();
        let mut boundary = fixture(root.path());
        boundary.home = root.path().to_path_buf();
        assert!(boundary.validate(&boundary.workspace).is_err());
        boundary.home = boundary.temp.clone();
        assert!(boundary.validate(&boundary.workspace).is_err());
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
