# RustGrid worker run contract

`rustgrid-agent` uses the following run-scoped control-plane endpoints after a
ticket has been atomically claimed. The configured RustGrid API URL already
contains `/api/v1`.

## Ephemeral GitHub Actions contract (manifest v3)

The GitHub Actions provider does not use any persistent worker endpoint or
credential. Its sequence is:

1. request a GitHub OIDC JWT using the workflow's
   `ACTIONS_ID_TOKEN_REQUEST_URL` and request token;
2. `POST /execution-auth/github-actions/exchange` with the execution UUID,
   one-time dispatch nonce, and JWT;
3. use the returned `rge_` bearer token for
   `/executions/{execution_id}/claim`, `/manifest`, `/heartbeat`,
   `/token/refresh`, `/worker-events`, `/telemetry/batch`, `/state`,
   `/github-token`, `/ai/responses`, and `/complete`.

The exchange response identifies the tenant, project, execution, attempt,
ephemeral worker/session, immutable repository, and GitHub workflow run. The
agent validates those identities, requires the workflow's `GITHUB_SHA` to
exactly match the manifest `base_sha`, and keeps `access_token` only in process
memory. It refreshes before expiry while renewing the execution lease.

Manifest version 3 contains:

- the execution/run/ticket/project identity and terminal budgets;
- immutable GitHub repository and installation IDs, clone/web origins,
  `base_sha`, PR target `base_ref`, deterministic branch, and repository-token
  endpoint;
- the resolved model, AI gateway endpoint, input/output/model-call/cost limits;
- the hashed execution policy and mission-scoped lifecycle endpoint paths.

Every endpoint path is validated against the configured RustGrid API origin and
execution UUID before use. The policy hash is computed from the typed v3 policy
wire representation. The agent rejects a mismatched workflow repository ID,
model or budget mismatch, unsafe Git ref, alternate gateway origin, sensitive
child-environment variable, or unsupported sandbox policy.

A fresh deterministic branch is created from the locally present immutable
`base_sha`; the agent does not fetch mutable `refs/heads/{base_ref}` to seed the
mission. A retry may fetch only its deterministic remote execution branch.

`POST /executions/{execution_id}/ai/responses` accepts only the constrained
Responses subset and requires a UUID `Idempotency-Key`. The agent uses an
internal function-tool adapter so the execution bearer never enters Codex,
`OPENAI_API_KEY`, `CODEX_API_KEY`, a config file, or a repository subprocess.
RustGrid is authoritative for model-call usage and cost; the agent sends
non-model execution telemetry only. Each required quality gate emits
deterministic `phase.started` and `phase.completed` telemetry with a
`quality_gate:*` phase name so successful completion has durable validation
evidence.

Successful completion requires the deterministic branch, 40-character head
SHA, pull-request number and URL. The completion idempotency key is derived
from the complete request. Failures use stable machine-readable codes and never
include provider response bodies. Cancellation or token revocation stops
repository commands and suppresses unsafe publication.

## Token consumption

At terminal finalization, the worker writes the aggregate consumption from every completed Codex turn in the run to `PUT /agent-runs/{run_id}/token-consumption`. The payload contains `provider`, `input_tokens`, `cached_input_tokens`, `output_tokens`, and `total_tokens`; retries replace the same per-run resource idempotently. This report is sent before the successful terminal status update, and unsuccessful runs attempt the same report before failure, cancellation, or timeout handling.

## Execution manifest

`GET /agent-runs/{run_id}/manifest`

```json
{
  "manifest_version": 2,
  "run": { "id": "run-uuid", "ticket_id": "ticket-uuid" },
  "project_id": "project-uuid",
  "project_key": "RG",
  "project_name": "RustGrid",
  "ticket_id": "ticket-uuid",
  "ticket_key": "RG-1",
  "ticket_title": "Example",
  "repository_id": 42,
  "repository": "RustGrid/example",
  "clone_url": "https://github.com/RustGrid/example.git",
  "web_base_url": "https://github.com",
  "default_branch": "main",
  "installation_id": 12345,
  "required_workflows": [],
  "required_permissions": {},
  "execution_policy": {
    "policy_version": 1,
    "codex": {
      "command": ["codex", "exec", "--json", "--model", "gpt-5.6-terra"],
      "environment_allowlist": ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"],
      "idle_timeout_seconds": 300
    },
    "quality_gates": [
      {"id": "gate-1", "command": "cargo test", "timeout_seconds": 900, "required": true}
    ],
    "timeout_seconds": 3600,
    "sandbox": {
      "mode": "workspace_write", "network_access": true,
      "writable_roots": ["."], "approval_policy": "never"
    }
  },
  "execution_policy_sha256": "64-lowercase-hex-characters"
}
```

