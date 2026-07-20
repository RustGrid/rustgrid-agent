# Changelog

All notable changes to rustgrid-agent are documented here. The project follows
Semantic Versioning.

## Unreleased

### Fixed

- Connect detailed Codex execution and token telemetry to the live streaming
  path so model-call usage reaches RustGrid instead of leaving only the legacy
  aggregate token report.
- Isolate production Codex runs from ambient user plugins and configuration,
  classify mission complexity, enforce class-specific compaction limits, bound
  eagerly loaded ticket/repository context, and emit advisory budget warnings.

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
