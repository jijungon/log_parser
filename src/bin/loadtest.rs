//! 부하 테스트 도구 (프로덕션 파서와 **별개** 바이너리, 같은 코드 재사용).
//!
//! 파일도 Vector도 없이 메모리에서 대용량 로그를 **스트리밍 생성**해 실제
//! `process::process_line`(strip→정규화→severity→fingerprint→dedup)에 그대로 통과시키고,
//! 그 결과를 실제 `CycleState`로 envelope에 조립한 뒤 실제 `transport`로 직렬화·gzip·(선택)POST 한다.
//!
//! 측정: 파싱 처리량(lines/s·MB/s), dedup 압축비, peak RSS, 전송 바이트(gzip 후)·지연.
//!
//! 사용:
//!   loadtest --gb 100 [--endpoint http://host:8099/ingest] [--window-seconds 86400]
//!            [--lru-cap 50000] [--distinct 64] [--categories /etc/log_parser/categories.yaml]
//!   (lru-cap 기본 50000 = 프로덕션 agent.yaml과 동일)
//!
//! 주의1: cgroup 자가 격리는 프로덕션 바이너리 몫이라 여기선 적용되지 않는다(=미제한 처리량).
//! 5% CPU cap이 켜진 실환경 벽시계는 대략 이 수치의 ×20으로 외삽한다.
//!
//! 주의2: 모든 라인이 같은 타임스탬프라 dedup 창이 '시간으로' 비워지지 않는다(worst case).
//! 따라서 고유 패턴이 lru_cap을 넘으면 LRU폐기로 나타나는데, 이는 **메모리 상한을 지키려는
//! 백프레셔**(OOM 대신 오래된 패턴 폐기)를 증명한다. 실운영은 창이 30초마다 회전하므로,
//! '30초 안에' 고유 패턴이 lru_cap을 넘는 극단적 경우에만 폐기가 발생한다.

use anyhow::Result;
use log_parser::config::TransportConfig;
use log_parser::coordinator::cycle::CycleState;
use log_parser::dedup::window::DedupWindow;
use log_parser::normalize::categories::CategoryMatcher;
use log_parser::normalize::fields::{init_global, FieldExtractor};
use log_parser::{process, transport};
use std::time::Instant;

// 본문 템플릿 — 가변부({ip}/{pid}/{num}/{port})는 정규화가 지워 병합되고,
// 서비스명({svc})은 단어라 살아남아 '서로 다른 템플릿'을 만든다(카디널리티 제어).
const BODIES: &[&str] = &[
    "sshd[{pid}]: Accepted password for root from {ip} port {port} ssh2",
    "sshd[{pid}]: Failed password for invalid user admin from {ip} port {port} ssh2",
    "kernel: [{num}] Out of memory: Killed process {pid} ({svc})",
    "systemd[1]: Started {svc} daemon.",
    "systemd[1]: {svc}.service: Main process exited, code=exited status={num}",
    "nginx[{pid}]: {ip} - - GET /{svc}/api?id={num} HTTP/1.1 200 {num}",
    "{svc}[{pid}]: connection from {ip} closed after {num}ms",
    "kernel: TCP: request_sock_TCP: Possible SYN flooding on port {port}",
    "CRON[{pid}]: (root) CMD (/usr/bin/{svc} --run {num})",
    "dockerd[{pid}]: level=error msg=\"container {svc} exited code={num}\"",
    "postfix/smtpd[{pid}]: connect from {svc}[{ip}]",
    "kernel: EXT4-fs (vda1): mounted filesystem, opts {svc}",
];

// 서비스명 풀 — 실제로 다른 정규화 템플릿을 만드는 유일한 축. 풀 크기 × BODIES ≈ 고유 이벤트 수.
const SVCS: &[&str] = &[
    "auth", "billing", "cache", "cart", "catalog", "checkout", "comment", "coupon", "delivery",
    "email", "feed", "gateway", "geo", "graph", "image", "index", "inventory", "invoice", "job",
    "kafka", "ledger", "login", "mailer", "media", "metrics", "notify", "order", "payment",
    "profile", "push", "queue", "rank", "recommend", "redis", "report", "review", "router",
    "scheduler", "search", "session", "shipping", "sms", "stock", "stream", "sync", "tax",
    "thumb", "token", "trace", "upload", "user", "vault", "video", "wallet", "webhook", "worker",
    "auditlog", "cdn", "dns", "loadbal", "proxy", "storage", "telemetry", "vpn",
];

/// 서비스 워드를 buf에 직접 기록(할당 없음). 낮은 k는 읽기 좋은 SVCS,
/// 높은 k는 base-26 letters로 — 정규화가 안 지우는 단어라 '고유 템플릿'을 만든다.
fn write_tag(mut k: u64, buf: &mut String) {
    if (k as usize) < SVCS.len() {
        buf.push_str(SVCS[k as usize]);
        return;
    }
    k += 1;
    while k > 0 {
        buf.push((b'a' + (k % 26) as u8) as char);
        k /= 26;
    }
}

