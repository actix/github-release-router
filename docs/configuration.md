# Configuration

Start with [`config.example.toml`](../config.example.toml). Configuration is loaded once at startup. Restart the service after a change. Unknown fields, invalid regular expressions, and invalid references stop startup.

## Sources and destinations

A **source** defines a GitHub webhook secret and an explicit repository allowlist. Its name selects the endpoint: `[sources.releases]` accepts requests at `/webhooks/github/releases`. Source names can contain ASCII letters, digits, hyphens, and underscores.

Repository names use the `owner/name` format and match exactly, including letter case. Signed events for repositories outside the source allowlist return `403`. An authenticated GitHub ping does not need a repository, which permits organization webhook setup.

A **destination** defines a Discord webhook URL. Create a named destination for each webhook you want to use. URLs copied from Discord and URLs with `/api/v10/webhooks/` are accepted. The URL must use `https://discord.com`; suffixes such as `/github` and query strings are rejected.

## Secrets

Each secret must use exactly one reference:

```toml
secret = { file = "/run/secrets/github_webhook_secret" }
# Or: secret = { env = "GITHUB_WEBHOOK_SECRET" }

# Under a destination:
webhook = { file = "/run/secrets/discord_webhook_url" }
# Or: webhook = { env = "DISCORD_WEBHOOK_URL" }
```

Keep credential values out of TOML and source control. The service removes trailing CR/LF from secret values and preserves other bytes. Secret values and webhook URLs are excluded from application logs and queue records.

Compose mounts the two files in `secrets/` at `/run/secrets/`. For local execution, change the file references to local paths or use environment references. Add any extra secret files to the Compose service's `secrets` list and top-level `secrets` definitions, or to the Kubernetes Secret.

## Routes

Each `[[routes]]` entry needs a unique `name`, one `repository`, and one or more named `destinations`. The repository must appear in a source allowlist. All matching routes contribute destinations; overlapping routes send only one message to each destination.

To add a repository, add its `owner/name` to a source's `repositories` list and add a route:

```toml
# First add example/cli to the existing source allowlist:
# repositories = ["example/app", "example/cli"]

[[routes]]
name = "cli-releases"
repository = "example/cli"
destinations = ["releases"]
include_prereleases = true
```

Configure a repository webhook for each repository, or use an organization webhook. Reuse a source when hooks share a secret. Create a new source with an independent secret when hooks need separate credentials. Destination names can refer to webhooks in different Discord channels or servers.

## Tag filters

By default, a route allows all tags and excludes prereleases. Set `include_prereleases = true` to announce prereleases too. Only the `published` release action creates announcements.

Use optional regular expressions to select tags within a route:

```toml
include_tags = ["^app-v", "^v[0-9]+[.][0-9]+[.][0-9]+$"]
exclude_tags = ["-(internal|test)$"]
```

Patterns are case-insensitive and can match anywhere in the tag. Use `^` and `$` to match the complete tag. A tag must match at least one inclusion pattern, unless `include_tags` is omitted or empty. Any matching exclusion rejects the tag for that route. Other routes can still match.

For example, these filters include `app-v1.0.0` and `V1.0.0`, but exclude `app-v1.0.0-internal` and `APP-v1.0.0-TEST`. Omit `exclude_tags` or use `[]` to exclude no tags.

## HTTP listener

`bind` defaults to `0.0.0.0:8080`. Use an IP address and port. TLS ends at your reverse proxy. The service accepts JSON bodies up to 2 MiB and rejects content encodings so it can verify the original signed bytes.
