use crate::envelope::Envelope;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use tracing::{error, info, warn};
use ulid::Ulid;

/// Two-pool WAL spool.
///
/// `new/`     — 현재 cycle WAL. save() → 전송 성공 시 commit()으로 삭제.
/// `retry/`   — 전송 실패 envelope. move_to_retry() → POST /drain-spool로 재전송.
/// `corrupt/` — 파싱 실패(잘림 등) 파일 격리. 재전송 경로 밖, 포렌식용 보존.
pub struct Spool {
    new_dir: PathBuf,
    retry_dir: PathBuf,
    corrupt_dir: PathBuf,
    pub(crate) max_bytes: u64,         // new/ 용량 상한 (0 = 무제한)
    pub(crate) used_bytes: AtomicU64,  // new/ 현재 사용량
    retry_file_count: AtomicUsize,     // retry/ 파일 수 (O(1) 조회, fs::read_dir 불필요)
    retry_max_bytes: u64,              // retry/ 용량 상한 (0 = 무제한)
    retry_ttl_secs: u64,               // retry/ 보관 기간 (0 = 무기한)
    save_lock: Mutex<()>,              // eviction check + write 직렬화
}

impl Spool {
    pub fn new(base_dir: &str, max_mb: u64) -> Result<Self> {
        let base = PathBuf::from(base_dir);
        let new_dir = base.join("new");
        let retry_dir = base.join("retry");
        let corrupt_dir = base.join("corrupt");
        fs::create_dir_all(&new_dir)
            .with_context(|| format!("spool new/ 디렉토리 생성 실패: {}", new_dir.display()))?;
        fs::create_dir_all(&retry_dir)
            .with_context(|| format!("spool retry/ 디렉토리 생성 실패: {}", retry_dir.display()))?;

        // 기동 시 이전 쓰기 도중 crash로 남은 temp 파일(.{ulid}.json.tmp) 정리 —
        // rename 전이므로 어차피 불완전한 데이터, WAL 대상이 아니다.
        for dir in [&new_dir, &retry_dir] {
            if let Ok(entries) = fs::read_dir(dir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if !is_json_file(&p) {
                        if fs::remove_file(&p).is_ok() {
                            warn!(path = %p.display(), "spool temp/잔여 파일 정리 (이전 쓰기 중단 흔적)");
                        }
                    }
                }
            }
        }

        // 실패 시 bail — 초기 used_bytes 오류는 무제한 쓰기로 이어지므로 기동 거부
        // (.json 파일만 집계 — temp/숨김 파일은 WAL이 아니므로 용량에서 제외)
        let used_bytes: u64 = fs::read_dir(&new_dir)
            .with_context(|| format!("spool new/ 초기 사용량 계산 실패: {}", new_dir.display()))?
            .flatten()
            .filter(|e| is_json_file(&e.path()))
            .filter_map(|e| e.metadata().ok())
            .map(|m| m.len())
            .sum();

