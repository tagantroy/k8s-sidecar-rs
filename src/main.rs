use std::process::ExitCode;

use k8s_sidecar_rs::config::Config;
use k8s_sidecar_rs::metrics::Metrics;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("k8s-sidecar-rs {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let cfg = match Config::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("k8s-sidecar: configuration error: {e:#}");
            return ExitCode::from(2);
        }
    };

    let filter = EnvFilter::try_new(&cfg.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    let fmt = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);
    if cfg.log_json {
        fmt.json().flatten_event(true).init();
    } else {
        fmt.init();
    }

    info!(version = env!("CARGO_PKG_VERSION"), folder = %cfg.folder.display(), selector = %cfg.label_selector(), "starting");
    for name in &cfg.ignored {
        warn!(setting = %name, "setting is not used by this implementation; ignored");
    }

    let result = async {
        let client = k8s_sidecar_rs::client().await?;
        k8s_sidecar_rs::run(cfg, client, Metrics::new()).await
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = format!("{e:#}"), "exiting");
            ExitCode::FAILURE
        }
    }
}
