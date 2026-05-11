use crate::envelope::Envelope;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tracing::{info, warn};
use ulid::Ulid;

/// Two-pool WAL spool.
///
/// `new/`   — 현재 cycle WAL. save() → 전송 성공 시 commit()으로 삭제.
/// `retry/` — 전송 실패 envelope. move_to_retry() → POST /drain-spool로 재전송.
pub struct Spool {
    new_dir: PathBuf,
    retry_dir: PathBuf,
    pub(crate) max_bytes: u64,      // new/ 용량 상한 (0 = 무제한)
    pub(crate) used_bytes: AtomicU64, // new/ 현재 사용량
}

impl Spool {
    pub fn new(base_dir: &str, max_mb: u64) -> Result<Self> {
        let base = PathBuf::from(base_dir);
        let new_dir = base.join("new");
        let retry_dir = base.join("retry");
        fs::create_dir_all(&new_dir)
            .with_context(|| format!("spool new/ 디렉토리 생성 실패: {}", new_dir.display()))?;
        fs::create_dir_all(&retry_dir)
            .with_context(|| format!("spool retry/ 디렉토리 생성 실패: {}", retry_dir.display()))?;

        let used_bytes = fs::read_dir(&new_dir)
            .ok()
            .map(|d| d.flatten().filter_map(|e| e.metadata().ok()).map(|m| m.len()).sum())
            .unwrap_or(0);

        Ok(Self {
            new_dir,
            retry_dir,
            max_bytes: max_mb * 1024 * 1024,
            used_bytes: AtomicU64::new(used_bytes),
        })
    }

    /// envelope을 new/ WAL에 기록. new/ 용량 초과 시 oldest 파일을 retry/로 evict 후 저장.
    pub fn save(&self, envelope: &Envelope) -> Result<PathBuf> {
        let json = serde_json::to_vec(envelope).context("spool 직렬화 실패")?;
        let json_len = json.len() as u64;

        if self.max_bytes > 0 {
            while self.used_bytes.load(Ordering::SeqCst) + json_len > self.max_bytes {
                if !self.evict_oldest_to_retry() {
                    break; // new/ 비었거나 rename 실패 — 용량 초과 허용
                }
            }
        }

        let id = Ulid::new().to_string();
        let path = self.new_dir.join(format!("{id}.json"));
        fs::write(&path, &json)
            .with_context(|| format!("spool 쓰기 실패: {}", path.display()))?;
        self.used_bytes.fetch_add(json_len, Ordering::SeqCst);
        Ok(path)
    }

