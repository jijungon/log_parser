# coordinator

[← src](../README.md) · 관련: [transport](../transport/README.md) · [dedup](../dedup/README.md) · [inbound](../inbound/README.md)

Cycle 단위로 DedupEvent를 수집하고 Envelope를 조립해 전송하는 핵심 루프.

| 파일 | 역할 |
|------|------|
| `mod.rs` | 메인 select 루프 — 이벤트 수신·flush 신호·5초 dedup_tick·cycle_tick·종료 신호 처리, 백오프 전송(`send_with_backoff`). 수신 루프와 종료 flush가 `ingest_event()` 공용 헬퍼(normalize→dedup→cycle) 공유 |
| `cycle.rs` | Cycle 상태 (seq, 수집 중 events, severity·category 카운터) 관리 및 Envelope 조립. per-cycle 이벤트 cap — critical은 항상 수용(oldest evict), non-critical은 전원 critical이면 스킵 |

**Cycle**: 에이전트가 로그를 수집하는 단위 기간. 기본 30분마다 또는 `/flush` 호출 시 종료되고, 종료 시 `seq`가 1 증가합니다. `(host_id, boot_id, seq)` 조합이 수신측의 중복 방지 키가 됩니다.

---

## 전송 흐름 (cycle_tick)

```
cycle_tick (30분)
  → 1회 직렬화 → spool.save_bytes()   new/ WAL 저장 (spawn_blocking)
  → persist_seq()                     WAL 저장 성공 후에만 seq 영속화
  → send_with_backoff()               라이브 전송 Semaphore(4) 안에서 백오프 재시도
      성공          → spool.commit() + auto_drain.trigger()  (retry/ 자동 drain)
      Fatal(4xx)    → park_to_retry()
      재시도 한도 소진 → park_to_retry()
```

- **재시도 한도**: non-critical은 최초 전송 포함 `retry_max_normal`+1회(기본 6회) 후 포기. critical은 2×`retry_max_normal`회 재시도까지 버틴 뒤 **retry/로 파킹** — 무한 재시도로 전송 태스크·압축 body가 누적돼 128MB cgroup 안에서 OOM되는 것을 막는다 (디스크 보존 원칙은 WAL로 유지, error! 로그).
- **park_to_retry**: WAL 파일이 있으면 retry/로 이동. WAL 저장이 실패했던 envelope(빈 경로)은 메모리 bytes를 `save_bytes_to_retry()`로 retry/에 직접 저장 — 그것마저 실패했을 때만 실제 유실(byte/event 수와 함께 error!).
- **compress-once**: 직렬화+gzip을 루프 진입 전 1회만 수행하고 모든 재시도에서 재사용 (재시도 폭풍 시 5% CPU 예산 보호).
- **동시성 상한**: 라이브 전송·startup replay 각각 Semaphore(4) — 수신 서버 장기 장애 시 태스크 무한 누적 방지.

## Graceful shutdown (SIGTERM/SIGINT)

종료 신호(watch 채널, select 최우선 arm) 수신 시: 채널 잔여 이벤트 흡수 → dedup `flush_all()` → finalize → **spool 저장 → 저장 성공 후에만 seq 영속화** 순으로 마감하고 종료합니다. **네트워크 전송은 하지 않으며** 다음 기동의 startup replay가 배달합니다 (빈 cycle은 저장 생략). main은 이 마감을 최대 10초까지 기다립니다.

## 데몬 재시작 후 복구

시작 시 `spool.pending()`(new/ 목록)을 스캔해 미처리 WAL 파일을 최대 4건 동시 재전송합니다. 파싱 불가(잘린) 파일은 `load_or_quarantine()`이 corrupt/로 격리합니다. retry/ 파일은 자동 drain(라이브 전송 성공 시) 또는 `POST /drain-spool`로 재전송됩니다.

## seq 영속성

`static_state.enabled: true`일 때 cycle 종료(cycle_tick·`/flush`·graceful shutdown)마다 다음 seq를 `seq_state_path`에 저장합니다. 저장은 `write_file_atomic`(temp→fsync→rename)을 spawn_blocking으로 수행 — 일반 write가 crash 시 남기는 잘린 seq 파일은 기동 파싱 실패 → seq 1 리셋으로 이어집니다. cycle_tick에서는 WAL 저장 성공 후에만 seq를 앞으로 보내 "전송된 것처럼 보이는 구멍"을 방지합니다.

## 관련 설정 키

| 키 | 기본값 | 의미 |
|----|-------|------|
| `cycle.window_seconds` | 1800 | cycle 주기(초) |
| `transport.retry_max_normal` | 5 | 백오프 재시도 한도 (critical 2×) |
| `pipeline.body_max_events_per_cycle` | 100000 | cycle당 이벤트 cap |
| `pipeline.body_max_size_mb` | 50 | envelope 크기 경고 임계 (초과 시 warn만, 전송은 진행) |
| `static_state.enabled` / `seq_state_path` | true / `/var/lib/log_parser/seq.state` | seq 영속화 |
