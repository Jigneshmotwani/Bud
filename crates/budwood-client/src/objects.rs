//! A local, verified cache of immutable objects in front of a [`Remote`].
//!
//! Every object is checked against its id before it is cached, so a
//! misbehaving server or proxy cannot corrupt a working copy.

use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{bail, Context, Result};
use budwood_core::{empty_tree_id, ObjectId, Tree, TreeSource};

use crate::remote::Remote;

pub struct Objects {
    remote: Remote,
    dir: PathBuf,
    tmp_counter: AtomicU64,
}

impl Objects {
    pub fn new(remote: Remote, dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { remote, dir, tmp_counter: AtomicU64::new(0) })
    }

    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    fn path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    fn cached(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(id)) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading object cache"),
        }
    }

    /// Adds an object the caller has already verified.
    pub fn store(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        let path = self.path(id);
        if path.exists() {
            return Ok(());
        }
        fs::create_dir_all(path.parent().unwrap())?;
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.dir.join(format!("tmp-{}-{n}", std::process::id()));
        fs::write(&tmp, data)?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn blob(&self, id: &ObjectId) -> Result<Vec<u8>> {
        if let Some(data) = self.cached(id)? {
            return Ok(data);
        }
        let data = self.remote.get_object(id)?;
        if ObjectId::for_blob(&data) != *id {
            bail!("server returned corrupt blob {id}");
        }
        self.store(id, &data)?;
        Ok(data)
    }

    /// Ensures all `ids` are cached, fetching the missing ones in batches.
    pub fn prefetch_blobs(&self, ids: impl IntoIterator<Item = ObjectId>) -> Result<()> {
        let mut seen = HashSet::new();
        let wanted: Vec<ObjectId> = ids
            .into_iter()
            .filter(|id| seen.insert(*id) && !self.path(id).exists())
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        for (id, data) in self.remote.fetch(&wanted)? {
            if !seen.contains(&id) || ObjectId::for_blob(&data) != id {
                bail!("server returned corrupt or unrequested blob {id}");
            }
            self.store(&id, &data)?;
        }
        Ok(())
    }

    fn tree(&self, id: &ObjectId) -> Result<Tree> {
        if *id == empty_tree_id() {
            return Ok(Tree::new());
        }
        let (bytes, from_cache) = match self.cached(id)? {
            Some(bytes) => (bytes, true),
            None => (self.remote.get_object(id)?, false),
        };
        let tree = Tree::decode(&bytes)?;
        if tree.id() != *id {
            bail!("tree {id} failed verification");
        }
        if !from_cache {
            self.store(id, &bytes)?;
        }
        Ok(tree)
    }
}

impl TreeSource for Objects {
    fn get_tree(&self, id: &ObjectId) -> budwood_core::Result<Tree> {
        self.tree(id).map_err(|e| budwood_core::Error::Storage(format!("{e:#}")))
    }
}
