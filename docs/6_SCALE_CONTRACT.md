# 파서 출력 계약 — 대규모 확장분 (구현예정 포함)

> `4_RECEIVER_CONTRACT.md`·`RECEIVER_TYPE_SPEC.md`의 **확장 문서**.
> 대규모 플릿(수천~수만 호스트) 전제에서, 파서가 **추가로 내보내야/노출해야** 하는 것들의 명세.
> 원칙: **이 문서의 항목은 전부 파서(엣지)가 책임진다.** 중앙 플랫폼(Kafka·ClickHouse·벡터·S3)을 어떻게 짓는지는 여기서 다루지 않으며, 그 설계는 `log_stack_AI/docs/`(수신측)에 있다.
> 상태 표기: **[적용됨]** = 소스 반영·검증 완료 / **[예정]** = 미구현, 계약만 확정.

---

## 0. 왜 이 문서가 파서 쪽에 있나

파서는 receiver를 만들지 않지만, "무엇을 내보내는가"는 파서가 소유·책임진다(`4_RECEIVER_CONTRACT.md` §0 철학과 동일). 아래 항목들은 **어떤 중앙 플랫폼을 나중에 짓든 공통 전제**이므로, 지금 파서에서 먼저 확정·구현한다.

---

## 1. 증분 pull API — `GET /events?since_seq` [보류 — 미채택]

> **결정 (2026-07-15)**: 중앙 플랫폼이 기존 push + 스냅샷 pull(`/trigger-sos`·`/flush`)로 소비하기로 하여 **이 증분 API는 채택하지 않는다.** 놓친 데이터 복구는 파서의 spool `retry/` + `POST /drain-spool`로 이미 해결되므로 기능 공백이 없다. 아래 명세는 향후 초 단위 신선도가 실제 요구될 때 재검토용으로 보존한다. (`log_stack_AI/docs/1_CENTRAL_PLATFORM_ROADMAP.md §2.5`)


기존 push(30분 배치) + sos(스냅샷) 외에, 소비자가 **"지난번 이후 새 것만"** 당겨가는 커밋로그형 창구를 추가한다. 로컬 이벤트 스토어(SQLite 장부, 구조-1)를 원천으로 한다.

```
GET /events?since_seq=<cursor>&limit=<n>

응답:
{
  "db_epoch":    "<UUID>",      # 스토어(장부) 파일의 세대 식별자
  "events":      [ <DedupEvent>, ... ],   # seq 오름차순
  "latest_seq":  <u64>,         # 현재 스토어의 최신 seq (소비자 lag 계산용)
  "oldest_seq":  <u64>,         # 보존 하한 (이보다 오래된 건 TTL로 사라짐)
  "next_cursor": <u64>,         # 다음 호출에 넣을 since_seq
  "has_more":    <bool>
}
```

**계약 규칙:**
| 규칙 | 내용 |
|---|---|
| 커서 = `(db_epoch, seq)` | 소비자는 둘을 함께 저장. `db_epoch`가 응답과 다르면 스토어가 재생성된 것 → 전체 resync |
| resync 신호 | 요청한 `since_seq < oldest_seq`이면 `410 Gone` + `{ "resync": true, "oldest_seq": N }` |
| keyset 페이지네이션 | `WHERE seq > ? ORDER BY seq LIMIT n` — offset 미사용 |
| 멱등 | 재호출로 같은 seq를 다시 받아도 무해 (§3 멱등키로 소비자가 dedup) |

> 참조 패턴: Kafka consumer offset, journald `--after-cursor`(boot-ID = 여기서 db_epoch), Debezium 증분 스냅샷.

---

## 2. push 신호화 — envelope에 `max_event_seq` [보류 — §1과 함께 미채택]

> §1(증분 pull)을 안 쓰므로 이 push 신호도 불필요. 중앙 플랫폼은 push envelope 자체를 데이터로 받고, 갭은 spool drain으로 복구한다.


30분 push envelope의 headers에 이 사이클이 스토어에 기록한 **최대 seq**를 실어 보낸다. 소비자는 push로 "새 데이터 있음"을 알고, 실제 데이터는 §1로 당겨간다(= push 신호 / pull 데이터). push는 **호스트 생존 신호** 역할도 유지한다.

```
headers: {
  ...,
  "max_event_seq": <u64>,   # [예정] 이 사이클까지 스토어에 있는 최신 seq
  "seq_range":     [<from>, <to>]   # [예정] 이 envelope이 커버하는 범위
}
```

