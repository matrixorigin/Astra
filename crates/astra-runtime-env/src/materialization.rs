//! Stable identity for a physical local checkout.
//!
//! The CLI and standalone Edge must publish the same materialization identity:
//! it is tied to a persisted device identity and the canonical checkout, not a
//! process id or a cache root.

use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod volume;

fn canonical_workspace_dir(workspace_dir: &Path) -> Result<PathBuf, String> {
    workspace_dir.canonicalize().map_err(|error| {
        format!(
            "failed to canonicalize workspace directory '{}': {error}",
            workspace_dir.display()
        )
    })
}

const MATERIALIZATION_ID_DIRECTORY: &str = "edge-materializations";
const MATERIALIZATION_ID_FILE_SUFFIX: &str = ".id";
const MATERIALIZATION_ID_MAX_BYTES: u64 = 128;

pub fn materialization_id_path_in_state(workspace_dir: &Path, state_root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(workspace_dir.to_string_lossy().as_bytes());
    let workspace_key = format!("{:x}", hasher.finalize());
    state_root
        .join(MATERIALIZATION_ID_DIRECTORY)
        .join(format!("{workspace_key}{MATERIALIZATION_ID_FILE_SUFFIX}"))
}

/// Return the state file used for the stable device identity. This path is
/// deliberately rooted in Astra's default local state directory; an
/// `ASTRA_STATE_ROOT` override may isolate caches, but it must not turn one
/// physical device into multiple materialization identities.
fn device_identity_path(state_root: &Path) -> PathBuf {
    state_root
        .join(MATERIALIZATION_ID_DIRECTORY)
        .join("device.id")
}

/// Publish one identity file without ever exposing a partial value. A synced
/// temporary file is hard-linked into place, which gives us no-replace
/// semantics on the same filesystem. The caller may provide an expected value
/// to bind a cached identity to an independently persisted physical identity.
fn load_or_create_identity_file(
    path: &Path,
    expected: Option<(&str, &str)>,
    create_value: impl Fn() -> String,
    description: &str,
) -> Result<String, String> {
    let directory = path
        .parent()
        .ok_or_else(|| format!("{description} has no parent directory"))?;
    fs::create_dir_all(directory).map_err(|error| {
        format!(
            "failed to create Astra identity directory '{}': {error}",
            directory.display()
        )
    })?;
    sync_identity_directory(directory, description)?;
    if let Some(parent) = directory.parent() {
        sync_identity_directory(parent, description)?;
    }
    for _ in 0..3 {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if !metadata.file_type().is_file() || metadata.len() > MATERIALIZATION_ID_MAX_BYTES
                {
                    return Err(format!(
                        "{description} '{}' is not a regular bounded file",
                        path.display()
                    ));
                }
                let value = fs::read_to_string(path).map_err(|error| {
                    format!("failed to read {description} '{}': {error}", path.display())
                })?;
                let value = value.trim();
                if !crate::is_valid_provider_id(value) {
                    return Err(format!("{description} '{}' is invalid", path.display()));
                }
                if let Some((expected, hint)) = expected
                    && expected != value
                {
                    return Err(format!(
                        "{description} '{}' does not match its canonical identity. {hint}",
                        path.display()
                    ));
                }
                return Ok(value.to_owned());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Never create the final path before the bytes are complete.
                // If the process crashes, only an unreferenced temporary file
                // remains and a later process can safely publish a new one.
                let temporary = path.with_file_name(format!(
                    ".{}-tmp-{}",
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("identity"),
                    uuid::Uuid::new_v4()
                ));
                let value = expected.map_or_else(&create_value, |(value, _)| value.to_owned());
                let publish = (|| -> Result<bool, String> {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&temporary)
                        .map_err(|error| {
                            format!(
                                "failed to create {description} temporary file '{}': {error}",
                                temporary.display()
                            )
                        })?;
                    file.write_all(value.as_bytes()).map_err(|error| {
                        format!(
                            "failed to write {description} temporary file '{}': {error}",
                            temporary.display()
                        )
                    })?;
                    file.sync_all().map_err(|error| {
                        format!(
                            "failed to persist {description} temporary file '{}': {error}",
                            temporary.display()
                        )
                    })?;
                    match fs::hard_link(&temporary, path) {
                        Ok(()) => {
                            sync_identity_directory(directory, description)?;
                            Ok(true)
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            Ok(false)
                        }
                        Err(error) => Err(format!(
                            "failed to publish {description} '{}': {error}",
                            path.display()
                        )),
                    }
                })();
                let _ = fs::remove_file(&temporary);
                match publish {
                    Ok(true) => return Ok(value),
                    Ok(false) => continue,
                    Err(error) => return Err(error),
                }
            }
            Err(error) => {
                return Err(format!(
                    "failed to inspect {description} '{}': {error}",
                    path.display()
                ));
            }
        }
    }
    Err(format!(
        "{description} '{}' changed during initialization",
        path.display()
    ))
}

