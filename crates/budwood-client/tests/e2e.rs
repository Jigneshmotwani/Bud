//! End-to-end tests: real workspaces talking to a real in-process server.

mod common;

use std::fs;
use std::time::{Duration, SystemTime};

use budwood_client::{UpdateConflict, Workspace};
use budwood_core::{EntryKind, RepoPath};
use common::*;

const REPO: &str = "test";

#[test]
fn clone_add_commit_and_clone_again() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    assert_eq!(ws1.base(), 0);
    assert!(status(&mut ws1).is_empty());

    let root = ws1.root().to_path_buf();
    write(&root, "README", "hello\n");
    write(&root, "src/main.rs", "fn main() {}\n");
    assert_eq!(status(&mut ws1), ["A README", "A src/main.rs"]);

    let outcome = ws1.commit("initial", "alice", &[]).unwrap();
    assert_eq!(outcome.changeset.seq, 1);
    assert!(!outcome.rebased);
    assert_eq!(outcome.changeset.author, "alice");
    assert_eq!(ws1.base(), 1);
    assert!(status(&mut ws1).is_empty());
    assert!(ws1.commit("again", "alice", &[]).is_err(), "nothing to commit");

    let ws2 = clone(&server, REPO, &tmp.path().join("two"));
    assert_eq!(ws2.base(), 1);
    assert_eq!(files(ws2.root()), ["README", "src/main.rs"]);
    assert_eq!(read(ws2.root(), "src/main.rs"), "fn main() {}\n");

    // Reopening from a subdirectory finds the same workspace and state.
    let mut reopened = Workspace::open(&ws2.root().join("src")).unwrap();
    assert_eq!(reopened.root(), ws2.root());
    assert_eq!(reopened.base(), 1);
    assert!(status(&mut reopened).is_empty());

    // Cloning into a non-empty directory is refused.
    assert!(Workspace::clone(&server.url, REPO, ws2.root(), vec![]).is_err());
}

#[test]
fn modify_delete_and_sync() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    let r1 = ws1.root().to_path_buf();
    write(&r1, "a.txt", "one\n");
    write(&r1, "dir/b.txt", "bee\n");
    write(&r1, "dir/c.txt", "see\n");
    ws1.commit("add", "alice", &[]).unwrap();

    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    let r2 = ws2.root().to_path_buf();

    write(&r1, "a.txt", "one, modified\n");
    fs::remove_file(r1.join("dir/b.txt")).unwrap();
    fs::remove_file(r1.join("dir/c.txt")).unwrap();
    write(&r1, "new/d.txt", "dee\n");
    assert_eq!(status(&mut ws1), ["M a.txt", "A new/d.txt", "D dir/b.txt", "D dir/c.txt"].map(String::from).tap_sort());
    let cs = ws1.commit("change", "alice", &[]).unwrap().changeset;
    assert_eq!(cs.seq, 2);
    assert_eq!(cs.changes.len(), 4);
    assert!(status(&mut ws1).is_empty());

    let outcome = ws2.sync(None).unwrap();
    assert_eq!((outcome.from, outcome.to, outcome.updated), (1, 2, 4));
    assert_eq!(read(&r2, "a.txt"), "one, modified\n");
    assert_eq!(read(&r2, "new/d.txt"), "dee\n");
    assert!(!exists(&r2, "dir"), "emptied directory is removed");
    assert!(status(&mut ws2).is_empty());

    // Syncing again is a no-op; syncing back to an old revision works too.
    let again = ws2.sync(None).unwrap();
    assert_eq!((again.from, again.to, again.updated), (2, 2, 0));
    let back = ws2.sync(Some("1")).unwrap();
    assert_eq!(back.to, 1);
    assert_eq!(files(&r2), ["a.txt", "dir/b.txt", "dir/c.txt"]);
    assert!(status(&mut ws2).is_empty());
    ws2.sync(None).unwrap();
    assert_eq!(files(&r2), ["a.txt", "new/d.txt"]);
}