The server must derive this document from the claimed ticket, project binding,
and GitHub App installation. The worker rejects unsupported schema versions,
identity mismatches, missing values, zero installation IDs, and a local origin
that does not match `owner/name`.

The worker verifies the policy SHA-256, executes only the server-owned Codex
command and gates, applies their timeouts and environment allowlist, and refuses
a sandbox policy it cannot enforce. The manifest's `workspace_write` mode
describes the effective repository scope. The local executor enforces it with
Codex `workspace-write`; the production executor maps it to Codex
`danger-full-access` only inside the disposable Docker Sandbox microVM, which
enforces the same filesystem scope plus the process, network, and resource
boundaries. Approval policy remains `never` in both cases.

The optional user-selected model is not read directly from client metadata by
the worker. RustGrid validates the requested identifier against its configured
catalog and places `--model <id>` in the snapshotted, hashed command above.

## Queue and capacity

The control plane announces the worker and issues its bound credential before
the process starts. The process receives both the worker UUID and credential,
then proves the binding by heartbeating that UUID. It never registers a new
worker using its runtime credential.

The heartbeat advertises `max_concurrency`. The worker resumes
`GET /agent-workers/{worker_id}/queue/stream` with `Last-Event-ID`, replays gaps
through `GET /agent-workers/{worker_id}/queue`, and reconciles only active runs
from `GET /agent-workers/{worker_id}/runs?status=running`. That recovery
collection spans every project in the credential's tenant and returns only
actively leased runs whose `worker_id` matches the registered worker, up to its
advertised capacity. Queue events carry `run_id`, `ticket_id`, and `project_id`,
but remain wake-up signals; the worker recovery collection is the source of
truth. Production workers never call `claim-next`. Polling remains a bounded
fallback when the stream is temporarily unavailable.

The worker has no configured project lock. Each run manifest provides the
project and repository context. RustGrid authorizes manifest access by tenant,
assigned worker identity, and active lease.

## GitHub installation token

`POST /agent-runs/{run_id}/github-token`

The request has no body. Worker identity and repository scope are derived from
the bound worker credential and active run lease.

```json
{
  "token": "ghs_secret",
  "expires_at": "2026-07-11T12:00:00Z",
  "repository": "RustGrid/example",
  "permissions": { "contents": "write", "pull_requests": "write" }
}
```

The server must verify the worker owns the live run lease and that the requested
installation matches the manifest. Tokens should be repository-scoped and must
never be persisted in RustGrid responses, logs, or activity metadata.

## Ordered progress events

`POST /agent-runs/{run_id}/events`

```json
{
  "event_type": "progress",
  "data": {
    "schema_version": 1,
    "sequence": 7,
    "timestamp_unix_ms": 1752200000000,
    "phase": "executing",
    "event_type": "step.codex.running",
    "severity": "info",
    "message": "Running Codex locally",
    "data": {}
  }
}
```

The server assigns the durable sequence:

```json
{ "sequence": 8, "run_id": "run-uuid", "event_type": "progress", "data": {}, "created_at": "..." }
```

The request idempotency key is stable for `run_id + client sequence`. If a
response is lost, the worker replays from its last server sequence with
`GET /agent-runs/{run_id}/events?after_sequence=N&limit=500`, finds the client
sequence in event data, and retries once only when the event was not accepted.

## Lease failure semantics

`POST /agent-runs/{run_id}/lease` continues to use the existing lease contract.
`404` and `409` mean ownership is lost. Transient failures are tolerated only
while the last confirmed lease remains safely inside its expiry window. When
ownership is lost or becomes uncertain, the worker cancels local commands and
does not publish a terminal run or ticket mutation.
