//! Core data model shared by the Budwood server and client.
//!
//! Budwood stores history as a linear log of *changesets*. Unlike a git commit,
//! a changeset does not point at a full snapshot that clients must download;
//! it lists the files that changed relative to its parent, each pointing at a
//! content-addressed blob. Snapshots (trees) are *derived data* that the server
//! computes and that clients fetch lazily, one object at a time.

pub mod api;
mod changeset;
mod error;
mod id;
mod ops;
mod path;
mod tree;

pub use changeset::{validate_changes, Change, Changeset};
pub use error::{Error, Result};
pub use id::ObjectId;
pub use ops::{apply_changes, diff_trees, lookup, walk_files, TreeSink, TreeSource};
pub use path::{RepoPath, METADATA_DIR};
pub use tree::{empty_tree_id, EntryKind, FileKind, FileState, Tree, TreeEntry};
