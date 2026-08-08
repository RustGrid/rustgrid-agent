# Changelog

All notable changes to rustgrid-agent are documented here. The project follows
Semantic Versioning.

## Unreleased

## 1.4.42 - 2026-08-08

### Fixed

- Express legacy already-applied implementation satisfaction as one minimal
  boolean predicate so the release candidate passes the pinned Rust 1.94
  strict Clippy gate without changing lifecycle behavior.

## 1.4.41 - 2026-08-08

### Changed

- Separate repository-operation verification, implementation-barrier proof,
  validation-gate results, review, completion, and publication evidence into
  explicit lifecycle proof classes.
- Evaluate lifecycle invariants only in the phases where their required
  evidence can legally exist, with startup validation for invariant
  dependencies and a deterministic next-phase resolver.
- Persist one resumability decision containing the reason, next unresolved
  node, and repository fingerprint across terminal projections and callbacks.
- Emit structured invariant, future-evidence suppression, implementation
  barrier, and next-implementation-node telemetry.

### Fixed

- Stop requiring automated validation immediately after an individual
  repository operation is verified while other implementation targets remain.
- Advance multi-target, mutation-fallback, and already-applied executions to
  the next unresolved implementation node without replaying completed work.
- Require current fingerprint-bound validation only for review, completion,
  and publication decisions, and stale prior validation after later repairs.
- Preserve the exact orchestration-state invariant category, specific failure
  code, actual phase, and resumability in runtime and terminal reporting.

## 1.4.40 - 2026-08-08

### Changed

- Reduce every verified repository operation through one authoritative,
  atomic graph transition before selecting another orchestration decision.
- Persist mutation lifecycle from proposal and validation through unverified
  application and deterministic verification, with replay-safe defaults for
  older checkpoints.
- Resolve mutation fallback and validation repair through independent,
  owner-scoped budgets and retain separate planning, review, and artifact
  repair namespaces.
- Emit contract-validated repository reduction, implementation barrier,
  repair-session, cycle reconciliation, and invariant telemetry.

### Fixed

- Complete active nodes and final attempts after verified modifications,
  creations, deletions, moves, renames, and already-applied operations instead
  of leaving successful work Ready, Running, or Superseded.
- Prevent cycle recovery and partial-review dependency overrides from starting
  validation while required source or test mutations remain incomplete.
- Keep recovered failure status separate from execution-node status and
  preserve completed implementation nodes during validation repair.
- Classify repair-accounting and successful-mutation convergence defects with
  typed, resumable orchestration failures instead of initialization or generic
  infrastructure failures.

## 1.4.39 - 2026-08-08

### Changed

- Represent already-satisfied repository operations as typed, immutable
  evidence and converge them through the same execution graph as applied
  mutations.
- Derive orchestration decisions, externally reported phases, retry identity,
  and terminal selection from canonical graph, repository, validation, and
  publication state.
- Track lease renewal independently from semantic progress and bound repeated
  deterministic orchestration cycles with typed cancellation ownership.

### Fixed

- Advance downstream work after an operation is already applied instead of
  repeatedly probing or dispatching the same mutation decision.
- Reconcile stale active-node pointers without changing graph revision and
  preserve useful partial diffs when later validation or infrastructure work
  cannot complete.
- Keep semantic decision identity stable across graph-revision-only changes,
  deduplicate repeated decisions, and stop no-progress cycles after two
  identical observations.
- Preserve validation-repair intent, failure identity, and current repository
  evidence when an already-applied repair proceeds directly to validation.
- Update the pinned Node runtime image to 24.18.1, which contains upstream
  fixes for the newly disclosed high-severity Node vulnerabilities.

## 1.4.38 - 2026-08-05

### Changed

- Derive validation-repair capacity from structured failure assertions,
  implicated targets, gate criticality, target size, and remaining signed
  mission capacity.
- Persist bounded repair-call, repository-write, context-rebuild, reallocation,
  and attempt-identity evidence so recovery and telemetry remain deterministic.
- Represent phase-persistence degradation as a typed, durable condition while
  preserving the canonical worker-domain outcome.

### Fixed

- Prevent repair retries or reallocations from exceeding target, call, cost,
  repository-write, context-rebuild, or mission limits.
- Preserve successful and partial-reviewable terminal outcomes when a late
  phase-transition write fails, reporting degraded health instead of a generic
  infrastructure or initialization failure.
- Bind validation-repair attempts to exact semantic model calls, revisions,
  repository fingerprints, assertions, targets, and validation reruns.
- Require versioned phase-transition payloads and the canonical
  `review_incomplete_diff` decision for repair-to-review transitions.

## 1.4.37 - 2026-08-05

### Changed

- Persist one canonical worker-domain terminal result before callback delivery,
  including typed completion, remaining-work, publication, health, and finality
  evidence with a deterministic result identity.
- Deliver terminal acknowledgements through a durable outbox with deterministic
  idempotency, bounded retry, exact-envelope replay, and restart recovery that
  never reruns an already-finalized mission.
- Separate callback transport health from the mission outcome and emit stable
  lifecycle telemetry for acknowledgement, retry, missing-callback, and
  projection-repair states.

### Fixed

- Prevent callback timeouts, workflow conclusions, runner disappearance, and
  stale reconciliation from replacing a finalized healthy or reviewable result.
- Preserve externally pending review as a healthy terminal outcome and use typed
  evidence precedence when legacy completion fields disagree.
- Validate callback identity, terminal revision, authority, compatibility status,
  and process exit evidence before accepting an acknowledgement.
- Recover the exact persisted callback after worker restart while suppressing
  stale terminal writes and duplicate mission execution.

## 1.4.36 - 2026-08-04

### Changed

