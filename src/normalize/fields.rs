use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tracing::info;

// ── 설정 파일 스키마 (config/fields.yaml) ──────────────────────────────────────

#[derive(Deserialize)]
struct FieldsFile {
    #[serde(default)]
    fields: Vec<Entry>,
    #[serde(default)]
    settings: Settings,
}

#[derive(Deserialize)]
struct Entry {
    /// 캡처 그룹 1을 값으로 쓰는 정규식.
    pattern: String,
    /// 저장할 필드 이름.
    key: String,
    /// true 면 값을 정수로 파싱해 숫자 필드로 저장.
    #[serde(default)]
    numeric: bool,
}

#[derive(Deserialize, Clone)]
struct Settings {
    /// 메시지 안의 임의 `key=value`(logfmt)를 자동 필드로 승격.
    #[serde(default)]
    logfmt: bool,
    /// 메시지 안의 JSON 객체(`{...}`) 최상위 스칼라를 자동 필드로 승격.
    #[serde(default)]
    json: bool,
    /// logfmt/json 자동 승격 시 허용할 키 목록. 비어 있으면 전부 허용(단, 상한 적용).
    /// auditd 처럼 key=value 가 폭주하는 로그를 막기 위한 안전장치.
    #[serde(default)]
    allow: Vec<String>,
    /// 자동 승격으로 추가할 수 있는 최대 필드 수(폭주 방지).
    #[serde(default = "default_max_auto")]
    max_auto_fields: usize,
}

fn default_max_auto() -> usize {
    20
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            logfmt: false,
            json: false,
            allow: vec![],
            max_auto_fields: default_max_auto(),
        }
    }
}

// ── 컴파일된 추출기 ─────────────────────────────────────────────────────────────

struct Extractor {
    re: Regex,
    key: String,
    numeric: bool,
}

pub struct FieldExtractor {
    extractors: Vec<Extractor>,
    settings: Settings,
    allow: HashSet<String>,
}

// 임의 logfmt 토큰: key=value / key="quoted value"
static LOGFMT_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"\b([A-Za-z_][A-Za-z0-9_.\-]*)=(?:"([^"]*)"|(\S+))"#).unwrap()
});

impl FieldExtractor {
    /// config/fields.yaml 로드. 파일이 없거나 깨지면 호출측에서 builtin() 으로 fallback.
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("fields.yaml 읽기 실패: {path}"))?;
        let me = Self::build(&text)?;
        info!(
            extractors = me.extractors.len(),
            logfmt = me.settings.logfmt,
            json = me.settings.json,
            path,
            "fields.yaml 로드 완료"
        );
        Ok(me)
    }

    /// yaml 텍스트 → FieldExtractor. load() 와 테스트가 공유.
    fn build(text: &str) -> Result<Self> {
        let file: FieldsFile = serde_yaml::from_str(text).context("fields.yaml 파싱 실패")?;
        let mut extractors = Vec::with_capacity(file.fields.len());
        for e in file.fields {
            let re = Regex::new(&e.pattern)
                .with_context(|| format!("필드 패턴 컴파일 실패: {}", e.pattern))?;
            extractors.push(Extractor {
                re,
                key: e.key,
                numeric: e.numeric,
            });
        }
        let allow = file.settings.allow.iter().cloned().collect();
        Ok(Self {
            extractors,
            settings: file.settings,
            allow,
        })
    }

    /// fields.yaml 없을 때 fallback — 기존 하드코딩 추출기 6종(동작 보존, 자동 파싱 off).
    pub fn builtin() -> Self {
        let raw: &[(&str, &str, bool)] = &[
            (r"[Kk]illed process (\d+)", "pid", true),
            (r"for (?:invalid )?user (\S+)", "user", false),
            (r"\bdev (\S+?)(?:[,\s]|$)", "dev", false),
            (r"^(\S+\.(?:service|socket|mount|target|timer))\b", "unit", false),
            (r"\bpid=(\d+)", "pid", true),
            (r"\buser=(\S+)", "user", false),
        ];
        let extractors = raw
            .iter()
            .map(|(pat, key, numeric)| Extractor {
                re: Regex::new(pat).unwrap(),
                key: key.to_string(),
                numeric: *numeric,
            })
            .collect();
        Self {
            extractors,
            settings: Settings::default(),
            allow: HashSet::new(),
        }
    }

    /// 메시지 본문(syslog prefix 제거 후)에서 구조화 필드 추출.
    /// 1) 설정 추출기(first-match-wins per key) → 2) logfmt → 3) JSON.
    /// 앞 단계에서 이미 채운 키는 뒤 단계가 덮어쓰지 않는다.
    pub fn extract(&self, msg: &str) -> HashMap<String, Value> {
        let mut fields = HashMap::new();

        for ext in &self.extractors {
            if fields.contains_key(&ext.key) {
                continue;
            }
            if let Some(cap) = ext.re.captures(msg) {
                if let Some(m) = cap.get(1) {
                    if let Some(v) = to_value(m.as_str(), ext.numeric) {
                        fields.insert(ext.key.clone(), v);
                    }
                }
            }
        }

        let mut auto = 0usize;
        // '=' 바이트가 없으면 logfmt 매치 불가 — captures_iter 전체 스캔 생략
        if self.settings.logfmt && msg.as_bytes().contains(&b'=') {
            for cap in LOGFMT_RE.captures_iter(msg) {
                if auto >= self.settings.max_auto_fields {
                    break;
                }
                let key = cap.get(1).unwrap().as_str();
                if !self.allowed(key) || fields.contains_key(key) {
                    continue;
                }
                let val = cap
                    .get(2)
                    .or_else(|| cap.get(3))
                    .map(|m| m.as_str())
                    .unwrap_or("");
                fields.insert(key.to_string(), to_value(val, false).unwrap());
                auto += 1;
            }
        }

        if self.settings.json {
            if let Some(obj) = extract_json_object(msg) {
                for (k, v) in obj {
                    if auto >= self.settings.max_auto_fields {
                        break;
                    }
                    if !self.allowed(&k) || fields.contains_key(&k) || !v.is_scalar() {
                        continue;
                    }
                    fields.insert(k, v);
                    auto += 1;
                }
            }
        }

        fields
    }

    fn allowed(&self, key: &str) -> bool {
        self.allow.is_empty() || self.allow.contains(key)
    }
}

