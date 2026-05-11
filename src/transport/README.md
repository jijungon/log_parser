# transport

Envelope를 수신측 서버로 전송하고, 실패 시 spool에 보관해 재시도하는 모듈.

| 파일 | 역할 |
|------|------|
| `http.rs` | gzip 압축·Bearer 인증·HTTP POST 전송, 응답 코드별 에러 분류 |
| `spool.rs` | 두 풀 WAL 파일 관리 (`new/` + `retry/`) — 아래 상세 참조 |

**에러 분류**:
- `Retryable` — 5xx, 네트워크 오류 → backoff 재시도 (5s→10s→20s…, 최대 300s)
- `Fatal` — 4xx → retry/로 이동, 재시도 중단 (수동 drain 필요)
- `RateLimited` — 429 → `Retry-After` 헤더 값만큼 대기 후 재시도

---

## Spool 두 풀 구조

```
spool_dir/
├── new/     ← 현재 cycle WAL (coordinator가 전송 전 저장)
│            전송 성공 → commit() 삭제
│            전송 실패 → move_to_retry() → retry/ 이동
│
└── retry/   ← 전송 실패 envelope (explicit drain 대기)
             POST /drain-spool 호출 시 시간 창 단위로 재전송
```

### new/ 동작

- `save()`: coordinator가 30분 cycle envelope을 전송 **전에** 저장 (WAL 원칙)
- `commit()`: 전송 성공 후 삭제 + `used_bytes` 감소
- `move_to_retry()`: 전송 실패 후 `retry/`로 이동 + `used_bytes` 감소
- 용량 초과 시(`max_mb` 초과): oldest 파일을 `retry/`로 evict 후 신규 저장
- 데몬 재시작 시: `pending()`으로 `new/` 내 미처리 파일 조회 → 백오프 재전송

### retry/ 동작

- `move_to_retry()`: `new/`에서 이동될 때 자동 생성
- `drain_window(from, to)`: ULID 생성 시각 기준 시간 창 필터링
- `drain_commit()`: drain 전송 성공 후 삭제
- 용량 상한 없음 — 운영자가 `POST /drain-spool`로 명시적 drain

### ULID 파일명

파일명 = ULID (예: `01JXYZ....json`). ULID에는 **생성 시각**(cycle 시작 시각, 밀리초)이 인코딩되어 있어 시간 창 기반 필터링이 가능하고, 자연 정렬이 시간순 정렬과 일치합니다.
