# src

Rust 에이전트 소스. 데이터 흐름 순으로 모듈이 구성되어 있습니다.

| 모듈 | 역할 |
|------|------|
| `main.rs` | 프로세스 시작, 모듈 조립, SIGTERM 처리 |
| `config.rs` | YAML 설정 파싱 및 유효성 검사 |
| `envelope.rs` | Envelope / DedupEvent 타입 정의 |
| `platform/` | 호스트 환경 감지 (host ID, boot ID, 로그 경로, cgroup) |
| `pipeline/` | Vector 실행·IPC 수신, 로그 수집 |
| `normalize/` | 로그 정규화 (토큰화 → severity → category → 필드 추출) |
| `dedup/` | 슬라이딩 윈도우 중복 제거 |
| `coordinator/` | Cycle 상태 관리, Envelope 조립, flush 신호 처리 |
| `transport/` | HTTP 전송, 재시도, spool WAL |
| `inbound/` | Pull API 서버 (`/stat`, `/trigger-sos`, `/flush`) |
