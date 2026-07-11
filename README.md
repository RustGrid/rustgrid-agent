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
8. Attaches the pull request, marks the ticket `done`, and completes the RustGrid run. Terminal failures and explicit requests for human intervention mark the ticket `blocked`.

## Requirements

- macOS or Linux with Git installed.
- Access to the GitHub repository named in the agent configuration.
- A RustGrid API key with the permissions listed in [Credentials](#credentials).
- A GitHub token that can push branches and create pull requests.
- Codex CLI installed, authenticated, and available as `codex`, unless `codex_command` or `CODEX_COMMAND` points to another compatible command.
- A Git author name and email configured for the commit the agent creates.
- Rust 1.85 or newer when installing from source. A Homebrew installation does not require a separate Rust toolchain at runtime.

The agent must be run from inside the Git repository it will modify. Its configured base branch must already exist locally, and the repository must have an `origin` remote for the configured GitHub repository.

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
  "quality_gate_command": "cargo test",
  "codex_command": "codex exec --full-auto --json -"
}
```

Commit this file with the repository so every agent uses the same project, base branch, and quality gate. Do not put secrets in it.

Exactly one of `project_id` and `project_key` is required:

- Use `project_key` for a human-readable RustGrid key such as `RG`. `watch` resolves it to the project ID through the RustGrid API.
- Use `project_id` when the project UUID is already known.

### 2. Set credentials

Export credentials in the shell or inject them through the process manager that starts the agent:

```sh
export RUSTGRID_API_KEY=rg_...
export GITHUB_TOKEN=github_pat_...
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
| `repo.owner` | Yes | GitHub organization or account that owns the target repository. |
| `repo.name` | Yes | GitHub repository name. |
| `default_base_branch` | No | Local branch from which agent branches are created and the base used for pull requests. Defaults to `main`. |
| `quality_gate_command` | Yes | Command run after Codex finishes, for example `cargo test` or `npm test`. |
| `codex_command` | No | Command that accepts the generated prompt on stdin. Defaults to `codex exec --full-auto --json -`. The runner adds `--json` automatically when the executable is `codex`. |
| `heartbeat_interval_seconds` | No | Worker heartbeat and run-lease renewal interval. Defaults to 15 seconds; allowed range is 5–300. |
| `lease_seconds` | No | Duration requested for each run lease. Defaults to 900 seconds; must exceed three heartbeat intervals. |

Unknown JSON fields and empty required values are rejected. Command strings support quoted arguments, but they are parsed into an executable and arguments rather than evaluated by a shell. Shell operators, substitutions, environment expansion, pipes, and redirections therefore do not work. Put multi-step logic in a checked-in script and configure that script as the command instead.

`CODEX_COMMAND` overrides `codex_command`, which is useful for local experiments without changing the committed configuration:

```sh
export CODEX_COMMAND='codex exec --full-auto --json -'
```

## Credentials

| Variable | Required | Purpose |
| --- | --- | --- |
| `RUSTGRID_API_KEY` | Yes | Authenticates RustGrid API requests. |
| `GITHUB_TOKEN` | Yes | Pushes the generated branch and creates its pull request. |
| `RUSTGRID_API_URL` | No | Overrides `https://app.rustgrid.com/api/v1`. |
| `CODEX_COMMAND` | No | Overrides the configured Codex command. |

The RustGrid API key needs these permissions:

- `tickets:read`, `tickets:update`, `comments:read`, and `comments:create`
- `agents:workers:register` and `agents:workers:heartbeat`
- `agents:runs:claim` and `agents:runs:update`
- `agents:steps:create`
- `agents:links:create`
- `agents:quality_gates:read` and `agents:quality_gates:create`
- `projects:read` when `watch` resolves a configured `project_key`

`GITHUB_TOKEN` needs permission to push branches and create pull requests in the configured repository. With a fine-grained personal access token, grant repository contents read/write and pull requests read/write. Organization policy or branch protection may require additional approval.

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
rustgrid-agent run <ticket-uuid> --allow-dirty
```

Fetches and processes one ticket. The identifier is the RustGrid ticket UUID, not a key such as `RG-1`.

By default, a dirty worktree stops the run before the ticket is claimed. `--allow-dirty` records all paths that were already dirty and excludes them from the agent commit. If Codex edits one of those paths, that path remains uncommitted.

### `watch`

```sh
rustgrid-agent watch
rustgrid-agent watch --interval 30
rustgrid-agent watch --once
rustgrid-agent watch --allow-dirty
```

Registers one worker, heartbeats it, and processes queued tickets serially. `--interval` controls the delay in seconds after each poll and defaults to 15. `--once` performs one poll and exits, which is useful for schedulers and smoke tests. A failed ticket is reported and watch mode continues to the next poll.

Run only one watcher per working copy. Each successful ticket changes the current branch, and existing generated branch names are never overwritten.

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

Process managers should restart `serve` after an unexpected exit and send it
SIGINT for graceful shutdown.

## Run lifecycle and recovery

A successful run creates a branch, commit, pull request, RustGrid external link, individual agent-feedback comments, and an auditable sequence of run steps. The ticket moves to `in_progress` after it is claimed and to `done` only after the pull request is attached. Quality-gate output sent to RustGrid is capped at 16 KB.

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
- **Working tree is dirty:** commit or stash the changes, or deliberately use `--allow-dirty`.
- **Base branch is missing:** fetch it and create or switch to the locally configured `default_base_branch` before running the agent.
- **Codex cannot start:** confirm the Codex CLI is installed, authenticated, and available on `PATH`; then inspect `rustgrid-agent status`.
- **Quality gate fails:** run the exact configured command locally. Shell syntax is not interpreted.
- **Push or pull-request creation fails:** verify the `origin` remote, repository fields, token permissions, organization policy, and base branch.
- **No committable changes:** Codex either changed nothing or changed only paths that were dirty before an `--allow-dirty` run.

## Safety model

- A dirty worktree is rejected by default.
- With `--allow-dirty`, paths that were already dirty are excluded from the agent commit.
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

The [release workflow](.github/workflows/release.yml) rejects tags that do not match the Cargo package version. It runs formatting, lint, and test checks; packages the locked crate; calculates its SHA-256 checksum; generates a versioned Homebrew formula from [`packaging/homebrew/rustgrid-cli.rb.in`](packaging/homebrew/rustgrid-cli.rb.in); and creates the GitHub release. The release contains both of these assets:

- `rustgrid-agent-X.Y.Z.crate`, the immutable source archive
- `rustgrid-cli.rb`, the formula with the release URL and checksum filled in

The release URL will have this form:

```text
https://github.com/RustGrid/rustgrid-agent/releases/download/vX.Y.Z/rustgrid-agent-X.Y.Z.crate
```

### 2. Create the formula

Download `rustgrid-cli.rb` from the GitHub release and add it as `Formula/rustgrid-cli.rb` in a public `RustGrid/homebrew-tap` repository. The generated formula has this shape:

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

The release workflow replaces both `X.Y.Z` values and the checksum. The formula is named `rustgrid-cli` because that is the requested Homebrew package name, while Cargo installs the existing `rustgrid-agent` executable.

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

Rust 1.85 or newer is recommended.

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

## License

Licensed under the [MIT License](LICENSE).
