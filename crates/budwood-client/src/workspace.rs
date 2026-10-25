//! A sparse working copy of one repository.
//!
//! A workspace materializes only the paths in its sparse set. Its metadata
//! lives in `.bud/`: `config.json` (server, repo, sparse paths), `state.json`
//! (the base revision and an index of the files checked out from it) and an
//! bounded object cache. The index stores each file's size and mtime so
//! `status` only re-hashes files whose metadata changed.
//!
//! Operations that change `.bud/` state (status, diff, commit, sync, revert,
//! sparse add, cache gc) hold an exclusive lock file, `.bud/lock`, so that
//! concurrent `bud` processes can't corrupt it.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use budwood_core::api::{CommitRequest, Revision};
use budwood_core::{diff_trees, walk_files, Change, Changeset, FileKind, FileState, ObjectId, RepoPath, METADATA_DIR};
use serde::{Deserialize, Serialize};

use crate::ignore::Ignore;
use crate::objects::{CacheGc, CacheUsage, Objects, DEFAULT_CACHE_MAX_BYTES};
use crate::remote::{ApiError, Remote};

/// Files modified more recently than this are re-hashed on every `status`
/// until they age, since a same-size rewrite within the filesystem's
/// timestamp granularity would otherwise go unnoticed.
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Checkouts fetch and write files in batches of this many, and may evict
/// cached objects between batches, so a checkout larger than the cache budget
/// doesn't need twice its size on disk.
const CHECKOUT_BATCH: usize = budwood_core::api::MAX_BATCH;

const LOCK_FILE: &str = "lock";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub server: String,
    pub repo: String,
    /// Checked-out prefixes; `[""]` is the whole repository. Never overlapping.
    pub sparse: Vec<RepoPath>,
    /// Size budget for the object cache in `.bud/objects`; default 2 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_max_bytes: Option<u64>,
}

impl Config {
    /// The effective object cache budget.
    pub fn cache_max_bytes(&self) -> u64 {
        self.cache_max_bytes.unwrap_or(DEFAULT_CACHE_MAX_BYTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct State {
    base: u64,
    base_root: ObjectId,
    files: BTreeMap<RepoPath, IndexEntry>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct IndexEntry {
    state: FileState,
    size: u64,
    /// 0 when the stat data can't be trusted and the file must be re-hashed.
    mtime_ns: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub fn letter(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Deleted => 'D',
        }
    }
}

/// A difference between the working copy and its base revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalChange {
    pub path: RepoPath,
    pub kind: ChangeKind,
    pub new: Option<FileState>,
}

/// Local changes that collide with incoming ones.
#[derive(Debug, thiserror::Error)]
#[error("local changes conflict with incoming changes to {} path(s): {}", .paths.len(), list(.paths))]
pub struct UpdateConflict {
    pub paths: Vec<RepoPath>,
}

fn list(paths: &[RepoPath]) -> String {
    paths.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")
}

/// Another process, or another handle in this one, holds the workspace lock.
#[derive(Debug, thiserror::Error)]
#[error("workspace is locked by another bud process{}; wait for it to finish", .pid.map(|p| format!(" (pid {p})")).unwrap_or_default())]
pub struct WorkspaceLocked {
    /// The lock file.
    pub path: PathBuf,
    /// The holder's pid, if it could be read.
    pub pid: Option<u32>,
}

/// The exclusive lock on a workspace's `.bud/` state: an OS file lock on
/// `.bud/lock`. The OS releases it when the guard drops or the process exits,
/// however it exits, so a killed `bud` never leaves a stale lock behind.
#[derive(Debug)]
pub struct WorkspaceLock {
    path: PathBuf,
    file: fs::File,
}

impl WorkspaceLock {
    fn acquire(meta_dir: &Path) -> Result<Self> {
        let path = meta_dir.join(LOCK_FILE);
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                // Empty while the holder is still writing its pid.
                let pid = fs::read_to_string(&path).ok().and_then(|s| s.trim().parse().ok());
                return Err(WorkspaceLocked { path, pid }.into());
            }
            Err(fs::TryLockError::Error(e)) => return Err(e).with_context(|| format!("locking {}", path.display())),
        }
        // The pid is only informational, for the error message above.
        file.set_len(0)?;
        file.write_all(format!("{}\n", std::process::id()).as_bytes())?;
        Ok(Self { path, file })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        // Closing the file releases the lock.
        let _ = self.file.set_len(0);
    }
}

