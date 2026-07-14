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
}

pub type Result<T> = std::result::Result<T, KanbanError>;
