//! The Budwood server: the authoritative changeset log for one or more
//! repositories, content-addressed object storage, and the v1 HTTP API.

pub mod blame;
pub mod derived;
pub mod http;
pub mod meta;
pub mod service;
pub mod store;

use std::future::Future;
use std::sync::Arc;

pub use service::{Service, ServiceError};

/// Default request body limit, which bounds the size of a single blob.
pub const DEFAULT_MAX_BODY_BYTES: usize = 256 * 1024 * 1024;

/// Serves the API on `listener` until `shutdown` resolves.
pub async fn serve(
    listener: tokio::net::TcpListener,
    service: Arc<Service>,
    max_body_bytes: usize,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, http::router(service, max_body_bytes)).with_graceful_shutdown(shutdown).await
}