#[test]
fn server_rebases_disjoint_commits() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    write(ws1.root(), "a.txt", "a1\n");
    write(ws1.root(), "b.txt", "b1\n");
    ws1.commit("base", "alice", &[]).unwrap();
    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));

    write(ws1.root(), "a.txt", "a2\n");
    let first = ws1.commit("edit a", "alice", &[]).unwrap();
    assert_eq!(first.changeset.seq, 2);
    assert!(!first.rebased);

    write(ws2.root(), "b.txt", "b2\n");
    let second = ws2.commit("edit b", "bob", &[]).unwrap();
    assert!(second.rebased);
    assert_eq!(second.changeset.seq, 3);
    assert_eq!(second.changeset.base, 1);
    assert_eq!(second.pulled, 1);
    assert_eq!(ws2.base(), 3);
    assert_eq!(read(ws2.root(), "a.txt"), "a2\n", "rebased commit pulls in the other change");
    assert!(status(&mut ws2).is_empty());

    let remote = server.remote(REPO);
    assert_eq!(remote.file("head", &path("a.txt")).unwrap(), b"a2\n");
    assert_eq!(remote.file("head", &path("b.txt")).unwrap(), b"b2\n");

    ws1.sync(None).unwrap();
    assert_eq!(read(ws1.root(), "b.txt"), "b2\n");
    assert!(status(&mut ws1).is_empty());
}

#[test]
fn same_file_conflict_then_revert_and_sync() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    write(ws1.root(), "shared.txt", "v1\n");
    write(ws1.root(), "other.txt", "o1\n");
    ws1.commit("base", "alice", &[]).unwrap();
    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));

    write(ws1.root(), "shared.txt", "alice's\n");
    ws1.commit("alice edits", "alice", &[]).unwrap();

    write(ws2.root(), "shared.txt", "bob's\n");
    let err = ws2.commit("bob edits", "bob", &[]).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("commit rejected") && msg.contains("shared.txt"), "{msg}");
    assert_eq!(ws2.base(), 1);
    assert_eq!(read(ws2.root(), "shared.txt"), "bob's\n", "local edit kept");

    let err = ws2.sync(None).unwrap_err();
    let conflict = err.downcast_ref::<UpdateConflict>().expect("update conflict");
    assert_eq!(conflict.paths, [path("shared.txt")]);
    assert_eq!(ws2.base(), 1, "a refused sync changes nothing");
    assert_eq!(read(ws2.root(), "shared.txt"), "bob's\n");

    let reverted = ws2.revert(&[path("shared.txt")]).unwrap();
    assert_eq!(summarize(&reverted), ["M shared.txt"]);
    assert_eq!(read(ws2.root(), "shared.txt"), "v1\n");
    ws2.sync(None).unwrap();
    assert_eq!(ws2.base(), 2);
    assert_eq!(read(ws2.root(), "shared.txt"), "alice's\n");
    assert!(status(&mut ws2).is_empty());

    // An identical change on both sides is not a conflict for sync.
    write(ws1.root(), "other.txt", "same\n");
    write(ws2.root(), "other.txt", "same\n");
    ws1.commit("o", "alice", &[]).unwrap();
    ws2.sync(None).unwrap();
    assert!(status(&mut ws2).is_empty());
}

#[test]
fn commit_only_some_paths_and_revert_others() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    write(ws.root(), "keep/a", "a\n");
    write(ws.root(), "later/b", "b\n");
    let cs = ws.commit("partial", "alice", &[path("keep")]).unwrap().changeset;
    assert_eq!(cs.changes.len(), 1);
    assert_eq!(status(&mut ws), ["A later/b"]);

    // Reverting an added file removes it, and its directory.
    ws.revert(&[path("later")]).unwrap();
    assert!(!exists(ws.root(), "later"));
    assert!(status(&mut ws).is_empty());
}

