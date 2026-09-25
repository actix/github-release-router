FROM lukemathwalker/cargo-chef:latest-rust-slim-trixie AS chef

#############
## Planner ##
#############
FROM chef AS planner
WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples

RUN cargo chef prepare --recipe-path recipe.json

#############
## Builder ##
#############
FROM chef AS builder
WORKDIR /build

COPY --from=planner /build/recipe.json recipe.json

RUN cargo --locked chef cook --release --recipe-path recipe.json

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples

RUN cargo --locked build --release

############
## Runner ##
############
FROM bitnami/minideb:trixie
WORKDIR /app

RUN install_packages ca-certificates curl tini

COPY --from=builder /build/target/release/github-release-router /usr/local/bin/github-release-router
USER 10001:10001

EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8080/readyz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["github-release-router", "--config", "/etc/router/config.toml", "serve"]
