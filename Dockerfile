FROM rust:1.98-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY examples ./examples
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 router \
    && useradd --uid 10001 --gid router --no-create-home router \
    && mkdir /data \
    && chown router:router /data
COPY --from=builder /build/target/release/github-release-router /usr/local/bin/github-release-router
USER 10001:10001
WORKDIR /data
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD curl --fail --silent http://127.0.0.1:8080/readyz || exit 1
ENTRYPOINT ["github-release-router"]
CMD ["--config", "/etc/router/config.toml", "serve"]
