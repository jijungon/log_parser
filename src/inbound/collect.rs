//! Pure async stat collection functions — no HTTP, no panics.
//! All functions return serde_json::Value; failures return reasonable defaults.

use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::process::Command;
use tracing::debug;

// ──────────────────────────────────────────────────────────────────────────────
// Metrics
// ──────────────────────────────────────────────────────────────────────────────

struct CpuSample {
    user: u64,
    nice: u64,
    system: u64,
    idle: u64,
    iowait: u64,
    irq: u64,
    softirq: u64,
}

fn read_cpu_sample() -> Option<CpuSample> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().find(|l| l.starts_with("cpu "))?;
    let parts: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    if parts.len() < 7 {
        return None;
    }
    Some(CpuSample {
        user: parts[0],
        nice: parts[1],
        system: parts[2],
        idle: parts[3],
        iowait: parts[4],
        irq: parts[5],
        softirq: parts[6],
    })
}

pub async fn collect_metrics() -> Value {
    // /proc 읽기 — spawn_blocking으로 executor 스레드 보호
    let (s1, disk1, net1) = tokio::task::spawn_blocking(|| {
        (read_cpu_sample(), read_diskstats(), read_net_dev())
    })
    .await
    .unwrap_or((None, Default::default(), Default::default()));

    // Single 200ms sleep covers all three delta measurements
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // After-samples
    let (s2, disk2, net2) = tokio::task::spawn_blocking(|| {
        (read_cpu_sample(), read_diskstats(), read_net_dev())
    })
    .await
    .unwrap_or((None, Default::default(), Default::default()));

    let cpu = if let (Some(a), Some(b)) = (s1, s2) {
        let total_a = a.user + a.nice + a.system + a.idle + a.iowait + a.irq + a.softirq;
        let total_b = b.user + b.nice + b.system + b.idle + b.iowait + b.irq + b.softirq;
        let delta = (total_b as f64 - total_a as f64).max(1.0);
        let idle_delta = b.idle as f64 - a.idle as f64;
        let iowait_delta = b.iowait as f64 - a.iowait as f64;
        let user_delta = (b.user + b.nice) as f64 - (a.user + a.nice) as f64;
        let sys_delta = b.system as f64 - a.system as f64;
        json!({
            "usage_pct":   round2(100.0 * (1.0 - idle_delta / delta)),
            "iowait_pct":  round2(100.0 * iowait_delta / delta),
            "user_pct":    round2(100.0 * user_delta / delta),
            "system_pct":  round2(100.0 * sys_delta / delta),
        })
    } else {
        json!({ "usage_pct": null, "iowait_pct": null, "user_pct": null, "system_pct": null })
    };

    let disk_io = compute_disk_io(disk1, disk2, 0.2);
    let network = compute_network(net1, net2, 0.2);

    // Memory + load avg + PSI — /proc reads, bundle in spawn_blocking
    let (memory, load_avg, pressure) = tokio::task::spawn_blocking(|| {
        (collect_meminfo_metrics(), collect_loadavg(), collect_psi())
    })
    .await
    .unwrap_or_else(|_| (json!({}), json!({}), json!({})));

    json!({
        "cpu": cpu,
        "memory": memory,
        "disk_io": disk_io,
        "network": network,
        "load_avg": load_avg,
        "pressure": pressure,
    })
}

fn collect_meminfo_metrics() -> Value {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mut map: HashMap<&str, u64> = HashMap::new();
    for line in text.lines() {
        if let Some((key, rest)) = line.split_once(':') {
            let val = rest.split_whitespace().next().and_then(|s| s.parse().ok()).unwrap_or(0);
            map.insert(key.trim(), val);
        }
    }
    let total = map.get("MemTotal").copied().unwrap_or(0) / 1024;
    let free = map.get("MemFree").copied().unwrap_or(0) / 1024;
    let available = map.get("MemAvailable").copied().unwrap_or(0) / 1024;
    let buffers = map.get("Buffers").copied().unwrap_or(0) / 1024;
    let cached = map.get("Cached").copied().unwrap_or(0) / 1024;
    let used = total.saturating_sub(free + buffers + cached);
    let swap_total = map.get("SwapTotal").copied().unwrap_or(0) / 1024;
    let swap_free = map.get("SwapFree").copied().unwrap_or(0) / 1024;
    let swap_used = swap_total.saturating_sub(swap_free);
    json!({
        "total_mb":     total,
        "used_mb":      used,
        "free_mb":      free,
        "available_mb": available,
        "swap_total_mb": swap_total,
        "swap_used_mb":  swap_used,
    })
}

