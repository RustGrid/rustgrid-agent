# Execution Protocol v1

## Protocol contract

Execution Protocol v1 is a deterministic state machine. Its transition
function is:

```text
reduce(state, domain_event) -> Result<new_state, ProtocolViolation>
decide(state)               -> Decision
```

`Decision` is exactly one of:

- `Emit(event)` for deterministic convergence that requires no side effect;
- `Perform(effect_request, action_envelope)` for one admitted side effect;
- `Finish(canonical_result)` when a terminal predicate is satisfied;
- `Wait(reason)` when progress depends on an already-running durable effect.

The reducer is the only writer of protocol state. The event store appends with
an expected aggregate revision, applies the event to a clone, validates all
invariants, and commits the new snapshot atomically. Direct phase or node
mutation is not part of the API.

## Root modes and strict initialization

The aggregate root records one explicit mode:

```rust
enum ExecutionProtocolModeV1 {
    CompatibilityScaffold,
    StrictV1,
}
```

`CompatibilityScaffold` preserves pre-freeze private tests while their
builders are migrated. It is not eligible for strict decisions, reductions,
the transaction runner, or external effects. `StrictV1` is the only
production-eligible shape. Its revision-zero root binds the requested
`DiscoveryGoal`, the validation policy, and the finalization/publication
policy in addition to execution identity, initial repository revision, graph
budgets, and mission budgets. Missing policies, invalid policy authority, or a
publication base revision different from the initial repository revision are
bootstrap errors rather than facts an adapter may repair later.

Repository profiling remains a separately trusted, deterministic adapter
precondition in this foundation slice. Initialization then proceeds through
normal event authority:

1. an authority-fenced, revision-zero compare-and-swap records the exact
   `RepositoryProfileRecorded` event after checking its revision and validation
   policy binding;
2. strict `decide` emits `GoalRecorded` for exactly the root's requested goal;
3. strict `decide` derives and emits the repository-profile proof; and
4. the reducer-owned proof authorizes `Profiling -> Discovery`.

The adapter cannot inject a different goal, synthesize the proof, or advance
the position. A pristine strict aggregate without a profile reports the typed
profile-initialization precondition and performs no ordinary protocol effect.

## Authority-fenced transaction runner

The private runner contract atomically loads the aggregate event stream, the
execution-attempt authority fence, and any unresolved effect intent. The fence
binds the execution and attempt plus lease epoch/status and cancellation
revision/status. Every event, intent, and effect-observation write is an
expected-revision compare-and-swap carrying that same authority. Confirmed
lease loss suppresses writes; cancellation remains fail closed until an
explicit reducer-owned cancellation transition exists.

For `Perform`, the runner persists a request identity and its trigger,
aggregate revision, repository revision, and authority fence before invoking
the adapter. A definitive observation may resolve the intent and append its
validated reducer event atomically. An indeterminate observation leaves the
same intent unresolved and requires reconciliation; it cannot allocate or
dispatch a replacement request. Persisted intent identity contains bounded
hashes and typed request metadata, not raw provider bodies or credentials.

For `Finish`, the runner does not report completion directly. It first uses an
authority-fenced CAS to append `CanonicalResultRecorded`. Only after a reload
finds that exact terminal result committed may the runner return `Finished`.
The interfaces and deterministic in-memory tests establish this ordering; a
real durable store, control-plane authority source, effect adapters, and
production route remain deferred.

## Protocol position

```rust
enum ProtocolPosition {
    Profiling(ProfileStep),
    Discovery(DiscoveryStep),
    Planning(PlanningStep),
    Implementation(ImplementationStep),
    Validation(ValidationStep),
    Repair(RepairStep),
    Review(ReviewStep),
    Publication(PublicationStep),
    Terminal(CanonicalResult),
}
```

At most one active work owner exists. Position is derived from reduced facts;
it is not separately persisted as a mutable phase field. A snapshot may cache
the derived position and must reject a cache mismatch during restore.

## Legal top-level transitions

