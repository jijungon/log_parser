use crate::inbound::{check_auth, InboundState};
use crate::transport;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{atomic::Ordering, Arc};
use tracing::{info, warn};

// drain 코어(상태·가드·백그라운드 태스크)는 transport::drain으로 이동 —
// coordinator의 수신 복구 자동 drain과 in_progress 가드를 공유하기 위함.
// 기존 경로(crate::inbound::drain::DrainState) 호환을 위해 재노출한다.
pub use crate::transport::drain::DrainState;

// ── 요청/응답 타입 ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct DrainQuery {
    pub from: String,
    pub to: String,
}

#[derive(Serialize)]
struct WindowInfo {
    from: String,
    to: String,
}

#[derive(Serialize)]
struct DrainAccepted {
    drain_id: String,
    window: WindowInfo,
    queued: usize,
    bytes: u64,
}

#[derive(Serialize)]
struct DrainConflict {
    status: &'static str,
    drain_id: Option<String>,
    remaining: u64,
    started_at: Option<String>,
    window: Option<WindowInfo>,
}

#[derive(Serialize)]
struct DrainStatus {
    drain_id: Option<String>,
    status: &'static str,
    window: Option<WindowInfo>,
    queued: u64,
    remaining: u64,
    succeeded: u64,
    failed: u64,
    started_at: Option<String>,
    completed_at: Option<String>,
    spool_new_bytes: u64,
    spool_retry_count: usize,
}

// ── POST /drain-spool ─────────────────────────────────────────────────────────

/// `POST /drain-spool?from=<RFC3339>&to=<RFC3339>`
///
/// retry/ 의 지정 시간 창 내 파일들을 백그라운드에서 재전송.
/// - `202` + `drain_id` — drain 작업 시작
/// - `409` — 이미 drain 진행 중
/// - `401` — 인증 실패
/// - `400` — from/to 파라미터 파싱 실패
pub async fn handle_drain_spool(
    State(st): State<Arc<InboundState>>,
    headers: HeaderMap,
    Query(params): Query<DrainQuery>,
) -> Response {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !st.flush_token.is_empty() && !check_auth(auth, &st.flush_token) {
        warn!("drain-spool 인증 실패 — 401");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let from = match params.from.parse::<DateTime<Utc>>() {
        Ok(t) => t,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid 'from' parameter — expected RFC3339"})),
            )
                .into_response()
        }
    };
    let to = match params.to.parse::<DateTime<Utc>>() {
        Ok(t) => t,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid 'to' parameter — expected RFC3339"})),
            )
                .into_response()
        }
    };

    if from >= to {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "'from' must be before 'to'"})),
        )
            .into_response();
    }

    // drain 코어 시작 — in_progress 가드는 coordinator의 자동 drain과 공유 (transport::drain)
    let started = transport::drain::try_start(
        Arc::clone(&st.drain_state),
        Arc::clone(&st.spool),
        st.transport_cfg.clone(),
        from,
        to,
        "http",
    )
    .await;

    // 중복 drain 방지 — 가드 획득 실패 시 진행 중 drain 정보와 함께 409
    let Some(started) = started else {
        let drain_id = st.drain_state.drain_id.read().await.clone();
        let remaining = st.drain_state.remaining.load(Ordering::SeqCst);
        let started_at = st.drain_state.started_at.read().await.map(|t| t.to_rfc3339());
        let window_from = *st.drain_state.window_from.read().await;
        let window_to = *st.drain_state.window_to.read().await;
        let window = match (window_from, window_to) {
            (Some(f), Some(t)) => Some(WindowInfo { from: f.to_rfc3339(), to: t.to_rfc3339() }),
            _ => None,
        };
        warn!(drain_id = ?drain_id, "drain 이미 진행 중 — 409");
        return (
            StatusCode::CONFLICT,
            Json(DrainConflict { status: "in_progress", drain_id, remaining, started_at, window }),
        )
            .into_response();
    };

    info!(drain_id = %started.drain_id, queued = started.queued, "drain-spool 시작");

    (
        StatusCode::ACCEPTED,
        Json(DrainAccepted {
            drain_id: started.drain_id,
            window: WindowInfo { from: from.to_rfc3339(), to: to.to_rfc3339() },
            queued: started.queued,
            bytes: started.bytes,
        }),
    )
        .into_response()
}

