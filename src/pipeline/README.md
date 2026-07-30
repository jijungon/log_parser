# pipeline

[← src](../README.md) · 관련: [platform](../platform/README.md) · [normalize](../normalize/README.md) · [coordinator](../coordinator/README.md)

Vector 프로세스를 실행하고 로그 이벤트를 수신하는 모듈.

| 파일 | 역할 |
|------|------|
| `vector_config.rs` | platform probe 결과 기반 Vector TOML 동적 생성 (소스·필터·라우팅·sink). 사용자 제공 `vector_config` 파일이 있으면 생성 생략 |
| `vector_spawn.rs` | Vector 실행·감시 — 비정상 종료 시 5초 후 재시작, 시간당 한도 초과 시 중단. 자식 PID를 에이전트 cgroup에 등록, `restarts_24h` 카운터를 envelope `process_health`로 보고 |
| `vector_receiver.rs` | Vector → 에이전트 Unix socket(권한 0600) IPC 수신 루프. JSON 파싱 실패 라인은 warn 후 스킵(루프 유지) |
| `raw_event.rs` | Vector 출력 JSON 역직렬화 타입 (`RawLogEvent`, 스키마 FROZEN) |

**데이터 흐름**: Vector 프로세스 → Unix socket ×2(critical/normal) → `raw_event::RawLogEvent` → mpsc 채널 → [coordinator](../coordinator/README.md)(내부에서 [normalize](../normalize/README.md) 적용)

## 생성되는 Vector 설정 (vector_config.rs)

- 소스는 probe 결과에 따라 journald / file_syslog / file_auth / file_audit만 생성. syslog·auth 파일 소스에는 타임스탬프 헤더 기준 multiline 병합(스택트레이스 이어붙임) 적용 — audit(비-syslog 포맷)·journald(이미 구조화)는 제외.
- `drop_noise` 필터가 journald debug(PRIORITY 7)를 소켓 이전에 버려 Rust 부하·전송량을 절감.
- `route_severity`가 `.log_parser_severity` 기준으로 critical / normal 소켓을 분리.
- **sink 정책 비대칭 (의도적)**: 두 sink 모두 512MB disk buffer + acknowledgements.
  - critical: `when_full = "block"` — 유실 대신 배압.
  - normal: `when_full = "drop_newest"` — 버퍼가 가득 차면 **Vector 내부에서** 최신 이벤트가 버려진다. 이 드롭은 에이전트 카운터로는 관측되지 않는다(알려진 관측 한계).

## 장애 시 동작 (vector_spawn.rs)

- Vector 정상 종료 → 감시 태스크 정상 반환. 비정상 종료 → 5초 후 재시작.
- 시간당 재시작 횟수가 `vector_max_restarts_per_hour`(기본 5)에 도달하거나 spawn이 연속 5회 실패하면 감시 태스크가 에러로 종료 → main이 프로세스를 내린다.

## 관련 설정 키 (`pipeline.`)

| 키 | 기본값 |
|----|-------|
| `vector_bin` / `vector_config` | `/app/vector/bin/vector` / `/etc/log_parser/vector.toml` |
| `vector_critical_sock` / `vector_normal_sock` | `/run/log_parser/events_{critical,normal}.sock` |
| `vector_max_restarts_per_hour` | 5 |
| `channel_capacity` | 10000 |
