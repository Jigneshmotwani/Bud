//! Workspace robustness: the workspace lock, retrying failed clones, and path
//! arguments.

mod common;

use std::fs;
use std::path::Path;

use budwood_client::{Workspace, WorkspaceLocked};
use budwood_core::{ObjectId, RepoPath};
use common::*;

const REPO: &str = "test";

fn locked_error(err: anyhow::Error) -> WorkspaceLocked {
    match err.downcast::<WorkspaceLocked>() {
        Ok(locked) => locked,
        Err(other) => panic!("expected WorkspaceLocked, got {other:#}"),
    }
}

#[test]
fn concurrent_operations_on_one_workspace_are_refused() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    let root = ws.root().to_path_buf();
    let lock_path = root.join(".bud/lock");
    write(&root, "a.txt", "a\n");
    ws.commit("a", "alice", &[]).unwrap();
    assert!(!lock_path.exists(), "released after each operation");

    // Another handle (standing in for another process) holds the lock.
    let other = Workspace::open(&root).unwrap();
    let guard = other.lock().unwrap();
    assert_eq!(guard.path(), lock_path);
    let pid = fs::read_to_string(&lock_path).unwrap();
    assert!(pid.starts_with(&format!("{} ", std::process::id())), "{pid}");

    write(&root, "a.txt", "changed\n");
    let err = locked_error(ws.status().unwrap_err());
    assert_eq!((err.pid, err.stale, err.path.as_path()), (Some(std::process::id()), false, lock_path.as_path()));
    let msg = err.to_string();
    assert!(msg.contains("locked by another bud process") && msg.contains(&format!("rm '{}'", lock_path.display())));
    locked_error(ws.commit("c", "alice", &[]).unwrap_err());
    locked_error(ws.sync(None).unwrap_err());
    locked_error(ws.revert(&[path("a.txt")]).unwrap_err());
    locked_error(ws.diff(&[]).unwrap_err());
    locked_error(ws.gc_cache().unwrap_err());
    assert_eq!(read(&root, "a.txt"), "changed\n", "a refused revert changes nothing");

    drop(guard);
    assert!(!lock_path.exists());
    assert_eq!(status(&mut ws), ["M a.txt"]);

    // Many handles racing: each operation either runs or is refused cleanly,
    // and the state stays intact.
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let mut ws = Workspace::open(&root).unwrap();
                for _ in 0..20 {
                    match ws.status() {
                        Ok(changes) => assert_eq!(summarize(&changes), ["M a.txt"]),
                        Err(e) => drop(locked_error(e)),
                    }
                }
            });
        }
    });
    assert!(!lock_path.exists());
    let mut reopened = Workspace::open(&root).unwrap();
    assert_eq!(status(&mut reopened), ["M a.txt"]);
}

#[test]
fn stale_lock_is_reported_with_how_to_remove_it() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    let lock_path = ws.root().join(".bud/lock");

    // A process that has exited, as if bud had been killed mid-operation.
    let mut child = std::process::Command::new(std::env::current_exe().unwrap()).arg("--list").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    fs::write(&lock_path, format!("{dead} 12345\n")).unwrap();

    let err = locked_error(ws.status().unwrap_err());
    assert_eq!(err.pid, Some(dead));
    let msg = err.to_string();
    assert!(msg.contains(&format!("rm '{}'", lock_path.display())), "{msg}");
    if cfg!(target_os = "linux") {
        assert!(err.stale);
        assert!(msg.contains("is stale") && msg.contains(&format!("process {dead}")), "{msg}");
    }
    // The CLI prints errors with `{:#}`; the lock message is the whole story.
    assert_eq!(format!("{:#}", ws.status().unwrap_err()), msg);
    assert!(lock_path.exists(), "never removed automatically");

    // Unreadable contents still produce a helpful error.
    fs::write(&lock_path, "").unwrap();
    let err = locked_error(ws.status().unwrap_err());
    assert_eq!((err.pid, err.stale), (None, false));
    assert!(err.to_string().contains("rm '"), "{err}");

    fs::remove_file(&lock_path).unwrap();
    assert!(status(&mut ws).is_empty());
}

