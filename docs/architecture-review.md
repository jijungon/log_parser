> **이관 문서**: 원본은 저장소 밖(`/Users/joji/app/logger/PARSER_ARCHITECTURE_REVIEW.md`)에서 작성된 아키텍처 리뷰(2026-07-30 검증 노트 포함)를 docs/로 이관한 것이다.
> 이관 시 본문은 수정하지 않았다 — 코드 라인 번호·권고 상태는 리뷰 시점(2026-07-30, 3-에이전트 코드 대조) 기준.

# log_parseer_ai 파서 아키텍처 리뷰

> 대상: `/Users/joji/app/logger/log_parseer_ai` (Rust, cgroup 128MB / CPU 5%, 무서버, Linux 상주)
> 관점: 아키텍처 구조 · 전달 방식 · 에러 처리 · 기성 로그 전달 제품 비교 · 로그 포맷 발전 방향
> 방식: 순수 분석. 코드 근거는 `src/` 상대경로 라인. **모든 개선 권고는 제안(뼈대)이며 승인 전 적용 없음.**

---

## 1. Executive Summary

- **이 파서는 "로그 포워더"가 아니라 "엣지 집계기(edge aggregator)"다.** 기성 포워더(Fluent Bit/Vector/Filebeat)가 원본 라인을 1:1 전달하는 것과 달리, 우리는 Vector를 *수집기로만* 임베드하고 Rust 파이프라인에서 **정규화 → 지문 dedup → 30분 cycle 집계 → envelope push** 한다. body는 원본이 아니라 `DedupEvent{template, count, ...}` 요약이다.
- **128MB/5%CPU 예산이 성립하는 이유가 바로 이 집계 모델.** 따라서 기성품 기능을 채택할 때는 "그것이 raw-forwarding 전제에서만 의미 있는가"를 매번 걸러야 한다.
- **신뢰성 메커니즘은 이미 기성품 중상위권이다.** WAL spool(3-pool + dir fsync), crash recovery, bounded retry + 지수 백오프(critical 2×), dead-letter TTL·용량 관리, graceful shutdown 무손실을 이미 갖췄다. Fluent Bit 기본 설정보다 오히려 견고한 부분도 있다.
- **지금 손댈 가치가 있는 건 값싼 High 3개뿐:** ① envelope `schema_version` 필드, ② 멱등 키 헤더(cycle 중복 방지), ③ 드롭/유실 카운터(silent loss 가시화). 셋 다 에이전트 비용이 상수 몇 개 수준.
- **과잉설계로 기각/보류:** sqlite/WAL-lite 저장 엔진 교체(기각), OTel/ECS 필드 전면 개명·protobuf 전환·백프레셔 완전 전파(downstream 로드맵이 요구하기 전까지 보류).

---

## 2. 파서 아키텍처 현황 (코드 근거)

실제 흐름은 두 층: **Vector(수집·1차 분류) → Unix socket push → Rust(정규화·dedup·전송)**.

### 2-1. 파이프라인 단계

**(A) 수집 계층 — Vector** (파서가 자식 프로세스로 spawn·감시, `main.rs:170-196`)
- 소스(tail/pull): journald(`vector_config.rs:126`), 파일 tail `syslog`/`auth`/`audit`(`:54,:78,:100`) — 전부 `type="file"`.
- 1차 severity/source 부여: journald PRIORITY→severity 매핑(`:132-144`), 파일 소스는 `info` 고정.
- 노이즈 드롭 필터: journald debug 제거(`:170-175`).
- severity 라우팅: critical vs normal 분기(`:177-183`).
- Sink(push→Unix socket): `type="socket" mode="unix"`, JSON codec, `acknowledgements.enabled=true`, **disk 버퍼 512MB `when_full="block"`**(`:187-208`).

**(B) 처리 계층 — Rust**
- inbound(수신 소켓): critical/normal 두 Unix 소켓 bind(권한 0o600), 라인별 JSON → `RawLogEvent` → mpsc(용량 기본 10000, `main.rs:217`) — `vector_receiver.rs:31-84`.
- 정규화 + dedup: coordinator 루프(`coordinator/mod.rs::run_pipeline`), 이벤트 → `process_line`(`process.rs:36-77`) = `strip_syslog_prefix → normalize → severity::finalize → fingerprint → dedup`.
- transport(push→/ingest): cycle 타이머 만료 시 `cycle.finalize` → spool WAL 저장 → `send_with_backoff` → gzip HTTP POST(`transport/http.rs:52-78`).

