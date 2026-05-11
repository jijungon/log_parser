use crate::config::PipelineConfig;
use crate::platform::cgroup;
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::time::sleep;
use tracing::{error, info, warn};

pub struct VectorHandle {
    cfg: PipelineConfig,
    cgroup_path: String,
    config_path: String,
    pub restarts_24h: Arc<AtomicU32>,
}

impl VectorHandle {
    pub fn new(cfg: PipelineConfig, cgroup_path: String, config_path: String) -> Self {
        Self {
            cfg,
            cgroup_path,
            config_path,
            restarts_24h: Arc::new(AtomicU32::new(0)),
        }
    }

    async fn spawn_once(&self) -> Result<Child> {
        let child = Command::new(&self.cfg.vector_bin)
            .arg("--config")
            .arg(&self.config_path)
            .spawn()
            .with_context(|| format!("Vector 기동 실패: {}", self.cfg.vector_bin))?;

        let pid = child.id().context("Vector PID 획득 실패")?;
        cgroup::attach_pid(&self.cgroup_path, pid)
            .unwrap_or_else(|e| warn!("Vector PID cgroup 등록 실패 (계속 진행): {e}"));

        info!(pid, config = %self.config_path, "Vector 기동");
        Ok(child)
    }

    /// Runs Vector, restarting on failure until max_restarts_per_hour is exceeded.
    /// Returns Ok(()) on clean shutdown signal (SIGTERM propagated), Err on fatal limit.
    pub async fn run_supervised(self: Arc<Self>) -> Result<()> {
        let mut child = self.spawn_once().await?;
        let mut crash_times: VecDeque<Instant> = VecDeque::new();

        loop {
            let status = child.wait().await;

            match &status {
                Ok(s) if s.success() => {
                    info!("Vector 정상 종료");
                    return Ok(());
                }
                Ok(s) => warn!(code = ?s.code(), "Vector 비정상 종료"),
                Err(e) => error!("Vector wait() 실패: {e}"),
            }

            let now = Instant::now();
            crash_times.push_back(now);
            // Prune entries outside the window to prevent unbounded growth
            crash_times.retain(|&t| now.duration_since(t) < Duration::from_secs(3600));
            let restarts_in_window = count_crashes_in_window(&crash_times, now, Duration::from_secs(3600));
            let total = self.restarts_24h.fetch_add(1, Ordering::Relaxed) + 1;

            if restarts_in_window >= self.cfg.vector_max_restarts_per_hour {
                error!(
                    in_hour = restarts_in_window,
                    limit = self.cfg.vector_max_restarts_per_hour,
                    "Vector 시간당 재시작 한도 초과 — 중단"
                );
                anyhow::bail!("Vector max restarts/hour exceeded");
            }

            info!(
                total_restarts = total,
                restarts_this_hour = restarts_in_window,
                "Vector 5초 후 재시작"
            );
            sleep(Duration::from_secs(5)).await;
            let mut spawn_attempts = 0u32;
            child = loop {
                match self.spawn_once().await {
                    Ok(c) => break c,
                    Err(e) => {
                        spawn_attempts += 1;
                        if spawn_attempts >= 5 {
                            anyhow::bail!("Vector spawn 연속 실패 {spawn_attempts}회 — 중단: {e}");
                        }
                        error!(restarts_in_window, attempt = spawn_attempts, "Vector 재시작 실패: {e} — 5초 후 재시도");
                        sleep(Duration::from_secs(5)).await;
                    }
                }
            };
        }
    }
}

/// 슬라이딩 윈도우 내 크래시 횟수 반환 (순수 함수 — 테스트 가능)
pub fn count_crashes_in_window(
    crash_times: &VecDeque<Instant>,
    now: Instant,
    window: Duration,
) -> u32 {
    crash_times
        .iter()
        .filter(|&&t| now.duration_since(t) < window)
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_crashes_returns_zero() {
        let q = VecDeque::new();
        assert_eq!(count_crashes_in_window(&q, Instant::now(), Duration::from_secs(3600)), 0);
    }

    #[test]
    fn all_recent_crashes_counted() {
        let now = Instant::now();
        let mut q = VecDeque::new();
        // 3 crashes just happened
        for _ in 0..3 {
            q.push_back(now);
        }
        assert_eq!(count_crashes_in_window(&q, now, Duration::from_secs(3600)), 3);
    }

    #[test]
    fn old_crashes_excluded() {
        let now = Instant::now();
        let mut q = VecDeque::new();
        // 2 crashes over 1 hour ago
        let old = now - Duration::from_secs(3601);
        q.push_back(old);
        q.push_back(old);
        // 1 recent crash
        q.push_back(now);
        assert_eq!(count_crashes_in_window(&q, now, Duration::from_secs(3600)), 1);
    }

    #[test]
    fn limit_at_max_stops_restart() {
        // max_restarts=3: at exactly 3 in-window crashes, >=3 fires
        let now = Instant::now();
        let mut q = VecDeque::new();
        for _ in 0..3 { q.push_back(now); }
        let count = count_crashes_in_window(&q, now, Duration::from_secs(3600));
        assert!(count >= 3, "should hit limit at exactly max restarts");
    }
}
