# config

에이전트 설정 파일 모음.

| 파일 | 용도 |
|------|------|
| `agent.yaml` | 전체 설정 키와 기본값 — 운영 환경에 맞게 필요한 키만 override |
| `agent_docker.yaml` | Docker 실행용 설정 (`docker-compose.yml`에서 마운트) |
| `agent_test.yaml` | 로컬 테스트용 설정 |
| `categories.yaml` | 로그 카테고리 분류 규칙 — 패턴 추가·수정은 여기서 |
| `vector.toml` | Vector 파이프라인 기본 설정 (에이전트가 런타임에 재생성) |
| `.env.example` | 환경변수 템플릿 — 복사 후 실제 토큰 값 입력 |

설정 변경 후에는 에이전트 재시작이 필요합니다.
