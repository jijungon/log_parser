use aho_corasick::AhoCorasick;
use once_cell::sync::Lazy;

/// 키워드 기반 severity 보정 — Vector 1차 할당 override
static CRITICAL_KEYWORDS: &[&str] = &[
    "panic:",
    "kernel bug",
    "kernel panic",
    "out of memory: killed",
    "oops:",
    "rcu_sched self-detected stall",
    "nmi: not continuing",
];

// 단일 SIMD 패스 매처 — 라인마다 to_lowercase() 할당 + 키워드별 재스캔 제거
static CRITICAL_AC: Lazy<AhoCorasick> = Lazy::new(|| {
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(CRITICAL_KEYWORDS)
        .expect("critical keyword matcher")
});

/// Vector가 할당한 initial severity + raw message로 최종 severity 결정
pub fn finalize(initial: &str, message: &str) -> &'static str {
    if CRITICAL_AC.is_match(message) {
        return "critical";
    }
    match initial {
        "critical" => "critical",
        "error" => "error",
        "warn" => "warn",
        _ => "info",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_panic_is_critical() {
        assert_eq!(finalize("info", "kernel panic: not syncing"), "critical");
    }

    #[test]
    fn oom_killed_is_critical() {
        assert_eq!(finalize("error", "out of memory: killed process 1234"), "critical");
    }

    #[test]
    fn keyword_match_is_case_insensitive() {
        assert_eq!(finalize("info", "KERNEL PANIC detected"), "critical");
    }

    #[test]
    fn initial_error_passthrough() {
        assert_eq!(finalize("error", "some error occurred"), "error");
    }

    #[test]
    fn initial_warn_passthrough() {
        assert_eq!(finalize("warn", "low disk space"), "warn");
    }

    #[test]
    fn initial_critical_passthrough() {
        assert_eq!(finalize("critical", "auth failure"), "critical");
    }

    #[test]
    fn unrecognized_initial_falls_back_to_info() {
        assert_eq!(finalize("debug", "verbose message"), "info");
    }
}
