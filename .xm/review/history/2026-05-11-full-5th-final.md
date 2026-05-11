# x-review: full — Request Changes 🔄
- Date: 2026-05-11 (5th review — final)
- Branch: (no git)
- Lenses: security, logic, perf, tests
- Agents: 4
- Findings: 24 (Critical: 0, High: 2, Medium: 12, Low: 10)

---

**Verdict: Request Changes 🔄** — High 2건 (LGTM 기준: 0건 필요)

## High (2)

[High] inbound/collect.rs:982 — collect_logs RFC-3164 이외 형식 라인 조용히 드롭 (logic)
→ Rocky/Alma SOS 로그 섹션 비어있는 원인. ISO-8601/journald 형식 미지원.
→ Fix: None 시 ISO-8601 2차 파싱 또는 fallback + warn!

[High] platform/capability.rs — probe() 테스트 없음 (tests)
→ 시작 게이트, 회귀 시 로그 수집 전체 비활성화.
→ Fix: Probes 구조체 직접 구성 단위 테스트 추가

## Medium (12)

[Medium] transport/http.rs:19 — 아웃바운드 Bearer 토큰 미설정 시 warn만 출력 (security)
[Medium] collect.rs:531 — sysfs 경로 미검증 인터페이스명 삽입 (security)
[Medium] dedup/window.rs:48 — 이벤트 타임스탬프 랙 시 premature expiry (logic)
[Medium] collect.rs:946 — year 경계 parse_syslog_ts 미래 타임스탬프 (logic)
[Medium] coordinator/mod.rs:200 — backoff jitter 없음 thundering herd (logic)
[Medium] transport/spool.rs:56 — commit() TOCTOU metadata read (logic + perf)
[Medium] collect.rs:448 — collect_connections/sockstat spawn_blocking 없음 (perf)
[Medium] collect.rs:46 — collect_metrics() sync reads spawn_blocking 없음 (perf)
[Medium] dedup/window.rs:87 — flush_expired O(N) 전체 스캔 (perf)
[Medium] normalize/tokens.rs:58 — 7 regex into_owned() 불필요 할당 (perf)
[Medium] coordinator/mod.rs:218 — RateLimited 브랜치 테스트 없음 (tests)
[Medium] transport/http.rs:154 — send_429 retry_after 값 미검증 (tests)

## Low (10)

[Low] collect.rs:722 — /etc/hosts envelope 포함 (security)
[Low] collect.rs:549 — systemctl/lspci 절대경로 없이 호출 (security)
[Low] collect.rs:663 — chronyc/ntpq 절대경로 없이 호출 (security)
[Low] coordinator/mod.rs:70 — eviction log % 100 == 1 off-by-one (logic)
[Low] collect.rs:987 — SOS 로그 severity "info" 하드코딩 (logic)
[Low] pipeline/vector_spawn.rs:63 — crash_times 무한 증가 (logic)
[Low] dedup/window.rs:103 — flush_all() 두 패스 수집 (perf)
[Low] transport/spool.rs:56 — commit() 2 syscall (perf)
[Low] collect.rs:979 — BufReader::lines() 라인당 String 할당 (perf)
[Low] dedup/window.rs:169 — flush_expired 테스트 window=0만 검증 (tests)

## Summary

| Lens | Findings | Critical | High | Medium | Low |
|------|----------|----------|------|--------|-----|
| security | 5 | 0 | 0 | 2 | 3 |
| logic | 8 | 0 | 1 | 4 | 3 |
| perf | 7 | 0 | 0 | 4 | 3 |
| tests | 4 | 0 | 1 | 2 | 1 |
| Total | 24 | 0 | 2 | 12 | 10 |

CoVe-removed: logic [High] "break 'outer 후 flush_all 미호출" — flush_all은 루프 종료 후 정상 실행됨

## High 추이
1회: 9 → 2회: 7 → 3회: 5 → 4회: 4 → 5회: 2 (Block→Block→Block→Block→Request Changes)
