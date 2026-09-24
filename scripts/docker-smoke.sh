#!/bin/sh
set -eu

# No external network: the test cannot send an announcement to Discord.
image=${1:-github-release-router:local}
smoke_dir=$(mktemp -d)
name="release-router-smoke-$(basename "$smoke_dir" | tr '[:upper:]' '[:lower:]')"
volume="$name-data"
cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    docker volume rm "$volume" >/dev/null 2>&1 || true
    rm -rf "$smoke_dir"
}
trap cleanup EXIT INT TERM
chmod 755 "$smoke_dir"
cp examples/release.json "$smoke_dir/release.json"
cat > "$smoke_dir/config.toml" <<'CONFIG'
database = "/data/router.sqlite"
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
docker volume create "$volume" >/dev/null

start() {
    docker run --detach --name "$name" --network none --read-only \
        --cap-drop ALL --security-opt no-new-privileges:true \
        --mount "type=volume,source=$volume,target=/data" \
        --mount "type=bind,source=$smoke_dir,target=/etc/router,readonly" \
        --env TEST_GITHUB_SECRET=test-secret \
        --env TEST_DISCORD_URL=https://discord.com/api/webhooks/123/test-only \
        "$image" >/dev/null
    attempts=0
    until docker exec "$name" curl --fail --silent http://127.0.0.1:8080/readyz >/dev/null; do
        attempts=$((attempts + 1))
        if [ "$attempts" -ge 30 ]; then docker logs "$name"; exit 1; fi
        sleep 1
    done
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
    docker logs "$name" 2>&1 | rg -q 'shutdown requested'
}

start
if docker exec "$name" github-release-router --config /etc/router/config.toml serve > "$smoke_dir/second.log" 2>&1; then
    echo 'Second server unexpectedly acquired the database' >&2
    exit 1
fi
rg -q 'another server holds this database lock' "$smoke_dir/second.log"
post first-delivery
stop
docker cp "$name:/data" "$smoke_dir/before" >/dev/null
test "$(sqlite3 "$smoke_dir/before/router.sqlite" 'SELECT count(*) FROM jobs;')" = 1
test "$(sqlite3 "$smoke_dir/before/router.sqlite" "SELECT count(*) FROM jobs WHERE state = 'pending';")" = 1
docker rm "$name" >/dev/null

start
post second-delivery
stop
docker cp "$name:/data" "$smoke_dir/after" >/dev/null
test "$(sqlite3 "$smoke_dir/after/router.sqlite" 'SELECT count(*) FROM jobs;')" = 1
test "$(sqlite3 "$smoke_dir/after/router.sqlite" 'SELECT count(*) FROM incoming;')" = 2
echo 'Docker smoke passed: non-root startup, health, readiness, instance lock, durable queue, restart deduplication, graceful shutdown.'
