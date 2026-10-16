use super::resolve_bash_workdir;
use std::fs::{self, File};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::symlink;
use std::path::Path;

const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
const WRITE_FILE: u64 = 1 << 1;
const HANDLED: u64 = READ_FILE | READ_DIR | WRITE_FILE;

// The ruleset is irreversible, so this test runs in its own subprocess.
fn restrict_filesystem(workspace: &Path) {
    #[repr(C)]
    struct RulesetAttr {
        handled_access_fs: u64,
    }
    #[repr(C, packed)]
    struct PathBeneathAttr {
        allowed_access: u64,
        parent_fd: i32,
    }
    let attr = RulesetAttr {
        handled_access_fs: HANDLED,
    };
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr as *const RulesetAttr,
            std::mem::size_of::<RulesetAttr>(),
            0,
        )
    };
    assert!(
        fd >= 0,
        "create Landlock ruleset: {}",
        std::io::Error::last_os_error()
    );
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };
    let mut paths = vec![(workspace, HANDLED)];
    for path in ["/usr", "/bin", "/lib", "/lib64", "/etc", "/proc"] {
        if Path::new(path).exists() {
            paths.push((Path::new(path), READ_FILE | READ_DIR));
        }
    }
    for (path, access) in paths {
        let directory = File::open(path).unwrap();
        let attr = PathBeneathAttr {
            allowed_access: access,
            parent_fd: directory.as_raw_fd(),
        };
        assert_eq!(
            unsafe {
                libc::syscall(
                    libc::SYS_landlock_add_rule,
                    ruleset.as_raw_fd(),
                    1,
                    &attr as *const PathBeneathAttr,
                    0,
                )
            },
            0,
            "add Landlock rule: {}",
            std::io::Error::last_os_error()
        );
    }
    assert_eq!(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        0
    );
    assert_eq!(
        unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset.as_raw_fd(), 0) },
        0,
        "restrict Landlock: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
#[ignore = "requires a Linux kernel with Landlock enabled; run with --ignored"]
fn bash_workdir_under_landlock() {
    const FIXTURE_ENV: &str = "ASTRA_LANDLOCK_WORKDIR_TEST_FIXTURE";
    let Ok(fixture) = std::env::var(FIXTURE_ENV) else {
        let fixture = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shell_ops::landlock_tests::bash_workdir_under_landlock",
                "--ignored",
                "--nocapture",
            ])
            .env(FIXTURE_ENV, fixture.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };
    let fixture = Path::new(&fixture);
    let workspace = fixture.join("workspace");
    let nested = workspace.join(".moi");
    let outside = fixture.join("private");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir(&outside).unwrap();
    fs::write(nested.join("input"), "source").unwrap();
    fs::write(outside.join("secret"), "private").unwrap();
    symlink(&nested, workspace.join("inside")).unwrap();
    symlink(&outside, workspace.join("escape")).unwrap();
    restrict_filesystem(&workspace);
    assert_eq!(
        File::open("/").unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    assert!(
        fs::read_dir(fixture).is_err(),
        "workspace ancestors remain unreadable"
    );

    let prepared =
        resolve_bash_workdir(&workspace, &serde_json::json!({"workdir": ".moi"})).unwrap();
    let inspection = prepared.inspection();
    assert!(astra_sandbox::PinnedWorkspaceInspection::from_paths(&workspace, &nested).is_ok());
    assert!(inspection.matches_root_path(&workspace).unwrap());
    assert_eq!(
        inspection.working_directory_relative().unwrap(),
        Path::new(".moi")
    );
    assert!(inspection.open_source(&nested.join("input"), false).is_ok());
    assert!(
        inspection
            .open_source(Path::new("inside/input"), true)
            .is_ok()
    );
    assert!(
        inspection
            .regular_siblings(
                Path::new(".moi/input"),
                &File::open(nested.join("input")).unwrap(),
                10
            )
            .is_ok()
    );
    let restored = inspection
        .open_for_restore(Path::new(".moi/restored"), true, None)
        .unwrap();
    drop(restored);
    assert!(!inspection.target_is_inside(Path::new("../../private/secret")));
    assert!(!inspection.target_is_inside(Path::new("../escape/secret")));
    assert!(
        inspection
            .open_source(&outside.join("secret"), false)
            .is_err()
    );
    assert!(resolve_bash_workdir(&workspace, &serde_json::json!({"workdir": "escape"})).is_err());

    let retained = workspace.join("retained");
    fs::rename(&nested, &retained).unwrap();
    symlink(&outside, &nested).unwrap();

    // Launch with the same retained handle installation used by Bash execution.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = runtime.block_on(async {
        let mut command = tokio::process::Command::new("/bin/bash");
        command.args([
            "--noprofile",
            "--norc",
            "-c",
            "echo test; pwd; cat input; printf pinned > marker",
        ]);
        prepared.install_on_tokio_command(&mut command).unwrap();
        command.output().await.unwrap()
    });
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("test\n{}\nsource", retained.display())
    );
    assert_eq!(fs::read(retained.join("marker")).unwrap(), b"pinned");
    assert!(!outside.join("marker").exists());
    assert_eq!(
        fs::read(outside.join("secret")).unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    assert_eq!(
        fs::read_dir("/").unwrap_err().raw_os_error(),
        Some(libc::EACCES)
    );
    assert_eq!(
        fs::write(outside.join("secret"), "overwrite")
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EACCES)
    );
}
