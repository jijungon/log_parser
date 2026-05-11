# test-server

log_parser Phase B 검증용 수신 서버.

## 시작

```bash
cp .env.example .env
# .env 편집 후
docker compose up -d
```

## 저장 구조

```
storage/
├── ingest.jsonl    ← log_parser push (/ingest) — flush 트리거 후 도착분 포함
├── stat.jsonl      ← /pull/stat 응답
└── sos.jsonl       ← /pull/sos 응답
```

## 엔드포인트

| 방향 | 메서드 | 경로 | 설명 |
|---|---|---|---|
| 수신 | POST | `/ingest` | log_parser push 수신 → ingest.jsonl |
| 수신 | GET | `/health` | 헬스 체크 |
| pull | GET | `/pull/stat` | stat_report /stat 호출 → stat.jsonl |
| pull | POST | `/pull/sos` | sos_report /trigger-sos 호출 → sos.jsonl |
| 조회 | GET | `/envelopes` | 전체 수신 목록 |
| 조회 | GET | `/envelopes?source=ingest` | 소스별 필터 |
| 조회 | GET | `/envelopes/{source}/{n}` | 특정 envelope 상세 |
| 조회 | GET | `/analyze` | docs/*.example 비교 |

## agent.yaml 설정

```yaml
transport:
  kind: "http_json"
  endpoint: "http://<test-server-host>:8080/ingest"
  token_env: "PUSH_OUTBOUND_TOKEN"
  gzip: true
```

`PUSH_OUTBOUND_TOKEN` = `.env`의 `RECV_TOKEN` 값과 동일하게.
