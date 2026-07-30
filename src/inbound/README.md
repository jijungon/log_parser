# inbound

[← src](../README.md) · 관련: [transport](../transport/README.md) · [coordinator](../coordinator/README.md)

수신측 서버가 에이전트를 호출하는 Pull API 서버 (기본 포트 9100).

| 파일 | 역할 |
|------|------|
| `mod.rs` | axum 라우터, Bearer 인증(상수 시간 비교), gzip 응답 생성, `InboundState` 정의 |
| `stat.rs` | `GET /stat` — 현재 시스템 상태 수집 후 `stat_snapshot` envelope 반환 |
| `sos.rs` | `POST /trigger-sos` — 풀 진단 스냅샷 수집 후 `sos_snapshot` envelope 반환 |
| `flush.rs` | `POST /flush` — 현재 cycle 즉시 방출, rate limiter (기본 6회/시간) |
| `raw.rs` | `GET /raw` — dedup을 거치지 않은 원문 로그 bounded 조회 (아래 상세) |
| `drain.rs` | `POST /drain-spool` / `GET /drain-status` — HTTP 얇은 층. 실행 코어·상태(`DrainState`)는 [transport/drain.rs](../transport/README.md)로 이동, 기존 경로 호환용 re-export 유지 |
| `collect.rs` | stat·sos 공통 시스템 정보 수집 로직. `collect_logs()`는 RFC-3164 및 ISO-8601 형식 모두 지원(Rocky/Alma 호환), 동기 I/O는 `spawn_blocking`으로 처리 |

외부에서 접근하려면 `agent.yaml`의 `inbound:` 섹션에 `token_env`(flush/drain), `stat_token_env`, `sos_token_env`(sos/raw)를 설정해야 합니다. 세 토큰 중 하나라도 미설정 시 에이전트 기동이 거부됩니다. stat·sos·raw는 collection rate limit을 공유합니다(flush의 10배, 최소 60회/시간).

---

## GET /raw

요약(`/flush`·`/trigger-sos`)으로 개요를 본 뒤 상세 대처가 필요할 때, 최근 원문 로그를 on-demand로 당깁니다. **bounded**: `since`(기본 1h, 최대 24h) ~ `until`(선택, 창의 최근쪽 상한) 창 + `max_mb`(기본 10, 하드캡 30) 상한 — 초과 시 라인 경계에서 자르고 `X-Raw-Truncated: true` 헤더로 표시.

```
GET :9100/raw?since=2h&until=1h&sources=syslog,auth,kernel,journald&max_mb=10
Authorization: Bearer <SOS_INBOUND_TOKEN>      # 새 토큰 없이 sos 토큰 재사용
```

- `since`/`until`은 `30s`/`15m`/`2h`/`1d` 형식. chrono `try_*` 파싱이라 오버플로 값도 panic 없이 무시 — since는 기본 1h 창으로 폴백, until은 상한 없음(now까지)으로 처리.
- 파일 소스는 로그로테이션(`.1`, `.N.gz`)을 **오래된 것부터** 이어 읽어 창 안의 회전된 분까지 포함 (mtime이 창 밖이면 파일 통째 스킵).
- journald는 `journalctl --since [--until] --merge` — `--merge` 덕에 도커 파서도 마운트된 호스트 저널을 machine-id와 무관하게 읽는다. journalctl 부재/실패 시 해당 소스만 조용히 생략.
- 응답: gzip `text/plain` + `X-Raw-Bytes`/`X-Raw-Lines`/`X-Raw-Window` 헤더.

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
- 중복 drain 방지: `in_progress` AtomicBool CAS 가드 — coordinator의 **자동 drain과 공유**되어 동시에 drain은 항상 1개 (HTTP끼리·자동 drain 진행 중이면 409, 반대로 HTTP 진행 중이면 자동 drain이 조용히 생략)
- 인증 토큰: `flush_token`과 동일 (`FLUSH_INBOUND_TOKEN` 환경변수)

## GET /drain-status

현재 drain 진행 상황 또는 마지막 drain 결과를 조회합니다. 자동 drain의 진행·결과도 같은 상태로 조회됩니다.

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
