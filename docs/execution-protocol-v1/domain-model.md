# Protocol v1 domain architecture

The types in this document are design-level Rust. Names and fields are intended
to make invalid states difficult to represent; wire details are finalized only
after backend contract review.

## Aggregate and reducer

```rust
struct ExecutionState {
    protocol_version: ProtocolVersion,
    aggregate_revision: u64,
    execution: ExecutionIdentity,
    objective: MissionObjective,
    repository: RepositoryState,
    profile: Option<RepositoryProfile>,
    plan: Option<AcceptedPlan>,
    graph: ExecutionGraphV1,
    position: ProtocolPosition,
    evidence: EvidenceLedger,
    failures: FailureLedger,
    budgets: BudgetLedger,
    publication: PublicationLedger,
    terminal: Option<CanonicalResult>,
}

fn decide(state: &ExecutionState) -> Result<Decision, ProtocolViolation>;
fn reduce(state: &ExecutionState, event: &DomainEvent)
    -> Result<ExecutionState, ProtocolViolation>;
```

The aggregate contains no HTTP client, filesystem handle, process, Git handle,
clock, or provider adapter. `decide` and `reduce` are deterministic. Time,
repository content, provider output, process output, and remote state enter only
as validated observations in typed events.

The event append transaction is:

1. load aggregate at expected revision;
2. reject an event ID previously stored with different canonical bytes;
3. reduce against a clone;
4. run global and event-specific invariants;
5. append event and snapshot with compare-and-swap;
6. publish telemetry from the committed event.

An exact replay returns the already-committed revision and does not repeat
progress, spending, mutation, or publication.

## Identities and repository revisions

Opaque newtypes prevent accidental namespace mixing:

```rust
ExecutionId, ExecutionAttempt, NodeId, TargetId, PlanId, EvidenceId,
RepositoryRevisionId, FailureRevisionId, RepairIntentId, ReservationId,
ModelCallId, ValidationRunId, PublicationAttemptId, EventId
```

Semantic IDs use canonical serialization and a protocol-specific namespace.
Random IDs may identify transport attempts but must not define replay identity.

`RepositoryState` records:

- immutable base SHA;
- current source-tree hash;
- monotonic repository revision ID;
- changed-path set and per-path hashes;
- dependency/lock fingerprint;
- generated-file policy version.

Every successful verified mutation advances the repository revision. Evidence
that describes content or validation always names the revision it observed.

## Node model

```rust
enum NodeKindV1 {
    Discovery,
    Planning,
    Implementation { target: TargetId, intent: ImplementationIntent },
    Validation { gate: ValidationGateId },
    ValidationRepair { failure: FailureRevisionId, target: TargetId },
    Review,
    CompletionEvaluation,
    Publication,
}

enum NodeStateV1 {
    Pending,
    Ready,
    Active(ActiveStep),
    Waiting(EffectId),
    Succeeded(NodeProofId),
    FailedRecoverable(FailureRevisionId),
    FailedTerminal(FailureRevisionId),
    Superseded { by: RepositoryRevisionId },
    Skipped(SkipProof),
}
```

There is no generic `Applied` status whose meaning varies by kind. Success is
always paired with the proof required by that node kind. The graph stores
dependencies and one stable node order; the reducer enforces at most one
active owner. Repair nodes are first-class target-bound nodes, not a phase flag
or a synthetic budget session.

An implementation node may be skipped only with a typed proof such as
`AlreadySatisfied` bound to the accepted intent and current repository
revision. That positive proof authority is not implemented through Phase 7,
so private v1 planned implementation nodes currently reject `AlreadySatisfied`
fail closed. A required validation node may never be skipped by model output.

## Repository profile

`RepositoryProfile` is produced deterministically before model-led discovery:

```rust
struct RepositoryProfile {
    schema_version: u16,
    profile_id: RepositoryProfileId,
    repository_revision: RepositoryRevisionId,
    ecosystems: Vec<EcosystemCapability>,
    source_roots: Vec<ProfilePath>,
    test_roots: Vec<ProfilePath>,
    metadata_files: Vec<MetadataObservation>,
    dependency_files: Vec<ProfilePath>,
    generated_rules: Vec<GeneratedPathRule>,
    validation_candidates: Vec<ValidationCommandCandidate>,
    repository_size: RepositorySizeClass,
    text_file_limits: FileSizePolicy,
    uncertainties: Vec<ProfileUncertainty>,
}
```

Profile adapters parse known metadata formats through a registry of small,
deterministic detectors. Unknown files remain unknown; the profile never
pretends to understand an ecosystem. A generic fallback records directory and
text-file structure without inventing build commands.

Each inferred validation command carries provenance and trust:

```rust
enum CommandProvenance {
    SignedExecutionPolicy,
    ParsedProjectMetadata { evidence_id: EvidenceId },
    ParsedCiConfiguration { evidence_id: EvidenceId },
}
```

Repository metadata guides command selection but cannot expand the signed
process policy. A command is executable only when both profile provenance and
the manifest's command policy authorize it.

Generated-path rules use metadata evidence (generator config, generated-file
markers, checked-in API manifests, output directories) and express whether a
path is read-only, regenerated through an authorized command, or ordinary
source. A generated output is never directly mutated merely because a model
selected it.

## Evidence architecture

Evidence is immutable and typed:

```rust
enum Evidence {
    Search(SearchEvidence),
    Candidate(CandidatePathEvidence),
    File(FileEvidence),
    Relationship(RelationshipEvidence),
    ImpactMap(ImpactMapEvidence),
    Plan(PlanEvidence),
    TargetContext(TargetContextEvidence),
    MutationIntent(MutationIntentEvidence),
    MutationVerification(MutationVerificationEvidence),
    ImplementationBarrier(ImplementationBarrierProof),
    Validation(ValidationEvidence),
    RepairEligibility(RepairEligibilityEvidence),
    Review(ReviewEvidence),
    Completion(CompletionEvidence),
    Publication(PublicationEvidence),
}
```

