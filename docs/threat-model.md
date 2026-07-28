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
- **Hosted provider credential theft:** GitHub OIDC request credentials and
  RustGrid execution tokens remain in the parent process and have redacted
  debug representations. The hosted path refuses inherited OpenAI/ChatGPT
  credentials, never loads Codex auth state, validates every manifest endpoint
  against the mission API origin, and gives repository subprocesses no
  RustGrid, GitHub, Actions, OpenAI, or other sensitive allowlisted variable.
  Repository-token Git operations additionally require the exact HTTPS origin
  and an unchanged local Git config/history, ignore ambient Git config and
  credential helpers, reject the external transport, and disable Git hooks.
  The hosted Linux coordinator is non-dumpable so same-user child processes
  cannot inspect its environment, memory, or descriptors through procfs or
  ptrace. Repository commands start behind a trusted gate in a root-owned
  cgroup-v2 leaf with `no_new_privs`; the blocked child is attached through a
  bounded privileged write and its membership is verified before release.
  Only the leaf's `cgroup.kill` is delegated to the coordinator and is drained
  after every focused command, dependency bootstrap, and quality gate and
  immediately before every GitHub token request. Session changes and
  double-forks therefore cannot leave a helper alive for later credentialed
  publication. Unsupported or escapable cgroup configurations fail closed.
- **Arbitrary model proxying:** the hosted adapter calls only the execution's
  fixed `/ai/responses` endpoint with the resolved model and bounded function
  tools. It cannot select provider resource endpoints, storage, or streaming.
- **Cross-run access:** production startup requires a working Docker Sandbox
  executor. Each run has a distinct microVM and only its disposable clone is
  mounted into that VM.
- **Command escape:** commands are argument-parsed without a shell, Git hooks
  are disabled, and quality gates receive only the allowlisted environment.
  Local execution keeps the Codex workspace sandbox. Production Codex disables
  its redundant inner OS sandbox so repository binaries can spawn, but only
  inside the per-run Docker Sandbox microVM that owns filesystem, process,
  resource, and network isolation.
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

Hosted Git publication materializes the short-lived authorization header in
the trusted Git subprocess environment. Repository-controlled descendants are
drained before that token is requested, and the coordinator is non-dumpable,
but a separate pre-existing process running as the same workflow user could
inspect the transient Git child through procfs. The canonical workflow must
therefore retain job-level runner isolation and must not start unrelated
same-user background processes. Removing this residual requires a future
credential-helper or authenticated broker design that does not place the token
in the Git child environment.

Codex authentication state is a high-value deployment secret. Production
sandboxes must use a dedicated least-privilege Codex identity, make its state
read-only where supported, avoid reusing developer credentials, and rotate it
after suspected workspace escape. Staging certification must explicitly test
that repository commands cannot read or publish that state.
