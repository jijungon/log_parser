# dedup

[← src](../README.md) · 관련: [normalize](../normalize/README.md) · [coordinator](../coordinator/README.md)

슬라이딩 윈도우 기반 중복 제거 모듈.

같은 `fingerprint`(template · severity · source를 `|`로 이어 스트리밍 Xxh3 해시 — [process.rs](../process.rs)가 유일한 정의처)를 가진 이벤트를 윈도우 내에서 하나로 묶어 `count`를 누적합니다.

| 파일 | 역할 |
|------|------|
| `window.rs` | `DedupWindow` — 병합(`try_merge`)·삽입(`push`)·만료 방출(`flush_expired`/`flush_all`) 구현 |

## 핵심 동작 / 불변식

- **try_merge fast-path**: 윈도우 안 중복이면 count·샘플만 병합하고 끝 — fields 추출·category 분류는 첫 등장(또는 창 만료)에만 수행. 병합 중 critical 이벤트가 오면 그룹 severity를 critical로 승격. 순서 역전 이벤트는 `ts_last`를 뒤로 당기지 않음.
- **샘플 상한**: 그룹당 원본 로그 샘플은 `dedup.sample_raws_cap`개까지 보관 — envelope 크기의 지배 요인이라 설정으로 노출. `0`이면 샘플 미보관(template·count만 전송, count 집계는 유지).
- **LRU 축출 = 조기 방출, 유실 0**: `lru_cap` 초과로 축출된 그룹은 폐기되지 않고 `evicted_pending` 버퍼로 이동 → 다음 `flush_expired`/`flush_all`(coordinator의 5초 dedup_tick)에서 방출된다. 버퍼 크기는 tick 사이 축출량으로 자연 유계.
- **total_evictions 카운터**: 조기 방출된 고유 패턴 수(폐기 아님). 평상시 0 — >0이면 카디널리티가 cap을 압박한다는 신호 (100건마다 warn 로그).

## 관련 설정 키 (`dedup.`)

| 키 | 기본값 | 의미 |
|----|-------|------|
| `window_seconds` | 30 | 병합 윈도우(초) |
| `lru_cap` | 50000 | 동시 추적 고유 패턴 수 (0이면 기동 거부) |
| `sample_raws_cap` | 3 | 그룹당 원본 샘플 수 (0 = 미보관) |