    /// 전송 성공 후 new/ 파일 삭제, used_bytes 감소
    pub fn commit(&self, path: &Path) {
        if path.as_os_str().is_empty() {
            return;
        }
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        match fs::remove_file(path) {
            Ok(()) => {
                if size > 0 {
                    self.used_bytes.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(size))).ok();
                }
            }
            Err(e) => warn!(path = %path.display(), err = %e, "spool 파일 삭제 실패"),
        }
    }

    /// 전송 실패 후 new/ → retry/ 이동. used_bytes 감소.
    pub fn move_to_retry(&self, path: &Path) {
        if path.as_os_str().is_empty() {
            return; // WAL 없이 전송된 경우 — 이동할 파일 없음
        }
        let filename = match path.file_name() {
            Some(f) => f,
            None => return,
        };
        let dest = self.retry_dir.join(filename);
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        match fs::rename(path, &dest) {
            Ok(()) => {
                if size > 0 {
                    self.used_bytes.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(size))).ok();
                }
                info!(path = %dest.display(), "전송 실패 envelope retry/로 이동");
            }
            Err(e) => warn!(src = %path.display(), dest = %dest.display(), err = %e,
                "retry/ 이동 실패 — 파일 new/에 보존"),
        }
    }

    /// ULID 순(시간순)으로 정렬된 new/ 미처리 파일 목록 (데몬 재시작 후 WAL 재전송용)
    pub fn pending(&self) -> Vec<PathBuf> {
        self.list_dir_sorted(&self.new_dir)
    }

    /// retry/ 에서 ULID 생성 시각이 [from, to) 범위인 파일 목록 (drain API용)
    pub fn drain_window(&self, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<PathBuf> {
        let entries = match fs::read_dir(&self.retry_dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %self.retry_dir.display(), err = %e, "retry/ 디렉토리 읽기 실패");
                return vec![];
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .filter(|p| ulid_timestamp(p).map_or(false, |ts| ts >= from && ts < to))
            .collect();
        paths.sort();
        paths
    }

    /// drain 전송 성공 후 retry/ 파일 삭제
    pub fn drain_commit(&self, path: &Path) {
        if let Err(e) = fs::remove_file(path) {
            warn!(path = %path.display(), err = %e, "retry/ drain 파일 삭제 실패");
        }
    }

    /// spool 파일에서 envelope 로드 (new/ 또는 retry/ 모두 사용 가능)
    pub fn load(&self, path: &Path) -> Result<Envelope> {
        let data = fs::read(path)
            .with_context(|| format!("spool 읽기 실패: {}", path.display()))?;
        serde_json::from_slice(&data).context("spool 역직렬화 실패")
    }

    pub fn log_pending(&self) {
        let n = self.pending().len();
        if n > 0 {
            info!(pending = n, dir = %self.new_dir.display(), "spool WAL 재전송 대기 파일 발견");
        }
    }

    // ── private ───────────────────────────────────────────────────────────────

    fn list_dir_sorted(&self, dir: &Path) -> Vec<PathBuf> {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                warn!(dir = %dir.display(), err = %e, "spool 디렉토리 읽기 실패 — 재전송 생략");
                return vec![];
            }
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
            .collect();
        paths.sort();
        paths
    }

    /// oldest new/ 파일을 retry/로 이동. 성공 시 true, new/ 비었거나 rename 실패 시 false.
    fn evict_oldest_to_retry(&self) -> bool {
        let files = self.list_dir_sorted(&self.new_dir);
        let oldest = match files.first() {
            Some(f) => f,
            None => return false,
        };
        let filename = match oldest.file_name() {
            Some(f) => f,
            None => return false,
        };
        let dest = self.retry_dir.join(filename);
        let size = fs::metadata(oldest).map(|m| m.len()).unwrap_or(0);
        match fs::rename(oldest, &dest) {
            Ok(()) => {
                if size > 0 {
                    self.used_bytes.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(size))).ok();
                }
                warn!(path = %dest.display(), "spool new/ 용량 초과 — oldest 파일 retry/로 evict");
                true
            }
            Err(e) => {
                warn!(src = %oldest.display(), err = %e, "eviction 실패 — 용량 초과 상태로 저장 진행");
                false
            }
        }
    }

    /// new/ 현재 사용량 (bytes)
    pub fn new_used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::SeqCst)
    }

    /// retry/ 대기 파일 수
    pub fn retry_count(&self) -> usize {
        match fs::read_dir(&self.retry_dir) {
            Ok(entries) => entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
                .count(),
            Err(_) => 0,
        }
    }
}

