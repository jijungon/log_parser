# log_parser — 마스터 플랜 (observability 통합본)

> 작성일: 2026-04-28 (갱신: 2026-05-07)
> 범위: Linux 시스템의 **로그 파일 + journald**를 실시간 수집·정제하여 외부 서비스로 송출하는 경량 단일 바이너리 에이전트 — 3개 에이전트(log_parser / stat_report / sos_report) 모두 본 프로젝트 범위.
>
> 에이전트 체계:
> - **log_parser**: 로그 수집·정제·송출 전담 (30분 push + `/flush`)
> - **stat_report**: 현재 시스템 상태 경량 스냅샷 (요청이 오면 즉시 응답, `/stat`)
> - **sos_report**: 사고 시 전체 포렌식 스냅샷 (단일 동기 호출, `POST /trigger-sos`)
>
> 분리된 영역:
> - **Vector 자체 수정**: log_parser는 Vector 0.55.0을 외부 도구로 사용. fork·수정은 별도

---

## 0. 문서 네비게이션

| 문서 | 내용 |
|------|------|
| **`2_MASTER_PLAN.md`** (본 문서) | 전체 구조·컴포넌트·설계 원칙 |
| `3_PHASE_B.md` | Phase B 5단계 실행 계획 + 완료 정의 + 용어집 |
| `IMPL_NOTES.md` | IPC·Transport·flush·OTLP·RawEvent 구현 세부사항 — **코드 작성 시 참조** |
| `3_AGENT_ROLES.md` | 5 에이전트 책임·구현 계약 |
| `1_observability-design.md` | 3계층 모델·envelope 스키마 — 설계 근거 참조 |
| `4_RECEIVER_CONTRACT.md` | 수신측 계약 — 송출 contract, 권장 동작, schema 진화 정책 |
| `5_TEST_RECEIVER.md` | Phase B 검증용 mock receiver 스펙 (Python, 테스트 전용) |

---

## 1. 한 줄 요약

> Linux 호스트에서 3개 에이전트(log_parser / stat_report / sos_report)가 협력해 로그·상태·포렌식 데이터를 수집·정제하여 외부 서비스로 송출하는 경량 Rust 에이전트 시스템. 모두 **단일 JSON envelope (event_kind + cycle + headers + body)** 형식 공유.
>
> - **log_parser**: 30분 cycle마다 로그 분석 결과 자동 push (gRPC OTLP) + `POST /flush` (port 9100)
> - **stat_report**: `GET /stat` pull, 즉시 응답 — 현재 수치·상태 7개 섹션 (port 9102)
> - **sos_report**: `POST /trigger-sos` → sos_snapshot 즉시 반환 — 전체 포렌식 (port 9101)
>
> **호스트는 분석하지 않는다** — 정제·수집·송출만. severity 결정·트리거 판단·집계·알림은 모두 수신측.

---

## 2. 3개 에이전트 역할 분리

### 2.1 핵심 원칙

```
log_parser   → 파일에 기록된 로그 분석 전담 (스트림)
stat_report  → 현재 시스템 상태 경량 스냅샷 (요청이 오면 즉시 응답)
sos_report   → 사고 시 전체 포렌식 스냅샷 (stat + logs 통합)
```

| | log_parser | stat_report | sos_report |
|---|---|---|---|
| 성격 | 로그 이벤트 스트림 분석 | 현재 상태 경량 스냅샷 | 시스템 전체 포렌식 스냅샷 |
| 수집 방식 | 로그 파일을 실시간 follow | 수집 시점에 한 번 읽기 | stat + logs 통합 수집 |
| 언제 | 항상 (30분 주기 자동 push) | 수신측 pull 시 즉시 | 문제 발생 시 수신측 트리거 |
| `/proc` `/sys` 수치 | 담당하지 않음 | **담당** | **담당** (stat 섹션 포함) |
| 로그 파일 분석 | **담당** (30분 cycle) | 담당하지 않음 | **담당** (직전 2~4시간) |
| 하드웨어·설정·프로세스 | 담당하지 않음 | **담당** | **담당** |
| port | 9100 | 9102 | 9101 |
| event_kind | `log_batch` | `stat_snapshot` | `sos_snapshot` |

**관계**: sos = stat + log_parser
- stat body = 7개 섹션 (metrics, processes, network, systemd, static_state, config, hardware)
- sos body = stat body + logs (2~4시간)
- log body = logs (30분 cycle)

### 2.2 운영 흐름

```
평시:
    log_parser → 30분마다 로그 분석 결과 push

문제 감지 시 수신측:
    GET  host:9102/stat         → stat_report  (즉시, 현재 수치·상태)
    POST host:9100/flush        → log_parser   (로그 스트림)
    POST host:9101/trigger-sos  → sos_report   (전체 포렌식, 수집 완료 후 응답)

log_parser   → "로그에서 무슨 이벤트가 발생했는가"
stat_report  → "지금 시스템 수치·상태는 어떠한가"
sos_report   → "사고 시점 전후 시스템이 어떤 상태였는가"
```

### 2.3 수집 범위 최종 표

