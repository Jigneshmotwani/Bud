//! The v1 HTTP API. Handlers are thin: each runs one [`Service`] call on the
//! blocking thread pool, since storage is synchronous.
//!
//! Every error, including the framework's own rejections (malformed JSON, a
//! bad query string, an oversized body, an unknown route), is returned as a
//! JSON [`ErrorBody`] with `Cache-Control: no-store`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request, State};
use axum::http::request::Parts;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use budwood_core::api::{
    codes, BlameResponse, CommitRequest, CommitResponse, CreateRepoRequest, DiffResponse, ErrorBody, IdList,
    LogResponse, RepoList, RevertRequest, Revision, TreeResponse,
};
use budwood_core::{Changeset, ObjectId, RepoPath};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use crate::service::{Service, ServiceError, ServiceResult};

type Svc = State<Arc<Service>>;

pub fn router(service: Arc<Service>, max_body_bytes: usize) -> Router {
    Router::new()
        .route("/v1/health", get(|| async { "ok" }))
        .route("/v1/repos", get(list_repos).post(create_repo))
        .route("/v1/repos/{repo}/head", get(head))
        .route("/v1/repos/{repo}/log", get(log))
        .route("/v1/repos/{repo}/changesets/{rev}", get(changeset))
        .route("/v1/repos/{repo}/changesets/{rev}/revert", post(revert))
        .route("/v1/repos/{repo}/commit", post(commit))
        .route("/v1/repos/{repo}/tree", get(tree))
        .route("/v1/repos/{repo}/file", get(file))
        .route("/v1/repos/{repo}/diff", get(diff))
        .route("/v1/repos/{repo}/blame", get(blame))
        .route("/v1/repos/{repo}/objects/missing", post(missing))
        .route("/v1/repos/{repo}/objects/fetch", post(fetch))
        .route("/v1/repos/{repo}/objects/{id}", get(get_object).put(put_object))
        .fallback(|| async { ApiError::rejected(StatusCode::NOT_FOUND, "no such route".into()) })
        .method_not_allowed_fallback(|| async {
            ApiError::rejected(StatusCode::METHOD_NOT_ALLOWED, "method not allowed for this route".into())
        })
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(service)
}

pub enum ApiError {
    Service(ServiceError),
    /// A request the framework rejected before it reached the service.
    Rejected {
        status: StatusCode,
        message: String,
    },
}

impl ApiError {
    fn rejected(status: StatusCode, message: String) -> Self {
        Self::Rejected { status, message }
    }
}

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        Self::Service(e)
    }
}

fn error_response(status: StatusCode, body: ErrorBody) -> Response {
    // Errors are never cacheable: a shared cache must not pin a 404 for an
    // object or revision that is about to exist.
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let error = match self {
            ApiError::Service(error) => error,
            ApiError::Rejected { status, message } => {
                let code = match status {
                    StatusCode::PAYLOAD_TOO_LARGE => codes::PAYLOAD_TOO_LARGE,
                    StatusCode::NOT_FOUND => codes::NOT_FOUND,
                    s if s.is_server_error() => codes::INTERNAL,
                    _ => codes::BAD_REQUEST,
                };
                let body = ErrorBody { code: code.to_string(), message, paths: vec![], missing: vec![] };
                return error_response(status, body);
            }
        };
        let message = error.to_string();
        let (status, code, paths, missing) = match error {
            ServiceError::NotFound(_) => (StatusCode::NOT_FOUND, codes::NOT_FOUND, vec![], vec![]),
            ServiceError::BadRequest(_) => (StatusCode::BAD_REQUEST, codes::BAD_REQUEST, vec![], vec![]),
            ServiceError::Conflict { paths, .. } => (StatusCode::CONFLICT, codes::CONFLICT, paths, vec![]),
            ServiceError::MissingObjects(ids) => (StatusCode::PRECONDITION_FAILED, codes::MISSING_OBJECTS, vec![], ids),
            ServiceError::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, codes::UNAVAILABLE, vec![], vec![]),
            ServiceError::Internal(ref e) => {
                tracing::error!("internal error: {e:#}");
                (StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL, vec![], vec![])
            }
        };
        let body = ErrorBody { code: code.to_string(), message, paths, missing };
        error_response(status, body)
    }
}

