# 변경 이력

> 최신 항목을 위에 추가한다. (루트 [README](README.md) 요약, 상세 설계는 [docs/](docs/README.md))

## 2026-07-23 — `/raw` 원문 로그 드릴다운 엔드포인트

요약(`/flush`·`/trigger-sos`, dedup)만으로 부족한 상세 대처를 위해, 최근 원문 로그를
on-demand로 당기는 `GET /raw`를 추가했다.

- **`GET /raw?since=1h&sources=syslog,auth,kernel,journald&max_mb=10`** — dedup 안 된 원문 라인 반환.
  파일(syslog/auth/kernel) + journald(`journalctl --since`). gzip `text/plain` + `X-Raw-Bytes`·`X-Raw-Lines`·`X-Raw-Truncated`·`X-Raw-Window` 헤더.
- **bounded**: `since` 기본 1h/최대 24h, `max_mb` 기본 10/하드캡 30 — 파서 cgroup(128MB) 보호. "전량"은 불가(전체는 소스 서버 직접).
- 인증 **SOS 토큰 재사용**(새 필수 토큰 없음), rate-limit은 `/trigger-sos`와 공유(`collection_rate`).
- `collect.rs`의 syslog/ISO 타임스탬프 파서를 `pub(crate)` 모듈 함수로 승격해 `/raw`와 공유(동작 불변).
- 검증: `cargo test` **181 passed**(신규 raw 6건, 파일 읽기 통합 테스트 포함).
- 배포: 각 서버 `docker compose build && up -d` 재배포 후 호출 가능. 도커 파서는 `journald` 소스에 호스트 저널 마운트 필요(미마운트 시 파일 소스만 동작).
- 후속: journald는 `journalctl --merge`로 조회 — 컨테이너 machine-id가 호스트와 달라, --merge 없이는 마운트된 호스트 저널을 못 읽고 "-- No entries --"가 되던 것을 수정.

## 2026-07-21 — 부하 테스트 도구 + 라이브러리 분리

프로덕션 파서와 **별개**로 파싱·전송 속도와 메모리 거동을 측정하는 도구를 추가했다.
파서 동작은 불변(모듈을 lib로 노출만).

