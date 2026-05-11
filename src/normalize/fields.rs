use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;

struct Extractor {
    re: Regex,
    key: &'static str,
    numeric: bool,
}

static EXTRACTORS: Lazy<Vec<Extractor>> = Lazy::new(|| {
    vec![
        // OOM killer: "Killed process 2481 (java)"
        Extractor {
            re: Regex::new(r"[Kk]illed process (\d+)").unwrap(),
            key: "pid",
            numeric: true,
        },
        // SSH / PAM: "for user root" / "for invalid user admin"
        Extractor {
            re: Regex::new(r"for (?:invalid )?user (\S+)").unwrap(),
            key: "user",
            numeric: false,
        },
        // block device: "dev sdb,"
        Extractor {
            re: Regex::new(r"\bdev (\S+?)(?:[,\s]|$)").unwrap(),
            key: "dev",
            numeric: false,
        },
        // systemd unit: "app-worker.service: ..."
        Extractor {
            re: Regex::new(r"^(\S+\.(?:service|socket|mount|target|timer))\b").unwrap(),
            key: "unit",
            numeric: false,
        },
        // key=value style: pid=123, user=foo
        Extractor {
            re: Regex::new(r"\bpid=(\d+)").unwrap(),
            key: "pid",
            numeric: true,
        },
        Extractor {
            re: Regex::new(r"\buser=(\S+)").unwrap(),
            key: "user",
            numeric: false,
        },
    ]
});

/// 메시지 본문(syslog prefix 제거 후)에서 구조화 필드 추출
pub fn extract_fields(msg: &str) -> HashMap<String, Value> {
    let mut fields = HashMap::new();
    for ext in EXTRACTORS.iter() {
        if fields.contains_key(ext.key) {
            continue;
        }
        if let Some(cap) = ext.re.captures(msg) {
            if let Some(m) = cap.get(1) {
                let val = m.as_str();
                let json_val = if ext.numeric {
                    val.parse::<i64>().ok().map(|n| Value::Number(n.into()))
                } else {
                    Some(Value::String(val.to_string()))
                };
                if let Some(v) = json_val {
                    fields.insert(ext.key.to_string(), v);
                }
            }
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_oom_pid() {
        let f = extract_fields("Killed process 2481 (java) total-vm:10240kB");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(2481)));
    }

    #[test]
    fn extracts_invalid_user() {
        let f = extract_fields("Failed password for invalid user admin from 192.168.1.1");
        assert_eq!(f.get("user"), Some(&serde_json::json!("admin")));
    }

    #[test]
    fn extracts_kv_user() {
        let f = extract_fields("session opened for user root by (uid=0)");
        assert_eq!(f.get("user"), Some(&serde_json::json!("root")));
    }

    #[test]
    fn extracts_kv_pid() {
        let f = extract_fields("process exited pid=5678 status=0");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(5678)));
    }

    #[test]
    fn first_match_wins_for_pid() {
        // OOM extractor is listed before kv extractor → OOM pid wins
        let f = extract_fields("Killed process 111 (app) pid=222");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(111)));
    }

    #[test]
    fn no_match_returns_empty() {
        let f = extract_fields("just a plain log message");
        assert!(f.is_empty());
    }
}
