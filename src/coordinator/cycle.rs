use crate::envelope::{
    BySeverity, Counts, Cycle, DedupEvent, Envelope, Headers, ProcessHealth, Section,
};
use std::collections::HashMap;
use tokio::time::Instant;

pub struct CycleState {
    seq: u64,
    host: String,
    host_id: String,
    boot_id: String,
    start_ts: chrono::DateTime<chrono::Utc>,
    started_at: Instant,
    events: Vec<DedupEvent>,
    critical: u64,
    error: u64,
    warn: u64,
    info: u64,
    by_category: HashMap<String, u64>,
    max_events: usize,
}

impl CycleState {
    pub fn new(
        seq: u64,
        host: String,
        host_id: String,
        boot_id: String,
        max_events: usize,
    ) -> Self {
        Self {
            seq,
            host,
            host_id,
            boot_id,
            start_ts: chrono::Utc::now(),
            started_at: Instant::now(),
            events: Vec::new(),
            critical: 0,
            error: 0,
            warn: 0,
            info: 0,
            by_category: HashMap::new(),
            max_events,
        }
    }

    pub fn push(&mut self, ev: DedupEvent) {
        // Enforce per-cycle event cap (0 = unlimited).
        if self.max_events > 0 && self.events.len() >= self.max_events {
            let victim_pos = if ev.severity == "critical" {
                // Critical event: evict the absolute oldest event to guarantee admission.
                Some(0)
            } else {
                // Non-critical event: evict oldest non-critical, or skip if all are critical.
                self.events.iter().position(|e| e.severity != "critical")
            };
            match victim_pos {
                Some(pos) => {
                    let dropped = self.events.remove(pos);
                    tracing::warn!(
                        category = %dropped.category,
                        severity = %dropped.severity,
                        "cycle event cap 도달 — 오래된 이벤트 제거"
                    );
                }
                None => {
                    // All existing events are critical and new event is non-critical → skip.
                    tracing::warn!(
                        category = %ev.category,
                        "cycle event cap 도달 — non-critical 이벤트 스킵 (모두 critical)"
                    );
                    return;
                }
            }
        }
        match ev.severity.as_str() {
            "critical" => self.critical += ev.count,
            "error" => self.error += ev.count,
            "warn" => self.warn += ev.count,
            _ => self.info += ev.count,
        }
        *self.by_category.entry(ev.category.clone()).or_insert(0) += ev.count;
        self.events.push(ev);
    }

