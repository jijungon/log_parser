use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub cycle: CycleConfig,
    #[serde(default)]
    pub cgroup: CgroupConfig,
    #[serde(default)]
    pub pipeline: PipelineConfig,
    #[serde(default)]
    pub dedup: DedupConfig,
    #[serde(default)]
    pub transport: TransportConfig,
    #[serde(default)]
    pub inbound: InboundConfig,
    #[serde(default)]
    pub static_state: StaticStateConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CycleConfig {
    #[serde(default = "default_window_seconds")]
    pub window_seconds: u64,
    #[serde(default)]
    pub host_id_override: String,
    #[serde(default)]
    pub boot_id_fallback: String,
}

#[derive(Debug, Deserialize)]
pub struct CgroupConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cgroup_path")]
    pub path: String,
    #[serde(default = "default_memory_max")]
    pub memory_max: String,
    #[serde(default = "default_cpu_max")]
    pub cpu_max: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PipelineConfig {
    #[serde(default = "default_vector_bin")]
    pub vector_bin: String,
    #[serde(default = "default_vector_config")]
    pub vector_config: String,
    #[serde(default = "default_critical_sock")]
    pub vector_critical_sock: String,
    #[serde(default = "default_normal_sock")]
    pub vector_normal_sock: String,
    #[serde(default = "default_max_restarts")]
    pub vector_max_restarts_per_hour: u32,
    #[serde(default = "default_channel_capacity")]
    pub channel_capacity: usize,
    #[serde(default = "default_body_max_events")]
    pub body_max_events_per_cycle: usize,
    #[serde(default = "default_body_max_size_mb")]
    pub body_max_size_mb: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DedupConfig {
    #[serde(default = "default_dedup_window")]
    pub window_seconds: u64,
    #[serde(default = "default_lru_cap")]
    pub lru_cap: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransportConfig {
    #[serde(default = "default_transport_kind")]
    pub kind: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default = "default_token_env")]
    pub token_env: String,
    #[serde(default = "default_true")]
    pub tls_enabled: bool,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_seconds: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_seconds: u64,
    #[serde(default = "default_retry_max_normal")]
    pub retry_max_normal: u32,
    #[serde(default = "default_retry_base")]
    pub retry_base_seconds: u64,
    #[serde(default = "default_retry_max")]
    pub retry_max_seconds: u64,
    #[serde(default = "default_spool_max_mb")]
    pub spool_max_mb: u64,
    #[serde(default = "default_spool_dir")]
    pub spool_dir: String,
    #[serde(default = "default_gzip_level")]
    pub http_gzip_level: u32,
}

#[derive(Debug, Deserialize)]
pub struct InboundConfig {
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_flush_token_env")]
    pub token_env: String,
    #[serde(default = "default_stat_token_env")]
    pub stat_token_env: String,
    #[serde(default = "default_sos_token_env")]
    pub sos_token_env: String,
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_hour: u32,
    #[serde(default = "default_body_size_limit")]
    pub body_size_limit_bytes: u64,
    #[serde(default = "default_response_timeout")]
    pub response_timeout_seconds: u64,
    #[serde(default = "default_envelope_size_limit_mb")]
    pub envelope_size_limit_mb: u64,
    #[serde(default = "default_serialize_strategy")]
    pub serialize_strategy: String,
}

#[derive(Debug, Deserialize)]
pub struct StaticStateConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_scan_interval")]
    pub scan_interval_seconds: u64,
    #[serde(default = "default_static_state_storage_path")]
    pub storage_path: String,
    #[serde(default = "default_seq_state_path")]
    pub seq_state_path: String,
}

impl Config {
    pub fn load(path: &str) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("agent.yaml 읽기 실패: {}", path))?;
        let config: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("agent.yaml 파싱 실패: {}", path))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.transport.endpoint.is_empty() {
            tracing::warn!("transport.endpoint 가 비어있음 — Transport 기동 시 실패");
        }
        anyhow::ensure!(self.dedup.lru_cap > 0, "dedup.lru_cap 은 0보다 커야 합니다");
        Ok(())
    }
}

impl Default for CycleConfig {
    fn default() -> Self {
        Self {
            window_seconds: default_window_seconds(),
            host_id_override: String::new(),
            boot_id_fallback: String::new(),
        }
    }
}

impl Default for CgroupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: default_cgroup_path(),
            memory_max: default_memory_max(),
            cpu_max: default_cpu_max(),
        }
    }
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            vector_bin: default_vector_bin(),
            vector_config: default_vector_config(),
            vector_critical_sock: default_critical_sock(),
            vector_normal_sock: default_normal_sock(),
            vector_max_restarts_per_hour: default_max_restarts(),
            channel_capacity: default_channel_capacity(),
            body_max_events_per_cycle: default_body_max_events(),
            body_max_size_mb: default_body_max_size_mb(),
        }
    }
}

impl Default for DedupConfig {
    fn default() -> Self {
        Self {
            window_seconds: default_dedup_window(),
            lru_cap: default_lru_cap(),
        }
    }
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            kind: default_transport_kind(),
            endpoint: String::new(),
            token_env: default_token_env(),
            tls_enabled: true,
            connect_timeout_seconds: default_connect_timeout(),
            request_timeout_seconds: default_request_timeout(),
            retry_max_normal: default_retry_max_normal(),
            retry_base_seconds: default_retry_base(),
            retry_max_seconds: default_retry_max(),
            spool_max_mb: default_spool_max_mb(),
            spool_dir: default_spool_dir(),
            http_gzip_level: default_gzip_level(),
        }
    }
}

