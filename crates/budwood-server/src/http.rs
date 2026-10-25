//! The v1 HTTP API. Handlers are thin: each runs one [`Service`] call on the
//! blocking thread pool, since storage is synchronous.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use budwood_core::api::{
    codes, CommitRequest, CommitResponse, CreateRepoRequest, DiffResponse, ErrorBody, IdList, LogResponse, RepoList,
    Revision, TreeResponse,
};
use budwood_core::{Changeset, ObjectId, RepoPath};
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
        .route("/v1/repos/{repo}/commit", post(commit))
        .route("/v1/repos/{repo}/tree", get(tree))
        .route("/v1/repos/{repo}/file", get(file))
        .route("/v1/repos/{repo}/diff", get(diff))
        .route("/v1/repos/{repo}/objects/missing", post(missing))
        .route("/v1/repos/{repo}/objects/fetch", post(fetch))
        .route("/v1/repos/{repo}/objects/{id}", get(get_object).put(put_object))
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .with_state(service)
}

pub struct ApiError(ServiceError);

impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = self.0.to_string();
        let (status, code, paths, missing) = match self.0 {
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
        (status, Json(body)).into_response()
    }
}

async fn blocking<T, F>(service: Arc<Service>, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Service) -> ServiceResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&service))
        .await
        .map_err(|e| ApiError(ServiceError::Internal(anyhow::anyhow!("task failed: {e}"))))?
        .map_err(ApiError)
}

fn parse_path(path: Option<String>) -> Result<RepoPath, ApiError> {
    RepoPath::new(path.unwrap_or_default()).map_err(|e| ApiError(ServiceError::BadRequest(e.to_string())))
}

fn parse_id(id: &str) -> Result<ObjectId, ApiError> {
    ObjectId::from_hex(id).map_err(|e| ApiError(ServiceError::BadRequest(e.to_string())))
}

fn bytes_response(data: Vec<u8>, immutable: bool) -> Response {
    let cache = if immutable { "public, max-age=31536000, immutable" } else { "no-cache" };
    (
        [(header::CONTENT_TYPE, "application/octet-stream"), (header::CACHE_CONTROL, cache)],
        data,
    )
        .into_response()
}

async fn list_repos(State(svc): Svc) -> Result<Json<RepoList>, ApiError> {
    let repos = blocking(svc, |s| s.list_repos()).await?;
    Ok(Json(RepoList { repos }))
}

async fn create_repo(State(svc): Svc, Json(req): Json<CreateRepoRequest>) -> Result<StatusCode, ApiError> {
    let created = blocking(svc, move |s| s.create_repo(&req.name)).await?;
    Ok(if created { StatusCode::CREATED } else { StatusCode::OK })
}

async fn head(State(svc): Svc, Path(repo): Path<String>) -> Result<Json<Revision>, ApiError> {
    Ok(Json(blocking(svc, move |s| s.resolve(&repo, "head")).await?))
}

#[derive(Deserialize)]
struct LogQuery {
    path: Option<String>,
    before: Option<u64>,
    limit: Option<usize>,
}

async fn log(State(svc): Svc, Path(repo): Path<String>, Query(q): Query<LogQuery>) -> Result<Json<LogResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let limit = q.limit.unwrap_or(50);
    let changesets = blocking(svc, move |s| s.log(&repo, &path, q.before, limit)).await?;
    Ok(Json(LogResponse { changesets }))
}

async fn changeset(State(svc): Svc, Path((repo, rev)): Path<(String, String)>) -> Result<Json<Changeset>, ApiError> {
    Ok(Json(blocking(svc, move |s| s.changeset(&repo, &rev)).await?))
}

async fn commit(State(svc): Svc, Path(repo): Path<String>, Json(req): Json<CommitRequest>) -> Result<Json<CommitResponse>, ApiError> {
    let resp = blocking(svc, move |s| s.commit(&repo, req)).await?;
    tracing::info!(
        seq = resp.changeset.seq,
        files = resp.changeset.changes.len(),
        rebased = resp.rebased,
        "committed"
    );
    Ok(Json(resp))
}

#[derive(Deserialize)]
struct RevQuery {
    rev: Option<String>,
    path: Option<String>,
}

async fn tree(State(svc): Svc, Path(repo): Path<String>, Query(q): Query<RevQuery>) -> Result<Json<TreeResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let rev = q.rev.unwrap_or_default();
    Ok(Json(blocking(svc, move |s| s.tree(&repo, &rev, &path)).await?))
}

async fn file(State(svc): Svc, Path(repo): Path<String>, Query(q): Query<RevQuery>) -> Result<Response, ApiError> {
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

async fn diff(State(svc): Svc, Path(repo): Path<String>, Query(q): Query<DiffQuery>) -> Result<Json<DiffResponse>, ApiError> {
    let path = parse_path(q.path)?;
    let to = q.to.unwrap_or_default();
    Ok(Json(blocking(svc, move |s| s.diff(&repo, &q.from, &to, &path)).await?))
}

async fn missing(State(svc): Svc, Path(repo): Path<String>, Json(req): Json<IdList>) -> Result<Json<IdList>, ApiError> {
    let ids = blocking(svc, move |s| s.missing(&repo, &req.ids)).await?;
    Ok(Json(IdList { ids }))
}

async fn fetch(State(svc): Svc, Path(repo): Path<String>, Json(req): Json<IdList>) -> Result<Response, ApiError> {
    let data = blocking(svc, move |s| s.fetch(&repo, &req.ids)).await?;
    Ok(bytes_response(data, false))
}

async fn get_object(State(svc): Svc, Path((repo, id)): Path<(String, String)>) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let data = blocking(svc, move |s| s.get_object(&repo, &id)).await?;
    Ok(bytes_response(data, true))
}

async fn put_object(State(svc): Svc, Path((repo, id)): Path<(String, String)>, body: Bytes) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    blocking(svc, move |s| s.put_blob(&repo, &id, &body)).await?;
    Ok(StatusCode::NO_CONTENT)
}