    /// Consumes self, returns (Envelope, next_seq)
    pub fn finalize(self, vector_restarts_24h: u32, agent_uptime_secs: u64) -> (Envelope, u64) {
        let duration_ms = self.started_at.elapsed().as_millis() as u64;
        let next_seq = self.seq + 1;
        let n_events = self.events.len();

        let end_ts = self.start_ts + chrono::Duration::milliseconds(duration_ms as i64);
        let window = format!("{}/{}", self.start_ts.to_rfc3339(), end_ts.to_rfc3339());
        let ts = self.start_ts.to_rfc3339();

        let body = if self.events.is_empty() {
            vec![]
        } else {
            let data = match serde_json::to_value(&self.events) {
                Ok(v) => v,
                Err(e) => {
                    tracing::error!(events = self.events.len(), err = %e, "cycle 이벤트 직렬화 실패 — 빈 body로 대체");
                    serde_json::Value::Array(vec![])
                }
            };
            vec![Section {
                section: "logs".to_string(),
                data,
            }]
        };

        let envelope = Envelope {
            event_kind: "log_batch".to_string(),
            cycle: Cycle {
                host: self.host,
                host_id: self.host_id,
                boot_id: self.boot_id,
                ts,
                window: Some(window),
                seq: Some(self.seq),
            },
            headers: Headers {
                total_sections: body.len(),
                counts: Some(Counts {
                    by_severity: BySeverity {
                        critical: self.critical,
                        error: self.error,
                        warn: self.warn,
                        info: self.info,
                    },
                    by_category: self.by_category,
                }),
                process_health: Some(ProcessHealth {
                    vector_restarts_24h,
                    agent_uptime_seconds: agent_uptime_secs,
                }),
                duration_ms: Some(duration_ms),
            },
            body,
        };

        tracing::debug!(seq = self.seq, dedup_events = n_events, "cycle finalize");
        (envelope, next_seq)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_event(severity: &str, count: u64) -> DedupEvent {
        DedupEvent {
            source: "test".to_string(),
            severity: severity.to_string(),
            category: "system.general".to_string(),
            fingerprint: "0000000000000001".to_string(),
            template: "test <NUM>".to_string(),
            sample_raws: vec!["raw line".to_string()],
            fields: HashMap::new(),
            ts_first: "2026-01-01T00:00:00Z".to_string(),
            ts_last: "2026-01-01T00:00:01Z".to_string(),
            count,
        }
    }

    #[test]
    fn push_increments_severity_counters() {
        let mut state = CycleState::new(1, "host".to_string(), "hid".to_string(), "bid".to_string(), 0);
        state.push(make_event("error", 3));
        state.push(make_event("warn", 2));
        state.push(make_event("critical", 1));
        let (env, next_seq) = state.finalize(0, 0);
        assert_eq!(next_seq, 2);
        let counts = env.headers.counts.unwrap();
        assert_eq!(counts.by_severity.error, 3);
        assert_eq!(counts.by_severity.warn, 2);
        assert_eq!(counts.by_severity.critical, 1);
    }

    #[test]
    fn finalize_body_contains_events() {
        let mut state = CycleState::new(1, "host".to_string(), "hid".to_string(), "bid".to_string(), 0);
        state.push(make_event("info", 1));
        let (env, _) = state.finalize(0, 0);
        assert!(!env.body.is_empty());
        assert_eq!(env.headers.total_sections, 1);
    }

    #[test]
    fn finalize_empty_cycle_has_no_body() {
        let state = CycleState::new(5, "host".to_string(), "hid".to_string(), "bid".to_string(), 0);
        let (env, next_seq) = state.finalize(0, 0);
        assert!(env.body.is_empty());
        assert_eq!(env.headers.total_sections, 0);
        assert_eq!(next_seq, 6);
    }

    #[test]
    fn initial_seq_reflected_in_envelope() {
        let state = CycleState::new(42, "h".to_string(), "hid".to_string(), "bid".to_string(), 0);
        let (env, next_seq) = state.finalize(0, 0);
        assert_eq!(env.cycle.seq, Some(42));
        assert_eq!(next_seq, 43);
    }

    #[test]
    fn cap_drops_oldest_non_critical_when_at_limit() {
        let mut state = CycleState::new(1, "host".to_string(), "hid".to_string(), "bid".to_string(), 2);
        state.push(make_event("info", 1));  // oldest non-critical
        state.push(make_event("warn", 1));
        assert_eq!(state.events.len(), 2);
        state.push(make_event("error", 1)); // should evict oldest non-critical (info)
        assert_eq!(state.events.len(), 2);
        // The remaining events are warn + error (info was evicted)
        assert!(!state.events.iter().any(|e| e.severity == "info"));
    }

    #[test]
    fn cap_skips_non_critical_when_all_existing_are_critical() {
        let mut state = CycleState::new(1, "host".to_string(), "hid".to_string(), "bid".to_string(), 2);
        state.push(make_event("critical", 1));
        state.push(make_event("critical", 1));
        assert_eq!(state.events.len(), 2);
        state.push(make_event("info", 1)); // non-critical; all existing are critical → skip
        assert_eq!(state.events.len(), 2);
    }

    #[test]
    fn cap_admits_critical_by_evicting_oldest() {
        let mut state = CycleState::new(1, "host".to_string(), "hid".to_string(), "bid".to_string(), 2);
        state.push(make_event("info", 1));
        state.push(make_event("warn", 1));
        state.push(make_event("critical", 1)); // critical always admitted; evicts oldest
        assert_eq!(state.events.len(), 2);
        assert!(state.events.iter().any(|e| e.severity == "critical"));
    }

    #[test]
    fn cap_admits_critical_into_all_critical_pool() {
        // All existing events are critical; new event is ALSO critical — oldest critical evicted.
        let mut state = CycleState::new(1, "host".to_string(), "hid".to_string(), "bid".to_string(), 2);
        state.push(make_event("critical", 1)); // idx 0 — will be evicted
        state.push(make_event("critical", 2)); // idx 1
        assert_eq!(state.events.len(), 2);
        state.push(make_event("critical", 3)); // new critical; evicts idx 0 (oldest)
        assert_eq!(state.events.len(), 2);
        // The remaining two should be count=2 and count=3 (count=1 was evicted)
        let counts: Vec<u64> = state.events.iter().map(|e| e.count).collect();
        assert!(counts.contains(&2));
        assert!(counts.contains(&3));
        assert!(!counts.contains(&1));
    }
}