- Separate implementation intent from validation-repair intent and require
  assertion-bound evidence before a repair can satisfy a failing validation.
- Persist typed repair attempts, no-change results, target progression,
  independent target state, and terminal repair decisions for deterministic
  recovery.
- Emit structured repair lifecycle telemetry and include unresolved repair
  evidence in draft partial-result summaries.

### Fixed

- Keep failed validation authoritative until a passing rerun instead of
  treating an earlier applied implementation as proof of a later repair.
- Prevent identical or no-change repair output from counting as a successful
  repository write or prematurely ending repair target progression.
- Route exhausted repair with a useful diff through incomplete review and
  healthy draft partial publication rather than initialization failure.
- Preserve the typed hosted lifecycle contract when repair orchestration falls
  back after an unresolved validation failure.

## 1.4.35 - 2026-08-04

### Changed

- Enforce mutation fallback strategies as executable, typed tool policies with
  an exact provider tool surface and tool choice for each target lifecycle.
- Select bounded patching or whole-file replacement from generic repository
  evidence, target size, operation kind, and prior typed application failures.
- Persist replay-safe mutation allowances, repair attempts, rejected responses,
  and verification evidence so recovery preserves the same policy and budgets.
- Explicitly pin patched transitive packages used by the container's npm
  runtime.

### Fixed

- Reject provider responses that violate the selected mutation policy before
  they can affect the repository, including mixed, missing, or disallowed tool
  calls and fallback escalation without recorded typed evidence.
- Keep mutation preparation, application, repair accounting, target
  verification, and audit events consistent across modifications, creations,
  deletions, renames, and moves without ticket-specific handling.
- Preserve healthy blocked, partial-reviewable, cancellation, and terminal
  outcomes when mutation work cannot safely continue.
- Resolve fixable high-severity vulnerabilities in the container's bundled
  `brace-expansion` and `ip-address` packages.

## 1.4.34 - 2026-08-04

### Changed

- Resolve every hosted terminal state through one canonical record containing
  mission outcome, process health, domain status, publication evidence,
  completion evidence, resumability, finality, and a deterministic result ID.
- Separate domain outcomes from worker health so healthy partial, externally
  pending, resumable blocked, and expected cancellation results exit normally.
- Reconcile workflow conclusions as infrastructure metadata and anomalies;
  finalized worker-domain results remain authoritative unless an explicit
  administrative override is applied.
- Project terminal API, UI, telemetry, and compatibility fields from the same
  canonical result and include its ID, revision, and authority in completion
  callbacks for optimistic backend enforcement.

### Fixed

- Prevent a useful published draft or externally pending result from becoming
  a generic process failure merely because work remains or a later workflow
  conclusion reports failure.
- Normalize ambiguous or contradictory completion evaluations using stronger
  deterministic graph, validation, remaining-evidence, and publication facts.
- Preserve branch, commit, pull-request, draft, and publication timestamps
  through infrastructure reconciliation and post-publication failures.
- Persist the canonical terminal result before resolving the process exit and
  keep noncritical terminal-callback failures from reversing healthy domain
  outcomes.

## 1.4.33 - 2026-08-03

### Changed

- Model every planned repository mutation with an explicit typed operation for
  modification, creation, deletion, rename, or move, and reject missing or
  ambiguous operation contracts during planning.
- Prepare target context from a deterministic operation-aware state probe so
  absent create targets and already-applied deletions or relocations are valid
  states rather than generic inspection failures.
- Bind provider mutation tools to the accepted operation and persist
  fingerprint-bound creation intent and created-target evidence for recovery.
- Emit structured target-probe, creation, conflict, and verification telemetry
  with stable operation-aware fields.

### Fixed

- Allow new files to proceed from an absent path through atomic creation,
  deterministic verification, validation, review, and publication without
  attempting to read the nonexistent target as an existing file.
- Prevent create, rename, and move operations from overwriting destinations
  that appear concurrently, including an OS-level no-clobber rename boundary.
- Reconcile matching create and relocation results as `AlreadyApplied` only
  when persisted intent evidence proves the expected content.
- Preserve previously applied graph nodes and reviewable repository changes
  when a later target conflicts, routing the incomplete diff to draft review
  instead of reporting orchestration initialization failure.

## 1.4.32 - 2026-08-03

### Changed

- Parse ANSI-decorated structured validation failures into typed assertion
  evidence containing the test file, suite path, test name, source location,
  assertion kind, expected and received values, implicated paths, and bounded
  diagnostic context.
- Rank validation-repair targets from generic repository evidence: assertion
  specificity, direct test imports, source-versus-test role, value occurrence,
  and normalized semantic overlap. Selection does not depend on ticket names,
  component roles, framework concepts, or specific state values.
- Build repair requests only after refreshing bounded current-fingerprint
  evidence for the selected target, failing test, and implicated changed files.
- Expose typed validation-repair actions and structured parsing, evidence,
  ranking, context-validation, transition-comparison, and incomplete-review
  telemetry.

### Fixed

- Prevent recognized validation failures from silently producing empty failing
  test and implicated-path collections; incomplete primary parsing now emits a
  bounded diagnostic event and uses a conservative structured fallback.
- Prevent validation repair from invoking a model without the selected target's
  current content, content hash, repository fingerprint, diff context, failure
  evidence, and accepted implementation intent.
- Preserve applied mutation status when a focused gate fails, and allow an
  unresolved or no-mutation validation repair to enter the explicit incomplete
  diff-review path without an illegal lifecycle transition.
- Keep incomplete validation review draft-only and partial-reviewable while
  preserving failed and pending gate evidence for recovery and publication.

## 1.4.31 - 2026-08-03

### Changed

- Build compact, deterministic completion-evidence packets from accepted
  changes, verified graph targets, changed paths, relevant validation gates,
  diff-review findings, unresolved failures, and external-review requirements.
