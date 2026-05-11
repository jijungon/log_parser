//! POST /trigger-sos handler — collects sos_snapshot (stat sections + logs).

use crate::envelope::{Cycle, Envelope, Headers, Section};
use super::{check_auth, check_envelope_size, gzip_envelope, collect, InboundState};
use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

pub async fn handle_sos(State(st): State<Arc<InboundState>>, headers: HeaderMap, _body: Bytes) -> Response {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !st.sos_token.is_empty() && !check_auth(auth, &st.sos_token) {
        warn!("/trigger-sos 인증 실패 — 401");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    {
        let mut rl = st.collection_rate.lock().await;
        if !rl.try_consume() {
            let retry = rl.retry_after_secs();
            warn!(retry_after = retry, "/trigger-sos rate-limit 초과");
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, retry.to_string())]).into_response();
        }
    }

    let start = Instant::now();

    // Collect stat sections and logs concurrently
    let log_paths = st.log_paths.clone();
    let (metrics, processes, network, systemd, static_state, config, hardware, logs) = tokio::join!(
        collect::collect_metrics(),
        collect::collect_processes(),
        collect::collect_network(),
        collect::collect_systemd(),
        async {
            if st.static_state_enabled { collect::collect_static_state().await }
            else { serde_json::Value::Null }
        },
        collect::collect_config(),
        collect::collect_hardware(),
        collect::collect_logs(&log_paths),
    );

    let duration_ms = start.elapsed().as_millis() as u64;

    let mut body = vec![
        Section { section: "metrics".to_string(),   data: metrics },
        Section { section: "processes".to_string(), data: processes },
        Section { section: "network".to_string(),   data: network },
        Section { section: "systemd".to_string(),   data: systemd },
    ];
    if st.static_state_enabled {
        body.push(Section { section: "static_state".to_string(), data: static_state });
    }
    body.extend([
        Section { section: "config".to_string(),   data: config },
        Section { section: "hardware".to_string(), data: hardware },
        Section { section: "logs".to_string(),     data: logs },
    ]);

    let envelope = Envelope {
        event_kind: "sos_snapshot".to_string(),
        cycle: Cycle {
            host:    st.host.clone(),
            host_id: st.host_id.clone(),
            boot_id: st.boot_id.clone(),
            ts:      chrono::Utc::now().to_rfc3339(),
            window:  None,
            seq:     None,
        },
        headers: Headers {
            total_sections: body.len(),
            counts:         None,
            process_health: None,
            duration_ms:    Some(duration_ms),
        },
        body,
    };

    if let Some(resp) = check_envelope_size(&envelope, st.envelope_size_limit_bytes) {
        return resp;
    }

    info!(duration_ms, "/trigger-sos 응답");
    gzip_envelope(&envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::FlushSignal;
    use crate::inbound::{flush::RateLimiter, InboundState};
    use axum::{body::Body, http::Request, routing::post, Router};
    use flate2::read::GzDecoder;
    use std::io::Read as _;
    use std::sync::Arc;
    use tower::ServiceExt as _;

    fn test_spool() -> Arc<crate::transport::spool::Spool> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let dir = std::env::temp_dir()
            .join(format!("spool_sos_{}_{}", std::process::id(), n));
        Arc::new(crate::transport::spool::Spool::new(dir.to_str().unwrap(), 10).unwrap())
    }

    fn make_state(token: &str) -> Arc<InboundState> {
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 1,
            serialize_strategy: "reject".to_string(),
            stat_token: String::new(),
            sos_token: token.to_string(),
            collection_rate: tokio::sync::Mutex::new(RateLimiter::new(600)),
            envelope_size_limit_bytes: 0,
            host: "h".to_string(),
            host_id: "hid".to_string(),
            boot_id: "bid".to_string(),
            static_state_enabled: true,
            log_paths: vec![],
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        })
    }

    fn app(state: Arc<InboundState>) -> Router {
        Router::new().route("/trigger-sos", post(handle_sos)).with_state(state)
    }

    fn app_with_body_limit(state: Arc<InboundState>, limit_bytes: usize) -> Router {
        Router::new()
            .route("/trigger-sos", post(handle_sos))
            .with_state(state)
            .layer(axum::extract::DefaultBodyLimit::max(limit_bytes))
    }

    fn bearer(token: &str) -> (&'static str, String) {
        ("Authorization", format!("Bearer {token}"))
    }

    #[tokio::test]
    async fn unauthorized_returns_401() {
        let state = make_state("secret");
        let resp = app(state)
            .oneshot(Request::post("/trigger-sos").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn empty_token_bypasses_auth() {
        // When sos_token is empty the auth guard is skipped entirely.
        let state = make_state("");
        let resp = app(state)
            .oneshot(Request::post("/trigger-sos").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn valid_token_returns_200() {
        let state = make_state("tok");
        let (k, v) = bearer("tok");
        let resp = app(state)
            .oneshot(Request::post("/trigger-sos").header(k, v).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn exhausted_rate_limit_returns_429() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        // RateLimiter::new(0) → 0 tokens, every try_consume() returns false immediately.
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 1,
            serialize_strategy: "reject".to_string(),
            stat_token: String::new(),
            sos_token: String::new(),
            collection_rate: tokio::sync::Mutex::new(RateLimiter::new(0)),
            envelope_size_limit_bytes: 0,
            host: "h".to_string(),
            host_id: "hid".to_string(),
            boot_id: "bid".to_string(),
            static_state_enabled: true,
            log_paths: vec![],
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        });
        let resp = app(state)
            .oneshot(Request::post("/trigger-sos").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn oversized_body_returns_413() {
        let state = make_state(""); // empty token = no auth guard
        let large_body = vec![0u8; 100];
        let resp = app_with_body_limit(state, 5) // 5-byte limit
            .oneshot(
                Request::post("/trigger-sos")
                    .header("content-type", "application/json")
                    .body(Body::from(large_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn disabled_static_state_omits_section() {
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 1,
            serialize_strategy: "reject".to_string(),
            stat_token: String::new(),
            sos_token: String::new(),
            collection_rate: tokio::sync::Mutex::new(RateLimiter::new(600)),
            envelope_size_limit_bytes: 0,
            host: "h".to_string(),
            host_id: "hid".to_string(),
            boot_id: "bid".to_string(),
            static_state_enabled: false,
            log_paths: vec![],
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        });
        let resp = app(state)
            .oneshot(Request::post("/trigger-sos").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let mut decoder = GzDecoder::new(bytes.as_ref());
        let mut json_str = String::new();
        decoder.read_to_string(&mut json_str).unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let body = envelope["body"].as_array().unwrap();
        let has_static = body.iter().any(|s| s["section"].as_str() == Some("static_state"));
        assert!(!has_static, "static_state section must be absent when disabled");
    }

    #[tokio::test]
    async fn envelope_too_large_returns_413() {
        // 1-byte limit: any valid envelope JSON will exceed it.
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 1,
            serialize_strategy: "reject".to_string(),
            stat_token: String::new(),
            sos_token: String::new(),
            collection_rate: tokio::sync::Mutex::new(RateLimiter::new(600)),
            envelope_size_limit_bytes: 1,
            host: "h".to_string(),
            host_id: "hid".to_string(),
            boot_id: "bid".to_string(),
            static_state_enabled: false,
            log_paths: vec![],
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        });
        let resp = app(state)
            .oneshot(Request::post("/trigger-sos").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }
}
