# config

> [← 프로젝트 루트](../README.md)

에이전트 설정 파일 모음.

| 파일 | 용도 |
|------|------|
| `agent.yaml` | 전체 설정 키와 기본값 — 운영 환경에 맞게 필요한 키만 override |
| `agent_docker.yaml` | Docker 실행용 설정 (`docker-compose.yml`에서 마운트) |
| `agent_test.yaml` | 로컬 테스트용 설정 |
| `categories.yaml` | 로그 카테고리 분류 규칙 — 패턴 추가·수정은 여기서 (`program:`·`logger:` 조건 지원) |
| `fields.yaml` | 필드 추출 규칙 + logfmt/JSON 자동 파싱 설정 — 추출 필드 추가·수정은 여기서 |
| `vector.toml` | Vector 파이프라인 기본 설정 (에이전트가 런타임에 재생성, 멀티라인·노이즈 필터 포함) |

설정 변경 후에는 에이전트 재시작이 필요합니다.

> 환경변수 템플릿 `.env.example`은 여기가 아니라 **repo 루트**에 있다
> (`cp .env.example .env` — docker compose가 루트의 `.env`를 읽는다).

## 어느 yaml이 어디에 쓰이나

에이전트는 **첫 번째 CLI 인자**로 설정 파일 경로를 받는다 (미지정 시 `/etc/log_parser/agent.yaml`).

| 파일 | 쓰이는 곳 | 특징 |
|------|-----------|------|
| `agent.yaml` | 베어메탈 운영 예시 겸 **전체 키·기본값 레퍼런스** | cgroup on · tls on. 모든 키에 주석 |
| `agent_docker.yaml` | 로컬 Docker — `docker-compose.yml`이 컨테이너의 `/etc/log_parser/agent.yaml`로 마운트 | cgroup off · endpoint `host.docker.internal:8080` (호스트의 test-server) |
| `agent_test.yaml` | 로컬 테스트 — 실행 시 인자로 지정 (예: `cargo run -- config/agent_test.yaml`) | cgroup off · endpoint `127.0.0.1:8080` |

## 주요 키와 기본값 (정본: `src/config.rs`)

아래 기본값은 키를 생략했을 때 코드가 채우는 값이다 (`src/config.rs`의 default 함수들).

### 전송·spool (`transport`)

| 키 | 기본값 | 설명 |
|----|--------|------|
| `spool_max_mb` | `2048` | `new/` WAL 용량 상한. 초과 시 가장 오래된 파일을 `retry/`로 evict 후 저장 |
| `retry_max_mb` | `1024` | `retry/` 데드레터 용량 상한 (`0` = 무제한). 초과 시 오래된 파일부터 삭제 |
| `retry_ttl_hours` | `168` (7일) | `retry/` 보관 기간 (`0` = 무기한). 초과 파일 삭제 |
| `auto_drain` | `true` | 라이브 전송이 다시 성공하면(= 수신측 복구 감지) `retry/` 전체를 자동 drain해 재전송. `false`면 수동 `POST /drain-spool`로만 재전송 |

> **corrupt/ 상한은 설정 키가 아니다** — 파싱 불가 파일 격리 디렉터리 `corrupt/`의 용량 상한은
> 코드 상수 **256MB**(`src/transport/spool.rs`의 `CORRUPT_MAX_MB`)로 고정이며, 초과 시
> 오래된 파일부터 삭제된다. yaml로 조정할 수 없다.

### dedup

| 키 | 기본값 | 설명 |
|----|--------|------|
| `sample_raws_cap` | `3` | dedup 그룹당 보관하는 원본 로그 샘플 수 (`0` = 샘플 미보관, 템플릿·count만 전송). **envelope 크기의 지배 요인** — 대역폭을 줄이려면 이 값을 낮춘다 |

### pipeline

| 키 | 기본값 | 설명 |
|----|--------|------|
| `body_max_size_mb` | `50` | envelope body 크기 임계값 (raw JSON 기준). **초과해도 차단하지 않는다** — warn 로그만 남기고 그대로 spool·전송한다 (무손실 원칙, 크기 관측용). `0`이면 체크 생략 |

나머지 키(cycle·cgroup·inbound·static_state 등)는 `agent.yaml`의 주석과
[`docs/install.md`](../docs/install.md)의 설정 키 표 참조.