- Use a bounded 3,072-token, low-reasoning completion profile and skip the
  provider call when deterministic repository evidence already proves the
  terminal result.
- Publish one canonical terminal result containing the domain outcome, process
  health, reason code, publication details, completion evaluation, and
  remaining work; compatibility events derive from that result.
- Expose discovery, planning, initial mutation, target repair, validation
  diagnosis, validation repair, diff-review, and completion-call accounting in
  terminal telemetry.

### Fixed

- Preserve healthy complete, complete-pending-review, partial-reviewable, and
  blocked outcomes as successful worker exits instead of allowing a later
  compatibility result to report generic uncertainty or process failure.
- Treat selection, cycling, persistence, restoration, fallback, regression,
  and build behavior as automated verification while leaving genuine product,
  design, accessibility, visual, and deployment decisions for external review.
- Prevent optional model interpretation from erasing or downgrading
  deterministic criterion-to-diff evidence.
- Reject completion nodes whose configured cost budget cannot fund one compact
  completion action, while keeping rejected pre-provider admission visible
  without consuming a provider call.
- Keep pull-request draft state independent from process success so completed
  implementation awaiting product or design review remains healthy and draft.

## 1.4.30 - 2026-08-03

### Changed

- Add an explicit incomplete-diff review path that preserves applied work,
  evaluates partial completion, and authorizes only draft publication when
  required validation remains failed or pending.
- Persist structured validation assertion evidence, bounded implicated repair
  targets, typed source-versus-test diagnoses, and typed no-mutation repair
  results for deterministic reconciliation and recovery.
- Separate validation-repair budgets and model-call accounting from completed
  mutation nodes, and emit lifecycle telemetry for parsing, diagnosis, target
  selection, incomplete review, partial evaluation, and draft publication.

### Fixed

- Keep successfully applied source and test mutation nodes applied after a
  focused code-validation failure instead of returning completed work to the
  mutation queue.
- Route a no-mutation validation repair with a non-empty safe diff through
  deterministic review and a partial-reviewable draft rather than reporting an
  orchestration initialization failure.
- Preserve failed and pending required gates, external-review requirements, and
  typed draft-only dependency overrides in recovery and pull-request output.
- Enforce `replace_file` as the only available and accepted mutation tool after
  a replacement fallback is selected, and resume partial runs from validation
  repair without repeating discovery, planning, or applied mutations.

## 1.4.29 - 2026-08-03

### Changed

- Split the hosted runtime and execution graph into bounded modules with
  explicit contracts for orchestration, lifecycle, provider interaction,
  recovery, publication, telemetry, transitions, validation, and persistence.
- Introduce typed sub-effect ports for repository, journal, control-plane, and
  publication operations so orchestration depends on narrow capabilities and
  tests can exercise failure behavior deterministically.
- Replace unstructured cross-module failures with bounded subsystem errors and
  an exhaustive top-level execution-failure taxonomy that carries structured
  retryability, terminal-outcome, and telemetry decisions.
- Add a secret-scanned repository packaging helper for producing a reviewable
  tracked-source archive without local credentials or untracked files.

### Fixed

- Preserve error sources and human-readable context while ensuring sensitive
  authentication values cannot appear in formatted errors or source chains.
- Map cancellation and lease loss explicitly so cancellation cannot become an
  infrastructure failure and a stale lease cannot authorize terminal writes.
- Replace string-based failure classification with compiler-checked mappings
  for terminal outcomes and telemetry codes, including retryable control-plane
  and provider-budget failures.
- Make the incremental stdout/stderr activity regression test independent of a
  machine-specific inactivity deadline while retaining its activity assertions.

## 1.4.28 - 2026-08-02

### Changed

- Split mutation execution into explicit context preparation, target mutation,
  deterministic target verification, and typed repair actions backed by
  append-only graph events and a strongly typed repository fingerprint.
- Build each mutation request from current-fingerprint target content,
  accepted intent and criteria, relevant impact-map areas, related-test
  excerpts, and preservation constraints instead of rediscovering the
  repository.
- Restrict target mutation calls to one exact path and the `apply_patch` and
  `replace_file` tools, with a 4,096-token medium-reasoning profile and no
  parallel tool calls.

### Fixed

- Reuse current target evidence without another model or repository-tool call,
  and emit cache-hit telemetry with the evidence identity, content hash, and
  repository fingerprint.
- Verify successful writes deterministically from target hashes, repository
  fingerprints, and the exact changed path before marking a mutation node
  applied; treat already-applied targets as successful no-ops.
- Reject free-form or no-change mutation responses as `MutationNotProduced`,
  allow at most one repair call, and prevent a localized target from spending
  repeated calls on repository exploration.
- Keep one graph attempt active across context preparation, mutation, and
  verification while preventing later targets from becoming eligible before
  the active target terminates.
- Report action-aware no-progress diagnostics including the active target,
  calls, read paths, cache-eligible duplicates, and mutation tools offered and
  invoked.
- Observe a repository command's completed process state before applying its
  inactivity deadline so a short-lived command cannot be misclassified as idle
  when a slower runner crosses the polling boundary.

## 1.4.27 - 2026-08-02

### Changed

- Give each validation gate a gate-specific execution, inactivity, startup,
  scheduling, and retry policy instead of reusing the graph node deadline as
  the process timeout.
- Build the validation graph deterministically from accepted targets, placing
  dependency bootstrap and focused changed-test gates before broad lint,
  typecheck, suite, build, and browser validation.
- Persist dependency-bootstrap and validation-process evidence, including
  output activity, elapsed time, configured limits, retry count, and precise
  pending, running, code-failure, infrastructure-failure, and timeout states.

### Fixed

