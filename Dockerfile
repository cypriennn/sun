FROM rust:1-slim AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock build.rs ./
RUN mkdir -p src /data \
    && printf 'use axum as _; use base64 as _; use bytes as _; use chrono as _; use dotenvy as _; use flowmaid as _; use futures_util as _; use http as _; use maud as _; use openidconnect as _; use pulldown_cmark as _; use pulldown_latex as _; use reqwest as _; use rmcp as _; use rustls as _; use serde as _; use serde_json as _; use sha2 as _; use tokio as _; use toml as _; use xberg as _;\nfn main() {}\n' > src/main.rs \
    && cargo build --release --locked \
    && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked

FROM gcr.io/distroless/cc-debian13:nonroot
WORKDIR /app
COPY --from=builder --chown=65532:65532 /data /app/data
COPY --from=builder /build/target/release/sun /usr/local/bin/sun
COPY --chown=65532:65532 config.example.toml /app/config.toml
ENV SUN_CONFIG=/app/config.toml SUN_BIND=0.0.0.0:3030
EXPOSE 3030
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/sun"]
