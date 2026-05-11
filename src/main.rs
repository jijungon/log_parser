mod config;
mod coordinator;
mod dedup;
mod envelope;
mod inbound;
mod normalize;
mod pipeline;
mod platform;
mod transport;

use anyhow::{Context, Result};
use std::sync::Arc;
use tracing::info;

const DEFAULT_CONFIG: &str = "/etc/log_parser/agent.yaml";
const DEFAULT_CATEGORIES: &str = "/etc/log_parser/categories.yaml";
const RUNTIME_VECTOR_CONFIG: &str = "/run/log_parser/vector.toml";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("log_parser=info".parse().unwrap()),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_CONFIG.to_string());
    let categories_path = std::env::var("CATEGORIES_PATH")
        .unwrap_or_else(|_| DEFAULT_CATEGORIES.to_string());

    info!(config = %config_path, "log_parser 시작");

    let cfg = config::Config::load(&config_path)
        .with_context(|| format!("설정 로드 실패: {config_path}"))?;

    info!(
        cycle_seconds = cfg.cycle.window_seconds,
        transport_kind = %cfg.transport.kind,
        "설정 로드 완료"
    );

    // ── Step 1: cgroup self-attach ────────────────────────────────────────────

    if cfg.cgroup.enabled {
        platform::cgroup::self_attach(
            &cfg.cgroup.path,
            &cfg.cgroup.memory_max,
            &cfg.cgroup.cpu_max,
        )
        .context("cgroup self-attach 실패 — 격리 없이 기동 거부")?;
        platform::cgroup::verify(&cfg.cgroup.path).context("cgroup 검증 실패")?;
    } else {
        tracing::warn!("cgroup 비활성화 — 리소스 격리 없이 실행 중");
    }

    info!("Step 1 완료 — cgroup 격리 + 설정 로드 검증 통과");

    // ── 호스트 메타데이터 ─────────────────────────────────────────────────────

    let host = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    // boot_id: prefer system value; fall back to config override for container environments.
    let boot_id = {
        let sys = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .unwrap_or_default()
            .trim()
            .to_string();
        if sys.is_empty() && !cfg.cycle.boot_id_fallback.is_empty() {
            cfg.cycle.boot_id_fallback.clone()
        } else {
            sys
        }
    };
    // host_id: config override takes priority (useful when /etc/machine-id is unreliable).
    let host_id = if !cfg.cycle.host_id_override.is_empty() {
        cfg.cycle.host_id_override.clone()
    } else {
        std::fs::read_to_string("/etc/machine-id")
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    // ── Shared resources (spool + transport) ──────────────────────────────────

    let spool = Arc::new(
        transport::spool::Spool::new(&cfg.transport.spool_dir, cfg.transport.spool_max_mb)
            .context("spool 초기화 실패")?,
    );
    let transport = transport::create(&cfg.transport).context("transport 초기화 실패")?;

    // ── Step 2: transport smoke test ──────────────────────────────────────────

    if !cfg.transport.endpoint.is_empty() {
        let dummy = envelope::Envelope {
            event_kind: "log_batch".to_string(),
            cycle: envelope::Cycle {
                host: host.clone(),
                host_id: host_id.clone(),
                boot_id: boot_id.clone(),
                ts: chrono::Utc::now().to_rfc3339(),
                window: None,
                seq: Some(0),
            },
            headers: envelope::Headers {
                total_sections: 0,
                counts: None,
                process_health: None,
                duration_ms: Some(0),
            },
            body: vec![],
        };
        match transport.send(&dummy).await {
            Ok(()) => info!("Step 2 완료 — envelope 전송 성공"),
            Err(e) => tracing::warn!("전송 실패 (test_server 미기동?): {e}"),
        }
    } else {
        info!("Step 2 완료 — transport.endpoint 미설정, 전송 생략");
    }

    // ── Step 3: distro detection + Vector spawn ───────────────────────────────

    let os = platform::discovery::OsInfo::detect();
    let probes = platform::capability::probe(
        &cfg.pipeline.vector_bin,
        os.syslog_path,
        os.auth_log_path,
    );

    if !probes.vector_ok {
        anyhow::bail!(
            "Vector 바이너리 없음: {} — 기동 불가",
            cfg.pipeline.vector_bin
        );
    }

    // Use the user-provided static vector config if the file exists; otherwise auto-generate.
    let user_vector_cfg = cfg.pipeline.vector_config.clone();
    let (runtime_cfg_path, skip_vector_autogen) = if std::path::Path::new(&user_vector_cfg).exists() {
        info!(path = %user_vector_cfg, "사용자 제공 vector config 발견 — auto-generation 건너뜀");
        (user_vector_cfg, true)
    } else {
        (
            std::env::var("VECTOR_RUNTIME_CONFIG")
                .unwrap_or_else(|_| RUNTIME_VECTOR_CONFIG.to_string()),
            false,
        )
    };

    let cgroup_path =
        if cfg.cgroup.enabled { cfg.cgroup.path.clone() } else { String::new() };

    let vector_handle = Arc::new(pipeline::vector_spawn::VectorHandle::new(
        cfg.pipeline.clone(),
        cgroup_path.clone(),
        runtime_cfg_path.clone(),
    ));

    let vector_task = if probes.has_log_sources() {
        if !skip_vector_autogen {
            pipeline::vector_config::write_runtime(&cfg.pipeline, &os, &probes, &runtime_cfg_path)
                .context("Vector runtime config 생성 실패")?;
            info!(path = %runtime_cfg_path, "Vector runtime config 생성 완료");
        }

        let h = Arc::clone(&vector_handle);
        tokio::spawn(async move { h.run_supervised().await })
    } else {
        tracing::warn!("수집 가능한 로그 소스 없음 — Vector 기동 생략 (컨테이너 환경?)");
        tokio::spawn(std::future::pending::<anyhow::Result<()>>())
    };

    info!("Step 3 완료 — Vector 기동 (host={host})");

    // ── Step 4: Unix socket receivers + pipeline ──────────────────────────────

    let categories = normalize::categories::CategoryMatcher::load(&categories_path)
        .unwrap_or_else(|e| {
            tracing::warn!("categories.yaml 로드 실패 ({e}) — fallback 사용");
            normalize::categories::CategoryMatcher::fallback()
        });

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(cfg.pipeline.channel_capacity);
    let (flush_tx, flush_rx) = tokio::sync::mpsc::channel::<coordinator::FlushSignal>(4);

    let recv_task = {
        let critical = cfg.pipeline.vector_critical_sock.clone();
        let normal = cfg.pipeline.vector_normal_sock.clone();
        let tx = event_tx;
        tokio::spawn(async move {
            pipeline::vector_receiver::run(critical, normal, tx).await
        })
    };

    // Load initial seq from seq_state_path to survive daemon restarts.
    let (initial_seq, seq_state_path) = if cfg.static_state.enabled {
        let path = cfg.static_state.seq_state_path.clone();
        let seq = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(1);
        (seq, path)
    } else {
        (1, String::new())
    };

    let pipeline_task = {
        let dedup_cfg = cfg.dedup.clone();
        let cycle_cfg = cfg.cycle.clone();
        let transport_cfg = cfg.transport.clone();
        let max_events = cfg.pipeline.body_max_events_per_cycle;
        let body_max_bytes = cfg.pipeline.body_max_size_mb * 1024 * 1024;
        let h = Arc::clone(&vector_handle);
        let host_for_pipeline = host.clone();
        let host_id_for_pipeline = host_id.clone();
        let boot_id_for_pipeline = boot_id.clone();
        let spool_for_pipeline = Arc::clone(&spool);
        let transport_for_pipeline = Arc::clone(&transport);
        tokio::spawn(async move {
            coordinator::run_pipeline(
                event_rx,
                flush_rx,
                dedup_cfg,
                cycle_cfg,
                transport_cfg,
                categories,
                host_for_pipeline,
                host_id_for_pipeline,
                boot_id_for_pipeline,
                h,
                max_events,
                body_max_bytes,
                initial_seq,
                seq_state_path,
                spool_for_pipeline,
                transport_for_pipeline,
            )
            .await
        })
    };

    info!("Step 4 완료 — pipeline 기동 (normalizer + dedup + coordinator)");

    // ── Step 5: inbound server (/flush, /stat, /trigger-sos, /drain-spool, /drain-status) ──

    let flush_token = std::env::var(&cfg.inbound.token_env).unwrap_or_default();
    if flush_token.is_empty() {
        anyhow::bail!(
            "환경변수 미설정: {} — 인증 없는 /flush endpoint 기동 거부",
            cfg.inbound.token_env
        );
    }
    let stat_token = std::env::var(&cfg.inbound.stat_token_env).unwrap_or_default();
    if stat_token.is_empty() {
        anyhow::bail!(
            "환경변수 미설정: {} — 인증 없는 /stat endpoint 기동 거부",
            cfg.inbound.stat_token_env
        );
    }
    let sos_token = std::env::var(&cfg.inbound.sos_token_env).unwrap_or_default();
    if sos_token.is_empty() {
        anyhow::bail!(
            "환경변수 미설정: {} — 인증 없는 /trigger-sos endpoint 기동 거부",
            cfg.inbound.sos_token_env
        );
    }

    // Compute log paths for sos section
    let log_paths: Vec<String> = {
        let mut paths = Vec::new();
        for candidate in &[os.syslog_path, os.auth_log_path, "/var/log/messages", "/var/log/secure", "/var/log/syslog", "/var/log/auth.log"] {
            if std::path::Path::new(candidate).exists() {
                let p = candidate.to_string();
                if !paths.contains(&p) {
                    paths.push(p);
                }
            }
        }
        paths
    };

    if cfg.static_state.enabled {
        info!(
            scan_interval_secs = cfg.static_state.scan_interval_seconds,
            storage_path = %cfg.static_state.storage_path,
            seq_state_path = %cfg.static_state.seq_state_path,
            initial_seq,
            "static_state: seq persistence enabled"
        );
    }

    let inbound_task = {
        let listen_addr = cfg.inbound.listen_addr.clone();
        let rate = cfg.inbound.rate_limit_per_hour;
        let timeout = cfg.inbound.response_timeout_seconds;
        let body_size_limit = cfg.inbound.body_size_limit_bytes;
        let envelope_size_limit_mb = cfg.inbound.envelope_size_limit_mb;
        let serialize_strategy = cfg.inbound.serialize_strategy.clone();
        let static_state_enabled = cfg.static_state.enabled;
        let transport_cfg = cfg.transport.clone();
        let ftx = flush_tx;
        let host2 = host.clone();
        let host_id2 = host_id.clone();
        let boot_id2 = boot_id.clone();
        let spool_for_inbound = Arc::clone(&spool);
        tokio::spawn(async move {
            inbound::serve(
                &listen_addr,
                ftx,
                flush_token,
                stat_token,
                sos_token,
                rate,
                timeout,
                body_size_limit,
                envelope_size_limit_mb,
                serialize_strategy,
                static_state_enabled,
                host2,
                host_id2,
                boot_id2,
                log_paths,
                spool_for_inbound,
                transport_cfg,
            ).await
        })
    };

    info!(
        addr = %cfg.inbound.listen_addr,
        "Step 5 완료 — /flush, /stat, /trigger-sos, /drain-spool, /drain-status 서버 기동"
    );

    // ── Main supervision loop ──────────────────────────────────────────────────

    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        result = vector_task => {
            match result {
                Ok(Ok(())) => info!("Vector 감시 태스크 정상 종료"),
                Ok(Err(e)) => { tracing::error!("Vector 치명 오류: {e}"); std::process::exit(1); }
                Err(e)     => { tracing::error!("Vector panic: {e}"); std::process::exit(1); }
            }
        }
        result = recv_task => {
            match result {
                Ok(Ok(())) => info!("Socket receiver 종료"),
                Ok(Err(e)) => { tracing::error!("Socket receiver 오류: {e}"); std::process::exit(1); }
                Err(e)     => { tracing::error!("Socket receiver panic: {e}"); std::process::exit(1); }
            }
        }
        result = pipeline_task => {
            match result {
                Ok(Ok(())) => info!("Pipeline 종료"),
                Ok(Err(e)) => { tracing::error!("Pipeline 오류: {e}"); std::process::exit(1); }
                Err(e)     => { tracing::error!("Pipeline panic: {e}"); std::process::exit(1); }
            }
        }
        result = inbound_task => {
            match result {
                Ok(Ok(())) => info!("Inbound 서버 종료"),
                Ok(Err(e)) => { tracing::error!("Inbound 서버 오류: {e}"); std::process::exit(1); }
                Err(e)     => { tracing::error!("Inbound 서버 panic: {e}"); std::process::exit(1); }
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("SIGINT 수신 — 종료");
        }
        _ = sigterm.recv() => {
            info!("SIGTERM 수신 — graceful 종료");
        }
    }

    Ok(())
}
