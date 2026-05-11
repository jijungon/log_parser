pub mod cycle;

use crate::config::{CycleConfig, DedupConfig, TransportConfig};
use crate::dedup::window::DedupWindow;
use crate::envelope::Envelope;
use crate::normalize::categories::CategoryMatcher;
use crate::pipeline::raw_event::RawLogEvent;
use crate::pipeline::vector_spawn::VectorHandle;
use crate::transport::TransportError;
use crate::transport::http::HttpJsonTransport;
use crate::transport::spool::Spool;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, oneshot};
use tokio::time::interval;
use tracing::{error, info, warn};
use xxhash_rust::xxh3::Xxh3;

/// /flush 핸들러 → Coordinator 채널
pub struct FlushSignal {
    pub reply: oneshot::Sender<Envelope>,
}

pub async fn run_pipeline(
    mut rx: mpsc::Receiver<RawLogEvent>,
    mut flush_rx: mpsc::Receiver<FlushSignal>,
    dedup_cfg: DedupConfig,
    cycle_cfg: CycleConfig,
    transport_cfg: TransportConfig,
    categories: CategoryMatcher,
    host: String,
    host_id: String,
    boot_id: String,
    vector_handle: Arc<VectorHandle>,
    max_events_per_cycle: usize,
    body_max_bytes: u64,
    initial_seq: u64,
    seq_state_path: String,
    spool: Arc<Spool>,
    transport: Arc<HttpJsonTransport>,
) -> Result<()> {
    let t = transport;
    let agent_start = SystemTime::now();

    // 시작 시 미처리 spool 재전송 (최대 4개 동시)
    spool.log_pending();
    let replay_sem = Arc::new(tokio::sync::Semaphore::new(4));
    for path in spool.pending() {
        let envelope = match spool.load(&path) {
            Ok(e) => e,
            Err(e) => {
                warn!(path = %path.display(), err = %e, "spool 로드 실패 — 스킵");
                continue;
            }
        };
        let has_critical = envelope_has_critical(&envelope);
        let t2 = Arc::clone(&t);
        let sp2 = Arc::clone(&spool);
        let max = transport_cfg.retry_max_normal;
        let permit = Arc::clone(&replay_sem).acquire_owned().await.unwrap();
        let retry_base = transport_cfg.retry_base_seconds;
        let retry_max  = transport_cfg.retry_max_seconds;
        tokio::spawn(async move {
            let _permit = permit;
            send_with_backoff(t2, envelope, path, max, has_critical, sp2, retry_base, retry_max).await;
        });
    }

    let mut dedup = DedupWindow::new(dedup_cfg.window_seconds, dedup_cfg.lru_cap);
    let mut cycle = cycle::CycleState::new(
        initial_seq,
        host.clone(),
        host_id.clone(),
        boot_id.clone(),
        max_events_per_cycle,
    );

    let mut dedup_tick = interval(Duration::from_secs(5));
    let mut cycle_tick = interval(Duration::from_secs(cycle_cfg.window_seconds));
    cycle_tick.tick().await; // 첫 틱 즉시 소비

    loop {
        tokio::select! {
            biased;

            // 이벤트 수신 → normalize → dedup (None = 채널 종료 → 루프 탈출)
            ev = rx.recv() => {
                let ev = match ev {
                    Some(ev) => ev,
                    None => {
                        info!("이벤트 채널 종료 — 남은 dedup 항목 방출 후 종료");
                        for ev in dedup.flush_all() { cycle.push(ev); }
                        return Ok(());
                    }
                };
                let raw = ev.raw_message().to_string();
                if raw.is_empty() { continue; }

                let (template, mut fields) = {
                    let msg = crate::normalize::tokens::strip_syslog_prefix(&raw);
                    (
                        crate::normalize::tokens::normalize(msg),
                        crate::normalize::fields::extract_fields(msg),
                    )
                };
                // Enrich with journald/file structured fields from Vector (message-extracted wins)
                if let Some(pid) = &ev.pid {
                    fields.entry("pid".to_string())
                        .or_insert_with(|| serde_json::Value::String(pid.clone()));
                }
                if let Some(unit) = &ev.unit {
                    fields.entry("unit".to_string())
                        .or_insert_with(|| serde_json::Value::String(unit.clone()));
                }
                if let Some(priority) = &ev.priority {
                    fields.entry("priority".to_string())
                        .or_insert_with(|| serde_json::Value::String(priority.clone()));
                }
                if let Some(fpath) = &ev.file_path {
                    fields.entry("file_path".to_string())
                        .or_insert_with(|| serde_json::Value::String(fpath.clone()));
                }
                if !ev.host.is_empty() {
                    fields.entry("source_host".to_string())
                        .or_insert_with(|| serde_json::Value::String(ev.host.clone()));
                }
                let severity  = crate::normalize::severity::finalize(&ev.log_parser_severity, &raw);
                let category  = categories.categorize(&raw);

                let fp = {
                    use std::hash::Hasher as _;
                    let mut h = Xxh3::new();
                    h.write(template.as_bytes());
                    h.write(b"|");
                    h.write(severity.as_bytes());
                    h.write(b"|");
                    h.write(ev.log_parser_source.as_bytes());
                    h.finish()
                };

                if let Some(ev) = dedup.push(fp, ev.log_parser_source, severity.to_string(),
                    category.to_string(), template, raw, ev.timestamp, fields) {
                    cycle.push(ev);
                }
            }

            // flush 요청 → cycle 즉시 finalize → oneshot 응답 (transport 미사용)
            Some(signal) = flush_rx.recv() => {
                for ev in dedup.flush_all() { cycle.push(ev); }

                let restarts = vector_handle.restarts_24h.load(Ordering::Relaxed);
                let uptime   = uptime_secs(&agent_start);
                let (envelope, next_seq) = cycle.finalize(restarts, uptime);

                info!(seq = envelope.cycle.seq, sections = envelope.headers.total_sections, "/flush 응답");
                persist_seq(&seq_state_path, next_seq).await;
                if signal.reply.send(envelope).is_err() {
                    warn!("flush 응답 채널 닫힘 — envelope 폐기");
                }

                cycle = cycle::CycleState::new(next_seq, host.clone(), host_id.clone(),
                    boot_id.clone(), max_events_per_cycle);
            }

            // 5초마다 만료된 dedup 항목 방출
            _ = dedup_tick.tick() => {
                for ev in dedup.flush_expired() { cycle.push(ev); }
            }

            // cycle 타이머 → finalize → spool WAL → transport push
            _ = cycle_tick.tick() => {
                for ev in dedup.flush_all() { cycle.push(ev); }

                let restarts = vector_handle.restarts_24h.load(Ordering::Relaxed);
                let uptime   = uptime_secs(&agent_start);
                let (envelope, next_seq) = cycle.finalize(restarts, uptime);

                info!(seq = envelope.cycle.seq, sections = envelope.headers.total_sections,
                    "cycle envelope 조립 → transport 전송");
                persist_seq(&seq_state_path, next_seq).await;

                // Body size guard — warn operator if envelope exceeds configured limit.
                if body_max_bytes > 0 {
                    match serde_json::to_vec(&envelope) {
                        Ok(v) if v.len() as u64 > body_max_bytes => {
                            warn!(
                                json_bytes = v.len(),
                                limit_bytes = body_max_bytes,
                                "cycle envelope이 body_max_size_mb 설정 초과"
                            );
                        }
                        Err(e) => warn!("cycle envelope 직렬화 실패 (size check 불가): {e}"),
                        _ => {}
                    }
                }

                // WAL: 전송 전에 spool에 기록
                let has_critical = envelope_has_critical(&envelope);
                let max = transport_cfg.retry_max_normal;
                match spool.save(&envelope) {
                    Ok(path) => {
                        let t2  = Arc::clone(&t);
                        let sp2 = Arc::clone(&spool);
                        let retry_base = transport_cfg.retry_base_seconds;
                        let retry_max  = transport_cfg.retry_max_seconds;
                        tokio::spawn(async move {
                            send_with_backoff(t2, envelope, path, max, has_critical, sp2, retry_base, retry_max).await;
                        });
                    }
                    Err(e) => {
                        // spool 공간 부족 등 — WAL 없이 백오프 재시도. commit은 no-op.
                        warn!("spool 저장 실패 ({e}) — 백오프 재시도 진행");
                        let t2  = Arc::clone(&t);
                        let sp2 = Arc::clone(&spool);
                        let retry_base = transport_cfg.retry_base_seconds;
                        let retry_max  = transport_cfg.retry_max_seconds;
                        tokio::spawn(async move {
                            send_with_backoff(t2, envelope, PathBuf::new(), max, has_critical, sp2, retry_base, retry_max).await;
                        });
                    }
                }

                cycle = cycle::CycleState::new(next_seq, host.clone(), host_id.clone(),
                    boot_id.clone(), max_events_per_cycle);
            }
        }
    }
}

