//! Shared helpers for end-to-end tests: an in-process server on a loopback
//! port, and small filesystem utilities.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use budwood_client::{LocalChange, Remote, Workspace};
use budwood_core::RepoPath;
use budwood_server::{serve, Service, DEFAULT_MAX_BODY_BYTES};
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// A real server running on its own runtime. Client calls are blocking, so
/// tests make them from the test thread, never from inside the runtime.
pub struct TestServer {
    pub url: String,
    runtime: Option<Runtime>,
    owned_data: Option<TempDir>,
}

impl TestServer {
    pub fn start() -> Self {
        let data = tempfile::tempdir().unwrap();
        let mut server = Self::start_at(data.path());
        server.owned_data = Some(data);
        server
    }

    /// Starts a server on an existing data directory, which the caller keeps.
    pub fn start_at(data: &Path) -> Self {
        let service = Arc::new(Service::open(data).unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            serve(listener, service, DEFAULT_MAX_BODY_BYTES, std::future::pending()).await.unwrap();
        });
        Self { url, runtime: Some(runtime), owned_data: None }
    }

    /// Starts a server with one repository named `repo`.
    pub fn with_repo(repo: &str) -> Self {
        let server = Self::start();
        assert!(Remote::create_repo(&server.url, repo).unwrap());
        server
    }

    pub fn remote(&self, repo: &str) -> Remote {
        Remote::new(&self.url, repo).unwrap()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

pub fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap()
}

pub fn clone(server: &TestServer, repo: &str, dir: &Path) -> Workspace {
    Workspace::clone(&server.url, repo, dir, vec![]).unwrap()
}

/// Writes a file under `root`, creating parent directories.
pub fn write(root: &Path, rel: &str, content: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

pub fn read(root: &Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

pub fn exists(root: &Path, rel: &str) -> bool {
    fs::symlink_metadata(root.join(rel)).is_ok()
}

/// `status` as `["A path", "M path", ...]`.
pub fn status(ws: &mut Workspace) -> Vec<String> {
    summarize(&ws.status().unwrap())
}

pub fn summarize(changes: &[LocalChange]) -> Vec<String> {
    changes.iter().map(|c| format!("{} {}", c.kind.letter(), c.path)).collect()
}

/// Every file under `root` except `.bud/`, as sorted relative paths.
pub fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let p: PathBuf = entry.path();
            if p == root.join(".bud") {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                walk(root, &p, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_str().unwrap().replace('\\', "/"));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}
