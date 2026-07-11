# Changelog

All notable changes to rustgrid-agent are documented here. The project follows Semantic Versioning once `1.0.0` is released.

## Unreleased

### Added

- Production-oriented worker supervision, recovery journals, typed outcomes, bounded execution, GitHub token brokering, queue replay, and structured lifecycle reporting.
- Open-source governance, container packaging, deployment guidance, and release certification controls.

### Security

- Production serving fails closed without an explicit isolation declaration and currently restricts each worker process to one active run.
- Server-provided execution policy cannot override worker-enforced Codex sandbox or approval settings.

## 0.1.0 - Unreleased

Initial public release candidate. A release date is added only after credentialed staging certification succeeds.
