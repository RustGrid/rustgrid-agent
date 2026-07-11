# RustGrid worker run contract

`rustgrid-agent` uses the following run-scoped control-plane endpoints after a
ticket has been atomically claimed. The configured RustGrid API URL already
contains `/api/v1`.

## Execution manifest

`GET /agent-runs/{run_id}/manifest`

```json
{
  "manifest_version": 1,
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
  "required_permissions": {}
}
```

The server must derive this document from the claimed ticket, project binding,
and GitHub App installation. The worker rejects unsupported schema versions,
identity mismatches, missing values, zero installation IDs, and a local origin
that does not match `owner/name`.

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
