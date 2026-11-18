//! End to end against a real k3s in Docker: initial sync, updates, label removal, deletes,
//! secrets in another namespace with a target-folder annotation, and the reload call.
//! Run with `cargo test -- --ignored` (needs Docker that allows privileged containers).
//! k3s is started with the `docker` command directly: a whole container-test framework
//! (~150 crates) for one container isn't worth it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Secret};
use k8s_sidecar_rs::config::Config;
use k8s_sidecar_rs::metrics::Metrics;
use kube::api::{DeleteParams, ObjectMeta, Patch, PatchParams, PostParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn reload_server() -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/api/admin/provisioning/dashboards/reload",
        l.local_addr().unwrap()
    );
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = s.read(&mut buf).await;
            c.fetch_add(1, Ordering::SeqCst);
            let _ = s
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await;
        }
    });
    (url, count)
}

/// The k3s version our clusters run; the image is published by the k3s maintainers.
const K3S_IMAGE: &str = "rancher/k3s:v1.37.1-k3s1";

/// A k3s server in Docker, removed on drop (also when the test fails).
struct K3s {
    id: String,
}

fn docker(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("running docker");
    assert!(
        out.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

impl K3s {
    fn start() -> Self {
        let id = docker(&[
            "run",
            "-d",
            "--privileged",
            "--userns=host",
            "-e",
            "K3S_KUBECONFIG_MODE=644",
            "-p",
            "127.0.0.1::6443",
            K3S_IMAGE,
            "server",
            "--snapshotter=native",
        ]);
        K3s { id }
    }

    /// Kubeconfig pointing at the published API port, once k3s has written it.
    async fn kubeconfig(&self) -> Kubeconfig {
        let port = docker(&["port", &self.id, "6443/tcp"]);
        let port = port
            .lines()
            .next()
            .and_then(|l| l.rsplit(':').next())
            .expect("published port")
            .to_string();
        for _ in 0..240 {
            let out = std::process::Command::new("docker")
                .args(["exec", &self.id, "cat", "/etc/rancher/k3s/k3s.yaml"])
                .output()
                .expect("running docker");
            if out.status.success() && !out.stdout.is_empty() {
                let mut kc = Kubeconfig::from_yaml(&String::from_utf8_lossy(&out.stdout)).unwrap();
                for c in &mut kc.clusters {
                    if let Some(cluster) = c.cluster.as_mut() {
                        cluster.server = Some(format!("https://127.0.0.1:{port}"));
                    }
                }
                return kc;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        panic!("k3s didn't write its kubeconfig within 2 minutes");
    }
}

impl Drop for K3s {
    fn drop(&mut self) {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", &self.id])
            .output();
    }
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..300 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn read(p: &Path) -> Option<String> {
    std::fs::read_to_string(p).ok()
}

fn cm(name: &str, labels: &[(&str, &str)], data: &[(&str, &str)]) -> ConfigMap {
    ConfigMap {
        metadata: ObjectMeta {
            name: Some(name.into()),
            labels: Some(
                labels
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        },
        data: Some(
            data.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "needs Docker (privileged k3s container)"]
async fn syncs_against_real_k3s() {
    let k3s = K3s::start();
    let kubeconfig = k3s.kubeconfig().await;
    let client = Client::try_from(
        kube::Config::from_custom_kubeconfig(kubeconfig, &KubeConfigOptions::default())
            .await
            .unwrap(),
    )
    .unwrap();

    let cms: Api<ConfigMap> = Api::namespaced(client.clone(), "default");
    // The API is up before the default namespace and service accounts are: retry.
    let pre = cm("dash-a", &[("grafana_dashboard", "1")], &[("a.json", "v1")]);
    let mut created = false;
    for _ in 0..120 {
        if cms.create(&PostParams::default(), &pre).await.is_ok() {
            created = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(created, "creating the first ConfigMap");
    cms.create(
        &PostParams::default(),
        &cm("not-a-dashboard", &[("other", "1")], &[("no.json", "x")]),
    )
    .await
    .unwrap();

    let out = tempfile::tempdir().unwrap();
    let (url, reloads) = reload_server().await;
    let env: HashMap<String, String> = [
        ("LABEL", "grafana_dashboard"),
        ("LABEL_VALUE", "1"),
        ("FOLDER", out.path().to_str().unwrap()),
        ("RESOURCE", "both"),
        ("NAMESPACE", "ALL"),
        ("FOLDER_ANNOTATION", "grafana_folder"),
        ("REQ_URL", url.as_str()),
        ("REQ_METHOD", "POST"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let cfg = Config::from_map(&env, || None).unwrap();

    let metrics = Metrics::new();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let task = {
        let (client, metrics) = (client.clone(), metrics.clone());
        tokio::spawn(async move {
            k8s_sidecar_rs::run_watch(&cfg, client, metrics, async {
                let _ = stop_rx.await;
            })
            .await
        })
    };

    // Initial sync: labelled object written, unlabelled ignored, one reload.
    let a = out.path().join("a.json");
    eventually("initial file", || read(&a).as_deref() == Some("v1")).await;
    eventually("ready", || metrics.is_ready()).await;
    assert!(!out.path().join("no.json").exists());
    eventually("first reload", || reloads.load(Ordering::SeqCst) >= 1).await;
    let after_initial = reloads.load(Ordering::SeqCst);

    // Update.
    cms.patch(
        "dash-a",
        &PatchParams::default(),
        &Patch::Merge(serde_json_value(r#"{"data":{"a.json":"v2"}}"#)),
    )
    .await
    .unwrap();
    eventually("updated file", || read(&a).as_deref() == Some("v2")).await;

    // A Secret in another namespace, into an annotated sub-folder.
    let nss: Api<Namespace> = Api::all(client.clone());
    nss.create(
        &PostParams::default(),
        &Namespace {
            metadata: ObjectMeta {
                name: Some("team".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let secrets: Api<Secret> = Api::namespaced(client.clone(), "team");
    let secret = Secret {
        metadata: ObjectMeta {
            name: Some("dash-s".into()),
            labels: Some([("grafana_dashboard".to_string(), "1".to_string())].into()),
            annotations: Some([("grafana_folder".to_string(), "Team/Boards".to_string())].into()),
            ..Default::default()
        },
        string_data: Some([("s.json".to_string(), "secret dash".to_string())].into()),
        ..Default::default()
    };
    secrets
        .create(&PostParams::default(), &secret)
        .await
        .unwrap();
    let s = out.path().join("Team/Boards/s.json");
    eventually("secret file in annotated folder", || {
        read(&s).as_deref() == Some("secret dash")
    })
    .await;

    // Label removed → file removed.
    cms.patch(
        "dash-a",
        &PatchParams::default(),
        &Patch::Merge(serde_json_value(
            r#"{"metadata":{"labels":{"grafana_dashboard":null}}}"#,
        )),
    )
    .await
    .unwrap();
    eventually("file of unlabelled object removed", || !a.exists()).await;

    // Deleted → file and its empty folders removed.
    secrets
        .delete("dash-s", &DeleteParams::default())
        .await
        .unwrap();
    eventually("file of deleted object removed", || {
        !s.exists() && !out.path().join("Team").exists()
    })
    .await;

    eventually("a reload after the later changes", || {
        reloads.load(Ordering::SeqCst) > after_initial
    })
    .await;

    stop_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

fn serde_json_value(s: &str) -> serde_json::Value {
    serde_json::from_str(s).unwrap()
}
