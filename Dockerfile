FROM lukemathwalker/cargo-chef:latest-rust-slim-trixie AS chef
WORKDIR /build

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --locked --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN cargo build --release --locked

FROM bitnami/minideb:trixie
RUN install_packages ca-certificates curl tini \
    && mkdir /data \
    && chown 10001:10001 /data
COPY --from=builder /build/target/release/github-release-router /usr/local/bin/github-release-router
USER 10001:10001
WORKDIR /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8080/readyz || exit 1
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["github-release-router", "--config", "/etc/router/config.toml", "serve"]
