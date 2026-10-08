fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --locked -- -D warnings

test:
    cargo test --locked

run config="config.toml":
    cargo run --locked -- --config {{ config }} serve

preview payload="examples/release.json":
    cargo run --locked -- preview {{ payload }}

container-build:
    docker build -t github-release-router:local .

container-run:
    docker compose up --build -d

# Requires Docker, OpenSSL, and ripgrep. Uses no external network.
container-smoke image="github-release-router:local":
    ./scripts/docker-smoke.sh "{{ image }}"
