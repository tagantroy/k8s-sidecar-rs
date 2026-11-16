# Migrating from kiwigrid/k8s-sidecar

k8s-sidecar-rs reads the same environment variables as [kiwigrid/k8s-sidecar](https://github.com/kiwigrid/k8s-sidecar), so for most setups migrating means **changing the image and nothing else**. This guide shows how to check that your setup is one of them, how to switch, how to verify, and how to roll back.

## 1. Check your settings

List the environment of every sidecar container you run today. kiwigrid containers are usually named `*-sc-*` or `sidecar`:

```sh
kubectl get pods -A -o json | jq -r '
  .items[] | . as $p | (.spec.initContainers // []) + .spec.containers | .[]
  | select(.image | test("k8s-sidecar"))
  | "\($p.metadata.namespace)/\($p.metadata.name) \(.name)",
    (.env // [] | .[] | "  \(.name)=\(.value // "<from secret/configmap>")")'
```

Compare each variable with this table:

| Variable | Status |
|---|---|
| `LABEL`, `LABEL_VALUE`, `FOLDER`, `RESOURCE`, `NAMESPACE`, `FOLDER_ANNOTATION`, `METHOD` (`WATCH`, `LIST`), `UNIQUE_FILENAMES`, `DEFAULT_FILE_MODE`, `REQ_URL` (`http://`), `REQ_METHOD`, `REQ_USERNAME`, `REQ_PASSWORD`, `REQ_PAYLOAD`, `REQ_TIMEOUT`, `REQ_RETRY_TOTAL`, `LOG_LEVEL`, `LOG_FORMAT` | **supported**, same meaning |
| `IGNORE_ALREADY_PROCESSED`, `WATCH_SERVER_TIMEOUT`, `WATCH_CLIENT_TIMEOUT`, `ERROR_THROTTLE_SLEEP`, `REQ_RETRY_CONNECT`, `REQ_RETRY_READ`, `REQ_RETRY_BACKOFF_FACTOR`, `ENABLE_5XX` | **accepted and ignored** (logged at start-up). They tune kiwigrid's own internals; the result on disk is the same |
| `SCRIPT`, `METHOD=SLEEP`, `SLEEP_TIME`, `RESOURCE_NAME`, `SKIP_TLS_VERIFY=true`, `REQ_SKIP_TLS_VERIFY=true`, an `https://` `REQ_URL` | **not supported: the container refuses to start** (exit code 2, the reason on stderr). Keep kiwigrid for these containers |

If anything you use is in the last row, stop here for that container. Everything else can switch.

### Behaviour that differs (check if you rely on it)
- **Keys ending in `.url`:** kiwigrid downloads the URL and writes the content. k8s-sidecar-rs skips the key and logs a warning. If you ship dashboards as `.url` keys, put the JSON in the ConfigMap instead.
- **The folder annotation must stay inside `FOLDER`.** A relative path, or an absolute path inside `FOLDER`, works as before. `..` or an absolute path elsewhere makes the object be skipped, with a warning.
- **Two objects writing the same file:** kiwigrid lets the last write win. k8s-sidecar-rs picks the first by namespace, kind and name, whatever the event order, and logs a warning. With `UNIQUE_FILENAMES=true` this never happens.
- **The reload call is debounced:** one request after a burst of changes (up to 5 s after the first change), instead of one per object.
- **The image runs as user `65532`** with no shell. If your pod sets its own `runAsUser` (Grafana's chart uses 472), that wins. The container needs write access to `FOLDER`, as before.

RBAC is unchanged: `get`, `list`, `watch` on `configmaps` and/or `secrets`.

## 2. Switch

### Grafana's Helm chart (or kube-prometheus-stack)
Every sidecar the chart runs (dashboards, data sources, alerts, plugins, notifiers, and the `initDatasources` init container) uses one image setting:

```yaml
sidecar:                    # under `grafana:` in kube-prometheus-stack
  image:
    registry: ghcr.io
    repository: tagantroy/k8s-sidecar-rs
    tag: "0.1.1"
    sha: ""                 # clear it if you had pinned kiwigrid's digest
  resources:                # optional; it needs very little
    requests: {cpu: 5m, memory: 16Mi}
    limits: {memory: 64Mi}
```

Chart options that map to unsupported settings: `sidecar.skipTlsVerify: true`, `sidecar.dashboards.script`, `sidecar.datasources.script`, and `watchMethod: SLEEP` on any sidecar. Remove them, or keep kiwigrid.

### Your own Deployment
Change the `image:` of the sidecar container. Nothing else.

```yaml
- name: config-sync
  image: ghcr.io/tagantroy/k8s-sidecar-rs:0.1.1
  env:            # unchanged
    - {name: LABEL, value: my_config}
    - {name: FOLDER, value: /config}
```

## 3. Verify

1. **The pod starts.** If a sidecar container exits with code 2, read its log: it names the setting it doesn't support.
   ```sh
   kubectl logs <pod> -c <sidecar-container>
   ```
2. **The first sync finished.** The log has `initial sync done` with the number of objects, one `wrote` line per file, and `reload requested` if `REQ_URL` is set. A few `reload request failed` warnings right after the pod starts are normal: the app wasn't listening yet, and the retries take care of it.
3. **The files are there:**
   ```sh
   kubectl exec <pod> -c <app-container> -- ls -R <FOLDER>
   ```
   The image has no shell, so list the folder from another container in the pod that mounts it (in Grafana's chart, the `grafana` container).
4. **The app sees them.** For Grafana, the dashboard count is the easiest check:
   ```sh
   curl -s -u admin:<password> http://<grafana>/api/search?type=dash-db | jq length
   ```
   Compare it with the count before the switch.
5. **Changes still arrive.** Edit a labelled ConfigMap: within seconds the log shows `wrote` and `reload requested`.

Optional: set `METRICS_ADDR=0.0.0.0:9102` to get `/metrics` and `/healthz` (see the README). Don't add a readiness probe to a sidecar that shares a pod with your app: if the sidecar reported "not ready", your app would drop out of its Service.

## 4. Roll back

Put the previous image back (for Grafana's chart, remove the `sidecar.image` block) and redeploy. The files on disk are compatible both ways, so nothing needs cleaning up.

One exception: if you used `UNIQUE_FILENAMES`, both tools name files the same way. Without it, files from objects that lost a conflict may differ in which content won (see "Two objects writing the same file").