```text
Profiling
  -> Discovery
  -> Terminal(InsufficientEvidence | InfrastructureFailed | Canceled)

Discovery
  -> Discovery
  -> Planning
  -> Terminal(InsufficientEvidence | BudgetBlocked | Canceled)

Planning
  -> Planning
  -> Implementation
  -> Terminal(SucceededNoOp | InsufficientEvidence | BudgetBlocked | Canceled)

Implementation
  -> Implementation
  -> Validation
  -> Terminal(BlockedNoDiff | BudgetBlocked | InfrastructureFailed | Canceled)

Validation
  -> Validation
  -> Repair
  -> Review
  -> Terminal(BudgetBlocked | InfrastructureFailed | Canceled)

Repair
  -> Repair
  -> Validation(exact originating gate rerun)
  -> Terminal(NoValidRepair | BudgetBlocked | InfrastructureFailed | Canceled)

Review
  -> Publication
  -> Terminal(BlockedNoDiff | ValidationFailed | BudgetBlocked |
              InfrastructureFailed | Canceled)

Publication
  -> Publication
  -> Terminal(Succeeded | PartialReviewable | PublicationFailed | Canceled)
```

No other top-level transition is legal. In particular, discovery cannot jump
to mutation, implementation cannot bypass its barrier, repair cannot jump to
review, and review cannot fabricate validation success.

## State contracts

Every row specifies domain requirements. Adapter failures become typed effect
results and cannot mutate these fields directly.

### Profiling

| Contract | Definition |
| --- | --- |
| Entry | Signed manifest validated; execution claimed; immutable checkout fingerprinted; no model call admitted. |
| Authoritative fields | `execution_id`, `attempt`, `engine_version`, signed mission budget, repository revision 0, profile status. |
| Allowed actions | Deterministically inspect bounded metadata paths authorized by policy; classify languages/build systems; record `RepositoryProfile`. |
| Forbidden actions | Provider calls, repository mutation, arbitrary repository command execution, planning, validation, publication. |
| Exit | A valid profile with provenance is recorded, even if some capabilities are unknown. |
| Failure | Invalid signed policy is terminal; unreadable checkout is infrastructure failure; an unknown ecosystem is not itself failure. |
| Idempotency | Profile identity is the hash of metadata path/content hashes plus profile schema. Exact replay is a no-op. |
| Persistence | Persist metadata observations, profile, repository revision, and profile hash before discovery. Never persist credentials or expanded secret-bearing CI values. |

### Discovery

Discovery uses these deterministic substates:

```text
NeedCandidates -> NeedGroundedReads -> NeedRelations -> ReadyToSynthesize
       ^                  |                  |
       +---- targeted gap resolution <------+
```

| Contract | Definition |
| --- | --- |
| Entry | Repository profile recorded; discovery node is ready; repository revision matches checkout. |
| Authoritative fields | Search identities/results, candidate records, file evidence IDs, line ranges, related-test evidence, unresolved questions, confidence, impact areas, node budget. |
| Allowed actions | `NeedCandidates`: bounded list/search. `NeedGroundedReads`: reads constrained to ranked candidates. `NeedRelations`: deterministic profile lookup and bounded related-file reads; targeted search only for a named unresolved relation. `ReadyToSynthesize`: impact-map result only. |
| Forbidden actions | Mutation, validation, publication, vague plan creation, unrestricted search when mandatory evidence-deepening is active. |
| Exit | Current-revision evidence satisfies the discovery evidence policy and a validated impact map covers the ticket criteria. |
| Failure | No useful evidence, invalid impact map after bounded repair, or exhausted budget with no legal convergence yields a typed blocked outcome. |
| Idempotency | Normalized query plus scope/profile/repository revision identifies a search. Path/range/content hash identifies a read. Exact replay records no progress. |
| Persistence | Persist every search identity, candidate, evidence ID/hash/range, relationship, unresolved question, and impact-map version before the next decision. Content may live in a content-addressed local evidence store; durable remote events carry safe summaries and hashes. |

Mandatory discovery depth rule:

```text
if candidate_paths.non_empty
   && inspected_file_evidence.is_empty
   && node_budget.model_calls_remaining <= 1
   && impact_map_not_yet_valid
then next_action_class = AcquireGroundedEvidence
     search tools = forbidden
```

Exact budget exhaustion invokes deterministic convergence first. Existing
evidence may finalize discovery without another call; otherwise the execution
ends `InsufficientEvidence` or `BudgetBlocked`. Exhaustion never admits another
call.

### Planning

