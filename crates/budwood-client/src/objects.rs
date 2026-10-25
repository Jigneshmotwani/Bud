//! A local, verified, bounded cache of immutable objects in front of a [`Remote`].
//!
//! Every object is checked against its id before it is cached, so a
//! misbehaving server or proxy cannot corrupt a working copy.
//!
//! The cache has a size budget. Every object in it came from (or was uploaded
//! to) the server, so any of them can be evicted and fetched again later; all
//! reads go through [`Objects::blob`] or the [`TreeSource`] impl, which fall
//! back to the server on a miss. Recency is the file's mtime, refreshed on a
//! cache hit at most once per [`TOUCH_INTERVAL`]. The cache's total size is
//! tracked approximately in a small sidecar file (`objects/usage`), so checking
//! the budget is cheap; only going over it costs a scan of the cache.
//!
//! Eviction runs only from [`Objects::trim`] and the explicit `gc` calls,
//! which callers invoke at points where nothing fetched earlier in the
//! operation is still waiting to be used (between checkout batches, at the
//! end of an operation), so an object is never evicted between being
//! prefetched and being read.

use std::collections::HashSet;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use budwood_core::{empty_tree_id, ObjectId, Tree, TreeSource};

use crate::remote::Remote;

/// The default cache budget: 2 GiB.
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 2 << 30;

/// A cache hit refreshes the object's mtime only if it is older than this, so
/// hot objects don't cost a metadata write on every read.
pub const TOUCH_INTERVAL: Duration = Duration::from_secs(60);

/// Temporary files older than this are leftovers of a crashed process.
const STALE_TMP: Duration = Duration::from_secs(3600);

const USAGE_FILE: &str = "usage";

/// The size of the local object cache.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheUsage {
    pub objects: u64,
    pub bytes: u64,
}

/// What a garbage collection of the cache did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheGc {
    pub freed_objects: u64,
    pub freed_bytes: u64,
    /// The cache after collection.
    pub remaining: CacheUsage,
}

pub struct Objects {
    remote: Remote,
    dir: PathBuf,
    tmp_counter: AtomicU64,
    max_bytes: u64,
    usage: Mutex<Usage>,
}

/// The approximate cache size: the last value known on disk plus what this
/// handle has stored since.
#[derive(Default)]
struct Usage {
    base: Option<u64>,
    added: u64,
}

