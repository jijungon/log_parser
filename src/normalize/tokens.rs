use once_cell::sync::Lazy;
use regex::Regex;

// RFC 3164 syslog 헤더: "May  8 04:41:04 hostname proc[PID]: " 또는 "... proc: "
static SYSLOG_PREFIX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[A-Za-z]{3}\s+\d{1,2}\s+\d{2}:\d{2}:\d{2}\s+\S+\s+[^\s:]+(?:\[\d+\])?:\s*")
        .unwrap()
});

/// syslog 헤더(타임스탬프+호스트+프로세스)를 제거하고 메시지 본문만 반환
pub fn strip_syslog_prefix(msg: &str) -> &str {
    SYSLOG_PREFIX.find(msg).map_or(msg, |m| &msg[m.end()..])
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

/// 가변 토큰을 placeholder로 치환 → 정규화된 template 반환
pub fn normalize(msg: &str) -> String {
    let mut s = msg.to_string();
    for (re, placeholder) in PATTERNS.iter() {
        s = re.replace_all(&s, *placeholder).into_owned();
    }
    s
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
    fn replaces_ipv4() {
        assert_eq!(normalize("connect from 192.168.1.100 failed"), "connect from <IP4> failed");
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
