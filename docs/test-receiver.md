# Phase B 검증용 Test Receiver

> **목적**: Phase B 구현 검증 전용. 운영 수신측(receiver-contract.md)과 무관한 별도 스펙.
> 단순하게 만들고 빠르게 쓴다. 품질·확장성·운영 요건 없음.
>
> **⚠ 이 문서는 Phase B 당시 스펙(historical)이다.** 실제로 구동 가능한 더미 수신 서버는
> [`../test_server/`](../test_server/) (FastAPI — `POST /ingest` 수신 + `/pull/stat`·`/pull/sos` 호출 + `/envelopes`·`/logs`·`/hosts`·`/compare` 조회)로 구현돼 있으며,
> **멱등성이 없어**(재전송 시 그대로 중복 저장) 운영 수신측 참조로는 부적합하다. 멱등 패턴은 [`../examples/receiver_example.py`](../examples/receiver_example.py) 참조.
> 아래 본문 중 설계 당시와 달라진 사실(포트·`/flush` 응답 형식)은 현행 기준으로 정정해 두었다 — pull API는 **전부 단일 포트 :9100**이다.

---

## 1. 이 서비스가 하는 것

```
[ log_parser 에이전트 (:9100) ]     [ test-receiver ]

  outbound push (30분)   ──────→   POST /ingest     (envelope 수신 + 저장)
  POST :9100/flush       ←──────   receiver가 호출 (응답 바디로 envelope 반환)
  GET  :9100/stat        ←──────   receiver가 호출
  POST :9100/trigger-sos ←──────   receiver가 호출
```

**두 가지 역할:**
1. **Server**: log_parser의 outbound push를 받는 ingest endpoint
2. **Client**: flush / stat / sos 호출을 직접 실행하는 CLI

---

## 2. 기술 스택

| 항목 | 선택 | 이유 |
|---|---|---|
| 언어 | Python 3.10+ | 빠른 작성. 의존성 최소화 |
| HTTP server | `http.server` (stdlib) 또는 `flask` | 외부 의존성 1개 이하 |
| HTTP client | `requests` | 간단 |
| 저장 | 로컬 JSONL 파일 (`received.jsonl`) | DB 불필요 |
| 인증 | env var로 토큰 주입 | 하드코딩 금지, 운영 수준 불필요 |

> `flask`가 없으면 `pip install flask requests` 한 줄로 해결.

---

## 3. 파일 구조

```
test_receiver/
├── receiver.py      ← HTTP server (POST /ingest)
├── trigger.py       ← CLI: flush / stat / sos 호출
├── received.jsonl   ← 수신된 envelope 저장 (append, 실행마다 누적)
└── README.md        ← 실행 방법 2줄
```

---

## 4. Server — `receiver.py`

### 4.1 기동

```bash
python receiver.py --port 8080 --token test-token
```

환경변수로도 주입 가능:
```bash
RECV_PORT=8080 RECV_TOKEN=test-token python receiver.py
```

### 4.2 endpoint

**`POST /ingest`**

```
Authorization: Bearer <token>
Content-Type: application/json
Content-Encoding: gzip    ← 있으면 자동 해제, 없으면 raw JSON 그대로 수락
Body: <Envelope JSON>
```

응답:
- `200 OK` + JSON: `{"ok": true, "seq": 42}` (저장된 envelope의 순번)
- `401 Unauthorized`: 토큰 불일치
- `400 Bad Request`: JSON 파싱 실패

동작:
1. gzip 해제 — Python HTTP 서버는 request body를 자동으로 해제하지 않으므로 수동 처리 필요
   (`Content-Encoding: gzip` 헤더 확인 후 `gzip.decompress()`)
2. JSON 파싱
3. `received.jsonl`에 1줄 append (수신 시각 + envelope 전체)
4. 콘솔에 요약 출력 (아래 §5 형식)

> **gzip 처리 정리**
> - receiver.py (서버): log_parser push의 request body gzip → **수동 해제 필요** (HTTP 서버는 자동 처리 안 함)
> - trigger.py stat/sos (클라이언트): response body gzip → `requests`가 **자동 해제** (`response.json()` 바로 호출)
> - trigger.py flush (클라이언트): **응답도 gzip envelope** — `/flush`는 현재 cycle의 `log_batch` envelope을 HTTP 응답 바디로 직접 반환한다 (stat/sos와 동일하게 `requests`가 자동 해제). `{"ok": true}` 류의 확인 응답이 아니다

**`GET /health`** → `200 OK {"ok": true}`

### 4.3 콘솔 출력 형식

```
[10:30:08] ▶ log_batch  host=db-prod-03  seq=42  window=10:00~10:30
           critical=3  error=8  warn=14  info=312
           categories: kernel.oom×1  disk.io_error×1  auth.bruteforce×1

[10:30:09] ▶ stat_snapshot  host=db-prod-03  sections=7  duration=43ms

[10:30:11] ▶ sos_snapshot  host=db-prod-03  sections=8  duration=14320ms
```

