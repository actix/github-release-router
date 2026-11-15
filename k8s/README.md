# Kubernetes deployment

These manifests run one replica as UID 10001 with a read-only filesystem. They mount configuration and credentials as files and expose a ClusterIP service. No persistent volume is required.

Run these commands from the project root.

## Configure the deployment

1. Edit `k8s/configmap.yaml`. Replace both instances of `example/app` with your repository's `owner/name`. Use the same source names, destinations, and route options described in the [configuration guide](../docs/configuration.md).
2. Edit `k8s/deployment.yaml`. Replace the placeholder image with your published image, preferably pinned to the digest from a successful promotion. The included container workflow publishes for `linux/amd64`; use an amd64 node or build an image for your cluster's architecture. For a private registry, configure image-pull credentials.
3. Create `secrets/github_webhook_secret` and `secrets/discord_webhook_url` as described in the [Compose setup](../README.md#deploy-with-docker-compose). You do not need a local `config.toml` for Kubernetes.

## Apply the resources

```sh
kubectl apply -f k8s/namespace.yaml
kubectl -n github-release-router create secret generic github-release-router-secrets \
  --from-file=github_webhook_secret=secrets/github_webhook_secret \
  --from-file=discord_webhook_url=secrets/discord_webhook_url
kubectl apply -k k8s/
kubectl -n github-release-router rollout status deployment/github-release-router
```

The [Secret template](secret.example.yaml) shows the required keys. It is excluded from `kustomization.yaml`; do not apply its placeholder values or commit credentials.

To check readiness locally:

```sh
kubectl -n github-release-router port-forward service/github-release-router 8080:8080
# In another terminal:
curl --fail http://127.0.0.1:8080/readyz
```

## Expose the webhook

Adapt `k8s/ingress.example.yaml` to your hostname, ingress controller, and TLS certificate. It uses nginx annotations and permits bodies up to 2 MiB. Create the TLS Secret with your certificate manager or cluster tooling, then apply the ingress:

```sh
kubectl apply -f k8s/ingress.example.yaml
```

Configure GitHub to send release events to `https://YOUR_HOST/webhooks/github/releases` with your webhook secret, as described in the [GitHub setup](../README.md#5-connect-github).

Keep one replica. Duplicate detection and Discord cooldowns are local to the process. The `Recreate` strategy prevents pods from overlapping. A deployment clears pending deliveries and recent history. After changing the ConfigMap or Secret, run `kubectl -n github-release-router rollout restart deployment/github-release-router`. See [operations](../docs/operations.md) for recovery steps.