fn collect_loadavg() -> Value {
    let text = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let parts: Vec<&str> = text.split_whitespace().collect();
    json!({
        "1m":  parts.first().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
        "5m":  parts.get(1).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
        "15m": parts.get(2).and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0),
    })
}

#[derive(Default)]
struct DiskStat {
    reads: u64,
    writes: u64,
    io_ticks: u64, // field 10 (ms spent doing I/O)
}

fn read_diskstats() -> HashMap<String, DiskStat> {
    let text = std::fs::read_to_string("/proc/diskstats").unwrap_or_default();
    let mut map = HashMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 14 {
            continue;
        }
        let dev = parts[2];
        // skip loop, ram, dm devices
        if dev.starts_with("loop") || dev.starts_with("ram") || dev.starts_with("dm-") {
            continue;
        }
        // only keep whole-disk names (no partition suffix digit at end after letters)
        // e.g. sda, sdb, nvme0n1 — skip sda1, sdb2, nvme0n1p1
        if is_partition(dev) {
            continue;
        }
        let reads: u64 = parts[3].parse().unwrap_or(0);
        let writes: u64 = parts[7].parse().unwrap_or(0);
        let io_ticks: u64 = parts[12].parse().unwrap_or(0);
        map.insert(dev.to_string(), DiskStat { reads, writes, io_ticks });
    }
    map
}

fn is_partition(dev: &str) -> bool {
    if dev.starts_with("nvme") {
        // nvme0n1p1: partition suffix is 'p' followed by one or more digits
        if let Some(p_pos) = dev.rfind('p') {
            let suffix = &dev[p_pos + 1..];
            return !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit());
        }
        return false;
    }
    if dev.starts_with("loop") {
        return false;
    }
    // sda1, sdb2, hda1, vdb2: ends with a digit
    dev.chars().last().map(|c| c.is_ascii_digit()).unwrap_or(false)
}

fn compute_disk_io(
    a: HashMap<String, DiskStat>,
    b: HashMap<String, DiskStat>,
    interval: f64,
) -> Value {
    let mut obj = serde_json::Map::new();
    for (dev, sb) in &b {
        if let Some(sa) = a.get(dev) {
            let reads_ps = round2((sb.reads.saturating_sub(sa.reads)) as f64 / interval);
            let writes_ps = round2((sb.writes.saturating_sub(sa.writes)) as f64 / interval);
            // util_pct: ms doing IO / (interval * 1000) * 100
            let io_ms = sb.io_ticks.saturating_sub(sa.io_ticks) as f64;
            let util = round2((io_ms / (interval * 1000.0)) * 100.0).min(100.0);
            obj.insert(dev.clone(), json!({
                "reads_per_sec":  reads_ps,
                "writes_per_sec": writes_ps,
                "util_pct":       util,
            }));
        }
    }
    Value::Object(obj)
}

#[derive(Default)]
struct NetStat {
    rx_bytes: u64,
    tx_bytes: u64,
}

fn read_net_dev() -> HashMap<String, NetStat> {
    let text = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    let mut map = HashMap::new();
    for line in text.lines().skip(2) {
        if let Some((iface, rest)) = line.split_once(':') {
            let iface = iface.trim();
            if iface == "lo" {
                continue;
            }
            let parts: Vec<&str> = rest.split_whitespace().collect();
            let rx: u64 = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
            let tx: u64 = parts.get(8).and_then(|s| s.parse().ok()).unwrap_or(0);
            map.insert(iface.to_string(), NetStat { rx_bytes: rx, tx_bytes: tx });
        }
    }
    map
}

fn compute_network(
    a: HashMap<String, NetStat>,
    b: HashMap<String, NetStat>,
    interval: f64,
) -> Value {
    let mut obj = serde_json::Map::new();
    for (iface, sb) in &b {
        if let Some(sa) = a.get(iface) {
            let rx_ps = round2(sb.rx_bytes.saturating_sub(sa.rx_bytes) as f64 / interval);
            let tx_ps = round2(sb.tx_bytes.saturating_sub(sa.tx_bytes) as f64 / interval);
            obj.insert(iface.clone(), json!({
                "rx_bytes_per_sec": rx_ps,
                "tx_bytes_per_sec": tx_ps,
            }));
        }
    }
    Value::Object(obj)
}