All evidence includes `evidence_id`, producer node/event, repository revision,
schema version, and a safe summary. Content evidence additionally records path,
line/range, content hash, truncation, encoding, and a content-addressed blob
reference. Raw content is not copied into the control-plane notebook or event
payload.

Authoritative storage and model context are separate:

- the evidence ledger may retain all bounded observations required for replay;
- the event stream retains identities, hashes, provenance, and safe summaries;
- a local encrypted/content-addressed artifact store may retain permitted raw
  excerpts and command outputs;
- `ContextBuilder` selects only evidence needed for one action and verifies the
  current repository hash before materializing content.

## Context architecture

Every provider call receives a purpose-built `ContextManifest`:

```rust
struct ContextManifest {
    action_id: ActionId,
    node_id: NodeId,
    purpose: ModelPurpose,
    repository_revision: RepositoryRevisionId,
    evidence_ids: Vec<EvidenceId>,
    mandatory_sections: Vec<ContextSection>,
    optional_sections: Vec<ContextSection>,
    input_token_ceiling: u32,
    estimated_input_tokens: u32,
    compaction: Vec<CompactionDecision>,
    materialized_context_hash: ContentHash,
}
```

Materialization order is deterministic:

1. reserve fixed tokens for protocol instructions and output schema;
2. include ticket goal and only acceptance criteria relevant to the action;
3. include the active plan/target or failure revision;
4. include latest authoritative target content or required ranges;
5. include supporting evidence in stable ranked order;
6. include target-local failure history newest-first within a fixed cap;
7. omit optional evidence until the estimate fits;
8. use bounded ranges or deterministic summaries where their evidence type
   permits it;
9. fail `ContextTooLarge` if mandatory content alone exceeds the signed input
   ceiling.

Compaction decisions are persisted and observable. The builder never silently
truncates a target file when a mutation strategy requires its complete content.
Historical model turns are not authoritative context and are omitted unless a
specific action requires one bounded prior response by ID.

### Private Phase 4 target-context slice

The implemented private slice specializes this architecture for initial
implementation targets without enabling mutation. The graph-selected active
node determines the target; a deterministic `TargetContextLoadRequest` binds
the execution attempt, node attempt, plan and target identities, repository
revision, target-relevant criteria, required and optional evidence IDs,
validation expectations, signed input ceiling, and operation-owned path
expectations. Create requires an absent destination, modify/delete require the
expected current source hash, and move requires both the expected source and an
absent destination.

The read-only materializer returns exact path observations and
content-addressed evidence artifacts. Raw artifact bytes are used only to
verify hashes, encoding, full-file/range scope, and the bounded context token
projection; they have a redacted `Debug` representation and are not
serializable protocol fields. A
successful result persists an `ArtifactReceipt`-based `TargetContextManifest`
with the target content selection, mandatory sections, selected optional
sections, token estimate, compaction decisions, repository fingerprint, and
stable materialized/manifest hashes. Replay reconstructs the implementation
projection from `TargetContextPrepared` events and revalidates it against the
authoritative load request.

Mandatory sections are never silently omitted. If full target content does not
fit, only exact evidence-backed ranges may replace it; if the minimum legal
projection still exceeds the signed ceiling, preparation returns the typed
`implementation_context_too_large` error without changing aggregate state.
Optional evidence is admitted only when it is target-local and fits, otherwise
its deterministic omission is recorded.

Phase 4 itself ends at `ImplementationContextReady`. The private Phase 5 slice
now consumes that boundary, but Phase 4 still does not persist a domain
failure/convergence event for a loader adapter error; failed preparation
remains an atomic typed boundary error until later effect-lifecycle work.

## Action envelope and tool admission

The reducer chooses an `ActionClass`; the admission policy turns it into an
immutable `ActionEnvelope`:

```rust
struct ActionEnvelope {
    action_id: ActionId,
    node_id: NodeId,
    action_class: ActionClass,
    repository_revision: RepositoryRevisionId,
    context_manifest_id: ContextManifestId,
    allowed_tools: Vec<ToolAuthorization>,
    tool_choice: ToolChoice,
    input_token_ceiling: u32,
    output_token_allowance: u32,
    budget_owner: NodeId,
    reservation_id: ReservationId,
}
```

Provider serialization accepts an `ActionEnvelope`, not a phase plus defaults.
It has no code path that adds tools after admission. Before dispatch, a
contract check canonicalizes the actual payload tool names and requires exact
equality with the authorized tool names. The same payload hash is stored with
the reservation event.

### Exact tool matrix

| Protocol action class | Exposed tools | Required restrictions |
| --- | --- | --- |
| Discover candidates | `list_files`, `search_text` | Scope/query count bounded; no mutation; only while candidate evidence is insufficient. |
| Ground candidate evidence | `read_file` or `read_files` | Path enums equal the ranked candidate set; named choice forced when one operation is selected; no search. |
| Resolve a named relationship | `related_tests`, targeted `search_text`, bounded reads | Search scope and semantic question are fixed by the reducer; unavailable when mandatory first read is pending. |
| Record impact map | `record_impact_map` | No repository tools. |
| Record plan | `record_plan` | No repository or mutation tools; target schema requires exact operations and evidence IDs. |
| Modify existing target | Feasible canonical subset of `apply_patch`, `replace_file` | Every schema binds one exact path and expected hash; replacement is included only when complete context and conservative output bounds fit. |
| Create target | `create_file` | Exact path enum and creation specification; no patch/replace/delete. |
| Delete target | `delete_file` | Exact path and expected current hash. |
| Move target | `move_file` | Exact source/destination and expected hash. |
| Mutation fallback or model retry | Exactly the selected feasible strategy | Named tool choice forced; no default mutation tool rehydration. |
| Record semantic diff review | `record_diff_review` | Exactly one immutable content-addressed path/page binding; strict schema; forced named choice; no reads, mutation, additional properties, or parallel calls. |
| Record completion evaluation | `record_completion_evaluation` | Complete plan/ancestry/manifest/review context; strict schema; forced named choice; no repository or mutation tools, additional properties, or parallel calls. |
| Profiling, apply, verify, validation, eligibility, publication, terminal | none | These are deterministic worker actions. |

