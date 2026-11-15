# Development and container publishing

## Local commands

Install stable Rust and `just`. The toolchain file selects stable Rust with rustfmt and Clippy. Builds use the committed `Cargo.lock`.

```sh
just fmt
just fmt-check
just lint
just test
just preview
just preview path/to/webhook.json
just run config.toml
```

Preview renders a webhook payload without configuration, credentials, or network calls. Its JSON output contains `payload` (the Discord message) and `notes` (the full attachment, when needed). Tests use in-memory queues and local fake Discord servers.

## Containers

```sh
just container-build
just container-smoke
just container-run
```

The smoke test needs Docker, OpenSSL, and ripgrep. It checks startup as a non-root user with a read-only filesystem, health, readiness, duplicate detection, state reset after restart, and SIGTERM shutdown. It uses an isolated container with no external network and removes it when done.

## GitHub Actions

The [CI workflow](../.github/workflows/ci.yml) checks formatting, Clippy, and tests. The [container workflow](../.github/workflows/build.yml) builds for `linux/amd64`. Both use standard GitHub-hosted Ubuntu runners. Pull requests build and smoke-test the image without publishing it.

A push to `main` publishes a candidate to `ghcr.io/<owner>/<repository>:sha-<full-commit>`, using the current GitHub repository name. After CI and the build pass, the promotion job smoke-tests that exact image digest and updates `:stable` and `:latest` without rebuilding. Failed checks leave those tags unchanged. Manual runs publish and promote only on `main`.

The workflow uses `GITHUB_TOKEN` with package write permission. Enable GitHub Actions and permit package publishing in the repository or organization settings. New container packages are private by default; make the package public or configure registry credentials for your deployment. See [GitHub's container registry guide](https://docs.github.com/en/packages/working-with-a-github-packages-registry/working-with-the-container-registry).

For a registry deployment, use the digest recorded in the promotion job summary or the promoted `:stable` tag. Compose builds a local image by default; it does not pull the published image. The published image targets amd64. Build locally on another architecture.
