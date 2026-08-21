# Protocol v1 migration and risk plan

Protocol v1 is a strangler migration, not a rewrite cutover. Legacy executions
continue under the engine and checkpoint schema that created them. New protocol
code first runs only deterministic conformance fixtures, then shadow decisions,
then explicitly selected new executions.

## Current components and disposition

### Preserve as proven primitives

| Current component | Location | Protocol v1 disposition |
| --- | --- | --- |
| Pure snapshot reconciliation | `src/hosted_orchestrator.rs` | Extract policy that remains valid into Protocol v1 `decide`; do not retain legacy phase/notebook inputs. |
| Atomic event application | `src/execution_graph/{snapshot,transition}.rs` | Reuse clone-validate-commit and exact replay semantics behind the v1 aggregate. |
| Graph and lifecycle invariants | `src/execution_graph/{invariant,lifecycle_invariant}.rs` | Port as typed v1 invariants and add state/action/tool properties. |
| Node capabilities | `src/execution_graph/node.rs` | Replace parallel phase/tool lists with one richer action-capability authority. |
| Verified repository operations | `src/execution_graph/recovery.rs` | Preserve reducer behavior; adapt it to typed v1 intents/revisions. |
| Implementation barrier | `src/execution_graph/graph.rs` | Preserve as an explicit proof-producing transition. |
| Node budget reservations | `src/execution_graph/{node,budget}.rs` | Preserve and extend with input/output ceilings; make it the only call authority. |
| Evidence store | `src/execution_graph/validation.rs` | Preserve stable identities/hash/range reuse; separate blob storage from provider context. |
| Target context | `ExecutionSnapshot::target_execution_context` | Keep the useful target/evidence selection ideas but replace notebook/history inputs with `ContextManifest`. |
| Validation repair evidence/eligibility | `src/execution_graph/recovery.rs`, `src/hosted_orchestrator.rs`, `src/hosted/recovery.rs` | Consolidate into one repair policy before the shared target executor. |
| Provider request enforcement | `src/hosted/provider.rs`, `src/hosted/model_session.rs` | Keep exact serialized tool checks; serialize only from `ActionEnvelope`. |
| Validation process/parser/output capture | `src/hosted/execution/validation.rs` | Keep runner and bounded output; place parser implementations behind profile-selected adapters. |
| Publication ports/reconciliation | `src/hosted/publication.rs` | Keep narrow Git/GitHub ports and idempotent external reconciliation. |
| Canonical terminal authority | `src/hosted/terminal.rs` | Preserve domain-result/callback separation and typed health. |
| Deterministic simulation | `src/hosted_simulation.rs` | Reuse fake-effect approach; remove its independent phase law and drive v1 directly. |
| Typed execution failures | `src/error.rs` and subsystem errors | Preserve bounded typed errors; keep `anyhow` only at CLI/setup/reporting composition boundaries. |

### Extract or replace during migration

- Consolidate repair selection currently split across graph recovery, pure
  orchestration, and hosted recovery.
- Replace Vitest-shaped parsing assumptions with parser adapters selected by
  repository capabilities while retaining the generic structured result.
- Replace JavaScript-only dependency bootstrap and `package.json`/`npm` command
  inference with repository-profile detectors and signed command policy.
- Remove theme-specific deterministic planning heuristics rather than porting
  them.
- Rebuild semantic progress/cycle identity from v1 state and facts; retain the
  proven same-state/same-decision/no-progress rule.
- Translate existing provider schemas to typed `ToolAuthorization` values once;
  do not keep graph, phase, and JSON-name policy lists in parallel.

## Compatibility boundaries

### Engine version

Add a signed manifest field such as:

```json
{"execution_protocol_version": 1}
```

The backend persists the selected engine version on the execution before
dispatch. It is immutable for an execution attempt and included in event,
checkpoint, provider-reservation, completion, and continuation contracts.

- Existing executions/checkpoints with no field route to the legacy engine.
- New Protocol v1 executions route only when worker and backend advertise the
  exact compatible schema set.
- A continuation inherits its source engine version. There is no automatic
  active-checkpoint conversion.
- Changing the default requires a backend configuration rollout, not an AgentOps
  unsigned input.

