//! Test-only helpers that make subprocess fixtures deterministic.

#[cfg(unix)]
use std::io::{self, Write};
#[cfg(unix)]
use std::path::Path;

/// Publish an executable shell fixture only after its writer is closed.
///
/// Directly executing a just-written script is normally fine, but overlay
/// filesystems can transiently return `ETXTBSY` while they still observe the
/// inode as writable. Staging, flushing, chmodding, atomically renaming, and
/// flushing the mount ensures every harness test sees the same
/// executable-publish boundary.
#[cfg(unix)]
pub(crate) fn write_executable_shim(path: &Path, contents: impl AsRef<[u8]>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent)?;
    staged.write_all(contents.as_ref())?;
    staged.as_file().sync_all()?;

    let mut permissions = staged.as_file().metadata()?.permissions();
    permissions.set_mode(0o755);
    staged.as_file().set_permissions(permissions)?;

    let published = staged.persist(path).map_err(|error| error.error)?;
    drop(published);

    // `File::sync_all` flushes the staged inode; `sync` also flushes the
    // containing overlay's rename and mode metadata. This is test-only and
    // replaces several inconsistent, per-test workarounds.
    std::process::Command::new("sync").status().ok();
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_executable_shim;

    #[test]
    fn executable_shim_is_spawnable_after_atomic_publish() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shim = dir.path().join("shim");
        write_executable_shim(&shim, "#!/bin/sh\nprintf 'ready\\n'\n").expect("publish shim");

        let output = std::process::Command::new(shim)
            .output()
            .expect("spawn published shim");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ready\n");
    }
}
