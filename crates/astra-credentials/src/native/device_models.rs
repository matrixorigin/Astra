//! NativeStore owns local device-source consent separately from provider
//! credentials and shared model definitions. The existing auth lock serializes
//! enable/fence with logout, login and workspace changes. No auth.json format
//! change is needed; an old binary still reads the same native session.
use super::*;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeviceConsent {
    version: u32,
    context: String,
    connections: BTreeMap<uuid::Uuid, u64>,
}

fn context(session: &NativeSession) -> Result<String, String> {
    let bytes = serde_json::to_vec(&(
        "astra-native-device-consent-v1",
        session.environment.key(),
        &session.generation,
        &session.session_id,
        &session.subject,
        &session.astra_user_id,
        &session.moi_principal_id,
        &session.workspace_id,
        &session.role_id,
    ))
    .map_err(|_| "cannot encode device model consent")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

impl NativeStore {
    pub(super) fn clear_device_consent_locked(&self) -> Result<(), String> {
        match fs::remove_file(self.root.join("auth-device-models.json")) {
            Ok(()) => sync_auth_directory(&self.root)
                .map_err(|_| "cannot sync cleared device model consent".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("cannot clear device model consent".into()),
        }
    }

    fn read_device_consent(&self) -> Result<Option<DeviceConsent>, String> {
        let path = self.root.join("auth-device-models.json");
        if fs::symlink_metadata(&path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        private_open(&path, false)?
            .take(16_385)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read device model consent")?;
        if bytes.len() > 16_384 {
            return Err("device model consent exceeds its limit".into());
        }
        let value: DeviceConsent =
            serde_json::from_slice(&bytes).map_err(|_| "invalid device model consent")?;
        if value.version != 1
            || value.context.len() != 64
            || value.connections.len() > 16
            || value.connections.values().any(|revision| *revision == 0)
        {
            return Err("invalid device model consent".into());
        }
        Ok(Some(value))
    }

    /// Presentation/publication only: token rotation suspends execution, not
    /// the user choice. A dispatch must still obtain device_connection_lease.
    pub fn device_connection_selected(
        &self,
        expected: &NativeSession,
        id: uuid::Uuid,
        revision: u64,
    ) -> Result<bool, String> {
        self.check_dir()?;
        let lock = private_open(&self.root.join("auth.lock"), true)?;
        FileExt::lock_shared(&lock).map_err(|_| "cannot lock MOI credentials")?;
        validate_device_identity(&self.read_locked()?, expected)?;
        Ok(self.read_device_consent()?.is_some_and(|value| {
            context(expected).is_ok_and(|key| key == value.context)
                && value.connections.get(&id) == Some(&revision)
        }))
    }

    pub fn device_connection_enabled(
        &self,
        expected: &NativeSession,
        id: uuid::Uuid,
        revision: u64,
    ) -> Result<bool, String> {
        let _lease = self.device_model_lease(expected)?;
        Ok(self.read_device_consent()?.is_some_and(|value| {
            context(expected).is_ok_and(|key| key == value.context)
                && value.connections.get(&id) == Some(&revision)
        }))
    }

    /// Called only for an explicit model/source choice. A new login or workspace
    /// has a different context and cannot inherit a previous consent record.
    pub fn enable_device_connection(
        &self,
        expected: &NativeSession,
        id: uuid::Uuid,
        revision: u64,
    ) -> Result<Committed<()>, String> {
        if revision == 0 || expected.workspace_id.is_none() {
            return Err("select a MOI workspace and a valid ChatGPT connection".into());
        }
        self.check_dir()?;
        let lock = private_open(&self.root.join("auth.lock"), true)?;
        FileExt::lock_exclusive(&lock).map_err(|_| "cannot lock MOI credentials")?;
        validate_device_session(&self.read_locked()?, expected)?;
        let key = context(expected)?;
        let mut value = self
            .read_device_consent()?
            .filter(|value| value.context == key)
            .unwrap_or(DeviceConsent {
                version: 1,
                context: key,
                connections: BTreeMap::new(),
            });
        value.connections.insert(id, revision);
        if value.connections.len() > 16 {
            return Err("too many device ChatGPT connections".into());
        }
        let bytes = serde_json::to_vec(&value).map_err(|_| "cannot encode device model consent")?;
        let tmp = self
            .root
            .join(format!("auth-device-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = private_create(&tmp)?;
            file.write_all(&bytes)
                .map_err(|_| "cannot write device model consent")?;
            file.sync_all()
                .map_err(|_| "cannot sync device model consent")?;
            fs::rename(&tmp, self.root.join("auth-device-models.json"))
                .map_err(|_| "cannot publish device model consent")?;
            Ok(Committed {
                value: (),
                directory_synced: sync_auth_directory(&self.root).is_ok(),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }

    pub fn device_connection_lease(
        &self,
        expected: &NativeSession,
        id: uuid::Uuid,
        revision: u64,
    ) -> Result<NativeSessionLease, String> {
        let lease = self.device_model_lease(expected)?;
        let consent = self
            .read_device_consent()?
            .ok_or("select this ChatGPT model again from /model")?;
        if consent.context != context(expected)? || consent.connections.get(&id) != Some(&revision)
        {
            return Err("device ChatGPT is not enabled for this MOI login and workspace".into());
        }
        Ok(lease)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::native::tests::{session, store};

    fn signed_in(store: &NativeStore) -> NativeSession {
        let current = store.publish(session("same-owner")).unwrap().0;
        store
            .select_workspace(&current, Some("workspace-a"))
            .unwrap();
        store.current().unwrap()
    }

    #[test]
    fn consent_is_explicit_login_workspace_and_connection_revision_scoped() {
        let (directory, store) = store();
        let current = signed_in(&store);
        let id = uuid::Uuid::new_v4();
        let original_auth = fs::read(directory.path().join("auth.json")).unwrap();
        assert!(!store.device_connection_enabled(&current, id, 1).unwrap());
        store.enable_device_connection(&current, id, 1).unwrap();
        assert!(store.device_connection_enabled(&current, id, 1).unwrap());
        assert!(!store.device_connection_enabled(&current, id, 2).unwrap());
        assert_eq!(
            fs::read(directory.path().join("auth.json")).unwrap(),
            original_auth
        );
        let lease = store.device_connection_lease(&current, id, 1).unwrap();
        let writer = private_open(&directory.path().join("auth.lock"), true).unwrap();
        assert_eq!(
            FileExt::try_lock_exclusive(&writer).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(lease);
        store
            .select_workspace(&current, Some("workspace-b"))
            .unwrap();
        let changed = store.current().unwrap();
        assert!(!store.device_connection_enabled(&changed, id, 1).unwrap());
        assert!(store.enable_device_connection(&current, id, 1).is_err());
        store.enable_device_connection(&changed, id, 1).unwrap();
        store.logout().unwrap();
        assert!(store.device_connection_lease(&changed, id, 1).is_err());
        let next = signed_in(&store);
        assert!(!store.device_connection_enabled(&next, id, 1).unwrap());
    }

    #[test]
    fn workspace_roundtrip_requires_a_new_explicit_device_choice() {
        let (_directory, store) = store();
        let a = signed_in(&store);
        let id = uuid::Uuid::new_v4();
        store.enable_device_connection(&a, id, 1).unwrap();
        store.select_workspace(&a, Some("workspace-b")).unwrap();
        let b = store.current().unwrap();
        store.select_workspace(&b, Some("workspace-a")).unwrap();
        let returned = store.current().unwrap();
        assert!(!store.device_connection_selected(&returned, id, 1).unwrap());
        assert!(!store.device_connection_enabled(&returned, id, 1).unwrap());
        store.enable_device_connection(&returned, id, 1).unwrap();
        assert!(store.device_connection_lease(&returned, id, 1).is_ok());
    }

    #[test]
    fn independent_native_sessions_do_not_revoke_each_others_device_consent() {
        let (_one, first) = store();
        let (_two, second) = store();
        let a = signed_in(&first);
        let b = signed_in(&second);
        let id = uuid::Uuid::new_v4();
        first.enable_device_connection(&a, id, 1).unwrap();
        second.enable_device_connection(&b, id, 1).unwrap();
        first.logout().unwrap();
        assert!(second.device_connection_lease(&b, id, 1).is_ok());
        second
            .update_with_control(&b, None, |current| {
                current.access_token = "synthetic-rotated-platform-token".into();
                Ok(())
            })
            .unwrap();
        assert!(second.device_connection_lease(&b, id, 1).is_ok());
    }

    #[test]
    fn attachment_identity_survives_rotation_but_not_relogin_workspace_or_role_change() {
        use crate::{LocalModelScope, native_device::NativeDeviceAttachment};
        let (_directory, store) = store();
        let current = signed_in(&store);
        let id = uuid::Uuid::new_v4();
        store.enable_device_connection(&current, id, 1).unwrap();
        let attachment = NativeDeviceAttachment::new(&store, &current).unwrap();
        let scope = LocalModelScope::for_native_session(&current).unwrap();
        let key = attachment.binding_key();
        store
            .update_with_control(&current, None, |session| {
                session.access_token = "synthetic-rotated-access".into();
                session.refresh_pending = true;
                Ok(())
            })
            .unwrap();
        // Namespace/liveness do not revoke a terminal during token settlement.
        assert!(attachment.matches_scope(&scope));
        let (_, pending) = attachment.resolve().unwrap();
        assert!(store.device_model_lease(&pending).is_err());
        assert!(store.device_connection_selected(&pending, id, 1).unwrap());
        assert!(store.device_connection_enabled(&pending, id, 1).is_err());
        assert_eq!(attachment.binding_key(), key);
        store
            .update_with_control(&current, None, |session| {
                session.refresh_pending = false;
                session.role_id = Some("another-role".into());
                Ok(())
            })
            .unwrap();
        assert!(!attachment.matches_scope(&scope));
        assert!(attachment.resolve().is_err());
        store.logout().unwrap();
        let next = signed_in(&store);
        assert_eq!(
            LocalModelScope::for_native_session(&next)
                .unwrap()
                .identity(),
            scope.identity()
        );
        assert!(attachment.resolve().is_err());
    }

    #[test]
    fn insecure_or_corrupt_consent_cannot_authorize_a_fence() {
        use std::os::unix::fs::PermissionsExt;
        let (directory, store) = store();
        let current = signed_in(&store);
        let id = uuid::Uuid::new_v4();
        store.enable_device_connection(&current, id, 1).unwrap();
        let path = directory.path().join("auth-device-models.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.device_connection_lease(&current, id, 1).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&path, b"invalid private record").unwrap();
        assert!(store.device_connection_enabled(&current, id, 1).is_err());
        assert!(store.enable_device_connection(&current, id, 1).is_err());
    }
}