impl Objects {
    pub fn new(remote: Remote, dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            remote,
            dir,
            tmp_counter: AtomicU64::new(0),
            max_bytes: DEFAULT_CACHE_MAX_BYTES,
            usage: Mutex::new(Usage::default()),
        })
    }

    /// Sets the cache budget in bytes.
    pub fn set_max_bytes(&mut self, max_bytes: u64) {
        self.max_bytes = max_bytes;
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
    }

    pub fn remote(&self) -> &Remote {
        &self.remote
    }

    fn path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    fn cached(&self, id: &ObjectId) -> Result<Option<Vec<u8>>> {
        let mut file = match fs::File::open(self.path(id)) {
            Ok(file) => file,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading object cache"),
        };
        let mut data = Vec::new();
        file.read_to_end(&mut data).context("reading object cache")?;
        touch(&file);
        Ok(Some(data))
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
        self.usage.lock().unwrap().added += data.len() as u64;
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
        let wanted: Vec<ObjectId> = ids.into_iter().filter(|id| seen.insert(*id) && !self.path(id).exists()).collect();
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

    /// Scans the cache and returns its exact size.
    pub fn cache_usage(&self) -> Result<CacheUsage> {
        let entries = self.scan()?;
        Ok(CacheUsage { objects: entries.len() as u64, bytes: entries.iter().map(|e| e.size).sum() })
    }

    /// Enforces the budget if this handle stored anything since the last
    /// check: when the estimated size is over budget, evicts least recently
    /// used objects down to 80% of it. Cheap when under budget.
    ///
    /// Call this only when no object fetched earlier in the current operation
    /// is still waiting to be read.
    pub fn trim(&self) -> Result<CacheGc> {
        let mut usage = self.usage.lock().unwrap();
        if usage.added == 0 {
            return Ok(CacheGc::default());
        }
        let base = match usage.base {
            Some(base) => base,
            None => match self.read_usage_file() {
                Some(base) => base,
                None => {
                    // Unknown (e.g. a cache from before the sidecar existed):
                    // the scan already includes what this handle added.
                    let exact = self.cache_usage()?.bytes;
                    usage.added = 0;
                    exact
                }
            },
        };
        let estimate = base.saturating_add(usage.added);
        usage.added = 0;
        if estimate <= self.max_bytes {
            usage.base = Some(estimate);
            self.write_usage_file(estimate);
            return Ok(CacheGc::default());
        }
        let gc = self.evict_to(self.low_water())?;
        usage.base = Some(gc.remaining.bytes);
        Ok(gc)
    }

    /// Like [`Objects::trim`], but always scans, so it also corrects the size
    /// estimate and cleans up leftover temporary files.
    pub fn gc(&self) -> Result<CacheGc> {
        let mut usage = self.usage.lock().unwrap();
        let current = self.cache_usage()?;
        let gc = if current.bytes > self.max_bytes {
            self.evict_to(self.low_water())?
        } else {
            self.write_usage_file(current.bytes);
            CacheGc { remaining: current, ..CacheGc::default() }
        };
        *usage = Usage { base: Some(gc.remaining.bytes), added: 0 };
        Ok(gc)
    }

    /// Evicts least recently used objects until the cache holds at most
    /// `target` bytes.
    pub fn gc_to(&self, target: u64) -> Result<CacheGc> {
        let mut usage = self.usage.lock().unwrap();
        let gc = self.evict_to(target)?;
        *usage = Usage { base: Some(gc.remaining.bytes), added: 0 };
        Ok(gc)
    }

    fn low_water(&self) -> u64 {
        self.max_bytes - self.max_bytes / 5
    }

    fn evict_to(&self, target: u64) -> Result<CacheGc> {
        let mut entries = self.scan()?;
        // Oldest first; ties broken by path so eviction is deterministic.
        entries.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
        let mut total: u64 = entries.iter().map(|e| e.size).sum();
        let mut gc = CacheGc::default();
        let mut kept = entries.len() as u64;
        for entry in &entries {
            if total <= target {
                break;
            }
            match fs::remove_file(&entry.path) {
                Ok(()) => {}
                // Someone else removed it; it's gone either way.
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("evicting {}", entry.path.display())),
            }
            total -= entry.size;
            kept -= 1;
            gc.freed_bytes += entry.size;
            gc.freed_objects += 1;
            if let Some(parent) = entry.path.parent() {
                // Succeeds only once the fan-out directory is empty.
                let _ = fs::remove_dir(parent);
            }
        }
        gc.remaining = CacheUsage { objects: kept, bytes: total };
        self.write_usage_file(total);
        Ok(gc)
    }

    /// Every cached object, plus cleanup of stale temporary files.
    fn scan(&self) -> Result<Vec<CacheEntry>> {
        let mut out = Vec::new();
        let now = SystemTime::now();
        for entry in read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let file_type = entry.file_type()?;
            if file_type.is_dir() && name.len() == 2 {
                for object in read_dir(&entry.path())? {
                    let object = object?;
                    let meta = match object.metadata() {
                        Ok(meta) if meta.is_file() => meta,
                        Ok(_) => continue,
                        Err(e) if e.kind() == ErrorKind::NotFound => continue,
                        Err(e) => return Err(e.into()),
                    };
                    let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    out.push(CacheEntry { path: object.path(), size: meta.len(), mtime });
                }
            } else if file_type.is_file() && name.starts_with("tmp-") {
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > STALE_TMP);
                if stale {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(out)
    }

    fn read_usage_file(&self) -> Option<u64> {
        fs::read_to_string(self.dir.join(USAGE_FILE)).ok()?.trim().parse().ok()
    }

    /// Best effort: a lost or stale estimate is corrected by the next scan.
    fn write_usage_file(&self, bytes: u64) {
        let tmp = self.dir.join(format!("tmp-usage-{}", std::process::id()));
        if fs::write(&tmp, format!("{bytes}\n")).is_ok() {
            let _ = fs::rename(&tmp, self.dir.join(USAGE_FILE));
        }
    }
}

struct CacheEntry {
    path: PathBuf,
    size: u64,
    mtime: SystemTime,
}

fn read_dir(dir: &Path) -> Result<fs::ReadDir> {
    fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))
}

/// Marks a cache hit as recently used. Best effort: recency only steers
/// which objects are evicted first.
fn touch(file: &fs::File) {
    let now = SystemTime::now();
    let stale = file
        .metadata()
        .and_then(|m| m.modified())
        .map_or(true, |t| now.duration_since(t).unwrap_or_default() > TOUCH_INTERVAL || t > now + TOUCH_INTERVAL);
    if stale {
        let _ = file.set_modified(now);
    }
}

impl TreeSource for Objects {
    fn get_tree(&self, id: &ObjectId) -> budwood_core::Result<Tree> {
        self.tree(id).map_err(|e| budwood_core::Error::Storage(format!("{e:#}")))
    }
}
