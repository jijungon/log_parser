# normalize

[← src](../README.md) · 관련: [dedup](../dedup/README.md) · [pipeline](../pipeline/README.md)

원시 로그 문자열을 template·severity·category·구조화 필드로 변환하는 모듈. 호출 순서는 [process.rs](../process.rs)의 `process_line` 참조 (strip → 토큰화 → severity → dedup 병합 판정 → 첫 등장에만 fields·category).

| 파일 | 역할 |
|------|------|
| `tokens.rs` | syslog 헤더 strip(RFC 3164·ISO 8601)·program 추출 + 가변 값(숫자·IP·UUID·경로 등)을 placeholder로 치환해 template 생성. RegexSet 프리필터로 매칭된 패턴만 치환 |
| `severity.rs` | 키워드 기반 severity 보정 (`critical`/`error`/`warn`/`info`) — aho-corasick 단일 스캔, 아래 상세 |
| `categories.rs` | `categories.yaml` 규칙 기반 category 분류 (first-match-wins). 리터럴 패턴은 aho-corasick로 단일 스캔, 실제 정규식만 개별 평가. `program:`·`logger:` 게이트 지원 |
| `fields.rs` | `fields.yaml` 규칙 기반 구조화 필드 추출 + logfmt(`key=value`)·JSON 자동 파싱(allow 목록·`max_auto_fields` 상한). 전역 인스턴스(`init_global`)를 `extract_fields()` 자유함수가 사용 |

## 핵심 동작 / 불변식

- **카디널리티 억제 마스크** (tokens.rs) — 값마다 template이 갈라져 fingerprint가 폭증하는 것을 차단:
  - `SHA256:<FPR>` — ssh 키 지문 (`SHA256:` + base64 20자 이상)
  - `[<ADDR>]` — 커널 스택 프레임 주소 (0x 접두 없는 소문자 hex 8~16자리)
  - `<CID>`/`<VETH>`/`<BR>`/`<MNT>` — 컨테이너 런타임 ID (docker-*.scope, veth, br-, overlay2 등)
  - 순서 중요: 더 구체적인 패턴이 PATH/NUM 등 일반 패턴보다 먼저 적용된다.
- **severity 규칙** (severity.rs):
  1. CRITICAL 키워드 매치 시 initial 무관하게 critical — kernel panic·OOM kill 계열에 더해 `"remounting filesystem read-only"`(fs 무결성 훼손)·`"machine check events logged"`(하드웨어 MCE) 포함.
  2. ERROR 승격 키워드 tier(8종: i/o error, blk_update_request, ext4-fs error, xfs: internal error, segfault at, general protection fault, hardware error, edac mc)는 **info/warn → error 승격만** 하고 이미 높은 severity는 절대 강등하지 않는다. 파일 소스는 base severity=info로 들어오므로 이 키워드들이 위험 이벤트를 승격시키는 유일한 지점.
- **규칙 순서 계약** (categories.rs는 first-match-wins 매처): `config/categories.yaml`에서 좁은 패턴을 넓은 패턴보다 **위에** 둔다 — 예: `container.oom`("Memory cgroup out of memory")이 `kernel.oom`("Out of memory: Killed")보다 먼저 와야 한다. 실제 cgroup OOM 라인은 두 문구를 모두 포함한다 (narrow-before-broad).

분류·필드 규칙 추가·수정은 소스가 아닌 `config/categories.yaml` · `config/fields.yaml`에서 합니다(코드 변경·재빌드 불필요).
파일이 없으면 각각 fallback(전체 `system.general`) / builtin 추출기로 동작합니다.
