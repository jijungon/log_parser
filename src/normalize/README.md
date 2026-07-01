# normalize

원시 로그 문자열을 구조화된 DedupEvent로 변환하는 모듈. 순서대로 적용됩니다.

| 파일 | 역할 |
|------|------|
| `tokens.rs` | 가변 값(숫자·IP·UUID 등)을 placeholder로 치환해 template 생성 |
| `severity.rs` | 메시지·syslog 레벨 기반 severity 분류 (`critical` / `error` / `warn` / `info`) |
| `categories.rs` | `categories.yaml` 규칙 기반 category 코드 분류. 리터럴 패턴은 aho-corasick로 단일 스캔, 실제 정규식만 개별 평가. `program:`·`logger:` 게이트 지원 |
| `fields.rs` | `fields.yaml` 규칙 기반 구조화 필드 추출 + logfmt(`key=value`)·JSON 자동 파싱. 전역 인스턴스(`init_global`)를 `extract_fields()` 자유함수가 사용 |

분류·필드 규칙 추가·수정은 소스가 아닌 `config/categories.yaml` · `config/fields.yaml`에서 합니다(코드 변경·재빌드 불필요).
파일이 없으면 각각 fallback(전체 `system.general`) / builtin 추출기로 동작합니다.
