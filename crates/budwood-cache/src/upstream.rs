//! The HTTP client for the next tier up: a budwood-server, or another cache.

use std::time::Duration;

use anyhow::{bail, Context};
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::Request;
use axum::http::{header, HeaderMap, HeaderName, StatusCode};
use axum::response::Response;
use budwood_core::api::IdList;
use budwood_core::ObjectId;
use reqwest::Client;

use crate::FetchError;

/// Headers that describe one connection rather than the message, which a
/// proxy must not forward (RFC 9110 section 7.6.1), plus `host`.
const HOP_BY_HOP: &[HeaderName] = &[
    header::CONNECTION,
    header::PROXY_AUTHENTICATE,
    header::PROXY_AUTHORIZATION,
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::HOST,
];

/// How long one object request may take, end to end.
const OBJECT_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
pub struct Upstream {
    base: String,
    http: Client,
}

impl Upstream {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        if !(base.starts_with("http://") || base.starts_with("https://")) {
            bail!("upstream must be an http:// or https:// URL, got {base:?}");
        }
        let http = Client::builder()
            .user_agent(concat!("budwood-cache/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .context("building HTTP client")?;
        Ok(Self { base, http })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    fn object_url(&self, repo: &str, rest: &str) -> String {
        format!("{}/v1/repos/{repo}/objects/{rest}", self.base)
    }

    /// Fetches one object's bytes, unverified.
    pub async fn get_object(&self, repo: &str, id: &ObjectId) -> Result<Bytes, FetchError> {
        let req = self.http.get(self.object_url(repo, &id.to_hex())).timeout(OBJECT_TIMEOUT);
        read_success(req.send().await).await
    }

    /// Fetches a batch of objects as `id[32] len:u64le bytes` frames, unverified.
    pub async fn fetch(&self, repo: &str, ids: &[ObjectId]) -> Result<Bytes, FetchError> {
        let req =
            self.http.post(self.object_url(repo, "fetch")).json(&IdList { ids: ids.to_vec() }).timeout(OBJECT_TIMEOUT);
        read_success(req.send().await).await
    }

    /// Forwards a request verbatim and streams the response back.
    pub async fn forward(&self, req: Request) -> Result<Response, reqwest::Error> {
        let (parts, body) = req.into_parts();
        let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
        let mut headers = parts.headers;
        strip_hop_by_hop(&mut headers);
        let mut upstream = self.http.request(parts.method, format!("{}{path}", self.base)).headers(headers);
        if !body.is_end_stream() {
            upstream = upstream.body(reqwest::Body::wrap_stream(body.into_data_stream()));
        }
        let resp = upstream.send().await?;

        let status = resp.status();
        let mut headers = resp.headers().clone();
        strip_hop_by_hop(&mut headers);
        let mut out = Response::new(Body::from_stream(resp.bytes_stream()));
        *out.status_mut() = status;
        *out.headers_mut() = headers;
        Ok(out)
    }
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    // `Connection` may name further per-connection headers.
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::try_from(name.trim()).ok())
        .collect();
    for name in HOP_BY_HOP.iter().chain(&named) {
        headers.remove(name);
    }
    headers.remove("keep-alive");
}

async fn read_success(resp: reqwest::Result<reqwest::Response>) -> Result<Bytes, FetchError> {
    let resp = resp.map_err(transport)?;
    let status = resp.status();
    if status.is_success() {
        return resp.bytes().await.map_err(transport);
    }
    let content_type = resp.headers().get(header::CONTENT_TYPE).cloned();
    let body = resp.bytes().await.unwrap_or_default();
    Err(FetchError::Status { status: StatusCode::from_u16(status.as_u16()).unwrap(), content_type, body })
}

fn transport(e: reqwest::Error) -> FetchError {
    // `{:#}`-style chain: reqwest's top-level message omits the cause.
    let mut message = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    FetchError::Transport(message)
}

/// Splits a batch response into `(id, bytes)` frames.
pub fn parse_frames(body: &Bytes) -> anyhow::Result<Vec<(ObjectId, Bytes)>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < body.len() {
        if body.len() - at < 40 {
            bail!("truncated object frame");
        }
        let id = ObjectId::from_bytes(body[at..at + 32].try_into().unwrap());
        let len = u64::from_le_bytes(body[at + 32..at + 40].try_into().unwrap());
        let start = at + 40;
        let end = usize::try_from(len).ok().and_then(|len| start.checked_add(len)).filter(|&end| end <= body.len());
        let Some(end) = end else { bail!("truncated object {id}") };
        out.push((id, body.slice(start..end)));
        at = end;
    }
    Ok(out)
}

/// Appends one batch frame to `out`.
pub fn push_frame(out: &mut Vec<u8>, id: &ObjectId, data: &[u8]) {
    out.extend_from_slice(id.as_bytes());
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
}
