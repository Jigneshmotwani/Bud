use crate::{ObjectId, RepoPath};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid path {path:?}: {reason}")]
    InvalidPath { path: String, reason: &'static str },
    #[error("invalid object id {0:?}")]
    InvalidId(String),
    #[error("corrupt object: {0}")]
    Corrupt(String),
    #[error("object {0} not found")]
    ObjectNotFound(ObjectId),
    #[error("path {0} does not exist")]
    PathNotFound(RepoPath),
    #[error("conflict at {path}: {reason}")]
    PathConflict { path: RepoPath, reason: &'static str },
    #[error("invalid changes: {0}")]
    InvalidChanges(String),
    #[error("storage error: {0}")]
    Storage(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
