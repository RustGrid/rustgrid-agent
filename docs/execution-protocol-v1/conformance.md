# Protocol v1 conformance suite

Protocol v1 is not eligible for real missions until this suite passes. The
suite tests the protocol, adapters, and serialized provider boundary separately
so a lifecycle test cannot pass by mocking away the contract under test.

## Test layers

1. **Reducer conformance** uses pure state/event fixtures and asserts exact
   accepted/rejected transitions, state hashes, and replay equality.
2. **Protocol simulation** extends the existing deterministic simulation style
   with scripted repository, provider, process, and publication observations.
   It executes the production Protocol v1 `decide` and `reduce` functions.
3. **Repository fixtures** are tiny checked-in repositories operated through
   the real filesystem/search/mutation/verification adapters. No external
   network or real provider is used.
4. **Serialized provider contract** captures the actual JSON payload and
   asserts exact tools, schemas, tool choice, context manifest, token limits,
   metadata, and reservation identity.
5. **Process adapter contract** runs fixture commands and verifies start,
   completion, timeout, output truncation, parsing, and cancellation events.
6. **Publication contract** first proves intent/observation reconciliation in
   the pure reducer. Its promotion adapter contract then uses local Git remotes
   and a fake GitHub port to verify the same commit/push/PR identities and
   remote-movement handling before live integration.
7. **Backend contract fixtures** serialize every Protocol v1 event/request
   against versioned RustGrid schemas before worker promotion.

Existing `hosted_simulation`, execution-graph reducer tests, provider payload
tests, command tests, local Git tests, and terminal callback tests are seeds.
They remain green during migration; they do not substitute for this suite.

## Fixture format

Each fixture directory contains:

```text
fixture.toml                 mission, profile expectations, signed budgets
repository/                  minimal multi-ecosystem repository
provider-script.json         typed candidate responses, never control flow
process-script.json          expected commands/results when a real command is unsuitable
expected-events.json         exact canonical domain-event sequence
expected-result.json         canonical outcome, health, remaining work
```

`expected-events.json` lists every canonical event type and its stable semantic
fields. Timestamps, transport request IDs, and duration measurements may use
typed matchers because they are not reducer identity. Unexpected, missing, or
out-of-order domain events fail the fixture.

Common event abbreviations below are descriptive only; fixtures use full
versioned names.

## Required fixture matrix

