use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresignedArtifactDownload {
    pub artifact_id: String,
    pub download_url: String,
    pub expires_at: String,
    pub signature: String,
    pub method: String,
}

pub fn build_presigned_artifact_download(
    base_path: &str,
    user_id: &str,
    session_id: &str,
    artifact_id: &str,
    secret: &str,
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> PresignedArtifactDownload {
    let expires = now + Duration::seconds(ttl_seconds.max(1));
    let expires_epoch = expires.timestamp();
    let signature =
        artifact_download_signature(user_id, session_id, artifact_id, expires_epoch, secret);
    let separator = if base_path.contains('?') { '&' } else { '?' };
    let download_url =
        format!("{base_path}{separator}expires_at={expires_epoch}&signature={signature}");
    PresignedArtifactDownload {
        artifact_id: artifact_id.to_string(),
        download_url,
        expires_at: expires.to_rfc3339(),
        signature,
        method: "GET".to_string(),
    }
}

pub fn artifact_download_signature(
    user_id: &str,
    session_id: &str,
    artifact_id: &str,
    expires_epoch: i64,
    secret: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update(b"|");
    hasher.update(session_id.as_bytes());
    hasher.update(b"|");
    hasher.update(artifact_id.as_bytes());
    hasher.update(b"|");
    hasher.update(expires_epoch.to_string().as_bytes());
    hasher.update(b"|");
    hasher.update(secret.as_bytes());
    format!("sha256:{:x}", hasher.finalize())
}