Inside the private Protocol v1 implementation, `protocol_mode` is a separate
required root field. `CompatibilityScaffold` preserves pre-freeze private tests
only. `StrictV1` is the sole production-eligible mode and binds the requested
`DiscoveryGoal`, validation policy, and finalization/publication policy at
revision zero. Strict decision, reduction, restore, and runner paths reject a
compatibility root before dispatch. This internal distinction does not enable
backend routing or change the signed manifest contract by itself.

### Events and checkpoints

Protocol v1 uses a separate event namespace/schema and one aggregate snapshot.
The backend must accept and validate these events before a v1 worker is
deployed. Event IDs use a new namespace so a legacy semantic event cannot
collide with a v1 event.

The foundation freeze sets the private domain-event envelope to schema v2.
Exactly the first event has no cause; every later event names an already
committed prior event, correlation remains stable for the execution attempt,
and node ownership equals the reducer-derived payload owner. Those fields are
required and identity-bearing. Pre-freeze private schema-v1 events and private
snapshots without `protocol_mode` are deliberately rejected. They are test-only
wire artifacts, so this change uses fixture regeneration rather than a serde
default or silent migration. Any future persisted-private-data importer must
be explicit, one-way, and independently fixture-tested.

Legacy notebook and graph readers remain available in a read-only
`legacy_checkpoint` adapter. A one-time import may be built later only for
terminal/review-safe checkpoints with explicit fixtures. The v1 reducer never
uses a projected legacy notebook as resume authority and never uses
bidirectional graph/notebook synchronization. If an old UI temporarily needs a
legacy-shaped view, a one-way compatibility projector may publish a clearly
non-authoritative read model under a distinct key.

Do not store Protocol v1 state under `run.metadata.worker_notebook`: current
legacy compatibility handling can treat an unknown notebook/version mismatch
as no notebook and start fresh. Use a distinct, required v1 checkpoint
envelope. Likewise, do not reuse the currently dormant
`RunJournal.hosted_execution` field until it has an explicit production caller,
schema, and recovery test. The local journal remains schema 1 for legacy local
`run/watch/serve`; initial Protocol v1 routing is hosted GitHub Actions only.

Legacy graph events are not guaranteed to rebuild a complete state from an
empty aggregate because older `RepositoryEvidenceRecorded` and `GraphCreated`
events may omit materialized evidence/topology. Any future importer must ingest
the complete validated materialized snapshot, not replay only its event list.

### Public and operational contracts

- Preserve CLI entry points `execute_github_actions` and
  `report_emergency_failure`; the CLI chooses the engine adapter after signed
  manifest validation.
- Preserve secrets, OIDC, lease, GitHub installation, repository isolation, and
  control-plane trust boundaries.
- Preserve current completion behavior until the backend accepts the v1
  terminal taxonomy and callback-health fields.
- Keep callback delivery outside mission truth. The private delivery projection
  binds the exact event and result hashes from a replay-validated strict
  terminal aggregate; the future durable store must prove that aggregate was
  physically committed. A durable outbox, callback transport, and backend
  acknowledgement route must land before it can replace existing completion
  delivery.
- Preserve the current AI call identity contract during legacy execution. Its
  semantic index is execution-attempt scoped, so changing engines mid-attempt
  could submit a different payload under an existing identity.
- Add a temporary explicit mapping from v1 action classes to accepted gateway
  phase metadata until the gateway natively validates v1 action identities.
- Regenerate backend/AgentOps clients only after the OpenAPI/event schema lands.
- Release order is backend compatibility -> AgentOps observability -> candidate
  worker -> canary -> stable worker default.

The current OpenAPI shape leaves inner worker-event `data` effectively
untyped, while runtime backend validation is stricter. Migration acceptance
therefore requires executable cross-repository contract fixtures; OpenAPI
generation alone is insufficient. Current worker-event idempotency incorporates
serialized payload and completion idempotency incorporates the full completion
request, so additive compatibility fields must be versioned and fixture-tested
rather than assumed harmless.

## Delivery phases

Each phase is independently mergeable and keeps all legacy paths green. Every
phase report must list changed files, protocol tests run, legacy gates run,
complexity deleted, compatibility impact, and remaining migration risk.

### Phase 0: contract freeze and package approval

Deliver this architecture package, versioned wire sketches, an inventory of
legacy serialized fixtures, and agreed promotion metrics. No production
behavior changes.

Exit criteria:

