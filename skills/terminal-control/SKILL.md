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

Do not treat logs as the visible state of an alternate-screen TUI.

Named-session screen reads are immediate by default. Do not pass `--settle-ms 0` or
`--deadline-ms 0`; omit both options. Set them only to intentional nonzero values when a specific
transition needs quiet-output settling.

`wait` defaults to a five-second maximum and returns as soon as its text appears. Do not pass
`--timeout 5000`; omit it. Set `--timeout` only when intentionally choosing a different limit.

## Drive Input Precisely

Send plain text with `text:<value>` and named keys as separate input atoms:

```bash
termctrl send app text:/connect enter
termctrl send app down enter
termctrl send app ctrl-c
printf '%s' 'multiline prompt' | termctrl send app --stdin
```

Use `wait` after sending input instead of sleeping or assuming that the interface has updated.

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