| # | Generic fixture | Required canonical event sequence | Primary assertions |
| ---: | --- | --- | --- |
| 1 | Tiny one-line text change in a static site | `Profile -> Search -> Candidates -> Read -> ImpactMap -> Plan -> Context -> Reserve -> Candidate -> Applied -> Verified -> Barrier -> FocusedPass -> Review -> Eligible -> Commit -> Push -> PR -> Succeeded` | One target, one verified line change, cheapest legal call path, no ecosystem assumption. |
| 2 | Small source edit in a Rust library | Same normal path with focused unit gate then suite/build gates | Profile-derived commands have provenance; all required gates bind to final revision. |
| 3 | Large Python file, small edit | Normal path; mutation envelope contains bounded patch only; verify; validate; publish | Full target is not inlined when unnecessary; context and output remain below ceilings. |
| 4 | Two-file Go change | Plan creates two dependency-ordered targets; each context/candidate/apply/verify sequence occurs once; barrier follows both | No validation before both targets complete; context from target A does not leak into B. |
| 5 | TypeScript source plus test change | Source target then test target, barrier, focused and project gates, PR | Test mutation is plan-authorized and distinct from validation repair. |
| 6 | Malformed patch recovery | Primary patch rejected -> failure revision -> feasible fallback selected -> restricted reservation -> replacement applied/verified -> normal validation/publication | Exact provider fallback payload exposes only the selected tool; attempt identities are distinct and replay-stable. |
| 7 | Large Java target where replacement cannot fit | Patch rejected -> feasibility records replacement too large -> bounded patch retry or explicit `NoSafeFallback` | `replace_file` is absent from serialized payload and is never dispatched. |
| 8 | Small configuration target where replacement fits | Patch rejected -> replacement feasible -> forced `replace_file` -> verified -> gates -> PR | Full-file output allowance includes conservative serialization margin. |
| 9 | Python validation failure with source repair | Initial implementation -> focused failure -> parsed revision -> candidates -> source eligible/selected -> repair verified -> exact focused rerun passes -> broader gates -> PR | Non-zero exit is domain evidence; repair owns a separate node/budget; rerun cannot be skipped. |
| 10 | Stale-test candidate without specification proof | Validation failure -> candidates -> test eligibility rejected -> no repair node for test -> valid source selected or `NoValidRepair` | Ineligible target consumes zero calls and never receives mutation authorization. |
| 11 | Duplicate discovery search | First search records evidence; exact replay is rejected/idempotent; convergence chooses read/finalize or a typed block | Duplicate identity does not create progress; productive distinct queries remain distinct. |
| 12 | Sparse repository with no useful evidence | Profile -> bounded searches/reads -> exact exhaustion -> convergence evaluated -> `InsufficientEvidence` | No vague plan, mutation, validation, or publication occurs. |
| 13 | Discovery exact exhaustion with legal convergence | Final admitted call records grounding; usage becomes exactly max; convergence accepts impact map -> planning | No fourth call; exact exhaustion is valid state rather than mission failure. |
| 14 | Discovery exact exhaustion without sufficient evidence | Usage becomes exactly max -> convergence fails with reason -> `BudgetBlocked` or `InsufficientEvidence` | No call beyond max and no borrowing from planning. |
| 15 | Already-satisfied no-op mission | Profile/discovery evidence proves behavior; validated no-op plan/evaluation -> `SucceededNoOp` | No mutation, commit, push, or PR; no previous failure is silently erased. |
| 16 | Generated output plus generator source | Profile records generated rule; discovery may read output; plan rejects direct output mutation and selects generator/source or blocks | Generated path never appears in an authorized mutation tool unless a registered regeneration operation owns it. |
| 17 | Failing build/typecheck | Verified implementation -> focused pass -> build/typecheck non-zero -> structured failure revision -> eligible repair or current typed no-repair/budget convergence | Failure remains validation-domain truth; it is not process infrastructure failure. |
| 18 | Multiple failing tests | Gate completion -> parser records all bounded failure identities -> deterministic ranking/eligibility -> bounded repair revisions/reruns | Tail evidence survives truncation; one repair cannot mark unrelated failures resolved. |
| 19 | External-review acceptance criterion | Full engineering path and required gates pass -> review marks external evidence pending -> normal/draft publication per policy -> `PartialReviewable` | PR exists, process health is healthy, and external review is the only remaining work. |
| 20 | Publication guard rejection | Current validation is stale or failure revision active -> eligibility rejected -> terminal blocked/validation result | No commit/push/PR side effect; backend and worker name the same failed predicate. |

The repositories deliberately span static text, Rust, Python, Go, TypeScript,
Java, and generic configuration, but protocol expectations are expressed in
capabilities rather than language names.

## Protected golden paths

### Golden A: normal delivery

```text
Profile
-> Discovery(search/read/impact map)
-> Planning(accepted exact targets)
-> Implementation(all verified)
-> ImplementationBarrier
-> FocusedValidationPassed
-> RequiredValidationPassed
-> DiffReviewed
-> CompletionAccepted
-> PublicationEligible
-> CommitCreated
-> BranchPushed
-> PullRequestCreated
-> Succeeded
```

### Golden B: evidence-driven validation repair

```text
normal path through focused validation
-> ValidationEvidenceRecorded(DomainFailed)
-> ValidationFailureRevisionRecorded
-> RepairCandidatesRanked
-> stale-test candidate ineligible
-> current single-path source baseline eligible and selected
-> RepairTargetContextPrepared(ValidationRepair)
-> shared mutation lifecycle
-> MutationVerified
-> MutationVerified proof -> RepairVerified proof -> repair node succeeded
-> PriorValidationInvalidated(all old-revision evidence)
-> ValidationRerunScheduled(exact gate, new revision)
-> focused rerun passed
-> broader validation passed
-> RequiredValidationPassed -> Review                  [Phase 6 checkpoint]
-> exact diff/page review -> completion -> Eligible    [Phase 7 repaired checkpoint]
-> commit intent/observation
-> exact-lease push intent/observation
-> PR intent/observation
-> Succeeded or PartialReviewable                      [Phase 7 pure protocol]
```

