use std::path::Path;
use tracing::{info, warn};

pub struct Probes {
    pub vector_ok: bool,
    pub journald_ok: bool,
    pub journald_dir: Option<String>, // Some(path) = journalctl 없이 파일 직접 읽기
    pub syslog_ok: bool,
    pub auth_ok: bool,
    pub audit_ok: bool,
}

pub fn probe(vector_bin: &str, syslog_path: &str, auth_path: &str) -> Probes {
    let vector_ok = Path::new(vector_bin).is_file();
    if !vector_ok {
        warn!(path = vector_bin, "Vector 바이너리 없음");
    }

    // journald: journalctl 우선, 없으면 저널 디렉토리 직접 읽기
    let journalctl_ok = std::process::Command::new("journalctl")
        .arg("--version")
        .output()
        .is_ok();

    let (journald_ok, journald_dir) = if journalctl_ok {
        info!("journalctl 확인 — journald source 활성화");
        (true, None)
    } else {
        // volatile(/run) 우선, 없으면 persistent(/var)
        let dir = ["/run/log/journal", "/var/log/journal"]
            .iter()
            .find(|p| Path::new(p).exists())
            .map(|p| p.to_string());

        if let Some(ref d) = dir {
            info!(dir = %d, "저널 디렉토리 발견 — journal_directory 모드로 활성화");
            (true, dir)
        } else {
            warn!("journalctl 없음, 저널 디렉토리도 없음 — journald source 비활성화");
            (false, None)
        }
    };

    let syslog_ok = Path::new(syslog_path).exists();
    if !syslog_ok {
        warn!(path = syslog_path, "syslog 파일 없음 — source 비활성화");
    }

    let auth_ok = Path::new(auth_path).exists();
    if !auth_ok {
        warn!(path = auth_path, "auth 로그 없음 — source 비활성화");
    }

    let audit_ok = Path::new("/var/log/audit/audit.log").exists();
    if !audit_ok {
        warn!("audit.log 없음 — graceful skip");
    }

    Probes { vector_ok, journald_ok, journald_dir, syslog_ok, auth_ok, audit_ok }
}

impl Probes {
    pub fn has_log_sources(&self) -> bool {
        self.journald_ok || self.syslog_ok || self.auth_ok || self.audit_ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probes(journald: bool, syslog: bool, auth: bool, audit: bool) -> Probes {
        Probes { vector_ok: false, journald_ok: journald, journald_dir: None,
                 syslog_ok: syslog, auth_ok: auth, audit_ok: audit }
    }

    #[test]
    fn has_log_sources_true_when_journald_ok() {
        assert!(probes(true, false, false, false).has_log_sources());
    }

    #[test]
    fn has_log_sources_true_when_syslog_ok() {
        assert!(probes(false, true, false, false).has_log_sources());
    }

    #[test]
    fn has_log_sources_true_when_auth_ok() {
        assert!(probes(false, false, true, false).has_log_sources());
    }

    #[test]
    fn has_log_sources_true_when_audit_ok() {
        assert!(probes(false, false, false, true).has_log_sources());
    }

    #[test]
    fn has_log_sources_false_when_all_false() {
        assert!(!probes(false, false, false, false).has_log_sources());
        // vector_ok alone does not count as a log source
        let p = Probes { vector_ok: true, journald_ok: false, journald_dir: None,
                         syslog_ok: false, auth_ok: false, audit_ok: false };
        assert!(!p.has_log_sources());
    }

    #[test]
    fn probe_vector_false_for_nonexistent_binary() {
        let p = probe("/nonexistent/vector/bin", "/nonexistent/syslog", "/nonexistent/auth");
        assert!(!p.vector_ok, "nonexistent binary should not be vector_ok");
        assert!(!p.syslog_ok);
        assert!(!p.auth_ok);
    }

    #[test]
    fn probe_syslog_true_for_existing_file() {
        // /proc/version always exists on Linux — use it as a stand-in
        let p = probe("/nonexistent/vector", "/proc/version", "/nonexistent/auth");
        assert!(p.syslog_ok, "existing path should set syslog_ok");
        assert!(!p.auth_ok);
    }

    #[test]
    fn journald_dir_fallback_when_journalctl_missing() {
        // probe() with a known-absent binary and nonexistent paths exercises the journald_dir
        // detection logic; the exact result depends on the host, but it must not panic.
        let p = probe("/nonexistent/vector", "/nonexistent/syslog", "/nonexistent/auth");
        // journald_dir is either Some(path) or None depending on whether /run/log/journal exists
        let _ = p.journald_dir;
    }
}
