//! Repository operations, independent of the HTTP transport.
//!
//! The server does the heavy lifting: it owns history, computes trees, answers
//! queries and performs rebases, so clients never need a full copy of the repo.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use budwood_core::api::{CommitRequest, CommitResponse, DiffResponse, ListEntry, Revision, TreeResponse, MAX_BATCH};
use budwood_core::{
    apply_changes, diff_trees, empty_tree_id, lookup, validate_changes, Change, Changeset, EntryKind, ObjectId,
    RepoPath, TreeSink, TreeSource,
};

use crate::meta::{HeadMoved, MetaStore};
use crate::store::{FsObjectStore, ObjectStore, Trees};

/// How many times a commit is retried when other commits win the race to
/// append. Each retry re-checks for conflicts against the new head.
const COMMIT_ATTEMPTS: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("{message}")]
    Conflict { message: String, paths: Vec<RepoPath> },
    #[error("{} blob(s) must be uploaded before committing", .0.len())]
    MissingObjects(Vec<ObjectId>),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type ServiceResult<T> = Result<T, ServiceError>;

fn internal(e: budwood_core::Error) -> ServiceError {
    ServiceError::Internal(e.into())
}

pub struct Service {
    objects: Arc<dyn ObjectStore>,
    meta: MetaStore,
}

impl Service {
    /// Opens (or initializes) a server data directory.
    pub fn open(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let objects = FsObjectStore::open(data_dir.join("objects"))?;
        let meta = MetaStore::open(&data_dir.join("meta.sqlite3"))?;
        Self::new(Arc::new(objects), meta)
    }

    pub fn new(objects: Arc<dyn ObjectStore>, meta: MetaStore) -> anyhow::Result<Self> {
        let service = Self { objects, meta };
        // Clients fetch the empty tree like any other.
        Trees(&*service.objects).put_tree(&budwood_core::Tree::new())?;
        Ok(service)
    }

