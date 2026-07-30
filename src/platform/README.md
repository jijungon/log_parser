# platform

[← src](../README.md) · 관련: [pipeline](../pipeline/README.md)

호스트 환경 감지 및 런타임 자기 제한(cgroup).

| 파일 | 역할 |
|------|------|
| `discovery.rs` | `/etc/os-release` 기반 배포판 계열 판별(Debian/RHEL/Unknown) → syslog·auth 로그 경로 결정 (RHEL 계열: `/var/log/messages`·`/var/log/secure`) |
| `capability.rs` | Vector 바이너리, journald(journalctl 우선 → 저널 디렉토리 폴백), syslog/auth/audit 파일 존재 여부를 `Probes` 구조체로 반환. `has_log_sources()`로 로그 소스 유무를 판단해 시작 게이트 역할 수행 |
| `cgroup.rs` | cgroup v2 self-attach — `memory.max`·`cpu.max` 기록 후 자기 PID 등록·검증. Vector 자식 PID도 같은 cgroup에 등록(`attach_pid`) |

## 핵심 동작 / 장애 시

- Vector 바이너리가 없으면 기동 거부. probe 결과 로그 소스가 하나도 없으면 Vector 기동만 생략(경고)하고 나머지는 계속 (main.rs Step 3).
- `cgroup.enabled: true`(기본)일 때 self-attach 또는 검증 실패는 **기동 거부** — 격리 없는 실행을 막는다 (main.rs Step 1).
- `cgroup.enabled: false`면 리소스 격리 없이 실행(warn). 도커 배포 프로필(`config/agent_docker.yaml`)이 이 모드를 사용하며, 현재 `docker-compose.yml`에는 mem_limit 등 컨테이너 수준 제한도 걸려 있지 않다 — 컨테이너 배포 시 자원 상한은 별도 확보 필요.
- Vector 자식 PID의 cgroup 등록 실패는 경고만 남기고 계속 진행.

## 관련 설정 키 (`cgroup.`)

| 키 | 기본값 | 의미 |
|----|-------|------|
| `enabled` | true | cgroup self-attach 사용 여부 |
| `path` | `/sys/fs/cgroup/log_parser_agent` | cgroup 경로 |
| `memory_max` | `128m` | 메모리 상한 |
| `cpu_max` | `"50000 1000000"` | CPU 상한 (50ms/1000ms = 5%) |
