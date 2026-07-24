//! GET /raw — 원문(raw) 로그 드릴다운 엔드포인트.
//!
//! 요약(`/flush`·`/trigger-sos`)으로 개요를 본 뒤, **상세 대처가 필요할 때** 최근 원문 로그를
//! 같은 :9100에서 on-demand로 당긴다. dedup을 거치지 않은 raw 라인을 그대로 반환한다.
//!
//! **bounded** (전량 아님): `since`(기본 1h, 최대 24h)~`until`(선택, 창 최근쪽 상한) 범위
//! + `max_mb`(기본 10, 하드캡 30) 상한. 상한 초과 시 라인 경계에서 자르고 `X-Raw-Truncated: true` 표시.
//!
//! 소스: 파일(syslog/auth/kernel — 파서가 아는 로그 경로 + 로그로테이션 `.1`·`.2.gz` 포함)
//! + journald(`journalctl --since [--until]`). 인증은 기존 `SOS_INBOUND_TOKEN` 재사용(새 필수 토큰
//! 없음), rate-limit은 `collection_rate` 공유.
//!
//! 사용: `GET /raw?since=2h&until=1h&sources=syslog,auth,kernel,journald&max_mb=10`
//!   (until 생략 시 now까지. 특정 사건 창만 좁혀 받을 때 since~until로 지정)
//!
//! 주의: 도커 파서는 호스트 저널이 컨테이너에 마운트돼 있어야 journald 소스가 동작한다
//! (docker-compose.yml에 `/var/log/journal`·`/run/log/journal` 마운트 존재). journalctl은
//! `--merge`로 machine-id에 무관하게 마운트된 저널을 읽는다 — 컨테이너 machine-id는 호스트와
//! 달라서 --merge가 없으면 "-- No entries --"가 된다. 저널 부재(rsyslog-only)면 조용히 생략.

use super::{check_auth, collect, InboundState};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Datelike, Duration, Utc};
use flate2::{write::GzEncoder, Compression};
use serde::Deserialize;
use std::io::Write as _;
use std::sync::Arc;
use tracing::{info, warn};

const DEFAULT_MB: u64 = 10;
const MAX_MB_HARD: u64 = 30;
const DEFAULT_SINCE_HOURS: i64 = 1;
const MAX_SINCE_HOURS: i64 = 24;

#[derive(Deserialize)]
pub struct RawParams {
    since: Option<String>,
    until: Option<String>,
    sources: Option<String>,
    max_mb: Option<u64>,
}

/// "30s" / "15m" / "2h" / "1d" → Duration. 잘못된 형식이면 None.
fn parse_since(s: &str) -> Option<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let n: i64 = s[..split].parse().ok()?;
    match &s[split..] {
        "s" => Some(Duration::seconds(n)),
        "m" => Some(Duration::minutes(n)),
        "h" => Some(Duration::hours(n)),
        "d" => Some(Duration::days(n)),
        _ => None,
    }
}