#[derive(Debug)]
pub struct CommitOutcome {
    pub changeset: Changeset,
    pub rebased: bool,
    /// Files updated from other people's changes the commit was rebased over.
    pub pulled: usize,
}

#[derive(Debug)]
pub struct SyncOutcome {
    pub from: u64,
    pub to: u64,
    pub updated: usize,
}

pub struct Workspace {
    root: PathBuf,
    config: Config,
    state: State,
    objects: Objects,
    ignore: Ignore,
}

impl Workspace {
    /// Creates a working copy of `repo` in `dir`, checking out only `sparse`
    /// (or everything, if empty).
    ///
    /// `config.json` is written last, so a half-finished clone is never
    /// mistaken for a workspace. If the checkout fails, everything the clone
    /// created is removed again, so it can simply be retried.
    pub fn clone(server: &str, repo: &str, dir: &Path, sparse: Vec<RepoPath>) -> Result<Self> {
        if dir.exists() && fs::read_dir(dir)?.next().is_some() {
            let meta = dir.join(METADATA_DIR);
            if meta.is_dir() && !meta.join("config.json").is_file() {
                bail!("{} contains an interrupted clone; remove it and clone again", dir.display());
            }
            bail!("{} already exists and is not empty", dir.display());
        }
        let remote = Remote::new(server, repo)?;
        let head = remote.head().with_context(|| format!("contacting {server}"))?;
        // The topmost directory this clone creates, if `dir` doesn't exist.
        let created =
            dir.ancestors().take_while(|a| !a.as_os_str().is_empty() && !a.exists()).last().map(Path::to_path_buf);
        let result = (|| {
            fs::create_dir_all(dir.join(METADATA_DIR))?;
            let root = dir.canonicalize()?;
            let config = Config {
                server: remote.server().to_string(),
                repo: repo.to_string(),
                sparse: normalize_sparse(sparse),
                cache_max_bytes: None,
            };
            let mut objects = Objects::new(remote, root.join(METADATA_DIR).join("objects"))?;
            objects.set_max_bytes(config.cache_max_bytes());
            let state = State { base: head.seq, base_root: head.root_tree, files: BTreeMap::new() };
            let mut ws = Self { ignore: Ignore::load(&root)?, root, config, state, objects };
            ws.locked(|ws| {
                for prefix in ws.config.sparse.clone() {
                    ws.materialize(&prefix)?;
                }
                ws.save_state()?;
                ws.save_config()
            })?;
            Ok(ws)
        })();
        result.map_err(|e: anyhow::Error| match remove_partial_clone(dir, created.as_deref()) {
            Ok(()) => e.context(format!("clone failed (removed the partial checkout in {})", dir.display())),
            Err(cleanup) => e.context(format!(
                "clone failed, and removing the partial checkout also failed ({cleanup}); remove {} before retrying",
                dir.display()
            )),
        })
    }

    /// Opens the workspace containing `start`.
    pub fn open(start: &Path) -> Result<Self> {
        let start = start.canonicalize()?;
        let root = start
            .ancestors()
            .find(|dir| dir.join(METADATA_DIR).join("config.json").is_file())
            .ok_or_else(|| anyhow!("not inside a bud workspace (no {METADATA_DIR}/ found)"))?
            .to_path_buf();
        let meta = root.join(METADATA_DIR);
        let config: Config = read_json(&meta.join("config.json"))?;
        let state: State = read_json(&meta.join("state.json"))?;
        let remote = Remote::new(&config.server, &config.repo)?;
        let mut objects = Objects::new(remote, meta.join("objects"))?;
        objects.set_max_bytes(config.cache_max_bytes());
        Ok(Self { ignore: Ignore::load(&root)?, root, config, state, objects })
    }

