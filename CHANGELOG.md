# Changelog

All notable changes to rustgrid-agent are documented here. The project follows
Semantic Versioning.

## Unreleased

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
