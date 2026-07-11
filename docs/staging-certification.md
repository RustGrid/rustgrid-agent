# Staging certification

Production promotion requires a credentialed run against the same RustGrid and
GitHub App topology used in production. Local mocks and contract tests do not
satisfy this gate.

## Prerequisites

- A dedicated staging tenant, project, linked GitHub App installation, and test repository.
- A least-privilege worker API key stored by the deployment secret manager.
- A per-run filesystem/container boundary with CPU, memory, process, disk, and network controls.
- `RUSTGRID_AGENT_ISOLATION=per_run` set only inside that deployment boundary.
- Required GitHub workflows enabled on the test repository.

## Required scenarios

1. Complete a normal ticket from queue claim through `awaiting_review` and verify
   every progress event, feedback comment, quality gate, branch, commit, pull
   request, and external link.
2. Restart the worker after branch creation, commit, push, and pull-request
   creation. Each restart must reconcile without a second commit or pull request.
3. Revoke the run lease during Codex and immediately before publication. The
   child must stop and no stale terminal update may be accepted.
4. Interrupt the queue stream, drop an event response, and force a cursor
   conflict. Durable replay must preserve ordering without losing an event.
5. Issue a token close to expiry and verify refresh without persisting or logging
   either token.
   Verify separately that repository commands cannot read the worker API key,
   GitHub token, Codex authentication state, SSH agent socket, or deployment
   credential variables.
6. Produce excessive stdout, stderr, a single oversized line, a large file, and
   a workspace symlink. The run must fail within its configured limits without
   escaping its fresh container boundary or affecting the next scheduled run.
7. Send SIGTERM and verify draining; send SIGINT to a separate run and verify
   immediate child-process-group cancellation.

## Evidence

Retain run IDs, ticket IDs, worker IDs, GitHub pull-request URLs, ordered event
exports, worker logs, resource-limit evidence, and screenshots or API results
showing final ticket/run state. Record the tested agent commit and deployment
image digest. Any failed scenario blocks promotion.

Use `docs/staging-evidence-template.md` for the signed certification record.
