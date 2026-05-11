use anyhow::{bail, Context, Result};
use std::fs;
use std::path::Path;
use tracing::info;

pub fn self_attach(path: &str, memory_max: &str, cpu_max: &str) -> Result<()> {
    let cgroup = Path::new(path);

    fs::create_dir_all(cgroup)
        .with_context(|| format!("cgroup 디렉토리 생성 실패: {path}"))?;

    let mem_bytes = parse_memory(memory_max)
        .with_context(|| format!("memory_max 파싱 실패: {memory_max}"))?;

    fs::write(cgroup.join("memory.max"), format!("{mem_bytes}\n"))
        .context("memory.max 쓰기 실패")?;

    fs::write(cgroup.join("cpu.max"), format!("{cpu_max}\n"))
        .context("cpu.max 쓰기 실패")?;

    let pid = std::process::id();
    fs::write(cgroup.join("cgroup.procs"), format!("{pid}\n"))
        .context("cgroup.procs 쓰기 실패")?;

    info!(
        cgroup = path,
        pid,
        memory_max,
        cpu_max,
        "cgroup self-attach 완료"
    );

    Ok(())
}

// Vector 자식 프로세스 PID를 같은 cgroup에 등록
pub fn attach_pid(cgroup_path: &str, pid: u32) -> Result<()> {
    let procs = Path::new(cgroup_path).join("cgroup.procs");
    fs::write(&procs, format!("{pid}\n"))
        .with_context(|| format!("cgroup.procs 에 PID {pid} 등록 실패"))?;
    info!(cgroup = cgroup_path, pid, "자식 PID cgroup 등록");
    Ok(())
}

pub fn verify(cgroup_path: &str) -> Result<()> {
    let procs_path = Path::new(cgroup_path).join("cgroup.procs");
    let pid = std::process::id().to_string();

    let procs = fs::read_to_string(&procs_path)
        .with_context(|| format!("cgroup.procs 읽기 실패: {}", procs_path.display()))?;

    if !procs.lines().any(|l| l.trim() == pid) {
        bail!("PID {pid} 이 cgroup.procs 에 없음 — self-attach 실패");
    }

    let mem_current = fs::read_to_string(Path::new(cgroup_path).join("memory.current"))
        .unwrap_or_else(|_| "읽기 실패".into());

    info!(
        cgroup = cgroup_path,
        pid,
        memory_current = mem_current.trim(),
        "cgroup 검증 통과"
    );

    Ok(())
}

fn parse_memory(s: &str) -> Result<u64> {
    let s = s.trim().to_lowercase();
    if let Some(n) = s.strip_suffix('m') {
        let mb: u64 = n.parse()?;
        Ok(mb * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('g') {
        let gb: u64 = n.parse()?;
        Ok(gb * 1024 * 1024 * 1024)
    } else {
        Ok(s.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_megabytes() {
        assert_eq!(parse_memory("256m").unwrap(), 256 * 1024 * 1024);
    }

    #[test]
    fn parse_gigabytes() {
        assert_eq!(parse_memory("2g").unwrap(), 2u64 * 1024 * 1024 * 1024);
    }

    #[test]
    fn parse_raw_bytes() {
        assert_eq!(parse_memory("1073741824").unwrap(), 1073741824);
    }

    #[test]
    fn parse_invalid_returns_err() {
        assert!(parse_memory("abc").is_err());
    }

    #[test]
    fn parse_trims_whitespace() {
        assert_eq!(parse_memory("  512m  ").unwrap(), 512 * 1024 * 1024);
    }
}
