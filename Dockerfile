# Build with the official Rust image, run on distroless (no shell, non-root).
#   docker build -t k8s-sidecar-rs .
#   docker buildx build --platform linux/amd64,linux/arm64 -t k8s-sidecar-rs .
#
# Multi-arch by cross-compiling: the build stage always runs on the build machine's own
# architecture ($BUILDPLATFORM) and compiles for $TARGETARCH with Debian's cross toolchain.
# The final stage only copies the binary, so no emulation (QEMU) is needed for any target.
FROM --platform=$BUILDPLATFORM rust:1.99.1-slim-trixie AS build
ARG TARGETARCH
WORKDIR /src
RUN set -eu; \
    case "$TARGETARCH" in \
      amd64) triple=x86_64-unknown-linux-gnu;  gnu=x86_64-linux-gnu;  gcc=gcc-x86-64-linux-gnu ;; \
      arm64) triple=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; gcc=gcc-aarch64-linux-gnu ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    if [ "$(dpkg --print-architecture)" != "$TARGETARCH" ]; then \
      apt-get update; \
      apt-get install -y --no-install-recommends "$gcc" "libc6-dev-$TARGETARCH-cross"; \
      rm -rf /var/lib/apt/lists/*; \
    fi; \
    rustup target add "$triple"; \
    echo "$triple" > /target-triple; \
    # The target's C compiler (ring), for the target only: a plain CC would also be used for
    # build scripts that run on the build machine. On a native build it's the host's own.
    echo "CC_$(echo "$triple" | tr - _)=$gnu-gcc" > /target-env; \
    printf '[target.%s]\nlinker = "%s-gcc"\n' "$triple" "$gnu" > /usr/local/cargo/config.toml
# Dependencies first, so a source-only change doesn't rebuild them.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && env "$(cat /target-env)" cargo build --release --locked --target "$(cat /target-triple)" \
 && rm -rf src
COPY src src
RUN touch src/main.rs src/lib.rs \
 && env "$(cat /target-env)" cargo build --release --locked --target "$(cat /target-triple)" \
 && cp "target/$(cat /target-triple)/release/k8s-sidecar" /k8s-sidecar

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /k8s-sidecar /usr/local/bin/k8s-sidecar
# Numeric: Kubernetes can only verify runAsNonRoot for a numeric user.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/k8s-sidecar"]
