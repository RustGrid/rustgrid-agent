# rustgrid-agent

`rustgrid-agent` claims RustGrid tickets, runs Codex in a local Git repository, verifies the result, and publishes it as a GitHub pull request. Every major action is written back to the RustGrid agent run for an auditable, demo-friendly timeline.

## Install

Rust 1.85 or newer is recommended.

```sh
cargo install --path .
rustgrid-agent --help
```

Copy [`.rustgrid-agent.example.json`](.rustgrid-agent.example.json) to `.rustgrid-agent.json`, edit it, and commit it with the repository:

```json
{
  "project_key": "RG",
  "repo": { "owner": "RustGrid", "name": "agent-runner-CLI" },
  "default_base_branch": "main",
  "quality_gate_command": "cargo test",
  "codex_command": "codex exec --full-auto -"
}
```

Exactly one of `project_id` and `project_key` must be present. `codex_command` may be overridden by the environment.

Set credentials in the environment; secrets never belong in the JSON config:

```sh
export RUSTGRID_API_URL=https://app.rustgrid.com/api/v1
export RUSTGRID_API_KEY=rg_...
export GITHUB_TOKEN=github_pat_...
export CODEX_COMMAND='codex exec --full-auto -' # optional
```

`GITHUB_TOKEN` needs permission to push branches and create pull requests. HTTPS pushes receive it through child-process-only Git configuration; SSH remotes continue to use normal SSH configuration. The token is never placed in Git command arguments or remote URLs.

## Commands

```sh
rustgrid-agent register
rustgrid-agent status
rustgrid-agent run RG-123
rustgrid-agent watch
```

Useful options:

```sh
rustgrid-agent run RG-123 --allow-dirty
rustgrid-agent watch --interval 30
rustgrid-agent watch --once
rustgrid-agent --config path/to/config.json status
```

`watch` heartbeats the worker, polls for the next ticket, and processes tickets serially until Ctrl-C. `--once` makes one poll and is convenient for cron jobs and smoke tests.

## Run lifecycle

For each ticket the runner:

1. Registers and heartbeats a worker, fetches and claims the ticket, and creates an agent run.
2. Checks repository safety and creates `agent/<ticket-key>-<slug>` from the configured base branch.
3. Builds a prompt from the title, description, comments, custom fields, previous gate failures, and root `AGENTS.md`/`README.md` files.
4. Sends that prompt to Codex on stdin and streams Codex output to the terminal.
5. Runs the quality gate directly, without shell evaluation, and reports its output and passed/failed state.
6. Fails if Codex produced no committable changes; otherwise commits only agent-created paths.
7. Pushes the branch, opens a GitHub pull request, attaches its URL to RustGrid, and completes the run.

Failures are printed with their full error chain. Once a RustGrid run exists, failures also append a failed step and mark the run failed. Quality-gate output sent to RustGrid is capped at 16 KB.

## Safety

- A dirty working tree is rejected by default.
- With `--allow-dirty`, every path that was already dirty is excluded from the agent commit. Codex edits to those paths are intentionally not committed.
- Only paths reported by Git inside the discovered repository root are staged. The runner never uses `git add .`.
- Existing branches are never overwritten.
- Codex and quality-gate command strings are parsed into executable arguments and run directly. Shell operators, substitutions, and redirections are not evaluated.
- Secrets are not logged, put in prompts, embedded in Git URLs, or written to disk.
- API errors include the HTTP status, bounded response body, and RustGrid request ID when available.

The runner leaves the agent branch and worktree in place after a failure so the error is visible and recoverable. It does not reset or delete user work.

## RustGrid agent API contract

The public RustGrid base API is `/api/v1`. Agent endpoints are isolated in `src/api.rs` because the authenticated agent API can evolve independently. This version uses:

| Action | Method and path |
| --- | --- |
| Register worker | `POST /agent-workers/register` |
| Heartbeat | `POST /agent-workers/{id}/heartbeat` |
| Fetch ticket context | `GET /tickets/{id}?include=comments,custom_fields,quality_gate_failures` |
| Claim ticket | `POST /tickets/{id}/claim` |
| Next queued ticket | `GET /agent-tickets/next` |
| Create/update run | `POST /agent-runs`, `PATCH /agent-runs/{id}` |
| Append step | `POST /agent-runs/{id}/steps` |
| Report gate | `POST /agent-runs/{id}/quality-gates` |
| Attach PR | `POST /agent-runs/{id}/attachments` |

Responses may be direct objects or wrapped in `data`, `ticket`, `worker`, `run`, or `agent_run`. Create/claim requests use idempotency keys. All API requests use bearer authentication.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```