fn collect_psi() -> Value {
    fn parse_psi_file(path: &str) -> Value {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return json!({}),
        };
        let mut result = serde_json::Map::new();
        for line in text.lines() {
            // "some avg10=0.00 avg60=0.00 avg300=0.00 total=0"
            let parts: Vec<&str> = line.split_whitespace().collect();
            let kind = match parts.first().copied() {
                Some("some") => "some_pct",
                Some("full") => "full_pct",
                _ => continue,
            };
            // find avg10 value
            let avg10 = parts.iter()
                .find(|p| p.starts_with("avg10="))
                .and_then(|p| p.strip_prefix("avg10="))
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0);
            result.insert(kind.to_string(), json!(round2(avg10)));
        }
        Value::Object(result)
    }

    json!({
        "cpu":    parse_psi_file("/proc/pressure/cpu"),
        "memory": parse_psi_file("/proc/pressure/memory"),
        "io":     parse_psi_file("/proc/pressure/io"),
    })
}

// ──────────────────────────────────────────────────────────────────────────────
// Processes
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_processes() -> Value {
    tokio::task::spawn_blocking(|| {
        // Read meminfo once for per-process mem_pct (avoids re-read in helpers)
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let mem_total_kb: u64 = meminfo.lines()
            .find(|l| l.starts_with("MemTotal:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);

        let boot_time_secs = read_btime();
        let uid_map = parse_passwd();

        let mut procs = Vec::new();

        let proc_dir = match std::fs::read_dir("/proc") {
            Ok(d) => d,
            Err(_) => return json!([]),
        };

        for entry in proc_dir.flatten().take(500) {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let pid: u32 = match name_str.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };

            if let Some(info) = read_proc_info(pid, mem_total_kb, boot_time_secs, &uid_map) {
                procs.push(info);
            }
        }

        // Sort by mem_rss_mb desc, take top 20
        procs.sort_by(|a, b| {
            let am = a.get("mem_rss_mb").and_then(|v| v.as_u64()).unwrap_or(0);
            let bm = b.get("mem_rss_mb").and_then(|v| v.as_u64()).unwrap_or(0);
            bm.cmp(&am)
        });
        procs.truncate(20);

        Value::Array(procs)
    })
    .await
    .unwrap_or(json!([]))
}

fn read_btime() -> u64 {
    let text = std::fs::read_to_string("/proc/stat").unwrap_or_default();
    for line in text.lines() {
        if line.starts_with("btime ") {
            return line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        }
    }
    0
}

const CLK_TCK: u64 = 100;

fn read_proc_info(
    pid: u32,
    mem_total_kb: u64,
    boot_time_secs: u64,
    uid_map: &HashMap<u32, String>,
) -> Option<Value> {
    let clk_tck = CLK_TCK;
    let stat_path = format!("/proc/{}/stat", pid);
    let stat_text = std::fs::read_to_string(&stat_path).ok()?;

    // Parse /proc/PID/stat — comm is in parens, may contain spaces
    let comm_start = stat_text.find('(')?;
    let comm_end = stat_text.rfind(')')?;
    let comm = &stat_text[comm_start + 1..comm_end];
    let rest: Vec<&str> = stat_text[comm_end + 2..].split_whitespace().collect();
    // rest[0] = state, rest[1] = ppid, ...
    // rest[11] = utime, rest[12] = stime, rest[19] = num_threads, rest[21] = starttime
    if rest.len() < 22 {
        return None;
    }

    let state = rest[0];
    let utime: u64 = rest[11].parse().unwrap_or(0);
    let stime: u64 = rest[12].parse().unwrap_or(0);
    let num_threads: u32 = rest[19].parse().unwrap_or(0);
    let starttime: u64 = rest[21].parse().unwrap_or(0);

    // CPU pct: we'd need two samples; for stat snapshot, approximate with total
    // as a lightweight measure (not accurate without two-sample diff, but acceptable)
    let cpu_total = utime + stime;
    let elapsed_ticks = {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let proc_start = boot_time_secs + starttime / clk_tck;
        ((now.saturating_sub(proc_start)) * clk_tck).max(1)
    };
    let cpu_pct = round2(100.0 * cpu_total as f64 / elapsed_ticks as f64);

    // Memory from /proc/PID/status
    let status_path = format!("/proc/{}/status", pid);
    let status_text = std::fs::read_to_string(&status_path).unwrap_or_default();
    let vm_rss_kb: u64 = parse_status_field(&status_text, "VmRSS").unwrap_or(0);
    let mem_rss_mb = vm_rss_kb / 1024;
    let mem_pct = round2(100.0 * vm_rss_kb as f64 / mem_total_kb as f64);

    // User: parse Uid from status, look up name from pre-built map
    let uid_num = parse_status_field(&status_text, "Uid").unwrap_or(0) as u32;
    let user = uid_map.get(&uid_num).cloned().unwrap_or_else(|| uid_num.to_string());

    // FDSize: current fd table allocation size from already-read status (avoids per-PID readdir).
    let open_files = parse_status_field(&status_text, "FDSize").unwrap_or(0);

    // Start time as rfc3339 approximation
    let start_unix = boot_time_secs + starttime / clk_tck;
    let start_time = DateTime::from_timestamp(start_unix as i64, 0)
        .map(|dt: DateTime<Utc>| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());

    Some(json!({
        "pid":        pid,
        "name":       comm,
        "user":       user,
        "cpu_pct":    cpu_pct,
        "mem_pct":    mem_pct,
        "mem_rss_mb": mem_rss_mb,
        "state":      state,
        "threads":    num_threads,
        "open_files": open_files,
        "start_time": start_time,
    }))
}

fn parse_status_field(text: &str, key: &str) -> Option<u64> {
    for line in text.lines() {
        if line.starts_with(key) {
            return line.split_whitespace().nth(1).and_then(|s| s.parse().ok());
        }
    }
    None
}

fn parse_passwd() -> HashMap<u32, String> {
    let text = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(':').collect();
            if parts.len() >= 3 {
                let uid: u32 = parts[2].parse().ok()?;
                Some((uid, parts[0].to_string()))
            } else {
                None
            }
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────────────────────
// Network
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_network() -> Value {
    // /proc/net reads + sysfs per-interface — all blocking, bundle in one spawn_blocking
    let (connections, sockstat, interfaces) = tokio::task::spawn_blocking(|| {
        (collect_connections(), collect_sockstat(), collect_interfaces())
    })
    .await
    .unwrap_or_else(|_| (json!({}), json!({}), json!({})));

    json!({
        "connections": connections,
        "sockstat":    sockstat,
        "interfaces":  interfaces,
    })
}

fn collect_connections() -> Value {
    let mut established = 0u64;
    let mut time_wait = 0u64;
    let mut close_wait = 0u64;

    for path in &["/proc/net/tcp", "/proc/net/tcp6"] {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        for line in text.lines().skip(1) {
            // field at index 3 is the state in hex
            let parts: Vec<&str> = line.split_whitespace().collect();
            if let Some(state_hex) = parts.get(3) {
                match *state_hex {
                    "01" => established += 1,
                    "06" => time_wait += 1,
                    "08" => close_wait += 1,
                    _ => {}
                }
            }
        }
    }

    json!({
        "established": established,
        "time_wait":   time_wait,
        "close_wait":  close_wait,
    })
}

fn collect_sockstat() -> Value {
    let text = std::fs::read_to_string("/proc/net/sockstat").unwrap_or_default();
    let mut tcp_alloc = 0u64;
    let mut udp_inuse = 0u64;

    for line in text.lines() {
        if line.starts_with("TCP:") {
            // "TCP: inuse 4 orphan 0 tw 0 alloc 7 mem 1"
            let parts: Vec<&str> = line.split_whitespace().collect();
            for i in 0..parts.len().saturating_sub(1) {
                if parts[i] == "alloc" {
                    tcp_alloc = parts[i + 1].parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("UDP:") {
            let parts: Vec<&str> = line.split_whitespace().collect();
            for i in 0..parts.len().saturating_sub(1) {
                if parts[i] == "inuse" {
                    udp_inuse = parts[i + 1].parse().unwrap_or(0);
                }
            }
        }
    }

    json!({
        "tcp_alloc": tcp_alloc,
        "udp_inuse": udp_inuse,
    })
}

fn collect_interfaces() -> Value {
    let mut obj = serde_json::Map::new();
    let text = std::fs::read_to_string("/proc/net/dev").unwrap_or_default();
    for line in text.lines().skip(2) {
        if let Some((iface, _)) = line.split_once(':') {
            let iface = iface.trim();
            if iface == "lo" {
                continue;
            }
            let mtu = std::fs::read_to_string(format!("/sys/class/net/{}/mtu", iface))
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0);
            let operstate = std::fs::read_to_string(format!("/sys/class/net/{}/operstate", iface))
                .unwrap_or_default();
            let state = if operstate.trim().eq_ignore_ascii_case("up") { "UP" } else { "DOWN" };
            obj.insert(iface.to_string(), json!({ "state": state, "mtu": mtu }));
        }
    }
    Value::Object(obj)
}

// ──────────────────────────────────────────────────────────────────────────────
// Systemd
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_systemd() -> Value {
    let output = Command::new("systemctl")
        .args([
            "list-units",
            "--no-pager",
            "--no-legend",
            "--plain",
            "--all",
            "--state=failed,active",
            "--type=service",
        ])
        .output()
        .await;

    let text = match output {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => {
            String::from_utf8_lossy(&o.stdout).into_owned()
        }
        _ => {
            debug!("systemctl unavailable — returning empty systemd array");
            return json!([]);
        }
    };

    let mut units = Vec::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let unit = parts[0];
        let load = parts[1];
        let active = parts[2];
        let sub = parts[3];
        units.push(json!({
            "unit":     unit,
            "load":     load,
            "active":   active,
            "sub":      sub,
            "restarts": 0,
            "main_pid": null,
            "since":    "",
        }));
    }

    Value::Array(units)
}

// ──────────────────────────────────────────────────────────────────────────────
// Static State
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_static_state() -> Value {
    // /sys and /proc reads — bundle synchronous helpers in spawn_blocking
    let (selinux, sysctl, kernel_modules, cmdline) = tokio::task::spawn_blocking(|| {
        (collect_selinux(), collect_sysctl(), collect_kernel_modules(), collect_cmdline())
    })
    .await
    .unwrap_or_else(|_| (json!({}), json!({}), json!({}), json!({})));
    let ntp = collect_ntp().await;

    json!({
        "selinux":        selinux,
        "sysctl":         sysctl,
        "kernel_modules": kernel_modules,
        "cmdline":        cmdline,
        "ntp":            ntp,
    })
}

fn collect_selinux() -> Value {
    let mode = match std::fs::read_to_string("/sys/fs/selinux/enforce") {
        Ok(s) => match s.trim() {
            "1" => "enforcing",
            "0" => "permissive",
            _ => "disabled",
        },
        Err(_) => "disabled",
    };
    json!({ "mode": mode })
}

fn collect_sysctl() -> Value {
    let mappings = [
        ("vm.overcommit_memory", "/proc/sys/vm/overcommit_memory"),
        ("vm.swappiness", "/proc/sys/vm/swappiness"),
        ("kernel.panic", "/proc/sys/kernel/panic"),
        ("net.core.somaxconn", "/proc/sys/net/core/somaxconn"),
        ("net.ipv4.tcp_tw_reuse", "/proc/sys/net/ipv4/tcp_tw_reuse"),
    ];
    let mut obj = serde_json::Map::new();
    for (key, path) in &mappings {
        let val = std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "".to_string());
        obj.insert(key.to_string(), json!(val));
    }
    Value::Object(obj)
}

fn collect_kernel_modules() -> Value {
    let text = std::fs::read_to_string("/proc/modules").unwrap_or_default();
    let modules: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .take(50)
        .collect();
    json!(modules)
}

fn collect_cmdline() -> Value {
    let s = std::fs::read_to_string("/proc/cmdline")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    json!(s)
}

async fn collect_ntp() -> Value {
    // Try chronyc first
    if let Ok(out) = Command::new("chronyc").arg("tracking").output().await {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut reference = String::new();
            let mut offset_ms = 0.0f64;
            let mut stratum = 0u64;
            for line in text.lines() {
                if line.contains("Reference ID") {
                    if let Some(ip) = line.split('(').nth(1).and_then(|s| s.split(')').next()) {
                        reference = ip.to_string();
                    }
                } else if line.contains("Stratum") {
                    stratum = line.split(':').nth(1)
                        .and_then(|s| s.trim().parse().ok())
                        .unwrap_or(0);
                } else if line.contains("System time") || line.contains("Last offset") {
                    // "System time     :   0.000000100 seconds slow of NTP time"
                    if let Some(val) = line.split(':').nth(1) {
                        let num: f64 = val.split_whitespace().next()
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0.0);
                        offset_ms = round2(num * 1000.0);
                    }
                }
            }
            if !reference.is_empty() || stratum > 0 {
                return json!({ "reference": reference, "offset_ms": offset_ms, "stratum": stratum });
            }
        }
    }

    // Try ntpq
    if let Ok(out) = Command::new("ntpq").arg("-p").output().await {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines().skip(2) {
                if line.starts_with('*') || line.starts_with('+') {
                    let parts: Vec<&str> = line[1..].split_whitespace().collect();
                    if parts.len() >= 9 {
                        let reference = parts[0].to_string();
                        let stratum: u64 = parts[2].parse().unwrap_or(0);
                        let offset_ms: f64 = parts[8].parse().unwrap_or(0.0);
                        return json!({ "reference": reference, "offset_ms": offset_ms, "stratum": stratum });
                    }
                }
            }
        }
    }

    debug!("NTP commands unavailable");
    Value::Null
}

