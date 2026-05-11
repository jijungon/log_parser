# log_parser — Agent Role Definitions (5 에이전트)

> 위치: 4문서 위계의 **생성 주체 (WHO)** — 총괄·구조는 `2_MASTER_PLAN.md`, 실행은 `2_MASTER_PLAN.md §18`

---

## 통합 후 결정 사항 (반드시 숙지)

| 변경 | 근거 | 영향 |
|------|------|------|
| **Live Collection 자체 구현 → Vector 0.55.0 채택** | Phase A: RSS 47MB / CPU 0.4% / drop 0건 | Live는 Vector가 담당 (Agent 3) |
| **stat_report · sos_report 구현 분리** | log_parser Phase B 이후 별도 스텝. 본 프로젝트 범위 | 설계 완료(`2_MASTER_PLAN.md §7.8·§7.9`). Agent 2 담당. Phase B 완료 후 구현 |
| **15 Phase → 5 Step 축소** | Phase A 검증 + SOS 분리 | 본 문서 §"개발 Step 매핑" |
| **cgroup v2 자가 격리 채택** | Phase A: 200MB 폭주 시 시스템 영향 +5MB | Phase B Step 1 (Coordinator 책임) |
| **MASTER_PLAN + ARCHITECTURE 통합** | observability-design 프레임 채택 | Envelope 스키마 SSoT는 `2_MASTER_PLAN.md` §10 |
| **분석 영역 모두 수신측으로 이전** (2026-05-06) | log_parser는 정제·송출만. 호스트 측 분석은 운영 유연성·확장성 제약 | **Agent 6·7·8·9 폐기** (Trigger Detector, Heartbeat Aggregator, Cool-down, Snapshot Liaison) |
| **단일 envelope 모델 채택** | 분기 없는 단일 형식. 수신측 단일 dispatch | event_kind="log_batch" 고정. headers + body |
| **Push + Pull (`/flush`)** (2026-05-06) | 평시 push 가벼움 + 사고시 수신측이 즉시 pull 가능 | Agent 5에 inbound `/flush` endpoint 추가. 원칙 #5 갱신 |
| **카테고리 매핑 외부화** (`categories.yaml`) | 새 카테고리 추가 시 코드 변경 불필요 | Agent 4 책임. config 룩업 |
| **모든 시간·자원 단위 외부화** (`agent.yaml`) | 운영 환경별 조정 가능 | cycle, dedup window, LRU cap, cgroup 모두 config |

---

## 공통 전제 (모든 에이전트 공유)

```
언어:       Rust (edition 2021)
런타임:     tokio (async)
공유 타입:  2_MASTER_PLAN.md §10 — Envelope 스키마 (단일 형식, SSoT)
원칙:       2_MASTER_PLAN.md §12 (5원칙) 절대 준수
완료 기준:  3_PHASE_B.md §1 해당 Step "검증" 항목 통과
참조 문서:  2_MASTER_PLAN.md (총괄·구조·요구사항·Envelope 스키마·컴포넌트)
            2_MASTER_PLAN.md §18 (작업 순서·산출물·검증 게이트)
```

---

## Agent 1 — Platform Discovery Engineer

### 담당 모듈
```
src/platform/
├── cgroup.rs       ← cgroup v2 자가 격리 (Coordinator와 공동, Step 1 핵심)
├── discovery.rs    ← OS/커널/init/cgroup/컨테이너 탐지
└── capability.rs   ← 명령어·파일·권한 런타임 probe
```

**완료 기준**: 3_PHASE_B.md §1 Step 1·3 — cgroup self-attach 동작, vector.toml distro 분기 (Ubuntu `/var/log/syslog` / RHEL `/var/log/messages`)

---

## Agent 2 — Snapshot Engineer (stat_report · sos_report)

> **Phase B 이후 구현**. 본 프로젝트 범위. 설계·API 명세는 `2_MASTER_PLAN.md §7.8·§7.9` 완료.

