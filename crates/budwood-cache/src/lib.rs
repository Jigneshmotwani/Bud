//! Budwood's regional caching tier: a read-through cache of immutable,
//! content-addressed objects in front of an upstream budwood-server (or
//! another cache, so tiers chain).
//!
//! Object reads (`GET objects/{id}` and `POST objects/fetch`) are answered
//! from local disk when possible. Misses go upstream, are verified against
//! their ids, and are stored for next time; concurrent misses for the same
//! object share one upstream request. Every other request, including all
//! writes and everything mutable, is proxied upstream unchanged.
//!
//! Objects are cached globally, not per repository, and a hit is served
//! without asking upstream whether the repository exists or the caller may
//! read it. Authorization belongs upstream; serving it from the cache tier is
//! future work.

pub mod http;
pub mod store;
pub mod upstream;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use axum::body::Bytes;
use axum::http::{HeaderValue, StatusCode};
use budwood_core::{ObjectId, Tree};
use serde::{Deserialize, Serialize};
use tokio::sync::OnceCell;

use crate::store::DiskStore;
use crate::upstream::{parse_frames, Upstream};

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7272";

#[derive(Clone, Debug)]
pub struct Config {
    /// Base URL of the next tier up, e.g. `http://server:7171`.
    pub upstream: String,
    pub cache_dir: PathBuf,
    /// Size budget for cached objects; unbounded if `None`.
    pub max_cache_bytes: Option<u64>,
}

/// Counters reported by `GET /v1/cache/stats`. Hits and misses count
/// objects; `upstream_fetches` counts upstream requests for objects.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    /// Misses that waited on another request's in-flight fetch.
    pub coalesced: u64,
    pub upstream_fetches: u64,
    /// Objects from upstream that did not match their ids.
    pub rejected: u64,
    pub evictions: u64,
    pub objects_cached: u64,
    pub bytes_cached: u64,
    pub max_cache_bytes: Option<u64>,
}

/// Why an object could not be served.
#[derive(Clone, Debug, thiserror::Error)]
pub enum FetchError {
    /// Upstream answered with an error, which is passed through as is.
    #[error("upstream returned HTTP {status}")]
    Status { status: StatusCode, content_type: Option<HeaderValue>, body: Bytes },
    #[error("upstream returned bad data: {0}")]
    Corrupt(String),
    #[error("upstream request failed: {0}")]
    Transport(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}

/// True if `data` is the blob or the encoded tree named by `id`. An id does
/// not say which kind of object it names, so both are tried.
pub fn verify(id: &ObjectId, data: &[u8]) -> bool {
    ObjectId::for_blob(data) == *id || Tree::decode(data).is_ok_and(|tree| tree.id() == *id)
}

type Flight = Arc<OnceCell<Result<Bytes, FetchError>>>;

#[derive(Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    coalesced: AtomicU64,
    upstream_fetches: AtomicU64,
    rejected: AtomicU64,
}

pub struct Cache {
    upstream: Upstream,
    store: Arc<DiskStore>,
    /// Upstream fetches in progress, keyed by repository too so that one
    /// repository's error is never handed to another's readers.
    inflight: Mutex<HashMap<(String, ObjectId), Flight>>,
    counters: Counters,
    max_cache_bytes: Option<u64>,
}

impl Cache {
    pub fn open(config: &Config) -> anyhow::Result<Self> {
        let upstream = Upstream::new(&config.upstream)?;
        let store = DiskStore::open(&config.cache_dir, config.max_cache_bytes)
            .with_context(|| format!("opening cache in {}", config.cache_dir.display()))?;
        Ok(Self {
            upstream,
            store: Arc::new(store),
            inflight: Mutex::default(),
            counters: Counters::default(),
            max_cache_bytes: config.max_cache_bytes,
        })
    }

    pub fn upstream(&self) -> &Upstream {
        &self.upstream
    }