/// 값 문자열을 필드 값으로 변환. numeric 이면 정수 우선, 실패 시 None.
/// 비numeric 은 숫자처럼 보이면 숫자로, 아니면 문자열로 저장(자동 파싱 편의).
fn to_value(raw: &str, numeric: bool) -> Option<Value> {
    if numeric {
        return raw.parse::<i64>().ok().map(|n| Value::Number(n.into()));
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Some(Value::Number(n.into()));
    }
    if let Ok(f) = raw.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(f) {
            return Some(Value::Number(num));
        }
    }
    Some(Value::String(raw.to_string()))
}

/// 메시지에서 첫 번째 `{...}` 균형 잡힌 JSON 객체를 찾아 파싱.
fn extract_json_object(msg: &str) -> Option<serde_json::Map<String, Value>> {
    let start = msg.find('{')?;
    let bytes = msg.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let slice = &msg[start..=i];
                    return serde_json::from_str::<Value>(slice)
                        .ok()
                        .and_then(|v| match v {
                            Value::Object(m) => Some(m),
                            _ => None,
                        });
                }
            }
            _ => {}
        }
    }
    None
}

trait IsScalar {
    fn is_scalar(&self) -> bool;
}
impl IsScalar for Value {
    fn is_scalar(&self) -> bool {
        matches!(
            self,
            Value::String(_) | Value::Number(_) | Value::Bool(_)
        )
    }
}

// ── 전역 인스턴스 (호출부 시그니처 유지용) ─────────────────────────────────────
// coordinator/collect 는 extract_fields(msg) 자유함수를 그대로 호출한다.
// main 이 기동 시 init_global(path) 로 config 기반 인스턴스를 심고,
// 미초기화 시(테스트 등)에는 builtin() 이 lazily 사용된다.

static GLOBAL: OnceLock<FieldExtractor> = OnceLock::new();

/// 기동 시 1회 호출. 이미 설정돼 있으면 무시(재호출 안전).
pub fn init_global(extractor: FieldExtractor) {
    let _ = GLOBAL.set(extractor);
}

fn global() -> &'static FieldExtractor {
    GLOBAL.get_or_init(FieldExtractor::builtin)
}

