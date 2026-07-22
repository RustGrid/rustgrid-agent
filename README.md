# rustgrid-agent

[![CI](https://github.com/RustGrid/rustgrid-agent/actions/workflows/ci.yml/badge.svg)](https://github.com/RustGrid/rustgrid-agent/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`rustgrid-agent` is the worker that turns RustGrid tickets into ready-to-review
GitHub pull requests. RustGrid assigns a run, the worker executes Codex inside an
isolated Docker Sandbox, validates the result with repository-specific quality
gates, publishes the branch and pull request, and reports progress and token
consumption back to RustGrid.

This repository contains the worker, not the RustGrid control plane or AgentOps
web application. Running it requires a compatible RustGrid instance and a
project connected to GitHub through the RustGrid GitHub App.

> [!IMPORTANT]
> `1.0.0` is the first stable public artifact release. Publishing the CLI,
> Homebrew formula, and container does not approve a worker deployment for
> production. Operators must complete the credentialed checks in
> [staging certification](docs/staging-certification.md) against the published
> image digest before production deployment.

## What it does

For each run, the worker:

1. Authenticates a tenant-scoped worker and maintains its heartbeat and run
   lease.
2. Receives a run assigned by RustGrid, then validates its versioned execution
   manifest and policy hash.
3. Obtains a short-lived, repository-scoped GitHub App token and creates an
   isolated clone under the configured workspace root.
4. Stages clean ticket attachments and builds a Codex prompt from the ticket,
   comments, custom fields, prior gate failures, and repository instructions.
5. Runs the exact Codex command and model selected by RustGrid inside a dedicated
   Docker Sandbox microVM.
6. Publishes concise Codex updates, ordered lifecycle events, run steps, and
   aggregate token consumption to RustGrid.
7. Runs required local gates, repairs eligible failures, commits only
   agent-created changes, and waits for required GitHub workflows.
8. Opens or updates a pull request, attaches it to the ticket, and moves the
   ticket to `awaiting_review`. Failures that require intervention move it to
   `blocked` and retain the workspace for recovery.

The directory from which the worker is launched is not used as the run
repository. Every run gets its own clone, branch, journal, and sandbox.

## Platform support

| Host | CLI | Production `serve` | Distribution |
| --- | --- | --- | --- |
| macOS | Supported | Supported on hosts that meet the current Docker Sandboxes requirements | Homebrew tap and checksummed native archive |
| Ubuntu Linux | Supported | Supported when Docker Sandboxes, KVM, and worker preflight checks pass | Checksummed `linux-x86_64` native archive |
| Windows | Not currently supported or tested | Not supported | No native artifact |

The local executor is for development and tests only. Production `serve` fails
closed unless `executor.kind` is `docker_sandbox`. See Docker's current
[Docker Sandboxes prerequisites](https://docs.docker.com/ai/sandboxes/get-started/)
before provisioning a host.

## Requirements

- Git and a configured Git author name and email.
- A RustGrid account allowed to authorize a worker through device login.
- A RustGrid project connected to a GitHub repository through the RustGrid
  GitHub App.
- For production: Docker Sandboxes (`sbx`) 0.34.0 or newer, a working microVM
  runtime, an effective network policy, and Codex authentication stored through
  the Docker credential proxy.
- Rust 1.94 or newer when building from source. Release binaries and Homebrew
  installations do not require Rust at runtime.

## Install

### From source

Install Rust with [`rustup`](https://rust-lang.org/tools/install/) and then:

```sh
git clone https://github.com/RustGrid/rustgrid-agent.git
cd rustgrid-agent
rustup toolchain install 1.94 --profile minimal
cargo install --locked --path .
rustgrid-agent --version
```

Cargo installs the executable into `~/.cargo/bin` by default. Restart the shell
or add that directory to `PATH` if `rustgrid-agent` is not found.

### Tagged releases

Stable releases publish checksummed and attested source, macOS, and Linux
artifacts on [GitHub Releases](https://github.com/RustGrid/rustgrid-agent/releases).
Verify the matching `.sha256` file and GitHub artifact attestation before
installing a native binary. The exact artifact names and controls are documented
in [the release workflow](.github/workflows/release.yml) and
[release checklist](docs/release-checklist.md).

### Homebrew

The public tap command is:

```sh
brew install RustGrid/tap/rustgrid-agent
```

The unqualified `brew install rustgrid-agent` command will only be documented
if a formula is later accepted into Homebrew/core.

## Production quick start

### 1. Install Docker Sandboxes

On a supported macOS host:

```sh
brew trust docker/tap
brew install docker/tap/sbx
```

On a supported Ubuntu Linux host:

```sh
curl -fsSL https://get.docker.com | sudo REPO_ONLY=1 sh
sudo apt-get install docker-sbx
sudo usermod -aG kvm "$USER"
newgrp kvm
```

Then authenticate Docker and Codex and initialize a balanced network policy:

```sh
sbx login
sbx secret set -g openai --oauth
sbx policy init balanced
sbx diagnose
```

Review the effective network rules before admitting production work. Docker's
[Codex authentication guide](https://docs.docker.com/ai/sandboxes/agents/codex/)
explains the host-side credential proxy; the OpenAI credential is not copied
into the sandbox.

### 2. Configure and log in to RustGrid

Run the guided setup once. It detects the host's logical CPUs and memory,
recommends a balanced concurrent-job count, and derives the per-sandbox resource
settings:

```sh
rustgrid-agent setup
```

Press Enter to accept the recommendation or enter a different number. For an
unattended host or a later capacity change, pass the basic setting directly:

```sh
rustgrid-agent setup --max-concurrency 4
```

Setup writes the stable user configuration to
`$XDG_CONFIG_HOME/rustgrid-agent/config.json`, or
`~/.config/rustgrid-agent/config.json` when `XDG_CONFIG_HOME` is unset. It is
safe to rerun: worker identity and login metadata are preserved while detected
CPU and memory capacity are refreshed. On the first run it imports a legacy
`.rustgrid-agent.json` from the current directory without deleting the original.

Then log in:

```sh
rustgrid-agent login
```

The default control-plane instance is `https://app.rustgrid.com`. Login prints a
one-time code, opens the verification URL returned by RustGrid, waits for
approval, and stores worker identity alongside the setup configuration. If
setup was skipped, login still creates a single-run production Docker Sandbox
configuration at the stable user path. Use `--no-browser` on a headless host:

```sh
rustgrid-agent login --no-browser
```

Use a different compatible control plane only when required:

```sh
rustgrid-agent login --instance https://rustgrid.example.com
```

The `/api/v1` suffix is optional. `RUSTGRID_INSTANCE_URL` provides the same
override for managed hosts; the legacy exact `RUSTGRID_API_URL` override has
highest precedence.

The scoped worker credential is stored in macOS Keychain or Linux Secret
Service. If the OS store is unavailable, the agent warns and uses an atomic,
owner-only file in the user's configuration directory. No secret is written to
`.rustgrid-agent.json`.

### 3. Validate and start the worker

The stable user configuration works from any directory:

```sh
rustgrid-agent status
rustgrid-agent serve
```

`status` verifies the configuration, credential, remote worker state, Docker
Sandbox client and daemon, template and Codex pins, network policy, capacity,
and RustGrid connectivity. It exits non-zero when the host is not production
ready. `serve` then heartbeats the worker, recovers active assignments, consumes
the durable queue, and runs up to `max_concurrency` isolated jobs.

For a service manager, start with the example
[systemd unit](packaging/systemd/rustgrid-agent.service) and the
[production operations guide](docs/operations.md).

## Model and execution settings

The model is not selected from `.rustgrid-agent.json`:

1. A user selects an allowed model in AgentOps.
2. RustGrid validates that model against its server-side catalog.
3. RustGrid snapshots `--model <id>` into the run's signed execution policy.
4. The worker verifies the policy hash and executes that exact command.

The `executor.codex_version` setting pins the **Codex CLI version**, not the
model. The selected immutable Docker Sandbox template must already contain that
exact version; the worker verifies `codex --version` before starting the run and
never installs or upgrades Codex while a ticket is starting. Local `codex_command`,
`quality_gate_command`, and timeout fields are retained only for configuration
compatibility and cannot override a claimed run.

RustGrid also owns the quality gates, timeouts, environment allowlist, required
GitHub workflows, repository binding, and sandbox policy in each manifest. See
[the worker run contract](docs/worker-api-contract.md) for the full schema.

## Configuration

The default path is `$XDG_CONFIG_HOME/rustgrid-agent/config.json`, or
`~/.config/rustgrid-agent/config.json`. When that file does not exist, the CLI
continues to recognize a legacy `.rustgrid-agent.json` in the current directory.
`RUSTGRID_AGENT_CONFIG` or the global `--config` option selects an explicit
path; the option may appear before or after the subcommand:

```sh
rustgrid-agent --config /etc/rustgrid-agent/agent.json status
rustgrid-agent status --config /etc/rustgrid-agent/agent.json
```

`setup` is the supported way to create and resize a production configuration.
It detects host capacity and keeps the detailed sandbox resource fields in sync.
`login` creates a conservative single-run configuration when the file does not
exist. By contrast, deserializing an explicitly minimal configuration uses the
development-only local executor unless `executor` is set.

| Field | Default or constraint | Purpose |
| --- | --- | --- |
| `instance_url` | `https://app.rustgrid.com` | RustGrid control-plane origin. A trailing `/api/v1` is accepted and normalized away. |
| `installation_id` | Generated UUID | Stable, non-secret identity for this installation. |
| `worker_id`, `worker_name`, `tenant_id` | Written by `login` | Non-secret metadata for the linked tenant worker. |
| `credential_store` | Written by `login` | Non-secret source metadata such as `os_keychain` or `private_file_fallback`. |
| `credential_expires_at_unix` | Written by `login` | Non-secret credential expiry used by readiness checks. |
| `project_key`, `project_id` | Deprecated and ignored | Legacy project hints. Workers are tenant-scoped. |
| `repo.owner`, `repo.name` | Deprecated and ignored for runs | Legacy bootstrap hints. The manifest repository is authoritative. |
| `default_base_branch` | `main` | Fallback only when a claimed manifest omits its default branch. |
| `quality_gate_command`, `codex_command` | Deprecated and ignored for runs | Accepted for compatibility; the manifest owns these commands. |
| `heartbeat_interval_seconds` | `15`; range `5..=300` | Worker heartbeat and lease-renewal interval. |
| `max_concurrency` | `1`; range `1..=100` | Capacity advertised to RustGrid. Values above one require per-run Docker Sandbox isolation. |
| `executor.kind` | `local` in a minimal file | `local` is development-only; production requires `docker_sandbox`. |
| `executor.command` | `sbx` | Docker Sandboxes executable. |
| `executor.template` | Digest-pinned default | Production rejects mutable tags and requires a 64-character `@sha256:` digest. |
| `executor.codex_version` | `0.142.4` | Exact numeric Codex CLI version, for example `0.142.4`. Must match the pinned template. |
| `executor.cpus`, `executor.memory` | `4`, `8g` | CPU and memory allocated to each run sandbox. |
| `executor.capacity_cpus`, `executor.capacity_memory` | `4`, `8g` | Host capacity reserved for all concurrent worker sandboxes. Startup rejects overcommit. |
| `lease_seconds` | `900`; range `30..=86400` | Requested run lease. Must exceed three heartbeat intervals. |
| `workspace_root` | OS temp directory under `rustgrid-agent/workspaces` | Durable parent for isolated clones, journals, and retained run state. Set an explicit durable path in production. |
| `command_timeout_seconds`, `run_timeout_seconds` | Deprecated | Accepted for compatibility; manifest timeouts are authoritative. |
| `failed_workspace_retention_hours` | `72`; maximum `720` | Retention for unsuccessful workspaces and stopped sandboxes. |
| `max_command_output_bytes` | `8388608`; minimum `65536` | Combined in-memory command-output budget. |
| `max_workspace_bytes` | `5368709120`; minimum `67108864` | Maximum run-workspace size. The worker monitors active sandbox execution. |
| `max_child_memory_bytes` | `8589934592`; minimum `268435456` | Unix child address-space ceiling for local execution. |
| `max_child_file_bytes` | `1073741824`; minimum `1048576` | Unix per-child file-size ceiling for local execution. |
| `max_child_open_files` | `1024`; range `64..=65536` | Unix per-child open-file ceiling for local execution. |

Unknown JSON fields, invalid UUIDs, insecure instance URLs, empty required
values, invalid resource sizes, and unsafe executor combinations are rejected.
Use [`.rustgrid-agent.example.json`](.rustgrid-agent.example.json) as the
capacity-oriented production example. Never put secrets in this file.

## Environment variables

| Variable | Purpose |
| --- | --- |
| `RUSTGRID_AGENT_CONFIG` | Stable configuration path override. `--config` takes precedence. |
| `RUSTGRID_INSTANCE_URL` | Overrides the configured RustGrid control-plane origin. |
| `RUSTGRID_API_URL` | Legacy, highest-precedence exact API URL, including custom proxy prefixes. |
| `RUSTGRID_CREDENTIALS_DIR` | Overrides the private-file credential directory for a service account. |
| `RUSTGRID_CREDENTIAL_STORE=file` | Forces the owner-only file store instead of the OS keychain. |
| `RUSTGRID_AGENT_LOG=json` | Emits newline-delimited structured lifecycle logs. |
| `NO_COLOR=1` | Disables terminal color in human-readable output. |

Worker identity and authentication are loaded only from the configuration and
the credential store populated by `rustgrid-agent login`. Environment variables
cannot override either value.

The device-authenticated credential requests these runtime permissions:

- `projects:read`, `tickets:read`, `tickets:update`, `comments:read`, and
  `comments:create`;
- `agents:workers:heartbeat`;
- `agents:runs:read`, `agents:runs:create`, `agents:runs:claim`, and
  `agents:runs:update`;
- read/create/update/delete permissions for agent steps, links, and quality
  gates.

It deliberately excludes worker registration, worker credential
administration, and API-key administration permissions. GitHub credentials are
issued per run, held only in memory, and scoped by RustGrid to the manifest
repository.

## Commands

### `login`

```sh
rustgrid-agent login
rustgrid-agent login --no-browser
rustgrid-agent login --instance https://rustgrid.example.com
```

Starts device authorization and creates the configuration if needed. The
one-time code expires and is never itself a worker credential.

### `logout`

```sh
rustgrid-agent logout
```

Revokes the current bound credential server-side before removing the local
copy. Network or server failures retain the local credential so logout can be
retried safely. Repeated logout is safe.

### `register`

```sh
rustgrid-agent register
```

Deprecated compatibility command that heartbeats an already authenticated
worker. New installations should use `login`.

### `status`

```sh
rustgrid-agent status
rustgrid-agent status --json
```

Shows local and remote readiness without exposing secrets. JSON output uses
schema version `1`, is printed even for an unhealthy worker, and exits non-zero
when production checks fail.

### `run`

```sh
rustgrid-agent run <ticket-uuid>
```

Claims and processes one ticket manually. The identifier is the RustGrid ticket
UUID, not a display key such as `RG-1`. The target repository still comes from
the resulting run manifest; the launch directory is never staged or committed.

### `watch`

```sh
rustgrid-agent watch
rustgrid-agent watch --interval 30
rustgrid-agent watch --once
```

Consumes assigned runs across all authorized tenant projects, up to
`max_concurrency`. `--interval` controls the empty-queue wait and defaults to 15
seconds. `--once` performs one reconciliation cycle, requires
`max_concurrency=1`, and is useful for schedulers and smoke tests. Unlike
`serve`, `watch` does not enforce the production executor preflight.

### `serve`

```sh
rustgrid-agent serve
rustgrid-agent serve --interval 30
```

Production entrypoint. It validates Docker Sandbox isolation, resumes active
leases before taking new work, and supervises every run independently. SIGTERM
drains active runs without accepting new assignments. SIGINT cancels active
child process groups and exits safely.

## Run lifecycle, recovery, and telemetry

Manifest version `2` owns the repository, input prompt, selected model, Codex
command, quality gates, timeouts, environment allowlist, sandbox policy, GitHub
permissions, and required workflows. The worker rejects identity mismatches,
unknown versions, invalid policy hashes, unpinned execution settings, and
repository origins that do not match the manifest.

Lifecycle events are published in monotonically increasing client sequence and
reconciled against RustGrid after ambiguous responses. Recovery state is
checkpointed atomically at:

```text
<workspace_root>/<workspace-id>/journal.json
```

The journal records the run and ticket identity, phase, event cursors, branch,
commit, pull request, retained executor, recovery lineage, last error, and token
consumption. Do not edit it while a worker is running.

Successful runs destroy their sandbox and workspace after the terminal RustGrid
update succeeds. Failed, blocked, cancelled, timed-out, and interrupted runs
retain their workspace and stop the sandbox for the configured retention period.
RustGrid can explicitly adopt that state in a later attempt with
`metadata.resume_from_run_id`; the worker never guesses recovery lineage. A
manifest with `metadata.fresh_start: true` always uses a new run-specific branch
and cannot include recovery lineage.

Codex JSONL `turn.completed` usage is accumulated across every Codex and repair
turn. At terminal finalization the worker idempotently writes `input_tokens`,
`cached_input_tokens`, `output_tokens`, and `total_tokens` to
`PUT /agent-runs/{run_id}/token-consumption`. Cached input is a subset of input,
and total consumption is input plus output. Successful runs require this report;
unsuccessful runs attempt it before terminal failure handling.

For coding missions, the worker first checks out the repository and then
classifies the mission as `configuration`, `single_file`, `multi_file`, or
`repository_wide`. Classification selects an explicit ownership and focused
validation plan plus separate limits for initial prompt, inference turns, tool
calls, peak context, fresh/cached cumulative input, output, and Codex duration.
At 70%, 90%, and 100% the worker records advisory budget telemetry without
interrupting the active Codex session or discarding its context. Initial prompts
that already exceed an advisory budget ask Codex to minimize broad discovery,
but task completion and correctness take priority over the budget estimate.

The initial coding prompt includes the complete ticket context and applicable
repository instructions. Codex starts with targeted inspection but may expand
to relevant source and tests when correctness requires it. Dependency bootstrap,
full repository tests/builds, commit, publication, and GitHub checks remain
worker-owned. Successful dependency state and full-gate results are reused only
while their manifest, lockfile, command, and source-tree fingerprints match.
Validated direct metadata operations remain the only path that intentionally
skips repository checkout and Codex.

Human-readable lifecycle logs are the default. Use `RUSTGRID_AGENT_LOG=json`
for service-manager collection. RustGrid receives bounded progress, step,
quality-gate, ticket-comment, and terminal summaries; complete artifact upload
and central OTLP export remain external deployment integrations described in
[telemetry and data handling](docs/telemetry.md).

## Security model

- Production runs execute in separate Docker Sandbox microVMs. Only the
  disposable run clone crosses the filesystem boundary.
- Codex uses `danger-full-access` only inside that disposable microVM. Local
  development retains Codex `workspace-write`.
- RustGrid API calls, GitHub token acquisition, commits, pushes, and pull-request
  publication stay in the trusted coordinator.
- The execution policy can allow only explicitly named environment variables.
  Protected credential and process-control names are rejected.
- Allowed values cross into the sandbox through a private, shell-quoted,
  short-lived file under the run clone's `.git` directory. The file is deleted
  after execution and cannot be committed.
- Repository Git hooks are disabled for coordinator-owned clone, branch,
  commit, rebase, and publication operations.
- Only paths changed inside the isolated clone are staged; the runner never uses
  `git add .` against an operator checkout.
- GitHub tokens are short-lived, repository-scoped, never placed in remote URLs,
  and refreshed before expiry.
- Command output, workspace growth, run time, and local Unix child resources are
  bounded. Production host quotas remain required as defense in depth.
- Every terminal cleanup failure is visible. Unsuccessful work is retained
  rather than silently discarded.

The worker executes untrusted repository code. Read
[the threat model](docs/threat-model.md),
[architecture and trust boundaries](docs/architecture.md), and
[security policy](SECURITY.md) before operating it with production access.

## RustGrid API contract

The configured API base is `/api/v1`. The worker uses these endpoint groups:

| Purpose | Method and path |
| --- | --- |
| Start and poll device login | `POST /agent-workers/device-authorizations`; `POST /agent-workers/device-authorizations/token` |
| Revoke current device credential | `POST /agent-workers/{worker_id}/credentials/current/revoke` |
| Inspect and heartbeat the worker | `GET /agent-workers`; `POST /agent-workers/{worker_id}/heartbeat` |
| Replay and stream the assignment queue | `GET /agent-workers/{worker_id}/queue`; `GET /agent-workers/{worker_id}/queue/stream` |
| Recover active assigned runs | `GET /agent-workers/{worker_id}/runs?status=running` |
| Fetch ticket context | `GET /tickets/{ticket_id}`; comments and quality-gate result collections |
| Claim a ticket manually | `POST /tickets/{ticket_id}/agent-runs/claim` |
| Fetch run policy and attachments | `GET /agent-runs/{run_id}/manifest`; attachment and variant download targets |
| Obtain and refresh GitHub access | `POST /agent-runs/{run_id}/github-token` |
| Renew a run lease | `POST /agent-runs/{run_id}/lease` |
| Publish and replay lifecycle events | `POST` and `GET /agent-runs/{run_id}/events` |
| Append run steps | `POST /agent-runs/{run_id}/steps` |
| Report token consumption | `PUT /agent-runs/{run_id}/token-consumption` |
| Update ticket and run status | `PATCH /tickets/{ticket_id}`; `PATCH /agent-runs/{run_id}` |
| Report gates and attach the pull request | `POST /tickets/{ticket_id}/quality-gate-results`; `POST /tickets/{ticket_id}/external-links` |

Mutations that can be retried use idempotency keys. Ticket and run updates use
strong ETags and `If-Match`. All authenticated calls use bearer credentials;
device authorization start and token polling are intentionally unauthenticated.
The committed [OpenAPI snapshot](openapi.current.json) pins the runtime contract
assertions used by CI. The [worker API contract](docs/worker-api-contract.md)
and [device-authentication contract](docs/device-authentication.md) document the
run and login boundaries used by the client.

## Troubleshooting

- **`login` is an unrecognized subcommand:** compare `command -v
  rustgrid-agent`, `rustgrid-agent --version`, and `cargo run -- --help`. The
  executable on `PATH` is usually older than the checkout; reinstall with
  `cargo install --locked --path . --force`.
- **The browser opens the wrong device URL:** the CLI opens
  `verification_uri_complete` exactly as returned by RustGrid. Check the
  control-plane AgentOps/browser URL configuration rather than changing the
  worker's API origin.
- **`status` reports an executor failure:** run `sbx diagnose`, confirm `sbx`
  0.34.0 or newer, inspect the active policy, verify KVM access on Linux, and
  confirm the configured Codex version matches the pinned template.
- **Codex cannot authenticate:** run `sbx secret set -g openai --oauth` as the
  same OS account that runs the worker. Sandboxes do not inherit `~/.codex`.
- **Token consumption remains zero:** verify the manifest invokes Codex, inspect
  JSON lifecycle logs for valid `turn.completed` usage, and verify RustGrid
  accepts `PUT /agent-runs/{run_id}/token-consumption`.
- **No work is assigned:** confirm the worker is online in AgentOps, the project
  has an enabled GitHub repository binding, and the run is assigned to this
  worker. Queue events are wake-up hints; the active-run collection is the
  source of truth.
- **A run fails after changing code:** inspect the retained workspace and final
  diagnostics. Required local gates and GitHub workflows receive bounded repair
  attempts before the ticket is blocked.

More operational failure modes and recovery procedures are in
[production operations](docs/operations.md).

## Documentation

- [Architecture and trust boundaries](docs/architecture.md)
- [Device authentication](docs/device-authentication.md)
- [Worker API contract](docs/worker-api-contract.md)
- [Compatibility policy](docs/compatibility.md)
- [Production operations](docs/operations.md)
- [Container deployment](deploy/README.md)
- [Known limitations](docs/known-limitations.md)
- [Telemetry and data handling](docs/telemetry.md)
- [Staging certification](docs/staging-certification.md)
- [Release checklist](docs/release-checklist.md)
- [Roadmap](ROADMAP.md) and [changelog](CHANGELOG.md)

## Development

Install the pinned toolchain and run the same core checks as CI:

```sh
rustup toolchain install 1.94 --profile minimal --component clippy,rustfmt
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo package --locked --allow-dirty
bash scripts/check-secrets.sh
```

CI runs on Linux and macOS, validates the container and deployment manifests,
checks dependency policy with `cargo-deny`, verifies the release package, and
asserts required RustGrid contracts in the OpenAPI snapshot.

Before opening a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md) and the
[Code of Conduct](CODE_OF_CONDUCT.md). Use GitHub Issues for reproducible bugs,
GitHub Discussions for community support, and the private process in
[SECURITY.md](SECURITY.md) for vulnerabilities. See [SUPPORT.md](SUPPORT.md) for
the support boundary.

## Release process

Pushing a matching semantic-version tag triggers the protected
[release workflow](.github/workflows/release.yml). It refuses private source,
runs locked quality gates, and is designed to publish:

- a source crate, checksum, SPDX SBOM, and provenance attestation;
- checksummed and attested native Linux and macOS archives;
- a generated Homebrew formula committed to `RustGrid/homebrew-tap`; and
- a scanned and attested `ghcr.io/rustgrid/rustgrid-agent` image.

Maintainers must complete [the release checklist](docs/release-checklist.md) and
must not replace assets for an existing version. Homebrew/core publication is a
separate third-party review and is not assumed by this project.

The workflow publishes versioned artifacts; it does not deploy a worker.
Production deployment approval is a separate gate performed against the exact
published image digest through [staging certification](docs/staging-certification.md).

## License

Licensed under the [MIT License](LICENSE).