| Contract | Definition |
| --- | --- |
| Entry | Discovery completed with a validated impact map and current evidence. |
| Authoritative fields | Acceptance criteria, impact map ID, evidence IDs, plan candidate/revision, validation errors, planning-node call budget, trusted per-kind graph-budget contract, and remaining mission capacity at candidate reduction. |
| Allowed actions | Produce or repair one typed `PlanCandidate`; request an explicit evidence gap from the orchestrator. |
| Forbidden actions | Repository mutation; consuming discovery budget; broad exploration; accepting `.` or category labels as file targets without a typed repository-scoped operation and executor. |
| Exit | `PlanAccepted` records exact targets, operations, dependencies, criteria, evidence, validation expectations, and risk/size estimates; the execution graph is materialized. |
| Failure | Unresolved or unsafe targets after the signed budget produce `InsufficientEvidence` or `BudgetBlocked`. A proven already-satisfied mission may produce `SucceededNoOp` only from an independently authoritative current-revision satisfaction observation; ordinary impact/file relevance is insufficient. |
| Idempotency | Plan semantic ID hashes discovery artifact, normalized targets, operations, dependencies, and criterion mapping. Exact replay is idempotent; same ID/different plan is a conflict. |
| Persistence | Persist candidate and validation errors; only a validated accepted plan may create implementation nodes. |

### Implementation

Implementation runs one planned target at a time through:

```text
SelectTarget -> PrepareContext -> GenerateCandidate -> ApplyCandidate
             -> VerifyRepository -> CompleteTarget -> SelectTarget
                                         |
                                         +-> ImplementationBarrier
```

The implemented private Phase 4 slice covers `SelectTarget -> PrepareContext`.
`NodeStarted` is the authoritative target selection. The next decision performs
a deterministic read-only `LoadTargetContext` request, and an accepted
`TargetContextPrepared` event advances the cached step to `GenerateCandidate`.
The request and persisted manifest bind the exact node/attempt, accepted
target/plan, repository revision, operation path expectations, evidence set,
and signed input ceiling.

The implemented private Phase 5 slice continues through candidate generation,
apply, independent verification, success, and typed terminal convergence. It
is still not production routing: hosted/backend/CLI and existing provider
paths do not call it, and no production wire contract changes.

| Contract | Definition |
| --- | --- |
| Entry | Accepted plan graph exists; dependencies select one ready target; no repair owner is active. |
| Authoritative fields | Active target/node, typed operation, expected repository revision and hashes, context manifest, attempt identity, mutation strategy, candidate hash, verification evidence, node budget. |
| Allowed actions | Deterministic target probe/context build; one admitted provider call with only target-legal mutation tools; apply the selected mutation; verify repository state. |
| Forbidden actions | Writes outside the authorized target operation; mutation before context; provider-directed target changes; completion based only on model claims; validation before the barrier. |
| Exit | Every required implementation node is `Succeeded` through verified operation evidence, including independently verified `AlreadyApplied` application observations, and the implementation barrier proof is recorded. |
| Failure | Repository drift rebuilds context within budget; typed mutation failure selects a feasible same-target fallback; no feasible fallback or exhausted target budget blocks explicitly. |
| Idempotency | Attempt ID derives from execution, node, intent, repository revision, and monotonically allocated node attempt index. Apply replay verifies the recorded after-hash; same ID/different payload is rejected. |
| Persistence | Persist context manifest before provider dispatch, reservation before contact, candidate hash before apply, and verification evidence plus new repository revision before target completion. |

#### Private Phase 5 mutation contract

For the active target, the reducer first persists a canonical feasibility set.
Modify considers `apply_patch` and `replace_file` in canonical order and exposes
only those that fit the context and conservative serialized-output bounds.
Create, delete, and move each own exactly one tool. Initial multi-tool modify
uses required tool choice; singleton policies and every fallback or model retry
force one named tool. The exact serialized provider request is authoritative:
strict schemas bind paths and expected hashes, content fields have maximum
lengths, additional properties and parallel calls are forbidden, and request
bytes bind action/call/reservation, target/context/repository, attempt,
budget-owner, and token-limit identities.

Candidate bytes are materialized outside the event schema and redacted from
diagnostics. Serializable candidate records retain content-addressed handles,
hashed store locators, deterministic persistence receipts, hashes, lengths,
encodings, and operation bindings. Apply does not itself advance protocol
repository state. A separately derived verification request must prove the
exact operation-owned changed-path set, before/after states, expected candidate
result, and after fingerprint. Only accepted `MutationVerified` evidence
derives the next rolling repository revision and authorizes node success.

Repository drift produces no provider dispatch. An exact drift failure binds
the expected and observed revisions/fingerprints and, within budget, authorizes
`TargetContextSuperseded`; the aggregate adopts the observed revision, retains
prior context history, and prepares fresh context before rebuilding policy.
No feasible initial strategy, exhausted call/cost/duration admission, exhausted
mutation attempts or context rebuilds, no safe fallback, and repeated
definitively-uncontacted release all become persisted typed convergence rather
than protocol errors or indefinite waits.

