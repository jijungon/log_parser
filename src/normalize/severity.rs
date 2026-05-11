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

/// Vector가 할당한 initial severity + raw message로 최종 severity 결정
pub fn finalize(initial: &str, message: &str) -> &'static str {
    let lower = message.to_lowercase();
    for kw in CRITICAL_KEYWORDS {
        if lower.contains(kw) {
            return "critical";
        }
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
