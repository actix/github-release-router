# GitHub release router

A small Rust service that receives signed GitHub webhooks and sends release announcements to Discord. Source repositories need no GitHub Actions workflow. One Actix Web process serves HTTP and runs one delivery worker. SQLite stores the queue and deduplication records.

## Setup

1. In Discord server **771444961383153695**, open **#web → Edit Channel → Integrations → Webhooks → New Webhook**. Select #web and copy the webhook URL. You need permission to manage webhooks. Use the regular Execute Webhook URL, never its `/github` suffix.
2. Copy `config.example.toml` to `config.toml`. Create a `secrets` directory. Put the Discord URL in `secrets/discord_webhook_url`. Create two independent random GitHub secrets, for example with `openssl rand -hex 32`, and save them as `secrets/actix_github_secret` and `secrets/personal_github_secret`. Do not commit these files. Secret files must be readable by container UID 10001; protect their parent directory on the host. Trailing CR/LF is removed; other bytes are retained.
3. Run `docker compose up --build -d`. The named volume inherits `/data` ownership from the image. An existing bind mount instead needs write access for UID/GID 10001. Keep one service instance per database. Do not share the SQLite files over a network filesystem.
4. Put an HTTPS reverse proxy in front of `127.0.0.1:8080`. Forward `/webhooks/github/` without rewriting the body. Set a request limit of at least 2 MiB and preserve the GitHub headers. TLS ends at the proxy. The container binds to `0.0.0.0:8080`; Compose exposes it only on host loopback. A proxy in another container can use `http://router:8080` on the Compose network instead.
5. In GitHub repository **Settings → Webhooks → Add webhook**, set the URL to `https://YOUR_HOST/webhooks/github/actix` for actix/actix-web, or `/webhooks/github/personal` for robjtede/actix-web-lab. Select **application/json**, enter the corresponding secret, keep SSL verification enabled, and select **Releases** under individual events. Organization hooks use **Organization Settings → Webhooks** and the same endpoint and secret. Their source allowlist still applies to each release event. GitHub sends a ping when the hook is created; an authenticated ping returns `200 pong`.

The service accepts URLs copied from Discord (`https://discord.com/api/webhooks/ID/TOKEN`) and versioned URLs (`https://discord.com/api/v10/webhooks/ID/TOKEN`). It never requests GitHub data or uses a Discord bot token.

## Configuration

TOML defines named `sources`, named `destinations`, and `[[routes]]`. Unknown fields and invalid references fail startup. Each secret uses exactly one reference: `{ env = "VARIABLE_NAME" }` or `{ file = "/path/to/secret" }`. Config files contain no credential values. Secret values and webhook URLs are excluded from logs and stored queue records. Configuration is loaded once; restart to apply changes. Queued jobs use the current URL for their destination name, so keep names stable when rotating tokens.

A source has an explicit repository allowlist. Unauthorized repository events return 403. A ping is source-authenticated but need not contain a repository, which permits organization pings. Other events need an allowed `repository.full_name`. Non-release events and release actions other than `published` return 204 without creating jobs.

Routes compare `repository.full_name` exactly and case-sensitively. A configured `tag_prefix` uses a case-sensitive prefix match. **Omit `tag_prefix` to include all releases from the repository.** Prereleases match only routes with `include_prereleases = true`. All matching routes contribute destinations; each destination receives one message even when routes overlap. An unmatched published release returns 202 with no jobs.

To add the third, fourth, or later repository, add its real `owner/name` to a source allowlist and add a route. Use a new source and secret if the hook needs a different trust boundary. Add named destinations to send to more channels, then list those destination names in routes. The example intentionally includes only the two known repositories.

## Delivery and recovery

