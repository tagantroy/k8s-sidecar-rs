//! A small Rust drop-in for kiwigrid/k8s-sidecar: watches ConfigMaps and Secrets with a
//! label, writes their data as files into a folder, and calls a reload URL after changes.

pub mod config;
pub mod metrics;
pub mod object;
pub mod paths;
pub mod reload;
pub mod sync;

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::future::Future;

use anyhow::{Context, Result};
use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Secret};
use kube::api::ListParams;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client, Resource};
use serde::de::DeserializeOwned;
use tokio::sync::mpsc;
use tracing::{info, warn};

use config::{Config, Method, Namespaces};
use metrics::Metrics;
use object::{IntoObj, Kind, Obj, ObjKey};
use reload::Reloader;
use sync::{SyncConfig, Syncer};

/// One watched scope: a kind in one namespace, or in all of them.
#[derive(Debug, Clone)]
struct Source {
    kind: Kind,
    namespace: Option<String>,
}

impl Source {
    fn contains(&self, key: &ObjKey) -> bool {
        key.kind == self.kind
            && self
                .namespace
                .as_ref()
                .is_none_or(|ns| *ns == key.namespace)
    }
}

fn sources(cfg: &Config) -> Vec<Source> {
    let namespaces: Vec<Option<String>> = match &cfg.namespaces {
        Namespaces::All => vec![None],
        Namespaces::List(list) => list.iter().cloned().map(Some).collect(),
    };
    let mut kinds = Vec::new();
    if cfg.resources.configmaps() {
        kinds.push(Kind::ConfigMap);
    }
    if cfg.resources.secrets() {
        kinds.push(Kind::Secret);
    }
    kinds
        .into_iter()
        .flat_map(|kind| {
            namespaces.iter().map(move |namespace| Source {
                kind,
                namespace: namespace.clone(),
            })
        })
        .collect()
}

fn api<K>(client: &Client, namespace: Option<&str>) -> Api<K>
where
    K: Resource<Scope = k8s_openapi::NamespaceResourceScope, DynamicType = ()>,
{
    match namespace {
        Some(ns) => Api::namespaced(client.clone(), ns),
        None => Api::all(client.clone()),
    }
}

enum Event {
    InitStart(usize),
    InitApply(usize, Obj),
    InitDone(usize),
    Apply(Obj),
    Delete(ObjKey),
}

async fn watch<K>(
    idx: usize,
    api: Api<K>,
    selector: String,
    tx: mpsc::Sender<Event>,
    metrics: Metrics,
) where
    K: Resource + Clone + DeserializeOwned + Debug + Send + 'static + IntoObj,
{
    let stream = watcher(api, watcher::Config::default().labels(&selector)).default_backoff();
    futures::pin_mut!(stream);
    while let Some(ev) = stream.next().await {
        let ev = match ev {
            Ok(watcher::Event::Init) => Event::InitStart(idx),
            Ok(watcher::Event::InitApply(o)) => Event::InitApply(idx, o.into_obj()),
            Ok(watcher::Event::InitDone) => Event::InitDone(idx),
            Ok(watcher::Event::Apply(o)) => Event::Apply(o.into_obj()),
            Ok(watcher::Event::Delete(o)) => Event::Delete(o.obj_key()),
            Err(e) => {
                warn!(error = %e, "watch error; retrying");
                metrics.watch_error();
                continue;
            }
        };
        if tx.send(ev).await.is_err() {
            return;
        }
    }
}

/// Read one object again (it took over a contested file). `None` if it's gone or
/// lost the label.
async fn fetch(client: &Client, key: &ObjKey, cfg: &Config) -> Result<Option<Obj>> {
    let ns = Some(key.namespace.as_str());
    let (obj, labels) = match key.kind {
        Kind::ConfigMap => match api::<ConfigMap>(client, ns).get_opt(&key.name).await? {
            Some(o) => (
                Some(o.clone().into_obj()),
                o.metadata.labels.unwrap_or_default(),
            ),
            None => (None, Default::default()),
        },
        Kind::Secret => match api::<Secret>(client, ns).get_opt(&key.name).await? {
            Some(o) => (
                Some(o.clone().into_obj()),
                o.metadata.labels.unwrap_or_default(),
            ),
            None => (None, Default::default()),
        },
    };
    let labelled = match (&cfg.label_value, labels.get(&cfg.label)) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(want), Some(have)) => want == have,
    };
    Ok(obj.filter(|_| labelled))
}

/// Apply objects that took over contested files, following hand-overs until settled.
async fn refetch(
    client: &Client,
    cfg: &Config,
    syncer: &mut Syncer,
    mut keys: Vec<ObjKey>,
) -> bool {
    let mut changed = false;
    let mut seen = HashSet::new();
    while let Some(key) = keys.pop() {
        if !seen.insert(key.clone()) {
            continue;
        }
        let out = match fetch(client, &key, cfg).await {
            Ok(Some(obj)) => syncer.apply(&obj),
            Ok(None) => syncer.remove(&key),
            Err(e) => {
                warn!(object = %key, error = %e, "re-reading object failed; it will be corrected on the next change or resync");
                continue;
            }
        };
        changed |= out.changed;
        keys.extend(out.refetch);
    }
    changed
}

fn sync_config(cfg: &Config) -> SyncConfig {
    SyncConfig {
        folder: cfg.folder.clone(),
        folder_annotation: cfg.folder_annotation.clone(),
        unique_filenames: cfg.unique_filenames,
        file_mode: cfg.file_mode,
    }
}