- protocol states/outcomes and backend ownership approved;
- signed engine-version rollout agreed;
- all legacy snapshot/event/provider/terminal schemas inventoried;
- baseline test and production metrics captured.

### Phase 1: protocol aggregate, reducer, and invariants

Status: implemented side-by-side and not routed to production.

Introduce `src/execution_protocol/` with identities, state, node types, events,
pure `decide`/`reduce`, canonical result, and in-memory event store. Port atomic
reduction and the minimum proven graph invariants. Do not call it from hosted
production execution.

Tests:

- legal/illegal transition tables;
- exact replay and same-ID/different-payload conflict;
- one active owner;
- dependency/barrier/publication ordering;
- terminal immutability;
- generated event-trace properties.

Complexity deleted: none yet; new code is deliberately parallel and test-only.

### Phase 2: repository profile and discovery machine

Status: implemented side-by-side for checked-in Protocol v1 fixtures and not
routed to production.

Add deterministic profile detectors and evidence-driven discovery states.
Route only Protocol v1 repository fixtures through them. Implement the single
typed discovery tool envelope and bounded context builder for discovery.

Tests:

- ecosystems and unknown fallback;
- generated-path policy;
- search identity/dedup;
- criterion-bound context and serialized tool authority;
- mandatory candidate reads in bounded batches and on the final call;
- relationship and impact evidence binding;
- node-or-mission call/cost/duration exhaustion convergence/block;
- invalid observation recovery without refunded spend;
- no useful evidence.

Complexity eligible for deletion only in the v1 path: phase-based discovery
tool allowlists and prompt-only convergence hints.

Complexity deleted from the legacy engine: none. The private v1 slice already
centralizes discovery tool authority, context construction, evidence identity,
and multidimensional budget convergence, but legacy execution remains intact
until the backend contract and production migration gates are complete.

### Phase 3: planning validation and graph materialization

Status: the private change-plan and graph-materialization slice is implemented
side by side and is not routed to production. Positive `SucceededNoOp` remains
deferred until a later phase supplies an independently authoritative,
replayable criterion-satisfaction observation; ordinary discovery relevance
evidence fails closed.

Add `PlanCandidate`, exact target validation, criterion/evidence coverage,
operation semantics, dependency checking, risk/size estimates, and accepted
graph creation. Planning uses its own model-call budget and cannot call
discovery tools. A separately trusted, replay-preserved graph contract supplies
per-kind implementation, validation, review, completion, and publication
budgets and is checked against remaining mission capacity before acceptance.

Tests include vague paths, creation/deletion/move, generated targets, evidence
gaps, fail-closed unproven no-op, provider output bounds, trusted graph budgets,
and deterministic serialization. Positive no-op proof coverage remains a
promotion gate rather than being fabricated from file/impact relevance.

Complexity eligible for deletion in v1: hosted/graph duplicate plan types and
theme-specific plan fallback.

### Phase 4: target-scoped implementation context

Status: the private target-scoped implementation-context slice is implemented
side by side and is not routed to production.

The graph's active `Implementation` node remains the sole target selector. It
produces a deterministic read-only `LoadTargetContext` request bound to the
execution and node attempt, accepted plan/target, current repository revision,
operation-owned source/destination expectations, required and target-local
optional evidence, validation expectations, and the node's signed input-token
ceiling. No phase or notebook projection independently chooses the target.

The materialization boundary loads exact content-addressed artifacts, verifies
their hashes, encoding, ranges, path state, and repository fingerprint, and
keeps raw bytes outside serializable protocol state with redacted `Debug`.
Only receipts and an authoritative bounded projection are persisted. The
projection selects full current target content where it fits, may use exact
evidence-backed ranges, omits optional evidence in stable order, records every
compaction decision, and derives stable materialized-context and manifest
hashes. Stale revision/path/content observations are rejected before state
changes. Mandatory overflow returns the typed
`implementation_context_too_large` error atomically rather than increasing the
ceiling or persisting a partial context.

Tests prove target and attempt isolation, operation-specific create/modify/
delete/move probes, stable request/context identities, full-file and exact-range
selection, bounded optional compaction, stale or mismatched artifact rejection,
mandatory-context overflow, redacted materialization diagnostics, manifest
schema strictness, persisted projection/replay, and the context-ready reducer
boundary.

Phase 4 itself ends at `ImplementationContextReady`; the private Phase 5 slice
now consumes that boundary. Durable protocol events for Phase 4 loader-side
failure/convergence remain later work.

