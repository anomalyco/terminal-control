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

Use `session.mouse(MouseEvent::new(Action::Move, 12, 4))` to hover and `Action::Click` to click,
with types from `terminal_control::mouse`. Coordinates are zero-based cells; `Down`/`Move`/`Up`
form a drag. Reporting follows the application's negotiated protocol and fails if disabled.
Set `recording::VideoOptions::pointer_overlay` to `Some(PointerOptions::default())` to visualize
typed mouse input at export, or use `PointerOptions { reduced_motion: true }` for fades only.
The overlay does not modify `Frame` or screen captures.

To request optional application semantics, enable the OpenTUI host profile before launch and read
the provider with the same embedded session:

```rust
use std::time::Duration;

let mut options = terminal_control::shot::Options::default();
options.opentui_host = true;
let mut session = terminal_control::session::Session::start(
    &["my-opentui-app".to_owned()],
    None,
    None,
    &options,
)?;
let semantics = session.semantic_snapshot(Duration::from_secs(1))?;
```

No connected provider returns `termctrl-semantic-snapshot-v1` with an empty `nodes` array. See
[semantic-protocol.md](semantic-protocol.md) for the application-side wire contract.

## Versioned Structured Output

- A `save --format json` capture is a `Frame` object with `version: 2`, described by `schemas/frame-v2.schema.json`.
- A `.termctrl` recording is JSON Lines: its first line is a version-2 header and subsequent lines are timed output, input, mouse, resize, or marker entries, described by `schemas/recording-entry-v2.schema.json`. Current readers also accept version 1 (`schemas/recording-entry-v1.schema.json`). Older binaries cannot read v2 recordings.
- A `mouse` entry contains `at_ms`, the typed `event`, and delivered `bytes`. It is client input, not output, and replay never feeds it into the terminal emulator. Mouse timestamps must be nondecreasing.
- Recording byte arrays contain the original terminal or input bytes as integers from `0` to `255`; recordings can contain sensitive text or input.
