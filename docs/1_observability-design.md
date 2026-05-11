# 계층형 관측성(Observability) 설계

호스트(예: Grafana on Docker VM)의 관측성 데이터를 어떻게 수집·송출·보존할지에 대한 설계.
"스트림(로그)·스냅샷(snapshot)"은 본질이 다른 데이터이므로
**한 도구로 합치지 않고 역할별 분리**로 다룬다.

스트림 계층은 단일 envelope으로 정제된 log을 수집·송출한다. 호스트는 분석하지 않고 정제·수집·송출만 한다.

---

## 1. 핵심 원칙 (의미 SSoT)

> 본 §은 **무엇을·왜** 분리하는가에 관한 의미 원칙. **어떻게 지키는가의 구현 원칙은 `2_MASTER_PLAN.md §12`** 참조.

1. 두 종류의 데이터를 **각자 알맞은 도구**로 분리
   - 로그 스트림 = log_parser → 단일 endpoint (30분 push + POST /flush)
   - 현재 상태 = stat_report → 수신측 GET /stat 요청 시 즉시
   - 포렌식 스냅샷 = sos_report → 수신측 POST /trigger-sos → sos_snapshot 반환 (단일 동기 호출)
2. 스트림 계층(log_parser)은 **정제·수집·송출** 담당
   - 정제: 토큰 정규화, 중복 제거, severity·category 태깅
   - 송출: 단일 envelope 형식 (headers + body)
   - **분석·집계·트리거 판단은 모두 수신측**
3. 송출 메커니즘은 **push + on-demand pull**
   - 평시 push: log_parser가 30분 cycle마다 envelope 자동 송출
   - 사고 시 pull: 수신측이 stat_report·log_parser·sos_report에 각각 호출
4. **단일 envelope 형식** — 분기 없음
5. 외부 → 호스트는 **"송출 타이밍 신호"만** 허용 (flush/sos 트리거)
   - 호스트 동작·설정 변경 명령은 불가

---

## 2. 3 에이전트 데이터 모델

| 에이전트 | 본질 | 수집 내용 | 언제 |
|---|---|---|---|
| **log_parser** | 시간순 로그 이벤트 스트림 | journal, `/var/log/*`, 컨테이너 로그 | 30분 자동 push + `POST /flush` |
| **stat_report** | 현 시점 수치·상태 스냅샷 | CPU/메모리/디스크 수치, 프로세스, 네트워크, 설정 파일, 하드웨어 | 수신측 `GET /stat` 요청 시 즉시 |
| **sos_report** | 사고 시점 전체 포렌식 스냅샷 | stat 7개 섹션 + 직전 2~4시간 로그 분석 | 수신측 `POST /trigger-sos` → sos_snapshot 반환 |

**관계**: sos = stat + log_parser (sos body = stat 7개 섹션 + logs 1개 섹션)

3개 에이전트는 **서로 모름**. 수신측이 `host_id`로 결과를 묶어 사고 분석.

---

## 3. 에이전트별 역할

| 에이전트 | 역할 | 수집 대상 | 언제 | port |
|---|---|---|---|---|
| log_parser | 로그 스트림 정제·송출 | journal, `/var/log/*` | 30분 자동 push + `POST /flush` | 9100 |
| stat_report | 현재 상태 스냅샷 | `/proc`·`/sys`·설정·하드웨어 | `GET /stat` on-demand | 9102 |
| sos_report | 사고 포렌식 스냅샷 | stat 7섹션 + 직전 2~4시간 로그 | `POST /trigger-sos` on-demand | 9101 |

사고 시 수신측은 세 에이전트를 병렬 호출하고 `host_id`로 결과를 묶어 분석한다 (흐름은 §7 참조).

> 구현 세부사항·모듈 위치·완료 기준 → **`3_AGENT_ROLES.md`** / **`2_MASTER_PLAN.md §2`** 참조.

---

## 4. Envelope 공통 형식 (3개 에이전트)

모든 에이전트가 동일한 외형 사용:

```json
{
  "event_kind": "log_batch" | "stat_snapshot" | "sos_snapshot",
  "cycle":   { "host", "host_id", "boot_id", "ts", ... },
  "headers": { "total_sections": N, ... },
  "body":    [ { "section": "섹션명", "data": ... } ]
}
```

**log_parser 예시** (`event_kind="log_batch"`):