The signature extractor adapts the [Actix Web Lab Discord example](https://github.com/robjtede/actix-web-lab/blob/main/actix-web-lab/examples/discord_webhook.rs): `RequestSignature<Json<Value>, GithubSignature>` passes original payload chunks to HMAC-SHA256 while decoding JSON. `verify_slice` compares the digest in constant time. Missing, duplicate, malformed, or wrong signatures fail authentication. Content encodings are rejected so decompression cannot change signed bytes. Incoming JSON is limited to **2 MiB**, including chunked bodies.

Published releases are committed in a SQLite transaction before HTTP 202. Discord calls occur only in the worker. SQLite uses WAL and FULL synchronous mode. Delivery IDs are deduplicated globally; jobs also have a unique `(repository, release ID, destination)` key. A redelivery with a new delivery ID cannot repeat a recorded release announcement. The first accepted payload wins; edits are not reposted. Retain the database when replacing a container.

Concurrency is fixed at **one outgoing request**. The shared HTTP client has a 5-second connect timeout, a 30-second request timeout, and redirects disabled. Every request sets `wait=true` and requires a returned message ID. HTTP 429 uses the larger of `retry_after` and `Retry-After`, rounded up with a one-second margin. A persistent shared cooldown pauses all destinations, including across restarts. Network failures, HTTP 408/5xx, and absent creation confirmation retry with exponential delay and jitter (2 seconds initially, at most 1,125 seconds). Each job gets 12 attempts; remaining failures become `failed`. Other HTTP errors fail immediately. A missing destination also fails explicitly.

SIGINT/SIGTERM stops intake and lets the active send finish, up to its timeout. Pending jobs remain durable. Startup restores interrupted `sending` jobs to `pending`. **Delivery is not exactly once:** Discord can accept a message before the process records success, or a response can be lost. Retrying that job can create a duplicate.

Once per hour, the service deletes incoming delivery IDs older than 30 days and removes message bodies from successful jobs older than 30 days. Compact release/destination records remain indefinitely to prevent old releases from being announced again. Pending and failed bodies remain until delivery succeeds. Monitor disk space; SQLite reuses freed pages but does not shrink the file automatically. Back up with SQLite's online backup API, or stop the service and copy the database together with its WAL files. Do not copy only the live main database file.

Inspect and retry outgoing failures:

```sh
docker compose exec router github-release-router --config /etc/router/config.toml failed
docker compose exec router github-release-router --config /etc/router/config.toml retry 42
# Local equivalents:
cargo run --locked -- --config config.toml failed
cargo run --locked -- --config config.toml retry 42
```

Inspection returns IDs, repositories, routes, destination names, attempts, and safe result labels. Retry resets one failed job's attempt budget. It does not bypass a Discord cooldown. Fix the cause first. Retry commands can run while the server is active and do not recover `sending` jobs.

[GitHub does not automatically redeliver failed webhook deliveries](https://docs.github.com/en/webhooks/using-webhooks/handling-failed-webhook-deliveries). If this service is unavailable, reports a storage error, or rejects an event due to a configuration error, fix the cause, open the repository or organization webhook's **Recent deliveries**, select the delivery, and choose **Redeliver**. This differs from retrying a persisted outgoing Discord failure. Monitor GitHub delivery failures as well as the service logs.

## Messages and operations

An embed shows the release name as its linked title, plus the repository and notes. Discord displays the message time. The tag is used as the title only when GitHub provides no release name. Supported Markdown is retained. Empty notes have a fallback. All mention parsing is disabled. Notes longer than the description limit get an excerpt and a complete `release-notes.md` attachment in the same multipart message.

The renderer budgets 256 UTF-16 units each for title and repository and 4,096 for description. Their sum stays below Discord's 6,000-character total embed limit. Unicode truncation preserves character boundaries. See [Discord embed limits](https://docs.discord.com/developers/resources/message#embed-limits) and [Execute Webhook](https://docs.discord.com/developers/resources/webhook#execute-webhook). The 2 MiB input limit also bounds the notes attachment.

- `GET /healthz`: 200 when HTTP is running; it does not check Discord or SQLite.
- `GET /readyz`: 200 when the delivery worker is active and SQLite responds; otherwise 503. Discord failures do not take down readiness.
- Tracing logs go to stderr as text with named fields and no application timestamp. The default level is `INFO`. HTTP handlers run in a request span that records the method, route, status, and request ID. Logs show startup, authentication rejections, webhook decisions, queue recovery, and delivery results. Retries include the attempt, reason, delay, and destination name. Set `RUST_LOG=github_release_router=debug` to include job claims, Discord response status codes, safe network error classes, database maintenance, and failed readiness checks. Logs exclude secrets, webhook URLs, request bodies, and raw HTTP client errors. Do not enable HTTP wire logging in production.
- Monitor failed jobs, queue age, disk space, readiness, and GitHub's recent deliveries. No metrics service is required.

## Local development and preview

Use stable Rust. `rust-toolchain.toml` selects stable; the Docker builder pins a stable compiler version. `Cargo.lock` is included and all builds use `--locked`.

```sh
just fmt
just fmt-check
just lint
just test
just preview                       # No config, credentials, or network calls
just preview path/to/webhook.json
just run config.toml
just container-build
just container-run
```

For local execution, set `database = "data/router.sqlite"` and use local secret files or environment references. Preview emits JSON containing `payload` (the Discord message) and `notes` (the full attachment, when needed). Tests use temporary SQLite files and loopback fake Discord servers. They never post to Discord.

Run `just container-smoke` after `just container-build` to check non-root startup, health, readiness, single-instance locking, queue persistence, restart deduplication, and SIGTERM shutdown. This script needs Docker, OpenSSL, the SQLite CLI, and ripgrep. It creates and removes its own container and volume. Its container has `--network none`, so no test can reach Discord.

## Container publishing

Pushing to `main` runs the [container workflow](.github/workflows/publish-container.yml). It builds the locked Dockerfile and publishes `ghcr.io/robjtede/github-release-router:latest` and a `sha-<commit>` tag. The workflow can also be started manually; `latest` is published only from the default branch. It uses the repository's `GITHUB_TOKEN` with package write permission. A new GHCR package is private by default, so make it public or grant your cluster image-pull access before deploying it.

## Kubernetes example

The [`k8s/`](/Users/rob/Development/github-release-router/k8s/) directory contains example resources for a single-replica deployment. It uses a `ReadWriteOnce` PVC for SQLite, mounts the TOML configuration read-only, mounts credentials as files, runs as UID 10001, and exposes a ClusterIP service. `deployment.yaml` uses a placeholder image; replace it with the image published from your registry. `ingress.example.yaml` is an optional nginx example and must be adapted to the ingress controller and certificate manager in use.

Create the Secret without committing credentials. For example, use `k8s/secret.example.yaml` as a template or generate it with `kubectl create secret generic`; do not apply the placeholder values. The example Secret is deliberately excluded from `kustomization.yaml`. Then apply the Secret and base resources:

```sh
kubectl apply -f k8s/namespace.yaml
kubectl -n github-release-router create secret generic github-release-router-secrets \
  --from-file=actix_github_secret \
  --from-file=personal_github_secret \
  --from-file=discord_webhook_url
kubectl apply -k k8s/
kubectl -n github-release-router rollout status deployment/github-release-router
kubectl -n github-release-router port-forward service/github-release-router 8080:8080
```

Configure GitHub hooks against the HTTPS ingress URL, for example `https://releases.example.com/webhooks/github/actix`. Keep one pod and one writer for the PVC. Back up SQLite using the same guidance as Compose, and scale down before moving the PVC to another node or changing storage. `kubectl delete -k k8s/` removes the example resources but leaves the PVC data unless the storage class reclaim policy removes it; verify your cluster policy before deleting production resources.
