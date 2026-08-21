# Execution Protocol v1 architecture package

Status: architecture proposed; Phase 1 through the private Phase 7 review,
publication, and terminal-mapping checkpoint are implemented side by side and
kept private from production routing. Positive no-op authority and every real
hosted/publication side-effect boundary remain deliberately deferred.

Execution Protocol v1 redesigns RustGrid Agent as a deterministic workflow
engine whose model calls supply bounded candidate content. The protocol, not a
prompt or provider response, owns legality, sequence, evidence, budgets,
retries, repair, validation, publication, and terminal authority.

This package is the design gate required before production routing changes:

- [Protocol](protocol.md) defines the finite state machine, state contracts,
  legal transitions, action envelopes, and terminal outcomes.
- [Domain architecture](domain-model.md) defines the aggregate, events,
  reducers, evidence, repository profile, context materialization, mutation,
  validation, repair, budgets, publication, and observability.
- [Conformance suite](conformance.md) defines fixture, golden-path, replay, and
  property-level acceptance coverage.
- [Migration](migration.md) identifies preserved and removed components,
  incremental delivery phases, compatibility boundaries, rollback, and risk.

## Implementation status

Phase 1 exists in `src/execution_protocol/`: versioned identities and
events, the authoritative aggregate, pure `decide`/`reduce`, atomic replay,
trusted-bootstrap budget contracts and reservations, proof-gated transitions,
graph ordering, proof-carrying terminal results, and a trusted-bootstrap
in-memory event store. Signature verification remains at the existing manifest
boundary until Protocol v1 is wired to a signed engine-version contract.

Phase 2 adds deterministic repository profiles and an evidence-driven
discovery machine. Profiles derive bounded ecosystem capabilities, metadata,
generated-path rules, and candidate-only validation commands from repository
observations. Discovery owns revision-bound search identities, canonical
evidence, criterion coverage, mandatory candidate grounding, unresolved
relationships, impact-map convergence, and exact call/cost/duration exhaustion.
One policy builder creates the bounded context manifest, reservation, action
identity, exact tool authorization, and provider envelope that the reducer
revalidates before dispatch. Invalid provider observations have a typed,
budget-preserving rejection path instead of becoming progress.

Phase 3 adds a bounded planning provider contract, canonical plan revisions,
semantic target/change identities, exact operation and current-evidence
validation, criterion-specific impact grounding, validation provenance,
dependency ordering, and reducer-owned graph materialization. A trusted,
replay-preserved graph-budget contract supplies per-kind downstream budgets and
prevents node or remaining-mission overcommit; the planning node's own budget
is never multiplied across the graph. Ordinary discovery evidence cannot
authorize `SucceededNoOp`: the private Phase 3 slice fails closed until a later
phase introduces an independently authoritative, replayable criterion-
satisfaction observation.

Phase 4 adds the deterministic, read-only implementation-context boundary. An
active implementation graph node produces one content-load request bound to
the accepted target, node attempt, plan revision, repository revision, signed
input ceiling, exact operation-owned path expectations, and target-local
evidence set. The loader verifies content-addressed artifacts and repository
path probes, keeps raw bytes in redacted non-serializable materialization
objects, and returns a bounded manifest containing receipts, selected sections,
compaction decisions, token estimates, and stable hashes. The reducer accepts
only the authoritative request projection and persists one replay-checked
`TargetContextPrepared` result per implementation node. Mandatory overflow is
an atomic typed `implementation_context_too_large` rejection.

Phase 5 continues from `ImplementationContextReady` with a private,
replay-checked mutation lifecycle. The reducer evaluates operation-owned
strategy feasibility, persists an initial or recovery attempt policy, and
derives the exact serialized provider request from that policy. Modify may
expose the feasible canonical subset of `apply_patch` then `replace_file`;
create, delete, and move expose only their operation-owned tool. Every fallback
and model retry is a new deterministic attempt with one forced named tool.
Strict schemas bind exact paths and expected hashes, disallow additional
properties and parallel tool calls, and conservatively bound candidate bytes.
The canonical serialized request, including tools, tool choice, context and
repository bindings, action/call/reservation identities, and token ceilings,
is the provider-boundary authority.