fn sync_identity_directory(directory: &Path, description: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        let handle = OpenOptions::new()
            .read(true)
            .open(directory)
            .map_err(|error| {
                format!(
                    "failed to open {description} directory '{}' for sync: {error}",
                    directory.display()
                )
            })?;
        handle.sync_all().map_err(|error| {
            format!(
                "failed to persist {description} directory '{}': {error}",
                directory.display()
            )
        })?;
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, description);
    }
    Ok(())
}

fn load_or_create_device_identity(state_root: &Path) -> Result<String, String> {
    load_or_create_identity_file(
        &device_identity_path(state_root),
        None,
        || format!("device-{}", uuid::Uuid::new_v4()),
        "Edge device identity",
    )
}

#[cfg(unix)]
fn checkout_binding(identity: &[u8], filesystem_uuid: &[u8], inode: u64) -> String {
    let mut binding = Sha256::new();
    binding.update(identity);
    binding.update([0]);
    binding.update(filesystem_uuid);
    binding.update([0]);
    binding.update(inode.to_le_bytes());
    format!("checkout-{:x}", binding.finalize())
}

/// Seed a new checkout identity from the device and canonical directory.
/// Keep the pre-UUID seed readable by older clients. With a UUID, device/inode
/// values only seed the ID and the separate binding
/// excludes the volatile mount number. Without the Linux UUID API, the original
/// device/inode seed remains the expected value on every start.
struct CheckoutIdentity {
    seed: String,
    // None means the Linux UUID API is unavailable. Keep the original strict
    // device/inode identity comparison; do not publish an inode-only witness.
    binding: Option<String>,
}

fn materialization_identity_for_workspace(
    workspace_dir: &Path,
    device_id: &str,
) -> Result<CheckoutIdentity, String> {
    let workspace = canonical_workspace_dir(workspace_dir)?;
    #[cfg(unix)]
    let directory = fs::File::open(&workspace).map_err(|error| {
        format!(
            "failed to inspect canonical workspace directory '{}': {error}",
            workspace.display()
        )
    })?;
    #[cfg(unix)]
    let metadata = directory
        .metadata()
        .map_err(|error| format!("failed to inspect open workspace directory: {error}"))?;
    #[cfg(not(unix))]
    let metadata = fs::metadata(&workspace)
        .map_err(|error| format!("failed to inspect workspace directory: {error}"))?;
    if !metadata.is_dir() {
        return Err(format!(
            "canonical workspace path '{}' is not a directory",
            workspace.display()
        ));
    }
    let mut identity = device_id.as_bytes().to_vec();
    identity.push(0);
    identity.extend_from_slice(workspace.to_string_lossy().as_bytes());
    #[cfg(unix)]
    let binding = {
        use std::os::unix::fs::MetadataExt;

        let volume = match volume::filesystem_uuid(&directory) {
            Ok(uuid) => Some(uuid),
            Err(error) if volume::uuid_api_unavailable(&error) => None,
            Err(error) => {
                return Err(format!(
                    "Cannot establish a stable filesystem identity for workspace '{}': {error}. \
                 Use a filesystem and OS that expose a filesystem UUID \
                 (macOS ATTR_VOL_UUID or Linux FS_IOC_GETFSUUID); \
                 no existing identity files were replaced.",
                    workspace.display()
                ));
            }
        };
        let binding = volume
            .as_deref()
            .map(|uuid| checkout_binding(&identity, uuid, metadata.ino()));
        identity.push(0);
        identity.extend_from_slice(&metadata.dev().to_le_bytes());
        identity.push(0);
        identity.extend_from_slice(&metadata.ino().to_le_bytes());
        binding
    };
    #[cfg(not(unix))]
    let binding = Some(format!("checkout-{:x}", Sha256::digest(&identity)));
    let digest = Sha256::digest(identity);
    Ok(CheckoutIdentity {
        seed: format!("materialization-{digest:x}"),
        binding,
    })
}

