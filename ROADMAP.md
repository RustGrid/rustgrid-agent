# Roadmap

## Post-1.0 priorities

- Complete credentialed staging certification, rollback rehearsal, and the
  documented soak period before the first production deployment.
- Add a remote executor option for Linux worker fleets; the current production executor launches a Docker Sandbox microVM per run.
- Add tenant-scoped artifact upload with bounded retention and redaction policy.
- Add deployment-integrated metrics/OTLP export without exposing ticket content.
- Formalize RustGrid worker API version negotiation and deprecation windows.
- Expand fault injection across control-plane outages, disk exhaustion, GitHub ambiguity, and process escape attempts.

## Non-goals

- Running repository code directly on an operator workstation as a recommended production model.
- Storing long-lived GitHub credentials in worker configuration.
- Replacing repository-specific quality gates with RustGrid-owned test logic.

Roadmap items are directional rather than promises. Security and compatibility work may change their order.
