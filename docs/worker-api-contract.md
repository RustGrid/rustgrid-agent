# RustGrid worker run contract

`rustgrid-agent` uses the following run-scoped control-plane endpoints after a
ticket has been atomically claimed. The configured RustGrid API URL already
contains `/api/v1`.

## Token consumption

At terminal finalization, the worker writes the aggregate consumption from every completed agent turn in the run to `PUT /agent-runs/{run_id}/token-consumption`. The payload contains `provider` (`codex` or `claude`, from the run's execution policy), `input_tokens`, `cached_input_tokens`, `output_tokens`, and `total_tokens`; retries replace the same per-run resource idempotently. This report is sent before the successful terminal status update, and unsuccessful runs attempt the same report before failure, cancellation, or timeout handling.

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
  "execution_policy_sha256": "64-lowercase-hex-characters",
  "architecture": null
}
```

The server must derive this document from the claimed ticket, project binding,
and GitHub App installation. The worker rejects unsupported schema versions,
identity mismatches, missing values, zero installation IDs, and a local origin
that does not match `owner/name`.

The worker verifies the policy SHA-256, executes only the server-owned agent
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

### Agent providers and policy versions

Policy version `1` always runs Codex and carries the `codex` block shown above.
Policy version `2` replaces it with a provider-tagged `agent` block. RustGrid
emits version `2` for models in its Claude Code catalog:

```json
"execution_policy": {
  "policy_version": 2,
  "agent": {
    "provider": "claude",
    "command": ["claude", "-p", "--output-format", "stream-json", "--verbose", "--model", "claude-opus-5-5"],
    "environment_allowlist": ["PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME"]
  },
  "quality_gates": [],
  "timeout_seconds": 3600,
  "sandbox": {"mode": "workspace_write", "network_access": true, "writable_roots": ["."], "approval_policy": "never"}
}
```

`agent.provider` is `codex` or `claude`, and `agent.idle_timeout_seconds` is
optional as it is for `codex`. A version `1` policy with an `agent` block, a
version `2` policy with a `codex` block, an unknown provider, or an unknown
version is rejected before any work starts. Workers released before version `2`
reject Claude policies instead of running Codex in their place.

A Claude command must invoke the `claude` executable. The worker owns print
mode, `stream-json` output, `--verbose`, `--no-session-persistence`, and the
permission mode, and rejects policies that set permissions, settings or
setting sources, tool allowlists, extra directories, MCP or plugin
configuration, custom agents, session resume or continuation, remote, cloud,
background, or worktree sessions, a different input or output format, or a
fallback model. Inside a Docker Sandbox microVM the worker adds
`--permission-mode bypassPermissions`, the Claude Code equivalent of Codex
`danger-full-access`. The local executor adds `--permission-mode acceptEdits`
with Claude Code's OS sandbox enabled, writes confined to the workspace, and no
unsandboxed fallback, the equivalent of Codex `workspace-write`.

Claude Code never receives the tenant's Anthropic key. Before each Claude
session the worker calls `POST /agent-runs/{run_id}/model-gateway-token`:

```json
{
  "token": "rgmg_...",
  "expires_at": "2026-10-23T12:00:00Z",
  "provider": "anthropic",
  "model": "claude-opus-5-5",
  "base_path": "/agent-gateway/anthropic"
}
```

The token is issued only to the bound worker while it holds an active lease on
a run whose signed policy uses Claude Code and whose tenant has a valid
Anthropic connection; a missing connection is `409` with code
`ai_provider_not_configured`. Claude Code sends Anthropic Messages requests to
`<API URL><base_path>/v1/messages` with `Authorization: Bearer <token>`.
RustGrid rechecks the lease on every request, rejects any model other than the
run's, attaches the key, and streams the response back.

The worker translates Claude Code's `stream-json` messages into the same agent
event stream it reads from Codex, so worker-owned command interception,
focused-validation evidence, completion readiness, idle detection, budgets, and
telemetry behave identically for both providers. Claude usage is normalized
before it is reported: `input_tokens` is uncached input plus cache writes plus
cache reads, and `cached_input_tokens` is the cache-read share.

## Architecture contract and escalations

`architecture` is optional and is `null` when the project defines no
architecture. When present it is an `AgentArchitectureContext`:

```json
{
  "version": 3,
  "document": {
    "schema_version": "rustgrid.architecture.v1",
    "boundaries": [
      {
        "id": "security-authz",
        "name": "Authorization",
        "paths": ["crates/ticket-service/src/security/**"],
        "may_depend_on": []
      }
    ],
    "rules": [],
    "decisions": [],
    "profile": {}
  },
  "ticket_boundary_ids": ["api"],
  "open_escalations": [],
  "escalation_path": "/api/v1/agent-runs/{run_id}/architecture-escalations",
  "instructions": "Escalate instead of crossing a boundary."
}
```

The worker rejects the manifest when the schema version is not
`rustgrid.architecture.v1`, a boundary ID is invalid or duplicated, a path glob
is absolute or contains `..`, any `may_depend_on`, `applies_to`,
`ticket_boundary_ids`, or open-escalation entry names an unknown boundary,
`escalation_path` does not name the claimed run, or the context exceeds
256 KiB. The worker always calls its own run-scoped path and never follows a
URL taken from the manifest.

Boundary `paths` are repository-relative globs: `*` and `?` match within one
path segment, `**` matches any number of segments, and a trailing `/` matches
everything below a directory.

The plan is checked twice:

1. The Codex prompt carries the contract, the ticket's declared boundaries, and
   any open escalations. Codex must list the files it will change and their
   owning boundaries before editing. If no compliant plan exists, it ends with
   `RUSTGRID_AGENT_STATUS: ESCALATE`, `RUSTGRID_ESCALATION_BOUNDARIES`,
   `RUSTGRID_ESCALATION_REASON`, and `RUSTGRID_PROPOSED_CHANGE` lines.
2. After every successful Codex session (initial, gate repair, and CI repair)
   the worker matches each agent-changed path against the boundaries. A path is
   in scope when it belongs to no boundary or to at least one declared
   boundary. When `ticket_boundary_ids` is empty no path scope is enforced.
   `may_depend_on`, rules, and decisions are enforced through the prompt, CI,
   and review, not by the worker's diff check.

Either check that fails raises an escalation:

`POST /agent-runs/{run_id}/architecture-escalations`

```json
{
  "boundary_ids": ["security-authz"],
  "reason": "Why the plan cannot stay inside the declared boundaries.",
  "proposed_change": "What the change would look like if approved."
}
```

The request carries an `Idempotency-Key` derived from the run and payload, so a
retry returns the original escalation. RustGrid answers `201` with an
`ArchitectureEscalationResponse`, `400` for an unknown boundary or invalid text,
and `409` when the project has no architecture or the worker lease is not
active; a `409` with a lease-lost error code is handled as lease loss. When an
open escalation in the manifest already covers the requested boundaries, the
worker reuses it instead of raising another.

After escalating, the run ends as a human-intervention block: the ticket moves
to `blocked`, the workspace and sandbox are retained, and the blocking reason
names the escalation ID. Approval is expected to be reflected in a later run's
`ticket_boundary_ids`; the manifest only lists open escalations.

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
