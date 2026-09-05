---
name: wips
description: Operate WIPS-managed coding-agent work, including inspection, search, creation, splitting, adoption, resumption, cross-pane messaging, tab renaming, workspace restoration, and explicit completion. Use for work tracked by WIPS; do not use for unrelated tmux administration.
---

# WIPS

Use WIPS as the control plane for its dedicated tmux workspace and durable
workflow state.

## Bootstrap

Before the first WIPS action in a task, run `scripts/ensure-wips.sh` relative to
this skill directory. The script:

- reuses `wips` when it is already installed;
- otherwise installs WIPS from its canonical GitHub repository with Cargo;
- verifies the executable with `wips --version`; and
- prints the resolved executable path as its final stdout line.

Use that absolute path as `<wips>` in the commands below. If Cargo is missing or
installation fails, report the blocker; do not substitute an unrelated package
manager or a different source.

## Preserve WIPS semantics

- Workflow state and process state are different. An exited or missing agent is
  still an open WIP until the user explicitly completes it.
- Use WIPS commands for mutations. Raw `tmux rename-window`, `kill-pane`, or
  `kill-window` bypass durable state and can make restoration inconsistent.
  Read-only tmux queries are acceptable when needed to resolve or verify IDs.
- Do not complete a pane or tab without explicit user intent. Completing a tab
  completes every WIP in it.
- Preserve attached clients and running panes. Detach normally with `prefix d`;
  do not kill the WIPS tmux server as a way to leave it.
- Quote pane IDs such as `'%3'` and window IDs such as `'@2'` because shells may
  interpret their prefix characters.

## Inspect before acting

Use `<wips> list` for open work and `<wips> list --all` when completed history
matters. The list is tab-separated and includes workflow state, runtime state,
agent, pane ID, session title, and working directory.

Use `<wips> doctor` when configuration, tmux, storage, or agent prerequisites
may be unhealthy. Do not run `start` merely to inspect state: it restores and
attaches an interactive tmux client.

## Rename the current tab

After bootstrap, if `TMUX_PANE` is set and that exact pane ID appears in
`<wips> list`, immediately run `<wips> rename "new title"`. Do not resolve the
window ID or run a tmux query first. Use
`<wips> rename --window "@N" "new title"` only when targeting another tab or
when no matching current WIPS pane is available.

## Supported actions

- Restore and attach: `<wips>` or `<wips> start`.
- Create a tab: `<wips> new`, optionally with `--agent NAME`, `--cwd PATH`, or
  `--resume PROVIDER_SESSION_ID`.
- Split from a pane: `<wips> new --split horizontal --target '%3'` for a
  left/right split, or use `vertical` for a top/bottom split. The target may
  default from `TMUX_PANE` only when running inside the intended WIPS pane.
- Search indexed prompts and responses: `<wips> search TERMS...`. Supplying no
  terms opens an interactive prompt.
- Resume an open, exited agent in place: `<wips> resume --pane '%3'`. Do not use
  resume for a pane whose runtime state is still running.
- Send a prompt to a running Codex or Claude agent:
  `<wips> send --pane '%3' 'message'`. Always take the pane ID from
  `<wips> list`; do not substitute a window ID. WIPS rejects exited panes, so
  resume them before sending.
- Rename a tab durably by following **Rename the current tab** above. Omit the
  title only when an interactive prompt is appropriate.
- Complete one WIP: `<wips> close --pane '%3'`.
- Complete a tab and all its WIPs: `<wips> close --window '@2'`.

Use `--confirm` with `close` only when a person is attached and able to answer
the prompt. In non-interactive automation, confirm scope with the user before
invocation rather than issuing a command that waits for input.

## Verify mutations

After creating, resuming, or completing work, inspect `<wips> list` (and
`--all` for completion) to confirm the durable result. After renaming, use a
read-only tmux window query against the configured WIPS socket when visual or
runtime verification is needed. A successful `send` confirms input delivery,
not that the receiving agent completed the request. Report the affected pane
or window ID and do not claim success from command launch alone.
