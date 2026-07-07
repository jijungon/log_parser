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

---

## 🚀 실행 절차 — 머신별 순서

> **역할 분담**: 🖥️ 서버 = 로그 주입만 / 💻 로컬 = 나머지(수집·색인·챗봇·판정) 전부.
> (로컬→VM SSH 키가 없어 주입만 VM에서 수동 실행하는 구조)

### 사전 준비 (💻 로컬, 최초 1회)
```bash
ollama serve        # LLM 답변용. 이미 떠 있으면 생략 (모델: exaone3.5:7.8b)
```

### 1단계 · 🖥️ 서버(VM) — `/app/log_parser`
```bash
cd /app/log_parser
git pull                              # 최신 fixtures/설정 받기
bash tests/inject_errors.sh           # 에러 로그 전체(21개) 주입
#   특정 1개만: bash tests/inject_errors.sh kernel.oom
```
→ `[inject] 완료` 뜨면 서버 할 일 끝.

### 2단계 · 💻 로컬 — `~/app/log_stack_AI`
```bash
cd ~/app/log_stack_AI
python3 tests/e2e_verify.py verify --assume-injected
#   특정 케이스만: python3 tests/e2e_verify.py verify --only kernel.oom
```
→ 이 **한 줄이 나머지 전부 자동**: flush+fetch(서버에서 로그 당김) → corpus 재빌드 →
   챗봇 serve 재기동 → 케이스별 질문 → `N/21 PASS` 리포트.
→ 완료까지 **~25–30분**(corpus 변경 시 임베딩 콜드 로드 + LLM). 끝나면 `http://localhost:8000` 유지.

### 한눈 요약
| 순서 | 머신 | 명령 |
|------|------|------|
| 사전 | 💻 로컬 | `ollama serve` |
| 1 | 🖥️ 서버 | `git pull` → `bash tests/inject_errors.sh` |
| 2 | 💻 로컬 | `python3 tests/e2e_verify.py verify --assume-injected` |

---

## 케이스를 추가/수정할 때만 (1단계 앞에 붙는 절차 · 💻 로컬)
```bash
# 1) 정본 편집
vi ~/app/log_parseer_ai/tests/error_cases.yaml
# 2) 주입 셸 재생성 (정본 → inject_errors.sh, drift 방지)
cd ~/app/log_stack_AI && python3 tests/e2e_verify.py gen
# 3) 커밋·푸시 (parseer repo)
cd ~/app/log_parseer_ai && git add tests/ && git commit -m "test: ..." && git push
```
→ 그다음 위 **1단계(서버 git pull + 주입)** 부터 진행.

> ⚠ 파서 **코드(`src/*.rs`)나 `config/*.yaml`** 을 바꿨으면, 1단계 `git pull` 뒤에
> **`docker compose down && docker compose up -d`** 를 추가해야 반영된다
> (config는 재기동만, 코드는 이미지 `build` 포함).

---

## 판정 기준 (케이스별 PASS)

- **분류**: 챗봇 검색 결과 후보에 정답 카테고리가 포함 (1위면 더 좋음)
- **게이트**: 근거 충분으로 게이트 통과(분석·대처 생성됨)
- **키워드**(soft): 답변에 기대 키워드 포함 여부 표시

## 주의

- **주입 라인은 categories.yaml 패턴을 포함**해야 그 카테고리로 분류된다.
  추가 전 로컬에서 분류를 교차검증하면 실수를 줄인다(첫 매칭 우선·program 게이트).
- `program:` 게이트 규칙(auth.event 등)은 `logger -t <program>` 태그를 맞춰야 한다.
- 주입 후 dedup 30초 창이 있으니, flush 전 몇 초 여유를 둔다.
- `kernel.panic` 등은 `kern.emerg` 로 주입하면 모든 터미널에 wall 방송이 뜬다 → fixture는 `kern.crit` 사용.
- corpus 가 바뀌면 serve 첫 로드가 임베딩 재계산으로 수 분 걸린다(캐시 후 빨라짐). 검증 후 serve 는
  기본 유지되며(`--stop-serve` 로 내림), 수동 정리는 `docker rm -f logstack_e2e_serve`.
- 대량(21개) 연속 LLM 요청 중 serve 가 메모리 압박으로 재시작될 수 있다 → `e2e_verify.py` 가 요청 재시도로 커버.