        let initial_retry_count: usize = fs::read_dir(&retry_dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| is_json_file(&e.path()))
                    .count()
            })
            .unwrap_or(0);

        Ok(Self {
            new_dir,
            retry_dir,
            corrupt_dir,
            max_bytes: max_mb * 1024 * 1024,
            used_bytes: AtomicU64::new(used_bytes),
            retry_file_count: AtomicUsize::new(initial_retry_count),
            // 기본값 = 무제한(0). 운영은 with_retry_limits()로 실제 한도 지정.
            // (테스트는 new()만 쓰므로 기존 동작 보존)
            retry_max_bytes: 0,
            retry_ttl_secs: 0,
            save_lock: Mutex::new(()),
        })
    }

    /// retry/ 데드레터 상한 설정 (운영용). max_mb=0/ttl_hours=0 이면 각각 무제한.
    /// 설정 즉시 기동 시점의 초과분을 정리한다.
    pub fn with_retry_limits(mut self, max_mb: u64, ttl_hours: u64) -> Self {
        self.retry_max_bytes = max_mb * 1024 * 1024;
        self.retry_ttl_secs = ttl_hours * 3600;
        self.enforce_retry_limits();
        self
    }

    /// retry/ 데드레터에 TTL·용량 상한을 적용해 오래된/초과 파일을 삭제한다.
    /// 미배달 데이터를 버리는 정책적 삭제이므로 드롭 시 경고를 남긴다.
    /// (수신 서버 장기 다운 시 디스크가 가득 차 호스트를 위협하는 것을 방지)
    fn enforce_retry_limits(&self) {
        if self.retry_max_bytes == 0 && self.retry_ttl_secs == 0 {
            return; // 한도 미설정 — 기존 동작(무제한) 보존
        }
        let entries = match fs::read_dir(&self.retry_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        // (경로, 생성시각, 크기) 수집 후 시간순(오래된 것 먼저) 정렬
        let mut files: Vec<(PathBuf, DateTime<Utc>, u64)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| is_json_file(p))
            .filter_map(|p| {
                let ts = ulid_timestamp(&p)?;
                let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                Some((p, ts, size))
            })
            .collect();
        files.sort_by_key(|(_, ts, _)| *ts);

        let now = Utc::now();
        let mut dropped = 0usize;

        // 1) TTL 초과 삭제
        if self.retry_ttl_secs > 0 {
            let ttl = chrono::Duration::seconds(self.retry_ttl_secs as i64);
            files.retain(|(p, ts, _)| {
                if now - *ts > ttl {
                    if fs::remove_file(p).is_ok() {
                        self.retry_file_count
                            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1)))
                            .ok();
                        dropped += 1;
                    }
                    false // 목록에서 제거
                } else {
                    true
                }
            });
        }

        // 2) 용량 상한 초과 시 오래된 것부터 삭제
        if self.retry_max_bytes > 0 {
            let mut total: u64 = files.iter().map(|(_, _, s)| *s).sum();
            let mut iter = files.into_iter();
            while total > self.retry_max_bytes {
                let (p, _, size) = match iter.next() {
                    Some(f) => f,
                    None => break,
                };
                if fs::remove_file(&p).is_ok() {
                    total = total.saturating_sub(size);
                    self.retry_file_count
                        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1)))
                        .ok();
                    dropped += 1;
                }
            }
        }

        if dropped > 0 {
            warn!(
                dropped,
                remaining = self.retry_file_count.load(Ordering::SeqCst),
                "retry/ 데드레터 상한 초과 — 오래된 미배달 envelope 삭제 (디스크 보호)"
            );
        }
    }

    /// envelope을 new/ WAL에 기록. new/ 용량 초과 시 oldest 파일을 retry/로 evict 후 저장.
    #[allow(dead_code)]
    pub fn save(&self, envelope: &Envelope) -> Result<PathBuf> {
        let json = serde_json::to_vec(envelope).context("spool 직렬화 실패")?;
        self.save_bytes(&json)
    }

    /// 직렬화된 bytes를 new/ WAL에 기록. save()와 동일한 eviction 정책 적용.
    /// coordinator가 size check를 위해 이미 직렬화한 bytes를 재사용할 수 있도록 제공.
    pub fn save_bytes(&self, json: &[u8]) -> Result<PathBuf> {
        let json_len = json.len() as u64;

        // Mutex로 eviction check + write 직렬화 — concurrent save() 경쟁 방지
        let _lock = self.save_lock.lock().expect("spool save lock poisoned");

        if self.max_bytes > 0 {
            // 파일 목록을 한 번만 수집해 루프 내 반복 dir scan O(n²) 방지
            let candidates = self.list_dir_sorted(&self.new_dir);
            let mut iter = candidates.iter();
            while self.used_bytes.load(Ordering::SeqCst) + json_len > self.max_bytes {
                let oldest = match iter.next() {
                    Some(p) => p,
                    None => break, // new/ 비었거나 모두 시도 — 용량 초과 허용
                };
                let filename = match oldest.file_name() {
                    Some(f) => f,
                    None => continue,
                };
                let dest = self.retry_dir.join(filename);
                let size = fs::metadata(oldest).map(|m| m.len()).unwrap_or(0);
                match fs::rename(oldest, &dest) {
                    Ok(()) => {
                        if size > 0 {
                            self.used_bytes.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
                                |v| Some(v.saturating_sub(size))).ok();
                        }
                        self.retry_file_count.fetch_add(1, Ordering::SeqCst);
                        warn!(path = %dest.display(), "spool new/ 용량 초과 — oldest 파일 retry/로 evict");
                    }
                    Err(e) => {
                        warn!(src = %oldest.display(), err = %e, "eviction 실패 — 건너뜀");
                    }
                }
            }
        }

        let id = Ulid::new().to_string();
        let path = write_atomic(&self.new_dir, &id, json)?;
        self.used_bytes.fetch_add(json_len, Ordering::SeqCst);

        // eviction으로 retry/에 파일이 유입됐을 수 있으니 상한 재적용
        drop(_lock);
        self.enforce_retry_limits();
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
                self.retry_file_count.fetch_add(1, Ordering::SeqCst);
                info!(path = %dest.display(), "전송 실패 envelope retry/로 이동");
                self.enforce_retry_limits();
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
            .filter(|p| is_json_file(p))
            .filter(|p| ulid_timestamp(p).map_or(false, |ts| ts >= from && ts < to))
            .collect();
        paths.sort();
        paths
    }

    /// drain 전송 성공 후 retry/ 파일 삭제
    pub fn drain_commit(&self, path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {
                self.retry_file_count.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
                    |v| Some(v.saturating_sub(1))).ok();
            }
            Err(e) => warn!(path = %path.display(), err = %e, "retry/ drain 파일 삭제 실패"),
        }
    }

    /// spool 파일에서 envelope 로드 (new/ 또는 retry/ 모두 사용 가능)
    pub fn load(&self, path: &Path) -> Result<Envelope> {
        let data = fs::read(path)
            .with_context(|| format!("spool 읽기 실패: {}", path.display()))?;
        serde_json::from_slice(&data).context("spool 역직렬화 실패")
    }

    /// load + 파싱 실패 시 corrupt/ 격리 (startup replay 경로용).
    /// 전원 유실 등으로 잘린 파일이 new/에 영영 남아 매 기동마다 실패를 반복하고
    /// used_bytes만 차지하는 것을 방지한다. 읽기(IO) 실패는 일시적일 수 있어 격리하지 않는다.
    pub fn load_or_quarantine(&self, path: &Path) -> Result<Envelope> {
        let data = fs::read(path)
            .with_context(|| format!("spool 읽기 실패: {}", path.display()))?;
        match serde_json::from_slice(&data) {
            Ok(env) => Ok(env),
            Err(e) => {
                self.quarantine(path);
                Err(anyhow::anyhow!(
                    "spool 역직렬화 실패 ({e}) — corrupt/로 격리: {}",
                    path.display()
                ))
            }
        }
    }

    /// 파손 spool 파일을 corrupt/로 이동 — 재전송 경로 밖으로 빼되 포렌식용으로 보존.
    /// new/ 파일이면 used_bytes, retry/ 파일이면 retry_file_count에서 차감한다.
    pub fn quarantine(&self, path: &Path) {
        let filename = match path.file_name() {
            Some(f) => f,
            None => return,
        };
        if let Err(e) = fs::create_dir_all(&self.corrupt_dir) {
            warn!(dir = %self.corrupt_dir.display(), err = %e, "corrupt/ 생성 실패 — 격리 생략");
            return;
        }
        let dest = self.corrupt_dir.join(filename);
        let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        match fs::rename(path, &dest) {
            Ok(()) => {
                if size > 0 && path.starts_with(&self.new_dir) {
                    self.used_bytes.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
                        |v| Some(v.saturating_sub(size))).ok();
                }
                if path.starts_with(&self.retry_dir) {
                    self.retry_file_count.fetch_update(Ordering::SeqCst, Ordering::SeqCst,
                        |v| Some(v.saturating_sub(1))).ok();
                }
                error!(path = %dest.display(), size, "파손된 spool 파일 corrupt/로 격리 — 수동 확인 필요");
            }
            Err(e) => warn!(src = %path.display(), err = %e, "corrupt/ 격리 실패 — 파일 원위치 보존"),
        }
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
            .filter(|p| is_json_file(p))
            .collect();
        paths.sort();
        paths
    }

    /// new/ 현재 사용량 (bytes)
    pub fn new_used_bytes(&self) -> u64 {
        self.used_bytes.load(Ordering::SeqCst)
    }

    /// retry/ 대기 파일 수 (O(1) — fs::read_dir 없이 AtomicUsize로 추적)
    pub fn retry_count(&self) -> usize {
        self.retry_file_count.load(Ordering::SeqCst)
    }
}