// ── GET /drain-status ─────────────────────────────────────────────────────────

pub async fn handle_drain_status(
    State(st): State<Arc<InboundState>>,
    headers: HeaderMap,
) -> Response {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !st.flush_token.is_empty() && !check_auth(auth, &st.flush_token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let in_progress = st.drain_state.in_progress.load(Ordering::SeqCst);
    let drain_id = st.drain_state.drain_id.read().await.clone();
    let window_from = *st.drain_state.window_from.read().await;
    let window_to = *st.drain_state.window_to.read().await;
    let queued = st.drain_state.queued.load(Ordering::SeqCst);
    let remaining = st.drain_state.remaining.load(Ordering::SeqCst);
    let succeeded = st.drain_state.succeeded.load(Ordering::SeqCst);
    let failed = st.drain_state.failed.load(Ordering::SeqCst);
    let started_at = *st.drain_state.started_at.read().await;
    let completed_at = *st.drain_state.completed_at.read().await;

    let status: &'static str = if in_progress {
        "in_progress"
    } else if drain_id.is_some() {
        "completed"
    } else {
        "idle"
    };

    let window = match (window_from, window_to) {
        (Some(f), Some(t)) => Some(WindowInfo { from: f.to_rfc3339(), to: t.to_rfc3339() }),
        _ => None,
    };

    Json(DrainStatus {
        drain_id,
        status,
        window,
        queued,
        remaining,
        succeeded,
        failed,
        started_at: started_at.map(|t| t.to_rfc3339()),
        completed_at: completed_at.map(|t| t.to_rfc3339()),
        spool_new_bytes: st.spool.new_used_bytes(),
        spool_retry_count: st.spool.retry_count(),
    })
    .into_response()
}

