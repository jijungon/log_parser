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
    // Catch-all for any additional Vector-provided fields; kept to prevent deserialization failure.
    #[allow(dead_code)]
    #[serde(flatten)]
    pub extra: serde_json::Value,
}

impl RawLogEvent {
    pub fn raw_message(&self) -> &str {
        self.message
            .as_deref()
            .or(self.file_message.as_deref())
            .unwrap_or("")
    }
}

