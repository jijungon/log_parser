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
}

pub struct CategoryMatcher {
    rules: Vec<(Regex, String)>,
}

impl CategoryMatcher {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("categories.yaml 읽기 실패: {path}"))?;
        let file: CategoriesFile =
            serde_yaml::from_str(&text).context("categories.yaml 파싱 실패")?;

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

            rules.push((re, entry.category));
        }

        info!(rules = rules.len(), path, "categories.yaml 로드 완료");
        Ok(Self { rules })
    }

    /// categories.yaml 없을 때 fallback — 모든 이벤트를 system.general로 분류
    pub fn fallback() -> Self {
        Self { rules: vec![] }
    }

    /// First-match-wins, case-insensitive substring/regex 매칭
    pub fn categorize<'a>(&'a self, message: &str) -> &'a str {
        for (re, category) in &self.rules {
            if re.is_match(message) {
                return category;
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
            .map(|(pat, cat)| {
                let re = RegexBuilder::new(pat).case_insensitive(true).build().unwrap();
                (re, cat.to_string())
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
}
