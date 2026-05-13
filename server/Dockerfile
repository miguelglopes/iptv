FROM rust:1.94-slim-bookworm AS builder
WORKDIR /build

COPY Cargo.toml ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/iptv-proxy target/release/iptv_proxy* target/release/deps/iptv_proxy*

COPY src ./src
RUN touch src/main.rs && cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl tini && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /srv/iptv-proxy
COPY --from=builder /build/target/release/iptv-proxy /usr/local/bin/iptv-proxy

ENV RUST_LOG=info,iptv_proxy=info
EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=5s --start-period=20s --retries=3 \
    CMD curl -fsS http://localhost:8080/api/status >/dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "iptv-proxy"]
