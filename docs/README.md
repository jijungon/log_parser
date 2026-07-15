# docs — log_parser 설계·계약 문서 색인

log_parser(서버측 파서) 설계·구현·수신측 계약 문서 모음. 처음 인수인계 받는다면 **아래 "읽는 순서"**부터.

## 읽는 순서 (인수인계용)

1. **[README.md](../README.md)** (repo 루트) — 파서가 무엇을·어떻게 하는지, 실행법, envelope 스키마. 시작점.
2. **[1_observability-design.md](1_observability-design.md)** — 무엇을·왜 관측하는가 (의미 원칙).
3. **[2_MASTER_PLAN.md](2_MASTER_PLAN.md)** — 전체 마스터 플랜(아키텍처·구현 원칙 통합본). 가장 큼.
4. **[4_RECEIVER_CONTRACT.md](4_RECEIVER_CONTRACT.md)** + **[RECEIVER_TYPE_SPEC.md](RECEIVER_TYPE_SPEC.md)** — 파서가 수신측에 보내는 것(계약·타입).
5. **[6_SCALE_CONTRACT.md](6_SCALE_CONTRACT.md)** — 대규모 확장 시 계약(위 4_의 확장). **최신 결정 반영**.

## 전체 목록

| 문서 | 내용 |
|------|------|
| [install.md](install.md) | **설치 · 설정 가이드** — 사전요건·Docker/소스 설치·agent.yaml 설정 키·토큰·검증 |
| [1_observability-design.md](1_observability-design.md) | 계층형 관측성 설계 — 무엇을·왜 분리하는가 (의미 원칙) |
| [2_MASTER_PLAN.md](2_MASTER_PLAN.md) | 마스터 플랜 (observability 통합본) — 아키텍처·구현 원칙 총괄 |
| [3_AGENT_ROLES.md](3_AGENT_ROLES.md) | Agent Role Definitions — 5개 에이전트 역할(생성 주체 WHO) |
| [3_PHASE_B.md](3_PHASE_B.md) | Phase B 실행 계획 + 용어집 (2_ §18~19 분리본) |
| [4_RECEIVER_CONTRACT.md](4_RECEIVER_CONTRACT.md) | 수신측 계약 — 외부 인터페이스 명세(파서↔수신측 약속) |
| [5_TEST_RECEIVER.md](5_TEST_RECEIVER.md) | Phase B 검증용 Test Receiver 스펙 (운영 수신측과 무관) |
| [6_SCALE_CONTRACT.md](6_SCALE_CONTRACT.md) | 대규모 확장 계약 — 4_의 확장. 증분 pull 미채택 등 최신 결정 반영 |
| [pull-api.md](pull-api.md) | On-demand Pull API 상세 — /stat·/trigger-sos·/flush·/drain-spool curl·에러 코드 |
| [pipeline.md](pipeline.md) | 처리 파이프라인 단계별 상세 — 흐름 다이어그램 + 로그 한 줄 7단계 |
| [IMPL_NOTES.md](IMPL_NOTES.md) | 구현 참조 — 코드 수준 세부사항(2_에서 분리) |
| [RECEIVER_TYPE_SPEC.md](RECEIVER_TYPE_SPEC.md) | 수신측 Information Type Spec — envelope/event_kind 상세 타입 |

> 루트 [`../README.md`](../README.md)는 오리엔테이션·운영·요약 허브다. 상세(타입·API·파이프라인)는 위 문서로 링크된다. 변경 이력은 [`../CHANGELOG.md`](../CHANGELOG.md).

## 현재 상태 메모 (2026-07-15 기준)

- **적용 완료**: 핫패스 성능 개선 + spool 안전·효율(retry/ 상한·compress-once). 루트 README·`6_SCALE_CONTRACT.md` 참조.
- **6_SCALE_CONTRACT.md 결정**: 증분 pull(`GET /events?since_seq`)·이벤트 스토어(장부)는 **미채택/보류**. 중앙 플랫폼은 기존 push + 스냅샷 pull로 소비. 중앙 플랫폼 로드맵은 별도 repo `log_stack_AI/docs/1_CENTRAL_PLATFORM_ROADMAP.md`.
- **수신측 연동 참조 스냅샷**: [`../reference/stack/`](../reference/stack/) (playbook·goldset, 정본은 log_stack_AI).
- 1_~5_는 초기 Phase B 설계 문서(2026-06 기준). 이후 결정은 6_ 및 루트 README가 우선.

## 관련 (docs/ 밖)

- [`../config/`](../config/) — 에이전트 설정(agent*.yaml)·분류(categories.yaml)·필드(fields.yaml)
- [`../reference/stack/`](../reference/stack/) — 수신측 참조 스냅샷
- [`../tests/`](../tests/) — E2E 테스트 하네스(error_cases.yaml·inject_errors.sh)
