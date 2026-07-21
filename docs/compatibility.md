# Compatibility policy

## Versions

- Execution manifest version: `2`
- Execution policy version: `1`
- Recovery journal schema: `1`
- Minimum Rust version for source builds: `1.94`
- Supported worker operating systems: Linux and macOS
- Interactive login server baseline: RustGrid with migration
  `0047_worker_device_authentication` and the plural
  `/api/v1/agent-workers/device-authorizations` endpoints

Unknown manifest, policy, and journal versions fail closed. Additive API response fields are accepted. Known external status values use typed enums with an `Unknown` representation where ignoring a new value is safe.

Before `1.0.0`, minor releases may contain intentional compatibility changes described in `CHANGELOG.md`. Patch releases must remain compatible with the current manifest, policy, journal, and documented command-line interface.

## Control-plane upgrades

Deploy additive RustGrid API changes before workers that require them. Do not remove an endpoint or required response field until all supported workers have been upgraded. The committed OpenAPI snapshot and CI assertions document the minimum worker contract.

Interactive login treats `404 Not Found` and `405 Method Not Allowed` from the
device-authorization start endpoint as an incompatible server and does not
silently create an administrative credential. Workers must use the credential
store populated by device login. The legacy exact `RUSTGRID_API_URL` endpoint
override remains supported while a control-plane deployment is upgraded.

## Rollback

Tagged releases must be able to read journal schema `1`. If a future release changes journal storage, it must include a forward migration or document that workers must be drained before upgrade. Rollback must never reinterpret a newer journal silently.
