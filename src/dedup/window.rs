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
}

impl DedupWindow {
    pub fn new(window_seconds: u64, lru_cap: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(lru_cap).expect("lru_cap > 0")),
            window_secs: window_seconds as i64,
            total_evictions: 0,
        }
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
                if state.sample_raws.len() < 3 {
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

        // LRU eviction 추적
        if self.cache.len() == self.cache.cap().get() {
            self.total_evictions += 1;
            if self.total_evictions % 100 == 1 {
                warn!(
                    total = self.total_evictions,
                    cap = self.cache.cap().get(),
                    "LRU evict 발생"
                );
            }
        }

        self.insert_new(fingerprint, source, severity, category, template, raw, ts, fields);
        None
    }

    /// 윈도우가 만료된 항목을 모두 방출
    pub fn flush_expired(&mut self) -> Vec<DedupEvent> {
        let now = Utc::now();
        let window = self.window_secs;
        let expired: Vec<u64> = self
            .cache
            .iter()
            .filter(|(_, s)| (now - s.ts_last).num_seconds() >= window)
            .map(|(k, _)| *k)
            .collect();

        expired
            .into_iter()
            .filter_map(|k| self.cache.pop(&k).map(|v| to_event(k, v)))
            .collect()
    }

    /// cycle 종료 시 모든 항목 방출
    pub fn flush_all(&mut self) -> Vec<DedupEvent> {
        let keys: Vec<u64> = self.cache.iter().map(|(k, _)| *k).collect();
        keys.into_iter()
            .filter_map(|k| self.cache.pop(&k).map(|v| to_event(k, v)))
            .collect()
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
        self.cache.push(
            fingerprint,
            DedupState {
                count: 1,
                ts_first: ts,
                ts_last: ts,
                sample_raws: vec![raw],
                severity,
                category,
                source,
                template,
                fields,
            },
        );
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
        let mut w = DedupWindow::new(30, 100);
        assert!(push_simple(&mut w, 1, "error").is_none());
        assert!(push_simple(&mut w, 1, "error").is_none());
        assert!(push_simple(&mut w, 1, "error").is_none());
        let events = w.flush_all();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].count, 3);
    }

    #[test]
    fn different_fingerprints_are_separate() {
        let mut w = DedupWindow::new(30, 100);
        push_simple(&mut w, 1, "error");
        push_simple(&mut w, 2, "warn");
        let events = w.flush_all();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn flush_expired_only_emits_stale() {
        let mut w = DedupWindow::new(0, 100); // 0s window → expires after ≥1 elapsed second
        push_simple(&mut w, 1, "info");
        // num_seconds() floors to integer seconds; need >1s to satisfy `elapsed > 0`
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let expired = w.flush_expired();
        assert_eq!(expired.len(), 1);
        assert!(w.flush_all().is_empty());
    }

    #[test]
    fn critical_severity_promoted() {
        let mut w = DedupWindow::new(30, 100);
        push_simple(&mut w, 1, "error"); // first insert as error
        push_simple(&mut w, 1, "critical"); // upgrade to critical
        let events = w.flush_all();
        assert_eq!(events[0].severity, "critical");
    }

    #[test]
    fn sample_raws_capped_at_3() {
        let mut w = DedupWindow::new(30, 100);
        for _ in 0..10 {
            push_simple(&mut w, 1, "info");
        }
        let events = w.flush_all();
        assert!(events[0].sample_raws.len() <= 3);
    }

    #[test]
    fn out_of_order_event_does_not_move_ts_last_backward() {
        let mut w = DedupWindow::new(30, 100);
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