/// ULID 파일명에서 생성 시각 추출
fn ulid_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    let stem = path.file_stem()?.to_str()?;
    let ulid: Ulid = stem.parse().ok()?;
    DateTime::<Utc>::from_timestamp_millis(ulid.timestamp_ms() as i64)
}

/// spool 스캔 대상 여부 — `{ulid}.json`만 유효. temp(`.{ulid}.json.tmp`)와
/// 숨김 파일(`.` 시작)은 쓰기 도중이거나 잔여물이므로 제외한다.
fn is_json_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    !name.starts_with('.') && name.ends_with(".json")
}

/// crash-safe 쓰기: 같은 디렉토리의 temp 파일에 write + fsync 후 rename.
/// 전원 유실이 rename 앞에서 일어나면 temp만 남고(기동 시 정리),
/// 뒤에서 일어나면 완전한 `.json`이 남는다 — 잘린 `.json`은 존재할 수 없다.
fn write_atomic(dir: &Path, id: &str, data: &[u8]) -> Result<PathBuf> {
    let tmp = dir.join(format!(".{id}.json.tmp"));
    let path = dir.join(format!("{id}.json"));
    if let Err(e) = write_and_rename(&tmp, &path, data) {
        let _ = fs::remove_file(&tmp); // 실패 잔여물 정리 (없어도 무해)
        return Err(e);
    }
    // 부모 디렉토리 fsync — rename(메타데이터) 자체의 내구성 보장 (unix).
    // 실패해도 데이터 자체는 fsync 완료 상태라 치명적이지 않음 — best effort.
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(path)
}

