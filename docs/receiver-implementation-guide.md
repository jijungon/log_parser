# 수신 서비스 구현 가이드 (인수인계 중심 문서)

> **대상**: log_parser가 보내는 데이터를 받는 서비스(수신측)를 **직접 구현할 사람**.
> 이 문서 하나로 "무엇을 받고, 어떤 응답을 돌려주고, 어떤 의미론을 지켜야 하는지"를 파악한 뒤,
> 세부 스키마는 [receiver-type-spec.md](receiver-type-spec.md), 계약 요약은 [receiver-contract.md](receiver-contract.md), pull 호출법은 [pull-api.md](pull-api.md)로 내려간다.
>
> 본 문서의 모든 필드명·응답 코드·수치는 소스(`src/envelope.rs`, `src/transport/http.rs`, `src/coordinator/mod.rs`, `src/transport/drain.rs`, `src/transport/spool.rs`, `src/inbound/`)와 `config/` 기본값에서 직접 확인한 값이다 (2026-07-30, c28ce46 기준).

---

## 1. 무엇을 받는가 — envelope 3종, 구현할 것은 1개

파서가 만드는 envelope은 세 종류지만, **수신측이 "서버로서 구현"해야 하는 것은 push를 받는 ingest 엔드포인트 하나뿐이다.** 나머지 둘은 수신측이 파서를 **호출(pull)** 했을 때의 HTTP 응답 바디로만 존재한다.

| event_kind | 전달 방향 | 수신측이 할 일 |
|---|---|---|
| `log_batch` | **push** — 파서가 30분(기본 `cycle.window_seconds: 1800`)마다 `transport.endpoint`로 POST | **ingest 엔드포인트 구현 (필수)** |
| `stat_snapshot` | **pull** — 수신측이 `GET :9100/stat` 호출 시 응답 바디 | HTTP 클라이언트 (선택) |
| `sos_snapshot` | **pull** — 수신측이 `POST :9100/trigger-sos` 호출 시 응답 바디 | HTTP 클라이언트 (선택) |

- push 목적지는 파서 설정 `transport.endpoint` **단일 URL**이다 (관례상 `.../ingest`, 예: `agent_docker.yaml`의 `http://host.docker.internal:8080/ingest`). 라이브 전송·재기동 replay·drain 재전송 **모두 이 하나의 URL로만** 온다 — 두 번째 push 엔드포인트는 없다 (`src/transport/http.rs`가 `cfg.endpoint` 하나만 사용).
- `POST :9100/flush`(디버그용)의 응답 바디도 `log_batch` envelope이지만, 이것은 **pull 응답**이며 ingest로 오지 않는다 (§3.4 seq 구멍 참조).
- envelope 3종의 공통 구조(`event_kind`/`cycle`/`headers`/`body`)와 섹션별 타입은 [receiver-type-spec.md](receiver-type-spec.md)가 정본. 실물 페이로드는 [`../examples/envelope-response.00.example`](../examples/envelope-response.00.example) 등 참조.

---

## 2. POST /ingest 구현 요건

### 2.1 요청 형식 (파서 → 수신측)

```
POST <transport.endpoint>
Authorization: Bearer <PUSH_OUTBOUND_TOKEN 값>
Content-Type: application/json
Content-Encoding: gzip
Body: gzip 압축된 Envelope JSON
```

- **본문은 항상 gzip**이다 (`Content-Encoding: gzip` 고정, 압축 레벨 기본 6 = `transport.http_gzip_level`). 협상 없이 무조건 압축해 보내므로 수신측은 반드시 해제 후 파싱해야 한다.
- **Bearer 토큰**: 파서가 환경변수(기본 이름 `PUSH_OUTBOUND_TOKEN`, `transport.token_env`로 변경 가능)에서 읽은 값을 그대로 실어 보낸다. 미설정 시 파서가 기동 자체를 거부하므로, 토큰 없는 요청은 파서가 보낸 것이 아니다. 검증은 상수 시간 비교 권장 (파서 자신의 inbound도 `subtle::ConstantTimeEq` 사용).
- **타임아웃**: 파서의 요청 타임아웃은 기본 **30초**(`transport.request_timeout_seconds`)다. 이 안에 응답하지 못하면 네트워크 오류(=재시도 대상)로 분류되어 **같은 envelope이 다시 온다**. 무거운 처리는 뒤로 미루고 저장 확정 즉시 2xx를 돌려줄 것.

### 2.2 응답 코드 계약 (수신측 → 파서)

파서의 응답 분류는 `src/transport/http.rs::send_compressed` 그대로다:

| 수신측 응답 | 파서 분류 | 파서 동작 |
|---|---|---|
| **2xx 전부** (`is_success`) | 성공 | spool `new/` 파일 삭제(commit) + retry/ 자동 drain 트리거 (§3.3) |
| **429** | RateLimited | `Retry-After` 헤더(**정수 초만** 파싱, 없거나 형식 불일치 시 60초) 만큼 대기 후 재시도. **429도 재시도 한도를 소모**하므로 남용하면 envelope이 `retry/`로 파킹된다 |
| **5xx** (`is_server_error`) | Retryable | 지수 백오프 재시도 (§3.3) |
| **그 외 전부** (3xx 포함 4xx: 400/401/403/404/413…) | Fatal | **즉시 포기** — 해당 envelope을 `retry/`로 파킹 (재시도 없음, drain으로만 재전송) |
| (연결 실패·타임아웃 등 전송 오류) | Retryable | 지수 백오프 재시도 |

- "수신 성공"은 **모든 2xx**다 (200/202/204 등 어느 것이든 가능).
- 2xx/429/5xx 이외는 전부 Fatal이므로 **리다이렉트(3xx)로 응답하지 말 것**.
- 4xx를 돌려주면 그 cycle은 자동으로 다시 오지 않는다 — 인증 설정 오류 등으로 4xx가 지속되면 데이터가 `retry/`에 쌓이다가 TTL(기본 168h) 후 삭제된다. **일시적 문제에는 5xx나 429를, 영구적으로 처리 불가한 요청에만 4xx를** 쓸 것.

### 2.3 크기 관련 설정 (수신측 body limit 산정 근거)

push되는 envelope 크기를 결정하는 파서 쪽 knob (전부 `config/agent.yaml`):

| 설정 | 기본값 | 의미 |
|---|---|---|
| `pipeline.body_max_events_per_cycle` | 100000 | cycle당 DedupEvent 상한 (초과 시 오래된 non-critical부터 축출) |
| `dedup.sample_raws_cap` | 3 | 이벤트(지문)당 원본 로그 샘플 수 — envelope 크기의 지배 요인 |
| `pipeline.body_max_size_mb` | 50 | 직렬화 JSON이 이를 넘으면 파서가 **warn 로그만 남기고 그대로 전송** (차단 아님) |

즉 50MB는 하드캡이 아니다. 수신측 요청 바디 한도는 **비압축 기준 `body_max_size_mb` 이상**으로 넉넉히 잡을 것 (gzip 후 전송 크기는 훨씬 작다). 참고로 `inbound.envelope_size_limit_mb`(기본 10)는 **pull 응답**(/stat 등)의 413 한도이며 push와 무관하다.

---

## 3. 신뢰성 계약 — 구현자가 반드시 알아야 할 의미론

### 3.1 at-least-once — 멱등(dedup) 처리는 수신측 책임

파서는 전송 전 envelope을 디스크 WAL(spool `new/`)에 먼저 쓰고, 성공(2xx)해야 지운다. 따라서:

- 수신측이 처리를 끝냈어도 **2xx 응답이 유실**되면(타임아웃 포함) 파서는 실패로 간주하고 **cycle 통째로 재전송**한다.
- 파서 재기동 시 `new/`에 남은(=commit 안 된) envelope을 전부 재전송한다(startup replay, 동시 4개).

**중복 제거 키는 `(cycle.host_id, cycle.boot_id, cycle.seq)`** — 셋이 모두 같으면 같은 데이터로 보고 upsert 한 번만 처리한다.

- `host_id` = `/etc/machine-id` 기반 (또는 `cycle.host_id_override`) — 재설치 전까지 불변.
- `boot_id` = 재부팅마다 변경 — 재부팅 전후 seq가 겹쳐도 별개 데이터.
- `seq` = cycle마다 1씩 증가. `/var/lib/log_parser/seq.state`에 영속화되어 **에이전트 재시작 후에도 이어진다** (상태 파일 유실 시에만 1로 리셋 — 이때도 boot 내 재사용 가능성이 있으므로 키 충돌 시 upsert가 안전).
- `stat_snapshot`/`sos_snapshot`은 **seq가 없다**(JSON 키 자체 생략). `(host_id, boot_id, ts)` 보조 키로 upsert 권장 — `ts`는 초 단위 정밀도라 같은 초 재호출 시 충돌할 수 있다. 패턴은 [`../examples/receiver_example.py`](../examples/receiver_example.py)의 `is_duplicate` 참조.

### 3.2 도착 순서 ≠ 발생 순서 — 시간 기준으로 정렬 저장할 것

**seq 오름차순 도착을 가정하면 안 된다.** 코드 구조상 다음이 정상 동작이다:

- 수신측 장애 동안 실패한 envelope은 `retry/`에 파킹된다. 파킹분의 자동 재전송(auto-drain)은 **그 이후의 라이브 전송이 성공한 직후**에 트리거되므로, **최신 seq envelope이 먼저 도착하고 파킹돼 있던 옛 seq envelope이 나중에 도착**하는 것이 기본 순서다 (2026-07-30 장애 훈련에서 실제 관측·확인).
- 재기동 replay(동시 4개)·라이브 전송(동시 4개)도 완료 순서를 보장하지 않는다.

→ 저장은 도착 순서가 아니라 **`cycle.window`(로그 발생 구간) 또는 이벤트의 `ts_first`/`ts_last` 기준으로 정렬·조회**하도록 설계한다. `(host_id, boot_id, seq)` 키로 저장하면 도착 순서는 자연히 무의미해진다.

### 3.3 파서의 재시도·파킹·자동 drain (수신측 관점 요약)

파서 내부 동작 (`src/coordinator/mod.rs::send_with_backoff`, `src/transport/drain.rs`):

1. **재시도**: 5xx·네트워크 오류 시 지수 백오프 — 기본 5s에서 2배씩, 최대 300s 간격 (`transport.retry_base_seconds`/`retry_max_seconds`). 일반 envelope은 최초 전송 포함 최대 **6회**(`retry_max_normal: 5`), `critical` 포함 envelope은 재시도 한도 **2배**(최대 11회).
2. **파킹**: 한도 소진 또는 4xx(Fatal) 시 envelope 파일을 `retry/`(데드레터)로 이동. 디스크에 있으므로 유실은 아니다.
3. **자동 drain (기본 on, `transport.auto_drain: true`)**: 이후 어느 라이브 전송이든 성공하면(= 수신측 복구 증거) `retry/` 전체를 백그라운드로 재전송한다. 수동으로는 `POST :9100/drain-spool?from&to`(202/409, [pull-api.md](pull-api.md)). 자동·수동은 같은 가드를 공유해 동시에 돌지 않는다(중복 발송 없음).

**수신측 관점 한 줄 요약: 수신 서비스가 죽었다 살아나면, 별도 조치 없이 밀린 envelope이 자동으로 온다.** 단:

- **최대 지연 = `retry/` TTL 기본 168h(7일)** (`transport.retry_ttl_hours`). TTL 또는 용량 상한(`retry_max_mb` 기본 1024MB, `new/`는 `spool_max_mb` 기본 2048MB — 넘치면 oldest가 `retry/`로 밀림) 초과분은 **오래된 것부터 삭제**되어 영구 유실된다. 7일 넘게 다운될 예정이면 그 전에 복구하거나 수동 drain 해야 한다.
- 파서는 그동안에도 계속 수집한다 (전송 태스크 동시 4개 제한으로 파서 자신은 죽지 않음).

### 3.4 seq 구멍 = "배달 중" 신호 (즉시 유실 판정 금지)

수신된 seq에 구멍이 있어도 대부분은 나중에 채워진다. 구멍의 원인 분류:

| 원인 | 채워지는가 |
|---|---|
| `retry/` 파킹 후 아직 drain 안 됨 | **채워짐** — 자동 drain(복구 후) 또는 수동 drain으로, TTL 168h 이내 |
| 파서가 다운된 채 재기동 대기 (`new/` 잔존) | **채워짐** — 재기동 replay |
| `POST /flush` 호출 (디버그) | **안 채워짐** — flush된 cycle은 HTTP 응답 바디로만 나가고 ingest로 오지 않는다. seq만 1 소모 |
| TTL/용량 초과 삭제, corrupt/ 격리, (드묾) 직렬화 실패 | **안 채워짐** — 파서 로그에 warn/error 기록 |

→ 모니터링 권장: 구멍 발견 시 경보가 아니라 **"TTL(168h) 경과 후에도 남은 구멍"만 유실로 확정**한다.

### 3.5 침묵 감시 — 35분 룰

파서는 이벤트가 0건이어도 30분마다 envelope을 보낸다 (빈 cycle은 `body: []`, `headers.total_sections: 0` — 그 자체가 alive 신호). 따라서 **호스트별로 35분(30분 + 유예 5분) 이상 `log_batch`가 없으면** 에이전트 다운·네트워크 단절을 의심하고 점검한다.

---

## 4. 구현 체크리스트