    /// Takes the exclusive workspace lock. Mutating operations take it
    /// themselves, so holding it makes them fail with [`WorkspaceLocked`].
    pub fn lock(&self) -> Result<WorkspaceLock> {
        WorkspaceLock::acquire(&self.root.join(METADATA_DIR))
    }

    /// Runs `op` under the workspace lock, then enforces the cache budget.
    fn locked<T>(&mut self, op: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let _lock = self.lock()?;
        let result = op(self);
        self.trim_cache();
        result
    }

    /// Best effort: anything evicted is fetched again when it is needed.
    fn trim_cache(&self) {
        let _ = self.objects.trim();
    }

    /// The exact size of the local object cache.
    pub fn cache_usage(&self) -> Result<CacheUsage> {
        self.objects.cache_usage()
    }

    /// Evicts least recently used objects from the cache if it is over its
    /// budget (down to 80% of it), and reports what was freed. Evicted objects
    /// are fetched from the server again when they are next needed.
    pub fn gc_cache(&mut self) -> Result<CacheGc> {
        let _lock = self.lock()?;
        self.objects.gc()
    }

    /// Evicts least recently used objects until the cache holds at most
    /// `max_bytes` (0 empties it), regardless of the budget.
    pub fn gc_cache_to(&mut self, max_bytes: u64) -> Result<CacheGc> {
        let _lock = self.lock()?;
        self.objects.gc_to(max_bytes)
    }

