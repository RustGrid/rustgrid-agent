# Contributing to rustgrid-agent

Thank you for improving the RustGrid agent. The worker executes repository-controlled code and holds access to production control-plane APIs, so changes to execution, credentials, leases, recovery, publishing, and isolation receive security-focused review.

## Before opening a change

- Use a GitHub issue for significant behavior or contract changes.
- Report vulnerabilities privately as described in `SECURITY.md`.
- Keep pull requests narrow and include tests for changed behavior.
- Do not add telemetry, network destinations, credential exposure, or production mocks without explicit design review.

## Development setup

Install the Rust version declared by `rust-toolchain.toml`, then run:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo package --locked --allow-dirty
```

Changes to the worker API must update the committed OpenAPI snapshot or its compatibility assertions. Changes to recovery must test restart behavior at the affected durable checkpoint.

## Pull-request expectations

A pull request should explain:

- the user or operator problem;
- security and compatibility impact;
- validation performed;
- rollback behavior;
- documentation or migration requirements.

Maintainers may request a threat-model update, staging evidence, or fault-injection test. All commits must be attributable under the repository's MIT license.

## Compatibility

The current compatibility boundaries are documented in `docs/compatibility.md`. Do not silently accept unknown manifest or policy versions. Additive server responses should remain forward compatible where the worker can safely ignore new values.