#[test]
fn sparse_clone_and_sparse_add() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut full = clone(&server, REPO, &tmp.path().join("full"));
    write(full.root(), "proj/a/x.txt", "x\n");
    write(full.root(), "proj/a/sub/y.txt", "y\n");
    write(full.root(), "proj/b/z.txt", "z\n");
    write(full.root(), "top.txt", "top\n");
    full.commit("tree", "alice", &[]).unwrap();

    let mut sparse = Workspace::clone(&server.url, REPO, &tmp.path().join("sparse"), vec![path("proj/a")]).unwrap();
    assert_eq!(sparse.config().sparse, [path("proj/a")]);
    assert_eq!(files(sparse.root()), ["proj/a/sub/y.txt", "proj/a/x.txt"]);
    assert!(status(&mut sparse).is_empty());

    // Files created outside the sparse set are not seen.
    write(sparse.root(), "elsewhere.txt", "ignored\n");
    assert!(status(&mut sparse).is_empty());
    fs::remove_file(sparse.root().join("elsewhere.txt")).unwrap();

    // Changes outside the sparse set don't land on disk, but the base moves.
    write(full.root(), "proj/b/z.txt", "z2\n");
    write(full.root(), "top.txt", "top2\n");
    full.commit("outside", "alice", &[]).unwrap();
    write(sparse.root(), "proj/a/x.txt", "x2\n");
    let outcome = sparse.commit("inside", "bob", &[]).unwrap();
    assert!(outcome.rebased);
    assert_eq!(outcome.pulled, 0);
    assert_eq!(files(sparse.root()), ["proj/a/sub/y.txt", "proj/a/x.txt"]);

    assert_eq!(sparse.sparse_add(path("proj/b")).unwrap(), 1);
    assert_eq!(sparse.sparse_add(path("proj/b/z.txt")).unwrap(), 0, "already covered");
    assert_eq!(sparse.config().sparse, [path("proj/a"), path("proj/b")]);
    assert_eq!(read(sparse.root(), "proj/b/z.txt"), "z2\n");
    assert!(!exists(sparse.root(), "top.txt"));
    assert!(status(&mut sparse).is_empty());

    // Widening to the root materializes the rest.
    assert_eq!(sparse.sparse_add(RepoPath::root()).unwrap(), 1);
    assert_eq!(read(sparse.root(), "top.txt"), "top2\n");
    assert_eq!(sparse.config().sparse, [RepoPath::root()]);

    // The sparse set persists across reopening.
    let mut reopened = Workspace::open(sparse.root()).unwrap();
    assert_eq!(reopened.config().sparse, [RepoPath::root()]);
    assert!(status(&mut reopened).is_empty());

    full.sync(None).unwrap();
    assert_eq!(read(full.root(), "proj/a/x.txt"), "x2\n");
}

