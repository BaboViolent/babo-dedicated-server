FROM rust:1-bookworm AS builder
WORKDIR /build
COPY server/Cargo.toml server/Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY server/src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
COPY --from=builder /build/target/release/bv2-server /usr/local/bin/bv2-server
COPY Content/main/maps /app/Content/main/maps
ENV BV2_BIND=0.0.0.0:8080
ENV BV2_CONTENT_ROOT=/app/Content/main
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=3s CMD curl -f http://127.0.0.1:8080/health || exit 1
USER nobody
ENTRYPOINT ["/usr/local/bin/bv2-server"]
