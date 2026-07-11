# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub private
vulnerability reporting for `RustGrid/rustgrid-agent`, or contact the RustGrid
maintainers through the private security channel published by the organization.
Include the affected version or commit, impact, reproduction steps, and any
evidence that credentials or tenant data were exposed.

## Supported versions

Until the first stable release, only the latest commit on `main` is supported.
After stable release, the latest minor release receives security fixes. Critical
credential or isolation defects may require immediately disabling affected
workers rather than waiting for a patched release.

## Credential safety

Never include RustGrid API keys or GitHub installation tokens in issues, logs,
run events, prompts, commits, or artifacts. Revoke suspected credentials before
sharing diagnostics. GitHub tokens are run-scoped, held only in memory, and must
be issued through the RustGrid broker.

## Production boundary

The worker executes repository-controlled code. Production deployments must
provide a genuine per-run filesystem and resource boundary and must not set
`RUSTGRID_AGENT_ISOLATION=per_run` until that boundary exists.