// ── 유틸리티 ──────────────────────────────────────────────────────────────────

async fn persist_seq(path: &str, seq: u64) {
    if !path.is_empty() {
        if let Err(e) = tokio::fs::write(path, seq.to_string()).await {
            warn!(path, "seq state 저장 실패: {e}");
        }
    }
}

fn uptime_secs(start: &SystemTime) -> u64 {
    SystemTime::now().duration_since(*start).unwrap_or_default().as_secs()
}

fn envelope_has_critical(env: &Envelope) -> bool {
    env.headers
        .counts
        .as_ref()
        .map(|c| c.by_severity.critical > 0)
        .unwrap_or(false)
}

/// 지수 백오프 재전송. 성공 시 spool 삭제.
/// critical envelope은 성공 또는 Fatal까지 무한 재시도 (원칙 #1).
/// non-critical은 최초 전송 포함 max_retries+1번 전송 후 포기 (spool 보존).
/// (max_retries=N → 1회 초기 전송 + N회 재시도 = 최대 N+1회)
async fn send_with_backoff(
    t: Arc<HttpJsonTransport>,
    envelope: Envelope,
    spool_path: PathBuf,
    max_retries: u32,
    has_critical: bool,
    spool: Arc<Spool>,
    retry_base: u64,
    retry_max: u64,
) {
    let mut backoff = retry_base;
    let mut attempt: u64 = 0;

    loop {
        match t.send(&envelope).await {
            Ok(()) => {
                spool.commit(&spool_path);
                if attempt > 0 {
                    info!(attempts = attempt + 1, "재시도 후 전송 성공");
                } else {
                    info!("envelope 전송 성공");
                }
                return;
            }
            Err(TransportError::Fatal(msg)) => {
                error!("치명 오류 — retry/로 이동: {msg}");
                spool.move_to_retry(&spool_path);
                return;
            }
            Err(TransportError::RateLimited { retry_after }) => {
                warn!(attempt, retry_after, "rate-limit — 대기 후 재시도");
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
            }
            Err(TransportError::Retryable(msg)) => {
                if !has_critical && attempt >= max_retries as u64 {
                    warn!(attempts = attempt + 1, "재시도 한도 소진 — retry/로 이동: {msg}");
                    spool.move_to_retry(&spool_path);
                    return;
                }
                warn!(attempt, wait = backoff, "재시도 가능 오류: {msg}");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(retry_max);
            }
        }
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::StatusCode as AxumStatus, routing::post, Router};
    use std::collections::VecDeque;
    use std::sync::Arc as StdArc;
    use tokio::sync::Mutex as TokioMutex;

    /// In-process axum server that returns status codes from a queue.
    async fn start_response_server(responses: Vec<u16>) -> String {
        let queue = StdArc::new(TokioMutex::new(VecDeque::from(responses)));
        let app = Router::new().route(
            "/ingest",
            post({
                let queue = StdArc::clone(&queue);
                move || {
                    let queue = StdArc::clone(&queue);
                    async move {
                        let mut q = queue.lock().await;
                        let code = q.pop_front().unwrap_or(200);
                        AxumStatus::from_u16(code).unwrap()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/ingest")
    }

    fn make_transport(url: &str) -> Arc<HttpJsonTransport> {
        Arc::new(HttpJsonTransport::test_new(url, "test"))
    }

    fn make_spool() -> Arc<Spool> {
        let dir =
            std::env::temp_dir().join(format!("spool_backoff_{}_{}", std::process::id(), rand_u64()));
        Arc::new(Spool::new(dir.to_str().unwrap(), 10).unwrap())
    }

    fn rand_u64() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64
    }

    fn test_envelope() -> Envelope {
        use crate::envelope::{Cycle, Headers};
        Envelope {
            event_kind: "log_batch".to_string(),
            cycle: Cycle {
                host: "test".to_string(),
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
    async fn fatal_moves_to_retry() {
        let url = start_response_server(vec![403]).await;
        let spool = make_spool();
        let path = spool.save(&test_envelope()).unwrap();
        send_with_backoff(
            make_transport(&url), test_envelope(), path.clone(),
            5, false, Arc::clone(&spool), 5, 300,
        ).await;
        // Fatal → moved to retry/ (not left in new/)
        assert!(!path.exists(), "new/ file should be moved out on Fatal");
        let retry_path = path.parent().unwrap().parent().unwrap()
            .join("retry").join(path.file_name().unwrap());
        assert!(retry_path.exists(), "file should be in retry/ after Fatal");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn success_commits_spool() {
        let url = start_response_server(vec![200]).await;
        let spool = make_spool();
        let path = spool.save(&test_envelope()).unwrap();
        send_with_backoff(
            make_transport(&url), test_envelope(), path.clone(),
            5, false, Arc::clone(&spool), 5, 300,
        ).await;
        assert!(!path.exists(), "spool file should be deleted on success");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn non_critical_exhausts_after_max_retries() {
        // 10 × 500 ensures we never succeed; max_retries=3 → 4 sends (1 initial + 3 retries)
        let url = start_response_server(vec![500; 10]).await;
        let spool = make_spool();
        let path = spool.save(&test_envelope()).unwrap();
        send_with_backoff(
            make_transport(&url), test_envelope(), path.clone(),
            3, false, Arc::clone(&spool), 5, 300,
        ).await;
        // Retryable exhausted → moved to retry/ for explicit drain
        assert!(!path.exists(), "new/ file should be moved to retry/ after exhaustion");
        let retry_path = path.parent().unwrap().parent().unwrap()
            .join("retry").join(path.file_name().unwrap());
        assert!(retry_path.exists(), "file should be in retry/ after retry exhaustion");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn critical_keeps_retrying_past_max_retries() {
        // First 5 requests return 500, then 200 — critical should succeed eventually
        let mut responses = vec![500u16; 5];
        responses.push(200);
        let url = start_response_server(responses).await;
        let spool = make_spool();
        let path = spool.save(&test_envelope()).unwrap();
        send_with_backoff(
            make_transport(&url), test_envelope(), path.clone(),
            3, true, Arc::clone(&spool), 5, 300, // max_retries=3 but has_critical=true
        ).await;
        assert!(!path.exists(), "critical envelope must eventually succeed");
        let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[tokio::test]
    async fn persist_seq_writes_and_reads_back() {
        let dir = std::env::temp_dir().join(
            format!("test_persist_seq_{}_{}", std::process::id(), rand_u64())
        );
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("seq.state");
        let path_str = path.to_str().unwrap();

        // Empty path guard: isolated dir must stay empty (falsifiable: if guard is absent,
        // tokio::fs::write("", ...) errors without writing to dir, but the warn! would fire).
        persist_seq("", 99).await;
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0, "empty path must not create files");

        persist_seq(path_str, 42).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "42");

        persist_seq(path_str, 43).await;
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "43");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
