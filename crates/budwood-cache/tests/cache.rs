//! The cache in front of a real budwood-server, and in front of fake
//! upstreams that misbehave or count requests.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use budwood_cache::http::CACHE_STATUS_HEADER;
use budwood_core::api::{codes, CommitRequest, CommitResponse, ErrorBody, IdList, LogResponse, Revision, MAX_BATCH};
use budwood_core::{Change, FileKind, FileState, ObjectId, RepoPath};
use common::*;
use reqwest::StatusCode;
use serde_json::json;

async fn get_object(base: &str, id: &ObjectId) -> reqwest::Response {
    http().get(repo_url(base, &format!("objects/{id}"))).send().await.unwrap()
}

async fn fetch(base: &str, ids: &[ObjectId]) -> reqwest::Response {
    http().post(repo_url(base, "objects/fetch")).json(&IdList { ids: ids.to_vec() }).send().await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn object_miss_then_hit() {
    let server = start_server().await;
    let cache = start_cache(&server.url, None).await;
    let data = b"hello, cache";
    let id = ObjectId::for_blob(data);
    server.service.put_blob(REPO, &id, data).unwrap();

    let resp = get_object(&cache.url, &id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[CACHE_STATUS_HEADER], "miss");
    assert!(resp.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    assert_eq!(&resp.bytes().await.unwrap()[..], data);
    let stats = cache.stats();
    assert_eq!((stats.hits, stats.misses, stats.upstream_fetches), (0, 1, 1));
    assert_eq!((stats.objects_cached, stats.bytes_cached), (1, data.len() as u64));

    let resp = get_object(&cache.url, &id).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[CACHE_STATUS_HEADER], "hit");
    assert_eq!(&resp.bytes().await.unwrap()[..], data);
    let stats = cache.stats();
    assert_eq!((stats.hits, stats.misses, stats.upstream_fetches), (1, 1, 1));

    // The stats endpoint reports the same counters.
    let stats: budwood_cache::CacheStats =
        http().get(format!("{}/v1/cache/stats", cache.url)).send().await.unwrap().json().await.unwrap();
    assert_eq!((stats.hits, stats.misses, stats.upstream_fetches, stats.bytes_cached), (1, 1, 1, 12));
}

#[tokio::test(flavor = "multi_thread")]
async fn trees_verify_and_errors_pass_through() {
    let server = start_server().await;
    let cache = start_cache(&server.url, None).await;
    let commit = commit_files(&server.service, &[("dir/a.txt", b"a"), ("b.txt", b"b")]);

    let resp = get_object(&cache.url, &commit.changeset.root_tree).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let tree = budwood_core::Tree::decode(&resp.bytes().await.unwrap()).unwrap();
    assert_eq!(tree.id(), commit.changeset.root_tree);
    assert!(tree.get("dir").is_some());

    // Upstream's 404 comes back as is, and nothing is cached.
    let absent = ObjectId::for_blob(b"never uploaded");
    for _ in 0..2 {
        let resp = get_object(&cache.url, &absent).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(resp.json::<ErrorBody>().await.unwrap().code, codes::NOT_FOUND);
    }
    assert_eq!(cache.stats().upstream_fetches, 3);
    assert_eq!(cache.stats().objects_cached, 1);

    let resp = http().get(repo_url(&cache.url, "objects/not-hex")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(resp.json::<ErrorBody>().await.unwrap().code, codes::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn batch_fetch_with_partial_hits() {
    let server = start_server().await;
    let cache = start_cache(&server.url, None).await;
    let blobs: Vec<&[u8]> = vec![b"alpha", b"beta", b"gamma"];
    let ids: Vec<ObjectId> = blobs.iter().map(|b| ObjectId::for_blob(b)).collect();
    for (id, data) in ids.iter().zip(&blobs) {
        server.service.put_blob(REPO, id, data).unwrap();
    }
    // Warm one of them.
    assert_eq!(get_object(&cache.url, &ids[0]).await.status(), StatusCode::OK);

    let request = [ids[2], ids[0], ids[1], ids[0]];
    let resp = fetch(&cache.url, &request).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let got = frames(&resp.bytes().await.unwrap());
    let want: Vec<(ObjectId, Vec<u8>)> = [2, 0, 1, 0].iter().map(|&i| (ids[i], blobs[i].to_vec())).collect();
    assert_eq!(got, want);
    let stats = cache.stats();
    // One upstream GET for the warm-up, then one batch for the two misses.
    assert_eq!((stats.hits, stats.misses, stats.upstream_fetches, stats.objects_cached), (1, 3, 2, 3));

    // Now everything is local.
    let resp = fetch(&cache.url, &request).await;
    assert_eq!(frames(&resp.bytes().await.unwrap()), want);
    assert_eq!(cache.stats().upstream_fetches, 2);

    // A missing object fails the batch with upstream's error.
    let resp = fetch(&cache.url, &[ids[0], ObjectId::for_blob(b"absent")]).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let too_many = vec![ids[0]; MAX_BATCH + 1];
    assert_eq!(fetch(&cache.url, &too_many).await.status(), StatusCode::BAD_REQUEST);
}

/// An upstream that returns the wrong bytes for every object.
fn corrupt_upstream() -> Router {
    Router::new().route("/v1/repos/{repo}/objects/{id}", get(|| async { "not what you asked for" })).route(
        "/v1/repos/{repo}/objects/fetch",
        post(|Json(req): Json<IdList>| async move {
            let mut out = Vec::new();
            for id in req.ids {
                out.extend_from_slice(id.as_bytes());
                out.extend_from_slice(&5u64.to_le_bytes());
                out.extend_from_slice(b"bogus");
            }
            out
        }),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_upstream_data_is_rejected() {
    let upstream = start_fake(corrupt_upstream()).await;
    let cache = start_cache(&upstream, None).await;
    let id = ObjectId::for_blob(b"the real content");

    for _ in 0..2 {
        let resp = get_object(&cache.url, &id).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(resp.json::<ErrorBody>().await.unwrap().code, codes::UNAVAILABLE);
    }
    let resp = fetch(&cache.url, &[id, ObjectId::for_blob(b"other")]).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let stats = cache.stats();
    assert_eq!((stats.rejected, stats.upstream_fetches, stats.objects_cached), (3, 3, 0));
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_batch_response_is_rejected() {
    let upstream = start_fake(Router::new().route(
        "/v1/repos/{repo}/objects/fetch",
        post(|Json(req): Json<IdList>| async move { req.ids[0].as_bytes().to_vec() }),
    ))
    .await;
    let cache = start_cache(&upstream, None).await;
    let resp = fetch(&cache.url, &[ObjectId::for_blob(b"x")]).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test(flavor = "multi_thread")]
async fn proxies_mutable_api() {
    let server = start_server().await;
    let cache = start_cache(&server.url, None).await;
    let client = http();

    let head: Revision = client.get(repo_url(&cache.url, "head")).send().await.unwrap().json().await.unwrap();
    assert_eq!(head.seq, 0);

    // Upload and commit through the cache.
    let data = b"fn main() {}\n";
    let blob = ObjectId::for_blob(data);
    let resp = client.put(repo_url(&cache.url, &format!("objects/{blob}"))).body(&data[..]).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let missing: IdList = client
        .post(repo_url(&cache.url, "objects/missing"))
        .json(&IdList { ids: vec![blob] })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(missing.ids.is_empty());
    for (n, path) in ["src/main.rs", "src/other.rs"].into_iter().enumerate() {
        let req = CommitRequest {
            base: n as u64,
            author: "tester".into(),
            message: format!("commit {n}"),
            changes: vec![Change::set(RepoPath::new(path).unwrap(), FileState { kind: FileKind::File, blob })],
        };
        let resp = client.post(repo_url(&cache.url, "commit")).json(&req).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp: CommitResponse = resp.json().await.unwrap();
        assert_eq!(resp.changeset.seq, n as u64 + 1);
    }

    // Reads through the cache match the server's own answers.
    for rest in ["head", "log", "log?limit=1", "changesets/1", "tree?path=src", "diff?from=0", "file?path=src/main.rs"]
    {
        let via_cache = client.get(repo_url(&cache.url, rest)).send().await.unwrap();
        let direct = client.get(repo_url(&server.url, rest)).send().await.unwrap();
        assert_eq!(via_cache.status(), StatusCode::OK, "{rest}");
        assert_eq!(via_cache.headers().get("content-type"), direct.headers().get("content-type"), "{rest}");
        assert_eq!(via_cache.bytes().await.unwrap(), direct.bytes().await.unwrap(), "{rest}");
    }
    let log: LogResponse = client.get(repo_url(&cache.url, "log?limit=1")).send().await.unwrap().json().await.unwrap();
    assert_eq!(log.changesets.len(), 1);
    assert_eq!(log.changesets[0].seq, 2);

    // Errors and other routes pass through too.
    let resp = client.get(format!("{}/v1/repos/nope/head", cache.url)).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(resp.json::<ErrorBody>().await.unwrap().code, codes::NOT_FOUND);
    let stale = CommitRequest {
        base: 1,
        author: "tester".into(),
        message: "conflicting".into(),
        changes: vec![Change::delete(RepoPath::new("src/other.rs").unwrap())],
    };
    let resp = client.post(repo_url(&cache.url, "commit")).json(&stale).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let resp = client.post(format!("{}/v1/repos", cache.url)).json(&json!({"name": "second"})).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = client.get(format!("{}/v1/health", cache.url)).send().await.unwrap();
    assert_eq!(resp.text().await.unwrap(), "ok");

    // None of that touched the object cache.
    assert_eq!(cache.stats(), budwood_cache::CacheStats::default());
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_reports_unreachable_upstream() {
    // Bind and drop to find a port nobody is listening on.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let cache = start_cache(&dead, None).await;
    let resp = http().get(repo_url(&cache.url, "head")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let resp = get_object(&cache.url, &ObjectId::for_blob(b"x")).await;
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
}

const SLOW_PAYLOAD: &[u8] = b"fetched exactly once";

/// An upstream that serves [`SLOW_PAYLOAD`] slowly and counts requests.
async fn slow_object(State(count): State<Arc<AtomicUsize>>) -> &'static [u8] {
    count.fetch_add(1, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
    SLOW_PAYLOAD
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_misses_share_one_upstream_fetch() {
    let count = Arc::new(AtomicUsize::new(0));
    let router = Router::new().route("/v1/repos/{repo}/objects/{id}", get(slow_object)).with_state(count.clone());
    let upstream = start_fake(router).await;
    let cache = start_cache(&upstream, None).await;
    let id = ObjectId::for_blob(SLOW_PAYLOAD);

    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let url = cache.url.clone();
        tasks.spawn(async move {
            let resp = get_object(&url, &id).await;
            assert_eq!(resp.status(), StatusCode::OK);
            resp.bytes().await.unwrap()
        });
    }
    while let Some(body) = tasks.join_next().await {
        assert_eq!(&body.unwrap()[..], SLOW_PAYLOAD);
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let stats = cache.stats();
    assert_eq!(stats.upstream_fetches, 1);
    assert_eq!(stats.hits + stats.misses, 32);
    assert!(stats.coalesced > 0);

    // The flight is gone once it finishes, and later reads are plain hits.
    assert_eq!(get_object(&cache.url, &id).await.headers()[CACHE_STATUS_HEADER], "hit");
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn tiers_chain() {
    let server = start_server().await;
    let regional = start_cache(&server.url, None).await;
    let local = start_cache(&regional.url, None).await;
    let commit = commit_files(&server.service, &[("a.txt", b"chained")]);
    let blob = ObjectId::for_blob(b"chained");

    for id in [blob, commit.changeset.root_tree] {
        assert_eq!(get_object(&local.url, &id).await.status(), StatusCode::OK);
    }
    assert_eq!(fetch(&local.url, &[blob, commit.changeset.root_tree]).await.status(), StatusCode::OK);
    assert_eq!((local.stats().upstream_fetches, regional.stats().upstream_fetches), (2, 2));

    // A second local tier finds everything in the regional one.
    let other = start_cache(&regional.url, None).await;
    let resp = fetch(&other.url, &[blob, commit.changeset.root_tree]).await;
    assert_eq!(frames(&resp.bytes().await.unwrap()).len(), 2);
    assert_eq!(regional.stats().hits, 2);
    assert_eq!(regional.stats().upstream_fetches, 2);

    // Mutable requests go all the way up.
    let head: Revision = http().get(repo_url(&local.url, "head")).send().await.unwrap().json().await.unwrap();
    assert_eq!(head.seq, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn size_bound_evicts_old_objects() {
    let server = start_server().await;
    let cache = start_cache(&server.url, Some(20)).await;
    let blobs: Vec<Vec<u8>> = (0..3u8).map(|n| vec![n; 8]).collect();
    let ids: Vec<ObjectId> = blobs.iter().map(|b| ObjectId::for_blob(b)).collect();
    for (id, data) in ids.iter().zip(&blobs) {
        server.service.put_blob(REPO, id, data).unwrap();
        assert_eq!(get_object(&cache.url, id).await.status(), StatusCode::OK);
    }
    let stats = cache.stats();
    assert_eq!((stats.objects_cached, stats.bytes_cached, stats.evictions), (2, 16, 1));
    assert_eq!(stats.max_cache_bytes, Some(20));
    // The oldest one is fetched again.
    assert_eq!(get_object(&cache.url, &ids[0]).await.headers()[CACHE_STATUS_HEADER], "miss");
    assert_eq!(get_object(&cache.url, &ids[2]).await.headers()[CACHE_STATUS_HEADER], "hit");
}
