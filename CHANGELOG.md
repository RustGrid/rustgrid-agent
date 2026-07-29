# Changelog

All notable changes to rustgrid-agent are documented here. The project follows
Semantic Versioning.

## Unreleased

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