The private Phase 6 path proves the sequence through `Review`, including replay
after every repair handoff. Phase 7 now proves that the complete repaired proof
chain is the review ancestry, reaches strict current-diff completion and
granted eligibility, and initializes the same intent-first publication state
used by the completed Golden A suffix. The publication suffix is therefore no
longer an unspecified contract. A single repaired-revision fixture running
that suffix through real Git/GitHub adapters and a live mission remains a
promotion requirement.

### Golden C: large-file feasible fallback

```text
large target context prepared
-> bounded patch candidate malformed
-> replacement feasibility rejected
-> distinct bounded patch retry selected and serialized exclusively
-> mutation verified
-> validation -> review -> commit -> push -> PR -> Succeeded
```

If no distinct bounded patch retry is safe, the corresponding negative fixture
must terminate explicitly; the golden fixture's repository/context is chosen so
one safe retry is feasible.

### Golden D: safe insufficient-evidence stop

```text
Profile
-> bounded Discovery
-> exact node exhaustion
-> ConvergenceEvaluated(no legal completion)
-> InsufficientEvidence
```

No plan, mutation, validation, publication, or generic infrastructure failure
may appear.

## Invariant and property tests

Use table-driven exhaustive tests for small enums and generated event traces
for aggregate properties. The generator creates legal and near-legal traces,
then mutates one precondition to prove rejection. Shrunk failing traces are
stored as permanent fixtures.

Required properties:

1. every provider dispatch has an earlier unreconciled matching admission and
   reservation with the same node/action/payload hash;
2. every terminal or checkpointed state has zero active reservations;
3. `consumed + reserved <= signed maximum` for mission and every node;
4. planning cannot start before discovery proof;
5. validation cannot start before a current implementation-barrier proof;
6. repair-node creation requires a matching prior eligible decision;
7. publication eligibility is impossible with an unresolved current required
   validation failure;
8. serialized tools exactly equal the current action envelope and are a subset
   of the state capability matrix;
9. repository mutations change only the typed operation ownership set;
10. mutation-node success requires verified filesystem evidence;
11. protocol position changes only through reducer events;
12. repair verification makes the exact originating gate rerun the next
    validation work owner;
13. callback/acknowledgement events cannot change the canonical domain result;
14. full-file replacement authorization implies conservative output
    feasibility;
15. exact event replay is a no-op and same identity/different payload is
    rejected;
16. current-revision mutation invalidates all affected older validation proof;
17. at most one active work owner exists;
18. cycle detection requires repeated semantic state and decision with no
    intervening typed progress;
19. deterministic convergence is evaluated before budget/cycle terminal
    decisions;
20. event and diagnostic serialization never contains fixture secrets;
21. an implementation node can fail only from the exact current mutation or
    readiness-convergence failure revision, and its canonical terminal result
    matches that convergence fact exactly;
22. only distinct mutation attempts whose provider call reconciled as consumed
    increment `mutation_attempts`; admitted, reserved, dispatched, or
    definitively released calls do not;
23. a definitively-uncontacted mutation action retry retains the same attempt
    policy but has distinct deterministic action, call, and reservation IDs and
    names the immediately preceding released action;
24. only verified mutation evidence advances the rolling repository revision;
    an application observation cannot, while an exact typed drift supersession
    may separately adopt its observed revision;
25. replay reconstructs the same context history, attempt/action chains,
    consumed-only usage, mutation ledger, repository revision, convergence,
    node failure, and terminal result;
26. every executable validation gate is the exact intersection of an accepted
    plan or required broad policy entry, a repository-profile candidate, and a
    validation-policy authorization carrying signed-policy evidence;
