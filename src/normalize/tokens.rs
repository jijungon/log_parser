use once_cell::sync::Lazy;
use regex::Regex;

// RFC 3164 syslog 헤더: "May  8 04:41:04 hostname proc[PID]: " 또는 "... proc: "
static SYSLOG_PREFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[A-Za-z]{3}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+\S+\s+[^\s:]+(?:\[\d+\])?:\s*")
        .unwrap()
});

// RFC 5424 / ISO 8601 타임스탬프 헤더 (rsyslog RSYSLOG_FileFormat):
//   "2026-06-15T00:25:23.123456+00:00 hostname proc[PID]: "
//   선택적 PRI/version 접두(예: "<34>1 ")도 허용. tz(Z 또는 ±hh:mm)·소수초는 선택.
static SYSLOG_PREFIX_ISO: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:<\d{1,3}>\d?\s+)?\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?\s+\S+\s+[^\s:]+(?:\[\d+\])?:\s*",
    )
    .unwrap()
});

/// syslog 헤더(타임스탬프+호스트+프로세스)를 제거하고 메시지 본문만 반환.
/// RFC 3164(전통적 syslog)와 RFC 5424/ISO 타임스탬프 형식을 모두 처리한다.
pub fn strip_syslog_prefix(msg: &str) -> &str {
    if let Some(m) = SYSLOG_PREFIX.find(msg) {
        return &msg[m.end()..];
    }
    if let Some(m) = SYSLOG_PREFIX_ISO.find(msg) {
        return &msg[m.end()..];
    }
    msg
}

// syslog 헤더에서 program(태그)만 캡처: "<시각> <host> <program>[<pid>]: " 의 program.
// RFC 3164·ISO 둘 다. 헤더가 없으면 매칭 안 됨(None).
static SYSLOG_PROGRAM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"^(?:[A-Za-z]{3}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}|(?:<\d{1,3}>\d?\s+)?\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})?)\s+\S+\s+([A-Za-z0-9_.\-]+?)(?:\[\d+\])?:\s",
    )
    .unwrap()
});

/// syslog 헤더의 program(태그)을 추출한다 (sshd, CRON, systemd-logind, kernel …).
/// 헤더가 없으면 None. 카테고리 분류에서 "출처 프로그램" 조건으로 쓰인다.
pub fn syslog_program(line: &str) -> Option<&str> {
    SYSLOG_PROGRAM
        .captures(line)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// 순서 중요: 더 구체적인 패턴이 먼저 와야 함
static PATTERNS: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        // UUID (hex-hyphen-hex)
        (
            Regex::new(
                r"(?i)[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
            )
            .unwrap(),
            "<UUID>",
        ),
        // IPv4
        (
            Regex::new(r"\b(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\b").unwrap(),
            "<IP4>",
        ),
        // IPv6 — MAC/HEX/NUM보다 먼저.
        // 세 형태: ① full 8그룹(7콜론) ② LEFT그룹들 + "::" + 선택 RIGHT ③ 선두 "::RIGHT"
        // "::" 압축 또는 8그룹을 요구하므로 6그룹 MAC·HH:MM:SS 시각과 충돌하지 않음.
        // leftmost-first(Perl/Rust) 의미에서 압축형이 부분매칭되지 않도록 RIGHT를 greedy로 흡수.
        (
            Regex::new(
                r"(?i)(?:\b(?:[0-9a-f]{1,4}:){7}[0-9a-f]{1,4}\b|\b(?:[0-9a-f]{1,4}:)+:(?:[0-9a-f]{1,4}(?::[0-9a-f]{1,4})*)?|::[0-9a-f]{1,4}(?::[0-9a-f]{1,4})*)",
            )
            .unwrap(),
            "<IP6>",
        ),
        // ── 컨테이너/네트워크 런타임 ID (Docker 호스트 노이즈 → fingerprint 분산 주범) ──
        // 컨테이너마다 달라지는 랜덤 ID를 placeholder로 묶어 같은 사건이 한 template이 되게 한다.
        // PATH/HEX/NUM 보다 먼저 와야 함 (랜덤 ID가 NUM/HEX로 쪼개지기 전에 통째로 치환).
        // docker-<hex>.scope 의 컨테이너 ID
        (
            Regex::new(r"(?i)\bdocker-[0-9a-f]{12,64}\b").unwrap(),
            "<CID>",
        ),
        // veth 가상 인터페이스 (veth7dbd160 …)
        (
            Regex::new(r"(?i)\bveth[0-9a-f]{6,}\b").unwrap(),
            "<VETH>",
        ),
        // 도커 브리지 (br-754c2e451615)
        (
            Regex::new(r"(?i)\bbr-[0-9a-f]{12}\b").unwrap(),
            "<BR>",
        ),
        // overlay2 / buildkit / runc / netns 마운트의 랜덤 ID
        (
            Regex::new(r"(?i)\b(?:overlay2|buildkit-executor|buildkit|runc|netns)[-/][0-9a-z]{8,}").unwrap(),
            "<MNT>",
        ),
        // MAC address  aa:bb:cc:dd:ee:ff
        (
            Regex::new(r"(?i)\b[0-9a-f]{2}(?::[0-9a-f]{2}){5}\b").unwrap(),
            "<HEX>",
        ),
        // Filesystem path (2+ segments)
        (
            Regex::new(r"(?:/[a-zA-Z0-9_@.+-]+){2,}").unwrap(),
            "<PATH>",
        ),
        // Block device names: sda1, nvme0n1, vdb, loop0
        (
            Regex::new(r"\b(?:sd[a-z]\d*|nvme\d+n\d+(?:p\d+)?|vd[a-z]\d*|hd[a-z]\d*|loop\d+)\b")
                .unwrap(),
            "<DEV>",
        ),
        // Long hex strings (8+ hex chars that aren't already replaced)
        (
            Regex::new(r"(?i)\b0x[0-9a-f]{4,}\b").unwrap(),
            "<HEX>",
        ),
        // Generic numbers (last — most general)
        (Regex::new(r"\b\d+\b").unwrap(), "<NUM>"),
    ]
});

