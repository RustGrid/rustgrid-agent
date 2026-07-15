# Architecture

`rustgrid-agent` is a control-plane worker that turns a leased RustGrid run into a reviewed GitHub pull request. RustGrid remains authoritative for tenant context, manifests, execution policy, leases, and run-scoped GitHub credentials.

## Components

- **Coordinator:** connects to a pre-announced worker identity, consumes the durable assignment queue, reconciles runs assigned by RustGrid, and drains on shutdown.
- **Supervisor:** renews the worker heartbeat and run lease independently of long-running child processes.
- **Execution:** creates a Docker Sandbox microVM around a dedicated clone, runs Codex and required gates there, and commits only agent-created paths from the trusted coordinator.
- **Publishing:** reconciles the branch, push, pull request, and required GitHub workflows.
- **Reporting:** writes the durable journal and publishes sequenced events, steps, comments, ticket states, and run states.
- **Finalization:** maps one typed terminal outcome to cleanup and external side effects.

## Run sequence

```text
control-plane assignment -> manifest validation -> token issuance -> isolated clone
     -> sandbox create -> Codex <-> sandbox gates
     -> commit -> push -> pull request
     -> required workflows <-> Codex CI repair -> awaiting_review -> successful cleanup

Failed, blocked, timed-out, cancelled, or lease-lost executions stop and retain
their Docker Sandbox alongside the durable workspace journal. The same run ID
can restart directly. A later attempt can explicitly name the failed run in
`run.metadata.resume_from_run_id`; the worker then atomically adopts its
workspace and executor while starting a fresh reporting sequence for the new
run. Startup protects recent retained sandboxes by their journaled executor IDs
and removes them after the configured failed-workspace retention window.
```

Every irreversible publication checkpoint is written atomically to `journal.json`. A restarted worker derives a recovery plan and reconciles existing Git and GitHub state rather than repeating side effects.

Required local gates aggregate failures and return them to Codex for a bounded
repair loop. Required GitHub workflow failures are resolved to the latest run,
failed jobs and steps, and bounded job-log tails. Each CI repair is locally
validated and pushed as a new commit to the existing pull request. Three
unsuccessful repair iterations produce a blocked handoff and retain the isolated
execution state.

## Trust boundaries

RustGrid and GitHub are trusted external control planes. Ticket content, repository content, Codex output, child processes, and network responses are untrusted. Docker Sandbox provides the production microVM boundary. Only the disposable run clone is mounted; control-plane credentials and publication stay in the parent coordinator. Unix limits remain defense in depth for the local executor.

The worker API key remains in the parent process. Child environments are rebuilt from an allowlist, while GitHub installation tokens are issued for the active run, validated against the manifest, held in memory, and refreshed before expiry.

## Ownership and concurrency

Lease loss stops and retains the affected sandbox and suppresses stale terminal writes. ETags and semantic idempotency keys protect concurrent control-plane mutations. Recovery adoption has one active owner: the source journal is reassigned before its workspace directory is moved, so competing attempts fail closed. Each unrelated active run has a unique sandbox and workspace, so `serve` may safely claim up to its configured capacity.

At startup the coordinator compares `sbx ls --json` with control-plane active
runs and journaled retained executor IDs, then removes managed orphans. New
sandbox names are hashes of run IDs, avoiding collisions and disclosure;
adopted attempts keep the source sandbox identity. Allowlisted environment values are transported in a
private temporary env file under non-committable `.git` metadata and deleted
after the sandboxed command exits.
