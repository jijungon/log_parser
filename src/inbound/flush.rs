use crate::coordinator::FlushSignal;
use super::{check_auth, check_envelope_size, gzip_envelope, InboundState};
use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use std::sync::Arc;
use std::time::Instant;
use tracing::{info, warn};

pub struct RateLimiter {
    tokens: f64,
    max: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(per_hour: u32) -> Self {
        let max = per_hour as f64;
        Self { tokens: max, max, refill_per_sec: max / 3600.0, last: Instant::now() }
    }

    pub fn try_consume(&mut self) -> bool {
        let secs = self.last.elapsed().as_secs_f64();
        self.last = Instant::now();
        self.tokens = (self.tokens + secs * self.refill_per_sec).min(self.max);
        if self.tokens >= 1.0 { self.tokens -= 1.0; true } else { false }
    }

    pub fn retry_after_secs(&self) -> u64 {
        ((1.0 - self.tokens) / self.refill_per_sec).ceil() as u64
    }
}

pub async fn handle_flush(State(st): State<Arc<InboundState>>, headers: HeaderMap) -> Response {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !st.flush_token.is_empty() && !check_auth(auth, &st.flush_token) {
        warn!("인증 실패 — 401");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();

    // rate-limit
    {
        let mut rl = st.flush_rate.lock().await;
        if !rl.try_consume() {
            let retry = rl.retry_after_secs();
            warn!(retry_after = retry, "rate-limit 초과");
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry.to_string())],
            )
                .into_response();
        }
    }

    // serialize: 진행 중 처리 (strategy에 따라 거절 또는 대기)
    {
        let mut inflight = st.flush_in_flight.lock().await;
        if *inflight {
            if st.serialize_strategy.as_str() == "reject" {
                warn!("flush 이미 처리 중 (reject) — 409");
                return (StatusCode::CONFLICT, [(header::RETRY_AFTER, "5")]).into_response();
            }
            // "wait" mode: release lock and poll; atomically claim slot when clear
            drop(inflight);
            let start = tokio::time::Instant::now();
            let timeout = std::time::Duration::from_secs(st.response_timeout_secs);
            loop {
                if start.elapsed() >= timeout {
                    warn!("flush 대기 시간 초과 — 503");
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let mut guard = st.flush_in_flight.lock().await;
                if !*guard {
                    *guard = true; // claim slot atomically while holding lock
                    break;
                }
            }
        } else {
            *inflight = true;
        }
    }

    // coordinator에 FlushSignal 전달
    if st.flush_tx.send(FlushSignal { reply: reply_tx }).await.is_err() {
        *st.flush_in_flight.lock().await = false;
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    // 응답 timeout
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(st.response_timeout_secs),
        reply_rx,
    )
    .await;

    *st.flush_in_flight.lock().await = false;

    let envelope = match result {
        Ok(Ok(env)) => env,
        _ => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };

    if let Some(resp) = check_envelope_size(&envelope, st.envelope_size_limit_bytes) {
        return resp;
    }

    info!(
        seq = envelope.cycle.seq,
        "/flush 응답"
    );

    gzip_envelope(&envelope)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::FlushSignal;
    use axum::{body::Body, http::Request, routing::post, Router};
    use std::sync::Arc;
    use tower::ServiceExt as _;

    fn test_spool() -> Arc<crate::transport::spool::Spool> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let dir = std::env::temp_dir()
            .join(format!("spool_flush_{}_{}", std::process::id(), n));
        Arc::new(crate::transport::spool::Spool::new(dir.to_str().unwrap(), 10).unwrap())
    }

    fn make_state(token: &str, rate_per_hour: u32) -> Arc<InboundState> {
        make_state_with_strategy(token, rate_per_hour, "reject")
    }

    fn make_state_with_strategy(token: &str, rate_per_hour: u32, strategy: &str) -> Arc<InboundState> {
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        Arc::new(InboundState {
            flush_tx: tx,
            flush_token: token.to_string(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(rate_per_hour)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 1,
            serialize_strategy: strategy.to_string(),
            stat_token: String::new(),
            sos_token: String::new(),
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
        Router::new().route("/flush", post(handle_flush)).with_state(state)
    }

    fn bearer(token: &str) -> (&'static str, String) {
        ("Authorization", format!("Bearer {token}"))
    }

    // ── RateLimiter unit tests ───────────────────────────────────────────────

    #[test]
    fn rate_limiter_full_bucket_on_creation() {
        let mut rl = RateLimiter::new(5);
        for _ in 0..5 {
            assert!(rl.try_consume());
        }
        assert!(!rl.try_consume());
    }

    #[test]
    fn rate_limiter_retry_after_positive_when_empty() {
        let mut rl = RateLimiter::new(1);
        rl.try_consume();
        assert!(rl.retry_after_secs() > 0);
    }

    // ── handle_flush handler tests ───────────────────────────────────────────

    #[tokio::test]
    async fn unauthorized_returns_401() {
        let state = make_state("secret", 100);
        let resp = app(state)
            .oneshot(Request::post("/flush").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let state = make_state("secret", 100);
        let (k, v) = bearer("wrong");
        let resp = app(state)
            .oneshot(Request::post("/flush").header(k, v).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rate_limited_returns_429() {
        let state = make_state("tok", 100);
        // drain bucket
        { let mut rl = state.flush_rate.lock().await; while rl.try_consume() {} }

        let (k, v) = bearer("tok");
        let resp = app(state)
            .oneshot(Request::post("/flush").header(k, v).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn in_flight_returns_409() {
        let state = make_state("tok", 100);
        *state.flush_in_flight.lock().await = true;

        let (k, v) = bearer("tok");
        let resp = app(Arc::clone(&state))
            .oneshot(Request::post("/flush").header(k, v).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_strategy_in_flight_returns_503_on_timeout() {
        // "wait" mode: in-flight flush never clears → times out → 503
        let state = make_state_with_strategy("", 100, "wait");
        *state.flush_in_flight.lock().await = true;
        let resp = app(Arc::clone(&state))
            .oneshot(Request::post("/flush").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }

    fn make_test_envelope() -> crate::envelope::Envelope {
        use crate::envelope::{Cycle, Headers};
        crate::envelope::Envelope {
            event_kind: "log_batch".to_string(),
            cycle: Cycle {
                host: "h".to_string(),
                host_id: "hid".to_string(),
                boot_id: "bid".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
                window: None,
                seq: Some(1),
            },
            headers: Headers {
                total_sections: 0,
                counts: None,
                process_health: None,
                duration_ms: Some(0),
            },
            body: vec![],
        }
    }

    #[tokio::test(start_paused = true)]
    async fn wait_strategy_proceeds_when_in_flight_clears() {
        // "wait" mode success path: in-flight clears at 10ms virtual, wait loop claims at 50ms.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 5,
            serialize_strategy: "wait".to_string(),
            stat_token: String::new(),
            sos_token: String::new(),
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
        });
        *state.flush_in_flight.lock().await = true;

        let state2 = Arc::clone(&state);
        tokio::spawn(async move {
            // Clear in-flight before the 50ms poll cycle, then reply to the FlushSignal.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            *state2.flush_in_flight.lock().await = false;
            if let Some(signal) = rx.recv().await {
                let _ = signal.reply.send(make_test_envelope());
            }
        });

        let resp = app(state)
            .oneshot(Request::post("/flush").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn flush_envelope_too_large_returns_413() {
        // envelope_size_limit_bytes=1: any valid envelope JSON will exceed it.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
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
            static_state_enabled: true,
            log_paths: vec![],
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        });

        // Background coordinator: receive FlushSignal and reply with an envelope.
        tokio::spawn(async move {
            if let Some(signal) = rx.recv().await {
                let _ = signal.reply.send(make_test_envelope());
            }
        });

        let resp = app(state)
            .oneshot(Request::post("/flush").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn closed_channel_returns_503() {
        let (tx, rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        drop(rx);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: "tok".to_string(),
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
            static_state_enabled: true,
            log_paths: vec![],
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        });
        let (k, v) = bearer("tok");
        let resp = app(state)
            .oneshot(Request::post("/flush").header(k, v).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
}
