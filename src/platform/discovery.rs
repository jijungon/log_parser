use std::collections::HashMap;
use tracing::info;

#[derive(Debug, Clone, PartialEq)]
pub enum DistroFamily {
    Debian,
    Rhel,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct OsInfo {
    pub id: String,
    pub version_id: String,
    pub family: DistroFamily,
    pub syslog_path: &'static str,
    pub auth_log_path: &'static str,
}

impl OsInfo {
    pub fn detect() -> Self {
        let os = Self::resolve(parse_os_release());
        info!(
            id = %os.id,
            version = %os.version_id,
            family = ?os.family,
            syslog = os.syslog_path,
            auth = os.auth_log_path,
            "OS 탐지 완료"
        );
        os
    }

    fn resolve(fields: HashMap<String, String>) -> Self {
        let id = fields.get("ID").cloned().unwrap_or_default().to_lowercase();
        let version_id = fields
            .get("VERSION_ID")
            .cloned()
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let id_like = fields.get("ID_LIKE").cloned().unwrap_or_default().to_lowercase();

        let family = if id == "ubuntu" || id == "debian" || id_like.contains("debian") {
            DistroFamily::Debian
        } else if matches!(id.as_str(), "rhel" | "rocky" | "almalinux" | "centos")
            || id_like.contains("rhel")
            || id_like.contains("fedora")
        {
            DistroFamily::Rhel
        } else {
            DistroFamily::Unknown
        };

        let (syslog_path, auth_log_path) = match family {
            DistroFamily::Debian | DistroFamily::Unknown => ("/var/log/syslog", "/var/log/auth.log"),
            DistroFamily::Rhel => ("/var/log/messages", "/var/log/secure"),
        };

        Self { id, version_id, family, syslog_path, auth_log_path }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(id: &str, id_like: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("ID".to_string(), id.to_string());
        if !id_like.is_empty() {
            m.insert("ID_LIKE".to_string(), id_like.to_string());
        }
        m
    }

    #[test]
    fn ubuntu_is_debian_family() {
        let os = OsInfo::resolve(fields("ubuntu", ""));
        assert_eq!(os.family, DistroFamily::Debian);
        assert_eq!(os.syslog_path, "/var/log/syslog");
        assert_eq!(os.auth_log_path, "/var/log/auth.log");
    }

    #[test]
    fn rocky_is_rhel_family() {
        let os = OsInfo::resolve(fields("rocky", "rhel centos fedora"));
        assert_eq!(os.family, DistroFamily::Rhel);
        assert_eq!(os.syslog_path, "/var/log/messages");
        assert_eq!(os.auth_log_path, "/var/log/secure");
    }

    #[test]
    fn almalinux_by_id_like_rhel() {
        let os = OsInfo::resolve(fields("almalinux", "rhel"));
        assert_eq!(os.family, DistroFamily::Rhel);
    }

    #[test]
    fn unknown_os_uses_debian_paths() {
        let os = OsInfo::resolve(fields("arch", ""));
        assert_eq!(os.family, DistroFamily::Unknown);
        assert_eq!(os.syslog_path, "/var/log/syslog");
        assert_eq!(os.auth_log_path, "/var/log/auth.log");
    }
}

fn parse_os_release() -> HashMap<String, String> {
    let text = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let mut map = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            map.insert(k.to_string(), v.trim_matches('"').to_string());
        }
    }
    map
}
