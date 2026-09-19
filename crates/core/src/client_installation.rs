//! Process-lifetime occupancy for MOI-managed local client installations.
//! Software distribution lives in the paired moi-cli; this module only owns
//! the shared advisory-lock boundary, delegation, and startup notification.
use std::{
    fs::{self, File, OpenOptions},
    io::{self, IsTerminal},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use fs2::FileExt;
use serde_json::Value;

pub const PROTOCOL: &str = "moi-client-update-v1";

fn failure(reason: &str) -> io::Error {
    io::Error::other(format!("CLI_UPDATE_{reason}"))
}

fn hash_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn json_file(path: &Path) -> io::Result<Value> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
        return Err(failure("METADATA_REJECTED"));
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|_| failure("METADATA_JSON"))
}

/// Only recognize an executable physically inside a managed release. Neither
/// PATH nor an environment variable can claim a standalone/open-source install.
pub fn managed_root(executable: &Path) -> io::Result<Option<PathBuf>> {
    let executable = executable.canonicalize()?;
    let Some(release) = executable.parent() else {
        return Ok(None);
    };
    let Some(releases) = release.parent() else {
        return Ok(None);
    };
    if releases.file_name().and_then(|s| s.to_str()) != Some("releases")
        || !release
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(hash_name)
    {
        return Ok(None);
    }
    let Some(root) = releases.parent() else {
        return Ok(None);
    };
    let config = match json_file(&root.join("installation.json")) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if config["schema"].as_str() != Some(PROTOCOL) {
        return Err(failure("INSTALLATION_CONTRACT"));
    }
    Ok(Some(root.to_owned()))
}

/// Keep the returned file alive until all work has stopped. fs2 uses the same
/// flock inode/semantics as moi-cli on Linux and macOS; process exit releases it.
pub fn acquire(executable: &Path) -> io::Result<Option<File>> {
    let Some(root) = managed_root(executable)? else {
        return Ok(None);
    };
    let lock_path = root.join("runtime.lock");
    let metadata = fs::symlink_metadata(&lock_path)?;
    if !metadata.is_file() {
        return Err(failure("LOCK_REJECTED"));
    }
    let lock = OpenOptions::new().read(true).write(true).open(lock_path)?;
    FileExt::try_lock_shared(&lock).map_err(|_| failure("BUSY"))?;
    match fs::symlink_metadata(root.join("transaction.json")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        _ => return Err(failure("RECOVERY_REQUIRED")),
    }
    if root.join("current").canonicalize()? != executable.canonicalize()?.parent().unwrap() {
        return Err(failure("RESTART_REQUIRED"));
    }
    Ok(Some(lock))
}

/// Called before application runtime initialization. The updater must not hold
/// the calling Astra's shared lease, otherwise it would block its own switch.
pub fn early_command(
    executable: &Path,
    args: &[String],
    allow_update: bool,
) -> io::Result<Option<i32>> {
    if args == ["--moi-update-protocol"] {
        println!("{PROTOCOL}");
        return Ok(Some(0));
    }
    if !allow_update || args.first().map(String::as_str) != Some("update") {
        return Ok(None);
    }
    if managed_root(executable)?.is_none() {
        return Err(failure("NOT_MANAGED"));
    }
    let exe = executable.canonicalize()?;
    let sibling = exe.parent().unwrap().join("moi-cli");
    if !fs::symlink_metadata(&sibling)?.is_file() {
        return Err(failure("PAIRED_CLI_REQUIRED"));
    }
    let status = Command::new(sibling).args(args).status()?;
    Ok(Some(status.code().unwrap_or(1)))
}

