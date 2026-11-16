# Build with the official Rust image, run on distroless (no shell, non-root).
#   docker build -t k8s-sidecar-rs .
FROM rust:1.99.1-slim-trixie AS build
WORKDIR /src
# Dependencies first, so a source-only change doesn't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src
COPY src src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /src/target/release/k8s-sidecar /usr/local/bin/k8s-sidecar
# Numeric: Kubernetes can only verify runAsNonRoot for a numeric user.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/k8s-sidecar"]