27. a `ValidationRepair` node is impossible until every ranked candidate has a
    persisted eligibility decision, and selection chooses the highest-ranked
    eligible candidate deterministically;
28. repair eligibility requires a valid, current, single-path mutation
    baseline owned either by the target's implementation node or by an exact
    same-target prior `RepairVerified` chain ending at the failure revision;
    the repair operation is a fresh `ModifyExisting` against its verified
    current file hash, and an older different-target baseline is not carried
    forward without a non-interference proof;
29. after repair mutation verification, unrelated progress is rejected until
    mutation proof, repair proof, node success, exact old-revision invalidation,
    rerun schedule, and rerun proof are committed in order;
30. repair context drift cannot rebuild context: exact
    `ContextRebuildUnavailable` convergence adopts the observed revision and
    maps to healthy `NoValidRepair`, while budget and infrastructure classes
    retain their distinct canonical mappings.
31. every changed path has exactly one same-index singleton-coverage page whose
    raw bytes, patch hash, byte length, content address, locator hash, and
    persistence-receipt hash all revalidate against the manifest;
32. every review/completion dispatch has one forced strict record-only schema,
    `parallel_tool_calls = false`, and a payload/admission hash that includes
    the conservative schema/context/raw-byte input estimate;
33. unplanned paths, missing planned changes, operation mismatches, unsafe or
    incomplete findings, and deterministic criterion-evidence gaps are
    blocking and cannot be changed by a model status;
34. a criterion classified by signed `external_review_criteria` can resolve
    only as `ExternalReviewRequired` with the exact kind, never `Satisfied`;
35. effect-derived review convergence equals the complete projection of its
    stored outstanding-request failure and repeats its typed failure ID and
    failure hash; equal safe codes or revisions cannot collapse distinct
    observations;
36. every commit, exact-lease push, and pull-request effect has an earlier exact
    intent; ambiguous delivery reconciles that intent, definitive retries are
    chained beneath signed ceilings, and only a granted current eligibility
    record can initialize publication;
37. every exhausted, permanent, or remote-moved publication convergence binds
    `final_attempt_id` plus the correctly tagged commit/push/pull-request
    `final_observation_id` and `final_observation_hash`, and rejects substitution
    with a different valid observation from that attempt;
38. review and publication convergence map through the canonical typed table,
    while hosted lease loss suppresses writes and cannot masquerade as a review
    failure or terminal proof.

## Adapter-contract assertions

The provider test captures actual request bytes, not an internal tool vector.
For every state/action combination it asserts:

- exact tool names and order;
- exact path enums and operation schema;
- explicit named tool choice when forced and `parallel_tool_calls = false`;
- input and output ceilings from the node contract;
- action, context, repository, and reservation identities;
- action index and prior released action identity, when applicable;
- no secret or unselected evidence content;
- rejection when the serialized tool set differs from the envelope.

The implemented command boundary invokes a real subprocess by exact
executable/argv without a shell. Current focused tests cover sorted fingerprinted
and redacted environment values, rejected secret/process-injection names,
ordered start/completion observations, startless cancellation and lease loss, a
real non-zero exit, a real timeout, combined output allocation, and bounded
head/tail receipts with exact byte counts. They also prove repository-scoped
diagnostics and per-failure expected/actual hashes. The parser registry
implements Cargo, Node, Pytest, Go, and generic adapters without serializing raw
output. Promotion coverage must additionally exercise repository-contained
working-directory rejection and definitely-not-recorded versus indeterminate
journal writes through real contract adapters. Private Phase 7 publication
tests currently drive typed persisted observations through the reducer; the
adapter contract that observes real local Git ref transitions and fake GitHub
calls remains a promotion gate.

## Existing coverage reused during migration

The current suite already has useful seeds for:

- graph transition replay, active-owner rejection, signed reservation bounds,
  current validation evidence, and terminal immutability;
- deterministic simulation benchmarks, over-budget dispatch denial, repair,
  partial publication, and infrastructure/publication separation;
- serialized mutation fallback and bounded-discovery tool filtering;
- path-scoped mutation, verified-write reduction, validation rerun, callback
  authority, and local Git publication.