/// [`Json`], with rejections (400, 413, 415, 422) as JSON errors.
pub struct ApiJson<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for ApiJson<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(Self(value)),
            Err(e @ JsonRejection::BytesRejection(_)) => Err(ApiError::rejected(e.status(), body_error(e.body_text()))),
            Err(e) => Err(ApiError::rejected(e.status(), e.body_text())),
        }
    }
}

/// [`axum::extract::Query`], with a 400 JSON error when the query doesn't parse.
pub struct ApiQuery<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for ApiQuery<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Query::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Query(value)) => Ok(Self(value)),
            Err(e) => Err(ApiError::rejected(e.status(), e.body_text())),
        }
    }
}

/// [`axum::extract::Path`], with JSON errors.
pub struct ApiPath<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned + Send> FromRequestParts<S> for ApiPath<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        match axum::extract::Path::<T>::from_request_parts(parts, state).await {
            Ok(axum::extract::Path(value)) => Ok(Self(value)),
            Err(e) => Err(ApiError::rejected(e.status(), e.body_text())),
        }
    }
}

/// A raw body, with a 413 JSON error when it exceeds the body limit.
pub struct ApiBytes(pub Bytes);

impl<S: Send + Sync> FromRequest<S> for ApiBytes {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        match Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(Self(bytes)),
            Err(e) => Err(ApiError::rejected(e.status(), body_error(e.body_text()))),
        }
    }
}

/// axum's message for an oversized body is "length limit exceeded"; say why.
fn body_error(text: String) -> String {
    if text.contains("length limit exceeded") {
        "request body is larger than the server's --max-body-bytes limit".into()
    } else {
        text
    }
}

async fn blocking<T, F>(service: Arc<Service>, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Service) -> ServiceResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&service))
        .await
        .map_err(|e| ApiError::Service(ServiceError::Internal(anyhow::anyhow!("task failed: {e}"))))?
        .map_err(ApiError::Service)
}

fn parse_path(path: Option<String>) -> Result<RepoPath, ApiError> {
    RepoPath::new(path.unwrap_or_default()).map_err(|e| ApiError::Service(ServiceError::BadRequest(e.to_string())))
}

fn parse_id(id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::from_hex(id).map_err(|e| ApiError::Service(ServiceError::BadRequest(e.to_string())))
}

fn bytes_response(data: Vec<u8>, immutable: bool) -> Response {
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "no-cache" };
    ([(header::CONTENT_TYPE, "application/octet-stream"), (header::CACHE_CONTROL, cache)], data).into_response()
}

async fn list_repos(State(svc): Svc) -> Result<Json<RepoList>, ApiError> {
    let repos = blocking(svc, |s| s.list_repos()).await?;
    Ok(Json(RepoList { repos }))
}

async fn create_repo(State(svc): Svc, ApiJson(req): ApiJson<CreateRepoRequest>) -> Result<StatusCode, ApiError> {
    let created = blocking(svc, move |s| s.create_repo(&req.name)).await?;
    Ok(if created { StatusCode::CREATED } else { StatusCode::OK })
}

async fn head(State(svc): Svc, ApiPath(repo): ApiPath<String>) -> Result<Json<Revision>, ApiError> {
    Ok(Json(blocking(svc, move |s| s.resolve(&repo, "head")).await?))
}

#[derive(Deserialize)]
struct LogQuery {
    path: Option<String>,
    before: Option<u64>,
    limit: Option<usize>,
}

async fn log(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiQuery(q): ApiQuery<LogQuery>,
) -> Result<Json<LogResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let limit = q.limit.unwrap_or(50);
    let changesets = blocking(svc, move |s| s.log(&repo, &path, q.before, limit)).await?;
    Ok(Json(LogResponse { changesets }))
}

async fn changeset(
    State(svc): Svc,
    ApiPath((repo, rev)): ApiPath<(String, String)>,
) -> Result<Json<Changeset>, ApiError> {
    Ok(Json(blocking(svc, move |s| s.changeset(&repo, &rev)).await?))
}

