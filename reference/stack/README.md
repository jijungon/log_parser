# reference/stack — 수신측(log_stack_AI) 참조 스냅샷

> **읽기 전용 참조본.** 인수인계 편의를 위해 수신측 데모 스택(`log_stack_AI`)의 산출물을
> 파서 repo 안에 복사해 둔 것이다. **정본(source of truth)이 아니다.**
> 스냅샷 시점: 2026-07-15.

## 이 파일들이 속한 곳 (책임 경계)

이 스냅샷은 **중앙 플랫폼(수신측)** 것이다. 파서는 로그 저장·조회·분석을 책임지지 않으며,
"어떻게 저장·소비하는가"는 받는 쪽 설계다. 관련 문서:
- 파서 출력 계약 + 저장이 왜 중앙 몫인지: [`../../docs/scale-contract.md`](../../docs/scale-contract.md)
- 파서 책임 경계 요약: 루트 [`README.md`](../../README.md) "설계 의도 · 책임 경계"
- 중앙 플랫폼 로드맵: `log_stack_AI/docs/1_CENTRAL_PLATFORM_ROADMAP.md` (별도 repo)

## 무엇이 왜 여기 있나

파서의 `config/categories.yaml`(카테고리 체계)이 **시스템 전체의 뼈대**라서, 수신측의
아래 두 파일이 그 카테고리에 **맞물려** 있다. 파서 담당자가 카테고리를 바꿀 때 하류에서
무엇이 함께 깨지는지 한눈에 보라고, 그 하류 파일을 여기에 스냅샷으로 둔다.

| 파일 | 무엇 | 정본 위치 |
|---|---|---|
| `playbook.yaml` | 카테고리별 원인·확인명령·표준대처 지식. 수신측 분석 LLM 프롬프트에 주입됨 | `log_stack_AI/playbook.yaml` |
| `goldset.yaml` | 검색 품질 평가용 정답셋(질문 + 정답 uid). 카테고리 단위로 묶임 | `log_stack_AI/goldset/goldset.yaml` |

## 동기화 규칙 (중요)

`config/categories.yaml`을 **추가/변경하면**, 정본 위치의 아래를 **반드시 같이 갱신**한다
(README 본문 `config/categories.yaml` 절 참조):

1. `log_stack_AI` 의 `CATEGORY_KO`(검색 한국어 설명, `src/extract.py`)
2. `log_stack_AI/playbook.yaml` (원인·대처 지식)
3. `log_stack_AI/goldset/goldset.yaml` (검색 채점 기준)

여기(`reference/stack/`)의 사본은 **자동으로 갱신되지 않는다.** 필요하면 정본에서 다시 복사할 것:

```bash
cp ../log_stack_AI/playbook.yaml         reference/stack/playbook.yaml
cp ../log_stack_AI/goldset/goldset.yaml  reference/stack/goldset.yaml
```

## 역할 요약 (README 본문에서 발췌)

```
categories = 로그 분류      (파서, 정본)
fields     = 필드 추출      (파서, 정본)
playbook   = 답변 지식(재료) (수신측)   ← 이 폴더 사본
goldset    = 검색 채점 기준  (수신측)   ← 이 폴더 사본
```
