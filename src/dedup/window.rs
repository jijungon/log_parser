use crate::envelope::DedupEvent;
use chrono::{DateTime, Utc};
use lru::LruCache;
use std::num::NonZeroUsize;
use tracing::warn;

struct DedupState {
    count: u64,
    ts_first: DateTime<Utc>,
    ts_last: DateTime<Utc>,
    sample_raws: Vec<String>,
    severity: String,
    category: String,
    source: String,
    template: String,
    fields: std::collections::HashMap<String, serde_json::Value>,
}

pub struct DedupWindow {
    cache: LruCache<u64, DedupState>,
    window_secs: i64,
    total_evictions: u64,
    // 그룹당 보관할 원본 로그 샘플 수 상한 (config dedup.sample_raws_cap) —
    // envelope 크기의 지배 요인이라 설정으로 노출한다.
    sample_raws_cap: usize,
    // LRU cap 초과로 축출된 그룹을 폐기하지 않고 보관 → 다음 flush_expired/flush_all에서 방출.
    // 크기는 5초 dedup_tick 사이의 축출량으로 자연히 유계(매 tick마다 비워짐) — 별도 상한 불필요.
    evicted_pending: Vec<DedupEvent>,
}

impl DedupWindow {
    pub fn new(window_seconds: u64, lru_cap: usize, sample_raws_cap: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(lru_cap).expect("lru_cap > 0")),
            window_secs: window_seconds as i64,
            total_evictions: 0,
            sample_raws_cap,
            evicted_pending: Vec::new(),
        }
    }

    /// 윈도우 안의 기존 항목이면 병합하고 true 반환.
    /// 병합 경로에서는 fields/category 계산이 필요 없으므로, 호출자는
    /// try_merge가 false일 때만 필드 추출·분류를 수행하고 push()를 부른다
    /// (중복 라인마다 계산 후 버려지던 낭비 제거 — 병합 시 push()가 인자를 무시하던 것과 동일 의미).
    pub fn try_merge(
        &mut self,
        fingerprint: u64,
        raw: &str,
        ts: DateTime<Utc>,
        severity: &str,
    ) -> bool {
        if let Some(state) = self.cache.get_mut(&fingerprint) {
            let elapsed = (ts - state.ts_last).num_seconds();
            if elapsed < self.window_secs {
                // 윈도우 안: 병합 (순서 역전된 이벤트는 ts_last를 뒤로 당기지 않음)
                state.count += 1;
                if ts > state.ts_last {
                    state.ts_last = ts;
                }
                if state.sample_raws.len() < self.sample_raws_cap {
                    state.sample_raws.push(raw.to_string());
                }
                if severity == "critical" && state.severity != "critical" {
                    state.severity = severity.to_string();
                }
                return true;
            }
        }
        false
    }

    /// 이벤트 하나를 처리. 윈도우 만료 시 DedupEvent 반환.
    pub fn push(
        &mut self,
        fingerprint: u64,
        source: String,
        severity: String,
        category: String,
        template: String,
        raw: String,
        ts: DateTime<Utc>,
        fields: std::collections::HashMap<String, serde_json::Value>,
    ) -> Option<DedupEvent> {
        if let Some(state) = self.cache.get_mut(&fingerprint) {
            let elapsed = (ts - state.ts_last).num_seconds();
            if elapsed < self.window_secs {
                // 윈도우 안: 병합 (순서 역전된 이벤트는 ts_last를 뒤로 당기지 않음)
                state.count += 1;
                if ts > state.ts_last {
                    state.ts_last = ts;
                }
                if state.sample_raws.len() < self.sample_raws_cap {
                    state.sample_raws.push(raw);
                }
                if severity == "critical" && state.severity != "critical" {
                    state.severity = severity;
                }
                return None;
            }
            // 윈도우 만료: 기존 항목 방출 후 새 항목 삽입
            let old = self.cache.pop(&fingerprint).unwrap();
            let emitted = to_event(fingerprint, old);
            self.insert_new(fingerprint, source, severity, category, template, raw, ts, fields);
            return Some(emitted);
        }

        // LRU eviction 추적 (축출분은 폐기되지 않고 insert_new에서 조기 방출 버퍼로 이동)
        if self.cache.len() == self.cache.cap().get() {
            self.total_evictions += 1;
            if self.total_evictions % 100 == 1 {
                warn!(
                    total = self.total_evictions,
                    cap = self.cache.cap().get(),
                    "LRU cap 도달 — 오래된 그룹을 조기 방출(다음 flush에 포함, 유실 없음)"
                );
            }
        }

        self.insert_new(fingerprint, source, severity, category, template, raw, ts, fields);
        None
    }

    /// 지금까지 발생한 LRU 축출 총수 = lru_cap 초과로 **조기 방출된** 고유 패턴 수
    /// (폐기 아님 — 누적 count/샘플은 다음 flush에서 그대로 방출됨).
    /// 평상시 0(창 회전으로 cap 도달 전에 flush됨). >0이면 카디널리티가 cap을 압박한다는 신호.
    pub fn total_evictions(&self) -> u64 {
        self.total_evictions
    }

    /// 윈도우가 만료된 항목을 모두 방출 (+ LRU 축출로 조기 방출 대기 중이던 그룹 포함)
    pub fn flush_expired(&mut self) -> Vec<DedupEvent> {
        let now = Utc::now();
        let window = self.window_secs;
        let expired: Vec<u64> = self
            .cache
            .iter()
            .filter(|(_, s)| (now - s.ts_last).num_seconds() >= window)
            .map(|(k, _)| *k)
            .collect();

        let mut out = std::mem::take(&mut self.evicted_pending);
        out.extend(
            expired
                .into_iter()
                .filter_map(|k| self.cache.pop(&k).map(|v| to_event(k, v))),
        );
        out
    }

    /// cycle 종료 시 모든 항목 방출 (+ LRU 축출로 조기 방출 대기 중이던 그룹 포함)
    pub fn flush_all(&mut self) -> Vec<DedupEvent> {
        let keys: Vec<u64> = self.cache.iter().map(|(k, _)| *k).collect();
        let mut out = std::mem::take(&mut self.evicted_pending);
        out.extend(
            keys.into_iter()
                .filter_map(|k| self.cache.pop(&k).map(|v| to_event(k, v))),
        );
        out
    }

    fn insert_new(
        &mut self,
        fingerprint: u64,
        source: String,
        severity: String,
        category: String,
        template: String,
        raw: String,
        ts: DateTime<Utc>,
        fields: std::collections::HashMap<String, serde_json::Value>,
    ) {
        // insert_new는 항상 "키 부재" 상태에서만 불리므로(push의 두 경로 모두),
        // LruCache::push의 반환값은 언제나 cap 초과로 축출된 **다른 키**의 LRU 항목이다.
        // 폐기하는 대신 flush와 동일한 변환(to_event)으로 DedupEvent를 만들어 버퍼에 보관
        // → 다음 flush_expired/flush_all(코디네이터 5초 dedup_tick)에서 방출 → 유실 0.
        // sample_raws_cap=0이면 샘플 자체를 보관하지 않는다 (템플릿·count만 전송)
        let sample_raws = if self.sample_raws_cap == 0 { Vec::new() } else { vec![raw] };
        if let Some((evicted_key, evicted_state)) = self.cache.push(
            fingerprint,
            DedupState {
                count: 1,
                ts_first: ts,
                ts_last: ts,
                sample_raws,
                severity,
                category,
                source,
                template,
                fields,
            },
        ) {
            debug_assert_ne!(evicted_key, fingerprint, "insert_new는 키 부재 시에만 호출");
            self.evicted_pending.push(to_event(evicted_key, evicted_state));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push_simple(w: &mut DedupWindow, fp: u64, sev: &str) -> Option<DedupEvent> {
        w.push(fp, "journald".to_string(), sev.to_string(), "system.general".to_string(),
            "template <NUM>".to_string(), "raw line".to_string(), Utc::now(),
            std::collections::HashMap::new())
    }

    #[test]
    fn within_window_merges_count() {
        let mut w = DedupWindow::new(30, 100, 3);
        assert!(push_simple(&mut w, 1, "error").is_none());
        assert!(push_simple(&mut w, 1, "error").is_none());
        assert!(push_simple(&mut w, 1, "error").is_none());
        let events = w.flush_all();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].count, 3);
    }

    #[test]
    fn different_fingerprints_are_separate() {
        let mut w = DedupWindow::new(30, 100, 3);
        push_simple(&mut w, 1, "error");
        push_simple(&mut w, 2, "warn");
        let events = w.flush_all();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn flush_expired_only_emits_stale() {
        let mut w = DedupWindow::new(0, 100, 3); // 0s window → expires after ≥1 elapsed second
        push_simple(&mut w, 1, "info");
        // num_seconds() floors to integer seconds; need >1s to satisfy `elapsed > 0`
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let expired = w.flush_expired();
        assert_eq!(expired.len(), 1);
        assert!(w.flush_all().is_empty());
    }

    #[test]
    fn critical_severity_promoted() {
        let mut w = DedupWindow::new(30, 100, 3);
        push_simple(&mut w, 1, "error"); // first insert as error
        push_simple(&mut w, 1, "critical"); // upgrade to critical
        let events = w.flush_all();
        assert_eq!(events[0].severity, "critical");
    }

    #[test]
    fn sample_raws_capped_at_3() {
        let mut w = DedupWindow::new(30, 100, 3);
        for _ in 0..10 {
            push_simple(&mut w, 1, "info");
        }
        let events = w.flush_all();
        assert_eq!(events[0].sample_raws.len(), 3, "기본 cap 3 — 초과 샘플은 버려짐");
    }

    #[test]
    fn sample_raws_cap_1_keeps_exactly_one() {
        let mut w = DedupWindow::new(30, 100, 1);
        for _ in 0..10 {
            push_simple(&mut w, 1, "info");
        }
        let events = w.flush_all();
        assert_eq!(events[0].sample_raws.len(), 1, "cap=1이면 샘플 정확히 1개만 보관");
    }

    #[test]
    fn sample_raws_cap_0_keeps_no_samples() {
        let mut w = DedupWindow::new(30, 100, 0);
        for _ in 0..5 {
            push_simple(&mut w, 1, "info");
        }
        let events = w.flush_all();
        assert!(events[0].sample_raws.is_empty(), "cap=0이면 샘플 미보관 (count·템플릿만)");
        assert_eq!(events[0].count, 5, "샘플 미보관이어도 count 집계는 유지");
    }

    #[test]
    fn lru_eviction_emits_instead_of_discarding() {
        let mut w = DedupWindow::new(30, 3, 3); // 작은 cap으로 축출 유도
        // fp=1에 5건 누적 → LRU 최후순위가 되도록 먼저 넣음
        for _ in 0..5 {
            push_simple(&mut w, 1, "error");
        }
        push_simple(&mut w, 2, "info");
        push_simple(&mut w, 3, "info");
        // cap(3) 가득 → fp=4 삽입이 LRU(fp=1)를 축출
        push_simple(&mut w, 4, "info");
        assert_eq!(w.total_evictions(), 1, "축출 카운터는 계속 증가해야 함");

        let events = w.flush_all();
        // 축출분(fp=1) + 창 보유분(fp=2,3,4) = 4건 — 유실 0
        assert_eq!(events.len(), 4);
        // 들어간 총 count(5+1+1+1=8) == 나온 총 count
        let total: u64 = events.iter().map(|e| e.count).sum();
        assert_eq!(total, 8, "in == out, 누적 count 유실 없음");
        // 축출된 그룹이 누적 count 5를 그대로 갖고 방출됐는지
        let evicted = events
            .iter()
            .find(|e| e.fingerprint == format!("{:016x}", 1u64))
            .expect("축출된 fp=1 그룹이 방출돼야 함");
        assert_eq!(evicted.count, 5);
    }

    #[test]
    fn evicted_pending_drains_via_flush_expired() {
        let mut w = DedupWindow::new(30, 2, 3);
        push_simple(&mut w, 1, "info");
        push_simple(&mut w, 2, "info");
        push_simple(&mut w, 3, "info"); // fp=1 축출
        // 창은 아직 만료 전(30s) — flush_expired는 축출 보류분만 방출
        let events = w.flush_expired();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].fingerprint, format!("{:016x}", 1u64));
        // 이중 방출 금지: 버퍼는 한 번만 비워짐
        assert!(w.flush_expired().is_empty());
        // 창 보유분(fp=2,3)은 그대로
        assert_eq!(w.flush_all().len(), 2);
    }

    #[test]
    fn out_of_order_event_does_not_move_ts_last_backward() {
        let mut w = DedupWindow::new(30, 100, 3);
        let now = Utc::now();
        let past = now - chrono::Duration::seconds(10);
        // First event: now
        w.push(1, "journald".to_string(), "info".to_string(), "system.general".to_string(),
            "tpl".to_string(), "r1".to_string(), now, std::collections::HashMap::new());
        // Second event: past (out-of-order) — should merge but NOT move ts_last backward
        w.push(1, "journald".to_string(), "info".to_string(), "system.general".to_string(),
            "tpl".to_string(), "r2".to_string(), past, std::collections::HashMap::new());
        let events = w.flush_all();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].count, 2);
        // ts_last must still be `now`, not rolled back to `past`
        assert_eq!(events[0].ts_last, now.to_rfc3339());
    }
}

fn to_event(fingerprint: u64, s: DedupState) -> DedupEvent {
    DedupEvent {
        source: s.source,
        severity: s.severity,
        category: s.category,
        fingerprint: format!("{fingerprint:016x}"),
        template: s.template,
        sample_raws: s.sample_raws,
        fields: s.fields,
        ts_first: s.ts_first.to_rfc3339(),
        ts_last: s.ts_last.to_rfc3339(),
        count: s.count,
    }
}
