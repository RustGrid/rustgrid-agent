# rustgrid-agent

`rustgrid-agent` turns queued RustGrid tickets into ready-to-review GitHub pull requests. It claims a ticket, gives the ticket and repository instructions to Codex, runs the repository's quality gate, commits the generated changes, opens a pull request, and records every major action in RustGrid.

The Cargo package, executable, GitHub repository, and Homebrew formula all use the name `rustgrid-agent`.

## What it does

For each ticket, the agent:

1. Registers and heartbeats a worker with RustGrid.
2. Fetches and claims the ticket and creates an agent run.
3. Checks the local Git repository and creates an `agent/<ticket-key>-<slug>` branch.
4. Builds a Codex prompt from the ticket title, description, comments, custom fields, previous quality-gate failures, and root `AGENTS.md` and `README.md` files.
5. Marks the ticket `in_progress`, runs Codex locally, and publishes each Codex agent update as one ticket comment.
6. Runs the configured quality gate without shell evaluation.
7. Commits only agent-created paths, pushes the branch, and opens a GitHub pull request.
8. Attaches the pull request, marks the ticket `awaiting_review`, and completes the RustGrid run. Terminal failures and explicit requests for human intervention mark the ticket `blocked`.

## Requirements

- macOS or Linux with Git installed.
- Access to the GitHub repository named in the agent configuration.
- A RustGrid API key with the permissions listed in [Credentials](#credentials).
- A RustGrid project linked to a GitHub repository through the RustGrid GitHub App.
- The server-selected Codex executable installed and authenticated on the worker host.
- A Git author name and email configured for the commit the agent creates.
- Rust 1.94 or newer when installing from source. A Homebrew installation does not require a separate Rust toolchain at runtime.

The agent creates an isolated clone for every run from the repository in the
RustGrid execution manifest. It does not need to start inside the target
repository.

## Project documentation

- [Architecture and trust boundaries](docs/architecture.md)
- [Compatibility policy](docs/compatibility.md)
- [Production operations](docs/operations.md)
- [Container deployment](deploy/README.md)
- [Known limitations](docs/known-limitations.md)
- [Telemetry and data handling](docs/telemetry.md)
- [Security policy](SECURITY.md)
- [Contributing](CONTRIBUTING.md) and [support](SUPPORT.md)
- [Roadmap](ROADMAP.md) and [changelog](CHANGELOG.md)

The binary does not create its own container or VM boundary. Production-safe
examples run `watch --once` in a fresh container/pod and destroy that boundary
after at most one claimed run.

## Install

### Homebrew

Once `rustgrid-agent` has been accepted into Homebrew/core, install it with:

```sh
brew install rustgrid-agent
rustgrid-agent --version
```

Until then, a public RustGrid tap can provide it with:

```sh
brew install RustGrid/tap/rustgrid-agent
rustgrid-agent --version
```

Use the fully qualified tap name until the formula is available from Homebrew/core. Maintainers can find the complete publication process in [Publishing with Homebrew](#publishing-with-homebrew).

### From source

Clone this repository and install the binary with Cargo:

```sh
git clone https://github.com/RustGrid/rustgrid-agent.git
cd rustgrid-agent
cargo install --locked --path .
rustgrid-agent --version
```

Cargo installs the executable into `~/.cargo/bin` by default. Add that directory to `PATH` if the command is not found.

## Quick start

### 1. Configure a target repository

In the repository that the agent will work on, copy [`.rustgrid-agent.example.json`](.rustgrid-agent.example.json) to `.rustgrid-agent.json` and update it:

```json
{
  "project_key": "RG",
  "repo": {
    "owner": "RustGrid",
    "name": "rustgrid-agent"
  },
  "default_base_branch": "main",
  "max_concurrency": 1
}
```

Commit this file with the repository so every agent uses the same project and local capacity. Do not put secrets in it.

Exactly one of `project_id` and `project_key` is required:

- Use `project_key` for a human-readable RustGrid key such as `RG`. `watch` resolves it to the project ID through the RustGrid API.
- Use `project_id` when the project UUID is already known.

### 2. Set credentials

Export credentials in the shell or inject them through the process manager that starts the agent:

```sh
export RUSTGRID_API_KEY=rg_...
```

The production RustGrid API URL is used by default. Set `RUSTGRID_API_URL` only when targeting a different deployment:

```sh
export RUSTGRID_API_URL=https://app.rustgrid.com/api/v1
```

### 3. Validate the setup

Run this command from the target Git repository:

```sh
rustgrid-agent status
rustgrid-agent register
```

`status` validates the configuration and command strings, locates the repository, reports whether the credentials are present, and shows whether the worktree is clean. It never prints credential values. `register` registers the machine as a RustGrid worker and sends an initial heartbeat.

Use `rustgrid-agent status --json` for machine-readable readiness data. It exits
non-zero unless credentials exist, per-run isolation is declared,
`max_concurrency` is 1, and RustGrid authentication plus project resolution
succeeds.
Interactive lifecycle output uses color when attached to a terminal; set the
standard `NO_COLOR` environment variable to disable it. Set
`RUSTGRID_AGENT_LOG=json` for newline-delimited structured lifecycle events.

### 4. Process tickets

Run a specific ticket by its RustGrid UUID:

```sh
rustgrid-agent run <ticket-uuid>
```

Or continuously claim queued tickets for the configured project:

```sh
rustgrid-agent watch
```

For a long-lived production worker, use daemon mode. It keeps worker heartbeats
and active-run leases alive independently while Codex and quality gates run:

```sh
rustgrid-agent serve
```

Stop watch mode with Ctrl-C. It finishes the current blocking operation before stopping.

## Configuration reference

The default configuration path is `.rustgrid-agent.json` in the current directory. Use a different path with the global `--config` option:

```sh
rustgrid-agent --config path/to/agent.json status
```

| Field | Required | Description |
| --- | --- | --- |
| `project_key` | One project field | Human-readable RustGrid project key. Mutually exclusive with `project_id`. |
| `project_id` | One project field | RustGrid project UUID. Mutually exclusive with `project_key`. |
| `repo.owner` | No | Deprecated bootstrap hint. The claimed execution manifest is authoritative. |
| `repo.name` | No | Deprecated bootstrap hint. The claimed execution manifest is authoritative. |
| `default_base_branch` | No | Bootstrap value used before a run is claimed. The execution manifest is authoritative for claimed runs. |
| `quality_gate_command` | No | Deprecated compatibility field; ignored for claimed runs. |
| `codex_command` | No | Deprecated compatibility field; ignored for claimed runs. |
| `heartbeat_interval_seconds` | No | Worker heartbeat and run-lease renewal interval. Defaults to 15 seconds; allowed range is 5–300. |
| `max_concurrency` | No | Simultaneous run capacity advertised to RustGrid. Defaults to 1; allowed range is 1–100 for non-production experimentation. `serve` currently requires exactly 1 because this binary does not create a separate runtime boundary for concurrent runs. |
| `lease_seconds` | No | Duration requested for each run lease. Defaults to 900 seconds; must exceed three heartbeat intervals. |
| `workspace_root` | No | Durable parent directory for isolated run workspaces. Defaults to the OS temporary directory. |
| `command_timeout_seconds` | No | Deprecated compatibility field; the manifest owns command and gate timeouts. |
| `run_timeout_seconds` | No | Deprecated compatibility field; the manifest owns total run timeout. |
| `failed_workspace_retention_hours` | No | Retention for failed/interrupted workspaces before startup cleanup. Defaults to 72. |
| `max_command_output_bytes` | No | Combined in-memory output budget for captured commands. Defaults to 8 MiB. |
| `max_workspace_bytes` | No | Maximum allowed run-workspace size. Defaults to 5 GiB. |
| `max_child_memory_bytes` | No | Per-child address-space ceiling on Unix. Defaults to 8 GiB. |
| `max_child_file_bytes` | No | Largest file a child may create on Unix. Defaults to 1 GiB. |
| `max_child_open_files` | No | Per-child open-file ceiling on Unix. Defaults to 1024. |

Unknown JSON fields and empty required values are rejected. Command strings support quoted arguments, but they are parsed into an executable and arguments rather than evaluated by a shell. Shell operators, substitutions, environment expansion, pipes, and redirections therefore do not work. Put multi-step logic in a checked-in script and configure that script as the command instead.

Codex commands, quality gates, timeouts, and sandbox behavior cannot be overridden locally; RustGrid snapshots them into each execution manifest.

## Credentials

| Variable | Required | Purpose |
| --- | --- | --- |
| `RUSTGRID_API_KEY` | Yes | Authenticates RustGrid API requests. |
| `RUSTGRID_API_URL` | No | Overrides `https://app.rustgrid.com/api/v1`. |
| `CODEX_COMMAND` | No | Overrides the configured Codex command. |
| `RUSTGRID_AGENT_ISOLATION` | Production | Must equal `per_run`; asserts that the deployment runtime isolates every run. `serve` fails closed without it. |

The RustGrid API key needs these permissions:

- `tickets:read`, `tickets:update`, `comments:read`, and `comments:create`
- `agents:workers:register` and `agents:workers:heartbeat`
- `agents:runs:claim` and `agents:runs:update`
- `agents:steps:create`
- `agents:links:create`
- `agents:quality_gates:read` and `agents:quality_gates:create`
- `projects:read` when `watch` resolves a configured `project_key`

GitHub credentials are issued by RustGrid for the GitHub App installation in
the claimed execution manifest. Tokens are held only in memory, refreshed before
expiry, and scoped by the server to the claimed run and repository.

For HTTPS remotes, the token is passed to the child `git push` process through temporary Git configuration. It is not placed in command arguments or remote URLs. SSH remotes continue to use the normal SSH configuration. Credential values are never written to the agent configuration, logs, or Codex prompt.

## Commands

### `register`

```sh
rustgrid-agent register
```

Registers the current machine as a worker and immediately heartbeats it. Use this to verify RustGrid connectivity before starting a run.

### `status`

```sh
rustgrid-agent status
```

Shows the resolved configuration path, API URL, project, repository root, base branch, commands, credential presence, and worktree state. It exits with an error when either required credential is missing.

### `run`

```sh
rustgrid-agent run <ticket-uuid>
```

Fetches and processes one ticket. The identifier is the RustGrid ticket UUID, not a key such as `RG-1`.

Every run uses a clean isolated workspace. Existing files in the directory from
which the agent was started are never staged or committed.

### `watch`

```sh
rustgrid-agent watch
rustgrid-agent watch --interval 30
rustgrid-agent watch --once
```

Registers one worker, heartbeats it, and processes queued tickets serially. `--interval` controls the delay in seconds after each poll and defaults to 15. `--once` performs one poll and exits, which is useful for schedulers and smoke tests. A failed ticket is reported and watch mode continues to the next poll.

Multiple worker processes must use distinct worker credentials and workspace
roots. A single `serve` process claims and executes one run at a time.

### `serve`

```sh
rustgrid-agent serve
rustgrid-agent serve --interval 30
```

`serve` is the production-oriented long-running worker entrypoint. It uses the
same atomic queue claim as `watch`, and every active run gets an independent
supervisor that:

- heartbeats the worker as `busy`;
- extends the run lease before it can expire;
- continues operating while Codex or a quality gate is blocking;
- reports degraded RustGrid connectivity without discarding local work; and
- stops before the terminal run update to preserve optimistic-concurrency correctness.

Ctrl-C also reaches an active Codex process through the cancellation token. The
child is terminated, the run becomes `cancelled`, and the ticket returns to
`todo` so another attempt can claim it safely.

Process managers should restart `serve` after an unexpected exit. SIGTERM
drains: it stops new claims and waits for active runs to finish. SIGINT cancels
active child process groups and exits safely.
At startup, `serve` queries RustGrid for runs already assigned to this worker
and resumes them before claiming new work. Each run owns its cancellation token,
so a lease loss, timeout, or cancellation cannot stop unrelated concurrent runs.

## Run lifecycle and recovery

A successful run creates a branch, commit, pull request, RustGrid external link, individual agent-feedback comments, and an auditable sequence of run steps. The ticket moves to `in_progress` after it is claimed and to `awaiting_review` after the pull request is attached. Quality-gate output sent to RustGrid is capped at 16 KB.

Before production promotion, complete the credentialed failure and recovery
matrix in [`docs/staging-certification.md`](docs/staging-certification.md).

Immediately after claim, the worker retrieves
`GET /agent-runs/{run_id}/manifest`. It rejects unknown manifest
versions, mismatched run/ticket identities, incomplete repository data, and a
local `origin` that does not match the claimed repository. GitHub tokens come
from `POST /agent-runs/{run_id}/github-token` and are refreshed before expiry.
The worker caches a valid token in memory until its refresh window, derives the
API origin from the manifest for GitHub Enterprise Server, paginates check runs,
and verifies the remote branch commit after every push.
Manifest version 2 also owns the Codex command, structured quality gates,
timeouts, environment allowlist, and sandbox policy. Local command settings are
accepted only for configuration-file compatibility and are not used to execute
a claimed run.

Each structured lifecycle event is published to
`POST /agent-runs/{run_id}/events` with a stable idempotency key. If the
response is ambiguous, the agent replays events after its last accepted server
sequence and retries once only when the event was not already committed.
Accepted sequences are persisted in the recovery journal.

Run steps carry a versioned lifecycle event envelope with a per-run sequence,
timestamp, phase, severity, event type, message, and structured data. Current
phases are `claimed`, `preparing`, `executing`, `verifying`, `publishing`,
`awaiting_review`, and terminal outcomes. Consumers should order the live
timeline by `sequence` and treat unknown fields as forward-compatible.

The worker also atomically checkpoints recovery state after phase and step
changes. Journals live at `.git/rustgrid-agent/runs/<run-id>.json`; because they
are inside Git metadata, they cannot become part of an agent commit. They record
the last phase and sequence plus any created branch, commit, and pull request.

Codex is instructed to emit concise progress updates. With Codex JSONL output, each completed `agent_message` becomes exactly one comment; reasoning summaries and command execution events are ignored. Compatible custom commands may emit plain text, where each non-empty output line becomes one comment.

When Codex cannot proceed without a decision, credential, permission, or external-system change, it emits `RUSTGRID_AGENT_STATUS: BLOCKED` and a specific `HUMAN_ACTION_REQUIRED`. The runner stops safely, adds a blocked comment, marks the ticket `blocked`, and fails the agent run. Other terminal automation failures use the same blocked handoff because a human must resolve the failed run before it can continue.

If Codex, the quality gate, Git push, or pull-request creation fails, the agent reports the error to RustGrid when a run exists and exits without resetting or deleting the work. The generated branch and worktree remain available for inspection. Resolve the underlying problem before retrying. If the same generated branch already exists, rename or remove it only after preserving any useful work; the agent will not overwrite it.

Common checks:

- **Configuration cannot be read:** run from the directory containing `.rustgrid-agent.json`, or pass `--config`.
- **Base branch is missing:** fetch it and create or switch to the locally configured `default_base_branch` before running the agent.
- **Codex cannot start:** confirm the Codex CLI is installed, authenticated, and available on `PATH`; then inspect `rustgrid-agent status`.
- **Quality gate fails:** run the exact configured command locally. Shell syntax is not interpreted.
- **Push or pull-request creation fails:** verify the `origin` remote, repository fields, token permissions, organization policy, and base branch.
- **Execution manifest fails:** verify that the project has an enabled GitHub App repository binding and that the run manifest matches the local `origin`.
- **Progress cursor conflict:** the agent automatically resynchronizes once; repeated conflicts stop the run to preserve ordered telemetry.
- **Lease ownership is lost:** local execution stops without publishing a terminal state because another worker or the control plane is authoritative.
- **No committable changes:** Codex did not change the isolated run workspace.

## Safety model

- A dirty worktree is rejected by default.
- Every run uses a repository clone dedicated to that run ID.
- Only new changed paths reported by Git inside the discovered repository root are staged. The runner never uses `git add .`.
- Existing local branches are never overwritten.
- Codex and quality-gate commands run directly without a shell.
- Secrets are not logged, embedded in Git URLs, written to disk, or included in prompts.
- API errors include the HTTP status, a bounded response body, and the RustGrid request ID when available.
- Failed runs leave the branch and worktree in place for recovery.

## Publishing with Homebrew

This section is for RustGrid maintainers. Homebrew distribution needs a versioned public release plus a formula. A tap can be published immediately under the RustGrid organization; the unqualified command `brew install rustgrid-agent` on a new machine requires acceptance into the central `Homebrew/homebrew-core` repository.

### 1. Create a release artifact

1. Choose a semantic version and update `version` in `Cargo.toml` and the root package entry in `Cargo.lock`.
2. Run the development checks listed below.
3. Commit the release change, create a matching `vX.Y.Z` tag, and push the tag.

The [release workflow](.github/workflows/release.yml) rejects tags that do not match the Cargo package version. It runs formatting, lint, and test checks; packages the locked crate; calculates its SHA-256 checksum; generates a versioned Homebrew formula from [`packaging/homebrew/rustgrid-agent.rb.in`](packaging/homebrew/rustgrid-agent.rb.in); generates an SPDX JSON SBOM; creates a GitHub artifact attestation binding the package and SBOM; and creates the GitHub release. The release contains these assets:

- `rustgrid-agent-X.Y.Z.crate`, the immutable source archive
- `rustgrid-agent-X.Y.Z.crate.sha256`, its SHA-256 checksum
- native Linux and macOS binary archives with SHA-256 checksums and attestations
- `rustgrid-agent.spdx.json`, the SPDX JSON software bill of materials
- `rustgrid-agent.rb`, the formula with the release URL and checksum filled in

The protected release environment also publishes an attested container image to
`ghcr.io/rustgrid/rustgrid-agent:vX.Y.Z`. Production deployments must pin its
digest rather than the mutable tag.

The release URL will have this form:

```text
https://github.com/RustGrid/rustgrid-agent/releases/download/vX.Y.Z/rustgrid-agent-X.Y.Z.crate
```

### 2. Create the formula

Download `rustgrid-agent.rb` from the GitHub release and add it as `Formula/rustgrid-agent.rb` in a public `RustGrid/homebrew-tap` repository. The generated formula has this shape:

```ruby
class RustgridAgent < Formula
  desc "Run Codex against RustGrid tickets and publish GitHub pull requests"
  homepage "https://github.com/RustGrid/rustgrid-agent"
  url "https://github.com/RustGrid/rustgrid-agent/releases/download/vX.Y.Z/rustgrid-agent-X.Y.Z.crate"
  sha256 "REPLACE_WITH_RELEASE_ARCHIVE_SHA256"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: ".")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/rustgrid-agent --version")
  end
end
```

The release workflow replaces both `X.Y.Z` values and the checksum. The formula and executable are both named `rustgrid-agent`.

Validate the formula in a clean Homebrew environment:

```sh
brew audit --strict --online RustGrid/tap/rustgrid-agent
brew install --build-from-source RustGrid/tap/rustgrid-agent
brew test RustGrid/tap/rustgrid-agent
rustgrid-agent --version
brew uninstall rustgrid-agent
```

Commit and push the formula to the default branch of the public tap. Users can then run:

```sh
brew install RustGrid/tap/rustgrid-agent
```

### 3. Enable `brew install rustgrid-agent`

For installation without a tap qualifier, submit `Formula/r/rustgrid-agent.rb` as a pull request to `Homebrew/homebrew-core`. Use the same stable release URL and checksum, follow the current [Homebrew formula requirements](https://docs.brew.sh/Acceptable-Formulae), and run the audit, install, and test checks requested by Homebrew's contribution guide. Core requires a stable, tagged, open-source project that builds on supported macOS and Linux versions; it also applies notability and third-party-use criteria. A public tap remains the supported route until the project qualifies.

Homebrew/core inclusion is reviewed by Homebrew maintainers and is not guaranteed. Until it is accepted, document the tap-qualified command. Once accepted, a new user can install with exactly:

```sh
brew install rustgrid-agent
```

### 4. Publish future versions

For every release:

1. Update the Cargo version and push its matching version tag.
2. Download the generated formula from the new GitHub release.
3. Run `brew audit`, install from source, and `brew test`.
4. Submit the formula update to the tap or Homebrew/core, depending on where the formula lives.

Do not replace an asset for an existing version: its checksum would change and break reproducible installs.

## RustGrid agent API contract

The RustGrid base API is `/api/v1`. The endpoint and payload mappings in `src/api.rs` match the RustGrid backend contract.

| Action | Method and path |
| --- | --- |
| Register worker | `POST /agent-workers/register` |
| Heartbeat | `POST /agent-workers/{id}/heartbeat` |
| Fetch ticket context | `GET /tickets/{id}`, `/tickets/{id}/comments`, `/tickets/{id}/quality-gate-results` |
| Publish agent feedback | `POST /tickets/{id}/comments` |
| Update ticket progress | `PATCH /tickets/{id}` with `If-Match` |
| Claim ticket and create run | `POST /tickets/{id}/agent-runs/claim` |
| Claim next queued ticket | `POST /agent-runs/claim-next` |
| Update run | `PATCH /agent-runs/{id}` with `If-Match` |
| Append step | `POST /agent-runs/{id}/steps` |
| Report gate | `POST /tickets/{id}/quality-gate-results` |
| Attach PR | `POST /tickets/{id}/external-links` |

Create and claim requests use idempotency keys. Run updates use the backend's versioned ETag format. All API requests use bearer authentication.

## Development

Rust 1.94 or newer is required.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Pull requests and pushes to `main` run these gates on Linux and macOS, verify
the release package, and apply `cargo-deny` advisory, license, dependency, and
source policy. Dependabot maintains both Rust crates and pinned GitHub Actions.

## License

Licensed under the [MIT License](LICENSE).