- Keep stdout and stderr activity alive independently, enforce a hard absolute
  timeout, and allow one model-free retry for transient validation
  infrastructure failures when the remaining mission budget can fund it.
- Publish useful applied changes as a clearly incomplete draft when validation
  cannot finish because of worker infrastructure, without claiming a test
  assertion failure or discarding authoritative partial-reviewable state.
- Resume timed-out partial executions from validation while preserving applied
  mutation nodes and preventing duplicate source changes.
- Split target inspection from mutation so stale or missing evidence exposes
  only repository read/search tools before a verified mutation action.

## 1.4.26 - 2026-08-02

### Changed

- Split planning into explicit plan construction, plan repair, and concrete
  evidence-gap actions selected from the accepted impact map and persisted
  repository evidence.
- Give plan construction and repair bounded 4,096-token profiles that expose
  and force only `record_implementation_plan`; repository reads are available
  only to an explicit evidence-gap action.
- Build planning context from current-fingerprint discovery excerpts, related
  tests, architecture findings, acceptance criteria, and validation commands
  derived from `package.json`.

### Fixed

- Fund one plan-construction call plus one compact plan-repair call within the
  provisional planning node, and make the repair attempt reachable through a
  typed recoverable planning failure.
- Reuse unchanged single and batch file evidence without rereading repository
  content or allowing duplicate reads to become the planning node's last
  successful action.
- Construct a conservative normally validated implementation plan from the
  accepted impact map when the bounded model calls cannot persist a valid
  plan, then continue through authoritative complexity classification and the
  first ready mutation node.
- Record successful impact-map normalization as non-blocking metadata with no
  artifact failure layer.

## 1.4.25 - 2026-08-02

### Changed

- Split discovery into explicit repository inspection, impact-map finalization,
  and impact-map repair actions selected from persisted evidence and artifact
  state.
- Give each discovery action a bounded provider profile: inspection exposes
  only repository-reading tools, while finalization and repair expose and force
  only `record_impact_map` with a compact output allowance.
- Estimate admission cost from the exact action request and emit the complete
  consumed, reserved, estimated, projected, and limiting cost calculation.

### Fixed

- Admit the third discovery call when the compact finalization request fits the
  remaining node budget instead of reserving a generic high-output request.
- Build a validated conservative impact map from persisted files, searches,
  related tests, acceptance criteria, and architecture findings when even the
  compact finalization request cannot fit.
- Treat localized-discovery policy redirection as an action transition rather
  than a failed repository operation, and prevent duplicate orchestration
  decision application without an intervening graph event or action result.
- Reject incoherent node configurations whose cost budget cannot reasonably
  fund their configured model-call allowance.

## 1.4.24 - 2026-08-02

### Changed

- Track reserved and consumed model calls separately and provision bounded
  discovery and planning bootstrap allowances before authoritative planning.
- Preserve structured healthy blocked outcomes while recording later workflow
  conclusions as infrastructure metadata.

### Fixed

- Admit a call when consumed and reserved usage plus the request exactly equals
  the node call limit, with atomic reservation and reconciliation.
- Keep discovery active until an impact map, deterministic fallback, concrete
  repository blocker, or genuinely exhausted budget is recorded.

## 1.4.23 - 2026-08-01

### Changed

- Separate unfinished orchestration nodes, remaining mutation targets, applied
  mutation targets, and completed validation nodes into strongly typed graph
  projections.
- Use a bounded provisional discovery/planning budget, then classify mission
  complexity authoritatively from the accepted plan and rebuild downstream
  node budgets without resetting consumed calls, cost, or duration.

### Fixed

- Stop treating discovery and planning notebook entries as repository mutation
  targets during lifecycle invariant validation.
- Restrict the applied-target exclusion invariant to source and test mutation
  nodes in applied or completed state, with node-specific invariant diagnostics.
- Keep graph-creation events and compatibility `last_successful_action` values
  from fabricating applied repository targets.
- Normalize legacy discovery/planning-only checkpoints to provisional
  complexity and dispatch the first discovery model request only after the
  fresh graph checkpoint is persisted.

## 1.4.22 - 2026-08-01

### Changed

- Resolve every hosted startup into an explicit fresh-run, graph-resume, or
  recovery-publication mode from compatible persisted state and the checked-out
  repository diff.
- Emit startup and recovery-publication decisions with notebook, graph, branch,
  and selected-next-action evidence for operational diagnosis.

### Fixed

- Start discovery normally on a clean fresh checkout instead of treating the
  manifest branch name alone as evidence of an interrupted publication.
- Resume the next graph node when a compatible execution graph is persisted,
  while reserving recovery publication for an explicit recoverable failure or
  interrupted publication with repository changes.
- Treat recovery publication with no remaining diff as a successful no-op and
  preserve startup and graph-initialization errors under their real failure
  categories instead of misreporting an AI gateway failure before any provider
  request was made.

## 1.4.21 - 2026-08-01

### Changed

- Make a serializable execution graph and its domain-event reducer the
  authoritative lifecycle state for hosted discovery, planning, mutation,
  validation, diff review, completion, and publication.
- Classify missions as tiny, small, medium, or large with deterministic
  per-node and mission-wide model-call, repair, cost, and duration budgets.
- Project graph state back into legacy worker notebooks for compatible resume,
  telemetry, and API behavior without allowing legacy fields to advance work.

### Fixed

- Reconcile hosted decisions through a pure deterministic orchestrator so
  validation, completion, publication, cancellation, and failure routes cannot
  bypass graph dependencies or reuse stale repository evidence.
- Preserve partial work and publish only authorized, validated draft-recovery
  pull requests, then terminate them as successful partial outcomes without
  re-entering the normal diff-review route.