/// 라인 n번째를 syslog(RFC3164 유사) 한 줄로 생성. `distinct`는 svc 워드 풀 크기
/// (고유 정규화 템플릿 ≈ distinct × {svc} 포함 BODY 수). 길이 ~120–160B.
fn gen_line(n: u64, distinct: u64, buf: &mut String) {
    buf.clear();
    // 고정 헤더 — strip_syslog_prefix가 제거하는 부분
    buf.push_str("Jul 16 10:23:45 grafana ");
    let body = BODIES[(n as usize) % BODIES.len()];
    let k = (n / BODIES.len() as u64) % distinct.max(1);
    let ip = format!("10.{}.{}.{}", (n >> 16) & 255, (n >> 8) & 255, n & 255);
    let pid = (n % 65535) + 1;
    let num = n % 100_000;
    let port = 1024 + (n % 60_000);
    for tok in body.split_inclusive(|c| c == '}') {
        // 각 조각에서 플레이스홀더를 치환 (단순·의존성 없는 방식)
        if let Some(idx) = tok.find('{') {
            buf.push_str(&tok[..idx]);
            match &tok[idx..] {
                "{ip}" => buf.push_str(&ip),
                "{pid}" => buf.push_str(&pid.to_string()),
                "{num}" => buf.push_str(&num.to_string()),
                "{port}" => buf.push_str(&port.to_string()),
                "{svc}" => write_tag(k, buf),
                other => buf.push_str(other), // unknown → 그대로
            }
        } else {
            buf.push_str(tok);
        }
    }
}

/// peak RSS(KB) — Linux `/proc/self/status`의 VmHWM. 비-Linux면 None.
fn peak_rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmHWM:") {
            return rest.split_whitespace().next().and_then(|v| v.parse().ok());
        }
    }
    None
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1).cloned())
}

fn human_bytes(b: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut f = b as f64;
    let mut i = 0;
    while f >= 1024.0 && i < U.len() - 1 {
        f /= 1024.0;
        i += 1;
    }
    format!("{f:.1}{}", U[i])
}

