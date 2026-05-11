use crate::inbound::{check_auth, InboundState};
use crate::transport;
use anyhow;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::sync::RwLock;
use tracing::{info, warn};
use ulid::Ulid;

// ── DrainState ────────────────────────────────────────────────────────────────

/// drain 작업의 진행 상태. InboundState에 직접 포함 (Arc<InboundState>로 공유).
pub struct DrainState {
    pub in_progress: AtomicBool,
    pub drain_id: RwLock<Option<String>>,
    pub window_from: RwLock<Option<DateTime<Utc>>>,
    pub window_to: RwLock<Option<DateTime<Utc>>>,
    pub queued: AtomicU64,
    pub remaining: AtomicU64,
    pub succeeded: AtomicU64,
    pub failed: AtomicU64,
    pub started_at: RwLock<Option<DateTime<Utc>>>,
    pub completed_at: RwLock<Option<DateTime<Utc>>>,
}

impl Default for DrainState {
    fn default() -> Self {
        Self {
            in_progress: AtomicBool::new(false),
            drain_id: RwLock::new(None),
            window_from: RwLock::new(None),
            window_to: RwLock::new(None),
            queued: AtomicU64::new(0),
            remaining: AtomicU64::new(0),
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            started_at: RwLock::new(None),
            completed_at: RwLock::new(None),
        }
    }
}

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

    // 중복 drain 방지
    if st
        .drain_state
        .in_progress
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
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
    }

    // 대상 파일 목록 — drain_window는 fs::read_dir, metadata는 fs::metadata: spawn_blocking으로 executor 보호
    let sp_dw = Arc::clone(&st.spool);
    let files = tokio::task::spawn_blocking(move || {
        let files = sp_dw.drain_window(from, to);
        let bytes: u64 = files
            .iter()
            .filter_map(|p| std::fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();
        (files, bytes)
    })
    .await
    .unwrap_or_else(|e| {
        warn!("drain_window spawn_blocking 패닉: {e} — 빈 목록으로 처리");
        (vec![], 0)
    });
    let (files, bytes) = files;
    let queued = files.len();

    let drain_id = Ulid::new().to_string();
    let now = Utc::now();

    // 상태 초기화
    *st.drain_state.drain_id.write().await = Some(drain_id.clone());
    *st.drain_state.window_from.write().await = Some(from);
    *st.drain_state.window_to.write().await = Some(to);
    st.drain_state.queued.store(queued as u64, Ordering::SeqCst);
    st.drain_state.remaining.store(queued as u64, Ordering::SeqCst);
    st.drain_state.succeeded.store(0, Ordering::SeqCst);
    st.drain_state.failed.store(0, Ordering::SeqCst);
    *st.drain_state.started_at.write().await = Some(now);
    *st.drain_state.completed_at.write().await = None;

    info!(drain_id, queued, "drain-spool 시작");

    // Transport는 실제 파일이 있을 때만 생성 (백그라운드 태스크 내부에서 lazy 생성)
    let st2 = Arc::clone(&st);
    tokio::spawn(async move { drain_task(st2, files).await });

    (
        StatusCode::ACCEPTED,
        Json(DrainAccepted {
            drain_id,
            window: WindowInfo { from: from.to_rfc3339(), to: to.to_rfc3339() },
            queued,
            bytes,
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

// ── 백그라운드 drain 태스크 ─────────────────────────────────────────────────────

/// panic 또는 cancellation 시에도 in_progress를 false로 복원하는 RAII 가드
struct InProgressGuard<'a>(&'a AtomicBool);
impl Drop for InProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn drain_task(st: Arc<InboundState>, files: Vec<std::path::PathBuf>) {
    let _guard = InProgressGuard(&st.drain_state.in_progress);
    // Transport는 실제 전송할 파일이 있을 때만 생성
    let transport = if files.is_empty() {
        None
    } else {
        match transport::create(&st.transport_cfg) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("drain transport 생성 실패 — 전체 실패 처리: {e}");
                let n = files.len() as u64;
                st.drain_state.failed.fetch_add(n, Ordering::SeqCst);
                st.drain_state.remaining.store(0, Ordering::SeqCst);
                *st.drain_state.completed_at.write().await = Some(Utc::now());
                return; // _guard가 in_progress = false 처리
            }
        }
    };

    // files 비어있으면 transport는 None — 루프 진입 전 추출해 unwrap 제거
    let transport = match transport {
        Some(t) => t,
        None => {
            *st.drain_state.completed_at.write().await = Some(Utc::now());
            return; // files 없음, _guard가 in_progress = false 처리
        }
    };

    for path in &files {
        // spool.load/drain_commit은 std::fs — spawn_blocking으로 executor 스레드 보호
        let sp_load = Arc::clone(&st.spool);
        let path_owned = path.clone();
        let envelope = match tokio::task::spawn_blocking(move || sp_load.load(&path_owned))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("drain load spawn_blocking 패닉: {e}")))
        {
            Ok(e) => e,
            Err(e) => {
                warn!(path = %path.display(), err = %e, "drain: envelope 로드 실패 — 스킵");
                st.drain_state.failed.fetch_add(1, Ordering::SeqCst);
                st.drain_state.remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1))).ok();
                continue;
            }
        };

        match transport.send(&envelope).await {
            Ok(()) => {
                let sp_commit = Arc::clone(&st.spool);
                let path_commit = path.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || sp_commit.drain_commit(&path_commit)).await {
                    warn!(path = %path.display(), "drain_commit spawn_blocking 패닉: {e}");
                }
                st.drain_state.succeeded.fetch_add(1, Ordering::SeqCst);
                info!(path = %path.display(), "drain: 전송 성공");
            }
            Err(e) => {
                warn!(path = %path.display(), err = %e, "drain: 전송 실패 — 파일 유지");
                st.drain_state.failed.fetch_add(1, Ordering::SeqCst);
            }
        }
        st.drain_state.remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1))).ok();
    }

    *st.drain_state.completed_at.write().await = Some(Utc::now());
    // _guard가 함수 종료 시 in_progress = false 처리

    let succeeded = st.drain_state.succeeded.load(Ordering::SeqCst);
    let failed = st.drain_state.failed.load(Ordering::SeqCst);
    let drain_id = st.drain_state.drain_id.read().await.clone().unwrap_or_default();
    info!(drain_id, succeeded, failed, "drain-spool 완료");
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
            drain_state: DrainState::default(),
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
            drain_state: DrainState::default(),
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
            drain_state: DrainState::default(),
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
            drain_state: DrainState::default(),
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
