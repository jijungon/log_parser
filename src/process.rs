//! 로그 한 줄 처리 — coordinator(push)와 inbound/collect(sos) 공용 경로.
//!
//! 두 경로가 각자 strip→normalize→severity→fingerprint→dedup를 중복 구현하던 것을
//! 하나로 합쳤다. 경로별 차이(초기 severity 힌트, 구조화 필드 보강)는 인자로 흡수한다.
//! fingerprint 공식·severity 계산·dedup 방식이 여기 한 곳에서만 정의된다.

use crate::dedup::window::DedupWindow;
use crate::envelope::DedupEvent;
use crate::normalize::{categories::CategoryMatcher, fields, severity, tokens};
use chrono::{DateTime, Utc};
use serde_json::Value;
use std::hash::Hasher as _;
use xxhash_rust::xxh3::Xxh3;

/// dedup fingerprint. **coordinator·collect 양쪽이 반드시 이 함수를 쓴다** —
/// 같은 (template, severity, source)면 어느 경로든 같은 지문이 나온다.
pub fn fingerprint(template: &str, severity: &str, source: &str) -> u64 {
    let mut h = Xxh3::new();
    h.write(template.as_bytes());
    h.write(b"|");
    h.write(severity.as_bytes());
    h.write(b"|");
    h.write(source.as_bytes());
    h.finish()
}

/// 로그 한 줄을 정규화·분류·중복묶기 한다. 윈도우가 만료 이벤트를 방출하면 반환.
///
/// - `raw`: 전체 메시지(syslog 헤더 포함 가능). severity·category 판정에 사용.
/// - `source`: 로그 출처(`journald`/`file.syslog`/`file.auth` 등).
/// - `initial_severity`: Vector 1차 분류(push) 또는 파일 재파싱 시 `"info"`(sos).
/// - `extra_fields`: 구조화 필드 보강(push: pid/unit/…, sos: 없음). 메시지에서
///   추출된 필드가 우선하고, 여기 값은 빈 자리만 채운다.
///
/// 윈도우 안 중복이면 병합하고 `None`(fields 추출·분류 생략 — 낭비 방지).
pub fn process_line(
    window: &mut DedupWindow,
    categories: &CategoryMatcher,
    raw: &str,
    source: &str,
    initial_severity: &str,
    ts: DateTime<Utc>,
    extra_fields: &[(&str, String)],
) -> Option<DedupEvent> {
    if raw.is_empty() {
        return None;
    }
    let msg = tokens::strip_syslog_prefix(raw);
    let template = tokens::normalize(msg);
    let severity = severity::finalize(initial_severity, raw);
    let fp = fingerprint(&template, severity, source);

    // 윈도우 안 중복이면 여기서 끝 (fields/category 계산 생략)
    if window.try_merge(fp, raw, ts, severity) {
        return None;
    }

    // 첫 등장(또는 창 만료)에만 fields 추출 + 보강 + 분류
    let mut fields = fields::extract_fields(msg);
    for (k, v) in extra_fields {
        fields
            .entry((*k).to_string())
            .or_insert_with(|| Value::String(v.clone()));
    }
    let category = categories.categorize_with_fields(raw, &fields);

    window.push(
        fp,
        source.to_string(),
        severity.to_string(),
        category.to_string(),
        template,
        raw.to_string(),
        ts,
        fields,
    )
}
