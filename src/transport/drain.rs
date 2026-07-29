//! retry/ 데드레터 drain 코어.
//!
//! HTTP `POST /drain-spool` 핸들러(inbound)와 coordinator의 수신 복구
//! 자동 drain이 공유하는 실행 엔진. in_progress 가드(AtomicBool CAS)
//! 하나를 두 경로가 공유하므로 동시에 drain은 항상 하나만 실행된다:
//! - HTTP drain 진행 중이면 자동 drain은 조용히 생략 (no-op)
//! - 자동 drain 진행 중이면 HTTP는 기존과 동일하게 409

use crate::config::TransportConfig;
use crate::transport;
use crate::transport::spool::Spool;
use chrono::{DateTime, Utc};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use ulid::Ulid;

// ── DrainState ────────────────────────────────────────────────────────────────

/// drain 작업의 진행 상태. main에서 Arc로 생성해
/// inbound(InboundState)와 coordinator(AutoDrainHandle)가 공유한다.
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

/// drain 시작 성공 결과 (HTTP 202 응답 본문·자동 drain 로그 구성용)
pub struct DrainStarted {
    pub drain_id: String,
    pub queued: usize,
    pub bytes: u64,
}

// ── drain 시작 (공용 진입점) ────────────────────────────────────────────────────

/// drain 시작 시도 — in_progress 가드 획득 실패(이미 진행 중) 시 `None`.
/// 성공 시 retry/의 [from, to) 창 파일 목록을 만들고 상태를 초기화한 뒤
/// 백그라운드 drain_task를 spawn한다. HTTP 핸들러와 자동 drain이 같은
/// 가드·상태를 공유하는 유일한 진입점.
pub async fn try_start(
    state: Arc<DrainState>,
    spool: Arc<Spool>,
    transport_cfg: TransportConfig,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    source: &'static str,
) -> Option<DrainStarted> {
    // 중복 drain 방지 — HTTP와 자동 drain이 같은 가드를 공유
    if state
        .in_progress
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return None;
    }

    // 대상 파일 목록 — drain_window는 fs::read_dir, metadata는 fs::metadata: spawn_blocking으로 executor 보호
    let sp_dw = Arc::clone(&spool);
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
    *state.drain_id.write().await = Some(drain_id.clone());
    *state.window_from.write().await = Some(from);
    *state.window_to.write().await = Some(to);
    state.queued.store(queued as u64, Ordering::SeqCst);
    state.remaining.store(queued as u64, Ordering::SeqCst);
    state.succeeded.store(0, Ordering::SeqCst);
    state.failed.store(0, Ordering::SeqCst);
    *state.started_at.write().await = Some(now);
    *state.completed_at.write().await = None;

    // Transport는 실제 파일이 있을 때만 생성 (백그라운드 태스크 내부에서 lazy 생성)
    let st2 = Arc::clone(&state);
    tokio::spawn(async move { drain_task(st2, spool, transport_cfg, files, source).await });

    Some(DrainStarted { drain_id, queued, bytes })
}

// ── 자동 drain (coordinator용 핸들) ─────────────────────────────────────────────

/// coordinator가 라이브 전송 성공(수신 복구 신호) 시 자동 drain을 트리거하는 핸들.
/// 기존에는 retry/에 파킹된 envelope이 수동 `POST /drain-spool` 없이는 TTL(168h)
/// 만료로 삭제될 수 있었다 — 전송 성공을 복구 신호로 삼아 전체 창을 자동 재전송한다.
#[derive(Clone)]
pub struct AutoDrainHandle {
    state: Arc<DrainState>,
    spool: Arc<Spool>,
    transport_cfg: TransportConfig,
}

impl AutoDrainHandle {
    pub fn new(state: Arc<DrainState>, spool: Arc<Spool>, transport_cfg: TransportConfig) -> Self {
        Self { state, spool, transport_cfg }
    }

    /// 공유 drain 상태 (inbound /drain-status와 동일 인스턴스)
    pub fn state(&self) -> &Arc<DrainState> {
        &self.state
    }

    /// 라이브 전송 성공 직후 호출 — 전송 성공 1회당 트리거 시도 1회.
    /// retry/ 비어 있으면 no-op (O(1) 카운터 선확인 — 태스크 spawn조차 없음).
    /// drain 실행은 백그라운드 태스크라 전송 태스크·coordinator 루프를 막지 않는다.
    pub fn trigger(&self) {
        if !self.transport_cfg.auto_drain {
            return; // transport.auto_drain=false — 운영자 비활성화
        }
        if self.spool.retry_count() == 0 {
            return; // 파킹된 envelope 없음
        }
        let h = self.clone();
        tokio::spawn(async move { h.run().await });
    }

    async fn run(self) {
        // 전체 창 drain — retry/의 모든 파일 대상 (HTTP drain과 동일 코어 재사용)
        let started = try_start(
            Arc::clone(&self.state),
            Arc::clone(&self.spool),
            self.transport_cfg.clone(),
            DateTime::<Utc>::MIN_UTC,
            DateTime::<Utc>::MAX_UTC,
            "auto",
        )
        .await;
        match started {
            // HTTP drain(또는 다른 자동 drain) 진행 중 — 가드 공유로 안전하게 생략
            None => debug!("drain 이미 진행 중 — retry/ 자동 drain 생략"),
            Some(s) if s.queued == 0 => {
                debug!(drain_id = %s.drain_id, "retry/ 자동 drain — 대상 파일 없음, 즉시 완료")
            }
            Some(s) => info!(drain_id = %s.drain_id,
                "수신 복구 감지 — retry/ 자동 drain 시작 ({} files)", s.queued),
        }
    }
}

