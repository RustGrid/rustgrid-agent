# Known limitations

- Production isolation depends on Docker Sandboxes, which currently require Docker Desktop and are unavailable on Linux hosts.
- Sandbox templates and kits are an early-access Docker feature; production rejects mutable template tags, but digest changes still require staging.
- `status --json` is a readiness check and contacts RustGrid; it is not an independent process-liveness endpoint.
- Complete artifact bundles and central OTLP export require deployment/control-plane integrations described in `docs/telemetry.md`.
- Failed workspaces may contain proprietary source and remain on disk until retention cleanup. Production storage must be encrypted and access controlled.
- Network access is required by the current Codex policy. The worker verifies that a Docker policy exists; exact destination rules remain an operator or organization-governance responsibility.
- Windows is not currently supported or tested.
- Homebrew/core inclusion depends on third-party review; the RustGrid tap is the initial supported Homebrew channel.
