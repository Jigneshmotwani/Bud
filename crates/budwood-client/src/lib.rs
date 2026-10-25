//! The Budwood client: an HTTP [`Remote`], a verified local [`Objects`]
//! cache, and sparse [`Workspace`]s.

pub mod ignore;
pub mod objects;
pub mod remote;
pub mod workspace;

pub use objects::{CacheGc, CacheUsage, Objects, DEFAULT_CACHE_MAX_BYTES};
pub use remote::{ApiError, Remote};
pub use workspace::{
    ChangeKind, CommitOutcome, Config, LocalChange, SyncOutcome, UpdateConflict, Workspace, WorkspaceLock,
    WorkspaceLocked,
};
