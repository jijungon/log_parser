# 설치 · 설정 가이드

log_parser 에이전트를 새 호스트에 설치하고 설정하는 전체 절차. (빠른 요약은 루트 [`README.md`](../README.md#사전-요건--빠른-시작-새-서버-bring-up))

---

## 1. 사전 요건

- **Linux 호스트** — journald/syslog·`/proc`·cgroup에 의존. macOS·Windows에선 실행 불가(단위 테스트만 가능). 로컬 검증은 Docker로.
- **Vector 실행 파일** — 에이전트가 로그 수집기로 Vector를 **자식 프로세스로 띄운다**. 기본 경로 `/app/vector/bin/vector`(`agent.yaml`의 `pipeline.vector_bin`으로 변경). **호스트에 Vector가 설치돼 있어야 한다** — 없으면 수집이 시작되지 않는다.
- **토큰 4개** — `PUSH_OUTBOUND_TOKEN`·`FLUSH_INBOUND_TOKEN`·`STAT_INBOUND_TOKEN`·`SOS_INBOUND_TOKEN`. 하나라도 미설정이면 **기동 거부**(§3.3).
- **빌드 도구** — 소스 빌드 시 Rust 툴체인, 또는 Docker.

---

## 2. 설치

### A. Docker (권장)

`docker-compose.yml`이 `agent_docker.yaml`·`categories.yaml`·`fields.yaml`을 컨테이너의 `/etc/log_parser/`로 마운트하고, 호스트 `/var/log`·journald·`/app/vector`를 붙인다.

```bash
cp config/.env.example config/.env      # 토큰 4개 채우기 (§3.3)
docker compose up -d --build            # :9100 노출, agent_docker.yaml로 기동
docker compose logs -f log-parser       # 기동·수집 로그 확인
```

- config는 런타임 마운트라 변경 시 **이미지 재빌드 불필요** — `docker compose down && up -d` 재기동만으로 반영.
- ⚠ **새 설정 파일을 추가하면 compose 볼륨에도 반드시 등록**해야 컨테이너가 읽는다(누락 시 builtin fallback).

### B. 소스 빌드 (bare-metal)

```bash
# 1. 빌드
cargo build --release

# 2. 디렉터리 준비
mkdir -p /run/log_parser /var/lib/log_parser/spool/new /var/lib/log_parser/spool/retry /etc/log_parser

# 3. 설정·규칙 배치
cp config/agent.yaml config/categories.yaml config/fields.yaml /etc/log_parser/

# 4. 환경변수 (§3.3)
cp config/.env.example config/.env    # 토큰 값 입력
set -a && source config/.env && set +a

# 5. 실행
./target/release/log_parser /etc/log_parser/agent.yaml

# 종료
kill -TERM <PID>
```

---

## 3. 설정

### 3.1 `agent.yaml` — 에이전트 동작 (전체 키는 [`config/agent.yaml`](../config/agent.yaml))

모든 키에 기본값이 있어 필요한 것만 override한다. 주요 섹션:

| 섹션 | 핵심 키 | 의미 |
|------|--------|------|
| `transport` | `endpoint`(**필수**), `token_env`, `tls_enabled`, `spool_max_mb`(512), `retry_max_normal`(5), `http_gzip_level`(6) | 수신측으로 push하는 방법. `endpoint`가 push 목적지 URL |
| `transport` | `retry_max_mb`(256), `retry_ttl_hours`(72) | `retry/` 데드레터 상한·TTL(0=무제한). 초과 시 오래된 미배달분 자동 삭제 |
| `inbound` | `listen_addr`(`127.0.0.1:9100`), `token_env`/`stat_token_env`/`sos_token_env`, `rate_limit_per_hour`(6) | pull API 바인드·인증. 원격 노출은 `0.0.0.0:9100`+방화벽 |
| `cycle` | `window_seconds`(1800), `host_override` | 송출 주기(30분), 호스트 표시명 고정(멀티서버·컨테이너 시 권장) |
| `dedup` | `window_seconds`(30), `lru_cap`(50000) | 같은 패턴 묶는 창·시그니처 캐시 상한 |
| `pipeline` | `vector_bin`, `channel_capacity`(10000), `body_max_size_mb`(50) | Vector 경로·내부 버퍼·사이클 상한 |
| `cgroup` | `enabled`(true), `memory_max`(128m), `cpu_max`(5%) | 자기 리소스 제한. **격리 실패 시 기동 거부**(로컬 테스트는 `agent_test.yaml`에서 off) |
| `static_state` | `enabled`(true), `seq_state_path` | `cycle.seq`를 파일에 영속화 → 재시작 후에도 이어짐 |

> 환경별 프로필: 운영=`agent.yaml`(cgroup on·tls on), 로컬=`agent_test.yaml`(cgroup off·수신처 :8080), 도커=`agent_docker.yaml`.

### 3.2 분류·필드 규칙

| 파일 | 역할 |
|------|------|
| [`config/categories.yaml`](../config/categories.yaml) | 로그 카테고리 분류 규칙(시스템의 뼈대). 바꾸면 수신측 `playbook.yaml`·`goldset.yaml`도 동기화 — 루트 README "가장 중요한 파일" 참조 |
| [`config/fields.yaml`](../config/fields.yaml) | 필드 추출 규칙(logfmt/json 자동 승격 포함) |

코드 변경·재빌드 없이 이 파일 수정 + 에이전트 재시작으로 반영된다.

### 3.3 토큰 (`.env`)

4개 모두 **임의 시크릿**(예: `openssl rand -hex 32`). **미설정 시 기동 거부.** 값을 맞춰야 하는 상대가 있다:

| 토큰 | 용도 | 값을 맞출 상대 |
|------|------|--------------|
| `PUSH_OUTBOUND_TOKEN` | push(`/ingest`) Bearer | **수신 서버**가 기대하는 값 |
| `FLUSH_INBOUND_TOKEN` | `/flush`·`/drain-spool`·`/drain-status` | 이 API를 호출하는 소비자 |
| `STAT_INBOUND_TOKEN` | `/stat` | 소비자 |
| `SOS_INBOUND_TOKEN` | `/trigger-sos` | 소비자 |

선택 env: `CATEGORIES_PATH`·`FIELDS_PATH`(규칙 파일 경로), `RUST_LOG`(로그 레벨). 템플릿은 [`config/.env.example`](../config/.env.example).

---

## 4. 동작 검증

- **즉시 확인**: `curl -H "Authorization: Bearer <STAT_INBOUND_TOKEN>" http://127.0.0.1:9100/stat`
- **전 경로 E2E**: [`../tests/`](../tests/) 하네스 — 합성 에러 로그 주입(`inject_errors.sh`) → 파서 수집·분류 → (수신측)답변까지 자동 검증. 상세는 [`../tests/README.md`](../tests/README.md).
- pull API 상세는 [`pull-api.md`](pull-api.md).
