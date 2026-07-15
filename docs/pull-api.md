# On-demand Pull API — 상세

에이전트가 `9100` 포트에서 제공하는 pull 엔드포인트의 호출 예시·파라미터·에러 코드. (요약은 루트 [`README.md`](../README.md#on-demand-pull-api))

기본 바인드는 `127.0.0.1:9100`(로컬호스트 전용). 원격 호출은 `agent.yaml`의 `inbound:` 설정 + 방화벽:

```yaml
inbound:
  listen_addr: "0.0.0.0:9100"
  stat_token_env:  "STAT_INBOUND_TOKEN"    # 미설정 시 인증 없이 접근 가능
  sos_token_env:   "SOS_INBOUND_TOKEN"     # 미설정 시 인증 없이 접근 가능
  token_env:       "FLUSH_INBOUND_TOKEN"   # flush/drain 공용 토큰 환경변수명
```

---

## GET /stat — 현재 시스템 상태

```bash
curl -s http://agent-host:9100/stat \
  -H "Authorization: Bearer ${STAT_INBOUND_TOKEN}" \
  --compressed | python3 -m json.tool
```

응답: `stat_snapshot` envelope

## POST /trigger-sos — SOS 풀 진단

```bash
curl -s -X POST http://agent-host:9100/trigger-sos \
  -H "Authorization: Bearer ${SOS_INBOUND_TOKEN}" \
  --compressed | python3 -m json.tool
```

응답: `sos_snapshot` envelope (최근 4시간 로그 포함, 타임아웃 120초 이상 권장)

## POST /flush — 현재 cycle 즉시 방출 (디버그용)

> **주의**: `/flush`는 현재 cycle의 envelope을 HTTP **응답 바디**로 직접 반환합니다.
> 수신측 `/ingest`로 전송되는 경로가 **아닙니다**. 호출 시 현재 cycle이 즉시 종료되고 seq가 1 증가합니다.

```bash
curl -s -X POST http://agent-host:9100/flush \
  -H "Authorization: Bearer ${FLUSH_INBOUND_TOKEN}" \
  --compressed | python3 -m json.tool
```

## POST /drain-spool — retry/ 파일 재전송

`retry/`에 쌓인 전송 실패 envelope을 지정한 시간 창 내에서 재전송합니다.

```bash
# 특정 30분 창의 파일 재전송
curl -s -X POST \
  "http://agent-host:9100/drain-spool?from=2026-05-01T00:00:00Z&to=2026-05-01T00:30:00Z" \
  -H "Authorization: Bearer ${FLUSH_INBOUND_TOKEN}"
```

응답 (202 Accepted):

```json
{ "drain_id": "01JXYZ...", "window": {...}, "queued": 47, "bytes": 450000 }
```

- 전송은 백그라운드에서 진행되므로 즉시 `202` 반환
- `409`: 이미 drain 진행 중 — `drain_id`·`remaining`·`started_at`·`window` 반환
- `400`: from/to 파싱 실패 또는 `from >= to`
- `from`/`to`: RFC3339 형식, ULID 생성 시각 기준 필터링

## GET /drain-status — drain 진행 상황 조회

```bash
curl -s http://agent-host:9100/drain-status \
  -H "Authorization: Bearer ${FLUSH_INBOUND_TOKEN}"
```

응답:

```json
{
  "drain_id": "01JXYZ...", "status": "in_progress",
  "window": {"from": "2026-05-01T00:00:00Z", "to": "2026-05-01T00:30:00Z"},
  "queued": 47, "remaining": 23, "succeeded": 20, "failed": 4,
  "started_at": "2026-05-11T09:00:00Z", "completed_at": null,
  "spool_new_bytes": 102400, "spool_retry_count": 12
}
```

| `status`      | 의미           |
| ------------- | ------------ |
| `idle`        | drain 이력 없음  |
| `in_progress` | drain 진행 중   |
| `completed`   | 마지막 drain 완료 |

---

## 에러 응답 코드 (에이전트 → 수신측 pull 응답)

수신측이 pull API(`/stat`·`/flush`·`/trigger-sos`·`/drain-spool`)를 호출했을 때 **에이전트가 돌려주는** 코드. (반대 방향인 push 응답 코드는 루트 README "수신 엔드포인트 구현 요건" 참조)

| 코드    | 의미                                                                                                      |
| ----- | ------------------------------------------------------------------------------------------------------- |
| `200` | 성공 (body: gzip JSON)                                                                                    |
| `202` | drain 작업 시작됨                                                                                            |
| `400` | from/to 파라미터 파싱 실패 또는 `from >= to`                                                                      |
| `401` | 토큰 인증 실패                                                                                                |
| `409` | 이미 처리 중 (flush 또는 drain 중복 호출)                                                                          |
| `413` | envelope 크기 초과 (`envelope_size_limit_mb` 설정, JSON 직렬화 기준)                                               |
| `429` | **에이전트 측 Rate limit** (`/flush`: 기본 6회/시간, `/stat`·`/trigger-sos`: 기본 60회/시간, `rate_limit_per_hour` 설정) |
| `503` | `/flush` — wait 모드 타임아웃 또는 coordinator 채널 종료 시                                                          |
