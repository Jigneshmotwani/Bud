//! The on-disk object cache, optionally bounded in size.
//!
//! Objects live at `<dir>/objects/<2 hex>/<62 hex>` and are written through a
//! temporary file and a rename, so a reader never sees a partial object. An
//! in-memory index tracks sizes and recency; when the cache grows past its
//! budget, the least recently used objects are deleted. Recency is not
//! persisted: after a restart, objects are ordered by modification time.
//!
//! Every method does blocking file I/O; async callers run them on the
//! blocking thread pool.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result};
use budwood_core::ObjectId;

pub struct DiskStore {
    objects: PathBuf,
    tmp: PathBuf,
    max_bytes: Option<u64>,
    index: Mutex<Index>,
    tmp_counter: AtomicU64,
}

/// Counters describing what the store holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct StoreStats {
    pub objects: u64,
    pub bytes: u64,
    pub evictions: u64,
}

#[derive(Default)]
struct Index {
    entries: HashMap<ObjectId, Entry>,
    /// Access tick -> object, oldest first.
    lru: BTreeMap<u64, ObjectId>,
    clock: u64,
    bytes: u64,
    evictions: u64,
}

struct Entry {
    size: u64,
    tick: u64,
}

impl Index {
    fn insert(&mut self, id: ObjectId, size: u64) {
        if self.entries.contains_key(&id) {
            return;
        }
        let tick = self.next_tick();
        self.entries.insert(id, Entry { size, tick });
        self.lru.insert(tick, id);
        self.bytes += size;
    }

    fn touch(&mut self, id: &ObjectId) -> bool {
        let tick = self.next_tick();
        let Some(entry) = self.entries.get_mut(id) else { return false };
        self.lru.remove(&entry.tick);
        entry.tick = tick;
        self.lru.insert(tick, *id);
        true
    }

    fn remove(&mut self, id: &ObjectId) -> bool {
        let Some(entry) = self.entries.remove(id) else { return false };
        self.lru.remove(&entry.tick);
        self.bytes -= entry.size;
        true
    }

    /// Drops least recently used entries, never `keep`, until the total fits
    /// in `max`. Returns the dropped ids; the caller deletes their files.
    fn evict(&mut self, max: u64, keep: Option<&ObjectId>) -> Vec<ObjectId> {
        let mut victims = Vec::new();
        let mut ticks = self.lru.iter();
        while self.bytes > max {
            let Some((_, id)) = ticks.next() else { break };
            if Some(id) == keep {
                continue;
            }
            let size = self.entries[id].size;
            self.bytes -= size;
            victims.push(*id);
        }
        for id in &victims {
            // The bytes were already subtracted above.
            let entry = self.entries.remove(id).expect("victim is indexed");
            self.lru.remove(&entry.tick);
        }
        self.evictions += victims.len() as u64;
        victims
    }

    fn next_tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

impl DiskStore {
    /// Opens (creating if needed) a cache in `dir`, indexing what is already
    /// there and evicting down to `max_bytes`.
    pub fn open(dir: &Path, max_bytes: Option<u64>) -> Result<Self> {
        let objects = dir.join("objects");
        let tmp = dir.join("tmp");
        fs::create_dir_all(&objects).with_context(|| format!("creating {}", objects.display()))?;
        // Leftovers from writes interrupted by a crash.
        if tmp.exists() {
            fs::remove_dir_all(&tmp).with_context(|| format!("clearing {}", tmp.display()))?;
        }
        fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;

        let mut found = scan(&objects).with_context(|| format!("scanning {}", objects.display()))?;
        found.sort_by_key(|&(_, _, mtime)| mtime);
        let mut index = Index::default();
        for (id, size, _) in found {
            index.insert(id, size);
        }
        let store =
            Self { objects, tmp, max_bytes, index: Mutex::new(Index::default()), tmp_counter: AtomicU64::new(0) };
        let victims = max_bytes.map(|max| index.evict(max, None)).unwrap_or_default();
        *store.index.lock().unwrap() = index;
        store.delete_files(&victims);
        Ok(store)
    }

    fn path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.to_hex();
        self.objects.join(&hex[..2]).join(&hex[2..])
    }

    pub fn contains(&self, id: &ObjectId) -> bool {
        self.index.lock().unwrap().entries.contains_key(id)
    }

