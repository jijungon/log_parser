# 변경 이력

> 최신 항목을 위에 추가한다. (루트 [README](README.md) 요약, 상세 설계는 [docs/](docs/README.md))

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
