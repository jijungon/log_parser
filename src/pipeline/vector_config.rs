use crate::config::PipelineConfig;
use crate::platform::capability::Probes;
use crate::platform::discovery::OsInfo;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Generate a distro-adapted vector.toml at `out_path`.
pub fn write_runtime(
    pipeline: &PipelineConfig,
    os: &OsInfo,
    probes: &Probes,
    out_path: &str,
) -> Result<()> {
    let toml = build(pipeline, os, probes);
    if let Some(parent) = Path::new(out_path).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("vector config 디렉토리 생성 실패: {}", parent.display()))?;
    }
    fs::write(out_path, &toml)
        .with_context(|| format!("vector.toml 쓰기 실패: {out_path}"))?;
    Ok(())
}

/// 파일 소스용 multiline 병합 블록.
/// syslog 타임스탬프 헤더(RFC3164 "Jun 15 ..." / ISO "2026-06-15T...")로 시작하는 줄을
/// "새 이벤트의 시작"으로 보고, 헤더 없는 후속 줄(자바 스택트레이스·커널 콜트레이스 등)은
/// 직전 이벤트에 이어 붙인다. tokens.rs 의 SYSLOG_PREFIX 정규식과 동일한 헤더 기준.
/// audit.log(비-syslog `type=...` 포맷)·journald(이미 구조화)에는 적용하지 않는다.
fn multiline_block(source: &str) -> String {
    let ts = r"^(?:<\d{1,3}>\d?\s+)?(?:\d{4}-\d{2}-\d{2}T|[A-Za-z]{3}\s+\d{1,2}\s)";
    format!(
        r#"[sources.{source}.multiline]
start_pattern = '{ts}'
mode = "halt_before"
condition_pattern = '{ts}'
timeout_ms = 1000
"#
    )
}

