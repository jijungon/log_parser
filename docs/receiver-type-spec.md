# 수신측 서버 — Information Type Spec

`POST /ingest`로 수신하는 모든 데이터 타입의 정형화된 스펙.
타입 표기는 TypeScript 스타일을 사용하며, 실제 전송 포맷은 **gzip 압축된 JSON**입니다.

---

## 목차

1. [전송 방식](#1-전송-방식)
2. [Envelope — 공통 외부 구조](#2-envelope--공통-외부-구조)
3. [event_kind별 타입](#3-event_kind별-타입)
   - [log_batch](#31-log_batch--30분-자동-push)
   - [stat_snapshot](#32-stat_snapshot--get-stat-응답)
   - [sos_snapshot](#33-sos_snapshot--post-trigger-sos-응답)
4. [DedupEvent](#4-dedupevent)
5. [섹션 타입 상세](#5-섹션-타입-상세)
   - [metrics](#51-metrics)
   - [processes](#52-processes)
   - [network](#53-network)
   - [systemd](#54-systemd)
   - [static_state](#55-static_state)
   - [config](#56-config)
   - [hardware](#57-hardware)
6. [식별자 타입](#6-식별자-타입)
7. [Optional 필드 처리 규칙](#7-optional-필드-처리-규칙)
8. [섹션 구성 요약](#8-섹션-구성-요약)

---

## 1. 전송 방식

```
POST <transport.endpoint>
Authorization: Bearer <PUSH_OUTBOUND_TOKEN>
Content-Type: application/json
Content-Encoding: gzip
```

- Body는 gzip 압축된 JSON. 수신 후 먼저 압축 해제 후 파싱.
- 수신 성공 응답: **2xx 전부** (200/202/204 등 — 파서는 `is_success()`로만 판정). 응답 코드별 파서 반응·재시도·자동 drain 의미론은 [receiver-implementation-guide.md](receiver-implementation-guide.md) §2~3.
- 중복 수신 방지 키: `(host_id, boot_id, seq)` 세 값이 모두 동일하면 동일 데이터.
  - `stat_snapshot` / `sos_snapshot`은 `seq` 없음 → upsert 처리 권장.
- 현재 envelope에는 `schema_version` 필드가 **없다** (도입 제안: [architecture-review.md](architecture-review.md) §4-4). 모르는 키는 무시하고 계속 처리할 것.

---

## 2. Envelope — 공통 외부 구조

모든 요청이 공유하는 최상위 구조.

```typescript
interface Envelope {
  event_kind: "log_batch" | "stat_snapshot" | "sos_snapshot"
  cycle:      Cycle
  headers:    Headers
  body:       Section[]
}

interface Cycle {
  host:     string   // 호스트명 (hostname)
  host_id:  string   // machine-id 기반 hex 문자열 — 재설치 전까지 불변
  boot_id:  string   // UUID 형식 — 재부팅마다 변경
  ts:       string   // RFC3339 — cycle 시작 타임스탬프
  window?:  string   // "시작/종료" ISO8601 구간 — log_batch 전용
  seq?:     number   // u64 — 단조 증가 순번 — log_batch 전용. seq.state 파일로 영속화되어
                     //       에이전트 재시작 후에도 이어짐 (상태 파일 유실 시에만 1로 리셋)
}

interface Headers {
  total_sections:   number
  counts?:          Counts         // log_batch 전용
  process_health?:  ProcessHealth  // log_batch 전용
  duration_ms?:     number         // 수집 소요 시간(ms)
}

interface Counts {
  by_severity: {
    critical: number
    error:    number
    warn:     number
    info:     number
  }
  by_category: Record<string, number>  // category 코드 → 이벤트 수
}

interface ProcessHealth {
  vector_restarts_24h:   number  // 최근 24시간 Vector 재시작 횟수
  agent_uptime_seconds:  number  // 에이전트 가동 시간(초)
}

interface Section {
  section: string           // 섹션 이름
  data:    SectionData      // 섹션별 타입 (아래 상세 참조)
}
```

---

## 3. event_kind별 타입

### 3.1 log_batch — 30분 자동 push

30분마다 에이전트가 자동으로 전송. `body`에 `logs` 섹션 1개 포함.

```typescript
interface LogBatchEnvelope extends Envelope {
  event_kind: "log_batch"
  cycle: Cycle & { window: string; seq: number }
  headers: Headers & { counts: Counts; process_health: ProcessHealth }
  body: [LogsSection] | []   // 이벤트 0건 cycle은 logs 섹션 없이 body: [] (total_sections: 0)
}

interface LogsSection {
  section: "logs"
  data:    DedupEvent[]   // 1개 이상 — 이벤트 0건이면 섹션 자체가 생략됨 (위 참조)
}
```

**수신측 처리 포인트**

| 조건 | 권장 대응 |
|---|---|
| `headers.counts.by_severity.critical > 0` | 즉시 알림 |
| `body`가 빈 배열 (`total_sections: 0`) | 호스트 정상 alive 신호 — 별도 처리 불필요 |
| `seq` 구멍 | 즉시 유실 판정 금지 — 파킹분(`retry/`)은 수신 복구 후 **자동 drain으로 늦게 도착**한다 (TTL 168h 이내). **도착 순서 ≠ 발생 순서** 전제, TTL 경과 후 잔존 구멍만 유실 확정 ([receiver-implementation-guide.md](receiver-implementation-guide.md) §3) |
| 35분 이상 수신 없음 | 호스트 이상 — 에이전트 다운 또는 네트워크 단절 |

---

### 3.2 stat_snapshot — GET /stat 응답

수신측이 `GET :9100/stat`을 호출했을 때 반환. `seq` 없음.

```typescript
interface StatSnapshotEnvelope extends Envelope {
  event_kind: "stat_snapshot"
  cycle: Cycle  // window, seq 없음
  headers: Headers & { duration_ms: number }
  body: [
    MetricsSection,
    ProcessesSection,
    NetworkSection,
    SystemdSection,
    StaticStateSection,  // static_state.enabled=false(기본 true)면 이 섹션 자체가 생략 → 6개
    ConfigSection,
    HardwareSection,
  ]  // 기본 7개 섹션, 순서 보장
}
```

---

### 3.3 sos_snapshot — POST /trigger-sos 응답

수신측이 `POST :9100/trigger-sos`를 호출했을 때 반환.
stat_snapshot 7개 섹션 + `logs` 1개 = **총 8개 섹션**.
`logs` 섹션에는 최근 4시간 로그(최대 500개 DedupEvent, ts_first 내림차순)가 포함됨.

```typescript
interface SosSnapshotEnvelope extends Envelope {
  event_kind: "sos_snapshot"
  cycle: Cycle  // window, seq 없음
  headers: Headers & { duration_ms: number }
  body: [
    MetricsSection,
    ProcessesSection,
    NetworkSection,
    SystemdSection,
    StaticStateSection,  // static_state.enabled=false(기본 true)면 이 섹션 자체가 생략 → 7개
    ConfigSection,
    HardwareSection,
    LogsSection,  // 최근 4시간, 최대 500개
  ]  // 기본 8개 섹션, 순서 보장
}
```

> 로그 파일 크기에 따라 수 초~수십 초 소요. HTTP 클라이언트 타임아웃 **120초 이상** 설정 필요.

---

## 4. DedupEvent

`log_batch`와 `sos_snapshot`의 `logs` 섹션 각 원소.
동일한 패턴의 로그를 하나로 묶어 전달하는 단위.

```typescript
interface DedupEvent {
  source:      "journald" | "file.syslog" | "file.auth" | "file.audit"
               // push(log_batch) 기준. sos_snapshot의 logs(파일 재파싱)는 file.syslog | file.auth 만
  severity:    "critical" | "error" | "warn" | "info"
  category:    CategoryCode       // 아래 분류표 참조
  fingerprint: string             // 16자리 hex — 동일 패턴의 고유 ID
  template:    string             // 가변값이 placeholder로 치환된 정규화 문자열
  sample_raws: string[]           // 원본 로그 라인 샘플 (기본 최대 3개 — dedup.sample_raws_cap 설정)
  fields:      Record<string, JsonValue>  // 추출된 구조화 필드
  ts_first:    string             // RFC3339 — 패턴 최초 발생 시각
  ts_last:     string             // RFC3339 — 패턴 마지막 발생 시각
  count:       number             // u64, 1 이상 — dedup 윈도우 내 중복 횟수
}
```

**Template Placeholder 규칙** (정본: `src/normalize/tokens.rs` — 구체적 패턴 우선 순서)

| Placeholder | 치환 대상 |
|---|---|
| `<UUID>` | UUID |
| `<IP4>` / `<IP6>` | IPv4 / IPv6 주소 |
| `SHA256:<FPR>` | SSH 키 지문 (`SHA256:` + base64 20자 이상) |
| `[<ADDR>]` | 커널 스택 프레임 주소 (`[<ffffffff8100abcd>]`) |
| `<CID>` | Docker 컨테이너 ID (`docker-<hex>`) |
| `<VETH>` / `<BR>` | veth 가상 인터페이스명 / 도커 브리지명 (`br-<hex12>`) |
| `<MNT>` | overlay2·buildkit·runc·netns 마운트 랜덤 ID |
| `<HEX>` | MAC 주소, `0x` 접두 긴 16진수 |
| `<PATH>` | 파일시스템 경로 (2단계 이상) |
| `<DEV>` | 블록 디바이스명 (sda1, nvme0n1, vdb, loop0 등) |
| `<NUM>` | 숫자 (PID, 포트, 횟수 등 — 가장 마지막에 적용) |

> `<WORD>`(단일 알파벳 토큰) placeholder는 **없다** — 일반 단어는 원문 그대로 남는다 (예: `Out of memory: Killed process <NUM> (java)`).

**Category 분류표** (정본: [`../config/categories.yaml`](../config/categories.yaml) — first-match-wins, 파일 수정 + 재시작으로 코드 변경 없이 추가 가능. 아래는 대표 패턴 요약)

| CategoryCode | 탐지 패턴 | 의미 |
|---|---|---|
| `kernel.oom` | `Out of memory: Killed` | 커널 OOM Killer 발동 |
| `kernel.bug` | `kernel BUG at` | 커널 버그 |
| `kernel.panic` | `panic:`, `Kernel panic` | 커널 패닉 |
| `process.crash` | `segfault at`, `general protection fault` | 세그폴트 |
| `fs.error` | `EXT4-fs error`, `XFS: Internal error` | 파일시스템 오류 |
| `fs.readonly` | `remounting filesystem read-only` | 파일시스템 읽기전용 전환 |
| `hw.mce` | `EDAC MC`, `Hardware Error`, `Machine Check` | 하드웨어 MCE 오류 |
| `disk.smart_error` | `SMART.*Threshold.*Exceeded` | 디스크 SMART 임계값 초과 |
| `disk.io_error` | `I/O error`, `blk_update_request` | 디스크 I/O 오류 |
| `disk.link_error` | `SATA link down` | 디스크 링크 오류 |
| `net.error` | `TCP: out of memory`, `nf_conntrack: table full` | 네트워크 오류 |
| `net.watchdog` | `NETDEV WATCHDOG` | NIC 워치독 |
| `systemd.unit_failure` | `Failed to start`, `failed with result` | 서비스 시작 실패 |
| `systemd.restart_loop` | `Start request repeated too quickly` | 서비스 재시작 루프 |
| `auth.failure` | `authentication failure`, `Failed password` | 인증 실패 |
| `auth.event` | `Accepted publickey` | 인증 성공 이벤트 |
| `auth.bruteforce` | `POSSIBLE BREAK-IN ATTEMPT` | 브루트포스 의심 |
| `session.activity` | `New session <N> of user`, `pam_unix(cron:session)` 등 | 정상 세션 생성·종료 활동 (CRON·logind) |
| `ntp.drift` | `System clock wrong`, `time stepped` | 시간 동기화 오류 |
| `container.oom` | `Memory cgroup out of memory` | 컨테이너 OOM |
| `selinux.denial` | `avc: denied`, `type=AVC` | SELinux/AppArmor 차단 |
| `system.general` | *(위 패턴 미매칭 전체)* | 일반 로그 |

**fields 추출 예시**

| category | 추출 필드 |
|---|---|
| `kernel.oom` | `pid: number` |
| `disk.io_error` | `dev: string` |
| `auth.failure` | `user: string` |
| `systemd.unit_failure` | `unit: string` |

---

## 5. 섹션 타입 상세

### 5.1 metrics

```typescript
interface MetricsSection {
  section: "metrics"
  data: {
    cpu: {
      usage_pct:   number | null  // null: /proc/stat 읽기 실패
      iowait_pct:  number | null
      user_pct:    number | null
      system_pct:  number | null
    }
    memory: {
      total_mb:     number
      used_mb:      number
      free_mb:      number
      available_mb: number
      swap_total_mb: number
      swap_used_mb:  number
    }
    disk_io: Record<string, {  // key: 디바이스명 (sda, nvme0n1 등, 파티션 제외)
      reads_per_sec:  number
      writes_per_sec: number
      util_pct:       number   // 0.0 ~ 100.0
    }>
    network: Record<string, {  // key: 인터페이스명 (lo 제외)
      rx_bytes_per_sec: number
      tx_bytes_per_sec: number
    }>
    load_avg: {
      "1m":  number
      "5m":  number
      "15m": number
    }
    pressure: {  // PSI — 커널 미지원 시 빈 객체 {}
      cpu:    { some_pct: number; full_pct: number }
      memory: { some_pct: number; full_pct: number }
      io:     { some_pct: number; full_pct: number }
    }
  }
}
```

> CPU / disk_io / network 수치는 **200ms 샘플링 간격 기반 순간값**입니다.

---

### 5.2 processes

```typescript
interface ProcessesSection {
  section: "processes"
  data: ProcessInfo[]  // mem_rss_mb 내림차순, 최대 20개
}

interface ProcessInfo {
  pid:        number
  name:       string   // /proc/{pid}/stat의 comm 필드
  user:       string   // UID → /etc/passwd 조회, 실패 시 UID 숫자 문자열
  cpu_pct:    number   // 프로세스 시작 이후 누적 CPU 사용률 (순간값 아님)
  mem_pct:    number   // VmRSS / MemTotal × 100
  mem_rss_mb: number
  state:      string   // R(running) S(sleeping) D(io-wait) Z(zombie) T(stopped)
  threads:    number
  open_files: number   // /proc/{pid}/fd 디렉토리 파일 수
  start_time: string   // RFC3339
}
```

---

### 5.3 network

```typescript
interface NetworkSection {
  section: "network"
  data: {
    connections: {
      established: number
      time_wait:   number
      close_wait:  number
    }
    sockstat: {
      tcp_alloc: number
      udp_inuse: number
    }
    interfaces: Record<string, {  // key: 인터페이스명 (lo 제외)
      state: "UP" | "DOWN"
      mtu:   number
    }>
  }
}
```

---

### 5.4 systemd

```typescript
interface SystemdSection {
  section: "systemd"
  data: SystemdUnit[]  // failed + active 서비스 유닛
}

interface SystemdUnit {
  unit:     string  // 유닛명 (예: "sshd.service")
  load:     string  // "loaded" | "not-found" | "masked" 등
  active:   string  // "active" | "failed" | "inactive" 등
  sub:      string  // "running" | "dead" | "exited" 등
  restarts: number  // 현재 구현에서는 항상 0
  main_pid: null    // 현재 구현에서는 항상 null
  since:    string  // 현재 구현에서는 항상 ""
}
```

---

### 5.5 static_state

```typescript
interface StaticStateSection {
  section: "static_state"
  data: {
    selinux: {
      mode: "enforcing" | "permissive" | "disabled"
    }
    sysctl: {
      "vm.overcommit_memory": string
      "vm.swappiness":        string
      "kernel.panic":         string
      "net.core.somaxconn":   string
      "net.ipv4.tcp_tw_reuse": string
    }
    kernel_modules: string[]  // /proc/modules 기준, 최대 50개
    cmdline:        string    // /proc/cmdline
    ntp: {
      reference:  string  // NTP 서버 IP 또는 hostname
      offset_ms:  number  // 시스템 시각 오차(ms)
      stratum:    number
    } | null  // chronyc / ntpq 모두 사용 불가 시 null
  }
}
```

---

### 5.6 config

```typescript
interface ConfigSection {
  section: "config"
  data: {
    files: {
      "/etc/sysctl.conf"?: string  // 최대 4096 bytes, 파일 없으면 키 생략
      "/etc/hosts"?:       string
      "/etc/hostname"?:    string
    }
    packages: {
      total:  number
      sample: Array<{ name: string; version: string }>  // 최대 5개
    }
  }
}
```

---

### 5.7 hardware

```typescript
interface HardwareSection {
  section: "hardware"
  data: {
    cpu: {
      model:            string
      sockets:          number
      cores_per_socket: number
      threads:          number  // 하이퍼스레딩 포함 전체 논리 코어 수
    }
    memory: {
      total_mb: number
    }
    disks: Array<{
      dev:          string  // 디바이스명 (sda, nvme0n1 등)
      model:        string
      size_gb:      number
      smart_status: "UNKNOWN"  // 현재 구현에서는 항상 "UNKNOWN"
    }>
    pci: Array<{
      class:  string  // PCI 클래스 (예: "Network controller")
      device: string  // 디바이스 설명
    }>  // lspci 기준, 최대 20개. lspci 미설치 시 빈 배열
  }
}
```

---

## 6. 식별자 타입

| 필드 | 형식 | 예시 | 특성 |
|---|---|---|---|
| `host_id` | hex 문자열 (32자) | `"a8f3c1b94d2e4f1ab8c31234567890ab"` | /etc/machine-id 기반, 재설치 전 불변 |
| `boot_id` | UUID 형식 (36자) | `"f1a2b3c4-5d6e-7f8a-9b0c-abcdef012345"` | /proc/sys/kernel/random/boot_id 기반, 재부팅마다 변경 |
| `fingerprint` | hex 문자열 (16자) | `"a3f1c9e2b847d056"` | xxHash3_64 기반, PID·IP가 달라도 같은 패턴이면 동일 값 |
| `seq` | u64 정수 | `42` | `seq.state` 파일로 영속화 — 에이전트 재시작 후에도 이어짐(상태 파일 유실 시에만 1로 리셋). `(host_id, boot_id)`와 함께 중복 방지 키 |

---

## 7. Optional 필드 처리 규칙

- `?` 표시 필드는 JSON 키 자체가 생략될 수 있음 (`null`이 아닌 키 없음).
- 수신측은 `envelope.get("cycle").get("seq")` 처럼 키 존재 여부를 먼저 확인해야 함.
- 알 수 없는 키는 **무시하고 계속 처리**. 에이전트 업데이트 시 키가 추가될 수 있음.
- `total_sections`는 항상 `body` 배열 길이와 일치.
- `body`가 빈 배열(`total_sections: 0`)인 경우 alive 신호로만 처리.

---

## 8. 섹션 구성 요약

| event_kind | 섹션 수 | 포함 섹션 |
|---|:---:|---|
| `log_batch` | 1 (빈 cycle은 0) | `logs` (이벤트 0건이면 섹션 생략, `body: []`) |
| `stat_snapshot` | 7 (`static_state` 비활성 시 6) | `metrics`, `processes`, `network`, `systemd`, `static_state`, `config`, `hardware` |
| `sos_snapshot` | 8 (`static_state` 비활성 시 7) | `metrics`, `processes`, `network`, `systemd`, `static_state`, `config`, `hardware`, `logs` |