Candidate content is materialized outside serializable protocol state and has
redacted diagnostics. Persisted candidate records carry content-addressed
handles, locator hashes, persistence-receipt hashes, content hashes, lengths,
and encodings. An accepted candidate becomes an exact apply request; an
application observation then produces a separate verification request.
Verification independently proves the owned path transitions and exact
candidate result. Only `MutationVerified` advances the rolling repository
revision and authorizes implementation success. Typed repository drift instead
adopts the observed revision through an exact context-supersession event,
retains context history, consumes the context-rebuild budget, and requires a
new context before another mutation policy.

Phase 5 also makes pre-dispatch dead ends durable. No feasible strategy,
exhausted call/cost/duration admission capacity, and exhaustion of the bounded
definitively-uncontacted action retry each produce typed readiness convergence.
A definitively uncontacted release may produce one new action under the same
mutation policy, but the new action has distinct deterministic action, call,
and reservation identities chained to the released action. Only calls
reconciled as consumed increment `mutation_attempts`; released calls do not.
Failure- and readiness-convergence records are the exact authority for the
implementation node's terminal failure revision and canonical terminal
mapping, and all projections are rebuilt and revalidated during replay.

Phase 6 adds a versioned validation policy and derives gates only from the
intersection of plan expectations, repository-profile command candidates, and
policy authorizations carrying signed-policy evidence. The reducer persists an
exact run request before execution. The process boundary invokes its executable
and argument vector directly without a shell, clears the inherited environment,
accepts only fingerprint-bound allowlisted values, rechecks repository/lease
authority before spawn, and records ordered start/completion observations.
Combined stdout/stderr capture is bounded; truncated streams retain verified
head and tail artifact receipts, byte/drop counts, and no raw bytes in protocol
state. Cargo, Node, Pytest, Go, and generic parsers emit bounded typed
diagnostics.

An exited process is validation-domain evidence: zero is a pass only when the
gate's expected semantics were observed, while every other exit becomes a
failure revision. Spawn, timeout, journal, transport, cancellation, and lease
loss remain typed process outcomes and cannot be converted into validation
evidence. Gate-run exhaustion, process failure, and repair failure each reduce
through an exact convergence fact before graph failure and canonical terminal
mapping.

Repair is evidence driven and reducer owned. Every deterministically ranked
candidate receives a persisted eligibility decision before the highest-ranked
eligible target can create a `ValidationRepair` node. Eligibility requires an
exact current `MutationVerified` baseline for that target. That baseline may
be the target's original implementation verification or the result of an exact
same-target prior `RepairVerified` ownership chain ending at the current
failure revision; test changes also require explicit specification evidence
and expected/actual hash authority.
The repair context binds the failure revision, originating gate, validation
evidence, baseline mutation evidence, repository revision, and separate repair
budget. The shared target executor may then produce a new verified mutation.
Only the exact `MutationVerified` -> `RepairVerified` -> repair-node-success
chain can invalidate every validation evidence item from the pre-repair
revision and schedule the exact originating gate at the new revision. The
reducer preflights every required gate's run ceiling before beginning repair.
After the exact originating rerun passes, any other invalidated gate owned by
that same node runs before global canonical gate order resumes. The remaining
reruns, required-validation proof, transition to review, and full replay are
reducer checked.

The implemented repair surface is intentionally narrow. It accepts only a
current, single-path verified file baseline. An initial implementation
baseline must come from `ModifyExisting` or `CreateFile`; a chained prior
repair baseline must be the exact same-target file-to-file mutation authorized
by its canonical `RepairVerified` proof. Every new repair is rebased to
`ModifyExisting` at the verified current content hash. Delete, move, absent,
multi-path, missing, and stale baselines are ineligible. An older baseline for
an otherwise untouched different target is not carried across another target's
repair because Phase 6 has no non-interference proof. Repair-context drift has
no rebuild path in Phase 6; it records
`ContextRebuildUnavailable`, adopts the observed revision as a typed terminal
fact, and fails closed as `NoValidRepair`.