Complexity eligible for deletion in v1: full notebook serialization,
phase-based notebook compaction, and historical-turn accumulation.

### Phase 5: mutation strategies and feasibility-aware fallback

Status: the private mutation-contract, lifecycle, and replay slice is
implemented side by side and is not routed to production.

The reducer computes a canonical feasibility set from the graph-owned target,
prepared context, signed input/output ceilings, operation, target size, and
remaining mutation capacity. Initial modify policy admits the feasible
canonical subset of `apply_patch` then `replace_file`; create, delete, and move
admit only their operation-owned tool. A typed failure selects either a forced
same-strategy model retry, a forced feasible fallback, an exact context rebuild
for repository drift, or durable convergence. Attempt policies bind target,
context, repository revision, feasibility hash, prior attempt, and monotonic
attempt index.

`MutationProviderRequestContract` is the serialized boundary authority. Its
strict JSON schemas bind exact source/destination paths and expected hashes,
bound candidate lengths, reject additional properties, disable parallel tool
calls, and require either the complete feasible initial set or one named tool.
Action, call, reservation, context, repository, budget-owner, and token-limit
identities are derived and reducer-revalidated before admission and dispatch.
Adapter-local tool rehydration is not authoritative.

Provider content becomes a candidate only after it is materialized as a
content-addressed artifact with a hashed store locator and deterministic
persistence receipt. Raw candidate and expected-after bytes remain
non-serializable and redacted; events retain receipts, hashes, byte lengths,
encoding, and typed operation data. Apply and verify are separate deterministic
effects. Verification must prove exactly the operation-owned path transitions
and candidate result. Only verified evidence advances the repository revision
and completes the implementation node.

Typed repository drift cannot dispatch a mutation against stale context. It
authorizes one exact `TargetContextSuperseded` observation, advances to the
observed repository revision, retains the old context in history, charges the
context-rebuild counter, and requires a newly prepared context and rebuilt
policy. The context and mutation ledgers retain all revisions/attempts and
revalidate their derived IDs and chains during replay.

Pre-dispatch dead ends are persisted as typed readiness convergence: no
feasible strategy; exhausted model-call, cost, or duration admission capacity;
or exhaustion of the bounded definitively-uncontacted action retry. A
definitively uncontacted release may produce one new action under the same
mutation-attempt policy, but the next action, model call, and reservation have
distinct deterministic IDs and name the preceding released action. Ambiguous
contact still requires authoritative reconciliation and cannot take this path.
Only calls reconciled as consumed increment the node and mission
`mutation_attempts` counters; releases consume neither a mutation attempt nor
provider usage.

Readiness or post-failure convergence is the only authority for a terminal
implementation `NodeFailed` event. Exact terminal mapping is replay-checked:
no feasible/no safe semantic fallback becomes `BlockedNoDiff`; admission,
mutation-attempt, or context-rebuild exhaustion becomes `BudgetBlocked`;
repeated uncontacted release and the implemented provider-protocol or artifact-
durability failures become `InfrastructureFailed`.

Focused private tests cover create/delete/move/modify authorization, exact
serialized schemas/tool choice, malformed patch fallback, bounded replacement,
candidate rejection, durable receipts and byte redaction, apply/verify
separation, ownership, rolling revisions, typed drift rebuild, readiness and
failure convergence, distinct uncontacted action identities, consumed-only
attempt accounting, exact node/terminal authority, tamper rejection, and
replay.

This phase does not connect hosted/backend/CLI routing or the production
provider path. The real durable artifact store and provider/filesystem adapter
contracts are still deferred. The private Phase 6 slice consumes the shared
mutation contract for validation repair, and the private Phase 7 slice now
consumes the resulting clean-or-repaired proof ancestry for deterministic
review and publication reconciliation. Positive no-op proof and full Golden C
adapter coverage remain deferred.

Complexity eligible for deletion in v1: parallel tool-policy representations,
adapter-side fallback reconstruction, and notebook mutation diagnostics used as
authority.

### Phase 6: validation and repair

Status: the private validation, process, repair, exact-rerun, and replay slice
is implemented side by side and is not routed to production.

