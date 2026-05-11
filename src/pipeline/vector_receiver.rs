use super::raw_event::RawLogEvent;
use anyhow::Result;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Bind Unix domain sockets and stream RawLogEvent into `tx`.
/// Blocks until the channel is closed.
pub async fn run(
    critical_sock: String,
    normal_sock: String,
    tx: mpsc::Sender<RawLogEvent>,
) -> Result<()> {
    // Remove stale socket files
    let _ = std::fs::remove_file(&critical_sock);
    let _ = std::fs::remove_file(&normal_sock);

    if let Some(p) = std::path::Path::new(&critical_sock).parent() {
        std::fs::create_dir_all(p)?;
    }

    let critical = UnixListener::bind(&critical_sock)?;
    let normal = UnixListener::bind(&normal_sock)?;

    std::fs::set_permissions(&critical_sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::set_permissions(&normal_sock, std::fs::Permissions::from_mode(0o600))?;

    info!(critical = %critical_sock, normal = %normal_sock, "Unix socket 바인드 완료");

    let tx2 = tx.clone();
    tokio::try_join!(
        accept_loop(critical, tx, "critical"),
        accept_loop(normal, tx2, "normal"),
    )?;

    Ok(())
}

async fn accept_loop(
    listener: UnixListener,
    tx: mpsc::Sender<RawLogEvent>,
    label: &'static str,
) -> Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut lines = BufReader::new(stream).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        match serde_json::from_str::<RawLogEvent>(&line) {
                            Ok(ev) => {
                                if tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => warn!(label, err = %e, "JSON 파싱 실패"),
                        }
                    }
                });
            }
            Err(e) => {
                error!(label, err = %e, "accept() 실패");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::time::{timeout, Duration};

    fn sock_paths(tag: &str) -> (String, String) {
        let base = format!("/tmp/vr_test_{}_{}", tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos());
        (format!("{}_crit.sock", base), format!("{}_norm.sock", base))
    }

    const VALID_JSON: &str = r#"{"log_parser_severity":"info","log_parser_source":"journald","timestamp":"2024-01-01T00:00:00Z","host":"h"}"#;

    #[tokio::test]
    async fn valid_event_reaches_channel() {
        let (crit, norm) = sock_paths("valid");
        let (tx, mut rx) = mpsc::channel::<RawLogEvent>(8);
        tokio::spawn(run(crit.clone(), norm, tx));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&crit).await.unwrap();
        stream.write_all(format!("{}\n", VALID_JSON).as_bytes()).await.unwrap();

        let ev = timeout(Duration::from_secs(2), rx.recv()).await
            .expect("timed out waiting for event")
            .expect("channel closed");
        assert_eq!(ev.log_parser_severity, "info");
        assert_eq!(ev.log_parser_source, "journald");
        assert_eq!(ev.host, "h");
    }

    #[tokio::test]
    async fn invalid_json_dropped_channel_stays_alive() {
        let (crit, norm) = sock_paths("invalid");
        let (tx, mut rx) = mpsc::channel::<RawLogEvent>(8);
        tokio::spawn(run(crit.clone(), norm, tx));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut stream = UnixStream::connect(&crit).await.unwrap();
        // invalid line first — must not panic or close channel
        stream.write_all(b"not valid json at all\n").await.unwrap();
        // valid line after — must still arrive
        stream.write_all(format!("{}\n", VALID_JSON).as_bytes()).await.unwrap();

        let ev = timeout(Duration::from_secs(2), rx.recv()).await
            .expect("timed out — invalid JSON may have broken the loop")
            .expect("channel closed");
        assert_eq!(ev.log_parser_severity, "info");
    }
}
