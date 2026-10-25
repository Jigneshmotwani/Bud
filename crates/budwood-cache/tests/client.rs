//! End to end: the blocking `bud` client library working through the cache.
//!
//! The client uses `reqwest::blocking`, which must not run on a tokio worker
//! thread, so the servers run on their own runtime and the client on the
//! test thread.

mod common;

use std::fs;

use budwood_client::{Remote, Workspace};
use common::*;
use tokio::runtime::Runtime;

struct Tiers {
    server: TestServer,
    cache: TestCache,
    // Dropped last, which stops the servers.
    _rt: Runtime,
}

impl Tiers {
    fn start() -> Self {
        let rt = Runtime::new().unwrap();
        let (server, cache) = rt.block_on(async {
            let server = start_server().await;
            let cache = start_cache(&server.url, None).await;
            (server, cache)
        });
        Self { server, cache, _rt: rt }
    }
}

#[test]
fn clone_commit_and_clone_again_through_the_cache() {
    let tiers = Tiers::start();
    let url = &tiers.cache.url;
    assert!(Remote::create_repo(url, "fresh").unwrap());
    assert!(Remote::list_repos(url).unwrap().contains(&"fresh".to_string()));

    let dirs = tempfile::tempdir().unwrap();
    let first = dirs.path().join("first");
    let mut ws = Workspace::clone(url, REPO, &first, vec![]).unwrap();
    fs::create_dir_all(first.join("src")).unwrap();
    fs::write(first.join("README"), "hello\n").unwrap();
    fs::write(first.join("src/lib.rs"), "pub fn f() {}\n").unwrap();
    let outcome = ws.commit("initial import", "tester", &[]).unwrap();
    assert_eq!(outcome.changeset.seq, 1);
    assert_eq!(tiers.server.service.resolve(REPO, "head").unwrap().seq, 1);

    let clone_into = |name: &str| {
        let dir = dirs.path().join(name);
        let ws = Workspace::clone(url, REPO, &dir, vec![]).unwrap();
        assert_eq!(ws.base(), 1);
        assert_eq!(fs::read_to_string(dir.join("README")).unwrap(), "hello\n");
        assert_eq!(fs::read_to_string(dir.join("src/lib.rs")).unwrap(), "pub fn f() {}\n");
    };
    clone_into("second");
    let after_second = tiers.cache.stats();
    assert!(after_second.misses > 0);

    // A third clone is served entirely from the cache.
    clone_into("third");
    let after_third = tiers.cache.stats();
    assert_eq!(after_third.upstream_fetches, after_second.upstream_fetches);
    assert!(after_third.hits > after_second.hits);

    // Commit from the second workspace and sync the first, both via the cache.
    let mut second = Workspace::open(&dirs.path().join("second")).unwrap();
    fs::write(dirs.path().join("second/src/lib.rs"), "pub fn g() {}\n").unwrap();
    assert_eq!(second.commit("rename f", "tester", &[]).unwrap().changeset.seq, 2);
    ws.sync(None).unwrap();
    assert_eq!(fs::read_to_string(first.join("src/lib.rs")).unwrap(), "pub fn g() {}\n");
}
