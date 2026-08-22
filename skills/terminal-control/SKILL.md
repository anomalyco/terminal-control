---
name: terminal-control
description: Drive and verify terminal applications with the termctrl CLI in a real PTY - read visible screens, run named live sessions, send typed keyboard input, wait for text, save evidence, record timelines, and export edited videos. Use when an agent must operate or test a TUI, REPL, interactive CLI, shell process, or OpenTUI application.
---

# Terminal Control

Use `termctrl` to observe the actual visible terminal state and drive interaction deterministically.

## Start With The Smallest Workflow

Read a disposable terminal application's settled visible screen when no further interaction is required:

```bash
termctrl show -- my-terminal-app
```

Keep an application alive when interaction or repeated inspection is required:

```bash
termctrl start app -- my-terminal-app
termctrl wait app "Ready"
termctrl show app
termctrl send app text:help enter
termctrl wait app "Commands"
termctrl show app
termctrl stop app
```

Always stop named sessions after use unless the user explicitly wants the live process retained.

Use a checked-in `.tape` when the interaction itself should be repeatable as a deterministic demo:

```bash
termctrl play demos/app.tape
```

`play` validates the complete source before launching its named session. Tape steps reuse the normal
`wait`, paced text/key input, click, drag, marker, recording, and stop behavior. Prefer `Wait` for
state transitions and use `Sleep` only for a deliberate presentation hold. `Setup`, `Action`, and
`Cleanup` are explicit argv-based host commands for controlled fixture changes; they do not
implicitly invoke a shell, default to a 30-second timeout, and retain bounded diagnostics. Setup runs
before Launch and reverse-order cleanup runs only after the owned session is confirmed stopped,
including safe failure paths. Relative paths resolve from the tape directory. The `.tape` authoring
source is distinct from any private `.termctrl`
recording it creates; inspect that recording and run `video --edit` as a separate phase.
Add `Pointer on` only when the demo needs structured click, move, and drag overlays. It requires `Record`,
writes opt-in recording version 2, and is rendered only with `--pointer`; ordinary recordings remain
version 1. Pointer capture currently applies to direct named sessions, not composed workspaces.
Use `Move X Y [Steps N] [Pace DURATION]` for smooth unpressed travel. The first Move establishes the
position; later Moves interpolate, and Click/RightClick/Drag update the same position. `RightClick X Y`
emits a secondary click and retains button evidence when `Pointer on` is enabled. `Key` accepts the
same input spellings as live `send`, including `shift+enter`. Use `Wait TEXT Match line` when a complete
visible row must match rather than a substring. Bare `--pointer` fades as
before. Use `--pointer=persistent` when a long presentation hold should retain the latest position;
click feedback remains brief and no pointer appears before the first event. Do not confuse this
render choice with the tape's `Pointer on` capture policy.

Enter a visible workspace that humans and agents control together:

```bash
termctrl run
termctrl run -- /usr/bin/nvim
termctrl run editor -- nvim
termctrl run workspace --tab-position top
termctrl run editor
termctrl attach editor
termctrl windows workspace --json
termctrl current --json
termctrl tab-position workspace top
termctrl move-window workspace editor --index 0
termctrl new-window workspace editor -- nvim
termctrl show workspace --window editor
termctrl panes workspace --json
termctrl layout workspace --grid 2x2
termctrl layout workspace --grid 2x2 -- nvim
termctrl send workspace --pane 1 text:opencode2 enter
termctrl focus workspace --pane 1
termctrl close-pane workspace --pane 1
termctrl resize-pane workspace --pane 1 --direction left --cells 5
termctrl zoom-pane workspace --pane 1
```

With no arguments, `run` starts `$SHELL` in the `workspace/main` window. The persistent tab strip
shows selection, pane count, zoom, and output (`+`), bell (`!`), or surviving-window pane exit (`x`)
activity. Use `tab-position NAME top|bottom` to move it live. Use `ctrl-b p` for the command palette,
`ctrl-b c/w` to create/list windows, `ctrl-b l` for the last window, `ctrl-b n` for next,
`ctrl-b </>` or tab dragging to reorder, and `ctrl-b t` to move tabs. Use `ctrl-b %` and
`ctrl-b "` to split; use `ctrl-b` plus arrows or `h/j/k`, or a mouse click, to focus. Use
`ctrl-b H/J/K/L` to resize,
`ctrl-b z` to toggle zoom, `ctrl-b d` to detach,
`ctrl-b q` to show stable pane IDs, and `ctrl-b ?` for help. `ctrl-b x` closes a pane, `ctrl-b &`
closes the current window, and `ctrl-b Q` closes the workspace after `y` confirmation.

`run NAME` creates when absent and reattaches when the workspace already exists. `attach NAME`
requires an existing workspace. Closing the terminal detaches without killing panes; `ctrl-b Q`
followed by `y`, or `termctrl stop NAME`, ends the workspace. Only one human terminal may be attached,
while agent controls remain available when detached.

Discover windows before panes. Window names are exact stable selectors; numeric indexes shift after reorder or close.
Pane IDs are globally stable across windows; do not infer identity from geometry or titles.
Inside a pane, use `termctrl current --json`; `TERMCTRL_WORKSPACE` and `TERMCTRL_PANE_ID` identify
the caller while the daemon resolves its current window after moves or renames.
Window-targeted `show`, `send`, `wait`, `logs`, `panes`, and `layout` do not change human selection.
Only `select-window` and `focus --pane ID` intentionally move the visible human context.