The private Protocol v1 suite additionally covers the Phase 4 read-only
implementation-context boundary: graph-owned target selection, deterministic
content-load requests, operation-owned path probes, target-local evidence,
content-addressed artifact verification with redacted raw bytes, full-file and
exact-range projections, optional compaction, typed mandatory overflow, stable
manifest identities, strict serialization, event projection/replay, and the
`ImplementationContextReady` boundary.

The private Phase 5 focused suite now covers the protocol-owned mutation slice:

| Contract | Focused proof |
| --- | --- |
| Feasibility and initial policy | Deterministic create/delete/move/modify strategy sets; small modify admits ordered patch/replacement while output- or context-limited replacement is omitted. |
| Exact serialized authority | Strict operation-owned schemas, exact paths/hashes, bounded content, exact tool order and tool choice, no additional properties or parallel calls, and exact action/call/reservation/context/repository/budget bindings. |
| Recovery | Malformed patch selects a forced feasible fallback with a distinct attempt identity; typed candidate regeneration is distinct from strategy fallback; non-retryable failures converge rather than rehydrate tools. |
| Candidate durability | Content addresses, locator hashes, persistence-receipt hashes, content hashes/length/encoding, exact patch expected-after content, tamper rejection, and non-serializable redacted raw bytes. |
| Apply and verify | Exact request/observation chains, operation ownership, create/modify/delete/move path transitions, independent verification, and rejection of mismatched paths, hashes, fingerprints, or candidate result. |
| Revision and drift | Verification-only rolling revision advancement; exact drift-bound context supersession/rebuild; retained prior contexts; no stale-context provider dispatch. |
| Readiness convergence | No feasible strategy; each exhausted admission dimension; bounded repeated definitively-uncontacted release; exact terminal failure revision. |
| Transport release | One same-policy retry with distinct chained action/call/reservation IDs; repeated release converges; only consumed calls count as mutation attempts. |
| Terminal and replay | Convergence-required `NodeFailed`; exact `BlockedNoDiff`/`BudgetBlocked`/`InfrastructureFailed` mapping; global progress freeze until the exact terminal result; exact verification-proof authority with positive `AlreadySatisfied` unavailable; event replay, usage reconstruction, ID-chain validation, and same-ID tamper rejection. |

These tests exercise the private reducer, serialized request contract, and
typed materialized observations. They do not claim a production provider call,
a real durable artifact-store write/read, or a real filesystem adapter. Hosted,
backend, CLI, and existing provider routing remain unchanged.

The private Phase 6 suite adds the following focused proof:

| Contract | Focused proof |
| --- | --- |
| Gate authority | Plan/profile/policy intersection, canonical focused-to-broad ordering, stable serialized request bindings, current revision, and tamper rejection. |
| Process boundary | Direct argv execution, environment fingerprinting/redaction, live authority, ordered observations, startless cancellation/lease loss, real non-zero exit, timeout, and bounded large-output head/tail receipts. |
| Result domains | Exit zero plus observed semantics is pass; other exits are domain failure evidence; infrastructure results cannot construct validation evidence or masquerade as domain failure. |
| Repair selection | Structured diagnostic/relationship ranking, decision for every candidate, unproven stale-test rejection, explicit specification authorization, and deterministic highest eligible selection. |
| Baseline authority | Missing, stale, absent-after, and multi-path baselines are rejected; an R2 failure can use the exact R1-to-R2 same-target `RepairVerified` mutation as its next baseline and reach R3/review/replay, while older different-target evidence is not carried forward without a non-interference proof. |
| Repair ownership | Separate purpose-bound context and budget, prebinding event/model/mutation rejection, shared mutation chain, and exact mutation/repair proof authority. |
| Handoff and rerun | Frozen proof/node/invalidation order, invalidation of the exact old-revision evidence set, exact originating-gate rerun at the new revision, same-owner gates before global canonical resumption, required-validation proof, and transition to review. |
| Terminal and replay | Every required gate's run ceiling is preflighted before repair; exact no-repair, gate-run-budget, validation infrastructure, repair semantic/budget/infrastructure mappings; strict serialized state; replay equality across the integrated Golden B checkpoint. |

