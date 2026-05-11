# Phase B — 실행 계획 + 용어집

> 2_MASTER_PLAN.md §18~19 분리본. 설계·아키텍처는 2_MASTER_PLAN.md 참조.

---

## 1. Phase B 5단계

각 단계의 "검증" 항목이 통과해야 다음 단계. 통과 못 하면 멈추고 원인 진단.

| Step | 작업·산출물 | 검증 | 담당 (`3_AGENT_ROLES.md`) |
|:-:|------|------|------|
| 1 | 스켈레톤 + cgroup 자가 격리 + `agent.yaml` 로드 | `cgroup.procs`에 PID, `memory.current`=RSS, default값 동작 | Coordinator + Agent 1 |
| 2 | `envelope.rs` 단일 형식 (headers + body) + `HttpJsonTransport` (Bearer+gzip) | mock 서버에서 envelope 정상 수신 (`event_kind="log_batch"`) | Coordinator + Agent 5 |
| 3 | Vector 운영 config (journald + file sources: syslog/messages, auth.log/secure, audit.log, docker ⚠️ + cursor + buffer.disk) | 60초 RSS<50MB·CPU<1%, cgroup 합산 안, 재시작 cursor 이어가기 | Agent 3 |
| 4 | Normalizer + Dedup(30s/50K LRU) + 카테고리 태깅(categories.yaml 전체 항목) + Coordinator envelope 조립 | 100→1 압축, LRU 50,001 evict, 30분 cycle envelope 1건, `categories.yaml` 룩업 | Agent 4 + Coordinator |
| 5 | 신뢰성(disk-buffer + backoff) + inbound `POST /flush` | 단절·재시작·burst에서 critical drop 0, `/flush` 즉시 응답 + 새 cycle | Agent 5 |

> 각 Step의 완료 기준 detail은 `3_AGENT_ROLES.md` 해당 에이전트 절. 룰·스키마는 `2_MASTER_PLAN.md §7` + `1_observability-design.md §4·§5·§6`.

---

## 2. 단계 간 게이트

- Step 1 통과 못 하면 모든 게 무너짐 — cgroup이 안 되면 원칙 #2가 깨짐
- Step 3에서 Vector 합산 RSS > 100MB → Vector config 재검토
- Step 4에서 envelope 크기 측정 — body 폭주 호스트에서 cgroup 메모리 캡 안에서 만들어지는지
- Step 5 drop 시뮬레이션 통과 못 하면 → 운영 투입 보류

---

## 3. Phase B에서 안 하는 것

- RCA 룰·알림·대시보드 — 외부 서비스 영역
- Live Collection 자체 구현 — Vector가 함
- stat_report / sos_report 구현 — Phase B 이후 별도 스텝
- `/proc`, `/sys` 수치 수집 — stat_report / sos_report 담당
- 판단 로직 — 외부 서비스
- Multi-line 병합 — 앱 로그 수집 시 별도
- PII 마스킹 — 운영 학습 후

---

## 4. Phase B 완료 정의

다음을 모두 만족하면 Phase B 종료:

- [ ] 5개 step 모두 검증 통과
- [ ] 실제 Linux 서버에 systemd unit으로 배포 → 24시간 운영
- [ ] 24시간 동안 critical 이벤트 drop 0건
- [ ] cgroup 측정값이 평시 <128MB / <5% 유지
- [ ] mock 수신측이 envelope **최소 48건/24시간** 수신 (empty cycle invariant 포함; POST /flush 호출 시 추가 발생)
- [ ] 모든 envelope이 단일 schema 통과 (`event_kind="log_batch"`, `cycle.host_id·boot_id·seq` 부착)
- [ ] 호스트 재부팅 시뮬레이션: 재시작 후 `cycle.seq` 단조 증가 유지
- [ ] `POST /flush` 호출 시 즉시 envelope 응답 + 새 cycle 시작
- [ ] `POST /flush` 호출 후 mock 수신측에서 deep-pull 시나리오 확인 (sos_report는 Phase B 이후 구현 — stub 또는 mock으로 대체)
- [ ] Vector 자식 강제 종료 시뮬레이션: 자동 재시작 + `process_health.vector_restarts_24h` 증가 확인

이후는 운영 학습 + 점진 개선 — Phase B와 별도 사이클.

---

## 5. 용어집

| 용어 | 한 줄 설명 |
|------|-----------|
| **Envelope** | 호스트가 송출하는 단일 형식. `event_kind + cycle + headers + body` 구조. 3개 에이전트 공통 외형 |
| **event_kind** | envelope 종류 식별자. `"log_batch"` / `"stat_snapshot"` / `"sos_snapshot"` |
| **headers** | 수신측이 빠르게 읽는 통계·hint. `total_sections` 필수 |
| **body** | 실제 수집 데이터. `section` + `data` 구조의 배열. 수신측 분석의 진실 source |
| **cycle** | envelope 송출 주기 또는 수집 메타. default 30분 (log_parser), `agent.yaml`로 외부화 |
| **Push / Pull** | 자동 30분 cycle 송출 (outbound) / `POST /flush`로 즉시 응답 (inbound 타이밍 신호) |
| **deep-pull** | 사고 시 수신측이 `GET host:9102/stat` + `POST host:9100/flush` + `POST host:9101/trigger-sos` 병렬 호출 |
| **stat_report** | 현재 시스템 상태 경량 스냅샷 에이전트. `GET /stat` pull on demand. 7개 섹션. port 9102 |
| **sos_report** | 사고 시 전체 포렌식 스냅샷 에이전트. 단일 동기 호출. stat + logs 통합. 8개 섹션. port 9101 |
| **categories.yaml** | category 패턴→이름 매핑 외부화 파일. 전체 21개 항목 |
| **process_health** | log_parser `envelope.headers`의 운영 신호 (`vector_restarts_24h` 등) |
| **empty cycle** | body가 빈 log_parser envelope. envelope 도착 자체가 호스트 alive 신호 |
| **total_sections** | headers 필수 필드. 수신측이 받은 body 배열 길이와 비교해 truncation 감지 |
| **deep-pull (sos)** | `POST /trigger-sos` 단일 호출로 수집 완료 후 sos_snapshot 반환. 수집 시간만큼 응답 대기 |

> Vector·VRL·xxhash·LRU 등 일반 용어는 외부 문서 참조.
