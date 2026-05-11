# x-review: 5문서 통합 최종 검수 — Request Changes 🔄

- Date: 2026-04-30
- Target: MASTER_PLAN.md / ARCHITECTURE.md / AGENT_ROLES.md / PHASE_B_PLAN.md / CLAUDE.md
- Lenses: docs, architecture, logic (comments-stale 실패 — agent-type typo)
- Agents: 3 (4 의도 중 1 fail)
- Findings: 11 (Critical: 0, High: 5, Medium: 3, Low: 3)

---

## Verdict: Request Changes 🔄

본 정비 작업(MASTER 갱신·PHASE_B 백링크·위계 정렬)은 LGTM. 그러나 AGENT_ROLES.md가 Phase A 결정 미반영 상태로 정지돼 4문서와 분기. Phase B Step 1 진입 전 정비 권고.

## Consensus High (2+ lens 일치)

[High] AGENT_ROLES.md L201–283 — Agent 3 "Live System Collection Engineer" 자체 구현 미갱신 (docs + logic)
→ Why: Phase A Day 3 RESULT.md L100-103이 Vector 대체·Agent 3 축소 권고. MASTER §3.1·§13 / ARCHITECTURE §0 모두 Vector 채택 단언 → AGENT_ROLES만 충돌
→ Fix: "Vector 운영 Engineer (vector.toml·VRL·systemd)"로 재정의 또는 Coordinator 흡수

[High] AGENT_ROLES.md L525–543 — "개발 Phase 매핑" 표가 15 Phase 그대로 (docs + logic)
→ Why: MASTER §13이 8단계 축소 truth로 선언, PHASE_B_PLAN 8단계 정의
→ Fix: PHASE_B 8단계 매핑으로 재작성 또는 1줄 포인터

## High — single lens (검증 가능)

[High] AGENT_ROLES.md L480–505 vs ARCHITECTURE.md §4 — Event 스키마 본문 동시 존재 (architecture)
→ Why: SSoT 위반. AGENT_ROLES 쪽이 source_id/sequence_id 더 가짐
→ Fix: 누락 필드를 ARCHITECTURE §4로 흡수, AGENT_ROLES는 포인터

[High] AGENT_ROLES.md L320–334 — severity 카테고리→레벨 매핑 3중 복제 (architecture)
→ Why: MASTER §8 + ARCHITECTURE §4 + AGENT_ROLES 셋 모두 본문 보유
→ Fix: ARCHITECTURE §4.1 신설해 SSoT화

[High] AGENT_ROLES.md L159–161 — SarParser "텍스트 형식" 단정 (logic)
→ Why: MASTER §17 #5 "결정 보류", ARCHITECTURE §3.1 "SAR 바이너리" — AGENT_ROLES만 단정
→ Fix: "Phase B Step 6에서 결정 후 갱신" 주석

[High] AGENT_ROLES.md L178–181 — SosParser trait 컴파일 불가 (docs)
→ Why: async fn + impl Trait + RPITIT 미적용으로 Rust 2021에서 못 씀
→ Fix: #[async_trait] 또는 BoxStream<'_, RawEvent>

## Medium

[Medium] AGENT_ROLES.md L114–197 — WHO 문서가 HOW 영역 침범 (architecture)
→ Fix: 각 Agent 절은 담당 모듈+완료 기준만, 처리 흐름은 ARCHITECTURE §2.X 포인터

[Medium] AGENT_ROLES.md L67–73, L91–93 — distro candidate / OS 매트릭스 P0/P1/P2 분류 불일치 (architecture + logic)

[Medium] ARCHITECTURE.md L364 — "트리거: 1시간" 근거 없음 (logic)
→ Fix: "운영 정책 (예시)"로 약화

## Low

[Low] ARCHITECTURE.md §4 예시 — count:123인데 sample_raws 1개. "최대 3개" 정책과 예시 불일치 (docs)

[Low] MASTER_PLAN.md L88 — "RSS <7MB"가 측정값(7.1~7.2MB)보다 약간 낙관 (logic)

[Low] PHASE_B_PLAN.md L7 — 청중 한 줄 누락 (다른 3문서엔 명시) (docs)

## Summary

| Lens | Findings | Critical | High | Medium | Low |
|------|---------|----------|------|--------|-----|
| docs | 7 | 0 | 2 | 3 | 2 |
| architecture | 7 | 0 | 3 | 3 | 1 |
| logic | 6 | 0 | 3 | 2 | 1 |
| comments-stale | (실패) | - | - | - | - |
| dedup 후 합 | 11 | 0 | 5 | 3 | 3 |

## 한 줄 결론

이번 작업으로 정비된 4문서는 깨끗. 위계 정렬이 드러낸 진짜 문제는 **AGENT_ROLES.md가 Phase A 이전 상태에 정지**된 것. 코드 진입 전 정비 필요 (특히 Agent 3·15 Phase 매핑·Event 스키마·SarParser·trait 시그니처).
