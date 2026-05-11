# inbound

수신측 서버가 에이전트를 호출하는 Pull API 서버 (기본 포트 9100).

| 파일 | 역할 |
|------|------|
| `mod.rs` | axum 라우터, Bearer 인증 유틸, gzip 응답 생성, `InboundState` 정의 |
| `stat.rs` | `GET /stat` — 현재 시스템 상태 수집 후 `stat_snapshot` envelope 반환 |
| `sos.rs` | `POST /trigger-sos` — 풀 진단 스냅샷 수집 후 `sos_snapshot` envelope 반환 |
| `flush.rs` | `POST /flush` — 현재 cycle 즉시 방출, rate limiter (기본 6회/시간) |
| `drain.rs` | `POST /drain-spool` / `GET /drain-status` — retry/ 재전송 (아래 상세) |
| `collect.rs` | stat·sos 공통 시스템 정보 수집 로직. `collect_logs()`는 RFC-3164 및 ISO-8601 형식 모두 지원(Rocky/Alma 호환), 동기 I/O는 `spawn_blocking`으로 처리 |

외부에서 접근하려면 `agent.yaml`의 `inbound:` 섹션에 `token_env`(flush/drain), `stat_token_env`, `sos_token_env`를 설정해야 합니다. 세 토큰 모두 미설정 시 에이전트 기동이 거부됩니다.

---

## POST /drain-spool

retry/ 에 쌓인 전송 실패 envelope을 지정 시간 창 단위로 재전송합니다.

```
POST :9100/drain-spool?from=2026-05-01T00:00:00Z&to=2026-05-01T00:30:00Z
Authorization: Bearer <FLUSH_INBOUND_TOKEN>
```

**응답:**

| 코드 | 의미 |
|------|------|
| `202` | drain 작업 시작 — `drain_id`·`queued`·`bytes` 반환 |
| `409` | 이미 drain 진행 중 — `drain_id`·`remaining`·`started_at`·`window` 반환 |
| `400` | from/to 파라미터 파싱 실패 또는 `from >= to` |
| `401` | 인증 실패 |

```json
// 202 응답 예시
{
  "drain_id": "01JXYZ...",
  "window": { "from": "2026-05-01T00:00:00Z", "to": "2026-05-01T00:30:00Z" },
  "queued": 47,
  "bytes": 450000
}
```

- `from`/`to`는 **RFC3339** 형식
- `from`은 포함(inclusive), `to`는 미포함(exclusive) — ULID 생성 시각 기준
- 전송 실패 파일은 retry/에 유지 (다음 drain 시 재시도 가능)
- 중복 drain 방지: `in_progress` AtomicBool + `compare_exchange`로 동시 실행 차단 (409 반환)
- 인증 토큰: `flush_token`과 동일 (`FLUSH_INBOUND_TOKEN` 환경변수)

## GET /drain-status

현재 drain 진행 상황 또는 마지막 drain 결과를 조회합니다.

```
GET :9100/drain-status
Authorization: Bearer <FLUSH_INBOUND_TOKEN>
```

```json
{
  "drain_id": "01JXYZ...",
  "status": "in_progress",
  "window": { "from": "2026-05-01T00:00:00Z", "to": "2026-05-01T00:30:00Z" },
  "queued": 47,
  "remaining": 23,
  "succeeded": 20,
  "failed": 4,
  "started_at": "2026-05-11T09:00:00Z",
  "completed_at": null,
  "spool_new_bytes": 102400,
  "spool_retry_count": 12
}
```

| `status` 값 | 의미 |
|-------------|------|
| `idle` | drain 이력 없음 |
| `in_progress` | drain 진행 중 |
| `completed` | 마지막 drain 완료 (결과 조회용) |
