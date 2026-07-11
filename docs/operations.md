# Operating rustgrid-agent

## Deployment

The production-safe container examples under `deploy/` run `watch --once` in a
fresh pod/container so the runtime boundary is destroyed after at most one run.
This is the recommended deployment until a dedicated remote executor exists.

Run one `serve` process per worker identity. The worker needs only its bound
`RUSTGRID_API_KEY`; GitHub credentials are issued per active run. Use a dedicated
unprivileged OS account and a writable workspace root.

Set `max_concurrency` to 1. The current in-process executor cannot establish a
separate filesystem and process boundary for concurrent runs, so `serve` fails
closed when a higher value is configured. Higher production concurrency requires
an external executor that launches every run in its own container or equivalent
runtime boundary; separate workspace directories alone are not isolation.

The example systemd unit is in
`packaging/systemd/rustgrid-agent.service`. Configure:

```text
RUSTGRID_API_KEY=rgk_...
RUSTGRID_API_URL=https://app.rustgrid.com/api/v1
RUSTGRID_AGENT_ISOLATION=per_run
```

`RUSTGRID_AGENT_ISOLATION=per_run` is a fail-closed deployment assertion, not
a sandbox implementation. Set it only after the runtime gives every run its own
filesystem boundary and CPU, memory, process, disk, and network controls.
`serve` refuses to start without it.

The configuration file should set `workspace_root` to durable local storage.
Successful workspaces are removed immediately. Failed, blocked, cancelled, and
interrupted workspaces are retained until `failed_workspace_retention_hours`.
Set `max_workspace_bytes` below the host disk alert threshold and use an OS or
container disk quota for enforcement while commands are actively writing.
The worker also applies Unix child limits for address space, individual file
size, open files, CPU time, wall time, and captured output. These limits are
defense in depth and do not replace per-run containers or host quotas.

Use `rustgrid-agent status --json` from process-manager readiness checks. It
reports configuration, credential presence, workspace location, and capacity
without exposing secrets. It also authenticates to RustGrid and resolves the
configured project; unhealthy JSON output is still printed before the command
exits non-zero. Local interactive telemetry is colorized; set
`NO_COLOR=1` for plain logs or `RUSTGRID_AGENT_LOG=json` for newline-delimited
structured lifecycle events collected by a service manager.

## External production boundaries

Complete artifact bundles require an artifact-upload endpoint in the RustGrid
worker API. Central OTLP export requires a deployment-selected collector and
credentials. Continuous disk, CPU, memory, and network enforcement belongs to
the host or container runtime; the worker performs bounded capture, policy
validation, and before/after workspace checks. A release candidate is not
production-approved until it completes a credentialed staging ticket against a
real RustGrid project and GitHub App installation.

## Health and alerts

Alert when any of these conditions occur:

- no worker heartbeat for three configured intervals;
- lease renewal has not succeeded inside the lease safety window;
- queue wait exceeds the service objective;
- progress event publishing repeatedly requires reconciliation;
- retained workspace disk usage exceeds 80%;
- token issuance returns 403, 409, or 502;
- GitHub rate limits repeatedly exhaust all bounded retries;
- the process restarts more than three times in ten minutes.

## Recovery

Each run stores `journal.json` beside its isolated repository. On a repeated
claim for the same run, the agent restores client/server event sequences and
reconciles the existing branch, commit, push, and open pull request. Never edit
the journal manually while the worker is running.

After a process restart, the worker first lists active runs assigned to its
worker ID and resumes up to its configured concurrency before consuming new
queue entries. Run cancellation is isolated: losing one lease stops only that
run, while the worker and its other active runs continue heartbeating.

Lease loss is fail-closed: local execution is cancelled and no terminal ticket
or run update is attempted. The control plane decides whether to requeue or
reassign the run.

SIGTERM requests a drain, stops new claims, and waits for active runs. SIGINT
requests immediate cancellation and terminates the complete Unix child process
group. Captured output is bounded by
`max_command_output_bytes`; the recovery journal retains the terminal diagnostic.

## Upgrade and rollback

1. Drain the worker by stopping new claims.
2. Allow the current run to finish or cancel it explicitly.
3. Replace the binary with the tagged release artifact; verify its published
   SHA-256 checksum and GitHub artifact attestation, and retain the SPDX SBOM.
4. Run `rustgrid-agent status`.
5. Start `serve` and confirm registration, heartbeat, and an empty claim poll.

Manifest version `2` is the current compatibility boundary. A worker refuses
unknown manifest versions. Roll back to the previous binary if registration or
manifest retrieval fails after an upgrade; retained workspaces remain available.

The worker advertises configured concurrency on every heartbeat, consumes the
durable queue stream with replay, and falls back to bounded polling only while
the stream is unavailable. Claimed runs use only the snapshotted server policy
and finish in `awaiting_review` after publishing the pull request.
