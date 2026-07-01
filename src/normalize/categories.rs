use aho_corasick::{AhoCorasick, MatchKind};
use anyhow::{Context, Result};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
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
    /// 선택: 추출된 `logger` 필드 조건(정규식). 설정 시 그 logger 일 때만 패턴 적용.
    /// 앱 로그 분류(예: c.parametacorp...Scrap → sa-scrap)에 사용. (Promtail match 단계 대응)
    #[serde(default)]
    logger: Option<String>,
}

/// 패턴 매칭 방식. 메타문자 없는 순수 리터럴은 aho-corasick 로 한 번에 스캔하고,
/// 진짜 정규식만 개별 regex 로 평가한다.
enum Matcher {
    /// aho-corasick 패턴 인덱스
    Literal(usize),
    Regex(Regex),
}

struct Rule {
    matcher: Matcher,
    program: Option<String>,
    logger: Option<Regex>,
    category: String,
}

pub struct CategoryMatcher {
    rules: Vec<Rule>,
    /// 리터럴 패턴 전용 멀티패턴 오토마톤 (인덱스 = Matcher::Literal(idx))
    ac: Option<AhoCorasick>,
    literal_count: usize,
}

/// 정규식 메타문자가 하나도 없으면 순수 리터럴로 간주 (case-insensitive 부분일치와 동일).
fn is_literal(pattern: &str) -> bool {
    !pattern.is_empty() && !pattern.bytes().any(|b| {
        matches!(
            b,
            b'\\' | b'.' | b'^' | b'$' | b'*' | b'+' | b'?'
                | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'|'
        )
    })
}