Tool responses are candidates only. The reducer validates schema, current
action ID, target binding, repository revision, and output completeness before
creating a candidate event. A provider cannot request a forbidden tool because
it is absent from the serialized payload; a malformed response is a typed
provider-contract failure.

## Planning and graph materialization

```rust
struct PlannedTargetV1 {
    target_id: TargetId,
    change_id: ChangeId,
    path: RepositoryPath,
    operation: TargetOperation,
    role: TargetRole,
    rationale: String,
    acceptance_criteria: NonEmpty<CriterionId>,
    required_evidence: NonEmpty<EvidenceId>,
    expected_validation: Vec<ValidationExpectation>,
    dependencies: Vec<TargetId>,
    estimated_change: ChangeEstimate,
}
```

Plan validation checks:

- repository-relative normalized paths and typed create/delete/move semantics;
- complete, non-truncated current evidence for each existing target or a typed
  creation specification grounded in current criterion evidence;
- all required acceptance criteria covered collectively and each target/
  criterion pair admitted by that criterion's impact area;
- dependency acyclicity and stable order;
- generated-file policy;
- validation expectation provenance and collective validation coverage for
  every criterion claimed by each target;
- non-vague targets; repository-scoped operations require a registered
  deterministic executor rather than an arbitrary path string;
- a feasible minimum node budget for each required operation, sourced from a
  trusted per-kind graph contract rather than copied from the planning node,
  within remaining mission capacity.

Only `PlanAccepted` materializes target and validation nodes. Repairing a plan
creates a new plan revision; it never mutates an accepted graph in place.

## Target-local mutation architecture

`TargetExecutionContext` contains only:

- goal and target-relevant criteria;
- the accepted target and operation;
- current complete content when the selected strategy requires it, otherwise
  the smallest sufficient exact ranges;
- required evidence and neighboring interfaces selected by ID;
- target-local prior failures for the current intent;
- expected validation;
- current repository revision and content hashes;
- feasible strategy set and remaining node budget.

### Feasibility

For every candidate strategy, `MutationFeasibility` computes:

```rust
struct MutationFeasibility {
    strategy: MutationStrategy,
    legal_for_operation: bool,
    target_size_bytes: u64,
    required_context_tokens: u32,
    worst_case_output_tokens: u32,
    serialized_tool_overhead_tokens: u32,
    output_allowance: u32,
    context_fits: bool,
    output_fits: bool,
    reason_code: FeasibilityReason,
}
```

The estimate uses a conservative encoding factor plus JSON/tool-call overhead.
`replace_file` and `create_file` require the complete candidate to fit with a
safety margin. Patch feasibility reserves syntax/context overhead and can be
bounded to exact ranges. Truncated provider output is rejected before mutation
parsing and never applied.

Fallback selection is a pure function of operation, typed failure, prior
strategy, target facts, feasibility results, and remaining attempts. Example:

```text
malformed patch + replacement fits -> force replace_file
malformed patch + replacement cannot fit + bounded patch retry is distinct
    -> force apply_patch with normalized exact context
repository revision changed -> rebuild context without provider mutation
no legal/feasible strategy -> NoSafeFallback
```

The selected fallback is persisted in `AttemptPolicySelected` and copied
unchanged into the next prepared action. It is scoped to one node attempt and
cannot leak to another target.

### Private Phase 5 mutation slice

The implemented private slice expresses this design through
`MutationFeasibilitySet`, `MutationAttemptPolicy`,
`PreparedMutationAction`, and `MutationProviderRequestContract`. Feasibility is
recomputed and reducer-checked against the active implementation node, accepted
target, prepared context, repository revision, signed context/output ceilings,
target size, complete-content availability, tool-schema overhead, and maximum
candidate bytes. Canonical initial strategies are:

```text
ModifyExisting -> feasible subset [apply_patch(initial), replace_file]
CreateFile     -> [create_file]
DeleteFile     -> [delete_file]
MoveFile       -> [move_file]
```

The policy owns the exact permitted strategy list. Initial multi-tool modify
uses `tool_choice: "required"`; singleton initial policies, model retries, and
fallback policies use a named tool choice. The provider request uses strict
function schemas with exact path/hash enums, bounded `patch` or `content`
strings, `additionalProperties: false`, and `parallel_tool_calls: false`.
Action, call, reservation, context, repository, attempt, budget owner, token
ceilings, and the hash of the exact canonical serialized request are all
cross-checked before dispatch. This serialized request—not an adapter-local
allowlist or prompt—is the authority.

Typed candidate failures determine recovery without parsing display strings.
Schema/hash/encoding failures that are safe to regenerate select a forced model
retry; malformed patch or apply rejection selects `replace_file` when feasible
and otherwise a normalized bounded patch retry; repository drift selects a
context rebuild; non-retryable contract, ownership, size, or verification
failures converge. Every retry/fallback is a new monotonically indexed mutation
attempt bound to the previous attempt. No feasible initial strategy,
call/cost/duration admission exhaustion, mutation-attempt exhaustion,
context-rebuild exhaustion, no safe fallback, and repeated definitively-
uncontacted release are explicit typed convergence facts.

