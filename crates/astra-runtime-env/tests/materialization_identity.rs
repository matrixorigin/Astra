use astra_runtime_env::{
    load_or_create_materialization_id_in_roots, materialization_id_path_in_state,
};
use sha2::{Digest, Sha256};
use std::fs;

#[cfg(unix)]
#[test]
fn persisted_identity_survives_a_filesystem_device_number_change() {
    use std::os::unix::fs::MetadataExt;

    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let device_id = "device-fixture";
    let metadata = workspace.metadata().unwrap();
    // Synthetic legacy writer: only the old mount number differs. Do not
    // mutate a real mount or use the developer's persisted identity files.
    let mut bytes = device_id.as_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(workspace.to_string_lossy().as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&(metadata.dev() ^ 1).to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    let legacy = format!("materialization-{:x}", Sha256::digest(bytes));
    let canonical_path = materialization_id_path_in_state(&workspace, device.path());
    fs::create_dir_all(canonical_path.parent().unwrap()).unwrap();
    fs::write(
        canonical_path.parent().unwrap().join("device.id"),
        device_id,
    )
    .unwrap();
    let legacy_bytes = format!("{legacy}\n").into_bytes();
    fs::write(&canonical_path, &legacy_bytes).unwrap();
    // An already published witness must also survive the mount-number change.
    let mut binding_bytes = device_id.as_bytes().to_vec();
    binding_bytes.push(0);
    binding_bytes.extend_from_slice(workspace.to_string_lossy().as_bytes());
    binding_bytes.push(0);
    binding_bytes.extend_from_slice(&metadata.ino().to_le_bytes());
    let binding = format!("checkout-{:x}", Sha256::digest(binding_bytes));
    let binding_path = canonical_path.with_extension("checkout");
    fs::write(&binding_path, &binding).unwrap();

    // CLI/default state and standalone Edge/isolated state use the same owner.
    assert_eq!(
        load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
            .unwrap(),
        legacy
    );
    assert_eq!(
        load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
            .unwrap(),
        legacy
    );
    assert_eq!(fs::read(canonical_path).unwrap(), legacy_bytes);
    assert_eq!(fs::read_to_string(binding_path).unwrap(), binding);
    assert_eq!(
        fs::read_to_string(materialization_id_path_in_state(&workspace, cache.path())).unwrap(),
        legacy
    );
}

#[test]
fn an_isolated_cache_cannot_replace_the_canonical_identity() {
    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let canonical =
        load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
            .unwrap();
    let mirror = materialization_id_path_in_state(&workspace, cache.path());
    fs::write(&mirror, "materialization-conflicting-cache").unwrap();

    let error = load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
        .unwrap_err();
    assert!(error.contains("does not match its canonical identity"));
    assert_eq!(
        fs::read_to_string(materialization_id_path_in_state(&workspace, device.path())).unwrap(),
        canonical
    );
    assert_eq!(
        fs::read_to_string(mirror).unwrap(),
        "materialization-conflicting-cache"
    );
}

#[test]
fn a_legacy_cache_without_a_canonical_owner_is_not_adopted() {
    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let mirror = materialization_id_path_in_state(&workspace, cache.path());
    fs::create_dir_all(mirror.parent().unwrap()).unwrap();
    let legacy_bytes = b"materialization-legacy-cache-only\n";
    fs::write(&mirror, legacy_bytes).unwrap();
    assert!(!materialization_id_path_in_state(&workspace, device.path()).exists());

    assert!(
        load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
            .unwrap_err()
            .contains("does not match its canonical identity")
    );
    assert_eq!(fs::read(mirror).unwrap(), legacy_bytes);
}

#[cfg(unix)]
#[test]
fn a_replacement_directory_at_the_same_path_cannot_reuse_the_identity() {
    use std::os::unix::fs::MetadataExt;

    for legacy in [None, Some("materialization-migrated-checkout\n")] {
        let device = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("checkout");
        fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let original_inode = workspace.metadata().unwrap().ino();
        let canonical_path = materialization_id_path_in_state(&workspace, device.path());
        if let Some(legacy) = legacy {
            fs::create_dir_all(canonical_path.parent().unwrap()).unwrap();
            fs::write(&canonical_path, legacy).unwrap();
        }
        let original =
            load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
                .unwrap();
        let original_bytes = fs::read(&canonical_path).unwrap();
        if let Some(legacy) = legacy {
            assert_eq!(original_bytes, legacy.as_bytes());
        }

        // Keep the original directory alive so inode reuse cannot hide replacement.
        fs::rename(&workspace, parent.path().join("previous-checkout")).unwrap();
        fs::create_dir(&workspace).unwrap();
        assert_ne!(workspace.metadata().unwrap().ino(), original_inode);
        assert!(
            load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
                .is_err()
        );
        assert_eq!(fs::read(canonical_path).unwrap(), original_bytes);
        assert_eq!(
            fs::read_to_string(materialization_id_path_in_state(&workspace, cache.path())).unwrap(),
            original
        );
    }
}

#[test]
fn malformed_canonical_identity_is_not_replaced() {
    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let canonical_path = materialization_id_path_in_state(&workspace, device.path());
    fs::create_dir_all(canonical_path.parent().unwrap()).unwrap();
    for invalid in ["invalid identity", &"a".repeat(129)] {
        fs::write(&canonical_path, invalid).unwrap();
        assert!(
            load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
                .is_err()
        );
        assert_eq!(fs::read_to_string(&canonical_path).unwrap(), invalid);
    }
}

#[test]
fn malformed_checkout_binding_is_not_replaced() {
    let device = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let original =
        load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
            .unwrap();
    let canonical_path = materialization_id_path_in_state(&workspace, device.path());
    let binding_path = canonical_path.with_extension("checkout");
    for invalid in ["invalid binding", &"a".repeat(129), "checkout-conflicting"] {
        fs::write(&binding_path, invalid).unwrap();
        assert!(
            load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
                .is_err()
        );
        assert_eq!(fs::read_to_string(&binding_path).unwrap(), invalid);
        assert_eq!(fs::read_to_string(&canonical_path).unwrap(), original);
    }
}

#[test]
fn an_interrupted_owner_publication_can_finish_for_the_same_checkout() {
    let device = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let original =
        load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
            .unwrap();
    let canonical_path = materialization_id_path_in_state(&workspace, device.path());
    let binding_path = canonical_path.with_extension("checkout");
    let binding = fs::read(&binding_path).unwrap();
    // Model a stop after publishing the witness but before publishing the ID.
    fs::remove_file(&canonical_path).unwrap();
    assert_eq!(
        load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
            .unwrap(),
        original
    );
    assert_eq!(fs::read(binding_path).unwrap(), binding);
}

#[cfg(unix)]
#[test]
fn symlinked_checkout_binding_is_rejected() {
    use std::os::unix::fs::symlink;

    let device = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let canonical_path = materialization_id_path_in_state(&workspace, device.path());
    fs::create_dir_all(canonical_path.parent().unwrap()).unwrap();
    let target = device.path().join("binding-target");
    fs::write(&target, "checkout-do-not-follow").unwrap();
    let binding_path = canonical_path.with_extension("checkout");
    symlink(&target, &binding_path).unwrap();
    assert!(
        load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
            .unwrap_err()
            .contains("not a regular bounded file")
    );
    assert_eq!(
        fs::read_to_string(target).unwrap(),
        "checkout-do-not-follow"
    );
    assert!(!canonical_path.exists());
}

#[cfg(unix)]
#[test]
fn aliases_share_the_canonical_owner_without_writing_the_checkout() {
    use std::os::unix::fs::symlink;

    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("checkout");
    symlink(workspace.path(), &alias).unwrap();
    let first =
        load_or_create_materialization_id_in_roots(&alias, cache.path(), device.path()).unwrap();
    assert_eq!(
        load_or_create_materialization_id_in_roots(workspace.path(), device.path(), device.path())
            .unwrap(),
        first
    );
    assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}

#[cfg(unix)]
#[test]
fn an_existing_default_state_alias_is_preserved_during_registration() {
    use std::os::unix::fs::symlink;

    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("checkout");
    symlink(workspace.path(), &alias).unwrap();
    let previous_path = materialization_id_path_in_state(&alias, device.path());
    fs::create_dir_all(previous_path.parent().unwrap()).unwrap();
    fs::write(&previous_path, "materialization-persisted-alias").unwrap();
    assert_eq!(
        load_or_create_materialization_id_in_roots(&alias, cache.path(), device.path()).unwrap(),
        "materialization-persisted-alias"
    );
    assert_eq!(
        load_or_create_materialization_id_in_roots(workspace.path(), cache.path(), device.path())
            .unwrap(),
        "materialization-persisted-alias"
    );
    assert_eq!(
        fs::read_to_string(previous_path).unwrap(),
        "materialization-persisted-alias"
    );
}

#[test]
fn concurrent_isolated_caches_share_one_durable_identity_owner() {
    let device = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let caches: Vec<_> = (0..8).map(|_| tempfile::tempdir().unwrap()).collect();
    let start = std::sync::Barrier::new(caches.len());
    let identities = std::thread::scope(|scope| {
        let workers: Vec<_> = caches
            .iter()
            .map(|cache| {
                let start = &start;
                let workspace = workspace.path();
                let device = device.path();
                scope.spawn(move || {
                    start.wait();
                    load_or_create_materialization_id_in_roots(workspace, cache.path(), device)
                        .unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    let canonical_path =
        materialization_id_path_in_state(&workspace.path().canonicalize().unwrap(), device.path());
    let canonical = fs::read_to_string(canonical_path).unwrap();
    for (cache, identity) in caches.iter().zip(identities) {
        assert_eq!(identity, canonical);
        assert_eq!(
            fs::read_to_string(materialization_id_path_in_state(
                workspace.path(),
                cache.path()
            ))
            .unwrap(),
            canonical
        );
    }
}

#[cfg(unix)]
#[test]
fn a_conflicting_default_state_alias_cannot_override_the_canonical_owner() {
    use std::os::unix::fs::symlink;

    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let canonical =
        load_or_create_materialization_id_in_roots(&workspace, device.path(), device.path())
            .unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("checkout");
    symlink(&workspace, &alias).unwrap();
    let previous_path = materialization_id_path_in_state(&alias, device.path());
    fs::write(&previous_path, "materialization-conflicting-alias").unwrap();
    assert!(
        load_or_create_materialization_id_in_roots(&alias, cache.path(), device.path())
            .unwrap_err()
            .contains("does not match its canonical identity")
    );
    assert_eq!(
        fs::read_to_string(materialization_id_path_in_state(&workspace, device.path())).unwrap(),
        canonical
    );
    assert_eq!(
        fs::read_to_string(previous_path).unwrap(),
        "materialization-conflicting-alias"
    );
}

#[cfg(unix)]
#[test]
fn symlinked_canonical_identity_is_rejected() {
    use std::os::unix::fs::symlink;

    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let canonical_path = materialization_id_path_in_state(&workspace, device.path());
    fs::create_dir_all(canonical_path.parent().unwrap()).unwrap();
    let target = device.path().join("not-an-identity-file");
    fs::write(&target, "materialization-do-not-follow").unwrap();
    symlink(&target, &canonical_path).unwrap();
    assert!(
        load_or_create_materialization_id_in_roots(&workspace, cache.path(), device.path())
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(target).unwrap(),
        "materialization-do-not-follow"
    );
}

#[cfg(unix)]
#[test]
fn a_dangling_default_state_alias_is_not_ignored() {
    use std::os::unix::fs::symlink;

    let device = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("checkout");
    symlink(workspace.path(), &alias).unwrap();
    let previous_path = materialization_id_path_in_state(&alias, device.path());
    fs::create_dir_all(previous_path.parent().unwrap()).unwrap();
    let missing = device.path().join("missing-target");
    symlink(&missing, &previous_path).unwrap();
    assert!(
        load_or_create_materialization_id_in_roots(&alias, cache.path(), device.path())
            .unwrap_err()
            .contains("not a regular bounded file")
    );
    assert_eq!(fs::read_link(previous_path).unwrap(), missing);
    assert!(!missing.exists());
}
