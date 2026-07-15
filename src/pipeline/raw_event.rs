use chrono::{DateTime, Utc};
use serde::Deserialize;

/// Vector socket sink에서 수신하는 단일 이벤트 (FROZEN — IMPL_NOTES.md §5)
#[derive(Debug, Deserialize)]
pub struct RawLogEvent {
    pub log_parser_severity: String,
    pub log_parser_source: String,
    pub timestamp: DateTime<Utc>,
    pub host: String,
    #[serde(rename = "MESSAGE")]
    pub message: Option<String>,
    #[serde(rename = "PRIORITY")]
    pub priority: Option<String>,
    #[serde(rename = "_SYSTEMD_UNIT")]
    pub unit: Option<String>,
    #[serde(rename = "_PID")]
    pub pid: Option<String>,
    #[serde(rename = "message")]
    pub file_message: Option<String>,
    #[serde(rename = "file")]
    pub file_path: Option<String>,
    // NOTE: Vector가 보내는 추가 필드는 serde 기본 동작으로 무시된다 (deny_unknown_fields 미사용).
    // 과거의 #[serde(flatten)] catch-all은 serde Content 버퍼링을 강제해 라인당 파싱 비용을
    // 약 2배로 만들었고, 값은 어디서도 읽히지 않아 제거함.
}

impl RawLogEvent {
    pub fn raw_message(&self) -> &str {
        self.message
            .as_deref()
            .or(self.file_message.as_deref())
            .unwrap_or("")
    }
}

