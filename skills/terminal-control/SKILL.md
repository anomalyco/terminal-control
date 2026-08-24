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

Keep an application visible and human-controlled in the current terminal pane while exposing the
same session controls to agents:

```bash
termctrl run -- /usr/bin/nvim
termctrl run editor -- nvim
```

The form without `NAME` uses the executable basename (`nvim`) and fails on a name collision; it
never chooses a suffixed name. Pass an explicit name when running multiple copies.

## Choose The Correct Observation

- Use `show` for current visible screen text. Prefer it for reasoning about full-screen TUIs.
- Use `logs` for readable retained output from normal-screen tools and log-like commands.
- Use `save --format ... --out ...` only when a persisted artifact is required.
- Use `video` only after explicitly recording a timeline with `--record`.

Do not treat logs as the visible state of an alternate-screen TUI.

Named-session reads return immediately by default. Add settling only when intentionally waiting for
quiet output. `wait` defaults to five seconds; set `--timeout` only when choosing another limit.

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
termctrl show app --format semantic
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
- Run `termctrl list` to discover running named sessions. Add `--state`, `--command`, or `--cwd` when narrowing discovery; use `--all` only when retained exited or unavailable entries are relevant.
- MCP agents can pass `state`, `command`, or `cwd` to `list_sessions` and use `get_session_status({ name })` for complete structured status without parsing CLI output.
- If a session socket path is too long, set `TERMCTRL_RUNTIME_DIR` to a short private directory under `/tmp` before starting sessions.
- If `termctrl` is unavailable, install Terminal Control with `cargo install terminal-control` or ask the user which installed binary to use.
