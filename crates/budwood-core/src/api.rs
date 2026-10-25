//! JSON types for the HTTP API (version 1), shared by server and client.
//!
//! Object bodies are raw bytes, not JSON. The batch fetch endpoint returns a
//! sequence of frames: `id[32] len:u64le bytes[len]`.

use serde::{Deserialize, Serialize};

use crate::{Change, Changeset, EntryKind, ObjectId, RepoPath};

/// Largest number of ids accepted by one `objects/missing` or `objects/fetch` call.
pub const MAX_BATCH: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreateRepoRequest {
    pub name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RepoList {
    pub repos: Vec<String>,
}

/// The state of the repository at some revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revision {
    pub seq: u64,
    /// `None` for revision 0, the empty repository.
    pub id: Option<ObjectId>,
    pub root_tree: ObjectId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LogResponse {
    /// Newest first.
    pub changesets: Vec<Changeset>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitRequest {
    /// The revision the changes were made against. The server rebases onto
    /// newer history if none of it touched the same paths.
    pub base: u64,
    pub author: String,
    pub message: String,
    pub changes: Vec<Change>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitResponse {
    pub changeset: Changeset,
    /// True when newer changesets landed between `base` and this one.
    pub rebased: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IdList {
    pub ids: Vec<ObjectId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ListEntry {
    pub name: String,
    pub kind: EntryKind,
    pub id: ObjectId,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeResponse {
    pub revision: Revision,
    pub path: RepoPath,
    pub kind: EntryKind,
    pub id: ObjectId,
    /// Children when `kind` is `tree`; empty otherwise.
    pub entries: Vec<ListEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffResponse {
    pub from: Revision,
    pub to: Revision,
    pub changes: Vec<Change>,
}

/// Machine-readable error codes.
pub mod codes {
    pub const NOT_FOUND: &str = "not_found";
    pub const BAD_REQUEST: &str = "bad_request";
    pub const CONFLICT: &str = "conflict";
    pub const MISSING_OBJECTS: &str = "missing_objects";
    pub const UNAVAILABLE: &str = "unavailable";
    pub const INTERNAL: &str = "internal";
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    /// For `conflict`: the paths that conflicted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<RepoPath>,
    /// For `missing_objects`: blobs that must be uploaded first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing: Vec<ObjectId>,
}