/// 메시지 본문에서 구조화 필드 추출 (전역 인스턴스 사용).
pub fn extract_fields(msg: &str) -> HashMap<String, Value> {
    global().extract(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    // builtin() 동작 = 기존 하드코딩 추출기 회귀 검증
    fn b() -> FieldExtractor {
        FieldExtractor::builtin()
    }

    #[test]
    fn extracts_oom_pid() {
        let f = b().extract("Killed process 2481 (java) total-vm:10240kB");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(2481)));
    }

    #[test]
    fn extracts_invalid_user() {
        let f = b().extract("Failed password for invalid user admin from 192.168.1.1");
        assert_eq!(f.get("user"), Some(&serde_json::json!("admin")));
    }

    #[test]
    fn extracts_kv_user() {
        let f = b().extract("session opened for user root by (uid=0)");
        assert_eq!(f.get("user"), Some(&serde_json::json!("root")));
    }

    #[test]
    fn extracts_kv_pid() {
        let f = b().extract("process exited pid=5678 status=0");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(5678)));
    }

    #[test]
    fn first_match_wins_for_pid() {
        let f = b().extract("Killed process 111 (app) pid=222");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(111)));
    }

    #[test]
    fn no_match_returns_empty() {
        let f = b().extract("just a plain log message");
        assert!(f.is_empty());
    }

    // ── 4번: logfmt / JSON ──────────────────────────────────────────────────

    fn with_settings(logfmt: bool, json: bool, allow: &[&str]) -> FieldExtractor {
        FieldExtractor {
            extractors: vec![],
            settings: Settings {
                logfmt,
                json,
                allow: allow.iter().map(|s| s.to_string()).collect(),
                max_auto_fields: 20,
            },
            allow: allow.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn logfmt_extracts_kv_pairs() {
        let e = with_settings(true, false, &[]);
        let f = e.extract("Scrap done site=naver.com code=200 duration=1.5");
        assert_eq!(f.get("site"), Some(&serde_json::json!("naver.com")));
        assert_eq!(f.get("code"), Some(&serde_json::json!(200)));
        assert_eq!(f.get("duration"), Some(&serde_json::json!(1.5)));
    }

    #[test]
    fn logfmt_respects_allowlist() {
        let e = with_settings(true, false, &["site"]);
        let f = e.extract("site=naver.com code=200 secret=abc");
        assert_eq!(f.get("site"), Some(&serde_json::json!("naver.com")));
        assert!(f.get("code").is_none());
        assert!(f.get("secret").is_none());
    }

    #[test]
    fn logfmt_handles_quoted_values() {
        let e = with_settings(true, false, &[]);
        let f = e.extract(r#"msg="hello world" level=info"#);
        assert_eq!(f.get("msg"), Some(&serde_json::json!("hello world")));
        assert_eq!(f.get("level"), Some(&serde_json::json!("info")));
    }

    #[test]
    fn json_extracts_top_level_scalars() {
        let e = with_settings(false, true, &[]);
        let f = e.extract(r#"request handled {"site":"naver.com","code":500,"ok":false}"#);
        assert_eq!(f.get("site"), Some(&serde_json::json!("naver.com")));
        assert_eq!(f.get("code"), Some(&serde_json::json!(500)));
        assert_eq!(f.get("ok"), Some(&serde_json::json!(false)));
    }

    #[test]
    fn json_skips_nested_non_scalar() {
        let e = with_settings(false, true, &[]);
        let f = e.extract(r#"{"a":1,"nested":{"x":2},"arr":[1,2]}"#);
        assert_eq!(f.get("a"), Some(&serde_json::json!(1)));
        assert!(f.get("nested").is_none());
        assert!(f.get("arr").is_none());
    }

    #[test]
    fn max_auto_fields_caps_explosion() {
        let mut e = with_settings(true, false, &[]);
        e.settings.max_auto_fields = 2;
        let f = e.extract("a=1 b=2 c=3 d=4");
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn config_extractor_from_yaml() {
        let yaml = r#"
fields:
  - pattern: 'req_id=(\w+)'
    key: req_id
settings:
  logfmt: true
  allow: [site]
"#;
        let e = FieldExtractor::build(yaml).unwrap();
        let f = e.extract("req_id=abc123 site=naver.com code=200");
        assert_eq!(f.get("req_id"), Some(&serde_json::json!("abc123")));
        assert_eq!(f.get("site"), Some(&serde_json::json!("naver.com")));
        assert!(f.get("code").is_none()); // allowlist 밖
    }

    #[test]
    fn real_fields_yaml_loads() {
        // 실제 config/fields.yaml 이 파싱/컴파일되는지 회귀 검증
        let yaml = include_str!("../../config/fields.yaml");
        let e = FieldExtractor::build(yaml).expect("실제 fields.yaml 파싱/컴파일");
        // 기존 핵심 추출기 동작 보존
        let f = e.extract("Killed process 2481 (java)");
        assert_eq!(f.get("pid"), Some(&serde_json::json!(2481)));
    }
}
