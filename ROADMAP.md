# Roadmap

## Release candidate

- Publish signed source, native binary, Homebrew, SBOM, and container artifacts.
- Complete credentialed staging certification and rollback rehearsal.
- Operate the release candidate through the documented soak period with no unresolved P0/P1 findings.

## Toward 1.0

- Introduce a dedicated executor protocol so long-lived coordinators can launch every run in a separately attested container or microVM.
- Add tenant-scoped artifact upload with bounded retention and redaction policy.
- Add deployment-integrated metrics/OTLP export without exposing ticket content.
- Formalize RustGrid worker API version negotiation and deprecation windows.
- Expand fault injection across control-plane outages, disk exhaustion, GitHub ambiguity, and process escape attempts.

## Non-goals

- Running repository code directly on an operator workstation as a recommended production model.
- Storing long-lived GitHub credentials in worker configuration.
- Replacing repository-specific quality gates with RustGrid-owned test logic.

Roadmap items are directional rather than promises. Security and compatibility work may change their order.