fn write_and_rename(tmp: &Path, path: &Path, data: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut f = fs::File::create(tmp)
        .with_context(|| format!("spool temp 생성 실패: {}", tmp.display()))?;
    f.write_all(data)
        .with_context(|| format!("spool temp 쓰기 실패: {}", tmp.display()))?;
    f.sync_all()
        .with_context(|| format!("spool temp fsync 실패: {}", tmp.display()))?;
    drop(f);
    fs::rename(tmp, path)
        .with_context(|| format!("spool rename 실패: {}", path.display()))?;
    Ok(())
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
        assert_eq!(spool.retry_count(), 1, "retry_file_count atomic must reflect move_to_retry");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn drain_commit_removes_from_retry() {
        let dir = tmp_dir("drain_commit");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let new_path = spool.save(&test_envelope()).unwrap();
        spool.move_to_retry(&new_path);
        assert_eq!(spool.retry_count(), 1, "retry_file_count must be 1 after move_to_retry");

        let retry_path = fs::read_dir(dir.join("retry")).unwrap()
            .flatten().find(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
            .map(|e| e.path()).unwrap();

        spool.drain_commit(&retry_path);
        assert!(!retry_path.exists(), "drain_commit must delete retry/ file");
        assert_eq!(spool.retry_count(), 0, "retry_file_count must decrement after drain_commit");
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
        assert_eq!(spool.retry_count(), 1, "retry_file_count atomic must reflect eviction");
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

    #[test]
    fn save_bytes_writes_raw_bytes() {
        let dir = tmp_dir("save_bytes");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let json = serde_json::to_vec(&test_envelope()).unwrap();
        let path = spool.save_bytes(&json).unwrap();
        assert!(path.exists());
        let on_disk = fs::read(&path).unwrap();
        assert_eq!(on_disk, json);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_bytes_leaves_no_tmp_files() {
        let dir = tmp_dir("no_tmp");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let json = serde_json::to_vec(&test_envelope()).unwrap();
        let path = spool.save_bytes(&json).unwrap();
        assert!(path.exists());
        // temp 파일(.{ulid}.json.tmp)이 rename 후 남아있으면 안 됨
        let leftovers: Vec<_> = fs::read_dir(dir.join("new")).unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| !is_json_file(p))
            .collect();
        assert!(leftovers.is_empty(), "save_bytes 후 temp 파일이 남으면 안 됨: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_json_is_quarantined_by_load_or_quarantine() {
        let dir = tmp_dir("quarantine");
        // 전원 유실로 잘린 json을 new/에 직접 생성 (구버전 fs::write 시나리오)
        fs::create_dir_all(dir.join("new")).unwrap();
        let id = Ulid::new().to_string();
        let bad_path = dir.join("new").join(format!("{id}.json"));
        fs::write(&bad_path, b"{\"event_kind\": \"log_ba").unwrap();

        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        assert!(spool.new_used_bytes() > 0, "초기 스캔이 파손 파일 크기를 집계");

        let res = spool.load_or_quarantine(&bad_path);
        assert!(res.is_err(), "잘린 json은 로드 실패해야 함");
        assert!(!bad_path.exists(), "파손 파일은 new/에서 제거되어야 함");
        assert!(
            dir.join("corrupt").join(format!("{id}.json")).exists(),
            "파손 파일은 corrupt/로 격리되어야 함"
        );
        assert_eq!(spool.new_used_bytes(), 0, "격리 후 used_bytes에서 차감되어야 함");
        assert!(spool.pending().is_empty(), "격리 후 replay 대상에서 제외되어야 함");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pending_ignores_tmp_and_hidden_files() {
        let dir = tmp_dir("ignore_tmp");
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        let real = spool.save(&test_envelope()).unwrap();
        // 쓰기 도중 형태의 temp/숨김 파일을 new/에 직접 생성
        fs::write(dir.join("new").join(".01ABC.json.tmp"), b"partial").unwrap();
        fs::write(dir.join("new").join(".hidden.json"), b"{}").unwrap();

        let pending = spool.pending();
        assert_eq!(pending.len(), 1, "temp/숨김 파일은 pending에서 제외되어야 함");
        assert_eq!(pending[0], real);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn startup_scan_cleans_leftover_tmp_files() {
        let dir = tmp_dir("startup_tmp");
        fs::create_dir_all(dir.join("new")).unwrap();
        fs::create_dir_all(dir.join("retry")).unwrap();
        let tmp_new = dir.join("new").join(".01XYZ.json.tmp");
        let tmp_retry = dir.join("retry").join(".01XYZ.json.tmp");
        fs::write(&tmp_new, b"partial write").unwrap();
        fs::write(&tmp_retry, b"partial write").unwrap();

        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        assert!(!tmp_new.exists(), "기동 스캔이 new/ temp 잔여물을 정리해야 함");
        assert!(!tmp_retry.exists(), "기동 스캔이 retry/ temp 잔여물을 정리해야 함");
        assert_eq!(spool.new_used_bytes(), 0, "temp 파일은 used_bytes에 집계되면 안 됨");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_ttl_deletes_old_files() {
        let dir = tmp_dir("retry_ttl");
        // 100시간 전 ULID 파일을 retry/에 직접 생성
        let old_ms = (Utc::now().timestamp_millis() - 100 * 3600 * 1000) as u64;
        let old_id = Ulid::from_parts(old_ms, 0).to_string();
        fs::create_dir_all(dir.join("retry")).unwrap();
        let old_path = dir.join("retry").join(format!("{old_id}.json"));
        fs::write(&old_path, b"{}").unwrap();

        // TTL 1시간 적용 → 100시간 전 파일 삭제 (with_retry_limits가 생성 시 enforce)
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap().with_retry_limits(0, 1);
        assert!(!old_path.exists(), "TTL 초과 retry 파일은 삭제되어야 함");
        assert_eq!(spool.retry_count(), 0, "retry_file_count가 삭제를 반영해야 함");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_no_limits_keeps_old_files() {
        let dir = tmp_dir("retry_nolimit");
        let old_ms = (Utc::now().timestamp_millis() - 100 * 3600 * 1000) as u64;
        let old_id = Ulid::from_parts(old_ms, 0).to_string();
        fs::create_dir_all(dir.join("retry")).unwrap();
        let old_path = dir.join("retry").join(format!("{old_id}.json"));
        fs::write(&old_path, b"{}").unwrap();

        // 한도 미설정(new()만) → 기존 동작 유지, 오래된 파일도 보존
        let spool = Spool::new(dir.to_str().unwrap(), 10).unwrap();
        assert!(old_path.exists(), "한도 미설정 시 오래된 파일도 보존해야 함");
        assert_eq!(spool.retry_count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlimited_spool_never_evicts() {
        let dir = tmp_dir("unlimited");
        let spool = Spool::new(dir.to_str().unwrap(), 0).unwrap(); // max_mb=0 → unlimited
        for _ in 0..5 {
            spool.save(&test_envelope()).unwrap();
        }
        let retry_count = fs::read_dir(dir.join("retry")).unwrap()
            .filter(|e| e.as_ref().ok().map_or(false, |e| e.path().extension().and_then(|x| x.to_str()) == Some("json")))
            .count();
        assert_eq!(retry_count, 0, "unlimited spool must never evict to retry/");
        let _ = fs::remove_dir_all(&dir);
    }
}
