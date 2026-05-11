# 6_REFACTOR — 코드 정리 체크리스트

> x-review 결과 (2026-05-08) 기반. 섹션 순서 = 작업 우선순위.  
> 각 항목 완료 후 `[x]` 체크 → 코드 적용.

---

## 1. 제거 (Remove)

기능 손실 없이 삭제 가능한 코드들.

- [x] **`RawEvent` 열거형 제거**  
  `pipeline/raw_event.rs:36`  
  `RawEvent::Log(RawLogEvent)` 단일 변형만 존재, 실제 match 사용처 없음.  
  → `RawEvent` 삭제, `RawLogEvent`를 직접 사용하도록 호출부 수정.

- [x] **`Transport` 트레이트 제거**  
  `transport/mod.rs:11`  
  `HttpJsonTransport` 구현체 하나뿐. `async_trait` 보일러플레이트 + 동적 디스패치 비용만 있음.  
  → 트레이트 삭제, `Arc<dyn Transport>` → `Arc<HttpJsonTransport>` 직접 사용.

- [x] **`StaticStateConfig` 제거**  
  `config.rs:125-133`  
  `enabled`, `scan_interval_seconds`, `storage_path`, `seq_state_path` 4개 필드 모두 파싱 후 아무 곳에서도 읽히지 않음.  
  → 구조체 + YAML 기본값 항목 삭제.

- [x] **`serialize_strategy` + `body_size_limit_bytes` 제거**  
  `config.rs:120-121`  
  `InboundConfig`에 정의되어 있으나 다운스트림 어디서도 `cfg.inbound.serialize_strategy` / `cfg.inbound.body_size_limit_bytes` 읽지 않음.  
  → 두 필드 삭제.

- [x] **`vector_metrics_sock` 제거**  
  `config.rs:54-55`  
  `PipelineConfig`에 정의되나 어떤 spawner나 receiver에도 전달되지 않음.  
  → 필드 삭제.

- [x] **`get_clk_tck()` 함수 → 상수로 교체**  
  `inbound/collect.rs:354-358`  
  함수 이름과 달리 실제 `sysconf(_SC_CLK_TCK)` 호출 없이 `100` 하드코딩.  
  → `fn get_clk_tck()` 삭제, `const CLK_TCK: u64 = 100` 상수로 교체.

- [ ] **`CycleState` 모듈 인라인 (선택)**  
  `coordinator/cycle.rs`  
  `push()` + `finalize()` 2개 메서드뿐, `coordinator/mod.rs` 외 호출처 없음.  
  → `coordinator/cycle.rs` 삭제 후 `coordinator/mod.rs`에 인라인.  
  ※ 파일 분리 자체를 유지하고 싶다면 스킵 가능.

---

## 2. High 버그 수정 (Fix)

데이터 유실·보안·프로세스 안정성 직결. 우선 처리.

- [x] **`/flush` 응답 시 envelope 유실**  
  `inbound/flush.rs:115-127`  
  coordinator가 cycle reset한 **후에** 크기 체크 → 503 반환 시 finalize된 envelope이 spool에도 저장 안 되고 영구 소실.  
  → 크기 게이트 제거(caller가 큰 응답 수신), 또는 503 반환 전에 spool 저장.

- [x] **인증 토큰 미설정 시 무음으로 엔드포인트 열림**  
  `main.rs:196-198` + `http.rs:20`  
  `std::env::var(...).unwrap_or_default()` → 빈 문자열 → flush/stat/sos 인증 없이 접근 가능 상태. 경고 없음.  
  → 시작 시 토큰이 비어 있으면 `warn!("환경변수 미설정: {} — 인증 비활성화", env_key)` 출력.

- [x] **비정상 task 종료 시 exit code 0**  
  `main.rs:259-278`  
  `recv_task` / `pipeline_task` / `inbound_task` 오류 종료 → 에러 로그만 남기고 `Ok(())` 반환 → `systemd Restart=on-failure` 트리거 안 됨.  
  → `vector_task` 처리와 동일하게 `std::process::exit(1)` 호출.

- [x] **spool 파일 손상 시 무음 폐기**  
  `coordinator/mod.rs:45`  
  `if let Ok(envelope) = spool.load(&path)` — 파일 손상·읽기 실패 시 에러 무시, 데이터 소실, 로그 없음.  
  → `match` 분기로 교체, `Err(e)` 시 `warn!(path = %path.display(), err = %e, "spool 로드 실패 — 스킵")`.

- [x] **`send_with_backoff` Retryable 분기 데드코드**  
  `coordinator/mod.rs:184`  
  `limit = u32::MAX`(critical)일 때 `attempt >= limit` 조건이 never-true → "한도 소진" 로그 경로 영구 미실행.  
  → 조건을 `if !has_critical && attempt >= limit`으로 수정.

---

## 3. Medium 개선 (Improve)

운영 안정성·유지보수성 향상.

- [x] **gzip 압축 로직 3중 복제 → 공통 함수 추출**  
  `inbound/flush.rs:105`, `inbound/stat.rs`, `transport/http.rs:36`  
  `flush.rs`는 level 6 하드코딩, `http.rs`는 config 값 사용 → 동작 불일치.  
  → `fn gzip_bytes(data: &[u8], level: u32) -> Result<Vec<u8>>` 공통 함수 추출 (`inbound/mod.rs` 또는 별도 util).

- [x] **`check_auth` + `RateLimiter` 위치 이동**  
  `inbound/flush.rs:41`  
  `stat.rs`, `sos.rs`에서 `flush.rs`를 import해 사용 — 핸들러 파일에 유틸 로직이 있는 구조.  
  → `inbound/mod.rs` 또는 `inbound/auth.rs`로 이동.

