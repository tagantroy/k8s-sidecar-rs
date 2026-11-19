# k8s-sidecar-rs

[![binary size](../../raw/badges/binary.svg)](../../actions/workflows/ci.yml) [![image size](../../raw/badges/image.svg)](../../actions/workflows/ci.yml)

A small Rust drop-in for [kiwigrid/k8s-sidecar](https://github.com/kiwigrid/k8s-sidecar): it watches ConfigMaps and Secrets with a label, writes their data as files into a folder, and calls a reload URL after changes. Grafana's Helm chart runs two copies of it (dashboards and data sources).

**Why:** the Python original uses ~80 MiB per copy. This one uses **1–2 MiB** (measured on a k3s cluster, below), in a single ~9 MB binary (the badges above show the current sizes, measured by CI on every change).

| | kiwigrid/k8s-sidecar | k8s-sidecar-rs |
|---|---|---|
| Memory: dashboards copy (26 ConfigMaps, 27 files, all namespaces) | 83 MiB | **2 MiB** |
| Memory: data-sources copy | 78 MiB | **1 MiB** |
| Image (compressed, amd64) | 29.2 MB (2.14.1) | 14.6 MB |
| Language | Python | Rust (`kube`, `tokio`) |
| Base | Python | distroless, non-root |

Measured 2026-11-16 in the same Grafana pod (kube-prometheus-stack, k3s 1.37) before and after switching the image: working-set memory from `kubectl top pod --containers`, as kept by Prometheus (`container_memory_working_set_bytes`). Together: ~161 MiB → ~3 MiB.

## Use with Grafana's Helm chart
Same environment variables as kiwigrid's, so only the image changes. Coming from kiwigrid? See the **[migration guide](docs/migrating-from-kiwigrid.md)**: what to check first, how to switch, verify and roll back.
```yaml
grafana:                      # under kube-prometheus-stack; top level in the grafana chart
  sidecar:
    image:
      registry: ghcr.io
      repository: tagantroy/k8s-sidecar-rs
      tag: "0.1.1"
```

## Settings
| Variable | Meaning | Default |
|---|---|---|
| `LABEL` | label the objects must have | required |
| `LABEL_VALUE` | the label's value; unset = any value | — |
| `FOLDER` | absolute folder to write into | required |
| `RESOURCE` | `configmap`, `secret` or `both` | `configmap` |
| `NAMESPACE` | `ALL`, or a comma-separated list | the pod's own namespace |
| `FOLDER_ANNOTATION` | annotation naming a sub-folder (relative, or absolute inside `FOLDER`) | `k8s-sidecar-target-directory` |
| `METHOD` | `WATCH` (keep in sync) or `LIST` (sync once and exit, e.g. as an init container) | `WATCH` |
| `UNIQUE_FILENAMES` | name files `namespace_<ns>.<kind>_<name>.<key>` | `false` |
| `DEFAULT_FILE_MODE` | octal file mode, e.g. `644` | process umask |
| `REQ_URL` | `http://` URL to call after changes (debounced: one call per burst) | — |
| `REQ_METHOD` | HTTP method for it | `GET` |
| `REQ_USERNAME`, `REQ_PASSWORD` | basic auth for it | — |
| `REQ_PAYLOAD` | JSON body for it | — |
| `REQ_TIMEOUT` | seconds per attempt | `10` |
| `REQ_RETRY_TOTAL` | retries (1 s, 2 s, 4 s … apart, max 30 s) | `5` |
| `LOG_LEVEL` | `DEBUG`, `INFO`, `WARN`, `ERROR` | `INFO` |
| `LOG_FORMAT` | `JSON` or `LOGFMT` | `JSON` |
| `METRICS_ADDR` | *(new)* serve `/metrics` and `/healthz` on e.g. `0.0.0.0:9102` | off |

### Differences from kiwigrid
**Refused at start-up** (rather than silently behaving differently): `SCRIPT`, `METHOD=SLEEP`, `RESOURCE_NAME`, `SKIP_TLS_VERIFY`/`REQ_SKIP_TLS_VERIFY`, `https://` reload URLs (the reload target runs in the same pod).

**Accepted and ignored** (logged): `IGNORE_ALREADY_PROCESSED`, `WATCH_SERVER_TIMEOUT`, `WATCH_CLIENT_TIMEOUT`, `ERROR_THROTTLE_SLEEP`, `REQ_RETRY_CONNECT`, `REQ_RETRY_READ`, `REQ_RETRY_BACKOFF_FACTOR`, `ENABLE_5XX`.

**Safer by design:**
- **The folder annotation can't escape `FOLDER`.** `..`, or an absolute path outside it, is refused and the object skipped. (Anyone who can create a labelled ConfigMap in a watched namespace controls that annotation.)
- **Keys ending in `.url` aren't downloaded** (kiwigrid fetches them): skipped with a warning.
- **Two objects writing the same file:** the first by namespace, kind and name wins, whatever the event order; a warning is logged, and when the winner goes away the next one takes over. kiwigrid: the last write wins.
- **Writes are atomic** (temporary file + rename) and skipped when the content is unchanged, so restarts and resyncs don't cause needless reloads. Directories left empty by a delete are removed.

## Metrics (with `METRICS_ADDR`)
`k8s_sidecar_objects`, `k8s_sidecar_files_written_total`, `k8s_sidecar_files_deleted_total`, `k8s_sidecar_skipped_total{reason}`, `k8s_sidecar_conflicts_total`, `k8s_sidecar_io_errors_total`, `k8s_sidecar_watch_errors_total`, `k8s_sidecar_reloads_total{result}`. `/healthz` returns 200 after the first full sync.

## RBAC
`get`, `list`, `watch` on `configmaps` and/or `secrets` in the watched namespaces (Grafana's chart already grants this).

## Development
```sh
cargo test                                # unit tests
cargo test --test k3s -- --ignored        # end to end against a real k3s (needs Docker)
```

## Contributing and security
See [CONTRIBUTING.md](CONTRIBUTING.md). Please report vulnerabilities privately, as described in [SECURITY.md](SECURITY.md).

## License
Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
