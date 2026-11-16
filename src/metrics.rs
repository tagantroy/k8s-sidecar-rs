//! Prometheus metrics and the optional `/metrics` + `/healthz` endpoint (METRICS_ADDR).
//! Minimal HTTP/1.1, one request per connection: nothing else is served.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

type Labels = Vec<(String, String)>;

fn label(k: &str, v: &str) -> Labels {
    vec![(k.to_string(), v.to_string())]
}

#[derive(Clone)]
pub struct Metrics(Arc<Inner>);

struct Inner {
    registry: Registry,
    objects: Gauge,
    files_written: Counter,
    files_deleted: Counter,
    skipped: Family<Labels, Counter>,
    conflicts: Counter,
    io_errors: Counter,
    watch_errors: Counter,
    reloads: Family<Labels, Counter>,
    ready: AtomicBool,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::with_prefix("k8s_sidecar");
        let inner = Inner {
            objects: Gauge::default(),
            files_written: Counter::default(),
            files_deleted: Counter::default(),
            skipped: Family::default(),
            conflicts: Counter::default(),
            io_errors: Counter::default(),
            watch_errors: Counter::default(),
            reloads: Family::default(),
            ready: AtomicBool::new(false),
            registry: Registry::default(),
        };
        registry.register("objects", "Objects currently synced", inner.objects.clone());
        registry.register(
            "files_written",
            "Files written",
            inner.files_written.clone(),
        );
        registry.register(
            "files_deleted",
            "Files deleted",
            inner.files_deleted.clone(),
        );
        registry.register(
            "skipped",
            "Keys or objects skipped, by reason",
            inner.skipped.clone(),
        );
        registry.register(
            "conflicts",
            "Writes skipped because another object owns the file",
            inner.conflicts.clone(),
        );
        registry.register(
            "io_errors",
            "Failed file writes or deletes",
            inner.io_errors.clone(),
        );
        registry.register(
            "watch_errors",
            "Errors from the Kubernetes watch (retried)",
            inner.watch_errors.clone(),
        );
        registry.register(
            "reloads",
            "Reload requests, by result",
            inner.reloads.clone(),
        );
        Metrics(Arc::new(Inner { registry, ..inner }))
    }

    pub fn set_objects(&self, n: usize) {
        self.0.objects.set(n as i64);
    }
    pub fn written(&self) {
        self.0.files_written.inc();
    }
    pub fn deleted(&self) {
        self.0.files_deleted.inc();
    }
    pub fn skipped(&self, reason: &str) {
        self.0.skipped.get_or_create(&label("reason", reason)).inc();
    }
    pub fn conflict(&self) {
        self.0.conflicts.inc();
    }
    pub fn io_error(&self) {
        self.0.io_errors.inc();
    }
    pub fn watch_error(&self) {
        self.0.watch_errors.inc();
    }
    pub fn reload(&self, ok: bool) {
        self.0
            .reloads
            .get_or_create(&label("result", if ok { "success" } else { "failure" }))
            .inc();
    }
    pub fn set_ready(&self) {
        self.0.ready.store(true, Ordering::Relaxed);
    }
    pub fn is_ready(&self) -> bool {
        self.0.ready.load(Ordering::Relaxed)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        encode(&mut out, &self.0.registry).expect("writing to a String can't fail");
        out
    }
}

/// `GET /metrics`, and `GET /healthz` (200 once the first full sync is done, 503 before).
pub async fn serve(addr: SocketAddr, metrics: Metrics) {
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!(%addr, error = %e, "metrics endpoint not started");
            return;
        }
    };
    info!(%addr, "metrics endpoint listening");
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let n =
                match tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf))
                    .await
                {
                    Ok(Ok(n)) => n,
                    _ => return,
                };
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.split_whitespace().nth(1).unwrap_or("");
            let (status, ctype, body) = match path {
                "/metrics" => (
                    "200 OK",
                    "application/openmetrics-text; version=1.0.0; charset=utf-8",
                    metrics.render(),
                ),
                "/healthz" if metrics.is_ready() => ("200 OK", "text/plain", "ok\n".to_string()),
                "/healthz" => (
                    "503 Service Unavailable",
                    "text/plain",
                    "syncing\n".to_string(),
                ),
                _ => ("404 Not Found", "text/plain", "not found\n".to_string()),
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders() {
        let m = Metrics::new();
        m.written();
        m.reload(false);
        m.skipped("url_key");
        let out = m.render();
        assert!(out.contains("k8s_sidecar_files_written_total 1"), "{out}");
        assert!(
            out.contains("k8s_sidecar_reloads_total{result=\"failure\"} 1"),
            "{out}"
        );
        assert!(
            out.contains("k8s_sidecar_skipped_total{reason=\"url_key\"} 1"),
            "{out}"
        );
    }
}