    /// Sets (or, with `None`, resets to the default) the cache budget and
    /// saves it in `config.json`. It is enforced the next time an operation
    /// adds to the cache, or by [`Workspace::gc_cache`].
    pub fn set_cache_max_bytes(&mut self, max_bytes: Option<u64>) -> Result<()> {
        let _lock = self.lock()?;
        self.config.cache_max_bytes = max_bytes;
        self.objects.set_max_bytes(self.config.cache_max_bytes());
        self.save_config()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The revision the working copy is based on.
    pub fn base(&self) -> u64 {
        self.state.base
    }

    pub fn remote(&self) -> &Remote {
        self.objects.remote()
    }

    /// Converts a filesystem path (absolute, or relative to `cwd`) to a repo path.
    ///
    /// `..` is resolved lexically. The workspace root is canonical, so when
    /// the path doesn't start with it literally (it goes through a symlink,
    /// e.g. `/tmp` -> `/private/tmp` on macOS), the first ancestor that is
    /// physically the root marks where the repository path begins; the rest
    /// is taken literally, so a tracked symlink names itself, not its target.
    /// An argument naming the workspace root is the root path.
    pub fn repo_path(&self, cwd: &Path, arg: &str) -> Result<RepoPath> {
        let joined = normalize_lexically(&cwd.join(arg));
        let relative = match joined.strip_prefix(&self.root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => self.physical_relative(&joined).ok_or_else(|| anyhow!("{arg} is outside the workspace"))?,
        };
        let text = relative
            .to_str()
            .ok_or_else(|| anyhow!("{arg} is not valid UTF-8"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        Ok(RepoPath::new(text)?)
    }

    /// `path` relative to the root, resolving symlinks only up to the point
    /// where the path enters the workspace: the shortest ancestor whose
    /// canonical form is the root or inside it (a symlink from outside may
    /// point into a subdirectory). The rest of the path is taken literally.
    fn physical_relative(&self, path: &Path) -> Option<PathBuf> {
        let mut ancestors: Vec<&Path> = path.ancestors().collect();
        ancestors.reverse();
        ancestors.into_iter().find_map(|ancestor| {
            let real = ancestor.canonicalize().ok()?;
            Some(real.strip_prefix(&self.root).ok()?.join(path.strip_prefix(ancestor).ok()?))
        })
    }

    fn fs_path(&self, path: &RepoPath) -> PathBuf {
        let mut out = self.root.clone();
        out.extend(path.components());
        out
    }

    fn in_scope(&self, path: &RepoPath) -> bool {
        self.config.sparse.iter().any(|prefix| path.is_within(prefix))
    }

    /// Differences between the files on disk and the base revision.
    ///
    /// Takes `&mut self` (and the workspace lock) because it refreshes stale
    /// stat data in the index.
    pub fn status(&mut self) -> Result<Vec<LocalChange>> {
        self.locked(Self::status_unlocked)
    }

    fn status_unlocked(&mut self) -> Result<Vec<LocalChange>> {
        // `.budignore` may have been checked out, synced or edited since open.
        self.ignore = Ignore::load(&self.root)?;
        let mut disk = Vec::new();
        for prefix in &self.config.sparse {
            self.collect_disk(prefix, false, &mut disk)?;
        }

        let mut changes = Vec::new();
        let mut found = HashSet::new();
        let mut refreshed = Vec::new();
        for (path, meta) in disk {
            let kind = platform::file_kind(&meta);
            let indexed = self.state.files.get(&path).copied();
            let state = match indexed {
                Some(e)
                    if e.mtime_ns != 0
                        && e.state.kind == kind
                        && e.size == meta.len()
                        && e.mtime_ns == mtime_ns(&meta) =>
                {
                    e.state
                }
                _ => {
                    let data = read_content(&self.fs_path(&path), &meta)?;
                    FileState { kind, blob: ObjectId::for_blob(&data) }
                }
            };
            match indexed {
                Some(e) if e.state == state => {
                    let fresh = index_entry(&meta, state);
                    if fresh.mtime_ns != 0 && fresh.mtime_ns != e.mtime_ns {
                        refreshed.push((path.clone(), fresh));
                    }
                }
                Some(_) => {
                    changes.push(LocalChange { path: path.clone(), kind: ChangeKind::Modified, new: Some(state) })
                }
                None => changes.push(LocalChange { path: path.clone(), kind: ChangeKind::Added, new: Some(state) }),
            }
            found.insert(path);
        }
        for path in self.state.files.keys() {
            if !found.contains(path) {
                changes.push(LocalChange { path: path.clone(), kind: ChangeKind::Deleted, new: None });
            }
        }
        if !refreshed.is_empty() {
            self.state.files.extend(refreshed);
            self.save_state()?;
        }
        changes.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(changes)
    }

    fn is_tracked_dir(&self, dir: &RepoPath) -> bool {
        self.state
            .files
            .range(dir.clone()..)
            .take_while(|(p, _)| p.as_str().starts_with(dir.as_str()))
            .any(|(p, _)| p.is_within(dir))
    }

    /// Every file on disk at or under `path`, skipping ignored untracked ones.
    /// `ignored_dir` is set below an ignored directory that holds tracked files.
    fn collect_disk(&self, path: &RepoPath, ignored_dir: bool, out: &mut Vec<(RepoPath, fs::Metadata)>) -> Result<()> {
        let fs_path = self.fs_path(path);
        let meta = match fs::symlink_metadata(&fs_path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", fs_path.display())),
        };
        let tracked = self.state.files.contains_key(path);
        if meta.is_dir() {
            let ignored = ignored_dir || (!path.is_root() && self.ignore.is_ignored(path, true));
            if ignored && !self.is_tracked_dir(path) {
                return Ok(());
            }
            let mut entries: Vec<_> = fs::read_dir(&fs_path)?.collect::<std::io::Result<_>>()?;
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let name = entry.file_name();
                let name = name.to_str().ok_or_else(|| anyhow!("{} has a non-UTF-8 name", entry.path().display()))?;
                // Ours, or a nested workspace's; never a repository path.
                if name == METADATA_DIR {
                    continue;
                }
                self.collect_disk(&path.join(name)?, ignored, out)?;
            }
        } else if (meta.is_file() || meta.file_type().is_symlink())
            && !path.is_root()
            && (tracked || !(ignored_dir || self.ignore.is_ignored(path, false)))
        {
            out.push((path.clone(), meta));
        }
        Ok(())
    }

    /// Uploads local changes (optionally only those under `only`) and appends
    /// them to the log. The server rebases them over newer changesets that
    /// touched other paths; the working copy is then updated to match.
    pub fn commit(&mut self, message: &str, author: &str, only: &[RepoPath]) -> Result<CommitOutcome> {
        self.locked(|ws| ws.commit_unlocked(message, author, only))
    }

    fn commit_unlocked(&mut self, message: &str, author: &str, only: &[RepoPath]) -> Result<CommitOutcome> {
        let mut changes = self.status_unlocked()?;
        if !only.is_empty() {
            changes.retain(|c| only.iter().any(|p| c.path.is_within(p)));
        }
        if changes.is_empty() {
            bail!("nothing to commit");
        }

        let mut by_blob: BTreeMap<ObjectId, &RepoPath> = BTreeMap::new();
        for change in &changes {
            if let Some(state) = change.new {
                by_blob.entry(state.blob).or_insert(&change.path);
            }
        }
        let ids: Vec<ObjectId> = by_blob.keys().copied().collect();
        for id in self.remote().missing(&ids)? {
            let path = by_blob[&id];
            let fs_path = self.fs_path(path);
            let meta = fs::symlink_metadata(&fs_path)?;
            let data = read_content(&fs_path, &meta)?;
            if ObjectId::for_blob(&data) != id {
                bail!("{path} changed while committing; try again");
            }
            self.objects.store(&id, &data)?;
            self.remote().put_blob(&id, data)?;
        }

        let req = CommitRequest {
            base: self.state.base,
            author: author.to_string(),
            message: message.to_string(),
            changes: changes.iter().map(|c| Change { path: c.path.clone(), new: c.new }).collect(),
        };
        let resp = self.remote().commit(&req).map_err(|e| match e.downcast::<ApiError>() {
            Ok(api) if api.is_conflict() => {
                let paths = list(&api.body.paths);
                anyhow!(
                    "commit rejected: {} ({paths}). Run `bud sync` to pick up the newer changes, then commit again.",
                    api.body.message
                )
            }
            Ok(api) => api.into(),
            Err(e) => e,
        })?;

        let seq = resp.changeset.seq;
        let target = Revision { seq, id: Some(resp.changeset.id), root_tree: resp.changeset.root_tree };
        let update = self.update_to(target).with_context(|| {
            format!("committed revision {seq}, but could not update the working copy; run `bud sync`")
        })?;
        Ok(CommitOutcome { changeset: resp.changeset, rebased: resp.rebased, pulled: update.updated })
    }

    /// Updates the working copy to `rev` (default: head), keeping local changes
    /// that don't collide with incoming ones.
    pub fn sync(&mut self, rev: Option<&str>) -> Result<SyncOutcome> {
        self.locked(|ws| {
            let target = ws.remote().resolve(rev.unwrap_or("head"))?;
            ws.update_to(target)
        })
    }

    fn update_to(&mut self, target: Revision) -> Result<SyncOutcome> {
        let from = self.state.base;
        let mut incoming = Vec::new();
        for prefix in &self.config.sparse {
            incoming.extend(diff_trees(&self.objects, self.state.base_root, target.root_tree, prefix)?);
        }
        let local: BTreeMap<RepoPath, Option<FileState>> =
            self.status_unlocked()?.into_iter().map(|c| (c.path, c.new)).collect();

        let mut conflicts = Vec::new();
        for change in &incoming {
            let collides = match local.get(&change.path) {
                Some(mine) => *mine != change.new,
                // A local file where a directory is coming, or local files
                // inside a directory being replaced by a file.
                None => {
                    change.path.ancestors().any(|dir| matches!(local.get(dir), Some(Some(_))))
                        || (change.new.is_some()
                            && local
                                .range(change.path.clone()..)
                                .take_while(|(p, _)| p.as_str().starts_with(change.path.as_str()))
                                .any(|(p, _)| p.is_within(&change.path) && *p != change.path))
                        || self.untracked_differs(change)?
                }
            };
            if collides {
                conflicts.push(change.path.clone());
            }
        }
        if !conflicts.is_empty() {
            return Err(UpdateConflict { paths: conflicts }.into());
        }

        let mut updated = 0;
        // Deletes first, so that files can replace directories and vice versa.
        for change in incoming.iter().filter(|c| c.new.is_none()) {
            if local.get(&change.path) != Some(&None) {
                self.remove_file(&change.path)?;
                updated += 1;
            }
            self.state.files.remove(&change.path);
        }
        let sets: Vec<(&RepoPath, FileState)> = incoming.iter().filter_map(|c| Some((&c.path, c.new?))).collect();
        for batch in sets.chunks(CHECKOUT_BATCH) {
            let is_local = |path: &RepoPath, state: FileState| local.get(path) == Some(&Some(state));
            self.objects.prefetch_blobs(batch.iter().filter(|(p, s)| !is_local(p, *s)).map(|(_, s)| s.blob))?;
            for &(path, state) in batch {
                let entry = if is_local(path, state) {
                    // Already identical on disk (e.g. our own committed change).
                    let meta = fs::symlink_metadata(self.fs_path(path))?;
                    index_entry(&meta, state)
                } else {
                    updated += 1;
                    self.write_file(path, state)?
                };
                self.state.files.insert(path.clone(), entry);
            }
            // This batch is on disk, so its objects may be evicted.
            self.trim_cache();
        }
        self.state.base = target.seq;
        self.state.base_root = target.root_tree;
        self.save_state()?;
        Ok(SyncOutcome { from, to: target.seq, updated })
    }

    /// True if `change` would overwrite an ignored, untracked file on disk
    /// that differs from the incoming one.
    fn untracked_differs(&self, change: &Change) -> Result<bool> {
        let Some(state) = change.new else {
            return Ok(false);
        };
        if self.state.files.contains_key(&change.path) {
            return Ok(false);
        }
        let fs_path = self.fs_path(&change.path);
        let meta = match fs::symlink_metadata(&fs_path) {
            Ok(meta) if !meta.is_dir() => meta,
            _ => return Ok(false),
        };
        Ok(platform::file_kind(&meta) != state.kind
            || ObjectId::for_blob(&read_content(&fs_path, &meta)?) != state.blob)
    }

    /// Discards local changes at or under `paths`, restoring the base revision.
    pub fn revert(&mut self, paths: &[RepoPath]) -> Result<Vec<LocalChange>> {
        self.locked(|ws| ws.revert_unlocked(paths))
    }

    fn revert_unlocked(&mut self, paths: &[RepoPath]) -> Result<Vec<LocalChange>> {
        let changes: Vec<LocalChange> =
            self.status_unlocked()?.into_iter().filter(|c| paths.iter().any(|p| c.path.is_within(p))).collect();
        let restore: Vec<ObjectId> =
            changes.iter().filter_map(|c| self.state.files.get(&c.path).map(|e| e.state.blob)).collect();
        self.objects.prefetch_blobs(restore)?;
        // Remove added files first, so that a file can replace a directory.
        for change in changes.iter().filter(|c| !self.state.files.contains_key(&c.path)) {
            self.remove_file(&change.path)?;
        }
        for change in &changes {
            if let Some(state) = self.state.files.get(&change.path).map(|e| e.state) {
                let entry = self.write_file(&change.path, state)?;
                self.state.files.insert(change.path.clone(), entry);
            }
        }
        self.save_state()?;
        Ok(changes)
    }

    /// Adds `path` to the sparse set and checks out the files under it.
    pub fn sparse_add(&mut self, path: RepoPath) -> Result<usize> {
        if self.in_scope(&path) {
            return Ok(0);
        }
        self.locked(|ws| {
            let count = ws.materialize(&path)?;
            let mut sparse = ws.config.sparse.clone();
            sparse.push(path.clone());
            ws.config.sparse = normalize_sparse(sparse);
            ws.save_state()?;
            ws.save_config()?;
            Ok(count)
        })
    }

    /// Checks out every untracked file under `prefix` at the base revision.
    /// Refuses, before writing anything, to overwrite a different local file.
    fn materialize(&mut self, prefix: &RepoPath) -> Result<usize> {
        let files: Vec<(RepoPath, FileState)> = walk_files(&self.objects, self.state.base_root, prefix)?
            .into_iter()
            .filter(|(p, _)| !self.state.files.contains_key(p))
            .collect();
        for (path, state) in &files {
            let fs_path = self.fs_path(path);
            if let Ok(meta) = fs::symlink_metadata(&fs_path) {
                let same = (meta.is_file() || meta.file_type().is_symlink())
                    && platform::file_kind(&meta) == state.kind
                    && ObjectId::for_blob(&read_content(&fs_path, &meta)?) == state.blob;
                if !same {
                    bail!("{path} already exists locally with different content; move it aside first");
                }
            }
        }
        for batch in files.chunks(CHECKOUT_BATCH) {
            self.objects.prefetch_blobs(batch.iter().map(|(_, s)| s.blob))?;
            for (path, state) in batch {
                let entry = self.write_file(path, *state)?;
                self.state.files.insert(path.clone(), entry);
            }
            // This batch is on disk, so its objects may be evicted.
            self.trim_cache();
        }
        Ok(files.len())
    }

    /// A unified diff of local changes against the base revision.
    ///
    /// A change of kind only (e.g. `chmod +x`) shows as `old kind:` and
    /// `new kind:` lines, like git's `old mode`/`new mode`.
    pub fn diff(&mut self, only: &[RepoPath]) -> Result<String> {
        self.locked(|ws| ws.diff_unlocked(only))
    }

    fn diff_unlocked(&mut self, only: &[RepoPath]) -> Result<String> {
        let mut out = String::new();
        for change in self.status_unlocked()? {
            if !only.is_empty() && !only.iter().any(|p| change.path.is_within(p)) {
                continue;
            }
            let old = match self.state.files.get(&change.path) {
                Some(e) => self.objects.blob(&e.state.blob)?,
                None => Vec::new(),
            };
            let new = match change.new {
                Some(_) => {
                    let fs_path = self.fs_path(&change.path);
                    read_content(&fs_path, &fs::symlink_metadata(&fs_path)?)?
                }
                None => Vec::new(),
            };
            let old_name = match change.kind {
                ChangeKind::Added => "/dev/null".to_string(),
                _ => format!("a/{}", change.path),
            };
            let new_name = match change.kind {
                ChangeKind::Deleted => "/dev/null".to_string(),
                _ => format!("b/{}", change.path),
            };
            out.push_str(&format!("diff {}\n", change.path));
            let old_kind = self.state.files.get(&change.path).map(|e| e.state.kind);
            if let (Some(old_kind), Some(new)) = (old_kind, change.new) {
                if old_kind != new.kind {
                    out.push_str(&format!("old kind: {}\nnew kind: {}\n", kind_name(old_kind), kind_name(new.kind)));
                }
            }
            if old == new && change.kind == ChangeKind::Modified {
                continue;
            }
            match (text(&old), text(&new)) {
                (Some(old), Some(new)) => {
                    let diff = similar::TextDiff::from_lines(old, new);
                    out.push_str(&diff.unified_diff().context_radius(3).header(&old_name, &new_name).to_string());
                    if !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
                _ => out.push_str(&format!("Binary files {old_name} and {new_name} differ\n")),
            }
        }
        Ok(out)
    }

    fn write_file(&self, path: &RepoPath, state: FileState) -> Result<IndexEntry> {
        let data = self.objects.blob(&state.blob)?;
        let fs_path = self.fs_path(path);
        if let Some(parent) = fs_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating directory for {path}"))?;
        }
        match fs::symlink_metadata(&fs_path) {
            Ok(meta) if meta.is_dir() => {
                fs::remove_dir(&fs_path).with_context(|| format!("{path} is a non-empty directory"))?
            }
            Ok(_) => fs::remove_file(&fs_path)?,
            Err(_) => {}
        }
        platform::write(&fs_path, state.kind, &data).with_context(|| format!("writing {path}"))?;
        Ok(index_entry(&fs::symlink_metadata(&fs_path)?, state))
    }

    /// Removes a file and any directories left empty, up to the workspace root.
    fn remove_file(&self, path: &RepoPath) -> Result<()> {
        let fs_path = self.fs_path(path);
        match fs::remove_file(&fs_path) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {path}")),
        }
        let mut dir = fs_path.parent();
        while let Some(d) = dir {
            if d == self.root || fs::remove_dir(d).is_err() {
                break;
            }
            dir = d.parent();
        }
        Ok(())
    }

    fn save_config(&self) -> Result<()> {
        write_json(&self.root.join(METADATA_DIR).join("config.json"), &self.config)
    }

    fn save_state(&self) -> Result<()> {
        write_json(&self.root.join(METADATA_DIR).join("state.json"), &self.state)
    }
}

/// Sorts the sparse set and drops prefixes covered by others.
fn normalize_sparse(mut sparse: Vec<RepoPath>) -> Vec<RepoPath> {
    if sparse.is_empty() || sparse.iter().any(|p| p.is_root()) {
        return vec![RepoPath::root()];
    }
    sparse.sort();
    sparse.dedup();
    let all = sparse.clone();
    sparse.retain(|p| !all.iter().any(|q| q != p && p.is_within(q)));
    sparse
}

fn kind_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::File => "file",
        FileKind::Executable => "executable",
        FileKind::Symlink => "symlink",
    }
}

