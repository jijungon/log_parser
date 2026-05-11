# IMPL_NOTES — 구현 참조

> 2_MASTER_PLAN.md에서 분리한 코드 수준 세부사항. 구현 단계에서 참조.

---

## 1. Vector → Rust IPC 메커니즘

> **결정**: Vector `socket` sink (Unix domain socket) → Rust `tokio::net::UnixListener`

Rust agent가 시작 시 `/run/log_parser/` 디렉토리를 생성하고 두 소켓 파일을 bind. Vector 자식 프로세스는 각 소켓에 JSON 라인을 write.

| 항목 | 결정 |
|------|------|
| 프로토콜 | **Unix domain socket** (`AF_UNIX`, stream) — TCP 스택 우회, 커널 IPC 직접 경유 |
| 인코딩 | `encoding.codec = "json"`, newline-delimited (Vector 기본값) |
| 인증 | **소켓 파일 권한 600** (log_parser 프로세스 소유) — 별도 토큰 불필요 |
| 소켓 분리 | `events_critical.sock` / `events_normal.sock` — buffer 전략 분리 |
| buffer 전략 | critical: `when_full=block` / normal: `when_full=drop_newest` — route transform으로 분기 |
| 구현 모듈 | `src/pipeline/vector_receiver.rs` |

**왜 Unix socket**: Vector와 Rust는 동일 호스트 동일 cgroup의 부모-자식 프로세스. TCP 루프백 대비 레이턴시 ~2-3배 낮음. 포트 충돌 없음. 파일시스템 권한이 인증을 대체.

**`agent.yaml` 키**:
```yaml
pipeline:
  vector_critical_sock: "/run/log_parser/events_critical.sock"
  vector_normal_sock:   "/run/log_parser/events_normal.sock"
```

---

## 2. Transport trait + 구현체

**Trait**:
```rust
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send(&self, envelope: &Envelope) -> Result<(), TransportError>;
    fn name(&self) -> &'static str;
}
```

구현체 (`agent.yaml`의 `transport.kind`로 선택):

| kind | 구현체 | 용도 |
|------|--------|------|
| `"otlp"` | `OtlpTransport` | **운영** — gRPC+protobuf, 모든 OTLP 호환 수신측 |
| `"http_json"` | `HttpJsonTransport` | Phase B 테스트·디버깅, OTLP 미지원 커스텀 수신측 |
| `"file"` | `FileTransport` | 로컬 개발, spool 검증 |

**재시도 전략 (exponential backoff + jitter)**:

```
초기 대기: 5s → 배수 2 → 최대 300s, jitter ±25%
```