### 담당 모듈
```
stat_report daemon (port 9102)   ← GET /stat → 7개 섹션 즉시 응답
sos_report daemon  (port 9101)   ← POST /trigger-sos → sos_snapshot 반환 (단일 동기 호출)
```

**완료 기준**: 2_MASTER_PLAN.md §7.8·§7.9 API 명세 완료. Phase B 이후 별도 Step 검증.

---

## Agent 3 — Vector Operations Engineer

### 담당 모듈
```
config/
├── vector.toml          ← Vector 운영 설정 (IPC: socket sink → Unix domain socket)
├── agent.yaml           ← Rust agent 설정 (cycle, dedup, cgroup, transport, pipeline)
└── categories.yaml      ← 카테고리 매핑 (Agent 4와 공동)

(systemd unit / docker-compose로 Vector + Rust agent 동시 기동 스크립트도 본 에이전트 책임)
```

> **IPC 핵심**: vector.toml의 세 `socket` sink가 **Unix domain socket** 파일에 JSON 라인을 write.
> Rust `vector_receiver.rs`가 각 소켓을 `UnixListener::bind`로 수신.
> 소켓 파일 권한 600 — 별도 토큰 인증 불필요. route transform으로 critical/normal 분기.
> `events_critical.sock`: `when_full=block` / `events_normal.sock`, `metrics.sock`: `when_full=drop_newest`

**완료 기준**: 3_PHASE_B.md §1 Step 3 — Vector 60초 운영 RSS <50MB / CPU <1%, cgroup 합산 한도 내, 재시작 후 journald cursor 이어가기

---

## Agent 4 — Pipeline Data Engineering Specialist

### 담당 모듈
```
src/pipeline/
├── raw_event.rs    ← RawEvent enum (§7.11) — Vector socket sink 수신 타입 (FROZEN)
└── vector_receiver.rs  ← tokio UnixListener — Unix domain socket 이벤트 수신·파싱

src/normalize/
├── tokens.rs       ← 가변 토큰 치환
├── severity.rs     ← PRIORITY·키워드 기반 severity 보정
└── categories.rs   ← categories.yaml 룩업

src/dedup/
└── window.rs       ← 슬라이딩 윈도우 + LRU (agent.yaml로 외부화)

config/
└── categories.yaml ← 카테고리 매핑 (Agent 3과 공동)
```

> 본 에이전트가 **카테고리 매핑·severity 라벨링의 SSoT**.
> `raw_event.rs`는 §7.11에서 freeze됨 — 필드 추가는 `extra` 필드로만 허용.

**완료 기준**: 3_PHASE_B.md §1 Step 4 — 동일 메시지 1,000번 → dedup 1건(count=1000), LRU 50,001번째 evict, categories.yaml 룩업, counts 누적 정확

---

## Agent 5 — Transport & Reliability Engineer

### 담당 모듈
```
src/transport/
├── mod.rs                  ← Transport trait + kind별 팩토리 (agent.yaml transport.kind)
├── otlp.rs                 ← OtlpTransport (운영, gRPC+protobuf, §7.7.3 매핑)
├── http.rs                 ← HttpJsonTransport (Phase B 테스트·디버깅, kind="http_json")
└── file.rs                 ← FileTransport (로컬 개발)

src/inbound/
└── flush.rs                ← POST /flush endpoint (Bearer 인증) — Step 5
```

> `OtlpTransport`가 운영 기본. `HttpJsonTransport`는 Phase B 검증용 + OTLP 미지원 환경 fallback.
> `src/batch/`는 별도 모듈 불요 — envelope batching은 Coordinator(★) 책임에 흡수.

**완료 기준**: 3_PHASE_B.md §1 Step 2·5 — mock에 정상 송신, 단절 5분 누락 0건, Critical burst drop 0건, `/flush` 즉시 응답 + 새 cycle 시작