#[cfg(unix)]
#[test]
fn executable_bit_and_symlinks_round_trip() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    let r1 = ws1.root().to_path_buf();
    write(&r1, "run.sh", "#!/bin/sh\necho hi\n");
    fs::set_permissions(r1.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("run.sh", r1.join("link")).unwrap();
    symlink("does/not/exist", r1.join("dangling")).unwrap();
    ws1.commit("exec and links", "alice", &[]).unwrap();
    assert!(status(&mut ws1).is_empty());

    let listing = server.remote(REPO).tree("head", &RepoPath::root()).unwrap();
    let kinds: Vec<(String, EntryKind)> = listing.entries.iter().map(|e| (e.name.clone(), e.kind)).collect();
    assert_eq!(
        kinds,
        [
            ("dangling".to_string(), EntryKind::Symlink),
            ("link".to_string(), EntryKind::Symlink),
            ("run.sh".to_string(), EntryKind::Executable)
        ]
    );

    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    let r2 = ws2.root().to_path_buf();
    assert_ne!(fs::metadata(r2.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(fs::read_link(r2.join("link")).unwrap().to_str(), Some("run.sh"));
    assert_eq!(fs::read_link(r2.join("dangling")).unwrap().to_str(), Some("does/not/exist"));
    assert_eq!(read(&r2, "link"), "#!/bin/sh\necho hi\n");

    // Dropping the executable bit and retargeting a link are both changes.
    fs::set_permissions(r1.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(r1.join("link")).unwrap();
    symlink("elsewhere", r1.join("link")).unwrap();
    assert_eq!(status(&mut ws1), ["M link", "M run.sh"]);
    ws1.commit("chmod", "alice", &[]).unwrap();

    ws2.sync(None).unwrap();
    assert_eq!(fs::metadata(r2.join("run.sh")).unwrap().permissions().mode() & 0o111, 0);
    assert_eq!(fs::read_link(r2.join("link")).unwrap().to_str(), Some("elsewhere"));
    assert!(status(&mut ws2).is_empty());

    // A symlink can replace a regular file and the change syncs.
    fs::remove_file(r1.join("run.sh")).unwrap();
    symlink("link", r1.join("run.sh")).unwrap();
    assert_eq!(status(&mut ws1), ["M run.sh"]);
    ws1.commit("file to link", "alice", &[]).unwrap();
    ws2.sync(None).unwrap();
    assert_eq!(fs::read_link(r2.join("run.sh")).unwrap().to_str(), Some("link"));
}

#[test]
fn budignore_hides_untracked_files() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    let r1 = ws1.root().to_path_buf();
    write(&r1, ".budignore", "# build output\ntarget/\n*.log\n");
    write(&r1, "src/main.rs", "fn main() {}\n");
    write(&r1, "target/debug/app", "binary");
    write(&r1, "src/target/out", "nested build dir");
    write(&r1, "debug.log", "log");
    write(&r1, "src/trace.log", "log");
    assert_eq!(status(&mut ws1), ["A .budignore", "A src/main.rs"]);
    ws1.commit("ignore", "alice", &[]).unwrap();
    assert!(status(&mut ws1).is_empty());

    // A fresh clone picks up the .budignore it just checked out.
    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    write(ws2.root(), "target/x", "build");
    write(ws2.root(), "y.log", "log");
    assert!(status(&mut ws2).is_empty());

    // Editing .budignore takes effect without reopening the workspace.
    write(&r1, ".budignore", "target/\n");
    assert_eq!(status(&mut ws1), ["M .budignore", "A debug.log", "A src/trace.log"]);
    write(&r1, ".budignore", "# build output\ntarget/\n*.log\n");
    assert!(status(&mut ws1).is_empty());
}

#[test]
fn tracked_files_under_ignored_directories() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    let root = ws.root().to_path_buf();
    // Committed before the ignore rule existed.
    write(&root, "target/keep.txt", "kept\n");
    ws.commit("keep", "alice", &[]).unwrap();
    write(&root, ".budignore", "target/\n");
    ws.commit("ignore", "alice", &[]).unwrap();

    // The tracked file is still tracked; new files beside it stay ignored.
    write(&root, "target/keep.txt", "changed\n");
    write(&root, "target/new.o", "object");
    write(&root, "target/deep/er.o", "object");
    assert_eq!(status(&mut ws), ["M target/keep.txt"]);
}

#[test]
fn file_and_directory_replacement() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    let r1 = ws1.root().to_path_buf();
    write(&r1, "a", "plain file\n");
    write(&r1, "b/inner", "inner\n");
    ws1.commit("file a", "alice", &[]).unwrap();
    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    let r2 = ws2.root().to_path_buf();
    let remote = server.remote(REPO);

    // File -> directory.
    fs::remove_file(r1.join("a")).unwrap();
    write(&r1, "a/x", "now a directory\n");
    assert_eq!(status(&mut ws1), ["D a", "A a/x"].map(String::from).tap_sort());
    ws1.commit("a becomes a dir", "alice", &[]).unwrap();
    assert!(status(&mut ws1).is_empty());
    assert_eq!(remote.tree("head", &path("a")).unwrap().kind, EntryKind::Tree);

    ws2.sync(None).unwrap();
    assert_eq!(read(&r2, "a/x"), "now a directory\n");
    assert!(status(&mut ws2).is_empty());

    // Directory -> file, committed from the other workspace.
    fs::remove_dir_all(r2.join("a")).unwrap();
    write(&r2, "a", "file again\n");
    assert_eq!(status(&mut ws2), ["A a", "D a/x"]);
    // And the same for b, in the same commit.
    fs::remove_dir_all(r2.join("b")).unwrap();
    write(&r2, "b", "b file\n");
    ws2.commit("a and b become files", "bob", &[]).unwrap();
    assert!(status(&mut ws2).is_empty());
    assert_eq!(remote.tree("head", &path("a")).unwrap().kind, EntryKind::File);

    ws1.sync(None).unwrap();
    assert_eq!(read(&r1, "a"), "file again\n");
    assert_eq!(read(&r1, "b"), "b file\n");
    assert!(status(&mut ws1).is_empty());

    // A local file -> directory replacement can be reverted.
    fs::remove_file(r1.join("a")).unwrap();
    write(&r1, "a/y", "temporary\n");
    ws1.revert(&[path("a")]).unwrap();
    assert_eq!(read(&r1, "a"), "file again\n");
    assert!(status(&mut ws1).is_empty());

    // And a directory -> file one.
    write(&r1, "c/q", "q\n");
    ws1.commit("dir c", "alice", &[]).unwrap();
    fs::remove_dir_all(r1.join("c")).unwrap();
    write(&r1, "c", "file\n");
    assert_eq!(status(&mut ws1), ["A c", "D c/q"]);
    ws1.revert(&[path("c")]).unwrap();
    assert_eq!(read(&r1, "c/q"), "q\n");
    assert!(status(&mut ws1).is_empty());
}