Gate construction intersects accepted-plan expectations and required broad
gates with repository-profile candidates and the versioned validation policy.
The policy binds its repository profile and signed-policy evidence, and each
candidate authorization supplies gate class, parser, timeout, combined output
limit, maximum runs, environment fingerprint, and dependency fingerprint. An
unknown profile command or a candidate absent from policy cannot become a gate.
Canonical ordering runs focused expectations before required suite/build/
typecheck/lint/metadata gates, with explicit gate dependencies.

`ValidationScheduled` persists the exact revision-, node-, attempt-, command-,
parser-, policy-, timeout-, and output-bound process request before the reducer
returns `RunProcess`. The locally exercised process adapter uses direct argv
execution with no shell, a canonical repository-contained working directory,
an empty inherited environment plus fingerprinted allowlisted values, and live
repository/lease authority checks before spawn and while running. It durably
orders start then completion observations, represents definitely-not-recorded
and indeterminate journal results separately, and terminates the process tree
on timeout, cancellation, or boundary failure.

Stdout and stderr share one hard capture ceiling. Complete streams retain a
verified head receipt; truncated streams retain verified head and tail
receipts plus exact original/captured/dropped byte counts. Raw bytes and
environment values stay in redacted, non-serializable, zeroized adapter
objects. Cargo, Node, Pytest, Go, and generic parsers return bounded typed
diagnostics, repository-scoped paths, hashed expected/actual values, parser
confidence, and the observed gate semantics. An exited command never becomes
infrastructure failure: a non-zero exit, or zero without the required
semantics, becomes domain failure evidence and a failure revision. Spawn,
timeout, journal, transport, cancellation, and lease loss remain typed process
outcomes and cannot create validation evidence.

Before ranking or activating a repair, the reducer preflights the run ceiling
of every required gate, not only the failing gate. Any exhausted gate converges
to `BudgetBlocked` without spending repair context, model, mutation, or budget
authority. After a verified repair, the exact originating gate runs first at
the new revision. If that gate is broad, any other invalidated gate owned by
the same validation node runs next before canonical global ordering resumes.

The reducer ranks repair candidates from structured diagnostics, implicated
paths, relationship evidence, acceptance criteria, and target role, then
persists an eligibility decision for every candidate before selection. Source
repair requires direct or relationship evidence. Test repair additionally
requires a policy authorization tied to specification evidence and the exact
stale-expected/accepted-actual hashes. Generated output is ineligible.

Eligibility also requires exact current `MutationVerificationEvidence` with a
canonical owner chain. The initial form must match the target's implementation
node. A later form may match an exact same-target prior `RepairVerified`
mutation whose baseline chain and after revision end at the current validation
failure revision. In either form, the changed-path/transition set must contain
exactly the target path. The implemented repair executor accepts only a current
file produced by an initial `ModifyExisting` or `CreateFile` operation, or by
a chained prior repair's file-to-file mutation, and rebases the next repair to
`ModifyExisting` with that file's verified after hash. Missing, stale, absent,
delete, move, and multi-path baselines are rejected before node creation. An
older baseline for a different target is not carried across another target's
repair without a non-interference proof.

The selected `ValidationRepair` node owns a separate signed budget and a
purpose-bound context containing the repair intent, failure revision,
originating gate, validation evidence, baseline mutation evidence, plan, and
repository revision. The shared Phase 5 mutation lifecycle then enforces the
same feasibility, exact action, reservation, candidate, apply, and independent
verification chain without permitting an implementation context or proof to
satisfy repair. Repository drift cannot rebuild a repair context in this
slice: `ContextRebuildUnavailable` records the observed revision and converges
the repair fail-closed to `NoValidRepair`.

Terminal mapping is structured and replay checked. No eligible/feasible/safe
repair and unavailable repair-context rebuild are healthy `NoValidRepair`;
gate-run, admission, mutation-attempt, or context-rebuild exhaustion are healthy
`BudgetBlocked`; authorized validation cancellation is healthy `Canceled`;
validation process failures and exhausted uncontacted/provider-protocol/
artifact-durability repair failures are `InfrastructureFailed` with failed
process health. Display text is never part of this classification.

Successful repair is a crash-safe frozen handoff. The new
`MutationVerified` fact advances the repository frontier; exact mutation and
repair proofs authorize repair-node success; `PriorValidationInvalidated`
names every validation evidence ID from the old revision; and only then may
`ValidationRerunScheduled` name the exact originating gate, repair evidence,
and new revision. That gate's deterministic next run attempt must pass before
required-validation proof and transition to review. Replay rebuilds and
revalidates the gate/run/evidence/failure/ranking/eligibility/context/mutation/
invalidation/rerun and terminal projections.