pub async fn handle_raw(
    State(st): State<Arc<InboundState>>,
    headers: HeaderMap,
    Query(p): Query<RawParams>,
) -> Response {
    // ── 인증 (SOS 토큰 재사용) ──
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !st.sos_token.is_empty() && !check_auth(auth, &st.sos_token) {
        warn!("/raw 인증 실패 — 401");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // ── rate limit (collection_rate 공유) ──
    {
        let mut rl = st.collection_rate.lock().await;
        if !rl.try_consume() {
            let retry = rl.retry_after_secs();
            warn!(retry_after = retry, "/raw rate-limit 초과");
            return (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, retry.to_string())])
                .into_response();
        }
    }

    // ── 파라미터 → bounded ──
    let now = Utc::now();
    let since = p.since.as_deref().and_then(parse_since).unwrap_or_else(|| Duration::hours(DEFAULT_SINCE_HOURS));
    let since = since.clamp(Duration::seconds(1), Duration::hours(MAX_SINCE_HOURS));
    let cutoff = now - since;
    // until: 창의 '최근쪽' 상한(예: since=2h&until=1h → [2h전, 1h전] 창). since보다 더 최근(값이 작음)
    // 이고 0 이상일 때만 유효. 없거나 부적합이면 None(상한 없음 = now까지).
    let until_cutoff: Option<DateTime<Utc>> = p.until.as_deref()
        .and_then(parse_since)
        .filter(|u| *u < since && *u >= Duration::zero())
        .map(|u| now - u);
    let max_bytes = (p.max_mb.unwrap_or(DEFAULT_MB).clamp(1, MAX_MB_HARD) * 1024 * 1024) as usize;

    let sources_raw = p.sources.as_deref().unwrap_or("syslog,auth,kernel,journald");
    let sources: Vec<String> = sources_raw
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    let want = |name: &str| sources.iter().any(|s| s == name);

    // ── 읽을 파일 목록 (파서가 아는 log_paths를 분류 + kern.log 명시 보강) ──
    let mut files: Vec<(String, String)> = Vec::new();
    for path in &st.log_paths {
        let is_auth = path.contains("auth") || path.contains("secure");
        let is_kern = path.contains("kern");
        let (label, wanted) = if is_auth {
            ("auth", want("auth"))
        } else if is_kern {
            ("kernel", want("kernel"))
        } else {
            ("syslog", want("syslog"))
        };
        if wanted {
            files.push((label.to_string(), path.clone()));
        }
    }
    if want("kernel")
        && std::path::Path::new("/var/log/kern.log").exists()
        && !files.iter().any(|(_, p)| p == "/var/log/kern.log")
    {
        files.push(("kernel".to_string(), "/var/log/kern.log".to_string()));
    }

    // ── 파일 읽기 (blocking — executor 보호) ──
    let year = Utc::now().year();
    let files_moved = files.clone();
    let (mut text, mut used, mut truncated) =
        tokio::task::spawn_blocking(move || read_files(&files_moved, cutoff, until_cutoff, max_bytes, year))
            .await
            .unwrap_or_else(|_| (String::new(), 0, false));

    // ── journald (남은 예산 안에서) ──
    if want("journald") && !truncated && used < max_bytes {
        if let Some((jtext, jused, jtrunc)) = read_journald(cutoff, until_cutoff, max_bytes - used).await {
            if !jtext.is_empty() {
                let hdr = "==> journald <==\n";
                text.push_str(hdr);
                text.push_str(&jtext);
                used += hdr.len() + jused;
                truncated = truncated || jtrunc;
            }
        }
    }

    let lines = text.lines().count();
    info!(bytes = used, lines, truncated, window_since = %cutoff.to_rfc3339(), "/raw 응답");

    // ── gzip text/plain ──
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(6));
    if enc.write_all(text.as_bytes()).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let body = match enc.finish() {
        Ok(v) => v,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    Response::builder()
        .status(200)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header("content-encoding", "gzip")
        .header("x-raw-bytes", used.to_string())
        .header("x-raw-lines", lines.to_string())
        .header("x-raw-truncated", truncated.to_string())
        .header("x-raw-window", format!("{}/{}", cutoff.to_rfc3339(), until_cutoff.unwrap_or(now).to_rfc3339()))
        .body(axum::body::Body::from(body))
        .unwrap()
}

/// 파일들을 [cutoff, until_cutoff] 창의 라인만 시간순으로 읽어 max_bytes까지 모은다.
/// 로그로테이션(.1 · .2.gz 등)도 오래된 것부터 이어 읽어, 24h 창 안에서 회전된 분까지 포함한다.
/// 반환: (본문, 사용 바이트, 상한도달로 잘렸는지).
fn read_files(
    files: &[(String, String)],
    cutoff: DateTime<Utc>,
    until_cutoff: Option<DateTime<Utc>>,
    max_bytes: usize,
    year: i32,
) -> (String, usize, bool) {
    use std::io::BufRead;
    // 회전 후보 (오래된→최신). logrotate 기본: name.N.gz … name.1 … name(현재)
    const SUFFIXES: &[&str] = &[".3.gz", ".2.gz", ".1.gz", ".1", ""];
    let mut out = String::new();
    let mut used = 0usize;
    let mut truncated = false;

    'src: for (label, path) in files {
        // 존재하는 회전 후보만 오래된 순으로. mtime이 cutoff 이전이면(마지막 기록이 창 밖) 통째로 스킵.
        let mut cands: Vec<(std::path::PathBuf, bool)> = Vec::new();
        for suf in SUFFIXES {
            let p = std::path::PathBuf::from(format!("{path}{suf}"));
            if !p.exists() {
                continue;
            }
            if let Ok(mt) = p.metadata().and_then(|m| m.modified()) {
                if DateTime::<Utc>::from(mt) < cutoff {
                    continue;
                }
            }
            cands.push((p, suf.ends_with(".gz")));
        }
        if cands.is_empty() {
            continue;
        }
        let header = format!("==> {label} ({path}) <==\n");
        if used + header.len() > max_bytes {
            truncated = true;
            break;
        }
        out.push_str(&header);
        used += header.len();

        for (cand, is_gz) in cands {
            let f = match std::fs::File::open(&cand) {
                Ok(f) => f,
                Err(_) => continue,
            };
            let reader: Box<dyn BufRead> = if is_gz {
                Box::new(std::io::BufReader::new(flate2::read::GzDecoder::new(f)))
            } else {
                Box::new(std::io::BufReader::new(f))
            };
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => continue,
                };
                if line.is_empty() {
                    continue;
                }
                // ts 파싱되면 [cutoff, until] 밖은 스킵. 파싱 안 되는 줄(멀티라인 등)은 포함.
                if let Some(ts) = collect::parse_syslog_ts(&line, year).or_else(|| collect::parse_iso_ts(&line)) {
                    if ts < cutoff {
                        continue;
                    }
                    if let Some(u) = until_cutoff {
                        if ts > u {
                            continue;
                        }
                    }
                }
                let need = line.len() + 1;
                if used + need > max_bytes {
                    truncated = true;
                    break 'src;
                }
                out.push_str(&line);
                out.push('\n');
                used += need;
            }
        }
    }
    (out, used, truncated)
}

