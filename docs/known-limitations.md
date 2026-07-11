# Known limitations

- The binary does not create a container or VM boundary. Operators must supply it and may set `RUSTGRID_AGENT_ISOLATION=per_run` only after doing so.
- Production `serve` supports one active run per worker process. Scale horizontally with separately isolated identities.
- `status --json` is a readiness check and contacts RustGrid; it is not an independent process-liveness endpoint.
- Complete artifact bundles and central OTLP export require deployment/control-plane integrations described in `docs/telemetry.md`.
- Failed workspaces may contain proprietary source and remain on disk until retention cleanup. Production storage must be encrypted and access controlled.
- Network access is required by the current Codex policy. Destination-level egress enforcement belongs to the runtime.
- Windows is not currently supported or tested.
- Homebrew/core inclusion depends on third-party review; the RustGrid tap is the initial supported Homebrew channel.
