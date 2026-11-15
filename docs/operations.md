# Operations

## Health and logs

- `GET /healthz` returns `200` when the HTTP server is running.
- `GET /readyz` returns `200` when the delivery worker is active, or `503` otherwise. It does not check Discord connectivity.

Logs go to stderr. They include webhook decisions, delivery results, and retry attempts. They exclude credentials, webhook URLs, and request bodies. With Compose:

```sh
docker compose logs -f router
```

The default log level is `INFO`. Set `RUST_LOG=github_release_router=debug` for more delivery details. Compose passes this variable to the container. Avoid HTTP wire logging because it can expose credentials.

Sentry reporting is optional. Set `SENTRY_DSN` to enable it and `SENTRY_ENVIRONMENT` to label the deployment. Compose passes both variables through. Without a DSN, reporting is disabled. Preview does not initialize Sentry.

## Delivery and recovery

The service accepts signed GitHub release events and queues matching announcements before returning `202`. Other event types and release actions return `204`. An unmatched published release returns `202` without a job.

The queue holds up to 64 jobs, including the active request and delayed retries. If all new destinations cannot fit, the service returns `503` without adding jobs or recording the delivery ID. Redeliver the event after capacity becomes available.

One worker sends one request at a time. It waits for Discord to confirm message creation. Rate limits pause all destinations for the required delay. Network errors, HTTP `408`/`5xx`, and missing confirmation retry with exponential delay and jitter. Each job gets up to 12 attempts. Other HTTP errors fail immediately. Permanent and exhausted failures appear in the logs and in Sentry when enabled.

Keep one service instance. Queue state, cooldowns, and duplicate detection are held in memory. **Stopping the process discards pending jobs and recent delivery history.** HTTP `202` does not guarantee delivery. A lost response or a redelivery after restart can create a duplicate announcement.

Delivery IDs and successful `(repository, release ID, destination)` keys are each retained for up to 30 days, with a limit of 4,096 entries per cache. Active jobs stay deduplicated until completion. Edits are not reposted while a release key is retained.

To recover a failed delivery or one lost during a restart:

1. Fix the cause shown in the service logs.
2. Open the repository or organization webhook's **Recent deliveries** in GitHub.
3. Select the event and choose **Redeliver**.
4. Check the logs for a successful Discord delivery.

Failed destinations can be retried by redelivery. Successful destinations remain deduplicated while their keys are cached.

## Updates and shutdown

After changing configuration, recreate the service:

```sh
docker compose up --build -d --force-recreate --wait
curl --fail http://127.0.0.1:8080/readyz
```

Use `docker compose stop` to stop it. On SIGINT or SIGTERM, the service stops intake and waits for the active send to finish. An outgoing request has a 5-second connect timeout and a 30-second total timeout. Pending jobs and retries do not drain during shutdown. Before a planned restart, check delivery logs and allow accepted work to finish. Redeliver lost events afterward.

## Message format

Each announcement includes a linked release title, repository name, and notes. The tag is the fallback title when the release has no name. Supported Markdown is retained, and all mention parsing is disabled.

Notes that exceed the embed description limit use an excerpt and a complete `release-notes.md` attachment. Unicode truncation preserves character boundaries. The input body limit also bounds the attachment size.