// ──────────────────────────────────────────────────────────────────────────────
// Config
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_config() -> Value {
    // /etc 파일 읽기 — 실제 디스크 파일: spawn_blocking으로 executor 스레드 보호
    let files_obj = tokio::task::spawn_blocking(|| {
        let config_files = ["/etc/sysctl.conf", "/etc/hosts", "/etc/hostname"];
        let mut map = serde_json::Map::new();
        const MAX_FILE_BYTES: usize = 4096;
        for path in &config_files {
            match std::fs::read_to_string(path) {
                Ok(text) => {
                    let truncated = if text.len() > MAX_FILE_BYTES {
                        text[..MAX_FILE_BYTES].to_string()
                    } else {
                        text
                    };
                    map.insert(path.to_string(), json!(truncated));
                }
                Err(_) => {}
            }
        }
        map
    })
    .await
    .unwrap_or_default();

    let packages = collect_packages().await;

    json!({
        "files":    Value::Object(files_obj),
        "packages": packages,
    })
}

async fn collect_packages() -> Value {
    // Try dpkg first
    if let Ok(out) = Command::new("dpkg-query")
        .args(["-W", "-f=${Package}\t${Version}\n"])
        .output()
        .await
    {
        if out.status.success() || !out.stdout.is_empty() {
            let text = String::from_utf8_lossy(&out.stdout);
            let pkgs: Vec<(&str, &str)> = text
                .lines()
                .filter_map(|l| {
                    let mut it = l.splitn(2, '\t');
                    Some((it.next()?, it.next()?))
                })
                .collect();
            let total = pkgs.len();
            let sample: Vec<Value> = pkgs
                .iter()
                .take(5)
                .map(|(n, v)| json!({ "name": n, "version": v }))
                .collect();
            return json!({ "total": total, "sample": sample });
        }
    }

    // Try rpm
    if let Ok(out) = Command::new("rpm")
        .args(["-qa", "--qf", "%{NAME}\t%{VERSION}\n"])
        .output()
        .await
    {
        if out.status.success() || !out.stdout.is_empty() {
            let text = String::from_utf8_lossy(&out.stdout);
            let pkgs: Vec<(&str, &str)> = text
                .lines()
                .filter_map(|l| {
                    let mut it = l.splitn(2, '\t');
                    Some((it.next()?, it.next()?))
                })
                .take(200)
                .collect();
            let total = pkgs.len();
            let sample: Vec<Value> = pkgs
                .iter()
                .take(5)
                .map(|(n, v)| json!({ "name": n, "version": v }))
                .collect();
            return json!({ "total": total, "sample": sample });
        }
    }

    debug!("No package manager available");
    json!({ "total": 0, "sample": [] })
}

