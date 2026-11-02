# syntax=docker/dockerfile:1

FROM rust:1.99-bookworm AS builder
WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends pkg-config \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY admin_dist ./admin_dist
RUN cargo build --release --locked \
    && mkdir -p /data \
    && touch /data/.keep \
    && chown -R 65532:65532 /data

FROM gcr.io/distroless/cc-debian12:nonroot

WORKDIR /app
COPY --from=builder /build/target/release/jproxy-rs /usr/local/bin/jproxy-rs
COPY --from=builder /build/admin_dist /app/admin_dist
COPY --from=builder --chown=65532:65532 /data /data

ENV DATABASE_PATH=/data/jproxy.db \
    LISTEN_ADDR=0.0.0.0:8117 \
    ADMIN_UI_DIR=/app/admin_dist \
    RUST_LOG=jproxy_rs=info,tower_http=info

EXPOSE 8117
VOLUME ["/data"]

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/jproxy-rs"]
