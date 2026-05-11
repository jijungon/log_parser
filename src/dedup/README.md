# dedup

슬라이딩 윈도우 기반 중복 제거 모듈.

같은 `fingerprint`(template · severity · source를 `|`로 이어 스트리밍 Xxh3 해시)를 가진 이벤트를 윈도우 내에서 하나로 묶어 `count`를 누적합니다. 윈도우 크기는 `config.yaml`의 `dedup.window_seconds`로 설정합니다 (기본값 30초).

| 파일 | 역할 |
|------|------|
| `window.rs` | DedupWindow 구조체 — 삽입·만료·drain 구현 |