- [x] **cycle `window` 필드: 설정값 대신 실제 경과 시간**  
  `coordinator/cycle.rs:65`  
  `/flush` 조기 발동 시에도 `window` = "start/start+1800s"로 기록 → 실제 데이터 범위와 불일치.  
  → `end_ts`를 `finalize()` 호출 시점의 `Utc::now()`로 계산.

- [x] **spool replay 동시 실행 수 제한**  
  `coordinator/mod.rs:39-54`  
  재시작 시 대기 파일 수만큼 `tokio::spawn` 생성 → 백로그 클 경우 HTTP 연결 폭발.  
  → `FuturesUnordered` + 세마포어(max 4) 또는 순차 재전송으로 제한.

- [x] **`/flush` oneshot 채널 실패 로그 추가**  
  `coordinator/mod.rs:105`  
  `let _ = signal.reply.send(envelope)` — 타임아웃 후 receiver drop 시 envelope 소멸, 로그 없음, seq 증가.  
  → `if signal.reply.send(envelope).is_err() { warn!("flush 응답 채널 닫힘 — envelope 폐기"); }`.

- [x] **Vector 재시작 윈도우: tumbling → sliding**  
  `pipeline/vector_spawn.rs:63-65`  
  crash 간격이 1h+1초이면 카운터 리셋 → 제한 우회 가능.  
  → `VecDeque<Instant>`로 최근 크래시 기록, 3600초 이내 항목만 count.

- [x] **spool TOCTOU 경쟁 조건 해소**  
  `transport/spool.rs:69`  
  두 task가 동시에 `check_capacity` 통과 → 용량 초과 저장 가능.  
  → `AtomicU64` 바이트 카운터 도입 (`save` 시 +, `commit` 시 -).

---

## 4. Performance

- [x] **`/etc/passwd` N회 읽기 → 1회로**  
  `inbound/collect.rs:447`  
  `uid_to_name()`이 프로세스마다 `/etc/passwd` 읽음.  
  → `collect_processes()` 루프 전 1회 파싱, `HashMap<u32, String>` 전달.

- [x] **open fd count: readdir N회 → `FDSize` 필드 재사용**  
  `inbound/collect.rs:414`  
  `/proc/{pid}/fd` readdir을 프로세스마다 실행.  
  → 이미 읽는 `/proc/{pid}/status`의 `FDSize:` 필드 파싱으로 대체.

- [x] **spool `check_capacity`: 디렉터리 전체 스캔 → 카운터**  
  `transport/spool.rs:59`  
  `save()` 호출마다 디렉터리 stat 전체 합산.  
  → Medium 3-7 항목(`AtomicU64`)과 동일 수정으로 해결됨.

- [x] **로그 파일 전체 읽기 → `BufReader` 스트리밍**  
  `inbound/collect.rs:987`  
  `/var/log/messages` 전체를 메모리에 올린 후 4시간 컷오프 필터링.  
  → `BufReader::lines()` 라인별 반복으로 전체 파일 메모리 적재 방지.  
  ※ syslog 파일은 오래된→새로운 순 정렬이므로 early-break 불가 (최신 줄이 파일 끝에 위치).

- [x] **`collect_metrics` 600ms 대기 → 200ms로 단축**  
  `inbound/collect.rs:46`  
  `tokio::time::sleep(200ms)` × 3 순차 실행.  
  → before 샘플 3개 병렬 수집 → sleep 1회 → after 샘플 3개 병렬 수집.

---

## 5. Tests 추가

현재 `#[cfg(test)]` 모듈 전무. 아래 순서로 추가.

- [x] **`transport/spool.rs` — WAL 라운드트립**  
  `save` → `load` → `commit` 검증, `commit` 후 파일 삭제 확인, 용량 초과 시 `Err` 반환 확인.

- [x] **`dedup/window.rs` — DedupWindow**  
  윈도우 내 병합 시 count 증가, 윈도우 만료 시 기존 이벤트 방출 + 새 항목 시작, `flush_expired` 만료 항목만 방출, critical severity 승격.

- [x] **`normalize/tokens.rs` — `normalize()`**  
  UUID·IPv4·MAC·PATH·숫자 각각 치환, 복합 템플릿 결과 검증.

- [x] **`normalize/severity.rs` — `finalize()`**  
  각 키워드 대소문자 변형, `initial` 값 매핑, 미인식 시 `"info"` 폴백.

- [x] **`platform/discovery.rs` — `OsInfo::detect`**  
  Ubuntu·RHEL·Rocky·미인식 OS별 `syslog_path` / `auth_log_path` 검증.

- [x] **`normalize/fields.rs` — `extract_fields()`**  
  OOM kill PID, SSH invalid user, key=value 형식, first-match 우선 규칙.

- [x] **`coordinator/cycle.rs` — `CycleState`**  
  `push()` 후 severity 카운터 확인, `finalize()` body 비어있지 않음 확인.

- [x] **`platform/cgroup.rs` — `parse_memory()`**  
  `"256m"`, `"2g"`, 정수, 잘못된 입력(`Err`) 케이스.

---

## 작업 순서 가이드

```
1단계: 제거 (섹션 1) → 빌드 통과 확인
2단계: High 버그 수정 (섹션 2)
3단계: Medium 개선 (섹션 3) — 우선도 낮은 항목은 선택
4단계: Perf (섹션 4) — 운영 투입 전 적용 권장
5단계: Tests (섹션 5) — 각 수정 직후 해당 테스트 추가
```