Candidate-bearing arguments cross the materialization boundary as
`DurableMutationArtifact`. Raw bytes are private, non-serializable, and redacted
from `Debug`. The event-safe `MutationArtifactReceipt` contains a SHA-256
content address, hashed store locator, deterministic persistence-receipt hash,
content hash, byte length, and encoding. Patch candidates additionally retain
an exact expected-after artifact, so later verification proves the intended
result rather than merely proving that a file changed. These handles are the
private contract types; a real durable store implementation and independent
store-existence contract test remain deferred.

A provider action follows the ordinary admission/reservation/dispatch/
reconciliation lifecycle. When the control plane definitively reconciles it as
uncontacted, the reservation is released. The reducer may prepare one bounded
transport action retry under the same `MutationAttemptPolicy`, but derives a
distinct action ID, model-call ID, and reservation ID from the next action index
and prior released action ID. Ambiguous contact cannot use this path. Repeated
definitive releases produce `UncontactedActionRetryExhausted` rather than
waiting forever. Only a call reconciled as consumed increments node and mission
`mutation_attempts`; an uncontacted release does not.

The ledger stores context history, every mutation attempt, and the complete
ordered action chain. Its cached current projections are derived conveniences,
not alternate authority. Replay rebuilds budgets, context history, rolling
repository revisions, candidate/apply/verification state, action release
chains, convergence, node failure, and terminal result, and rejects any ID,
binding, ordering, or usage mismatch.

An implementation `NodeFailed` event is legal only when it names the exact
failure revision produced by current readiness or mutation convergence. The
same convergence fact drives one canonical terminal mapping: no feasible
strategy and ordinary no-safe-fallback cases become healthy `BlockedNoDiff`;
admission, mutation-attempt, or context-rebuild exhaustion becomes healthy
`BudgetBlocked`; repeated definitively-uncontacted release and the currently
classified provider-protocol or artifact-durability failures become failed
`InfrastructureFailed`. The terminal reducer compares the complete expected
result, blocker code, node, revision, and process health rather than accepting
an outcome label alone.

### Verified writes

Apply and verification are two separately derived, deterministic effects:

1. validate the active node, attempt, candidate, intent, target operation,
   before revision/fingerprint, expected hashes, and owned path set;
2. derive the exact apply request from the persisted candidate; a production
   effect outbox must persist that request before invoking the isolated
   repository adapter;
3. record only a matching `Applied` or `AlreadyApplied` application observation;
4. derive a separate verification request from the accepted application;
5. independently enumerate all changed paths and before/after path states;
6. require the changed paths to equal the operation's ownership set and the
   transitions to prove exact create/modify/delete/move semantics;
7. compute the after fingerprint and deterministic next repository revision;
8. persist `MutationVerified`, then reduce the node to success and advance the
   rolling repository revision.

No model-authored success message, tool invocation, or process exit alone can
complete a mutation node. An application observation alone also cannot advance
the repository revision.

The current private aggregate does not yet contain separate
`ApplicationRequested` or `VerificationRequested` events. Their effect IDs and
payloads are deterministically reconstructed from the persisted candidate and
application observation, so replay requests are stable, but durable outbox
journaling and crash reconciliation at the real adapter boundary remain Phase
9 integration work rather than a property claimed by this private slice.

Repository drift is not disguised as a failed write. A drift failure binds the
expected and observed revisions and fingerprints. Within the context-rebuild
budget it authorizes exactly one `TargetContextSuperseded`, adopts the observed
revision as the aggregate's current revision, retains the prior context in
history, and requires a fresh context and rebuilt attempt policy before any
new provider dispatch. At exact rebuild exhaustion it converges to a typed
budget block instead of attempting an invalid supersession.

## Validation architecture

The private Phase 6 gate set is constructed, not discovered at execution time.
An accepted-plan expectation names a repository-profile command candidate, and
that candidate must also appear in `ValidationPolicyV1.authorizations`. Required
broad candidates are supplied by the same policy and must be present in the
profile with a non-focused class. The policy binds its repository-profile ID,
signed-policy evidence ID, parser, timeout, output/run limits, environment and
dependency fingerprints, and the separate repair-node budget. Profile metadata
can narrow this intersection but cannot expand the policy.

Validation gates are typed and canonically ordered:

```rust
enum ValidationGateClass { Focused, TestSuite, Build, Typecheck, Lint, Metadata }

struct ValidationGateV1 {
    gate_id: ValidationGateId,
    node_id: NodeId,
    class: ValidationGateClass,
    command: AuthorizedValidationCommand,
    required: bool,
    provenance: ValidationGateProvenance,
    timeout_ms: u64,
    output_limit_bytes: u64,
    max_runs: u32,
    dependencies: Vec<ValidationGateId>,
    repository_revision: RepositoryRevisionId,
}
```

Focused gates come from plan expectations; broader gates come from the signed-
policy/profile intersection. Their stable identities include command argv and
working directory, plan/profile/policy provenance, parser, criterion IDs,
revision, dependency chain, time/output limits, and run ceiling. The reducer
chains gates in canonical class/command/candidate order. Required broad gates
belong to the node that owns the canonically last focused expectation, so that
node cannot succeed before its broader gates finish. The reducer emits and
commits `ValidationScheduled` before it can return the matching `RunProcess`
effect. A run ID additionally binds execution/node attempts and distinguishes
`Initial` from `ExactRepairRerun`.

Before repair selection, the reducer preflights the run ceiling of every
required gate in canonical gate order. An exhausted earlier gate therefore
converges before ranking, node creation, context loading, model use, or mutation
for a later failing gate. After repair, the exact originating gate is always
first. If it is a broad gate, current-revision gates still missing on that same
owner node run next; only then does scheduling resume global canonical order
for the remaining invalidated gates.

