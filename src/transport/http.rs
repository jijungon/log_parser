use super::TransportError;
use crate::config::TransportConfig;
use crate::envelope::Envelope;
use anyhow::Result;
use flate2::{write::GzEncoder, Compression};
use reqwest::{Client, StatusCode};
use std::io::Write as _;
use std::time::Duration;

pub struct HttpJsonTransport {
    client: Client,
    endpoint: String,
    token: String,
    gzip_level: u32,
}

impl HttpJsonTransport {
    pub fn new(cfg: &TransportConfig) -> Result<Self> {
        let token = std::env::var(&cfg.token_env).unwrap_or_default();
        if token.is_empty() {
            anyhow::bail!(
                "환경변수 미설정: {} — 아웃바운드 Bearer 토큰 없음, 기동 거부",
                cfg.token_env
            );
        }

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(cfg.connect_timeout_seconds))
            .timeout(Duration::from_secs(cfg.request_timeout_seconds))
            .https_only(cfg.tls_enabled)
            .build()?;

        Ok(Self {
            client,
            endpoint: cfg.endpoint.clone(),
            token,
            gzip_level: cfg.http_gzip_level,
        })
    }

    pub async fn send(&self, envelope: &Envelope) -> Result<(), TransportError> {
        let json = serde_json::to_vec(envelope)
            .map_err(|e| TransportError::Fatal(format!("JSON 직렬화 실패: {e}")))?;

        let body = self.compress(&json)?;

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Encoding", "gzip")
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| TransportError::Retryable(format!("HTTP 요청 실패: {e}")))?;

        match resp.status() {
            s if s.is_success() => Ok(()),
            StatusCode::TOO_MANY_REQUESTS => {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(60);
                Err(TransportError::RateLimited { retry_after })
            }
            s if s.is_server_error() => Err(TransportError::Retryable(format!("서버 오류: {s}"))),
            s => Err(TransportError::Fatal(format!("클라이언트 오류: {s}"))),
        }
    }

    pub fn compress(&self, data: &[u8]) -> Result<Vec<u8>, TransportError> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::new(self.gzip_level));
        enc.write_all(data)
            .map_err(|e| TransportError::Fatal(format!("gzip 압축 실패: {e}")))?;
        enc.finish()
            .map_err(|e| TransportError::Fatal(format!("gzip finish 실패: {e}")))
    }

    #[cfg(test)]
    pub fn test_new(endpoint: &str, token: &str) -> Self {
        Self {
            client: Client::builder().build().unwrap(),
            endpoint: endpoint.to_string(),
            token: token.to_string(),
            gzip_level: 6,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::{Cycle, Headers, Envelope};
    use axum::{http::StatusCode as AxumStatus, routing::post, Router};
    use flate2::read::GzDecoder;
    use std::collections::VecDeque;
    use std::io::Read as _;
    use std::sync::Arc as StdArc;
    use tokio::sync::Mutex as TokioMutex;

    async fn start_response_server(responses: Vec<u16>) -> String {
        let queue = StdArc::new(TokioMutex::new(VecDeque::from(responses)));
        let app = Router::new().route(
            "/ingest",
            post({
                let queue = StdArc::clone(&queue);
                move || {
                    let queue = StdArc::clone(&queue);
                    async move {
                        let mut q = queue.lock().await;
                        let code = q.pop_front().unwrap_or(200);
                        AxumStatus::from_u16(code).unwrap()
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/ingest")
    }

    fn test_envelope() -> Envelope {
        Envelope {
            event_kind: "log_batch".to_string(),
            cycle: Cycle {
                host: "test".to_string(),
                host_id: "hid".to_string(),
                boot_id: "bid".to_string(),
                ts: "2026-01-01T00:00:00Z".to_string(),
                window: None,
                seq: Some(1),
            },
            headers: Headers {
                total_sections: 0,
                counts: None,
                process_health: None,
                duration_ms: Some(0),
            },
            body: vec![],
        }
    }

    #[tokio::test]
    async fn send_200_returns_ok() {
        let url = start_response_server(vec![200]).await;
        let t = HttpJsonTransport::test_new(&url, "tok");
        assert!(t.send(&test_envelope()).await.is_ok());
    }

    #[tokio::test]
    async fn send_429_returns_rate_limited() {
        let url = start_response_server(vec![429]).await;
        let t = HttpJsonTransport::test_new(&url, "tok");
        let err = t.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, TransportError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn send_500_returns_retryable() {
        let url = start_response_server(vec![500]).await;
        let t = HttpJsonTransport::test_new(&url, "tok");
        let err = t.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, TransportError::Retryable(_)));
    }

    #[tokio::test]
    async fn send_403_returns_fatal() {
        let url = start_response_server(vec![403]).await;
        let t = HttpJsonTransport::test_new(&url, "tok");
        let err = t.send(&test_envelope()).await.unwrap_err();
        assert!(matches!(err, TransportError::Fatal(_)));
    }

    #[test]
    fn compress_produces_valid_gzip() {
        let t = HttpJsonTransport::test_new("http://unused", "tok");
        let data = b"hello world from log_parser";
        let compressed = t.compress(data).unwrap();
        assert!(!compressed.is_empty());
        let mut decoder = GzDecoder::new(compressed.as_slice());
        let mut decoded = Vec::new();
        decoder.read_to_end(&mut decoded).unwrap();
        assert_eq!(decoded, data);
    }
}