/// journalctl --since <cutoff> [--until <until>] 로 저널을 받아 max_bytes까지 반환.
/// journalctl 부재/실패(컨테이너 미마운트 등) 시 None(해당 소스 생략).
async fn read_journald(
    cutoff: DateTime<Utc>,
    until_cutoff: Option<DateTime<Utc>>,
    max_bytes: usize,
) -> Option<(String, usize, bool)> {
    use tokio::process::Command;
    // 서버는 통상 UTC. --since/--until 은 시스템 로컬 시간 해석(UTC 호스트면 일치).
    let since = cutoff.format("%Y-%m-%d %H:%M:%S").to_string();
    // journalctl 자체 메모리 폭주 방지용 라인 상한(바이트 예산에서 대략 유도).
    let nlines = (max_bytes / 80).clamp(1000, 300_000).to_string();
    // --merge: machine-id 상관없이 마운트된 모든 저널을 읽는다. 도커 파서는 컨테이너 machine-id가
    // 호스트와 달라, 이게 없으면 마운트된 호스트 저널(/var/log/journal/<host-id>)을 못 찾고
    // "-- No entries --"가 된다. bare-metal에선 로컬 저널만 있어 동작 동일.
    let mut args: Vec<String> = vec![
        "--since".into(), since, "--merge".into(), "--no-pager".into(),
        "-o".into(), "short-iso".into(), "-n".into(), nlines,
    ];
    if let Some(u) = until_cutoff {
        args.push("--until".into());
        args.push(u.format("%Y-%m-%d %H:%M:%S").to_string());
    }
    let out = Command::new("journalctl").args(&args).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    if s.len() <= max_bytes {
        let owned = s.into_owned();
        let len = owned.len();
        Some((owned, len, false))
    } else {
        let cut = s[..max_bytes].rfind('\n').unwrap_or(max_bytes);
        Some((s[..cut].to_string(), cut, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinator::FlushSignal;
    use crate::inbound::{flush::RateLimiter, InboundState};
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt as _;

    fn test_spool() -> Arc<crate::transport::spool::Spool> {
        use std::time::{SystemTime, UNIX_EPOCH};
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
        let dir = std::env::temp_dir().join(format!("spool_raw_{}_{}", std::process::id(), n));
        Arc::new(crate::transport::spool::Spool::new(dir.to_str().unwrap(), 10).unwrap())
    }

    fn make_state(token: &str, rate: u32) -> Arc<InboundState> {
        make_state_paths(token, rate, vec![])
    }

    fn make_state_paths(token: &str, rate: u32, log_paths: Vec<String>) -> Arc<InboundState> {
        let (tx, _rx) = tokio::sync::mpsc::channel::<FlushSignal>(4);
        Arc::new(InboundState {
            flush_tx: tx,
            flush_token: String::new(),
            flush_rate: tokio::sync::Mutex::new(RateLimiter::new(100)),
            flush_in_flight: tokio::sync::Mutex::new(false),
            response_timeout_secs: 1,
            serialize_strategy: "reject".to_string(),
            stat_token: String::new(),
            sos_token: token.to_string(),
            collection_rate: tokio::sync::Mutex::new(RateLimiter::new(rate)),
            envelope_size_limit_bytes: 0,
            host: "h".to_string(),
            host_id: "hid".to_string(),
            boot_id: "bid".to_string(),
            static_state_enabled: true,
            log_paths,
            drain_state: crate::inbound::drain::DrainState::default(),
            spool: test_spool(),
            transport_cfg: crate::config::TransportConfig::default(),
        })
    }

    fn app(state: Arc<InboundState>) -> Router {
        Router::new().route("/raw", get(handle_raw)).with_state(state)
    }

    #[test]
    fn parse_since_units() {
        assert_eq!(parse_since("30s"), Some(Duration::seconds(30)));
        assert_eq!(parse_since("15m"), Some(Duration::minutes(15)));
        assert_eq!(parse_since("2h"), Some(Duration::hours(2)));
        assert_eq!(parse_since("1d"), Some(Duration::days(1)));
        assert_eq!(parse_since("bogus"), None);
        assert_eq!(parse_since("100"), None); // 단위 없음
    }

    #[tokio::test]
    async fn unauthorized_returns_401() {
        let resp = app(make_state("secret", 600))
            .oneshot(Request::get("/raw").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_token_returns_200() {
        // log_paths 비었고 journald도 테스트 환경에선 없거나 실패 → 빈 본문이라도 200.
        let (k, v) = ("Authorization", "Bearer tok".to_string());
        let resp = app(make_state("tok", 600))
            .oneshot(Request::get("/raw?sources=syslog,auth").header(k, v).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("content-encoding").unwrap(), "gzip");
        assert!(resp.headers().get("x-raw-truncated").is_some());
    }

    #[tokio::test]
    async fn empty_token_bypasses_auth() {
        let resp = app(make_state("", 600))
            .oneshot(Request::get("/raw?sources=syslog").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn exhausted_rate_limit_returns_429() {
        let resp = app(make_state("", 0))
            .oneshot(Request::get("/raw").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn returns_recent_lines_and_filters_old() {
        use flate2::read::GzDecoder;
        use std::io::{Read as _, Write as _};

        let now = Utc::now();
        let ts = now.format("%b %e %H:%M:%S").to_string(); // 지금 시각 syslog 포맷
        let dir = std::env::temp_dir().join(format!("rawtest_{}_{}", std::process::id(), now.timestamp_micros()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("syslog");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{ts} testhost demo: HELLO_RAW_MARKER recent").unwrap();
        writeln!(f, "Jan  1 00:00:00 testhost demo: OLD_LINE_MARKER stale").unwrap();
        drop(f);

        let state = make_state_paths("", 600, vec![path.to_str().unwrap().to_string()]);
        let resp = app(state)
            .oneshot(Request::get("/raw?sources=syslog&since=2h").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let mut txt = String::new();
        GzDecoder::new(bytes.as_ref()).read_to_string(&mut txt).unwrap();

        assert!(txt.contains("HELLO_RAW_MARKER"), "최근 라인은 포함되어야 함:\n{txt}");
        assert!(!txt.contains("OLD_LINE_MARKER"), "cutoff 이전 라인은 제외되어야 함:\n{txt}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn until_excludes_too_recent() {
        use flate2::read::GzDecoder;
        use std::io::{Read as _, Write as _};

        let now = Utc::now();
        // since=3h & until=1h → 창 [3h전, 1h전]. mid(90분전)=창 안, now=창보다 최근=제외.
        let mid_ts = (now - Duration::minutes(90)).format("%b %e %H:%M:%S").to_string();
        let now_ts = now.format("%b %e %H:%M:%S").to_string();
        let dir = std::env::temp_dir().join(format!("rawtest_until_{}_{}", std::process::id(), now.timestamp_micros()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("syslog");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "{mid_ts} testhost demo: MID_MARKER inside window").unwrap();
        writeln!(f, "{now_ts} testhost demo: NOW_MARKER too recent").unwrap();
        drop(f);

        let state = make_state_paths("", 600, vec![path.to_str().unwrap().to_string()]);
        let resp = app(state)
            .oneshot(Request::get("/raw?sources=syslog&since=3h&until=1h").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let mut txt = String::new();
        GzDecoder::new(bytes.as_ref()).read_to_string(&mut txt).unwrap();

        assert!(txt.contains("MID_MARKER"), "창 안 라인 포함:\n{txt}");
        assert!(!txt.contains("NOW_MARKER"), "until보다 최근 라인 제외:\n{txt}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