The process adapter owns effects, never state. It revalidates live repository
and lease authority immediately before spawn, resolves a repository-contained
working directory, executes the authorized executable plus argument vector
directly without a shell, clears the inherited environment, and installs only
allowlisted values matching the request fingerprint. Start and completion are
separate durable observations; exact already-recorded observations are safe,
while definitely-not-recorded and indeterminate writes remain distinct typed
journal states. Timeout, cancellation, and boundary failures terminate the
process tree.

Stdout and stderr share the gate's output limit. The adapter retains bounded
head/tail bytes, biases truncation capacity toward the failure-relevant tail,
persists each segment through a receipt checked for content hash, locator hash,
persistence-receipt hash, byte length, run, stream, and segment, then zeroizes
the materialized bytes. Events retain only receipts and exact original,
captured, dropped, and truncation metadata. Cargo, Node, Pytest, Go, and generic
parsers convert the ephemeral bytes into bounded diagnostics, repository-
scoped source locations, hashed expected/actual values, implicated paths/test
IDs, parser confidence, and a gate-semantics observation.

Exit status and infrastructure are separate domains. Zero produces `Passed`
only when expected semantics were observed; non-zero, or zero with missing
semantics, produces `DomainFailed` evidence and an exact failure revision.
Spawn, timeout, journal, transport, cancellation, and lease loss are
`ValidationProcessResult::InfrastructureFailure` and cannot construct
validation evidence. The current private reducer maps validation convergence
as follows:

| Convergence | Mission result | Process health | Canonical reason code |
| --- | --- | --- | --- |
| No eligible repair | `NoValidRepair` | `Healthy` | `validation_no_valid_repair` |
| Gate run ceiling reached | `BudgetBlocked` | `Healthy` | `validation_gate_run_budget_exhausted` |
| Authorized cancellation | `Canceled` | `Healthy` | `validation_process_canceled` |
| Spawn | `InfrastructureFailed` | `Failed` | `validation_process_spawn_failed` |
| Timeout | `InfrastructureFailed` | `Failed` | `validation_process_timeout` |
| Journal | `InfrastructureFailed` | `Failed` | `validation_process_journal_failed` |
| Transport | `InfrastructureFailed` | `Failed` | `validation_process_transport_failed` |
| Lease-loss result in the private reducer | `InfrastructureFailed` | `Failed` | `validation_process_lease_lost` |

The local adapter and reducer exercise the typed lease-loss result only. Phase
6 does not claim hosted lease ownership, terminal-write authority, or stale-
lease write suppression.

## Repair architecture

Repair is a deterministic pipeline:

```text
ValidationFailureRevisionRecorded
 -> RepairCandidatesRanked
 -> RepairEligibilityEvaluated (for every candidate)
 -> RepairTargetSelected
 -> ValidationRepairNodeCreated
 -> RepairTargetContextPrepared(ValidationRepair purpose)
 -> feasibility/policy/action/reservation/candidate/apply
 -> MutationVerified
 -> MutationVerified proof
 -> RepairVerified proof
 -> ValidationRepair node succeeded
 -> PriorValidationInvalidated(all evidence from old revision)
 -> ValidationRerunScheduled(originating_gate, new_revision)
 -> ValidationRerunScheduled proof
 -> exact gate run/pass
```

Candidate ranking uses parsed assertions, exact test/source locations,
implicated paths, relationship evidence, acceptance criteria, and target role.
It records component scores and breaks equal scores by target identity, so
candidate order and filenames are not hidden authority.

Eligibility is separate from ranking. A source target may be eligible through
direct failure/relationship evidence. A test target that changes asserted
behavior requires explicit specification/acceptance evidence proving the test
is stale and the exact diagnostic expected/actual hashes must match the policy
authorization. Generated outputs are ineligible in this slice. Every ranked
candidate receives one stable eligible/ineligible reason before selection.

Eligibility also requires a valid current `MutationVerificationEvidence`
baseline with a canonical owner chain. It may be owned by the target's original
implementation node, or by an exact same-target prior repair whose
`MutationVerified` evidence and canonical `RepairVerified` proof extend the
same baseline chain. Its after revision must be the failure revision's
repository revision, and its changed paths and path transitions must contain
exactly the target path. Phase 6 intentionally supports only a currently
existing file: an initial `ModifyExisting` file-to-file transition or
`CreateFile` absent-to-file transition, and a chained repair's file-to-file
transition, are rebased to a fresh
`ModifyExisting { expected_content_hash: current_hash }` repair operation.
Missing, stale, absent-after, delete, move, or multi-path baselines are
ineligible; they cannot reach model admission. An older baseline for another
target is not inferred to remain current across a repair because no
non-interference proof is persisted.

The repair node has its own signed budget and a
`TargetExecutionPurpose::ValidationRepair` purpose, bound to repair intent,
failure revision, originating gate, validation evidence, baseline mutation
evidence, target, plan, and repository revision. It uses a separate repair
context ledger and the same verified target executor, but cannot satisfy an
implementation node, consume its fallback count, inherit its context/action
envelope, or accept a generic provider call before the exact action is
prepared.

After repair `MutationVerified`, the aggregate temporarily permits the mutation
revision to lead validation state while freezing every unrelated event. It must
record the exact mutation proof, then the exact `RepairVerified` proof, then
repair-node success. Only that chain authorizes an invalidation naming every
validation evidence ID from the old revision. The rerun schedule binds that
invalidation, the verified repair evidence, new revision, repair/failure
identities, and exact originating gate. No other gate or work owner may run
first. A passing rerun clears the pending rerun; any other current-revision gate
missing on the same owner runs before global canonical ordering resumes for
the remaining invalidated gates. Only after every required gate passes at the
new revision may required-validation proof advance to review. Replay re-derives
and checks the entire chain.