pub fn startup_notice(executable: &Path, args: &[String]) {
    // Explicit agent marker supplements TTY detection. One-shot/JSON/helper
    // commands never add update text to their output streams.
    if std::env::var("MOI_AGENT_CALL").as_deref() == Ok("1")
        || !io::stdin().is_terminal()
        || !io::stderr().is_terminal()
        || !(args.is_empty() || args == ["interactive"])
    {
        return;
    }
    let Ok(Some(root)) = managed_root(executable) else {
        return;
    };
    if let (Ok(cache), Ok(state)) = (
        json_file(&root.join("cache.json")),
        json_file(&root.join("state.json")),
    ) && let Some(notice) = cached_notice(&cache, &state, chrono::Utc::now())
    {
        eprintln!("{notice}");
    }
    let Ok(exe) = executable.canonicalize() else {
        return;
    };
    let sibling = exe.parent().unwrap().join("moi-cli");
    // A bounded, short-lived anonymous checker, not a resident daemon. Reap it
    // off the UI thread. The Go owner applies the shared 24-hour cache and lock.
    let _ = std::thread::Builder::new()
        .name("moi-update-check".into())
        .spawn(move || {
            let _ = Command::new(sibling)
                .args(["update", "refresh"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        });
}

fn cached_notice(
    cache: &Value,
    state: &Value,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let channel = &cache["channel"];
    let expires = chrono::DateTime::parse_from_rfc3339(channel["expires_at"].as_str()?).ok()?;
    let bundle = channel["bundle_version"].as_str()?;
    // Only display a validated identifier, never arbitrary cached terminal text.
    if expires <= now
        || channel["manifest_sha256"] == state["current"]
        || bundle.is_empty()
        || bundle.len() > 80
        || !bundle.as_bytes()[0].is_ascii_alphanumeric()
        || !bundle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return None;
    }
    if cache["reason"].as_str() == Some("CLI_UPDATE_COMPATIBILITY_CHANGE") {
        return Some(format!(
            "MOI new release: {bundle}. This release requires a separate installation. Use the official installer with a new --dir and --skill-dir; keep the current installation and login data."
        ));
    }
    (cache["available"].as_bool() == Some(true))
        .then(|| format!("MOI update available: {bundle}. Run astra update."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn cached_notice_distinguishes_separate_install_and_filters_unsafe_text() {
        let now = chrono::Utc::now();
        let mut cache = serde_json::json!({
            "available": false,
            "reason": "CLI_UPDATE_COMPATIBILITY_CHANGE",
            "channel": {
                "bundle_version": "bundle-2",
                "manifest_sha256": "b".repeat(64),
                "expires_at": (now + chrono::Duration::hours(1)).to_rfc3339(),
            }
        });
        let state = serde_json::json!({"current": "a".repeat(64)});
        let notice = cached_notice(&cache, &state, now).unwrap();
        assert!(notice.contains("separate installation"));
        assert!(notice.contains("--skill-dir"));
        assert!(!notice.contains("Run astra update."));
        cache["reason"] = serde_json::json!("UNKNOWN");
        assert!(cached_notice(&cache, &state, now).is_none());
        cache["available"] = serde_json::json!(true);
        assert!(
            cached_notice(&cache, &state, now)
                .unwrap()
                .contains("Run astra update.")
        );
        cache["channel"]["bundle_version"] = serde_json::json!("bad\u{1b}[31m");
        assert!(cached_notice(&cache, &state, now).is_none());
        cache["channel"]["bundle_version"] = serde_json::json!("bundle-2");
        assert!(cached_notice(&cache, &state, now + chrono::Duration::hours(2)).is_none());
        cache["channel"]["manifest_sha256"] = state["current"].clone();
        assert!(cached_notice(&cache, &state, now).is_none());
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let release = root.join("releases").join("a".repeat(64));
        fs::create_dir_all(&release).unwrap();
        let exe = release.join("astra");
        fs::write(&exe, b"test").unwrap();
        fs::write(
            root.join("installation.json"),
            format!(r#"{{"schema":"{PROTOCOL}"}}"#),
        )
        .unwrap();
        File::create(root.join("runtime.lock")).unwrap();
        symlink(&release, root.join("current")).unwrap();
        (temp, exe)
    }
    #[test]
    fn managed_lease_blocks_switch_and_releases_on_drop() {
        let (temp, exe) = fixture();
        let lease = acquire(&exe).unwrap().unwrap();
        let exclusive = OpenOptions::new()
            .read(true)
            .write(true)
            .open(temp.path().join("runtime.lock"))
            .unwrap();
        assert!(exclusive.try_lock_exclusive().is_err());
        drop(lease);
        exclusive.try_lock_exclusive().unwrap();
        assert!(acquire(&exe).unwrap_err().to_string().contains("BUSY"));
    }
    #[test]
    fn interrupted_switch_and_old_executable_fail_closed() {
        let (temp, exe) = fixture();
        fs::write(temp.path().join("transaction.json"), b"{}").unwrap();
        assert!(
            acquire(&exe)
                .unwrap_err()
                .to_string()
                .contains("RECOVERY_REQUIRED")
        );
        fs::remove_file(temp.path().join("transaction.json")).unwrap();
        fs::remove_file(temp.path().join("current")).unwrap();
        symlink(temp.path(), temp.path().join("current")).unwrap();
        assert!(
            acquire(&exe)
                .unwrap_err()
                .to_string()
                .contains("RESTART_REQUIRED")
        );
    }
    #[test]
    fn standalone_installation_is_not_claimed() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("astra");
        fs::write(&exe, b"test").unwrap();
        assert!(acquire(&exe).unwrap().is_none());
        assert!(
            early_command(&exe, &["update".into()], true)
                .unwrap_err()
                .to_string()
                .contains("NOT_MANAGED")
        );
    }
}