#[test]
fn incoming_directory_replaces_local_file_conflict() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    write(ws1.root(), "seed", "s\n");
    ws1.commit("seed", "alice", &[]).unwrap();
    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));

    write(ws1.root(), "x/inner", "dir\n");
    ws1.commit("x is a dir", "alice", &[]).unwrap();
    // Locally, x is an uncommitted file.
    write(ws2.root(), "x", "file\n");
    let err = ws2.sync(None).unwrap_err();
    assert_eq!(err.downcast_ref::<UpdateConflict>().unwrap().paths, [path("x/inner")]);
    let err = ws2.commit("x file", "bob", &[]).unwrap_err();
    assert!(format!("{err:#}").contains("commit rejected"), "{err:#}");
    assert_eq!(read(ws2.root(), "x"), "file\n");
}

#[test]
fn log_and_diff() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    let root = ws.root().to_path_buf();
    write(&root, "docs/guide.md", "line one\nline two\nline three\n");
    write(&root, "src/lib.rs", "pub fn f() {}\n");
    ws.commit("first\n\nwith a body", "alice", &[]).unwrap();
    write(&root, "src/lib.rs", "pub fn f() -> u8 { 1 }\n");
    ws.commit("second", "bob", &[]).unwrap();
    write(&root, "docs/guide.md", "line one\nline 2\nline three\n");
    ws.commit("third", "alice", &[]).unwrap();

    let remote = server.remote(REPO);
    let seqs = |p: &str| remote.log(&path(p), None, 50).unwrap().iter().map(|c| c.seq).collect::<Vec<_>>();
    assert_eq!(seqs(""), [3, 2, 1]);
    assert_eq!(seqs("src"), [2, 1]);
    assert_eq!(seqs("src/lib.rs"), [2, 1]);
    assert_eq!(seqs("docs"), [3, 1]);
    assert_eq!(seqs("doc"), Vec::<u64>::new());
    assert_eq!(remote.log(&RepoPath::root(), Some(3), 1).unwrap()[0].seq, 2);
    let first = remote.changeset("1").unwrap();
    assert_eq!(first.summary(), "first");
    assert_eq!(remote.changeset(&first.id.to_hex()).unwrap().seq, 1);

    let diff = remote.diff("1", "head", &RepoPath::root()).unwrap();
    assert_eq!(diff.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["docs/guide.md", "src/lib.rs"]);
    assert_eq!(remote.diff("1", "2", &path("docs")).unwrap().changes, []);

    // Local diffs.
    write(&root, "docs/guide.md", "line one\nline two!\nline three\n");
    write(&root, "added.txt", "brand new\n");
    fs::remove_file(root.join("src/lib.rs")).unwrap();
    fs::write(root.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    let out = ws.diff(&[]).unwrap();
    assert!(out.contains("diff docs/guide.md\n--- a/docs/guide.md\n+++ b/docs/guide.md\n"), "{out}");
    assert!(out.contains("-line 2\n+line two!\n"), "{out}");
    assert!(out.contains("--- /dev/null\n+++ b/added.txt\n"), "{out}");
    assert!(out.contains("+brand new\n"), "{out}");
    assert!(out.contains("--- a/src/lib.rs\n+++ /dev/null\n"), "{out}");
    assert!(out.contains("-pub fn f() -> u8 { 1 }\n"), "{out}");
    assert!(out.contains("Binary files /dev/null and b/blob.bin differ\n"), "{out}");

    let only = ws.diff(&[path("docs")]).unwrap();
    assert!(only.starts_with("diff docs/guide.md\n") && !only.contains("added.txt"), "{only}");
}