| 구분 | 항목 | 근거·비고 |
|---|---|---|
| **필수** | `Content-Encoding: gzip` 본문 해제 후 JSON 파싱 | 파서는 항상 gzip으로 보냄 (§2.1) |
| **필수** | `Authorization: Bearer` 토큰 검증 (`PUSH_OUTBOUND_TOKEN` 값과 일치, 불일치 시 401) | §2.1 — 단 401은 Fatal→파킹이므로 토큰 로테이션은 파서·수신측 동시에 |
| **필수** | 저장 확정 후 **2xx** 응답 (30s 안에) | 2xx = 파서가 WAL을 지워도 된다는 승인 (§2.2) |
| **필수** | envelope 저장 (원본 보존 권장 — 파서는 저장을 책임지지 않는 설계) | 루트 README "설계 의도 · 책임 경계" |
| **권장** | `(host_id, boot_id, seq)` 멱등 upsert / stat·sos는 `(host_id, boot_id, ts)` upsert | at-least-once 재전송 (§3.1) |
| **권장** | `cycle.window`·`ts_first` 기준 시간 정렬 저장 (도착 순서 의존 금지) | 파킹→drain 재배달로 옛 envelope이 늦게 옴 (§3.2) |
| **권장** | seq 구멍 모니터링 — TTL 168h 경과 후 잔존분만 유실 확정 | §3.4 |
| **권장** | 호스트별 35분 침묵 감시 | 빈 cycle도 오므로 침묵 = 이상 (§3.5) |
| **권장** | 빠른 2xx + 비동기 후처리, 과부하 시 429+`Retry-After`(정수 초) 또는 5xx | §2.1 타임아웃 30s, §2.2 (429 남용은 파킹 유발) |
| **선택** | pull API 클라이언트: `GET /stat` · `POST /trigger-sos` · `GET /raw` · `POST /drain-spool` + `GET /drain-status` · `POST /flush` | 사고 시 상세 수집·회수 ([pull-api.md](pull-api.md), 전부 단일 포트 :9100) |
| **선택** | `fingerprint` 서버 간 상관 — 같은 지문이 여러 host에서 동시 발생 = 인프라 공통 장애 의심 | 지문 = `xxh3(template\|severity\|source)`, 호스트 무관 동일 패턴에 동일 값 |

---

## 5. 참고 구현과 검증법

### 5.1 참고 코드 2개

| 위치 | 성격 | 주의 |
|---|---|---|
| [`../examples/receiver_example.py`](../examples/receiver_example.py) | 최소 구현 참조 (FastAPI) — 인증·gzip 해제·**멱등 패턴(`is_duplicate`)**·필터링 예시 | 저장이 없는 골격. 멱등 키 사용법은 이것을 따를 것 |
| [`../test_server/`](../test_server/) | 실제 구동 가능한 더미 수신 서버 (FastAPI + JSONL 저장, `/envelopes`·`/logs`·`/hosts`·`/compare` 조회 포함) | **멱등성 없음** — 재전송이 오면 그대로 중복 저장된다. 검증용이지 운영 참조가 아님 |

### 5.2 손으로 확인하는 법 — 소방훈련 3종

파서를 옆에 띄워 두고 (Docker: 루트 README "빠른 시작") 아래 세 가지를 직접 재현하면 §2~3의 계약이 몸에 붙는다. 셋 다 실제 코드 동작으로 검증된 절차다.

1. **우체국 폐쇄 (수신 다운 → 자동 회수)** — 수신 서버를 내리면 파서 로그에 백오프 재시도 → 한도 소진 → `retry/` 파킹이 찍힌다. 수신 서버를 다시 올리면 **다음 30분 cycle(라이브 전송)이 성공한 직후** 파킹분이 자동 drain으로 도착한다. 이때 **최신 seq가 먼저, 파킹돼 있던 옛 seq가 나중에** 오는 것(§3.2)과, `GET :9100/drain-status`의 `succeeded` 증가를 확인.
2. **corrupt 격리 (깨진 WAL)** — spool `new/`의 `.json` 파일 하나를 일부러 자른 뒤(truncate) 파서를 재기동하면, 해당 파일만 `corrupt/`로 격리되고 기동·나머지 replay는 정상 진행된다 (매 기동 반복 실패 방지). 격리분은 자동 재전송되지 않는다 — seq 구멍의 "안 채워짐" 원인 중 하나(§3.4).
3. **raw 오버플로 (bounded 응답)** — `GET :9100/raw?since=24h&max_mb=1`처럼 넓은 창에 작은 예산을 주면, 예산 초과분이 라인 경계에서 잘리고 응답 헤더 `X-Raw-Truncated: true`로 표시된다 (헤더는 항상 존재, 평소엔 `false`). 잘못된 `since` 형식·과대값은 500이 아니라 기본 창(1h)으로 폴백된다.

### 5.3 스키마 검증 소스

구현 후 필드 하나라도 애매하면 문서보다 코드를 본다 — envelope 정본은 `src/envelope.rs` (구조체 그대로 직렬화되며, `Option` 필드는 `None`일 때 **JSON 키 자체가 생략**된다), 응답 분류 정본은 `src/transport/http.rs`. 현재 envelope에는 **`schema_version` 필드가 없다** — 도입 제안은 [architecture-review.md](architecture-review.md) §4-4 참조 (보류 상태). 미지의 키는 무시하고 계속 처리할 것.
