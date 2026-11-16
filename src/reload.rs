//! The reload call (e.g. Grafana's provisioning reload API), debounced: a burst of changes
//! (the first sync, a Helm upgrade touching many dashboards) leads to one request.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::{Instant, sleep, timeout_at};
use tracing::{info, warn};

use crate::config::Reload;
use crate::metrics::Metrics;

/// Wait this long without new changes before calling…
const QUIET: Duration = Duration::from_secs(1);
/// …but never longer than this after the first change.
const MAX_WAIT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct Reloader(Option<mpsc::UnboundedSender<()>>);

impl Reloader {
    /// Starts the background task; without a REQ_URL, `trigger` does nothing.
    pub fn spawn(cfg: Option<Reload>, metrics: Metrics) -> Self {
        let Some(cfg) = cfg else {
            return Reloader(None);
        };
        let (tx, mut rx) = mpsc::unbounded_channel::<()>();
        tokio::spawn(async move {
            let client = client(&cfg);
            while rx.recv().await.is_some() {
                let deadline = Instant::now() + MAX_WAIT;
                loop {
                    let quiet_until = (Instant::now() + QUIET).min(deadline);
                    match timeout_at(quiet_until, rx.recv()).await {
                        Ok(Some(())) if Instant::now() < deadline => continue,
                        Ok(None) => return,
                        _ => break,
                    }
                }
                send(&client, &cfg, &metrics).await;
            }
        });
        Reloader(Some(tx))
    }

    pub fn trigger(&self) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(());
        }
    }
}

pub fn client(cfg: &Reload) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(cfg.timeout)
        .build()
        .expect("plain HTTP client")
}

/// One reload, with retries (1 s, 2 s, 4 s … up to 30 s apart). Returns whether it succeeded.
pub async fn send(client: &reqwest::Client, cfg: &Reload, metrics: &Metrics) -> bool {
    let mut delay = Duration::from_secs(1);
    for attempt in 0..=cfg.retries {
        let mut req = client.request(cfg.method.clone(), &cfg.url);
        if let Some(user) = &cfg.username {
            req = req.basic_auth(user, cfg.password.as_deref());
        }
        if let Some(body) = &cfg.payload {
            req = req
                .header("content-type", "application/json")
                .body(body.clone());
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(url = %cfg.url, status = resp.status().as_u16(), "reload requested");
                metrics.reload(true);
                return true;
            }
            Ok(resp) => {
                warn!(url = %cfg.url, status = resp.status().as_u16(), attempt, "reload request failed")
            }
            Err(e) => warn!(url = %cfg.url, error = %e, attempt, "reload request failed"),
        }
        if attempt < cfg.retries {
            sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(30));
        }
    }
    metrics.reload(false);
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A tiny HTTP server that counts requests and remembers the last one.
    async fn server(status: u16) -> (String, Arc<AtomicUsize>, Arc<tokio::sync::Mutex<String>>) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/reload", l.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let last = Arc::new(tokio::sync::Mutex::new(String::new()));
        let (c, la) = (count.clone(), last.clone());
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = s.read(&mut buf).await.unwrap();
                *la.lock().await = String::from_utf8_lossy(&buf[..n]).to_string();
                c.fetch_add(1, Ordering::SeqCst);
                let _ = s
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await;
            }
        });
        (url, count, last)
    }

    fn cfg(url: String, retries: u32) -> Reload {
        Reload {
            url,
            method: reqwest::Method::POST,
            username: Some("admin".into()),
            password: Some("pw".into()),
            payload: None,
            timeout: Duration::from_secs(2),
            retries,
        }
    }

    #[tokio::test]
    async fn sends_with_basic_auth() {
        let (url, count, last) = server(200).await;
        let c = cfg(url, 0);
        assert!(send(&client(&c), &c, &Metrics::new()).await);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let req = last.lock().await.clone();
        assert!(req.starts_with("POST /reload"), "{req}");
        assert!(
            req.to_ascii_lowercase()
                .contains("authorization: basic ywrtaw46chc="),
            "{req}"
        );
    }

    #[tokio::test]
    async fn retries_then_gives_up() {
        let (url, count, _) = server(500).await;
        let c = cfg(url, 2);
        let m = Metrics::new();
        assert!(!send(&client(&c), &c, &m).await);
        assert_eq!(count.load(Ordering::SeqCst), 3);
        assert!(m.render().contains("result=\"failure\"} 1"));
    }

    #[tokio::test]
    async fn burst_becomes_one_request() {
        let (url, count, _) = server(200).await;
        let r = Reloader::spawn(Some(cfg(url, 0)), Metrics::new());
        for _ in 0..20 {
            r.trigger();
        }
        sleep(QUIET + Duration::from_millis(700)).await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