pub fn load_or_create_materialization_id(workspace_dir: &Path) -> Result<String, String> {
    let cache_root = crate::local_state_root_override()
        .unwrap_or_else(astra_core::local_state::local_state_root);
    let device_root = astra_core::local_state::default_local_state_root();
    load_or_create_materialization_id_in_roots(workspace_dir, &cache_root, &device_root)
}

pub fn load_or_create_materialization_id_in_roots(
    workspace_dir: &Path,
    cache_root: &Path,
    device_root: &Path,
) -> Result<String, String> {
    load_or_create_materialization_id_with_resolver(
        workspace_dir,
        cache_root,
        device_root,
        materialization_identity_for_workspace,
    )
}

fn load_or_create_materialization_id_with_resolver(
    workspace_dir: &Path,
    cache_root: &Path,
    device_root: &Path,
    resolve_identity: impl FnOnce(&Path, &str) -> Result<CheckoutIdentity, String>,
) -> Result<String, String> {
    let device_id = load_or_create_device_identity(device_root)?;
    let workspace = canonical_workspace_dir(workspace_dir)?;
    let canonical_path = materialization_id_path_in_state(&workspace, device_root);
    let previous_path = materialization_id_path_in_state(workspace_dir, device_root);
    let CheckoutIdentity { seed, binding } = resolve_identity(&workspace, &device_id)?;
    let checkout_path = canonical_path.with_extension("checkout");
    let legacy_validation = if binding.is_none() {
        match fs::symlink_metadata(&checkout_path) {
            Ok(_) => {
                return Err(format!(
                    "Cannot verify the saved filesystem UUID binding '{}': the Linux UUID API is \
                 unavailable. Use an OS/filesystem that supports FS_IOC_GETFSUUID; \
                 the saved binding was not removed or downgraded. Keep existing identity files.",
                    checkout_path.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "failed to inspect workspace checkout binding: {error}"
                ));
            }
        }
        Some((
            seed.as_str(),
            "The Linux UUID API is unavailable, so this checkout must still match its original \
             device and inode. A changed mount or directory cannot be verified. Restore the \
             original checkout or use a UUID-capable OS/filesystem; do not clear login credentials.",
        ))
    } else {
        None
    };
    // Older clients keyed their default-state file by the caller's path, even
    // though the ID used its canonical root. An alias is only a mirror once
    // the canonical owner exists; without one, it must prove the old seed.
    let previous_exists = previous_path != canonical_path
        && match fs::symlink_metadata(&previous_path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => {
                return Err(format!(
                    "failed to inspect workspace materialization identity: {error}"
                ));
            }
        };
    let alias_conflict_hint = "This path alias conflicts with the default-state owner. Stop its Astra/astra-edge \
         processes and back up/remove only the conflicting alias .id file before retrying; \
         keep the canonical .id/.checkout, device.id and login credentials.";
    let existing_canonical_id = if previous_exists {
        match fs::symlink_metadata(&canonical_path) {
            Ok(_) => Some(load_or_create_identity_file(
                &canonical_path,
                legacy_validation,
                || seed.clone(),
                "workspace materialization identity",
            )?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "failed to inspect canonical workspace identity: {error}"
                ));
            }
        }
    } else {
        None
    };
    let previous_id = if previous_exists {
        let expected = existing_canonical_id
            .as_deref()
            .map(|id| (id, alias_conflict_hint))
            .unwrap_or((
                seed.as_str(),
                "This alias cannot prove that its saved identity belongs to the current checkout. \
                 The link target, mount, or directory may have changed. Restore the original \
                 checkout or stop its Astra/astra-edge processes and back up/remove only the \
                 unverified alias .id before registering the current target again. \
                 Keep device.id and login credentials; no saved identities were replaced.",
            ));
        Some(load_or_create_identity_file(
            &previous_path,
            Some(expected),
            || seed.clone(),
            "workspace materialization identity",
        )?)
    } else {
        None
    };
    // With UUID evidence, bind a legacy .id's current checkout before publishing
    // a new owner. Without it, retain exact legacy validation and do not publish
    // a weaker inode-only witness. Both paths use the same atomic file owner.
    if let Some(checkout_binding) = binding {
        load_or_create_identity_file(
            &checkout_path,
            Some((
                &checkout_binding,
                &format!(
                    "The checkout, filesystem, or local device identity differs from its saved binding. \
                 If you intentionally replaced this checkout, stop its Astra/astra-edge processes, \
                 back up and remove only '{}' and '{}', plus this checkout's alias/cache .id mirrors, \
                 then register it again. Keep device.id and login credentials.",
                    canonical_path.display(),
                    canonical_path.with_extension("checkout").display()
                ),
            )),
            || checkout_binding.clone(),
            "workspace checkout binding",
        )?;
    }
    let canonical_id = match existing_canonical_id {
        Some(id) => id,
        None => load_or_create_identity_file(
            &canonical_path,
            legacy_validation,
            || previous_id.as_ref().unwrap_or(&seed).clone(),
            "workspace materialization identity",
        )?,
    };
    if previous_id.is_some() {
        load_or_create_identity_file(
            &previous_path,
            Some((&canonical_id, alias_conflict_hint)),
            || canonical_id.clone(),
            "workspace materialization identity",
        )?;
    }
    // Isolated state roots contain mirrors, not independent identity owners.
    // A conflicting mirror still fails closed and is never overwritten.
    load_or_create_identity_file(
        &materialization_id_path_in_state(workspace_dir, cache_root),
        Some((
            &canonical_id,
            "This cache is not the identity owner and will not be adopted. Stop its Astra/astra-edge \
             processes and back up/remove only this conflicting cache .id file before retrying; \
             keep the default-state .id/.checkout, device.id and login credentials.",
        )),
        || canonical_id.clone(),
        "workspace materialization identity",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn legacy_checkout_identity(
        workspace: &Path,
        device: &str,
    ) -> Result<CheckoutIdentity, String> {
        use std::os::unix::fs::MetadataExt;

        let metadata = workspace.metadata().map_err(|error| error.to_string())?;
        let mut bytes = device.as_bytes().to_vec();
        bytes.push(0);
        bytes.extend_from_slice(workspace.to_string_lossy().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&metadata.dev().to_le_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&metadata.ino().to_le_bytes());
        Ok(CheckoutIdentity {
            seed: format!("materialization-{:x}", Sha256::digest(bytes)),
            binding: None,
        })
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_uuid_api_preserves_unchanged_legacy_identity_bytes() {
        let device = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let device_id = load_or_create_device_identity(device.path()).unwrap();
        let legacy = legacy_checkout_identity(&workspace, &device_id)
            .unwrap()
            .seed;
        let owner = materialization_id_path_in_state(&workspace, device.path());
        let original_bytes = format!("{legacy}\n").into_bytes();
        fs::write(&owner, &original_bytes).unwrap();
        for _ in 0..2 {
            assert_eq!(
                load_or_create_materialization_id_with_resolver(
                    &workspace,
                    cache.path(),
                    device.path(),
                    legacy_checkout_identity,
                )
                .unwrap(),
                legacy
            );
            assert_eq!(fs::read(&owner).unwrap(), original_bytes);
            assert!(!owner.with_extension("checkout").exists());
        }
        // Once a UUID becomes available, the same legacy owner can acquire a
        // stable witness without rotating the ID or rewriting its bytes.
        assert_eq!(
            load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
                .unwrap(),
            legacy
        );
        assert_eq!(fs::read(&owner).unwrap(), original_bytes);
        assert!(owner.with_extension("checkout").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_uuid_api_can_register_a_new_checkout_without_an_inode_only_witness() {
        let device = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        let first = load_or_create_materialization_id_with_resolver(
            &workspace,
            cache.path(),
            device.path(),
            legacy_checkout_identity,
        )
        .unwrap();
        assert_eq!(
            load_or_create_materialization_id_with_resolver(
                &workspace,
                cache.path(),
                device.path(),
                legacy_checkout_identity,
            )
            .unwrap(),
            first
        );
        let owner = materialization_id_path_in_state(&workspace, device.path());
        assert_eq!(fs::read_to_string(&owner).unwrap(), first);
        assert!(!owner.with_extension("checkout").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_uuid_api_rejects_changed_device_or_inode_without_rewriting_state() {
        use std::os::unix::fs::MetadataExt;

        let device = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("checkout");
        fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let original = load_or_create_materialization_id_with_resolver(
            &workspace,
            cache.path(),
            device.path(),
            legacy_checkout_identity,
        )
        .unwrap();
        let owner = materialization_id_path_in_state(&workspace, device.path());
        // Model a changed mount number with all other facts held fixed.
        let changed_device = |workspace: &Path, device: &str| {
            let mut identity = legacy_checkout_identity(workspace, device)?;
            let metadata = workspace.metadata().unwrap();
            let mut bytes = device.as_bytes().to_vec();
            bytes.push(0);
            bytes.extend_from_slice(workspace.to_string_lossy().as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&(metadata.dev() ^ 1).to_le_bytes());
            bytes.push(0);
            bytes.extend_from_slice(&metadata.ino().to_le_bytes());
            identity.seed = format!("materialization-{:x}", Sha256::digest(bytes));
            Ok(identity)
        };
        let error = load_or_create_materialization_id_with_resolver(
            &workspace,
            cache.path(),
            device.path(),
            changed_device,
        )
        .unwrap_err();
        assert!(error.contains("must still match its original device and inode"));
        assert_eq!(fs::read_to_string(&owner).unwrap(), original);
        let old_inode = workspace.metadata().unwrap().ino();
        fs::rename(&workspace, parent.path().join("previous-checkout")).unwrap();
        fs::create_dir(&workspace).unwrap();
        assert_ne!(workspace.metadata().unwrap().ino(), old_inode);
        assert!(
            load_or_create_materialization_id_with_resolver(
                &workspace,
                cache.path(),
                device.path(),
                legacy_checkout_identity,
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&owner).unwrap(), original);
        assert!(!owner.with_extension("checkout").exists());
    }

    #[cfg(unix)]
    #[test]
    fn unavailable_uuid_api_never_discards_a_published_uuid_witness() {
        let device = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let workspace = workspace.path().canonicalize().unwrap();
        load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
            .unwrap();
        let owner = materialization_id_path_in_state(&workspace, device.path());
        let original = fs::read(&owner).unwrap();
        let witness = fs::read(owner.with_extension("checkout")).unwrap();
        let error = load_or_create_materialization_id_with_resolver(
            &workspace,
            cache.path(),
            device.path(),
            legacy_checkout_identity,
        )
        .unwrap_err();
        assert!(error.contains("saved binding was not removed or downgraded"));
        assert_eq!(fs::read(&owner).unwrap(), original);
        assert_eq!(fs::read(owner.with_extension("checkout")).unwrap(), witness);
    }

    #[cfg(unix)]
    #[test]
    fn checkout_binding_distinguishes_volumes_with_the_same_inode() {
        let prefix = b"device-fixture\0/canonical/checkout";
        let first = checkout_binding(prefix, &[1; 16], 2);
        assert_eq!(first, checkout_binding(prefix, &[1; 16], 2));
        assert_ne!(first, checkout_binding(prefix, &[2; 16], 2));
        assert_ne!(first, checkout_binding(prefix, &[1; 16], 3));
    }

    #[test]
    fn identity_is_stable_across_cache_roots_and_distinct_per_checkout() {
        let state_a = tempfile::tempdir().expect("state A");
        let state_b = tempfile::tempdir().expect("state B");
        let device_state = tempfile::tempdir().expect("device state");
        let independent_device_state = tempfile::tempdir().expect("independent device state");
        let workspace = tempfile::tempdir().expect("workspace");
        let first = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_a.path(),
            device_state.path(),
        )
        .unwrap();
        let reconnect = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_a.path(),
            device_state.path(),
        )
        .unwrap();
        let other_state = load_or_create_materialization_id_in_roots(
            workspace.path(),
            state_b.path(),
            device_state.path(),
        )
        .unwrap();
        assert_eq!(first, reconnect);
        assert_eq!(first, other_state);
        let independent_device_state_root = tempfile::tempdir().expect("independent device cache");
        let same_path_independent_device = load_or_create_materialization_id_in_roots(
            workspace.path(),
            independent_device_state_root.path(),
            independent_device_state.path(),
        )
        .unwrap();
        assert_ne!(first, same_path_independent_device);
        let independent_checkout = tempfile::tempdir().expect("independent checkout");
        let independent_checkout_id = load_or_create_materialization_id_in_roots(
            independent_checkout.path(),
            state_b.path(),
            device_state.path(),
        )
        .unwrap();
        assert_ne!(first, independent_checkout_id);
        fs::write(workspace.path().join("content-change"), b"changed").unwrap();
        assert_eq!(
            first,
            load_or_create_materialization_id_in_roots(
                workspace.path(),
                state_a.path(),
                device_state.path()
            )
            .unwrap()
        );
        assert!(
            materialization_id_path_in_state(workspace.path(), state_a.path())
                .starts_with(state_a.path())
        );
    }

    #[test]
    fn identity_publication_converges_under_concurrent_startup() {
        let state = tempfile::tempdir().expect("state");
        let device_state = tempfile::tempdir().expect("device state");
        let workspace = tempfile::tempdir().expect("workspace");
        let state_root = std::sync::Arc::new(state.path().to_path_buf());
        let device_root = std::sync::Arc::new(device_state.path().to_path_buf());
        let workspace_root = std::sync::Arc::new(workspace.path().to_path_buf());
        let workers = (0..16)
            .map(|_| {
                let state_root = std::sync::Arc::clone(&state_root);
                let device_root = std::sync::Arc::clone(&device_root);
                let workspace_root = std::sync::Arc::clone(&workspace_root);
                std::thread::spawn(move || {
                    load_or_create_materialization_id_in_roots(
                        &workspace_root,
                        &state_root,
                        &device_root,
                    )
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let identities = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();
        assert!(identities.windows(2).all(|pair| pair[0] == pair[1]));
        let identity_path = materialization_id_path_in_state(workspace.path(), state.path());
        assert_eq!(fs::read_to_string(identity_path).unwrap(), identities[0]);
    }
}