    pub fn stats(&self) -> CacheStats {
        let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
        let store = self.store.stats();
        CacheStats {
            hits: load(&self.counters.hits),
            misses: load(&self.counters.misses),
            coalesced: load(&self.counters.coalesced),
            upstream_fetches: load(&self.counters.upstream_fetches),
            rejected: load(&self.counters.rejected),
            evictions: store.evictions,
            objects_cached: store.objects,
            bytes_cached: store.bytes,
            max_cache_bytes: self.max_cache_bytes,
        }
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&DiskStore) -> T + Send + 'static,
    ) -> Result<T, FetchError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .map_err(|e| FetchError::Internal(format!("task failed: {e}")))
    }

    async fn lookup(&self, id: ObjectId) -> Result<Option<Bytes>, FetchError> {
        Ok(self.blocking(move |s| s.get(&id)).await?.map(Bytes::from))
    }

    /// Verifies objects from upstream and caches the good ones. Fails if any
    /// object does not match its id.
    async fn admit(&self, objects: Vec<(ObjectId, Bytes)>) -> Result<(), FetchError> {
        let bad = self
            .blocking(move |store| {
                for (id, data) in &objects {
                    if !verify(id, data) {
                        return Some(*id);
                    }
                    if let Err(e) = store.put(id, data) {
                        // Still servable; the next request just misses again.
                        tracing::warn!("caching object {id}: {e:#}");
                    }
                }
                None
            })
            .await?;
        match bad {
            Some(id) => {
                self.counters.rejected.fetch_add(1, Ordering::Relaxed);
                tracing::error!("upstream {} returned data that does not match object {id}", self.upstream.base());
                Err(FetchError::Corrupt(format!("object {id} failed verification")))
            }
            None => Ok(()),
        }
    }

    /// One object, from disk or upstream. The second value is true for a hit.
    pub async fn object(&self, repo: &str, id: ObjectId) -> Result<(Bytes, bool), FetchError> {
        if let Some(data) = self.lookup(id).await? {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
            return Ok((data, true));
        }
        self.counters.misses.fetch_add(1, Ordering::Relaxed);

        let key = (repo.to_string(), id);
        let flight = {
            let mut inflight = self.inflight.lock().unwrap();
            let flight = inflight.entry(key.clone()).or_default();
            if Arc::strong_count(flight) > 1 {
                self.counters.coalesced.fetch_add(1, Ordering::Relaxed);
            }
            flight.clone()
        };
        let guard = FlightGuard { cache: self, key, flight };
        // If the leading request is cancelled, a waiting one takes over.
        let result = guard
            .flight
            .get_or_init(|| async {
                // A fetch that finished just before we joined may have filled the cache.
                if let Some(data) = self.lookup(id).await? {
                    return Ok(data);
                }
                self.counters.upstream_fetches.fetch_add(1, Ordering::Relaxed);
                let data = self.upstream.get_object(repo, &id).await?;
                self.admit(vec![(id, data.clone())]).await?;
                Ok(data)
            })
            .await;
        result.clone().map(|data| (data, false))
    }

    /// Several objects, in request order, fetching all misses from upstream
    /// in one request.
    pub async fn objects(&self, repo: &str, ids: &[ObjectId]) -> Result<Vec<(ObjectId, Bytes)>, FetchError> {
        let mut unique: Vec<ObjectId> = Vec::with_capacity(ids.len());
        let mut seen = HashSet::new();
        unique.extend(ids.iter().filter(|id| seen.insert(**id)));

        let lookups = unique.clone();
        let mut found: HashMap<ObjectId, Bytes> = self
            .blocking(move |s| lookups.into_iter().filter_map(|id| Some((id, Bytes::from(s.get(&id)?)))).collect())
            .await?;
        let missing: Vec<ObjectId> = unique.into_iter().filter(|id| !found.contains_key(id)).collect();
        self.counters.hits.fetch_add(found.len() as u64, Ordering::Relaxed);
        self.counters.misses.fetch_add(missing.len() as u64, Ordering::Relaxed);

        if !missing.is_empty() {
            self.counters.upstream_fetches.fetch_add(1, Ordering::Relaxed);
            let body = self.upstream.fetch(repo, &missing).await?;
            let frames = parse_frames(&body).map_err(|e| FetchError::Corrupt(e.to_string()))?;
            let wanted: HashSet<ObjectId> = missing.iter().copied().collect();
            let mut fetched = Vec::with_capacity(frames.len());
            for (id, data) in frames {
                if !wanted.contains(&id) {
                    return Err(FetchError::Corrupt(format!("unrequested object {id} in batch response")));
                }
                fetched.push((id, data));
            }
            self.admit(fetched.clone()).await?;
            found.extend(fetched);
            if let Some(id) = missing.iter().find(|id| !found.contains_key(id)) {
                return Err(FetchError::Corrupt(format!("batch response is missing object {id}")));
            }
        }
        Ok(ids.iter().map(|id| (*id, found[id].clone())).collect())
    }
}

/// Removes a finished (or abandoned) flight from the in-flight table.
struct FlightGuard<'a> {
    cache: &'a Cache,
    key: (String, ObjectId),
    flight: Flight,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        let mut inflight = self.cache.inflight.lock().unwrap();
        let Some(current) = inflight.get(&self.key) else { return };
        // Keep an unfinished flight that other requests are still waiting on.
        // Each waiter holds one reference, and the table another.
        let others_waiting = Arc::strong_count(current) > 2;
        if Arc::ptr_eq(current, &self.flight) && (self.flight.initialized() || !others_waiting) {
            inflight.remove(&self.key);
        }
    }
}

/// Serves the cache on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    cache: Arc<Cache>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, http::router(cache)).with_graceful_shutdown(shutdown).await
}
