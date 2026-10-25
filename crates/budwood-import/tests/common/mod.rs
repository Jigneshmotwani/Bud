//! Shared helpers: an in-process Budwood server and scripted git repositories.

#![allow(dead_code)]

use std::cell::Cell;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use budwood_client::Remote;
use budwood_core::RepoPath;
use budwood_server::{serve, Service, DEFAULT_MAX_BODY_BYTES};
use tempfile::TempDir;
use tokio::runtime::Runtime;

/// A real server on its own runtime. Client calls are blocking, so tests make
/// them from the test thread, never from inside the runtime.
pub struct TestServer {
    pub url: String,
    runtime: Option<Runtime>,
    _data: TempDir,
}

impl TestServer {
    pub fn start() -> Self {
        let data = tempfile::tempdir().unwrap();
        let service = Arc::new(Service::open(data.path()).unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            serve(listener, service, DEFAULT_MAX_BODY_BYTES, std::future::pending()).await.unwrap();
        });
        Self { url, runtime: Some(runtime), _data: data }
    }

    /// A remote for a new, empty repository.
    pub fn repo(&self, name: &str) -> Remote {
        assert!(Remote::create_repo(&self.url, name).unwrap());
        Remote::new(&self.url, name).unwrap()
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

/// A git repository in a temporary directory, isolated from the user's git
/// configuration, whose commits get deterministic, increasing dates.
pub struct GitRepo {
    pub dir: TempDir,
    clock: Cell<u64>,
}

impl GitRepo {
    pub fn new() -> Self {
        let repo = Self { dir: tempfile::tempdir().unwrap(), clock: Cell::new(1_700_000_000) };
        repo.git(&["init", "-q", "-b", "main"]);
        repo
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs git and returns its trimmed stdout, panicking on failure.
    pub fn git(&self, args: &[&str]) -> String {
        let date = format!("{} +0200", self.clock.get());
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@x", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(self.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    pub fn set_executable(&self, rel: &str, executable: bool) {
        use std::os::unix::fs::PermissionsExt;
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(self.path().join(rel), fs::Permissions::from_mode(mode)).unwrap();
    }

    pub fn symlink(&self, target: &str, rel: &str) {
        std::os::unix::fs::symlink(target, self.path().join(rel)).unwrap();
    }

    pub fn remove(&self, rel: &str) {
        let p = self.path().join(rel);
        if p.is_dir() {
            fs::remove_dir_all(p).unwrap();
        } else {
            fs::remove_file(p).unwrap();
        }
    }

    /// Stages everything and commits; returns the new commit id.
    pub fn commit(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.commit_index(message)
    }

    /// Commits the index as it is.
    pub fn commit_index(&self, message: &str) -> String {
        self.clock.set(self.clock.get() + 60);
        self.git(&["commit", "-q", "--allow-empty", "-m", message]);
        self.head()
    }

    pub fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }
}