Repair repository drift is deliberately not rebuilt in Phase 6. A recovery
decision that would rebuild implementation context instead persists
`ContextRebuildUnavailable`, carries the exact observed revision into the
aggregate, and converges terminally without another provider dispatch. Repair
terminal mapping is replay checked:

| Repair convergence | Mission result | Process health | Canonical reason code |
| --- | --- | --- | --- |
| No eligible target | `NoValidRepair` | `Healthy` | `validation_no_valid_repair` |
| No feasible strategy | `NoValidRepair` | `Healthy` | `repair_no_feasible_strategy` |
| No safe fallback | `NoValidRepair` | `Healthy` | `repair_no_safe_fallback` |
| Unavailable context rebuild | `NoValidRepair` | `Healthy` | `repair_context_rebuild_unavailable` |
| Admission budget exhausted | `BudgetBlocked` | `Healthy` | `repair_admission_budget_exhausted` |
| Mutation-attempt budget exhausted | `BudgetBlocked` | `Healthy` | `repair_mutation_attempt_budget_exhausted` |
| Context-rebuild budget exhausted | `BudgetBlocked` | `Healthy` | `repair_context_rebuild_budget_exhausted` |
| Repeated definitely-uncontacted action | `InfrastructureFailed` | `Failed` | `repair_uncontacted_action_retry_exhausted` |
| Provider protocol failure | `InfrastructureFailed` | `Failed` | `repair_provider_protocol_failure` |
| Non-durable artifact | `InfrastructureFailed` | `Failed` | `repair_artifact_not_durable` |

This private slice has a real local subprocess adapter, but no hosted/backend/
CLI route or production event contract. The real durable mutation-artifact
store, mutation provider, and filesystem adapters remain deferred from Phase
5. Phase 7 now consumes the clean or exact repaired ancestry for a private
review/publication checkpoint. Durable Phase 4 loader-failure convergence,
cross-target baseline carry-forward without a persisted non-interference proof,
and all real hosted side-effect integration remain outside Phase 6.

## Private Phase 7 review and publication checkpoint

Phase 7 remains a pure private aggregate slice, but review and publication are
no longer unspecified placeholders. `ReviewStateV1` owns the signed
finalization policy, accepted plan and revision, current repository revision,
clean-or-repaired `EngineeringAncestryV1`, exact diff request/outcome, model
action reconciliation, page reviews, aggregate review, completion, read-only
publication authority, eligibility, and convergence.

The ancestry is reducer-derived, not model supplied. A clean execution binds
the implementation barrier and current required-validation proof. A repaired
execution additionally carries the canonical ordered root-to-current proof
chain through every exact `MutationVerified`, `RepairVerified`, invalidation,
rerun, and required-validation handoff. The ancestry endpoint, repository
fingerprint, and required-validation proof must equal the current revision.

### Complete diff and artifact authority

`DiffManifestRequestV1` binds the review node, accepted plan hash, finalization
policy, repository/installation-independent publication repository binding,
signed base ref and base revision, current revision and fingerprint,
required-validation proof, and path/page/byte ceilings. Its materialized form
retains raw bytes only in non-serializable, redacted, zeroized values.

The narrow v1 diff invariant is one page per changed path. At index `i`:

- page index is `i` and its coverage set is exactly `{i}`;
- raw page SHA-256 equals `DiffPathRecordV1.patch_hash`;
- raw page length equals `DiffPathRecordV1.patch_bytes`;
- the durable receipt repeats that content hash and length and binds a
  non-secret `sha256:<content_hash>` address, artifact-locator hash, and
  persistence-receipt hash.

Both raw materialization and deserialized `DiffManifestV1` revalidate the same
relationship. Cardinality, ordering, path ownership, plan operation, total
bytes, and before/after fingerprint equality are also exact. This prevents a
manifest from naming path metadata while presenting different page bytes.

Real resolution of the `sha256:` address is not implemented here. A future
scoped artifact-store port must resolve it, verify content hash, byte length,
locator hash, and persistence receipt, and supply bytes without serializing a
credential, signed URL, or raw diff into protocol state.

### Record-only review and completion

Every provider action is persisted before dispatch and owns one forced tool:
`record_diff_review` or `record_completion_evaluation`. `ReviewToolDefinitionV1`
contains the exact strict schema and schema hash; the envelope binds the named
choice and `parallel_tool_calls = false`. Released calls may be retried only up
to the signed per-binding ceiling with distinct chained action/call/reservation
IDs. Consumed calls alone spend provider usage.

`conservative_review_input_tokens` deterministically serializes the strict
schema plus accepted plan, ancestry, manifest, exact binding, criterion and
evidence IDs, and—during completion—the aggregate review. It then charges six
bytes for every referenced raw byte, adds fixed provider overhead, uses one
token per estimated byte, and saturates safely. A page review charges its one
page; completion charges all manifest bytes. Provider adapters may not append
ambient context outside this hash-bound material.

`DiffPlanAssessmentV1` deterministically owns each changed path by exactly one
accepted target and records missing targets, operation mismatches, and
unplanned paths. Any such condition is blocking. Non-advisory unsafe,
incomplete, unplanned, or criterion-evidence-gap findings must also be
blocking. Completion re-derives, per criterion, exact target/path indexes and
validation-expectation IDs from the accepted plan and current ancestry. A
model status cannot override an incomplete deterministic record.