Workspaces follow their current human terminal and reject `termctrl resize`. `run --record` records
the composed workspace, including tabs, splits, window switches, resizes, and markers. Composed ANSI
output is a rendered snapshot; pane-targeted ANSI is the original pane stream.

## Choose The Correct Observation

- Use `show` for current visible screen text. Prefer it for reasoning about full-screen TUIs.
- Use `logs` for readable retained output from normal-screen tools and log-like commands.
- Use `save --format ... --out ...` only when a persisted artifact is required.
- Use `video` only after explicitly recording a timeline with `--record`.
- Use `--record-pointer` plus `--pointer` only when graphical mouse causality is part of the evidence.
- Use persistent pointer rendering only when the latest target must remain legible through idle holds.
- Use `play` when a repeatable interaction belongs in a reviewed `.tape` source file.
- Use `play --json` when a caller needs stable success fields, or `--quiet` when it needs no stdout.

Do not treat logs as the visible state of an alternate-screen TUI.

Named-session screen reads are immediate by default. Do not pass `--settle-ms 0` or
`--deadline-ms 0`; omit both options. Set them only to intentional nonzero values when a specific
transition needs quiet-output settling.

`wait` defaults to a five-second maximum and returns as soon as its text appears. Do not pass
`--timeout 5000`; omit it. Set `--timeout` only when intentionally choosing a different limit.

## Semantic UI tree

The `show` command takes an option `--format semantic` which returns a semantic tree representing the
interactable UI elements. This requires direct support from the application to work; once the app is ready
you can run this and see if it returns anything. An empty tree means no provider was connected at that
moment. If the application is still starting, wait for visible readiness and retry once; otherwise use
the visible screen instead. Treat protocol and provider errors as actionable failures rather than retrying
them blindly.
The application must be launched with `--host opentui`; the full read command is
`termctrl show app --format semantic`.

Attempt to use the semantic UI tree if you need to discover available UI elements.

## Drive Input Precisely

Send plain text with `text:<value>` and named keys as separate input atoms:

```bash
termctrl send app text:/connect enter
termctrl send app down enter
termctrl send app ctrl-c
termctrl click app 12 4
termctrl drag app 12 4 30 4
printf '%s' 'multiline prompt' | termctrl send app --stdin
```

Use `wait` after sending input instead of sleeping or assuming that the interface has updated.

Mouse coordinates are zero-based application cells: X increases from left to right and Y from top
to bottom. For a workspace, coordinates are local to the selected pane or the pane targeted with
`--pane`/`--window`; do not include tab chrome or pane borders. `click` sends a primary press and
release. `drag` sends a primary press, linearly interpolated held-button motion events including the
destination, and release; tune the event count with `--steps` and their interval with `--pace-ms`.
The application must have enabled mouse tracking.

## Operate OpenTUI Applications

Use the OpenTUI host handshake for applications such as OpenCode:

```bash
termctrl start app --host opentui --cols 112 --rows 34 -- opencode
termctrl wait app "/connect"
termctrl show app
```

Use `resize` when the application requires more visible area. Use `restart app` to reuse stored launch settings after a deliberate application restart.

## Retain Evidence Deliberately

Save only requested formats:

```bash
termctrl save app --format txt --format png --out artifacts/current
```

Record demos only when the user wants a retained timeline or video. Add markers while the session is running, inspect them after stopping, then export with an explicit edit plan:

```bash
termctrl start app --record artifacts/run.termctrl -- my-terminal-app
termctrl wait app "Ready"
termctrl mark app ready
termctrl send app text:demo enter
termctrl wait app "Done" --timeout 60000
termctrl mark app done
termctrl stop app
termctrl markers artifacts/run.termctrl
termctrl show --recording artifacts/run.termctrl --at-marker done
termctrl video artifacts/run.termctrl --edit artifacts/run-edit.json --footer --out artifacts/run.mp4
```

Use edit-plan `speed` values conservatively when terminal text should remain readable. Use `hold_ms` or `--tail-ms` when the final frame is the payoff. Pass `--footer` when a polished demo should show the clip caption, elapsed timecode, and `TERMINAL CONTROL` branding in a bottom footer; omit it for ordinary videos.

Treat `.termctrl` recordings, ANSI transcripts, screen artifacts, command arguments, and terminal input as potentially sensitive. Do not retain them unless needed, and do not expose their contents unnecessarily.

## Recover From Problems

- Run `termctrl status app` to inspect state and launch settings.
- Run `termctrl list` for running sessions, or `termctrl list --all` to include retained exited and
  stale entries. Preview cleanup with `termctrl prune --dry-run`, then run `termctrl prune`.
- MCP agents should use semantic workspace tools for context, windows, panes, layout, focus, movement,
  resize, zoom, and close operations instead of sending human prefix shortcuts. Use `get_screen` or
  `interact` for visible state and `save_screen` only for a retained PNG.
- If a session socket path is too long, set `TERMCTRL_RUNTIME_DIR` to a short private directory under `/tmp` before starting sessions.
- If `termctrl` is unavailable, install Terminal Control with `cargo install terminal-control` or ask the user which installed binary to use.
