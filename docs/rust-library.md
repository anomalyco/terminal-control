# Rust Library

The crate exposes the shot engine, live sessions, and artifact model to Rust callers. The CLI is built on the same `terminal_control::shot`, `terminal_control::session`, `terminal_control::frame`, `terminal_control::render`, and `terminal_control::recording` modules.

## Render An ANSI Stream

```rust
let shot = terminal_control::shot::from_ansi(b"\x1b[32mready\x1b[0m".to_vec(), 1, 20, 1024)?;
assert_eq!(shot.frame.text(), "ready");
let svg = terminal_control::render::svg(&shot.frame, &terminal_control::render::Options::default());
```

## Embedded Sessions

A library session keeps one PTY-backed application in process for fast test interaction without repeatedly invoking the CLI:

```rust
use std::time::Duration;

let mut session = terminal_control::session::Session::start(
    &["my-terminal-app".to_owned()],
    None,
    None,
    &terminal_control::shot::Options::default(),
)?;
session.wait_for_text("Ready", Duration::from_secs(5))?;
let status = session.status()?;
session.send(b"help\r")?;
session.wait_for_idle(Duration::from_millis(250), Duration::from_secs(5))?;
let capture = session.capture(Duration::from_millis(250), Duration::from_secs(5))?;
let shot = capture.shot;
let exit = session.wait_for_exit(Duration::from_secs(5))?;
session.stop()?;
```

`session::Session` is the embedded lifecycle interface; the flat named-session CLI commands and the external driver are adapters over the same implementation.

## Versioned Structured Output

- A `save --format json` capture is a `Frame` object with `version: 1`, described by `schemas/frame-v1.schema.json`.
- A `.termctrl` recording is JSON Lines: its first line is a versioned header and subsequent lines are timed output, input, resize, or marker entries, each described by `schemas/recording-entry-v1.schema.json`.
- Recording byte arrays contain the original terminal or input bytes as integers from `0` to `255`; recordings can contain sensitive text or input.