// ── 백그라운드 drain 태스크 ─────────────────────────────────────────────────────

/// panic 또는 cancellation 시에도 in_progress를 false로 복원하는 RAII 가드
struct InProgressGuard<'a>(&'a AtomicBool);
impl Drop for InProgressGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

async fn drain_task(
    state: Arc<DrainState>,
    spool: Arc<Spool>,
    transport_cfg: TransportConfig,
    files: Vec<std::path::PathBuf>,
    source: &'static str,
) {
    let _guard = InProgressGuard(&state.in_progress);
    // Transport는 실제 전송할 파일이 있을 때만 생성
    let transport = if files.is_empty() {
        None
    } else {
        match transport::create(&transport_cfg) {
            Ok(t) => Some(t),
            Err(e) => {
                warn!("drain transport 생성 실패 — 전체 실패 처리: {e}");
                let n = files.len() as u64;
                state.failed.fetch_add(n, Ordering::SeqCst);
                state.remaining.store(0, Ordering::SeqCst);
                *state.completed_at.write().await = Some(Utc::now());
                return; // _guard가 in_progress = false 처리
            }
        }
    };

    // files 비어있으면 transport는 None — 루프 진입 전 추출해 unwrap 제거
    let transport = match transport {
        Some(t) => t,
        None => {
            *state.completed_at.write().await = Some(Utc::now());
            return; // files 없음, _guard가 in_progress = false 처리
        }
    };

    for path in &files {
        // spool.load/drain_commit은 std::fs — spawn_blocking으로 executor 스레드 보호
        let sp_load = Arc::clone(&spool);
        let path_owned = path.clone();
        let envelope = match tokio::task::spawn_blocking(move || sp_load.load(&path_owned))
            .await
            .unwrap_or_else(|e| Err(anyhow::anyhow!("drain load spawn_blocking 패닉: {e}")))
        {
            Ok(e) => e,
            Err(e) => {
                warn!(path = %path.display(), err = %e, "drain: envelope 로드 실패 — 스킵");
                state.failed.fetch_add(1, Ordering::SeqCst);
                state.remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1))).ok();
                continue;
            }
        };

        match transport.send(&envelope).await {
            Ok(()) => {
                let sp_commit = Arc::clone(&spool);
                let path_commit = path.clone();
                if let Err(e) = tokio::task::spawn_blocking(move || sp_commit.drain_commit(&path_commit)).await {
                    warn!(path = %path.display(), "drain_commit spawn_blocking 패닉: {e}");
                }
                state.succeeded.fetch_add(1, Ordering::SeqCst);
                info!(path = %path.display(), "drain: 전송 성공");
            }
            Err(e) => {
                warn!(path = %path.display(), err = %e, "drain: 전송 실패 — 파일 유지");
                state.failed.fetch_add(1, Ordering::SeqCst);
            }
        }
        state.remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1))).ok();
    }

    *state.completed_at.write().await = Some(Utc::now());
    // _guard가 함수 종료 시 in_progress = false 처리

    let succeeded = state.succeeded.load(Ordering::SeqCst);
    let failed = state.failed.load(Ordering::SeqCst);
    let drain_id = state.drain_id.read().await.clone().unwrap_or_default();
    info!(drain_id, succeeded, failed, source, "drain-spool 완료");
}

// ── 테스트 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn unique_temp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
        use std::time::{SystemTime, UNIX_EPOCH};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "log_parser_{tag}_{}_{seq}_{nanos}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn try_start_returns_none_when_guard_held() {
        let dir = unique_temp_dir("drain_core_guard");
        let spool = Arc::new(Spool::new(dir.to_str().unwrap(), 10).unwrap());
        let state = Arc::new(DrainState::default());
        // HTTP drain이 가드를 잡고 있는 상황 재현
        state.in_progress.store(true, Ordering::SeqCst);

        let started = try_start(
            Arc::clone(&state),
            Arc::clone(&spool),
            TransportConfig::default(),
            DateTime::<Utc>::MIN_UTC,
            DateTime::<Utc>::MAX_UTC,
            "auto",
        )
        .await;
        assert!(started.is_none(), "가드 점유 중에는 drain 시작 불가");
        assert!(state.in_progress.load(Ordering::SeqCst), "기존 가드를 건드리면 안 됨");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn try_start_empty_retry_completes_and_releases_guard() {
        let dir = unique_temp_dir("drain_core_empty");
        let spool = Arc::new(Spool::new(dir.to_str().unwrap(), 10).unwrap());
        let state = Arc::new(DrainState::default());

        let started = try_start(
            Arc::clone(&state),
            Arc::clone(&spool),
            TransportConfig::default(),
            DateTime::<Utc>::MIN_UTC,
            DateTime::<Utc>::MAX_UTC,
            "http",
        )
        .await
        .expect("빈 retry/도 drain 시작은 성공(즉시 완료)");
        assert_eq!(started.queued, 0);

        // background drain_task 완료 대기 — polling으로 flakiness 방지
        for _ in 0..100 {
            if !state.in_progress.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!state.in_progress.load(Ordering::SeqCst), "완료 후 가드 해제");
        assert!(state.completed_at.read().await.is_some(), "completed_at 기록");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
