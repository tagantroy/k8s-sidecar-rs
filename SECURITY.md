# Security

k8s-sidecar-rs writes files into a pod based on ConfigMaps and Secrets in the namespaces it
watches, and can read Secrets. Bugs that let someone write outside `FOLDER`, read objects it
shouldn't, or make it call a URL it wasn't configured for are security issues.

## Reporting a vulnerability
Please **don't open a public issue**. Use GitHub's private reporting instead: the repository's
**Security** tab → **Report a vulnerability**. Include the version, your settings (environment
variables, minus secrets) and, if you can, a ConfigMap or Secret that reproduces it.

You'll get an answer within a week. Fixes are released as a new version with a note in
`CHANGELOG.md`.

## Supported versions
Only the latest release. The project is 0.x: upgrade to get fixes.

## What the project does to stay safe
- The folder annotation and data keys can't lead outside `FOLDER` (tested).
- Keys ending in `.url` are never fetched; the reload call only goes to the configured
  `http://` URL.
- The image is distroless and runs as a non-root numeric user, with no shell.
- Dependencies are at least two weeks old when added or updated, and CI runs `cargo audit`
  on every push and weekly; it fails on vulnerable, unmaintained or yanked crates.
