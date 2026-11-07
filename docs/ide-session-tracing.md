# Tracing Claude Code IDE sessions

`rustgrid-agent trace` records what Claude Code does in a person's IDE on the
RustGrid ticket they are working on: the same trace an agent run leaves, so
the ticket shows what changed and how.

It needs RustGrid with interactive runs (`POST /api/v1/agent-runs/interactive`)
and `rustgrid-agent` 1.6.0 or later, signed in with `rustgrid-agent login`.

## What is recorded

**On the run's timeline:**
- every prompt;
- Claude's replies;
- activity steps ("Searching relevant source files", "Updating implementation",
  "Running focused validation");
- each command, with its exit code and the first 1,500 characters of its output;
- edited file paths;
- the tools Claude used and what they acted on;
- each model call's token usage, priced by RustGrid;
- pull requests Claude Code opened, which are also attached to the ticket.

**On the ticket**, when the session ends, one summary comment:
- prompt count, duration, model and cost;
- branch, commits, files changed and the pull request.

**Never sent:** file contents, full command output, thinking, images, and
Claude Code's own system messages.

Text is redacted on the developer's machine before upload, and again by
RustGrid. Redaction covers:
- RustGrid, OpenAI, Anthropic, GitHub, DigitalOcean, Stripe and Slack tokens;
- AWS keys;
- private keys;
- bearer tokens and JWTs;
- passwords in connection URLs;
- `.env`-style secret assignments.

The cases live in `src/trace/redaction_cases.json`, an identical copy of
RustGrid's.

## How it works

Claude Code runs `rustgrid-agent trace hook <event>` for `SessionStart`,
`UserPromptSubmit`, `PostToolUse`, `Stop` and `SessionEnd`.

- **The hook stays fast.** It records the session in `~/.rustgrid/trace`
  (private to the user) and returns at once. It starts a detached
  `rustgrid-agent trace flush`, which reads the session transcript from where it
  last stopped and uploads what is new.
- **Offline is fine.** If RustGrid is unreachable, nothing is lost: the
  transcript offset is only saved once its lines are uploaded, and the next hook
  retries.
- **Linking a ticket:**
  - **From the branch name:** a branch named after a ticket links the session
    (`agent/add-354-fix-login`, `rustgrid/ADD-12`).
  - **By hand:** type `/rustgrid-link ADD-354`. The hook validates the key with
    RustGrid, confirms the ticket title, and does not send that prompt to Claude.
  - **Unlinked:** until a session is linked, the hook blocks prompts with
    instructions. It also blocks prompts until the computer is signed in.
  - **Relinking:** linking another ticket mid-session ends the first ticket's
    run, with its summary comment, and traces the rest of the session on the new
    ticket.
- **Ending a session:** when it ends, the run closes and RustGrid posts the
  summary. If Claude Code never reports the end, RustGrid closes the run after
  two hours without activity.

## Rolling it out so it cannot be turned off

Install hooks through Claude Code **managed settings**, which users cannot
override, using your device management tool (Jamf, Intune, …):

1. Install `rustgrid-agent` 1.6.0+ at `/usr/local/bin/rustgrid-agent`.
2. Deploy [`packaging/claude-code/managed-settings.json`](../packaging/claude-code/managed-settings.json)
   to:
   - **macOS:** `/Library/Application Support/ClaudeCode/managed-settings.json`
   - **Linux and WSL:** `/etc/claude-code/managed-settings.json`

   If you already ship managed settings, merge the `hooks` block into them. To
   stop people adding their own hooks as well, also set
   `"allowManagedHooksOnly": true`.
3. Deploy [`packaging/claude-code/commands/rustgrid-link.md`](../packaging/claude-code/commands/rustgrid-link.md)
   to each user's `~/.claude/commands/` so `/rustgrid-link` autocompletes. The
   hook handles the command either way.
4. Each developer runs `rustgrid-agent login` once.

Before rolling out, confirm on a test machine that a user's own `disableAllHooks`
or project settings do not disable the managed hooks with your Claude Code
version.

## Troubleshooting

- **Problem log:** tracing problems never block a prompt. They are printed by
  the hook as `[rustgrid trace] …`, which Claude Code shows in its hook output.
- **Session state:** each session's state is in `~/.rustgrid/trace/<session-id>.json`.
  Deleting it makes the next flush re-upload the session; RustGrid ignores
  events it already has.
