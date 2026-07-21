//! log_parser 라이브러리 크레이트.
//!
//! 프로덕션 바이너리(`main.rs`)와 부하 도구(`bin/loadtest.rs`)가 **같은 코드**를
//! 공유하도록 모듈을 여기서 노출한다. 동작은 기존 바이너리와 동일 — 단지 모듈
//! 선언 위치가 `main.rs` → `lib.rs`로 옮겨졌을 뿐이다.

pub mod config;
pub mod coordinator;
pub mod dedup;
pub mod envelope;
pub mod inbound;
pub mod normalize;
pub mod pipeline;
pub mod platform;
pub mod process;
pub mod transport;

/// 규칙 파일 기본 경로. `inbound/collect.rs`가 `crate::DEFAULT_CATEGORIES`로 참조하므로
/// 크레이트 루트(lib)에 둔다.
pub const DEFAULT_CATEGORIES: &str = "/etc/log_parser/categories.yaml";
pub const DEFAULT_FIELDS: &str = "/etc/log_parser/fields.yaml";
