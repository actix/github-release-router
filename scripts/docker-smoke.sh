#!/bin/sh
set -eu

# No external network: the test cannot send an announcement to Discord.
image=${1:-github-release-router:local}
smoke_dir=$(mktemp -d)
name="release-router-smoke-$(basename "$smoke_dir" | tr '[:upper:]' '[:lower:]')"
cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -rf "$smoke_dir"
}
trap cleanup EXIT INT TERM
chmod 755 "$smoke_dir"
cp examples/release.json "$smoke_dir/release.json"
cat > "$smoke_dir/config.toml" <<'CONFIG'
[sources.test]
repositories = ["actix/actix-web"]
secret = { env = "TEST_GITHUB_SECRET" }
[destinations.web]
webhook = { env = "TEST_DISCORD_URL" }
[[routes]]
name = "smoke"
repository = "actix/actix-web"
destinations = ["web"]
CONFIG
chmod 644 "$smoke_dir"/*
signature=$(openssl dgst -sha256 -hmac test-secret "$smoke_dir/release.json" | awk '{print $NF}')

start() {
    docker run --detach --name "$name" --network none --read-only \
        --cap-drop ALL --security-opt no-new-privileges:true \
        --mount "type=bind,source=$smoke_dir,target=/etc/router,readonly" \
        --env NO_COLOR=1 \
        --env TEST_GITHUB_SECRET=test-secret \
        --env TEST_DISCORD_URL=https://discord.com/api/webhooks/123/test-only \
        "$image" >/dev/null
    attempts=0
    until docker exec "$name" curl --fail --silent http://127.0.0.1:8080/readyz >/dev/null; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 30 ]; then docker logs "$name"; exit 1; fi
        sleep 1
    done
    test "$(docker exec "$name" cat /proc/1/comm)" = tini
    test "$(docker exec "$name" id -u)" = 10001
    test "$(docker exec "$name" curl --fail --silent http://127.0.0.1:8080/healthz)" = ok
}

post() {
    status=$(docker exec "$name" curl --silent --output /dev/null --write-out '%{http_code}' \
        -H 'Content-Type: application/json' -H "X-Hub-Signature-256: sha256=$signature" \
        -H "X-GitHub-Delivery: $1" -H 'X-GitHub-Event: release' \
        --data-binary @/etc/router/release.json http://127.0.0.1:8080/webhooks/github/test)
    test "$status" = 202
}

stop() {
    docker stop --time 45 "$name" >/dev/null
    test "$(docker inspect --format '{{.State.ExitCode}}' "$name")" = 0
    docker logs "$name" 2>&1 | rg -q 'Shutdown requested'
}

start
post first-delivery
post first-delivery
post second-delivery
stop
docker logs "$name" > "$smoke_dir/before.log" 2>&1
rg -q 'result="queued"' "$smoke_dir/before.log"
rg -q 'result="duplicate_delivery"' "$smoke_dir/before.log"
rg -q 'result="duplicate_release"' "$smoke_dir/before.log"
docker rm "$name" >/dev/null

start
post first-delivery
stop
docker logs "$name" > "$smoke_dir/after.log" 2>&1
rg -q 'result="queued"' "$smoke_dir/after.log"
echo 'Docker smoke passed: non-root read-only startup, health, readiness, deduplication, cleared state after restart, graceful shutdown.'
