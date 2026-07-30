# src

> [← 프로젝트 루트](../README.md)

Rust 에이전트 소스. 데이터 흐름 순으로 모듈이 구성되어 있습니다.

| 모듈 | 역할 | README |
|------|------|--------|
| `lib.rs` | 크레이트 루트 — 모듈을 `pub`로 노출(프로덕션 바이너리·부하도구가 같은 코드 공유), 규칙 파일 기본 경로 상수 | — |
| `main.rs` | 프로세스 진입점 — 부트스트랩 Step 1 cgroup 격리 → 2 전송 스모크 테스트 → 3 배포판 탐지·Vector 기동 → 4 파이프라인(수신·dedup·coordinator) → 5 inbound 서버. SIGTERM/SIGINT 시 graceful 종료(진행 중 cycle spool 저장, 최대 10s 대기) | — |
| `config.rs` | `agent.yaml` 파싱·기본값·유효성 검사 | — |
| `envelope.rs` | Envelope / DedupEvent 타입 정의 | — |
| `process.rs` | 로그 한 줄 공용 처리(strip→토큰화→severity→fingerprint→dedup) — coordinator·inbound(sos)가 공유. fingerprint 공식(Xxh3)의 유일한 정의처 | — |
| `platform/` | 호스트 환경 감지 (배포판·로그 경로, 로그 소스 probe, cgroup 자기 제한) | [platform/](platform/README.md) |
| `pipeline/` | Vector 실행·감시, Unix socket IPC 수신 | [pipeline/](pipeline/README.md) |
| `normalize/` | 로그 정규화 (토큰화 template·severity·category·필드 추출) | [normalize/](normalize/README.md) |
| `dedup/` | 슬라이딩 윈도우 중복 제거 | [dedup/](dedup/README.md) |
| `coordinator/` | Cycle 상태 관리, Envelope 조립·전송, graceful shutdown | [coordinator/](coordinator/README.md) |
| `transport/` | HTTP 전송, 재시도, spool WAL(new/retry/corrupt), retry/ 자동 drain | [transport/](transport/README.md) |
| `inbound/` | Pull API 서버 :9100 (`/stat`, `/trigger-sos`, `/flush`, `/raw`, `/drain-spool`) | [inbound/](inbound/README.md) |

## 데이터 흐름

```
Vector (자식 프로세스, pipeline/vector_spawn이 감시·재시작)
  │  Unix socket ×2 (critical / normal, JSON line)
  ▼
pipeline/vector_receiver ──mpsc──▶ coordinator/run_pipeline
                                     │ ingest_event → process::process_line
                                     │   strip → tokens(template) → severity
                                     │   → fingerprint → dedup 병합/등록
                                     │   (첫 등장에만 fields 추출 + category 분류)
                                     ▼
                                   dedup window ── 5s tick 만료 방출 ──▶ cycle
                                     ▼
                                   cycle_tick(기본 30분) 또는 /flush → finalize
                                     ▼
                     transport: spool new/ WAL 저장 → HTTP push (백오프)
                       포기 시 retry/ 파킹 → 기동 replay·자동 drain·/drain-spool 재전송

  (병행) inbound :9100 pull API — /stat·/trigger-sos·/raw는 시스템 상태·원문 로그 즉석 수집
```

**개발 도구** — `bin/loadtest.rs`: 파싱·전송 부하 측정용 별개 바이너리(프로덕션과 무관).
실행 `cargo run --release --bin loadtest -- --gb 100 [--distinct N] [--endpoint URL]`.
상세·옵션은 파일 상단 주석, 결과 요약은 [CHANGELOG](../CHANGELOG.md) 참조.