Focused coverage includes policy/profile intersection and serialized request
tampering; pass, domain failure, timeout/infrastructure separation; bounded
failure-tail receipts; stale-test and baseline rejection; all-gate max-run
preflight; prebinding repair rejection; the verified repair/invalidation/exact-
rerun path through review, including late broad-gate owner ordering and a
second same-target repair using the first repair's verified baseline; terminal
authority; strict serialization; a real non-zero subprocess; timeout/large-
output capture; and exact replay. Cross-target multiple-failure carry-forward
and ecosystem-specific failing build/typecheck fixtures remain Phase 8
conformance work.

This Phase 6 slice does not itself wire hosted/backend/CLI routes, existing
provider traffic, production event schemas, Git/GitHub, or publication. The
real durable mutation artifact store, mutation provider, and filesystem
adapters remain Phase 5 deferments. Phase 7 now extends both the clean and
repaired Golden B checkpoints through private exact-diff review, completion,
eligibility, intent-first publication reconciliation, and canonical terminal
mapping. Real hosted effects remain deferred, as do durable Phase 4 loader-
failure convergence, positive no-op proof, and cross-target baseline carry-
forward without a persisted non-interference proof. Lease loss is exercised as
a typed local process outcome; hosted terminal-write authority and suppression
are not claimed by either private slice.

Complexity eligible for deletion in v1: legacy repair-session node/decision
forms and repair selection spread across three layers.

### Phase 7: review, publication, and terminal authority

Status: the private review, publication-reconciliation, canonical-terminal,
and replay slice is implemented side by side and is not routed to production.

The reducer constructs `EngineeringAncestryV1` from either the clean
implementation-barrier/current-validation chain or the complete ordered
repair proof chain through each mutation, repair, invalidation, exact rerun,
and current required-validation proof. `DiffManifestRequestV1` binds that
ancestry, accepted plan, signed base ref and base coordinate, current revision
and fingerprint, repository publication binding, and hard path/page/byte
ceilings before materialization.

The complete diff uses one page per changed path. Each page index and singleton
coverage set identify its path; raw bytes must match that path's patch hash and
byte length. The durable receipt repeats those values and binds a non-secret
content address, artifact-locator hash, and persistence-receipt hash. Raw bytes
are non-serializable, redacted, and zeroized. Plan ownership is deterministic:
unplanned paths, missing planned changes, operation mismatches, unsafe changes,
incomplete work, and criterion-evidence gaps are blocking.

Review and completion expose one forced record-only tool apiece, with strict
schemas, exact schema hashes, no additional properties, and
`parallel_tool_calls = false`. The action payload hashes plan, ancestry,
manifest, exact page or aggregate review context, deterministic criterion/
target/path/validation evidence, and the conservative input estimate. That
estimate includes strict schema/context serialization, fixed overhead, and a
six-byte charge for every referenced raw diff byte; completion charges the
whole diff. The model cannot override deterministic evidence. A criterion in
the signed external-review map cannot be `Satisfied`; its only resolved state
is the exact typed `ExternalReviewRequired` kind.

Eligibility persists all twelve typed current-state predicates. Diff-build and
publication-authority side effects have strict persisted failure records for
limits, repository drift, artifact durability, and unavailable authority. An
effect-derived convergence repeats the exact failure ID and failure hash and
must equal the projection of the stored failure, preventing two valid
observations with the same normalized reason from sharing terminal ancestry.
Budget, provider-protocol, definitely-uncontacted release, blocking review,
incomplete completion, and eligibility denial retain their own fail-closed
convergence facts. Lease loss is deliberately not a review failure.

A granted eligibility record initializes the pure publication ledger. Commit,
exact-lease push, and pull-request intents are persisted before their effect
requests; observations prove created/already-satisfied state or typed failure.
Ambiguous delivery reconciles the open intent instead of allocating another
attempt. Definitive retries use chained monotonic attempt identities beneath
signed per-operation ceilings. Permanent failures, ceiling exhaustion, and
exact remote movement converge to `PublicationFailed`. Each convergence binds
`final_attempt_id`, typed `final_observation_id` for commit, push, or
pull-request, and exact `final_observation_hash`; its ID/hash and replay
validation include that complete last-observation ancestry rather than the
attempt or reason alone. Confirmed normal publication maps to `Succeeded`,
while confirmed draft publication with only signed external review remaining
maps to `PartialReviewable`.