| severity | 재시도 정책 |
|---|---|
| critical 포함 envelope | **무한 재시도** (원칙 #1 — drop 절대 금지) |
| critical 없는 envelope | 최대 5회 재시도 후 drop |

**Disk spool (at-least-once 보장)**:

Coordinator가 envelope finalize 시 **송출 전에** spool 파일에 기록 → 송출 성공 후 삭제.

```
/var/lib/log_parser/spool/pending/
  <ULID>.envelope   ← 미송출 envelope (critical 포함 여부 메타 포함)
```

**`agent.yaml` 외부화**:
```yaml
transport:
  kind: "http_json"                 # "http_json" (Phase B) | "otlp" (수신측 확정 후)
  endpoint: "https://otelcol.example.com:4317"
  token_env: "PUSH_OUTBOUND_TOKEN"
  tls_enabled: true
  connect_timeout_seconds: 10
  request_timeout_seconds: 30
  retry_max_normal: 5
  retry_base_seconds: 5
  retry_max_seconds: 300
  spool_max_mb: 512
  spool_dir: "/var/lib/log_parser/spool"
  http_gzip_level: 6              # http_json 전용
```

---

## 3. Inbound `/flush` 상세

**역할**: 수신측의 "지금 envelope 송출해" 신호를 받아 Coordinator에 `FlushSignal` oneshot으로 전달.

**API 명세**:

```
POST /flush
Authorization: Bearer ${FLUSH_INBOUND_TOKEN}
Content-Type: application/json
Body: {} (empty)

Response 200 OK:
  Content-Type: application/json
  Content-Encoding: gzip
  Body: <Envelope JSON>

Response 401 Unauthorized: 토큰 없음/잘못됨
Response 409 Conflict:    이미 flush 처리 중 (Retry-After 헤더 포함)
Response 429 Too Many:    rate-limit 초과 (Retry-After 헤더 포함)
Response 503 Service Unavailable: Coordinator·Transport 비정상
```

**동시성 정책 (cycle timer와의 race 방지)**:
- Coordinator 안에 단일 mutex 또는 actor로 "envelope finalize"를 직렬화
- `/flush`와 cycle timer는 **둘 다 같은 채널의 메시지로 변환**되어 순서 보장
- 한 번에 하나의 finalize만 진행. 진행 중 두 번째 요청 → 첫 번째 완료 후 처리 (또는 409)
- `cycle.seq` 단조 증가 invariant: finalize 1회당 seq+1

**보안·운영 정책**:

| 항목 | 정책 |
|---|---|
| listen address | `127.0.0.1:9100` (외부 노출 안 함) |
| TLS | mTLS 또는 TLS termination 외부 reverse proxy |
| 인증 | Bearer 토큰 (env `${FLUSH_INBOUND_TOKEN}`) — 실패 시 401 + 100ms backoff |
| rate-limit | default `6 requests/hour/source-ip` (token bucket) |
| body size limit | 1KB |
| 응답 timeout | 5초 |
| envelope size 제한 | gzip 후 10MB. 초과 시 503 + Retry-After |

**`agent.yaml` 외부화**:
```yaml
inbound:
  listen_addr: "127.0.0.1:9100"
  token_env: "FLUSH_INBOUND_TOKEN"
  rate_limit_per_hour: 6
  body_size_limit_bytes: 1024
  response_timeout_seconds: 5
  envelope_size_limit_mb: 10
```

**구현 위치**: `src/inbound/flush.rs`, Coordinator와는 `tokio::sync::mpsc::Sender<FlushRequest>` 채널 결합.

---

## 4. OtlpTransport — Envelope → OTLP 매핑

> Envelope을 **OTLP `ExportLogsServiceRequest`** (gRPC+protobuf)로 변환.
> Grafana Cloud, Datadog, Elastic, Honeycomb 등 OTLP 네이티브 수신측 모두 호환.

**Rust 크레이트**: `tonic` + `opentelemetry-proto`

```
Envelope
  ├─ cycle + headers + process_health
  │     → OTLP Resource attributes (호스트·cycle 메타데이터 전체)
  └─ body[0].data[DedupEvent]   (body[0].section = "logs")
        → OTLP LogRecord[] (이벤트 1개 = LogRecord 1개)
```

**Resource attributes**:

| OTLP attribute key | 값 |
|---|---|
| `host.name` | `cycle.host` |
| `host.id` | `cycle.host_id` |
| `service.name` | `"log_parser"` |
| `log_parser.boot_id` | `cycle.boot_id` |
| `log_parser.seq` | `cycle.seq` |
| `log_parser.window` | `cycle.window` |
| `log_parser.counts.critical` | `headers.counts.by_severity.critical` |
| `log_parser.counts.error` | `headers.counts.by_severity.error` |
| `log_parser.vector_restarts_24h` | `headers.process_health.vector_restarts_24h` |

**LogRecord (DedupEvent 1개)**:

| OTLP 필드 | 값 |
|---|---|
| `time_unix_nano` | `ts_first` (nanoseconds) |
| `observed_time_unix_nano` | 현재 시각 |
| `severity_number` | critical=21 / error=17 / warn=13 / info=9 |
| `severity_text` | severity 문자열 |
| `body` | `template` |
| attribute `log_parser.source` | `source` |
| attribute `log_parser.category` | `category` |
| attribute `log_parser.fingerprint` | `fingerprint` |
| attribute `log_parser.count` | `count` |
| attribute `log_parser.ts_last` | `ts_last` |
| attribute `log_parser.sample_raws` | `serde_json::to_string(&sample_raws)` |

**수신측 OTLP 호환 서비스**:

| 서비스 | endpoint 형식 | 비고 |
|--------|-------------|------|
| Grafana Cloud | `https://otlp-gateway-<region>.grafana.net:443` | Basic base64(instanceId:apiKey) |
| Datadog | `https://api.datadoghq.com:443` | DD-API-KEY header |
| Elastic APM | `https://<apm-server>:8200` | Bearer |
| OpenTelemetry Collector | `https://otelcol.internal:4317` | 자체 설치, 가장 유연 |

**Empty cycle 처리**: `log_records = []`. Resource attributes 부착 → 수신측이 alive 확인 가능 (§7.6.1).

---

## 5. RawEvent 내부 스키마

> Vector `socket` sink가 Unix domain socket으로 전달하는 **내부 이벤트 타입**.
> 외부 envelope schema(§10)와 구별. 이 타입 변경 시 Normalizer 계약 깨짐.

### LogEvent (journald + file source)

Vector가 Unix socket에 write하는 단일 이벤트 JSON:

```json
{
  "log_parser_severity": "critical",
  "log_parser_source":   "journald",
  "MESSAGE":             "Out of memory: Killed process 2481 (java)",
  "PRIORITY":            "0",
  "_SYSTEMD_UNIT":       "kernel",
  "_PID":                "1",
  "host":                "app-prod-03",
  "timestamp":           "2026-05-06T10:13:42.000000000Z"
}
```

**Rust struct** (`src/pipeline/raw_event.rs`):

```rust
#[derive(Debug, Deserialize)]
pub struct RawLogEvent {
    pub log_parser_severity: String,           // "critical" | "error" | "warn" | "info"
    pub log_parser_source:   String,           // "journald" | "file.syslog" | "file.messages"
                                               // | "file.auth" | "file.audit" | "file.docker"
    pub timestamp:           DateTime<Utc>,
    pub host:                String,
    #[serde(rename = "MESSAGE")]
    pub message:             Option<String>,
    #[serde(rename = "PRIORITY")]
    pub priority:            Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    pub unit:                Option<String>,
    #[serde(rename = "_PID")]
    pub pid:                 Option<String>,
    #[serde(rename = "message")]
    pub file_message:        Option<String>,
    #[serde(rename = "file")]
    pub file_path:           Option<String>,
    #[serde(flatten)]
    pub extra:               serde_json::Value,
}

impl RawLogEvent {
    pub fn raw_message(&self) -> &str {
        self.message
            .as_deref()
            .or(self.file_message.as_deref())
            .unwrap_or("")
    }
}
```

### RawEvent (최상위 타입)

```rust
pub enum RawEvent {
    Log(RawLogEvent),
}
```

`events_critical.sock` / `events_normal.sock`: 모두 `RawEvent::Log` 파싱.

**Freeze 정책**: Phase B Step 2 시작 전 freeze. 추가 필드는 `extra` 필드로 흡수해 하위 호환 유지. 필드 제거·타입 변경은 MASTER_PLAN 개정 + Normalizer 계약 재검토 필요.