/// ULID 파일명에서 생성 시각 추출
fn ulid_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let ulid: Ulid = stem.parse().ok()?;
    DateTime::<Utc>::from_timestamp_millis(ulid.timestamp_ms() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Cycle, Headers, Envelope};

    fn test_envelope() -> Envelope {
        Envelope {
            event_kind: "log_batch".to_string(),
            cycle: Cycle {
                host: "test-host".to_string(),
                host_id: "hid".to_string(),
                boot_id: "bid".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
                window: None,
                seq: Some(1),
            },
            headers: Headers { total_sections: 0, counts: None, process_health: None, duration_ms: Some(0) },
            body: vec![],
        }
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("spool_{tag}_{}", std::process::id()))
    }

    #[test]
    fn save_places_file_in_new_dir() {
        let dir = tmp_dir("new_dir");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let path = spool.save(&test_envelope()).unwrap();
        assert!(path.starts_with(dir.join("new")), "save() must write to new/");
        assert!(path.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_deletes_from_new() {
        let dir = tmp_dir("commit");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let path = spool.save(&test_envelope()).unwrap();
        let used = spool.used_bytes.load(Ordering::SeqCst);
        assert!(used > 0);
        spool.commit(&path);
        assert!(!path.exists());
        assert_eq!(spool.used_bytes.load(Ordering::SeqCst), 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_to_retry_moves_file_and_decrements_counter() {
        let dir = tmp_dir("move_retry");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let new_path = spool.save(&test_envelope()).unwrap();
        let used_before = spool.used_bytes.load(Ordering::SeqCst);
        assert!(used_before > 0);

        spool.move_to_retry(&new_path);

        assert!(!new_path.exists(), "file must be removed from new/");
        assert_eq!(spool.used_bytes.load(Ordering::SeqCst), 0, "used_bytes must decrement");

        let retry_count = fs::read_dir(dir.join("retry")).unwrap()
            .filter(|e| e.as_ref().ok().map_or(false, |e| e.path().extension().and_then(|x| x.to_str()) == Some("json")))
            .count();
        assert_eq!(retry_count, 1, "file must appear in retry/");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_commit_removes_from_retry() {
        let dir = tmp_dir("drain_commit");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let new_path = spool.save(&test_envelope()).unwrap();
        spool.move_to_retry(&new_path);

        let retry_path = fs::read_dir(dir.join("retry")).unwrap()
            .flatten().find(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .map(|e| e.path()).unwrap();

        spool.drain_commit(&retry_path);
        assert!(!retry_path.exists(), "drain_commit must delete retry/ file");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_window_returns_file_in_range() {
        let dir = tmp_dir("drain_window");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let new_path = spool.save(&test_envelope()).unwrap();
        spool.move_to_retry(&new_path);

        // Wide window — must include the just-created ULID
        let from = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let to = Utc::now() + chrono::Duration::hours(1);
        let files = spool.drain_window(from, to);
        assert_eq!(files.len(), 1, "drain_window must return the retry file in range");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_window_excludes_file_outside_range() {
        let dir = tmp_dir("drain_window_excl");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let new_path = spool.save(&test_envelope()).unwrap();
        spool.move_to_retry(&new_path);

        // Window in the past — must exclude current file
        let from = chrono::DateTime::<Utc>::from_timestamp(0, 0).unwrap();
        let to = chrono::DateTime::<Utc>::from_timestamp(1, 0).unwrap(); // epoch+1s
        let files = spool.drain_window(from, to);
        assert!(files.is_empty(), "drain_window must exclude file outside range");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_on_cap_moves_oldest_to_retry() {
        let dir = tmp_dir("eviction");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();

        // Save a file and artificially force used_bytes to near-max
        let first_path = spool.save(&test_envelope()).unwrap();
        // Set used_bytes to max so next save triggers eviction
        spool.used_bytes.store(spool.max_bytes, Ordering::SeqCst);

        let _second_path = spool.save(&test_envelope()).unwrap();

        // first file should have been evicted to retry/
        assert!(!first_path.exists(), "oldest file must be evicted to retry/");
        let retry_count = fs::read_dir(dir.join("retry")).unwrap()
            .filter(|e| e.as_ref().ok().map_or(false, |e| e.path().extension().and_then(|x| x.to_str()) == Some("json")))
            .count();
        assert_eq!(retry_count, 1, "evicted file must appear in retry/");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_roundtrip() {
        let dir = tmp_dir("load");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let env = test_envelope();
        let path = spool.save(&env).unwrap();
        let loaded = spool.load(&path).unwrap();
        assert_eq!(loaded.event_kind, "log_batch");
        assert_eq!(loaded.cycle.host, "test-host");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_returns_new_files_sorted() {
        let dir = tmp_dir("pending");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let p1 = spool.save(&test_envelope()).unwrap();
        let p2 = spool.save(&test_envelope()).unwrap();
        let pending = spool.pending();
        assert_eq!(pending.len(), 2);
        assert!(pending[0] <= pending[1], "pending must be ULID-sorted");
        // pending/ should only contain new/ files
        assert!(pending.iter().all(|p| p.starts_with(dir.join("new"))));
        drop((p1, p2));
        let _ = fs::remove_dir_all(&dir);
    }
}