| 섹션 | 출처 | stat | log_parser | sos | 전송 방식 |
|---|---|:---:|:---:|:---:|---|
| metrics | /proc/stat, /proc/meminfo, /proc/diskstats, /proc/net/dev, /proc/loadavg, /proc/pressure/ | ✅ | | ✅ | pull 즉시 / 사고 시 |
| processes | ps aux, /proc/<pid>/ | ✅ | | ✅ | pull 즉시 / 사고 시 |
| network | ss, iptables/nftables, /proc/net/sockstat | ✅ | | ✅ | pull 즉시 / 사고 시 |
| systemd | systemctl status 전체 | ✅ | | ✅ | pull 즉시 / 사고 시 |
| static_state | sestatus, sysctl -a, lsmod, chronyc tracking, /proc/cmdline | ✅ | | ✅ | pull 즉시 / 사고 시 |
| config | /etc/* 원본 전체, 패키지 목록 | ✅ | | ✅ | pull 즉시 / 사고 시 |
| hardware | dmidecode, lshw, lspci, smartctl, lsblk | ✅ | | ✅ | pull 즉시 / 사고 시 |
| logs | journald, syslog, auth, audit, docker, dmesg | | ✅ (30분) | ✅ (2~4시간) | 30분 자동 push / 사고 시 |

> **config vs static_state 구분**: `config`는 설정 파일 원본 내용(`/etc/sysctl.conf`에 뭐라고 써있나), `static_state`는 현재 실제로 적용된 런타임 값(`sysctl -a`로 지금 무엇이 동작 중인가). 파일 내용과 런타임 적용값이 다를 수 있으므로 둘 다 필요.

### 2.4 observability 계층 안에서의 위치

스트림·스냅샷 분리 근거는 `1_observability-design.md §2 + §12`.
**본 프로젝트는 3개 에이전트 모두 담당** (log_parser: 스트림, stat_report/sos_report: 스냅샷).

---

## 4. 담당 / 비담당 + 핵심 요구사항

### 4.1 이 에이전트(log_parser)가 하지 않는 것

- `/proc`, `/sys` 수치 수집 (CPU, 메모리, 디스크 I/O, 네트워크 메트릭) — stat_report / sos_report 담당
- 문제 발생 여부 판단 / 트리거 매칭 / 임계값 분석
- severity 결정 후 알림·원인 분석·사고 분류
- 카운트 정렬·top N 산출 같은 분석
- 외부 명령 수신·실행 (예외: 송출 타이밍 신호 — flush trigger만)
- 모드 전환·일시적 상태 저장 (cool-down 등)
- **sos_report / stat_report 호출 결정·실행** (수신측이 결정)
- **실행 중 상태 조회** (프로세스·포트·iptables 등 — stat_report / sos_report 영역)
- **설정 파일·하드웨어 정보 수집** (패키지 목록, `/etc/*`, 하드웨어 정보 — stat_report / sos_report 담당)

### 4.2 핵심 요구사항

| # | 요구사항 | 설계 반영 |
|---|----------|----------|
| 1 | Live 수집을 검증된 도구로 | Vector 0.55.0 채택 |
| 2 | **속도**를 무시하지 않음 | Rust (Vector + 자체 보강 모듈) |
| 3 | 송출 endpoint를 단일 고정으로 | Transport trait + 단일 outbound sink |
| 4 | 반복 로그를 **자동 압축** | 정규화 → 시그니처 해시 → 30초 dedup 윈도우 |
| 5 | **severity·category 정확히 태깅** | severity 4단계 + category(`categories.yaml` 룩업) |
| 6 | 매 cycle마다 단일 형식으로 송출 | envelope (headers + body) 30분 cycle push |
| 7 | 수신측 on-demand 호출 | inbound `/flush` endpoint (송출 타이밍 신호만 허용) |
| 8 | 3개 에이전트 독립 운영 | log_parser·stat_report·sos_report는 서로 모름. 수신측이 deep-pull 시 각 endpoint에 병렬 호출 |

---

## 5. 언어·도구 선택

| 영역 | 선택 | 근거 |
|------|------|---------------------|
| **Live Collection** | **Vector 0.55.0** | 메모리 사용량(RSS) 47MB / CPU 0.4% / drop 0건. 자체 구현보다 검증된 형태로 90% 커버 |
| **자체 보강 모듈** (Coordinator·Normalizer·Pipeline·Transport·신뢰성) | **Rust** | tokio + serde + reqwest 생태계 |
| **자가 격리** | **cgroup v2** | 200MB 폭주 부하 중 cgroup 내부 OOM, 시스템 메모리 영향 +5MB만 |

---

## 6. 전체 아키텍처

```
        ┌──────────────────────────────────────────────────────────────────┐
        │  대상 호스트 (Linux)                                               │
        │  cgroup v2 (자가 격리, 별도 cgroup) — memory.max=128M cpu=5%      │
        │                                                                  │
        │  ┌─────────────────────────────────────────────┐                 │
        │  │  log_parser daemon (port 9100)              │                 │
        │  │                                             │                 │
        │  │  Vector 0.55.0 (수집 전담)                    │                 │
        │  │      │  journald · /var/log/* (로그 파일)      │                 │
        │  │      ▼                                       │                 │
        │  │  Pipeline (Normalize + Dedup + 태깅)          │                 │
        │  │      │                                       │                 │
        │  │      ▼                                       │                 │
        │  │  Coordinator (envelope 조립)                  │                 │
        │  │      │  cycle 만료 또는 flush trigger 시       │                 │
        │  │      ▼                                       │                 │
        │  │  Transport                                  │                 │
        │  │   ├─ outbound: 30분 push gRPC OTLP          │                 │
        │  │   └─ inbound  POST /flush (Bearer)           │                 │
        │  └─────────────────────────────────────────────┘                 │
        │                                                                  │
        │  ┌──────────────────────────────┐  ┌──────────────────────────┐  │
        │  │  stat_report daemon          │  │  sos_report daemon       │  │
        │  │  (port 9102)                 │  │  (port 9101)             │  │
        │  │   GET /stat → 즉시 응답       │  │  POST /trigger-sos       │  │
        │  │   7개 섹션 스냅샷             │  │    → 8개 섹션 응답        │  │
        │  │   (metrics·processes·        │  │    (수집 완료 후 반환)     │  │
        │  │    network·systemd·          │  │  (stat body + logs)      │  │
        │  │    static_state·config·      │  │                          │  │
        │  │    hardware)                 │  │                          │  │
        │  └──────────────────────────────┘  └──────────────────────────┘  │
        └──────────────────────────────────────────────────────────────────┘
                    │            ▲              ▲           ▲
          push(30분)│    /flush  │     /stat    │  /trigger-sos
                    │            │              │
                    ▼            │              │           │      │
              ┌─────────────────────────────────────────────────────────┐
              │  수신측 분석 서비스                                        │
              │  - envelope 인덱싱 (host_id, boot_id, seq 기준)           │
              │  - severity·패턴 매칭·alerting                           │
              │  - "상세 수집 필요" 판단 시                               │
              │      병렬: GET /stat + POST /flush + POST /trigger-sos   │
              │  - sos 결과 묶어 사고 분석                                │
              └─────────────────────────────────────────────────────────┘
```

**핵심 흐름**:
- **log_parser**: Vector → Pipeline (Normalize + Dedup + 태깅) → Coordinator (envelope 조립) → Transport (push or pull 응답)
- **stat_report**: `GET /stat` 수신 → 7개 섹션 수집 → `stat_snapshot` envelope 즉시 반환
- **sos_report**: `POST /trigger-sos` 수신 → 수집 실행 → `sos_snapshot` envelope 반환
- **공통 envelope 형식**: `event_kind` + `cycle` + `headers` + `body[]` 구조 (§10 참조)
- 호스트는 **분석 안 함** — 정제·수집·송출만
- 수신측이 분석·트리거 판단·각 에이전트 호출 결정 모두 담당
- 3개 daemon은 서로 모름 — 수신측이 `host_id`로 결과를 묶음

---

## 7. 컴포넌트별 역할

### 7.1 cgroup v2 — 자가 격리

**역할**: 에이전트 전체(Vector + Rust)가 절대 호스트를 망가뜨리지 못하게 하드 리미트.

커널이 강제하는 리소스 상한 — 에이전트가 아무리 망가져도 운영 서비스는 안전.

| 항목 | 값 | 근거 |
|------|----|------|
| `memory.max` | 128 MiB | 실측 평시 RSS ~54MB(Vector 47 + Rust 7)의 ~2.4배 여유 |
| `cpu.max` | 5% (`50000 1000000`) | 평시 <1% 측정 |
| OOM 동작 | cgroup 내부에서만 kill | 폭주 테스트에서 시스템 메모리 영향 +5MB |

**구현 위치**: Phase B Step 1. `/sys/fs/cgroup/log_parser_agent/` 생성 → 자기 PID 등록.

### 7.2 Vector 0.55.0 — Live Collection 전담

**역할**: 파일에 기록된 로그 분석 전담. `/proc`, `/sys` 수치 수집은 담당하지 않음 (stat_report / sos_report 담당).

`Linux 배포판(distribution) 줄임말입니다.`

**입력**:
- `journald` source — systemd journal 실시간 follow (distro 무관)
- `file` source — `/var/log/messages` 또는 `/var/log/syslog` (distro별 활성화)
- `file` source — `/var/log/auth.log` (Ubuntu/Debian) 또는 `/var/log/secure` (RHEL/Rocky/Alma)
- `file` source — `/var/log/audit/audit.log` (auditd 환경)
- `file` source — `/var/lib/docker/containers/*/*-json.log` (json-file driver) ⚠️ 추가 필요

**처리(transform)**:
- `dedupe` — 최근 N개 캐시 비교 단순 압축
- VRL `remap` — PRIORITY → severity 매핑 (envelope 카운트용 라벨)
- `filter` — 노이즈 제외

**핵심 보장 (buffer 정책 — severity별 분리)**:

> ⚠️ 단일 sink에 `when_full=block`을 걸면 source가 block되어 호스트의 다른 서비스 로그까지 영향 (journald forward 큐 막힘, file source가 logrotate와 충돌). 원칙 #2 위반. 따라서 severity별로 분리:

| sink | severity | `when_full` | 근거 |
|---|---|---|---|
| `to_rust_critical` | critical만 | `block` | drop 절대 금지 (원칙 #1) |
| `to_rust_normal` | error/warn/info | `drop_newest` | 호스트 보호 우선. 일부 손실 허용 |

- `buffer.type=disk` 둘 다 적용 (sink retry용 본래 용도)
- `acknowledgements.enabled=true` → 전 구간 최소 1회 전달 보장
- **disk buffer max_size**:
  - cgroup memory와 별개로 디스크 쿼터로 설정
  - default `max_size=512MB` per sink (`agent.yaml`로 외부화)
  - free space < 10% 시 `to_rust_normal`도 자동 drop 모드 전환 (운영 안전망)
- 평시 메모리 사용량(RSS) ~47MB, CPU ~0.4%

**왜 Rust로 안 만드나**: Vector가 이미 Rust 단일 바이너리로 위 기능을 검증된 형태로 제공. 직접 만들면 수개월 작업, 결과는 동일하거나 열등.

#### 7.2.1 Vector → Rust IPC 메커니즘

> 상세: `IMPL_NOTES.md §1`

Unix domain socket 2개: `events_critical.sock` (`when_full=block`) / `events_normal.sock` (`when_full=drop_newest`).  
구현: `src/pipeline/vector_receiver.rs`

### 7.3 Vector source distro 분기

| distro | sources |
|--------|---------|
| 공통 | journald, `/var/log/audit/audit.log`, `/var/lib/docker/containers/*/*-json.log` ⚠️ |
| Debian/Ubuntu | `/var/log/syslog`, `/var/log/auth.log` |
| RHEL/Rocky/Alma | `/var/log/messages`, `/var/log/secure` |

구현: `config/vector.toml` distro 분기 (Phase B Step 3).

### 7.4 Normalizer + Deduplicator (Rust)

> **수치 안내**: 윈도우 길이(30초)와 LRU 상한(50,000)은 직감값. **Phase B Step 4에서 실 운영 압축률 측정으로 확정**.

**처리 흐름**:

```
raw line                                예: "Out of memory: Killed process 2481 (java)"
   │
   ▼  [1] 가변 토큰 치환
   │       숫자 → <NUM>, IP → <IP>, UUID → <UUID>, 경로 → <PATH>
   │       결과: "Out of memory: Killed process <NUM> (<WORD>)"
   │
   ▼  [2] 시그니처 해시 (xxhash 또는 blake3)
   │
   ▼  [3] 시간 윈도우 집계 (30초, 튜닝 대상)
   │       key:   (fingerprint, host, source)
   │       value: { count, first_ts, last_ts, sample_raws[:3] }
   │
   ▼  [4] flush → Event 방출
           - count == 1: 원본 그대로
           - count > 1:  요약 ("× 123회 | 10:00 → 10:02")
```

**LRU 상한**: 시그니처 캐시 50,000개 (DoS 방지). 초과 시 가장 오래 안 쓰인 것부터 제거.

**Vector dedupe와의 관계**: Vector는 "최근 N개 캐시" 비교 방식. Rust 측 Aggregator는 시그니처 해시 + 시간 윈도우 방식. 두 단계가 직렬 동작.

**구현 위치**: `src/normalize/`, `src/dedup/`

**처리 예시**: `docs/normalizer-dedup.00.example`

### 7.5 Pipeline — 카테고리·severity 태깅 (외부화 매핑)

> 정제 영역. dedup된 이벤트에 severity와 category 라벨 부착.

- **severity 매핑**: PRIORITY 또는 키워드 기반 (`1_observability-design.md §6`)
  - PRIORITY 0~2 / 패닉 키워드 → critical
  - PRIORITY 3 / "ERROR" / segfault → error
  - PRIORITY 4 / "WARN" → warn
  - PRIORITY 5~7 / 기타 → info
- **category 매핑**: `categories.yaml` 외부화
  ```yaml
  - pattern: "Out of memory: Killed"
    category: kernel.oom
  - pattern: "EXT4-fs error"
    category: fs.error
  ```
- **categories.yaml 전체 분류 항목**:
  - `kernel.oom`, `kernel.panic`, `kernel.bug`
  - `process.crash`
  - `fs.error`, `fs.readonly`
  - `hw.mce`
  - `disk.io_error`, `disk.smart_error`, `disk.link_error`
  - `net.error`, `net.watchdog`, `net.interface`
  - `systemd.unit_failure`, `systemd.restart_loop`
  - `auth.failure`, `auth.event`, `auth.bruteforce`
  - `ntp.drift`
  - `container.oom`
  - `selinux.denial`
- **새 카테고리 추가**: config 수정 + agent 재시작 (코드 변경 불필요)
- **구현 위치**: `src/normalize/categories.rs` + `config/categories.yaml`
- **책임자**: `3_AGENT_ROLES.md` Agent 4 (Pipeline Specialist)



### 7.6 Coordinator — Envelope 조립

> 정제·송출 사이의 묶어서 송출 준비. 분석은 안 함.
> 타이머 터지면 버퍼 비우고 JSON 하나 만들어 보내기

- **트리거**:
  - cycle 만료 (default 30분, `agent.yaml`로 외부화 — P2)
  - inbound `POST /flush` 신호 (수신측이 상세 조사 시)
- **조립 작업**:
  - cycle 동안 누적된 DedupEvent들을 **fingerprint 기준으로 병합** 후 `{section: "logs", data: [...]}` 형태로 `body[0]`에 담음 (이벤트 없으면 `data: []`)
    - 같은 fingerprint가 여러 30초 윈도우에 걸쳐 들어온 경우 → 1개 DedupEvent로 합산
    - `count` = 전체 합산, `ts_first` = 가장 이른 시각, `ts_last` = 가장 늦은 시각, `sample_raws` = 첫 윈도우 것 유지
    - 예: OOM 로그가 0~30초에 47번, 30~60초에 23번 → `count=70, ts_first=0s, ts_last=59s`
    - sos_report의 `logs` 섹션도 동일한 `{section, data[DedupEvent]}` 구조 — 수신측이 파서 재사용 가능
  - Pipeline의 카운터로 `headers.counts.by_severity`, `headers.counts.by_category` 채움
  - `process_health` 헤더 채움 (Vector 재시작 카운터 등 — §7.11.5)
  - cycle 메타데이터(`window`, `host`, `host_id`, `boot_id`, `seq`) 부착
- **구현 위치**: `src/main.rs` + `src/coordinator/`
- **책임자**: `3_AGENT_ROLES.md` Coordinator (★)
- **분석 아님**: 단순 묶기. 패턴 매칭·임계값·정렬·top N 같은 분석은 안 함.

#### 7.6.1 Empty cycle invariant — 호스트 침묵 vs 다운 구분

**불변 규칙**: `body`가 빈 배열이어도 cycle 만료·flush 시 envelope을 **항상** 송출.  
→ envelope 도착 자체가 alive 신호. 안 오면 수신측이 호스트 다운과 정상 침묵을 구분 불가.

empty cycle envelope: `cycle.*` + `process_health` 정상 부착, `counts` 모두 0, `body: [{section:"logs", data:[]}]`.

**수신측 SLA 권장**: 30분 + grace 5분 = 35분 안에 없으면 이상 알림 (`host_id` 기준).  
`boot_id` 변경은 의도치 않은 재부팅 신호로 별도 처리.

#### 7.6.2 body overflow 정책 (폭주 호스트 대응)

| 항목 | 기본값 | `agent.yaml` 키 |
|------|--------|----------------|
| 최대 이벤트 수 / cycle | 100,000개 | `pipeline.body_max_events_per_cycle` |
| 최대 body 크기 (raw JSON) | 50 MB | `pipeline.body_max_size_mb` |

**overflow 시 drop 우선순위**:
1. 가장 오래된 `info` 이벤트부터 drop
2. info 다 dropped → 가장 오래된 `warn` 이벤트부터 drop
3. `error`/`critical` 이벤트는 **절대 drop 금지** (원칙 #1)
   - overflow 상태에서 critical/error가 오면 body에 보존하고 오래된 info/warn을 추가로 제거

**모니터링**: overflow 발생 시 `headers.counts.by_severity`에는 실제 수신 카운트(drop 전)를 기록, body의 이벤트 수는 상한 내 값. 수신측이 두 값의 차이로 drop 규모 추산 가능.

**WARN 로그**: overflow 시작 시 agent 로컬 tracing 로그에 `body_overflow=true, dropped=N` 기록.

### 7.7 Transport (outbound) + Inbound (`/flush`)

> 외부로 보내는 모듈과 신호를 받는 모듈은 **별개**로 분리.

> **변경 용이성 원칙**: 구체적인 선택(HTTP 클라이언트·재시도 파라미터·압축 방식 등)은 언제든 바꿀 수 있어야 한다.
> - **구현 교체** → `Transport` trait 뒤에 숨김. `HttpJsonTransport` 대신 다른 구현체로 swap 가능.
> - **파라미터 조정** → 모든 수치는 `agent.yaml`로 외부화. 코드 변경·재배포 없이 조정.
> - **수치값은 코드에 직접 쓰지 않음** — timeout·retry 횟수·gzip level 등은 전부 설정값.

#### 7.7.1 Transport (outbound, `src/transport/`)

> trait·구현체·재시도·spool·`agent.yaml` 상세: `IMPL_NOTES.md §2`

| kind | 구현체 | 용도 |
|------|--------|------|
| `"otlp"` | `OtlpTransport` | **운영** — gRPC+protobuf |
| `"http_json"` | `HttpJsonTransport` | Phase B 테스트·디버깅 |
| `"file"` | `FileTransport` | 로컬 개발 |

재시도: exponential backoff (5s → 300s), critical 포함 시 무한 재시도, 나머지 max 5회.  
Disk spool: `/var/lib/log_parser/spool/pending/<ULID>.envelope` (default 512MB 상한).

#### 7.7.2 Inbound `/flush` (`src/inbound/flush.rs`)

> 동시성 정책·보안 상세·`agent.yaml`: `IMPL_NOTES.md §3`

**API**: `POST /flush` → 200 OK (gzip Envelope) | 401 | 409 (처리 중) | 429 (rate-limit) | 503  
**보안**: `127.0.0.1:9100`, Bearer 인증, rate-limit 6회/hour, 응답 timeout 5초  
**동시성**: cycle timer와 flush 신호를 동일 채널 메시지로 직렬화. seq 단조 증가 보장.

#### 7.7.3 OtlpTransport — Envelope → OTLP 매핑

> 필드 매핑 테이블 상세: `IMPL_NOTES.md §4`

Envelope → OTLP `ExportLogsServiceRequest` (gRPC+protobuf).  
`cycle + headers` → Resource attributes / `body[0].data[DedupEvent]` → `LogRecord[]`.  
호환: Grafana Cloud, Datadog, Elastic APM, OpenTelemetry Collector.

### 7.8 stat_report — 현재 시스템 상태 경량 스냅샷

> **역할**: 현재 시스템 수치·상태를 즉시 반환하는 에이전트. 캐시를 미리 채워두어 요청 시 바로 응답.

- **endpoint**: `GET /stat` (127.0.0.1:9102, Bearer 인증)
- **전송**: 단일 JSON, gzip
- **응답 시간**: < 100ms (캐시에서 즉시 반환)
- **body**: 7개 섹션 (§2.3 수집 범위 표 참조)

**동작 방식 — 하이브리드 (캐시 선갱신 + 즉시 응답)**:

```
[백그라운드] 섹션별 TTL로 갱신 (log_parser push와 엇갈리게 오프셋 조정)
  ├── dynamic (metrics/processes/network/systemd): 30분마다 갱신 — xxh3_64 체크섬
  ├── static  (hardware/config/static_state):     6시간마다 갱신 — blake3 체크섬
  ├── 이전 캐시와 비교 → 동일하면 timestamp만 갱신, 변경 없음 표시
  └── 변경 있으면 캐시 교체

[요청 처리] GET /stat 수신
  └── 캐시에서 즉시 반환 (수집 대기 없음)
```

**log_parser와의 타이밍 분리**:
- log_parser push: 매 30분 `:00`, `:30` (예: 09:00, 09:30, …)
- stat_report 갱신: log_parser 대비 15분 오프셋 — `:15`, `:45` (예: 09:15, 09:45, …)
- 두 작업이 겹치지 않아 cgroup CPU/메모리 부하 분산

**변화 없음 감지 (no-change detection)**:
- 섹션별 체크섬(xxhash) 비교: 직전 캐시와 동일하면 갱신 건너뜀
- hardware, config 같이 거의 안 바뀌는 섹션은 실제 시스템 호출 횟수 최소화
- 헤더에 `last_changed_ts` 포함 → 수신측이 데이터 신선도 확인 가능

**API 명세**:

```
GET /stat
Authorization: Bearer ${STAT_INBOUND_TOKEN}

Response 200 OK:
  Content-Type: application/json
  Content-Encoding: gzip
  Body: <stat_snapshot envelope JSON>

Response 401 Unauthorized
Response 429 Too Many Requests: rate-limit 초과
Response 503 Service Unavailable: 수집 실패
```

**stat_snapshot envelope**:

```json
{
  "event_kind": "stat_snapshot",
  "cycle": { "host": "...", "host_id": "...", "boot_id": "...", "ts": "..." },
  "headers": { "total_sections": 7, "duration_ms": 850 },
  "body": [
    { "section": "metrics",      "data": { ... } },
    { "section": "processes",    "data": [ ... ] },
    { "section": "network",      "data": { ... } },
    { "section": "systemd",      "data": { ... } },
    { "section": "static_state", "data": { ... } },
    { "section": "config",       "data": { ... } },
    { "section": "hardware",     "data": { ... } }
  ]
}
```

`headers.total_sections`: 수신측이 받은 section 수와 비교해 데이터 잘림 감지. 부족하면 재요청.

**전체 응답 예시**: `docs/stat-report-response.00.example`

**운영 정책**:

| 항목 | 정책 |
|---|---|
| listen address | `127.0.0.1:9102` (외부 노출 안 함) |
| 인증 | Bearer 토큰 (env `${STAT_INBOUND_TOKEN}`) |
| rate-limit | default `30 requests/hour/source-ip` |
| 응답 timeout | 2초 (수집 + gzip + 응답까지) |

**`agent.yaml` 외부화**:
```yaml
stat_report:
  listen_addr: "127.0.0.1:9102"
  token_env: "STAT_INBOUND_TOKEN"
  rate_limit_per_hour: 30
  response_timeout_seconds: 2
  refresh_interval_seconds: 1800          # dynamic 섹션(metrics/processes/network/systemd) 30분 갱신
  refresh_static_interval_seconds: 21600  # static 섹션(hardware/config/static_state) 6시간 갱신
  refresh_offset_seconds: 900             # log_parser push 대비 +15분 오프셋
```

> `refresh_offset_seconds`는 log_parser의 `cycle_seconds`(default 1800)와 합산해 타이밍을 계산. 둘 다 1800 + 900 = 2700s 간격으로 보면 됨 — 항상 중간 지점에 위치.

---

### 7.9 sos_report — 사고 시 전체 포렌식 스냅샷

> **역할**: 사고 시 stat + logs를 통합한 전체 포렌식 스냅샷. 단일 동기 호출.

- **endpoint**: 127.0.0.1:9101, Bearer 인증
- **전송**: 단일 JSON, gzip (수집 완료 후 응답)

```
POST /trigger-sos
Authorization: Bearer ${SOS_TRIGGER_TOKEN}

Response 200 OK:
  Content-Type: application/json
  Content-Encoding: gzip
  Body: <sos_snapshot envelope JSON>

Response 401 Unauthorized
Response 409 Conflict:   이미 수집 중
Response 503 Service Unavailable
```

**sos_snapshot envelope**:

```json
{
  "event_kind": "sos_snapshot",
  "cycle": {
    "host": "...", "host_id": "...", "boot_id": "...",
    "ts": "..."
  },
  "headers": { "total_sections": 8, "duration_ms": 12000 },
  "body": [
    { "section": "metrics",      "data": { ... } },
    { "section": "processes",    "data": [ ... ] },
    { "section": "network",      "data": { ... } },
    { "section": "systemd",      "data": { ... } },
    { "section": "static_state", "data": { ... } },
    { "section": "config",       "data": { ... } },
    { "section": "hardware",     "data": { ... } },
    { "section": "logs",         "data": [ ... ] }
  ]
}
```

- **logs 섹션**: 수집 요청 기준 직전 2~4시간 로그 분석 결과. log_parser와 동일 형식, 시간 범위만 더 넓게
- `headers.total_sections`: 수신측이 받은 section 수와 비교해 데이터 잘림 감지

**전체 응답 예시**: `docs/sos-report-response.00.example`

**운영 정책**:

| 항목 | 정책 |
|---|---|
| listen address | `127.0.0.1:9101` (외부 노출 안 함) |
| 인증 | Bearer 토큰 (env `${SOS_TRIGGER_TOKEN}`) |
| cool-down | 같은 호스트·사유로 30분 내 재요청 차단 |
| 결과 보관 | 완료 후 1시간 유지, 이후 삭제 |

**`agent.yaml` 외부화**:
```yaml
sos_report:
  listen_addr: "127.0.0.1:9101"
  token_env: "SOS_TRIGGER_TOKEN"
  logs_lookback_hours: 3    # 2~4시간 범위, default 3
  cooldown_minutes: 30
  job_ttl_hours: 1
```

**3개 에이전트 공존 컨벤션**:
- 같은 `host_id` 사용 (모두 `/etc/machine-id` 참조) → 수신측에서 결과 묶기 가능
- 동일 systemd target에 묶어 함께 기동·종료
- 인증 토큰은 에이전트별 독립

### 7.10 수신측 약속 — 외부 인터페이스 명세

> 상세 내역: **`4_RECEIVER_CONTRACT.md`** 참조.
> (송출 contract, 수신측 권장 동작, schema 진화 정책, 구현 후보)

---

### 7.11 프로세스 안정성 관리 — Vector + Rust

> log_parser daemon은 `Vector` 자식 프로세스 + Rust 부모 프로세스 두 개로 동작. 한쪽이 죽으면 시스템이 깨지므로 안정성 관리 정책 필수.

#### 7.11.1 책임 분담

| 레벨 | 누가 | 무엇 |
|---|---|---|
| 시스템 (외부) | systemd | log_parser 바이너리 자체의 재시작 (`Restart=always`, `RestartSec=5s`) |
| 호스트 (내부) | Rust 부모 프로세스 (Coordinator + Agent 1) | Vector 자식 프로세스의 실행·종료 감지·재시작 |

#### 7.11.2 Vector 자식 프로세스 supervise (Rust 측)

- **실행**: Coordinator가 Vector를 자식 프로세스로 시작 (Agent 3의 vector.toml 사용). 자기 cgroup에 등록 (Agent 1)
- **종료 감지**: 별도 task가 Vector 자식 프로세스 종료 감시
- **비정상 종료 감지**: exit code != 0 또는 signal 종료
- **재시작 정책**:
  - exponential backoff: 1s → 2s → 4s → 8s → 16s → 32s (cap 60s)
  - max_restarts (default `5/hour`, `agent.yaml` 외부화)
  - max 초과 시 Rust 부모도 강제 종료 → systemd가 재시작
- **재시작 카운터 노출**: `envelope.headers.process_health.vector_restarts_24h` 필드로 수신측에 보고 (운영 가시성)

#### 7.11.3 Rust 부모 프로세스 (자기 자신)

- panic 시 process abort → systemd가 재시작
- 안전한 종료: SIGTERM 수신 → 처리 중인 envelope을 마저 완료하고 종료 (최대 30초)
- 비정상 종료 시 디스크 버퍼가 보존되어 다음 재시작 시 재전송 (Vector 측)

#### 7.11.4 systemd unit 권장값

```ini
[Service]
Type=simple
ExecStart=/usr/local/bin/log_parser --config /etc/log_parser/agent.yaml
Restart=always
RestartSec=5s
StartLimitBurst=5
StartLimitIntervalSec=300
# log_parser 자체가 cgroup self-attach하므로 systemd cgroup 설정은 불필요
```

#### 7.11.5 envelope.headers.process_health

```json
"process_health": {
  "vector_restarts_24h": 0,         // 24시간 내 Vector 재시작 횟수
  "agent_uptime_seconds": 86400,    // Rust 부모 가동 시간
  "last_vector_restart_ts": null    // 가장 최근 재시작 시각 (ISO8601 또는 null)
}
```

수신측 dashboard·alerting 룰에서 `vector_restarts_24h > 3` 같은 조건으로 운영 이상 감지.

---

### 7.12 RawEvent 내부 스키마

> struct 정의·freeze 정책 상세: `IMPL_NOTES.md §5`

Vector socket sink → `RawLogEvent` (`src/pipeline/raw_event.rs`).  
**Freeze**: Phase B Step 2 시작 전 freeze. 추가 필드는 `extra`로 흡수, 제거·타입 변경은 MASTER_PLAN 개정 + Normalizer 계약 재검토 필요.

---

## 8. 수집 범위 + 감시 영역 (log_parser 전담)

> Vector source별로 무엇을 수집하고 어떤 문제를 발견할 수 있는지 매핑. 판단은 모두 수신측. `categories.yaml` 기준 파일은 `3_AGENT_ROLES.md` Agent 4.
> `/proc`, `/sys` 수치(메모리 시계열, CPU/Load, Disk I/O, 네트워크 메트릭)와 시스템 상태(프로세스·네트워크·하드웨어·설정)는 stat_report / sos_report 담당 (§2.3 수집 범위 표 참조).

| 영역 | Vector source | 발견 가능한 문제 |
|------|---------------|----------------|
| OOM Kill | `journald` (kernel) | 프로세스 kill, 컨테이너 한도 초과 (`kernel.oom`) |
| 커널 패닉·버그 | `journald` (kernel) | 커널 패닉, BUG(), MCE (`kernel.panic`, `kernel.bug`, `hw.mce`) |
| 프로세스 크래시 | `journald` (kernel segfault) | 반복 크래시, 라이브러리 충돌, RAM 불량 (`process.crash`) |
| 파일시스템 에러 | `journald` + `file` | fs 손상, 읽기전용 전환, NFS 타임아웃 (`fs.error`, `fs.readonly`) |
| 디스크 에러 | `journald` | 디스크 I/O 에러, SMART 에러, 링크 에러 (`disk.io_error`, `disk.smart_error`, `disk.link_error`) |
| 네트워크 에러 | `journald` | NIC 에러, watchdog 타임아웃, 인터페이스 다운 (`net.error`, `net.watchdog`, `net.interface`) |
| systemd unit 실패 | `journald` | 재시작 루프, OOMKilled/Timeout, 의존성 연쇄 (`systemd.unit_failure`, `systemd.restart_loop`) |
| 인증·보안 이벤트 | `file` (`/var/log/auth.log` 또는 `/var/log/secure`) | 로그인 실패, sudo, 브루트포스 (`auth.failure`, `auth.event`, `auth.bruteforce`) |
| 감사 로그 | `file` (`/var/log/audit/audit.log`) | SELinux denial, 권한 에러, auditd 이벤트 (`selinux.denial`) |
| NTP / 시간 동기화 | `journald` / `file` | 시계 드리프트, NTP 끊김 (`ntp.drift` — categories.yaml 패턴으로 수집) |
| 컨테이너 OOM | `journald` + `file` (`/var/lib/docker/containers/*/*-json.log` ⚠️) | 컨테이너 메모리 한도 초과 (`container.oom`) |
| PSI | `file` (`/proc/pressure/*`, 선택·커널 4.20+) | CPU·메모리·I/O 압박 시점 특정 |

### 8.1 재시작 cursor 처리

| Source | Cursor 메커니즘 | 처리 |
|--------|----------------|------|
| `journald` | `journalctl --cursor` 자동 저장 | Vector 기본값 활용, `data_dir` 명시 |
| `file` | byte offset checkpoint | Vector 기본값 활용 |

→ Phase B Step 3에서 운영 config에 `data_dir` 명시.

---

## 10. Envelope 스키마 — 운영 명세

> **JSON 예시·불변 규칙은 `1_observability-design.md §4` 참조.** 본 §은 운영 명세(필드 의미 표 + 저장 정책 + 송출 규칙)에 집중.

### 10.0 통합 envelope 포맷 (3개 에이전트 공통)

모든 에이전트가 단일 JSON, 동일한 외형 사용:

```json
{
  "event_kind": "log_batch" | "stat_snapshot" | "sos_snapshot",
  "cycle":   { "host", "host_id", "boot_id", "ts", ... },
  "headers": { "total_sections": N, ... },
  "body":    [ { "section": "섹션명", "data": ... } ]
}
```

| 필드 | 의미 |
|---|---|
| `event_kind` | 에이전트 식별자. 수신측이 envelope 종류 구분에 사용 |
| `cycle` | 호스트 메타 + 수집 시점. 에이전트별 추가 필드 있음 (§10.1) |
| `headers` | 수신측이 빠르게 읽는 통계·hint. `total_sections` 필수 |
| `body[]` | 실제 수집 데이터. `section` 이름 + `data` 구조 |

`headers.total_sections`: 수신측이 받은 body 배열 길이와 비교해 데이터 잘림 감지. 부족하면 재요청.

### 10.1 헤더 필드

**cycle 공통 필드** (모든 에이전트):

| 필드 | 의미 |
|------|------|
| `cycle.host` | 표시용 hostname |
| `cycle.host_id` | 영구 식별자 (`/etc/machine-id`, 재부팅·IP 변경에 불변) |
| `cycle.boot_id` | 부팅 식별자 (`/proc/sys/kernel/random/boot_id`, 부팅마다 새 값) |
| `cycle.ts` | 수집 시작 시각 (ISO8601 UTC) |

**cycle 에이전트별 추가 필드**:

| 에이전트 | 추가 필드 |
|---|---|
| log_parser | `window` (ISO8601 시작/종료), `seq` (단조 증가, 영속화) |
| stat_report | — (없음) |
| sos_report | — (없음) |

**headers 에이전트별**:

| 에이전트 | headers 필드 |
|---|---|
| log_parser | `total_sections: 1`, `counts.by_severity`, `counts.by_category`, `process_health` |
| stat_report | `total_sections: 7`, `duration_ms` |
| sos_report | `total_sections: 8`, `duration_ms` |

**log_parser 전용**:

| 필드 | 의미 |
|------|------|
| `cycle.seq` | (host_id+boot_id) 안의 envelope 순번 (단조 증가, 영속화) |
| `headers.counts.by_severity / by_category` | severity 4단계 / category 키 셋 카운트. 이벤트 0건이면 0 또는 빈 객체 (empty cycle, §7.6.1) |
| `headers.process_health` | Vector 재시작 카운터 등 (§7.11.5) |

> 헤더는 빠른 판단용 요약값. 실제 데이터는 body.

**전체 응답 예시**: `docs/envelope-response.00.example`

#### 10.1.1 cycle.seq 영속화 정책

- 파일: `/var/lib/log_parser/seq.state` (atomic write: tmpfile rename + fsync)
- 재시작 시 파일에서 마지막 seq 읽어 +1부터 재개. 파일 없으면 seq=1
- **중복 방지 키**: `(host_id, boot_id, seq)` 3개. 같은 조합이 중복 도착하면 수신측에서 한 번만 처리
- `host_id`: `/etc/machine-id` 우선 → `agent.yaml` override → 없으면 기동 거부
- `boot_id`: `/proc/sys/kernel/random/boot_id` → 컨테이너 접근 불가 시 fallback 설정 가능

### 10.2 Body 이벤트 필드 (모든 항목 동일 schema)

| 필드 | 의미 |
|------|------|
| `source` / `severity` / `category` | 출처(`journald.kernel`/`file.syslog`/`file.auth`/`file.audit`/`file.docker`) / 4단계 / `categories.yaml` 룩업 |
| `fingerprint` / `template` | template 해시(수신측 dedup 키) / 가변 토큰 placeholder |
| `sample_raws` / `fields` | 디버깅용 원본 ≤3개 / 구조화 필드 (PID, IP, unit 등) |
| `ts_first` / `ts_last` / `count` | dedup 윈도우 내 처음·마지막 시각, 발생 횟수 |

### 10.3 송출 규칙

- **Push (자동)**: 30분 cycle마다 envelope 1건 (`agent.yaml`로 cycle 길이 외부화)
- **Pull (on-demand)**: `POST /flush` 시 즉시 envelope 1건 + 새 cycle 시작
- **단일 endpoint**: 모두 같은 outbound endpoint (http_json: Bearer + gzip / otlp: gRPC Authorization metadata)
- **at-least-once**: Vector disk buffer + Rust spool, 수신측 idempotency는 `cycle.seq`로

---

## 12. 설계 원칙 + 트레이드오프

> **무엇을·왜의 의미 원칙은 `1_observability-design.md §1`** 참조.

### 12.1 5원칙 + 구현 방법

| 설계 원칙 | 구현 방법 |
|-----------|----------|
| **#1 Critical 절대 drop 금지** | Vector `buffer.type=disk` + `when_full=block` + `acknowledgements`, Rust 측 디스크에 저장되는 재시도 대기열. 모든 critical 로그가 envelope.body로 전달됨. |
| **#2 에이전트가 장애 악화 금지** | cgroup v2 자가 격리 (`agent.yaml`로 외부화, default 128MB/5%). 분석 컴포넌트 폐기로 메모리 변동 폭 감소. |
| **#3 판단하지 않는다** | 호스트는 정제·수집·송출만 (Normalize, dedup, 태깅, envelope 조립). 패턴 매칭·임계값·집계·정렬·트리거 결정 모두 수신측. |
| **#4 설정 없이도 기본값 동작** | `agent.yaml` 모든 키에 기본값. `categories.yaml` 없어도 안전한 default 카테고리 매핑. |
| **#5 송출 타이밍 신호만 외부 허용** | outbound: 30분 cycle push. inbound: `POST /flush` (송출 타이밍 신호). 호스트 동작·설정 변경 명령은 불가. |

### 12.2 허용하는 트레이드오프

| 항목 | 이유 |
|------|------|
| 수집 시각 오차 ±500ms | 초 단위 이하는 결론에 영향 없음 |
| Info/Warning 일부 drop | critical/error가 핵심 |
| 카테고리 매핑이 호스트 측 (`categories.yaml`) | 운영 일관성 + 빠른 활용. 새 카테고리 추가는 config + 재시작 |
| Push cycle 길이 30분 default | 수신측이 더 빠른 데이터 원하면 `/flush` 호출 가능 |
| 외부 → 호스트 동작 변경 명령 없음 | 단순성·보안. 송출 타이밍 신호만 허용 |
| 분석은 수신측 부담 | 호스트는 정제·수집·송출만. 수신측 자유도·일관성 우선 |

### 12.3 설계 우선순위

```
1순위  에이전트 안정성       — 죽으면 아무것도 수집 안 됨
2순위  Critical 이벤트 보존   — 원인 분석의 핵심 증거
3순위  수집 정확도            — 타임스탬프 오차, 주기 적합성
4순위  처리 성능              — 위 항목보다 후순위
```

---

## 13. 디렉토리 구조

```
log_parser/
├── Cargo.toml
├── config/
│   ├── vector.toml          # Vector 운영 설정 (§7.2 + §7.2.1 IPC 설정 포함)
│   ├── agent.yaml           # cycle / dedup / cgroup / transport / inbound / pipeline
│   └── categories.yaml      # 카테고리 패턴→이름 매핑
├── src/
│   ├── main.rs              # 진입점: cgroup self-attach → 모듈 기동
│   ├── config.rs            # agent.yaml + categories.yaml 로드·검증·기본값
│   ├── envelope.rs          # 표준 Envelope 스키마 (§10) — 단일 형식 (headers + body)
│   ├── platform/            # cgroup.rs · discovery.rs · capability.rs
│   ├── pipeline/
│   │   ├── raw_event.rs     # RawEvent enum (§7.11) — RawLogEvent
│   │   └── vector_receiver.rs  # tokio UnixListener — Unix socket (events_critical/normal)
│   ├── normalize/           # tokens.rs · severity.rs · categories.rs
│   ├── dedup/window.rs      # 30s 윈도우 + LRU 50K
│   ├── coordinator/
│   │   └── envelope.rs      # cycle 만료/flush 시 조립 (§7.6 + body overflow §7.6.2)
│   ├── transport/           # mod.rs · http.rs · otlp.rs · file.rs (outbound)
│   └── inbound/flush.rs     # POST /flush (Bearer, 127.0.0.1:9100)
├── src/stat_report/         # GET /stat (127.0.0.1:9102)
└── src/sos_report/          # POST /trigger-sos (127.0.0.1:9101)
```

---

## 14. 성능 설계 및 2차 문제 대응

### 14.1 수집 주기

| 주기 | 항목 |
|------|------|
| **이벤트 기반** (연속) | `journald` follow — dmesg, systemd unit 이벤트 실시간 tail |
| **이벤트 기반** (연속) | `file` source tail — syslog/messages, auth.log/secure, audit.log, docker container 로그 |
| **30분** | envelope 조립·송출 (Coordinator) |

### 14.2 처리 파이프라인 — 단일 채널

```
Vector → [channel] → Normalizer → [channel] → Dedup → [channel]
                                                        │
                                                        ▼
                                                Coordinator (envelope 조립)
                                                        │  cycle 만료 또는 flush trigger
                                                        ▼
                                                    [channel]
                                                        │
                                                        ▼
                                                   Transport
                                                   (push or /flush 응답)
```

채널 가득 참 → info → warning → error 순으로 drop. critical은 절대 drop 금지.
**단일 흐름** — 분석 분기 없음으로 처리 흐름 구조 단순화.

### 14.3 에이전트 자체 리소스 캡 — 측정 검증

| 항목 | 측정값 | 판정 |
|------|--------|------|
| memory.max=128M cgroup | 200MB 폭주 시 cgroup 내부 OOM | ✅ |
| cpu.max=5% | 정확히 5% 강제 | ✅ |
| 폭주 시 시스템 영향 | +5MB | ✅ |
| dmesg에 OOM 기록 | 없음 (사일런트) | ✅ |

---

---

## 17. 열린 질문 / 리스크

| # | 항목 | 상태 |
|:-:|------|------|
| 1 | PII/민감정보 마스킹 | Phase B 외부 — 운영 학습 후 |
| 2 | Multi-line 이벤트 (스택트레이스) | Phase B 외부 — 앱 로그 수집 시 추가 |
| 3 | 컨테이너 환경 | Phase B 외부 — Docker json-file 로그 ⚠️ 추가 필요 |
| 4 | `categories.yaml` 권장 셋 | 운영 1주차 후 기준값 측정 후 조정 |
| 5 | sos_report / stat_report 구현 우선순위 | Phase B 이후 별도 스텝. Phase B §18.5는 mock으로 검증 |
| 6 | 수신측 alerting 룰 권장 셋 | 본 프로젝트 외부 — 운영 팀 담당 |
| 7 | 호스트 동작 변경 명령 도입 여부 | 현재 미도입. 운영 학습 후 재평가 |

**환경 검증 완료** (2026-05-06): Rust 1.95.0 / cgroup v2 / Vector 0.55.0 / journald / `/var/log/syslog` / `/etc/machine-id` / `boot_id` — 모두 ✅  
미완: Vector 재시작 cursor 동작 확인·Docker container log source 검증 → Phase B Step 3

---

> **Phase B 실행 계획 + 용어집** → `3_PHASE_B.md`
