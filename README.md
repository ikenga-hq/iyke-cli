# iyke

[![Version](https://img.shields.io/badge/version-v0.0.0-blue.svg)](https://github.com/ikenga-hq/iyke-cli/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

> `iyke` — the runtime controller for a running Ikenga shell. Drive panes, modes, tabs, and
> read live state from your terminal.

## What it is

`iyke` talks to a *running* Ikenga desktop app over its localhost control bridge. The app
binds an HTTP server to `127.0.0.1:<random-port>` with a per-launch bearer token; `iyke`
reads the control file the app writes and forwards subcommands as authenticated calls. Use
it to navigate, switch modes, open tabs, and inspect UI state from a script.

It's one of two Ikenga CLIs — the **runtime** one. (The other,
[`ikenga`](https://github.com/ikenga-hq/ikenga-cli), manages packages on disk. They share
no code.)

## Install

From the monorepo root:

```bash
cargo install --path iyke-cli
```

This puts an `iyke` binary in `~/.cargo/bin`. Make sure that directory is on your `PATH`.

## Examples

```bash
iyke state                           # show what the app is currently displaying
iyke --json state | jq .shell.route  # programmatic access

iyke go /finance/receivables         # navigate the focused pane
iyke mode files                      # switch the sidebar to the files mode

iyke open route /agents              # open a new tab in the focused pane
iyke open terminal --cmd "bun run dev"
iyke open mini-app video-engine

iyke split horizontal                # split the focused pane side-by-side
iyke focus index 2                   # focus the 2nd leaf pane (matches ⌃2)
iyke close                           # close the focused pane

# Spawn a terminal and own it from birth (no window in which another
# writer could get in between the spawn and a separate lease call)
iyke agent register --name orchestrator
SPAWN=$(iyke --json terminal-spawn --label builder --lease-for orchestrator \
  --cwd /home/you/project -- bun run dev)
TERMINAL=$(jq -r .terminal_id <<<"$SPAWN")
LEASE=$(jq -r .lease_token <<<"$SPAWN")

# ...work the terminal, then retire it. The tab stays by default, showing
# its exit status, so the scrollback survives for a post-mortem.
iyke terminal-kill builder            # add --close-tab to remove the tab too

# Kernel-authoritative terminal orchestration
TERMINAL=$(iyke --json terminals | jq -r '.[0].terminal_id')
iyke terminal-label "$TERMINAL" reviewer
iyke terminal-read --label reviewer --after 0
CURSOR=$(iyke --json terminal-read --label reviewer | jq -r .end_offset)
iyke terminal-send "run checks" --label reviewer --key Enter --no-focus --actor orchestrator
iyke terminal-wait --label reviewer --after "$CURSOR" --match 'READY|FAILED'
iyke terminal-read --label reviewer --mode screen

# Optional writer exclusion and restart guards
LEASE=$(iyke --json terminal-lease acquire "$TERMINAL" --agent-id orchestrator | jq -r .token)
PTY=$(iyke --json terminals | jq -r --arg id "$TERMINAL" '.[] | select(.terminal_id == $id) | .pty_id')
iyke terminal-send "continue" --terminal "$TERMINAL" --key Enter \
  --expected-pty-id "$PTY" --lease-token "$LEASE" --actor orchestrator

iyke terminal-lease release "$TERMINAL" --token "$LEASE"

# Shared coordination store with event-driven long polling

iyke scratchpad set --scope project:royalti-co --name handoff --body ready
iyke scratchpad watch --scope project:royalti-co --name handoff

# Agent inbox — where due timers deliver. `--ack` DELETES what it printed,
# so a plain re-run returns only what arrived since.
iyke inbox list --agent-id orchestrator
CURSOR=$(iyke --json inbox list --agent-id orchestrator | jq -r .next_since)
iyke inbox list --agent-id orchestrator --since "$CURSOR" --ack

# The durable task board the human works from — outlives any single run
iyke task create "Ship the control plane" --priority high --assigned-to orchestrator
iyke task list --status pending
iyke task update "$TASK_ID" --status in_progress --progress-pct 40
iyke task complete "$TASK_ID" --task-result "merged in #74"
```

Add `--json` to any command for machine-readable output. If the desktop app is not running,
every command exits non-zero with a clear message instead of hanging.

## Actions, menus and keys

The Actions noun (WP-62) reads and writes the same `actions.json` / `keybindings.json` layer
the D-06 Actions/Menus/Keys settings tabs do (G-ACTIONS,
`plans/shell-ux-rearchitecture/drafts/actions-schema.md` in the workspace meta-repo). Every
write round-trips into the running shell's frontend and through its validator — a CLI-authored
action or key can't skip a check the UI would apply.

```bash
# List the effective actions the shell currently merges.
iyke actions list
iyke actions list --scope personal

# Upsert one user action by id. --doc - reads the document from stdin.
iyke actions set explain-file --scope personal --file explain-file.json
echo '{"name":"Explain this file","run":{"kind":"skill","skill":"explain"}}' \
  | iyke actions set explain-file --scope personal --doc -

# Import a batch from a teammate's actions.json. An id already present in
# the target scope's file is skipped unless --overwrite is given. Exits
# non-zero if the write is refused or any item errored.
iyke actions import --from team --file teammate-actions.json --scope project
iyke actions import --from team --file teammate-actions.json --overwrite

# Show one effective menu (frozen menu ids, G-ACTIONS §1.3).
iyke menus show files
iyke menus show section/automations
iyke menus show native/file

# List the effective keymap, add a binding, and ask "what fires here".
iyke keys list --search explain
iyke keys set --scope personal --command explain-file --key mod+shift+e
iyke keys resolve mod+k
```

`--from pkg` and `--from vscode` are not implemented yet — they land with the Import tab
(WP-61); `actions import --from pkg|vscode` prints a clear message and exits non-zero.

### Brief a Chi to make an action

An `actions.json` entry is `{id, name, icon?, description?, run, placements?, scope}`. `id` is
`^[a-z0-9][a-z0-9-]{0,63}$` (no `.`, no `:`, unique in the file, never a built-in id); `name` is
1–80 characters; `scope` must equal the file it's written to (`personal` or `project`). Below is
a minimal valid entry per `run.kind` — hand one of these to a Chi as the shape to fill in:

```jsonc
// chi — dispatch a prompt to a session
{ "id": "explain-file", "name": "Explain this file", "scope": "personal",
  "run": { "kind": "chi", "target": "active", "prompt": "Explain {{file.path}}" } }

// shell — run a command (personal only, or project + trusted, G-ACTIONS §8.3)
{ "id": "refresh-pulse", "name": "Refresh pulse snapshots", "scope": "project",
  "run": { "kind": "shell", "command": "scripts/pulse/build-all.sh" } }

// iyke — call a bridge route
{ "id": "reveal-in-files", "name": "Reveal in Files", "scope": "personal",
  "run": { "kind": "iyke", "route": "/pane/navigate", "method": "POST" } }

// skill — invoke an installed skill
{ "id": "run-release-status", "name": "Release status", "scope": "personal",
  "run": { "kind": "skill", "skill": "release-status" } }

// workflow — disabled until a workflow runner exists (accepted, never runs)
{ "id": "nightly-audit", "name": "Nightly audit", "scope": "personal",
  "run": { "kind": "workflow", "workflow": "nightly-audit" } }

// open — a shell route, a pkg:// pane, or an external URL
{ "id": "open-docs", "name": "Open docs", "scope": "personal",
  "run": { "kind": "open", "url": "https://ikenga.dev/docs" } }
```

`placements` (default `[]`) is a list of `{ "at": "<menuId>", "when"?: "<expr>" }` — omit it for
an action reachable only by key or `iyke actions set`. The six `{{name}}` run variables
(`file.path`, `file.name`, `selection`, `project.root`, `pane.url`, `branch`) interpolate as
`""` when there's no matching context.

## Chi seats

A **seat** is a named, per-project slot for an agent session — `seat:<project>/<name>`
(G-SEATS, `plans/shell-ux-rearchitecture/drafts/seats-schema.md` in the workspace meta-repo).
It points at one session, a Chi run or an agent terminal, and keeps its name, scratchpad and
address when that session ends. Needs a shell with bridge API 5; an older shell gets a clear
"needs a newer Ikenga shell" error.

```bash
# What's seated in the active project (or --project <id>): status, session,
# who holds it since when, and engine caveats.
iyke seat ls
#   seat:royalti-co/lead             live    claude-code      terminal 9f2c… (agent live)
#       held by orchestrator since 2026-09-27 14:03Z
#   seat:royalti-co/nightly          run     openrouter       run 41d0…
#       not resumable after restart

# Create a seat: empty, around an open session, or around a past one.
iyke seat create docs --engine claude-code
iyke seat create docs --session <terminal-or-run-id>
iyke seat create docs --engine claude-code --resume <run-id>

# Send to a seat — its own addressing mode, not an alias of --label. The text
# goes to whatever the seat holds: typed into its terminal (with Enter), queued
# behind a busy run, or sent to an idle run. A vacant seat resumes its last
# session first, or starts a fresh one when it can't resume — and says which.
iyke terminal-send --seat docs "update the README for v0.6"

# Resume a vacant seat with a first turn (never falls back to a fresh session;
# a seat that can't resume answers not_resumable), or move a session in.
iyke seat resume docs --prompt "pick up where you left off"
iyke seat resume docs --session <terminal-or-run-id>

# Fill with a new session, clear (the scratchpad stays), release a hold.
iyke seat fill docs --prompt "draft the changelog"
iyke seat clear docs
iyke seat release docs
```

A `<seat>` is `<name>` (in the shell's active project), `@<name>`, `<project>/<name>` or
`seat:<project>/<name>`. A `<ref>` is a terminal id or a Chi run id; seating a terminal whose
engine the shell can't read off its command line needs `--engine`.

**Holds and takeover.** `--as <client>` names the caller (default `iyke` — this CLI has no
agent identity of its own); `--hold` holds the seat for that client (10 minutes, renewed by
each held call); another client is then refused with "held by X since T; --takeover to claim
it". `--takeover` is the only way past a hold, and the displaced client is told once, on its
next call. A hold is a courtesy between clients, not a security boundary. The three flags work
on every `seat` verb and on `terminal-send --seat`.

Rename and remove are app-only for now, and `seat resume` without `--prompt` or `--session`
answers `needs_prompt` (an interactive start needs the Ikenga window).

## Testing

`cargo test` covers argument parsing only — it proves clap accepts the flags,
nothing about what goes on the wire. For the terminal / inbox / task surface,
run the live suite against a running shell:

```bash
bun run tauri dev            # in the shell repo first
./scripts/live-test.sh       # 23 checks: real PTYs, leases, timer→inbox
```

It creates and reclaims its own terminals. Note the shell writes `control.json`
during Rust setup, *before* the webview registers its `iyke://` listeners — so
frontend-backed endpoints (`terminal-spawn`, `dom`) return `503 … timed out`
until then. The script waits for a real frontend round-trip rather than for the
control file; if you write your own harness, do the same.

## Links

- [`ikenga-cli`](https://github.com/ikenga-hq/ikenga-cli) — the package manager (the *other* CLI)
- [`ikenga`](https://github.com/ikenga-hq/ikenga) — the desktop shell it controls

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