A definitively uncontacted release may be followed by one bounded action retry
under the same mutation policy. That retry has a new deterministic action ID,
model-call ID, and reservation ID and explicitly chains to the previous
released action. A call increments `mutation_attempts` only after it is
reconciled as consumed, so released actions do not spend mutation-attempt
capacity. Ambiguous contact waits for authoritative control-plane
reconciliation and never guesses that a release is safe.

Convergence is the exact authority for a terminal implementation `NodeFailed`
event and its failure revision. No feasible/no safe semantic strategy maps to
`BlockedNoDiff`; admission, attempt, or context-rebuild exhaustion maps to
`BudgetBlocked`; repeated uncontacted release and the implemented
provider-protocol/artifact-durability classes map to `InfrastructureFailed`.
Once that terminal node failure is recorded, the aggregate freezes every
non-terminal event, including another ready implementation target, until the
one exact canonical result is recorded. Planned implementation success accepts
only the proof derived from its recorded mutation verification; forged
`MutationVerified` and the not-yet-implemented positive `AlreadySatisfied`
proof both fail closed.
Replay reconstructs and validates the full context, attempt, action,
reservation, candidate, application, verification, revision, convergence,
node-failure, usage, and terminal chains.

The real durable artifact store, provider, and filesystem adapters and their
contract tests remain deferred. The private Phase 6 slice now reuses this
mutation contract for validation repair, and the private Phase 7 slice consumes
the resulting clean-or-repaired ancestry for review and publication. Durable
loader-failure convergence, real hosted adapters, and positive no-op proof
remain deferred.

### Validation

| Contract | Definition |
| --- | --- |
| Entry | Implementation barrier proof is current; the next required gate is selected in canonical order, except that an exact repair rerun and its remaining same-owner gates precede global-order resumption. |
| Authoritative fields | Signed-policy/profile/plan provenance, gate and argv command identity, repository/dependency/environment fingerprints, node/run attempts, process identity, timeout, combined output limit and receipts, exit code, parsed assertions/paths, failure revision, deterministic run ceiling. |
| Allowed actions | Run only a command candidate present in both the repository profile and validation policy; execute its exact argv without a shell; capture bounded output; deterministically parse and record pass/failure evidence. |
| Forbidden actions | Shell expansion, inherited environment, model mutation tools, skipping a required gate, treating a process exit as infrastructure failure, constructing evidence from an infrastructure result, or reusing a pass from another repository revision. |
| Exit | Pass advances to the next gate or review. A domain failure advances to repair selection. An execution/transport failure follows typed infrastructure policy. |
| Failure | Non-zero exit, or zero without expected semantics, is validation-domain evidence. Any required gate's exhausted run ceiling is preflighted before repair and maps to `BudgetBlocked`; spawn/timeout/journal/transport and a lease-loss result in the private reducer are `InfrastructureFailed`; authorized cancellation is `Canceled`. Hosted lease terminal-write authority is outside this slice. |
| Idempotency | Gate identity includes provenance, command argv, working directory, parser, fingerprints, dependencies, limits, and repository revision. Run identity adds node/execution attempts and initial-versus-exact-rerun purpose. |
| Persistence | Persist `ValidationScheduled` before spawn; process identity once started; verified head/tail artifact receipts, byte/truncation metadata, and typed result after completion; then parsed evidence/failure revision before any repair decision. |

The private process adapter revalidates repository and lease authority before
spawn and while running, resolves the working directory inside the canonical
checkout, clears the inherited environment, and accepts only allowlisted values
whose canonical fingerprint matches the request. Its observation sink
distinguishes exact already-recorded writes from definitely-not-recorded and
indeterminate writes, so an ambiguous journal result cannot invent a second
completion. Timeout or cancellation terminates the process tree.

Stdout and stderr share one hard limit. A complete nonempty stream has one
verified head receipt; a truncated stream must also have a verified tail
receipt. Receipts bind content, locator, persistence acknowledgement, byte
length, run, stream, and segment; protocol state stores no raw bytes. Cargo,
Node, Pytest, Go, and generic parsers emit bounded typed diagnostics and
repository-scoped paths. Only `Exited` can become `ValidationEvidenceV1`:
expected semantics plus exit zero is `Passed`; every other exit is
`DomainFailed` and creates a failure revision.

### Repair

