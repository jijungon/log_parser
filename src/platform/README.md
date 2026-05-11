# platform

호스트 환경 감지 및 런타임 제약 설정.

| 파일 | 역할 |
|------|------|
| `discovery.rs` | `host_id`(`/etc/machine-id`), `boot_id`(`/proc/sys/kernel/random/boot_id`), 로그 파일 경로 자동 탐지 |
| `capability.rs` | Vector 바이너리, journald(journalctl 우선 → 디렉토리 폴백), syslog/auth/audit 파일 존재 여부를 `Probes` 구조체로 반환. `has_log_sources()`로 로그 소스 유무를 판단해 시작 게이트 역할 수행 |
| `cgroup.rs` | cgroup v2 메모리·CPU 제한 적용 |