| 구분 | 내용 |
|------|------|
| 라이브러리 분리 (#16) | 바이너리 전용 크레이트 → `src/lib.rs`로 모듈 `pub` 노출(lib+bin). `main.rs`는 `use log_parser::…`로 전환. 동작·테스트 불변(175 passed) |
| 부하 도구 (#16) | `src/bin/loadtest.rs` — 파일/Vector 없이 메모리 스트리밍 생성 → 실제 `process_line`→dedup→`CycleState` envelope→실제 transport(gzip+POST). 옵션 `--gb/--endpoint/--window-seconds/--lru-cap/--distinct`. 번들 수신기 `examples/loadtest_receiver.py`(표준 라이브러리) |
| 카디널리티 계측 (#17) | `--distinct`(고유 템플릿 dial) + `DedupWindow::total_evictions()` getter. 리포트에 만료방출/창보유/**LRU폐기** 분리 표시 |
| 기본값 정렬 (#18) | loadtest 기본 `lru_cap` 200,000 → **50,000**(프로덕션 `agent.yaml`과 동일). 이전 200k는 테스트 아티팩트 |

**실측 결과 (VM, cgroup off = 미제한 처리량):**

- **저카디널리티 100GB**: 파싱 **44.7 MB/s**(53만 lines/s) · peak RSS **39MB** · dedup 후 gzip **1.3MB**/사이클
- **고카디널리티(프로덕션 lru_cap 50k)**: 창이 50,000에서 하드바운드 · peak RSS **53.5MB**(128MB cage 이내) · 처리량 21.8 MB/s
- **결론**: 프로덕션 설정에서 대용량·고카디널리티 모두 **파싱·전송·메모리 안전 → 프로덕션 config 변경 불필요.** (극단 카디널리티 시 LRU폐기 관측성 노출은 중앙 모니터링 구현 시로 보류)

## 2026-07-16 — 대용량 처리량 특성 측정 · 파서 병렬화 검토(미채택)

대량 유입(100GB급) 처리량을 올릴 수 있는지 조사했다. 파서 병렬화를 프로토타입해
측정한 결과 **종단 병목은 파서가 아니라 Vector의 파일 소스 읽기**였고, 파서 최적화는
종단 개선 효과가 없어 **코드에 반영하지 않았다.** 프로토타입·측정 도구·상세 튜닝 절차는
로컬에만 두고 커밋하지 않는다(프로덕션 파서·config 불변).

| 구분 | 내용 |
|------|------|
| 파서 병렬화 프로토타입 (미채택) | design A = 병렬 `prepare`(strip/normalize/severity/fingerprint) + 단일 `dedup`(병합 정확성 유지). 실측 100GiB realistic **16.5분 / 103 MiB/s / 1.19M lines/s**, 단일 스레드 대비 **2.1x**(최악 고유템플릿은 1.14x). 출력 불변. → 아래 병목 측정으로 **종단 무효** 판정, 미머지 |
| 종단 스테이지 병목 측정 | lines/s: ① Vector 파일읽기 **~240k(병목)** < ③ 파서 588k(단일)/1027k(4워커) ≈ ② JSON 역직렬화 1125k. ④ 전송은 dedup 후라 사실상 무제한. remap/filter는 합쳐 7%뿐. **파서는 애초에 병목이 아니었음** |
| 유효 레버 (Vector측, 코드 아님) | (1) `max_read_bytes` 상향 — 실측 +13% @64k. (2) 파일 소스 병렬화 — Vector가 소스별 동시 읽기, 2소스 1.6x. 둘 다 **배포 config 튜닝 사안**이라 코드 변경 없음(현 생성기는 미적용, 필요 시 적용) |
| 실 로그 병합률 | Ubuntu 반복형 구조적 병합률 **87.7%** → 샤딩 불필요 재확인 |
| 결론 | **프로덕션 코드·config 변경 불필요.** 파서 추가 최적화는 수익 없음. 대용량 시 튜닝은 Vector 읽기 레버로 대응(볼륨형태→하류수용률→CPU사이징→레버 순) |

## 2026-07-16 — SOS 경로 정합 · 중복 제거

수신측 계약(출력·API)은 **그대로**. SOS(`/trigger-sos`)와 push 경로가 각자 복사해 쓰던
한 줄 처리(strip→정규화→severity→fingerprint→dedup)를 하나로 합쳐, 같은 로그가 두 경로에서
**같은 방식으로** 지문을 계산하도록 정합화했다. (단, severity 힌트가 갈리는 경우 — push는
Vector 1차 분류, SOS는 파일 재파싱이라 `"info"`부터 시작 — 는 SOS가 Vector 분류를 가질 수 없는
구조상 예외로 남는다.)

| 구분 | 내용 |
|------|------|
| SOS 지문 정합 (#13) | SOS 경로 `fingerprint` 공식을 coordinator와 일치 — 같은 (template·severity·source)면 push/SOS 동일 지문 |
| 공용 파이프라인 (#14) | `src/process.rs` 신설(`fingerprint()`+`process_line()` 단일 정의점). coordinator·collect 인라인 중복 제거 → 지문 발산 원천 차단, SOS도 `try_merge` 최적화(중복 라인 field 추출 생략) 획득. behavior-preserving (175 passed) |
| 문서 정비 (#12) | docs 파일명 무번호 kebab로 통일 |

## 2026-07-15 — 핫패스 성능·spool 안정성 개선 + 인수인계 문서 정비

수집·파싱 **방법론은 그대로** 두고(이미 적정), 출력(템플릿·지문) 불변인 성능 개선과 운영 안전성만 손봤다. 그리고 대규모 확장 시의 책임 경계·계약을 문서로 확정했다.

| 구분 | 내용 |
|------|------|
| 핫패스 성능 | 죽은 serde flatten 제거, 정규화 12패스 `RegexSet`+`Cow`, severity `aho-corasick` 단일 패스, 중복 라인 lazy extraction, logfmt 가드 — 라인당 CPU·할당 감소(출력 동일) |
| spool 안전 | `retry/` 데드레터 **용량 상한 + TTL**(무제한 디스크 성장 차단), **compress-once**(재시도마다 재직렬화·재gzip 제거) |
| 대규모 계약 | `docs/scale-contract.md` 신설 — 증분 pull(`since_seq`)·이벤트 스토어는 **미채택**, 중앙 플랫폼은 기존 push/스냅샷으로 소비. 책임 경계(저장은 중앙 몫)를 루트 README 상단에 명시 |
| 테스트 | 크로스플랫폼(macOS `/proc/version` 제거)·병렬 플래키 해소 (175 passed) |
| 인수인계 | `docs/README.md` 색인, `reference/stack/`에 수신측 산출물(playbook·goldset) 스냅샷 추가, README 슬림화 |

## 2026-07-01 — Promtail 파이프라인 벤치마킹

Promtail(Grafana Loki의 수집 에이전트)이 *"날것의 로그를 그대로 보내지 않고 파이프라인 스테이지로 가공한 뒤 보낸다"* 는 방식을 벤치마킹해, **수집 단계의 정제 능력**을 끌어올렸다.

| Promtail 개념               | 하는 일                                   | log_parser 구현                                                                     |
| ------------------------- | -------------------------------------- | --------------------------------------------------------------------------------- |
| **multiline**             | 여러 줄 로그(자바 스택트레이스·커널 콜트레이스)를 한 덩어리로 묶음 | Vector `multiline` — 타임스탬프 헤더로 시작하는 줄을 새 이벤트로 보고 후속 줄 병합(`halt_before`)           |
| **drop**                  | 필요 없는 잡음 로그를 버려 저장·전송량 절감              | Vector `drop_noise` 필터(기본: journald debug 제거)                                     |
| **regex / logfmt / json** | 메시지에서 구조화 필드 추출                        | `fields.yaml` — 정규식 캡처 + `logfmt`/`json` 자동 승격(allow 화이트리스트·개수 상한)                |
| **match**                 | 조건에 맞는 로그를 분류·라우팅                      | `categories.yaml` — aho-corasick 리터럴 선별 후 정규식 first-match, `program`/`logger` 게이트 |

**반영한 개선**

- **설정 주도화** — 필드 추출을 소스 하드코딩에서 `fields.yaml` 로딩형으로 전환(코드 변경·재빌드 없이 규칙 추가).
- **logfmt/JSON 자동 파싱** — 앱 로그의 `key=value`·JSON 객체를 필드로 승격(allow 화이트리스트·개수 상한 포함).
- **배포 완결** — `fields.yaml`을 컨테이너 `/etc/log_parser/`로 마운트하도록 `docker-compose.yml` 보강.
- **user 필드 정밀화** — `session opened for user root(uid=0)` 에서 `(uid=0)` 꼬리표를 잘라 `root`로 정규화. 하이픈·점 포함 계정명(`www-data`·`user.name`)은 보존.
