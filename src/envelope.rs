use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub event_kind: String,
    pub cycle: Cycle,
    pub headers: Headers,
    pub body: Vec<Section>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cycle {
    pub host: String,
    pub host_id: String,
    pub boot_id: String,
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Headers {
    pub total_sections: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_health: Option<ProcessHealth>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Counts {
    pub by_severity: BySeverity,
    pub by_category: HashMap<String, u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BySeverity {
    pub critical: u64,
    pub error: u64,
    pub warn: u64,
    pub info: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProcessHealth {
    pub vector_restarts_24h: u32,
    pub agent_uptime_seconds: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Section {
    pub section: String,
    pub data: serde_json::Value,
}

// Step 4에서 Pipeline이 생성. 지금은 스키마 정의만.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DedupEvent {
    pub source: String,
    pub severity: String,
    pub category: String,
    pub fingerprint: String,
    pub template: String,
    pub sample_raws: Vec<String>,
    pub fields: HashMap<String, serde_json::Value>,
    pub ts_first: String,
    pub ts_last: String,
    pub count: u64,
}