`event_kind`별로 포맷 분기. body 상세는 저장 파일에서 확인.

---

## 5. CLI — `trigger.py`

### 5.1 기동

```bash
# 환경변수로 에이전트 주소 + 토큰 설정
export AGENT_HOST=127.0.0.1
export FLUSH_TOKEN=my-flush-token
export STAT_TOKEN=my-stat-token
export SOS_TOKEN=my-sos-token
```

### 5.2 명령

```bash
# log_parser에 flush 요청 (현재 cycle 즉시 방출 — 디버그용)
python trigger.py flush
# → POST http://$AGENT_HOST:9100/flush
# → 응답: 현재 cycle의 log_batch envelope (gzip JSON, 응답 바디로 직접 반환)
# → ⚠ /ingest로 push되지 않는다 — flush된 cycle은 이 응답으로만 나가며 seq만 1 증가
#   (수신 저장소에 그 seq의 구멍이 생긴다. pull-api.md 참조)

# stat_report 조회
python trigger.py stat
# → GET http://$AGENT_HOST:9100/stat
# → 응답 Content-Encoding: gzip → requests 라이브러리가 자동 해제 → response.json() 바로 호출
# → 응답 envelope 콘솔 출력 + received.jsonl에 저장

# sos_report 전체 포렌식 (수집 완료까지 대기)
python trigger.py sos
# → POST http://$AGENT_HOST:9100/trigger-sos
# → 수집 완료까지 blocking (타임아웃 120초)
# → 응답 Content-Encoding: gzip → requests 라이브러리가 자동 해제 → response.json() 바로 호출
# → 응답 envelope 콘솔 출력 + received.jsonl에 저장

# 수신 목록 보기
python trigger.py list
# → received.jsonl 파싱해서 요약 출력

# 특정 envelope 상세 보기 (list 결과의 #번호)
python trigger.py show 3
```

### 5.3 list 출력 예시

```
#  ts                   kind           host          seq   critical  error  warn
── ──────────────────── ────────────── ───────────── ───── ──────── ───── ────
1  2026-05-08T10:00:08  log_batch      db-prod-03    41    0        2     5
2  2026-05-08T10:30:08  log_batch      db-prod-03    42    3        8     14
3  2026-05-08T10:30:09  stat_snapshot  db-prod-03    —     —        —     —
4  2026-05-08T10:30:11  sos_snapshot   db-prod-03    —     —        —     —
```

---

## 6. agent.yaml 연동 — log_parser 설정

log_parser의 `agent.yaml`에서 test receiver를 outbound endpoint로 지정:

```yaml
transport:
  kind: "http_json"                          # 현재 구현된 유일한 transport
  endpoint: "http://127.0.0.1:8080/ingest"  # test_receiver 주소
  token_env: "PUSH_OUTBOUND_TOKEN"
  # gzip은 항상 적용된다 (끄는 키 없음). 압축 레벨만 조절 가능: http_gzip_level (기본 6)
```

test_receiver 기동 시 토큰 맞춤:
```bash
RECV_TOKEN=$PUSH_OUTBOUND_TOKEN python receiver.py --port 8080
```

---

## 7. Phase B 검증 시나리오별 사용법

| Phase B 완료 정의 항목 | test_receiver 사용법 |
|---|---|
| envelope 정상 수신 (Step 2) | `receiver.py` 기동 후 log_parser 실행 → 콘솔 출력 확인 |
| 30분 cycle envelope 48건+ | `trigger.py list` → 24시간 후 count 확인 |
| empty cycle invariant | 조용한 호스트에서 `trigger.py list` — 이벤트 0건 envelope 도착 확인 |
| POST /flush 즉시 응답 | `trigger.py flush` → 응답 시간 + 새 seq 증가 확인 |
| cycle.seq 단조 증가 | `trigger.py list` → seq 열이 연속 증가인지 확인 |
| vector_restarts_24h 증가 | Vector 강제 kill 후 `trigger.py list` → process_health 확인 |
| 호스트 재부팅 후 seq 복구 | 재부팅 전후 `trigger.py list` → seq 끊김 없이 증가 확인 |
| deep-pull 시나리오 확인 | `trigger.py flush && trigger.py stat && trigger.py sos` 순차 실행 |

---

## 8. 비요건 (일부러 뺀 것)

- 인증 강화 (mTLS, rate-limit, IP 필터) — 불필요
- 데이터 영속성 (DB, S3) — JSONL 파일로 충분
- UI (웹 대시보드) — 콘솔 출력 + `trigger.py list`로 충분
- 고가용성, 재시작 복구 — 단순 프로세스
- Docker/systemd 패키징 — 로컬 `python` 실행
- schema 검증 (JSON Schema) — 콘솔 출력으로 육안 확인

> 이 서비스는 Phase B가 끝나면 버린다.
