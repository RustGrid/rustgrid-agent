# Architecture

`rustgrid-agent` is a control-plane worker that turns a leased RustGrid run into a reviewed GitHub pull request. RustGrid remains authoritative for tenant context, manifests, execution policy, leases, and run-scoped GitHub credentials.

## Mission context and budgets

Validated direct metadata operations are resolved before repository execution.
Every coding mission checks out its repository first. The worker then analyzes
the objective with the checked-out repository available and classifies it as
`configuration`, `single_file`, `multi_file`, or `repository_wide`. Explicit
`run.metadata.mission_class` values take precedence. The selected class, reason,
multidimensional budget, ownership boundary, focused-validation plan, and
logical tool bundles are published as lifecycle telemetry. Individual budget
dimensions may be overridden through signed run metadata.

Codex owns targeted discovery, implementation, and focused validation. The
worker owns dependency bootstrap, full repository gates, commit, publication,
and GitHub checks. Worker-owned commands are listed in the prompt. If Codex
attempts an exact full gate, the worker stops that attempt and starts a compact
corrective session instead of paying for duplicate deterministic work.

Budgets are evaluated after provider turns and tool events. The worker records
70%, 90%, and 100% threshold events as advisory telemetry and keeps the active
Codex session running so it retains context and can finish a validated change.
Initial prompts that already exceed a threshold receive focused guidance, but a
budget estimate never aborts a mission. A normal completion requires an
explicit implementation-complete declaration and successful focused validation
against the current source-tree hash. A code change with no viable focused
command may explicitly defer validation to the mandatory worker gate with a
reason; documentation-only changes may record why automated focused validation
is not applicable. Gates, publication, lease renewal, audit persistence, and
cleanup are never skipped or interrupted by a Codex budget.

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

Immediately before each initial or repair publication, the worker reconciles
the remote agent branch and rebases the complete agent commit range onto the
latest remote base branch. Rewritten existing agent branches use an exact
force-with-lease bound to the observed remote SHA, so another worker's movement
causes a safe stop instead of an overwrite. Changed commits always pass local
validation again before publication.

Locked dependency state is fingerprinted from the package manifest and lockfile
and persisted in `journal.json`. A successful bootstrap is reused until either
fingerprint changes or the installed dependency directory is invalid. Combined
quality gates omit a redundant leading install, and the same full gate is not
executed twice against the same source-tree hash within a repair cycle.

Required local gates retain complete output in the gate audit and send only a
normalized, ANSI-free summary or bounded failure excerpt into a new compact
repair session. The compact prompt contains the ticket summary, changed files,
current bounded diff, failure summary, and remaining cycles—not the prior tool
history. Required GitHub workflow failures are resolved to the latest run,
failed jobs and steps, and bounded job-log tails. Each CI repair is locally
validated and pushed as a new commit to the existing pull request. Three
unsuccessful repair iterations produce a blocked handoff and retain the isolated
execution state.

## Trust boundaries

RustGrid and GitHub are trusted external control planes. Ticket content, repository content, Codex output, child processes, and network responses are untrusted. Docker Sandbox provides the production microVM boundary. Only the disposable run clone is mounted; control-plane credentials and publication stay in the parent coordinator. Unix limits remain defense in depth for the local executor.

Inside the production microVM, Codex runs with its inner sandbox disabled so
repository toolchains can execute downloaded binaries and subprocesses such as
esbuild. This does not grant host access: the Docker Sandbox remains the outer
filesystem, process, network-policy, and resource boundary. Local execution
continues to use Codex `workspace-write` mode.

The worker API key remains in the parent process. Child environments are rebuilt from an allowlist, while GitHub installation tokens are issued for the active run, validated against the manifest, held in memory, and refreshed before expiry.

## Ownership and concurrency

Lease loss stops and retains the affected sandbox and suppresses stale terminal writes. ETags and semantic idempotency keys protect concurrent control-plane mutations. Recovery adoption has one active owner: the source journal is reassigned before its workspace directory is moved, so competing attempts fail closed. Each unrelated active run has a unique sandbox and workspace, so `serve` may safely claim up to its configured capacity.

At startup the coordinator compares `sbx ls --json` with control-plane active
runs and journaled retained executor IDs, then removes managed orphans. New
sandbox names are hashes of run IDs, avoiding collisions and disclosure;
adopted attempts keep the source sandbox identity. Allowlisted environment values are transported in a
private temporary env file under non-committable `.git` metadata and deleted
after the sandboxed command exits.