> ⚠️ 용어 주의: `:9100`의 "inbound"는 로그 수집이 아니라 **별도 HTTP pull API**(`/flush /stat /trigger-sos /raw /drain-spool /drain-status`, `inbound/mod.rs:187-199`). 실제 로그는 Vector→Unix socket **push**로 들어온다.

### 2-2. 전달 방식

- **push(아웃바운드)**: HTTP POST + gzip, `Authorization: Bearer`, transport 종류는 `http_json`만(`transport/mod.rs:19-24`). **배치 단위 = cycle envelope**, 주기 기본 **1800s(30분)**(`config.rs:263`).
- **pull(인바운드 `:9100`)**: `/flush`는 진행 cycle 즉시 finalize해 gzip 응답(전송 안 함), `/trigger-sos`는 요청 시 라이브 스냅샷 수집.
- **4개 토큰(env)**: `PUSH_OUTBOUND_TOKEN`(push Bearer), `FLUSH_INBOUND_TOKEN`, `STAT_INBOUND_TOKEN`, `SOS_INBOUND_TOKEN`. 미설정 시 해당 endpoint 기동 거부(`main.rs:298-317`), 인증은 상수시간 비교.
- **백프레셔·상한**: bounded mpsc(10000), 전송 동시성 Semaphore(4)로 압축 body 누적 OOM 방지, cycle 이벤트 상한 100000(초과 시 오래된 non-critical부터 evict, critical은 항상 수용), dedup LRU cap 50000(초과분은 폐기 아닌 조기 방출→유실 0), Vector disk 512MB — **critical sink만 `when_full="block"`(역압), normal sink는 `when_full="drop_newest"`**(vector_config.rs:198/211). ⚠️ 즉 normal 경로의 Vector 내부 드롭이 최대 silent-loss 지점이며, §5-3 드롭 계측(에이전트 측 카운터)으로는 잡히지 않는다 — Vector internal metrics 없이는 관측 불가.

### 2-3. 에러 처리

- **WAL 선기록**: envelope을 spool `new/`에 저장 후 push, 저장 성공 후에만 seq 영속화("구멍" 방지, `mod.rs:204-226`).
- **오류 3분류**(`http.rs:64-77`): `Fatal`(4xx)→즉시 `retry/` 파킹, `RateLimited`(429)→`Retry-After` 대기, `Retryable`(5xx·네트워크)→지수 백오프(base 5s→max 300s).
- **재시도 한도**: non-critical 기본 5, **critical 2×**. 소진 시 무한재시도 대신 `retry/` 파킹(OOM 방지).
- **성공 시**: spool 파일 commit(삭제) + `auto_drain.trigger()`로 `retry/` 자동 재전송.
- **드롭 지점(관측 포함)**: Vector JSON 파싱 실패(`warn!` 후 드롭), 빈 raw 스킵, `park_to_retry` 최종 유실 시에만 `error!(events,bytes)`, 손상 spool은 `corrupt/` 격리.
- **SOS 경로**: `/trigger-sos`가 8개 섹션 동시수집(metrics/processes/network/systemd/config/hardware/logs) → `sos_snapshot` envelope을 gzip 응답으로 즉시 반환(push 아님).
- **dedup 지문**: `xxh3(template | severity | source)`, coordinator·collect 양 경로 동일 함수. template은 가변토큰 placeholder 치환(`<UUID> <IP4> <PATH> <NUM>` 등, RegexSet 프리필터).
- **관측**: `tracing` 구조화 로깅, envelope headers에 `ProcessHealth{vector_restarts_24h, agent_uptime_seconds}` 포함, LRU 축출 100건마다 warn.

### 2-4. 현재 스키마

```
Envelope { event_kind:"log_batch",
           cycle{host,host_id,boot_id,ts,window,seq},
           headers{counts{by_severity,by_category}, process_health, duration_ms},
           body:[Section{section, data}] }
DedupEvent { source, severity, category, fingerprint, template,
             sample_raws[≤3], fields{}, ts_first, ts_last, count }
```
**핵심 관찰: 스키마 버전 필드가 없다.** `event_kind:"log_batch"`만 있고 `schema_version`이 어디에도 없음(§5-4 최우선 근거).

---

## 3. 기성 로그 전달 제품 비교

