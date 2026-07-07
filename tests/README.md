# 에러 케이스 E2E 테스트 하네스

서버(VM)에서 **합성 에러 로그를 주입** → 파서가 수집·분류 → 로컬 stack_AI가 받아
**챗봇(localhost:8000)이 제대로 답하는지** 케이스별로 자동 검증한다. (방향 A: 합성 로그 주입)

실제 장애(OOM·디스크풀 등)를 일으키지 않고 `logger`로 대표 로그 라인을 써넣어
**전 경로(Vector→normalize→envelope→fetch→corpus→챗봇)** 를 안전·재현 가능하게 검증한다.
kernel.panic·hw.mce처럼 실제로는 못 일으키는 것도 커버된다.

## 구성

| 파일 | 위치 | 역할 |
|------|------|------|
| `error_cases.yaml` | log_parseer_ai (여기, git) | **정본** — 주입 로그 + 챗봇 질문 + 기대신호 |
| `inject_errors.sh` | log_parseer_ai (git→VM) | 정본에서 **자동 생성**. VM에서 logger 주입 |
| `e2e_verify.py` | log_stack_AI (로컬) | flush→fetch→corpus→serve→/api/ask 자동 검증 |

## 실행 순서

```bash
# 0) (케이스 추가·수정 시) 로컬 stack_AI에서 주입 셸 재생성 후 커밋·푸시
python3 tests/e2e_verify.py gen
#   → log_parseer_ai/tests/inject_errors.sh 갱신 → git commit/push

# 1) VM(로그 수집 서버)에서 주입
cd /app/log_parser && git pull
bash tests/inject_errors.sh            # 전체 (또는: bash tests/inject_errors.sh kernel.oom)

# 2) 로컬 stack_AI에서 검증 (flush→fetch→corpus→serve 재기동→챗봇 질문→PASS/FAIL)
cd ~/app/log_stack_AI
python3 tests/e2e_verify.py verify --assume-injected
#   특정 케이스만: python3 tests/e2e_verify.py verify --only kernel.oom
```

## 판정 기준 (케이스별 PASS)

- **분류**: 챗봇 검색 결과 후보에 정답 카테고리가 포함 (1위면 더 좋음)
- **게이트**: 근거 충분으로 게이트 통과(분석·대처 생성됨)
- **키워드**(soft): 답변에 기대 키워드 포함 여부 표시

## 주의

- **주입 라인은 categories.yaml 패턴을 포함**해야 그 카테고리로 분류된다.
  추가 전 로컬에서 분류를 교차검증하면 실수를 줄인다(첫 매칭 우선·program 게이트).
- `program:` 게이트 규칙(auth.event 등)은 `logger -t <program>` 태그를 맞춰야 한다.
- 주입 후 dedup 30초 창이 있으니, flush 전 몇 초 여유를 둔다.
- SSH 키가 없으면 주입은 VM에서 수동 실행한다(위 1단계).
