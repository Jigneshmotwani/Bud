//! Content-addressed object storage.

use std::collections::HashMap;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use anyhow::{Context, Result};
use budwood_core::{empty_tree_id, ObjectId, Tree, TreeSink, TreeSource};

/// Immutable bytes keyed by their id. Callers verify ids before `put`, so
/// implementations only need to store and return bytes. Writing the same id
/// twice must be harmless.
pub trait ObjectStore: Send + Sync + 'static {
    fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>>;
    fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()>;
    fn contains(&self, id: &ObjectId) -> Result<bool>;
}

/// Objects as files under `root/ab/cdef...`, written atomically via rename.
pub struct FsObjectStore {
    root: PathBuf,
    tmp_counter: AtomicU64,
}

impl FsObjectStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp")).with_context(|| format!("creating {}", root.display()))?;
        Ok(Self { root, tmp_counter: AtomicU64::new(0) })
    }

    fn path_for(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }
}

impl ObjectStore for FsObjectStore {
    fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path_for(id)) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading object {id}")),
        }
    }

    fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        let path = self.path_for(id);
        if path.exists() {
            return Ok(());
        }
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.root.join("tmp").join(format!("{}-{}-{n}", id.short(), std::process::id()));
        write_synced(&tmp, data).with_context(|| format!("writing object {id}"))?;
        fs::create_dir_all(path.parent().unwrap())?;
        fs::rename(&tmp, &path).with_context(|| format!("storing object {id}"))?;
        Ok(())
    }

    fn contains(&self, id: &ObjectId) -> Result<bool> {
        Ok(self.path_for(id).exists())
    }
}

fn write_synced(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// An in-memory store, for tests.
#[derive(Default)]
pub struct MemObjectStore(RwLock<HashMap<ObjectId, Vec<u8>>>);

impl ObjectStore for MemObjectStore {
    fn get(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        Ok(self.0.read().unwrap().get(id).cloned())
    }

    fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        self.0.write().unwrap().entry(*id).or_insert_with(|| data.to_vec());
        Ok(())
    }

    fn contains(&self, id: &ObjectId) -> Result<bool> {
        Ok(self.0.read().unwrap().contains_key(id))
    }
}

/// Adapts an [`ObjectStore`] to the tree traits used by the core algorithms.
pub struct Trees<'a>(pub &'a dyn ObjectStore);

impl TreeSource for Trees<'_> {
    fn get_tree(&self, id: &ObjectId) -> budwood_core::Result<Tree> {
        if *id == empty_tree_id() {
            return Ok(Tree::new());
        }
        let bytes = self
            .0
            .get(id)
            .map_err(|e| budwood_core::Error::Storage(format!("{e:#}")))?
            .ok_or(budwood_core::Error::ObjectNotFound(*id))?;
        Tree::decode(&bytes)
    }
}

impl TreeSink for Trees<'_> {
    fn put_tree(&self, tree: &Tree) -> budwood_core::Result<ObjectId> {
        let bytes = tree.encode();
        let id = ObjectId::for_tree(&bytes);
        self.0
            .put(&id, &bytes)
            .map_err(|e| budwood_core::Error::Storage(format!("{e:#}")))?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_store_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsObjectStore::open(dir.path()).unwrap();
        let id = ObjectId::for_blob(b"hello");
        assert!(!store.contains(&id).unwrap());
        assert_eq!(store.get(&id).unwrap(), None);
        store.put(&id, b"hello").unwrap();
        store.put(&id, b"hello").unwrap();
        assert!(store.contains(&id).unwrap());
        assert_eq!(store.get(&id).unwrap().as_deref(), Some(&b"hello"[..]));
    }
}
