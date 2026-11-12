//! A private attachment refers to NativeStore; it never transports a token.
//! Shared model/ChatGPT records remain deployment/user-scoped. Each terminal's
//! scheduling authority retains its own login, workspace and Role candidate.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    LocalModelScope,
    native::{NativeSession, NativeStore},
};

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativeDeviceAttachment {
    directory: PathBuf,
    environment: String,
    generation: String,
    session_id: String,
    workspace_id: String,
    role_candidate: Option<String>,
}

impl std::fmt::Debug for NativeDeviceAttachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeDeviceAttachment")
            .finish_non_exhaustive()
    }
}

impl NativeDeviceAttachment {
    pub fn new(store: &NativeStore, session: &NativeSession) -> Result<Self, String> {
        let value = Self {
            directory: store.directory().to_owned(),
            environment: session.environment.key(),
            generation: session.generation.clone(),
            session_id: session.session_id.clone(),
            workspace_id: session
                .workspace_id
                .clone()
                .ok_or("Select a MOI workspace before using a device model")?,
            role_candidate: session.role_id.clone(),
        };
        let (current_store, current) = value.resolve()?;
        let _lease = current_store.device_model_lease(&current)?;
        if LocalModelScope::for_native_session(session)?.identity()
            != LocalModelScope::for_native_session(&current)?.identity()
        {
            return Err("MOI device model owner changed".into());
        }
        Ok(value)
    }

    /// Re-read the authoritative owner on every publication and final fence.
    /// Values received over private IPC are references, not authentication.
    pub fn resolve(&self) -> Result<(NativeStore, NativeSession), String> {
        if !self.directory.is_absolute() || self.directory.as_os_str().len() > 4096 {
            return Err("Invalid MOI credential owner reference".into());
        }
        let store = NativeStore::with_directory(self.directory.clone());
        let session = store.current()?;
        if session.environment.key() != self.environment
            || session.generation != self.generation
            || session.session_id != self.session_id
            || session.workspace_id.as_deref() != Some(self.workspace_id.as_str())
            || session.role_id != self.role_candidate
        {
            return Err("MOI sign-in or workspace changed; reconnect the device model".into());
        }
        LocalModelScope::for_native_identity(&session)?;
        Ok((store, session))
    }

    pub fn matches_scope(&self, scope: &LocalModelScope) -> bool {
        self.resolve().is_ok_and(|(_, session)| {
            LocalModelScope::for_native_identity(&session)
                .is_ok_and(|current| current.identity() == scope.identity())
        })
    }

    pub fn binding_key(&self) -> String {
        // Independent of socket/process/terminal leases and access-token rotation.
        let bytes = serde_json::to_vec(&(
            "astra-native-device-binding-v1",
            &self.environment,
            &self.session_id,
            &self.workspace_id,
            &self.role_candidate,
        ))
        .expect("a tuple of strings is serializable");
        format!("{:x}", Sha256::digest(bytes))
    }
}
