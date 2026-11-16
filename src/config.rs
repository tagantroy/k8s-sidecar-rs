//! Configuration from environment variables, compatible with the subset of
//! kiwigrid/k8s-sidecar's settings that this tool implements. Settings it doesn't
//! implement fail at start-up (or are logged as ignored) instead of silently changing
//! behaviour.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};

pub const DEFAULT_FOLDER_ANNOTATION: &str = "k8s-sidecar-target-directory";
pub const NAMESPACE_FILE: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

/// kiwigrid settings that don't change what ends up on disk; accepted and ignored.
const IGNORED: &[&str] = &[
    "IGNORE_ALREADY_PROCESSED",
    "WATCH_SERVER_TIMEOUT",
    "WATCH_CLIENT_TIMEOUT",
    "ERROR_THROTTLE_SLEEP",
    "REQ_RETRY_CONNECT",
    "REQ_RETRY_READ",
    "REQ_RETRY_BACKOFF_FACTOR",
    "ENABLE_5XX",
];

/// kiwigrid settings this tool doesn't implement: refuse to start rather than ignore them.
const UNSUPPORTED: &[(&str, &str)] = &[
    ("SCRIPT", "running a script after changes is not supported"),
    ("SLEEP_TIME", "METHOD=SLEEP is not supported; use WATCH"),
    (
        "RESOURCE_NAME",
        "filtering by resource name is not supported",
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resources {
    ConfigMaps,
    Secrets,
    Both,
}

impl Resources {
    pub fn configmaps(self) -> bool {
        self != Resources::Secrets
    }
    pub fn secrets(self) -> bool {
        self != Resources::ConfigMaps
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Namespaces {
    All,
    List(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Watch and keep the folder in sync until stopped.
    Watch,
    /// Sync once and exit (e.g. as an init container).
    List,
}

#[derive(Debug, Clone)]
pub struct Reload {
    pub url: String,
    pub method: reqwest::Method,
    pub username: Option<String>,
    pub password: Option<String>,
    pub payload: Option<String>,
    pub timeout: Duration,
    pub retries: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub label: String,
    pub label_value: Option<String>,
    pub folder: PathBuf,
    pub resources: Resources,
    pub namespaces: Namespaces,
    pub folder_annotation: String,
    pub method: Method,
    pub unique_filenames: bool,
    pub file_mode: Option<u32>,
    pub reload: Option<Reload>,
    pub log_level: String,
    pub log_json: bool,
    pub metrics_addr: Option<SocketAddr>,
    /// Settings that were set but are ignored (logged at start-up).
    pub ignored: Vec<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let env: HashMap<String, String> = std::env::vars().collect();
        Self::from_map(&env, || std::fs::read_to_string(NAMESPACE_FILE).ok())
    }

    /// `own_namespace` is only called when NAMESPACE is unset.
    pub fn from_map(
        env: &HashMap<String, String>,
        own_namespace: impl FnOnce() -> Option<String>,
    ) -> Result<Self> {
        let get = |k: &str| env.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());

        for (name, why) in UNSUPPORTED {
            if get(name).is_some() {
                bail!("{name} is set: {why}");
            }
        }
        for name in ["SKIP_TLS_VERIFY", "REQ_SKIP_TLS_VERIFY"] {
            if get(name).map(parse_bool).transpose()?.unwrap_or(false) {
                bail!("{name}=true is not supported");
            }
        }

        let label = get("LABEL").context("LABEL is required")?.to_string();
        let label_value = get("LABEL_VALUE").map(str::to_string);
        let folder = PathBuf::from(get("FOLDER").context("FOLDER is required")?);
        if !folder.is_absolute() {
            bail!("FOLDER must be an absolute path, got {}", folder.display());
        }

        let resources = match get("RESOURCE").map(str::to_ascii_lowercase).as_deref() {
            None | Some("configmap") => Resources::ConfigMaps,
            Some("secret") => Resources::Secrets,
            Some("both") => Resources::Both,
            Some(other) => bail!("RESOURCE must be configmap, secret or both, got {other:?}"),
        };

        let namespaces = match get("NAMESPACE") {
            Some(v) if v.eq_ignore_ascii_case("ALL") => Namespaces::All,
            Some(v) => {
                let list: Vec<String> = v
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect();
                if list.is_empty() {
                    bail!("NAMESPACE is empty");
                }
                Namespaces::List(list)
            }
            None => {
                let ns = own_namespace()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .context("NAMESPACE is unset and the pod's own namespace is unknown")?;
                Namespaces::List(vec![ns])
            }
        };

        let method = match get("METHOD").map(str::to_ascii_uppercase).as_deref() {
            None | Some("WATCH") => Method::Watch,
            Some("LIST") => Method::List,
            Some("SLEEP") => bail!("METHOD=SLEEP is not supported; use WATCH"),
            Some(other) => bail!("METHOD must be WATCH or LIST, got {other:?}"),
        };

        let file_mode = get("DEFAULT_FILE_MODE")
            .map(|v| {
                u32::from_str_radix(v, 8)
                    .with_context(|| format!("DEFAULT_FILE_MODE {v:?} is not octal"))
            })
            .transpose()?;

        let reload = match get("REQ_URL") {
            None => None,
            Some(url) => {
                if !url.starts_with("http://") {
                    bail!(
                        "REQ_URL must be plain http:// (the reload target is in the same pod), got {url:?}"
                    );
                }
                let method = get("REQ_METHOD").unwrap_or("GET").to_ascii_uppercase();
                let method = reqwest::Method::from_bytes(method.as_bytes())
                    .with_context(|| format!("REQ_METHOD {method:?} is invalid"))?;
                let timeout = get("REQ_TIMEOUT")
                    .map(|v| {
                        v.parse::<f64>()
                            .with_context(|| format!("REQ_TIMEOUT {v:?} is not a number"))
                    })
                    .transpose()?
                    .unwrap_or(10.0);
                if !(timeout > 0.0 && timeout.is_finite()) {
                    bail!("REQ_TIMEOUT must be positive");
                }
                let retries = get("REQ_RETRY_TOTAL")
                    .map(|v| {
                        v.parse::<u32>()
                            .with_context(|| format!("REQ_RETRY_TOTAL {v:?} is not a whole number"))
                    })
                    .transpose()?
                    .unwrap_or(5);
                Some(Reload {
                    url: url.to_string(),
                    method,
                    username: get("REQ_USERNAME").map(str::to_string),
                    password: get("REQ_PASSWORD").map(str::to_string),
                    payload: get("REQ_PAYLOAD").map(str::to_string),
                    timeout: Duration::from_secs_f64(timeout),
                    retries,
                })
            }
        };

        let log_json = match get("LOG_FORMAT").map(str::to_ascii_uppercase).as_deref() {
            None | Some("JSON") => true,
            Some("LOGFMT") | Some("TEXT") => false,
            Some(other) => bail!("LOG_FORMAT must be JSON or LOGFMT, got {other:?}"),
        };

        let metrics_addr = get("METRICS_ADDR")
            .map(|v| {
                v.parse::<SocketAddr>()
                    .with_context(|| format!("METRICS_ADDR {v:?} is not host:port"))
            })
            .transpose()?;

        Ok(Config {
            label,
            label_value,
            folder,
            resources,
            namespaces,
            folder_annotation: get("FOLDER_ANNOTATION")
                .unwrap_or(DEFAULT_FOLDER_ANNOTATION)
                .to_string(),
            method,
            unique_filenames: get("UNIQUE_FILENAMES")
                .map(parse_bool)
                .transpose()?
                .unwrap_or(false),
            file_mode,
            reload,
            log_level: get("LOG_LEVEL").unwrap_or("INFO").to_ascii_lowercase(),
            log_json,
            metrics_addr,
            ignored: IGNORED
                .iter()
                .filter(|k| get(k).is_some())
                .map(|k| k.to_string())
                .collect(),
        })
    }

    pub fn label_selector(&self) -> String {
        match &self.label_value {
            Some(v) => format!("{}={}", self.label, v),
            None => self.label.clone(),
        }
    }
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => bail!("{v:?} is not a boolean"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn parse(pairs: &[(&str, &str)]) -> Result<Config> {
        Config::from_map(&env(pairs), || Some("monitoring\n".into()))
    }

    #[test]
    fn grafana_dashboards_sidecar() {
        let c = parse(&[
            ("METHOD", "WATCH"),
            ("LABEL", "grafana_dashboard"),
            ("LABEL_VALUE", "1"),
            ("FOLDER", "/tmp/dashboards"),
            ("RESOURCE", "both"),
            ("NAMESPACE", "ALL"),
            ("FOLDER_ANNOTATION", "grafana_folder"),
            ("REQ_USERNAME", "admin"),
            ("REQ_PASSWORD", "pw"),
            (
                "REQ_URL",
                "http://localhost:3000/api/admin/provisioning/dashboards/reload",
            ),
            ("REQ_METHOD", "POST"),
        ])
        .unwrap();
        assert_eq!(c.label_selector(), "grafana_dashboard=1");
        assert_eq!(c.resources, Resources::Both);
        assert_eq!(c.namespaces, Namespaces::All);
        assert_eq!(c.folder_annotation, "grafana_folder");
        let r = c.reload.unwrap();
        assert_eq!(r.method, reqwest::Method::POST);
        assert_eq!(r.username.as_deref(), Some("admin"));
        assert_eq!(r.retries, 5);
    }

    #[test]
    fn defaults() {
        let c = parse(&[("LABEL", "x"), ("FOLDER", "/data")]).unwrap();
        assert_eq!(c.label_selector(), "x");
        assert_eq!(c.resources, Resources::ConfigMaps);
        assert_eq!(c.namespaces, Namespaces::List(vec!["monitoring".into()]));
        assert_eq!(c.method, Method::Watch);
        assert_eq!(c.folder_annotation, DEFAULT_FOLDER_ANNOTATION);
        assert!(c.reload.is_none() && c.log_json && !c.unique_filenames);
    }

    #[test]
    fn namespace_list_and_file_mode() {
        let c = parse(&[
            ("LABEL", "x"),
            ("FOLDER", "/d"),
            ("NAMESPACE", "a, b,"),
            ("DEFAULT_FILE_MODE", "644"),
        ])
        .unwrap();
        assert_eq!(c.namespaces, Namespaces::List(vec!["a".into(), "b".into()]));
        assert_eq!(c.file_mode, Some(0o644));
    }

    #[test]
    fn refuses_unsupported() {
        for extra in [
            ("SCRIPT", "/x.sh"),
            ("METHOD", "SLEEP"),
            ("REQ_URL", "https://example.com/reload"),
            ("SKIP_TLS_VERIFY", "true"),
            ("RESOURCE", "pods"),
            ("FOLDER", "relative/path"),
        ] {
            let mut pairs = vec![("LABEL", "x"), ("FOLDER", "/d")];
            pairs.retain(|(k, _)| *k != extra.0);
            pairs.push(extra);
            assert!(parse(&pairs).is_err(), "{extra:?} should be refused");
        }
    }

    #[test]
    fn requires_label_folder_and_namespace() {
        assert!(parse(&[("FOLDER", "/d")]).is_err());
        assert!(parse(&[("LABEL", "x")]).is_err());
        assert!(Config::from_map(&env(&[("LABEL", "x"), ("FOLDER", "/d")]), || None).is_err());
    }

    #[test]
    fn ignored_settings_are_reported() {
        let c = parse(&[
            ("LABEL", "x"),
            ("FOLDER", "/d"),
            ("IGNORE_ALREADY_PROCESSED", "true"),
        ])
        .unwrap();
        assert_eq!(c.ignored, vec!["IGNORE_ALREADY_PROCESSED".to_string()]);
    }
}
