# 수신측 계약 — 외부 인터페이스 명세

> 본 프로젝트 외부에 있는 **수신 서비스와의 약속**.
> 이 프로젝트는 receiver 자체를 만들지 않지만, envelope을 받는 쪽이 무엇을 보장해야 envelope 모델이 의미 있는지를 이 문서에 명시.
> receiver 구현은 본 프로젝트 외부 (운영 팀 또는 별개 프로젝트가 책임).

---

## 1. 송출 contract (log_parser → receiver)

> **kind="http_json" 기준** (Phase B default). kind="otlp"는 gRPC+protobuf — `2_MASTER_PLAN.md §7.7.3` 참조.

```
POST <ingest endpoint, agent.yaml의 transport.endpoint>
Authorization: Bearer ${INGEST_TOKEN}
Content-Type: application/json
Content-Encoding: gzip
Body: <Envelope JSON, 2_MASTER_PLAN.md §10 schema>

Response 2xx (200, 202, 204): 수신 성공. 송신 buffer 비움
Response 5xx, 429, 네트워크 에러: 재시도 가능 (간격을 점점 늘려가며)
Response 4xx (401, 403): 치명 오류. 토큰·인증 문제. 알림 로그
```

---

## 2. 수신측이 envelope으로 해야 하는 것 (권장)

| 책임 | 권장 동작 |
|---|---|
| 인덱싱 | `(host_id, boot_id, window)` 복합 키로 인덱싱 |
| 중복 방지 | `(host_id, boot_id, seq)` 3개 값이 모두 같으면 같은 데이터로 간주해 한 번만 처리 |
| body 분석 | severity·category·template·fingerprint 기반 패턴 매칭. 시간순 연결 |
| Alerting | `1_observability-design.md §9` 권장 룰 셋 (panic 키워드, hw 에러, fs.readonly, 재시작 루프, 에러율 폭증, 부팅 직후, 호스트 침묵) |
| 상세 수집 | "사고다" 판단 시 `GET host:9102/stat` + `POST host:9100/flush` + `POST host:9101/trigger-sos` 병렬 호출 |
| 호스트 침묵 감지 | log_parser envelope 30분 + grace 5분 안에 안 오면 호스트 이상 (host_id 기준) |
| Cool-down | 같은 호스트·같은 사고로 sos 중복 호출 방지 |
| 결과 묶기 | `host_id` 기준으로 log_batch / stat_snapshot / sos_snapshot 세 envelope을 연결 |

---

## 3. 수신측 프로젝트 자체

| 항목 | 결정 |
|---|---|
| 누가 만드나 | **본 프로젝트 외부**. 운영 팀 자체 구축 또는 별개 프로젝트 |
| 권장 구현 후보 | (a) Loki + Alertmanager + custom adapter, (b) ClickHouse + Grafana, (c) 자체 서비스 (Rust/Go) |
| Alerting 룰 owner | 환경별 임계값은 운영 팀 |
| Phase B 검증 | mock receiver로 대체. 실제 receiver 통합은 별도 마일스톤 |

---

## 4. Alerting 룰 예시 (sos 트리거 조건)

수신측 alerting 시스템(Alertmanager 또는 custom)에서 정의. log_parser는 판단하지 않음.

| 트리거 | 매칭 조건 |
|---|---|
| 패닉/OOM | `severity=critical AND template contains "Out of memory: Killed\|kernel BUG\|panic:"` |
| 하드웨어 에러 | `category=hw.mce` |
| 파일시스템 강등 | `template contains "remounting filesystem read-only"` |
| 재시작 루프 | `unit별 restarts >= 3 / cycle` (수신측이 body group_by로 계산) |
| 에러율 폭증 | `headers.counts.by_severity.error >= N` (baseline 대비 ×10 등) |
| 부팅 직후 | boot ID 변경 + 첫 envelope (의도 안 한 재부팅 가능성) |
| 호스트 침묵 후 복귀 | envelope 끊김 후 복귀 (35분 이상 미수신) |

---

## 5. Envelope schema 진화 정책

| 변경 종류 | schema_version 변경 | 호환성 |
|---|---|---|
| 키 추가 (additive) | 1.x → 1.(x+1) | 이전 버전 수신측과 호환. 기존 수신측은 새 키를 무시함 |
| 키 의미 변경·키 제거 | 1.x → 2.0 | 호환 깨짐. 수신측 업데이트 필요 |
| 값 타입 변경 | 1.x → 2.0 | 호환 깨짐. 수신측 업데이트 필요 |

수신측은 모르는 키를 만나면 **무시하고 계속 처리**해야 함.
알 수 없는 schema_version 메이저 버전을 만나면 4xx 응답 가능.
