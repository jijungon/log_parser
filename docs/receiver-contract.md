# 수신측 계약 — 외부 인터페이스 명세

> 본 프로젝트 외부에 있는 **수신 서비스와의 약속**.
> 이 프로젝트는 receiver 자체를 만들지 않지만, envelope을 받는 쪽이 무엇을 보장해야 envelope 모델이 의미 있는지를 이 문서에 명시.
> receiver 구현은 본 프로젝트 외부 (운영 팀 또는 별개 프로젝트가 책임).
> **구현 절차·체크리스트는 [receiver-implementation-guide.md](receiver-implementation-guide.md)**, 타입 정의는 [receiver-type-spec.md](receiver-type-spec.md).

---

## 1. 송출 contract (log_parser → receiver)

> **kind="http_json" 기준 — 현재 구현된 유일한 transport** (`src/transport/mod.rs`는 다른 kind에서 기동 거부). kind="otlp"(gRPC+protobuf)는 설계 시 후보였으나 **미구현** — `master-plan.md §7.7.3`은 이력 참조.

```
POST <ingest endpoint, agent.yaml의 transport.endpoint — 단일 URL>
Authorization: Bearer ${PUSH_OUTBOUND_TOKEN}   # transport.token_env가 가리키는 환경변수의 값
Content-Type: application/json
Content-Encoding: gzip                          # 항상 gzip (협상 없음)
Body: <Envelope JSON — receiver-type-spec.md schema>

Response 2xx (전부):          수신 성공 — 파서가 spool WAL 삭제(commit) + retry/ 자동 drain 트리거
Response 429:                Retry-After(정수 초, 없으면 60) 대기 후 재시도 — 재시도 한도를 소모함
Response 5xx · 네트워크 에러:  지수 백오프 재시도 (기본 5s→최대 300s 간격, 한도 소진 시 retry/ 파킹)
Response 그 외 (4xx·3xx 전부): 치명(Fatal) — 재시도 없이 즉시 retry/ 파킹 (drain으로만 재전송)
```

**배달 의미론 (수신측이 전제해야 하는 것)** — 상세·근거는 [receiver-implementation-guide.md](receiver-implementation-guide.md) §3:

- **at-least-once** — 2xx 응답이 유실되면 같은 cycle이 통째로 재전송된다 → 멱등 처리 필수 (§2).
- **파킹분 자동 회수** — `retry/`에 파킹된 envelope은 이후 라이브 전송 성공(수신 복구 증거) 시 **자동 drain**(`transport.auto_drain` 기본 true)으로, 또는 수동 `POST :9100/drain-spool`로 재전송된다.
- **도착 순서 ≠ 발생 순서** — 자동 drain은 최신 envelope 성공 *직후* 돌기 때문에 옛 seq가 늦게 도착하는 것이 정상.
- **최대 지연 = retry/ TTL 기본 168h** (`transport.retry_ttl_hours`) — 초과분은 오래된 것부터 삭제(영구 유실).

---

## 2. 수신측이 envelope으로 해야 하는 것 (권장)

| 책임 | 권장 동작 |
|---|---|
| 인덱싱 | `(host_id, boot_id, window)` 복합 키로 인덱싱 |
| 중복 방지 | `(host_id, boot_id, seq)` 3개 값이 모두 같으면 같은 데이터로 간주해 한 번만 처리 (stat/sos는 seq 없음 → `ts` 보조 키 upsert) |
| 재배달 수용 | 도착 순서에 의존하지 않고 `cycle.window`·`ts_first` 기준 시간 정렬 저장. seq 구멍은 TTL(168h)까지 "배달 중"으로 취급 후 유실 확정 |
| body 분석 | severity·category·template·fingerprint 기반 패턴 매칭. 시간순 연결 |
| Alerting | `observability-design.md §9` 권장 룰 셋 (panic 키워드, hw 에러, fs.readonly, 재시작 루프, 에러율 폭증, 부팅 직후, 호스트 침묵) |
| 상세 수집 | "사고다" 판단 시 `GET :9100/stat` + `POST :9100/trigger-sos` 호출, 원문 필요 시 `GET :9100/raw` — **전부 단일 포트 :9100** ([pull-api.md](pull-api.md)) |
| 호스트 침묵 감지 | log_parser envelope 30분 + grace 5분 안에 안 오면 호스트 이상 (host_id 기준 — 빈 cycle도 전송되므로 침묵은 항상 이상 신호) |
| Cool-down | 같은 호스트·같은 사고로 sos 중복 호출 방지 (pull API는 파서 측 rate-limit도 있음 — 429 + Retry-After) |
| 결과 묶기 | `host_id` 기준으로 log_batch / stat_snapshot / sos_snapshot 세 envelope을 연결 |

---

## 3. 수신측 프로젝트 자체

| 항목 | 결정 |
|---|---|
| 누가 만드나 | **본 프로젝트 외부**. 운영 팀 자체 구축 또는 별개 프로젝트 |
| 권장 구현 후보 | (a) Loki + Alertmanager + custom adapter, (b) ClickHouse + Grafana, (c) 자체 서비스 (Rust/Go) |
| Alerting 룰 owner | 환경별 임계값은 운영 팀 |
| Phase B 검증 | mock receiver로 대체. 실제 receiver 통합은 별도 마일스톤 |

---

## 4. Alerting 룰 예시 (sos 트리거 조건)

수신측 alerting 시스템(Alertmanager 또는 custom)에서 정의. log_parser는 판단하지 않음.

| 트리거 | 매칭 조건 |
|---|---|
| 패닉/OOM | `severity=critical AND template contains "Out of memory: Killed\|kernel BUG\|panic:"` |
| 하드웨어 에러 | `category=hw.mce` |
| 파일시스템 강등 | `template contains "remounting filesystem read-only"` |
| 재시작 루프 | `unit별 restarts >= 3 / cycle` (수신측이 body group_by로 계산) |
| 에러율 폭증 | `headers.counts.by_severity.error >= N` (baseline 대비 ×10 등) |
| 부팅 직후 | boot ID 변경 + 첫 envelope (의도 안 한 재부팅 가능성) |
| 호스트 침묵 후 복귀 | envelope 끊김 후 복귀 (35분 이상 미수신) |

---

## 5. Envelope schema 진화 정책

> **현재 envelope에는 `schema_version` 필드가 없다** (`src/envelope.rs` 기준 — 도입 제안은 [architecture-review.md](architecture-review.md) §4-4, 보류 상태). 아래 표는 필드 도입 시점부터 적용할 규율이며, 그때까지 실효 규칙은 마지막 두 줄(모르는 키 무시)이다.

| 변경 종류 | schema_version 변경 | 호환성 |
|---|---|---|
| 키 추가 (additive) | 1.x → 1.(x+1) | 이전 버전 수신측과 호환. 기존 수신측은 새 키를 무시함 |
| 키 의미 변경·키 제거 | 1.x → 2.0 | 호환 깨짐. 수신측 업데이트 필요 |
| 값 타입 변경 | 1.x → 2.0 | 호환 깨짐. 수신측 업데이트 필요 |

수신측은 모르는 키를 만나면 **무시하고 계속 처리**해야 함.
알 수 없는 schema_version 메이저 버전을 만나면 4xx 응답 가능.