---

## Coordinator (Orchestrator) — 진입점 + envelope 조립

> Agent 1·3·4·5가 구현하는 모듈들의 **진입점·조립자**. envelope 조립도 책임. cgroup self-attach는 Agent 1 단독 책임으로 분리 (SRP 명확화, 2026-05-06).

### 담당 파일
```
src/main.rs              ← CLI entrypoint, cgroup self-attach 호출 (Agent 1 모듈 위임)
src/wiring.rs            ← 모듈 조립, 파이프라인 채널 구동
src/config.rs            ← agent.yaml 로드·검증, 기본값 제공
src/envelope.rs          ← Envelope 스키마 구현 (2_MASTER_PLAN.md §10을 그대로 옮김)
src/coordinator/
└── envelope.rs          ← cycle 만료/flush 시 envelope 조립 (직렬화 + cycle.seq 단조 증가)
```

**완료 기준**: 3_PHASE_B.md §1 Step 1·2·4 — cgroup.procs에 PID 등록, 더미 envelope mock 수신, 30분 cycle envelope 1건(단일 schema)

---

## 에이전트 간 인터페이스 계약 (요약)

```
Vector (Agent 3) → Pipeline (Agent 4: Normalize + Dedup + 태깅) → Coordinator (★: envelope 조립) → Transport (Agent 5)

채널 타입:
  tokio::sync::mpsc::Sender<RawEvent>     (Vector → Normalizer)
  tokio::sync::mpsc::Sender<DedupEvent>   (Dedup → Coordinator)
  tokio::sync::mpsc::Sender<Envelope>     (Coordinator → Transport)
  tokio::sync::oneshot::Sender<FlushSignal> (Transport inbound → Coordinator)

공통 데이터 계약:
  Envelope = 2_MASTER_PLAN.md §10 (단일 형식, event_kind="log_batch")

배포 단위 (모두 본 프로젝트):
  log_parser daemon  (port 9100, Phase B)          ← 로그 스트림 정제·송출
  stat_report daemon (port 9102, Phase B 이후)      ← 현재 상태 스냅샷 (GET /stat)
  sos_report daemon  (port 9101, Phase B 이후)      ← 사고 포렌식 스냅샷 (POST /trigger-sos)
  → 같은 호스트에 3개 독립 프로세스. 서로 모름. 수신측이 host_id로 결과를 묶음
```

---

## 개발 Step과 에이전트 매핑

상세 표는 **3_PHASE_B.md §1** 참조.

| Step | 내용 | 주담당 |
|:----:|------|--------|
| 1 | 스켈레톤 + cgroup 자가 격리 + agent.yaml 로드 | **Coordinator + Agent 1** |
| 2 | Envelope 스키마 + Transport outbound | **Coordinator + Agent 5** |
| 3 | Vector 운영 config | **Agent 3** |
| 4 | Normalizer + Dedup + 태깅 + Coordinator envelope 조립 | **Agent 4 + Coordinator** |
| 5 | 신뢰성 마감 + Transport inbound `/flush` | **Agent 5** |

> Phase B 외부 — 추후 점진:
> - OS 매트릭스 확장 (P1·P2): Agent 1
> - PII 마스킹 / Multi-line 병합: Agent 4
> - 운영성 (Prometheus, health, graceful shutdown): Agent 5 + Coordinator
> - 패키징 (RPM/DEB/systemd unit): Coordinator
> - categories.yaml 권장 셋 정리: Agent 4

> Phase B 이후 별도 스텝 (본 프로젝트 범위):
> - **stat_report 구현**: Agent 2. `2_MASTER_PLAN.md §7.8` 설계 완료
> - **sos_report 구현**: Agent 2. `2_MASTER_PLAN.md §7.9` 설계 완료

> 본 프로젝트 외부:
> - **수신측 alerting 룰·sos 호출 결정**: 운영 팀 또는 별개 프로젝트