/// Undoes a failed clone: removes the directories it created, or empties the
/// directory it was given (which was empty to begin with).
fn remove_partial_clone(dir: &Path, created: Option<&Path>) -> std::io::Result<()> {
    if let Some(top) = created {
        return match fs::remove_dir_all(top) {
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            other => other,
        };
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

fn text(data: &[u8]) -> Option<&str> {
    if data.contains(&0) {
        return None;
    }
    std::str::from_utf8(data).ok()
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

fn index_entry(meta: &fs::Metadata, state: FileState) -> IndexEntry {
    let mtime = mtime_ns(meta);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64);
    let racy = now - mtime < RACY_WINDOW.as_nanos() as i64;
    IndexEntry { state, size: meta.len(), mtime_ns: if racy { 0 } else { mtime } }
}

fn read_content(path: &Path, meta: &fs::Metadata) -> Result<Vec<u8>> {
    if meta.file_type().is_symlink() {
        platform::read_link(path)
    } else {
        fs::read(path).with_context(|| format!("reading {}", path.display()))
    }
}

fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(unix)]
mod platform {
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use anyhow::Result;
    use budwood_core::FileKind;

    pub fn file_kind(meta: &fs::Metadata) -> FileKind {
        if meta.file_type().is_symlink() {
            FileKind::Symlink
        } else if meta.permissions().mode() & 0o111 != 0 {
            FileKind::Executable
        } else {
            FileKind::File
        }
    }

    pub fn read_link(path: &Path) -> Result<Vec<u8>> {
        Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
    }

    pub fn write(path: &Path, kind: FileKind, data: &[u8]) -> Result<()> {
        match kind {
            FileKind::Symlink => {
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(data), path)?;
            }
            FileKind::File | FileKind::Executable => {
                fs::write(path, data)?;
                let mode = if kind == FileKind::Executable { 0o755 } else { 0o644 };
                fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            }
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod platform {
    use std::fs;
    use std::path::Path;

    use anyhow::{bail, Result};
    use budwood_core::FileKind;

    pub fn file_kind(meta: &fs::Metadata) -> FileKind {
        if meta.file_type().is_symlink() {
            FileKind::Symlink
        } else {
            FileKind::File
        }
    }

    pub fn read_link(path: &Path) -> Result<Vec<u8>> {
        bail!("symlinks are not supported on this platform: {}", path.display())
    }

    pub fn write(path: &Path, kind: FileKind, data: &[u8]) -> Result<()> {
        if kind == FileKind::Symlink {
            bail!("symlinks are not supported on this platform: {}", path.display());
        }
        Ok(fs::write(path, data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn sparse_normalization() {
        assert_eq!(normalize_sparse(vec![]), vec![RepoPath::root()]);
        assert_eq!(normalize_sparse(vec![path("a/b"), path("c"), path("a"), path("c")]), vec![path("a"), path("c")]);
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(normalize_lexically(Path::new("/w/a/../b/./c")), PathBuf::from("/w/b/c"));
    }
}