`FinalizationPolicyV1.external_review_criteria` is signed authority. For a
classified criterion, `Satisfied` is invalid; the only resolved status is
`ExternalReviewRequired` with the exact mapped `ExternalReviewKindV1`.
Unclassified criteria cannot invent external review. `Unsatisfied` and
`Uncertain` remain incomplete. Thus `CompletePendingExternalReview` cannot be
used to hide incomplete engineering work.

### Eligibility, failures, and convergence

Publication eligibility stores all twelve typed predicate results rather than
a single model recommendation: current revision, barrier ancestry, verified
changes, current required validation, no active validation failure, complete
diff review, completion/mode agreement, signed coordinates, cancellation
absence, lease validity, unchanged remote head, and no active work or
reservation. A denial is durable and terminally blocked; only `Granted` may
initialize publication state.

The two review-side effects have persisted failure outcomes. A
`DiffManifestEffectFailureV1` is bound to the exact outstanding request, effect,
review node, expected revision, and fingerprint and carries one typed reason:
an exact limit/observed count, an observed revision/fingerprint drift, or a
safe artifact-durability code. A
`PublicationAuthorityEffectFailureV1` binds its outstanding request, effect,
policy, contract, completion, and revision and carries only safe authority
unavailability. Neither type has a lease-loss variant.

Failure records project a candidate `ReviewConvergenceReasonV1`; they do not
authorize terminal state themselves. The reducer must emit the exact matching
`ConvergenceEvaluated`, and replay rejects an effect-derived convergence
without its bound failure record or with a different reason. Once an effect
failure is recorded, only that convergence may follow. Every effect-derived
reason repeats the stored failure ID and failure hash—`DiffManifestFailureId`
for limit, drift, and artifact durability, or
`PublicationAuthorityFailureId` for unavailable authority—so equal safe codes
or revisions from distinct observations cannot collapse their proof ancestry.

Review terminal mapping is exhaustive and display-text independent:

| Review convergence | Mission result | Process health | Canonical reason |
| --- | --- | --- | --- |
| Diff limit, blocking diff, incomplete completion, denied eligibility | `BlockedNoDiff` | `Healthy` | Exact review/completion/eligibility code |
| Review or completion input/call budget exhausted | `BudgetBlocked` | `Healthy` | Owning node budget code |
| Repository drift while building the diff | `ValidationFailed` | `Healthy` | `review_repository_drift` |
| Artifact durability, provider protocol, repeated definitely-uncontacted release, authority unavailable | `InfrastructureFailed` | `Failed` | Exact typed infrastructure code |

### Intent-first publication reconciliation

`PublicationStateV1` is initialized only from a current granted eligibility
record and its signed `PublicationContractV1`. Each operation has a monotonic
attempt identity, operation-local attempt number, prior-attempt link,
repository revision, and eligibility ID.

The reducer persists `CommitIntentV1` before `CreateCommit`. Its tree binding is
derived from the read-only authority observation and fixes manifest/diff,
repository tree OID, parent commit OID, and commit identity hash. A confirmed
observation must prove tree, parent, metadata identity, and either `Created` or
`AlreadySatisfied` reconciliation.

Only a confirmed commit permits an `ExactLeasePushIntentV1`, which is persisted
before `PushExactLease` and binds the exact expected remote head and commit OID.
The observation is `Pushed`, `AlreadySatisfied`, exact
`RemoteBranchMoved`, or a typed failure. Only a confirmed push permits a
`PullRequestIntentV1`, persisted before `EnsurePullRequest`; retries must retain
the exact title/body hashes and lengths, draft bit, base/head coordinates, and
execution marker. A confirmed observation binds the PR number/URL, node ID,
observed head, coordinates, marker, and draft state.

An ambiguous transport result leaves the same intent open for reconciliation;
it cannot allocate a retry. Definitive retryable failures allocate a distinct
chained attempt only below the signed operation ceiling. Permanent failure,
ceiling exhaustion, or remote movement produces exact publication convergence.
Every such `PublicationConvergenceV1` binds `final_attempt_id`, a typed
`final_observation_id: PublicationObservationIdV1` discriminating commit, push,
or pull-request, and `final_observation_hash`. The convergence ID/hash include
that complete tuple, and revalidation requires equality with the last persisted
observation; neither an attempt ID nor a normalized reason alone is terminal
ancestry. A confirmed normal, non-draft completion maps to healthy `Succeeded`.
A confirmed draft completion in signed external-review mode, with completion
`CompletePendingExternalReview`, maps to healthy `PartialReviewable`.
Publication convergence maps to failed `PublicationFailed` with an exact
commit, push, pull-request, or remote-movement reason code.

These contracts do not perform external work. Real artifact-store resolution,
hosted Git/GitHub calls, control-plane lease/cancellation observations and
terminal-write authority, callback/outbox delivery, backend event schemas,
production routing, live publication, and positive no-op authority remain
explicitly deferred.

## Budget and reservation architecture

```rust
struct NodeBudgetContract {
    max_model_calls: u32,
    max_cost_micros: u64,
    max_duration: Duration,
    max_mutation_attempts: u32,
    max_context_rebuilds: u32,
    max_input_tokens_per_call: u32,
    max_output_tokens_per_call: u32,
}

struct NodeUsage {
    calls_reserved: u32,
    calls_consumed: u32,
    cost_reserved_micros: u64,
    cost_consumed_micros: u64,
    duration_consumed: Duration,
    mutation_attempts: u32,
    context_rebuilds: u32,
}
```

Mission totals constrain the sum, but node reservations never move merely
because position changes. If policy allows unused-capacity reallocation, it
must be an explicit signed `BudgetReallocated` event naming source, destination,
amount, and rule—not an implicit phase transition.

Provider call lifecycle:

```text
ModelCallAdmissionEvaluated
 -> ModelCallReserved
 -> ProviderDispatchStarted
 -> ProviderDispatchCompleted | ProviderDispatchFailed
 -> ModelCallReconciled(consumed | released, actual usage)
```