| Contract | Definition |
| --- | --- |
| Entry | A current validation failure revision exists and no repair node for that revision is active. |
| Authoritative fields | Failure revision and validation evidence; scored candidates; one decision for every candidate; selected target; current single-path mutation baseline; purpose-bound repair context; separate repair budget; mutation and repair proofs; invalidated evidence set; originating gate. |
| Allowed actions | Deterministic ranking and eligibility; activate the highest-ranked eligible target; load its `ValidationRepair` context; execute the shared verified target pipeline; invalidate old-revision validation; schedule the exact gate rerun after verified change. |
| Forbidden actions | Activating before every eligibility decision; using a missing/stale/absent/multi-path/delete/move baseline; changing a test without exact specification and expected/actual authority; borrowing implementation context/fallback counters; rebuilding a drifted repair context; skipping or substituting the originating rerun. |
| Exit | Exact `MutationVerified` and `RepairVerified` proofs authorize repair-node success, all validation evidence from the pre-repair revision is invalidated, and `ValidationRerunScheduled` names the exact gate and new revision. If no candidate is eligible, typed convergence maps to `NoValidRepair`. |
| Failure | No feasible/safe repair or repair-context drift yields `NoValidRepair`; admission/attempt/context-rebuild exhaustion yields `BudgetBlocked`; uncontacted-action, provider-protocol, or artifact-durability failure yields `InfrastructureFailed`. |
| Idempotency | Repair node ID hashes failure revision, repair intent, target identity, and repository revision. Exact replay is a no-op; later attempts receive deterministic distinct attempt IDs. |
| Persistence | Persist ranking and all eligibility decisions before node activation; purpose-bound context before provider admission; every attempt/reservation/failure; mutation then repair proof; exact invalidated evidence IDs; rerun schedule and proof before returning to validation. |

Repair eligibility requires a valid current `MutationVerified` baseline with
canonical ownership and exactly one changed path and one file-producing
transition. The owner may be the target's implementation node, or an exact
same-target prior repair whose canonical `RepairVerified` proof extends the
baseline chain and whose mutation ends at the validation failure revision.
Phase 6 rebases an initial `ModifyExisting` file-to-file or `CreateFile`
absent-to-file result, or a chained repair's file-to-file result, to a fresh
current `ModifyExisting` operation. It does not authorize delete, move, an
absent current path, or a multi-path repair. It also does not carry an older
baseline for another target across a repair without a non-interference proof.
Generated outputs are ineligible; test targets additionally require explicit
criterion/specification evidence and matching stale-expected/accepted-actual
hashes.

The repair purpose binds repair intent, failure, originating gate, validation
evidence, baseline mutation evidence, plan/target, and repository revision. A
verified repair creates a deliberately narrow revision handoff: unrelated
progress is frozen until the mutation proof, repair proof, node success,
old-revision validation invalidation, rerun schedule, and rerun handoff proof
are committed in order. The exact originating gate then runs at the new
revision with a distinct deterministic run attempt. After it passes, any other
invalidated gate owned by that same node runs before global canonical gate order
resumes. Every required gate must have a current pass, and every gate's run
ceiling is preflighted before repair work. Replay re-derives this sequence and
rejects a cached position or ledger that skips it.

Repair context drift has no rebuild implementation in Phase 6. It records the
typed `ContextRebuildUnavailable` convergence with the observed repository
revision and then fails closed to healthy `NoValidRepair`, without dispatching
another model call. Other semantic repair exhaustion is also healthy
`NoValidRepair`; budget exhaustion is healthy `BudgetBlocked`; typed transport,
provider-protocol, or artifact-durability exhaustion is failed
`InfrastructureFailed`.

### Review

| Contract | Definition |
| --- | --- |
| Entry | All required current-revision validation gates passed and no active failure revision remains. |
| Authoritative fields | Clean or repaired engineering ancestry, signed finalization/publication policy, exact diff request and manifest, immutable per-path page receipts, review action/reconciliation ledger, deterministic criterion evidence, completion evaluation, publication-authority observation, and eligibility decision. |
| Allowed actions | Materialize the exact current diff; dispatch bounded record-only review and completion calls; deterministically aggregate findings/evidence; observe read-only publication authority; evaluate eligibility. |
| Forbidden actions | Repository mutation; model-selected tools or parallel calls; reviewing an unbound page; treating a model claim as deterministic path/validation evidence; satisfying a policy-classified external criterion; publication before a granted eligibility record. |
| Exit | Completion is `Complete` for normal mode or `CompletePendingExternalReview` for the corresponding signed mode, and every publication predicate is recorded. |
| Failure | Diff limit, repository drift, artifact durability, budget/protocol/release exhaustion, blocking diff findings, incomplete criteria, unavailable authority, or denied eligibility converges through an exact typed fact. |
| Idempotency | Request, page, review, completion, authority, and eligibility IDs hash their full current-revision inputs. Replay cannot silently review a different plan, proof ancestry, diff, page receipt, policy, or publication coordinate. |
| Persistence | Persist request before diff materialization; persist effect failure or complete manifest; persist action before dispatch and reconciliation after it; persist page reviews, aggregate review, completion, authority request/observation, eligibility, and any convergence in order. |

