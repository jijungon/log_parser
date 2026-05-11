pub mod collect;
pub mod drain;
pub mod flush;
pub mod sos;
pub mod stat;

use crate::config::TransportConfig;
use crate::coordinator::FlushSignal;
use crate::envelope::Envelope;
use crate::transport::spool::Spool;
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use drain::DrainState;
use flate2::{write::GzEncoder, Compression};
use flush::RateLimiter;
use std::io::Write as _;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};

pub struct InboundState {
    // flush
    pub flush_tx:              mpsc::Sender<FlushSignal>,
    pub flush_token:           String,
    pub flush_rate:            Mutex<RateLimiter>,
    pub flush_in_flight:       Mutex<bool>,
    pub response_timeout_secs: u64,
    // "reject" → 409 when in-flight; "wait" → poll until done or timeout
    pub serialize_strategy:    String,
    // stat/sos auth + shared rate limiter (60/hour default)
    pub stat_token:       String,
    pub sos_token:        String,
    pub collection_rate:  Mutex<RateLimiter>,
    // max uncompressed envelope JSON size (0 = disabled)
    pub envelope_size_limit_bytes: u64,
    // identity
    pub host:    String,
    pub host_id: String,
    pub boot_id: String,
    // collection options
    pub static_state_enabled: bool,
    // log paths for sos
    pub log_paths: Vec<String>,
    // drain
    pub drain_state:   DrainState,
    pub spool:         Arc<Spool>,
    pub transport_cfg: TransportConfig,
}

/// Bearer 토큰 검증 (상수 시간 비교 — 타이밍 오라클 방지)
pub fn check_auth(authorization: Option<&str>, expected_token: &str) -> bool {
    use subtle::ConstantTimeEq;
    let token = match authorization.and_then(|v| v.strip_prefix("Bearer ")) {
        Some(t) => t,
        None => return false,
    };
    let a = token.as_bytes();
    let b = expected_token.as_bytes();
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

// Inbound response compression level (separate from outbound transport level in http.rs).
const INBOUND_GZIP_LEVEL: u32 = 6;

/// Returns 413 if the uncompressed JSON exceeds limit_bytes; None if within limit or check disabled.
pub fn check_envelope_size(envelope: &Envelope, limit_bytes: u64) -> Option<Response> {
    if limit_bytes == 0 {
        return None;
    }
    let json = match serde_json::to_vec(envelope) {
        Ok(j) => j,
        Err(e) => {
            warn!("envelope serialize failed in size check — skipping limit: {e}");
            return None;
        }
    };
    if json.len() as u64 > limit_bytes {
        warn!(size = json.len(), limit = limit_bytes, "envelope size limit 초과 — 413");
        return Some(StatusCode::PAYLOAD_TOO_LARGE.into_response());
    }
    None
}

/// envelope → gzip 압축 → HTTP Response
pub fn gzip_envelope(envelope: &Envelope) -> Response {
    let json = match serde_json::to_vec(envelope) {
        Ok(v) => v,
        Err(e) => {
            warn!("envelope 직렬화 실패: {e}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let mut enc = GzEncoder::new(Vec::new(), Compression::new(INBOUND_GZIP_LEVEL));
    if enc.write_all(&json).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let compressed = match enc.finish() {
        Ok(v) => v,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "application/json")
        .header("content-encoding", "gzip")
        .body(axum::body::Body::from(compressed))
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_authorization_returns_false() {
        assert!(!check_auth(None, "secret"));
    }

    #[test]
    fn missing_bearer_prefix_returns_false() {
        assert!(!check_auth(Some("secret"), "secret"));
        assert!(!check_auth(Some("Token secret"), "secret"));
    }

    #[test]
    fn length_mismatch_returns_false() {
        assert!(!check_auth(Some("Bearer short"), "longertoken"));
    }

    #[test]
    fn correct_token_returns_true() {
        assert!(check_auth(Some("Bearer mysecret"), "mysecret"));
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn serve(
    listen_addr: &str,
    flush_tx: mpsc::Sender<FlushSignal>,
    flush_token: String,
    stat_token: String,
    sos_token: String,
    rate_limit_per_hour: u32,
    response_timeout_secs: u64,
    body_size_limit_bytes: u64,
    envelope_size_limit_mb: u64,
    serialize_strategy: String,
    static_state_enabled: bool,
    host: String,
    host_id: String,
    boot_id: String,
    log_paths: Vec<String>,
    spool: Arc<Spool>,
    transport_cfg: TransportConfig,
) -> anyhow::Result<()> {
    // collection endpoints get 10× the flush rate (min 60/hour = 1/minute)
    let collection_rate_per_hour = (rate_limit_per_hour * 10).max(60);

    let state = Arc::new(InboundState {
        flush_tx,
        flush_token,
        flush_rate: Mutex::new(RateLimiter::new(rate_limit_per_hour)),
        flush_in_flight: Mutex::new(false),
        response_timeout_secs,
        serialize_strategy,
        stat_token,
        sos_token,
        collection_rate: Mutex::new(RateLimiter::new(collection_rate_per_hour)),
        envelope_size_limit_bytes: envelope_size_limit_mb * 1024 * 1024,
        host,
        host_id,
        boot_id,
        static_state_enabled,
        log_paths,
        drain_state: DrainState::default(),
        spool,
        transport_cfg,
    });

    let app = Router::new()
        .route("/flush", post(flush::handle_flush))
        .route("/stat", get(stat::handle_stat))
        .route("/trigger-sos", post(sos::handle_sos))
        .route("/drain-spool", post(drain::handle_drain_spool))
        .route("/drain-status", get(drain::handle_drain_status))
        .with_state(state)
        .layer(axum::extract::DefaultBodyLimit::max(body_size_limit_bytes as usize));

    let listener = tokio::net::TcpListener::bind(listen_addr).await?;
    info!(addr = listen_addr, "inbound 서버 시작 (/flush, /stat, /trigger-sos, /drain-spool, /drain-status)");
    axum::serve(listener, app).await?;
    Ok(())
}