// ── 테스트 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::FlushSignal;
    use crate::envelope::{Cycle, Envelope, Headers};
    use crate::inbound::flush::RateLimiter;
    use crate::transport::spool::Spool;
    use crate::config::TransportConfig;
    use axum::{body::Body, http::Request, routing::{get, post}, Router};
    use tower::ServiceExt as _;

    fn make_state() -> Arc<InboundState> {
        let dir = std::env::temp_dir().join(format!(
            "drain_test_{}_{}",
            std::process::id(),
            {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos()
            }
        ));
        let spool = Arc::new(Spool::new(dir.to_str().unwrap(), 10).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        Arc::new(InboundState {
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
            drain_state: Arc::new(DrainState::default()),
            spool,
            transport_cfg: TransportConfig::default(),
        })
    }

    fn app(state: Arc<InboundState>) -> Router {
        Router::new()
            .route("/drain-spool", post(handle_drain_spool))
            .route("/drain-status", get(handle_drain_status))
            .with_state(state)
    }

    #[tokio::test]
    async fn drain_status_idle_before_any_drain() {
        let state = make_state();
        let resp = app(state)
            .oneshot(Request::get("/drain-status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "idle");
        assert!(json["drain_id"].is_null());
        assert_eq!(json["queued"], 0);
        assert_eq!(json["spool_new_bytes"], 0);
        assert_eq!(json["spool_retry_count"], 0);
    }

    #[tokio::test]
    async fn drain_status_spool_fields_reflect_written_bytes() {
        let state = make_state();
        // Write something to spool so spool_new_bytes > 0
        let dummy = b"{\"test\":1}";
        state.spool.save_bytes(dummy).unwrap();
        let resp = app(state)
            .oneshot(Request::get("/drain-status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["spool_new_bytes"], dummy.len() as u64);
        assert_eq!(json["spool_retry_count"], 0);
    }

    #[tokio::test]
    async fn drain_status_spool_retry_count_reflects_move_to_retry() {
        let state = make_state();
        // save a file then move it to retry/ so retry_count becomes 1
        let path = state.spool.save_bytes(b"{\"test\":1}").unwrap();
        state.spool.move_to_retry(&path);
        let resp = app(state)
            .oneshot(Request::get("/drain-status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["spool_retry_count"], 1u64);
        assert_eq!(json["spool_new_bytes"], 0u64);
    }

    #[tokio::test]
    async fn drain_spool_400_on_bad_from_param() {
        let state = make_state();
        let resp = app(state)
            .oneshot(
                Request::post("/drain-spool?from=not-a-date&to=2026-01-01T00:30:00Z")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn drain_spool_400_on_bad_to_param() {
        let state = make_state();
        let resp = app(state)
            .oneshot(
                Request::post("/drain-spool?from=2026-01-01T00:00:00Z&to=not-a-date")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn drain_spool_400_on_inverted_window() {
        let state = make_state();
        // from > to — should be 400, not 202
        let resp = app(state)
            .oneshot(
                Request::post(
                    "/drain-spool?from=2026-01-01T01:00:00Z&to=2026-01-01T00:00:00Z",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("before"));
    }

    #[tokio::test]
    async fn drain_status_401_without_auth() {
        let dir = std::env::temp_dir().join(format!(
            "drain_status_auth_{}", std::process::id()
        ));
        let spool = Arc::new(Spool::new(dir.to_str().unwrap(), 10).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: "secret".to_string(),
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
            drain_state: Arc::new(DrainState::default()),
            spool,
            transport_cfg: TransportConfig::default(),
        });

        let resp = app(state)
            .oneshot(Request::get("/drain-status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drain_spool_409_when_in_progress() {
        let state = make_state();
        // Manually set in_progress
        state.drain_state.in_progress.store(true, Ordering::SeqCst);
        *state.drain_state.drain_id.write().await = Some("existing-drain".to_string());
        state.drain_state.remaining.store(5, Ordering::SeqCst);

        let resp = app(Arc::clone(&state))
            .oneshot(
                Request::post(
                    "/drain-spool?from=2026-01-01T00:00:00Z&to=2026-01-01T00:30:00Z",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "in_progress");
        assert_eq!(json["remaining"], 5);
        assert!(json.get("started_at").is_some(), "409 must include started_at field");
        assert!(json.get("window").is_some(), "409 must include window field");
    }

    #[tokio::test]
    async fn drain_spool_401_without_auth() {
        // State with non-empty token requires auth
        let dir = std::env::temp_dir().join(format!(
            "drain_auth_{}", std::process::id()
        ));
        let spool = Arc::new(Spool::new(dir.to_str().unwrap(), 10).unwrap());
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: "secret".to_string(),
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
            drain_state: Arc::new(DrainState::default()),
            spool,
            transport_cfg: TransportConfig::default(),
        });

        let resp = app(state)
            .oneshot(
                Request::post(
                    "/drain-spool?from=2026-01-01T00:00:00Z&to=2026-01-01T00:30:00Z",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn drain_spool_202_with_zero_queued_when_retry_empty() {
        let state = make_state();
        // retry/ is empty → queued=0, still 202
        let resp = app(Arc::clone(&state))
            .oneshot(
                Request::post(
                    "/drain-spool?from=2026-01-01T00:00:00Z&to=2026-01-01T00:30:00Z",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["queued"], 0);
        assert!(json["drain_id"].is_string());
    }

    #[tokio::test]
    async fn drain_status_shows_in_progress() {
        let state = make_state();
        // Simulate in-progress drain
        state.drain_state.in_progress.store(true, Ordering::SeqCst);
        *state.drain_state.drain_id.write().await = Some("drain-xyz".to_string());
        state.drain_state.queued.store(10, Ordering::SeqCst);
        state.drain_state.remaining.store(7, Ordering::SeqCst);
        state.drain_state.succeeded.store(3, Ordering::SeqCst);

        let resp = app(Arc::clone(&state))
            .oneshot(Request::get("/drain-status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "in_progress");
        assert_eq!(json["queued"], 10);
        assert_eq!(json["remaining"], 7);
        assert_eq!(json["succeeded"], 3);
    }

    #[tokio::test]
    async fn drain_clears_in_progress_on_completion() {
        let state = make_state();
        // Start a drain with no files — completes instantly
        let resp = app(Arc::clone(&state))
            .oneshot(
                Request::post(
                    "/drain-spool?from=2026-01-01T00:00:00Z&to=2026-01-01T00:30:00Z",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);

        // background task 완료 대기 — 고정 sleep 대신 polling으로 CI flakiness 방지
        for _ in 0..100 {
            if !state.drain_state.in_progress.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Status should show completed (not in_progress), drain_id set
        let resp2 = app(Arc::clone(&state))
            .oneshot(Request::get("/drain-status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = axum::body::to_bytes(resp2.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "completed", "drain with no files must complete");
        assert!(!state.drain_state.in_progress.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn drain_task_hot_path_send_retry_file_succeeds() {
        // spin up a local HTTP server that accepts any POST and responds 200
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = Router::new().route("/ingest", post(|| async { StatusCode::OK }));
        tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let endpoint = format!("http://{addr}/ingest");

        // unique env var — avoid parallel-test collision
        let token_env = format!(
            "DRAIN_INTEG_TOKEN_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        );
        std::env::set_var(&token_env, "test-token");

        // spool with a valid envelope file moved to retry/
        let dir = std::env::temp_dir().join(format!(
            "drain_integ_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        let spool = Arc::new(Spool::new(dir.to_str().unwrap(), 100).unwrap());

        let envelope = Envelope {
            event_kind: "log_batch".to_string(),
            cycle: Cycle {
                host: "h".to_string(),
                host_id: "hid".to_string(),
                boot_id: "bid".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
                window: None,
                seq: None,
            },
            headers: Headers {
                total_sections: 0,
                counts: None,
                process_health: None,
                duration_ms: None,
            },
            body: vec![],
        };
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let new_path = spool.save_bytes(&bytes).unwrap();
        spool.move_to_retry(&new_path);
        assert_eq!(spool.retry_count(), 1, "pre-condition: 1 file in retry/");

        let transport_cfg = TransportConfig {
            kind: "http_json".to_string(),
            endpoint,
            token_env: token_env.clone(),
            tls_enabled: false,
            connect_timeout_seconds: 5,
            request_timeout_seconds: 5,
            ..TransportConfig::default()
        };
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        let state = Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 5,
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
            drain_state: Arc::new(DrainState::default()),
            spool: Arc::clone(&spool),
            transport_cfg,
        });

        // trigger drain with a window that covers any ULID created in this century
        let resp = app(Arc::clone(&state))
            .oneshot(
                Request::post(
                    "/drain-spool?from=2000-01-01T00:00:00Z&to=2099-12-31T23:59:59Z",
                )
                .body(Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(json["queued"], 1, "1 retry file must be queued");

        // poll until drain_task completes (budget: 2s)
        for _ in 0..200 {
            if !state.drain_state.in_progress.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            !state.drain_state.in_progress.load(Ordering::SeqCst),
            "drain_task must complete within 2s"
        );
        assert_eq!(
            state.drain_state.succeeded.load(Ordering::SeqCst),
            1,
            "drain_task hot path: load → send → commit must succeed"
        );
        assert_eq!(state.drain_state.failed.load(Ordering::SeqCst), 0);

        std::env::remove_var(&token_env);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
