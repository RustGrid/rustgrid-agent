# Claude Code runs

The worker runs either Codex or Claude Code. RustGrid chooses the agent per run
from the model a user selects; the worker never picks one itself. Claude Code
uses the same lifecycle as Codex: the same prompt contract, worker-owned
gates, focused-validation evidence, completion checks, architecture
escalations, budgets, recovery, and telemetry.

## How a run selects Claude Code

1. The RustGrid operator lists Claude models in `AGENT_CLAUDE_MODELS_JSON`.
   The catalog is empty by default, so Claude Code is opt-in.
2. A user selects one of those models for a run.
3. RustGrid signs a version 2 execution policy whose `agent` block names
   `provider: "claude"` and the `claude` command with `--model <id>`. Codex
   models keep producing the unchanged version 1 policy.
4. The worker validates the policy and runs Claude Code in the run's sandbox.

The policy schema is in [the worker run contract](worker-api-contract.md#agent-providers-and-policy-versions).

## Enable Claude Code on a worker

Claude Code needs its own sandbox template, because Docker Sandboxes builds a
separate image and credential proxy for each agent.

1. Store an Anthropic API key in the Docker credential proxy, as the OS
   account that runs the worker. The key never enters the sandbox:

   ```sh
   printf '%s' "$ANTHROPIC_API_KEY" | sbx secret set -g anthropic
   ```

2. Choose a Docker Sandboxes Claude Code template release and pin it by digest,
   for example a `docker.io/docker/sandbox-templates:claude-code-docker-<release>`
   tag resolved to its `@sha256:` digest. Mutable tags are rejected in
   production.

3. Read the Claude Code version embedded in that template:

   ```sh
   sbx create --name claude-pin-check --template '<template>@sha256:<digest>' claude "$PWD"
   sbx exec claude-pin-check claude --version   # for example: 2.1.246 (Claude Code)
   sbx rm --force claude-pin-check
   ```

4. Add both pins to the executor configuration and restart the worker:

   ```json
   "executor": {
     "kind": "docker_sandbox",
     "claude_template": "docker.io/docker/sandbox-templates@sha256:<digest>",
     "claude_version": "2.1.246"
   }
   ```

`claude_template` and `claude_version` are set together or not at all. Before
every Claude run, the worker checks that `claude --version` in the sandbox
prints exactly `<claude_version> (Claude Code)`, and it never installs or
upgrades Claude Code while a run starts. Update the template digest and the
version together. A worker without the pins stops Claude runs for human
intervention and still runs Codex.

## Execution semantics

| Concern | Codex | Claude Code |
| --- | --- | --- |
| Command | `codex exec --json ...` | `claude --print --output-format stream-json --verbose ...` |
| Prompt | stdin | stdin |
| Ephemeral session | `--ephemeral` | `--no-session-persistence` |
| Docker Sandbox | `--sandbox danger-full-access` | `--permission-mode bypassPermissions` |
| Local executor | `--sandbox workspace-write` | `--permission-mode acceptEdits` plus the Claude Code OS sandbox with writes confined to the workspace and no unsandboxed fallback |
| Image attachments | `--image <path>` | The prompt lists the preview path; Claude reads it with its file tool |
| Usage | `turn.completed` usage | The final `result` usage, with cache writes and reads folded into input tokens |
| Token report provider | `codex` | `claude` |

The worker translates each Claude Code `stream-json` message into its canonical
agent events: `Bash` tool calls become command executions (failing results keep
their reported exit code), file edits become file changes, and assistant text
becomes agent messages. Running a worker-owned full gate is intercepted the
same way for both agents. Only a focused validation command that succeeds
counts as validation evidence. Thinking-only messages, token estimates, and
rate-limit notices do not reset the idle watchdog.

The worker removes `ANTHROPIC_*` and `CLAUDE_CODE_*` variables from every child
process, as it removes `OPENAI_*` and `CODEX_*`.

## Limitations

- GitHub-hosted execution supports only Codex policies; RustGrid rejects a
  Claude model for that provider.
- Workers released before policy version 2 reject Claude runs instead of
  running Codex. Upgrade every worker in a tenant before enabling Claude models.
- The legacy container and Kubernetes examples in [deploy](../deploy/README.md)
  use a read-only root filesystem verified for Codex authentication state only.
  Run Claude Code through the Docker Sandbox executor.
