use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum BackgroundTaskError {
    #[error("background task '{task_id}' was not found")]
    NotFound { task_id: String },
    #[error("background task '{task_id}' already terminated")]
    AlreadyTerminated { task_id: String },
    #[error("background task '{task_id}' has a stale handle")]
    StaleHandle { task_id: String },
    #[error("background task '{task_id}' cannot be stopped")]
    CannotStop { task_id: String },
    #[error("output artifact missing for background task '{task_id}': {path}")]
    OutputArtifactMissing { task_id: String, path: PathBuf },
    #[error("background task '{task_id}' output unavailable: {detail}")]
    OutputUnavailable { task_id: String, detail: String },
}

impl BackgroundTaskError {
    pub(crate) fn not_found(task_id: &str) -> Self {
        Self::NotFound {
            task_id: task_id.to_string(),
        }
    }

    pub(crate) fn output_artifact_missing(task_id: &str, path: &Path) -> Self {
        Self::OutputArtifactMissing {
            task_id: task_id.to_string(),
            path: path.to_path_buf(),
        }
    }

    pub(crate) fn output_unavailable(task_id: &str, detail: impl Into<String>) -> Self {
        Self::OutputUnavailable {
            task_id: task_id.to_string(),
            detail: detail.into(),
        }
    }
}