### 3-1. 신뢰성 프리미티브 매트릭스

| 프리미티브 | 우리 현황 | Fluent Bit | Vector | Filebeat | Fluentd | Promtail | OTel Collector |
|---|---|---|---|---|---|---|---|
| 디스크 버퍼(spool) | ✅ WAL 3-pool | filesystem storage | disk buffer | spool-to-disk | file buffer | WAL | file storage |
| at-least-once | ⚠️ 부분(§5-1) | ✅ | ✅ | ✅(ack+registry) | ✅ | ✅ | ✅ |
| 백프레셔 전파 | ⚠️ 세마포어 drop형 | mem_buf_limit | ✅ e2e | ✅(harvester 정지) | buffer full 정책 | ✅ | ✅ |
| 지수 백오프 재시도 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 수신측 ack/2PC | ❌ HTTP 2xx만 | ❌ | ⚠️(sink별) | ✅(LS/ES ack) | ⚠️ | ✅(Loki 204) | ⚠️ |
| checkpoint/offset | ✅ 파일 offset은 Vector data_dir 체크포인트 + seq(§5-2). 격차는 **최초 기동 `read_from=end`**(그 이전 라인 미수집)뿐 | ✅ pos db | ✅ | ✅ registry | ✅ pos file | ✅ positions.yaml | ✅ |
| dead-letter | ✅ retry/ TTL·cap | ⚠️ | ✅ | ⚠️ | ⚠️ | ❌ | ⚠️ |

**우리가 이미 상위권**: 디스크 버퍼 crash-safety(dir fsync까지), dead-letter 수명관리(TTL+용량), graceful-shutdown 무손실.
**빈틈 후보 3개** → §5로 전개: (a) 수신측 배달 확정 부재, (b) offset/checkpoint 정밀도, (c) 백프레셔가 "전파"가 아니라 "드롭"에 가까움.

### 3-2. 제품별 채택 가능 vs 부적합 (128MB/5%CPU 필터)

| 제품 | 채택 가능 | 부적합 |
|---|---|---|
| **Vector** (가장 가까운 참고) | disk buffer "commit은 sink 확인 후" 규율(이미 유사) | full e2e ack를 Rust까지 관통=복잡도↑, 집계기라 라인 단위 ack 무의미 |
| **Fluent Bit** (128MB 롤모델) | `mem_buf_limit` 스타일 메모리 상한 기반 백프레셔 | 멀티워커/멀티스레드 tail은 5%CPU에 과함 |
| **Filebeat/Beats** | registry(offset) 개념 = seq 정밀화 참고 | raw 라인 harvest 전제(축이 다름) |
| **Fluentd** | chunk 개념(=우리 cycle) | Ruby 런타임 메모리 풋프린트 128MB 부담 |
| **Promtail(Loki)** | WAL 재생 규율(동일 철학) | Loki 전용 라벨 모델 |
| **Logstash** | DLQ 개념(이미 `corrupt/`·`retry/`로 커버) | JVM, 무거움 |
| **OTel Collector** | **Logs Data Model**(§4 핵심 참고) | Collector 본체는 상주 에이전트로 과함 |

> **§3 결론:** 신뢰성 *메커니즘* 자체는 이미 중상위권. 지금 실효 개선은 "새 버퍼 엔진 도입"이 아니라 기존 WAL 위에서 (a)수신 확정 의미론 명확화, (b)백프레셔 드롭→전파(선택) 두 가지에 국한. 그 이상은 과잉설계.

---

## 4. 로그 포맷 발전 방향

### 4-1. OTel Logs Data Model / ECS 필드 매핑 (이름 정렬, 값·구조는 유지)

| 우리 | OTel | ECS |
|---|---|---|
| `severity` | SeverityText | `log.level` |
| **(신설) severity_number** | **SeverityNumber(1–24)** | — |
| `category` | Attributes[`event.category`] | `event.category` |
| `template` | Body(구조화) | `message` |
| `fingerprint` | Attributes[`log.fingerprint`] | `event.hash` |
| `count` | Attributes[`aggregate.count`] | — |
| `host/host_id/boot_id` | **Resource** | `host.name`/`host.id`/`host.boot.id` |
| `ts_first/ts_last` | ObservedTime/TimeUnix 범위 | `event.start`/`event.end` |