- Bind validation evidence to repository, dependency-lock, command, and
  environment fingerprints and invalidate finalization after remote branch
  reconciliation.
- Add deterministic replay coverage for attempts 17 through 20 and ten mission
  benchmarks, including collective acceptance coverage and multi-target
  duplicate-write recovery.

## 1.4.20 - 2026-08-01

### Changed

- Centralize hosted lifecycle advancement around authoritative target status,
  unresolved mutation failures, changed paths, and required validation gates.
- Preserve deterministic target order after each successful mutation and
  continue through every remaining planned target while call budget remains.

### Fixed

- Reconcile failed mutations as superseded when the target is already applied,
  a later successful mutation satisfies it, or the final diff contains it.
- Return `target_already_applied` for duplicate mutations without entering
  repair or recording a new unresolved failure.
- Add the legal recovery routes from repair to implementation or validation,
  and require validation followed by diff review before completion evaluation.
- Publish passing, useful partial implementations as draft pull requests when
  budget expires with explicit remaining targets.

## 1.4.19 - 2026-08-01

### Changed

- Represent implementation-plan acceptance-criterion references canonically as
  stable `ac-N` IDs while retaining legacy text input for notebook migration.
- Evaluate required criterion coverage across the complete plan and require
  each independently editable change to reference only its relevant criteria.

### Fixed

- Accept plans whose changes collectively cover every required criterion
  without requiring every change to duplicate every criterion.
- Deterministically attach missing criterion coverage to the uniquely most
  relevant existing change using impact-map paths and semantic evidence, then
  revalidate locally without consuming another provider call.
- Replace the original plan candidate with its repaired form before validation,
  persistence, telemetry, and transition into implementation.

## 1.4.18 - 2026-08-01

### Changed

- Track hosted implementation preparation, mutation, repair, and validation
  readiness as persisted substates with productive, neutral, recoverable,
  blocking, and duplicate tool outcomes.
- Drive multi-file work target by target with compact authoritative context,
  bounded preparation, one guided first-write recovery turn, and a shared
  implementation-and-repair budget.
- Cap the five-target hosted fixture at 20 model calls, 10 implementation and
  repair calls, ten minutes, and an estimated EUR 2 while preserving review and
  completion capacity.

### Fixed

- Preserve successful batch-read results, prevalidate every requested path,
  retry failed paths individually, and keep recoverable read failures from
  consuming healthy implementation progress.
- Forbid repository validation when every required target remains planned and
  the source tree is unchanged; return a structured resumable blocked result
  without running tests or replacing terminal telemetry.
- Fingerprint repository identity, HEAD, tracked edits, untracked contents, and
  dependency locks so one-byte changes invalidate tree-bound validation while
  identical trees reuse required-gate evidence exactly once.
- Preserve valid impact-map evidence and planning fragments, prevent
  informational progress reports from faking writes, and keep mutation
  authorization scoped to the current planned target.
- Reconcile committed and dirty target changes together on resumed branches,
  retain useful partial work, and admit matching restored implementation state
  to validation without losing previously applied targets.
- Bound AI requests and transport or registration retries by the execution
  deadline, conservatively account missing provider usage, and preserve
  accurate model-call, cost, token, and tool telemetry through publication.

## 1.4.17 - 2026-08-01

### Changed

- Drive hosted execution through a canonical, validated lifecycle from
  implementation to worker-owned validation, deterministic diff review,
  completion evaluation, and publication.
- Derive remaining work from authoritative per-target state and persist
  source-tree-bound validation evidence, required-gate status, and cancellation
  provenance in the resumable worker notebook.
- Size model-call budgets by ticket complexity, with separate implementation
  and repair ceilings, four-call zero-progress detection, and phase-specific
  compact model context.

### Fixed

- Transition immediately out of implementation when every planned target is
  applied, without waiting for `repository_snapshot`, a model declaration, or
  budget exhaustion.
- Deduplicate focused tests, test suites, builds, and other required gates for
  an unchanged source tree and dependency lock; supersede their evidence only
  after a relevant mutation.
- Bound final validation, focused commands, repair rounds, estimated model
  cost, and wall-clock duration so a healthy implementation cannot enter a
  costly validation loop.
- Preserve useful work, validation evidence, the branch, and an exact resume
  phase on cancellation, partial completion, or a guardrail stop, while keeping
  handled domain outcomes successful at the GitHub Actions process layer.
- Prevent illegal lifecycle transitions and require current required-gate
  evidence before deterministic diff review.

## 1.4.16 - 2026-07-31

### Changed

- Represent hosted implementation-plan targets as structured per-file records,
  with optional logical parent IDs and independently persisted target status.
- Normalize legacy semicolon-separated plan and notebook targets before
  implementation, and expose deterministic `repair_implementation_plan`
  evidence without consuming a model call.
- Emit five-call implementation progress windows with successful-write,
  changed-path, and repeated-failure accounting.
- Include planned-versus-changed path evidence and explicit completed,
  incomplete, root-cause, and resume sections in partial pull requests.

### Fixed

- Authorize concrete mutations by membership in the planned target set instead
  of comparing against a compound serialized target.
- Classify authorization, plan metadata, repository-policy, patch, and content
  failures separately; do not count preflight rejection as an executed write.
- Halt non-retryable mutation strategies immediately and cap genuine content
  repair at four failed writes per target.
- Prevent passing tests from satisfying criteria whose required planned paths
  are unchanged, while preserving successful resumable partial publication.
- Preserve legacy implementation notebooks, including applied or verified
  deletion targets, without repeating discovery or planning.

## 1.4.15 - 2026-07-31

### Changed

- Version and validate the `rustgrid.impact_map.v2` contract across the worker,
  RustGrid ingestion, generated projections, and AgentOps rendering.