// PATTERNS 전체를 한 번의 멀티패턴 스캔으로 검사하는 프리필터.
// placeholder 문자열(<UUID> 등)은 숫자·콜론·슬래시·0x를 포함하지 않으므로
// 앞선 치환이 뒤 패턴의 "새로운" 매치를 만들어낼 수 없다 → 원본 기준 판정이 안전하다.
static PATTERN_SET: Lazy<regex::RegexSet> = Lazy::new(|| {
    regex::RegexSet::new(PATTERNS.iter().map(|(re, _)| re.as_str())).unwrap()
});

/// 가변 토큰을 placeholder로 치환 → 정규화된 template 반환
pub fn normalize(msg: &str) -> String {
    use std::borrow::Cow;
    let matched = PATTERN_SET.matches(msg);
    if !matched.matched_any() {
        return msg.to_string();
    }
    let mut s: Cow<'_, str> = Cow::Borrowed(msg);
    for (idx, (re, placeholder)) in PATTERNS.iter().enumerate() {
        if !matched.matched(idx) {
            continue;
        }
        if let Cow::Owned(new) = re.replace_all(&s, *placeholder) {
            s = Cow::Owned(new);
        }
    }
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_syslog_full_prefix() {
        let line = "May  8 04:41:04 myhost sshd[1234]: Connection from 10.0.0.1";
        assert_eq!(strip_syslog_prefix(line), "Connection from 10.0.0.1");
    }

    #[test]
    fn strip_syslog_no_pid() {
        let line = "Jan  1 00:00:00 host kernel: Oops: divide by zero";
        assert_eq!(strip_syslog_prefix(line), "Oops: divide by zero");
    }

    #[test]
    fn strip_syslog_no_header_passthrough() {
        let line = "plain log line without syslog header";
        assert_eq!(strip_syslog_prefix(line), line);
    }

    #[test]
    fn strip_syslog_bracketed_pid() {
        let line = "Dec 31 23:59:59 router dhclient[42]: DHCPACK from 192.168.1.1";
        assert_eq!(strip_syslog_prefix(line), "DHCPACK from 192.168.1.1");
    }

    #[test]
    fn strip_syslog_iso_full_prefix() {
        let line = "2026-06-15T00:25:23.123456+00:00 myhost sshd[1234]: Connection from fe80::1";
        assert_eq!(strip_syslog_prefix(line), "Connection from fe80::1");
    }

    #[test]
    fn strip_syslog_iso_no_pid() {
        let line = "2026-06-15T00:25:23+00:00 host kernel: Oops";
        assert_eq!(strip_syslog_prefix(line), "Oops");
    }

    #[test]
    fn strip_syslog_iso_zulu() {
        let line = "2026-06-15T00:25:23Z host cron[9]: job done";
        assert_eq!(strip_syslog_prefix(line), "job done");
    }

    #[test]
    fn strip_syslog_iso_with_pri_version() {
        let line = "<34>1 2026-06-15T00:25:23.5Z host su[42]: failed";
        assert_eq!(strip_syslog_prefix(line), "failed");
    }

    #[test]
    fn replaces_ipv4() {
        assert_eq!(normalize("connect from 192.168.1.100 failed"), "connect from <IP4> failed");
    }

    #[test]
    fn replaces_ipv6_full() {
        assert_eq!(
            normalize("peer 2001:0db8:0000:0000:0000:ff00:0042:8329 up"),
            "peer <IP6> up"
        );
    }

    #[test]
    fn replaces_ipv6_compressed() {
        assert_eq!(normalize("link to fe80::1 ready"), "link to <IP6> ready");
        assert_eq!(normalize("addr 2001:db8::ff00:42:8329 seen"), "addr <IP6> seen");
    }

    #[test]
    fn replaces_ipv6_loopback() {
        // 선두 "::" 형태 (loopback ::1)
        assert_eq!(normalize("from ::1 port 22"), "from <IP6> port <NUM>");
    }

    #[test]
    fn replaces_container_runtime_ids() {
        assert_eq!(normalize("device veth7dbd160 left promiscuous mode"),
                   "device <VETH> left promiscuous mode");
        assert_eq!(normalize("br-754c2e451615: port 1 up"),
                   "<BR>: port <NUM> up");
        let cid = normalize("docker-fae7e8b692d583c986b470e9560ac81fc72b85a96e7d7c369f36b7c91f369394.scope: Deactivated");
        assert!(cid.starts_with("<CID>.scope"), "got: {cid}");
        assert_eq!(normalize("var-lib-docker-overlay2-hlbif58yuggf2zvwx7oweycqo-merged.mount up"),
                   "var-lib-docker-<MNT>-merged.mount up");
    }

    #[test]
    fn container_tokens_do_not_touch_normal_logs() {
        // 정상 로그는 그대로 (process 번호만 NUM)
        assert_eq!(normalize("Out of memory: Killed process 2481 (java)"),
                   "Out of memory: Killed process <NUM> (java)");
    }

    #[test]
    fn extracts_syslog_program() {
        assert_eq!(syslog_program("Jun 11 03:34:29 host sshd[3118225]: pam_unix(sshd:session): x"), Some("sshd"));
        assert_eq!(syslog_program("Jun 14 23:05:01 host CRON[3210742]: pam_unix(cron:session): x"), Some("CRON"));
        assert_eq!(syslog_program("Jun 11 03:34:29 host systemd-logind[657]: New session 8125 of user root."), Some("systemd-logind"));
        assert_eq!(syslog_program("May  8 10:13:42 host kernel: Out of memory"), Some("kernel"));
        assert_eq!(syslog_program("2026-06-15T00:25:23.1+00:00 host sshd[1]: Accepted publickey"), Some("sshd"));
        assert_eq!(syslog_program("plain line without header"), None);
    }

    #[test]
    fn replaces_ipv6_link_local() {
        assert_eq!(normalize("fe80::a00:27ff:fe4e:66a1 nbr"), "<IP6> nbr");
    }

    #[test]
    fn ipv6_does_not_eat_mac() {
        // MAC(6그룹·:: 없음)은 IPv6로 잡히면 안 되고 <HEX>로 가야 함
        assert_eq!(normalize("device aa:bb:cc:dd:ee:ff connected"), "device <HEX> connected");
    }

    #[test]
    fn ipv6_not_split_into_num() {
        let result = normalize("client fe80::1 disconnected");
        assert!(result.contains("<IP6>"), "got: {result}");
        assert!(!result.contains("<NUM>"), "IPv6 should not become NUM: {result}");
    }

    #[test]
    fn replaces_uuid() {
        assert_eq!(
            normalize("session 550e8400-e29b-41d4-a716-446655440000 started"),
            "session <UUID> started"
        );
    }

    #[test]
    fn replaces_mac_address() {
        assert_eq!(normalize("device aa:bb:cc:dd:ee:ff connected"), "device <HEX> connected");
    }

    #[test]
    fn replaces_path() {
        assert_eq!(normalize("opened /var/log/messages"), "opened <PATH>");
    }

    #[test]
    fn replaces_numbers() {
        assert_eq!(normalize("exited with code 1"), "exited with code <NUM>");
    }

    #[test]
    fn combined_ipv4_and_num() {
        let result = normalize("process 1234 connected from 10.0.0.1");
        assert!(result.contains("<NUM>"));
        assert!(result.contains("<IP4>"));
    }

    #[test]
    fn uuid_not_split_into_num() {
        let result = normalize("id=550e8400-e29b-41d4-a716-446655440000");
        assert!(result.contains("<UUID>"), "got: {result}");
        assert!(!result.contains("<NUM>"), "UUID should not become NUM: {result}");
    }
}
