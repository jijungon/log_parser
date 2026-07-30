# transport

[← src](../README.md) · 관련: [coordinator](../coordinator/README.md) · [inbound](../inbound/README.md)

Envelope를 수신측 서버로 전송하고, 실패 시 spool에 보관해 재시도·재전송하는 모듈.

| 파일 | 역할 |
|------|------|
| `http.rs` | gzip 압축·Bearer 인증·HTTP POST 전송, 응답 코드별 에러 분류. `compress()` 1회 + `send_compressed()` 반복으로 재시도마다 재압축하지 않음 |
| `spool.rs` | 세 풀 WAL 파일 관리 (`new/` + `retry/` + `corrupt/`) — 아래 상세 |
| `drain.rs` | retry/ 재전송 실행 코어 — HTTP `/drain-spool`(inbound)과 coordinator의 자동 drain이 공유 |

**에러 분류**:
- `Retryable` — 5xx, 네트워크 오류 → backoff 재시도 (5s→10s→20s…, 최대 300s)
- `Fatal` — 4xx → retry/로 이동, 재시도 중단
- `RateLimited` — 429 → `Retry-After` 헤더 값만큼 대기 후 재시도

---

## Spool 세 풀 구조

```
spool_dir/
├── new/     ← 현재 cycle WAL (전송 전 저장, 상한 2048MB)
│            전송 성공 → commit() 삭제
│            전송 포기 → move_to_retry() → retry/ 이동
│            용량 초과 → oldest 파일을 retry/로 evict 후 신규 저장
│
├── retry/   ← 전송 포기 envelope (상한 1024MB · TTL 168h — 초과 시 오래된 것부터 삭제, warn)
│            자동 drain(라이브 전송 성공 시) 또는 POST /drain-spool 로 재전송
│
└── corrupt/ ← 파싱 실패(잘림 등) 파일 격리 — 재전송 경로 밖, 포렌식 보존
             상수 상한 256MB, 초과 시 오래된 것부터 삭제
```

### 쓰기 불변식 (crash-safe)

- 모든 spool 파일과 seq 상태 파일은 `write_file_atomic`: 같은 디렉토리 temp 파일에 write+fsync → rename → 부모 디렉토리 fsync(best effort). **잘린 `.json`은 존재할 수 없다.**
- 기동 스캔이 이전 crash가 남긴 temp(`.{ulid}.json.tmp`)·숨김 파일을 정리하고, `.json`만 용량 집계 대상으로 센다.
- startup replay는 `load_or_quarantine()` 사용 — 파싱 실패 파일을 corrupt/로 격리해 매 기동 반복 실패와 used_bytes 점유를 막는다 (읽기 IO 실패는 일시적일 수 있어 격리하지 않음).

### new/ 동작

- `save_bytes()`: coordinator가 cycle envelope을 전송 **전에** 저장 (WAL 원칙, 직렬화 bytes 재사용)
- `commit()`: 전송 성공 후 삭제 + `used_bytes` 감소
- `move_to_retry()`: 전송 포기 후 `retry/`로 이동
- 데몬 재시작 시 `pending()`으로 미처리 파일 조회 → 백오프 재전송

### retry/ 동작

- `drain_window(from, to)`: ULID 생성 시각 기준 `[from, to)` 창 필터링
- `drain_commit()`: drain 전송 성공 후 삭제 (실패 파일은 유지 — 다음 drain 재시도)
- `save_bytes_to_retry()`: new/ WAL 저장이 실패했던 envelope의 마지막 방어선 — 메모리 bytes를 retry/에 직접 저장
- 상한(용량·TTL)은 기동 시와 파일 유입 시마다 적용

## retry/ 자동 drain (drain.rs)

- **라이브 전송 성공 = 수신 복구 신호** → `AutoDrainHandle::trigger()`가 retry/ 전체 창 drain을 백그라운드로 시작. retry/ 비면 no-op(O(1) 카운터 선확인), `transport.auto_drain: false`로 비활성화 가능.
- `try_start()`가 유일한 진입점 — `in_progress` AtomicBool CAS 가드를 HTTP drain과 **공유**해 동시에 drain은 항상 1개: HTTP drain 진행 중이면 자동 drain은 조용히 생략, 자동 drain 진행 중이면 HTTP는 409.
- `drain_task`는 RAII `InProgressGuard`로 panic 시에도 가드를 해제. 진행 상태(`DrainState`)는 `GET /drain-status`와 동일 인스턴스.

### ULID 파일명

파일명 = ULID (예: `01JXYZ....json`). ULID에는 **생성 시각**(밀리초)이 인코딩되어 있어 시간 창 기반 필터링이 가능하고, 자연 정렬이 시간순 정렬과 일치합니다.

## 관련 설정 키 (`transport.`)

| 키 | 기본값 | 의미 |
|----|-------|------|
| `endpoint` / `token_env` | — / `PUSH_OUTBOUND_TOKEN` | 수신 서버 주소·Bearer 토큰 env (토큰 미설정 시 기동 거부) |
| `spool_dir` / `spool_max_mb` | `/var/lib/log_parser/spool` / 2048 | spool 위치·new/ 상한 |
| `retry_max_mb` / `retry_ttl_hours` | 1024 / 168 | retry/ 용량·보관 상한 (0 = 무제한) |
| `auto_drain` | true | 전송 성공 시 retry/ 자동 drain |
| `retry_max_normal` | 5 | non-critical 재시도 한도 (critical은 2×) |
| `retry_base_seconds` / `retry_max_seconds` | 5 / 300 | backoff 시작·상한 |
