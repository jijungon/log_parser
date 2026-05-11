# coordinator

Cycle 단위로 DedupEvent를 수집하고 Envelope를 조립해 전송하는 핵심 루프.

| 파일 | 역할 |
|------|------|
| `mod.rs` | 메인 루프 — 타이머·flush 신호 수신, Envelope 전송 트리거 |
| `cycle.rs` | Cycle 상태 (seq, 수집 중인 events) 관리 및 Envelope 조립 |

**Cycle**: 에이전트가 로그를 수집하는 단위 기간. 기본 30분마다 또는 `/flush` 호출 시 종료되고, 종료 시 `seq`가 1 증가합니다. `(host_id, boot_id, seq)` 조합이 수신측의 중복 방지 키가 됩니다.

---

## 전송 흐름 (cycle_tick)

```
cycle_tick (30분)
  → spool.save()         new/ WAL 저장
  → send_with_backoff()  백오프 재시도
      성공 → spool.commit()       new/ 삭제
      Fatal(4xx) → spool.move_to_retry()   retry/ 이동
      Retryable 한도 소진 → spool.move_to_retry()   retry/ 이동
```

critical envelope은 성공 또는 Fatal까지 무한 재시도. non-critical은 `retry_max_normal+1`회 후 포기하고 retry/로 이동.

## 데몬 재시작 후 복구

시작 시 `spool.pending()`(new/ 목록)을 스캔해 미처리 WAL 파일을 최대 4건 동시 재전송합니다. retry/ 파일은 `POST /drain-spool` API로만 재전송됩니다.

## seq 영속성

`static_state.enabled: true`일 때 30분 cycle마다 다음 seq를 `seq_state_path`에 저장합니다. 데몬 재시작 시 이 값을 읽어 seq 연속성을 유지합니다.