// ──────────────────────────────────────────────────────────────────────────────
// Hardware
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_hardware() -> Value {
    // /proc/cpuinfo, /proc/meminfo, /sys/block reads — bundle in spawn_blocking
    let (cpu, memory, disks) = tokio::task::spawn_blocking(|| {
        (collect_hw_cpu(), collect_hw_memory(), collect_hw_disks())
    })
    .await
    .unwrap_or_else(|_| (json!({}), json!({}), json!([])));
    let pci = collect_pci().await;

    json!({
        "cpu":    cpu,
        "memory": memory,
        "disks":  disks,
        "pci":    pci,
    })
}

fn collect_hw_cpu() -> Value {
    let text = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let mut model = String::new();
    let mut physical_ids = std::collections::HashSet::new();
    let mut cores_per_socket = 0u64;
    let mut threads = 0u64;

    for line in text.lines() {
        if line.starts_with("model name") && model.is_empty() {
            model = line.split(':').nth(1).map(|s| s.trim().to_string()).unwrap_or_default();
        } else if line.starts_with("physical id") {
            if let Some(id) = line.split(':').nth(1).map(|s| s.trim().to_string()) {
                physical_ids.insert(id);
            }
        } else if line.starts_with("cpu cores") && cores_per_socket == 0 {
            cores_per_socket = line.split(':').nth(1)
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
        } else if line.starts_with("processor") {
            threads += 1;
        }
    }

    let sockets = if physical_ids.is_empty() { 1 } else { physical_ids.len() as u64 };

    json!({
        "model":            model,
        "sockets":          sockets,
        "cores_per_socket": cores_per_socket,
        "threads":          threads,
    })
}

