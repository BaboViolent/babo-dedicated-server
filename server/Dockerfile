FROM --platform=$BUILDPLATFORM rust:1-bookworm AS builder
ARG TARGETARCH
ARG SERVER_VERSION=development
ENV BV2_RELEASE_VERSION=$SERVER_VERSION
WORKDIR /build
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc-aarch64-linux-gnu libc6-dev-arm64-cross \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc
COPY server/Cargo.toml server/Cargo.lock* ./
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && cargo build --release --target "$target" \
    && rm -rf src
COPY server/src ./src
RUN case "$TARGETARCH" in \
      amd64) target=x86_64-unknown-linux-gnu ;; \
      arm64) target=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac \
    && touch src/main.rs \
    && cargo build --release --target "$target" \
    && cp "target/$target/release/bv2-server" /tmp/bv2-server

FROM debian:bookworm-slim
COPY --from=builder /tmp/bv2-server /usr/local/bin/bv2-server
COPY Content/main/maps /app/Content/main/maps
ENV BV2_BIND=0.0.0.0:8080
ENV BV2_CONTENT_ROOT=/app/Content/main
EXPOSE 8080
USER 65534:65534
ENTRYPOINT ["/usr/local/bin/bv2-server"]