fn build(pipeline: &PipelineConfig, os: &OsInfo, probes: &Probes) -> String {
    let critical_sock = &pipeline.vector_critical_sock;
    let normal_sock = &pipeline.vector_normal_sock;
    let syslog_path = os.syslog_path;
    let auth_path = os.auth_log_path;

    let mut source_names: Vec<&str> = vec![];

    let file_syslog_section = if probes.syslog_ok {
        source_names.push("file_label");
        let multiline = multiline_block("file_syslog");
        format!(
            r#"[sources.file_syslog]
type = "file"
include = ["{syslog_path}"]
read_from = "end"
ignore_older_secs = 86400

{multiline}
[transforms.file_label]
type = "remap"
inputs = ["file_syslog"]
source = '''
  .log_parser_severity = "info"
  .log_parser_source = "file.syslog"
'''
"#
        )
    } else {
        String::new()
    };

    let file_auth_section = if probes.auth_ok {
        source_names.push("auth_label");
        let multiline = multiline_block("file_auth");
        format!(
            r#"[sources.file_auth]
type = "file"
include = ["{auth_path}"]
read_from = "end"
ignore_older_secs = 86400

{multiline}
[transforms.auth_label]
type = "remap"
inputs = ["file_auth"]
source = '''
  .log_parser_severity = "info"
  .log_parser_source = "file.auth"
'''
"#
        )
    } else {
        String::new()
    };

    let file_audit_section = if probes.audit_ok {
        source_names.push("audit_label");
        r#"[sources.file_audit]
type = "file"
include = ["/var/log/audit/audit.log"]
read_from = "end"
ignore_older_secs = 86400

[transforms.audit_label]
type = "remap"
inputs = ["file_audit"]
source = '''
  .log_parser_severity = "info"
  .log_parser_source = "file.audit"
'''
"#
        .to_string()
    } else {
        String::new()
    };

    let journald_section = if probes.journald_ok {
        source_names.push("journald_severity");
        let dir_line = probes.journald_dir
            .as_deref()
            .map(|d| format!("journal_directory = \"{d}\"\n"))
            .unwrap_or_default();
        format!(
            r#"[sources.journald]
type = "journald"
{dir_line}
[transforms.journald_severity]
type = "remap"
inputs = ["journald"]
source = '''
  priority = to_int(.PRIORITY) ?? 6
  .log_parser_severity = if priority <= 2 {{
    "critical"
  }} else if priority == 3 {{
    "error"
  }} else if priority == 4 {{
    "warn"
  }} else {{
    "info"
  }}
  .log_parser_source = "journald"
'''
"#
        )
    } else {
        String::new()
    };

    format!(
        r#"# Vector 0.55.0 — log_parser runtime config (auto-generated, distro={distro:?})
# DO NOT EDIT — restart overwrites. Source: config/vector.toml

data_dir = "/var/lib/log_parser/vector"

# ── Sources ───────────────────────────────────────────────────────────────────

{journald_section}
{file_syslog_section}
{file_auth_section}
{file_audit_section}
# ── Transforms ────────────────────────────────────────────────────────────────

# 노이즈 제거(문 앞 필터): route/소켓으로 넘어가기 전에 잡음 로그를 버려 Rust 부하·전송량 절감.
# 원본이 필요하면 inbound pull API(/trigger-sos·/stat)로 회수 가능하므로 안전하게 버린다.
# 기본: journald debug(PRIORITY 7) 제거. 파일 소스는 .PRIORITY 없음(?? 6) → 통과.
# 앱 로그 DEBUG/TRACE 레벨까지 끄려면 아래 조건에 다음을 && 로 이어 붙인다:
#   && !match(to_string(.message) ?? to_string(.file_message) ?? "", r'(?i)\b(DEBUG|TRACE)\b')
[transforms.drop_noise]
type = "filter"
inputs = [{route_inputs_quoted}]
condition = '''
  (to_int(.PRIORITY) ?? 6) < 7
'''

[transforms.route_severity]
type = "route"
inputs = ["drop_noise"]

[transforms.route_severity.route]
critical = '.log_parser_severity == "critical"'
normal   = '.log_parser_severity != "critical"'

# ── Sinks ─────────────────────────────────────────────────────────────────────

[sinks.to_rust_critical]
type = "socket"
inputs = ["route_severity.critical"]
mode = "unix"
path = "{critical_sock}"
encoding.codec = "json"
acknowledgements.enabled = true

[sinks.to_rust_critical.buffer]
type = "disk"
max_size = 536870912
when_full = "block"

[sinks.to_rust_normal]
type = "socket"
inputs = ["route_severity.normal"]
mode = "unix"
path = "{normal_sock}"
encoding.codec = "json"
acknowledgements.enabled = true

[sinks.to_rust_normal.buffer]
type = "disk"
max_size = 536870912
when_full = "drop_newest"
"#,
        distro = os.family,
        journald_section = journald_section,
        file_syslog_section = file_syslog_section,
        file_auth_section = file_auth_section,
        file_audit_section = file_audit_section,
        route_inputs_quoted = source_names
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", "),
        critical_sock = critical_sock,
        normal_sock = normal_sock,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PipelineConfig;
    use crate::platform::capability::Probes;
    use crate::platform::discovery::{DistroFamily, OsInfo};

    fn default_pipeline() -> PipelineConfig {
        PipelineConfig::default()
    }

    fn probes(journald: bool, syslog: bool, auth: bool, audit: bool) -> Probes {
        Probes {
            vector_ok: true,
            journald_ok: journald,
            journald_dir: None,
            syslog_ok: syslog,
            auth_ok: auth,
            audit_ok: audit,
        }
    }

    fn ubuntu_os() -> OsInfo {
        OsInfo {
            id: "ubuntu".to_string(),
            version_id: "22.04".to_string(),
            family: DistroFamily::Debian,
            syslog_path: "/var/log/syslog",
            auth_log_path: "/var/log/auth.log",
        }
    }

    fn rhel_os() -> OsInfo {
        OsInfo {
            id: "rocky".to_string(),
            version_id: "9".to_string(),
            family: DistroFamily::Rhel,
            syslog_path: "/var/log/messages",
            auth_log_path: "/var/log/secure",
        }
    }

    #[test]
    fn journald_only_contains_journald_source() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(true, false, false, false));
        assert!(toml.contains("[sources.journald]"));
        assert!(!toml.contains("[sources.file_syslog]"));
        assert!(!toml.contains("[sources.file_auth]"));
        assert!(!toml.contains("[sources.file_audit]"));
        assert!(toml.contains("\"journald_severity\""));
    }

    #[test]
    fn syslog_only_contains_file_syslog_source() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(false, true, false, false));
        assert!(!toml.contains("[sources.journald]"));
        assert!(toml.contains("[sources.file_syslog]"));
        assert!(toml.contains("/var/log/syslog"));
    }

    #[test]
    fn rhel_uses_rhel_paths() {
        let toml = build(&default_pipeline(), &rhel_os(), &probes(false, true, true, false));
        assert!(toml.contains("/var/log/messages"));
        assert!(toml.contains("/var/log/secure"));
    }

    #[test]
    fn audit_section_present_when_audit_ok() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(false, false, false, true));
        assert!(toml.contains("[sources.file_audit]"));
        assert!(toml.contains("/var/log/audit/audit.log"));
    }

    #[test]
    fn all_sources_produces_four_inputs() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(true, true, true, true));
        assert!(toml.contains("[sources.journald]"));
        assert!(toml.contains("[sources.file_syslog]"));
        assert!(toml.contains("[sources.file_auth]"));
        assert!(toml.contains("[sources.file_audit]"));
        // route_severity inputs must reference all four labels
        assert!(toml.contains("\"journald_severity\""));
        assert!(toml.contains("\"file_label\""));
        assert!(toml.contains("\"auth_label\""));
        assert!(toml.contains("\"audit_label\""));
    }

    #[test]
    fn no_sources_produces_empty_route_inputs() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(false, false, false, false));
        // route_severity inputs list should be empty
        assert!(toml.contains("inputs = []"));
    }

    #[test]
    fn journald_dir_mode_emits_journal_directory_line() {
        let mut p = probes(true, false, false, false);
        p.journald_dir = Some("/run/log/journal".to_string());
        let toml = build(&default_pipeline(), &ubuntu_os(), &p);
        assert!(toml.contains("journal_directory = \"/run/log/journal\""));
    }

    #[test]
    fn critical_sink_uses_block_policy() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(true, false, false, false));
        assert!(toml.contains("when_full = \"block\""));
    }

    #[test]
    fn normal_sink_uses_drop_newest_policy() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(true, false, false, false));
        assert!(toml.contains("when_full = \"drop_newest\""));
    }

    #[test]
    fn file_sources_have_multiline() {
        // syslog·auth 파일 소스에는 multiline 병합이 붙는다.
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(false, true, true, false));
        assert!(toml.contains("[sources.file_syslog.multiline]"));
        assert!(toml.contains("[sources.file_auth.multiline]"));
        assert!(toml.contains("mode = \"halt_before\""));
    }

    #[test]
    fn audit_and_journald_have_no_multiline() {
        // audit(비-syslog 포맷)·journald(이미 구조화)에는 multiline을 붙이지 않는다.
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(true, false, false, true));
        assert!(!toml.contains(".multiline]"));
    }

    #[test]
    fn drop_noise_filter_present_and_feeds_route() {
        let toml = build(&default_pipeline(), &ubuntu_os(), &probes(true, true, false, false));
        assert!(toml.contains("[transforms.drop_noise]"));
        assert!(toml.contains("type = \"filter\""));
        // route_severity는 이제 drop_noise 출력을 입력으로 받는다.
        assert!(toml.contains("inputs = [\"drop_noise\"]"));
    }
}