impl CategoryMatcher {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("categories.yaml 읽기 실패: {path}"))?;
        let me = Self::build(&text)?;
        info!(
            rules = me.rules.len(),
            literals = me.literal_count,
            path,
            "categories.yaml 로드 완료"
        );
        Ok(me)
    }

    /// yaml 텍스트 → CategoryMatcher. load() 와 회귀 테스트가 공유한다.
    fn build(text: &str) -> Result<Self> {
        let file: CategoriesFile =
            serde_yaml::from_str(text).context("categories.yaml 파싱 실패")?;

        let mut rules = Vec::with_capacity(file.categories.len());
        let mut literals: Vec<String> = Vec::new();

        for entry in file.categories {
            let logger = match &entry.logger {
                Some(p) => Some(
                    RegexBuilder::new(p)
                        .case_insensitive(true)
                        .build()
                        .with_context(|| format!("logger 패턴 컴파일 실패: {p}"))?,
                ),
                None => None,
            };

            let matcher = if is_literal(&entry.pattern) {
                let idx = literals.len();
                literals.push(entry.pattern.clone());
                Matcher::Literal(idx)
            } else {
                let pattern = if entry.pattern.is_empty() {
                    "(?s).*".to_string() // fallback: match everything
                } else {
                    entry.pattern.clone()
                };
                let re = RegexBuilder::new(&pattern)
                    .case_insensitive(true)
                    .build()
                    .with_context(|| format!("카테고리 패턴 컴파일 실패: {}", entry.pattern))?;
                Matcher::Regex(re)
            };

            rules.push(Rule {
                matcher,
                program: entry.program,
                logger,
                category: entry.category,
            });
        }

        let literal_count = literals.len();
        let ac = if literals.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .ascii_case_insensitive(true)
                    .match_kind(MatchKind::Standard)
                    .build(&literals)
                    .context("aho-corasick 빌드 실패")?,
            )
        };

        Ok(Self {
            rules,
            ac,
            literal_count,
        })
    }

    /// categories.yaml 없을 때 fallback — 모든 이벤트를 system.general로 분류
    pub fn fallback() -> Self {
        Self {
            rules: vec![],
            ac: None,
            literal_count: 0,
        }
    }

    /// 필드 없이 분류 (하위호환 래퍼, 테스트·외부용).
    #[allow(dead_code)]
    pub fn categorize<'a>(&'a self, message: &str) -> &'a str {
        static EMPTY: once_cell::sync::Lazy<HashMap<String, Value>> =
            once_cell::sync::Lazy::new(HashMap::new);
        self.categorize_with_fields(message, &EMPTY)
    }

    /// First-match-wins. `program` 조건이 있는 규칙은 해당 program 일 때만,
    /// `logger` 조건이 있는 규칙은 fields["logger"] 가 매칭될 때만 적용한다.
    pub fn categorize_with_fields<'a>(
        &'a self,
        message: &str,
        fields: &HashMap<String, Value>,
    ) -> &'a str {
        // 리터럴 패턴들의 존재 여부를 단일 스캔으로 계산 (aho-corasick).
        let present: Option<Vec<bool>> = self.ac.as_ref().map(|ac| {
            let mut v = vec![false; self.literal_count];
            for m in ac.find_overlapping_iter(message) {
                v[m.pattern().as_usize()] = true;
            }
            v
        });

        let prog = super::tokens::syslog_program(message);

        for rule in &self.rules {
            if let Some(want) = &rule.program {
                match prog {
                    Some(p) if p.eq_ignore_ascii_case(want) => {}
                    _ => continue,
                }
            }
            if let Some(re) = &rule.logger {
                match fields.get("logger").and_then(|v| v.as_str()) {
                    Some(l) if re.is_match(l) => {}
                    _ => continue,
                }
            }
            let hit = match &rule.matcher {
                Matcher::Literal(id) => present.as_ref().map(|p| p[*id]).unwrap_or(false),
                Matcher::Regex(re) => re.is_match(message),
            };
            if hit {
                return rule.category.as_str();
            }
        }
        "system.general"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 정규식 규칙만으로 매처 구성 (ac 미사용 경로 테스트).
    fn matcher(rules: &[(&str, &str)]) -> CategoryMatcher {
        let compiled = rules
            .iter()
            .map(|(pat, cat)| Rule {
                matcher: Matcher::Regex(
                    RegexBuilder::new(pat).case_insensitive(true).build().unwrap(),
                ),
                program: None,
                logger: None,
                category: cat.to_string(),
            })
            .collect();
        CategoryMatcher {
            rules: compiled,
            ac: None,
            literal_count: 0,
        }
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
        let m = CategoryMatcher {
            rules: vec![Rule {
                matcher: Matcher::Regex(
                    RegexBuilder::new("Accepted publickey").case_insensitive(true).build().unwrap(),
                ),
                program: Some("sshd".into()),
                logger: None,
                category: "auth.event".into(),
            }],
            ac: None,
            literal_count: 0,
        };
        assert_eq!(
            m.categorize("Jun 14 23:54:52 host sshd[1]: Accepted publickey for root"),
            "auth.event"
        );
        assert_eq!(
            m.categorize("Jun 14 23:54:52 host myapp[1]: Accepted publickey debug"),
            "system.general"
        );
    }

    #[test]
    fn logger_condition_gates_match() {
        // logger 필드가 지정 정규식과 맞을 때만 분류
        let m = CategoryMatcher::build(
            r#"
categories:
  - pattern: "Scrap"
    category: sa-scrap
    logger: 'parametacorp.*Scrap'
  - pattern: ""
    category: system.general
"#,
        )
        .unwrap();

        let mut with_logger = HashMap::new();
        with_logger.insert(
            "logger".to_string(),
            Value::String("c.parametacorp.cexks.data.model.Scrap".into()),
        );
        assert_eq!(m.categorize_with_fields("Scrap done site=x", &with_logger), "sa-scrap");

        // logger 없거나 불일치 → 분류 안 됨(fallback)
        assert_eq!(m.categorize("Scrap done site=x"), "system.general");
        let mut other = HashMap::new();
        other.insert("logger".to_string(), Value::String("com.other.Thing".into()));
        assert_eq!(m.categorize_with_fields("Scrap done", &other), "system.general");
    }

    #[test]
    fn literal_and_regex_mix_via_aho_corasick() {
        // 리터럴("Out of memory")은 aho-corasick, 정규식("EDAC MC|Hardware Error")은 regex 경로
        let m = CategoryMatcher::build(
            r#"
categories:
  - pattern: "Out of memory: Killed"
    category: kernel.oom
  - pattern: "EDAC MC|Hardware Error"
    category: hw.mce
  - pattern: "avc: denied"
    category: selinux.denial
"#,
        )
        .unwrap();
        assert_eq!(m.literal_count, 2); // "Out of memory..." + "avc: denied"
        assert_eq!(m.categorize("Out of memory: Killed process 1 (x)"), "kernel.oom");
        assert_eq!(m.categorize("Hardware Error detected"), "hw.mce");
        assert_eq!(m.categorize("audit: avc: denied for pid 1"), "selinux.denial");
        assert_eq!(m.categorize("nothing here"), "system.general");
    }

    /// 실제 config/categories.yaml 로 (실로그 → 기대 카테고리) 회귀 검증.
    #[test]
    fn regression_against_real_categories_yaml() {
        let yaml = include_str!("../../config/categories.yaml");
        let m = CategoryMatcher::build(yaml).expect("실제 categories.yaml 파싱/컴파일");

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
            ("Jun 15 00:00:00 host myapp[1]: Accepted publickey debug message", "system.general"),
        ];

        for (line, expected) in cases {
            assert_eq!(m.categorize(line), *expected, "분류 회귀 실패: {line}");
        }
    }
}
