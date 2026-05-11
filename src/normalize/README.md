# normalize

원시 로그 문자열을 구조화된 DedupEvent로 변환하는 모듈. 순서대로 적용됩니다.

| 파일 | 역할 |
|------|------|
| `tokens.rs` | 가변 값(숫자·IP·UUID 등)을 placeholder로 치환해 template 생성 |
| `severity.rs` | 메시지·syslog 레벨 기반 severity 분류 (`critical` / `error` / `warn` / `info`) |
| `categories.rs` | `categories.yaml` 규칙 기반 category 코드 분류 (`kernel.oom`, `auth.failure` 등) |
| `fields.rs` | 정규식으로 구조화 필드 추출 (`pid`, `user`, `dev`, `unit`) |

분류 규칙 추가·수정은 소스가 아닌 `config/categories.yaml`에서 합니다.