The private v1 diff format is deliberately narrow: there is exactly one page
for each changed path. For path index `i`, page `i` covers only `{i}`, its raw
content SHA-256 equals the path's `patch_hash`, and its byte length equals
`patch_bytes`. The durable page receipt additionally binds a non-secret
`sha256:<content_hash>` address, store-locator hash, persistence-receipt hash,
and byte length. The complete manifest binds the accepted plan and its exact
path ownership assessment, signed base ref/base revision, current revision and
fingerprint, and required-validation proof. Raw page bytes never implement
`Serialize`, are redacted from diagnostics, and are zeroized on drop.

Each review action exposes exactly one strict named tool:
`record_diff_review` for one bound page or `record_completion_evaluation` for
the completed aggregate review. Additional properties and parallel calls are
forbidden. The schema, tool choice, plan, ancestry, manifest, criterion and
evidence IDs, and review context are payload-hash inputs. Admission uses a
deterministic conservative input estimate that includes those serialized bytes
plus six bytes per referenced raw byte, a fixed provider overhead, and all diff
bytes for completion. An over-ceiling action converges before dispatch.

Completion re-derives criterion-to-target path ownership and target validation
expectation IDs from the accepted plan, manifest, and current ancestry. The
model cannot override missing, unsafe, or stale deterministic facts. The signed
policy's external-review map is also authoritative: a classified criterion
cannot be `Satisfied`; its resolved status must be `ExternalReviewRequired`
with the exact mapped kind. `Unsatisfied` and `Uncertain` remain incomplete and
therefore fail closed.

An effect-derived convergence repeats the exact persisted failure ID and
failure-record hash: `DiffManifestFailureId` for limit, drift, and artifact
durability outcomes, or `PublicationAuthorityFailureId` for unavailable
read-only authority. The reducer compares the entire projected reason with the
stored failure. A normalized safe code or observed revision alone is never
terminal ancestry, and a different valid failure cannot collide with it.

### Publication

| Contract | Definition |
| --- | --- |
| Entry | `PublicationEligibilityGranted` exists for the current repository revision and requested publication mode. |
| Authoritative fields | Signed publication contract and eligibility; exact repository head/tree and commit parent; ordered operation attempt chain; commit, exact-lease push, and pull-request intents and observations; completion or convergence binding the final attempt, typed final observation ID, and observation hash. |
| Allowed actions | After persisting the matching intent, reconcile/create the exact commit, push the exact commit with the signed expected-old head, and reconcile/create/update one execution-marked pull request. |
| Forbidden actions | Model calls; effect before intent; changing commit/tree/parent or PR material across a retry; force push without the exact lease; allocating a retry after an ambiguous observation; publication with stale or denied eligibility. |
| Exit | Confirmed commit, push, and pull-request observations produce one publication completion and proof before the publication node succeeds and canonical result is derived. Legitimate no-op missions bypass publication through planning, not this state. |
| Failure | Definitive retryable failures allocate only bounded chained attempts; permanent failure, attempt exhaustion, or exact remote movement produces publication convergence bound to the exact final persisted observation and maps to `PublicationFailed`. |
| Idempotency | Commit tree/parent/metadata identity, branch plus expected-old SHA, stable PR material, and execution marker make created and already-satisfied observations equivalent authoritative reconciliation outcomes. |
| Persistence | Persist each intent before its irreversible effect and the exact observation immediately afterward. An open intent is reconciled on resume before any new attempt is allocated. |

For every exhausted, permanent, or remote-moved outcome,
`PublicationConvergenceV1` stores `final_attempt_id` together with the tagged
`PublicationObservationIdV1` (`Commit`, `Push`, or `PullRequest`) and the exact
`final_observation_hash`. Its convergence identity and hash include all three,
and revalidation requires them to equal the last persisted observation. The
attempt ID or reason variant alone cannot authorize publication terminal state.