- **가장 값싼 실효 항목: `severity_number`(OTel 1–24) 추가.** 텍스트 severity만 있으면 downstream 정렬·임계 질의가 약하다.
- 전면 개명은 수신처·goldset(log_stack_AI 채점)과 계약이 얽힘 → **별칭 병행** 또는 "우리 이름 유지 + 수신처 매핑"이 현실적.

### 4-2. 집계 메타는 표준에 억지로 욱여넣지 말 것
우리 차별점인 `template/count/fingerprint/ts범위`는 표준에 1:1 대응이 없다. `Attributes` 아래 **`agg.*` 네임스페이스**로 명시해 의미를 보존한다. 과표준화 경계.

### 4-3. 압축·전송 효율
현재 gzip JSON **유지 권장**. `sample_raws[]` 반복 문자열이 압축 전 body 대부분 → 지문당 sample 상한만 config화. protobuf/OTLP 전환은 30분 gzip 전송량 기준 이득 미미 + 디버깅성 상실 → 부적합.

### 4-4. 스키마 버저닝 (지금 없음 → 도입)
- **참고**: OTel 명시적 버전, ECS `ecs.version`, CloudEvents `specversion`.
- **채택안**: envelope 최상위에 **`schema_version:"1.0"`(SemVer)** 1개 추가. 규율: additive(필드 추가)=minor / 의미변경·제거=major. 수신처는 major만 게이팅, minor는 forward-compatible.
- **이것이 §4의 다른 모든 필드 정렬의 전제조건.** 버전 없이는 어떤 스키마 진화도 안전하게 굴릴 수 없다.

---

## 5. 개선 권고 — 우선순위 종합

| # | 권고 | 우선순위 | 지금 실효성 | 에이전트 비용 |
|---|---|---|---|---|
| 4-4 | envelope `schema_version` 필드 | **High** | 높음 (다른 진화의 전제) | 상수 1개 |
| 5-1 | 멱등 키 헤더(seq+host_id+boot_id) | **High** | 높음 (cycle 중복 방지) — ⚠️ **5-2가 하드 선행조건**: seq 파일 유실 시 `unwrap_or(1)` 리셋(main.rs:233-240)이라 같은 boot 내 키 충돌 가능 | 헤더 1줄 + 수신처 협조 |
| 5-3 | 드롭/유실 카운터 → process_health | **High** | 높음 (silent loss 가시화) | AtomicU64 몇 개 |
| 4-1a | `severity_number`(OTel 1–24) | Med | 즉시 이득 | 매핑 테이블 |
| 5-2 | seq 파일 atomic write | Med | 중 (crash 마감) | 기존 코드 재사용 |
| 4-1b | ECS/OTel 필드 전면 정렬 | Med | downstream 의존 | 계약 동시변경 |
| — | 백프레셔 전파 | Low | 낮음 (5-3로 대체) | 높음(과잉위험) |
| — | `agg.*` 네임스페이스 | Low | 중(우리 한정) | 저 |
| — | sqlite/WAL-lite 저장 전환 | **기각** | 과잉설계 | — |

### High 3개 상술

**5-1. 멱등 키 헤더 → at-least-once를 "실질적 exactly-once 근사"로**
- 참고: Filebeat↔Logstash ack, Vector e2e ack, Kafka idempotent producer.
- 채택안: 이미 존재하는 `cycle.seq + host_id + boot_id`를 멱등 키로 승격해 `Idempotency-Key: {host_id}:{boot_id}:{seq}` HTTP 헤더로 노출, 수신처 `/ingest`가 이 키로 dedup. 에이전트 변경은 헤더 1줄.
- 근거: 재시도가 성공했는데 2xx 응답만 유실되면 **cycle 통째 중복 전송**이 발생하고, 지문 dedup은 라인 단위라 cycle 중복을 못 거른다 → 집계가 부풀려짐. 수신처 협조(계약 변경) 필요.

**5-3. 드롭/유실 카운터 (silent loss 가시화)**
- 참고: OTel `otelcol_processor_dropped`, Fluent Bit metrics.
- 채택안: CycleState event-cap drop, `retry/` eviction/TTL drop, `park_to_retry` 최종 유실을 카운터로 집계해 이미 존재하는 `process_health`에 `dropped_events_24h`, `retry_evicted_24h` 필드로 노출(`warn!`/`error!`는 이미 있으니 카운터화만).
- 근거: "조용한 손실"이 현재 가장 큰 운영 리스크. §백프레셔 구현보다 훨씬 싸게 같은 문제(손실 인지)를 해결.