- Preserve normalized impact-map evidence and compact deterministic repair
  state across hosted continuation attempts.

### Fixed

- Continue directly to planning when deterministic impact-map recovery is
  sufficient, while rejecting malformed or schema-drifted artifacts before
  persistence.

## 1.4.14 - 2026-07-31

### Changed

- Track planned source changes with stable change IDs and keep ordered write
  attempts as diagnostic history separate from intended-change state.
- Reconcile failed edits against later target writes, the final repository
  diff, implementation declarations, and authoritative validation before
  completion evaluation.
- Add bounded range, unique-symbol, unified-diff, and small-file rewrite tools
  that report before/after hashes, changed ranges, and concise diff summaries.
- Include passed test and build commands in satisfied code-criterion evidence.

### Fixed

- Prevent ambiguous or absent intermediate replacements from making completed,
  changed, and validated work appear incomplete.
- Prevent earlier successful writes and successful no-op writes from
  incorrectly superseding later failures.
- Stop repeated ambiguous replacement loops after one bounded retry and direct
  repair toward deterministic editing strategies.
- Treat published complete, external-review-pending, partial, and blocked
  mission outcomes as healthy GitHub Actions process results while preserving
  nonzero exits for invalid terminal results and publication failures.
- Keep review-pending pull requests normally titled and reserve draft
  `[INCOMPLETE]` presentation for work that needs implementation continuation.

## 1.4.13 - 2026-07-30

### Changed

- Separate mission outcome, implementation completeness, verification
  readiness, evaluator source, and worker process health in hosted completion
  results.
- Classify manual, accessibility, visual, product, and deployment checks as
  pending external review instead of unfinished source implementation.
- Apply project verification policy when deciding whether missing browser E2E
  coverage blocks a theme implementation.
- Reserve more final review and completion-evaluation capacity for 60-call
  missions, retry invalid evaluator results, and reallocate unused
  implementation capacity to finalization.
- Emit cache-observability telemetry that distinguishes cold starts, stable
  prefix changes, tool-order changes, and provider-reported zero cache reads.

### Fixed

- Prevent fully implemented work awaiting human review from being reported as
  an incomplete implementation that must be continued.
- Keep partial-result and resumable semantics limited to missions with actual
  remaining implementation or automated-verification work.
- Reconcile evaluator output with authoritative declarations, changed paths,
  tool failures, acceptance criteria, and validation evidence before reporting
  completion.

## 1.4.12 - 2026-07-30

### Changed

- Allocate a signed 60-call hosted mission as 8 discovery calls, 4 planning
  calls, 45 implementation and repair calls, 2 diff-review calls, and 1
  completion-evaluation call.
- Cap discovery and planning as the hosted budget grows so additional capacity
  funds implementation and repair instead of repeated exploration.
- Keep first-write guardrails anchored to calls 17 and 22 for larger missions,
  matching the local worker's bias toward producing and validating a concrete
  change early.
- Compact raw hosted conversation history to the latest three turn windows
  after notebook checkpointing, while preserving the durable notebook and
  existing diff as the authoritative continuation state.
- Accept internally consistent backend-signed hosted budgets up to 100 calls
  while continuing to enforce the exact signed mission limit.

### Fixed

- Prevent the old phase proportions and raw-history growth from exhausting a
  larger hosted budget before the implementation is complete.
- Add 70% and 90% budget guidance that prioritizes remaining acceptance
  criteria and the smallest complete validated result instead of starting new
  broad exploration.

## 1.4.11 - 2026-07-30

### Changed

- Detect resumable hosted work from the signed base SHA, deterministic branch,
  explicit incomplete draft pull request, and preserved diff before starting a
  later GitHub Actions attempt.
- Restore the draft pull request's remaining-work list and acceptance criteria
  into the worker notebook, preserving an authoritative notebook when one is
  available.
- Resume partial work from planning or the notebook's later phase instead of
  restarting repository discovery.
- Emit `worker.partial_run_detected` with the pull request, changed paths,
  remaining work, and actual resume phase.

### Fixed

- Prevent later hosted attempts from discarding or duplicating valid work when
  notebook metadata is unavailable or stale.
- Refuse to adopt arbitrary draft pull requests, first-attempt branches, or
  branches without changes relative to the signed mission base.

## 1.4.10 - 2026-07-30

### Changed

- Validate the hosted startup request, function-tool schemas, metadata, and
  model-facing options before repository work, then revalidate each exact agent
  and completion-evaluator request immediately before dispatch.
- Validate strict structured-output schemas whenever a request supplies a
  `text.format` configuration.
- Preserve bounded provider diagnostics, request identifiers, adapter and
  payload-schema versions, provider attempts, explicit gateway status, and
  reservation accounting through worker telemetry and terminal results.

### Fixed

- Encode `metadata.model_call_budget` as a string, as required by the provider,
  instead of sending the integer value that caused upstream HTTP 400
  `invalid_type` rejections.
- Derive provider-dispatch failures from authoritative provider evidence
  instead of reusing stale registration-conflict or dispatch-uncertain
  diagnostics.
- Restore phase capacity only when RustGrid explicitly proves that an invalid
  provider request consumed no semantic model call and incurred zero actual
  cost.
- Preserve actionable validation codes and exact paths, including
  `ai_tool_schema_invalid`, `ai_response_schema_invalid`, and request-envelope
  failures when their corresponding request shapes are validated, and retain
  safe provider errors up to the backend contract bounds.

## 1.4.9 - 2026-07-30

### Changed

- Separate the stable semantic AI-call identity from worker-session transport
  attempts, and retry proven pre-dispatch registration failures up to three
  times with bounded jitter without reinitializing the notebook or phase.

### Fixed