Phase 7 adds the private current-revision review checkpoint. A signed diff
request binds the accepted plan, base ref and base revision, current protocol
revision and repository fingerprint, required-validation proof, and hard path,
page, and byte ceilings. The complete diff uses the narrow v1 representation
of exactly one immutable page per changed path: page index and singleton path
coverage are exact, and the raw page hash and length must equal that path's
`patch_hash` and `patch_bytes`. Persisted page receipts bind the content hash,
non-secret `sha256:` address, locator hash, persistence-receipt hash, and byte
length. Raw diff bytes are non-serializable, redacted, and zeroized.

Review and completion calls are forced record-only provider actions. Their
strict schemas, named tool choice, `parallel_tool_calls = false`, context,
current clean-or-repaired engineering ancestry, complete diff receipts, plan,
review evidence, and conservative input estimate are hash-bound to the action.
The estimator charges the full signed schema/context plus the referenced raw
page bytes (all diff bytes for completion) with conservative escaping and fixed
provider overhead. Deterministic plan/path/validation evidence cannot be
overridden by the model. Unsafe or unplanned changes, missing planned changes,
incomplete implementation, and evidence gaps are blocking. A criterion named
by the signed external-review policy cannot be recorded as satisfied; its only
resolved form is `ExternalReviewRequired` with the exact signed kind.

Review-side effect failures are durable request/effect/revision-bound facts:
typed diff limits, repository drift, artifact-durability failure, and
publication-authority unavailability project to an exact reducer-owned
convergence reason that repeats the persisted failure ID and failure hash, so
distinct valid observations cannot collapse to the same terminal authority.
Review budget, provider protocol, repeated definitely-
uncontacted release, blocking review, incomplete completion, and denied
eligibility also converge through typed facts. No failure record is itself
terminal authority, and lease loss is not modeled as a review failure.

Publication eligibility is an exhaustive current-state predicate. A granted
record initializes a pure publication ledger that persists an exact commit
intent before `CreateCommit`, an exact-lease push intent before
`PushExactLease`, and a pull-request intent before `EnsurePullRequest`.
Observations prove created or already-satisfied external state; retries receive
distinct chained attempt identities only after definitive failure, while an
ambiguous result leaves the same intent open for reconciliation. Remote branch
movement and exhausted/permanent commit, push, or pull-request failures have
exact convergence and canonical `PublicationFailed` mappings. Every such
`PublicationConvergenceV1` binds `final_attempt_id` plus the typed
`final_observation_id` for the commit, push, or pull-request observation and its
exact `final_observation_hash`; an attempt ID or normalized failure reason
alone is not publication terminal ancestry. Confirmed normal publication maps
to `Succeeded`; confirmed draft publication with only the signed external-
review criteria remaining maps to `PartialReviewable`.

The Phase 5 through Phase 7 implementations remain internal protocol slices.
They do not route hosted, backend, CLI, or existing provider traffic; emit
backend events; replace production behavior; or perform a live commit, push,
or pull request. The real diff and mutation artifact-store resolution ports,
hosted Git/GitHub adapters, control-plane lease/cancellation authority,
callback/outbox delivery, production routing, live publication, durable Phase
4 loader-failure convergence, and positive no-op proof remain deferred. The
validation subprocess adapter is real and locally contract-tested, but is not
connected to production routing.

These private slices are exercised only by checked-in protocol fixtures and
conformance tests. The module is compiled but is not called by hosted or local
execution and does not change any existing checkpoint, CLI, provider,
mutation, or publication contract. Production routing remains a later,
backend-first migration phase.

## Deliverable map