fn collect_hw_memory() -> Value {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let total_mb: u64 = text.lines()
        .find(|l| l.starts_with("MemTotal:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u64>().ok())
        .map(|kb| kb / 1024)
        .unwrap_or(0);
    json!({ "total_mb": total_mb })
}

fn collect_hw_disks() -> Value {
    let sys_block = match std::fs::read_dir("/sys/block") {
        Ok(d) => d,
        Err(_) => return json!([]),
    };

    let mut disks = Vec::new();
    for entry in sys_block.flatten() {
        let dev = entry.file_name().to_string_lossy().into_owned();
        if dev.starts_with("loop") || dev.starts_with("ram") || dev.starts_with("dm-") {
            continue;
        }

        let model = std::fs::read_to_string(format!("/sys/block/{}/device/model", dev))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let size_gb = std::fs::read_to_string(format!("/sys/block/{}/size", dev))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|sectors| sectors * 512 / 1_000_000_000)
            .unwrap_or(0);

        disks.push(json!({
            "dev":          dev,
            "model":        model,
            "size_gb":      size_gb,
            "smart_status": "UNKNOWN",
        }));
    }

    Value::Array(disks)
}

async fn collect_pci() -> Value {
    let output = Command::new("lspci").output().await;
    let text = match output {
        Ok(o) if o.status.success() || !o.stdout.is_empty() => {
            String::from_utf8_lossy(&o.stdout).into_owned()
        }
        _ => {
            debug!("lspci unavailable");
            return json!([]);
        }
    };

    let devices: Vec<Value> = text
        .lines()
        .take(20)
        .filter_map(|line| {
            // "0000:00:01.0 Class Device: Description"
            // skip first field (address), then class: device
            let rest = line.splitn(2, ' ').nth(1)?;
            let (class, device) = rest.split_once(':')?;
            Some(json!({
                "class":  class.trim(),
                "device": device.trim(),
            }))
        })
        .collect();

    Value::Array(devices)
}

// ──────────────────────────────────────────────────────────────────────────────
// Logs (SOS only)
// ──────────────────────────────────────────────────────────────────────────────

pub async fn collect_logs(log_paths: &[String]) -> Value {
    let log_paths = log_paths.to_vec();
    tokio::task::spawn_blocking(move || {
        use crate::dedup::window::DedupWindow;
        use crate::normalize::{fields, severity, tokens};
        use chrono::Datelike;
        use std::io::BufRead;

        let cutoff: DateTime<Utc> = Utc::now() - Duration::hours(4);
        let max_events = 500usize;
        let year = Utc::now().year();

        fn parse_syslog_ts(line: &str, year: i32) -> Option<DateTime<Utc>> {
            use chrono::{NaiveDateTime, TimeZone};
            let mut it = line.split_whitespace();
            let month = it.next()?;
            let day = it.next()?;
            let time = it.next()?;
            let dt_str = format!("{} {} {} {}", year, month, day, time);
            let mut dt = NaiveDateTime::parse_from_str(&dt_str, "%Y %b %d %H:%M:%S")
                .ok()
                .and_then(|ndt| Utc.from_local_datetime(&ndt).single())?;
            // Year-boundary fix: if parsed date is more than a day in the future, retry with year-1
            if dt > Utc::now() + Duration::days(1) {
                let dt_str2 = format!("{} {} {} {}", year - 1, month, day, time);
                if let Some(prev) = NaiveDateTime::parse_from_str(&dt_str2, "%Y %b %d %H:%M:%S")
                    .ok()
                    .and_then(|ndt| Utc.from_local_datetime(&ndt).single())
                {
                    dt = prev;
                }
            }
            Some(dt)
        }

        // Secondary parser for ISO-8601 / RFC-3339 format (rsyslog, journald text export)
        // e.g. "2024-05-08T04:41:04+00:00 hostname proc: message"
        fn parse_iso_ts(line: &str) -> Option<DateTime<Utc>> {
            use chrono::DateTime as ChronoDateTime;
            let token = line.split_whitespace().next()?;
            ChronoDateTime::parse_from_rfc3339(token)
                .ok()
                .map(|dt| dt.with_timezone(&Utc))
        }

        let mut window = DedupWindow::new(14400u64, 50000);
        let mut all_events: Vec<crate::envelope::DedupEvent> = Vec::new();

        'outer: for path in &log_paths {
            let source = if path.contains("auth") || path.contains("secure") {
                "file.auth"
            } else {
                "file.syslog"
            }.to_string();

            let file = match std::fs::File::open(path) {
                Ok(f) => f,
                Err(_) => {
                    debug!("log path not readable: {}", path);
                    continue;
                }
            };

            for line in std::io::BufReader::new(file).lines() {
                let line = match line { Ok(l) => l, Err(_) => continue };
                if line.is_empty() { continue; }
                let ts = parse_syslog_ts(&line, year).or_else(|| parse_iso_ts(&line));
                if let Some(ts) = ts {
                    if ts < cutoff { continue; }
                    let stripped = tokens::strip_syslog_prefix(&line);
                    let template = tokens::normalize(stripped);
                    let fields_map = fields::extract_fields(stripped);
                    let sev = severity::finalize("info", stripped);

                    let fingerprint = {
                        use std::hash::Hasher as _;
                        let mut h = xxhash_rust::xxh3::Xxh3::new();
                        h.write(source.as_bytes());
                        h.write(b":");
                        h.write(template.as_bytes());
                        h.finish()
                    };

                    if let Some(emitted) = window.push(
                        fingerprint, source.clone(), sev.to_string(),
                        "system.general".to_string(), template, line, ts, fields_map,
                    ) {
                        all_events.push(emitted);
                        if all_events.len() >= max_events { break 'outer; }
                    }
                }
            }
        }

        for ev in window.flush_all() {
            all_events.push(ev);
            if all_events.len() >= max_events { break; }
        }

        all_events.sort_by(|a, b| b.ts_first.cmp(&a.ts_first));
        all_events.truncate(max_events);

        match serde_json::to_value(all_events) {
            Ok(v) => v,
            Err(_) => json!([]),
        }
    })
    .await
    .unwrap_or(json!([]))
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper
// ──────────────────────────────────────────────────────────────────────────────

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_partition_whole_disk_false() {
        assert!(!is_partition("sda"));
        assert!(!is_partition("vdb"));
        assert!(!is_partition("hda"));
    }

    #[test]
    fn is_partition_legacy_partition_true() {
        assert!(is_partition("sda1"));
        assert!(is_partition("sdb2"));
        assert!(is_partition("vdb2"));
        assert!(is_partition("hda3"));
    }

    #[test]
    fn is_partition_nvme_whole_disk_false() {
        assert!(!is_partition("nvme0n1"));
        assert!(!is_partition("nvme1n2"));
    }

    #[test]
    fn is_partition_nvme_partition_true() {
        assert!(is_partition("nvme0n1p1"));
        assert!(is_partition("nvme1n2p3"));
    }

    #[test]
    fn is_partition_loop_device_false() {
        assert!(!is_partition("loop0"));
        assert!(!is_partition("loop10"));
    }
}