- Keep pre-dispatch registration retries outside model-call accounting and
  report precise retry/exhaustion telemetry without inferring safety from
  ambiguous legacy gateway failures.

## 1.4.8 - 2026-07-30

### Changed

- Scope hosted AI registration idempotency to execution ID, execution attempt,
  worker session, and semantic call index so a later attempt cannot replay a
  failed request from an earlier session.

### Fixed

- Restore the worker's phase ledger when RustGrid confirms an AI request failed
  before provider dispatch without consuming call budget, retry only explicitly
  retryable registration failures, and preserve the notebook and active phase.
- Report RustGrid gateway status, upstream provider status, failure stage,
  provider contact, budget consumption, and reservation reconciliation as
  separate safe diagnostics. Legacy `409 ai_provider_request_failed` replays
  are surfaced as `ai_request_idempotency_conflict`.

## 1.4.7 - 2026-07-29

### Changed

- Accept the canonical hosted `model_call_budget` contract together with
  requested, resolved, received, source, and clamp audit fields. Manifest v4
  fails with `execution_budget_mismatch` before the first model request when
  any value differs, while explicit legacy v3 custom budgets remain supported.
- Add a one-call `artifact_repair` phase that can only invoke
  `record_impact_map` and reuses the existing discovery notebook instead of
  repeating repository reads or searches.

### Fixed

- Recover a valid impact map from stored tool arguments, assistant JSON, or
  preserved notebook discovery progress when the primary tool invocation
  fails, and separate semantic production from metadata persistence.
- Treat phase-transition, notebook, and tool-event persistence failures as
  recoverable bookkeeping failures. Retry them idempotently without consuming
  another discovery call, preserve precise artifact diagnostics, and continue
  to planning whenever the impact map remains valid in memory or checkpoint
  state.

## 1.4.6 - 2026-07-29

### Changed

- Enforce hosted model-call budgets through explicit discovery, planning,
  implementation/repair, diff-review, completion-evaluation, validation, and
  publication phases. A signed 40-call mission receives the hard
  `8/4/20/4/4` allocation, while custom budgets use proportional allocation
  and retain at least half of their calls for implementation and repair.
- Require a machine-readable implementation plan between discovery and source
  mutation, and preserve a versioned worker notebook in durable execution
  events for context compaction and continuation.
- Group repository search results by file, reject duplicate and repeated search
  loops, restrict post-discovery inspection to mapped paths, and page complete
  diff review before an implementation declaration can be accepted.
- Require hosted coding missions to provide at least 10 model calls so every
  mandatory phase has viable capacity.

### Fixed

- Prevent discovery and planning from borrowing reserved implementation calls,
  prevent a final model message from bypassing missing phase artifacts, and
  stop zero-write runs at the implementation progress deadline with a precise
  structured blocker.
- Preserve specific AI gateway error codes, active phase, budget consumption,
  tool usage, last successful action, recoverability, and safe underlying
  request diagnostics instead of collapsing every failure into a generic
  `hosted_agent_execution_failed` result.

## 1.4.5 - 2026-07-29

### Changed

- Treat the 25 percent hosted discovery allocation as an advisory telemetry
  target instead of stopping execution when it is reached.
- Retain the complete hosted model turn history until the signed input ceiling
  requires trimming the oldest turns.
- Report discovery and planning consumption against targets, including calls
  consumed beyond each target.

### Fixed

- Allow hosted discovery to continue past the former five-call default until
  the required implementation impact map is complete, while preserving the
  signed overall mission budget and independent completion-review reserve.

## 1.4.4 - 2026-07-29

### Changed

- Attribute hosted-execution commits to the GitHub account that initiated the
  workflow using GitHub's ID-based private commit email format.

### Fixed

- Replace the unmatched `rustgrid-agent@users.noreply.github.com` commit email
  that caused downstream deployment providers to reject agent-created commits.
- Validate the GitHub actor login and numeric account ID before repository work,
  including GitHub App bot identities, and fail closed when either value is
  missing or malformed.

## 1.4.3 - 2026-07-29

### Added

- Add a structured implementation impact map, final diff-review declaration,
  independent completion evaluator, and per-phase model-budget telemetry for
  hosted GitHub Actions executions.
- Preserve partial work in a clearly marked draft pull request and support a
  later continuation attempt on the same deterministic branch.

### Changed

- Separate implementation completeness from technical validation. A hosted
  execution succeeds only when the evaluator proves every mapped acceptance
  criterion from concrete diff evidence and every required technical gate
  passes.
- Reserve model-call capacity for completion review, batch repository
  discovery, reconcile pull-request draft state after continuation, and report
  `partial_result` instead of overloading failure for valid resumable work.

### Fixed

- Prevent passing regression tests or builds from turning a budget-exhausted,
  partial implementation into a successful execution.
- Block automatic success after an unrecovered source-changing tool failure,
  a missing impact map, a stale implementation declaration, or acceptance
  criteria without changed-path evidence.

## 1.4.2 - 2026-07-28

### Added

- Add a bounded exact-text replacement tool for hosted implementation sessions
  so targeted repository edits do not require shell mutation commands.

### Fixed

- Reject shell operators, redirects, heredocs, and command chaining in the
  direct-process focused-command tool, and report non-zero command exits as
  failed tool calls instead of successful results.
- Continue to worker-owned authoritative quality gates when the implementation
  model exhausts its call budget after producing changes, while preserving
  fail-closed behavior for empty implementations and exhausted repair sessions.

## 1.4.1 - 2026-07-28

### Fixed

- Use the shared `https://app.rustgrid.com` control-plane origin for ephemeral
  GitHub Actions execution so the OIDC exchange targets the production API
  instead of the retired `api.rustgrid.com` hostname.

## 1.4.0 - 2026-07-28

### Added

