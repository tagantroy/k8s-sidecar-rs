# Contributing

Issues and pull requests are welcome. For anything bigger than a small fix, open an issue first
so we can agree on the approach.

## Ground rules
- **Compatibility with kiwigrid/k8s-sidecar:** settings keep kiwigrid's names and meaning. A
  kiwigrid setting we don't implement must be refused at start-up (or explicitly listed as
  ignored), never silently behave differently. Update the tables in `README.md` and
  `docs/migrating-from-kiwigrid.md` with any change.
- **Dependencies:** as few as possible, and only versions released at least two weeks ago
  (for new and updated crates alike). `cargo audit --deny warnings` must pass; an exception
  goes in `.cargo/audit.toml` with a reason and a way out.
- **Tests with every change:** unit tests for logic, and the end-to-end test for anything that
  touches the Kubernetes side.

## Building and testing
Needs Rust: the version in `rust-toolchain.toml`, which `rustup` installs and uses automatically. It's also the minimum we support (`rust-version` in `Cargo.toml`) and the Docker build's version; CI checks all three match, so upgrade Rust in all three at once and, for the end-to-end test, Docker that
allows privileged containers.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                                # unit tests
cargo test --test k3s -- --ignored        # end to end against a real k3s in Docker (~15 s)
cargo audit --deny warnings               # cargo install cargo-audit
docker build -t k8s-sidecar-rs .          # the image
python3 scripts/sizes.py k8s-sidecar-rs   # binary and image size (CI reports the change vs main)
```

CI runs all of these on every pull request.

## Licence
By contributing you agree that your contributions are licensed under the project's terms:
either the Apache License 2.0 or the MIT licence, at the user's option, without additional
terms.