### Terminal

| Contract | Definition |
| --- | --- |
| Entry | Exactly one exhaustive terminal predicate is satisfied. |
| Authoritative fields | Canonical outcome, reason code, first fatal blocker, remaining work, repository revision, validation/publication proof, process health. |
| Allowed actions | Idempotent result reporting and cleanup that cannot change the canonical outcome. |
| Forbidden actions | Model/repository/validation/publication work; replacing an existing canonical result; mapping callback failure to mission failure. |
| Exit | None. A continuation creates a new execution attempt/epoch through an explicit resume event. |
| Failure | Callback/journal transport after canonical persistence is represented only in the separate delivery projection and must never replace the canonical result. Hosted lease loss suppresses stale writes rather than becoming review/publication convergence. |
| Idempotency | Canonical result identity hashes execution attempt and domain proof. Same result replay is a no-op; different result is rejected. |
| Persistence | Canonical result is persisted before callbacks. Callback attempt/acknowledgement is a separate transport record. |

### Post-terminal delivery projection

Callback delivery is not a domain event in `ExecutionState`. A distinct,
strictly serialized projection is created only from a replay-validated strict
terminal aggregate supplied by the runner/store boundary and binds the
execution attempt, terminal event ID and hash, canonical-result hash, callback
payload hash, and idempotency key. The pure projection cannot independently
prove physical durability; that remains an obligation of the deferred durable
store implementation. It persists an attempt intent before send. Acknowledgement
settles delivery; a definite failure may allocate only a bounded next attempt;
an indeterminate result reconciles the same attempt before anything else. The
reconciliation ceiling counts observations made by reconciliation after the
initial send observation, so a ceiling of one authorizes exactly one reconcile
call.

Delivery acknowledgement, retry exhaustion, reconciliation exhaustion, and
transport failure are operational states only. They cannot reduce the mission
aggregate, rewrite the canonical outcome or process health, or manufacture a
second terminal event. This private projection does not implement a callback
transport, durable outbox store, or backend route.

## Event-envelope authority

Private domain-event envelope schema v2 makes causal context mandatory and
identity-bearing. The first stored event is the aggregate's single causal
root and has `causation_id = null`. Every subsequent event names an event ID
already present in that aggregate. `correlation_id` is non-empty and constant
for the execution attempt. `node_id` is exactly the owner derived from the
payload and current aggregate, including explicit `null` for aggregate-level
events; an unknown or mismatched owner is rejected. Causation, correlation,
and node ownership participate in semantic event identity and replay checks.

Effect-result events also carry an optional typed observation binding containing
the exact durable effect-intent ID and safe canonical request digest. The
binding, causation, execution attempt, aggregate/repository revisions, and
payload are checked by the atomic outbox commit and retained in event identity;
ordinary reducer events carry explicit `null`. No display or semantic-key
parsing supplies this authority.

The pre-freeze private schema-v1 envelope omitted this authority, and the
pre-freeze snapshot omitted the required protocol mode. Both old wire shapes
are deliberately rejected. There is no serde default, inferred context, or
silent migration on strict restore; affected private fixtures must be
regenerated or passed through a separately reviewed explicit importer.

## Canonical outcomes

```rust
enum MissionOutcomeV1 {
    Succeeded,
    SucceededNoOp,
    PartialReviewable,
    BlockedNoDiff,
    NoValidRepair,
    InsufficientEvidence,
    ValidationFailed,
    BudgetBlocked,
    InfrastructureFailed,
    PublicationFailed,
    Canceled,
}

enum ProcessHealth {
    Healthy,
    Degraded { code: ProcessHealthCode },
    Failed { code: ProcessHealthCode },
}
```

Terminal resolution is exhaustive and lives in one pure function:

| Outcome | Required terminal proof |
| --- | --- |
| `Succeeded` | Current engineering work complete, required gates passed, review complete, and non-draft publication confirmed. |
| `SucceededNoOp` | Current repository evidence proves the objective already satisfied and the validated plan/evaluation authorizes no mutation. |
| `PartialReviewable` | Engineering complete, gates passed, PR confirmed, and only an explicit external-human-review criterion remains. |
| `BlockedNoDiff` | Work cannot safely produce a verified diff and the repository is not proven to be a legitimate no-op. |
| `NoValidRepair` | Current validation failure has no eligible, feasible repair target within policy. |
| `InsufficientEvidence` | Bounded discovery/planning cannot ground a safe executable plan. |
| `ValidationFailed` | Required current-revision validation remains failed after allowed repair policy is resolved. |
| `BudgetBlocked` | The owning node is exactly exhausted, deterministic convergence failed, and required work remains. |
| `InfrastructureFailed` | A typed local/control-plane/provider/process failure prevents protocol progress while terminal write authority remains valid. |
| `PublicationFailed` | Publication eligibility was granted but commit/push/PR reconciliation exhausted its typed retry policy. |
| `Canceled` | An authorized cancellation was observed and active effects were stopped/checkpointed. |