| Requested deliverable | Package location |
| --- | --- |
| A. Executive diagnosis | This document: “Executive diagnosis” and “Preserve, replace, and isolate” |
| B. Execution Protocol v1 | [Protocol](protocol.md) |
| C. Domain model | [Domain architecture](domain-model.md): aggregate through evidence model |
| D. Tool-admission model | [Domain architecture](domain-model.md): “Action envelope and tool admission” |
| E. Context architecture | [Domain architecture](domain-model.md): “Context architecture” |
| F. Mutation strategy | [Domain architecture](domain-model.md): “Target-local mutation architecture” |
| G. Validation and repair | [Domain architecture](domain-model.md): validation and repair sections |
| H. Budgets/reservations | [Domain architecture](domain-model.md): “Budget and reservation architecture” |
| I. Observability/events | [Domain architecture](domain-model.md): “Event schema and observability” |
| J. Conformance matrix | [Conformance suite](conformance.md) |
| K. Migration plan | [Migration](migration.md): compatibility and delivery phases |
| L. Removal list | [Migration](migration.md): “Removal list after parity” |
| M. Risk analysis | [Migration](migration.md): “Migration risks” |
| N. Architecture score | This document: “Architecture score” |

## Executive diagnosis

The current engine has several strong domain mechanisms, but the execution
authority is distributed. A pure graph reconciler exists alongside a mutable
hosted notebook, `ExecutionPhase`, `PhaseLedger`, graph-node budgets, provider
turn history, compatibility projections, and adapter-local counters. A fact can
therefore be represented in more than one place and observed at different
times. The repeated stabilization history is consistent with authority drift,
not merely missing edge-case checks.

The primary architectural causes are:

1. **More than one lifecycle projection.** `ExecutionGraph` and its domain
   events are intended to be canonical, while `WorkerNotebook.phase`, legacy
   status collections, and `PhaseLedger.active` still influence execution.
2. **Admission is separated from semantic selection.** The pure reconciler may
   choose a broad action while the provider adapter later narrows tools using
   notebook and budget details. Correctness then depends on both layers
   reconstructing the same state.
3. **Budget ownership is duplicated.** Signed graph-node budgets and the hosted
   phase ledger can disagree about the call that is currently legal even when
   both are internally consistent.
4. **One coordinator materializes too much mutable state.** `GatewayAgent`
   combines decisions, provider turns, notebook compatibility, graph changes,
   reservations, repository observations, repair state, and telemetry. This
   makes ordering part of implementation control flow instead of part of the
   domain protocol.
5. **Compatibility is interleaved with new execution.** The notebook-to-graph
   bridge, duplicated fields, legacy repair node/session shapes, and projection
   updates are useful for old checkpoints but remain on the live decision path.
6. **Model interaction is still loop-shaped.** Tool filtering has improved,
   but several phases continue to ask a model what to do next and then recover
   from choices the deterministic state could have ruled out before dispatch.
7. **Evidence storage and provider context are insufficiently separated.** The
   notebook is both a recovery projection and an input artifact, encouraging
   broad historical state to leak into target-local calls.

These problems produce characteristic failures: a legal event rejected after
a phase projection lags, a fallback policy lost while rebuilding a request,
stale validation reopened after repair, or a node budget checked through the
wrong accounting view. Each local fix can be correct while the next boundary
still disagrees.

## Preserve, replace, and isolate

Preserve these proven concepts and move them behind Protocol v1 types:

- atomic, typed graph/event reduction and strict lifecycle validation;
- immutable evidence identities bound to repository fingerprints;
- verified-write reduction and exact target ownership;
- the implementation barrier;
- structured validation parsing and bounded failure-output capture;
- separate validation-repair ownership and pre-activation eligibility;
- signed node budgets and reserve/reconcile accounting;
- provider tool filtering at the serialized request boundary;
- publication preconditions and idempotent Git/GitHub reconciliation;
- canonical worker-domain terminal authority, independent of callback health;
- narrow side-effect ports and deterministic simulation fakes;
- typed cancellation, lease-loss, repository-movement, and infrastructure
  failures.

