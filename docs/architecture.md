# Architecture

`rustgrid-agent` is a control-plane worker that turns a leased RustGrid run into a reviewed GitHub pull request. RustGrid remains authoritative for tenant context, manifests, execution policy, leases, and run-scoped GitHub credentials.

## Components

- **Coordinator:** registers the worker, consumes the durable queue, pauses claims while degraded, and drains on shutdown.
- **Supervisor:** renews the worker heartbeat and run lease independently of long-running child processes.
- **Execution:** creates a Docker Sandbox microVM around a dedicated clone, runs Codex and required gates there, and commits only agent-created paths from the trusted coordinator.
- **Publishing:** reconciles the branch, push, pull request, and required GitHub workflows.
- **Reporting:** writes the durable journal and publishes sequenced events, steps, comments, ticket states, and run states.
- **Finalization:** maps one typed terminal outcome to cleanup and external side effects.

## Run sequence

```text
queue claim -> manifest validation -> token issuance -> isolated clone
     -> sandbox create -> Codex -> sandbox gates -> sandbox destroy
     -> commit -> push -> pull request
     -> required workflows -> awaiting_review -> cleanup
```

Every irreversible publication checkpoint is written atomically to `journal.json`. A restarted worker derives a recovery plan and reconciles existing Git and GitHub state rather than repeating side effects.

## Trust boundaries

RustGrid and GitHub are trusted external control planes. Ticket content, repository content, Codex output, child processes, and network responses are untrusted. Docker Sandbox provides the production microVM boundary. Only the disposable run clone is mounted; control-plane credentials and publication stay in the parent coordinator. Unix limits remain defense in depth for the local executor.

The worker API key remains in the parent process. Child environments are rebuilt from an allowlist, while GitHub installation tokens are issued for the active run, validated against the manifest, held in memory, and refreshed before expiry.

## Ownership and concurrency

Lease loss stops and destroys the affected sandbox and suppresses stale terminal writes. ETags and semantic idempotency keys protect concurrent control-plane mutations. Each active run has a unique sandbox and workspace, so `serve` may safely claim up to its configured capacity.

At startup the coordinator compares `sbx ls --json` with control-plane active
runs and removes managed orphans. Sandbox names are hashes of run IDs, avoiding
collisions and disclosure. Allowlisted environment values are transported in a
private temporary env file under non-committable `.git` metadata and deleted
after the sandboxed command exits.