/// METHOD=LIST: sync once, reload if anything changed, return.
pub async fn run_once(cfg: &Config, client: Client, metrics: Metrics) -> Result<()> {
    let selector = cfg.label_selector();
    let lp = ListParams::default().labels(&selector);
    let mut syncer = Syncer::new(sync_config(cfg), metrics.clone());
    let mut changed = false;
    for src in sources(cfg) {
        let ns = src.namespace.as_deref();
        let objs: Vec<Obj> = match src.kind {
            Kind::ConfigMap => api::<ConfigMap>(&client, ns)
                .list(&lp)
                .await?
                .items
                .into_iter()
                .map(IntoObj::into_obj)
                .collect(),
            Kind::Secret => api::<Secret>(&client, ns)
                .list(&lp)
                .await?
                .items
                .into_iter()
                .map(IntoObj::into_obj)
                .collect(),
        };
        for obj in objs {
            changed |= syncer.apply(&obj).changed;
        }
    }
    info!(objects = syncer.len(), changed, "synced once");
    metrics.set_ready();
    if changed && let Some(r) = &cfg.reload {
        reload::send(&reload::client(r), r, &metrics).await;
    }
    Ok(())
}

/// METHOD=WATCH: keep the folder in sync until `shutdown` completes.
pub async fn run_watch(
    cfg: &Config,
    client: Client,
    metrics: Metrics,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let selector = cfg.label_selector();
    let srcs = sources(cfg);
    let (tx, mut rx) = mpsc::channel::<Event>(256);
    for (idx, src) in srcs.iter().enumerate() {
        let ns = src.namespace.as_deref();
        let (tx, m, sel) = (tx.clone(), metrics.clone(), selector.clone());
        match src.kind {
            Kind::ConfigMap => tokio::spawn(watch(idx, api::<ConfigMap>(&client, ns), sel, tx, m)),
            Kind::Secret => tokio::spawn(watch(idx, api::<Secret>(&client, ns), sel, tx, m)),
        };
        info!(kind = src.kind.as_str(), namespace = ns.unwrap_or("ALL"), selector = %selector, "watching");
    }
    drop(tx);

    let reloader = Reloader::spawn(cfg.reload.clone(), metrics.clone());
    let mut syncer = Syncer::new(sync_config(cfg), metrics.clone());
    // Keys seen during each source's current (re)list, to drop objects deleted meanwhile.
    let mut listing: HashMap<usize, HashSet<ObjKey>> = HashMap::new();
    let mut synced: HashSet<usize> = HashSet::new();

    futures::pin_mut!(shutdown);
    loop {
        let ev = tokio::select! {
            _ = &mut shutdown => { info!("stopping"); return Ok(()); }
            ev = rx.recv() => match ev {
                Some(ev) => ev,
                None => anyhow::bail!("all watches ended"),
            },
        };
        let out = match ev {
            Event::InitStart(idx) => {
                listing.insert(idx, HashSet::new());
                continue;
            }
            Event::InitApply(idx, obj) => {
                listing.entry(idx).or_default().insert(obj.key.clone());
                syncer.apply(&obj)
            }
            Event::InitDone(idx) => {
                let seen = listing.remove(&idx).unwrap_or_default();
                let gone: Vec<ObjKey> = syncer
                    .keys()
                    .filter(|k| srcs[idx].contains(k) && !seen.contains(*k))
                    .cloned()
                    .collect();
                let mut out = sync::Outcome::default();
                for key in gone {
                    out.merge(syncer.remove(&key));
                }
                if synced.insert(idx) && synced.len() == srcs.len() {
                    info!(objects = syncer.len(), "initial sync done");
                    metrics.set_ready();
                }
                out
            }
            Event::Apply(obj) => syncer.apply(&obj),
            Event::Delete(key) => syncer.remove(&key),
        };
        let mut changed = out.changed;
        if !out.refetch.is_empty() {
            changed |= refetch(&client, cfg, &mut syncer, out.refetch).await;
        }
        if changed {
            reloader.trigger();
        }
    }
}

/// Run as configured until SIGTERM/SIGINT (WATCH) or after one pass (LIST).
pub async fn run(cfg: Config, client: Client, metrics: Metrics) -> Result<()> {
    if let Some(addr) = cfg.metrics_addr {
        tokio::spawn(metrics::serve(addr, metrics.clone()));
    }
    match cfg.method {
        Method::List => run_once(&cfg, client, metrics).await,
        Method::Watch => run_watch(&cfg, client, metrics, shutdown_signal()).await,
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

/// Kubernetes client from the in-cluster service account (or KUBECONFIG outside a cluster).
pub async fn client() -> Result<Client> {
    Client::try_default()
        .await
        .context("connecting to the Kubernetes API")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap as Map;

    #[test]
    fn sources_cover_kinds_and_namespaces() {
        let env: Map<String, String> = [
            ("LABEL", "x"),
            ("FOLDER", "/d"),
            ("RESOURCE", "both"),
            ("NAMESPACE", "a,b"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let cfg = Config::from_map(&env, || None).unwrap();
        let s = sources(&cfg);
        assert_eq!(s.len(), 4);
        let key = ObjKey {
            namespace: "b".into(),
            kind: Kind::Secret,
            name: "n".into(),
        };
        assert_eq!(s.iter().filter(|src| src.contains(&key)).count(), 1);
        let all = Source {
            kind: Kind::Secret,
            namespace: None,
        };
        assert!(all.contains(&key));
    }
}