- Add ephemeral `execute --provider github-actions` and
  `report-emergency-failure` commands with GitHub OIDC exchange, mission claim,
  heartbeat, execution-token refresh, event/telemetry reporting, deterministic
  branch and pull-request recovery, validation, and idempotent completion.
- Add a bounded internal Responses function-tool adapter for the RustGrid AI
  gateway, plus the version 3 hosted execution contract and operator runbook.

### Security

- Keep OpenAI credentials server-side and execution/GitHub credentials
  in-memory, zeroize secret buffers, validate all mission endpoints and policy
  hashes, refuse inherited provider credentials, reject symlink escapes, and
  strip GitHub Actions/OIDC/provider credentials from repository subprocesses.
- Isolate every repository-controlled hosted command in a root-owned cgroup-v2
  leaf, attach the blocked child through a bounded privileged write, verify
  membership before execution, and drain all descendants before publication.
- Patch the npm runtime's embedded `brace-expansion` and `tar` packages to
  versions without their fixable container findings.

### Changed

- Upgrade all open Dependabot suggestions for Rust dependencies and
  `actions/checkout`, including the `sha2` and `base64` major-version updates.

### Fixed

- Make hosted cgroup attachment work on GitHub's Ubuntu runners, where moving a
  process from the protected parent requires privilege at the common ancestor
  even if the destination membership file is writable.

## 1.3.0 - 2026-07-23

### Changed

- Remove worker ID and API-key environment authentication. Worker identity now
  comes only from the configuration written by device login, and authentication
  comes only from the OS keychain or owner-only credential-file store.
- Separate Codex reasoning from worker-owned deterministic delivery. Prompts now
  expose focused validation and full-gate ownership, reusable dependency state
  prevents duplicate installs, and gate failures create compact repair sessions.
- Enforce multidimensional mission budgets during provider turns and tool calls,
  derive routine progress from tool activity, and distinguish agent sessions,
  inference turns, context size, and cumulative token usage in telemetry.
- Normalize and summarize worker command output before repair context ingestion
  while retaining complete raw gate evidence and source-tree fingerprints.
- Preserve ticket requirements, changed paths, bounded diffs, validation
  evidence, and budget state across constrained Codex restarts. Completion now
  requires explicit implementation readiness and current focused-validation
  evidence for code changes.

### Fixed

- Connect detailed Codex execution and token telemetry to the live streaming
  path so model-call usage reaches RustGrid instead of leaving only the legacy
  aggregate token report.
- Classify coding missions only after repository checkout, restore complete
  ticket and repository-instruction context, and keep optimization budgets
  advisory so they cannot under-equip or prematurely terminate a valid task.
- Prevent ticket assignment from freezing during Docker Sandbox preparation:
  report preparation progress immediately, make sandbox commands cancellable
  and time-bounded, and verify the Codex version embedded in the pinned template
  instead of installing a different CLI release during every run.
- Prevent duplicate active attempts for one ticket in a worker session, bind
  step idempotency to the exact request body, and retry RustGrid's explicit
  in-flight idempotency conflict without retrying permanent 409 responses.
- Enforce one local `serve`, `watch`, or `run` execution owner per worker
  identity so duplicate processes cannot steal leases or race deterministic
  sandbox names.

## 1.2.0 - 2026-07-19

### Added

- Add versioned execution, phase, turn, model-call, and tool-call telemetry
  snapshots with stable event identifiers and bounded batch delivery to the
  RustGrid telemetry API.
- Add a durable, size-limited telemetry outbox so transient delivery failures
  do not interrupt agent runs and can be retried safely.
- Normalize Codex turn usage into provider-reported token details while
  preserving the existing aggregate token-consumption report.

### Changed

- Refresh the checked-in RustGrid OpenAPI contract for telemetry ingestion and
  related API updates.

## 1.1.0 - 2026-07-19

### Added

- Add `rustgrid-agent setup`, which detects host CPU and memory, recommends a
  concurrent-job count, and derives production Docker Sandbox capacity without
  requiring operators to maintain low-level resource fields manually.
- Add a stable user-level configuration path with an environment override and
  safe import of legacy working-directory configuration and worker identity.

## 1.0.1 - 2026-07-19

### Security

- Move the release runtime to a digest-pinned Node 24.18 image based on Debian
  trixie, upgrade installed operating-system packages, and pin npm 12.0.1 with
  its patched `undici` 6.27.0 dependency.
- Preserve a complete Grype vulnerability report while blocking publication on
  every fixable High or Critical finding. Unfixed distribution findings remain
  visible for deployment review instead of making remediation impossible.

### Fixed

- Update Anchore's scanner action to its Node 24 release, removing the GitHub
  Actions Node 20 deprecation warning.
- Derive the CI Homebrew formula version from Cargo metadata instead of a
  hard-coded release number.

## 1.0.0 - 2026-07-19

Initial stable public artifact release. Production deployment remains subject
to the separate staging certification and approval process.

### Added

- Production-oriented worker supervision, recovery journals, typed outcomes, bounded execution, GitHub token brokering, queue replay, and structured lifecycle reporting.
- Open-source governance, container packaging, deployment guidance, and release certification controls.

### Security

- Production serving requires a preflighted Docker Sandbox executor, digest-pinned template, effective network policy, and aggregate capacity admission. Every concurrent run receives its own microVM.
- Sandbox identities are collision-resistant and journaled; startup removes managed orphans, active execution enforces workspace quotas, and allowlisted secrets use short-lived mode-0600 env files rather than process arguments.
- Server-provided execution policy cannot override worker-enforced Codex sandbox or approval settings.

### Fixed

- Homebrew publication downloads the generated formula from the tagged source
  repository explicitly, even though the workflow checks out the tap in a
  nested directory.