#[test]
fn same_size_edit_right_after_clone_is_detected() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    write(ws1.root(), "f.txt", "aaaa");
    ws1.commit("f", "alice", &[]).unwrap();
    // Same size, immediately after the commit wrote the index.
    write(ws1.root(), "f.txt", "bbbb");
    assert_eq!(status(&mut ws1), ["M f.txt"]);
    write(ws1.root(), "f.txt", "aaaa");
    assert!(status(&mut ws1).is_empty());

    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    write(ws2.root(), "f.txt", "cccc");
    assert_eq!(status(&mut ws2), ["M f.txt"]);

    // Once the file is old enough for its stat data to be cached, an edit
    // that keeps the size is still caught by the changed mtime.
    write(ws2.root(), "f.txt", "aaaa");
    let old = SystemTime::now() - Duration::from_secs(60);
    fs::File::options().write(true).open(ws2.root().join("f.txt")).unwrap().set_modified(old).unwrap();
    assert!(status(&mut ws2).is_empty());
    let mut reopened = Workspace::open(ws2.root()).unwrap();
    assert!(status(&mut reopened).is_empty());
    write(ws2.root(), "f.txt", "dddd");
    assert_eq!(status(&mut reopened), ["M f.txt"]);
}

#[test]
fn errors_from_the_server() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    assert!(!budwood_client::Remote::create_repo(&server.url, REPO).unwrap());
    assert!(Workspace::clone(&server.url, "missing", &tmp.path().join("x"), vec![]).is_err());
    assert!(!tmp.path().join("x").exists());
    let remote = server.remote(REPO);
    let err = remote.file("head", &path("nope")).unwrap_err();
    assert!(err.downcast_ref::<budwood_client::ApiError>().unwrap().is_not_found());
    assert!(remote.resolve("5").is_err());
    assert_eq!(budwood_client::Remote::list_repos(&server.url).unwrap(), [REPO]);
}

/// Sorting helper for expected-status literals written in a readable order.
trait TapSort {
    fn tap_sort(self) -> Vec<String>;
}

impl<const N: usize> TapSort for [String; N] {
    fn tap_sort(self) -> Vec<String> {
        let mut v = self.to_vec();
        v.sort_by(|a, b| a[2..].cmp(&b[2..]));
        v
    }
}

#[test]
fn concurrent_commits_to_disjoint_paths_all_land() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut seed = clone(&server, REPO, &tmp.path().join("seed"));
    write(seed.root(), "seed", "s\n");
    seed.commit("seed", "alice", &[]).unwrap();

    let mut workspaces: Vec<Workspace> =
        (0..8).map(|i| clone(&server, REPO, &tmp.path().join(format!("w{i}")))).collect();
    std::thread::scope(|scope| {
        for (i, ws) in workspaces.iter_mut().enumerate() {
            scope.spawn(move || {
                write(ws.root(), &format!("dir{i}/file"), &format!("{i}\n"));
                ws.commit(&format!("commit {i}"), "racer", &[]).unwrap();
            });
        }
    });

    let head = server.remote(REPO).head().unwrap();
    assert_eq!(head.seq, 9);
    seed.sync(None).unwrap();
    let mut expected: Vec<String> = (0..8).map(|i| format!("dir{i}/file")).collect();
    expected.push("seed".into());
    assert_eq!(files(seed.root()), expected);
    for ws in &mut workspaces {
        assert!(status(ws).is_empty());
    }
}

#[test]
fn many_files_are_fetched_in_batches() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    // More than one `objects/missing` and `objects/fetch` batch.
    let n = budwood_core::api::MAX_BATCH + 100;
    for i in 0..n {
        write(ws1.root(), &format!("d{}/f{i}", i % 7), &format!("content {i}\n"));
    }
    write(ws1.root(), "empty", "");
    let cs = ws1.commit("many", "alice", &[]).unwrap().changeset;
    assert_eq!(cs.changes.len(), n + 1);

    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    assert_eq!(files(ws2.root()).len(), n + 1);
    assert_eq!(read(ws2.root(), "d6/f1000"), "content 1000\n");
    assert_eq!(read(ws2.root(), "empty"), "");
    assert!(status(&mut ws2).is_empty());
}

