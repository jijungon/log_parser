# docs — log_parser 설계·계약 문서 색인

log_parser(엣지 파서) 문서 허브. 오리엔테이션·운영 요약은 루트 [`../README.md`](../README.md)가 시작점이고, 여기서는 **역할별로** 무엇을 읽을지 안내한다.

---

## A. 서비스 이해 — 아키텍처

현행 동작을 이해하는 데 필요한 문서.

| 문서 | 내용 (정직한 한 줄) |
|------|------|
| [pipeline.md](pipeline.md) | 로그 한 줄이 수집→전송까지 거치는 7단계 + push/pull 흐름 다이어그램. 현행 일치 |
| [observability-design.md](observability-design.md) | 왜 스트림(로그)과 스냅샷을 분리하는가 — 의미 원칙(SSoT). 철학은 유효하나 포트 구성(9101/9102)은 설계 당시 기준 — 실제는 단일 :9100 |
| [architecture-review.md](architecture-review.md) | **(신규 이관)** 2026-07-30 외부 아키텍처 리뷰 — 기성 포워더 비교, 신뢰성 평가, 개선 권고(High 3개)와 검증 노트. 현시점 상태 평가로는 가장 최신 |

**설계 이력 (historical)** — 초기 Phase B(2026-04~06) 설계 문서 묶음. 당시 의사결정의 배경을 보는 용도이며, **이후 구현과 다른 부분이 있다** (예: stat/sos 별도 포트 9101/9102 구상 → 실제는 단일 :9100, `/raw`·자동 drain 등 이후 추가분 미반영). 현행과 충돌하면 루트 README·A/B군 문서가 우선한다.

| 문서 | 내용 |
|------|------|
| [master-plan.md](master-plan.md) | 마스터 플랜 통합본(2026-05-07 갱신) — 아키텍처·구현 원칙 총괄. 가장 크고, 배경 이해용 |
| [phase-b.md](phase-b.md) | Phase B 5단계 실행 계획 + 용어집 (master-plan §18~19 분리본) |
| [impl-notes.md](impl-notes.md) | 구현 참조 — Vector IPC 등 코드 수준 결정 기록 |
| [agent-roles.md](agent-roles.md) | 구축 당시 5-에이전트 역할 분담(WHO) 정의 |

---

## B. 수신 서비스 구현자 경로 (인수인계 핵심)

**수신 서비스를 만들 사람은 이 순서대로 읽는다.**

1. [receiver-implementation-guide.md](receiver-implementation-guide.md) — **시작점.** 무엇을 받는가(3종 중 구현은 `/ingest` 1개), 응답 코드 계약, at-least-once·멱등 키·도착 순서·자동 drain 의미론, 구현 체크리스트, 검증법
2. [receiver-contract.md](receiver-contract.md) — 파서↔수신측 계약 요약 — 송출 형식, 수신측 권장 책임, alerting 룰 예시, 스키마 진화 정책
3. [receiver-type-spec.md](receiver-type-spec.md) — envelope·DedupEvent·7개 섹션의 필드 단위 타입 정의 (스키마 정본, TypeScript 표기)
4. [pull-api.md](pull-api.md) — 수신측이 파서를 호출하는 pull API(:9100) — `/stat`·`/trigger-sos`·`/raw`·`/flush`·`/drain-spool`·`/drain-status` curl·파라미터·에러 코드
5. [test-receiver.md](test-receiver.md) — Phase B 당시 검증용 test receiver 스펙(historical) — 실제 구동 더미는 [`../test_server/`](../test_server/)
6. [`../examples/receiver_example.py`](../examples/receiver_example.py) — 최소 수신 구현 + 멱등(`is_duplicate`)·필터링 예시 코드

---

## C. 운영

| 문서 | 내용 |
|------|------|
| [install.md](install.md) | 설치·설정 가이드 — 사전요건, Docker/소스 설치, `agent.yaml` 설정 키, 토큰 4개, 동작 검증 |
| [pull-api.md](pull-api.md) | 운영 중 상세 수집·회수 창구 — 사고 시 `/stat`·`/trigger-sos`·`/raw`, 장애 후 `/drain-spool` |
| [scale-contract.md](scale-contract.md) | 대규모 플릿 확장 계약 — 항목별 [적용됨]/[예정] 상태 표기. 증분 pull·이벤트 스토어는 **미채택** 결정 반영 |

---

## 현재 상태 메모 (2026-07-30 기준)

- **적용 완료**: retry/ 자동 drain(수신 복구 시 파킹분 자동 회수), persist_seq 원자화, corrupt/ 상한 256MB, `dedup.sample_raws_cap` 설정화. 상세는 [`../CHANGELOG.md`](../CHANGELOG.md).
- **scale-contract.md 결정**: 증분 pull(`GET /events?since_seq`)·이벤트 스토어는 미채택/보류. 중앙 플랫폼은 기존 push + 스냅샷 pull로 소비 (로드맵은 별도 repo `log_stack_AI/docs/1_CENTRAL_PLATFORM_ROADMAP.md`).
- **미구현 제안 목록**: `schema_version`·멱등 키 헤더·드롭 카운터 — [architecture-review.md](architecture-review.md) §5 (전부 제안 상태, envelope에 아직 없음).
- **수신측 연동 참조 스냅샷**: [`../reference/stack/`](../reference/stack/) (playbook·goldset, 정본은 log_stack_AI).

## 관련 (docs/ 밖)

- [`../README.md`](../README.md) — 루트 허브: 책임 경계, 빠른 시작, 수신 엔드포인트 요건·재시도 정책 요약
- [`../config/`](../config/) — 에이전트 설정(agent*.yaml)·분류(categories.yaml)·필드(fields.yaml)
- [`../examples/`](../examples/) — envelope 실물 샘플 + 수신 예시 코드
- [`../test_server/`](../test_server/) — 구동 가능한 더미 수신 서버(멱등성 없음, 검증용)
- [`../tests/`](../tests/) — E2E 테스트 하네스(error_cases.yaml·inject_errors.sh)