    /// Reads a cached object, marking it recently used.
    pub fn get(&self, id: &ObjectId) -> Option<Vec<u8>> {
        if !self.index.lock().unwrap().touch(id) {
            return None;
        }
        match fs::read(self.path(id)) {
            Ok(data) => Some(data),
            Err(e) => {
                // Evicted concurrently, or deleted behind our back.
                if e.kind() != ErrorKind::NotFound {
                    tracing::warn!("reading cached object {id}: {e}");
                }
                self.index.lock().unwrap().remove(id);
                None
            }
        }
    }

    /// Adds an object the caller has already verified against `id`. Objects
    /// larger than the whole budget are not cached.
    pub fn put(&self, id: &ObjectId, data: &[u8]) -> Result<()> {
        let size = data.len() as u64;
        if self.contains(id) || self.max_bytes.is_some_and(|max| size > max) {
            return Ok(());
        }
        let path = self.path(id);
        fs::create_dir_all(path.parent().unwrap())?;
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = self.tmp.join(format!("{}-{n}", id.to_hex()));
        fs::write(&tmp, data).with_context(|| format!("writing {}", tmp.display()))?;
        if let Err(e) = fs::rename(&tmp, &path) {
            let _ = fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("renaming into {}", path.display()));
        }
        let victims = {
            let mut index = self.index.lock().unwrap();
            index.insert(*id, size);
            self.max_bytes.map(|max| index.evict(max, Some(id))).unwrap_or_default()
        };
        self.delete_files(&victims);
        Ok(())
    }

    fn delete_files(&self, ids: &[ObjectId]) {
        for id in ids {
            match fs::remove_file(self.path(id)) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                Err(e) => tracing::warn!("evicting {id}: {e}"),
            }
        }
    }

    pub fn stats(&self) -> StoreStats {
        let index = self.index.lock().unwrap();
        StoreStats { objects: index.entries.len() as u64, bytes: index.bytes, evictions: index.evictions }
    }
}

/// Every well-named object file under `objects`, with its size and mtime.
fn scan(objects: &Path) -> Result<Vec<(ObjectId, u64, SystemTime)>> {
    let mut found = Vec::new();
    for shard in fs::read_dir(objects)? {
        let shard = shard?;
        let Some(prefix) = shard.file_name().to_str().map(str::to_owned) else { continue };
        if prefix.len() != 2 || !shard.file_type()?.is_dir() {
            continue;
        }
        for file in fs::read_dir(shard.path())? {
            let file = file?;
            let Some(rest) = file.file_name().to_str().map(str::to_owned) else { continue };
            let Ok(id) = ObjectId::from_hex(&format!("{prefix}{rest}")) else { continue };
            let meta = file.metadata()?;
            if meta.is_file() {
                found.push((id, meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)));
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        ObjectId::for_blob(&[n])
    }

    #[test]
    fn put_get_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(dir.path(), None).unwrap();
        assert!(store.get(&id(1)).is_none());
        store.put(&id(1), b"one").unwrap();
        assert_eq!(store.get(&id(1)).unwrap(), b"one");
        assert_eq!(store.stats().bytes, 3);
        drop(store);

        let store = DiskStore::open(dir.path(), None).unwrap();
        assert_eq!(store.get(&id(1)).unwrap(), b"one");
        assert_eq!(store.stats().objects, 1);
    }

    #[test]
    fn evicts_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(dir.path(), Some(10)).unwrap();
        store.put(&id(1), &[0; 4]).unwrap();
        store.put(&id(2), &[0; 4]).unwrap();
        store.get(&id(1)).unwrap();
        store.put(&id(3), &[0; 4]).unwrap();
        assert!(store.contains(&id(1)));
        assert!(!store.contains(&id(2)));
        assert!(store.contains(&id(3)));
        let stats = store.stats();
        assert_eq!((stats.objects, stats.bytes, stats.evictions), (2, 8, 1));
        assert!(!store.path(&id(2)).exists());

        // Too big to ever fit: served by the caller, but not cached.
        store.put(&id(4), &[0; 11]).unwrap();
        assert!(!store.contains(&id(4)));
    }

    #[test]
    fn reopening_with_a_smaller_budget_evicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(dir.path(), None).unwrap();
        for n in 0..5 {
            store.put(&id(n), &[n; 10]).unwrap();
        }
        drop(store);
        let store = DiskStore::open(dir.path(), Some(25)).unwrap();
        assert_eq!(store.stats().objects, 2);
        assert_eq!(store.stats().bytes, 20);
    }

    #[test]
    fn missing_file_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(dir.path(), None).unwrap();
        store.put(&id(1), b"one").unwrap();
        fs::remove_file(store.path(&id(1))).unwrap();
        assert!(store.get(&id(1)).is_none());
        assert_eq!(store.stats().bytes, 0);
    }
}