#[tokio::main]
async fn main() -> Result<()> {
    let gb: f64 = arg("--gb").and_then(|s| s.parse().ok()).unwrap_or(10.0);
    let endpoint = arg("--endpoint").unwrap_or_default();
    let window_seconds: u64 = arg("--window-seconds").and_then(|s| s.parse().ok()).unwrap_or(86_400);
    // 기본값은 프로덕션 agent.yaml(dedup.lru_cap=50000)과 정렬 — 테스트가 실제 거동을 반영하도록.
    let lru_cap: usize = arg("--lru-cap").and_then(|s| s.parse().ok()).unwrap_or(50_000);
    // svc 워드 풀 크기 = 카디널리티 dial. 기본은 읽기 좋은 SVCS 풀 크기. 크게 주면 고유 템플릿↑.
    let distinct: u64 = arg("--distinct").and_then(|s| s.parse().ok()).unwrap_or(SVCS.len() as u64);
    let categories_path = arg("--categories").unwrap_or_default();

    // 필드 추출 전역 초기화 (미초기화 시 extract_fields가 동작 안 함)
    init_global(FieldExtractor::builtin());
    let categories = if categories_path.is_empty() {
        CategoryMatcher::fallback()
    } else {
        CategoryMatcher::load(&categories_path).unwrap_or_else(|e| {
            eprintln!("categories.yaml 로드 실패({e}) — fallback 사용");
            CategoryMatcher::fallback()
        })
    };

    let target_bytes = (gb * (1u64 << 30) as f64) as u64;
    eprintln!(
        "[loadtest] 목표 {gb} GB 생성·파싱 (window={window_seconds}s, lru_cap={lru_cap}, \
         distinct(svc풀)={distinct}, source=file.syslog, endpoint={})",
        if endpoint.is_empty() { "(전송 생략)" } else { &endpoint }
    );

    let base_ts = chrono::Utc::now();
    let mut window = DedupWindow::new(window_seconds, lru_cap);
    // 시간 만료로 방출된 이벤트 '수'만 센다(누적 저장 X — 고카디널리티에서 도구 메모리 폭발 방지).
    // LRU 폐기분은 window.total_evictions()로 따로 읽는다.
    let mut emitted_expired: u64 = 0;

    let mut in_lines: u64 = 0;
    let mut in_bytes: u64 = 0;
    let mut line = String::with_capacity(200);
    let mut next_mark: u64 = 5 << 30; // 5GB마다 진행 로그

    let t0 = Instant::now();
    while in_bytes < target_bytes {
        gen_line(in_lines, distinct, &mut line);
        in_bytes += line.len() as u64 + 1;
        in_lines += 1;
        if process::process_line(&mut window, &categories, &line, "file.syslog", "info", base_ts, &[])
            .is_some()
        {
            emitted_expired += 1;
        }
        if in_bytes >= next_mark {
            let secs = t0.elapsed().as_secs_f64();
            eprintln!(
                "  … {} · {:.0} lines/s · {}/s",
                human_bytes(in_bytes),
                in_lines as f64 / secs,
                human_bytes((in_bytes as f64 / secs) as u64),
            );
            next_mark += 5 << 30;
        }
    }
    let parse_elapsed = t0.elapsed();
    let lru_drops = window.total_evictions(); // lru_cap 초과로 폐기된 고유 패턴 수
    let flushed = window.flush_all();
    let cycle_events = flushed.len() as u64; // 현재 창 보유 = 1사이클 envelope(≤ lru_cap)
    let rss = peak_rss_kb();

    // ── envelope 조립: 한 사이클 분량(flush_all)만 = 실제 송출 단위(≤ lru_cap) ──
    let mut cyc = CycleState::new(1, "loadtest".into(), "hid".into(), "bid".into(), usize::MAX);
    for ev in flushed {
        cyc.push(ev);
    }
    let (envelope, _next_seq) = cyc.finalize(0, parse_elapsed.as_secs());

    // ── 전송 경로 (실제 transport: 직렬화 + gzip + 선택 POST) ──
    std::env::set_var("LOADTEST_TOKEN", "loadtest");
    let mut tcfg: TransportConfig = serde_json::from_str("{}")?;
    tcfg.endpoint = endpoint.clone();
    tcfg.tls_enabled = false;
    tcfg.token_env = "LOADTEST_TOKEN".into();
    let t = transport::create(&tcfg)?;

    let json = serde_json::to_vec(&envelope)?;
    let gz0 = Instant::now();
    let gz = t.compress(&json)?;
    let gz_elapsed = gz0.elapsed();

    let post = if endpoint.is_empty() {
        None
    } else {
        let p0 = Instant::now();
        match t.send(&envelope).await {
            Ok(()) => Some((true, p0.elapsed())),
            Err(e) => {
                eprintln!("[loadtest] 전송 실패(수신기 미기동?): {e}");
                Some((false, p0.elapsed()))
            }
        }
    };

    // ── 리포트 ──
    let secs = parse_elapsed.as_secs_f64();
    let distinct_total = emitted_expired + cycle_events + lru_drops;
    let ratio = if distinct_total > 0 { in_lines as f64 / distinct_total as f64 } else { 0.0 };
    println!("\n===== loadtest 결과 =====");
    println!(
        "입력  : {} · {} lines (avg {}B/line)",
        human_bytes(in_bytes),
        in_lines,
        if in_lines > 0 { in_bytes / in_lines } else { 0 }
    );
    println!(
        "파싱  : {:.1}s · {:.0} lines/s · {}/s{}",
        secs,
        in_lines as f64 / secs,
        human_bytes((in_bytes as f64 / secs) as u64),
        match rss {
            Some(kb) => format!(" · peak RSS {}", human_bytes(kb * 1024)),
            None => " · peak RSS n/a(비-Linux)".into(),
        }
    );
    println!(
        "dedup : {} lines → ~{} 고유 (만료방출 {} + 창보유 {} + LRU폐기 {}) · {:.0}:1",
        in_lines, distinct_total, emitted_expired, cycle_events, lru_drops, ratio
    );
    if lru_drops > 0 {
        println!(
            "  ⚠ LRU폐기 {} — 카디널리티가 lru_cap({})/window({}s) 상한 초과 → 오래된 고유패턴 폐기(메모리 상한 유지). \
             실운영 30s 창에선 '30초당' 고유패턴이 lru_cap 넘을 때만 발생.",
            lru_drops, lru_cap, window_seconds
        );
    }
    println!(
        "전송  : envelope {} → gzip {} ({:.1}:1) · gzip {:.0}ms{}",
        human_bytes(json.len() as u64),
        human_bytes(gz.len() as u64),
        json.len() as f64 / gz.len().max(1) as f64,
        gz_elapsed.as_secs_f64() * 1000.0,
        match post {
            Some((true, d)) => format!(" · POST {:.0}ms ✓", d.as_secs_f64() * 1000.0),
            Some((false, d)) => format!(" · POST {:.0}ms ✗(수신기 확인)", d.as_secs_f64() * 1000.0),
            None => " · POST 생략(--endpoint 없음)".into(),
        }
    );
    // 100GB 외삽 (선형)
    if in_bytes > 0 {
        let scale = (100.0 * (1u64 << 30) as f64) / in_bytes as f64;
        let full_secs = secs * scale;
        println!(
            "외삽  : 100GB → 파싱 CPU ≈{:.0}min (uncapped) · 5% cap이면 ≈{:.1}h",
            full_secs / 60.0,
            full_secs * 20.0 / 3600.0
        );
    }
    Ok(())
}
