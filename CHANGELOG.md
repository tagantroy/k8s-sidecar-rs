# Changelog

## 0.1.1 (unreleased)
- Image runs as numeric user `65532` instead of `nonroot`, so pods with `runAsNonRoot: true` but no `runAsUser` can start it.

## 0.1.0 (unreleased)
First version: a drop-in for the kiwigrid/k8s-sidecar settings that Grafana's Helm chart uses (dashboards and data sources). See the README for what is and isn't supported.
