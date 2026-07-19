# Known limitations

- Production isolation depends on Docker Sandboxes. macOS and Ubuntu Linux hosts must meet Docker's current virtualization, architecture, and KVM requirements; the worker fails closed when `sbx` preflight does not pass.
- Docker Sandbox templates and local kits evolve with `sbx`; production rejects mutable template tags, but digest and kit changes still require staging.
- `status --json` is a readiness check and contacts RustGrid; it is not an independent process-liveness endpoint.
- Complete artifact bundles and central OTLP export require deployment/control-plane integrations described in `docs/telemetry.md`.
- Failed workspaces may contain proprietary source and remain on disk until retention cleanup. Production storage must be encrypted and access controlled.
- Cross-attempt reuse requires the RustGrid retry creator to copy the failed run ID into `metadata.resume_from_run_id`; the worker deliberately does not infer lineage from ticket history.
- Network access is required by the current Codex policy. The worker verifies that a Docker policy exists; exact destination rules remain an operator or organization-governance responsibility.
- Windows is not currently supported or tested.
- Homebrew/core inclusion depends on third-party review. The supported Homebrew
  installation source is the public `RustGrid/tap` tap.