#[test]
fn failed_clone_is_cleaned_up_and_can_be_retried() {
    let data = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let server = TestServer::start_at(data.path());
    assert!(budwood_client::Remote::create_repo(&server.url, REPO).unwrap());
    let mut ws = clone(&server, REPO, &tmp.path().join("seed"));
    write(ws.root(), "a/one.txt", "one\n");
    write(ws.root(), "b/two.txt", "two\n");
    ws.commit("seed", "alice", &[]).unwrap();

    // The server loses a blob, so checkouts fail after `.bud/` was created
    // and some files were written.
    let hex = ObjectId::for_blob(b"two\n").to_hex();
    let stored = data.path().join("objects").join(&hex[..2]).join(&hex[2..]);
    let aside = data.path().join("aside");
    fs::rename(&stored, &aside).unwrap();

    let target = tmp.path().join("two");
    let err = Workspace::clone(&server.url, REPO, &target, vec![]).err().expect("clone fails");
    assert!(format!("{err:#}").contains("clone failed (removed the partial checkout"), "{err:#}");
    assert!(!target.exists(), "a directory the clone created is removed");

    let nested = tmp.path().join("x/y/z");
    Workspace::clone(&server.url, REPO, &nested, vec![]).err().expect("clone fails");
    assert!(!tmp.path().join("x").exists(), "so are missing parents it created");

    let empty = tmp.path().join("empty");
    fs::create_dir(&empty).unwrap();
    Workspace::clone(&server.url, REPO, &empty, vec![]).err().expect("clone fails");
    assert!(empty.is_dir() && fs::read_dir(&empty).unwrap().next().is_none(), "an existing empty dir is emptied");

    // A sparse clone that doesn't need the lost blob works meanwhile.
    let sparse = Workspace::clone(&server.url, REPO, &tmp.path().join("sparse"), vec![path("a")]).unwrap();
    assert_eq!(files(sparse.root()), ["a/one.txt"]);
    let guard = sparse.lock().unwrap();
    locked_error(Workspace::open(sparse.root()).unwrap().sparse_add(path("b")).unwrap_err());
    drop(guard);

    // Once the server is healthy again, the same clones simply succeed.
    fs::rename(&aside, &stored).unwrap();
    for dir in [&target, &nested, &empty] {
        let mut ws = Workspace::clone(&server.url, REPO, dir, vec![]).unwrap();
        assert_eq!(files(ws.root()), ["a/one.txt", "b/two.txt"]);
        assert!(status(&mut ws).is_empty());
    }

    // An interrupted clone (killed before cleanup) is named as such.
    let interrupted = tmp.path().join("interrupted");
    fs::create_dir_all(interrupted.join(".bud/objects")).unwrap();
    let err = Workspace::clone(&server.url, REPO, &interrupted, vec![]).err().expect("clone fails");
    assert!(err.to_string().contains("interrupted clone"), "{err:#}");
}

#[test]
fn path_arguments() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let ws = clone(&server, REPO, &tmp.path().join("one"));
    let root = ws.root().to_path_buf();
    write(&root, "src/lib.rs", "x\n");
    let rp = |cwd: &Path, arg: &str| ws.repo_path(cwd, arg).map_err(|e| e.to_string());
    let s = |p: &Path| p.to_str().unwrap().to_string();

    // The workspace root, however it is spelled, is the root path.
    for arg in [s(&root), format!("{}/", s(&root)), ".".into(), "./".into(), "src/..".into()] {
        assert_eq!(rp(&root, &arg), Ok(RepoPath::root()), "{arg}");
    }
    assert_eq!(rp(&root.join("src"), ".."), Ok(RepoPath::root()));
    assert_eq!(rp(&root.join("src"), "lib.rs"), Ok(path("src/lib.rs")));
    assert_eq!(rp(&root.join("src"), "../docs/new.md"), Ok(path("docs/new.md")));
    assert_eq!(rp(Path::new("/"), &s(&root.join("src/lib.rs"))), Ok(path("src/lib.rs")));
    assert!(rp(&root, "..").unwrap_err().contains("outside the workspace"));
    assert!(rp(&root.join("src"), "../../one-sibling").unwrap_err().contains("outside"));
    assert!(rp(&root, ".bud/config.json").is_err(), ".bud is never a repository path");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        // An alias for the directory holding the workspace, like macOS's
        // /tmp -> /private/tmp.
        let links = tempfile::tempdir().unwrap();
        let alias = links.path().join("alias");
        symlink(tmp.path(), &alias).unwrap();
        let via = alias.join("one");
        assert_eq!(rp(&root, &s(&via)), Ok(RepoPath::root()));
        assert_eq!(rp(&root, &format!("{}/", s(&via))), Ok(RepoPath::root()));
        assert_eq!(rp(&root, &s(&via.join("src/lib.rs"))), Ok(path("src/lib.rs")));
        assert_eq!(rp(&root, &s(&via.join("not/yet/created"))), Ok(path("not/yet/created")));
        assert_eq!(rp(&via.join("src"), "lib.rs"), Ok(path("src/lib.rs")), "cwd through the alias");
        assert_eq!(rp(&via.join("src"), ".."), Ok(RepoPath::root()));
        assert!(rp(&root, &s(&alias)).unwrap_err().contains("outside"));

        // A tracked symlink inside the workspace names itself, not its target.
        symlink("src", root.join("link")).unwrap();
        assert_eq!(rp(&root, &s(&via.join("link"))), Ok(path("link")));
        // A symlink to the workspace itself, from outside.
        symlink(&root, links.path().join("ws")).unwrap();
        assert_eq!(rp(&root, &s(&links.path().join("ws"))), Ok(RepoPath::root()));
        assert_eq!(rp(&root, &s(&links.path().join("ws/src/lib.rs"))), Ok(path("src/lib.rs")));
        // And a shortcut into a subdirectory.
        symlink(root.join("src"), links.path().join("shortcut")).unwrap();
        assert_eq!(rp(&root, &s(&links.path().join("shortcut"))), Ok(path("src")));
        assert_eq!(rp(&root, &s(&links.path().join("shortcut/lib.rs"))), Ok(path("src/lib.rs")));
    }
}
