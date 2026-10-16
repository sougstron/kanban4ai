use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum KanbanError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("YAML error: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),

    #[error("invalid task file format: {0}")]
    InvalidTaskFile(PathBuf),

    #[error("{0}")]
    Invalid(String),

    #[error("{0}")]
    Permission(String),

    #[error(
        "cannot move {}: board has active agent sessions (use --force to override)",
        .0.display()
    )]
    ActiveSessions(PathBuf),

    #[error("BACKUP_VERIFY_FAILED: {0}")]
    BackupVerifyFailed(String),

    #[error("BACKUP_FORMAT_NEWER: {0}")]
    BackupFormatNewer(String),

    #[error("BACKUP_DIR_INSIDE_STORE: {0}")]
    BackupDirInsideStore(String),

    #[error("BACKUP_ACTIVE_SESSIONS: {0}")]
    BackupActiveSessions(String),
}

pub type Result<T> = std::result::Result<T, KanbanError>;