Focused private coverage exercises clean Golden A review/publication, complete
repaired Golden B ancestry and external review, exact page receipt/bytes
tamper rejection, diff drift and authority failure convergence, strict provider
reconciliation and budget behavior, blocking-review terminal authority,
intent-before-effect ordering, remote movement, redaction, and replay.

This phase defines pure effect requests and durable observations; it does not
perform real artifact-store resolution, hosted Git/GitHub calls, control-plane
lease/cancellation checks or terminal CAS, callback/outbox delivery, backend
event routing, production provider routing, live publication, or positive
no-op authority. Those are explicit promotion boundaries, not implied by the
private Golden A/B results.

Complexity eligible for deletion in v1: duplicate completion/outcome mappings
and adapter-local terminal replacement logic.

### Phase 8: complete conformance suite

Status: foundation started; Phase 8 is not complete.

The current foundation checkpoint freezes `StrictV1` versus
`CompatibilityScaffold`, strict root-bound goal/validation/finalization
authority, the profile-first reducer-owned initialization sequence, causal
domain-event schema v2, the authority-fenced CAS/outbox runner contract, and a
separate post-terminal delivery projection. The runner persists an effect
intent before invocation, reconciles an indeterminate result against the same
intent, persists the exact intent ID and safe request digest on the resolving
observation envelope, and persists `CanonicalResultRecorded` before it can report
`Finished`. These implementations are exercised against deterministic
in-memory ports only.

The checked-in `tiny_static_change` artifact is labeled
`fixture_scope = "schema_foundation"`, and its expected events are labeled
`trace_kind = "checkpoint_summary"`. It proves fixture loading, containment,
hashing, schema, and tamper rejection only. It is not reduced as a canonical
event stream, does not prove its result, and does not count as fixture 1 or as
1/20 completion.

Land all 20 checked-in fixture repositories, state/tool properties, Golden A-D,
backend wire fixtures, secret scanning, and deterministic semantic-event hashes.
Run repeated seeded CI and package-source compilation.

Still required are complete strict schema-v2 reducer traces and derived results
for all 20 fixtures, property generation, real durable store and outbox
contracts, control-plane authority integration, real provider/process/Git/
GitHub adapter contracts and production wiring, backend wire fixtures, and
callback delivery. No production routing is enabled until this phase passes.

### Phase 9: production migration

1. Deploy backend storage/validation that accepts but does not dispatch v1.
2. Deploy AgentOps event rendering and filtering for both versions.
3. Run v1 `decide` in side-effect-free shadow mode from captured facts where
   translation is lossless; record divergences without reserving provider
   calls, running commands, mutating files, publishing, or finalizing results.
4. Publish an immutable candidate worker and run synthetic/staging missions.
5. Enable explicit internal canaries for new tiny/small missions.
6. Expand by repository capability and complexity only after promotion metrics
   hold.
7. Make v1 the default for new executions while retaining legacy resume.
8. Disable new legacy dispatch after an observation window.
9. Remove legacy execution code only after all legacy checkpoints expire or are
   explicitly archived/migrated.

Rollback changes only new-dispatch selection. Existing v1 attempts remain
readable/resumable by a compatible v1 worker; rollback never interprets their
checkpoints through the legacy engine.

If continuation migration becomes a business requirement, it is a separate
post-v1 feature. It may run only at a new-attempt quiescent boundary with no
active reservation, process, mutation, publication effect, terminal result, or
repository mismatch. It imports the complete materialized legacy snapshot,
records one deterministic `ImportedLegacyCheckpoint` digest event, and starts a
new v1 epoch. It never switches an active attempt in place.

## Removal list after parity

Delete only when no supported checkpoint or route consumes the item:

1. `WorkerNotebook` fields that duplicate graph position, plan, evidence,
   failures, budgets, validation, remaining work, and completion;
2. `HostedOrchestrationCheckpoint` mirroring of `ExecutionSnapshot`, including
   `replace_from_snapshot` and routine legacy materialization;
3. `graph_bridge` bidirectional synchronization and
   `legacy_import_completed` from the active path;
