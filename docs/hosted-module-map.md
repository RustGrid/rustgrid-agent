# Hosted execution module map

`crate::hosted` is the GitHub Actions composition root. This refactor changes module ownership only; it does not change the two public entry points, wire formats, retry policy, execution decisions, repository semantics, or persistence behavior.

The complete item-by-item inventory is in [`hosted-symbol-inventory.md`](hosted-symbol-inventory.md). That inventory maps each production type, constant, function, and inherent method to its owning file.

## Dependency direction

```text
environment
  -> authentication
  -> control_plane
  -> contracts / provider_protocol
  -> tools / model_session
  -> execution::{discovery, planning, implementation, validation, diff_review, completion}
  -> recovery / publication
  -> hosted::mod (composition root)

lifecycle_state -> graph_bridge -> execution::orchestration
impact_map      -> execution::{discovery, planning}
telemetry/errors are observed by the composition and execution layers
```

Dependencies point toward the composition root. Phase modules do not parse environment variables. Provider protocol validation does not choose orchestration actions. Repository tools do not own provider turns or call accounting.

## Responsibilities and required dependencies

| Module | Responsibility | Required dependencies |
| --- | --- | --- |
| `environment` | GitHub Actions environment parsing, URL/ref validation, secret redaction and zeroization, process hardening, retry/deadline helpers | `reqwest::Url`, process environment, zeroization |
| `authentication` | GitHub OIDC request and mission-scoped execution-token exchange | validated environment values and bounded HTTP decoding |
| `control_plane` | Claim, manifest, heartbeat, event, state, telemetry, completion, GitHub-token and provider gateway HTTP operations | in-memory token state, manifest endpoints, retry/decode policy |
| `contracts` | Manifest, completion, plan, tool-progress and artifact checkpoint wire types | Serde and stable lifecycle/protocol enums |
| `provider_protocol` | Provider request envelope, tool schema, request-size and compact-notebook validation | signed AI limits and contract types; no orchestration decisions |
| `provider` | Phase/action tool profiles, prompts and dependency bootstrap selection | execution decisions and persisted notebook evidence |
| `model_session` | Provider turns, call admission, usage/cost accounting and model/tool loop | control-plane client, phase ledger, graph budget and repository tools |
| `tools::{filesystem,search,mutation}` | Safe repository paths, bounded reads/searches and deterministic mutations | repository root and explicit size/path controls; no provider client |
| `execution::discovery` | Impact-map validation, recovery and deterministic fallback | impact-map schema and persisted discovery evidence |
| `execution::planning` | Plan normalization, criterion coverage, repair and target authorization | accepted impact map and repository path validation |
| `execution::implementation` | Write-failure reconciliation and mutation preflight | planned targets, graph snapshot and repository state |
| `execution::validation` | Dependency bootstrap, quality-gate dispatch, evidence and validation-ledger checkpoint | signed quality-gate policy and graph-selected time budgets |
| `execution::diff_review` | Deterministic changed-path review after required gates | repository snapshot and planned targets |
| `execution::completion` | Independent/fallback completeness evaluation and final repository fingerprint | diff, validation evidence, declarations and unresolved failures |
| `execution::orchestration` | Sole adapter that applies pure execution decisions and persists graph/notebook projections | graph bridge, phase ledger, model/repository observations |
| `lifecycle_state` | Persisted notebook compatibility projections and lifecycle-derived state | contract types and canonical lifecycle helpers |
| `graph_bridge` | Pure translation between notebook compatibility state and execution graph | lifecycle and execution-graph types; no I/O |
| `recovery` | Startup/recovery authorization, cancellation preservation and graph validation replay | durable graph/notebook state and repository evidence |
| `publication` | Branch reconciliation, commit/push, draft PR creation and finalization validation | Git/GitHub clients and authorized completion state |
| `telemetry` | Stable execution, validation, cache and failure payload construction | observed state only |
| `errors` | Typed fail-closed execution failures and actionable diagnostics | provider-contact evidence, graph state and safe truncation |
| `mod` | Public CLI entry points, supervisor wiring and end-to-end hosted execution composition | all capability modules through private interfaces |

## Shared mutable state

`GatewayAgent` remains the execution-scoped mutable coordinator because its fields participate in the same atomic model/tool/checkpoint loop:

- `PhaseLedger` and `CostGuard` enforce the signed model-call, cost and duration envelopes. Splitting either from provider dispatch would allow admission and accounting to diverge.
- `WorkerNotebook`, the embedded graph checkpoint, accepted impact map/plan and declaration are updated together so replay never observes a graph decision without its compatibility projection.
- Tool usage, failures, search guard, repair targets and write blockers feed both progress telemetry and fail-closed completion classification.
- Diff-review cursor/digest and repository-progress counters prevent repeated model output from being mistaken for repository progress.
- The shared `running` flag and stop reason connect heartbeat cancellation to model, validation, recovery and publication boundaries.
- Repository, manifest, control-plane client and containment policy are borrowed or execution-scoped dependencies; they are not a general application context.

No global context was introduced. Capability-specific functions continue to accept explicit repository, manifest, policy, deadline or evidence values where they do not need the execution coordinator.

## Serialization-sensitive and externally visible contracts

The following remain field-for-field compatible and retain their Serde attributes and enum wire names:

- `HostedManifest` and nested execution, GitHub, AI and policy structures.
- `CompletionRequest`, `CompletionStatus`, `CompletionEvaluation`, criterion evidence and review checklist items.
- `WorkerNotebook`, `HostedOrchestrationCheckpoint`, artifact checkpoints, implementation plans/declarations, intended changes, write attempts, validation evidence and remaining-work records.
- Impact-map schema/version and generated impact-map types.
- Recovery/publication result categories and startup-mode names used in telemetry.
- Telemetry event names, phases and payload keys.
- UUID v5 namespaces and material ordering for completion, model-call, event and orchestration idempotency keys.

Golden serialization tests live with `contracts`; lifecycle and graph checkpoint compatibility tests remain with their owning modules. End-to-end execution, replay, recovery and publication tests remain at the `hosted` integration boundary.

## Incremental migration sequence used

1. Inventory the monolith and preserve the public composition entry points.
2. Move trust-boundary, wire-contract and control-plane leaf capabilities without changing signatures.
3. Move repository tools and provider protocol validation.
4. Split `GatewayAgent` inherent methods by phase/capability while keeping one coordinator and one orchestration decision adapter.
5. Move recovery, validation replay and publication as independently testable capabilities.
6. Retain end-to-end tests at the composition boundary and add module-local serialization golden tests.
7. Run formatting, warnings-denied Clippy, tests and package verification.

Internal visibility is limited to `pub(super)` for direct children and `pub(in crate::hosted)` where a nested capability must be consumed by a hosted sibling. The crate-level public API remains only `execute_github_actions` and `report_emergency_failure`.