Rules:

- dispatch requires a persisted active reservation and matching payload hash;
- at most one active reservation belongs to a node attempt;
- reservation counts against remaining budget;
- contacted/chargeable calls reconcile as consumed exactly once;
- definitively uncontacted calls release exactly once;
- ambiguous provider contact follows the control-plane's authoritative usage
  record and never guesses;
- a released mutation action may be followed only by the protocol's bounded
  same-policy action retry, with distinct deterministic action, call, and
  reservation IDs chained to the released action;
- `mutation_attempts` increments once per distinct mutation attempt that has a
  consumed call, not when an action is prepared, admitted, reserved, or
  definitively released;
- `consumed == max` is valid and means no new calls;
- `consumed + reserved > max` is an invariant violation;
- convergence from existing evidence runs before a terminal budget decision.

Validation command budgets are deterministic-effect budgets, separate from
model-call counts. A validation rerun cannot consume repair model capacity.

## Event schema and observability

Every stored event uses a common envelope:

```rust
struct EventEnvelope<T> {
    protocol_version: u16,
    event_id: EventId,
    execution_id: ExecutionId,
    execution_attempt: u32,
    sequence: u64,
    aggregate_revision_before: u64,
    causation_id: Option<EventId>,
    correlation_id: CorrelationId,
    node_id: Option<NodeId>,
    repository_revision: RepositoryRevisionId,
    semantic_identity: ContentHash,
    occurred_at: Timestamp, // metadata, never reducer identity
    payload: T,
}
```

Minimum domain event families:

- execution/profile: `ExecutionStarted`, `RepositoryObserved`,
  `RepositoryProfileRecorded`;
- graph: `NodeReady`, `NodeStarted`, `NodeSucceeded`, `NodeFailed`,
  `NodeSuperseded`, `ImplementationBarrierSatisfied`;
- discovery/planning: `SearchCompleted`, `CandidatesRecorded`,
  `FileEvidenceRecorded`, `RelationshipRecorded`, `ImpactMapAccepted`,
  `PlanCandidateRejected`, `PlanAccepted`;
- provider/budget: `ActionEnvelopeCreated`, `ModelCallAdmissionEvaluated`,
  `ModelCallReserved`, `ProviderDispatchStarted`, `ProviderDispatchCompleted`,
  `ModelCallReconciled`, `ContextCompacted`;
- mutation: `TargetContextPrepared`, `TargetContextSuperseded`,
  `FeasibilityEvaluated`, `AttemptPolicySelected`, `ActionPrepared`,
  `ActionReleased`, `ActionRejected`, `CandidateRecorded`, `AttemptFailed`,
  `ApplicationObserved`, `MutationVerified`, `ConvergenceEvaluated`,
  `ReadinessConvergenceEvaluated`;
- validation/repair: `ValidationScheduled`, `ValidationProcessStarted`,
  `ValidationProcessCompleted`, `ValidationEvidenceRecorded`,
  `ValidationFailureRevisionRecorded`, `RepairCandidatesRanked`,
  `RepairEligibilityEvaluated`, `RepairTargetSelected`,
  `RepairTargetContextPrepared`, `PriorValidationInvalidated`,
  `ValidationRerunScheduled`, `ConvergenceEvaluated`;
- review: `DiffManifestRequested`, `DiffManifestBuildFailed`,
  `DiffManifestRecorded`, `ActionPrepared`, `ActionReleased`,
  `ActionRejected`, `DiffPageReviewed`, `DiffReviewRecorded`,
  `CompletionEvaluationRecorded`, `PublicationAuthorityRequested`,
  `PublicationAuthorityObservationFailed`, `PublicationAuthorityObserved`,
  `PublicationEligibilityEvaluated`, `ConvergenceEvaluated`;
- publication: `CommitIntentPersisted`, `CommitObserved`,
  `PushIntentPersisted`, `PushObserved`, `PullRequestIntentPersisted`,
  `PullRequestObserved`, `CompletionRecorded`, `ConvergenceEvaluated`;
- control/terminal: `ConvergenceEvaluated`, `CycleObserved`,
  `CancellationRequested`, `CanonicalResultRecorded`,
  `TerminalCallbackAttempted`, `TerminalCallbackAcknowledged`.

Telemetry is a projection of committed events plus adapter observations. For
every provider request it includes authorized tool names, actual serialized
tool names and hashes, context token estimate, output allowance, budget owner,
and reservation. For every failure it carries `is_first_fatal`, typed category,
code, retryability, owner, causation chain IDs, and safe context. Downstream
effects are linked as consequences and never overwrite the first fatal blocker.

`Display`, `Debug`, events, and source chains accept only redacted safe values.
Secrets, raw authorization headers, provider bodies, repository credentials,
and expanded secret environment values are prohibited schema fields.

## Cycle and convergence model

Semantic state hashes include protocol position, active owner/step, repository
revision, evidence frontier, failure revision, budget frontier, and graph
statuses. Semantic decision hashes include action class, target, normalized
query/scope, gate, repair intent, and action envelope policy.

An observation increments a cycle count only when both hashes repeat and no
progress event occurred. Progress events are typed facts such as new current
evidence, verified repository change, new validation revision, or node success;
heartbeats and repeated telemetry are not progress.

Before a fatal cycle decision, `decide` must try deterministic convergence:

- finalize discovery from sufficient existing evidence;
- accept an already valid plan;
- complete an already verified target;
- reuse current validation evidence;
- schedule a required rerun;
- continue partially completed publication reconciliation.

Cycle thresholds remain fixed and signed. Convergence changes state through
normal events and reducers; it never suppresses cycle detection.