4. hosted `ExecutionPhase`, `CanonicalExecutionState`, and `PhaseLedger` as
   lifecycle/budget authorities;
5. duplicate hosted and graph plan/target types;
6. duplicate graph/hosted/completion outcome enums and mapping chains;
7. broad `phase_permits_tool` and default phase tool surfaces;
8. full-notebook provider context and general historical turn retention;
9. `GatewayAgent` as a monolithic mutable lifecycle owner, replaced by
   transaction runner plus narrow adapters;
10. legacy `ValidationRepairSession` node kind and legacy `RepairTarget`
    decision variant;
11. source-scanning tests that police who may mutate phases, replaced by
    unrepresentable direct mutation and reducer API visibility;
12. theme-specific planning and JavaScript/npm-only validation heuristics;
13. overlapping search/phase/cycle guardrails whose rules move into v1
    discovery and semantic-progress policy;
14. legacy event writers and backend validators after retention expiry.

Code is not removed merely because an equivalent v1 component exists. Removal
requires call-site absence, replay fixtures for the retirement boundary, and a
documented minimum supported checkpoint version.

## Migration risks

| Risk | Consequence | Mitigation / release gate |
| --- | --- | --- |
| Event/backend schema drift | Hosted `execution_event_invalid` at a new boundary | Backend contract fixtures and backend-first deployment; exact protocol/schema negotiation. |
| Pre-freeze private wire is mistaken for supported persistence | Schema-v1 events or mode-less snapshots are silently assigned authority they never carried | Deliberate strict rejection; regenerate private fixtures; require an explicit one-way importer if any persisted private data must later be retained. |
| Two engines create operational ambiguity | Wrong worker resumes a checkpoint | Immutable persisted engine version; worker capability negotiation; no fallback reinterpretation. |
| Shadow translation is lossy | False confidence from divergence data | Shadow only states with complete fact translation; mark unsupported states explicitly; never dispatch shadow effects. |
| Event volume or source evidence grows excessively | Storage/cost regression or source exposure | Safe summaries/hashes in events, content-addressed bounded artifacts, retention policy, size tests. |
| Repository profile misclassifies commands/generated paths | Unsafe command or mutation | Provenance plus signed process policy; unknown is safe; generated-file negative fixtures. |
| Token estimates undercount JSON/provider overhead | Truncated context/output and invalid mutation | Conservative tokenizer/serialization margin; actual serialized-byte tests; explicit overflow. |
| Repair policy overfits parsers/ecosystems | Wrong target or weakened tests | Adapter parsers with generic output; eligibility before ranking activation; cross-ecosystem fixtures. |
| Event-sourced reducer becomes one enormous enum/module | New monolith | Bounded event families and subsystem-owned validators converting into one aggregate event envelope. |
| Model quality remains variable | Safe but frequent blocked missions | Measure semantic completion separately; improve content prompts only after protocol correctness; no lifecycle relaxation. |
| Publication retries duplicate side effects | Duplicate branches/PRs | Persist intent and observed IDs; reconcile Git/GitHub before retry; force-with-lease. |
| Lease/cancellation races emit stale results | Authority violation | Check lease/cancellation before and after every effect; lease loss suppresses writes; terminal CAS. |
| In-memory runner/delivery tests are mistaken for operational durability | Restart may repeat an effect or lose terminal acknowledgement | Production durable event/outbox CAS, atomic authority reads, reconciliation adapters, callback transport, and crash-window tests are Phase 8/9 gates. |
| Legacy removal breaks old recovery | Lost resumability | Versioned replay corpus, retention window, explicit archive/migration tool, delayed deletion. |
| Temporary parallel code increases maintenance cost | Slower delivery and inconsistent fixes | Phase deadlines, v1-only feature rule after canary, tracked removal metrics, no bidirectional sync. |
| Conformance passes while real adapters differ | Production-only failures | Real serialized provider/process/Git/backend contract layers in addition to pure simulation. |

## Phase report template

Every implementation phase closes with:

```text
Phase:
Outcome:
Changed files:
Protocol behavior added:
Focused tests:
Protocol/conformance tests:
Legacy quality gates:
Complexity deleted (files/types/authority paths):
Serialized or backend contract impact:
Known gaps:
Remaining migration risk:
Eligible for next phase: yes/no, with evidence
```
