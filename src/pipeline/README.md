# pipeline

Vector 프로세스를 실행하고 로그 이벤트를 수신하는 모듈.

| 파일 | 역할 |
|------|------|
| `vector_config.rs` | 에이전트 설정을 바탕으로 Vector TOML 설정 파일 동적 생성 |
| `vector_spawn.rs` | Vector 프로세스 실행·감시, 재시작 처리 |
| `vector_receiver.rs` | Vector → 에이전트 간 Unix socket IPC 수신 루프 |
| `raw_event.rs` | Vector 출력 JSON 역직렬화 타입 |

**데이터 흐름**: Vector 프로세스 → Unix socket → `raw_event::RawLogEvent` → `normalize/`
