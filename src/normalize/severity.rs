use aho_corasick::AhoCorasick;
use once_cell::sync::Lazy;

/// 키워드 기반 severity 보정 — Vector 1차 할당 override
/// 파일 소스(syslog/auth/kern)는 base severity=info 로 들어오므로,
/// 여기 키워드가 위험 이벤트를 승격시키는 유일한 지점이다.
/// 키워드는 categories.yaml 의 실제 커널 문구와 정렬해 소문자 부분일치로 유지한다.
static CRITICAL_KEYWORDS: &[&str] = &[
    "panic:",
    "kernel bug",
    "kernel panic",
    "out of memory: killed",
    "oops:",
    "rcu_sched self-detected stall",
    "nmi: not continuing",
    // 호스트 무결성 훼손 이벤트 (fs.readonly) — 쓰기 불능 = 서비스 정지 수준
    "remounting filesystem read-only",
    // 하드웨어 치명 이벤트 (hw.mce) — MCE 기록은 하드웨어 장애 신호
    "machine check events logged",
];

/// ERROR 승격 키워드 — info/warn → error 로만 승격, 이미 높은 severity 는 강등하지 않는다.
/// 짧고 모호한 단어(오탐 위험)는 배제하고 커널 로그 고유 문구만 사용.
static ERROR_KEYWORDS: &[&str] = &[
    // 디스크 I/O (disk.io_error) — "I/O error" / "Buffer I/O error" 모두 커버
    "i/o error",
    "blk_update_request",
    // 파일시스템 (fs.error) — "xfs (" 는 정상 마운트 로그까지 잡으므로 내부 에러 문구만
    "ext4-fs error",
    "xfs: internal error",
    // 프로세스 크래시 (process.crash) — bare "segfault" 는 앱 로그 오탐 위험 → "segfault at"
    "segfault at",
    "general protection fault",
    // 하드웨어 (hw.mce)
    "hardware error",
    "edac mc",
];

// 단일 SIMD 패스 매처 — 라인마다 to_lowercase() 할당 + 키워드별 재스캔 제거
static CRITICAL_AC: Lazy<AhoCorasick> = Lazy::new(|| {
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(CRITICAL_KEYWORDS)
        .expect("critical keyword matcher")
});

static ERROR_AC: Lazy<AhoCorasick> = Lazy::new(|| {
    AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(ERROR_KEYWORDS)
        .expect("error keyword matcher")
});

/// Vector가 할당한 initial severity + raw message로 최종 severity 결정
pub fn finalize(initial: &str, message: &str) -> &'static str {
    if CRITICAL_AC.is_match(message) {
        return "critical";
    }
    match initial {
        "critical" => "critical",
        "error" => "error",
        other => {
            // info/warn 만 error 로 승격 — 강등은 없다
            if ERROR_AC.is_match(message) {
                return "error";
            }
            match other {
                "warn" => "warn",
                _ => "info",
            }
        }
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

    // ── CRITICAL 승격 (파일 소스 base=info 커버리지 보강) ────────────────────

    #[test]
    fn fs_remount_readonly_is_critical() {
        assert_eq!(
            finalize("info", "EXT4-fs (sda1): Remounting filesystem read-only"),
            "critical"
        );
    }

    #[test]
    fn machine_check_logged_is_critical() {
        // "hardware error"(ERROR 키워드)도 포함하지만 CRITICAL 이 우선한다
        assert_eq!(
            finalize("info", "mce: [Hardware Error]: Machine check events logged"),
            "critical"
        );
    }

    // ── ERROR 승격 ────────────────────────────────────────────────────────────

    #[test]
    fn buffer_io_error_upgrades_info_to_error() {
        assert_eq!(
            finalize("info", "Buffer I/O error on device sdb1, logical block 0"),
            "error"
        );
    }

    #[test]
    fn blk_update_request_is_error() {
        // 메시지 안의 "critical" 단어는 키워드가 아니므로 critical 로 승격되지 않는다
        assert_eq!(
            finalize("info", "blk_update_request: critical target error, dev sdb, sector 0"),
            "error"
        );
    }

    #[test]
    fn ext4_fs_error_is_error() {
        assert_eq!(
            finalize("info", "EXT4-fs error (device sdb1): ext4_find_entry:1455: inode #131076"),
            "error"
        );
    }

    #[test]
    fn xfs_internal_error_is_error() {
        assert_eq!(
            finalize(
                "info",
                "XFS: Internal error xfs_trans_cancel at line 1097 of file fs/xfs/xfs_trans.c"
            ),
            "error"
        );
    }

    #[test]
    fn segfault_is_error() {
        assert_eq!(
            finalize(
                "info",
                "myapp[3421]: segfault at 0 ip 00007f9c8e2a4a80 sp 00007ffd0e6c2a48 error 4 in libc-2.31.so"
            ),
            "error"
        );
    }

    #[test]
    fn general_protection_fault_is_error() {
        assert_eq!(
            finalize("info", "traps: myapp[1234] general protection fault ip:55d0a8 sp:7ffe error:0"),
            "error"
        );
    }

    #[test]
    fn hardware_error_is_error() {
        assert_eq!(
            finalize("info", "[Hardware Error]: Corrected error, no action required."),
            "error"
        );
    }

    #[test]
    fn edac_mc_is_error() {
        assert_eq!(
            finalize("info", "EDAC MC0: 1 CE memory read error on CPU_SrcID#0_MC#0"),
            "error"
        );
    }

    #[test]
    fn error_keyword_upgrades_warn() {
        assert_eq!(finalize("warn", "Buffer I/O error on device dm-0"), "error");
    }

    #[test]
    fn error_keyword_never_downgrades_critical() {
        assert_eq!(finalize("critical", "I/O error, dev sda, sector 1"), "critical");
    }

    #[test]
    fn error_keyword_is_case_insensitive() {
        assert_eq!(finalize("info", "BUFFER I/O ERROR ON DEVICE SDB"), "error");
    }

    // ── 오탐 방지 (near-miss 는 승격하지 않는다) ─────────────────────────────

    #[test]
    fn benign_io_mention_stays_info() {
        // "io error"(슬래시 없음)는 "i/o error" 와 매칭되지 않는다
        assert_eq!(
            finalize("info", "user requested io error simulation disabled"),
            "info"
        );
    }

    #[test]
    fn benign_xfs_mount_stays_info() {
        // 정상 XFS 마운트 로그 — "xfs: internal error" 문구만 승격 대상
        assert_eq!(finalize("info", "XFS (sda1): Mounting V5 Filesystem"), "info");
    }

    #[test]
    fn benign_segfault_mention_stays_info() {
        // "segfault at" 커널 형식이 아닌 언급은 승격하지 않는다
        assert_eq!(
            finalize("info", "segfault handler registered for worker pool"),
            "info"
        );
    }

    #[test]
    fn benign_mount_readonly_stays_info() {
        // "mounting ... read-only"(re- 접두 없음)는 critical 키워드와 매칭되지 않는다
        assert_eq!(
            finalize("info", "mounting filesystem read-only as requested by user"),
            "info"
        );
    }

    #[test]
    fn benign_machine_check_mention_stays_info() {
        // "events logged" 없는 machine check 언급은 승격하지 않는다
        assert_eq!(
            finalize("info", "mce: Machine check polling timer started"),
            "info"
        );
    }

    #[test]
    fn ordinary_info_line_stays_info() {
        assert_eq!(
            finalize("info", "Starting Daily apt download activities..."),
            "info"
        );
    }
}
