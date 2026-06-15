use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use tracing::info;

#[derive(Deserialize)]
struct CategoriesFile {
    categories: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    pattern: String,
    category: String,
    /// 선택: syslog program(태그) 조건. 설정 시 그 program 일 때만 패턴 적용.
    #[serde(default)]
    program: Option<String>,
}

struct Rule {
    re: Regex,
    program: Option<String>,
    category: String,
}

pub struct CategoryMatcher {
    rules: Vec<Rule>,
}

impl CategoryMatcher {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("categories.yaml 읽기 실패: {path}"))?;
        let rules = Self::build_rules(&text)?;
        info!(rules = rules.len(), path, "categories.yaml 로드 완료");
        Ok(Self { rules })
    }

    /// yaml 텍스트 → 규칙 목록. load() 와 회귀 테스트가 공유한다.
    fn build_rules(text: &str) -> Result<Vec<Rule>> {
        let file: CategoriesFile =
            serde_yaml::from_str(text).context("categories.yaml 파싱 실패")?;

        let mut rules = Vec::new();
        for entry in file.categories {
            let pattern = if entry.pattern.is_empty() {
                "(?s).*".to_string() // fallback: match everything
            } else {
                entry.pattern.clone()
            };

            let re = RegexBuilder::new(&pattern)
                .case_insensitive(true)
                .build()
                .with_context(|| format!("카테고리 패턴 컴파일 실패: {}", entry.pattern))?;

            rules.push(Rule {
                re,
                program: entry.program,
                category: entry.category,
            });
        }
        Ok(rules)
    }

    /// categories.yaml 없을 때 fallback — 모든 이벤트를 system.general로 분류
    pub fn fallback() -> Self {
        Self { rules: vec![] }
    }

    /// First-match-wins. `program` 조건이 있는 규칙은 해당 program 일 때만 적용한다.
    /// (헤더 없는 줄은 program=None → program 조건 규칙은 자동으로 건너뜀)
    pub fn categorize<'a>(&'a self, message: &str) -> &'a str {
        let prog = super::tokens::syslog_program(message);
        for rule in &self.rules {
            if let Some(want) = &rule.program {
                match prog {
                    Some(p) if p.eq_ignore_ascii_case(want) => {}
                    _ => continue,
                }
            }
            if rule.re.is_match(message) {
                return rule.category.as_str();
            }
        }
        "system.general"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(rules: &[(&str, &str)]) -> CategoryMatcher {
        let compiled = rules
            .iter()
            .map(|(pat, cat)| Rule {
                re: RegexBuilder::new(pat).case_insensitive(true).build().unwrap(),
                program: None,
                category: cat.to_string(),
            })
            .collect();
        CategoryMatcher { rules: compiled }
    }

    #[test]
    fn first_match_wins() {
        let m = matcher(&[("out of memory", "kernel.oom"), ("memory", "kernel.memory")]);
        assert_eq!(m.categorize("Out of memory: Killed process 123 (java)"), "kernel.oom");
    }

    #[test]
    fn fallback_to_system_general() {
        let m = matcher(&[("out of memory", "kernel.oom")]);
        assert_eq!(m.categorize("normal syslog line"), "system.general");
    }

    #[test]
    fn case_insensitive_match() {
        let m = matcher(&[("EXT4-FS ERROR", "fs.error")]);
        assert_eq!(m.categorize("EXT4-fs error on /dev/sda"), "fs.error");
    }

    #[test]
    fn empty_rules_returns_system_general() {
        let m = CategoryMatcher::fallback();
        assert_eq!(m.categorize("any message"), "system.general");
    }

    #[test]
    fn regex_pattern_matches() {
        let m = matcher(&[("EDAC MC|Hardware Error", "hw.mce")]);
        assert_eq!(m.categorize("EDAC MC0: 1 CE on /MEM/A0/CPU0"), "hw.mce");
        assert_eq!(m.categorize("Hardware Error detected on CPU"), "hw.mce");
        assert_eq!(m.categorize("unrelated log line"), "system.general");
    }

    #[test]
    fn program_condition_gates_match() {
        // program=sshd 조건: sshd 일 때만 auth.event, 다른 program 이면 건너뜀
        let m = CategoryMatcher {
            rules: vec![Rule {
                re: RegexBuilder::new("Accepted publickey").case_insensitive(true).build().unwrap(),
                program: Some("sshd".into()),
                category: "auth.event".into(),
            }],
        };
        assert_eq!(
            m.categorize("Jun 14 23:54:52 host sshd[1]: Accepted publickey for root"),
            "auth.event"
        );
        // 같은 문구라도 program 이 sshd 가 아니면 적용 안 됨
        assert_eq!(
            m.categorize("Jun 14 23:54:52 host myapp[1]: Accepted publickey debug"),
            "system.general"
        );
    }

    /// 실제 config/categories.yaml 로 (실로그 → 기대 카테고리) 회귀 검증.
    /// 규칙을 바꿨을 때 기존 분류가 깨지면 여기서 실패한다.
    #[test]
    fn regression_against_real_categories_yaml() {
        let yaml = include_str!("../../config/categories.yaml");
        let rules = CategoryMatcher::build_rules(yaml).expect("실제 categories.yaml 파싱/컴파일");
        let m = CategoryMatcher { rules };

        let cases: &[(&str, &str)] = &[
            ("Jun 14 23:54:52 host sshd[3211286]: Accepted publickey for root from 10.255.10.33 port 49268 ssh2: RSA SHA256:abc", "auth.event"),
            ("Jun 11 03:34:29 host sshd[3118225]: pam_unix(sshd:session): session opened for user root(uid=0) by (uid=0)", "auth.event"),
            ("Jun 14 23:55:53 host sshd[3211286]: pam_unix(sshd:session): session closed for user root", "auth.event"),
            ("Jun 15 02:00:01 host sshd[999]: Disconnected from user root 10.255.10.33 port 49268", "auth.event"),
            ("Jun 8 07:01:01 host sshd[1]: Failed password for invalid user admin from 1.2.3.4 port 22 ssh2", "auth.bruteforce"),
            ("Jun 14 23:05:01 host CRON[3210742]: pam_unix(cron:session): session opened for user root(uid=0) by (uid=0)", "session.activity"),
            ("Jun 14 23:54:52 host systemd-logind[657]: New session 8883 of user root.", "session.activity"),
            ("Jun 15 01:41:12 host systemd[1]: session-8888.scope: Deactivated successfully.", "session.activity"),
            ("Jun 11 03:25:41 host systemd-logind[657]: Session 8103 logged out. Waiting for processes to exit.", "session.activity"),
            ("May  8 10:13:42 db-prod-03 kernel: Out of memory: Killed process 2481 (java)", "kernel.oom"),
            ("May  8 08:41:05 db-prod-03 kernel: blk_update_request: I/O error, dev sdb, sector 4096000", "disk.io_error"),
            ("May  8 08:41:10 db-prod-03 kernel: EXT4-fs error (device sdb1): ext4_find_entry:1455", "fs.error"),
            ("May  8 10:14:03 db-prod-03 systemd[1]: app-worker.service: Start request repeated too quickly", "systemd.restart_loop"),
            ("Jun 15 00:00:07 host systemd[1]: Starting Daily apt download activities...", "system.general"),
            // program 앵커: sshd 가 아닌 program 의 'Accepted publickey' 문구는 auth.event 로 안 감
            ("Jun 15 00:00:00 host myapp[1]: Accepted publickey debug message", "system.general"),
        ];

        for (line, expected) in cases {
            assert_eq!(m.categorize(line), *expected, "분류 회귀 실패: {line}");
        }
    }
}
