# GitHub Actions hosted execution

The GitHub Actions provider runs one RustGrid mission on a disposable
GitHub-hosted runner. It does not register a persistent worker and does not use
GitHub Secrets for RustGrid or OpenAI credentials.

## Authentication and lifecycle

The canonical workflow grants only `contents: read` and `id-token: write`,
checks out without persisted credentials, and installs a checksummed,
version-pinned agent binary. RustGrid dispatches the workflow with an execution
UUID and one-time nonce.

`rustgrid-agent execute --provider github-actions --execution-id <uuid>`:

1. requests a JWT from GitHub's OIDC endpoint for the RustGrid API audience;
2. exchanges that JWT and dispatch nonce for a 15-minute `rge_` execution
   bearer bound to tenant, project, execution, attempt, repository and workflow
   run;
3. claims the mission, validates manifest v3 and starts heartbeat/token refresh;
4. requires `GITHUB_SHA` to match the manifest's exact 40-hex `base_sha`,
   then reuses the deterministic remote branch or creates it directly from
   that immutable commit already present in the Actions checkout; mutable
   `base_ref` is never fetched to seed a fresh execution;
5. calls the RustGrid AI gateway through the bounded internal repository-tool
   adapter and enforces hard discovery, planning, implementation/repair, and
   paged diff-review allocations. The default signed 40-call policy is
   `8/4/20/4/4`; search loops, missing phase artifacts, and missed write
   thresholds produce durable guardrails rather than unrestricted exploration;
6. runs required technical validation without execution or GitHub credentials,
   then uses a reserved fresh model context to evaluate ticket requirements
   against the impact map, implementation plan, versioned worker notebook,
   complete diff, changed paths, implementation declaration, validation
   evidence, and unresolved tool failures;
7. obtains a short-lived repository-scoped GitHub App token, pushes without
   force, and creates or locates the pull request. Complete work opens a normal
   pull request; partial, incomplete, or uncertain work is preserved in a
   prominently marked draft pull request for continuation. Remote branch state is
   reconciled with bounded retries and revalidated before publication. Before
   the token is used, the agent rechecks the exact credential-free HTTPS
   origin, unchanged Git config and unchanged branch history; tokenized Git
   disables hooks, external protocols, credential helpers, and ambient Git
   configuration;
8. reports technical validation and implementation completeness independently,
   and completes successfully only when every applicable criterion has concrete
   diff evidence and all required technical gates pass.

Continuation attempts reuse the deterministic branch and draft pull request,
rerun implementation in a fresh model context, and reconcile the pull-request
title, body, and draft state. A completed continuation removes the incomplete
marker and marks the pull request ready for review. When the manifest contains
`run.metadata.worker_notebook`, the agent skips completed discovery only if the
base SHA, branch, and repository-diff fingerprint match the checkpoint.

RustGrid revokes AI access when the execution becomes terminal, cancelled,
timed out or lost. A revoked token stops the heartbeat supervisor and cancels
active repository commands.

## Required workflow environment

Hosted execution accepts only the canonical workflow values and GitHub-provided
default identity values:

```text
RUSTGRID_API_URL
RUSTGRID_EXECUTION_ID
RUSTGRID_DISPATCH_NONCE
RUSTGRID_OIDC_REQUEST_URL
RUSTGRID_OIDC_REQUEST_TOKEN
GITHUB_REPOSITORY
GITHUB_REPOSITORY_ID
GITHUB_RUN_ID
GITHUB_RUN_ATTEMPT
GITHUB_ACTOR
GITHUB_ACTOR_ID
GITHUB_WORKFLOW_REF
GITHUB_SHA
GITHUB_REF
```

GitHub provides `GITHUB_ACTOR` and `GITHUB_ACTOR_ID` as immutable default
variables for the account that initiated the workflow. The agent validates that
pair and configures commits as
`<id>+<login>@users.noreply.github.com`, so GitHub and downstream deployment
providers can associate the commit with the initiating account. Workflow reruns
retain the original actor identity and privileges rather than attributing
commits to a different rerun initiator.

Do not add `OPENAI_API_KEY`, `CODEX_API_KEY`, `CHATGPT_TOKEN`, a permanent
RustGrid token, or a GitHub App token. The agent fails closed if it inherits an
OpenAI or ChatGPT provider credential. It also accepts the OIDC request
credential only for GitHub's `actions.githubusercontent.com` token-service
hosts (or loopback HTTP in tests) and rejects a predeclared audience. On the
canonical Linux runner, the coordinator is non-dumpable before repository
commands begin, preventing same-user child processes from inspecting its
environment, memory, or file descriptors through procfs/ptrace. Every
repository-controlled focused command, dependency bootstrap, and quality gate
runs in a root-owned cgroup-v2 leaf. A trusted `/proc/self/exe` gate attaches
the blocked child before repository code starts through a bounded privileged
write to the leaf's `cgroup.procs`, verifies membership, sets `no_new_privs`,
and then releases it. Only the leaf's `cgroup.kill` control is delegated to the
unprivileged coordinator so it can drain the command after execution and before
any GitHub token is requested. This also kills descendants that use `setsid`
and double-fork. Hosted execution
fails closed on non-Linux systems, cgroup v1, kernels without `cgroup.kill`, a
writable parent cgroup, effective capabilities, root execution, or a runner
without non-interactive `/usr/bin/sudo`. The GitHub authorization header exists
transiently in the trusted Git subprocess environment during publication, so
the canonical job must remain isolated and must not start unrelated same-user
background processes. See the threat model for this residual boundary.

## Retry and recovery

The branch is `rustgrid/<lowercase-ticket-key>-<first-eight-execution-id>`.
Workflow retries for the same execution fetch and resume that branch and reuse
an open pull request. Ambiguous pull-request creation is resolved by looking up
the deterministic head branch. A new RustGrid execution attempt has a new
execution UUID and therefore a distinct branch. The agent uses normal Git push
semantics and never force-pushes the hosted branch.

The workflow's emergency callback is useful only when the primary step failed
before consuming the one-time OIDC exchange. After a successful exchange,
RustGrid reconciliation is authoritative for a runner that disappears before
completion.

## Operational diagnosis

Safe logs identify the execution, lifecycle step, HTTP status, machine-readable
error code and request ID. They do not include response bodies for failed
provider calls, OIDC JWTs, dispatch nonces, execution tokens or GitHub tokens.
Cancelled completion deliberately omits failure fields, matching the terminal
worker API contract; typed failure fields are sent only for `failed`.

Failed result events retain a bounded structured diagnostic containing the
specific failure code, active phase, total and phase call usage, tool-operation
counts, last successful action, safe underlying error class/message, request ID
when available, recoverability, resume phase, and recommended action. The
legacy completion request continues sending the stable `failure_code` and
sanitized `failure_message` until the control plane accepts the additive
structured fields directly.

Common failures:

- `ephemeral_worker_auth_failed` or `github_oidc_claim_mismatch`: verify the
  workflow path/version, repository binding, run attempt and dispatch
  correlation.
- `execution_token_invalid`: the token expired or the execution was
  cancelled/terminal; do not retry work outside RustGrid reconciliation.
- `execution_ai_budget_exceeded`: increase the mission budget through the
  authorized RustGrid retry flow rather than bypassing the gateway.
- `github_actions_permission_missing`: upgrade the RustGrid GitHub App
  installation permissions before dispatching another execution.
- `pull_request_creation_failed`: inspect the repository binding, branch
  protection and GitHub App `contents:write`/`pull_requests:write` readiness.

`report-emergency-failure` is best effort and deliberately uses the same OIDC
verification rather than accepting an unauthenticated terminal callback.