---

## 3. 멱등키 (기존 계약 재확인 + pull 경로 적용) [적용됨 일부]

`4_RECEIVER_CONTRACT.md §2`가 이미 `(host_id, boot_id, seq)` 중복 방지를 명시함. 대규모에선 이 키를 **push·pull·drain·재생 전 경로**에서 동일하게 쓴다. 수신측은 이 키로 upsert → at-least-once 전송이 안전.

---

## 4. `template_id` — 임베딩·조인을 위한 안정 식별자 [예정 · 지문 회전 1회]

**버그 수정 포함**: 현재 cycle 경로는 `xxh3(template|severity|source)`, sos 경로는 `xxh3(source:template)`로 **공식이 다르다** → 같은 로그가 두 경로에서 조인되지 않음. 이를 통일하고, 추가로:

| 필드 | 정의 | 용도 |
|---|---|---|
| `fingerprint` | `xxh3(template | severity | source)` (통일) | **dedup 키** (기존 유지) |
| `template_id` | `xxh3(template 텍스트만)` | 수신측이 **템플릿당 1회만 임베딩** (벡터 비용 = 이벤트 수 아닌 템플릿 종류 수) |

> 회전 주의: template_id 도입 + 공식 통일은 fingerprint를 **1회 회전**시킨다. 다른 회전성 변경과 **한 번에 묶어** 적용한다. (연속성은 요구사항 아님 — 오너 확인됨)

---

## 5. 카운터 노출 — logs-to-metrics [예정]

`CycleState`가 이미 계산 중인 `by_category`·`by_severity` 카운트를 두 곳에 노출한다.

| 노출 | 형태 | 용도 |
|---|---|---|
| `GET /stat` 의 `log_counts` 섹션 | 현재 사이클 카운터(읽기 전용) | "지금 category별 몇 건"을 이벤트 스캔·`/flush` 없이 즉답 |
| 스토어 `counters` 테이블 | `(bucket_ts, category, severity, count)` 사이클마다 batch insert | "추이"를 인덱스 GROUP BY로 |

> 참조: Vector `log_to_metric`, OTel count connector, google/mtail.

---

## 6. 운영·전송 위생 [예정]

| 항목 | 계약/동작 |
|---|---|
| critical 즉시 push | critical severity는 30분 사이클을 우회, 단건 mini-envelope(`event_kind="critical_alert"`)로 즉시 전송. 폭주 방지 토큰버킷 |
| compress-once | spool에 `.json.gz`로 저장, 전송 시 동일 바이트 재사용(재직렬화·재압축 금지) |
| retry/ 상한 | 데드레터에 용량 cap + age TTL. 무제한 디스크 성장 금지 |
| 억제 회계 | dedup 접힘/LRU evict 수를 `(fingerprint, dropped_count)` meta-event로 방출 → 수신측이 "요약이 뺀 양"을 정량화 |

---

## 7. schema 진화 (기존 §5 준수)

위 추가 키(`max_event_seq`, `template_id`, `log_counts` 등)는 전부 **키 추가(additive)** → `4_RECEIVER_CONTRACT.md §5`에 따라 minor 버전업, 기존 수신측과 호환(모르는 키 무시).

---

## 부록 · 구현 항목 매핑

| 계약 조항 | 구현 항목(구조) | 상태 |
|---|---|---|
| §1 pull API | 구조-2 | **보류(미채택)** — 중앙이 push+스냅샷으로 소비 |
| §2 push 신호 | 구조-2 | **보류(미채택)** |
| §3 멱등키 | (기존 계약) | 이미 존재 — push 재전달에 그대로 사용 |
| §4 template_id·지문 통일 | 구조-5 | 선택 — RAG 붙일 때 |
| §5 카운터 | 구조-4 | 선택 — 대시보드 붙일 때 |
| §6 critical 즉시 push | 구조-3 | 선택 — 실시간 알림 필요 시 |
| §6 compress-once | 구조-6 | **적용됨** (cargo test 통과) |
| §6 retry 상한 | 구조-7 | **적용됨** (cargo test 통과) |
| §6 억제 회계 | 구조-8 | 선택 |
| (기반) 이벤트 스토어 | 구조-1 | **보류** — since_seq 미채택으로 주 존재이유 소멸; sos 지연이 실제 문제될 때만 |
| (핫패스 성능) | 코드-1~5 | **적용됨** (cargo test 통과) |