    fn trees(&self) -> Trees<'_> {
        Trees(&*self.objects)
    }

    pub fn create_repo(&self, name: &str) -> ServiceResult<bool> {
        validate_repo_name(name).map_err(|reason| ServiceError::BadRequest(format!("invalid repo name {name:?}: {reason}")))?;
        Ok(self.meta.create_repo(name, now())?)
    }

    pub fn list_repos(&self) -> ServiceResult<Vec<String>> {
        Ok(self.meta.list_repos()?)
    }

    fn require_repo(&self, repo: &str) -> ServiceResult<()> {
        if self.meta.repo_exists(repo)? {
            Ok(())
        } else {
            Err(ServiceError::NotFound(format!("no repository named {repo:?}")))
        }
    }

    fn head_revision(&self, repo: &str) -> ServiceResult<Revision> {
        Ok(match self.meta.head(repo)? {
            Some(cs) => revision_of(&cs),
            None => Revision { seq: 0, id: None, root_tree: empty_tree_id() },
        })
    }

    /// Resolves `head` (or empty), a sequence number, or a full changeset id.
    pub fn resolve(&self, repo: &str, rev: &str) -> ServiceResult<Revision> {
        self.require_repo(repo)?;
        let rev = rev.trim();
        if rev.is_empty() || rev == "head" {
            return self.head_revision(repo);
        }
        if rev == "0" {
            return Ok(Revision { seq: 0, id: None, root_tree: empty_tree_id() });
        }
        let found = if let Ok(seq) = rev.parse::<u64>() {
            self.meta.by_seq(repo, seq)?
        } else if let Ok(id) = ObjectId::from_hex(rev) {
            self.meta.by_id(repo, &id)?
        } else {
            return Err(ServiceError::BadRequest(format!("invalid revision {rev:?}")));
        };
        found
            .map(|cs| revision_of(&cs))
            .ok_or_else(|| ServiceError::NotFound(format!("no revision {rev:?} in {repo:?}")))
    }

    pub fn changeset(&self, repo: &str, rev: &str) -> ServiceResult<Changeset> {
        let revision = self.resolve(repo, rev)?;
        if revision.seq == 0 {
            return Err(ServiceError::NotFound("revision 0 is the empty repository".into()));
        }
        self.meta
            .by_seq(repo, revision.seq)?
            .ok_or_else(|| ServiceError::NotFound(format!("no revision {rev:?}")))
    }

    pub fn log(&self, repo: &str, path: &RepoPath, before: Option<u64>, limit: usize) -> ServiceResult<Vec<Changeset>> {
        self.require_repo(repo)?;
        Ok(self.meta.log(repo, path, before, limit.clamp(1, 1000))?)
    }

    pub fn tree(&self, repo: &str, rev: &str, path: &RepoPath) -> ServiceResult<TreeResponse> {
        let revision = self.resolve(repo, rev)?;
        let entry = lookup(&self.trees(), revision.root_tree, path)
            .map_err(internal)?
            .ok_or_else(|| ServiceError::NotFound(format!("{path} does not exist at revision {}", revision.seq)))?;
        let entries = if entry.kind == EntryKind::Tree {
            let tree = self.trees().get_tree(&entry.id).map_err(internal)?;
            tree.iter()
                .map(|(name, e)| ListEntry { name: name.to_string(), kind: e.kind, id: e.id })
                .collect()
        } else {
            Vec::new()
        };
        Ok(TreeResponse { revision, path: path.clone(), kind: entry.kind, id: entry.id, entries })
    }

    pub fn read_file(&self, repo: &str, rev: &str, path: &RepoPath) -> ServiceResult<Vec<u8>> {
        let listing = self.tree(repo, rev, path)?;
        if listing.kind == EntryKind::Tree {
            return Err(ServiceError::BadRequest(format!("{path} is a directory")));
        }
        self.load(&listing.id)
    }

    pub fn diff(&self, repo: &str, from: &str, to: &str, path: &RepoPath) -> ServiceResult<DiffResponse> {
        let from = self.resolve(repo, from)?;
        let to = self.resolve(repo, to)?;
        let changes = diff_trees(&self.trees(), from.root_tree, to.root_tree, path).map_err(internal)?;
        Ok(DiffResponse { from, to, changes })
    }

    fn load(&self, id: &ObjectId) -> ServiceResult<Vec<u8>> {
        self.objects
            .get(id)?
            .ok_or_else(|| ServiceError::NotFound(format!("object {id} not found")))
    }

    pub fn get_object(&self, repo: &str, id: &ObjectId) -> ServiceResult<Vec<u8>> {
        self.require_repo(repo)?;
        self.load(id)
    }

    pub fn missing(&self, repo: &str, ids: &[ObjectId]) -> ServiceResult<Vec<ObjectId>> {
        self.require_repo(repo)?;
        check_batch(ids)?;
        let mut missing = Vec::new();
        for id in ids {
            if !self.objects.contains(id)? {
                missing.push(*id);
            }
        }
        Ok(missing)
    }

    /// Several objects in one response, framed as `id[32] len:u64le bytes`.
    pub fn fetch(&self, repo: &str, ids: &[ObjectId]) -> ServiceResult<Vec<u8>> {
        self.require_repo(repo)?;
        check_batch(ids)?;
        let mut out = Vec::new();
        for id in ids {
            let data = self.load(id)?;
            out.extend_from_slice(id.as_bytes());
            out.extend_from_slice(&(data.len() as u64).to_le_bytes());
            out.extend_from_slice(&data);
        }
        Ok(out)
    }

    pub fn put_blob(&self, repo: &str, id: &ObjectId, data: &[u8]) -> ServiceResult<()> {
        self.require_repo(repo)?;
        let actual = ObjectId::for_blob(data);
        if actual != *id {
            return Err(ServiceError::BadRequest(format!("blob hashes to {actual}, not {id}")));
        }
        Ok(self.objects.put(id, data)?)
    }

    /// Appends a changeset, rebasing it over newer history when that history
    /// touched none of the same paths.
    pub fn commit(&self, repo: &str, req: CommitRequest) -> ServiceResult<CommitResponse> {
        self.require_repo(repo)?;
        let bad = ServiceError::BadRequest;
        if req.message.trim().is_empty() {
            return Err(bad("commit message is empty".into()));
        }
        if req.author.trim().is_empty() {
            return Err(bad("author is empty".into()));
        }
        let changes = validate_changes(req.changes).map_err(|e| bad(e.to_string()))?;

        let mut blobs: Vec<ObjectId> = changes.iter().filter_map(|c| c.new.map(|s| s.blob)).collect();
        blobs.sort_unstable();
        blobs.dedup();
        let mut missing = Vec::new();
        for id in blobs {
            if !self.objects.contains(&id)? {
                missing.push(id);
            }
        }
        if !missing.is_empty() {
            return Err(ServiceError::MissingObjects(missing));
        }

        for _ in 0..COMMIT_ATTEMPTS {
            let head = self.head_revision(repo)?;
            if req.base > head.seq {
                return Err(bad(format!("base revision {} is newer than head {}", req.base, head.seq)));
            }
            if req.base < head.seq {
                let touched = self.meta.touched_paths(repo, req.base, head.seq)?;
                let paths = conflicting_paths(&changes, &touched);
                if !paths.is_empty() {
                    return Err(ServiceError::Conflict {
                        message: format!(
                            "{} path(s) were changed after revision {}; sync and retry",
                            paths.len(),
                            req.base
                        ),
                        paths,
                    });
                }
            }
            let root_tree = apply_changes(&self.trees(), head.root_tree, &changes).map_err(|e| match e {
                budwood_core::Error::PathNotFound(ref path) | budwood_core::Error::PathConflict { ref path, .. } => {
                    ServiceError::Conflict { message: e.to_string(), paths: vec![path.clone()] }
                }
                budwood_core::Error::InvalidChanges(msg) => bad(msg),
                other => internal(other),
            })?;
            let changeset = Changeset {
                id: ObjectId::from_bytes([0; 32]),
                seq: head.seq + 1,
                parent: head.id,
                base: req.base,
                root_tree,
                author: req.author.clone(),
                message: req.message.clone(),
                timestamp: now(),
                changes: changes.clone(),
            }
            .seal();
            match self.meta.append(repo, &changeset)? {
                Ok(()) => {
                    let rebased = req.base < head.seq;
                    return Ok(CommitResponse { changeset, rebased });
                }
                Err(HeadMoved) => continue,
            }
        }
        Err(ServiceError::Unavailable("too many concurrent commits; retry".into()))
    }
}

