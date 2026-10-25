//! The bounded object cache: budgets, LRU eviction and transparent re-fetching.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use budwood_client::{Workspace, DEFAULT_CACHE_MAX_BYTES};
use budwood_core::ObjectId;
use common::*;

const REPO: &str = "test";

fn cached_blob(root: &Path, content: &str) -> PathBuf {
    let hex = ObjectId::for_blob(content.as_bytes()).to_hex();
    root.join(".bud/objects").join(&hex[..2]).join(&hex[2..])
}

/// Sets the mtime of every cached object to `when`.
fn age_all_objects(root: &Path, when: SystemTime) {
    for fanout in fs::read_dir(root.join(".bud/objects")).unwrap() {
        let fanout = fanout.unwrap();
        if fanout.file_type().unwrap().is_dir() {
            for object in fs::read_dir(fanout.path()).unwrap() {
                set_mtime(&object.unwrap().path(), when);
            }
        }
    }
}

fn set_mtime(path: &Path, when: SystemTime) {
    fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
}

fn content(i: usize, version: u32) -> String {
    format!("file {i} version {version}\n{}", "x".repeat(400))
}

#[test]
fn cache_stays_within_budget_and_files_are_refetched_transparently() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    for i in 0..30 {
        write(ws1.root(), &format!("d{}/f{i}", i % 3), &content(i, 1));
    }
    ws1.commit("v1", "alice", &[]).unwrap();

    let mut ws2 = clone(&server, REPO, &tmp.path().join("two"));
    let r2 = ws2.root().to_path_buf();
    assert_eq!(ws2.config().cache_max_bytes(), DEFAULT_CACHE_MAX_BYTES);
    let before = ws2.cache_usage().unwrap();
    assert!(before.bytes > 30 * 400 && before.objects >= 30, "{before:?}");

    // Shrinking the budget and collecting evicts down to 80% of it.
    let budget = 4096;
    ws2.set_cache_max_bytes(Some(budget)).unwrap();
    let gc = ws2.gc_cache().unwrap();
    assert!(gc.freed_bytes > 0 && gc.freed_objects > 0, "{gc:?}");
    assert_eq!(gc.remaining, ws2.cache_usage().unwrap());
    assert_eq!(gc.remaining.bytes + gc.freed_bytes, before.bytes);
    assert!(gc.remaining.bytes <= budget * 4 / 5, "{gc:?}");
    // A second collection has nothing to do.
    assert_eq!(ws2.gc_cache().unwrap().freed_bytes, 0);

    // The budget is persisted, and older configs without it still load.
    let config = fs::read_to_string(r2.join(".bud/config.json")).unwrap();
    assert!(config.contains("\"cache_max_bytes\": 4096"), "{config}");
    let config1 = fs::read_to_string(ws1.root().join(".bud/config.json")).unwrap();
    assert!(!config1.contains("cache_max_bytes"), "{config1}");
    let mut ws2 = Workspace::open(&r2).unwrap();
    assert_eq!(ws2.config().cache_max_bytes, Some(budget));
    assert!(status(&mut ws2).is_empty());

    // Updates bring in more than the budget; the cache is trimmed afterwards.
    for i in 0..30 {
        write(ws1.root(), &format!("d{}/f{i}", i % 3), &content(i, 2));
    }
    ws1.commit("v2", "alice", &[]).unwrap();
    assert_eq!(ws2.sync(None).unwrap().updated, 30);
    assert!(ws2.cache_usage().unwrap().bytes <= budget, "{:?}", ws2.cache_usage().unwrap());
    for i in 0..30 {
        assert_eq!(read(&r2, &format!("d{}/f{i}", i % 3)), content(i, 2));
    }

    // Evicted base contents are fetched again for diff and revert.
    assert!(!cached_blob(&r2, &content(7, 2)).exists());
    write(&r2, "d1/f7", "local edit\n");
    let diff = ws2.diff(&[]).unwrap();
    assert!(diff.contains("-file 7 version 2\n") && diff.contains("+local edit\n"), "{diff}");
    fs::remove_file(r2.join("d2/f8")).unwrap();
    assert_eq!(status(&mut ws2), ["M d1/f7", "D d2/f8"]);
    ws2.gc_cache_to(0).unwrap();
    assert_eq!(ws2.cache_usage().unwrap().objects, 0);
    ws2.revert(&[path("d1"), path("d2")]).unwrap();
    assert_eq!(read(&r2, "d1/f7"), content(7, 2));
    assert_eq!(read(&r2, "d2/f8"), content(8, 2));
    assert!(status(&mut ws2).is_empty());

    // A cache without its size sidecar (e.g. from an older bud) is rescanned.
    fs::remove_file(r2.join(".bud/objects/usage")).unwrap();
    write(ws1.root(), "d0/f0", &content(0, 3));
    ws1.commit("v3", "alice", &[]).unwrap();
    ws2.sync(None).unwrap();
    assert_eq!(read(&r2, "d0/f0"), content(0, 3));
    assert!(ws2.cache_usage().unwrap().bytes <= budget);
    let recorded: u64 = fs::read_to_string(r2.join(".bud/objects/usage")).unwrap().trim().parse().unwrap();
    assert_eq!(recorded, ws2.cache_usage().unwrap().bytes);
}

