# Telemetry and data handling

The worker emits human-readable lifecycle logs by default and newline-delimited JSON when `RUSTGRID_AGENT_LOG=json`. Service managers should collect stdout and stderr with access controls appropriate for repository metadata.

## Required production signals

The RustGrid control plane and deployment platform should alert on:

- worker heartbeat age and readiness;
- active runs and claim latency;
- queue cursor lag;
- lease-renewal age and lease loss;
- API and GitHub retry counts;
- ambiguous event reconciliation;
- workspace bytes and retained workspace count;
- sandbox create/destroy latency, orphan cleanup count, and quota-stop events;
- active sandbox count compared with assigned active runs;
- run outcome category and child termination reason;
- process and container restart rate.

## Execution-efficiency semantics

An agent session is one `codex exec` process. A model inference turn is one
provider `turn.started`/`turn.completed` pair inside that process. Tool calls are
captured from provider item events. These counts are separate: a single agent
session may contain many inference turns and tool calls. Historical runs that
only contain an aggregate model call must not be reinterpreted as turn-level
data; consumers should show inference turns as unavailable.

Provider-reported input is cumulative model input across inference turns, not a
single context window. Cached input is a subset of input; fresh input is input
minus cached input. `context_tokens_before_call` is the context size for a turn,
and the maximum observed value is peak context. Missing provider context or
reasoning fields remain unavailable rather than being estimated as zero.

Each completed model-call snapshot includes the explicit mission class,
initial-prompt estimate, budget, session count, and provider-turn semantics in
its sanitized usage metadata. Lifecycle budget events include the threshold,
current multidimensional usage, and action. Ownership interruptions and
duplicate gate/bootstrap avoidance are recorded as steps.

Worker command output has three model-facing modes: `summary`,
`failure_excerpt`, and explicit `full_requested`. Successful output is reduced
to aggregate results; failures are bounded to 160 lines and 24,000 characters.
ANSI control sequences and duplicate lines are removed from normalized output.
Raw output remains in the quality-gate record and under the run checkout's
`.git/rustgrid-agent-audit/commands` directory for local recovery and forensics.
The raw path, original/model character counts, mode, and truncation flag are
included in lifecycle metadata.

## Sensitive data

Never export API keys, GitHub tokens, authorization headers, raw environment variables, private source, or unbounded command output. Ticket descriptions, comments, paths, branch names, gate output, and pull-request URLs may be tenant-sensitive.

Retained logs and workspaces need documented owner, region, access policy, encryption, and deletion period. The default failed-workspace retention is 72 hours; operators should choose the minimum period needed for incident response.

## Export integration

The worker does not embed an OTLP client. Sandbox lifecycle, cleanup, and quota
events are included in structured stdout. Collect them through the deployment
logging agent and derive infrastructure metrics from RustGrid worker/run state
plus Docker Sandbox metrics. A future native metrics endpoint must remain
unauthenticated only on a private loopback or sidecar network and must never
expose ticket content.

Artifact upload is not enabled until the RustGrid worker API supplies a tenant-scoped artifact endpoint with size, retention, and authorization policy. Until then, retain only bounded lifecycle data and the local failed workspace.
