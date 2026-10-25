use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{Error, FileState, ObjectId, RepoPath};

/// A change to one file: `new` is the file's new state, or `None` to delete it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub path: RepoPath,
    pub new: Option<FileState>,
}

impl Change {
    pub fn set(path: RepoPath, state: FileState) -> Self {
        Self { path, new: Some(state) }
    }

    pub fn delete(path: RepoPath) -> Self {
        Self { path, new: None }
    }
}

/// Checks that a list of changes is well-formed and returns it sorted by path.
///
/// Paths must be unique and must name files, not the root. A path may be the
/// ancestor of another (deleting file `a` and creating `a/b`); whether that
/// applies cleanly is decided by [`crate::apply_changes`].
pub fn validate_changes(mut changes: Vec<Change>) -> Result<Vec<Change>, Error> {
    if changes.is_empty() {
        return Err(Error::InvalidChanges("no changes".into()));
    }
    let mut seen = BTreeSet::new();
    for change in &changes {
        if change.path.is_root() {
            return Err(Error::InvalidChanges("cannot change the repository root".into()));
        }
        if !seen.insert(&change.path) {
            return Err(Error::InvalidChanges(format!("duplicate path {}", change.path)));
        }
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(changes)
}

/// One entry in a repository's linear history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub id: ObjectId,
    /// Position in the log; the first changeset is 1 and 0 is the empty repository.
    pub seq: u64,
    pub parent: Option<ObjectId>,
    /// The `seq` the author's working copy was at. When it is less than
    /// `seq - 1`, the server rebased the change over newer history.
    pub base: u64,
    /// Root tree after applying `changes` to the parent's root.
    pub root_tree: ObjectId,
    pub author: String,
    pub message: String,
    /// Seconds since the Unix epoch, assigned by the server.
    pub timestamp: i64,
    pub changes: Vec<Change>,
}

/// Every field except `id`, in a fixed order; its JSON encoding is hashed.
#[derive(Serialize)]
struct Canonical<'a> {
    seq: u64,
    parent: &'a Option<ObjectId>,
    base: u64,
    root_tree: &'a ObjectId,
    author: &'a str,
    message: &'a str,
    timestamp: i64,
    changes: &'a [Change],
}

impl Changeset {
    pub fn compute_id(&self) -> ObjectId {
        let canonical = Canonical {
            seq: self.seq,
            parent: &self.parent,
            base: self.base,
            root_tree: &self.root_tree,
            author: &self.author,
            message: &self.message,
            timestamp: self.timestamp,
            changes: &self.changes,
        };
        let bytes = serde_json::to_vec(&canonical).expect("changeset serializes");
        ObjectId::for_changeset(&bytes)
    }

    /// Fills in `id` from the other fields.
    pub fn seal(mut self) -> Self {
        self.id = self.compute_id();
        self
    }

    pub fn verify(&self) -> bool {
        self.id == self.compute_id()
    }

    /// The first line of the message.
    pub fn summary(&self) -> &str {
        self.message.lines().next().unwrap_or("")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{empty_tree_id, FileKind};

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn validation() {
        assert!(validate_changes(vec![]).is_err());
        assert!(validate_changes(vec![Change::delete(RepoPath::root())]).is_err());
        assert!(validate_changes(vec![Change::delete(path("a")), Change::delete(path("a"))]).is_err());
        let sorted = validate_changes(vec![Change::delete(path("b")), Change::delete(path("a"))]).unwrap();
        assert_eq!(sorted[0].path, path("a"));
    }

    #[test]
    fn id_covers_content() {
        let state = FileState { kind: FileKind::File, blob: ObjectId::for_blob(b"x") };
        let cs = Changeset {
            id: ObjectId::from_bytes([0; 32]),
            seq: 1,
            parent: None,
            base: 0,
            root_tree: empty_tree_id(),
            author: "a".into(),
            message: "m".into(),
            timestamp: 0,
            changes: vec![Change::set(path("f"), state)],
        }
        .seal();
        assert!(cs.verify());
        let mut tampered = cs.clone();
        tampered.message = "other".into();
        assert!(!tampered.verify());
    }
}
