# Operating rustgrid-agent

## Deployment

Run one `serve` process per worker identity. The worker needs only its bound
`RUSTGRID_API_KEY`; GitHub credentials are issued per active run. Use a dedicated
unprivileged OS account and a writable workspace root.

The example systemd unit is in
`packaging/systemd/rustgrid-agent.service`. Configure:

```text
RUSTGRID_API_KEY=rgk_...
RUSTGRID_API_URL=https://app.rustgrid.com/api/v1
```

The configuration file should set `workspace_root` to durable local storage.
Successful workspaces are removed immediately. Failed, blocked, cancelled, and
interrupted workspaces are retained until `failed_workspace_retention_hours`.

## Health and alerts

Alert when any of these conditions occur:

- no worker heartbeat for three configured intervals;
- lease renewal has not succeeded inside the lease safety window;
- queue wait exceeds the service objective;
- progress event publishing repeatedly requires reconciliation;
- retained workspace disk usage exceeds 80%;
- token issuance returns 403, 409, or 502;
- the process restarts more than three times in ten minutes.

## Recovery

Each run stores `journal.json` beside its isolated repository. On a repeated
claim for the same run, the agent restores client/server event sequences and
reconciles the existing branch, commit, push, and open pull request. Never edit
the journal manually while the worker is running.

Lease loss is fail-closed: local execution is cancelled and no terminal ticket
or run update is attempted. The control plane decides whether to requeue or
reassign the run.

## Upgrade and rollback

1. Drain the worker by stopping new claims.
2. Allow the current run to finish or cancel it explicitly.
3. Replace the binary with the tagged release artifact and verify its published SHA-256 checksum.
4. Run `rustgrid-agent status`.
5. Start `serve` and confirm registration, heartbeat, and an empty claim poll.

Manifest version `1` is the current compatibility boundary. A worker refuses
unknown manifest versions. Roll back to the previous binary if registration or
manifest retrieval fails after an upgrade; retained workspaces remain available.

## Current server-owned follow-ups

The checked API does not expose a queue notification stream, an
`awaiting_review` ticket status, or manifest fields for command/time-limit
policy. The worker therefore polls `claim-next`, records `awaiting_review` as a
run phase, and uses locally configured commands and limits until those contracts
are added.