```json
{
  "event_kind": "log_batch",
  "cycle": {
    "host": "app-prod-03",
    "host_id": "a8f3c1b9-...",
    "boot_id": "f1a2b3c4-...",
    "ts": "2026-05-06T10:30:00Z",
    "window": "2026-05-06T10:00:00Z/2026-05-06T10:30:00Z",
    "seq": 1234
  },
  "headers": {
    "total_sections": 1,
    "counts": {
      "by_severity": { "critical": 1, "error": 47, "warn": 32, "info": 1247 },
      "by_category": { "kernel.oom": 1, "fs.error": 2 }
    },
    "process_health": { "vector_restarts_24h": 0, "agent_uptime_seconds": 86400 }
  },
  "body": [
    {
      "section": "logs",
      "data": [
        {
          "source": "journald.kernel",
          "severity": "critical",
          "category": "kernel.oom",
          "fingerprint": "a3f1c9...",
          "template": "Out of memory: Killed process <NUM> (<WORD>)",
          "sample_raws": ["Out of memory: Killed process 2481 (java)"],
          "ts_first": "2026-05-06T10:13:42Z",
          "ts_last":  "2026-05-06T10:13:42Z",
          "count": 1
        }
      ]
    }
  ]
}
```

**불변 규칙**:
- 3개 에이전트 모두 동일한 `event_kind + cycle + headers + body` 외형.
- `headers.total_sections`: 수신측이 받은 body 배열 길이와 비교해 데이터 잘림 감지.
- `headers`는 빠른 판단용 요약값. 실제 데이터는 body.
- body는 `[{section, data}]` 배열 구조. 에이전트마다 섹션 구성이 다름.
- log_parser: 이벤트 0건이어도 envelope 반드시 송출 (`body: [{section:"logs", data:[]}]`).

필드 상세·에이전트별 차이는 `2_MASTER_PLAN.md §10` 참조.

---

## 5. 카테고리 태깅 (정제 영역)

호스트는 `categories.yaml` 룩업으로 각 이벤트에 category 태그를 단다. 도메인 의미를 호스트가 미리 분류해 수신측 부담을 줄인다.

```yaml
# /etc/log_parser/categories.yaml
- pattern: "Out of memory: Killed"
  category: kernel.oom
- pattern: "EXT4-fs error"
  category: fs.error
- pattern: "remounting filesystem read-only"
  category: fs.readonly
- pattern: "EDAC MC|Hardware Error"
  category: hw.mce
# ... (운영 환경별 추가)
```

**중요**: 카테고리 매핑은 **외부 config**. 새 카테고리 추가 시 코드 변경·재배포 불필요. config 수정 + agent 재시작.

수신측이 새 카테고리 분류를 원하면 body의 `template`을 자체 분석 가능 — 호스트 매핑은 hint에 가까움.

---

## 6. severity 태깅 (정제 영역)

PRIORITY 또는 패턴 기반 단순 매핑:

| severity | 매핑 기준 |
|---|---|
| critical | journald PRIORITY 0~2, 또는 `Out of memory: Killed`·`kernel BUG`·`panic:` 같은 키워드 |
| error | PRIORITY 3, "ERROR" 라인, `segfault`·`I/O error` 등 |
| warn | PRIORITY 4, "WARN" 라인 |
| info | PRIORITY 5~7, "INFO"·"DEBUG" |

severity는 카테고리와 별개의 라벨. headers.counts.by_severity로 분포 요약.

---

## 7. 송출 시간선 — 평시 vs 사고

### 평시 1시간 (조용함)

```
10:30 ──[push envelope #1: critical=0, error=2]──→ endpoint
11:00 ──[push envelope #2: critical=0, error=1]──→ endpoint

총: 자동 push 2건 (~수십 KB)
```

### 사고 발생한 1시간

```
10:13:42  호스트에서 OOM 발생
          → log_parser는 그저 dedup·태깅 후 body에 누적

10:30     ──[push envelope #1: critical=1, kernel.oom=1, ... + body 안에 OOM 라인]──→ endpoint
          수신측이 envelope.body 분석:
            "severity=critical AND category=kernel.oom 발견 → 상세 수집 필요"

10:30:05  수신측이 호스트에 병렬 호출:
          ┌─ GET  host:9102/stat          ──→ stat_report 응답 (현재 상태 7섹션)
          ├─ POST host:9100/flush         ──→ log_parser 응답 (envelope #1.5)
          └─ POST host:9101/trigger-sos   ──→ sos_report 응답 (8섹션 포렌식)
          수신측이 셋 묶어 사고 분석

10:30:10  수신측 alerting이 운영자에게 알림

11:00     ──[push envelope #2: critical=0, error=0, ...]──→ endpoint (평소 페이스 회복)
```

---

## 8. Vector 설정 — 핵심 원칙