impl Default for InboundConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            token_env: default_flush_token_env(),
            stat_token_env: default_stat_token_env(),
            sos_token_env: default_sos_token_env(),
            rate_limit_per_hour: default_rate_limit(),
            body_size_limit_bytes: default_body_size_limit(),
            response_timeout_seconds: default_response_timeout(),
            envelope_size_limit_mb: default_envelope_size_limit_mb(),
            serialize_strategy: default_serialize_strategy(),
        }
    }
}

impl Default for StaticStateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_seconds: default_scan_interval(),
            storage_path: default_static_state_storage_path(),
            seq_state_path: default_seq_state_path(),
        }
    }
}

fn default_window_seconds() -> u64 { 1800 }
fn default_true() -> bool { true }
fn default_cgroup_path() -> String { "/sys/fs/cgroup/log_parser_agent".into() }
fn default_memory_max() -> String { "128m".into() }
fn default_cpu_max() -> String { "50000 1000000".into() }
fn default_vector_bin() -> String { "/app/vector/bin/vector".into() }
fn default_vector_config() -> String { "/etc/log_parser/vector.toml".into() }
fn default_critical_sock() -> String { "/run/log_parser/events_critical.sock".into() }
fn default_normal_sock() -> String { "/run/log_parser/events_normal.sock".into() }
fn default_max_restarts() -> u32 { 5 }
fn default_channel_capacity() -> usize { 10000 }
fn default_body_max_events() -> usize { 100000 }
fn default_body_max_size_mb() -> u64 { 50 }
fn default_dedup_window() -> u64 { 30 }
fn default_lru_cap() -> usize { 50000 }
fn default_transport_kind() -> String { "http_json".into() }
fn default_token_env() -> String { "PUSH_OUTBOUND_TOKEN".into() }
fn default_connect_timeout() -> u64 { 10 }
fn default_request_timeout() -> u64 { 30 }
fn default_retry_max_normal() -> u32 { 5 }
fn default_retry_base() -> u64 { 5 }
fn default_retry_max() -> u64 { 300 }
fn default_spool_max_mb() -> u64 { 512 }
fn default_spool_dir() -> String { "/var/lib/log_parser/spool".into() }
fn default_gzip_level() -> u32 { 6 }
fn default_listen_addr() -> String { "127.0.0.1:9100".into() }
fn default_flush_token_env() -> String { "FLUSH_INBOUND_TOKEN".into() }
fn default_stat_token_env() -> String { "STAT_INBOUND_TOKEN".into() }
fn default_sos_token_env() -> String { "SOS_INBOUND_TOKEN".into() }
fn default_rate_limit() -> u32 { 6 }
fn default_body_size_limit() -> u64 { 1024 }
fn default_response_timeout() -> u64 { 5 }
fn default_envelope_size_limit_mb() -> u64 { 10 }
fn default_serialize_strategy() -> String { "wait".into() }
fn default_scan_interval() -> u64 { 1800 }
fn default_static_state_storage_path() -> String { "/var/lib/log_parser/host_static_state.json".into() }
fn default_seq_state_path() -> String { "/var/lib/log_parser/seq.state".into() }

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).expect("parse failed")
    }

    #[test]
    fn empty_yaml_uses_defaults() {
        let cfg = parse("{}");
        assert_eq!(cfg.cycle.window_seconds, 1800);
        assert_eq!(cfg.dedup.window_seconds, 30);
        assert_eq!(cfg.dedup.lru_cap, 50000);
        assert_eq!(cfg.transport.spool_max_mb, 512);
        assert_eq!(cfg.inbound.rate_limit_per_hour, 6);
        assert_eq!(cfg.inbound.body_size_limit_bytes, 1024);
        assert_eq!(cfg.inbound.envelope_size_limit_mb, 10);
        assert_eq!(cfg.inbound.serialize_strategy, "wait");
        assert!(cfg.static_state.enabled);
        assert_eq!(cfg.static_state.scan_interval_seconds, 1800);
    }

    #[test]
    fn override_values_are_applied() {
        let cfg = parse("cycle:\n  window_seconds: 900\ndedup:\n  window_seconds: 60\n");
        assert_eq!(cfg.cycle.window_seconds, 900);
        assert_eq!(cfg.dedup.window_seconds, 60);
        // unset fields still use defaults
        assert_eq!(cfg.dedup.lru_cap, 50000);
    }

    #[test]
    fn inbound_fields_parsed() {
        let yaml = "inbound:\n  rate_limit_per_hour: 12\n  envelope_size_limit_mb: 20\n  serialize_strategy: reject\n";
        let cfg = parse(yaml);
        assert_eq!(cfg.inbound.rate_limit_per_hour, 12);
        assert_eq!(cfg.inbound.envelope_size_limit_mb, 20);
        assert_eq!(cfg.inbound.serialize_strategy, "reject");
    }

    #[test]
    fn static_state_fields_parsed() {
        let yaml = "static_state:\n  enabled: false\n  scan_interval_seconds: 3600\n";
        let cfg = parse(yaml);
        assert!(!cfg.static_state.enabled);
        assert_eq!(cfg.static_state.scan_interval_seconds, 3600);
    }
}