fn revision_of(cs: &Changeset) -> Revision {
    Revision { seq: cs.seq, id: Some(cs.id), root_tree: cs.root_tree }
}

fn check_batch(ids: &[ObjectId]) -> ServiceResult<()> {
    if ids.len() > MAX_BATCH {
        return Err(ServiceError::BadRequest(format!("at most {MAX_BATCH} ids per request")));
    }
    Ok(())
}

/// Paths in `changes` that overlap something in `touched`: the same path, or
/// one being a directory that contains the other.
fn conflicting_paths(changes: &[Change], touched: &[RepoPath]) -> Vec<RepoPath> {
    let touched_paths: HashSet<&str> = touched.iter().map(|p| p.as_str()).collect();
    let touched_dirs: HashSet<&str> = touched.iter().flat_map(|p| p.ancestors()).collect();
    changes
        .iter()
        .map(|c| &c.path)
        .filter(|path| {
            touched_paths.contains(path.as_str())
                || touched_dirs.contains(path.as_str())
                || path.ancestors().any(|dir| touched_paths.contains(dir))
        })
        .cloned()
        .collect()
}

fn validate_repo_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 100 {
        return Err("must be 1-100 characters");
    }
    if name.starts_with('.') {
        return Err("must not start with '.'");
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err("may only contain ASCII letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use budwood_core::{FileKind, FileState};

    use super::*;
    use crate::store::MemObjectStore;

    fn service() -> Service {
        let svc = Service::new(Arc::new(MemObjectStore::default()), MetaStore::open_in_memory().unwrap()).unwrap();
        svc.create_repo("r").unwrap();
        svc
    }

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn upload(svc: &Service, content: &str) -> FileState {
        let blob = ObjectId::for_blob(content.as_bytes());
        svc.put_blob("r", &blob, content.as_bytes()).unwrap();
        FileState { kind: FileKind::File, blob }
    }

    fn commit(svc: &Service, base: u64, changes: Vec<Change>) -> ServiceResult<CommitResponse> {
        svc.commit("r", CommitRequest { base, author: "t".into(), message: "m".into(), changes })
    }

    #[test]
    fn commit_and_read_back() {
        let svc = service();
        let state = upload(&svc, "hello");
        let resp = commit(&svc, 0, vec![Change::set(path("dir/a.txt"), state)]).unwrap();
        assert_eq!(resp.changeset.seq, 1);
        assert!(!resp.rebased);
        assert!(resp.changeset.verify());
        assert_eq!(svc.read_file("r", "head", &path("dir/a.txt")).unwrap(), b"hello");
        assert_eq!(svc.tree("r", "1", &path("dir")).unwrap().entries.len(), 1);
        assert_eq!(svc.log("r", &path("dir"), None, 10).unwrap().len(), 1);
        assert_eq!(svc.log("r", &path("di"), None, 10).unwrap().len(), 0);
    }

    #[test]
    fn requires_uploaded_blobs() {
        let svc = service();
        let state = FileState { kind: FileKind::File, blob: ObjectId::for_blob(b"never uploaded") };
        assert!(matches!(
            commit(&svc, 0, vec![Change::set(path("a"), state)]),
            Err(ServiceError::MissingObjects(ids)) if ids == vec![state.blob]
        ));
    }

    #[test]
    fn rejects_mismatched_blob() {
        let svc = service();
        assert!(matches!(
            svc.put_blob("r", &ObjectId::for_blob(b"a"), b"b"),
            Err(ServiceError::BadRequest(_))
        ));
    }

    #[test]
    fn rebases_disjoint_changes_and_rejects_overlaps() {
        let svc = service();
        let one = upload(&svc, "1");
        let two = upload(&svc, "2");
        commit(&svc, 0, vec![Change::set(path("a/x"), one), Change::set(path("b/y"), one)]).unwrap();

        // Two authors start from revision 1.
        commit(&svc, 1, vec![Change::set(path("a/x"), two)]).unwrap();
        let rebased = commit(&svc, 1, vec![Change::set(path("b/y"), two)]).unwrap();
        assert!(rebased.rebased);
        assert_eq!(rebased.changeset.seq, 3);
        assert_eq!(svc.read_file("r", "3", &path("a/x")).unwrap(), b"2");
        assert_eq!(svc.read_file("r", "3", &path("b/y")).unwrap(), b"2");

        // Same file, and a file under a directory that was replaced, conflict.
        for p in ["a/x", "a/x/z"] {
            match commit(&svc, 1, vec![Change::set(path(p), one)]) {
                Err(ServiceError::Conflict { paths, .. }) => assert_eq!(paths, vec![path(p)]),
                other => panic!("expected conflict for {p}, got {other:?}"),
            }
        }
    }

    #[test]
    fn diff_between_revisions() {
        let svc = service();
        let one = upload(&svc, "1");
        commit(&svc, 0, vec![Change::set(path("a"), one)]).unwrap();
        commit(&svc, 1, vec![Change::delete(path("a")), Change::set(path("b"), one)]).unwrap();
        let diff = svc.diff("r", "1", "head", &RepoPath::root()).unwrap();
        assert_eq!(diff.changes, vec![Change::delete(path("a")), Change::set(path("b"), one)]);
    }

    #[test]
    fn fetch_frames_objects() {
        let svc = service();
        let state = upload(&svc, "abc");
        let framed = svc.fetch("r", &[state.blob]).unwrap();
        assert_eq!(framed.len(), 32 + 8 + 3);
        assert_eq!(&framed[..32], state.blob.as_bytes());
        assert_eq!(&framed[40..], b"abc");
    }

    #[test]
    fn repo_names() {
        let svc = service();
        assert!(!svc.create_repo("r").unwrap());
        for bad in ["", ".hidden", "a/b", "sp ace"] {
            assert!(svc.create_repo(bad).is_err(), "{bad:?}");
        }
        assert!(matches!(svc.resolve("nope", "head"), Err(ServiceError::NotFound(_))));
    }
}