Replace or isolate these mechanisms:

- replace `ExecutionPhase` plus `WorkerNotebook.phase` with one derived
  `ProtocolPosition`;
- replace `PhaseLedger` admission with the single signed node-budget ledger;
- replace the model-directed phase loop with reducer-produced action
  envelopes;
- replace broad notebook input with purpose-built context manifests;
- isolate all legacy notebook import/projection code at a versioned migration
  adapter, never in the Protocol v1 reducer;
- replace synthetic/parallel repair accounting with ordinary repair nodes that
  own their complete signed budgets;
- split `GatewayAgent` into a pure decider, a transactional event runner, and
  narrow effect adapters;
- remove prompt-only legality rules; prompts may explain policy, but the
  serialized request must enforce it.

## Target architecture

```text
signed manifest + repository checkout
                 |
                 v
        Protocol v1 aggregate  <---- append-only domain events
                 |
          decide(state) -> EffectRequest
                 |
       admission + ActionEnvelope
                 |
        narrow effect adapter
  (repository / provider / process / Git / GitHub)
                 |
             EffectResult
                 |
       validate -> domain event -> reduce
                 |
          CAS append + snapshot
```

Only `reduce(previous_state, event)` changes authoritative execution state.
`decide(state)` is pure and returns either one legal effect, a deterministic
convergence event, or a canonical terminal decision. Adapters cannot set a
phase, complete a node, consume a budget, activate repair, or authorize
publication directly.

## Architectural decisions

1. The protocol is an event-sourced aggregate with an optional materialized
   snapshot. Events are the authority; the snapshot is a replay optimization.
2. Exactly one protocol position and at most one active work owner exist.
3. A node owns all of its model, cost, duration, repair, input, and output
   limits. No phase ledger exists in Protocol v1.
4. Every external effect has a durable admission/reservation before dispatch
   and a durable reconciliation afterward.
5. Provider requests are generated from an `ActionEnvelope`; tools not present
   in the envelope cannot be serialized.
6. Repository evidence is stored broadly by immutable ID. Model context is a
   bounded, reproducible projection over selected evidence IDs.
7. Mutation and validation-repair share the verified target executor but use
   distinct typed intents and graph nodes.
8. Every repository mutation creates a new repository revision and invalidates
   validation evidence tied to older revisions.
9. Repair eligibility is a prerequisite event for repair-node creation.
10. Publication is a deterministic predicate over current authoritative state,
    not a model recommendation.
11. Existing executions remain on their recorded engine version. Protocol v1
    starts only new executions until an explicit continuation migration is
    designed and proven.

## Architecture score

| Dimension | Weight | Current | Protocol v1 design |
| --- | ---: | ---: | ---: |
| Single lifecycle authority | 20 | 9 | 19 |
| Deterministic action legality | 15 | 7 | 14 |
| Evidence and bounded context | 15 | 8 | 14 |
| Budget and reservation integrity | 10 | 7 | 10 |
| Mutation safety and feasibility | 10 | 8 | 9 |
| Validation and repair correctness | 10 | 7 | 9 |
| Recovery, terminal, publication safety | 10 | 9 | 9 |
| Conformance and operability | 10 | 6 | 9 |
| **Total** | **100** | **61** | **93** |

The current score reflects substantial local correctness but penalizes
duplicated authority and adapter-order dependence. The proposed score is a
design score, not a claim about the current implementation. It remains below
100 because repository profiling is heuristic, model output is probabilistic,
external infrastructure can fail, and migration itself creates temporary
complexity. The implementation earns the proposed score only after all
conformance gates and real-mission canaries pass.

## Design acceptance gate

Production routing should begin only after review confirms:

- the protocol state and terminal taxonomy;
- the event and persistence compatibility strategy;
- the signed-manifest changes for `engine_version` and node budgets;
- backend acceptance of Protocol v1 event envelopes;
- the conformance fixture matrix and promotion thresholds;
- the rule that active legacy checkpoints are not silently converted.