async fn commit(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiJson(req): ApiJson<CommitRequest>,
) -> Result<Json<CommitResponse>, ApiError> {
    let resp = blocking(svc, move |s| s.commit(&repo, req)).await?;
    tracing::info!(seq = resp.changeset.seq, files = resp.changeset.changes.len(), rebased = resp.rebased, "committed");
    Ok(Json(resp))
}

async fn revert(
    State(svc): Svc,
    ApiPath((repo, rev)): ApiPath<(String, String)>,
    ApiJson(req): ApiJson<RevertRequest>,
) -> Result<Json<CommitResponse>, ApiError> {
    let resp = blocking(svc, move |s| s.revert(&repo, &rev, req)).await?;
    tracing::info!(seq = resp.changeset.seq, base = resp.changeset.base, rebased = resp.rebased, "reverted");
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct RevQuery {
    rev: Option<String>,
    path: Option<String>,
}

async fn tree(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiQuery(q): ApiQuery<RevQuery>,
) -> Result<Json<TreeResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let rev = q.rev.unwrap_or_default();
    Ok(Json(blocking(svc, move |s| s.tree(&repo, &rev, &path)).await?))
}

async fn file(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiQuery(q): ApiQuery<RevQuery>,
) -> Result<Response, ApiError> {
    let path = parse_path(q.path)?;
    let rev = q.rev.unwrap_or_default();
    let data = blocking(svc, move |s| s.read_file(&repo, &rev, &path)).await?;
    Ok(bytes_response(data, false))
}

#[derive(Deserialize)]
struct DiffQuery {
    from: String,
    to: Option<String>,
    path: Option<String>,
}

async fn diff(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiQuery(q): ApiQuery<DiffQuery>,
) -> Result<Json<DiffResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let to = q.to.unwrap_or_default();
    Ok(Json(blocking(svc, move |s| s.diff(&repo, &q.from, &to, &path)).await?))
}

async fn blame(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiQuery(q): ApiQuery<RevQuery>,
) -> Result<Json<BlameResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let rev = q.rev.unwrap_or_default();
    Ok(Json(blocking(svc, move |s| s.blame(&repo, &rev, &path)).await?))
}

async fn missing(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiJson(req): ApiJson<IdList>,
) -> Result<Json<IdList>, ApiError> {
    let ids = blocking(svc, move |s| s.missing(&repo, &req.ids)).await?;
    Ok(Json(IdList { ids }))
}

async fn fetch(
    State(svc): Svc,
    ApiPath(repo): ApiPath<String>,
    ApiJson(req): ApiJson<IdList>,
) -> Result<Response, ApiError> {
    let data = blocking(svc, move |s| s.fetch(&repo, &req.ids)).await?;
    Ok(bytes_response(data, false))
}

async fn get_object(State(svc): Svc, ApiPath((repo, id)): ApiPath<(String, String)>) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let data = blocking(svc, move |s| s.get_object(&repo, &id)).await?;
    Ok(bytes_response(data, true))
}