전체 toml 예시는 `2_MASTER_PLAN.md §7.2` (운영 config SSoT). 본 §은 **반드시 지킬 원칙 4개**:

1. **sources**: `journald` + `file` (distro별 syslog/messages, auth, audit, docker)
2. **severity 매핑**: VRL `remap`으로 PRIORITY → critical/error/warn/info (§6 룰)
3. **buffer 분리** (원칙 #1+#2 절충):
   - critical sink: `buffer.type=disk`, `when_full=block` (drop 금지)
   - normal sink: `buffer.type=disk`, `when_full=drop_newest` (호스트 보호)
4. **at-least-once**: 모든 sink에 `acknowledgements.enabled=true`

> critical만 block을 허용하고 나머지는 drop — 호스트 다른 서비스(journald, logrotate) 영향 차단. 전체 toml 설정은 `2_MASTER_PLAN.md §7.2` 참조.

---

## 9. 수신측 책임 (분석 영역)

> 수신측 책임 목록·alerting 룰 예시·schema 진화 정책 → **`4_RECEIVER_CONTRACT.md`** 참조.

---

## 10. 3개 에이전트 공존

| 에이전트 | 책임 | port | 프로젝트 |
|---|---|---|---|
| log_parser | 로그 스트림 정제·수집·송출 | 9100 | **본 프로젝트** |
| stat_report | 현재 상태 스냅샷 (pull) | 9102 | **본 프로젝트** |
| sos_report | 사고 포렌식 스냅샷 (단일 동기 호출) | 9101 | **본 프로젝트** |
| 수신측 분석 서비스 | 분석·알림·상세 수집 결정 | — | 본 프로젝트 외부 |

**3개 에이전트는 서로 모름.** 수신측이 `host_id`로 결과를 묶어 사고 분석.

공존 컨벤션:
- 같은 `host_id` 사용 (`/etc/machine-id` 참조)
- 동일 systemd target에 묶어 함께 기동·종료
- 인증 토큰은 에이전트별 독립

> **API 명세·envelope 상세·Phase B 검증 계획은 `2_MASTER_PLAN.md §7.8·§7.9` 참조.**

---

## 11. 비용·지연 요약

| 흐름 | 시점 | 빈도 | 크기 | 백엔드 비용 |
|---|---|---|---|---|
| log_parser push (자동) | 30분 cycle 정시 | 1건 / 30분 | ~수십 KB ~ 수 MB / cycle | 매우 낮음 |
| stat_report pull | 수신측 요청 시 | 필요 시마다 | ~수 MB gzip | 요청당 |
| sos_report (사고 시) | 수신측 트리거 | 사고당 1~수회 | ~1~3MB gzip (분석된 결과) | 사고당 |

평시 트래픽 = 거의 push만. 사고 시 = push 그대로 + pull 추가.

---

## 12. 분리해서 다루는 이유 (반례 모음)

| 시도 | 문제점 |
|---|---|
| log_parser 하나로 메트릭+상태+스냅샷 수집 | 데이터 모델·주기·도구 의도 불일치. 스트림 도구로 스냅샷 흉내 → 어정쩡 |
| sos report를 5~30분 주기로 돌리기 | 28초/회 + 50MB tar → 호스트 부하·인덱스 폭증·tar 파싱 비용. 게다가 사고 정확한 순간은 못 잡힘 |
| 호스트 측 트리거 카탈로그·playbook | 새 트리거 추가 시 호스트 재배포 필요. 수신측 유연성↓ |
| 호스트 측 R1~R4 분류 | severity 결정을 호스트가 함. 수신측 분류 체계 자유도↓ |
| "에러 발생 후" 로그 송출 시작 | 사고 직전 인과를 못 봄. body에 항상 모든 정제 라인이 있어야 함 |

---

## 13. 강건성 보강

강건성 메커니즘 상세: `2_MASTER_PLAN.md §7.7.1` (disk spool), `§7.11` (process supervision), `§10.1.1` (idempotency · seq 영속화), `§7.4` (dedup LRU 50K 캡).

---

## 14. 부록: 에이전트별 도구 매핑

| 에이전트 | 역할 | 수신측 권장 |
|---|---|---|
| log_parser (본 프로젝트) | 로그 스트림 | Loki·Elasticsearch·custom ingest |
| stat_report (본 프로젝트) | 현재 상태 pull | custom 분석 서비스 또는 시계열 DB |
| sos_report (본 프로젝트) | 사고 포렌식 | custom 분석 서비스 |

(메트릭·트레이스·프로파일링은 본 프로젝트 영역 외 — Prometheus/Tempo/Pyroscope 등 별개 스택)

---