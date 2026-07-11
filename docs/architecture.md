# Architecture

`rustgrid-agent` is a control-plane worker that turns a leased RustGrid run into a reviewed GitHub pull request. RustGrid remains authoritative for tenant context, manifests, execution policy, leases, and run-scoped GitHub credentials.

## Components

- **Coordinator:** registers the worker, consumes the durable queue, pauses claims while degraded, and drains on shutdown.
- **Supervisor:** renews the worker heartbeat and run lease independently of long-running child processes.
- **Execution:** prepares a dedicated clone, runs Codex and required local gates with bounded resources and a sanitized environment, and commits only agent-created paths.
- **Publishing:** reconciles the branch, push, pull request, and required GitHub workflows.
- **Reporting:** writes the durable journal and publishes sequenced events, steps, comments, ticket states, and run states.
- **Finalization:** maps one typed terminal outcome to cleanup and external side effects.

## Run sequence

```text
queue claim -> manifest validation -> token issuance -> isolated clone
     -> Codex -> local gates -> commit -> push -> pull request
     -> required workflows -> awaiting_review -> cleanup
```

Every irreversible publication checkpoint is written atomically to `journal.json`. A restarted worker derives a recovery plan and reconciles existing Git and GitHub state rather than repeating side effects.

## Trust boundaries

RustGrid and GitHub are trusted external control planes. Ticket content, repository content, Codex output, child processes, and network responses are untrusted. The process-level sandbox and Unix limits are defense in depth. A container, microVM, or equivalent runtime boundary is required to contain repository-controlled code.

The worker API key remains in the parent process. Child environments are rebuilt from an allowlist, while GitHub installation tokens are issued for the active run, validated against the manifest, held in memory, and refreshed before expiry.

## Ownership and concurrency

Lease loss cancels the affected execution and suppresses stale terminal writes. ETags and semantic idempotency keys protect concurrent control-plane mutations. Production `serve` currently requires `max_concurrency=1`; scale by running multiple independently isolated worker instances with distinct worker identities and workspace volumes.
