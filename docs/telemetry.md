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
- run outcome category and child termination reason;
- process and container restart rate.

## Sensitive data

Never export API keys, GitHub tokens, authorization headers, raw environment variables, private source, or unbounded command output. Ticket descriptions, comments, paths, branch names, gate output, and pull-request URLs may be tenant-sensitive.

Retained logs and workspaces need documented owner, region, access policy, encryption, and deletion period. The default failed-workspace retention is 72 hours; operators should choose the minimum period needed for incident response.

## Export integration

The worker does not embed an OTLP client. Collect structured stdout through the deployment logging agent and derive infrastructure metrics from RustGrid worker/run state plus container metrics. A future native metrics endpoint must remain unauthenticated only on a private loopback or sidecar network and must never expose ticket content.

Artifact upload is not enabled until the RustGrid worker API supplies a tenant-scoped artifact endpoint with size, retention, and authorization policy. Until then, retain only bounded lifecycle data and the local failed workspace.