This coverage includes a real local validation subprocess adapter but still does
not claim hosted/backend/CLI wiring, a production validation artifact sink, a
production mutation provider, a durable mutation-artifact store, a real
mutation filesystem adapter, Git/GitHub publication, or a live mission.
Phase 7 now consumes both clean and repaired validation ancestry. Durable Phase
4 loader-failure convergence, positive no-op authority, and end-to-end real-
adapter Golden C remain open.

The private Phase 7 suite adds the following focused proof:

| Contract | Focused proof |
| --- | --- |
| Clean and repaired ancestry | Golden A binds the current barrier/validation chain; repaired Golden B preserves the ordered mutation/repair/invalidation/rerun chain and rejects a well-formed stale ancestry. |
| Complete diff authority | Signed base ref/base revision, current revision/fingerprint, accepted-plan ownership, one page per path, exact singleton index coverage, raw bytes/path patch hash and length equality, content address and receipt hashes, tamper rejection, and redacted non-serializable bytes. |
| Record-only provider boundary | Exact forced strict review/completion schemas, no additional properties or parallel calls, payload identity, conservative schema/context/raw-byte input estimate, consumed-only usage, and bounded definitely-uncontacted release. |
| Completion and external review | Deterministic criterion/target/path/validation evidence cannot be model-overridden; gaps block; signed external criteria require their exact `ExternalReviewRequired` kind and cannot be marked `Satisfied`. |
| Effect failure ancestry | Diff limit/drift/artifact and unavailable-authority records bind the exact outstanding request/effect/revision; projected convergence repeats failure ID/hash and exact state equality is required. |
| Eligibility and terminal mapping | Twelve typed predicates, current authority, exact blocked/budget/validation/infrastructure mappings, and rejection of forged convergence before canonical terminal persistence. |
| Publication reconciliation | Granted eligibility only; commit/push/PR intent before effect, exact created/already-satisfied observations, open-intent reconciliation, chained ceilings, convergence bound to `final_attempt_id` plus typed `final_observation_id`/`final_observation_hash` for exhausted/permanent/remote-moved outcomes, normal `Succeeded`, external-review `PartialReviewable`, and replay. |

These are pure protocol and typed-observation tests. They do not resolve an
artifact from a real store, call hosted Git/GitHub or the control plane, own
lease/cancellation or terminal CAS, deliver a callback/outbox record, route a
production provider/event, publish a live branch/PR, or prove a positive no-op.

Important gaps that Protocol v1 must close rather than relabel as covered:

- checked-in synthetic repository fixtures;
- one full repaired Golden B suffix through the real Git/GitHub adapter contract;
- end-to-end Golden C;
- real durable mutation-artifact store/provider/filesystem adapter contracts;
- real diff-artifact address resolution and receipt verification;
- hosted control-plane lease/cancellation and terminal-write suppression;
- callback/outbox delivery and backend/production routing;
- live publication and positive no-op authority;
- generated-file mutation admission;
- failing build/typecheck lifecycle;
- cross-target multiple-failure baseline carry-forward with non-interference proof;
- exhaustive state-by-tool authorization;
- generated trace/property coverage rather than isolated examples.

## Promotion gates

A phase may merge only when its new reducer tests and all previously enabled
Protocol v1 fixtures pass. Protocol v1 may enter shadow mode only when:

- all reducer properties pass under deterministic seeds and randomized CI;
- all 20 fixtures match their exact event sequences;
- Golden A-D pass repeatedly with identical semantic event hashes;
- current legacy golden/replay/API/CLI tests remain green;
- provider, backend event, process, Git, GitHub, and persistence contracts pass;
- format, warnings-denied Clippy, full tests, packaged-source compilation, and
  secret scanning pass.

Canary promotion additionally requires zero lifecycle invariant failures,
zero forbidden-tool payloads, zero stale reservations, and zero backend schema
rejections across the agreed sample. Mission-level model quality failures may
occur, but they must terminate through a specified safe outcome.