#[test]
fn unusual_file_names() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    let names = ["sp ace/a b.txt", "q?&=+%#.txt", "ünï/cødé ☃.txt", "dir0/x", "dir/x", "dir-/x"];
    for name in names {
        write(ws1.root(), name, name);
    }
    ws1.commit("names", "alice", &[]).unwrap();
    let remote = server.remote(REPO);
    for name in names {
        assert_eq!(remote.file("head", &path(name)).unwrap(), name.as_bytes(), "{name}");
        assert_eq!(remote.log(&path(name), None, 10).unwrap().len(), 1, "{name}");
    }
    write(ws1.root(), "dir0/x", "changed");
    ws1.commit("dir0", "alice", &[]).unwrap();
    assert_eq!(remote.log(&path("dir"), None, 10).unwrap().len(), 1, "dir does not match dir0 or dir-");

    let ws2 = clone(&server, REPO, &tmp.path().join("two"));
    let mut expected: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    expected.sort();
    assert_eq!(files(ws2.root()), expected);
}

#[test]
fn nested_workspace_does_not_break_status() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut outer = clone(&server, REPO, &tmp.path().join("outer"));
    write(outer.root(), ".budignore", "nested/\n");
    outer.commit("ignore nested", "alice", &[]).unwrap();

    // Another workspace cloned inside this one, ignored by name; its `.bud`
    // directory must not trip up the outer workspace.
    let mut inner = clone(&server, REPO, &outer.root().join("nested"));
    assert!(status(&mut outer).is_empty());
    write(inner.root(), "deep/file", "x\n");
    assert_eq!(status(&mut inner), ["A deep/file"]);
    fs::create_dir_all(outer.root().join("sub/.bud")).unwrap();
    write(outer.root(), "sub/.bud/config.json", "{}");
    write(outer.root(), "sub/real", "r\n");
    assert_eq!(status(&mut outer), ["A sub/real"]);
}

#[test]
fn sync_does_not_overwrite_ignored_local_files() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    write(ws1.root(), ".budignore", "*.log\n");
    ws1.commit("ignore logs", "alice", &[]).unwrap();
    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    write(ws2.root(), "app.log", "precious local data\n");
    write(ws2.root(), "same.log", "identical\n");
    assert!(status(&mut ws2).is_empty());

    // Someone stops ignoring logs and commits files at the same paths.
    write(ws1.root(), ".budignore", "");
    write(ws1.root(), "app.log", "committed\n");
    write(ws1.root(), "same.log", "identical\n");
    ws1.commit("track logs", "alice", &[]).unwrap();

    let err = ws2.sync(None).unwrap_err();
    assert_eq!(err.downcast_ref::<UpdateConflict>().expect("conflict").paths, [path("app.log")]);
    assert_eq!(read(ws2.root(), "app.log"), "precious local data\n");
    assert_eq!(ws2.base(), 1);

    fs::remove_file(ws2.root().join("app.log")).unwrap();
    ws2.sync(None).unwrap();
    assert_eq!(read(ws2.root(), "app.log"), "committed\n");
    assert!(status(&mut ws2).is_empty());
}

#[test]
fn history_survives_a_server_restart() {
    let data = tempfile::tempdir().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let server = TestServer::start_at(data.path());
    assert!(budwood_client::Remote::create_repo(&server.url, REPO).unwrap());
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    write(ws.root(), "a/b.txt", "persisted\n");
    ws.commit("before restart", "alice", &[]).unwrap();
    drop(server);

    let server = TestServer::start_at(data.path());
    let remote = server.remote(REPO);
    assert_eq!(remote.head().unwrap().seq, 1);
    assert_eq!(remote.file("1", &path("a/b.txt")).unwrap(), b"persisted\n");
    assert_eq!(remote.log(&path("a"), None, 10).unwrap()[0].message, "before restart");
    let fresh = clone(&server, REPO, &tmp.path().join("two"));
    assert_eq!(read(fresh.root(), "a/b.txt"), "persisted\n");
}
