# Deployment examples

These container and Kubernetes manifests are legacy one-shot deployment examples, not the recommended production executor. Production `serve` uses Docker Sandboxes and currently requires a Docker Desktop host. The examples remain useful for CI and controlled single-run evaluation with `executor.kind=local`.

## Build

Choose and review an explicit Codex version:

```sh
docker build --build-arg CODEX_VERSION=X.Y.Z -t rustgrid-agent:local .
```

The image runs as UID/GID `65532`, contains Git, CA certificates, Tini, the RustGrid worker, and the pinned Codex package. The image does not contain credentials or Codex authentication state.

Release builds additionally require `RUST_BUILDER_IMAGE` and
`NODE_RUNTIME_IMAGE` to be immutable `name@sha256:digest` references configured
in the protected GitHub release environment.

## Docker Compose

Copy `.rustgrid-agent.example.json` to `deploy/agent.json`, set
`max_concurrency` to `1`, and perform device login once into a private bootstrap
directory. The login writes non-secret worker metadata to `agent.json` and the
credential to `deploy/credentials`:

```sh
mkdir -p deploy/credentials
RUSTGRID_CREDENTIAL_STORE=file \
RUSTGRID_CREDENTIALS_DIR="$PWD/deploy/credentials" \
rustgrid-agent --config "$PWD/deploy/agent.json" login --no-browser
chmod 700 deploy/credentials
chmod 600 deploy/credentials/*
sudo chown -R 65532:65532 deploy/credentials
```

Keep both paths outside source control, authenticate the `codex-home` volume
using your secret-management/bootstrap process, then run:

```sh
CODEX_VERSION=X.Y.Z docker compose -f deploy/compose.yml up --build --abort-on-container-exit
```

Schedule a newly created container for each poll. Do not restart a stopped container in place and do not persist the workspace tmpfs.

## Kubernetes

`deploy/kubernetes/worker-cronjob.yaml` creates a fresh pod for each queue poll with `concurrencyPolicy: Forbid`, an ephemeral workspace, a read-only root filesystem, dropped capabilities, resource limits, and no service-account token. Replace the image placeholder with a verified digest and provide:

- `rustgrid-agent` ConfigMap containing the device-authenticated `agent.json`;
- `rustgrid-agent-credentials` Secret created from the bootstrapped credential
  directory, preserving its generated filename;
- `rustgrid-agent-codex` PVC or a safer bootstrap mechanism containing only Codex authentication state.

For example:

```sh
kubectl create secret generic rustgrid-agent-credentials \
  --from-file=deploy/credentials
```

The init container copies the Secret into an ephemeral volume as UID `65532`
and enforces mode `0600` before the worker starts. This preserves the agent's
owner-only credential-file invariant without weakening Secret volume handling.

Apply an egress policy appropriate for the configured RustGrid endpoint, GitHub host, Codex service, and package registries needed by target repositories. Standard Kubernetes `NetworkPolicy` is IP/CIDR based and cannot safely express changing SaaS hostnames by itself; use a controlled egress proxy or CNI with FQDN policy.

## Long-lived serve mode

The included systemd unit is an operator reference. Configure the Docker Sandbox executor before starting `serve`; it refuses the local executor.