async fn put_object(
    State(svc): Svc,
    ApiPath((repo, id)): ApiPath<(String, String)>,
    ApiBytes(body): ApiBytes,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    blocking(svc, move |s| s.put_blob(&repo, &id, &body)).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Method;
    use budwood_core::api::BlameResponse;
    use budwood_core::{Change, FileKind, FileState};
    use tower::ServiceExt;

    use super::*;
    use crate::meta::MetaStore;
    use crate::store::MemObjectStore;

    const LIMIT: usize = 1024;

    fn app() -> (Router, Arc<Service>) {
        let svc =
            Arc::new(Service::new(Arc::new(MemObjectStore::default()), MetaStore::open_in_memory().unwrap()).unwrap());
        svc.create_repo("r").unwrap();
        (router(svc.clone(), LIMIT), svc)
    }

    struct Reply {
        status: StatusCode,
        cache: Option<String>,
        body: Vec<u8>,
    }

    impl Reply {
        fn error(&self) -> ErrorBody {
            serde_json::from_slice(&self.body)
                .unwrap_or_else(|e| panic!("not an ErrorBody ({e}): {}", String::from_utf8_lossy(&self.body)))
        }

        fn json<T: DeserializeOwned>(&self) -> T {
            serde_json::from_slice(&self.body).unwrap()
        }
    }

    async fn call(app: &Router, method: Method, uri: &str, content_type: Option<&str>, body: Vec<u8>) -> Reply {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        let resp = app.clone().oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
        let status = resp.status();
        let cache = resp.headers().get(header::CACHE_CONTROL).map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec();
        Reply { status, cache, body }
    }

    async fn get(app: &Router, uri: &str) -> Reply {
        call(app, Method::GET, uri, None, vec![]).await
    }

    async fn post_json(app: &Router, uri: &str, body: &str) -> Reply {
        call(app, Method::POST, uri, Some("application/json"), body.as_bytes().to_vec()).await
    }

    fn assert_error(reply: &Reply, status: StatusCode, code: &str) -> ErrorBody {
        assert_eq!(reply.status, status, "{}", String::from_utf8_lossy(&reply.body));
        assert_eq!(reply.cache.as_deref(), Some("no-store"));
        let body = reply.error();
        assert_eq!(body.code, code, "{}", body.message);
        assert!(!body.message.is_empty());
        body
    }

    #[tokio::test]
    async fn service_errors_are_not_cacheable() {
        let (app, _) = app();
        assert_error(&get(&app, "/v1/repos/nope/head").await, StatusCode::NOT_FOUND, codes::NOT_FOUND);
        let missing = ObjectId::for_blob(b"later");
        assert_error(
            &get(&app, &format!("/v1/repos/r/objects/{missing}")).await,
            StatusCode::NOT_FOUND,
            codes::NOT_FOUND,
        );
        assert_error(&get(&app, "/v1/repos/r/objects/xyz").await, StatusCode::BAD_REQUEST, codes::BAD_REQUEST);
        assert_error(&get(&app, "/v1/repos/r/tree?rev=bogus").await, StatusCode::BAD_REQUEST, codes::BAD_REQUEST);
        // Success responses keep their caching headers.
        let ok = get(&app, "/v1/repos/r/head").await;
        assert_eq!((ok.status, ok.cache), (StatusCode::OK, None));
        let empty = get(&app, &format!("/v1/repos/r/objects/{}", budwood_core::empty_tree_id())).await;
        assert_eq!(empty.cache.as_deref(), Some("public, max-age=31536000, immutable"));
    }

    #[tokio::test]
    async fn framework_rejections_are_json() {
        let (app, _) = app();
        // Malformed JSON.
        assert_error(&post_json(&app, "/v1/repos", "{").await, StatusCode::BAD_REQUEST, codes::BAD_REQUEST);
        // Well-formed JSON of the wrong shape, including a bad id.
        assert_error(&post_json(&app, "/v1/repos", "{}").await, StatusCode::UNPROCESSABLE_ENTITY, codes::BAD_REQUEST);
        let bad_id = post_json(&app, "/v1/repos/r/objects/missing", r#"{"ids":["zz"]}"#).await;
        assert_error(&bad_id, StatusCode::UNPROCESSABLE_ENTITY, codes::BAD_REQUEST);
        // Missing content type.
        let reply = call(&app, Method::POST, "/v1/repos", None, br#"{"name":"x"}"#.to_vec()).await;
        assert_error(&reply, StatusCode::UNSUPPORTED_MEDIA_TYPE, codes::BAD_REQUEST);
        // Query strings that don't parse.
        assert_error(&get(&app, "/v1/repos/r/diff").await, StatusCode::BAD_REQUEST, codes::BAD_REQUEST);
        assert_error(&get(&app, "/v1/repos/r/log?limit=lots").await, StatusCode::BAD_REQUEST, codes::BAD_REQUEST);
        // Bodies over the limit, raw and JSON.
        let id = ObjectId::for_blob(&[b'x'; LIMIT + 1]);
        let reply = call(&app, Method::PUT, &format!("/v1/repos/r/objects/{id}"), None, vec![b'x'; LIMIT + 1]).await;
        let body = assert_error(&reply, StatusCode::PAYLOAD_TOO_LARGE, codes::PAYLOAD_TOO_LARGE);
        assert!(body.message.contains("max-body-bytes"), "{}", body.message);
        let big = format!(r#"{{"name":"{}"}}"#, "a".repeat(LIMIT));
        assert_error(
            &post_json(&app, "/v1/repos", &big).await,
            StatusCode::PAYLOAD_TOO_LARGE,
            codes::PAYLOAD_TOO_LARGE,
        );
        // Unknown routes and methods.
        assert_error(&get(&app, "/v1/nothing").await, StatusCode::NOT_FOUND, codes::NOT_FOUND);
        let reply = call(&app, Method::DELETE, "/v1/repos/r/head", None, vec![]).await;
        assert_error(&reply, StatusCode::METHOD_NOT_ALLOWED, codes::BAD_REQUEST);
    }

    fn put(svc: &Service, content: &str) -> FileState {
        let blob = ObjectId::for_blob(content.as_bytes());
        svc.put_blob("r", &blob, content.as_bytes()).unwrap();
        FileState { kind: FileKind::File, blob }
    }

    #[tokio::test]
    async fn blame_and_revert_routes() {
        let (app, svc) = app();
        let path = |s: &str| RepoPath::new(s).unwrap();
        let (v1, v2) = (put(&svc, "a\nb\n"), put(&svc, "a\nB\n"));
        for (base, state) in [(0, v1), (1, v2)] {
            let req = CommitRequest {
                base,
                author: format!("author{base}"),
                message: format!("change {base}"),
                changes: vec![Change::set(path("f.txt"), state)],
            };
            svc.commit("r", req).unwrap();
        }

        let reply = get(&app, "/v1/repos/r/blame?path=f.txt&rev=head").await;
        assert_eq!(reply.status, StatusCode::OK);
        let blame: BlameResponse = reply.json();
        assert_eq!(blame.revision.seq, 2);
        let summary: Vec<_> =
            blame.lines.iter().map(|l| (l.line_no, l.seq, l.author.as_str(), l.text.as_str())).collect();
        assert_eq!(summary, [(1, 1, "author0", "a"), (2, 2, "author1", "B")]);
        assert_error(&get(&app, "/v1/repos/r/blame?path=nope").await, StatusCode::NOT_FOUND, codes::NOT_FOUND);
        assert_error(&get(&app, "/v1/repos/r/blame").await, StatusCode::BAD_REQUEST, codes::BAD_REQUEST);

        let reply = post_json(&app, "/v1/repos/r/changesets/2/revert", r#"{"author":"bot"}"#).await;
        assert_eq!(reply.status, StatusCode::OK, "{}", String::from_utf8_lossy(&reply.body));
        let resp: CommitResponse = reply.json();
        assert_eq!(resp.changeset.seq, 3);
        assert!(resp.changeset.message.starts_with("Revert \"change 1\"\n\nThis reverts revision 2 ("));
        assert_eq!(resp.changeset.changes, vec![Change::set(path("f.txt"), v1)]);

        // Revision 1 is behind the revert, which touched the same file.
        let reply = post_json(&app, "/v1/repos/r/changesets/1/revert", r#"{"author":"bot","message":"undo"}"#).await;
        let body = assert_error(&reply, StatusCode::CONFLICT, codes::CONFLICT);
        assert_eq!(body.paths, vec![path("f.txt")]);
        let reply = post_json(&app, "/v1/repos/r/changesets/00/revert", r#"{"author":"bot"}"#).await;
        assert_error(&reply, StatusCode::NOT_FOUND, codes::NOT_FOUND);
        let reply = post_json(&app, "/v1/repos/r/changesets/1/revert", r#"{"message":"no author"}"#).await;
        assert_error(&reply, StatusCode::UNPROCESSABLE_ENTITY, codes::BAD_REQUEST);
    }
}
