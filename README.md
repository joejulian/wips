# WIPS

WIPS keeps interactive coding-agent work in progress ready to resume. It runs
agent sessions in a dedicated tmux server, records their durable provider
session IDs and layout in SQLite, and indexes lifecycle-hook messages for local
search.

An agent process exiting does not mean the work is finished. WIPS deliberately
keeps that pane and workflow open until you explicitly close it through WIPS.
That distinction makes normal exits, crashes, terminal disconnects, and machine
restarts recoverable without turning every interruption into a completed task.

WIPS currently targets Linux and WSL. Its process and terminal model is built
around tmux and Unix-style executables; native Windows and other platforms are
not supported yet.

## Requirements

- Rust 1.85 or newer and Cargo
- tmux
- At least one supported agent CLI installed and authenticated:
  [Codex CLI](https://developers.openai.com/codex/cli/) or Claude Code

SQLite is bundled into the binary, so a separate SQLite installation is not
required.

## Install and start

From a source checkout:

```console
cargo install --path .
wips
```

Running `wips` creates the local configuration and state on first use, starts or
restores the dedicated tmux session, and attaches the current terminal. Later
invocations attach to the same WIPS session.

Useful commands include:

```console
# Create a new tab with the default agent.
wips new

# Select a configured agent and working directory.
wips new --agent claude --cwd ./project

# Split an existing pane left/right or top/bottom.
wips new --split horizontal --target '%3'
wips new --split vertical --target '%3'

# Inspect open work, or include completed work.
wips list
wips list --all

# Search locally indexed prompts and responses.
wips search database migration

# Resume an exited agent in its existing pane.
wips resume --pane '%3'

# Reload WIPS runners after installing a new binary, keeping agents alive.
wips reload
wips reload --pane '%3'

# Give a tab a durable title (or omit the title to be prompted).
wips rename --window '@2' 'database migration'

# Paste and submit a message to a running Codex or Claude pane.
wips send --pane '%3' 'Review the failing tests and fix the root cause.'

# Adopt a previous agent session into a new tab or split by its provider
# session ID (a Claude UUID, or a Codex session ID). The session does not
# need to have been tracked by WIPS before.
wips new --agent claude --resume 3fa85f64-5717-4562-b3fc-2c963f66afa6
wips new --split horizontal --target '%3' --resume 3fa85f64-5717-4562-b3fc-2c963f66afa6

# Explicitly complete one WIP or a whole tab.
wips close --pane '%3'
wips close --window '@2'

# Check configuration, paths, tmux, and provider prerequisites.
wips doctor
```

Pane IDs begin with `%` and window IDs begin with `@`. Quoting them avoids shell
job-control interpretation.

`wips send` accepts only a tracked, running pane, so a split tab always has an
explicit recipient. It loads the message through stdin into an isolated tmux
buffer, uses bracketed paste so multiline prompts remain one input event, and
then submits the prompt. Prompt text is not placed in the tmux command's argv.

When Codex assigns or changes its user-facing session name, WIPS reads that
name from Codex's local state and applies it as the durable tmux tab title.
Automatic renaming is limited to tabs with exactly one
open agent session; split tabs keep their explicit shared title so panes cannot
compete over it. `wips rename` remains available for an explicit title.

`wips reload` replaces each running WIPS pane runner with the installed WIPS
binary while keeping the tmux panes and agent processes alive. It checks every
target before starting; a pane launched by an older WIPS runner without reload
support must first be resumed with a newer binary after its agent exits.

## tmux keys

`prefix` means the tmux prefix, `Ctrl-b` by default. WIPS keeps the normal tmux
bindings and adds:

| Key | Action |
| --- | --- |
| `prefix c` | Create a new tab and WIP |
| `prefix %` | Split left/right and create a WIP |
| `prefix "` | Split top/bottom and create a WIP |
| `prefix x` | Confirm and complete the current WIP |
| `prefix &` | Confirm and complete the current tab |
| `prefix r` | Resume an exited WIP in the current pane |
| `prefix ,` | Change the current tab's title |
| `prefix f` | Open local WIP search |

Use the normal `prefix d` to detach without completing anything. WIPS configures
tmux with `remain-on-exit`, so an exited agent remains visible and resumable.

## Configuration

The first run creates a Codex-only default. A configuration with generic Codex
and Claude presets looks like this:

```toml
default_agent = "codex"

[tmux]
socket = "wips"
session = "wips"

[agents.codex]
kind = "codex"
program = "codex"
args = []

[agents.claude]
kind = "claude"
program = "claude"
args = []
```

Preset names are passed to `wips new --agent NAME`. `program` is one executable
name or path, and every `args` entry is one literal argument. WIPS uses native
process APIs: it does not join the values into a command line or ask a shell to
split them. Spaces, quotes, `$`, semicolons, glob characters, and pipes inside
an argument remain part of that argument.

WIPS owns provider session identity and resume arguments. Do not put Codex
`resume`/`--last` or Claude `--session-id`/`--resume`/`--continue` flags in a
preset; configuration validation rejects identity options that would conflict
with WIPS.

WIPS also owns the Codex CLI overrides for `SessionStart`,
`UserPromptSubmit`, and `Stop`. Put other Codex hooks in the normal Codex
configuration files, where hook sources merge; a preset that directly
overrides one of those three hook keys is rejected rather than silently
replacing either hook.

WIPS does not add approval, sandbox, or permission-bypass flags. Any optional
provider flags in `args` are an explicit local configuration choice and retain
the provider CLI's own meaning.

## Lifecycle and recovery

WIPS tracks workflow state separately from process state:

- An agent starting or running is an open WIP.
- A normal exit, crash, or killed terminal changes runtime state but leaves the
  WIP open.
- `wips resume` restarts an open, exited WIP with the provider's durable session
  ID.
- Only `wips close` (or the matching tmux key) marks a WIP completed. Closing a
  tab completes its contained WIPs.

Session metadata, working directories, provider IDs, tab ordering, pane layout,
and searchable hook content are stored in SQLite. If the dedicated tmux server
is still alive, `wips` reconciles and attaches to it. If it is gone, WIPS
reconstructs the open tabs and panes from SQLite, reapplies saved layouts, and
uses the provider's resume mechanism where a provider session ID is available.
Completed records remain available to history-oriented commands such as
`wips list --all`.

`wips search WORDS...` queries the local message index populated by
`UserPromptSubmit` and `Stop` hooks. Hook delivery is part of search and resume
bookkeeping; provider transcripts remain the provider's own files and are not
replaced by the WIPS index.

## Provider integration

Claude supports assigning a UUID before startup. WIPS launches a new Claude
session with `--session-id UUID`, resumes it with `--resume UUID`, and supplies a
generated `--settings` document. Its `SessionStart`, `UserPromptSubmit`, and
`Stop` hooks use Claude's exec form: the current WIPS executable is the command
and `["hook"]` is the argument vector, with no hook shell involved.

Codex generates its session ID when the real interactive session starts. WIPS
launches Codex normally and injects stable `-c` definitions for the same three
lifecycle events. The hook command is exactly:

```sh
"$WIPS_EXECUTABLE" hook
```

The executable path, logical WIP session ID, and state identifiers are passed
through environment variables, which keeps the reviewed Codex hook definition
stable across panes and sessions.

After the `SessionStart` hook supplies Codex's provider session ID, WIPS polls
the thread's explicit name from Codex's local state database using a read-only
connection. It does not open the thread in app-server or infer a tab title from
terminal output.

### Trusting the Codex hook

Codex requires review before a non-managed command hook can run. When Codex
reports that the WIPS hook needs review, open `/hooks`, inspect the three WIPS
lifecycle entries, and trust them. Codex hashes the exact definition, so a new
or changed definition requires review again.

WIPS intentionally does not pass `--dangerously-bypass-hook-trust`. Until the
hook is trusted, Codex skips it; session-ID capture and searchable message
indexing for that run may therefore be incomplete.

### Why WIPS starts Codex interactively

A background `codex exec noop` can obtain a Codex session ID, but it also
creates a synthetic model turn, consumes prompt/model tokens, and pollutes the
conversation that the user later resumes. WIPS instead learns the ID from the
`SessionStart` hook of the actual interactive conversation.

Creating an empty thread through the Codex app server is not a substitute: an
empty app-server thread is not yet a durable provider conversation that can be
relied on for later CLI resume. WIPS therefore uses the provider's real session
lifecycle rather than manufacturing a placeholder turn or empty thread.

## Files and XDG paths

On Linux, the defaults are:

| Purpose | Default |
| --- | --- |
| Configuration | `${XDG_CONFIG_HOME:-$HOME/.config}/wips/config.toml` |
| State directory | `${XDG_STATE_HOME:-$HOME/.local/state}/wips` |
| SQLite database | `${XDG_STATE_HOME:-$HOME/.local/state}/wips/wips.sqlite3` |

Set `WIPS_CONFIG` to override the complete configuration-file path and
`WIPS_STATE_DIR` to override the state directory. WIPS passes those resolved
paths to its lifecycle hook process so the hook always opens the same database
as the parent session. Relative overrides are made absolute from the directory
where WIPS was invoked before any agent changes its working directory.

New application-owned directories are created with mode `0700` on Unix, and
new configuration, database, and generated hook-settings files are restricted
to mode `0600`.

## Architecture and tradeoffs

WIPS is intentionally small and local:

- tmux owns terminal multiplexing, pane survival, detach/attach behavior, and
  layout application. A dedicated socket isolates WIPS from the user's normal
  tmux server.
- SQLite owns durable workflow metadata and full-text search. Runtime tmux state
  is reconciled with the database rather than treated as the only source of
  truth.
- Provider adapters build native executable-plus-argv specifications. They do
  not invoke `sh -c` for configured agent programs.
- Lifecycle hooks connect provider-assigned session IDs and messages back to the
  durable logical WIP record for that pane.

This design favors transparent local state and ordinary CLI tools over a
resident network service. It also means WIPS depends on tmux semantics and on
the supported providers' lifecycle/resume contracts.

## Safety and privacy

WIPS does not store provider credentials or replace provider authentication.
It does store working-directory paths, titles, provider session identifiers,
and hook-supplied searchable prompt/response content in the local state
database. Treat the state directory as sensitive and include it in any local
backup policy deliberately.

Configured agent arguments are never evaluated by WIPS as shell source. The
Claude hook is also exec-form. Codex currently represents command hooks as a
command string, so WIPS uses the fixed, quoted environment-variable command
shown above and leaves Codex's hook trust gate enabled.

Completing a WIP is explicit because it changes durable workflow state and may
remove its live tmux pane. Review confirmation prompts before closing panes or
tabs. Detaching is the non-destructive way to leave work running.

## Development

```console
cargo fmt --check
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
```

WIPS is licensed under Apache-2.0.
