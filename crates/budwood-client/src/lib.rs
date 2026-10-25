//! The Budwood client: an HTTP [`Remote`], a verified local [`Objects`]
//! cache, and sparse [`Workspace`]s.

pub mod ignore;
pub mod objects;
pub mod remote;
pub mod workspace;

pub use objects::Objects;
pub use remote::{ApiError, Remote};
pub use workspace::{ChangeKind, CommitOutcome, LocalChange, SyncOutcome, UpdateConflict, Workspace};