The resolver matches structured state and typed failures. It never parses a
message. Adding an outcome or failure kind makes the resolver and telemetry-code
mapping non-exhaustive at compile time.

`PartialReviewable` is successful/reviewable only when the engineering work is
verified, required gates pass, a PR exists, and the remaining criterion
requires external human review. It is not a substitute for incomplete code or
failed validation.

Cancellation and lease loss remain structured hosted control outcomes.
Cancellation may persist `Canceled` only while valid terminal-write authority
exists. Confirmed lease loss must suppress all worker writes and must not be
converted to review/publication infrastructure convergence under stale
authority. The private Phase 7 reducer proves canonical mappings from its
recorded domain facts, but does not implement the control-plane lease/cancel
checks, terminal CAS, callback, or outbox boundary.

## Publication eligibility

`PublicationEligibilityGranted` may be reduced only if all applicable
predicates hold:

1. `CurrentRepositoryRevision`: the review, completion, authority, and
   engineering ancestry bind the aggregate's current revision;
2. `ImplementationBarrierAncestry`: the exact clean-or-repaired implementation
   barrier and required-validation proof chain is current;
3. `VerifiedChangesPresent`: the exact diff contains verified changes;
4. `RequiredValidationCurrent`: every required gate has a current pass;
5. `NoActiveValidationFailure`: no current failure revision remains active;
6. `CompleteDiffReviewed`: every exact path/page is reviewed and no blocking
   finding or unsafe/incomplete plan assessment remains;
7. `CompletionPermitsRequestedMode`: completion and the signed normal or
   external-review mode agree;
8. `SignedPublicationCoordinates`: repository, installation, base revision,
   base ref, head branch, expected remote head, and contract identities match;
9. `CancellationAbsent`: the read-only authority observation reports no
   cancellation;
10. `LeaseValid`: that same observation reports a valid lease;
11. `RemoteHeadUnchanged`: observed and signed expected remote heads agree;
12. `NoActiveWorkOrReservation`: no work owner or reservation remains active.

The backend should validate the same typed predicate version. It must not
weaken validation globally; a mismatch is a versioned contract error with the
failed predicate IDs.

This package currently proves the predicate and publication state machine only
inside the private reducer and fixtures. Real artifact-store resolution of
diff-page addresses, hosted Git/GitHub effects, control-plane
lease/cancellation observations, callback/outbox delivery, production routing,
live publication, and positive no-op authority are explicit later integration
boundaries.

## Retry semantics

Retry, replay, repair, and continuation are different protocol operations:

- **Replay** submits the same semantic event/effect identity. It must return the
  existing result or detect an identity/payload conflict without new spending.
- **Dispatch replay before reconciliation** retains the current action/call
  identity and cannot invent a second reservation. Once the control plane
  definitively reconciles that call as uncontacted and releases it, the bounded
  mutation transport retry is a new deterministic action, call, and reservation
  under the same mutation policy, chained to the released action. An ambiguous
  result is reconciled with the control plane before any further call.
- **Model retry** is a new admitted mutation attempt under the same graph node,
  consumes a call, and is legal only for a typed invalid candidate with
  remaining node budget.
- **Mutation fallback** is a new target-local attempt selected by deterministic
  feasibility policy. It is not a transport retry.
- **Validation rerun** is a deterministic command attempt bound to a new
  repository revision. It spends no model call.
- **Control-plane/event retry** uses the same event ID and expected revision;
  it cannot rerun the underlying provider, command, mutation, or publication
  effect.
- **Repository conflict or remote branch movement** never retries blindly. It
  first produces a new observation and a reducer decision to rebuild context,
  revalidate, reconcile, or stop.
- **Continuation** starts a new execution epoch from an explicitly resumable
  checkpoint. It does not reopen terminal state in the prior epoch.

Retryability is an enum/property of the typed effect error. Display text and
provider prose are never consulted. Every retry policy names its owner, maximum
attempts, backoff/deadline policy, and whether the attempt consumes node or
mission budget.
