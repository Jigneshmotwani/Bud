//! The cache's HTTP front end: the v1 API, with object reads answered from
//! the cache and everything else proxied upstream, plus `GET /v1/cache/stats`.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use budwood_core::api::{codes, ErrorBody, IdList, MAX_BATCH};
use budwood_core::ObjectId;

use crate::upstream::push_frame;
use crate::{Cache, CacheStats, FetchError};

type St = State<Arc<Cache>>;

/// Response header saying whether an object came from the cache: `hit` or `miss`.
pub const CACHE_STATUS_HEADER: &str = "x-budwood-cache";

pub fn router(cache: Arc<Cache>) -> Router {
    Router::new()
        .route("/v1/cache/stats", get(stats))
        .route("/v1/repos/{repo}/objects/fetch", post(fetch).fallback(proxy))
        .route("/v1/repos/{repo}/objects/{id}", get(get_object).fallback(proxy))
        .fallback(proxy)
        .with_state(cache)
}

fn error(status: StatusCode, code: &str, message: String) -> Response {
    let body = ErrorBody { code: code.to_string(), message, paths: vec![], missing: vec![] };
    (status, Json(body)).into_response()
}

impl IntoResponse for FetchError {
    fn into_response(self) -> Response {
        match self {
            FetchError::Status { status, content_type, body } => {
                let mut resp = (status, body).into_response();
                if let Some(content_type) = content_type {
                    resp.headers_mut().insert(header::CONTENT_TYPE, content_type);
                }
                resp
            }
            FetchError::Corrupt(_) | FetchError::Transport(_) => {
                error(StatusCode::BAD_GATEWAY, codes::UNAVAILABLE, self.to_string())
            }
            FetchError::BadRequest(message) => error(StatusCode::BAD_REQUEST, codes::BAD_REQUEST, message),
            FetchError::Internal(_) => {
                tracing::error!("{self}");
                error(StatusCode::INTERNAL_SERVER_ERROR, codes::INTERNAL, self.to_string())
            }
        }
    }
}

async fn stats(State(cache): St) -> Json<CacheStats> {
    Json(cache.stats())
}

async fn get_object(State(cache): St, Path((repo, id)): Path<(String, String)>) -> Result<Response, FetchError> {
    let id = ObjectId::from_hex(&id).map_err(|e| FetchError::BadRequest(e.to_string()))?;
    let (data, hit) = cache.object(&repo, id).await?;
    let status = HeaderValue::from_static(if hit { "hit" } else { "miss" });
    Ok((
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream")),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable")),
            (header::HeaderName::from_static(CACHE_STATUS_HEADER), status),
        ],
        data,
    )
        .into_response())
}

async fn fetch(State(cache): St, Path(repo): Path<String>, Json(req): Json<IdList>) -> Result<Response, FetchError> {
    if req.ids.len() > MAX_BATCH {
        return Err(FetchError::BadRequest(format!("at most {MAX_BATCH} ids per request")));
    }
    let objects = cache.objects(&repo, &req.ids).await?;
    let mut out = Vec::with_capacity(objects.iter().map(|(_, data)| 40 + data.len()).sum());
    for (id, data) in &objects {
        push_frame(&mut out, id, data);
    }
    Ok(([(header::CONTENT_TYPE, "application/octet-stream"), (header::CACHE_CONTROL, "no-cache")], out).into_response())
}

async fn proxy(State(cache): St, req: Request) -> Response {
    let target = format!("{} {}", req.method(), req.uri());
    match cache.upstream().forward(req).await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::warn!("proxying {target}: {e}");
            error(StatusCode::BAD_GATEWAY, codes::UNAVAILABLE, format!("upstream request failed: {e}"))
        }
    }
}
