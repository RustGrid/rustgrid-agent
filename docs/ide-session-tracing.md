# Tracing Claude Code IDE sessions

`rustgrid-agent trace` records what Claude Code does in a person's IDE on the
RustGrid ticket they are working on: the same trace an agent run leaves, so
the ticket shows what changed and how.

It needs RustGrid with interactive runs (`POST /api/v1/agent-runs/interactive`)
and `rustgrid-agent` 1.6.1 or later.

## Set up

```sh
brew install RustGrid/tap/rustgrid-agent
rustgrid-agent trace install
```

`trace install`:
- writes the Claude Code hooks for the binary's real location into **managed
  settings**, which users cannot override. That file is shared by everyone on
  the computer, so `sudo` asks for your password for that one step;
- merges with any settings already there, adding or replacing only its own
  hooks;
- installs the `/rustgrid-link` command;
- signs the computer in to RustGrid if it isn't yet;
- runs `trace status` to check that it all works.

Restart Claude Code afterwards.

| Command | What it does |
|---|---|
| `rustgrid-agent trace status` | Shows whether sessions are traced, where the hooks are, whether RustGrid answers, which ticket the current branch names, and the last traced session. |
| `rustgrid-agent trace install --user` | Writes the hooks into your own `~/.claude/settings.json` instead, without sudo. Good for trying it out, but you can remove them. |
| `rustgrid-agent trace uninstall` | Removes exactly what `install` added. |

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
`UserPromptSubmit`, `PreToolUse` (file edits only), `PostToolUse`, `Stop` and
`SessionEnd`.

- **The hook stays fast.** It records the session in `~/.rustgrid/trace`
  (private to the user) and returns at once. It starts a detached
  `rustgrid-agent trace flush`, which reads the session transcript from where it
  last stopped and uploads what is new.
- **Offline is fine.** If RustGrid is unreachable, nothing is lost: the
  transcript offset is only saved once its lines are uploaded, and the next hook
  retries.
- **Linking a ticket:**
  - **Every new conversation starts unlinked.** A new Claude Code chat, or
    `/clear`, has no ticket, even on a branch named after one. Type
    `/rustgrid-link ADD-354`: the hook validates the key with RustGrid and
    links the session. `/clear` in a linked session ends that ticket's run,
    with its summary comment, at the point of the clear. A resumed (`--resume`,
    `--continue`) or compacted conversation keeps its link.
  - **Ticket branch:** linking also puts the repository on a branch
    named after the ticket (`agent/add-354-<title>`), so the session's work
    never lands on a branch belonging to something else. A branch that already
    names the ticket is kept, an existing `agent/add-354-<title>` branch is
    reused, and otherwise a new one is created from the current commit, with
    uncommitted changes carried over. If git refuses the switch (for example,
    local changes the other branch would overwrite), the session stays linked
    on its current branch and Claude tells you how to switch. RustGrid
    records the branch the session was on when it was linked.
  - **Branch prefix:** ticket branches start with `agent/` unless Git config
    sets `rustgrid.branchPrefix`. Set it per repository
    (`git config rustgrid.branchPrefix rustgrid` gives
    `rustgrid/add-354-<title>`) or for every repository with `--global`.
    Cloud runs use the same setting. Titles are cut at the last whole word
    within 48 characters.
  - **Ticket content:** `/rustgrid-link` also gives Claude the ticket's title,
    type, status, priority and description, so the session starts from the
    whole ticket. The ticket is written to
    `.git/rustgrid-agent/tickets/<KEY>/ticket.md` in the repository, which git
    never commits (`~/.rustgrid/tickets/<KEY>/` outside a repository). Its
    attachments download in the background to the `attachments` folder next
    to it: at most 50 files, 25 MB each and 100 MB in total. Files flagged by
    the virus scan are skipped, and each file is checked against its SHA-256
    before it appears. Claude gets each file's path and reads the ones it
    needs. If the ticket's content can't be loaded within the hook's time
    limit, the session is still linked and Claude says so. A download problem
    is kept in `attachments.error` in the same folder. Sessions linked from
    the branch name by older versions are still traced, but their
    content is not loaded.
  - **Architecture contract:** when the project defines one, the session
    works under it like any other run. RustGrid pins the run to the version
    current when the session starts, and the link writes it to
    `architecture.md` next to `ticket.md`, with the plan check Claude follows
    before changing code. Claude gets the Block and Ask guardrails and the
    protected paths inline. When Claude cannot stay inside the ticket's
    boundaries, it ends its reply with the escalation lines from the plan
    check, and the hook raises the escalation for an architect on the run. An
    escalation that could not be raised is kept in `architecture.error`.
  - **Protected paths:** before Claude edits a file (`Edit`, `Write`,
    `MultiEdit`, `NotebookEdit`), the `PreToolUse` hook checks it against the
    contract the link wrote. An edit to a `read_only` path is blocked. An edit
    to an `ask` path is blocked until an architect approves an escalation for
    its boundary; Claude is told to raise one. Run `/rustgrid-link` again after
    the decision to pick it up. The ticket's declared boundaries are not
    enforced here, because the person steers the session. The check reads only
    local files and lets the edit through on any problem. It does not see
    shell commands, so a `Bash` write is not blocked; the runner and review
    remain the authority. Installs from before this hook keep tracing, and
    `trace status` asks for `trace install` to add it.
  - **Unlinked:** until a session is linked, the hook blocks prompts with
    instructions. It also blocks prompts until the computer is signed in.
  - **Relinking:** linking another ticket mid-session ends the first ticket's
    run, with its summary comment, and traces the rest of the session on the new
    ticket.