#[test]
fn checkouts_larger_than_the_budget_work() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws1 = clone(&server, REPO, &tmp.path().join("one"));
    // More than one checkout batch, so the cache is trimmed mid-checkout.
    let n = budwood_core::api::MAX_BATCH + 50;
    for i in 0..n {
        write(ws1.root(), &format!("big/d{}/f{i}", i % 5), &format!("content {i}\n"));
    }
    write(ws1.root(), "small", "s\n");
    ws1.commit("many", "alice", &[]).unwrap();

    let mut ws2 = Workspace::clone(&server.url, REPO, &tmp.path().join("two"), vec![path("small")]).unwrap();
    ws2.set_cache_max_bytes(Some(0)).unwrap();
    assert_eq!(ws2.sparse_add(path("big")).unwrap(), n);
    assert_eq!(read(ws2.root(), "big/d3/f1038"), "content 1038\n");
    assert_eq!(files(ws2.root()).len(), n + 1);
    assert_eq!(ws2.cache_usage().unwrap().objects, 0);
    assert!(status(&mut ws2).is_empty());
}

#[test]
fn least_recently_used_objects_are_evicted_first() {
    let server = TestServer::with_repo(REPO);
    let tmp = tempfile::tempdir().unwrap();
    let mut ws = clone(&server, REPO, &tmp.path().join("one"));
    let root = ws.root().to_path_buf();
    for name in ["a", "b", "c"] {
        write(&root, name, &format!("{name}{}\n", "-".repeat(100)));
    }
    ws.commit("abc", "alice", &[]).unwrap();
    let blob = |name: &str| cached_blob(&root, &format!("{name}{}\n", "-".repeat(100)));
    assert!(blob("a").exists() && blob("b").exists() && blob("c").exists());

    let now = SystemTime::now();
    let hours = |h: u64| now - Duration::from_secs(h * 3600);
    age_all_objects(&root, hours(3));
    set_mtime(&blob("b"), hours(5));
    set_mtime(&blob("a"), hours(4));

    // Freeing one byte evicts exactly the oldest object.
    let usage = ws.cache_usage().unwrap();
    let gc = ws.gc_cache_to(usage.bytes - 1).unwrap();
    assert_eq!(gc.freed_objects, 1);
    assert!(!blob("b").exists() && blob("a").exists());

    // A cache hit makes an object recently used: diff reads a's base blob.
    write(&root, "a", "changed\n");
    assert!(ws.diff(&[]).unwrap().contains("+changed\n"));
    let usage = ws.cache_usage().unwrap();
    ws.gc_cache_to(usage.bytes - 1).unwrap();
    assert!(blob("a").exists(), "touched on use, so no longer the oldest");
    assert!(!blob("b").exists());

    // Stale temporary files from crashed processes are cleaned up.
    let tmp_file = root.join(".bud/objects/tmp-999999-0");
    fs::write(&tmp_file, "partial").unwrap();
    set_mtime(&tmp_file, hours(2));
    ws.gc_cache().unwrap();
    assert!(!tmp_file.exists());
}
