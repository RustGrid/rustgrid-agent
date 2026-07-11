# Threat model

## Protected assets

- RustGrid worker credentials and tenant-scoped ticket data.
- Run-scoped GitHub installation tokens and repository write access.
- Other runs, retained workspaces, worker hosts, and deployment infrastructure.
- Ordered lifecycle history used for audit and recovery.

## Trust boundaries

RustGrid owns manifests, leases, policy, and token issuance. GitHub owns
repository and check-run state. Ticket text, repository contents, Codex output,
quality-gate processes, and network responses are untrusted. Docker Sandbox
owns each run's filesystem, process, resource, and network isolation.

## Primary threats and controls

- **Credential theft:** secrets are removed from child environments; GitHub
  tokens are scoped, cached only in memory, and validated against the manifest.
- **Cross-run access:** production startup requires a working Docker Sandbox
  executor. Each run has a distinct microVM and only its disposable clone is
  mounted into that VM.
- **Command escape:** commands are argument-parsed without a shell, Codex uses a
  workspace sandbox, Git hooks are disabled, and quality gates receive only the
  allowlisted environment.
- **Resource exhaustion:** wall/CPU/address-space/file/open-file/output limits,
  symlink-safe accounting, and deployment quotas bound untrusted children.
- **Replay or duplicate side effects:** leases, ETags, ordered events, semantic
  idempotency keys, and a durable journal reconcile retries and restarts.
- **Stale ownership:** lease loss cancels local execution and suppresses terminal
  writes from the former owner.
- **Supply-chain compromise:** locked dependencies, `cargo-deny`, immutable
  action SHAs, SBOM generation, and artifact attestations protect releases.

## Residual risks

The agent cannot independently attest the Docker Desktop host, enforce
GitHub repository rules unavailable on the current plan, or protect against a
compromised RustGrid control plane. Credentialed staging and periodic isolation
escape tests remain mandatory.

Codex authentication state is a high-value deployment secret. Production
sandboxes must use a dedicated least-privilege Codex identity, make its state
read-only where supported, avoid reusing developer credentials, and rotate it
after suspected workspace escape. Staging certification must explicitly test
that repository commands cannot read or publish that state.