- **Ending a session:** when it ends, the run closes and RustGrid posts the
  summary.
  - **Pull request:** a pull request is the session's result. When the turn
    that opened one finishes, the run closes with its summary, the pull request
    attached. A later prompt reopens the run, which closes again only when it
    opens another pull request.
  - **New conversation:** the IDE's **+** (new conversation) button starts a
    session without ending the previous one. When a session starts, the run of
    a session in the same folder closes once that session has waited 30
    seconds with no turn running. A session anywhere closes after 30 minutes
    without activity.
  - **Used again:** if a closed session gets another prompt (another tab, or a
    resumed conversation), its run reopens on the same ticket.
  - **Never reported:** RustGrid closes a run after two hours without activity.

## Rolling it out to a team

Have your device management tool (Jamf, Intune, …) run, as root:

```sh
brew install RustGrid/tap/rustgrid-agent   # or install the release binary
rustgrid-agent trace install --no-login
```

This writes the managed hooks for everyone on the computer. Each person signs
in once: Claude Code tells them to run `rustgrid-agent login` before their first
prompt.

To also stop people adding their own hooks, set `"allowManagedHooksOnly": true`
in the same managed settings file. The file is at
`/Library/Application Support/ClaudeCode/managed-settings.json` on macOS and
`/etc/claude-code/managed-settings.json` on Linux. The reference copy is
[`packaging/claude-code/managed-settings.json`](../packaging/claude-code/managed-settings.json).

Before relying on it, confirm on a test machine that a user's own
`disableAllHooks` does not disable the managed hooks with your Claude Code
version. `trace status` warns when a user has set it.

## Troubleshooting

- **Problem log:** tracing problems never block a prompt. They are printed by
  the hook as `[rustgrid trace] …`, which Claude Code shows in its hook output.
- **Status:** `rustgrid-agent trace status` is the first thing to run. It shows
  the last upload problem of the latest session.
- **Upload problems:** uploads run in the background, so their last problem is
  kept in `~/.rustgrid/trace/<session-id>.error` until an upload succeeds.
- **Signed in again:** a run belongs to the worker that started it. After
  `rustgrid-agent login` signs the computer in as a new worker, RustGrid
  refuses uploads to runs the earlier sign-in started. Telemetry it refuses is
  skipped, and the rest of the run still uploads. If RustGrid refuses the run's
  prompts and replies too, the session stops uploading to that run instead of
  retrying, and `trace status` says so. New sessions are traced normally.
- **Session state:** each session's state is in `~/.rustgrid/trace/<session-id>.json`.
  Deleting it makes the next flush re-upload the session; RustGrid ignores
  events it already has.
