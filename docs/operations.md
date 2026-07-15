# Operating rustgrid-agent

## Deployment

Production uses the standalone Docker Sandboxes CLI. Install Docker Desktop and
`sbx`, authenticate Codex through Docker's credential proxy, and configure
`executor.kind` as `docker_sandbox`.

Run one `serve` process per worker identity. The worker needs only its bound
`RUSTGRID_WORKER_API_KEY`; GitHub credentials are issued per active run. Use a dedicated
unprivileged OS account and a writable workspace root.

Set `max_concurrency` from measured host capacity. Each claimed run receives its
own microVM with configured CPU and memory limits. The local executor is limited
to one run and is rejected by `serve`.

Set `capacity_cpus` and `capacity_memory` to the capacity reserved for this
worker. Startup rejects configurations where concurrent sandbox allocations can
exceed either ceiling. Pin `template` by verified `@sha256:` digest; tags are
rejected by production readiness. Pin `codex_version` to an exact numeric
release. Use `sbx` 0.34.0 or newer.

At sandbox creation, the coordinator materializes a coordinator-owned local mixin kit
outside the mounted repository. The kit installs `@openai/codex` at the exact
configured version. Sandbox admission then requires `codex --version` to match
exactly. A retained sandbox with another version receives the kit through
`sbx kit add` before reuse; failed upgrades preserve the sandbox and fail the
run as transient infrastructure. Worker startup also fails closed unless
`kit.allowLocalKits` is enabled. The kit explicitly allows the npm registry and
sets bounded fetch retry/backoff variables for every sandbox process. This
covers transient `gateway.docker.internal` DNS failures that occur after the
initial `npm ping` admission probe.

The example systemd unit is in
`packaging/systemd/rustgrid-agent.service`. Configure:

```text
RUSTGRID_WORKER_API_KEY=rgk_...
RUSTGRID_WORKER_ID=00000000-0000-4000-8000-000000000000
RUSTGRID_API_URL=https://app.rustgrid.com/api/v1
```

Long-running workers must use `RUSTGRID_WORKER_API_KEY` with a credential
bound to the exact pre-announced identity in `RUSTGRID_WORKER_ID`. Startup
heartbeats that worker and fails closed when the binding does not match. The credential is required for leased run events, manifests, and
run-scoped GitHub token issuance.

The configuration file should set `workspace_root` to durable local storage.
Successful workspaces are removed immediately. Failed, blocked, cancelled, and
interrupted workspaces are retained until `failed_workspace_retention_hours`.
Their Docker Sandboxes are stopped without being removed and journaled as
recoverable. If RustGrid retries the same run ID, the worker restarts that
sandbox through `sbx exec` and preserves its installed dependencies and internal
filesystem state. A new run can adopt the retained work from an earlier attempt
when its execution manifest contains an explicit lineage marker:

```json
{
  "run": {
    "attempt": 2,
    "metadata": { "resume_from_run_id": "<failed-run-uuid>" }
  }
}
```

The source must belong to the same ticket, be unsuccessful and terminal, and
have a successfully retained executor. Adoption atomically transfers ownership
of the workspace to the new run, reuses the stopped sandbox, preserves Git and
publication checkpoints, and resets run-scoped event and step sequences. The
worker never guesses a recovery source from ticket history. Successful runs
destroy the sandbox only after the terminal RustGrid update succeeds.
Set `max_workspace_bytes` below the host disk alert threshold and use an OS or
host disk quota for enforcement while commands are actively writing.
The worker also monitors workspace growth while sandbox commands run and stops
the sandbox if `max_workspace_bytes` is crossed. Retain a host filesystem quota
as defense against races and writes outside the polling interval.
The worker also applies Unix child limits for address space, individual file
size, open files, CPU time, wall time, and captured output. These limits are
defense in depth for local development and do not replace host quotas.
Codex additionally has an output-inactivity watchdog. The signed execution
policy may set `codex.idle_timeout_seconds`; when omitted, the worker uses the
smaller of 300 seconds and the total run timeout. Crossing that deadline kills
the command process tree and restarts a fresh ephemeral Codex process against
the same retained workspace, up to three attempts. Exhaustion reports a
timed-out run and leaves the sandbox recoverable by a later run attempt.

When the signed execution policy contains an npm-family quality gate, every
newly created Docker Sandbox must pass an `npm ping` against the public registry
before it is admitted for Codex or quality-gate execution. Failed admission
destroys and recreates the sandbox up to three times with bounded backoff.
Exhaustion is reported as transient worker infrastructure failure, not as a
repository defect or human-action blocker. Non-JavaScript runs do not acquire
an npm dependency merely because they share the worker.

Use `rustgrid-agent status --json` from process-manager readiness checks. It
reports configuration, credential presence, tenant scope, workspace location,
and capacity without exposing secrets. It also authenticates to RustGrid and
verifies access to the worker's active-run recovery collection; unhealthy JSON output is still printed before the command
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

Publication also reconciles concurrent updates to the generated remote branch.
If the remote is ahead of the agent commit, the local branch fast-forwards. If
the histories diverge, the coordinator rebases the runner-owned commit onto the
fetched remote head, reruns all quality gates, atomically checkpoints the new
commit, and retries a normal push up to three times. A conflict aborts the
rebase and retains the clean pre-rebase workspace for a human-assisted retry.

Publication also fetches the manifest-selected remote base branch immediately
before every initial or CI-repair push and rebases the complete agent commit
range when the base advanced. If the agent branch already exists, the rewritten
history is pushed only with an exact force-with-lease for the previously
observed SHA. A stale lease is a concurrency signal and must not be bypassed
with an unconditional force push.

Required validation is iterative. Local quality gates collect all required
failures before returning diagnostics to Codex, with three total validation
attempts. Once a pull request exists, a failed required GitHub workflow starts
up to three CI repair iterations. The worker fetches failed job and step details
and bounded log tails, asks Codex to repair the retained workspace, reruns local
gates, commits the repair, pushes the existing branch, and waits on the new SHA.
If the final attempt still fails, the ticket is marked blocked and the sandbox
is retained for human inspection. Workflow timeouts, cancellation, and lease
loss do not masquerade as code-repair iterations.

After a process restart, the worker first lists actively leased runs assigned to
its worker ID across every project in the tenant and resumes up to its configured
concurrency before consuming new queue entries. Run cancellation is isolated:
losing one lease stops only that run, while the worker and its other active runs
continue heartbeating.

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