**5-2. seq 파일 atomic write (Med, 값싼 마감)**
- 현재 "WAL 저장 성공 후에만 persist_seq" 규율은 옳으나, seq 파일 쓰기 자체는 `write(path, seq)`라 전원유실 시 truncate 위험. spool의 `write_atomic`(temp+rename)을 seq에도 재사용. 수 바이트라 fsync 비용 무시가능.

---

## 6. 정직한 결론 (과잉설계 경계)

- **신뢰성 메커니즘은 이미 기성품 중상위권** — 새 저장 엔진 도입은 명백한 과잉. sqlite ring buffer/WAL-lite는 우리 쓰기 프로파일(30분당 소량 배치)에 이득이 없다 → **기각**.
- **지금 손 댈 가치가 있는 건 값싼 High 3개**: `schema_version`(4-4) / 멱등 키(5-1) / 드롭 계측(5-3). 셋 다 비용 최소·이득 최대.
- **보류가 정답인 것**: OTel/ECS 필드 전면 개명, protobuf/OTLP 전환, 백프레셔 완전 전파 — downstream이 실제로 표준 파이프라인을 붙이기 전까지는 과잉설계. 우리 집계 스키마의 고유 가치(`agg.*`)는 표준에 희생시키지 말 것.
- 순서 제안: **4-4(버전 필드) → 5-1(멱등 키) → 5-3(드롭 계측) → 4-1a(severity_number)**, 나머지는 downstream 로드맵 확인 후.

---

*근거 원본: `/tmp/explorer-parser-report.md`(코드 라인 근거), `/tmp/architect-parser-analysis.md`(비교·권고 3단 구조). 본 문서는 두 리포트의 사실만 재구성한 통합본이며, 모든 개선안은 승인 전 제안 상태.*

---

## 7. 리뷰어 검증 노트 (2026-07-30 정정·보충)

3-에이전트 코드 대조에서 발견된 본문 오류는 §2-2 백프레셔 행·§3-1 checkpoint 행·§5 5-1 행에 **정정 반영 완료**. 아래는 본문이 누락했던 사실과 후속 조치 상태.

### 본문 누락 보충

- **severity 승격 경로 없는 카테고리 13/21**: `auth.bruteforce`·`selinux.denial`·`disk.smart_error`·`net.error` 등은 severity 키워드에 해당 문구가 없어 파일 소스에서 info 고정 출하. (categories.yaml 매칭과 severity.rs 키워드가 독립이라 생기는 틈)
- **cycle cap 결함 3개** (cycle.rs:50-84): cap 도달 시 ①counts 카운터 미차감 ②evict가 O(n) remove ③non-critical 무성 스킵(로그·카운터 없음). §5-3 드롭 계측 구현 시 함께 다룰 것.
- **디스크 총예산 미문서화**: spool new/ 2048MB + retry/ 1024MB + corrupt/(상한 도입 전 무제한) + Vector data_dir 512MB ≈ **최대 ~4GB** — 운영 문서에 합산 예산으로 명시 필요.

### 후속 조치 상태 (2026-07-30 배치1)

- ✅ persist_seq 원자화(5-2) — `write_file_atomic` 재사용으로 반영
- ✅ corrupt/ 상한(256MB, 오래된 것부터 삭제) 반영 → "무제한" 누락 항목 해소
- ✅ `sample_raws` 상한 3 → `dedup.sample_raws_cap` 설정화 (§2-4 스키마의 `[≤3]`은 기본값 기준)
- ✅ `body_max_size_mb` 문서-동작 불일치 → agent.yaml 주석을 실동작(warn 후 통과)으로 정정
- 📊 system.general fallback 계측: **원시 라인 가중 8.3%**(전체 55 envelope 누적; 최신 envelope 0.4%) — 대량 발생 로그는 잘 분류됨. 단 **고유 패턴 기준 ~83%**가 fallback — 다양성 축은 미분류(조용한 서버의 저빈도 잡로그). 파서 출하 효율 관점에선 categories 정밀화 급하지 않음, 검색(스택) 품질 관점에서만 선택적 개선 대상.
- ⛔ critical 즉시 push: 기존 pull-모델 결정(2026-07-23) 유지, 재기각. Idempotency-Key(5-1): 수신처 로드맵 확정 시까지 보류.
