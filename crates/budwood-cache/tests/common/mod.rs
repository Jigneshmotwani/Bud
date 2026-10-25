//! Shared helpers: in-process servers, caches and fake upstreams on
//! ephemeral localhost ports. Call these from inside a tokio runtime.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use budwood_cache::{Cache, CacheStats, Config};
use budwood_core::api::{CommitRequest, CommitResponse};
use budwood_core::{Change, FileKind, FileState, ObjectId, RepoPath};
use budwood_server::{Service, DEFAULT_MAX_BODY_BYTES};
use tempfile::TempDir;

pub const REPO: &str = "demo";

pub struct TestServer {
    pub url: String,
    pub service: Arc<Service>,
    _dir: TempDir,
}

/// A real budwood-server with one repository, [`REPO`].
pub async fn start_server() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let service = Arc::new(Service::open(dir.path()).unwrap());
    service.create_repo(REPO).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(budwood_server::serve(listener, service.clone(), DEFAULT_MAX_BODY_BYTES, std::future::pending()));
    TestServer { url, service, _dir: dir }
}

pub struct TestCache {
    pub url: String,
    pub cache: Arc<Cache>,
    _dir: TempDir,
}

impl TestCache {
    pub fn stats(&self) -> CacheStats {
        self.cache.stats()
    }
}

pub async fn start_cache(upstream: &str, max_cache_bytes: Option<u64>) -> TestCache {
    let dir = tempfile::tempdir().unwrap();
    let config = Config { upstream: upstream.to_string(), cache_dir: dir.path().join("cache"), max_cache_bytes };
    let cache = Arc::new(Cache::open(&config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(budwood_cache::serve(listener, cache.clone(), std::future::pending()));
    TestCache { url, cache, _dir: dir }
}

/// Serves `router` as a stand-in upstream; returns its base URL.
pub async fn start_fake(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

pub fn repo_url(base: &str, rest: &str) -> String {
    format!("{base}/v1/repos/{REPO}/{rest}")
}

/// Commits `files` directly on the server, uploading their blobs first.
pub fn commit_files(service: &Service, files: &[(&str, &[u8])]) -> CommitResponse {
    let base = service.resolve(REPO, "head").unwrap().seq;
    let mut changes = Vec::new();
    for (path, data) in files {
        let blob = ObjectId::for_blob(data);
        service.put_blob(REPO, &blob, data).unwrap();
        changes.push(Change::set(RepoPath::new(*path).unwrap(), FileState { kind: FileKind::File, blob }));
    }
    let req = CommitRequest { base, author: "tester".into(), message: "test commit".into(), changes };
    service.commit(REPO, req).unwrap()
}

/// Splits a batch fetch response into frames.
pub fn frames(mut body: &[u8]) -> Vec<(ObjectId, Vec<u8>)> {
    let mut out = Vec::new();
    while !body.is_empty() {
        let id = ObjectId::from_bytes(body[..32].try_into().unwrap());
        let len = u64::from_le_bytes(body[32..40].try_into().unwrap()) as usize;
        out.push((id, body[40..40 + len].to_vec()));
        body = &body[40 + len..];
    }
    out
}
