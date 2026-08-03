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

The ephemeral GitHub Actions executor uses a separate hard phase controller.
Its signed hosted coding budget is divided into discovery, planning,
implementation/repair, diff-review, and completion-evaluation allocations.
For the 40-call default these are `8/4/20/4/4`; earlier phases cannot consume
later reservations, while unused discovery/planning calls roll forward only to
implementation and repair. Validation and publication remain worker-owned
phases. A versioned notebook, structured impact map and plan, search guard,
write-progress thresholds, complete paged diff review, and fresh completion
evaluator prevent technical gates from disguising incomplete functional work.
Manifest v4 carries one canonical `model_call_budget` plus requested, resolved,
source, and clamp audit fields. The worker compares that value with both the
persisted execution limit and the gateway limit before the first model call;
any difference is `execution_budget_mismatch`, never a silent lower runtime
budget.

Impact-map semantics are independent from event persistence. A valid map stays
available in memory and in the versioned notebook even when a phase or tool
event cannot be written. Event writes use stable idempotency identities and
revision/hash checkpoint metadata; a failed write is retried without a model
call. Strict recovery can reuse tool arguments, assistant JSON, and previously
recorded discovery progress. Only a still-invalid artifact enters the
supplemental one-call `artifact_repair` phase, where repository reads, searches,
and mutations are forbidden.

## Components

- **Coordinator:** connects to a pre-announced worker identity, consumes the durable assignment queue, reconciles runs assigned by RustGrid, and drains on shutdown.
- **Supervisor:** renews the worker heartbeat and run lease independently of long-running child processes.
- **Execution:** creates a Docker Sandbox microVM around a dedicated clone, runs Codex and required gates there, and commits only agent-created paths from the trusted coordinator.
- **Publishing:** reconciles the branch, push, pull request, and required GitHub workflows.
- **Reporting:** writes the durable journal and publishes sequenced events, steps, comments, ticket states, and run states.
- **Finalization:** maps one typed terminal outcome to cleanup and external side effects.

## Ports and adapters

Domain reconciliation remains data-in/data-out. `hosted_orchestrator` consumes an
`ExecutionSnapshot` and returns one `ExecutionDecision`; it does not read the
environment, call HTTP, run Git or subprocesses, touch the filesystem, or read a
wall clock. `hosted_simulation` drives that same reducer from in-memory scripted
effects for deterministic end-to-end tests.

Side effects are reached through narrow, consumer-owned ports:

| Consumer | Port | Production adapter | Responsibility |
| --- | --- | --- | --- |
| Persistent run supervision | `LeaseControlPlane` | `RustGridLeaseControlPlane` over `RustGridClient` | Worker heartbeat and run-lease renewal only |
| Supervisor loop | `ExecutionEnvironment` | `SystemExecutionEnvironment` | Monotonic time, shutdown observation, and bounded sleeping |
| Hosted lease supervision | `HostedLeaseControlPlane` | `HostedApiClient` | Hosted execution heartbeat and typed lease invalidation |
| Hosted model session | `ModelProvider` | `HostedApiClient` AI-response adapter | One registered, deadline-bound model invocation |
| Pull-request reconciliation | `GitHubPublisher` | `GitHubClient` | Find, create, update, and confirm draft state |
| Branch publication | `RepositoryPublisher` | `GitRepositoryPublisher` over `Repo` | Reconcile and push one authorized branch/commit |
| Recovery journal | `EventStore` | `FilesystemEventStore` | Load and atomically replace the versioned journal |
| Hosted retry and credential policy | `HostedClock` | `SystemHostedClock` | System/monotonic time and retry sleeping |

The traits live with the policy that consumes them. Read and mutation
capabilities remain separate: for example, publication cannot use arbitrary
GitHub endpoints, and lease supervision cannot mutate execution state beyond a
renewal. Errors retain typed lease-loss and remote-branch-movement identities so
reconciliation never depends on transport strings.

The CLI is the composition root. Hosted execution explicitly loads the GitHub
Actions environment, constructs the hardened HTTP client, exchanges OIDC,
builds `HostedApiClient` with `SystemHostedClock`, and passes the concrete Git,
GitHub, process-containment, and filesystem adapters into the application flow.
The persistent worker similarly constructs `RustGridClient`; `RunSupervisor`
wraps it in its lease-only adapter. There is no service locator or mutable
dependency registry. Dynamic dispatch is limited to the clock stored by cloned
hosted API adapters and the store retained by the non-generic public
`RunJournal`; making either container generic would spread adapter types across
the API and reporting layers. Orchestration, lease supervision, model
invocation, and publication use static dispatch.

Adapter contract tests continue to exercise real HTTP request shapes, Git
force-with-lease behavior, atomic journal replacement, and GitHub publication
fallbacks. In-memory fakes cover lease loss, transient and permanent failures,
remote movement, duplicate publication, model-budget exhaustion, clock
advancement, and journal write retry. The token-refresh contract test combines
the manual clock with a loopback HTTP adapter fixture. Full in-memory replay
uses no sockets, Git, subprocesses, or wall-clock waiting.

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
