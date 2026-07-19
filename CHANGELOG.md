# Changelog

All notable changes to rustgrid-agent are documented here. The project follows Semantic Versioning once `1.0.0` is released.

## Unreleased

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

## 1.0.0 - Unreleased

Initial public release candidate. A release date is added only after credentialed staging certification succeeds.
