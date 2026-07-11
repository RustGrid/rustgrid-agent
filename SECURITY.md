# Security policy

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub private
vulnerability reporting for `RustGrid/rustgrid-agent`. If private reporting is
unavailable, contact a listed repository maintainer without vulnerability
details to establish a private channel; do not send sensitive evidence publicly.
Include the affected version or commit, impact, reproduction steps, and any
evidence that credentials or tenant data were exposed.

RustGrid aims to acknowledge reports within three business days, provide an
initial assessment within seven business days, and coordinate disclosure after
a fix or mitigation is available. Complex issues may require a longer embargo;
the reporter will receive status updates at least every fourteen days.

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

The worker executes repository-controlled code. Production `serve` therefore
requires the Docker Sandbox executor and fails closed if `sbx` is unavailable.
Do not use the local executor for untrusted repositories.
