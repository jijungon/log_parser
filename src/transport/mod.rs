pub mod http;
pub mod spool;

use crate::config::TransportConfig;
use anyhow::Result;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("재시도 가능: {0}")]
    Retryable(String),
    #[error("치명 오류: {0}")]
    Fatal(String),
    #[error("rate-limit: {retry_after}초 후 재시도")]
    RateLimited { retry_after: u64 },
}

pub fn create(cfg: &TransportConfig) -> Result<Arc<http::HttpJsonTransport>> {
    match cfg.kind.as_str() {
        "http_json" => Ok(Arc::new(http::HttpJsonTransport::new(cfg)?)),
        other => anyhow::bail!("지원하지 않는 transport.kind: {other}"),
    }
}
